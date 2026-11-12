//! Workflow: a JavaScript script, run in an embedded QuickJS sandbox, that orchestrates subagents in the background.
use rquickjs::{CatchResultExt, Context, Ctx, Function, Runtime};
use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::FollowUp;
use super::subagent::{self, Launch, Run, Runs};

mod journal;
mod schema;

use journal::{Ledger, Recorded, prior_journal};
use schema::{contradiction, parse_json_answer, validate};

const HARNESS: &str = include_str!("workflow-harness.js");
pub const DEEP_RESEARCH: &str = include_str!("deep-research.js");
const MAX_AGENTS_PER_RUN: usize = 1000;
const MAX_CONCURRENT: usize = 16;
const SCHEMA_ATTEMPTS: usize = 5;
/// Past this many agents the task panel warns that the run is large.
pub const LARGE_RUN: usize = 25;

/// One Workflow run, shared with /workflows and the task panel.
pub struct Workflow {
    /// `wf-N`, the task id TaskStop and /workflows use.
    pub id: String,
    /// Names the run's directory; `resumeFromRunId` takes it.
    pub run_id: String,
    pub name: String,
    pub description: String,
    pub script_path: PathBuf,
    pub script: String,
    started: Instant,
    stop: Arc<AtomicBool>,
    /// No new agent starts while paused; running ones finish.
    pub paused: AtomicBool,
    pub state: Mutex<WorkflowState>,
}

#[derive(Default)]
pub struct WorkflowState {
    pub phases: Vec<Phase>,
    pub logs: Vec<String>,
    /// Some once the script returned or threw.
    pub result: Option<Result<String, String>>,
    pub elapsed: Option<Duration>,
}

pub struct Phase {
    pub title: String,
    pub steps: Vec<Step>,
}

pub struct Step {
    pub label: String,
    /// None while queued and for a result replayed from the resumed run.
    pub run: Option<Arc<Run>>,
    pub status: StepStatus,
    pub attempt: usize,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum StepStatus {
    Queued,
    Running,
    Done,
    Cached,
    Failed,
}

impl StepStatus {
    pub fn word(self) -> &'static str {
        match self {
            StepStatus::Queued => "queued",
            StepStatus::Running => "running",
            StepStatus::Done => "done",
            StepStatus::Cached => "cached",
            StepStatus::Failed => "failed",
        }
    }
}

impl Workflow {
    pub fn is_running(&self) -> bool {
        self.state.lock().unwrap().result.is_none()
    }

    pub fn status(&self) -> &'static str {
        match &self.state.lock().unwrap().result {
            None if self.paused.load(Ordering::Relaxed) => "paused",
            None => "running",
            Some(Ok(_)) => "done",
            Some(Err(_)) if self.stop.load(Ordering::Relaxed) => "stopped",
            Some(Err(_)) => "failed",
        }
    }

    pub fn elapsed(&self) -> Duration {
        self.state.lock().unwrap().elapsed.unwrap_or_else(|| self.started.elapsed())
    }

    /// Stops the script and every agent it started; a second stop re-signals the agents still exiting.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        for run in self.runs() {
            run.stop();
        }
    }

    /// The agent processes of this run that have not exited yet.
    pub fn live_agents(&self) -> usize {
        self.runs().iter().filter(|run| run.is_running()).count()
    }

    fn runs(&self) -> Vec<Arc<Run>> {
        let state = self.state.lock().unwrap();
        state.phases.iter().flat_map(|phase| &phase.steps).filter_map(|step| step.run.clone()).collect()
    }

    /// Agents by status over the whole run.
    pub fn counts(&self) -> HashMap<StepStatus, usize> {
        let mut counts = HashMap::new();
        for phase in &self.state.lock().unwrap().phases {
            for step in &phase.steps {
                *counts.entry(step.status).or_insert(0) += 1;
            }
        }
        counts
    }

    /// The progress line after the run's name: status, agents finished of all started, time, and the large-run warning.
    pub fn summary(&self) -> String {
        let counts = self.counts();
        let total: usize = counts.values().sum();
        let finished = counts.get(&StepStatus::Done).unwrap_or(&0) + counts.get(&StepStatus::Cached).unwrap_or(&0);
        let large = if total > LARGE_RUN { " · Large workflow, /workflows can stop it" } else { "" };
        format!("{} · {finished}/{total} agents · {}s{large}", self.status(), self.elapsed().as_secs())
    }
}

