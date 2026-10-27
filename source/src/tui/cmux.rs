//! Reports the agent to the cmux sidebar the way cmux's own Claude Code hooks do: a status pill,
//! the workspace loading spinner, notifications and the session to resume when cmux reopens.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

const STATUS_KEY: &str = "agent";
const LOADER_ID: &str = "agent";

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Status {
    Running,
    NeedsInput,
    Idle,
}

/// What cmux last heard, so each frame sends only what changed.
pub struct Cmux {
    commands: Sender<Vec<String>>,
    worker: JoinHandle<()>,
    status: Option<Status>,
    resume: Option<(String, PathBuf)>,
}

impl Cmux {
    /// Some inside a cmux terminal, which exports the surface it runs in.
    pub fn detect() -> Option<Cmux> {
        std::env::var_os("CMUX_SURFACE_ID")?;
        let cli = std::env::var_os("CMUX_BUNDLED_CLI_PATH").filter(|path| Path::new(path).is_file()).unwrap_or_else(|| "cmux".into());
        let (commands, inbox) = mpsc::channel::<Vec<String>>();
        // One worker runs them in order: a late "Running" must never land after the turn's "Idle".
        let worker = std::thread::spawn(move || {
            for args in inbox {
                // A cmux that stops answering must not hold the agent's exit, which waits for this queue.
                _ = crate::process::output_within(crate::process::command(&cli).args(args), Duration::from_secs(3));
            }
        });
        Some(Cmux { commands, worker, status: None, resume: None })
    }

    /// `resume` is the saved session and the directory it runs in; None when nothing was saved.
    pub fn report(&mut self, status: Status, resume: Option<(String, PathBuf)>) {
        if self.status != Some(status) {
            for args in status_commands(self.status, status) {
                self.send(args);
            }
            self.status = Some(status);
        }
        if self.resume != resume {
            self.send(match &resume {
                Some((id, cwd)) => resume_command(id, cwd),
                None => strings(&["surface", "resume", "clear"]),
            });
            self.resume = resume;
        }
    }

    pub fn notify(&self, body: &str) {
        self.send(strings(&["notify", "--title", "agent", "--body", body]));
    }

    /// Clears the pill and spinner and waits for cmux to hear it; the resume binding stays for the next launch.
    pub fn finish(self) {
        self.send(strings(&["clear-status", STATUS_KEY]));
        self.send(strings(&["workspace", "loading", "off", "--id", LOADER_ID]));
        drop(self.commands);
        _ = self.worker.join();
    }

    fn send(&self, args: Vec<String>) {
        _ = self.commands.send(args);
    }
}

/// Pill text, icon and color copy cmux's Claude Code hooks, so both agents read the same in the sidebar.
fn status_commands(previous: Option<Status>, status: Status) -> Vec<Vec<String>> {
    let (value, icon, color) = match status {
        Status::Running => ("Running", "bolt.fill", "#4C8DFF"),
        Status::NeedsInput => ("Needs input", "bell.fill", "#4C8DFF"),
        Status::Idle => ("Idle", "pause.circle.fill", "#8E8E93"),
    };
    let mut commands = vec![strings(&["set-status", STATUS_KEY, value, "--icon", icon, "--color", color])];
    let loading = status == Status::Running;
    if previous.is_some_and(|previous| (previous == Status::Running) != loading) || (previous.is_none() && loading) {
        commands.push(strings(&["workspace", "loading", if loading { "on" } else { "off" }, "--id", LOADER_ID]));
    }
    // Work resumed, so the "needs you" notification of this surface is answered.
    if loading && previous == Some(Status::NeedsInput) {
        commands.push(strings(&["clear-notifications"]));
    }
    commands
}

/// cmux runs this command when it restores the surface; it asks once before auto-running it.
fn resume_command(session_id: &str, cwd: &Path) -> Vec<String> {
    let agent = std::env::current_exe().map_or_else(|_| "agent".to_string(), |path| path.to_string_lossy().into_owned());
    strings(&["surface", "resume", "set", "--kind", "agent", "--name", "agent", "--checkpoint", session_id, "--cwd", &cwd.to_string_lossy(), "--", &agent, "--resume", session_id])
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| arg.to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verbs(commands: Vec<Vec<String>>) -> Vec<String> {
        commands.into_iter().map(|args| args[..args.len().min(3)].join(" ")).collect()
    }

    #[test]
    fn the_spinner_follows_running_and_resuming_clears_the_notification() {
        assert_eq!(verbs(status_commands(None, Status::Idle)), ["set-status agent Idle"]);
        assert_eq!(verbs(status_commands(None, Status::Running)), ["set-status agent Running", "workspace loading on"]);
        assert_eq!(verbs(status_commands(Some(Status::Running), Status::NeedsInput)), ["set-status agent Needs input", "workspace loading off"]);
        assert_eq!(verbs(status_commands(Some(Status::NeedsInput), Status::Running)), ["set-status agent Running", "workspace loading on", "clear-notifications"]);
        assert_eq!(verbs(status_commands(Some(Status::NeedsInput), Status::Idle)), ["set-status agent Idle"]);
    }
}
