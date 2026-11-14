//! Code mode: a JavaScript script, run in an embedded QuickJS sandbox, that makes this session's tool calls and returns one value.
use rquickjs::{CatchResultExt, Context, Ctx, Function, Runtime};
use serde_json::Value;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::{ABORTED, Agent, Event};
use crate::tools;

const HARNESS: &str = include_str!("code-harness.js");
const TIME_LIMIT: Duration = Duration::from_secs(600);

#[derive(Default)]
struct Inbox {
    /// The `tool()` calls made since the last batch: promise id, tool name, input.
    calls: Vec<(u32, String, Value)>,
    /// Some once the script returned or threw.
    outcome: Option<Result<String, String>>,
}

impl Agent {
    /// Code: runs `code` to its end. Each `tool()` call runs as a call from the model does, permission check included, one at a time.
    pub(super) fn code(&mut self, id: &str, input: &Value, emit: &mut dyn FnMut(Event)) -> Result<String, String> {
        // The schema is left out while the setting is off, but a model can still name the tool.
        if !self.code_mode {
            return Err("Code mode is off. Make the tool calls directly.".into());
        }
        let code = input["code"].as_str().ok_or("Code needs `code`.")?;
        self.run_script(id, code, TIME_LIMIT, emit)
    }

    /// `limit` bounds the script's own work: its JavaScript and its tool calls, not the time the user takes to answer a prompt.
    fn run_script(&mut self, id: &str, code: &str, limit: Duration, emit: &mut dyn FnMut(Event)) -> Result<String, String> {
        let js = |e: rquickjs::Error| format!("JavaScript runtime: {e}");
        let runtime = Runtime::new().map_err(js)?;
        runtime.set_memory_limit(512 << 20);
        let deadline = Arc::new(Mutex::new(Instant::now() + limit));
        let (abort, js_deadline) = (self.abort.clone(), deadline.clone());
        runtime.set_interrupt_handler(Some(Box::new(move || abort.load(Ordering::Relaxed) || Instant::now() > *js_deadline.lock().unwrap())));
        let context = Context::full(&runtime).map_err(js)?;
        let inbox = Rc::new(RefCell::new(Inbox::default()));
        context.with(|ctx| install(&ctx, &inbox, code))?;
        let mut blocks = Vec::new();
        let mut ran = 0;
        loop {
            context.with(|ctx| while ctx.execute_pending_job() {});
            if self.abort.load(Ordering::Relaxed) {
                return Err(ABORTED.into());
            }
            if Instant::now() > *deadline.lock().unwrap() {
                return Err(format!("The script ran past {} minutes and was stopped after {ran} tool calls.", limit.as_secs() / 60));
            }
            if let Some(outcome) = inbox.borrow_mut().outcome.take() {
                // Images and frames the script's calls returned go to the model with the script's value.
                self.result_blocks = blocks;
                return match outcome {
                    Ok(value) => Ok(tools::truncate_head(value, "code")),
                    Err(error) => Err(format!("{error}\n\n{ran} tool calls ran before the script failed.")),
                };
            }
            let calls = std::mem::take(&mut inbox.borrow_mut().calls);
            if calls.is_empty() {
                return Err("The script is awaiting something that never settles.".into());
            }
            for (promise, name, input) in calls {
                let outcome = if self.offers_to_script(&name) {
                    ran += 1;
                    // The script parses what Bash prints, so the command runs as written: rtk turns the `total` row of `wc -l` into `Σ 9`.
                    let rtk = std::mem::replace(&mut self.rtk, tools::rtk::Rtk::OFF);
                    let waited = self.waited_on_user;
                    self.in_script = true;
                    let call = self.run_call(&format!("{id}-{ran}"), &name, &input, None, emit);
                    self.in_script = false;
                    self.rtk = rtk;
                    *deadline.lock().unwrap() += self.waited_on_user - waited;
                    let (content, output, is_error) = call?;
                    blocks.extend(content.as_array().into_iter().flatten().filter(|block| block["type"] != "text").cloned());
                    if is_error { Err(output) } else { Ok(output) }
                } else {
                    Err(format!("No tool named {name} is offered to this script."))
                };
                context.with(|ctx| settle(&ctx, promise, outcome))?;
            }
        }
    }

    /// A script calls only what the model may call directly, so --tools and a subagent's narrower list hold inside it.
    fn offers_to_script(&self, name: &str) -> bool {
        let (_, offered) = self.system_and_tools();
        name != "Code" && offered.as_array().is_some_and(|tools| tools.iter().any(|tool| tool["name"] == name))
    }
}

