//! The session server's wire format: one JSON object per line over the socket.
//! Client to server: a hello, then `{"input": ..}` per terminal event. Server to client: `{"server": pid}` first, so a
//! client that loses its connection can reattach, then `{"output": ..}` with terminal bytes, `{"ping": true}` after
//! `PING_EVERY` without another line, and at the end `{"exit": ..}` with the text to print after leaving the screen,
//! `{"switch": environment}` when /env moves the terminal to a new session there, or `{"error": ..}`.
//! The terminal's computer may not be the session's, so the server asks the terminal to `{"copy": text}`,
//! `{"open": url}` or `{"paste_image": true}`; the terminal answers `{"copied": true}`, `{"image": ..}` or
//! `{"failed": error}`, and sends a dropped image file's bytes in the `image` of its paste. /env asks it to
//! `{"forget": environment}` saved in its settings; it answers with its new `{"place": ..}`.

use crossterm::event::{Event as Input, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use serde_json::{Value, json};
use std::path::Path;
use std::time::Duration;

/// Bumped when a message changes shape, so a client and a server of different releases refuse each other plainly.
pub const PROTOCOL: u64 = 4;
/// A remote client that hears nothing for `SILENCE` takes the link as gone and reconnects.
pub const PING_EVERY: Duration = Duration::from_secs(5);
pub const SILENCE: Duration = Duration::from_secs(15);

const NAMED_KEYS: [(&str, KeyCode); 21] = [
    ("Backspace", KeyCode::Backspace),
    ("Enter", KeyCode::Enter),
    ("Left", KeyCode::Left),
    ("Right", KeyCode::Right),
    ("Up", KeyCode::Up),
    ("Down", KeyCode::Down),
    ("Home", KeyCode::Home),
    ("End", KeyCode::End),
    ("PageUp", KeyCode::PageUp),
    ("PageDown", KeyCode::PageDown),
    ("Tab", KeyCode::Tab),
    ("BackTab", KeyCode::BackTab),
    ("Delete", KeyCode::Delete),
    ("Insert", KeyCode::Insert),
    ("Null", KeyCode::Null),
    ("Esc", KeyCode::Esc),
    ("CapsLock", KeyCode::CapsLock),
    ("ScrollLock", KeyCode::ScrollLock),
    ("NumLock", KeyCode::NumLock),
    ("PrintScreen", KeyCode::PrintScreen),
    ("Pause", KeyCode::Pause),
];

const BUTTONS: [(&str, MouseButton); 3] = [("left", MouseButton::Left), ("right", MouseButton::Right), ("middle", MouseButton::Middle)];

/// The first line a client sends: its terminal size, the variables that pick its notification sequences, and the
/// environments it can move to with /env, named as its own settings name them.
pub fn hello(size: (u16, u16), place: &Place) -> Value {
    let env: serde_json::Map<String, Value> = super::notify::TERMINAL_VARS.iter().filter_map(|name| Some((name.to_string(), Value::String(std::env::var(name).ok()?)))).collect();
    json!({"protocol": PROTOCOL, "version": env!("CARGO_PKG_VERSION"), "size": [size.0, size.1], "env": env, "environment": place.environment, "environments": place.environments, "saved": place.saved})
}

/// The place a hello or a `{"place": ..}` answer carries; a client older than `saved` offers nothing to remove.
pub fn place(value: &Value) -> Place {
    let names = |key: &str| value[key].as_array().into_iter().flatten().filter_map(|name| Some(name.as_str()?.to_string())).collect();
    Place { environment: value["environment"].as_str().unwrap_or(crate::tailnet::LOCAL).to_string(), environments: names("environments"), saved: names("saved") }
}

/// Where a client sees the session it attaches to: `environment` is one of `environments`, `local` included.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Place {
    pub environment: String,
    pub environments: Vec<String>,
    /// The ones in the `environments` setting of the terminal's computer, which /env can remove.
    pub saved: Vec<String>,
}

/// A pasted image as the terminal sends it: the base64 bytes of the block it read, and the dropped file's path.
pub fn image(block: Result<Value, String>, path: Option<&Path>) -> Value {
    match block {
        Ok(block) => json!({"data": block["source"]["data"], "path": path.map(|path| path.display().to_string())}),
        Err(error) => json!({"error": error}),
    }
}

