use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::permissions::Mode;
use super::{FollowUp, prompt, usage};
use crate::api::Usage;

const DEFAULT_SWARM_PROMPT: &str = include_str!("swarm-prompt.md");
const MAX_RUNNING: usize = 64;
/// Sent ahead of every task, since the child's system prompt is the root's.
const CHILD_BRIEF: &str = "You are a subagent. Another agent spawned you for the task below and reads only your final answer. \
Nobody answers questions here: finish the task on your own, making the reasonable call where it is unclear. \
End with a short report: what you found or changed with `path:line` references, how you checked it, and what you did not do and why.";

/// jcode's swarm prompt lookup: `.agents/swarm-prompt.md` up to the git root, then `~/.agents/swarm-prompt.md`, then the built-in.
pub fn swarm_prompt(agent_dir: &Path, cwd: &Path, trusted: bool, implementation_effort: &str) -> String {
    prompt::project_agents_dirs(cwd, trusted)
        .iter()
        .chain([&agent_dir.to_path_buf()])
        .filter_map(|dir| prompt::read_text(&dir.join("swarm-prompt.md")))
        .map(|text| without_leading_comment(&text).to_string())
        .find(|text| !text.is_empty())
        .unwrap_or_else(|| without_leading_comment(DEFAULT_SWARM_PROMPT).to_string())
        .replace("{implementation_effort}", implementation_effort)
}

/// The `<!-- ... -->` note on top of a swarm prompt is for whoever edits the file, not for the model.
fn without_leading_comment(text: &str) -> &str {
    let text = text.trim();
    match text.strip_prefix("<!--").and_then(|rest| rest.split_once("-->")) {
        Some((_, after)) => after.trim(),
        None => text,
    }
}

/// One child process, shared with /tasks.
pub struct Run {
    pub id: String,
    pub label: String,
    pub task: String,
    pub model: String,
    pub thinking: String,
    pub background: bool,
    /// The Workflow run this agent belongs to; its answer goes to the script, not to the model.
    pub workflow: Option<String>,
    started: Instant,
    child: Mutex<Option<Child>>,
    /// One JSON string per line: the task, then what the user types to the run.
    stdin: Mutex<Option<ChildStdin>>,
    pub state: Mutex<RunState>,
    launch: Launch,
    /// SendMessage resumed the finished run, so its next answer arrives as a message.
    resumed: AtomicBool,
    /// The subagent whose SendMessage resumed this run; the answer goes to it instead of to main.
    reply_to: Mutex<Option<String>>,
}

#[derive(Default)]
pub struct RunState {
    pub tool_calls: u64,
    pub last_tool: String,
    /// Some once the child exited: its last answer, or the error.
    pub result: Option<Result<String, String>>,
    pub session: Option<String>,
    pub elapsed: Option<Duration>,
    /// The child's tokens, known once it reports them at the end.
    pub usage: Usage,
}

impl Run {
    pub fn status(&self) -> &'static str {
        match &self.state.lock().unwrap().result {
            None => "running",
            Some(Ok(_)) => "done",
            Some(Err(_)) => "failed",
        }
    }

    pub fn is_running(&self) -> bool {
        self.state.lock().unwrap().result.is_none()
    }

    pub fn elapsed(&self) -> Duration {
        self.state.lock().unwrap().elapsed.unwrap_or_else(|| self.started.elapsed())
    }

    pub fn stop(&self) {
        // SIGTERM, not SIGKILL: the child's handler then ends its own bash commands, terminals and servers.
        self.signal(crate::process::SIGTERM);
    }

    /// Steers the running child: it reads the text after its current tool batch or at turn end.
    pub fn message(&self, text: &str) -> Result<(), String> {
        let mut stdin = self.stdin.lock().unwrap();
        let stdin = stdin.as_mut().ok_or(format!("{} has finished.", self.id))?;
        writeln!(stdin, "{}", Value::from(text)).map_err(|e| format!("Cannot message {}: {e}", self.id))
    }

    fn signal(&self, signal: i32) {
        if let Some(child) = self.child.lock().unwrap().as_mut() {
            crate::process::signal_group(child.id(), signal);
        }
    }

    /// The row /tasks and the transcript show: label, last tool, call count, time, tokens.
    pub fn summary(&self) -> String {
        let (status, elapsed) = (self.status(), self.elapsed().as_secs());
        let state = self.state.lock().unwrap();
        let tool = if state.last_tool.is_empty() { String::new() } else { format!(", last {}", state.last_tool) };
        let tokens = if state.usage == Usage::default() { String::new() } else { format!(", {}", usage::brief(state.usage)) };
        format!("{} {} {status}: {} tool calls{tool}, {elapsed}s{tokens}", self.id, self.label, state.tool_calls)
    }

    pub fn result_text(&self) -> String {
        let state = self.state.lock().unwrap();
        match &state.result {
            Some(Ok(answer)) => answer.clone(),
            Some(Err(error)) => format!("Error: {error}"),
            None => "Error: still running".into(),
        }
    }
}

