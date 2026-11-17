//! Code mode: a JavaScript script, run in an embedded QuickJS sandbox, that makes this session's tool calls and returns one value.
use rquickjs::{CatchResultExt, Context, Ctx, Function, Runtime};
use serde_json::Value;
use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::permissions::{self, Mode};
use super::{ABORTED, Agent, Event};
use crate::tools::{self, conflicts, rtk::Rtk};

const HARNESS: &str = include_str!("code-harness.js");
const TIME_LIMIT: Duration = Duration::from_secs(600);
/// How many of a script's Bash calls run at the same time.
const BASH_AT_ONCE: usize = 8;

/// A `tool()` call the script made: promise id, tool name, input.
type Call = (u32, String, Value);

#[derive(Default)]
struct Inbox {
    /// The `tool()` calls made since the last batch.
    calls: Vec<Call>,
    /// Some once the script returned or threw.
    outcome: Option<Result<String, String>>,
}

impl Agent {
    /// Code: runs `code` to its end. Each `tool()` call runs as a call from the model does, permission check included.
    pub(super) fn code(&mut self, id: &str, input: &Value, emit: &mut dyn FnMut(Event)) -> Result<String, String> {
        // The schema is left out while the setting is off, but a model can still name the tool.
        if !self.code_mode {
            return Err("Code mode is off. Make the tool calls directly.".into());
        }
        let code = input["code"].as_str().ok_or("Code needs `code`.")?;
        self.script_reviewed = self.review_script(code, emit)?;
        let result = self.run_script(id, code, TIME_LIMIT, emit);
        self.script_reviewed = false;
        result
    }

    /// Auto mode: the reviewer judges the script once, and its calls then skip the reviewer.
    /// False when nothing was reviewed, so each call is: another mode, or a script over the reviewer's length limit.
    fn review_script(&mut self, code: &str, emit: &mut dyn FnMut(Event)) -> Result<bool, String> {
        if *self.permission_mode.lock().unwrap() != Mode::Auto {
            return Ok(false);
        }
        let request = permissions::script_request(code);
        let Some(prompt) = permissions::review_prompt(&self.user_requests, self.follow_up.as_deref(), &self.cwd, &self.extra_dirs, &request, &Value::Null) else {
            return Ok(false);
        };
        match self.review(&request, Some(prompt), emit) {
            Some(denial) => Err(denial),
            None => Ok(true),
        }
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
            // Every call the script made before it next awaited one: `Promise.all` puts its calls here together.
            let calls = std::mem::take(&mut inbox.borrow_mut().calls);
            if calls.is_empty() {
                return Err("The script is awaiting something that never settles.".into());
            }
            let bash_offered = self.offers_to_script("Bash");
            let groups: Vec<&[Call]> = calls.chunk_by(|a, b| bash_offered && self.runs_beside(a) && self.runs_beside(b)).collect();
            for group in groups {
                let waited = self.waited_on_user;
                let outcomes = match group {
                    [(_, name, input)] => vec![self.script_call(id, &mut ran, name, input, &mut blocks, emit)?],
                    _ => self.bash_together(id, &mut ran, group, emit)?,
                };
                *deadline.lock().unwrap() += self.waited_on_user - waited;
                for ((promise, ..), outcome) in group.iter().zip(outcomes) {
                    context.with(|ctx| settle(&ctx, *promise, outcome))?;
                }
            }
        }
    }

    /// One call of the script, run as a call from the model runs. `blocks` collects the images and frames it returned.
    fn script_call(&mut self, id: &str, ran: &mut usize, name: &str, input: &Value, blocks: &mut Vec<Value>, emit: &mut dyn FnMut(Event)) -> Result<Result<String, String>, String> {
        if !self.offers_to_script(name) {
            return Ok(Err(format!("No tool named {name} is offered to this script.")));
        }
        *ran += 1;
        // The script parses what Bash prints, so the command runs as written: rtk turns the `total` row of `wc -l` into `Σ 9`.
        let rtk = std::mem::replace(&mut self.rtk, Rtk::OFF);
        self.in_script = true;
        let call = self.run_call(&format!("{id}-{ran}"), name, input, None, emit);
        self.in_script = false;
        self.rtk = rtk;
        let (content, output, is_error) = call?;
        blocks.extend(content.as_array().into_iter().flatten().filter(|block| block["type"] != "text").cloned());
        Ok(if is_error { Err(output) } else { Ok(output) })
    }

    /// A foreground Bash call that `bash_together` can run beside its neighbours, once Bash is offered. Everything else takes `script_call`:
    /// `cd DIR` moves the session, a call with unknown parameters is refused there, and an editor's terminal runs one command at a time.
    fn runs_beside(&self, (_, name, input): &Call) -> bool {
        name == "Bash"
            && input["run_in_background"] != true
            && self.editor_terminal.is_none()
            && tools::cd_target(input["command"].as_str().unwrap_or_default()).is_none()
            && tools::undeclared_params(&self.tools, name, input).is_none()
    }

    /// Bash calls the script made together. Each one is checked in turn, as `run_tool` checks it, because a check can ask the user;
    /// then the ones let through run at the same time, between one pair of rewind snapshots.
    fn bash_together(&mut self, id: &str, ran: &mut usize, group: &[Call], emit: &mut dyn FnMut(Event)) -> Result<Vec<Result<String, String>>, String> {
        let mut calls = Vec::new();
        for (_, _, input) in group {
            *ran += 1;
            let call = format!("{id}-{ran}");
            let started = self.start_call(&call, "Bash", input, &[], emit);
            let refusal = self.stopped("Bash", input).or_else(|| self.refusal(&call, "Bash", input, &[], emit));
            calls.push((call, input, started, refusal));
        }
        let allowed: Vec<&Value> = calls.iter().filter(|(.., refusal)| refusal.is_none()).map(|(_, input, ..)| *input).collect();
        let mut results = self.checkpointed("Bash", &[], emit, |agent, _| bash_at_once(&allowed, &agent.cwd, &agent.abort))?.into_iter();
        let mut outcomes = Vec::new();
        for (call, input, started, refusal) in calls {
            let outcome = match refusal {
                Some(reason) => Err(reason),
                None => {
                    let hint = conflicts::hint(input["command"].as_str().unwrap_or_default(), &self.cwd).map(|hint| format!("\n\n{hint}")).unwrap_or_default();
                    match results.next().expect("one result for each allowed call") {
                        Ok(output) => Ok(output + &hint),
                        Err(output) => Err(output + &hint),
                    }
                }
            };
            let (Ok(output) | Err(output)) = &outcome;
            self.end_call(&call, "Bash", started, output, outcome.is_err(), emit)?;
            outcomes.push(outcome);
        }
        Ok(outcomes)
    }

    /// A script calls only what the model may call directly, so --tools and a subagent's narrower list hold inside it.
    fn offers_to_script(&self, name: &str) -> bool {
        let (_, offered) = self.system_and_tools();
        name != "Code" && offered.as_array().is_some_and(|tools| tools.iter().any(|tool| tool["name"] == name))
    }
}