/// What a workflow's agents run with; each `agent()` call overrides label, task, model, thinking and explore.
pub struct Host {
    pub runs: Arc<Mutex<Runs>>,
    pub follow_ups: Arc<Mutex<Vec<FollowUp>>>,
    pub base: Launch,
    /// Catalog model ids with the `provider=name` account each runs on.
    pub models: Vec<(String, Option<String>)>,
}

/// The `export const meta = { ... }` block a script starts with.
#[derive(Debug, PartialEq)]
pub struct Meta {
    pub name: String,
    pub description: String,
    pub phases: Vec<String>,
}

/// A saved or bundled workflow, which runs as `/<name>`.
pub struct Saved {
    pub meta: Meta,
    pub script: String,
    /// None for a bundled workflow.
    pub path: Option<PathBuf>,
}

/// Workflows by name: project `.agents/workflows/` closest first, then `~/.agents/workflows/`, then the bundled ones.
/// The first of a name wins, so a project workflow shadows a personal one.
pub fn saved(project_agents_dirs: &[PathBuf], agent_dir: &Path) -> Vec<Saved> {
    let mut found: Vec<Saved> = Vec::new();
    for dir in project_agents_dirs.iter().chain([&agent_dir.to_path_buf()]) {
        let Ok(entries) = std::fs::read_dir(dir.join("workflows")) else { continue };
        let mut paths: Vec<PathBuf> = entries.filter_map(|entry| Some(entry.ok()?.path())).filter(|path| path.extension().is_some_and(|ext| ext == "js")).collect();
        paths.sort();
        for path in paths {
            let Ok(script) = std::fs::read_to_string(&path) else { continue };
            // A script whose meta block does not evaluate is left out of `/` commands.
            let Ok(meta) = parse_meta(&script) else { continue };
            if !found.iter().any(|saved| saved.meta.name == meta.name) {
                found.push(Saved { meta, script, path: Some(path) });
            }
        }
    }
    let bundled = Saved { meta: parse_meta(DEEP_RESEARCH).expect("the bundled workflow has a meta block"), script: DEEP_RESEARCH.into(), path: None };
    if !found.iter().any(|saved| saved.meta.name == bundled.meta.name) {
        found.push(bundled);
    }
    found
}

/// `/<workflow> [input]`: the model runs the saved workflow and shapes the input into the script's `args`.
pub fn command_prompt(name: &str, input: &str) -> String {
    let call = format!("Run the saved workflow `{name}` now: call the Workflow tool with name \"{name}\".");
    if input.trim().is_empty() {
        return call;
    }
    format!("{call} Pass this input as `args`, as structured data where it is a list or has fields:\n\n{}", input.trim())
}

/// Reads `meta` without running the script: the object literal after `export const meta =`, evaluated alone.
pub fn parse_meta(script: &str) -> Result<Meta, String> {
    let missing = "The script must begin with `export const meta = { name, description }` as a literal.";
    let rest = script.trim_start().strip_prefix("export const meta").ok_or(missing)?;
    let literal = rest.trim_start().strip_prefix('=').ok_or(missing)?.trim_start();
    let literal = &literal[..object_literal_len(literal).ok_or(missing)?];
    let json = with_context(|ctx| ctx.eval::<Option<String>, _>(format!("JSON.stringify(({literal}))")).catch(&ctx).map_err(|e| format!("Bad `meta` block: {e}")))?;
    let meta: Value = serde_json::from_str(&json.unwrap_or_default()).map_err(|_| "Bad `meta` block.".to_string())?;
    let field = |key: &str| meta[key].as_str().map(str::trim).filter(|value| !value.is_empty()).map(String::from);
    Ok(Meta {
        name: field("name").ok_or("`meta` needs a `name`.")?,
        description: field("description").ok_or("`meta` needs a `description`.")?,
        phases: meta["phases"].as_array().into_iter().flatten().filter_map(|phase| phase.as_str().or(phase["title"].as_str())).map(String::from).collect(),
    })
}

