//! The session server's side of the socket: one reader and one writer thread per attached terminal.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use crossterm::event::Event as Input;
use serde_json::{Value, json};

use super::Msg;
use super::render::{Line, Screen};
use super::wire;

/// A terminal that stops reading for this long is dropped, so it cannot stall the session for the others.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);

/// One attached terminal. Every client draws the same frame, sized to the smallest of them, like tmux.
pub struct Client {
    pub id: u64,
    pub size: (usize, usize),
    pub focused: Option<bool>,
    /// Where this terminal's computer sees the session, and where /env can move it.
    pub place: wire::Place,
    env: HashMap<String, String>,
    screen: Screen,
    /// Escape sequences (title, notifications, progress) that go out ahead of the next frame.
    pending: String,
    lines: Sender<String>,
    writer: JoinHandle<()>,
}

impl Client {
    pub fn env(&self, name: &str) -> Option<String> {
        self.env.get(name).cloned()
    }

    pub fn queue(&mut self, sequence: &str) {
        self.pending.push_str(sequence);
    }

    pub fn invalidate(&mut self) {
        self.screen.invalidate();
    }

    pub fn draw(&mut self, rows: Vec<Line>, cursor: (u16, u16)) {
        let mut out = std::mem::take(&mut self.pending).into_bytes();
        self.screen.draw(&mut out, rows, Some(cursor)).expect("a Vec takes every write");
        self.send(json!({"output": String::from_utf8_lossy(&out)}));
    }

    /// Sends what is still queued and `{"exit": text}`, then waits until the writer has written it or given up.
    pub fn finish(self, text: &str) {
        if !self.pending.is_empty() {
            self.send(json!({"output": self.pending}));
        }
        self.send(json!({"exit": text}));
        self.close();
    }

    /// /env: tells the terminal to start a new session in `environment`, then lets go of it.
    pub fn switch(self, environment: &str) {
        self.send(json!({"switch": environment}));
        self.close();
    }

    fn close(self) {
        drop(self.lines);
        let _ = self.writer.join();
    }

    /// A send fails only once the writer gave up on this client; its reader then reports `Msg::Detached`.
    pub fn send(&self, message: Value) {
        let _ = self.lines.send(message.to_string());
    }
}

/// Listens on `socket`, readable by this user only, and turns each connection into `Msg::Attached`,
/// `Msg::Input` and `Msg::Detached`.
pub fn listen(socket: &Path, events: Sender<Msg>) -> Result<(), String> {
    let _ = std::fs::remove_file(socket);
    let listener = UnixListener::bind(socket).map_err(|e| format!("Cannot listen on {}: {e}", socket.display()))?;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600)).map_err(|e| format!("Cannot restrict {}: {e}", socket.display()))?;
    std::thread::spawn(move || {
        for (id, stream) in (0..).zip(listener.incoming().map_while(Result::ok)) {
            let events = events.clone();
            std::thread::spawn(move || serve_client(id, stream, &events));
        }
    });
    Ok(())
}

fn serve_client(id: u64, stream: UnixStream, events: &Sender<Msg>) {
    let Ok(reader) = stream.try_clone() else { return };
    // A connection that never says hello would otherwise hold this thread forever.
    let _ = stream.set_read_timeout(Some(HELLO_TIMEOUT));
    let mut lines = BufReader::new(reader).lines();
    let Some(hello) = lines.next().and_then(Result::ok).and_then(|line| serde_json::from_str::<Value>(&line).ok()) else { return };
    let _ = stream.set_read_timeout(None);
    let size = match accepted(&hello) {
        Ok(size) => size,
        Err(error) => {
            let _ = writeln!(&stream, "{}", json!({"error": error}));
            return;
        }
    };
    let env = hello["env"].as_object().into_iter().flatten().filter_map(|(name, value)| Some((name.clone(), value.as_str()?.to_string()))).collect();
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    let (lines_out, outbox) = mpsc::channel::<String>();
    let _ = lines_out.send(json!({"server": std::process::id()}).to_string());
    let writer = std::thread::spawn(move || {
        write_lines(&outbox, &stream, wire::PING_EVERY);
        // Wakes this client's reader, which then reports the detach.
        let _ = stream.shutdown(std::net::Shutdown::Both);
    });
    let place = wire::Place {
        environment: hello["environment"].as_str().unwrap_or(crate::tailnet::LOCAL).to_string(),
        environments: hello["environments"].as_array().into_iter().flatten().filter_map(|name| Some(name.as_str()?.to_string())).collect(),
    };
    let client = Client { id, size: (size.0 as usize, size.1 as usize), focused: None, place, env, screen: Screen::new(), pending: String::new(), lines: lines_out, writer };
    if events.send(Msg::Attached(client)).is_err() {
        return;
    }
    for line in lines.map_while(Result::ok) {
        let Some(event) = serde_json::from_str::<Value>(&line).ok().and_then(|message| from_terminal(id, &message)) else { continue };
        if events.send(event).is_err() {
            return;
        }
    }
    let _ = events.send(Msg::Detached(id));
}

