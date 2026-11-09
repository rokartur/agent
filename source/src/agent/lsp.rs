use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use crate::settings::LspServer;

/// Covers a cold start: tsc-rs answers in 0.1 s, clangd in 1.5 s, an indexing server takes longer.
const TIMEOUT: Duration = Duration::from_secs(30);
/// Some servers publish syntax errors first and type errors right after, for the same version.
const QUIET: Duration = Duration::from_millis(200);
/// New errors listed per edit; the rest become "+N more".
const MAX_LISTED_ERRORS: usize = 10;
const MAX_FRAME_BYTES: usize = 64 << 20;
/// A tsc-rs server holds 60-130 MB; one unused this long stops and the next edit starts it again.
const IDLE: Duration = Duration::from_secs(5 * 60);

/// Language servers from settings, started on the first edit of their language, one per language and root.
pub struct Lsp {
    languages: BTreeMap<String, LspServer>,
    servers: HashMap<(String, PathBuf), Server>,
    /// Languages whose server is missing, stopped or silent; skipped for the rest of the session.
    off: HashSet<String>,
    /// Error messages of each file before this session first edited it.
    baselines: HashMap<PathBuf, Vec<String>>,
}

struct Error {
    line: u64,
    column: u64,
    /// The first line only, and with no position, so an old error on a moved or edited line stays old.
    message: String,
}

impl Lsp {
    pub fn new(languages: BTreeMap<String, LspServer>) -> Lsp {
        Lsp { languages, servers: HashMap::new(), off: HashSet::new(), baselines: HashMap::new() }
    }

    pub fn stop_idle(&mut self) {
        self.servers.retain(|_, server| server.last_used.elapsed() < IDLE);
    }

    /// Records the errors `file` has before its first edit in this session. Err is a notice for the user.
    pub fn before_edit(&mut self, file: &Path, abort: &AtomicBool) -> Result<(), String> {
        if self.baselines.contains_key(file) {
            return Ok(());
        }
        let errors = if file.exists() { self.errors(file, abort)? } else { Some(Vec::new()) };
        let Some(errors) = errors else { return Ok(()) };
        self.baselines.insert(file.to_path_buf(), errors.into_iter().map(|error| error.message).collect());
        Ok(())
    }

    /// The errors `file` has now and did not have before its first edit, as text for the tool result.
    pub fn after_edit(&mut self, file: &Path, cwd: &Path, abort: &AtomicBool) -> Result<Option<String>, String> {
        if !self.baselines.contains_key(file) {
            return Ok(None);
        }
        let Some(errors) = self.errors(file, abort)? else { return Ok(None) };
        let added = new_errors(errors, &self.baselines[file]);
        if added.is_empty() {
            return Ok(None);
        }
        let shown = file.strip_prefix(cwd).unwrap_or(file).display();
        let mut lines = vec![format!("[lsp] {} new error{} in {shown}:", added.len(), if added.len() == 1 { "" } else { "s" })];
        lines.extend(added.iter().take(MAX_LISTED_ERRORS).map(|error| format!("{}:{} {}", error.line + 1, error.column + 1, error.message)));
        if added.len() > MAX_LISTED_ERRORS {
            lines.push(format!("+{} more", added.len() - MAX_LISTED_ERRORS));
        }
        Ok(Some(lines.join("\n")))
    }

    /// None when no server checks the file or Esc stopped the wait.
    fn errors(&mut self, file: &Path, abort: &AtomicBool) -> Result<Option<Vec<Error>>, String> {
        let extension = file.extension().and_then(|extension| extension.to_str()).unwrap_or_default();
        let Some((language, server)) = self.languages.iter().find(|(_, server)| !server.command.trim().is_empty() && server.extensions.iter().any(|known| known == extension)) else {
            return Ok(None);
        };
        if self.off.contains(language) {
            return Ok(None);
        }
        let (language, server) = (language.clone(), server.clone());
        let deadline = Instant::now() + TIMEOUT;
        self.stop_idle();
        // `/tmp` and `/private/tmp`, or a symlinked checkout, would otherwise start a second server on one directory.
        let file = &crate::process::canonicalize(file).map_err(|e| format!("[lsp] Cannot read {}: {e}", file.display()))?;
        let key = (language.clone(), root(file, &server.roots));
        if !self.servers.contains_key(&key) {
            match Server::start(&server.command, &key.1, deadline, abort) {
                Ok(started) => self.servers.insert(key.clone(), started),
                Err(Stop::Aborted) => return Ok(None),
                Err(stop) => return Err(self.turn_off(&key, &server.command, stop)),
            };
        }
        let text = std::fs::read_to_string(file).map_err(|e| format!("[lsp] Cannot read {}: {e}", file.display()))?;
        match self.servers.get_mut(&key).expect("started above").diagnostics(file, &text, language_id(&language, extension), deadline, abort) {
            Ok(diagnostics) => Ok(Some(errors_in(&diagnostics))),
            Err(Stop::Aborted) => Ok(None),
            Err(stop) => Err(self.turn_off(&key, &server.command, stop)),
        }
    }