/// What a child needs from its parent.
#[derive(Clone)]
pub struct Launch {
    pub label: String,
    pub task: String,
    pub cwd: PathBuf,
    pub model: String,
    pub thinking: String,
    pub mode: Mode,
    pub parent_session: String,
    /// Only read and rg, no modes prompt.
    pub explore: bool,
    /// `provider=name` of the account the parent uses for the child's model.
    pub account: Option<String>,
    /// The parent's --add-dir list, absolute.
    pub extra_dirs: Vec<PathBuf>,
    /// The parent trusts the project, so the child loads its files too.
    pub trusted: bool,
    /// Keeps WebSearch, WebFetch and GetSearchContent, which Workflow agents get.
    pub web: bool,
    /// The editor's MCP servers of an `agent acp` parent; null when it passed no list.
    pub client_mcp: Value,
}

#[derive(Default)]
pub struct Runs {
    pub list: Vec<Arc<Run>>,
}

impl Runs {
    /// By id, or the newest run with that label.
    pub fn named(&self, name: &str) -> Option<Arc<Run>> {
        let by_id = self.list.iter().find(|run| run.id == name);
        by_id.or_else(|| self.list.iter().filter(|run| run.label == name).max_by_key(|run| run.started)).cloned()
    }

    /// Children belong to the session, so quitting stops them.
    pub fn stop_all(&self) {
        for run in &self.list {
            run.stop();
        }
    }

    pub fn running(&self) -> usize {
        self.list.iter().filter(|run| run.is_running()).count()
    }

    /// Running runs first, each group newest first.
    pub fn running_first(&mut self) {
        self.list.sort_by_key(|run| (!run.is_running(), std::cmp::Reverse(run.started)));
    }

    /// The SendMessage addresses a new child gets in its brief.
    fn roster(&self) -> String {
        let mut names = vec!["`main` (the agent that spawned you)".to_string()];
        names.extend(self.list.iter().filter(|run| run.workflow.is_none()).map(|run| format!("`{}` ({})", run.id, run.label)));
        format!("Agents you can reach with SendMessage: {}.\n\n", names.join(", "))
    }
}

/// Starts a child and a thread reading its JSONL; a background run queues its answer as a follow-up.
pub fn spawn(runs: &Arc<Mutex<Runs>>, launch: Launch, background: bool, workflow: Option<String>, follow_ups: &Arc<Mutex<Vec<FollowUp>>>) -> Result<Arc<Run>, String> {
    let (run, roster) = {
        let mut runs = runs.lock().unwrap();
        if runs.running() >= MAX_RUNNING {
            return Err(format!("{MAX_RUNNING} subagents are already running, the limit. Wait for one to finish."));
        }
        let roster = runs.roster();
        let run = Arc::new(Run {
            id: format!("sa-{}", runs.list.len() + 1),
            label: launch.label.clone(),
            task: launch.task.clone(),
            model: launch.model.clone(),
            thinking: launch.thinking.clone(),
            background,
            workflow,
            started: Instant::now(),
            child: Mutex::default(),
            stdin: Mutex::default(),
            state: Mutex::default(),
            launch,
            resumed: AtomicBool::new(false),
            reply_to: Mutex::default(),
        });
        // Listed before the child starts, so a parallel spawn cannot take the same id.
        runs.list.push(run.clone());
        (run, roster)
    };
    let message = format!("{CHILD_BRIEF}\n\n{roster}<task>\n{}\n</task>", run.task);
    if let Err(error) = start(&run, &message, None, runs, follow_ups) {
        runs.lock().unwrap().list.retain(|listed| !Arc::ptr_eq(listed, &run));
        return Err(error);
    }
    Ok(run)
}