/// Globals the script sees: the harness and the native hooks it calls. Then starts the script as one async function, so its body can `await` and `return`.
fn install(ctx: &Ctx, inbox: &Rc<RefCell<Inbox>>, code: &str) -> Result<(), String> {
    let js = |e: rquickjs::Error| format!("JavaScript runtime: {e}");
    let globals = ctx.globals();
    let calls = inbox.clone();
    let request = move |id: u32, name: String, input: String| calls.borrow_mut().calls.push((id, name, serde_json::from_str(&input).unwrap_or(Value::Null)));
    globals.set("__request", Function::new(ctx.clone(), request).map_err(js)?).map_err(js)?;
    let finished = inbox.clone();
    let finish = move |ok: bool, text: String| finished.borrow_mut().outcome = Some(if ok { Ok(text) } else { Err(text) });
    globals.set("__finish", Function::new(ctx.clone(), finish).map_err(js)?).map_err(js)?;
    ctx.eval::<(), _>(HARNESS).catch(ctx).map_err(|e| format!("Code harness: {e}"))?;
    let main = format!("(async function () {{{code}\n}})().then((value) => __done(true, value), (error) => __done(false, error));");
    ctx.eval::<(), _>(main).catch(ctx).map_err(|e| format!("The script failed: {e}"))
}

