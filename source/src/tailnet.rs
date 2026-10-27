//! Environments on the other computers of one tailnet (ADR 0002). `agent serve --tailnet` accepts this user's other
//! devices on this computer's Tailscale addresses, then lists, attaches to or starts the session servers here and
//! relays the wire between the TCP connection and the session's socket. `--on`, /env and `agent attach` are its clients.
//!
//! Each connection sends one request line, `{"list": true}`, `{"attach": id}`, `{"start": {"cwd", "args"}}` or
//! `{"reattach": pid}` for the session server a dropped connection showed, with
//! `protocol` and `version`, and gets one reply line carrying the listener's own `protocol` and `version`: the
//! sessions, `{"ok": true}`, or `{"error": ..}`. After an ok the connection carries the session's wire unchanged.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

use crate::agent::peers;
use crate::serve::{self, Listed, Stream};
use crate::settings::Settings;
use crate::tui::{PROTOCOL, SILENCE};

pub const LOCAL: &str = "local";
/// `--on auto`: the least loaded environment that has the directory, see `place`.
pub const AUTO: &str = "auto";
const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Reached through the Mac app when its CLI is not on PATH, as under launchd.
const MACOS_APP_CLI: &str = "/Applications/Tailscale.app/Contents/MacOS/Tailscale";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const LIST_TIMEOUT: Duration = Duration::from_secs(5);
/// The host waits up to 10 s for the session it delivers a message to (`peers::deliver`).
const MESSAGE_REPLY_TIMEOUT: Duration = Duration::from_secs(15);
/// A start waits up to `serve::START_TIMEOUT` on the host for the session server to listen.
const START_REPLY_TIMEOUT: Duration = Duration::from_secs(75);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_REQUEST_BYTES: u64 = 1 << 20;
/// A relay that cannot write to its peer for this long gives up, so the session server sees the terminal detach.
const RELAY_WRITE_TIMEOUT: Duration = Duration::from_secs(30);
/// A peer that went away without closing, asleep or off the network, is dropped after about 30 + 3 * 10 s.
const KEEPALIVE_IDLE_SECS: libc::c_int = 30;
const KEEPALIVE_INTERVAL_SECS: libc::c_int = 10;
const KEEPALIVE_PROBES: libc::c_int = 3;

/// `local` first, then the computers in `environments`, then this user's other computers online on the tailnet.
pub fn environments(settings: &Settings) -> Vec<String> {
    // Without Tailscale, or with it stopped, only the computers in `environments` are offered.
    let found = tailscale(&["status", "--json"]).map(|status| discovered(&status)).unwrap_or_default();
    let mut names = vec![LOCAL.to_string()];
    for name in settings.environments.iter().chain(&found) {
        if !names.contains(name) {
            names.push(name.clone());
        }
    }
    names
}

/// The peers in `tailscale status --json` a listener there would admit this computer from, as `owned_by` decides,
/// that are online and run macOS or Linux, named by the first label of their MagicDNS name.
fn discovered(status: &Value) -> Vec<String> {
    let Some(owner) = status["Self"]["UserID"].as_u64().filter(|_| status["BackendState"] == "Running") else { return Vec::new() };
    let mut names = Vec::new();
    for peer in status["Peer"].as_object().into_iter().flat_map(|peers| peers.values()) {
        let tagged = peer["Tags"].as_array().is_some_and(|tags| !tags.is_empty());
        let desktop = matches!(peer["OS"].as_str(), Some("macOS" | "linux"));
        if peer["Online"] != true || peer["UserID"].as_u64() != Some(owner) || tagged || !desktop {
            continue;
        }
        if let Some(name) = peer["DNSName"].as_str().and_then(|dns| dns.split('.').next()).filter(|name| !name.is_empty()) {
            names.push(name.to_string());
        }
    }
    names.sort();
    names
}

pub fn check(settings: &Settings, name: &str) -> Result<(), String> {
    if name == LOCAL || name == AUTO {
        return Ok(());
    }
    let names = environments(settings);
    if names.iter().any(|known| known == name) {
        return Ok(());
    }
    Err(format!("No environment {name}. It must be one of your computers, online on the tailnet and running `agent serve --tailnet` (`agent service install`); now there are: {}.", names.join(", ")))
}

/// A new session on `environment` in `cwd`, or in the host's home when `cwd` does not exist there.
pub fn start(settings: &Settings, environment: &str, cwd: &Path, args: &[String]) -> Result<Stream, String> {
    let (reader, writer, _) = request(settings, environment, json!({"start": {"cwd": cwd, "args": args}}), START_REPLY_TIMEOUT)?;
    Ok(Stream::Remote(reader, writer))
}