/// SendMessage to a subagent: steers it while it runs, resumes it in the background once it finished.
/// `from` is the subagent that sent it, which then gets the resumed run's answer; None is main.
pub fn send(runs: &Arc<Mutex<Runs>>, to: &str, text: &str, from: Option<&str>, follow_ups: &Arc<Mutex<Vec<FollowUp>>>) -> Result<String, String> {
    let target = runs.lock().unwrap().named(to).ok_or_else(|| format!("No agent named {to}. ListAgents lists the agents you can message."))?;
    if target.workflow.is_some() {
        return Err(format!(
            "{} belongs to workflow {}, whose script takes its answer; message the workflow's agents through the script instead.",
            target.id,
            target.workflow.as_deref().unwrap_or_default()
        ));
    }
    let signed = format!("Message from {}, another agent, not the user:\n\n{text}", from.unwrap_or("main"));
    if target.is_running() && target.message(&signed).is_ok() {
        return Ok(format!("Delivered to {} ({}); it reads the message after its current tool batch.", target.id, target.label));
    }
    resume(&target, &signed, from, runs, follow_ups)?;
    Ok(format!("{} ({}) resumed in the background with your message; its answer arrives as a message.", target.id, target.label))
}

/// Restarts a finished child on its own session, so it keeps its whole conversation.
pub fn resume(run: &Arc<Run>, text: &str, reply_to: Option<&str>, runs: &Arc<Mutex<Runs>>, follow_ups: &Arc<Mutex<Vec<FollowUp>>>) -> Result<(), String> {
    if run.child.lock().unwrap().is_some() {
        return Err(format!("{} was stopped and has not exited yet. Send again in a moment.", run.id));
    }
    let session = run.state.lock().unwrap().session.clone().ok_or_else(|| format!("{} kept no session, so it cannot be resumed.", run.id))?;
    let session = Path::new(&session).file_stem().unwrap_or_default().to_string_lossy().into_owned();
    {
        let mut state = run.state.lock().unwrap();
        state.result = None;
        state.elapsed = None;
    }
    run.resumed.store(true, Ordering::Relaxed);
    *run.reply_to.lock().unwrap() = reply_to.map(String::from);
    if let Err(error) = start(run, text, Some(&session), runs, follow_ups) {
        run.state.lock().unwrap().result = Some(Err(error.clone()));
        return Err(error);
    }
    Ok(())
}