    fn turn_off(&mut self, key: &(String, PathBuf), command: &str, stop: Stop) -> String {
        self.servers.remove(key);
        self.off.insert(key.0.clone());
        let why = match stop {
            Stop::NotInstalled => "is not installed".into(),
            Stop::CannotStart(e) => format!("cannot start: {e}"),
            Stop::Stopped => "stopped".into(),
            Stop::TimedOut => format!("gave no answer within {} s", TIMEOUT.as_secs()),
            Stop::Aborted => unreachable!("an aborted wait leaves the server on"),
        };
        format!("[lsp] {command} {why}, no {} diagnostics in this session.", key.0)
    }
}

/// The nearest directory above `file` with one of `markers` or `.git`, else the file's own directory.
fn root(file: &Path, markers: &[String]) -> PathBuf {
    let parent = file.parent().expect("an edited file has a directory");
    for dir in parent.ancestors() {
        if markers.iter().any(|marker| dir.join(marker).exists()) || dir.join(".git").exists() {
            return dir.to_path_buf();
        }
    }
    parent.to_path_buf()
}

/// TypeScript and clangd tell JSX and C apart from TSX and C++ by the language id, not the extension.
fn language_id<'a>(language: &'a str, extension: &str) -> &'a str {
    match extension {
        "tsx" => "typescriptreact",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "javascriptreact",
        "c" | "h" => "c",
        _ => language,
    }
}

fn errors_in(diagnostics: &Value) -> Vec<Error> {
    let mut errors = Vec::new();
    for diagnostic in diagnostics.as_array().into_iter().flatten() {
        // LSP leaves a missing severity to the client; treat it as an error.
        if diagnostic["severity"].as_u64().is_some_and(|severity| severity != 1) {
            continue;
        }
        let start = &diagnostic["range"]["start"];
        let (Some(line), Some(column)) = (start["line"].as_u64(), start["character"].as_u64()) else { continue };
        let message = diagnostic["message"].as_str().unwrap_or_default().lines().next().unwrap_or_default().to_string();
        errors.push(Error { line, column, message });
    }
    errors
}

/// `errors` minus one match per baseline message, so a second copy of an old error still counts as new.
fn new_errors(errors: Vec<Error>, baseline: &[String]) -> Vec<Error> {
    let mut left = baseline.to_vec();
    let mut added = Vec::new();
    for error in errors {
        match left.iter().position(|message| *message == error.message) {
            Some(index) => {
                left.swap_remove(index);
            }
            None => added.push(error),
        }
    }
    added
}

enum Stop {
    NotInstalled,
    CannotStart(std::io::Error),
    Stopped,
    TimedOut,
    Aborted,
}

struct Server {
    child: Child,
    /// Messages for the writer thread, so a server that stops reading cannot block past the deadline or Esc.
    outbox: Sender<Value>,
    /// Responses and notifications; a reader thread answers the server's own requests.
    inbox: Receiver<Value>,
    next_id: u64,
    /// The server answers `textDocument/diagnostic`; otherwise it publishes diagnostics on its own.
    pull: bool,
    versions: HashMap<PathBuf, u64>,
    last_used: Instant,
}