/// Sends `text` from session `from` here to the session `to` names on `environment`, and says how it went.
pub fn message(settings: &Settings, environment: &str, to: &str, from: &str, from_cwd: &Path, text: &str) -> Result<String, String> {
    let body = json!({"message": {"to": to, "from": from, "from_cwd": from_cwd, "text": text}});
    let (_, _, reply) = request(settings, environment, body, MESSAGE_REPLY_TIMEOUT)?;
    let sent = reply["sent"].as_str().ok_or_else(|| format!("{environment} sent no answer to the message."))?;
    Ok(format!("{sent} (on {environment})"))
}

/// Where `--on auto` starts a session in `cwd`, and why: of `local` and the environments that answer and have `cwd`,
/// the one with the lowest load per CPU. All are asked at once, so one that does not answer costs at most
/// `LIST_TIMEOUT`.
pub fn place(settings: &Settings, cwd: &Path) -> (String, String) {
    let remote = &environments(settings)[1..];
    let mut loads = vec![(LOCAL.to_string(), load())];
    std::thread::scope(|scope| {
        let asked: Vec<_> = remote.iter().map(|environment| (environment, scope.spawn(move || request(settings, environment, json!({"list": true, "cwd": cwd}), LIST_TIMEOUT)))).collect();
        for (environment, answer) in asked {
            let Ok((_, _, reply)) = answer.join().expect("the load thread") else { continue };
            // A listener older than `--on auto` sends neither, and is left out.
            if let Some(load) = reply["load"].as_f64().filter(|_| reply["has_cwd"] == true) {
                loads.push((environment.clone(), load));
            }
        }
    });
    let (chosen, _) = least_loaded(&loads);
    let shown: Vec<String> = loads.iter().map(|(environment, load)| format!("{environment} {load:.2}")).collect();
    (chosen.clone(), format!("Auto placement started this session on {chosen}, the least loaded computer with {}; load per CPU: {}.", cwd.display(), shown.join(", ")))
}

/// The first of `loads` with the lowest load, so `local`, listed first, wins a tie.
fn least_loaded(loads: &[(String, f64)]) -> &(String, f64) {
    let mut least = &loads[0];
    for candidate in &loads[1..] {
        if candidate.1 < least.1 {
            least = candidate;
        }
    }
    least
}

/// This computer's 1-minute load average per CPU.
fn load() -> f64 {
    let mut averages = [0.0];
    // SAFETY: getloadavg writes at most the one sample asked for into `averages`.
    unsafe { libc::getloadavg(averages.as_mut_ptr(), 1) };
    averages[0] / std::thread::available_parallelism().map_or(1, usize::from) as f64
}

pub fn attach(settings: &Settings, environment: &str, session: &str) -> Result<Stream, String> {
    let (reader, writer, _) = request(settings, environment, json!({"attach": session}), LIST_TIMEOUT)?;
    Ok(Stream::Remote(reader, writer))
}

/// How an attempt to get back to a session after a dropped connection went.
pub enum Reattach {
    Back(Stream),
    /// The host answered no, so trying again does not help.
    Refused(String),
    /// The host did not answer, or not as itself; why.
    NotYet(String),
}

pub fn reattach(settings: &Settings, environment: &str, server: u32) -> Reattach {
    match ask(settings, environment, json!({"reattach": server}), LIST_TIMEOUT) {
        Err(failure) => Reattach::NotYet(failure),
        Ok((reader, writer, reply)) => match refusal(environment, &reply) {
            Some(refusal) => Reattach::Refused(refusal),
            None => Reattach::Back(Stream::Remote(reader, writer)),
        },
    }
}

/// The running sessions of every environment but `local`, asked at once, and why the ones in `environments` that did
/// not answer did not; those show the sessions they had when they last answered, kept in `~/.agents/environments.json`.
/// A discovered computer that does not answer is left out: most do not run the listener.
pub fn listing(home: &Path, settings: &Settings) -> (Vec<Listed>, Vec<String>) {
    listing_of(home, settings, &environments(settings)[1..])
}

fn listing_of(home: &Path, settings: &Settings, remote: &[String]) -> (Vec<Listed>, Vec<String>) {
    if remote.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let answers: Vec<(&String, Result<Vec<Value>, String>)> = std::thread::scope(|scope| {
        let asked: Vec<_> = remote.iter().map(|environment| (environment, scope.spawn(move || sessions(settings, environment)))).collect();
        asked.into_iter().map(|(environment, answer)| (environment, answer.join().expect("the listing thread"))).collect()
    });
    let path = home.join(".agents/environments.json");
    let mut seen = std::fs::read_to_string(&path).ok().and_then(|text| serde_json::from_str::<Value>(&text).ok()).filter(Value::is_object).unwrap_or_else(|| json!({}));
    let (mut listed, mut failures, mut answered) = (Vec::new(), Vec::new(), false);
    for (environment, answer) in answers {
        let (sessions, reachable) = match answer {
            Ok(sessions) => {
                seen[environment] = json!(sessions);
                answered = true;
                (sessions, true)
            }
            Err(failure) if settings.environments.contains(environment) => {
                failures.push(failure);
                (seen[environment].as_array().cloned().unwrap_or_default(), false)
            }
            Err(_) => continue,
        };
        listed.extend(sessions.iter().filter_map(|session| entry(environment, session, reachable)));
    }
    if answered {
        let _ = std::fs::write(&path, seen.to_string());
    }
    (listed, failures)
}

