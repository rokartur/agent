//! The terminal side of a session server: `agent` starts one per session and attaches to it, `agent attach`
//! returns to a running one. Each server is an `agent serve` process listening on `~/.agents/serve/<pid>.sock`.

use std::io::{BufRead, IsTerminal, Write};
use std::os::fd::IntoRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use crate::agent::peers::{self, Peer};

/// A crashed server's log stays this long for a bug report, then the next listing removes it.
const CRASH_LOG_AGE: Duration = Duration::from_secs(7 * 24 * 3600);
/// Startup downloads the mode rules when they are missing, so it can take a few seconds on a slow network.
const START_TIMEOUT: Duration = Duration::from_secs(60);

fn dir(home: &Path) -> PathBuf {
    home.join(".agents/serve")
}

pub fn socket(home: &Path, pid: u32) -> PathBuf {
    dir(home).join(format!("{pid}.sock"))
}

fn log(home: &Path, pid: u32) -> PathBuf {
    dir(home).join(format!("{pid}.log"))
}

/// `agent serve`: no terminal reads its output, so stdout and stderr go to its log, which the client that
/// started it shows when it exits before listening.
pub fn redirect_output(home: &Path) -> Result<(), String> {
    std::fs::create_dir_all(home.join(".agents")).map_err(|e| format!("Cannot create {}: {e}", home.join(".agents").display()))?;
    crate::process::private_dir(&dir(home))?;
    let path = log(home, std::process::id());
    let file = std::fs::OpenOptions::new().create(true).write(true).truncate(true).mode(0o600).open(&path).map_err(|e| format!("Cannot open {}: {e}", path.display()))?;
    let fd = file.into_raw_fd();
    // SAFETY: dup2 only takes descriptors; `fd` is open and stays open for the life of the process.
    if unsafe { libc::dup2(fd, 1) == -1 || libc::dup2(fd, 2) == -1 } {
        return Err(format!("Cannot write to {}: {}", path.display(), std::io::Error::last_os_error()));
    }
    Ok(())
}

/// The log of a server that exits cleanly holds nothing worth keeping.
pub fn remove_log(home: &Path) {
    let _ = std::fs::remove_file(log(home, std::process::id()));
}

/// A server ended by a signal leaves neither a socket nor a log; this process has neither when it is no server.
pub fn remove_files(home: &Path) {
    let _ = std::fs::remove_file(socket(home, std::process::id()));
    remove_log(home);
}

