use serde_json::Value;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::agent::FollowUp;
use crate::tools::format_size;

/// pi's background-terminals limits.
const MAX_RUNNING: usize = 8;
const MAX_TITLE_CHARS: usize = 80;
const STATUS_TAIL: (u64, usize) = (16 * 1024, 400);
const STATUS_STDERR_TAIL: (u64, usize) = (8 * 1024, 200);
const RESULT_TAIL: (u64, usize) = (8 * 1024, 40);
const RESULT_STDERR_TAIL: (u64, usize) = (4 * 1024, 20);
// shortcut: the viewer rewraps the whole tail each time the log grows; past 1 MiB it points to the full log file.
const VIEW_TAIL: (u64, usize) = (1024 * 1024, 20_000);
const FORCE_KILL_AFTER: Duration = Duration::from_secs(2);
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// The background terminals of one agent; shared with the UI for /tasks and the widget.
#[derive(Default)]
pub struct Terminals {
    pub list: Vec<Arc<Terminal>>,
    counter: u64,
}

pub struct Terminal {
    pub id: String,
    pub title: String,
    pub cwd: PathBuf,
    pub pid: u32,
    started: Instant,
    stdout: PathBuf,
    stderr: PathBuf,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    exit: Option<(Status, String)>,
    settled: Option<Instant>,
    killed: bool,
    /// The model already saw the final state (TaskOutput or TaskStop), so no follow-up.
    reported: bool,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Status {
    Running,
    Done,
    Failed,
    Killed,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Running => "running",
            Status::Done => "done",
            Status::Failed => "failed",
            Status::Killed => "killed",
        }
    }
}

impl Terminals {
    pub fn status_of(&self, input: &Value, follow_ups: &Arc<Mutex<Vec<FollowUp>>>) -> Result<String, String> {
        let Some(id) = input["task_id"].as_str() else { return Ok(self.describe_all()) };
        let terminal = self.find(id)?;
        terminal.mark_reported(follow_ups);
        Ok(terminal.status_text())
    }

    pub fn describe_all(&self) -> String {
        if self.list.is_empty() {
            return "No background terminals.".into();
        }
        self.list.iter().map(|t| t.describe()).collect::<Vec<_>>().join("\n")
    }

    pub fn start(&mut self, input: &Value, cwd: &Path, follow_ups: &Arc<Mutex<Vec<FollowUp>>>) -> Result<String, String> {
        let command = input["command"].as_str().unwrap_or_default().trim();
        if command.is_empty() {
            return Err("command must not be empty.".into());
        }
        if self.running() >= MAX_RUNNING {
            return Err(format!("Max {MAX_RUNNING} background terminals can run concurrently. Stop one with TaskStop before starting another."));
        }
        let title: String = input["description"].as_str().unwrap_or_default().split_whitespace().collect::<Vec<_>>().join(" ").chars().take(MAX_TITLE_CHARS).collect();
        let title = if title.is_empty() { "terminal".to_string() } else { title };
        let (stdout_file, stdout) = super::private_file("bg", "stdout")?;
        let (stderr_file, stderr) = super::private_file("bg", "stderr")?;
        let pid = crate::process::command("sh").args(["-c", command]).current_dir(cwd).stdout(stdout_file).stderr(stderr_file).spawn().map_err(|e| format!("Cannot start sh: {e}"))?.id();
        self.counter += 1;
        let id = format!("bt-{}", self.counter);
        let terminal = Arc::new(Terminal { id, title, cwd: cwd.to_path_buf(), pid, started: Instant::now(), stdout, stderr, state: Mutex::default() });
        self.list.push(terminal.clone());
        let follow_ups = follow_ups.clone();
        let watched = terminal.clone();
        std::thread::spawn(move || {
            // shortcut: every settled `sh` stays a zombie until the agent exits, so `stop` and `kill_all` never signal a
            // reused group; reap once the group is empty if a session starts thousands of terminals.
            let status = crate::process::wait_unreaped(pid);
            // follow_ups before state, the order TaskOutput and TaskStop use, so neither sees the exit before its report is queued.
            let mut queued = follow_ups.lock().unwrap();
            let mut state = watched.state.lock().unwrap();
            let exit = match status {
                Ok(status) if state.killed && status.signal().is_some() => (Status::Killed, exit_text(status.code(), status.signal())),
                Ok(status) if status.success() => (Status::Done, "exit 0".to_string()),
                Ok(status) => (Status::Failed, exit_text(status.code(), status.signal())),
                Err(e) => (Status::Failed, format!("wait failed: {e}")),
            };
            state.exit = Some(exit);
            state.settled = Some(Instant::now());
            let report = !state.reported;
            drop(state);
            if report {
                queued.push(FollowUp { source: watched.id.clone(), text: watched.result_text() });
            }
        });
        Ok(format!(
            "Started background terminal {id} \"{title}\" (pid {pid}, {cwd}).\nIt runs in the background with no stdin. You'll get a message when it exits, or use TaskOutput(task_id: \"{id}\") to peek, TaskStop to stop it, TaskOutput without task_id to see all.",
            id = terminal.id,
            title = terminal.title,
            pid = terminal.pid,
            cwd = terminal.cwd.display()
        ))
    }

