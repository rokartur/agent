use serde_json::Value;
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub mod ask;
pub mod bg;
pub mod conflicts;
pub mod goal;
pub mod guard;
pub mod hashline;
pub mod plan;
pub mod rtk;
pub mod todo;
pub mod web;

use crate::process;
use hashline::{Anchors, resolve, str_arg};
use rtk::Rtk;

pub const MAX_LINES: usize = 2000;
pub const MAX_BYTES: usize = 50 * 1024;
const MAX_COLUMNS: usize = 500;
const SEARCH_ANCHOR_BUDGET: u32 = 10 * MAX_LINES as u32;

const SCHEMAS: &str = include_str!("tools.json");

/// Every tool in tools.json order; `Agent` carries the swarm prompt like jcode's `swarm`.
/// One fixed list: the tools head the cached prompt prefix, so changing it mid-session rewrites the whole cache.
pub fn schemas(swarm_prompt: &str, context_tree_query: bool) -> Value {
    let all: Vec<Value> = serde_json::from_str(SCHEMAS).expect("tools.json is valid JSON");
    let mut tools: Vec<Value> = all.into_iter().filter(|tool| context_tree_query || tool["name"] != "ContextTreeQuery").collect();
    if let Some(tool) = tools.iter_mut().find(|tool| tool["name"] == "Agent") {
        let base = tool["description"].as_str().expect("tools.json describes Agent");
        tool["description"] = format!("{base}\n\nSwarm prompt (user-tunable via ~/.agents/swarm-prompt.md):\n{swarm_prompt}").into();
    }
    Value::Array(tools)
}

/// Runs one tool call. The bool is true when the result is an error. Setting `abort` kills a running bash or rg.
pub fn run(name: &str, input: &Value, cwd: &Path, anchors: &mut Anchors, rtk: &Rtk, abort: &AtomicBool) -> (String, bool) {
    let result = match name {
        "Read" => anchors.read(cwd, input),
        "Bash" => bash(input, cwd, rtk, abort),
        "Write" => write(input, cwd),
        "ReplaceLines" => anchors.replace(input),
        "Edit" => anchors.replace_match(cwd, input),
        "InsertLines" => anchors.insert(input),
        "Grep" => rg(input, cwd, anchors, abort),
        _ => Err(format!("Tool {name} not found")),
    };
    match result {
        Ok(text) => (text, false),
        Err(text) => (text, true),
    }
}

enum Killed {
    Timeout,
    Aborted,
}

fn bash(input: &Value, cwd: &Path, rtk: &Rtk, abort: &AtomicBool) -> Result<String, String> {
    let command = rtk.rewrite(str_arg(input, "command")?);
    let timeout = input["timeout"].as_f64().filter(|ms| *ms > 0.0);
    // A timeout too large for an Instant is no timeout.
    let deadline = timeout.and_then(|ms| Instant::now().checked_add(Duration::try_from_secs_f64(ms / 1000.0).ok()?));
    let child = process::command("bash").arg("-c").arg(format!("exec 2>&1; {command}")).current_dir(cwd).stdout(Stdio::piped()).spawn().map_err(|e| format!("Cannot start bash: {e}"))?;
    let mut capture = Capture::default();
    let (status, killed) = wait_streaming(child, abort, deadline, |chunk| capture.push(chunk))?;
    let output = truncate_tail(rtk.strip(String::from_utf8_lossy(&capture.text).into_owned()), rtk.max_bytes, capture.spilled());
    match killed {
        Some(Killed::Timeout) => return Err(append_status(&output, &format!("Command timed out after {} ms", timeout.unwrap_or_default()))),
        Some(Killed::Aborted) => return Err(append_status(&output, "Command aborted")),
        None => {}
    }
    let code = exit_code(status);
    if code != 0 {
        return Err(append_status(&output, &format!("Command exited with code {code}")));
    }
    Ok(if output.is_empty() { "(no output)".into() } else { output })
}

fn exit_code(status: ExitStatus) -> i32 {
    status.code().unwrap_or_else(|| 128 + status.signal().expect("no exit code means a signal ended it"))
}