/// Runs the child process for `run`, sends `message` as its task, and follows its JSONL on a thread.
fn start(run: &Arc<Run>, message: &str, resume: Option<&str>, runs: &Arc<Mutex<Runs>>, follow_ups: &Arc<Mutex<Vec<FollowUp>>>) -> Result<(), String> {
    let launch = &run.launch;
    let exe = std::env::current_exe().map_err(|e| format!("Cannot find the agent binary: {e}"))?;
    // A child has no UI to ask in, so ask becomes auto.
    let mode = match launch.mode {
        Mode::FullAccess => "--full-access",
        Mode::Ask | Mode::Auto => "--auto",
    };
    let mut args = vec!["--headless", "--model", &launch.model, "--thinking", &launch.thinking, mode, "--parent", &launch.parent_session];
    if let Some(account) = &launch.account {
        args.extend(["--account", account]);
    }
    if launch.explore {
        args.push("--explore");
    }
    if launch.trusted {
        args.push("--trust");
    }
    if launch.web {
        args.push("--web");
    }
    if let Some(session) = resume {
        args.extend(["--resume", session]);
    }
    let mut command = crate::process::command(exe);
    if !launch.client_mcp.is_null() {
        command.env(crate::mcp::CLIENT_SERVERS_ENV, launch.client_mcp.to_string());
    }
    let mut child = command
        .args(args)
        .args(launch.extra_dirs.iter().flat_map(|dir| [std::ffi::OsStr::new("--add-dir"), dir.as_os_str()]))
        .current_dir(&launch.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("Cannot start subagent: {e}"))?;
    let mut stdin = child.stdin.take().expect("stdin is piped");
    if let Err(e) = writeln!(stdin, "{}", Value::from(message)) {
        // The child leads its own process group, out of reach of stop_all once dropped.
        crate::process::signal_group(child.id(), crate::process::SIGTERM);
        let _ = child.wait();
        return Err(format!("Cannot send the task: {e}"));
    }
    let stdout = child.stdout.take().expect("stdout is piped");
    *run.child.lock().unwrap() = Some(child);
    *run.stdin.lock().unwrap() = Some(stdin);
    let (watched, runs, follow_ups) = (run.clone(), runs.clone(), follow_ups.clone());
    std::thread::spawn(move || {
        let result = read_events(BufReader::new(stdout), &watched, &mut |to, text| route(&watched, to, text, &runs, &follow_ups));
        // Taken out first: holding the lock through `wait` would block `Run::stop` until the child exits.
        let child = watched.child.lock().unwrap().take();
        if let Some(mut child) = child {
            let _ = child.wait();
        }
        finish(&watched, result, &follow_ups);
        if let Some(sender) = watched.reply_to.lock().unwrap().take() {
            let answer = format!("Answer from {} ({}) to your message:\n\n{}", watched.id, watched.label, watched.result_text());
            let delivered = runs.lock().unwrap().named(&sender).filter(|run| run.is_running()).is_some_and(|run| run.message(&answer).is_ok());
            if !delivered {
                follow_ups.lock().unwrap().push(FollowUp { source: format!("{}:message", watched.id), text: answer });
            }
        }
    });
    Ok(())
}

/// A child's SendMessage: `main` gets it as a message, a sibling through `send`.
fn route(from: &Run, to: &str, text: &str, runs: &Arc<Mutex<Runs>>, follow_ups: &Arc<Mutex<Vec<FollowUp>>>) {
    // Not the run's own id: WaitForAgents on that run withdraws its follow-ups.
    let source = format!("{}:message", from.id);
    if to == "main" {
        let text = format!("Message from subagent {} ({}), another agent, not the user:\n\n{text}", from.id, from.label);
        follow_ups.lock().unwrap().push(FollowUp { source, text });
        return;
    }
    if let Err(error) = send(runs, to, text, Some(&from.id), follow_ups) {
        let text = format!("Subagent {} ({}) could not message {to}: {error}", from.id, from.label);
        follow_ups.lock().unwrap().push(FollowUp { source, text });
    }
}

/// Follows the child's JSONL into `run` until stdout closes; returns its answer or error.
fn read_events(stdout: impl BufRead, run: &Run, on_message: &mut dyn FnMut(&str, &str)) -> Result<String, String> {
    let mut result = Err("The subagent exited without an answer.".to_string());
    // Raw lines: a non-UTF-8 line would end `lines()` and leave the child blocked on a full pipe.
    for line in stdout.split(b'\n').map_while(Result::ok) {
        let Ok(event) = serde_json::from_slice::<Value>(&line) else { continue };
        if event["type"] == "message" {
            // Outside the state lock: a message to this run itself reads its state.
            on_message(event["to"].as_str().unwrap_or("main"), event["text"].as_str().unwrap_or_default());
            continue;
        }
        let mut state = run.state.lock().unwrap();
        match event["type"].as_str() {
            Some("tool") => {
                state.tool_calls += 1;
                state.last_tool = event["name"].as_str().unwrap_or_default().to_string();
            }
            Some("session") => state.session = event["path"].as_str().map(String::from),
            Some("usage") => state.usage = usage::from_entry(&event),
            Some("done") => result = Ok(event["answer"].as_str().unwrap_or_default().to_string()),
            Some("error") => result = Err(event["message"].as_str().unwrap_or_default().to_string()),
            _ => {}
        }
    }
    result
}