    fn find(&self, id: &str) -> Result<Arc<Terminal>, String> {
        let found = self.list.iter().find(|t| t.id == id).cloned();
        found.ok_or_else(|| format!("Unknown terminal id \"{id}\". Known: {}.", self.known()))
    }

    fn known(&self) -> String {
        let ids: Vec<&str> = self.list.iter().map(|t| t.id.as_str()).collect();
        if ids.is_empty() { "none".into() } else { ids.join(", ") }
    }

    /// The terminals `TaskStop` names; it runs without the lock, since stopping can take seconds.
    pub fn for_kill(&self, input: &Value) -> Result<Vec<Arc<Terminal>>, String> {
        let mut ids: Vec<&str> = Vec::new();
        for id in input["task_ids"].as_array().into_iter().flatten().filter_map(Value::as_str) {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        if ids.is_empty() {
            return Err("Provide at least one terminal id.".into());
        }
        let unknown: Vec<&str> = ids.iter().copied().filter(|id| self.list.iter().all(|t| t.id != *id)).collect();
        if !unknown.is_empty() {
            return Err(format!("Unknown terminal id(s): {}. Known: {}.", unknown.join(", "), self.known()));
        }
        Ok(ids.iter().filter_map(|id| self.list.iter().find(|t| t.id == *id).cloned()).collect())
    }

    /// Process groups do not end with the agent, so quitting kills them, also the members that outlived their `sh`.
    pub fn kill_all(&self) {
        for terminal in &self.list {
            crate::process::signal_group(terminal.pid, crate::process::SIGKILL);
        }
    }

    pub fn running(&self) -> usize {
        self.list.iter().filter(|t| t.status() == Status::Running).count()
    }

    /// Running shells first, each group newest first.
    pub fn running_first(&mut self) {
        self.list.sort_by_key(|t| (t.status() != Status::Running, std::cmp::Reverse(t.started)));
    }
}

/// TaskStop: SIGTERM to each group, SIGKILL after `FORCE_KILL_AFTER`, then the final states.
pub fn kill(terminals: &[Arc<Terminal>], follow_ups: &Arc<Mutex<Vec<FollowUp>>>, abort: &AtomicBool) -> String {
    let was_running: Vec<bool> = terminals.iter().map(|t| t.stop()).collect();
    let deadline = Instant::now() + STOP_TIMEOUT;
    while terminals.iter().any(|t| t.status() == Status::Running) && Instant::now() < deadline && !abort.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(50));
    }
    let mut lines = Vec::new();
    for (terminal, was_running) in terminals.iter().zip(was_running) {
        terminal.mark_reported(follow_ups);
        let (status, exit) = terminal.exit();
        let (id, title) = (&terminal.id, &terminal.title);
        lines.push(match status {
            Status::Running => format!("{id} \"{title}\" did not stop within {}s.", STOP_TIMEOUT.as_secs()),
            _ if !was_running => format!("{id} \"{title}\" was already {} ({exit}).", status.as_str()),
            Status::Killed => format!("Killed {id} \"{title}\" ({exit})."),
            _ => format!("{id} \"{title}\" exited on its own before the kill landed ({exit})."),
        });
    }
    lines.join("\n")
}

impl Terminal {
    pub fn status(&self) -> Status {
        self.exit().0
    }

    fn exit(&self) -> (Status, String) {
        let state = self.state.lock().unwrap();
        state.exit.clone().unwrap_or((Status::Running, "exit -".into()))
    }