/// Length of the `{ ... }` at the start of `text`, skipping braces inside strings and comments.
fn object_literal_len(text: &str) -> Option<usize> {
    if !text.starts_with('{') {
        return None;
    }
    let bytes = text.as_bytes();
    let (mut depth, mut i) = (0usize, 0);
    while i < bytes.len() {
        match bytes[i] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            quote @ (b'"' | b'\'' | b'`') => {
                i += 1;
                while i < bytes.len() && bytes[i] != quote {
                    i += if bytes[i] == b'\\' { 2 } else { 1 };
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => i = text[i..].find('\n').map_or(bytes.len(), |end| i + end),
            b'/' if bytes.get(i + 1) == Some(&b'*') => i = text[i + 2..].find("*/").map_or(bytes.len(), |end| i + 2 + end + 1),
            _ => {}
        }
        i += 1;
    }
    None
}

fn with_context<T>(f: impl FnOnce(Ctx) -> Result<T, String>) -> Result<T, String> {
    let runtime = Runtime::new().map_err(|e| format!("Cannot start the JavaScript runtime: {e}"))?;
    // A meta block like `{ name: (() => { while (true); })() }` would otherwise hang the caller.
    let deadline = Instant::now() + Duration::from_secs(1);
    runtime.set_interrupt_handler(Some(Box::new(move || Instant::now() > deadline)));
    let context = Context::full(&runtime).map_err(|e| format!("Cannot start the JavaScript runtime: {e}"))?;
    context.with(f)
}

/// The script as one async function, so its body can `await` and `return`; lines keep their numbers.
fn wrapped(script: &str) -> String {
    let body = script.trim_start().replacen("export const meta", "const meta", 1);
    format!("(async function () {{{body}\n}})")
}

/// Rejects what cannot run before anything starts: no meta, module loading, a syntax error.
pub fn check(script: &str) -> Result<Meta, String> {
    let meta = parse_meta(script)?;
    if script.contains("import(") || script.lines().any(|line| line.trim_start().starts_with("import ")) {
        return Err("Workflow scripts cannot load modules: the body is plain JavaScript. Put work that needs a library in an agent's task.".into());
    }
    with_context(|ctx| ctx.eval::<(), _>(wrapped(script)).catch(&ctx).map_err(|e| format!("Syntax error in the workflow script: {e}")))?;
    Ok(meta)
}

/// Starts `script` on its own thread as `wf-N` and returns at once; its result arrives as a follow-up.
/// `runs_dir` holds one directory per run, with the script and the journal a later run resumes from.
pub fn launch(workflows: &Arc<Mutex<Vec<Arc<Workflow>>>>, script: String, args: Value, resume_from: Option<&str>, runs_dir: &Path, host: Host) -> Result<Arc<Workflow>, String> {
    let meta = check(&script)?;
    let prior = match resume_from {
        Some(run_id) => Some(prior_journal(workflows, runs_dir, run_id)?),
        None => None,
    };
    let workflow = {
        let mut list = workflows.lock().unwrap();
        let run_id = format!("{}-{}", meta.name.replace(|c: char| !c.is_ascii_alphanumeric() && c != '-', "-"), unique_suffix());
        let dir = runs_dir.join(&run_id);
        std::fs::create_dir_all(&dir).map_err(|e| format!("Cannot create {}: {e}", dir.display()))?;
        let script_path = dir.join("script.js");
        std::fs::write(&script_path, &script).map_err(|e| format!("Cannot write {}: {e}", script_path.display()))?;
        let state = WorkflowState { phases: meta.phases.iter().map(|title| Phase { title: title.clone(), steps: Vec::new() }).collect(), ..Default::default() };
        let workflow = Arc::new(Workflow {
            id: format!("wf-{}", list.len() + 1),
            run_id,
            name: meta.name,
            description: meta.description,
            script_path,
            script,
            started: Instant::now(),
            stop: Arc::default(),
            paused: AtomicBool::new(false),
            state: Mutex::new(state),
        });
        list.push(workflow.clone());
        workflow
    };
    let watched = workflow.clone();
    std::thread::spawn(move || {
        let result = execute(&watched, &args, prior.unwrap_or_default(), &host);
        // The script can end, fail or be stopped with agents still running; nothing would supervise them.
        for run in watched.runs() {
            run.stop();
        }
        let mut queued = host.follow_ups.lock().unwrap();
        let text = match &result {
            Ok(answer) => format!("Workflow {} ({}) finished.\n\n{answer}", watched.id, watched.name),
            Err(error) => format!(
                "Workflow {} ({}) failed: {error}\n\nRelaunch with resumeFromRunId {} to reuse its finished agents; the script is at {}.",
                watched.id,
                watched.name,
                watched.run_id,
                watched.script_path.display()
            ),
        };
        {
            let mut state = watched.state.lock().unwrap();
            state.elapsed = Some(watched.started.elapsed());
            state.result = Some(result);
        }
        queued.push(FollowUp { source: watched.id.clone(), text });
    });
    Ok(workflow)
}

fn unique_suffix() -> String {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
    format!("{:x}", nanos % (1 << 40))
}

/// The directory of an earlier run; the run id comes from the model, so it must not climb out of `runs_dir`.
pub fn run_dir(runs_dir: &Path, run_id: &str) -> Result<PathBuf, String> {
    if run_id.is_empty() || !run_id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(format!("Bad run id {run_id}."));
    }
    Ok(runs_dir.join(run_id))
}

