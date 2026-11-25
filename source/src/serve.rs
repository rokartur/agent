//! The terminal side of a session server: `agent` starts one per session and attaches to it, `agent attach`
//! returns to a running one. Each server is an `agent serve` process listening on `~/.agents/serve/<pid>.sock`;
//! `crate::tailnet` reaches the ones on other computers. A session starts and finds its managed sessions here too.

use std::io::{BufRead, BufReader, IsTerminal, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use crate::agent::peers::{self, Peer};
use crate::local_socket;
use crate::settings::Settings;
use crate::tailnet::{self, LOCAL, Reattach};
use crate::tui::{Ending, Place};

/// Set on a session server a remote start began, and inherited by its subagents: the Tailscale name of the computer
/// the user's terminal is on.
pub const USER_DEVICE: &str = "AGENT_USER_DEVICE";

/// A session server this terminal reaches: its socket on this computer, or `agent serve --tailnet` on another one.
pub enum Stream {
    Local(local_socket::Stream),
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
    /// The address of the session that started it with StartSession, or empty.
    pub started_by: String,
}

impl Listed {
    fn local(peer: Peer) -> Listed {
        Listed { environment: LOCAL.into(), session: peer.session, title: peer.title, cwd: peer.cwd, busy: peer.busy, reachable: true, started_by: peer.started_by }
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
    let file = crate::process::private_file().create(true).write(true).truncate(true).open(&path).map_err(|e| format!("Cannot open {}: {e}", path.display()))?;
    crate::process::redirect_output_to(file).map_err(|e| format!("Cannot write to {}: {e}", path.display()))
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
/// the message from it as `agent` always did. `user_device` names the computer of a remote start's terminal.
pub fn start(home: &Path, args: &[String], cwd: &Path, piped: bool, user_device: Option<&str>) -> Result<local_socket::Stream, String> {
    launch(home, args, cwd, piped, user_device).map(|(_, stream)| stream)
}

/// A session to start for another session, which then gets each of its answers (ADR 0004).
pub struct Managed<'a> {
    pub cwd: &'a Path,
    /// Starts it in a new git worktree of `cwd` by this name.
    pub worktree: Option<&'a str>,
    pub title: &'a str,
    pub model: &'a str,
    pub thinking: &'a str,
    pub full_access: bool,
    /// The starting session as this computer addresses it: its id, or `<computer>:<id>`.
    pub started_by: &'a str,
    pub prompt: &'a str,
}

/// The orchestrator's reviewer refuses a StartSession call over 16000 characters unread, and Windows passes one
/// command line of at most 32767 with every quote doubled: a prompt this long fits both with its JSON escapes.
const MAX_PROMPT_BYTES: usize = 12_000;
const MAX_TITLE_CHARS: usize = 80;

/// Starts a session with no terminal. `managed.prompt` is its first prompt, typed as a subagent's task is, so its
/// reviewer has a request to judge calls against; later messages from its orchestrator stay follow-ups.
pub fn start_managed(home: &Path, managed: &Managed, user_device: Option<&str>) -> Result<Peer, String> {
    check_request(managed)?;
    if let Some(max) = crate::settings::load(home)?.max_sessions
        && live(home).iter().filter(|peer| peer.started_by == managed.started_by).count() >= max
    {
        return Err(format!("{max} sessions you started already run on this computer, the limit in subagents.max_sessions. Stop one with TaskStop first."));
    }
    check_project(home, managed.cwd)?;
    let created = managed.worktree.map(|name| crate::worktree::create(managed.cwd, name)).transpose()?;
    let cwd = created.as_ref().map_or(managed.cwd, |created| &created.path);
    let started = launch(home, &managed_args(managed), cwd, false, user_device).and_then(|(pid, _)| listed_or_stop(home, pid));
    // Nothing runs in it: a server that fails to start or to list itself is stopped. A kept branch would refuse
    // the same name on a retry.
    if let (Err(error), Some(created)) = (&started, &created)
        && let Err(kept) = crate::worktree::discard(managed.cwd, created)
    {
        return Err(format!("{error} Its worktree {} stays: {kept}", created.path.display()));
    }
    started
}

/// What refuses a StartSession on any computer: the orchestrator's checks it before a project is copied to another.
pub fn check_request(managed: &Managed) -> Result<(), String> {
    // The session submits its first prompt as typed text, where `!` runs a shell command unasked and `/` a command.
    if managed.prompt.trim_start().starts_with(['!', '/']) {
        return Err("The prompt cannot start with ! or /: the new session would run it as a command.".into());
    }
    if managed.prompt.trim().is_empty() || managed.prompt.len() > MAX_PROMPT_BYTES {
        return Err(format!("The prompt must be 1 to {MAX_PROMPT_BYTES} bytes; put longer detail in a file and name it in the prompt."));
    }
    // The title is an option's value, which cannot start with a dash, and a line in every ListAgents.
    if managed.title.trim().is_empty() || managed.title.chars().count() > MAX_TITLE_CHARS || managed.title.starts_with('-') || managed.title.chars().any(char::is_control) {
        return Err(format!("The title must be one line of 1 to {MAX_TITLE_CHARS} characters that does not start with a dash."));
    }
    Ok(())
}

/// A session in `auto` edits inside its directory unreviewed, so that directory is in one git project, and not the
/// one a home directory under git makes of every directory in it.
pub fn check_project(home: &Path, cwd: &Path) -> Result<(), String> {
    if !cwd.is_dir() {
        return Err(format!("{} is not a directory on this computer.", cwd.display()));
    }
    let root = crate::agent::prompt::project_root(cwd);
    let home = crate::process::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    if !root.join(".git").exists() || home.starts_with(&root) {
        return Err(format!("{} is not inside a git project of its own. A session edits its directory unreviewed, so name a project in `cwd`.", cwd.display()));
    }
    Ok(())
}

fn managed_args(managed: &Managed) -> Vec<String> {
    // Never above the orchestrator: full access only when it has it, auto for the rest (ADR 0004).
    let mode = if managed.full_access { "--full-access" } else { "--auto" };
    // `--` keeps a prompt that starts with a dash from being read as an option.
    ["--model", managed.model, "--thinking", managed.thinking, mode, "--started-by", managed.started_by, "--title", managed.title, "--", managed.prompt].map(String::from).to_vec()
}

fn listed_or_stop(home: &Path, pid: u32) -> Result<Peer, String> {
    if let Some(peer) = live(home).into_iter().find(|peer| peer.pid == pid) {
        return Ok(peer);
    }
    let _ = stop(home, pid);
    Err("The session started but did not list itself, so it was stopped.".into())
}

/// The running session `id` names, when the session at `started_by` started it: a session stops only its own.
pub fn find_managed(home: &Path, id: &str, started_by: &str) -> Result<Peer, String> {
    let peer = find(home, id)?.ok_or_else(|| format!("No session {id} runs here."))?;
    if peer.started_by != started_by {
        return Err(format!("Session {} was not started by this session, so TaskStop leaves it alone.", peer.session));
    }
    Ok(peer)
}

fn launch(home: &Path, args: &[String], cwd: &Path, piped: bool, user_device: Option<&str>) -> Result<(u32, local_socket::Stream), String> {
    let exe = std::env::current_exe().map_err(|e| format!("Cannot find the agent binary: {e}"))?;
    let pid = start_server(&exe, args, cwd, piped, user_device)?;
    let (socket, log) = (socket(home, pid), log(home, pid));
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        if let Ok(stream) = local_socket::connect(&socket) {
            return Ok((pid, stream));
        }
        if !crate::process::alive(pid) {
            let output = std::fs::read_to_string(&log).unwrap_or_default();
            let _ = std::fs::remove_file(&log);
            let output = output.trim();
            return Err(if output.is_empty() { "The session server stopped before it listened.".into() } else { output.trim_start_matches("error: ").to_string() });
        }
        if Instant::now() > deadline {
            // Left running, it would listen later and work on its prompt with nothing attached to it.
            let _ = crate::process::signal_process(pid, crate::process::SIGTERM);
            return Err(format!("The session server {pid} did not listen within {} s and was stopped; its output is in {}.", START_TIMEOUT.as_secs(), log.display()));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// sh starts the server in the background and exits, so the server is no child of ours: on SIGHUP, closing the
/// terminal, `process::hang_up_children` would end it. An async list's stdin is /dev/null unless redirected.
#[cfg(unix)]
fn start_server(exe: &Path, args: &[String], cwd: &Path, piped: bool, user_device: Option<&str>) -> Result<u32, String> {
    let mut command = crate::process::command("sh");
    command.args(["-c", r#""$0" serve "$@" <&0 >/dev/null 2>&1 & echo $!"#]).arg(exe).args(args).current_dir(cwd).stdout(Stdio::piped()).stderr(Stdio::null());
    if piped {
        command.stdin(Stdio::inherit());
    }
    if let Some(device) = user_device {
        command.env(USER_DEVICE, device);
    }
    let output = command.output().map_err(|e| format!("Cannot start the session server: {e}"))?;
    String::from_utf8_lossy(&output.stdout).trim().parse().map_err(|_| format!("Cannot start the session server: sh exited with {}.", output.status))
}

/// The server leaves this terminal's job, so closing the terminal does not end it, and runs on a hidden console.
#[cfg(windows)]
fn start_server(exe: &Path, args: &[String], cwd: &Path, piped: bool, user_device: Option<&str>) -> Result<u32, String> {
    let mut command = crate::process::command(exe);
    command.arg("serve").args(args).current_dir(cwd).stdout(Stdio::null()).stderr(Stdio::null());
    if piped {
        command.stdin(Stdio::inherit());
    }
    if let Some(device) = user_device {
        command.env(USER_DEVICE, device);
    }
    let child = crate::process::spawn_detached(&mut command).map_err(|e| format!("Cannot start the session server: {e}"))?;
    Ok(child.id())
}

pub fn connect(home: &Path, pid: u32) -> Result<local_socket::Stream, String> {
    local_socket::connect(&socket(home, pid)).map_err(|e| format!("Cannot reach session server {pid}: {e}"))
}

/// Stops the session server `pid`. On Unix it ends its turn and children on SIGTERM (`process::exit_on_signal`);
/// Windows terminates it, and its job object ends the children.
pub fn stop(home: &Path, pid: u32) -> Result<(), String> {
    // A server killed without cleanup leaves its socket while its pid may already be another program's: only one
    // still accepting on that socket is signalled.
    connect(home, pid)?;
    crate::process::signal_process(pid, crate::process::SIGTERM).map_err(|e| e.to_string())
}

/// Shows the session until it ends or this terminal detaches, then prints what the server said goodbye with. /env
/// leaves it for a new session in another environment, which this terminal then shows. A remote session whose
/// connection drops stays on screen while this terminal reconnects to it.
/// `connect` runs in raw mode: keys typed meanwhile would otherwise reach the session cooked, Enter as the LF that
/// raw mode reads as Ctrl+J, a newline in the editor.
pub fn show(home: &Path, environment: &str, connect: impl FnOnce() -> Result<Stream, String>) -> Result<(), String> {
    let fullscreen = crate::tui::Fullscreen::enter()?;
    if environment != LOCAL {
        crate::tui::notice(&format!("Connecting to {environment}..."))?;
    }
    let (mut environment, mut stream) = (environment.to_string(), connect()?);
    loop {
        // Read again each time: /env in a session on another computer may have removed a saved environment here.
        let settings = &crate::settings::load(home)?;
        let place = Place { environment: environment.clone(), environments: tailnet::environments(settings), saved: settings.environments.clone() };
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
            Ending::Attach(server) if environment == LOCAL => {
                stream = Stream::Local(self::connect(home, server)?);
                continue;
            }
            Ending::Attach(server) => {
                stream = match tailnet::reattach(settings, &environment, server) {
                    Reattach::Back(stream) => stream,
                    Reattach::Refused(reason) | Reattach::NotYet(reason) => return Err(reason),
                };
                continue;
            }
            // The server drops a terminal that stops reading for 5 s, e.g. behind a locked screen, and runs on.
            Ending::Lost(Some(server)) if environment == LOCAL => {
                stream = Stream::Local(listening_again(home, server)?);
                continue;
            }
            Ending::Lost(None) if environment == LOCAL => return Err("The session server stopped.".into()),
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
        stream = if next == LOCAL { Stream::Local(start(home, &[], &cwd, false, None)?) } else { tailnet::start(settings, &next, &cwd, &[])? };
        environment = next;
    }
}

/// /reload execs the server on the same pid, which listens again once the new binary has started.
pub fn listening_again(home: &Path, server: u32) -> Result<local_socket::Stream, String> {
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        if let Ok(stream) = connect(home, server) {
            return Ok(stream);
        }
        if !crate::process::alive(server) || Instant::now() > deadline {
            return Err(format!("The session server stopped; its output is in {}.", log(home, server).display()));
        }
        std::thread::sleep(Duration::from_millis(20));
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
                return show(&home, LOCAL, || Ok(Stream::Local(start(&home, &["--resume".to_string(), id.clone()], &cwd, false, None)?)));
            }
        },
        _ => return Err("Usage: agent attach [session id].".into()),
    };
    show(&home, &listed.environment, || {
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
        // A server ended by TerminateProcess or SIGKILL never ran the cleanup that removes its tool outputs.
        let _ = std::fs::remove_dir_all(crate::tools::spill_dir_of(pid));
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
        let spills = crate::tools::spill_dir_of(dead);
        std::fs::create_dir_all(&spills).unwrap();
        std::fs::write(spills.join("out.txt"), "tool output").unwrap();
        sweep(&home);
        assert_eq!([socket.exists(), log.exists(), live_socket.exists(), old_log.exists(), spills.exists()], [false, true, true, false, false]);
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn a_lost_terminal_waits_for_its_reloaded_server_to_listen_again() {
        let home = std::env::temp_dir().join(format!("agent-serve-reload-{}", std::process::id()));
        std::fs::create_dir_all(dir(&home)).unwrap();
        let path = socket(&home, std::process::id());
        let listener = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            local_socket::bind(&path).unwrap()
        });
        let reconnected = listening_again(&home, std::process::id());
        let _listener = listener.join().unwrap();
        assert!(reconnected.is_ok(), "{:?}", reconnected.err());
        assert!(listening_again(&home, 999_999_999).unwrap_err().starts_with("The session server stopped"));
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn stopping_a_server_whose_socket_is_a_leftover_signals_nothing() {
        let home = std::env::temp_dir().join(format!("agent-serve-stop-{}", std::process::id()));
        std::fs::create_dir_all(dir(&home)).unwrap();
        // This test's own pid stands for a pid reused by an unrelated program: a signal would end the test run.
        let pid = std::process::id();
        std::fs::write(socket(&home, pid), "").unwrap();
        let error = stop(&home, pid).unwrap_err();
        assert!(error.starts_with("Cannot reach session server"), "{error}");
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn a_session_starts_at_most_max_sessions_and_finds_only_its_own() {
        let home = std::env::temp_dir().join(format!("agent-serve-managed-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(dir(&home)).unwrap();
        std::fs::create_dir_all(peers::dir(&home)).unwrap();
        let max_sessions = 3;
        std::fs::write(crate::settings::path(&home), serde_json::json!({"subagents": {"max_sessions": max_sessions}}).to_string()).unwrap();
        fn managed<'a>(cwd: &'a Path, prompt: &'a str, title: &'a str, full_access: bool) -> Managed<'a> {
            Managed { cwd, worktree: None, title, model: "m", thinking: "high", full_access, started_by: "orch1", prompt }
        }
        let start = |prompt: &str, title: &str| start_managed(&home, &managed(&home, prompt, title, false), None).map(|peer| peer.session).unwrap_err();
        // The mode follows the orchestrator's, never the model's input.
        assert_eq!(managed_args(&managed(&home, "fix it", "t", false))[4], "--auto");
        assert_eq!(managed_args(&managed(&home, "fix it", "t", true))[4], "--full-access");
        let long = "x".repeat(MAX_PROMPT_BYTES + 1);
        let long_title = "t".repeat(MAX_TITLE_CHARS + 1);
        for (prompt, title, refusal) in [
            ("!rm -rf ~", "t", "The prompt cannot start"),
            (" /quit", "t", "The prompt cannot start"),
            ("  ", "t", "The prompt must be"),
            (&long, "t", "The prompt must be"),
            ("fix it", " ", "The title must be"),
            ("fix it", "--full-access", "The title must be"),
            ("fix it", "a\nb", "The title must be"),
            ("fix it", &long_title, "The title must be"),
        ] {
            assert!(start(prompt, title).starts_with(refusal), "{prompt:.20}: {}", start(prompt, title));
        }

        // Sleeping children stand for running sessions: a card needs a live pid of its own.
        let mut sleepers: Vec<_> = (0..max_sessions).map(|_| crate::process::command("sleep").arg("30").spawn().unwrap()).collect();
        let card = |n: usize, pid: u32, started_by: &str| {
            std::fs::write(peers::dir(&home).join(format!("{pid}.json")), serde_json::json!({"pid": pid, "session": format!("s{n}"), "started_by": started_by}).to_string()).unwrap();
            std::fs::write(socket(&home, pid), "").unwrap();
        };
        for (n, sleeper) in sleepers.iter().enumerate() {
            card(n, sleeper.id(), if n == 0 { "orch2" } else { "orch1" });
        }
        let owned = |id: &str| find_managed(&home, id, "orch1").map(|peer| peer.pid);
        assert_eq!(owned("s1"), Ok(sleepers[1].id()));
        assert_eq!(owned("s0").unwrap_err(), "Session s0 was not started by this session, so TaskStop leaves it alone.");
        assert_eq!(owned("zz").unwrap_err(), "No session zz runs here.");
        // One short of the cap leaves room: at the cap the next start is refused before anything starts.
        card(0, sleepers[0].id(), "orch1");
        assert_eq!(start("fix it", "t"), "3 sessions you started already run on this computer, the limit in subagents.max_sessions. Stop one with TaskStop first.");

        for sleeper in &mut sleepers {
            sleeper.kill().unwrap();
            sleeper.wait().unwrap();
        }
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn a_session_starts_only_inside_a_git_project_whose_root_is_not_the_home_directory() {
        let home = std::env::temp_dir().join(format!("agent-serve-project-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join("repo/.git")).unwrap();
        std::fs::create_dir_all(home.join("repo/src")).unwrap();
        std::fs::create_dir_all(home.join("plain")).unwrap();
        assert_eq!(check_project(&home, &home.join("repo/src")), Ok(()));
        assert!(check_project(&home, &home.join("plain")).unwrap_err().contains("is not inside a git project of its own"));
        assert!(check_project(&home, &home.join("repo/gone")).unwrap_err().ends_with("is not a directory on this computer."));
        // A home directory under git would make a project of every directory in it.
        std::fs::create_dir_all(home.join(".git")).unwrap();
        assert!(check_project(&home, &home.join("plain")).is_err());
        assert!(check_project(&home, &home).is_err());
        assert_eq!(check_project(&home, &home.join("repo")), Ok(()));
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn the_list_strips_control_characters_from_cards() {
        let peer = Peer { pid: 1, session: "abc".into(), title: "t\x1b]0;x\x07".into(), cwd: "/p".into(), busy: true, started_by: String::new(), first_session: String::new(), socket: PathBuf::new() };
        assert_eq!(list(&[Listed::local(peer)]), "  1  abc  working  /p  t]0;x");
    }

    #[test]
    fn remote_sessions_are_tagged_and_offline_ones_greyed_out_without_a_number() {
        let listed = |environment: &str, session: &str, reachable| Listed {
            environment: environment.into(),
            session: session.into(),
            title: "t".into(),
            cwd: "/p".into(),
            busy: false,
            reachable,
            started_by: String::new(),
        };
        let rows = list(&[listed(LOCAL, "a", true), listed("studio", "b", false), listed("studio", "c", true)]);
        assert_eq!(rows, format!("  1  local   a  idle     /p  t\n{DIM}     studio  b  offline  /p  t{RESET}\n  2  studio  c  idle     /p  t"));
    }
}
