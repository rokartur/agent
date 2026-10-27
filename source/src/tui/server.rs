//! The session server's side of the socket: one reader and one writer thread per attached terminal.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::mpsc::{self, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

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
    fn send(&self, message: Value) {
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
    let writer = std::thread::spawn(move || {
        for line in outbox {
            if writeln!(&stream, "{line}").is_err() {
                break;
            }
        }
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
        let Some(input) = serde_json::from_str::<Value>(&line).ok().and_then(|message| wire::decode(&message["input"])) else { continue };
        if events.send(Msg::Input(id, input)).is_err() {
            return;
        }
    }
    let _ = events.send(Msg::Detached(id));
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
}