/// Writes each line of `outbox` to `out`, and a ping after `ping_every` without one, until either side lets go.
fn write_lines(outbox: &Receiver<String>, mut out: impl Write, ping_every: Duration) {
    loop {
        let line = match outbox.recv_timeout(ping_every) {
            Ok(line) => line,
            Err(RecvTimeoutError::Timeout) => json!({"ping": true}).to_string(),
            Err(RecvTimeoutError::Disconnected) => return,
        };
        if writeln!(out, "{line}").is_err() {
            return;
        }
    }
}

/// What a terminal's line means to the session; None for a line it does not know.
fn from_terminal(id: u64, message: &Value) -> Option<Msg> {
    if let Some(error) = message["failed"].as_str() {
        return Some(Msg::TerminalFailed(error.to_string()));
    }
    let input = wire::decode(&message["input"]);
    if message["image"].is_null() {
        return input.map(|input| Msg::Input(id, input));
    }
    let image = wire::decode_image(&message["image"]);
    match input {
        Some(Input::Paste(text)) => Some(Msg::Image { id, paste: Some(text), image }),
        Some(input) => Some(Msg::Input(id, input)),
        None => Some(Msg::Image { id, paste: None, image }),
    }
}

/// The terminal size a hello carries, or why this server refuses the client.
fn accepted(hello: &Value) -> Result<(u16, u16), String> {
    if hello["protocol"] != wire::PROTOCOL {
        let theirs = hello["version"].as_str().unwrap_or("unknown");
        return Err(format!("This session runs agent v{}, which cannot talk to agent v{theirs}.", env!("CARGO_PKG_VERSION")));
    }
    wire::size(&hello["size"]).ok_or_else(|| "The hello carries no terminal size.".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hello_of_another_protocol_is_refused_with_both_versions() {
        let error = accepted(&json!({"protocol": wire::PROTOCOL + 1, "version": "9.9.9", "size": [80, 24]})).unwrap_err();
        assert!(error.contains("9.9.9") && error.contains(env!("CARGO_PKG_VERSION")), "{error}");
        assert_eq!(accepted(&json!({"protocol": wire::PROTOCOL, "size": [80, 24]})), Ok((80, 24)));
        assert!(accepted(&json!({"protocol": wire::PROTOCOL})).is_err());
    }

    #[test]
    fn a_quiet_session_pings_its_terminal() {
        let (lines, outbox) = mpsc::channel();
        lines.send("{\"output\":\"a\"}".to_string()).unwrap();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            drop(lines);
        });
        let mut out = Vec::new();
        write_lines(&outbox, &mut out, Duration::from_millis(10));
        let out = String::from_utf8(out).unwrap();
        assert!(out.starts_with("{\"output\":\"a\"}\n{\"ping\":true}\n"), "{out}");
    }

    #[test]
    fn a_terminal_line_becomes_input_an_image_or_a_failure() {
        let paste = json!({"type": "paste", "text": "/a.png"});
        assert!(matches!(from_terminal(7, &json!({"input": paste})), Some(Msg::Input(7, Input::Paste(text))) if text == "/a.png"));
        let dropped = from_terminal(7, &json!({"input": paste, "image": {"error": "too big"}}));
        assert!(matches!(dropped, Some(Msg::Image { id: 7, paste: Some(text), image: Err(error) }) if text == "/a.png" && error == "too big"));
        let clipboard = from_terminal(7, &json!({"image": {"error": "No image in the clipboard"}}));
        assert!(matches!(clipboard, Some(Msg::Image { id: 7, paste: None, image: Err(_) })));
        assert!(matches!(from_terminal(7, &json!({"failed": "Cannot run pbcopy"})), Some(Msg::TerminalFailed(error)) if error == "Cannot run pbcopy"));
        assert!(from_terminal(7, &json!({"what": 1})).is_none());
    }
}
