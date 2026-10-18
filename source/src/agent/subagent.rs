use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
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

/// jcode's swarm prompt lookup: `.agents/swarm-prompt.md`, then `~/.agents/swarm-prompt.md`, then the built-in.
pub fn swarm_prompt(agent_dir: &Path, cwd: &Path, implementation_effort: &str) -> String {
    [cwd.join(".agents"), agent_dir.to_path_buf()]
        .iter()
        .filter_map(|dir| prompt::read_text(&dir.join("swarm-prompt.md")))
        .map(|text| text.trim().to_string())
        .find(|text| !text.is_empty())
        .unwrap_or_else(|| DEFAULT_SWARM_PROMPT.trim().to_string())
        .replace("{implementation_effort}", implementation_effort)
}

/// One child process, shared with /subagents.
pub struct Run {
    pub id: String,
    pub label: String,
    pub task: String,
    pub background: bool,
    started: Instant,
    child: Mutex<Option<Child>>,
    pub state: Mutex<RunState>,
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
        if let Some(child) = self.child.lock().unwrap().as_mut() {
            // SIGTERM, not SIGKILL: the child's handler then ends its own bash commands, terminals and servers.
            crate::process::signal_group(child.id(), crate::process::SIGTERM);
        }
    }

    /// The row /subagents and the transcript show: label, last tool, call count, time, tokens.
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
pub struct Launch {
    pub label: String,
    pub task: String,
    pub cwd: PathBuf,
    pub model: String,
    pub thinking: String,
    pub mode: Mode,
    pub parent_session: String,
    /// `provider=name` of the account the parent uses for the child's model.
    pub account: Option<String>,
}

#[derive(Default)]
pub struct Runs {
    pub list: Vec<Arc<Run>>,
}

impl Runs {
    /// Starts the child and a thread reading its JSONL; a background run queues its answer as a follow-up.
    pub fn spawn(&mut self, launch: Launch, background: bool, follow_ups: &Arc<Mutex<Vec<FollowUp>>>) -> Result<Arc<Run>, String> {
        let running = self.list.iter().filter(|run| run.state.lock().unwrap().result.is_none()).count();
        if running >= MAX_RUNNING {
            return Err(format!("{MAX_RUNNING} subagents are already running, the limit. Wait for one to finish."));
        }
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
        let mut child = crate::process::command(exe)
            .args(args)
            .current_dir(&launch.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("Cannot start subagent: {e}"))?;
        let mut stdin = child.stdin.take().expect("stdin is piped");
        let message = format!("{CHILD_BRIEF}\n\n<task>\n{}\n</task>", launch.task);
        if let Err(e) = stdin.write_all(message.as_bytes()) {
            // The child leads its own process group, out of reach of stop_all once dropped.
            crate::process::signal_group(child.id(), crate::process::SIGTERM);
            let _ = child.wait();
            return Err(format!("Cannot send the task: {e}"));
        }
        drop(stdin);
        let stdout = child.stdout.take().expect("stdout is piped");
        let run = Arc::new(Run {
            id: format!("sa-{}", self.list.len() + 1),
            label: launch.label,
            task: launch.task,
            background,
            started: Instant::now(),
            child: Mutex::new(Some(child)),
            state: Mutex::default(),
        });
        self.list.push(run.clone());
        let (watched, follow_ups) = (run.clone(), follow_ups.clone());
        std::thread::spawn(move || {
            let result = read_events(BufReader::new(stdout), &watched);
            if let Some(mut child) = watched.child.lock().unwrap().take() {
                let _ = child.wait();
            }
            finish(&watched, result, &follow_ups);
        });
        Ok(run)
    }

    /// Children belong to the session, so quitting stops them.
    pub fn stop_all(&self) {
        self.list.iter().for_each(|run| run.stop());
    }

    pub fn running(&self) -> usize {
        self.list.iter().filter(|run| run.is_running()).count()
    }

    /// Moves running runs to the top, each group kept in start order.
    pub fn running_first(&mut self) {
        self.list.sort_by_key(|run| !run.is_running());
    }
}