    /// The model saw the final state, so the watcher's exit report is withdrawn or never queued.
    /// Locks follow_ups before state, the order the watcher uses.
    fn mark_reported(&self, follow_ups: &Mutex<Vec<FollowUp>>) {
        let mut queued = follow_ups.lock().unwrap();
        let mut state = self.state.lock().unwrap();
        if state.exit.is_some() {
            state.reported = true;
            queued.retain(|f| f.source != self.id);
        }
    }

    /// Sends SIGTERM to the whole group and escalates to SIGKILL later; false when its `sh` had already exited.
    /// Members that outlived the `sh` still get the signals.
    pub fn stop(&self) -> bool {
        let mut state = self.state.lock().unwrap();
        let was_running = state.exit.is_none();
        state.killed = true;
        drop(state);
        let group = self.pid;
        crate::process::signal_group(group, crate::process::SIGTERM);
        std::thread::spawn(move || {
            std::thread::sleep(FORCE_KILL_AFTER);
            crate::process::signal_group(group, crate::process::SIGKILL);
        });
        was_running
    }

    fn describe(&self) -> String {
        let (status, exit) = self.exit();
        let size = |path: &Path| format_size(std::fs::metadata(path).map_or(0, |m| m.len() as usize));
        format!(
            "{} [{}] \"{}\" (pid {}, {}, {exit}, {}, stdout {}, stderr {})",
            self.id,
            status.as_str(),
            self.title,
            self.pid,
            self.elapsed(),
            self.cwd.display(),
            size(&self.stdout),
            size(&self.stderr)
        )
    }

    pub fn elapsed(&self) -> String {
        let end = self.state.lock().unwrap().settled.unwrap_or_else(Instant::now);
        let seconds = end.duration_since(self.started).as_secs_f64().round() as u64;
        match seconds {
            0..60 => format!("{seconds}s"),
            60..3600 => format!("{}m{:02}s", seconds / 60, seconds % 60),
            _ => format!("{}h{:02}m", seconds / 3600, seconds / 60 % 60),
        }
    }

    /// Combined size of both logs; the /tasks viewer rereads `log` when it changes.
    pub fn log_size(&self) -> u64 {
        [&self.stdout, &self.stderr].iter().map(|path| std::fs::metadata(path).map_or(0, |m| m.len())).sum()
    }

    /// Both streams for the /tasks viewer, stderr only when it has something.
    pub fn log(&self) -> String {
        let mut text = section("stdout", &self.stdout, VIEW_TAIL);
        if std::fs::metadata(&self.stderr).is_ok_and(|m| m.len() > 0) {
            text.push_str(&format!("\n\n{}", section("stderr", &self.stderr, VIEW_TAIL)));
        }
        text
    }

    fn status_text(&self) -> String {
        let stdout = section("stdout", &self.stdout, STATUS_TAIL);
        let stderr = section("stderr", &self.stderr, STATUS_STDERR_TAIL);
        format!("{}\n\n{stdout}\n\n{stderr}", self.describe())
    }

    fn result_text(&self) -> String {
        let (status, exit) = self.exit();
        let how = if status == Status::Killed { "was killed".to_string() } else { format!("exited ({exit})") };
        let mut text = format!("Background terminal {} \"{}\" {how} after {}.", self.id, self.title, self.elapsed());
        text.push_str(&format!("\n\n{}", section("stdout", &self.stdout, RESULT_TAIL)));
        if std::fs::metadata(&self.stderr).is_ok_and(|m| m.len() > 0) {
            text.push_str(&format!("\n\n{}", section("stderr", &self.stderr, RESULT_STDERR_TAIL)));
        }
        text
    }
}

fn exit_text(code: Option<i32>, signal: Option<i32>) -> String {
    match (code, signal) {
        (Some(code), _) => format!("exit {code}"),
        (None, Some(crate::process::SIGHUP)) => "SIGHUP".into(),
        (None, Some(2)) => "SIGINT".into(),
        (None, Some(6)) => "SIGABRT".into(),
        (None, Some(crate::process::SIGKILL)) => "SIGKILL".into(),
        (None, Some(11)) => "SIGSEGV".into(),
        (None, Some(13)) => "SIGPIPE".into(),
        (None, Some(crate::process::SIGTERM)) => "SIGTERM".into(),
        (None, Some(signal)) => format!("signal {signal}"),
        (None, None) => "exit ?".into(),
    }
}