/// Waits for foreground runs; setting `abort` (Esc) stops them, and kills the ones still running 2 s later.
pub fn wait(runs: &[Arc<Run>], abort: &AtomicBool) {
    let mut aborted_at = None;
    while runs.iter().any(|r| r.is_running()) {
        if abort.load(Ordering::Relaxed) {
            let since = *aborted_at.get_or_insert_with(Instant::now);
            let signal = if since.elapsed() < Duration::from_secs(2) { crate::process::SIGTERM } else { crate::process::SIGKILL };
            for run in runs {
                run.signal(signal);
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Records a run's answer; a background or resumed run also queues it as a follow-up for the next turn.
/// Holding `follow_ups` while the status flips lets WaitForAgents, which sees the run done, find its follow-up to withdraw.
fn finish(run: &Run, result: Result<String, String>, follow_ups: &Mutex<Vec<FollowUp>>) {
    let mut queued = follow_ups.lock().unwrap();
    {
        run.stdin.lock().unwrap().take();
        let mut state = run.state.lock().unwrap();
        state.elapsed = Some(run.started.elapsed());
        state.result = Some(result);
    }
    let answers_main = run.workflow.is_none() && run.reply_to.lock().unwrap().is_none();
    if answers_main && (run.background || run.resumed.load(Ordering::Relaxed)) {
        let text = format!("Subagent {} ({}) finished.\n\n{}", run.id, run.label, run.result_text());
        queued.push(FollowUp { source: run.id.clone(), text });
    }
}

/// WaitForAgents: the answers of `runs` once they finish or `timeout` passes, withdrawing the follow-ups they queued.
/// Esc ends the wait but leaves the runs going, so their answers still arrive as follow-ups.
pub fn wait_background(runs: &[Arc<Run>], timeout: Duration, abort: &AtomicBool, follow_ups: &Mutex<Vec<FollowUp>>) -> Result<String, String> {
    let deadline = Instant::now() + timeout;
    while runs.iter().any(|r| r.is_running()) && Instant::now() < deadline {
        if abort.load(Ordering::Relaxed) {
            return Err(crate::agent::ABORTED.into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    // Answers are read under the lock, so a run that finishes now either shows here or keeps its follow-up.
    let mut queued = follow_ups.lock().unwrap();
    let answers: Vec<String> = runs.iter().map(|r| format!("## {} ({}) {}\n\n{}", r.label, r.id, r.status(), r.result_text())).collect();
    queued.retain(|f| !runs.iter().any(|r| r.id == f.source));
    Ok(answers.join("\n\n"))
}

/// One child to spawn; `model` and `effort` are None when the parent's apply.
#[derive(Debug, PartialEq)]
pub struct Task {
    pub label: String,
    pub prompt: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub explore: bool,
}

/// The `Agent` input: `{description, prompt, model?, effort?, explore?}` or `{tasks: [...]}`.
pub fn tasks(input: &Value) -> Result<Vec<Task>, String> {
    let one = |item: &Value| -> Result<Task, String> {
        let label = item["description"].as_str().filter(|l| !l.trim().is_empty()).ok_or("Each task needs a description.")?;
        let prompt = item["prompt"].as_str().filter(|p| !p.trim().is_empty()).ok_or("Each task needs a prompt.")?;
        let model = item["model"].as_str().filter(|m| *m != "inherit").map(String::from);
        let effort = item["effort"].as_str().map(String::from);
        // The child rejects an unknown --thinking before its first JSONL line, so the parent would see no answer.
        if let Some(effort) = effort.as_deref().filter(|e| !crate::settings::THINKING_LEVELS.contains(e)) {
            return Err(format!("Unknown effort {effort}; use one of: {}.", crate::settings::THINKING_LEVELS.join(", ")));
        }
        Ok(Task { label: label.to_string(), prompt: prompt.to_string(), model, effort, explore: item["explore"].as_bool() == Some(true) })
    };
    match input["tasks"].as_array() {
        Some(list) if !list.is_empty() => list.iter().map(one).collect(),
        Some(_) => Err("tasks must not be empty.".into()),
        None => Ok(vec![one(input)?]),
    }
}

#[cfg(test)]
pub(crate) fn scan_run() -> Run {
    Run {
        id: "sa-1".into(),
        label: "scan".into(),
        task: "scan".into(),
        model: "opus".into(),
        thinking: "high".into(),
        background: true,
        workflow: None,
        started: Instant::now(),
        child: Mutex::new(None),
        stdin: Mutex::new(None),
        state: Mutex::default(),
        launch: test_launch(),
        resumed: AtomicBool::new(false),
        reply_to: Mutex::default(),
    }
}

#[cfg(test)]
pub fn test_launch() -> Launch {
    Launch {
        label: "next".into(),
        task: "look".into(),
        cwd: PathBuf::from("/nonexistent/agent-subagent-test"),
        model: "opus".into(),
        thinking: "low".into(),
        mode: Mode::Auto,
        parent_session: "parent".into(),
        explore: false,
        account: None,
        extra_dirs: Vec::new(),
        trusted: false,
        web: false,
        client_mcp: Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn project_swarm_prompt_wins_and_blank_falls_through() {
        let root = std::env::temp_dir().join(format!("agent-swarm-prompt-{}", std::process::id()));
        let (home, project) = (root.join("home"), root.join("project"));
        std::fs::create_dir_all(project.join(".agents")).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let default = swarm_prompt(&home, &project, true, "medium");
        assert!(default.contains("Implementation tasks: `effort: \"medium\"`.") && default.starts_with("When to spawn:"), "{default}");
        std::fs::write(home.join("swarm-prompt.md"), "global {implementation_effort}\n").unwrap();
        std::fs::write(project.join(".agents/swarm-prompt.md"), "  \n").unwrap();
        assert_eq!(swarm_prompt(&home, &project, true, "high"), "global high");
        std::fs::write(project.join(".agents/swarm-prompt.md"), "project").unwrap();
        assert_eq!(swarm_prompt(&home, &project, true, "high"), "project");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn single_and_parallel_task_shapes() {
        let one = tasks(&json!({"description": "scout", "prompt": "look", "model": "inherit", "effort": "low", "explore": true})).unwrap();
        assert_eq!(one, [Task { label: "scout".into(), prompt: "look".into(), model: None, effort: Some("low".into()), explore: true }]);
        assert_eq!(tasks(&json!({"tasks": [{"description": "a", "prompt": "x"}, {"description": "b", "prompt": "y"}]})).unwrap().len(), 2);
        assert!(tasks(&json!({"description": "a", "prompt": "x", "effort": "huge"})).unwrap_err().starts_with("Unknown effort huge;"));
        assert_eq!(tasks(&json!({"tasks": [{"description": " ", "prompt": "x"}]})).unwrap_err(), "Each task needs a description.");
    }

    #[test]
    fn a_run_finishing_during_bg_wait_is_returned_once_not_as_a_follow_up() {
        let run = Arc::new(scan_run());
        let follow_ups = Arc::new(Mutex::new(vec![FollowUp { source: "bt-1".into(), text: "terminal".into() }]));
        let finisher = {
            let (run, follow_ups) = (run.clone(), follow_ups.clone());
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(150));
                finish(&run, Ok("found 3".into()), &follow_ups);
            })
        };
        let answer = wait_background(&[run], Duration::from_secs(5), &AtomicBool::new(false), &follow_ups).unwrap();
        finisher.join().unwrap();
        assert_eq!(answer, "## scan (sa-1) done\n\nfound 3");
        let sources: Vec<String> = follow_ups.lock().unwrap().iter().map(|f| f.source.clone()).collect();
        assert_eq!(sources, ["bt-1"]);
    }

    #[test]
    fn the_childs_usage_record_shows_in_the_run_summary() {
        let run = scan_run();
        let usage = Usage { input: 1200, output: 300, cache_read: 40_000, cache_write: 0 };
        let stdout: String = [json!({"type": "tool", "name": "Read"}), usage::entry("opus", usage, 0), json!({"type": "done", "answer": "ok"})].iter().map(|event| format!("{event}\n")).collect();
        assert_eq!(read_events(stdout.as_bytes(), &run, &mut |_, _| {}), Ok("ok".into()));
        assert_eq!(run.summary(), "sa-1 scan running: 1 tool calls, last Read, 0s, ↑1k ↓300 cache 40k");
    }

    #[test]
    fn a_non_utf8_line_does_not_hide_the_answer() {
        let mut stdout = b"\xff\xfe not text\n".to_vec();
        stdout.extend(format!("{}\n", json!({"type": "done", "answer": "ok"})).as_bytes());
        assert_eq!(read_events(stdout.as_slice(), &scan_run(), &mut |_, _| {}), Ok("ok".into()));
    }

    #[test]
    fn finished_runs_do_not_count_toward_the_limit() {
        let mut runs = Runs::default();
        for _ in 0..MAX_RUNNING {
            let run = scan_run();
            run.state.lock().unwrap().result = Some(Ok("done".into()));
            runs.list.push(Arc::new(run));
        }
        let runs = Arc::new(Mutex::new(runs));
        let error = spawn(&runs, test_launch(), true, None, &Arc::default()).err().unwrap();
        assert!(error.starts_with("Cannot start subagent"), "{error}");
    }

    #[test]
    fn running_runs_move_to_the_top_newest_first() {
        let mut runs = Runs::default();
        let start = Instant::now();
        for (seconds, (id, done)) in [("sa-1", true), ("sa-2", false), ("sa-3", true), ("sa-4", false)].into_iter().enumerate() {
            let run = Run { id: id.into(), started: start + Duration::from_secs(seconds as u64), ..scan_run() };
            if done {
                run.state.lock().unwrap().result = Some(Ok("done".into()));
            }
            runs.list.push(Arc::new(run));
        }
        runs.running_first();
        let ids: Vec<&str> = runs.list.iter().map(|run| run.id.as_str()).collect();
        assert_eq!(ids, ["sa-4", "sa-2", "sa-3", "sa-1"]);
    }

    #[test]
    fn send_refuses_workflow_agents_and_resolves_a_label_to_the_newest_run() {
        let start = Instant::now();
        let older = Run { id: "sa-1".into(), started: start, ..scan_run() };
        let newer = Run { id: "sa-2".into(), started: start + Duration::from_secs(1), ..scan_run() };
        let owned = Run { id: "sa-3".into(), label: "step".into(), workflow: Some("wf-1".into()), ..scan_run() };
        for run in [&older, &newer] {
            run.state.lock().unwrap().result = Some(Ok("done".into()));
        }
        let runs = Arc::new(Mutex::new(Runs { list: vec![Arc::new(older), Arc::new(newer), Arc::new(owned)] }));
        let follow_ups = Arc::default();
        assert!(send(&runs, "step", "hi", None, &follow_ups).unwrap_err().starts_with("sa-3 belongs to workflow wf-1"));
        assert_eq!(send(&runs, "scan", "hi", None, &follow_ups).unwrap_err(), "sa-2 kept no session, so it cannot be resumed.");
        assert!(send(&runs, "nobody", "hi", None, &follow_ups).unwrap_err().starts_with("No agent named nobody."));
    }
}