/// Starts `agent serve <args>` and connects once it listens. A piped stdin goes to the server, which reads the
/// message from it as `agent` always did.
pub fn start(home: &Path, args: &[String]) -> Result<UnixStream, String> {
    let exe = std::env::current_exe().map_err(|e| format!("Cannot find the agent binary: {e}"))?;
    // sh starts the server in the background and exits, so the server is no child of ours: on SIGHUP, closing the
    // terminal, `process::hang_up_children` would end it. An async list's stdin is /dev/null unless redirected.
    let mut command = crate::process::command("sh");
    command.args(["-c", r#""$0" serve "$@" <&0 >/dev/null 2>&1 & echo $!"#]).arg(exe).args(args).stdout(Stdio::piped()).stderr(Stdio::null());
    if !std::io::stdin().is_terminal() {
        command.stdin(Stdio::inherit());
    }
    let output = command.output().map_err(|e| format!("Cannot start the session server: {e}"))?;
    let pid: u32 = String::from_utf8_lossy(&output.stdout).trim().parse().map_err(|_| format!("Cannot start the session server: sh exited with {}.", output.status))?;
    let (socket, log) = (socket(home, pid), log(home, pid));
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        if let Ok(stream) = UnixStream::connect(&socket) {
            return Ok(stream);
        }
        if !crate::process::alive(pid) {
            let output = std::fs::read_to_string(&log).unwrap_or_default();
            let _ = std::fs::remove_file(&log);
            let output = output.trim();
            return Err(if output.is_empty() { "The session server stopped before it listened.".into() } else { output.trim_start_matches("error: ").to_string() });
        }
        if Instant::now() > deadline {
            return Err(format!("The session server {pid} did not listen within {} s; its output is in {}.", START_TIMEOUT.as_secs(), log.display()));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

pub fn connect(home: &Path, pid: u32) -> Result<UnixStream, String> {
    UnixStream::connect(socket(home, pid)).map_err(|e| format!("Cannot reach session server {pid}: {e}"))
}

/// Shows the session until it ends or this terminal detaches, then prints what the server said goodbye with.
pub fn show(stream: UnixStream) -> Result<(), String> {
    let text = crate::tui::attach(stream)?;
    print!("{text}");
    Ok(())
}

/// Sessions a terminal can attach to: the cross-session cards of live processes that also listen for terminals.
pub fn live(home: &Path) -> Vec<Peer> {
    sweep(home);
    peers::list(home).into_iter().filter(|peer| socket(home, peer.pid).exists()).collect()
}

/// The live session whose id starts with `prefix`, if any.
pub fn find(home: &Path, prefix: &str) -> Result<Option<Peer>, String> {
    let mut matches: Vec<Peer> = live(home).into_iter().filter(|peer| peer.session.starts_with(prefix)).collect();
    if matches.len() > 1 {
        return Err(format!("{prefix} matches {} running sessions; use a longer id:\n{}", matches.len(), list(&matches)));
    }
    Ok(matches.pop())
}

/// `agent attach [id]`: a running session, or with an id that none has, the saved one, like `agent -r <id>`.
pub fn attach_command(args: &[String]) -> Result<(), String> {
    if !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
        return Err("agent attach needs a terminal.".into());
    }
    let home = crate::home()?;
    let stream = match args {
        [] => connect(&home, pick(live(&home))?.pid)?,
        [id] if !id.starts_with('-') => match find(&home, id)? {
            Some(peer) => connect(&home, peer.pid)?,
            None => start(&home, &["--resume".to_string(), id.clone()])?,
        },
        _ => return Err("Usage: agent attach [session id].".into()),
    };
    show(stream)
}

fn pick(mut sessions: Vec<Peer>) -> Result<Peer, String> {
    match sessions.len() {
        0 => return Err("No session is running. Start one with `agent`, or open a saved one with `agent -r`.".into()),
        1 => return Ok(sessions.remove(0)),
        _ => {}
    }
    eprint!("{}\nAttach to [1-{}]: ", list(&sessions), sessions.len());
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer).map_err(|e| format!("Cannot read the answer: {e}"))?;
    let index = answer.trim().parse::<usize>().ok().filter(|n| (1..=sessions.len()).contains(n)).ok_or_else(|| format!("{} is not on the list.", answer.trim()))?;
    Ok(sessions.remove(index - 1))
}

fn list(sessions: &[Peer]) -> String {
    let mut lines = Vec::new();
    for (n, peer) in (1..).zip(sessions) {
        let state = if peer.busy { "working" } else { "idle" };
        let title: String = peer.title.chars().filter(|c| !c.is_control()).collect();
        let cwd: String = peer.cwd.chars().filter(|c| !c.is_control()).collect();
        lines.push(format!("{n:>3}  {}  {state:<7}  {cwd}  {title}", peer.session));
    }
    lines.join("\n")
}

/// Drops the sockets of servers that died without cleaning up, and their logs once they are old.
fn sweep(home: &Path) {
    let Ok(entries) = std::fs::read_dir(dir(home)) else { return };
    for path in entries.map_while(Result::ok).map(|entry| entry.path()) {
        let Some(pid) = path.file_stem().and_then(|stem| stem.to_str()).and_then(|stem| stem.parse::<u32>().ok()) else { continue };
        if crate::process::alive(pid) {
            continue;
        }
        let old = path.metadata().and_then(|metadata| metadata.modified()).is_ok_and(|modified| modified.elapsed().is_ok_and(|age| age > CRASH_LOG_AGE));
        if path.extension().is_some_and(|ext| ext == "sock") || old {
            let _ = std::fs::remove_file(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dead_server_loses_its_socket_now_and_its_log_after_a_week() {
        let home = std::env::temp_dir().join(format!("agent-serve-sweep-{}", std::process::id()));
        std::fs::create_dir_all(dir(&home)).unwrap();
        // Above every pid_max, so no process has it.
        let dead = 999_999_999;
        let (socket, log) = (socket(&home, dead), log(&home, dead));
        let (live_socket, old_log) = (super::socket(&home, std::process::id()), super::log(&home, dead + 1));
        for path in [&socket, &log, &live_socket, &old_log] {
            std::fs::write(path, "").unwrap();
        }
        std::fs::File::options().write(true).open(&old_log).unwrap().set_modified(std::time::SystemTime::now() - CRASH_LOG_AGE * 2).unwrap();
        sweep(&home);
        assert_eq!([socket.exists(), log.exists(), live_socket.exists(), old_log.exists()], [false, true, true, false]);
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn the_list_strips_control_characters_from_cards() {
        let peer = Peer { pid: 1, session: "abc".into(), title: "t\x1b]0;x\x07".into(), cwd: "/p".into(), busy: true, socket: PathBuf::new() };
        assert_eq!(list(&[peer]), "  1  abc  working  /p  t]0;x");
    }
}