/// Writes a run's script as `<name>.js` in `dir`, which then serves it as `/<name>`.
/// The name comes from the script's `meta`; a project save also refuses a symlinked `.agents` or `workflows`.
pub fn save(script: &str, name: &str, dir: &Path, project: bool) -> Result<PathBuf, String> {
    if name.is_empty() || name.starts_with('.') || !name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')) {
        return Err(format!("Cannot save a workflow named {name:?}: its name may hold only letters, digits, '-', '_' and '.'."));
    }
    let path = dir.join(format!("{name}.js"));
    let mut guarded = vec![path.clone()];
    if project {
        guarded.extend([dir.to_path_buf(), dir.parent().unwrap_or(dir).to_path_buf()]);
    }
    if let Some(link) = guarded.iter().find(|path| path.symlink_metadata().is_ok_and(|meta| meta.file_type().is_symlink())) {
        return Err(format!("{} is a symlink, so the workflow was not saved through it.", link.display()));
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("Cannot create {}: {e}", dir.display()))?;
    std::fs::write(&path, script).map_err(|e| format!("Cannot write {}: {e}", path.display()))?;
    Ok(path)
}

/// The closest `.agents/workflows` that exists between `cwd` and the project root, else the root's.
pub fn project_save_dir(cwd: &Path) -> PathBuf {
    let root = super::prompt::project_root(cwd);
    let cwd = crate::process::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let mut dirs = cwd.ancestors().take_while(|dir| dir.starts_with(&root)).map(|dir| dir.join(".agents").join("workflows"));
    dirs.find(|dir| dir.is_dir()).unwrap_or_else(|| root.join(".agents").join("workflows"))
}

/// One `agent()` call from the script.
struct Call {
    id: u32,
    prompt: String,
    options: Value,
    phase: String,
}

/// A call waiting for or running its agent: its promise id, its step in `phases`, its journal key.
struct Live {
    id: u32,
    phase: usize,
    step: usize,
    key: (String, usize),
    launch: Launch,
    schema: Option<Value>,
    agent: Agent,
    attempts: usize,
}

