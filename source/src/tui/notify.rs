/// Every variable `sequence` and `progress` read: a client sends them, since the session server's own came from whichever terminal started it.
pub const TERMINAL_VARS: [&str; 12] = [
    "KITTY_WINDOW_ID",
    "TERM",
    "GHOSTTY_RESOURCES_DIR",
    "WEZTERM_EXECUTABLE",
    "VTE_VERSION",
    "TERM_PROGRAM",
    "LC_TERMINAL",
    "LC_TERMINAL_VERSION",
    "TERM_PROGRAM_VERSION",
    "WT_SESSION",
    "ConEmuANSI",
    "TMUX",
];

/// The escape sequence that shows a desktop notification in the terminal we run in.
/// `env` reads environment variables. tmux renames TERM_PROGRAM, so Tuni, and iTerm2 without LC_TERMINAL, get the bell there.
pub fn sequence(title: &str, body: &str, env: impl Fn(&str) -> Option<String>) -> String {
    let (title, body) = (printable(title), printable(body));
    let is = |name: &str, value: &str| env(name).is_some_and(|found| found == value);
    let raw = if env("KITTY_WINDOW_ID").is_some() || is("TERM", "xterm-kitty") {
        format!("\x1b]99;;{title}: {body}\x1b\\")
    } else if env("GHOSTTY_RESOURCES_DIR").is_some()
        || env("WEZTERM_EXECUTABLE").is_some()
        || env("VTE_VERSION").is_some()
        || is("TERM", "foot")
        || is("TERM_PROGRAM", "tuni")
        || is("TERM_PROGRAM", "rex")
    {
        format!("\x1b]777;notify;{title};{body}\x07")
    } else if is("LC_TERMINAL", "iTerm2") || is("TERM_PROGRAM", "iTerm.app") {
        format!("\x1b]9;{title}: {body}\x07")
    } else {
        // Terminal.app and the rest have no notification sequence; the bell bounces the Dock icon or marks the tab.
        return "\x07".to_string();
    };
    through_tmux(raw, &env)
}

/// OSC 9;4 indeterminate progress while `busy`, cleared otherwise; None where the terminal would misread it.
/// WezTerm and iTerm2 before 3.6.6 take any OSC 9 as a notification, so the allowlist stays narrow.
pub fn progress(busy: bool, env: impl Fn(&str) -> Option<String>) -> Option<String> {
    let is = |name: &str, value: &str| env(name).is_some_and(|found| found == value);
    let iterm_version = env("LC_TERMINAL_VERSION").or_else(|| env("TERM_PROGRAM_VERSION")).unwrap_or_default();
    let iterm_supports = (is("LC_TERMINAL", "iTerm2") || is("TERM_PROGRAM", "iTerm.app")) && version(&iterm_version) >= vec![3, 6, 6];
    let supported = env("GHOSTTY_RESOURCES_DIR").is_some() || env("WT_SESSION").is_some() || env("ConEmuANSI").is_some() || is("TERM_PROGRAM", "tuni") || iterm_supports;
    if !supported {
        return None;
    }
    let state = if busy { 3 } else { 0 };
    Some(through_tmux(format!("\x1b]9;4;{state}\x07"), &env))
}

/// What the agent is doing, as Rex's Program Status Protocol (OSC 7501) names it: a state, what a blocked turn waits for, one line of text.
#[derive(Clone, PartialEq, Debug)]
pub struct Status {
    pub state: &'static str,
    pub kind: Option<&'static str>,
    pub msg: String,
}

/// OSC 7501 for Rex, which draws tab and sidebar activity from it and decides itself when to notify or play a sound.
/// https://www.superlogical.com/rex/docs/build/program-status
pub fn program_status(status: &Status, env: impl Fn(&str) -> Option<String>) -> Option<String> {
    if env("TERM_PROGRAM").is_none_or(|program| program != "rex") {
        return None;
    }
    let mut body = format!("state={}", status.state);
    if let Some(kind) = status.kind {
        body.push_str(&format!(":kind={kind}"));
    }
    body.push_str(":app=agent");
    let msg = printable(&status.msg);
    if !msg.is_empty() {
        body.push_str(&format!(":msg={}", crate::image::base64(msg.as_bytes())));
    }
    Some(through_tmux(format!("\x1b]7501;{body}\x1b\\"), &env))
}

fn version(text: &str) -> Vec<u32> {
    text.split('.').map_while(|part| part.parse().ok()).collect()
}

fn through_tmux(raw: String, env: impl Fn(&str) -> Option<String>) -> String {
    if env("TMUX").is_none() {
        return raw;
    }
    // tmux forwards a sequence to the outer terminal only inside DCS passthrough, with every ESC doubled.
    format!("\x1bPtmux;{}\x1b\\", raw.replace('\x1b', "\x1b\x1b"))
}

