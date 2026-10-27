//! The terminal side of an attached session: forwards key, mouse and resize events, writes the frames it gets back.

use std::io::{BufRead, BufReader, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossterm::event;
use crossterm::terminal;
use serde_json::{Value, json};

use super::wire;
use crate::image;

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
pub fn attach(reader: impl Read, sender: impl Write + Send + 'static, place: &wire::Place) -> Result<Ending, String> {
    let size = terminal::size().map_err(|e| format!("Cannot read the terminal size: {e}"))?;
    // The key reader and the answers to the server's requests both write lines.
    let sender = Arc::new(Mutex::new(sender));
    writeln!(sender.lock().unwrap(), "{}", wire::hello(size, place)).map_err(|e| format!("Cannot reach the session: {e}"))?;
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
        let (stop, sender) = (stop.clone(), sender.clone());
        move || {
            while !stop.load(Ordering::Relaxed) {
                if !event::poll(INPUT_POLL).unwrap_or(false) {
                    continue;
                }
                let Ok(input) = event::read() else { return };
                let Some(message) = input_message(&input) else { continue };
                if writeln!(sender.lock().unwrap(), "{message}").is_err() {
                    return;
                }
            }
        }
    });
    let ending = show(BufReader::new(reader), &mut out, |request| {
        let Some(answer) = serve_request(request) else { return };
        // A failed write means the connection is gone; reading it then ends the session.
        let _ = writeln!(sender.lock().unwrap(), "{answer}");
    });
    stop.store(true, Ordering::Relaxed);
    let _ = keys.join();
    super::leave(&mut out);
    ending
}

/// A terminal event as the line that carries it. A dropped image file is read here, since the path names a file on
/// this computer and the session may run on another one.
fn input_message(input: &event::Event) -> Option<Value> {
    let mut message = json!({"input": wire::encode(input)?});
    if let event::Event::Paste(text) = input
        && let Some(path) = image::pasted_path(text)
    {
        message["image"] = wire::image(image::block(&path), Some(&path));
    }
    Some(message)
}

/// Does on this computer what the session asked for, and returns the answer to send back, if any.
fn serve_request(request: &Value) -> Option<Value> {
    let done = if let Some(text) = request["copy"].as_str() {
        copy_to_clipboard(text)
    } else if let Some(url) = request["open"].as_str() {
        open_in_browser(url)
    } else if request["paste_image"] == true {
        return Some(json!({"image": wire::image(image::from_clipboard(), None)}));
    } else {
        return None;
    };
    done.err().map(|error| json!({"failed": error}))
}

/// Writes the session's frames to `out` and hands its requests to `requests`, until it lets go of this terminal.
fn show(stream: impl BufRead, out: &mut impl Write, mut requests: impl FnMut(&Value)) -> Result<Ending, String> {
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
        } else {
            requests(&message);
        }
    }
    Ok(Ending::Lost)
}

fn copy_to_clipboard(text: &str) -> Result<(), String> {
    let (program, args): (&str, &[&str]) = if cfg!(target_os = "macos") {
        ("pbcopy", &[])
    } else if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        ("wl-copy", &[])
    } else {
        ("xclip", &["-selection", "clipboard"])
    };
    let mut child = crate::process::command(program)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("Cannot run {program}: {e}"))?;
    // The stdin handle drops before `wait`, so the child sees EOF; it is reaped even when the write failed.
    let written = child.stdin.take().unwrap().write_all(text.as_bytes());
    child.wait().map_err(|e| e.to_string())?;
    written.map_err(|e| e.to_string())
}

fn open_in_browser(url: &str) -> Result<(), String> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(format!("Not a web link: {url}"));
    }
    if !cfg!(target_os = "macos") {
        // Reaped on a thread: xdg-open runs a browser without a desktop handler in the foreground.
        let mut child =
            crate::process::command("xdg-open").arg(url).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().map_err(|e| format!("Cannot run xdg-open: {e}"))?;
        std::thread::spawn(move || child.wait());
        return Ok(());
    }
    // output() and not status(): open's stderr would draw over the alternate screen.
    let output = crate::process::command("open").arg(url).output().map_err(|e| format!("Cannot run open: {e}"))?;
    if !output.status.success() {
        return Err(format!("open {url} failed: {}", String::from_utf8_lossy(&output.stderr).trim()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_go_to_the_terminal_until_exit() {
        let mut out = Vec::new();
        let stream = "{\"output\":\"a\\u001b[2Kb\"}\n{\"output\":\"c\"}\n{\"exit\":\"bye\"}\n{\"output\":\"never\"}\n";
        assert_eq!(show(stream.as_bytes(), &mut out, |_| panic!("no request was sent")), Ok(Ending::Exit("bye".into())));
        assert_eq!(out, b"a\x1b[2Kbc");
        assert_eq!(show("{\"switch\":\"studio\"}\n".as_bytes(), &mut Vec::new(), |_| {}), Ok(Ending::Switch("studio".into())));
    }

    #[test]
    fn requests_between_frames_reach_this_computer() {
        let mut out = Vec::new();
        let mut requests = Vec::new();
        let stream = "{\"output\":\"a\"}\n{\"copy\":\"x\"}\n{\"output\":\"b\"}\n{\"paste_image\":true}\n";
        assert_eq!(show(stream.as_bytes(), &mut out, |request| requests.push(request.clone())), Ok(Ending::Lost));
        assert_eq!(out, b"ab");
        assert_eq!(requests, [json!({"copy": "x"}), json!({"paste_image": true})]);
    }

    #[test]
    fn a_dropped_image_file_travels_with_its_paste() {
        let dir = std::env::temp_dir().join(format!("agent-drop-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shot.png");
        std::fs::write(&path, crate::image::tests::PIXEL).unwrap();
        let dropped = input_message(&event::Event::Paste(format!("'{}'", path.display()))).unwrap();
        assert_eq!(dropped["image"]["path"], json!(path.display().to_string()));
        assert!(dropped["image"]["data"].as_str().is_some_and(|data| !data.is_empty()), "{dropped}");
        assert!(input_message(&event::Event::Paste("just text".into())).unwrap().get("image").is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn only_a_failed_or_asked_for_request_is_answered() {
        assert_eq!(serve_request(&json!({"open": "file:///etc/passwd"})), Some(json!({"failed": "Not a web link: file:///etc/passwd"})));
        assert_eq!(serve_request(&json!({"unknown": 1})), None);
    }

    #[test]
    fn a_refusal_is_an_error_and_a_dropped_connection_is_lost() {
        assert_eq!(show("{\"error\":\"too old\"}\n".as_bytes(), &mut Vec::new(), |_| {}), Err("too old".into()));
        assert_eq!(show("{\"output\":\"a\"}\n".as_bytes(), &mut Vec::new(), |_| {}), Ok(Ending::Lost));
    }
}