impl Server {
    // shortcut: the command splits on whitespace, so no quoted arguments; run it through `sh -c` if a server needs them.
    fn start(command: &str, root: &Path, deadline: Instant, abort: &AtomicBool) -> Result<Server, Stop> {
        let mut words = command.split_whitespace();
        let program = words.next().expect("languages with an empty command are skipped");
        let mut child = match crate::process::command(program).args(words).current_dir(root).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn() {
            Ok(child) => child,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(Stop::NotInstalled),
            Err(e) => return Err(Stop::CannotStart(e)),
        };
        let outbox = writer(child.stdin.take().expect("stdin is piped"));
        let stdout = child.stdout.take().expect("stdout is piped");
        let (sender, inbox) = mpsc::channel();
        let replies = outbox.clone();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            while let Some(message) = read_frame(&mut reader) {
                if let (Some(id), Some(method)) = (message.get("id"), message["method"].as_str()) {
                    // workspace/configuration wants one value per item; null means "use your defaults".
                    let items = message["params"]["items"].as_array().filter(|_| method == "workspace/configuration");
                    let result = items.map_or(Value::Null, |items| Value::Array(vec![Value::Null; items.len()]));
                    let _ = replies.send(json!({"id": id, "result": result}));
                } else if sender.send(message).is_err() {
                    return;
                }
            }
        });
        let mut server = Server { child, outbox, inbox, next_id: 1, pull: false, versions: HashMap::new(), last_used: Instant::now() };
        let uri = file_uri(root);
        let name = root.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
        let capabilities = json!({"textDocument": {"publishDiagnostics": {"versionSupport": true}, "diagnostic": {"dynamicRegistration": false}}});
        let params = json!({"processId": std::process::id(), "rootUri": uri, "workspaceFolders": [{"uri": uri, "name": name}], "capabilities": capabilities});
        let reply = server.request("initialize", params, deadline, abort)?;
        server.notify("initialized", json!({}))?;
        server.pull = !reply["result"]["capabilities"]["diagnosticProvider"].is_null();
        Ok(server)
    }

    /// Sends the whole text as a new version, then returns the diagnostics for that version.
    fn diagnostics(&mut self, file: &Path, text: &str, language_id: &str, deadline: Instant, abort: &AtomicBool) -> Result<Value, Stop> {
        let uri = file_uri(file);
        let version = self.versions.get(file).map_or(1, |version| version + 1);
        // Diagnostics queued for an earlier version carry no version on some servers and would pass as this one's.
        while self.inbox.try_recv().is_ok() {}
        if version == 1 {
            self.notify("textDocument/didOpen", json!({"textDocument": {"uri": uri, "languageId": language_id, "version": version, "text": text}}))?;
        } else {
            self.notify("textDocument/didChange", json!({"textDocument": {"uri": uri, "version": version}, "contentChanges": [{"text": text}]}))?;
        }
        self.versions.insert(file.to_path_buf(), version);
        self.last_used = Instant::now();
        if self.pull {
            let reply = self.request("textDocument/diagnostic", json!({"textDocument": {"uri": uri}}), deadline, abort)?;
            // An error reply has no items; returning Null would read as "no errors".
            return reply["result"]["items"].is_array().then(|| reply["result"]["items"].clone()).ok_or(Stop::Stopped);
        }
        let own = |message: &Value| {
            let params = &message["params"];
            message["method"] == "textDocument/publishDiagnostics"
                && params["uri"].as_str().is_some_and(|published| same_document(published, &uri))
                && (params["version"].is_null() || params["version"] == version)
        };
        let mut published = self.wait_for(deadline, abort, own)?;
        loop {
            match self.wait_for(Instant::now() + QUIET, abort, own) {
                Ok(newer) => published = newer,
                Err(Stop::TimedOut) => return Ok(published["params"]["diagnostics"].clone()),
                Err(stop) => return Err(stop),
            }
        }
    }

    fn request(&mut self, method: &str, params: Value, deadline: Instant, abort: &AtomicBool) -> Result<Value, Stop> {
        let id = self.next_id;
        self.next_id += 1;
        self.outbox.send(json!({"id": id, "method": method, "params": params})).map_err(|_| Stop::Stopped)?;
        self.wait_for(deadline, abort, |message| message["id"] == id && message.get("method").is_none())
    }

    fn notify(&self, method: &str, params: Value) -> Result<(), Stop> {
        self.outbox.send(json!({"method": method, "params": params})).map_err(|_| Stop::Stopped)
    }

    /// The first message that `wanted` accepts; checks Esc every 50 ms.
    fn wait_for(&self, until: Instant, abort: &AtomicBool, wanted: impl Fn(&Value) -> bool) -> Result<Value, Stop> {
        loop {
            if abort.load(Ordering::Relaxed) {
                return Err(Stop::Aborted);
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(Stop::TimedOut);
            }
            match self.inbox.recv_timeout(left.min(Duration::from_millis(50))) {
                Ok(message) if wanted(&message) => return Ok(message),
                Ok(_) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Err(Stop::Stopped),
            }
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        crate::process::signal_group(self.child.id(), crate::process::SIGKILL);
        let _ = self.child.wait();
    }
}

/// Writes each queued message to the server; ends when the server closes its stdin or every sender is gone.
fn writer(mut stdin: ChildStdin) -> Sender<Value> {
    let (outbox, queued) = mpsc::channel::<Value>();
    std::thread::spawn(move || {
        for mut message in queued {
            message["jsonrpc"] = "2.0".into();
            let body = message.to_string();
            if write!(stdin, "Content-Length: {}\r\n\r\n{body}", body.len()).and_then(|()| stdin.flush()).is_err() {
                return;
            }
        }
    });
    outbox
}

/// One `Content-Length` framed JSON message; None when the stream ends or breaks the framing.
fn read_frame(reader: &mut impl BufRead) -> Option<Value> {
    let mut length = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().ok();
        }
    }
    // A broken or hostile server could announce terabytes; the allocation would abort the agent.
    let mut body = vec![0; length.filter(|length| *length <= MAX_FRAME_BYTES)?];
    reader.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