/// Control characters would end the sequence early and let the rest of the text drive the terminal.
pub fn printable(text: &str) -> String {
    let line = text.lines().find(|line| !line.trim().is_empty()).unwrap_or_default();
    line.chars().filter(|c| !c.is_control()).take(120).collect::<String>().replace(';', ",")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| vars.iter().find(|(key, _)| *key == name).map(|(_, value)| value.to_string())
    }

    #[test]
    fn each_terminal_family_gets_its_own_sequence() {
        assert_eq!(sequence("agent", "Done", env(&[("GHOSTTY_RESOURCES_DIR", "/g")])), "\x1b]777;notify;agent;Done\x07");
        assert_eq!(sequence("agent", "Done", env(&[("KITTY_WINDOW_ID", "1")])), "\x1b]99;;agent: Done\x1b\\");
        assert_eq!(sequence("agent", "Done", env(&[("TERM_PROGRAM", "iTerm.app")])), "\x1b]9;agent: Done\x07");
        assert_eq!(sequence("agent", "Done", env(&[("TERM_PROGRAM", "Apple_Terminal")])), "\x07");
        assert_eq!(sequence("agent", "Done", env(&[("TERM_PROGRAM", "tuni")])), "\x1b]777;notify;agent;Done\x07");
        assert_eq!(sequence("agent", "Done", env(&[("TERM_PROGRAM", "rex")])), "\x1b]777;notify;agent;Done\x07");
    }

    #[test]
    fn tmux_wraps_the_sequence_in_passthrough() {
        let vars = env(&[("TMUX", "/tmp/tmux"), ("LC_TERMINAL", "iTerm2")]);
        assert_eq!(sequence("agent", "Done", vars), "\x1bPtmux;\x1b\x1b]9;agent: Done\x07\x1b\\");
    }

    #[test]
    fn progress_goes_only_to_terminals_that_draw_it() {
        assert_eq!(progress(true, env(&[("GHOSTTY_RESOURCES_DIR", "/g")])).as_deref(), Some("\x1b]9;4;3\x07"));
        assert_eq!(progress(false, env(&[("GHOSTTY_RESOURCES_DIR", "/g")])).as_deref(), Some("\x1b]9;4;0\x07"));
        assert_eq!(progress(true, env(&[("LC_TERMINAL", "iTerm2"), ("LC_TERMINAL_VERSION", "3.6.6")])).as_deref(), Some("\x1b]9;4;3\x07"));
        assert_eq!(progress(true, env(&[("LC_TERMINAL", "iTerm2"), ("LC_TERMINAL_VERSION", "3.5.14")])), None);
        assert_eq!(progress(true, env(&[("TERM_PROGRAM", "tuni")])).as_deref(), Some("\x1b]9;4;3\x07"));
        assert_eq!(progress(true, env(&[("TERM_PROGRAM", "rex")])), None);
        assert_eq!(progress(true, env(&[("WEZTERM_EXECUTABLE", "/w")])), None);
        assert_eq!(progress(true, env(&[("GHOSTTY_RESOURCES_DIR", "/g"), ("TMUX", "/t")])).as_deref(), Some("\x1bPtmux;\x1b\x1b]9;4;3\x07\x1b\\"));
    }

    #[test]
    fn rex_gets_the_program_status_record() {
        let rex = || env(&[("TERM_PROGRAM", "rex")]);
        let working = Status { state: "working", kind: None, msg: String::new() };
        assert_eq!(program_status(&working, rex()).as_deref(), Some("\x1b]7501;state=working:app=agent\x1b\\"));
        let blocked = Status { state: "blocked", kind: Some("question"), msg: "Which?\n\x1b]0;x".to_string() };
        assert_eq!(program_status(&blocked, rex()).as_deref(), Some("\x1b]7501;state=blocked:kind=question:app=agent:msg=V2hpY2g/\x1b\\"));
        assert_eq!(program_status(&working, env(&[("GHOSTTY_RESOURCES_DIR", "/g")])), None);
    }

    #[test]
    fn text_cannot_inject_escape_sequences() {
        let vars = env(&[("GHOSTTY_RESOURCES_DIR", "/g")]);
        assert_eq!(sequence("agent", "\nrm x\x1b]0;evil\x07; ok\nmore", vars), "\x1b]777;notify;agent;rm x]0,evil, ok\x07");
    }

    #[test]
    fn terminal_vars_name_every_variable_read() {
        let read = std::cell::RefCell::new(Vec::new());
        let recording = |ghostty: bool| {
            let read = &read;
            move |name: &str| {
                read.borrow_mut().push(name.to_string());
                (ghostty && name == "GHOSTTY_RESOURCES_DIR").then(|| "/g".to_string())
            }
        };
        for ghostty in [false, true] {
            sequence("agent", "Done", recording(ghostty));
            progress(true, recording(ghostty));
            program_status(&Status { state: "idle", kind: None, msg: String::new() }, recording(ghostty));
        }
        for name in read.into_inner() {
            assert!(TERMINAL_VARS.contains(&name.as_str()), "{name}");
        }
    }
}