fn settle(ctx: &Ctx, promise: u32, outcome: Result<String, String>) -> Result<(), String> {
    let (ok, text) = match outcome {
        Ok(text) => (true, text),
        Err(text) => (false, text),
    };
    let settle: Function = ctx.globals().get("__settle").map_err(|e| format!("JavaScript runtime: {e}"))?;
    settle.call::<_, ()>((promise, ok, text)).catch(ctx).map_err(|e| format!("JavaScript runtime: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::permissions::{Decision, Mode};
    use crate::agent::{Client, Session};
    use serde_json::json;
    use std::path::PathBuf;

    /// An agent in full access with Code offered, in a project of its own under a temp home.
    fn agent(name: &str) -> (Agent, PathBuf) {
        let home = crate::process::canonicalize(std::env::temp_dir()).unwrap().join(format!("agent-code-{name}-{}", std::process::id()));
        let cwd = home.join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let settings = crate::settings::load(&home).unwrap();
        let session = Session::start(&home.join("sessions"), true, &cwd, "m").unwrap();
        let mut agent = Agent::new(Client::new(&home).unwrap(), &settings, home.clone(), cwd, session, home.join("sessions"), None);
        *agent.permission_mode.lock().unwrap() = Mode::FullAccess;
        agent.code_mode = true;
        agent.tools = agent.tool_schemas();
        (agent, home)
    }

    fn run(agent: &mut Agent, code: &str) -> Result<String, String> {
        agent.code("c1", &json!({"code": code}), &mut |_| {})
    }

    #[cfg(unix)]
    #[test]
    fn a_script_runs_its_tool_calls_in_order_and_returns_its_value() {
        let (mut agent, home) = agent("runs");
        let code = r#"
            await tool("Write", { file_path: "it's a.txt", content: "one" });
            const listed = await $`ls ${["it's a.txt"]}`;
            const missing = await $`ls nope`.nothrow();
            return { listed: listed.trim(), missing: missing.includes("nope") };
        "#;
        let mut calls = Vec::new();
        let result = agent.code("c1", &json!({"code": code}), &mut |event| {
            if let Event::ToolStart { id, name, .. } = event {
                calls.push(format!("{id} {name}"));
            }
        });
        let edited = agent.edited.clone();
        let cwd = agent.cwd.clone();
        std::fs::remove_dir_all(&home).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&result.unwrap()).unwrap(), json!({"listed": "it's a.txt", "missing": true}));
        assert_eq!(calls, ["c1-1 Write", "c1-2 Bash", "c1-3 Bash"]);
        assert_eq!(edited, [cwd.join("it's a.txt")], "the edit hooks at the prompt's end see the script's edits");
    }

    #[test]
    fn a_script_calls_only_the_tools_offered_to_the_model() {
        let (mut agent, home) = agent("offered");
        agent.only_tools = Some(vec!["Read".into(), "Code".into()]);
        let bash = run(&mut agent, "return await tool('Bash', { command: 'touch made' }).nothrow();");
        let nested = run(&mut agent, "return await tool('Code', { code: 'return 1' }).nothrow();");
        agent.code_mode = false;
        let off = run(&mut agent, "return 1;");
        let made = agent.cwd.join("made").exists();
        std::fs::remove_dir_all(&home).unwrap();
        assert_eq!(off.unwrap_err(), "Code mode is off. Make the tool calls directly.");
        assert_eq!(bash.unwrap(), "No tool named Bash is offered to this script.");
        assert_eq!(nested.unwrap(), "No tool named Code is offered to this script.");
        assert!(!made);
    }

    #[cfg(unix)]
    #[test]
    fn a_call_the_user_denies_stops_the_script() {
        let (mut agent, home) = agent("denied");
        *agent.permission_mode.lock().unwrap() = Mode::Ask;
        let code = "await $`touch made`; await $`touch after`; return 'done';";
        let result = agent.code("c1", &json!({"code": code}), &mut |event| {
            if let Event::Permission { reply, .. } = event {
                reply.send(Decision::Deny).unwrap();
            }
        });
        let made = ["made", "after"].map(|file| agent.cwd.join(file).exists());
        std::fs::remove_dir_all(&home).unwrap();
        assert_eq!(result.unwrap_err(), ABORTED);
        assert_eq!(made, [false, false]);
    }

    #[cfg(unix)]
    #[test]
    fn a_script_that_throws_never_settles_or_returns_nothing_says_so() {
        let (mut agent, home) = agent("fails");
        let thrown = run(&mut agent, "await $`true`; throw new Error('boom');").unwrap_err();
        let syntax = run(&mut agent, "return (;").unwrap_err();
        let stuck = run(&mut agent, "await new Promise(() => {});").unwrap_err();
        let silent = run(&mut agent, "await $`true`;").unwrap();
        std::fs::remove_dir_all(&home).unwrap();
        assert!(thrown.starts_with("Error: boom") && thrown.ends_with("1 tool calls ran before the script failed."), "{thrown}");
        assert!(syntax.starts_with("The script failed:"), "{syntax}");
        assert_eq!(stuck, "The script is awaiting something that never settles.");
        assert_eq!(silent, "The script returned no value.");
    }

    #[cfg(unix)]
    #[test]
    fn cat_in_a_script_prints_the_file_as_it_is_unless_a_read_rule_denies_it() {
        let (mut agent, home) = agent("cat");
        std::fs::write(agent.cwd.join("data.json"), r#"{"name": "x"}"#).unwrap();
        std::fs::write(agent.cwd.join(".env"), "KEY=1").unwrap();
        agent.permission_rules.deny = vec!["Read(.env*)".into()];
        let name = run(&mut agent, "return JSON.parse(await $`cat data.json`).name;");
        let denied = run(&mut agent, "return await $`cat .env`.nothrow();");
        std::fs::remove_dir_all(&home).unwrap();
        assert_eq!(name.unwrap(), "x");
        assert_eq!(denied.unwrap(), "Denied by the permission rule \"Read(.env*)\" in settings.json.");
    }

    #[test]
    fn what_a_script_logs_comes_back_before_its_value_or_error() {
        let (mut agent, home) = agent("logs");
        let both = run(&mut agent, "console.log('found', { n: 2 }); console.error(new Error('odd')); return [1];");
        let only_logs = run(&mut agent, "console.log('done');");
        let thrown = run(&mut agent, "console.log('step 1'); throw new Error('boom');").unwrap_err();
        std::fs::remove_dir_all(&home).unwrap();
        assert_eq!(both.unwrap(), "found {\"n\":2}\nError: odd\n[\n  1\n]");
        assert_eq!(only_logs.unwrap(), "done");
        assert!(thrown.starts_with("step 1\nError: boom"), "{thrown}");
    }

    #[test]
    fn the_time_limit_stops_a_busy_script_but_not_one_waiting_for_the_user() {
        let (mut agent, home) = agent("limit");
        let busy = agent.run_script("c1", "await null; while (true) {}", Duration::from_millis(100), &mut |_| {});
        *agent.permission_mode.lock().unwrap() = Mode::Ask;
        let write = "await tool('Write', { file_path: 'a.txt', content: 'x' }); return 'done';";
        let asked = agent.run_script("c2", write, Duration::from_millis(500), &mut |event| {
            if let Event::Permission { reply, .. } = event {
                std::thread::sleep(Duration::from_millis(600));
                reply.send(Decision::Allow).unwrap();
            }
        });
        std::fs::remove_dir_all(&home).unwrap();
        assert!(busy.unwrap_err().starts_with("The script ran past"));
        assert_eq!(asked.unwrap(), "done");
    }
}