fn sessions(settings: &Settings, environment: &str) -> Result<Vec<Value>, String> {
    let (_, _, reply) = request(settings, environment, json!({"list": true}), LIST_TIMEOUT)?;
    reply["sessions"].as_array().cloned().ok_or_else(|| format!("{environment} sent no session list."))
}

fn entry(environment: &str, session: &Value, reachable: bool) -> Option<Listed> {
    let text = |key: &str| session[key].as_str().unwrap_or_default().to_string();
    Some(Listed { environment: environment.into(), session: session["session"].as_str()?.into(), title: text("title"), cwd: text("cwd"), busy: session["busy"].as_bool().unwrap_or(false), reachable })
}

/// Asks `environment` for `body` and returns the connection and the reply, or why it refused.
fn request(settings: &Settings, environment: &str, body: Value, timeout: Duration) -> Result<(BufReader<TcpStream>, TcpStream, Value), String> {
    let (reader, stream, reply) = ask(settings, environment, body, timeout)?;
    if let Some(refusal) = refusal(environment, &reply) {
        return Err(refusal);
    }
    Ok((reader, stream, reply))
}

/// Connects to `environment`, checks the computer there is this user's, sends `body` and reads the reply.
fn ask(settings: &Settings, environment: &str, body: Value, timeout: Duration) -> Result<(BufReader<TcpStream>, TcpStream, Value), String> {
    let stream = connect(environment, settings.tailnet_port)?;
    let host = stream.peer_addr().map_err(|e| format!("Cannot reach {environment}: {e}"))?;
    // Keys typed into a session reach the host, so a host owned by someone else must not get them.
    same_owner(host.ip(), own_user()?).map_err(|refusal| format!("{environment} is not your computer ({refusal}), so agent does not connect to it."))?;
    exchange(stream, environment, body, timeout)
}

fn connect(environment: &str, port: u16) -> Result<TcpStream, String> {
    let addresses = (environment, port).to_socket_addrs().map_err(|e| format!("Cannot find {environment} on the tailnet: {e}."))?;
    let mut failure = None;
    for address in addresses {
        match TcpStream::connect_timeout(&address, CONNECT_TIMEOUT) {
            Ok(stream) => return Ok(stream),
            Err(error) => failure = Some(error),
        }
    }
    let failure = failure.map_or_else(|| "no address".to_string(), |error| error.to_string());
    Err(format!("{environment} does not answer on port {port} ({failure}); is it awake and running `agent serve --tailnet`?"))
}

fn exchange(stream: TcpStream, environment: &str, mut body: Value, timeout: Duration) -> Result<(BufReader<TcpStream>, TcpStream, Value), String> {
    for_keystrokes(&stream);
    body["protocol"] = json!(PROTOCOL);
    body["version"] = json!(VERSION);
    writeln!(&stream, "{body}").map_err(|e| format!("Cannot reach {environment}: {e}"))?;
    stream.set_read_timeout(Some(timeout)).map_err(|e| format!("Cannot reach {environment}: {e}"))?;
    let mut reader = BufReader::new(stream.try_clone().map_err(|e| format!("Cannot reach {environment}: {e}"))?);
    let mut line = String::new();
    reader.read_line(&mut line).map_err(|e| format!("{environment} did not answer: {e}"))?;
    if line.is_empty() {
        return Err(format!("{environment} closed the connection without an answer; its `agent serve --tailnet` may have just stopped."));
    }
    // After a reply a session sends at least a ping every `PING_EVERY`, so a read that waits longer finds the link gone.
    stream.set_read_timeout(Some(SILENCE)).map_err(|e| format!("Cannot reach {environment}: {e}"))?;
    let reply: Value = serde_json::from_str(&line).map_err(|_| format!("{environment} answered, but not as agent serve --tailnet."))?;
    Ok((reader, stream, reply))
}

