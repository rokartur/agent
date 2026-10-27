//! The terminal side of a session server: `agent` starts one per session and attaches to it, `agent attach`
//! returns to a running one. Each server is an `agent serve` process listening on `~/.agents/serve/<pid>.sock`;
//! `crate::tailnet` reaches the ones on other computers.

use std::io::{BufRead, BufReader, IsTerminal, Write};
use std::net::TcpStream;
use std::os::fd::IntoRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use crate::agent::peers::{self, Peer};
use crate::settings::Settings;
use crate::tailnet::{self, LOCAL, Reattach};
use crate::tui::{Ending, Place};

/// A session server this terminal reaches: its socket on this computer, or `agent serve --tailnet` on another one.
pub enum Stream {
    Local(UnixStream),
    Remote(BufReader<TcpStream>, TcpStream),
}

/// A running session as `agent attach` lists it. One on a computer that does not answer is the one it had when it
/// last did, and cannot be attached to.
#[derive(Clone, Debug, PartialEq)]
pub struct Listed {
    pub environment: String,
    pub session: String,
    pub title: String,
    pub cwd: String,
    pub busy: bool,
    pub reachable: bool,
}

impl Listed {
    fn local(peer: Peer) -> Listed {
        Listed { environment: LOCAL.into(), session: peer.session, title: peer.title, cwd: peer.cwd, busy: peer.busy, reachable: true }
    }
}

/// A crashed server's log stays this long for a bug report, then the next listing removes it.
const CRASH_LOG_AGE: Duration = Duration::from_secs(7 * 24 * 3600);
/// Startup downloads the mode rules when they are missing, so it can take a few seconds on a slow network.
const START_TIMEOUT: Duration = Duration::from_secs(60);
/// A woken laptop finds its host again within this long of the network coming back.
const MAX_RECONNECT_WAIT: Duration = Duration::from_secs(3);

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