/// The results of `inputs` in their order, `BASH_AT_ONCE` commands running at a time and as written, without rtk.
/// shortcut: a chunk waits for its slowest command before the next chunk starts; a work queue once scripts fan out past 8 long commands.
fn bash_at_once(inputs: &[&Value], cwd: &Path, abort: &AtomicBool) -> Vec<Result<String, String>> {
    let mut results = Vec::new();
    for chunk in inputs.chunks(BASH_AT_ONCE) {
        std::thread::scope(|scope| {
            let running: Vec<_> = chunk.iter().map(|input| scope.spawn(move || bash_unless_stopped(input, cwd, abort))).collect();
            results.extend(running.into_iter().map(|command| command.join().unwrap_or_else(|_| Err("The command's thread panicked.".into()))));
        });
    }
    results
}

/// A "No" at a later call's permission prompt stops the turn, so a call allowed before it must not start.
fn bash_unless_stopped(input: &Value, cwd: &Path, abort: &AtomicBool) -> Result<String, String> {
    if abort.load(Ordering::Relaxed) {
        return Err(ABORTED.into());
    }
    tools::bash(input, cwd, &Rtk::OFF, abort, None)
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

    #[test]
    fn dollar_refuses_a_value_that_is_no_argument_and_runs_nothing() {
        let (mut agent, home) = agent("quote");
        let values =
            [("undefined", "undefined"), ("null", "null"), ("{ a: 1 }", "an object"), ("$`echo inner`", "a promise, await it first"), ("() => 1", "a function"), ("['a', undefined]", "undefined")];
        let mut started = 0;
        for (value, named) in values {
            let code = format!("const value = {value}; await $`touch made ${{value}}`; return 'ran';");
            let error = agent.code("c1", &json!({"code": code}), &mut |event| started += usize::from(matches!(event, Event::ToolStart { .. }))).unwrap_err();
            assert!(error.starts_with(&format!("TypeError: $ got {named} where a command argument goes, so nothing ran.")), "{value}: {error}");
        }
        let made = agent.cwd.join("made").exists();
        std::fs::remove_dir_all(&home).unwrap();
        assert_eq!((started, made), (0, false));
    }

    #[cfg(unix)]
    #[test]
    fn bash_calls_made_together_run_at_the_same_time() {
        let (mut agent, home) = agent("together");
        // Each end of a FIFO blocks until the other end opens it, so these two commands end only when both run at once.
        let code = r#"
            await $`mkfifo pipe`;
            const [sent, got, written] = await Promise.all([
                tool("Bash", { command: "echo ping > pipe && echo sent", timeout: 5000 }),
                tool("Bash", { command: "read line < pipe && echo got $line", timeout: 5000 }),
                tool("Write", { file_path: "after.txt", content: "x" }),
            ]);
            return [sent.trim(), got.trim(), written.length > 0];
        "#;
        let mut events = Vec::new();
        let result = agent.code("c1", &json!({"code": code}), &mut |event| match event {
            Event::ToolStart { id, name, .. } => events.push(format!("start {id} {name}")),
            Event::ToolEnd { id, .. } => events.push(format!("end {id}")),
            _ => {}
        });
        let tool_calls = agent.tool_calls;
        std::fs::remove_dir_all(&home).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&result.unwrap()).unwrap(), json!(["sent", "got ping", true]));
        assert_eq!(events, ["start c1-1 Bash", "end c1-1", "start c1-2 Bash", "start c1-3 Bash", "end c1-2", "end c1-3", "start c1-4 Write", "end c1-4"]);
        assert_eq!(tool_calls, 4);
    }

    #[cfg(unix)]
    #[test]
    fn a_call_refused_among_calls_made_together_leaves_the_others_their_results() {
        let (mut agent, home) = agent("refused");
        agent.permission_rules.deny = vec!["Bash(rm *)".into()];
        let kept = run(&mut agent, "return await Promise.all([$`echo a`, $`rm -r x`.nothrow(), $`echo c`]);");
        *agent.permission_mode.lock().unwrap() = Mode::Ask;
        let mut asked = 0;
        let denied = agent.code("c2", &json!({"code": "await Promise.all([$`touch one`, $`touch two`]); return 'done';"}), &mut |event| {
            if let Event::Permission { reply, .. } = event {
                asked += 1;
                reply.send(if asked == 1 { Decision::Allow } else { Decision::Deny }).unwrap();
            }
        });
        let made = ["one", "two"].map(|file| agent.cwd.join(file).exists());
        std::fs::remove_dir_all(&home).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&kept.unwrap()).unwrap(), json!(["a\n", "Denied by the permission rule \"Bash(rm *)\" in settings.json.", "c\n"]));
        assert_eq!(denied.unwrap_err(), ABORTED);
        assert_eq!((asked, made), (2, [false, false]), "the call allowed first must not run once the user denies the next");
    }

    #[cfg(unix)]
    #[test]
    fn auto_mode_reviews_the_script_once_and_then_not_its_calls() {
        let (mut agent, home) = agent("auto");
        *agent.permission_mode.lock().unwrap() = Mode::Auto;
        // This home has no account, so every review fails, and a failed review denies.
        let mut reviewed = Vec::new();
        let script = agent.code("c1", &json!({"code": "await $`touch made`; return 'done';"}), &mut |event| match event {
            Event::Reviewed { detail, allowed, .. } => reviewed.push(format!("reviewed {detail} {allowed}")),
            Event::ToolStart { .. } => reviewed.push("a call started".into()),
            _ => {}
        });
        let made_before = agent.cwd.join("made").exists();
        let each_call = agent.run_script("c2", "return await $`touch made`.nothrow();", TIME_LIMIT, &mut |_| {});
        agent.script_reviewed = true;
        let unreviewed = agent.run_script("c3", "await $`touch made`; return 'done';", TIME_LIMIT, &mut |_| {});
        *agent.permission_mode.lock().unwrap() = Mode::Ask;
        let mut asked = 0;
        let in_ask = agent.run_script("c4", "await $`touch other`; return 'done';", TIME_LIMIT, &mut |event| {
            if let Event::Permission { reply, .. } = event {
                asked += 1;
                reply.send(Decision::Allow).unwrap();
            }
        });
        let made_after = agent.cwd.join("made").exists();
        std::fs::remove_dir_all(&home).unwrap();
        assert!(script.unwrap_err().starts_with("Denied by the auto-mode reviewer: review failed"));
        assert_eq!(reviewed, ["reviewed the script false"]);
        assert!(!made_before);
        assert!(each_call.unwrap().starts_with("Denied by the auto-mode reviewer: review failed"), "a script nobody reviewed keeps the review of each call");
        assert_eq!((unreviewed.unwrap().as_str(), made_after), ("done", true));
        assert_eq!((in_ask.unwrap().as_str(), asked), ("done", 1), "a reviewed script's calls still ask once the user leaves auto mode");
    }
}