/// How long output may keep arriving after the command exits, so `server &` cannot hold the call.
const DRAIN_AFTER_EXIT: Duration = Duration::from_millis(500);

/// Feeds `child`'s stdout to `sink` until the child exits, or kills its process group once `abort` is set or
/// `deadline` passes.
fn wait_streaming(mut child: Child, abort: &AtomicBool, deadline: Option<Instant>, mut sink: impl FnMut(&[u8])) -> Result<(ExitStatus, Option<Killed>), String> {
    const POLL: Duration = Duration::from_millis(10);
    let chunks = read_chunks(child.stdout.take().expect("stdout is piped"));
    let killed = loop {
        match chunks.recv_timeout(POLL) {
            Ok(chunk) => sink(&chunk),
            Err(mpsc::RecvTimeoutError::Disconnected) => std::thread::sleep(POLL),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if abort.load(Ordering::Relaxed) {
            break Some(Killed::Aborted);
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            break Some(Killed::Timeout);
        }
        match child.try_wait() {
            Ok(Some(_)) => break None,
            Ok(None) => {}
            Err(e) => {
                process::signal_group(child.id(), process::SIGKILL);
                let _ = child.wait();
                return Err(e.to_string());
            }
        }
    };
    if killed.is_some() {
        // The child is not reaped yet, so its pid cannot have been reused for another group.
        process::signal_group(child.id(), process::SIGKILL);
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    let drain_until = Instant::now() + DRAIN_AFTER_EXIT;
    while let Some(left) = drain_until.checked_duration_since(Instant::now())
        && let Ok(chunk) = chunks.recv_timeout(left)
    {
        sink(&chunk);
    }
    Ok((status, killed))
}

/// Reads `pipe` on its own thread until EOF. It keeps reading after the receiver is gone, so a background job that
/// still writes there does not die of SIGPIPE.
fn read_chunks(mut pipe: impl Read + Send + 'static) -> mpsc::Receiver<Vec<u8>> {
    let (sender, chunks) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buffer = vec![0; 64 * 1024];
        loop {
            match pipe.read(&mut buffer) {
                Ok(0) => return,
                Ok(read) => {
                    let _ = sender.send(buffer[..read].to_vec());
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => return,
            }
        }
    });
    chunks
}

/// Bash keeps at most this much output in memory; beyond it the whole stream goes to a spill file.
const KEEP_BYTES: usize = 4 * MAX_BYTES;

/// A command's output: all of it, or its last KEEP_BYTES once it outgrew them.
#[derive(Default)]
struct Capture {
    text: Vec<u8>,
    dropped_lines: usize,
    spill: Option<SpillFile>,
}

impl Capture {
    fn push(&mut self, chunk: &[u8]) {
        self.text.extend_from_slice(chunk);
        match &mut self.spill {
            Some(spill) => spill.write(chunk),
            None if self.text.len() > KEEP_BYTES => {
                let mut spill = SpillFile::create("bash");
                spill.write(&self.text);
                self.spill = Some(spill);
            }
            None => return,
        }
        let cut = self.text.len().saturating_sub(KEEP_BYTES);
        self.dropped_lines += self.text[..cut].iter().filter(|&&byte| byte == b'\n').count();
        self.text.drain(..cut);
    }

    fn spilled(&self) -> Option<Spilled> {
        let spill = self.spill.as_ref()?;
        Some(Spilled { lines: self.dropped_lines, path: spill.path.clone() })
    }
}

/// The part of a stream that Capture let go of: its line count, and the file with the whole stream.
struct Spilled {
    lines: usize,
    path: String,
}

fn append_status(output: &str, status: &str) -> String {
    if output.is_empty() { status.to_string() } else { format!("{output}\n\n{status}") }
}

fn write(input: &Value, cwd: &Path) -> Result<String, String> {
    let path = resolve(cwd, str_arg(input, "file_path")?);
    let content = str_arg(input, "content")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("Cannot create {}: {e}", parent.display()))?;
    }
    // Reading a FIFO blocks and /dev/zero never ends; abort cannot stop either.
    if path.exists() && !path.is_file() {
        return Err(format!("{} is not a regular file.", path.display()));
    }
    // The rename in replace_file would succeed over a file the user cannot even read.
    if let Err(e) = std::fs::File::open(&path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        return Err(format!("Cannot read {}: {e}", path.display()));
    }
    hashline::replace_file(&path, content.as_bytes())?;
    let path = path.canonicalize().map_err(|e| e.to_string())?;
    // No echo of the content: it is already in the call, and every later turn would pay for it twice.
    Ok(format!("Wrote {} lines to {}.", content.lines().count(), path.display()))
}

