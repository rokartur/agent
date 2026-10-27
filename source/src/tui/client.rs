//! The terminal side of an attached session: forwards key, mouse and resize events, writes the frames it gets back.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

use crossterm::event;
use crossterm::terminal;
use serde_json::{Value, json};

use super::wire;

/// Shows the session on `stream` until the server ends it; returns the text to print after leaving the screen.
pub fn attach(stream: UnixStream) -> Result<String, String> {
    let size = terminal::size().map_err(|e| format!("Cannot read the terminal size: {e}"))?;
    let mut sender = stream.try_clone().map_err(|e| format!("Cannot use the session socket: {e}"))?;
    writeln!(sender, "{}", wire::hello(size)).map_err(|e| format!("Cannot reach the session: {e}"))?;
    let mut out = std::io::stdout();
    super::enter(&mut out).map_err(|e| format!("Cannot set up the terminal: {e}"))?;
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        super::leave(&mut std::io::stdout());
        previous_hook(info);
    }));
    // Exits with the process: a send fails only after the server closed the socket, which ends the loop below too.
    std::thread::spawn(move || {
        while let Ok(input) = event::read() {
            let Some(input) = wire::encode(&input) else { continue };
            if writeln!(sender, "{}", json!({"input": input})).is_err() {
                return;
            }
        }
    });
    let ending = show(BufReader::new(stream), &mut out);
    super::leave(&mut out);
    ending
}

fn show(stream: impl BufRead, out: &mut impl Write) -> Result<String, String> {
    for line in stream.lines() {
        let line = line.map_err(|e| format!("Lost the session: {e}"))?;
        let message: Value = serde_json::from_str(&line).map_err(|e| format!("The session sent a broken message: {e}"))?;
        if let Some(output) = message["output"].as_str() {
            out.write_all(output.as_bytes()).and_then(|()| out.flush()).map_err(|e| format!("Cannot write to the terminal: {e}"))?;
        } else if let Some(text) = message["exit"].as_str() {
            return Ok(text.to_string());
        } else if let Some(error) = message["error"].as_str() {
            return Err(error.to_string());
        }
    }
    Err("The session server stopped.".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_go_to_the_terminal_until_exit() {
        let mut out = Vec::new();
        let stream = "{\"output\":\"a\\u001b[2Kb\"}\n{\"output\":\"c\"}\n{\"exit\":\"bye\"}\n{\"output\":\"never\"}\n";
        assert_eq!(show(stream.as_bytes(), &mut out), Ok("bye".into()));
        assert_eq!(out, b"a\x1b[2Kbc");
    }

    #[test]
    fn a_refusal_or_a_dropped_server_is_an_error() {
        assert_eq!(show("{\"error\":\"too old\"}\n".as_bytes(), &mut Vec::new()), Err("too old".into()));
        assert_eq!(show("{\"output\":\"a\"}\n".as_bytes(), &mut Vec::new()), Err("The session server stopped.".into()));
    }
}