enum Agent {
    Queued,
    Running(Arc<Run>),
    /// Could not start (the global subagent limit, a missing binary), which resolves to null like an API error.
    NotStarted,
}

#[derive(Default)]
struct Inbox {
    calls: Vec<Call>,
    outcome: Option<Result<String, String>>,
}

/// Runs the script to its end; the loop starts queued agents, polls running ones and settles their promises.
fn execute(workflow: &Arc<Workflow>, args: &Value, prior: HashMap<(String, usize), Recorded>, host: &Host) -> Result<String, String> {
    let js = |e: rquickjs::Error| format!("JavaScript runtime: {e}");
    let runtime = Runtime::new().map_err(js)?;
    runtime.set_memory_limit(512 << 20);
    let stop = workflow.stop.clone();
    runtime.set_interrupt_handler(Some(Box::new(move || stop.load(Ordering::Relaxed))));
    let context = Context::full(&runtime).map_err(js)?;
    let inbox = Rc::new(RefCell::new(Inbox::default()));
    context.with(|ctx| install(&ctx, workflow, &inbox, args))?;
    let mut ledger = Ledger::open(&workflow.script_path.with_file_name("journal.jsonl"), prior)?;
    let mut accepted = 0;
    let mut live: Vec<Live> = Vec::new();
    let limit = std::thread::available_parallelism().map_or(MAX_CONCURRENT, |cpus| cpus.get().min(MAX_CONCURRENT));
    loop {
        let mut progressed = context.with(|ctx| {
            let mut ran = false;
            while ctx.execute_pending_job() {
                ran = true;
            }
            ran
        });
        if workflow.stop.load(Ordering::Relaxed) {
            return Err("Stopped.".into());
        }
        if let Some(outcome) = inbox.borrow_mut().outcome.take() {
            return outcome;
        }
        let calls = std::mem::take(&mut inbox.borrow_mut().calls);
        for call in calls {
            progressed = true;
            accepted += 1;
            if let Some((id, outcome)) = accept(workflow, call, accepted, host, &mut ledger, &mut live) {
                context.with(|ctx| settle(&ctx, id, outcome))?;
            }
        }
        let running = live.iter().filter(|call| matches!(call.agent, Agent::Running(_))).count();
        if !workflow.paused.load(Ordering::Relaxed) {
            for call in live.iter_mut().filter(|call| matches!(call.agent, Agent::Queued)).take(limit.saturating_sub(running)) {
                start(workflow, call, host);
                progressed = true;
            }
        }
        let mut index = 0;
        while index < live.len() {
            let Some(outcome) = poll(workflow, &mut live[index], host) else {
                index += 1;
                continue;
            };
            let call = live.remove(index);
            ledger.finished(&call.key, &outcome, workflow.stop.load(Ordering::Relaxed));
            let status = if matches!(outcome, Ok(Value::Null) | Err(_)) { StepStatus::Failed } else { StepStatus::Done };
            set_status(workflow, call.phase, call.step, status);
            context.with(|ctx| settle(&ctx, call.id, outcome))?;
            progressed = true;
        }
        if !progressed && live.is_empty() {
            return Err("The script is awaiting something that never settles: no agent is running.".into());
        }
        if !progressed {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// Globals the script sees: the harness, `args`, and the native hooks it calls.
fn install(ctx: &Ctx, workflow: &Arc<Workflow>, inbox: &Rc<RefCell<Inbox>>, args: &Value) -> Result<(), String> {
    let js = |e: rquickjs::Error| format!("JavaScript runtime: {e}");
    let globals = ctx.globals();
    let calls = inbox.clone();
    let request = move |id: u32, prompt: String, options: String, phase: String| {
        let options = serde_json::from_str(&options).unwrap_or(Value::Null);
        calls.borrow_mut().calls.push(Call { id, prompt, options, phase });
    };
    globals.set("__request", Function::new(ctx.clone(), request).map_err(js)?).map_err(js)?;
    let logged = workflow.clone();
    globals.set("__log", Function::new(ctx.clone(), move |text: String| logged.state.lock().unwrap().logs.push(text)).map_err(js)?).map_err(js)?;
    let phased = workflow.clone();
    globals.set("__phase", Function::new(ctx.clone(), move |title: String| phase_index(&phased, &title)).map_err(js)?).map_err(js)?;
    let finished = inbox.clone();
    let finish = move |ok: bool, text: String| finished.borrow_mut().outcome = Some(if ok { Ok(present(&text)) } else { Err(text) });
    globals.set("__finish", Function::new(ctx.clone(), finish).map_err(js)?).map_err(js)?;
    globals.set("__args", if args.is_null() { None } else { Some(args.to_string()) }).map_err(js)?;
    ctx.eval::<(), _>(HARNESS).catch(ctx).map_err(|e| format!("Workflow harness: {e}"))?;
    ctx.eval::<(), _>("globalThis.args = __args == null ? undefined : JSON.parse(__args);").catch(ctx).map_err(|e| format!("Workflow args: {e}"))?;
    let main = format!(
        "{}().then((result) => __finish(true, JSON.stringify(result === undefined ? null : result)), (error) => __finish(false, error && error.stack ? `${{error}}\\n${{error.stack}}` : String(error)));",
        wrapped(&workflow.script)
    );
    ctx.eval::<(), _>(main).catch(ctx).map_err(|e| format!("The workflow script failed: {e}"))
}

/// The script's return value as the model reads it: a string as is, anything else as JSON.
fn present(json: &str) -> String {
    match serde_json::from_str::<Value>(json) {
        Ok(Value::String(text)) => text,
        Ok(value) => serde_json::to_string_pretty(&value).unwrap_or_default(),
        Err(_) => json.to_string(),
    }
}

fn phase_index(workflow: &Workflow, title: &str) -> usize {
    let mut state = workflow.state.lock().unwrap();
    if let Some(index) = state.phases.iter().position(|phase| phase.title == title) {
        return index;
    }
    state.phases.push(Phase { title: title.to_string(), steps: Vec::new() });
    state.phases.len() - 1
}

fn set_status(workflow: &Workflow, phase: usize, step: usize, status: StepStatus) {
    workflow.state.lock().unwrap().phases[phase].steps[step].status = status;
}

/// Takes the script's `accepted`-th call: rejects it, replays its result from the earlier run, or queues it in `live`.
/// Returns the settlement when there is one now.
fn accept(workflow: &Workflow, call: Call, accepted: usize, host: &Host, ledger: &mut Ledger, live: &mut Vec<Live>) -> Option<(u32, Result<Value, String>)> {
    if accepted > MAX_AGENTS_PER_RUN {
        return Some((call.id, Err(format!("This run reached {MAX_AGENTS_PER_RUN} agents, the limit per run."))));
    }
    let schema = call.options.get("schema").filter(|schema| schema.is_object()).cloned();
    if let Some(error) = schema.as_ref().and_then(contradiction) {
        return Some((call.id, Err(format!("The schema contradicts itself: {error}"))));
    }
    let launch = match agent_launch(&call, host, schema.as_ref()) {
        Ok(launch) => launch,
        Err(error) => return Some((call.id, Err(error))),
    };
    let key = ledger.key(&call);
    let phase = phase_index(workflow, &call.phase);
    let step = {
        let mut state = workflow.state.lock().unwrap();
        let steps = &mut state.phases[phase].steps;
        steps.push(Step { label: launch.label.clone(), run: None, status: StepStatus::Queued, attempt: 1 });
        steps.len() - 1
    };
    ledger.write(&key, "start", None);
    if let Some(result) = ledger.replay(&key) {
        set_status(workflow, phase, step, StepStatus::Cached);
        ledger.write(&key, "done", Some(&result));
        return Some((call.id, Ok(result)));
    }
    live.push(Live { id: call.id, phase, step, key, launch, schema, agent: Agent::Queued, attempts: 0 });
    None
}

/// The Launch for one `agent(prompt, options)`: options `label`, `model`, `effort` and `explore`, and `schema` added to the task.
fn agent_launch(call: &Call, host: &Host, schema: Option<&Value>) -> Result<Launch, String> {
    let options = &call.options;
    let mut launch = host.base.clone();
    launch.label = options["label"].as_str().map(String::from).unwrap_or_else(|| call.prompt.split_whitespace().take(6).collect::<Vec<_>>().join(" "));
    launch.task = match schema {
        Some(schema) => format!("{}\n\nWhen you finish, reply with only a JSON value matching this JSON Schema, no prose and no code fence:\n{schema}", call.prompt),
        None => call.prompt.clone(),
    };
    if let Some(wanted) = options["model"].as_str() {
        let lower = wanted.to_lowercase();
        let exact = host.models.iter().find(|(id, _)| id == wanted);
        let (model, account) = exact.or_else(|| host.models.iter().find(|(id, _)| id.to_lowercase().contains(&lower))).ok_or_else(|| format!("Unknown model {wanted}."))?;
        launch.model = model.clone();
        launch.account = account.clone();
    }
    if let Some(effort) = options["effort"].as_str() {
        launch.thinking = effort.to_string();
    }
    launch.explore = options["explore"] == true;
    Ok(launch)
}

fn start(workflow: &Workflow, call: &mut Live, host: &Host) {
    call.attempts = 1;
    match subagent::spawn(&host.runs, call.launch.clone(), false, Some(workflow.id.clone()), &host.follow_ups) {
        Ok(run) => {
            let mut state = workflow.state.lock().unwrap();
            let step = &mut state.phases[call.phase].steps[call.step];
            step.run = Some(run.clone());
            step.status = StepStatus::Running;
            call.agent = Agent::Running(run);
        }
        Err(error) => {
            workflow.state.lock().unwrap().logs.push(format!("{}: {error}", call.launch.label));
            call.agent = Agent::NotStarted;
        }
    }
}

/// None while the agent runs; then the value `agent()` resolves to, or the error it rejects with.
fn poll(workflow: &Workflow, call: &mut Live, host: &Host) -> Option<Result<Value, String>> {
    let run = match &call.agent {
        Agent::Queued => return None,
        Agent::NotStarted => return Some(Ok(Value::Null)),
        Agent::Running(run) => run.clone(),
    };
    let answer = match run.state.lock().unwrap().result.clone()? {
        Ok(answer) => answer,
        // A stopped agent or an unrecoverable API error resolves to null.
        Err(_) => return Some(Ok(Value::Null)),
    };
    let Some(schema) = &call.schema else { return Some(Ok(Value::String(answer))) };
    let error = match parse_json_answer(&answer).and_then(|value| validate(&value, schema, "$").map(|()| value)) {
        Ok(value) => return Some(Ok(value)),
        Err(error) => error,
    };
    if call.attempts >= SCHEMA_ATTEMPTS {
        return Some(Err(format!("The agent's output failed the schema after {SCHEMA_ATTEMPTS} attempts: {error}")));
    }
    call.attempts += 1;
    workflow.state.lock().unwrap().phases[call.phase].steps[call.step].attempt = call.attempts;
    let retry = format!("Your answer did not match the schema: {error}. Reply again with only the JSON value, no prose and no code fence.");
    match subagent::resume(&run, &retry, None, &host.runs, &host.follow_ups) {
        Ok(()) => None,
        Err(error) => Some(Err(format!("Cannot retry the agent for its schema: {error}"))),
    }
}

fn settle(ctx: &Ctx, id: u32, outcome: Result<Value, String>) -> Result<(), String> {
    let (ok, text) = match outcome {
        Ok(value) => (true, value.to_string()),
        Err(error) => (false, error),
    };
    let settle: Function = ctx.globals().get("__settle").map_err(|e| format!("JavaScript runtime: {e}"))?;
    settle.call::<_, ()>((id, ok, text)).catch(ctx).map_err(|e| format!("JavaScript runtime: {e}"))
}

#[cfg(test)]
mod tests;