/// pi's output section: the stream's tail within `max_bytes` and `max_lines`, with a pointer to the full log.
fn section(label: &str, path: &Path, (max_bytes, max_lines): (u64, usize)) -> String {
    let total = std::fs::metadata(path).map_or(0, |m| m.len());
    if total == 0 {
        return format!("{label}: (empty)");
    }
    let mut raw = Vec::new();
    if let Ok(mut file) = File::open(path) {
        let _ = file.seek(SeekFrom::Start(total.saturating_sub(max_bytes)));
        let _ = file.take(max_bytes).read_to_end(&mut raw);
    }
    let text = String::from_utf8_lossy(&raw);
    let mut lines: Vec<&str> = text.trim_end_matches('\n').split('\n').collect();
    if total > max_bytes && lines.len() > 1 {
        lines.remove(0);
    }
    let kept = &lines[lines.len().saturating_sub(max_lines)..];
    let shown = kept.join("\n");
    let mut out = format!("{label}:\n{shown}");
    if shown.len() as u64 + 1 < total {
        out.push_str(&format!("\n[{label} truncated: showing last {} of {}. Full log: {}]", format_size(shown.len()), format_size(total as usize), path.display()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn exit_is_reported_once_unless_status_already_saw_it() {
        let dir = std::env::temp_dir();
        let follow_ups = Arc::default();
        let mut terminals = Terminals::default();
        let started = terminals.start(&json!({"command": "echo out; echo err >&2; exit 3", "description": "  a\nb "}), &dir, &follow_ups).unwrap();
        assert!(started.starts_with("Started background terminal bt-1 \"a b\""));
        wait_until(|| !follow_ups.lock().unwrap().is_empty());
        let text = follow_ups.lock().unwrap().first().expect("no exit report within 5s").text.clone();
        assert!(text.starts_with("Background terminal bt-1 \"a b\" exited (exit 3) after 0s.\n\nstdout:\nout\n\nstderr:\nerr"), "{text}");
        let status = terminals.status_of(&json!({"task_id": "bt-1"}), &follow_ups).unwrap();
        assert!(status.starts_with("bt-1 [failed] \"a b\""), "{status}");
        assert!(follow_ups.lock().unwrap().is_empty());
        assert_eq!(terminals.status_of(&json!({"task_id": "bt-9"}), &follow_ups).unwrap_err(), "Unknown terminal id \"bt-9\". Known: bt-1.");
    }

    #[test]
    fn kill_reports_a_killed_terminal_once_without_a_follow_up() {
        let dir = std::env::temp_dir();
        let follow_ups = Arc::default();
        let mut terminals = Terminals::default();
        terminals.start(&json!({"command": "sleep 30 & sleep 30", "description": "sleeper"}), &dir, &follow_ups).unwrap();
        let targets = terminals.for_kill(&json!({"task_ids": ["bt-1"]})).unwrap();
        let abort = AtomicBool::new(false);
        assert_eq!(kill(&targets, &follow_ups, &abort), "Killed bt-1 \"sleeper\" (SIGTERM).");
        assert!(follow_ups.lock().unwrap().is_empty());
        assert_eq!(kill(&targets, &follow_ups, &abort), "bt-1 \"sleeper\" was already killed (SIGTERM).");
    }

    #[test]
    fn kill_reaches_children_that_outlive_the_shell() {
        let dir = std::env::temp_dir();
        let follow_ups = Arc::default();
        let mut terminals = Terminals::default();
        terminals.start(&json!({"command": "sleep 30 & echo $!", "description": "orphan"}), &dir, &follow_ups).unwrap();
        let terminal = terminals.find("bt-1").unwrap();
        wait_until(|| terminal.status() != Status::Running);
        let child = std::fs::read_to_string(&terminal.stdout).unwrap().trim().to_string();
        assert_eq!(kill(&[terminal], &follow_ups, &AtomicBool::new(false)), "bt-1 \"orphan\" was already done (exit 0).");
        wait_until(|| !alive(&child));
        assert!(!alive(&child), "sleep {child} survived TaskStop");
    }

    fn wait_until(done: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn alive(pid: &str) -> bool {
        crate::process::command("kill").args(["-0", pid]).stderr(std::process::Stdio::null()).status().unwrap().success()
    }
}
