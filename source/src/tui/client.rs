//! The terminal side of an attached session: forwards key, mouse and resize events, writes the frames it gets back.

use std::io::{BufRead, BufReader, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crossterm::event;
use crossterm::terminal;
use serde_json::{Value, json};

use super::wire;

/// How long the key reader waits for an event before it checks whether the session let go of this terminal.
const INPUT_POLL: Duration = Duration::from_millis(50);

/// How a session lets go of this terminal.
#[derive(Debug, PartialEq)]
pub enum Ending {
    /// The text to print after leaving the screen.
    Exit(String),
    /// /env picked this environment: the terminal starts a new session there.
    Switch(String),
    /// The connection closed or failed with no goodbye: the server crashed, or the network to its computer went.
    Lost,
}

/// Shows the session the server on the other end of `reader` and `sender` runs, until it lets go of this terminal.
pub fn attach(reader: impl Read, mut sender: impl Write + Send + 'static, place: &wire::Place) -> Result<Ending, String> {
    let size = terminal::size().map_err(|e| format!("Cannot read the terminal size: {e}"))?;
    writeln!(sender, "{}", wire::hello(size, place)).map_err(|e| format!("Cannot reach the session: {e}"))?;
    let mut out = std::io::stdout();
    super::enter(&mut out).map_err(|e| format!("Cannot set up the terminal: {e}"))?;
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        super::leave(&mut std::io::stdout());
        previous_hook(info);
    }));
    // Stops with the session, so after /env the next session's reader is the only one taking keys from the terminal.
    let stop = Arc::new(AtomicBool::new(false));
    let keys = std::thread::spawn({
        let stop = stop.clone();
        move || {
            while !stop.load(Ordering::Relaxed) {
                if !event::poll(INPUT_POLL).unwrap_or(false) {
                    continue;
                }
                let Ok(input) = event::read() else { return };
                let Some(input) = wire::encode(&input) else { continue };
                if writeln!(sender, "{}", json!({"input": input})).is_err() {
                    return;
                }
            }
        }
    });
    let ending = show(BufReader::new(reader), &mut out);
    stop.store(true, Ordering::Relaxed);
    let _ = keys.join();
    super::leave(&mut out);
    ending
}

fn show(stream: impl BufRead, out: &mut impl Write) -> Result<Ending, String> {
    for line in stream.lines() {
        let Ok(line) = line else { return Ok(Ending::Lost) };
        let message: Value = serde_json::from_str(&line).map_err(|e| format!("The session sent a broken message: {e}"))?;
        if let Some(output) = message["output"].as_str() {
            out.write_all(output.as_bytes()).and_then(|()| out.flush()).map_err(|e| format!("Cannot write to the terminal: {e}"))?;
        } else if let Some(text) = message["exit"].as_str() {
            return Ok(Ending::Exit(text.to_string()));
        } else if let Some(environment) = message["switch"].as_str() {
            return Ok(Ending::Switch(environment.to_string()));
        } else if let Some(error) = message["error"].as_str() {
            return Err(error.to_string());
        }
    }
    Ok(Ending::Lost)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_go_to_the_terminal_until_exit() {
        let mut out = Vec::new();
        let stream = "{\"output\":\"a\\u001b[2Kb\"}\n{\"output\":\"c\"}\n{\"exit\":\"bye\"}\n{\"output\":\"never\"}\n";
        assert_eq!(show(stream.as_bytes(), &mut out), Ok(Ending::Exit("bye".into())));
        assert_eq!(out, b"a\x1b[2Kbc");
        assert_eq!(show("{\"switch\":\"studio\"}\n".as_bytes(), &mut Vec::new()), Ok(Ending::Switch("studio".into())));
    }

    #[test]
    fn a_refusal_is_an_error_and_a_dropped_connection_is_lost() {
        assert_eq!(show("{\"error\":\"too old\"}\n".as_bytes(), &mut Vec::new()), Err("too old".into()));
        assert_eq!(show("{\"output\":\"a\"}\n".as_bytes(), &mut Vec::new()), Ok(Ending::Lost));
    }
}