/// pi file-search's rg argv as JSON events: the pattern after `--`, counts clamped.
fn rg_args(input: &Value) -> Result<Vec<String>, String> {
    let first = match input["output_mode"].as_str() {
        Some("files_with_matches") => "--files-with-matches",
        Some("count") => "--count",
        _ => "--json",
    };
    let mut args = vec![first.to_string()];
    args.push(if input["-i"].as_bool() == Some(true) { "--ignore-case" } else { "--smart-case" }.into());
    if input["fixed_strings"].as_bool() == Some(true) {
        args.push("--fixed-strings".into());
    }
    if input["hidden"].as_bool() == Some(true) {
        args.push("--hidden".into());
    }
    if !input["-C"].is_null() {
        args.extend(["--context".into(), clamped(&input["-C"], 0, 0, MAX_CONTEXT_LINES)]);
    }
    if let Some(glob) = input["glob"].as_str().filter(|g| !g.is_empty()) {
        args.extend(["--glob".into(), glob.into()]);
    }
    if let Some(file_type) = input["type"].as_str().filter(|t| !t.is_empty()) {
        args.extend(["--type".into(), file_type.into()]);
    }
    args.extend(["--max-count".into(), clamped(&input["limit"], DEFAULT_MATCHES, 1, MAX_MATCHES)]);
    args.extend(["--".into(), str_arg(input, "pattern")?.into()]);
    args.extend(search_path(input));
    Ok(args)
}

fn clamped(value: &Value, default: u64, min: u64, max: u64) -> String {
    let number = value.as_f64().map_or(default, |n| n.floor().max(0.0) as u64);
    number.clamp(min, max).to_string()
}

/// The `path` argument with pi's normalization: a leading `@` dropped, `~` expanded.
fn search_path(input: &Value) -> Option<String> {
    let path = input["path"].as_str()?.trim();
    let path = path.strip_prefix('@').unwrap_or(path);
    // Without HOME `~` stays literal, so the search fails instead of running on `/`.
    let home = std::env::var("HOME");
    let path = match (path.strip_prefix('~'), home) {
        (Some(""), Ok(home)) => home,
        (Some(rest), Ok(home)) if rest.starts_with('/') => format!("{home}{rest}"),
        _ => path.to_string(),
    };
    (!path.is_empty()).then_some(path)
}

fn rg(input: &Value, cwd: &Path, anchors: &mut Anchors, abort: &AtomicBool) -> Result<String, String> {
    let args = rg_args(input)?;
    let output = search("rg", &args, cwd, abort, RG_OK)?;
    // rg refuses --json next to --files-with-matches or --count, so those modes return its plain rows.
    let rows = if args[0] == "--json" { rg_rows(&output, cwd, anchors)? } else { output };
    Ok(if rows.is_empty() { "No matches found".into() } else { truncate_search(rows, "rg") })
}

