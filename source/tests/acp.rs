//! `agent acp` end to end: the binary on a throwaway home, an editor on its stdin and stdout, and a fake Anthropic API.
// The test plays the editor; no TUI shares its terminal, which is what process::command guards.
#![allow(clippy::disallowed_methods)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::Duration;

use serde_json::{Value, json};

const WAIT: Duration = Duration::from_secs(30);

/// A throwaway home with a project directory, removed on drop.
struct Home {
    dir: PathBuf,
}

impl Home {
    fn new(name: &str) -> Home {
        let dir = std::env::temp_dir().join(format!("agent-acp-e2e-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("project")).unwrap();
        Home { dir: dir.canonicalize().unwrap() }
    }

    fn project(&self) -> PathBuf {
        self.dir.join("project")
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Answers the model list and every request without tools (titles, summaries) itself, and each request with tools
/// with the next of `replies`; the bodies of the requests with tools go to `requests`.
struct FakeApi {
    base: String,
    requests: Receiver<Value>,
}

fn fake_api(replies: Vec<String>) -> FakeApi {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (sent, requests) = mpsc::channel();
    std::thread::spawn(move || {
        let mut replies = replies.into_iter();
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request_line = String::new();
            reader.read_line(&mut request_line).unwrap();
            let mut length = 0;
            let mut header = String::new();
            while reader.read_line(&mut header).unwrap() > 2 {
                if let Some(value) = header.to_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap();
                }
                header.clear();
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            let (status, body) = if request_line.contains("/v1/models") {
                (200, r#"{"data":[{"id":"claude-haiku-9","max_input_tokens":200000,"max_tokens":8192}]}"#.to_string())
            } else if request_line.starts_with("GET") {
                (404, "{}".to_string())
            } else {
                let request: Value = serde_json::from_slice(&body).unwrap();
                if request["tools"].as_array().is_none_or(Vec::is_empty) {
                    (200, reply(&[text("Fake title")], "end_turn"))
                } else {
                    let _ = sent.send(request);
                    match replies.next() {
                        Some(reply) => (200, reply),
                        None => (400, r#"{"error":{"type":"invalid_request_error","message":"The fake API has no more replies."}}"#.to_string()),
                    }
                }
            };
            let _ = write!(stream, "HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
        }
    });
    FakeApi { base, requests }
}

fn text(text: &str) -> Value {
    json!({"type": "text", "text": text})
}

fn tool_use(id: &str, name: &str, input: Value) -> Value {
    json!({"type": "tool_use", "id": id, "name": name, "input": input})
}

/// One streamed model reply: text blocks arrive as deltas, tool calls whole.
fn reply(blocks: &[Value], stop_reason: &str) -> String {
    let mut events = vec![json!({"type": "message_start", "message": {"id": "msg", "usage": {"input_tokens": 100}}})];
    for (index, block) in blocks.iter().enumerate() {
        if block["type"] == "text" {
            events.push(json!({"type": "content_block_start", "index": index, "content_block": {"type": "text", "text": ""}}));
            events.push(json!({"type": "content_block_delta", "index": index, "delta": {"type": "text_delta", "text": block["text"]}}));
        } else {
            events.push(json!({"type": "content_block_start", "index": index, "content_block": block}));
        }
        events.push(json!({"type": "content_block_stop", "index": index}));
    }
    events.push(json!({"type": "message_delta", "delta": {"stop_reason": stop_reason}, "usage": {"output_tokens": 10}}));
    events.push(json!({"type": "message_stop"}));
    events.iter().map(|event| format!("data: {event}\n\n")).collect()
}

/// `agent acp` as an editor sees it. Every message the agent sends lands in `seen`; permission requests get `allow_once`.
struct Editor {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<Value>,
    next_id: u64,
    seen: Vec<Value>,
}

impl Editor {
    fn start(home: &Home, api: &FakeApi) -> Editor {
        let mut child = Command::new(env!("CARGO_BIN_EXE_agent"))
            .arg("acp")
            .current_dir(home.project())
            .env("HOME", &home.dir)
            .env("ANTHROPIC_API_KEY", "sk-fake")
            .env("ANTHROPIC_BASE_URL", &api.base)
            .env("OPENROUTER_BASE_URL", &api.base)
            .env("CHATGPT_BASE_URL", &api.base)
            .env_remove("OPENROUTER_API_KEY")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sent, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if sent.send(serde_json::from_str(&line).unwrap()).is_err() {
                    break;
                }
            }
        });
        let mut editor = Editor { child, stdin, lines, next_id: 0, seen: Vec::new() };
        editor.request("initialize", json!({"protocolVersion": 1, "clientCapabilities": {}})).unwrap();
        editor
    }

    fn send(&mut self, message: &Value) {
        writeln!(self.stdin, "{message}").unwrap();
    }

    /// The result of `method`, or its error message.
    fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        loop {
            let message = match self.lines.recv_timeout(WAIT) {
                Ok(message) => message,
                Err(RecvTimeoutError::Timeout) => panic!("no answer to {method} in {WAIT:?}; seen {:#?}", self.seen),
                Err(RecvTimeoutError::Disconnected) => panic!("agent acp exited during {method}; seen {:#?}", self.seen),
            };
            if message["id"] == id && message["method"].is_null() {
                return match message.get("error") {
                    Some(error) => Err(error["message"].as_str().unwrap().to_string()),
                    None => Ok(message["result"].clone()),
                };
            }
            if message["method"] == "session/request_permission" {
                self.send(&json!({"jsonrpc": "2.0", "id": message["id"], "result": {"outcome": {"outcome": "selected", "optionId": "allow_once"}}}));
            }
            self.seen.push(message);
        }
    }

    fn new_session(&mut self, cwd: &Path) -> String {
        let session = self.request("session/new", json!({"cwd": cwd, "mcpServers": []})).unwrap();
        session["sessionId"].as_str().unwrap().to_string()
    }

    fn prompt(&mut self, session_id: &str, text: &str) -> Result<Value, String> {
        self.request("session/prompt", json!({"sessionId": session_id, "prompt": [{"type": "text", "text": text}]}))
    }

    /// The `session/update` payloads seen so far, of kind `kind`.
    fn updates(&self, kind: &str) -> Vec<&Value> {
        self.seen.iter().filter(|message| message["method"] == "session/update").map(|message| &message["params"]["update"]).filter(|update| update["sessionUpdate"] == kind).collect()
    }
}

impl Drop for Editor {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn a_prompt_streams_the_reply_and_ends_the_turn() {
    let home = Home::new("prompt");
    let api = fake_api(vec![reply(&[text("Hello from the model.")], "end_turn")]);
    let mut editor = Editor::start(&home, &api);
    let session_id = editor.new_session(&home.project());

    let result = editor.prompt(&session_id, "Say hello.").unwrap();

    assert_eq!(result, json!({"stopReason": "end_turn"}));
    let request = api.requests.recv_timeout(WAIT).unwrap();
    let last = request["messages"].as_array().unwrap().last().unwrap();
    assert!(last["content"].as_array().unwrap().iter().any(|block| block["text"] == "Say hello."), "{last}");
    let chunks: Vec<&str> = editor.updates("agent_message_chunk").iter().map(|update| update["content"]["text"].as_str().unwrap()).collect();
    assert_eq!(chunks.concat(), "Hello from the model.");
}
