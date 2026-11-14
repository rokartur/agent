use std::time::Duration;

/// The model's bash calls: the command goes through `rtk rewrite`, the output loses ANSI codes and keeps its last `max_bytes`.
/// shortcut: pi's build/test/git/linter output filters are not ported; `rtk rewrite` already routes those commands through rtk.
pub struct Rtk {
    enabled: bool,
    /// The byte cap on the kept tail of bash output; the full output goes to a temp file.
    pub max_bytes: usize,
}

impl Rtk {
    /// The same tail as pi's bash: at 24KB, printing a 1000-line source file took a second call for its head.
    pub const ON: Rtk = Rtk { enabled: true, max_bytes: super::MAX_BYTES };
    /// For `!` commands the user typed and for every call when the rtk setting is off: no rewrite, no ANSI strip, the MAX_BYTES tail.
    pub const OFF: Rtk = Rtk { enabled: false, max_bytes: super::MAX_BYTES };

    /// The command bash runs: rtk's rewrite of `command`, or `command` itself when rtk is off, missing, slow, refuses or has no rewrite.
    pub fn rewrite(&self, command: &str) -> String {
        if !self.enabled || matches!(command.split_whitespace().next(), None | Some("rtk")) {
            return command.to_string();
        }
        // Runs before bash starts, where abort cannot reach it, so a hung rtk must not block the call.
        match crate::process::output_within(crate::process::command("rtk").args(["rewrite", command]), Duration::from_secs(2)) {
            Ok(Some(output)) => rewritten(command, output.status.code(), &String::from_utf8_lossy(&output.stdout)).unwrap_or_else(|| command.to_string()),
            Ok(None) | Err(_) => command.to_string(),
        }
    }

    /// The bash output without ANSI codes, so they spend none of `max_bytes`.
    pub fn strip(&self, output: String) -> String {
        if self.enabled && output.contains('\x1b') { strip_ansi(&output) } else { output }
    }
}

/// `rtk rewrite` exits 0 or 3 with the new command; 1 means no rtk equivalent, 2 means rtk refuses.
fn rewritten(original: &str, exit_code: Option<i32>, stdout: &str) -> Option<String> {
    if !matches!(exit_code, Some(0 | 3)) {
        return None;
    }
    let command = stdout.trim();
    (!command.is_empty() && command != original.trim() && !has_short_git_status(command)).then(|| command.to_string())
}

/// shortcut: rtk 1.2.0 prints `git status --short/--porcelain` without its final newline (rtk-ai/rtk#3303), which glues the
/// last path to the next command's output; such a command runs unrewritten until rtk fixes it.
fn has_short_git_status(rewritten: &str) -> bool {
    rewritten.split("rtk git status").skip(1).any(|rest| {
        let args = rest.split(['&', '|', ';', '\n']).next().unwrap_or_default();
        args.split_whitespace().any(|arg| arg.starts_with("--porcelain") || arg == "--short" || (arg.starts_with('-') && !arg.starts_with("--") && arg.contains('s')))
    })
}

/// Removes CSI (`ESC [ ... final byte`), OSC (`ESC ] ... BEL` or `ESC ] ... ESC \`) and charset (`ESC ( B`) sequences.
pub fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\x1b' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('[') => while chars.next().is_some_and(|ch| !('\x40'..='\x7e').contains(&ch)) {},
            Some(']') => {
                // An OSC cut off by a killed command ends at the newline, not at the end of the output.
                while let Some(ch) = chars.next() {
                    if ch == '\n' {
                        out.push(ch);
                        break;
                    }
                    if ch == '\x07' || (ch == '\x1b' && chars.next_if_eq(&'\\').is_some()) {
                        break;
                    }
                }
            }
            Some('(' | ')' | '*' | '+') => {
                chars.next();
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrite_needs_success_and_a_different_command() {
        assert_eq!(rewritten("git status", Some(3), "rtk git status\n").as_deref(), Some("rtk git status"));
        assert_eq!(rewritten("git log", Some(0), "rtk git log").as_deref(), Some("rtk git log"));
        assert_eq!(rewritten("echo hi", Some(1), ""), None);
        assert_eq!(rewritten("rm x", Some(2), "rtk rm x"), None);
        assert_eq!(rewritten("ls", Some(0), "ls\n"), None);
        assert_eq!(rewritten("ls", Some(0), "  "), None);
    }

    #[test]
    fn short_git_status_runs_unrewritten() {
        for flags in ["--porcelain", "--porcelain=v2", "-s", "-sb", "--short"] {
            assert_eq!(rewritten("x", Some(3), &format!("rtk git add a && rtk git status {flags} && rtk git diff --stat")), None, "{flags}");
        }
        let long = "rtk git status && rtk git diff -s --stat";
        assert_eq!(rewritten("x", Some(3), long).as_deref(), Some(long));
    }

    #[test]
    fn output_loses_ansi_only_when_enabled() {
        assert_eq!(Rtk::ON.strip("\x1b[31mred\x1b[0m \x1b]8;;http://x\x07link\x1b]8;;\x1b\\".into()), "red link");
        assert_eq!(Rtk::OFF.strip("\x1b[1mx".into()), "\x1b[1mx");
        assert_eq!(strip_ansi("a\x1b(Bb\x1b[2~c\x1b]8;;x\nd"), "abc\nd");
    }

    #[test]
    fn rewrite_skips_rtk_when_off_or_already_rtk() {
        assert_eq!(Rtk::OFF.rewrite("git status"), "git status");
        assert_eq!(Rtk::ON.rewrite("rtk\tgit status"), "rtk\tgit status");
        assert_eq!(rewritten("ls\n", Some(0), "ls\n"), None);
    }
}