/// rg's JSON events as `path:anchor│line` rows, `path-anchor│line` for context, so an edit can follow without a read.
/// A file the anchors cannot track (binary, not UTF-8) shows line numbers instead, and so does every file past the
/// MAX_LINES rows the page shows, so anchoring them costs nothing. Past SEARCH_ANCHOR_BUDGET new anchors in one call
/// the rest show line numbers too: anchors are never freed, and 2000 hit files of 2000 lines would use up the session's.
fn rg_rows(json: &str, cwd: &Path, anchors: &mut Anchors) -> Result<String, String> {
    let mut rows = Vec::new();
    let mut tracked: Option<PathBuf> = None;
    let minted_before = anchors.minted();
    for line in json.lines() {
        let event: Value = serde_json::from_str(line).map_err(|e| format!("Cannot parse rg output: {e}"))?;
        let data = &event["data"];
        let path = data["path"]["text"].as_str().unwrap_or("[path is not UTF-8]");
        let separator = match event["type"].as_str() {
            Some("begin") => {
                let in_budget = rows.len() < MAX_LINES && anchors.minted() - minted_before < SEARCH_ANCHOR_BUDGET;
                tracked = if in_budget { cwd.join(path).canonicalize().ok().filter(|file| anchors.track(file, MAX_LINES).is_ok()) } else { None };
                continue;
            }
            Some("match") => ':',
            Some("context") => '-',
            _ => continue,
        };
        let number = data["line_number"].as_u64().ok_or("rg event without line_number")? as usize;
        let mark = match tracked.as_deref().and_then(|file| anchors.line_anchor(file, number)) {
            Some(anchor) => format!("{anchor}│"),
            None => format!("{number}{separator}"),
        };
        let text = data["lines"]["text"].as_str().map_or("[line is not UTF-8]", |text| text.trim_end_matches(['\n', '\r']));
        rows.push(format!("{path}{separator}{mark}{}", cut_columns(text)));
    }
    Ok(rows.join("\n"))
}

/// rg ignores --max-columns in JSON mode, so long lines are cut here, the way its preview cuts them.
fn cut_columns(text: &str) -> String {
    match text.char_indices().nth(MAX_COLUMNS) {
        Some((end, _)) => format!("{} [... omitted end of long line]", &text[..end]),
        None => text.to_string(),
    }
}

/// rg exits 1 for "no matches", and 2 when a path failed even after printing matches for the others.
const RG_OK: &[i32] = &[0, 1, 2];
const MAX_CONTEXT_LINES: u64 = 20;
const DEFAULT_MATCHES: u64 = 100;
const MAX_MATCHES: u64 = 1000;

fn search(program: &str, args: &[String], cwd: &Path, abort: &AtomicBool, ok_codes: &[i32]) -> Result<String, String> {
    let mut child = process::command(program).args(args).current_dir(cwd).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| format!("Cannot run {program}: {e}"))?;
    let mut stderr = child.stderr.take().expect("stderr is piped");
    let stderr = std::thread::spawn(move || {
        let mut text = Vec::new();
        let _ = stderr.read_to_end(&mut text);
        String::from_utf8_lossy(&text).trim().to_string()
    });
    let mut stdout = Vec::new();
    // Headless and print mode have no Esc, so a walk of `/` or a hung network mount needs its own limit.
    let deadline = Instant::now().checked_add(Duration::from_secs(120));
    let (status, killed) = wait_streaming(child, abort, deadline, |chunk| stdout.extend_from_slice(chunk))?;
    match killed {
        Some(Killed::Timeout) => return Err(format!("{program} timed out after 120s")),
        Some(Killed::Aborted) => return Err(format!("{program} aborted")),
        None => {}
    }
    let code = exit_code(status);
    let stdout = String::from_utf8_lossy(&stdout).trim_end_matches('\n').to_string();
    // rg exits 2 when any path failed; with --json it still prints a summary event, so only a match makes it a partial success.
    let matched = if args.iter().any(|arg| arg == "--json") { stdout.contains("\"type\":\"match\"") } else { !stdout.is_empty() };
    if !ok_codes.contains(&code) || (code == 2 && !matched) {
        let stderr = stderr.join().expect("the stderr reader does not panic");
        return Err(format!("{program} failed: {}", if stderr.is_empty() { format!("exit code {code}") } else { stderr }));
    }
    Ok(stdout)
}

/// pi file-search's page: the first MAX_LINES / MAX_BYTES, with the full output in a temp file.
fn truncate_search(text: String, label: &str) -> String {
    let Some(head) = cut_head(&text, label) else { return text };
    let content = &text[..head.bytes];
    format!(
        "{content}\n\n[Output truncated: {} of {} lines ({} of {}). Full output saved to: {}. Read it with offset={} to continue.]",
        head.lines,
        head.total,
        format_size(content.len()),
        format_size(text.len()),
        head.path,
        head.lines + 1
    )
}