/// Why `environment` refused, naming the side to update when the two releases cannot talk.
fn refusal(environment: &str, reply: &Value) -> Option<String> {
    if let Some(theirs) = reply["protocol"].as_u64().filter(|theirs| *theirs != PROTOCOL) {
        let version = reply["version"].as_str().unwrap_or("unknown");
        return Some(if theirs > PROTOCOL {
            format!("{environment} runs agent v{version}, which v{VERSION} here cannot talk to. Update this computer to v{version} with `agent update`.")
        } else {
            format!("{environment} runs agent v{version}, which v{VERSION} here cannot talk to. Update {environment} to v{VERSION}: run `agent update` there.")
        });
    }
    reply["error"].as_str().map(str::to_string)
}

/// `agent serve --tailnet`: serves this user's other devices on this computer's Tailscale addresses until killed.
pub fn serve_command(args: &[String]) -> Result<(), String> {
    if !args.is_empty() {
        return Err("Usage: agent serve --tailnet.".into());
    }
    let home = crate::home()?;
    crate::service::trim_log(&home);
    let settings = crate::settings::load(&home)?;
    if !settings.serve_tailnet {
        return Err("agent serve --tailnet runs only with \"serve_tailnet\": true in ~/.agents/settings.json.".into());
    }
    let status = tailscale(&["status", "--json", "--peers=false"])?;
    if status["BackendState"] != "Running" {
        return Err(format!("Tailscale is {}; start it with `tailscale up`.", status["BackendState"].as_str().unwrap_or("not running")));
    }
    let owner = status["Self"]["UserID"].as_u64().ok_or("Tailscale reports no user for this computer.")?;
    let addresses: Vec<IpAddr> = status["Self"]["TailscaleIPs"].as_array().into_iter().flatten().filter_map(|ip| ip.as_str()?.parse().ok()).collect();
    if addresses.is_empty() {
        return Err("Tailscale gives this computer no address yet.".into());
    }
    let mut listeners = Vec::new();
    for address in &addresses {
        let address = SocketAddr::new(*address, settings.tailnet_port);
        listeners.push(TcpListener::bind(address).map_err(|e| format!("Cannot listen on {address}: {e}"))?);
    }
    let login = status["User"][owner.to_string()]["LoginName"].as_str().unwrap_or("this user");
    let shown: Vec<String> = addresses.iter().map(IpAddr::to_string).collect();
    eprintln!("Accepting {login}'s computers on port {} of {}.", settings.tailnet_port, shown.join(" and "));
    std::thread::scope(|scope| {
        for listener in &listeners {
            scope.spawn(|| accept(listener, owner, &home));
        }
    });
    Ok(())
}