/// The API block an `image` carries, rebuilt from its bytes so a broken one never reaches the model.
pub fn decode_image(image: &Value) -> Result<Value, String> {
    if let Some(error) = image["error"].as_str() {
        return Err(error.to_string());
    }
    let mut block = crate::image::from_base64(image["data"].as_str().ok_or("The terminal sent an image without data.")?)?;
    if let Some(path) = image["path"].as_str() {
        block[super::DROPPED_PATH] = path.into();
    }
    Ok(block)
}

/// None for keys the TUI never asks the terminal to report: media and lone modifier keys.
pub fn encode(input: &Input) -> Option<Value> {
    Some(match input {
        Input::Key(key) => {
            let mut value = json!({"type": "key", "mods": key.modifiers.bits(), "kind": kind_name(key.kind), "state": key.state.bits()});
            match key.code {
                KeyCode::Char(c) => value["char"] = json!(c.to_string()),
                KeyCode::F(n) => value["f"] = json!(n),
                code => value["code"] = json!(NAMED_KEYS.iter().find(|(_, named)| *named == code)?.0),
            }
            value
        }
        Input::Mouse(mouse) => {
            let (kind, button) = match mouse.kind {
                MouseEventKind::Down(button) => ("down", Some(button)),
                MouseEventKind::Up(button) => ("up", Some(button)),
                MouseEventKind::Drag(button) => ("drag", Some(button)),
                MouseEventKind::Moved => ("moved", None),
                MouseEventKind::ScrollDown => ("scroll_down", None),
                MouseEventKind::ScrollUp => ("scroll_up", None),
                MouseEventKind::ScrollLeft => ("scroll_left", None),
                MouseEventKind::ScrollRight => ("scroll_right", None),
            };
            let button = button.map(|button| BUTTONS.iter().find(|(_, known)| *known == button).expect("every MouseButton is in BUTTONS").0);
            json!({"type": "mouse", "kind": kind, "button": button, "column": mouse.column, "row": mouse.row, "mods": mouse.modifiers.bits()})
        }
        Input::Paste(text) => json!({"type": "paste", "text": text}),
        Input::Resize(width, height) => json!({"type": "resize", "size": [width, height]}),
        Input::FocusGained => json!({"type": "focus", "focused": true}),
        Input::FocusLost => json!({"type": "focus", "focused": false}),
    })
}

/// None for anything `encode` does not write; the server skips such a line.
pub fn decode(value: &Value) -> Option<Input> {
    let number = |key: &str| value[key].as_u64().and_then(|n| u16::try_from(n).ok());
    let mods = || value["mods"].as_u64().map(|bits| KeyModifiers::from_bits_truncate(bits as u8));
    Some(match value["type"].as_str()? {
        "key" => {
            let code = if let Some(text) = value["char"].as_str() {
                let mut chars = text.chars();
                let c = chars.next()?;
                if chars.next().is_some() {
                    return None;
                }
                KeyCode::Char(c)
            } else if let Some(n) = value["f"].as_u64() {
                KeyCode::F(u8::try_from(n).ok()?)
            } else {
                let name = value["code"].as_str()?;
                NAMED_KEYS.iter().find(|(known, _)| *known == name)?.1
            };
            let kind = match value["kind"].as_str()? {
                "press" => KeyEventKind::Press,
                "repeat" => KeyEventKind::Repeat,
                "release" => KeyEventKind::Release,
                _ => return None,
            };
            Input::Key(KeyEvent { code, modifiers: mods()?, kind, state: KeyEventState::from_bits_truncate(value["state"].as_u64()? as u8) })
        }
        "mouse" => {
            let button = || value["button"].as_str().and_then(|name| BUTTONS.iter().find(|(known, _)| *known == name)).map(|(_, button)| *button);
            let kind = match value["kind"].as_str()? {
                "down" => MouseEventKind::Down(button()?),
                "up" => MouseEventKind::Up(button()?),
                "drag" => MouseEventKind::Drag(button()?),
                "moved" => MouseEventKind::Moved,
                "scroll_down" => MouseEventKind::ScrollDown,
                "scroll_up" => MouseEventKind::ScrollUp,
                "scroll_left" => MouseEventKind::ScrollLeft,
                "scroll_right" => MouseEventKind::ScrollRight,
                _ => return None,
            };
            Input::Mouse(MouseEvent { kind, column: number("column")?, row: number("row")?, modifiers: mods()? })
        }
        "paste" => Input::Paste(value["text"].as_str()?.to_string()),
        "resize" => {
            let (width, height) = size(&value["size"])?;
            Input::Resize(width, height)
        }
        "focus" if value["focused"].as_bool()? => Input::FocusGained,
        "focus" => Input::FocusLost,
        _ => return None,
    })
}