/// RFC 3986 path: every byte except unreserved characters and `/` is percent-encoded. A Windows path becomes
/// `file:///C:/dir/file`, with the drive's `:` kept as RFC 8089 writes it.
fn file_uri(path: &Path) -> String {
    use std::fmt::Write as _;
    let mut uri = String::from("file://");
    let path = path.to_string_lossy();
    #[cfg(windows)]
    let path = path.replace('\\', "/");
    let mut rest = path.as_bytes();
    if cfg!(windows)
        && let [drive, b':', tail @ ..] = rest
        && drive.is_ascii_alphabetic()
    {
        write!(uri, "/{}:", *drive as char).expect("writing to a String cannot fail");
        rest = tail;
    }
    for &byte in rest {
        if byte.is_ascii_alphanumeric() || b"/-._~".contains(&byte) {
            uri.push(byte as char);
        } else {
            write!(uri, "%{byte:02X}").expect("writing to a String cannot fail");
        }
    }
    uri
}

/// Whether a server's `published` URI names the document sent as `sent`. Windows servers may lowercase the drive
/// and encode its `:` (`file:///c%3A/dir/file`), and Windows paths ignore case.
fn same_document(published: &str, sent: &str) -> bool {
    if cfg!(windows) {
        return published.replace("%3A", ":").replace("%3a", ":").eq_ignore_ascii_case(sent);
    }
    published == sent
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_split_a_stream_into_messages() {
        let first = r#"{"id":1,"result":"é"}"#;
        let stream = format!("Content-Length: {}\r\nContent-Type: x\r\n\r\n{first}content-length: 2\r\n\r\n{{}}Content-Length: 9\r\n\r\n{{}}", first.len());
        let mut reader = stream.as_bytes();
        assert_eq!(read_frame(&mut reader), Some(json!({"id": 1, "result": "é"})));
        assert_eq!(read_frame(&mut reader), Some(json!({})));
        assert_eq!(read_frame(&mut reader), None);
        assert_eq!(read_frame(&mut "Content-Length: 100000000000\r\n\r\n{}".as_bytes()), None);
    }

    #[test]
    fn only_errors_missing_from_the_baseline_are_new() {
        let error = |line: u64, message: &str| json!({"range": {"start": {"line": line, "character": 8}}, "severity": 1, "message": message});
        let before = json!([error(0, "old"), error(1, "unknown c")]);
        let baseline: Vec<String> = errors_in(&before).into_iter().map(|error| error.message).collect();
        let warning = json!({"range": {"start": {"line": 0, "character": 0}}, "severity": 2, "message": "unused"});
        let after = json!([error(4, "old"), error(1, "unknown c"), error(2, "unknown c"), warning, error(0, "new\nsecond line")]);
        let added: Vec<(u64, String)> = new_errors(errors_in(&after), &baseline).into_iter().map(|error| (error.line, error.message)).collect();
        assert_eq!(added, [(2, "unknown c".to_string()), (0, "new".to_string())]);
    }

    #[test]
    fn uris_encode_everything_but_path_characters() {
        assert_eq!(file_uri(Path::new("/tmp/a b/zażółć.ts")), "file:///tmp/a%20b/za%C5%BC%C3%B3%C5%82%C4%87.ts");
    }

    #[test]
    #[cfg(windows)]
    fn windows_uris_put_the_drive_in_the_path() {
        let uri = file_uri(Path::new(r"C:\proj\a b.rs"));
        assert_eq!(uri, "file:///C:/proj/a%20b.rs");
        assert!(same_document("file:///c%3A/proj/a%20b.rs", &uri));
        assert!(!same_document("file:///c%3A/proj/other.rs", &uri));
    }

    #[test]
    fn servers_unused_for_the_idle_time_stop() {
        let server = |last_used: Instant| {
            let mut child = crate::process::command("sleep").arg("60").stdin(Stdio::piped()).spawn().unwrap();
            let outbox = writer(child.stdin.take().unwrap());
            Server { child, outbox, inbox: mpsc::channel().1, next_id: 1, pull: false, versions: HashMap::new(), last_used }
        };
        let mut lsp = Lsp::new(BTreeMap::new());
        lsp.servers.insert(("typescript".into(), "/old".into()), server(Instant::now() - IDLE - Duration::from_secs(1)));
        lsp.servers.insert(("typescript".into(), "/recent".into()), server(Instant::now()));
        lsp.stop_idle();
        assert_eq!(lsp.servers.into_keys().collect::<Vec<_>>(), [("typescript".to_string(), PathBuf::from("/recent"))]);
    }
}