/// Follows the child's JSONL into `run` until stdout closes; returns its answer or error.
fn read_events(stdout: impl BufRead, run: &Run) -> Result<String, String> {
    let mut result = Err("The subagent exited without an answer.".to_string());
    for line in stdout.lines().map_while(Result::ok) {
        let Ok(event) = serde_json::from_str::<Value>(&line) else { continue };
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

/// Waits for foreground runs; setting `abort` (Esc) kills them.
pub fn wait(runs: &[Arc<Run>], abort: &AtomicBool) {
    while runs.iter().any(|r| r.is_running()) {
        if abort.load(Ordering::Relaxed) {
            runs.iter().for_each(|r| r.stop());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Records a run's answer; a background run also queues it as a follow-up for the next turn.
/// Holding `follow_ups` while the status flips lets bg_wait, which sees the run done, find its follow-up to withdraw.
fn finish(run: &Run, result: Result<String, String>, follow_ups: &Mutex<Vec<FollowUp>>) {
    let mut queued = follow_ups.lock().unwrap();
    let mut state = run.state.lock().unwrap();
    state.elapsed = Some(run.started.elapsed());
    state.result = Some(result);
    drop(state);
    if run.background {
        let text = format!("Subagent {} ({}) finished.\n\n{}", run.id, run.label, run.result_text());
        queued.push(FollowUp { source: run.id.clone(), text });
    }
}

/// bg_wait: the answers of `runs` once they finish or `timeout` passes, withdrawing the follow-ups they queued.
/// Esc ends the wait but leaves the runs going, so their answers still arrive as follow-ups.
pub fn wait_background(runs: &[Arc<Run>], timeout: Duration, abort: &AtomicBool, follow_ups: &Mutex<Vec<FollowUp>>) -> Result<String, String> {
    let deadline = Instant::now() + timeout;
    while runs.iter().any(|r| r.is_running()) && Instant::now() < deadline {
        if abort.load(Ordering::Relaxed) {
            return Err("Operation aborted".into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    follow_ups.lock().unwrap().retain(|f| !runs.iter().any(|r| r.id == f.source));
    let answers: Vec<String> = runs.iter().map(|r| format!("## {} ({}) {}\n\n{}", r.label, r.id, r.status(), r.result_text())).collect();
    Ok(answers.join("\n\n"))
}

/// One child to spawn; `model` and `effort` are None when the parent's apply.
#[derive(Debug, PartialEq)]
pub struct Task {
    pub label: String,
    pub prompt: String,
    pub model: Option<String>,
    pub effort: Option<String>,
}

/// The `subagent` input: `{label, prompt, model?, effort?}` or `{tasks: [...]}`.
pub fn tasks(input: &Value) -> Result<Vec<Task>, String> {
    let one = |item: &Value| -> Result<Task, String> {
        let label = item["label"].as_str().filter(|l| !l.trim().is_empty()).ok_or("Each task needs a label.")?;
        let prompt = item["prompt"].as_str().filter(|p| !p.trim().is_empty()).ok_or("Each task needs a prompt.")?;
        let model = item["model"].as_str().filter(|m| *m != "inherit").map(String::from);
        let effort = item["effort"].as_str().map(String::from);
        // The child rejects an unknown --thinking before its first JSONL line, so the parent would see no answer.
        if let Some(effort) = effort.as_deref().filter(|e| !crate::tui::THINKING_LEVELS.contains(e)) {
            return Err(format!("Unknown effort {effort}; use one of: {}.", crate::tui::THINKING_LEVELS.join(", ")));
        }
        Ok(Task { label: label.to_string(), prompt: prompt.to_string(), model, effort })
    };
    match input["tasks"].as_array() {
        Some(list) if !list.is_empty() => list.iter().map(one).collect(),
        Some(_) => Err("tasks must not be empty.".into()),
        None => Ok(vec![one(input)?]),
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
        assert!(swarm_prompt(&home, &project, "medium").contains("Implementation tasks: `effort: \"medium\"`."));
        std::fs::write(home.join("swarm-prompt.md"), "global {implementation_effort}\n").unwrap();
        std::fs::write(project.join(".agents/swarm-prompt.md"), "  \n").unwrap();
        assert_eq!(swarm_prompt(&home, &project, "high"), "global high");
        std::fs::write(project.join(".agents/swarm-prompt.md"), "project").unwrap();
        assert_eq!(swarm_prompt(&home, &project, "high"), "project");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn single_and_parallel_task_shapes() {
        let one = tasks(&json!({"label": "scout", "prompt": "look", "model": "inherit", "effort": "low"})).unwrap();
        assert_eq!(one, [Task { label: "scout".into(), prompt: "look".into(), model: None, effort: Some("low".into()) }]);
        assert_eq!(tasks(&json!({"tasks": [{"label": "a", "prompt": "x"}, {"label": "b", "prompt": "y"}]})).unwrap().len(), 2);
        assert!(tasks(&json!({"label": "a", "prompt": "x", "effort": "huge"})).unwrap_err().starts_with("Unknown effort huge;"));
        assert_eq!(tasks(&json!({"tasks": [{"label": " ", "prompt": "x"}]})).unwrap_err(), "Each task needs a label.");
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
        let stdout: String = [json!({"type": "tool", "name": "read"}), usage::entry("opus", usage, 0), json!({"type": "done", "answer": "ok"})].iter().map(|event| format!("{event}\n")).collect();
        assert_eq!(read_events(stdout.as_bytes(), &run), Ok("ok".into()));
        assert_eq!(run.summary(), "sa-1 scan running: 1 tool calls, last read, 0s, ↑1k ↓300 cache 40k");
    }

    #[test]
    fn finished_runs_do_not_count_toward_the_limit() {
        let mut runs = Runs::default();
        for _ in 0..MAX_RUNNING {
            let run = scan_run();
            run.state.lock().unwrap().result = Some(Ok("done".into()));
            runs.list.push(Arc::new(run));
        }
        let launch = Launch {
            label: "next".into(),
            task: "look".into(),
            cwd: PathBuf::from("/nonexistent/agent-subagent-test"),
            model: "opus".into(),
            thinking: "low".into(),
            mode: Mode::Auto,
            parent_session: "parent".into(),
            account: None,
        };
        let error = runs.spawn(launch, true, &Arc::default()).err().unwrap();
        assert!(error.starts_with("Cannot start subagent"), "{error}");
    }

    #[test]
    fn running_runs_move_to_the_top_in_start_order() {
        let mut runs = Runs::default();
        for (id, done) in [("sa-1", true), ("sa-2", false), ("sa-3", true), ("sa-4", false)] {
            let run = Run { id: id.into(), ..scan_run() };
            if done {
                run.state.lock().unwrap().result = Some(Ok("done".into()));
            }
            runs.list.push(Arc::new(run));
        }
        runs.running_first();
        let ids: Vec<&str> = runs.list.iter().map(|run| run.id.as_str()).collect();
        assert_eq!(ids, ["sa-2", "sa-4", "sa-1", "sa-3"]);
    }

    fn scan_run() -> Run {
        Run { id: "sa-1".into(), label: "scan".into(), task: "scan".into(), background: true, started: Instant::now(), child: Mutex::new(None), state: Mutex::default() }
    }
}