fn accept(listener: &TcpListener, owner: u64, home: &Path) {
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let home = home.to_path_buf();
                std::thread::spawn(move || admit(stream, owner, &home));
            }
            Err(error) => {
                eprintln!("Cannot accept a connection: {error}");
                // EMFILE and its kind repeat at once; the pause keeps this loop from spinning on them.
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

fn admit(stream: TcpStream, owner: u64, home: &Path) {
    let Ok(peer) = stream.peer_addr() else { return };
    let device = match same_owner(peer.ip(), owner) {
        Ok(device) => device,
        Err(refusal) => {
            eprintln!("Refused {peer}: {refusal}");
            reply(&stream, json!({"error": "This computer accepts sessions only from its owner's devices."}));
            return;
        }
    };
    handle(stream, home, &device);
}

/// One admitted connection from `device`: reads its request, answers it, and for a session relays the wire until
/// either side ends.
fn handle(stream: TcpStream, home: &Path, device: &str) {
    for_keystrokes(&stream);
    let Ok(clone) = stream.try_clone() else { return };
    let mut reader = BufReader::new(clone);
    let mut line = String::new();
    let _ = stream.set_read_timeout(Some(REQUEST_TIMEOUT));
    if reader.by_ref().take(MAX_REQUEST_BYTES).read_line(&mut line).is_err() {
        return;
    }
    let _ = stream.set_read_timeout(None);
    let request = serde_json::from_str(&line).unwrap_or(Value::Null);
    match answer(home, device, &request) {
        Ok(Answer::Reply(answer)) => reply(&stream, answer),
        Ok(Answer::Session(socket)) => {
            reply(&stream, json!({"ok": true}));
            relay(reader, stream, socket);
        }
        Err(error) => reply(&stream, json!({"error": error})),
    }
}

enum Answer {
    Reply(Value),
    Session(UnixStream),
}

fn answer(home: &Path, device: &str, request: &Value) -> Result<Answer, String> {
    if request["protocol"] != PROTOCOL {
        return Err(format!("This computer runs agent v{VERSION}, which cannot talk to agent v{}.", request["version"].as_str().unwrap_or("unknown")));
    }
    if request["list"] == true {
        let sessions: Vec<Value> = serve::live(home).into_iter().map(|peer| json!({"session": peer.session, "title": peer.title, "cwd": peer.cwd, "busy": peer.busy})).collect();
        let mut listed = json!({"sessions": sessions, "load": load()});
        if let Some(cwd) = request["cwd"].as_str() {
            listed["has_cwd"] = json!(Path::new(cwd).is_absolute() && Path::new(cwd).is_dir());
        }
        return Ok(Answer::Reply(listed));
    }
    if let Some(prefix) = request["attach"].as_str() {
        let peer = serve::find(home, prefix)?.ok_or_else(|| format!("No session {prefix} runs here."))?;
        return serve::connect(home, peer.pid).map(Answer::Session);
    }
    if let Some(server) = request["reattach"].as_u64() {
        let ended = || format!("The session ended: its server {server} no longer runs.");
        let pid = u32::try_from(server).map_err(|_| ended())?;
        return serve::connect(home, pid).map(Answer::Session).map_err(|_| ended());
    }
    if request["start"].is_object() {
        return start_here(home, &request["start"]).map(Answer::Session);
    }
    if request["message"].is_object() {
        return deliver_here(home, device, &request["message"]).map(|sent| Answer::Reply(json!({"ok": true, "sent": sent})));
    }
    Err("This computer's agent does not know that request.".into())
}

/// A SendMessage from a session on `device` to a session here. The sender is named `<device>:<session>`, the address
/// a reply from here reaches it by.
fn deliver_here(home: &Path, device: &str, message: &Value) -> Result<String, String> {
    let to = message["to"].as_str().filter(|to| !to.is_empty()).ok_or("The message names no session.")?;
    let text = message["text"].as_str().filter(|text| !text.is_empty()).ok_or("The message is empty.")?;
    let peer = peers::find(home, to)?.ok_or_else(|| format!("No session {to} runs here."))?;
    let from = format!("{device}:{}", message["from"].as_str().unwrap_or("unknown"));
    peers::send(&peer, &from, Path::new(message["from_cwd"].as_str().unwrap_or("unknown")), text, None)
}

fn start_here(home: &Path, start: &Value) -> Result<UnixStream, String> {
    let args: Vec<String> = start["args"].as_array().into_iter().flatten().filter_map(|arg| Some(arg.as_str()?.to_string())).collect();
    // parse_args prints and exits the process for these, which would end this listener.
    if args.iter().any(|arg| matches!(arg.as_str(), "-h" | "--help" | "--version")) {
        return Err("A remote session takes no --help or --version.".into());
    }
    let options = crate::parse_args(args.clone())?;
    if options.acp || options.serve || options.print || options.json || options.headless || options.parent.is_some() {
        return Err("A remote session is interactive, so it takes no acp, serve, -p, --json, --headless or --parent.".into());
    }
    let (cwd, missing) = start_dir(home, start["cwd"].as_str());
    let mut args = args;
    if let Some(missing) = missing {
        args.extend(["--missing-cwd".to_string(), missing.display().to_string()]);
    }
    crate::local_session(home, &options, &args, &cwd, false)
}

/// Where a remote start runs: the directory it asked for, or home and the asked one when that is not on this computer.
fn start_dir(home: &Path, asked: Option<&str>) -> (PathBuf, Option<PathBuf>) {
    match asked.map(PathBuf::from).filter(|cwd| cwd.is_absolute()) {
        Some(cwd) if cwd.is_dir() => (cwd, None),
        missing => (home.to_path_buf(), missing),
    }
}

fn reply(stream: &TcpStream, mut message: Value) {
    message["protocol"] = json!(PROTOCOL);
    message["version"] = json!(VERSION);
    let _ = writeln!(&*stream, "{message}");
}

/// Copies both ways between the connection and the session's socket; the end of either direction closes both.
fn relay(mut from_peer: BufReader<TcpStream>, peer: TcpStream, session: UnixStream) {
    let (Ok(mut to_peer), Ok(mut from_session)) = (peer.try_clone(), session.try_clone()) else { return };
    let _ = peer.set_write_timeout(Some(RELAY_WRITE_TIMEOUT));
    let mut to_session = session;
    let frames = std::thread::spawn(move || {
        let _ = std::io::copy(&mut from_session, &mut to_peer);
        let _ = to_peer.shutdown(std::net::Shutdown::Both);
    });
    let _ = std::io::copy(&mut from_peer, &mut to_session);
    let _ = to_session.shutdown(std::net::Shutdown::Both);
    let _ = frames.join();
}

fn own_user() -> Result<u64, String> {
    tailscale(&["status", "--json", "--peers=false"])?["Self"]["UserID"].as_u64().ok_or_else(|| "Tailscale reports no user for this computer.".into())
}

/// The name of the device at `ip`, the first label of its MagicDNS name, when it is an untagged device of `owner`.
fn same_owner(ip: IpAddr, owner: u64) -> Result<String, String> {
    let whois = tailscale(&["whois", "--json", &ip.to_string()])?;
    owned_by(&whois, owner)?;
    Ok(whois["Node"]["Name"].as_str().and_then(|dns| dns.split('.').next()).filter(|name| !name.is_empty()).map_or_else(|| ip.to_string(), str::to_string))
}

/// A tagged device belongs to the tailnet rather than a person, and a node shared from another tailnet reports
/// the sharer: only an untagged device of `owner` passes.
fn owned_by(whois: &Value, owner: u64) -> Result<(), String> {
    if whois["Node"]["Tags"].as_array().is_some_and(|tags| !tags.is_empty()) {
        return Err("a tagged device".into());
    }
    match whois["UserProfile"]["ID"].as_u64() {
        Some(user) if user == owner => Ok(()),
        _ => Err(format!("logged in as {}", whois["UserProfile"]["LoginName"].as_str().unwrap_or("someone else"))),
    }
}

fn tailscale(args: &[&str]) -> Result<Value, String> {
    let program = if crate::process::installed("tailscale") || !Path::new(MACOS_APP_CLI).exists() { "tailscale" } else { MACOS_APP_CLI };
    // Without TERM, as under launchd, the app's binary starts its GUI instead unless TAILSCALE_BE_CLI is set.
    let output = crate::process::command(program).args(args).env("TAILSCALE_BE_CLI", "1").output().map_err(|e| format!("Cannot run tailscale: {e}"))?;
    if !output.status.success() {
        return Err(format!("tailscale {} failed: {}", args.join(" "), String::from_utf8_lossy(&output.stderr).trim()));
    }
    serde_json::from_slice(&output.stdout).map_err(|e| format!("tailscale {} printed no JSON: {e}", args.join(" ")))
}

/// Sends each keystroke and frame at once, not held back by Nagle's algorithm, and turns on TCP keepalive, so a
/// read on a connection whose peer vanished fails instead of waiting forever.
fn for_keystrokes(stream: &TcpStream) {
    let _ = stream.set_nodelay(true);
    use std::os::fd::AsRawFd;
    #[cfg(target_os = "macos")]
    const IDLE: libc::c_int = libc::TCP_KEEPALIVE;
    #[cfg(not(target_os = "macos"))]
    const IDLE: libc::c_int = libc::TCP_KEEPIDLE;
    let options = [
        (libc::SOL_SOCKET, libc::SO_KEEPALIVE, 1),
        (libc::IPPROTO_TCP, IDLE, KEEPALIVE_IDLE_SECS),
        (libc::IPPROTO_TCP, libc::TCP_KEEPINTVL, KEEPALIVE_INTERVAL_SECS),
        (libc::IPPROTO_TCP, libc::TCP_KEEPCNT, KEEPALIVE_PROBES),
    ];
    for (level, name, value) in options {
        // SAFETY: setsockopt reads size_of::<c_int>() bytes from `value`, which lives across the call.
        unsafe { libc::setsockopt(stream.as_raw_fd(), level, name, (&raw const value).cast(), std::mem::size_of::<libc::c_int>() as libc::socklen_t) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_start_from_a_directory_missing_here_runs_in_home_and_names_it() {
        let home = std::env::temp_dir();
        assert_eq!(start_dir(&home, home.to_str()), (home.clone(), None));
        assert_eq!(start_dir(&home, Some("/no/such/agent/dir")), (home.clone(), Some(PathBuf::from("/no/such/agent/dir"))));
        assert_eq!(start_dir(&home, Some("relative")), (home.clone(), None));
        assert_eq!(start_dir(&home, None), (home, None));
        assert_eq!(crate::parse_args(vec!["serve".into(), "--missing-cwd".into(), "/x".into()]).unwrap().missing_cwd, Some(PathBuf::from("/x")));
    }

    #[test]
    fn auto_picks_the_least_loaded_computer_and_local_on_a_tie() {
        let loads = |pairs: &[(&str, f64)]| pairs.iter().map(|(name, load)| (name.to_string(), *load)).collect::<Vec<_>>();
        assert_eq!(least_loaded(&loads(&[("local", 0.9), ("studio", 0.2), ("box", 0.5)])).0, "studio");
        assert_eq!(least_loaded(&loads(&[("local", 0.3), ("studio", 0.3)])).0, "local");
        assert_eq!(least_loaded(&loads(&[("local", 0.0)])).0, "local");
    }

    #[test]
    fn a_listing_says_the_load_and_whether_the_asked_directory_is_here() {
        let home = std::env::temp_dir().join(format!("agent-tailnet-load-{}", std::process::id()));
        let list = |cwd: Value| match answer(&home, "laptop", &json!({"protocol": PROTOCOL, "list": true, "cwd": cwd})) {
            Ok(Answer::Reply(reply)) => reply,
            _ => panic!("no listing"),
        };
        let here = list(json!(std::env::temp_dir()));
        assert!(here["load"].as_f64().is_some_and(|load| load >= 0.0), "{here}");
        assert_eq!(here["has_cwd"], true);
        assert_eq!(list(json!("/no/such/agent/dir"))["has_cwd"], false);
        assert_eq!(list(json!("relative"))["has_cwd"], false);
        assert!(list(Value::Null).get("has_cwd").is_none());
    }

    #[test]
    fn a_message_from_another_computer_reaches_a_session_here_named_by_that_computer() {
        let home = std::env::temp_dir().join(format!("agent-tailnet-message-{}", std::process::id()));
        let follow_ups = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let inbox = peers::open(&home, &follow_ups).unwrap();
        // peers::list skips this process, so the inbox poses as the parent process, which is alive.
        let parent = std::os::unix::process::parent_id();
        let dir = peers::dir(&home);
        std::fs::rename(inbox.socket(), dir.join(format!("{parent}.sock"))).unwrap();
        std::fs::write(dir.join(format!("{parent}.json")), json!({"pid": parent, "session": "abc123", "title": "api", "cwd": "/repo", "busy": false}).to_string()).unwrap();
        let ask = |message: Value| answer(&home, "laptop", &json!({"protocol": PROTOCOL, "message": message}));
        let Ok(Answer::Reply(sent)) = ask(json!({"to": "abc", "from": "def456", "from_cwd": "/src", "text": "hi"})) else { panic!("the message was not sent") };
        assert_eq!(sent["sent"], "Sent to session abc123 in /repo.");
        let text = follow_ups.lock().unwrap()[0].text.clone();
        assert!(text.starts_with("Message from another agent session laptop:def456 in /src, not from the user.") && text.ends_with("hi"), "{text}");
        assert_eq!(ask(json!({"to": "zzz", "text": "hi"})).err(), Some("No session zzz runs here.".into()));
        assert_eq!(ask(json!({"to": "abc", "text": ""})).err(), Some("The message is empty.".into()));
        drop(inbox);
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn a_reattach_reaches_the_session_server_it_names_while_it_runs() {
        let home = std::env::temp_dir().join(format!("agent-reattach-{}", std::process::id()));
        let socket = serve::socket(&home, 4242);
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let _server = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        assert!(matches!(answer(&home, "laptop", &json!({"protocol": PROTOCOL, "reattach": 4242})), Ok(Answer::Session(_))));
        let ended = |server: u64| answer(&home, "laptop", &json!({"protocol": PROTOCOL, "reattach": server})).err();
        assert_eq!(ended(4243), Some("The session ended: its server 4243 no longer runs.".into()));
        assert_eq!(ended(1 << 40), Some(format!("The session ended: its server {} no longer runs.", 1u64 << 40)));
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn only_an_untagged_device_of_the_owner_is_admitted() {
        let whois = |id: u64, tags: Value| json!({"Node": {"Tags": tags}, "UserProfile": {"ID": id, "LoginName": "eve@example.com"}});
        assert_eq!(owned_by(&whois(7, Value::Null), 7), Ok(()));
        assert_eq!(owned_by(&whois(8, Value::Null), 7), Err("logged in as eve@example.com".into()));
        assert_eq!(owned_by(&whois(7, json!(["tag:server"])), 7), Err("a tagged device".into()));
        assert!(owned_by(&json!({}), 7).is_err());
    }

    #[test]
    fn an_incompatible_pair_names_the_side_to_update_and_the_version() {
        let newer = refusal("studio", &json!({"error": "x", "protocol": PROTOCOL + 1, "version": "9.0.0"})).unwrap();
        assert!(newer.contains("Update this computer to v9.0.0 with `agent update`"), "{newer}");
        let older = refusal("studio", &json!({"error": "x", "protocol": PROTOCOL - 1, "version": "0.0.1"})).unwrap();
        assert!(older.contains(&format!("Update studio to v{VERSION}: run `agent update` there")), "{older}");
        assert_eq!(refusal("studio", &json!({"error": "No session x runs here.", "protocol": PROTOCOL})), Some("No session x runs here.".into()));
        assert_eq!(refusal("studio", &json!({"ok": true, "protocol": PROTOCOL})), None);
    }

    #[test]
    fn environments_put_local_first_once_and_check_refuses_others() {
        let home = std::env::temp_dir().join(format!("agent-tailnet-names-{}", std::process::id()));
        let mut settings = crate::settings::load(&home).unwrap();
        settings.environments = vec!["studio".into(), LOCAL.into(), "studio".into()];
        let names = environments(&settings);
        assert_eq!(names[..2], ["local", "studio"]);
        assert_eq!(names.iter().filter(|name| *name == "studio").count(), 1);
        assert_eq!(check(&settings, "studio"), Ok(()));
        assert!(check(&settings, "no-such-agent-computer").unwrap_err().contains("local, studio"));
    }

    #[test]
    fn only_online_untagged_desktops_of_this_login_are_discovered() {
        let peer = |dns: &str, os: &str, user: u64, online: bool, tags: Value| json!({"DNSName": dns, "OS": os, "UserID": user, "Online": online, "Tags": tags});
        let status = json!({"BackendState": "Running", "Self": {"UserID": 7}, "Peer": {
            "k1": peer("studio.tail1.ts.net.", "macOS", 7, true, Value::Null),
            "k2": peer("box.tail1.ts.net.", "linux", 7, true, json!([])),
            "k3": peer("server.tail1.ts.net.", "linux", 7, true, json!(["tag:server"])),
            "k4": peer("eves-mac.tail1.ts.net.", "macOS", 8, true, Value::Null),
            "k5": peer("asleep.tail1.ts.net.", "macOS", 7, false, Value::Null),
            "k6": peer("phone.tail1.ts.net.", "iOS", 7, true, Value::Null),
            "k7": peer("pc.tail1.ts.net.", "windows", 7, true, Value::Null),
        }});
        assert_eq!(discovered(&status), ["box", "studio"]);
        let mut stopped = status.clone();
        stopped["BackendState"] = json!("Stopped");
        assert!(discovered(&stopped).is_empty());
        assert!(discovered(&json!({})).is_empty());
    }

    /// The listener's side of a connection, over loopback with the Tailscale check left out.
    fn serve_one(home: &Path) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let home = home.to_path_buf();
        std::thread::spawn(move || handle(listener.accept().unwrap().0, &home, "laptop"));
        port
    }

    #[test]
    fn a_listener_lists_sessions_and_refuses_another_protocol_by_version() {
        let home = std::env::temp_dir().join(format!("agent-tailnet-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        let ask = |body: Value| exchange(connect("127.0.0.1", serve_one(&home)).unwrap(), "studio", body, LIST_TIMEOUT);
        let (_, _, reply) = ask(json!({"list": true})).unwrap();
        assert_eq!(reply["sessions"], json!([]));
        let (_, _, reply) = ask(json!({"attach": "abc"})).unwrap();
        assert_eq!(refusal("studio", &reply), Some("No session abc runs here.".into()));
        let stream = connect("127.0.0.1", serve_one(&home)).unwrap();
        writeln!(&stream, "{}", json!({"protocol": PROTOCOL - 1, "version": "0.0.1", "list": true})).unwrap();
        let reply: Value = serde_json::from_str(&BufReader::new(stream).lines().next().unwrap().unwrap()).unwrap();
        assert_eq!((reply["protocol"].as_u64(), reply["version"].as_str()), (Some(PROTOCOL), Some(VERSION)));
        assert!(reply["error"].as_str().unwrap().contains("v0.0.1"));
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn an_environment_that_does_not_answer_keeps_the_sessions_it_last_listed() {
        let home = std::env::temp_dir().join(format!("agent-tailnet-cache-{}", std::process::id()));
        std::fs::create_dir_all(home.join(".agents")).unwrap();
        let mut settings = crate::settings::load(&home).unwrap();
        settings.environments = vec!["127.0.0.1".into()];
        // Port 1 on loopback refuses at once, like a computer that is up but not serving.
        settings.tailnet_port = 1;
        std::fs::write(home.join(".agents/environments.json"), json!({"127.0.0.1": [{"session": "abc", "title": "t", "cwd": "/p", "busy": true}]}).to_string()).unwrap();
        let (listed, failures) = listing_of(&home, &settings, &settings.environments);
        assert_eq!(listed, [Listed { environment: "127.0.0.1".into(), session: "abc".into(), title: "t".into(), cwd: "/p".into(), busy: true, reachable: false }]);
        assert_eq!(failures.len(), 1);
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn a_discovered_computer_that_does_not_answer_is_left_out_quietly() {
        let home = std::env::temp_dir().join(format!("agent-tailnet-quiet-{}", std::process::id()));
        std::fs::create_dir_all(home.join(".agents")).unwrap();
        let mut settings = crate::settings::load(&home).unwrap();
        settings.tailnet_port = 1;
        std::fs::write(home.join(".agents/environments.json"), json!({"127.0.0.1": [{"session": "abc", "title": "t", "cwd": "/p", "busy": true}]}).to_string()).unwrap();
        assert_eq!(listing_of(&home, &settings, &["127.0.0.1".into()]), (Vec::new(), Vec::new()));
        std::fs::remove_dir_all(&home).unwrap();
    }
}