/// Keeps the first MAX_LINES / MAX_BYTES of `text`; the full text goes to a temp file named after `label`.
pub fn truncate_head(text: String, label: &str) -> String {
    let Some(head) = cut_head(&text, label) else { return text };
    format!("{}\n\n[Showing lines 1-{} of {}. Full output: {}]", &text[..head.bytes], head.lines, head.total, head.path)
}

struct Head {
    bytes: usize,
    lines: usize,
    total: usize,
    path: String,
}

/// The longest head of `text` within MAX_LINES / MAX_BYTES, or None when it all fits; spills the full text.
/// A first line over MAX_BYTES is cut, so the head is never empty.
fn cut_head(text: &str, label: &str) -> Option<Head> {
    let total = text.lines().count();
    if total <= MAX_LINES && text.len() <= MAX_BYTES {
        return None;
    }
    let path = spill(text, label);
    let (lines, bytes) = fit(text.split_inclusive('\n'), MAX_BYTES);
    if lines == 0 {
        return Some(Head { bytes: text.floor_char_boundary(MAX_BYTES), lines: 1, total, path });
    }
    Some(Head { bytes: text[..bytes].strip_suffix('\n').map_or(bytes, str::len), lines, total, path })
}

/// How many of `lines`, at most MAX_LINES, fit whole in `max_bytes`, and their bytes.
fn fit<'a>(lines: impl Iterator<Item = &'a str>, max_bytes: usize) -> (usize, usize) {
    let (mut count, mut bytes) = (0, 0);
    for line in lines.take(MAX_LINES) {
        if bytes + line.len() > max_bytes {
            break;
        }
        bytes += line.len();
        count += 1;
    }
    (count, bytes)
}

/// Keeps the last MAX_LINES / `max_bytes` of `text`, like pi's bash tool. `spilled` is the part of the stream
/// before `text` that was already let go of.
fn truncate_tail(text: String, max_bytes: usize, spilled: Option<Spilled>) -> String {
    let body = text.trim_end_matches('\n');
    let dropped_lines = spilled.as_ref().map_or(0, |spilled| spilled.lines);
    let total = dropped_lines + body.lines().count();
    if spilled.is_none() && total <= MAX_LINES && text.len() <= max_bytes {
        return text;
    }
    let path = match spilled {
        Some(spilled) => spilled.path,
        None => spill(&text, "bash"),
    };
    let (kept, bytes) = fit(body.split_inclusive('\n').rev(), max_bytes);
    if kept == 0 {
        let start = body.ceil_char_boundary(body.len().saturating_sub(max_bytes));
        return format!("{}\n\n[Showing the last {} of line {total}. Full output: {path}]", &body[start..], format_size(body.len() - start));
    }
    format!("{}\n\n[Showing lines {}-{total} of {total}. Full output: {path}]", &body[body.len() - bytes..], total - kept + 1)
}

/// Saves `text` to its own spill file and returns the path, or why it could not be saved.
fn spill(text: &str, label: &str) -> String {
    let mut file = SpillFile::create(label);
    file.write(text.as_bytes());
    file.path
}

/// An owner-only temp file for output too long to show; `path` says why it is missing when it could not be written.
struct SpillFile {
    file: Option<File>,
    path: String,
}

impl SpillFile {
    fn create(label: &str) -> Self {
        match private_file(label, "log") {
            Ok((file, path)) => Self { file: Some(file), path: path.display().to_string() },
            Err(e) => Self { file: None, path: format!("(not saved: {e})") },
        }
    }

    fn write(&mut self, bytes: &[u8]) {
        let Some(file) = &mut self.file else { return };
        if let Err(e) = file.write_all(bytes) {
            self.file = None;
            self.path = format!("(not saved: {e})");
        }
    }
}

pub(crate) fn spill_dir() -> PathBuf {
    std::env::temp_dir().join(format!("agent-{}", std::process::id()))
}

/// A new owner-only `{stem}-{n}.{extension}` in this run's spill dir.
fn private_file(stem: &str, extension: &str) -> Result<(File, PathBuf), String> {
    static CREATED: AtomicUsize = AtomicUsize::new(0);
    let dir = spill_dir();
    crate::process::private_dir(&dir)?;
    let path = dir.join(format!("{stem}-{}.{extension}", CREATED.fetch_add(1, Ordering::Relaxed)));
    let file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path).map_err(|e| format!("Cannot create {}: {e}", path.display()))?;
    Ok((file, path))
}