/// Starts `agent serve <args>` in `cwd` and connects once it listens. A `piped` stdin goes to the server, which reads
/// the message from it as `agent` always did.
pub fn start(home: &Path, args: &[String], cwd: &Path, piped: bool) -> Result<UnixStream, String> {
    let exe = std::env::current_exe().map_err(|e| format!("Cannot find the agent binary: {e}"))?;
    // sh starts the server in the background and exits, so the server is no child of ours: on SIGHUP, closing the
    // terminal, `process::hang_up_children` would end it. An async list's stdin is /dev/null unless redirected.
    let mut command = crate::process::command("sh");
    command.args(["-c", r#""$0" serve "$@" <&0 >/dev/null 2>&1 & echo $!"#]).arg(exe).args(args).current_dir(cwd).stdout(Stdio::piped()).stderr(Stdio::null());
    if piped {
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

/// Shows the session until it ends or this terminal detaches, then prints what the server said goodbye with. /env
/// leaves it for a new session in another environment, which this terminal then shows. A remote session whose
/// connection drops stays on screen while this terminal reconnects to it.
/// `connect` runs in raw mode: keys typed meanwhile would otherwise reach the session cooked, Enter as the LF that
/// raw mode reads as Ctrl+J, a newline in the editor.
pub fn show(home: &Path, settings: &Settings, environment: &str, connect: impl FnOnce() -> Result<Stream, String>) -> Result<(), String> {
    let fullscreen = crate::tui::Fullscreen::enter()?;
    if environment != LOCAL {
        crate::tui::notice(&format!("Connecting to {environment}..."))?;
    }
    let (mut environment, mut stream) = (environment.to_string(), connect()?);
    loop {
        let place = Place { environment: environment.clone(), environments: tailnet::environments(settings) };
        let ending = match stream {
            Stream::Local(socket) => crate::tui::attach(socket.try_clone().map_err(|e| format!("Cannot use the session socket: {e}"))?, socket, &place)?,
            Stream::Remote(reader, writer) => crate::tui::attach(reader, writer, &place)?,
        };
        let next = match ending {
            Ending::Exit(text) => {
                drop(fullscreen);
                print!("{text}");
                return Ok(());
            }
            Ending::Switch(next) => next,
            Ending::Lost(_) if environment == LOCAL => return Err("The session server stopped.".into()),
            Ending::Lost(Some(server)) => {
                let Some(back) = reconnect(settings, &environment, server)? else {
                    drop(fullscreen);
                    println!("Left the session on {environment}. It keeps running there, and `agent attach` returns to it.");
                    return Ok(());
                };
                stream = back;
                continue;
            }
            Ending::Lost(None) => return Err(format!("Lost the connection to {environment}. If {environment} is up, the session runs on there and `agent attach` returns to it.")),
        };
        tailnet::check(settings, &next)?;
        let cwd = std::env::current_dir().map_err(|e| format!("Cannot read the working directory: {e}"))?;
        stream = if next == LOCAL { Stream::Local(start(home, &[], &cwd, false)?) } else { tailnet::start(settings, &next, &cwd, &[])? };
        environment = next;
    }
}

/// Tries `server` on `environment` again until it answers or refuses; None when the user leaves with Ctrl+C.
fn reconnect(settings: &Settings, environment: &str, server: u32) -> Result<Option<Stream>, String> {
    let leaving = "Ctrl+C leaves; the session keeps running there.";
    crate::tui::notice(&format!("Reconnecting to {environment}... {leaving}"))?;
    let mut wait = Duration::from_secs(1);
    loop {
        let reason = match tailnet::reattach(settings, environment, server) {
            Reattach::Back(stream) => return Ok(Some(stream)),
            Reattach::Refused(refusal) => return Err(refusal),
            Reattach::NotYet(reason) => reason,
        };
        crate::tui::notice(&format!("Reconnecting to {environment}. {leaving} {reason}"))?;
        if crate::tui::ctrl_c_within(wait) {
            return Ok(None);
        }
        wait = (wait * 2).min(MAX_RECONNECT_WAIT);
    }
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
        return Err(format!("{prefix} matches {} running sessions; use a longer id:\n{}", matches.len(), list(&matches.into_iter().map(Listed::local).collect::<Vec<_>>())));
    }
    Ok(matches.pop())
}

/// `agent attach [id]`: a running session here or in an environment, or with an id that none has, the saved one here,
/// like `agent -r <id>`.
pub fn attach_command(args: &[String]) -> Result<(), String> {
    if !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
        return Err("agent attach needs a terminal.".into());
    }
    let home = crate::home()?;
    let settings = crate::settings::load(&home)?;
    let listed = match args {
        [] => pick(listing(&home, &settings))?,
        [id] if !id.starts_with('-') => match find_anywhere(&home, &settings, id)? {
            Some(listed) => listed,
            None => {
                let cwd = std::env::current_dir().map_err(|e| format!("Cannot read the working directory: {e}"))?;
                return show(&home, &settings, LOCAL, || Ok(Stream::Local(start(&home, &["--resume".to_string(), id.clone()], &cwd, false)?)));
            }
        },
        _ => return Err("Usage: agent attach [session id].".into()),
    };
    show(&home, &settings, &listed.environment, || {
        if listed.environment != LOCAL {
            return tailnet::attach(&settings, &listed.environment, &listed.session);
        }
        let peer = find(&home, &listed.session)?.ok_or_else(|| format!("Session {} has just ended.", listed.session))?;
        Ok(Stream::Local(connect(&home, peer.pid)?))
    })
}

/// Running sessions here, then in each environment; the reasons environments did not answer go to stderr.
fn listing(home: &Path, settings: &Settings) -> Vec<Listed> {
    let mut sessions: Vec<Listed> = live(home).into_iter().map(Listed::local).collect();
    let (remote, failures) = tailnet::listing(home, settings);
    for failure in failures {
        eprintln!("{DIM}{failure}{RESET}");
    }
    sessions.extend(remote);
    sessions
}

/// The session whose id starts with `prefix`: here first, so a local one does not wait on asleep computers.
fn find_anywhere(home: &Path, settings: &Settings, prefix: &str) -> Result<Option<Listed>, String> {
    if let Some(peer) = find(home, prefix)? {
        return Ok(Some(Listed::local(peer)));
    }
    let (remote, _) = tailnet::listing(home, settings);
    let (mut reachable, unreachable): (Vec<Listed>, Vec<Listed>) = remote.into_iter().filter(|listed| listed.session.starts_with(prefix)).partition(|listed| listed.reachable);
    if reachable.len() > 1 {
        return Err(format!("{prefix} matches {} running sessions; use a longer id:\n{}", reachable.len(), list(&reachable)));
    }
    if let Some(gone) = unreachable.first().filter(|_| reachable.is_empty()) {
        return Err(format!("Session {} ran on {}, which does not answer now.", gone.session, gone.environment));
    }
    Ok(reachable.pop())
}

fn pick(sessions: Vec<Listed>) -> Result<Listed, String> {
    let mut reachable: Vec<Listed> = sessions.iter().filter(|listed| listed.reachable).cloned().collect();
    if reachable.is_empty() {
        let offline = if sessions.is_empty() { String::new() } else { format!("\n{}", list(&sessions)) };
        return Err(format!("No session is running. Start one with `agent`, or open a saved one with `agent -r`.{offline}"));
    }
    if sessions.len() == 1 {
        return Ok(reachable.remove(0));
    }
    eprint!("{}\nAttach to [1-{}]: ", list(&sessions), reachable.len());
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer).map_err(|e| format!("Cannot read the answer: {e}"))?;
    let index = answer.trim().parse::<usize>().ok().filter(|n| (1..=reachable.len()).contains(n)).ok_or_else(|| format!("{} is not on the list.", answer.trim()))?;
    Ok(reachable.remove(index - 1))
}

const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

/// Numbers the sessions `pick` takes and greys out the rest; the environment column shows once one is remote.
fn list(sessions: &[Listed]) -> String {
    let tagged = sessions.iter().any(|listed| listed.environment != LOCAL);
    let width = sessions.iter().map(|listed| listed.environment.chars().count()).max().unwrap_or(0);
    let mut lines = Vec::new();
    let mut number = 0;
    for listed in sessions {
        let state = if !listed.reachable {
            "offline"
        } else if listed.busy {
            "working"
        } else {
            "idle"
        };
        let title: String = listed.title.chars().filter(|c| !c.is_control()).collect();
        let cwd: String = listed.cwd.chars().filter(|c| !c.is_control()).collect();
        let session: String = listed.session.chars().filter(|c| !c.is_control()).collect();
        let environment = if tagged { format!("{:<width$}  ", listed.environment) } else { String::new() };
        let row = format!("{environment}{session}  {state:<7}  {cwd}  {title}");
        if listed.reachable {
            number += 1;
            lines.push(format!("{number:>3}  {row}"));
        } else {
            lines.push(format!("{DIM}     {row}{RESET}"));
        }
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
        assert_eq!(list(&[Listed::local(peer)]), "  1  abc  working  /p  t]0;x");
    }

    #[test]
    fn remote_sessions_are_tagged_and_offline_ones_greyed_out_without_a_number() {
        let listed = |environment: &str, session: &str, reachable| Listed { environment: environment.into(), session: session.into(), title: "t".into(), cwd: "/p".into(), busy: false, reachable };
        let rows = list(&[listed(LOCAL, "a", true), listed("studio", "b", false), listed("studio", "c", true)]);
        assert_eq!(rows, format!("  1  local   a  idle     /p  t\n{DIM}     studio  b  offline  /p  t{RESET}\n  2  studio  c  idle     /p  t"));
    }
}