/// A `[width, height]` pair; a zero side would leave nothing to draw on.
pub fn size(value: &Value) -> Option<(u16, u16)> {
    let side = |index: usize| value[index].as_u64().and_then(|n| u16::try_from(n).ok()).filter(|n| *n > 0);
    Some((side(0)?, side(1)?))
}

fn kind_name(kind: KeyEventKind) -> &'static str {
    match kind {
        KeyEventKind::Press => "press",
        KeyEventKind::Repeat => "repeat",
        KeyEventKind::Release => "release",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_events_survive_the_wire() {
        let key = |code, modifiers| Input::Key(KeyEvent::new(code, modifiers));
        let mouse = |kind| Input::Mouse(MouseEvent { kind, column: 12, row: 3, modifiers: KeyModifiers::SHIFT });
        let events = [
            key(KeyCode::Char('é'), KeyModifiers::NONE),
            key(KeyCode::Char('c'), KeyModifiers::CONTROL),
            key(KeyCode::Enter, KeyModifiers::ALT),
            key(KeyCode::F(12), KeyModifiers::NONE),
            key(KeyCode::Esc, KeyModifiers::NONE),
            Input::Key(KeyEvent::new_with_kind(KeyCode::Up, KeyModifiers::NONE, KeyEventKind::Release)),
            mouse(MouseEventKind::Down(MouseButton::Left)),
            mouse(MouseEventKind::Drag(MouseButton::Middle)),
            mouse(MouseEventKind::ScrollUp),
            Input::Paste("two\nlines".into()),
            Input::Resize(120, 40),
            Input::FocusGained,
            Input::FocusLost,
        ];
        for event in events {
            let line = encode(&event).unwrap().to_string();
            assert_eq!(decode(&serde_json::from_str(&line).unwrap()), Some(event));
        }
    }

    #[test]
    fn unknown_or_malformed_input_is_skipped() {
        assert_eq!(encode(&Input::Key(KeyEvent::new(KeyCode::Media(crossterm::event::MediaKeyCode::Play), KeyModifiers::NONE))), None);
        assert_eq!(decode(&json!({"type": "key", "mods": 0, "kind": "press", "state": 0, "char": "ab"})), None);
        assert_eq!(decode(&json!({"type": "key", "mods": 0, "kind": "press", "state": 0, "code": "Launch"})), None);
        assert_eq!(decode(&json!({"type": "resize", "size": [0, 40]})), None);
        assert_eq!(decode(&json!({"type": "shell", "mods": 0})), None);
    }

    #[test]
    fn an_image_survives_the_wire_and_a_broken_one_is_refused() {
        let path = std::env::temp_dir().join(format!("agent-wire-{}.png", std::process::id()));
        std::fs::write(&path, crate::image::tests::PIXEL).unwrap();
        let sent = crate::image::block(&path);
        std::fs::remove_file(&path).unwrap();
        let mut received = decode_image(&image(sent.clone(), Some(&path))).unwrap();
        assert_eq!(received.as_object_mut().unwrap().remove(super::super::DROPPED_PATH), Some(json!(path.display().to_string())));
        assert_eq!(received, sent.unwrap());
        assert_eq!(decode_image(&image(Err("No image in the clipboard".into()), None)), Err("No image in the clipboard".into()));
        assert!(decode_image(&json!({"data": "not base64!"})).is_err());
        assert!(decode_image(&json!({"data": "aGVsbG8="})).is_err());
    }
}