/// Spill files are only for this run, so quitting removes them.
pub fn remove_spills() {
    let _ = std::fs::remove_dir_all(spill_dir());
}

pub fn format_size(bytes: usize) -> String {
    match bytes {
        b if b < 1024 => format!("{b}B"),
        // Under 1023.95KB, which `{:.1}` would print as 1024.0KB.
        b if b < 1024 * 1024 - 51 => format!("{:.1}KB", b as f64 / 1024.0),
        b => format!("{:.1}MB", b as f64 / (1024.0 * 1024.0)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    fn run_bash(input: Value, abort: &Arc<AtomicBool>) -> (String, bool) {
        run("Bash", &input, Path::new("/tmp"), &mut Anchors::new("t"), &Rtk::OFF, abort)
    }

    #[test]
    fn context_tree_query_is_offered_only_when_asked() {
        let offers = |query: bool| schemas("", query).as_array().unwrap().iter().any(|tool| tool["name"] == "ContextTreeQuery");
        assert!(offers(true));
        assert!(!offers(false));
    }

    #[test]
    fn bash_reports_exit_code_and_merged_output() {
        let (out, err) = run_bash(json!({"command": "echo out; echo err >&2; exit 3"}), &Arc::default());
        assert!(err);
        assert_eq!(out, "out\nerr\n\n\nCommand exited with code 3");
    }

    #[test]
    fn bash_timeout_kills_the_command() {
        let (out, err) = run_bash(json!({"command": "echo start; sleep 30", "timeout": 500}), &Arc::default());
        assert!(err);
        assert_eq!(out, "start\n\n\nCommand timed out after 500 ms");
    }

    #[test]
    fn bash_commands_cannot_take_over_the_terminal() {
        // `zsh -i` from a command took the tty's foreground group, and the TUI stopped on its next read.
        let (out, _) = run_bash(json!({"command": "if : </dev/tty; then echo tty; else echo no tty; fi"}), &Arc::default());
        assert!(out.ends_with("no tty\n"), "{out}");
    }

    #[test]
    fn abort_kills_a_running_command() {
        let abort = Arc::new(AtomicBool::new(false));
        let setter = abort.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            setter.store(true, Ordering::Relaxed);
        });
        let started = Instant::now();
        let (out, err) = run_bash(json!({"command": "echo start; sleep 30"}), &abort);
        assert!(err && started.elapsed() < Duration::from_secs(5));
        assert_eq!(out, "start\n\n\nCommand aborted");
    }

    #[test]
    fn tail_truncation_keeps_the_end() {
        let text: String = (1..=2500).map(|i| format!("{i}\n")).collect();
        let out = truncate_tail(text, MAX_BYTES, None);
        assert!(out.starts_with("501\n"));
        assert!(out.contains("[Showing lines 501-2500 of 2500."));
    }

    #[test]
    fn tail_truncation_cuts_one_long_line_on_a_char_boundary() {
        let out = truncate_tail(format!("{}x", "é".repeat(10)), 6, None);
        assert!(out.starts_with("ééx\n\n[Showing the last 5B of line 1."), "{out}");
    }

    #[test]
    fn tail_truncation_ignores_trailing_blank_lines_and_carriage_returns() {
        let out = truncate_tail(format!("{}\n\n", "a".repeat(13_000)), 12_000, None);
        assert!(out.starts_with(&"a".repeat(12_000)), "{}", &out[..40]);
        let out = truncate_tail(format!("{}\nok\n\n\n", "é".repeat(10)), 6, None);
        assert!(out.starts_with("ok\n\n[Showing lines 2-2 of 2."), "{out}");
        let out = truncate_tail("éé\r\n".repeat(3000), 12_000, None);
        assert!(out.starts_with("éé\r\n") && out.contains("[Showing lines 1001-3000 of 3000."), "{}", &out[out.len() - 120..]);
    }

    #[test]
    fn tail_of_a_spilled_stream_counts_the_lines_it_dropped() {
        let spilled = Spilled { lines: 7000, path: "/tmp/full.log".into() };
        let out = truncate_tail("x\ny\n".into(), MAX_BYTES, Some(spilled));
        assert_eq!(out, "x\ny\n\n[Showing lines 7001-7002 of 7002. Full output: /tmp/full.log]");
    }

    #[test]
    fn head_truncation_keeps_whole_lines_of_crlf_text_and_part_of_an_overlong_first_line() {
        let out = truncate_head("éé\r\n".repeat(2500), "test");
        assert!(out.starts_with("éé\r\n") && out.contains("[Showing lines 1-2000 of 2500."), "{}", &out[out.len() - 120..]);
        let out = truncate_head(format!("{}\nb\n", "a".repeat(60_000)), "test");
        assert!(out.starts_with(&"a".repeat(MAX_BYTES)) && out.contains("[Showing lines 1-1 of 2."), "{}", &out[out.len() - 120..]);
    }

    #[test]
    fn spills_get_their_own_owner_only_files() {
        use std::os::unix::fs::PermissionsExt;
        let (first, second) = (spill("one", "test"), spill("two", "test"));
        assert_ne!(first, second);
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "one");
        assert_eq!(std::fs::metadata(&second).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn bash_does_not_wait_for_background_jobs_or_panic_on_a_huge_timeout() {
        let started = Instant::now();
        let (out, err) = run_bash(json!({"command": "sleep 3 & echo started", "timeout": 1e20}), &Arc::default());
        assert!(!err && started.elapsed() < Duration::from_secs(2), "{out}");
        assert_eq!(out, "started\n");
    }

    #[test]
    fn bash_output_over_the_memory_cap_streams_to_the_full_log() {
        let (out, err) = run_bash(json!({"command": "seq 1 100000"}), &Arc::default());
        assert!(!err && out.contains("[Showing lines 98001-100000 of 100000. Full output: "), "{}", &out[out.len() - 160..]);
        let path = out.rsplit("Full output: ").next().unwrap().trim_end_matches(']');
        let full = std::fs::read_to_string(path).unwrap();
        assert_eq!(full.lines().count(), 100_000);
        assert!(full.starts_with("1\n2\n") && full.ends_with("\n100000\n"));
    }

    #[test]
    fn rg_keeps_matches_when_another_path_fails() {
        let dir = std::env::temp_dir().join(format!("agent-rg-partial-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "needle\n").unwrap();
        let rows = search("rg", &["needle".into(), "a.txt".into(), "missing.txt".into()], &dir, &Arc::default(), RG_OK).unwrap();
        assert_eq!(rows, "a.txt:needle");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rg_files_and_count_modes_skip_anchors() {
        let dir = std::env::temp_dir().join(format!("agent-rg-modes-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "needle\nneedle\n").unwrap();
        let mut anchors = Anchors::new("t");
        let abort = AtomicBool::new(false);
        assert_eq!(rg(&json!({"pattern": "needle", "output_mode": "files_with_matches"}), &dir, &mut anchors, &abort).unwrap(), "a.txt");
        assert_eq!(rg(&json!({"pattern": "needle", "output_mode": "count"}), &dir, &mut anchors, &abort).unwrap(), "a.txt:2");
        assert_eq!(anchors.minted(), 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rg_reports_a_missing_path_instead_of_no_matches() {
        let error = rg(&json!({"pattern": "needle", "path": "/nonexistent-agent-path"}), Path::new("/tmp"), &mut Anchors::new("t"), &AtomicBool::new(false)).unwrap_err();
        assert!(error.starts_with("rg failed"), "{error}");
    }

    #[test]
    fn long_bash_output_keeps_its_end_and_saves_the_full_log() {
        // 30 KB in 2000 lines: under the line and byte caps, over rtk's 24 KB.
        let (out, _) =
            run("Bash", &json!({"command": "awk 'BEGIN { for (i = 1; i <= 2000; i++) print \"line\", i, \"xxxx\" }'"}), Path::new("/tmp"), &mut Anchors::new("t"), &Rtk::ON, &Arc::default());
        assert!(out.contains("\nline 2000 xxxx\n"), "{}", &out[out.len().saturating_sub(300)..]);
        let path = out.rsplit("Full output: ").next().unwrap().trim_end_matches(']');
        assert_eq!(std::fs::read_to_string(path).unwrap(), (1..=2000).map(|i| format!("line {i} xxxx\n")).collect::<String>());
    }

    #[test]
    fn rg_hits_in_a_long_file_show_line_numbers_and_mint_no_anchors() {
        let dir = std::env::temp_dir().join(format!("agent-rg-long-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("long.txt"), format!("needle\n{}", "x\n".repeat(MAX_LINES))).unwrap();
        let found = rg(&json!({"pattern": "needle"}), &dir, &mut Anchors::new("t"), &AtomicBool::new(false)).unwrap();
        assert_eq!(found, "long.txt:1:needle");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn one_rg_call_stops_minting_anchors_past_its_budget() {
        let dir = std::env::temp_dir().join(format!("agent-rg-budget-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file_lines = MAX_LINES - 1;
        let files = SEARCH_ANCHOR_BUDGET as usize / file_lines + 2;
        for index in 0..files {
            std::fs::write(dir.join(format!("{index}.txt")), format!("needle\n{}", "x\n".repeat(file_lines - 1))).unwrap();
        }
        let found = rg(&json!({"pattern": "needle"}), &dir, &mut Anchors::new("t"), &AtomicBool::new(false)).unwrap();
        assert_eq!(found.lines().filter(|row| row.ends_with(":1:needle")).count(), 1, "{found}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rg_rows_carry_anchors_that_replace_accepts_without_a_read() {
        let dir = std::env::temp_dir().join(format!("agent-rg-anchors-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "alpha\nbeta\ngamma\n").unwrap();
        let mut anchors = Anchors::new("");
        let abort = Arc::new(AtomicBool::new(false));
        let (found, is_error) = run("Grep", &json!({"pattern": "beta", "-C": 1}), &dir, &mut anchors, &Rtk::OFF, &abort);
        assert!(!is_error, "{found}");
        let rows: Vec<&str> = found.lines().collect();
        assert_eq!(rows.len(), 3, "{found}");
        assert!(rows[0].starts_with("a.txt-") && rows[0].ends_with("│alpha"), "{found}");
        let anchor = rows[1].strip_prefix("a.txt:").and_then(|row| row.strip_suffix("│beta")).expect(&found);
        let (replaced, is_error) = run("ReplaceLines", &json!({"remove_from": anchor, "remove_to": anchor, "text": "BETA"}), &dir, &mut anchors, &Rtk::OFF, &abort);
        assert!(!is_error, "{replaced}");
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "alpha\nBETA\ngamma\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn write_reports_the_line_count_without_echoing_the_content() {
        let dir = std::env::temp_dir().join(format!("agent-write-echo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut anchors = Anchors::new("");
        let (text, is_error) = run("Write", &json!({"file_path": "a.txt", "content": "alpha\nbeta\n"}), &dir, &mut anchors, &Rtk::OFF, &Arc::new(AtomicBool::new(false)));
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(!is_error && text.contains("2 lines") && !text.contains("alpha"), "{text}");
    }

    #[test]
    fn write_refuses_to_replace_a_file_it_cannot_read() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("agent-write-unreadable-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("locked.txt");
        std::fs::write(&file, "keep").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).unwrap();
        let mut anchors = Anchors::new("");
        let (text, is_error) = run("Write", &json!({"file_path": file, "content": "lost"}), &dir, &mut anchors, &Rtk::OFF, &Arc::new(AtomicBool::new(false)));
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        let kept = std::fs::read_to_string(&file).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(is_error && text.starts_with("Cannot read"), "{text}");
        assert_eq!(kept, "keep");
    }

    #[test]
    fn search_arguments_match_pi_file_search() {
        let rg = rg_args(&json!({"pattern": "-x", "path": "@src", "-C": 50, "limit": 5000, "-i": true, "glob": "*.rs"})).unwrap();
        assert_eq!(rg.join(" "), "--json --ignore-case --context 20 --glob *.rs --max-count 1000 -- -x src");
    }
}
