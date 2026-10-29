//! `agent acp` end to end: the binary on a throwaway home, an editor on its stdin and stdout, and a fake Anthropic API.
// The test plays the editor; no TUI shares its terminal, which is what process::command guards.
#![allow(clippy::disallowed_methods)]

use std::collections::{HashMap, VecDeque};
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
                (200, r#"{"data":[{"id":"claude-haiku-9","max_input_tokens":200000,"max_tokens":8192},{"id":"claude-sonnet-9","max_input_tokens":200000,"max_tokens":8192}]}"#.to_string())
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

/// `agent acp` as an editor sees it. Every message the agent sends lands in `seen`; permission requests get `permission_answer`,
/// and forms the next of `form_replies`, or a decline.
struct Editor {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<Value>,
    next_id: u64,
    seen: Vec<Value>,
    form_replies: VecDeque<Value>,
    /// Unsaved buffers by path, for a client that offers fs: reads come from here before the disk, writes land here.
    buffers: HashMap<String, String>,
    /// The output and exit code of each command a terminal/create ran, by terminal id.
    terminals: HashMap<String, (String, i32)>,
    /// The directories of the sessions opened, the only ones whose files the fs methods reach, as in Zed.
    worktrees: Vec<PathBuf>,
    /// Terminal methods the fake terminal answers with an error, and those it never answers.
    terminal_errors: Vec<&'static str>,
    terminal_silent: Vec<&'static str>,
    /// The option every permission request gets.
    permission_answer: &'static str,
}

impl Editor {
    fn start(home: &Home, api: &FakeApi) -> Editor {
        Editor::start_with(home, api, json!({}), "sk-fake")
    }

    /// An empty `api_key` starts it with no account.
    fn start_with(home: &Home, api: &FakeApi, capabilities: Value, api_key: &str) -> Editor {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agent"));
        if api_key.is_empty() {
            command.env_remove("ANTHROPIC_API_KEY");
        } else {
            command.env("ANTHROPIC_API_KEY", api_key);
        }
        let mut child = command
            .arg("acp")
            .current_dir(home.project())
            .env("HOME", &home.dir)
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
        let mut editor = Editor {
            child,
            stdin,
            lines,
            next_id: 0,
            seen: Vec::new(),
            form_replies: VecDeque::new(),
            buffers: HashMap::new(),
            terminals: HashMap::new(),
            worktrees: Vec::new(),
            terminal_errors: Vec::new(),
            terminal_silent: Vec::new(),
            permission_answer: "allow_once",
        };
        editor.request("initialize", json!({"protocolVersion": 1, "clientCapabilities": capabilities})).unwrap();
        editor
    }

    fn send(&mut self, message: &Value) {
        writeln!(self.stdin, "{message}").unwrap();
    }

    /// The result of `method`, or its error message.
    fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.start_request(method, params);
        let answer = self.answer(id, method);
        match answer.get("error") {
            Some(error) => Err(error["message"].as_str().unwrap().to_string()),
            None => Ok(answer["result"].clone()),
        }
    }

    fn start_request(&mut self, method: &str, params: Value) -> u64 {
        self.next_id += 1;
        self.send(&json!({"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params}));
        self.next_id
    }

    /// The answer to request `id`, answering the agent's own requests while it waits.
    fn answer(&mut self, id: u64, method: &str) -> Value {
        self.until(&format!("answer to {method}"), |message| message["id"] == id && message["method"].is_null())
    }

    /// The first message `wanted` takes, answering the agent's own requests while it waits.
    fn until(&mut self, what: &str, wanted: impl Fn(&Value) -> bool) -> Value {
        loop {
            let message = match self.lines.recv_timeout(WAIT) {
                Ok(message) => message,
                Err(RecvTimeoutError::Timeout) => panic!("no {what} in {WAIT:?}; seen {:#?}", self.seen),
                Err(RecvTimeoutError::Disconnected) => panic!("agent acp exited before {what}; seen {:#?}", self.seen),
            };
            if wanted(&message) {
                return message;
            }
            if message["method"] == "session/request_permission" {
                self.send(&json!({"jsonrpc": "2.0", "id": message["id"], "result": {"outcome": {"outcome": "selected", "optionId": self.permission_answer}}}));
            }
            let method = message["method"].as_str().unwrap_or_default();
            let outside = method.starts_with("fs/") && !self.worktrees.iter().any(|worktree| Path::new(message["params"]["path"].as_str().unwrap()).starts_with(worktree));
            if outside {
                self.send(&json!({"jsonrpc": "2.0", "id": message["id"], "error": {"code": -32002, "message": "Resource not found"}}));
            }
            if method == "fs/read_text_file" && !outside {
                let path = message["params"]["path"].as_str().unwrap();
                let content = self.buffers.get(path).cloned().unwrap_or_else(|| std::fs::read_to_string(path).unwrap());
                self.send(&json!({"jsonrpc": "2.0", "id": message["id"], "result": {"content": content}}));
            }
            if method == "fs/write_text_file" && !outside {
                let params = &message["params"];
                self.buffers.insert(params["path"].as_str().unwrap().to_string(), params["content"].as_str().unwrap().to_string());
                self.send(&json!({"jsonrpc": "2.0", "id": message["id"], "result": null}));
            }
            if self.terminal_errors.contains(&method) {
                self.send(&json!({"jsonrpc": "2.0", "id": message["id"], "error": {"code": -32603, "message": "The terminal is gone."}}));
            } else if !self.terminal_silent.contains(&method)
                && let Some(answer) = self.answer_terminal(&message)
            {
                self.send(&json!({"jsonrpc": "2.0", "id": message["id"], "result": answer}));
            }
            if message["method"] == "elicitation/create" {
                let result = self.form_replies.pop_front().unwrap_or(json!({"action": "decline"}));
                self.send(&json!({"jsonrpc": "2.0", "id": message["id"], "result": result}));
            }
            self.seen.push(message);
        }
    }

    /// A terminal that runs its command to the end at terminal/create.
    fn answer_terminal(&mut self, message: &Value) -> Option<Value> {
        let params = &message["params"];
        let terminal_id = params["terminalId"].as_str().unwrap_or_default().to_string();
        Some(match message["method"].as_str()? {
            "terminal/create" => {
                let args: Vec<&str> = params["args"].as_array().unwrap().iter().map(|arg| arg.as_str().unwrap()).collect();
                let ran = Command::new(params["command"].as_str().unwrap()).args(args).current_dir(params["cwd"].as_str().unwrap()).output().unwrap();
                let id = format!("term-{}", self.terminals.len());
                self.terminals.insert(id.clone(), (String::from_utf8_lossy(&ran.stdout).into_owned(), ran.status.code().unwrap()));
                json!({"terminalId": id})
            }
            "terminal/wait_for_exit" => json!({"exitCode": self.terminals[&terminal_id].1}),
            "terminal/output" => json!({"output": self.terminals[&terminal_id].0, "truncated": false}),
            "terminal/release" | "terminal/kill" => json!({}),
            _ => return None,
        })
    }

    fn new_session(&mut self, cwd: &Path) -> String {
        self.worktrees.push(cwd.to_path_buf());
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

#[test]
fn the_session_id_still_works_after_the_model_moves_the_session_with_cd() {
    let home = Home::new("cd");
    std::fs::create_dir(home.project().join("sub")).unwrap();
    let api = fake_api(vec![reply(&[tool_use("toolu_cd", "Bash", json!({"command": "cd sub"}))], "tool_use"), reply(&[text("Moved.")], "end_turn"), reply(&[text("Still here.")], "end_turn")]);
    let mut editor = Editor::start(&home, &api);
    let session_id = editor.new_session(&home.project());

    editor.prompt(&session_id, "Go to sub.").unwrap();
    let again = editor.prompt(&session_id, "And now?");

    assert_eq!(again, Ok(json!({"stopReason": "end_turn"})));
    let ended = editor.updates("tool_call_update").into_iter().find(|update| update.get("content").is_some()).unwrap();
    assert!(ended["content"][0]["content"]["text"].as_str().unwrap().contains("Session moved to"), "{ended}");
}

#[test]
fn a_permission_request_names_the_call_and_shows_its_input() {
    let home = Home::new("permission");
    let api = fake_api(vec![reply(&[tool_use("toolu_touch", "Bash", json!({"command": "touch made"}))], "tool_use"), reply(&[text("Done.")], "end_turn")]);
    let mut editor = Editor::start(&home, &api);
    let session_id = editor.new_session(&home.project());

    editor.prompt(&session_id, "Make a file.").unwrap();

    let asked = editor.seen.iter().find(|message| message["method"] == "session/request_permission").expect("a permission request");
    let tool_call = &asked["params"]["toolCall"];
    assert_eq!((&tool_call["toolCallId"], &tool_call["kind"]), (&json!("toolu_touch"), &json!("execute")));
    assert_eq!(tool_call["rawInput"], json!({"command": "touch made"}));
    assert!(home.project().join("made").exists());
    // Zed keeps the title a permission request gives the card, so the answer gives the call its own title back.
    let started = editor.updates("tool_call").into_iter().find(|update| update["toolCallId"] == "toolu_touch").unwrap();
    let retitled = editor.updates("tool_call_update").into_iter().rfind(|update| update["toolCallId"] == "toolu_touch" && update.get("title").is_some());
    assert_eq!(tool_call["title"], "Would you like to run the following command? touch made");
    assert_eq!(retitled.map(|update| &update["title"]), Some(&started["title"]));
}

#[test]
fn an_image_in_the_prompt_reaches_the_model_next_to_the_text() {
    const PIXEL_PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";
    let home = Home::new("image");
    let api = fake_api(vec![reply(&[text("A pixel.")], "end_turn")]);
    let mut editor = Editor::start(&home, &api);
    let session_id = editor.new_session(&home.project());

    let prompt = json!([{"type": "image", "data": PIXEL_PNG, "mimeType": "image/png"}, {"type": "text", "text": "What is this?"}]);
    editor.request("session/prompt", json!({"sessionId": session_id, "prompt": prompt})).unwrap();
    let broken = editor.request("session/prompt", json!({"sessionId": session_id, "prompt": [{"type": "image", "data": "bm90IGFuIGltYWdl", "mimeType": "image/png"}]}));

    let request = api.requests.recv_timeout(WAIT).unwrap();
    let content = &request["messages"].as_array().unwrap().last().unwrap()["content"];
    let kinds: Vec<&str> = content.as_array().unwrap().iter().map(|block| block["type"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["image", "text"], "{content}");
    assert_eq!(content[1]["text"], "What is this?");
    assert!(broken.is_err(), "{broken:?}");
}

#[test]
fn skills_are_slash_commands_that_reach_the_model_expanded() {
    let home = Home::new("skill");
    let api = fake_api(vec![reply(&[text("Cześć, Bob.")], "end_turn")]);
    let mut editor = Editor::start(&home, &api);
    let session_id = editor.new_session(&home.project());
    // Added after the session opened, as a profile turned on in the TUI adds it.
    let skill = home.dir.join(".agents/skills/greet");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(skill.join("SKILL.md"), "---\nname: greet\ndescription: Greets someone.\n---\nSay hello in Polish.\n").unwrap();

    editor.prompt(&session_id, "/skill:greet Bob").unwrap();

    let offered = editor.updates("available_commands_update");
    let names: Vec<&str> = offered.last().unwrap()["availableCommands"].as_array().unwrap().iter().map(|command| command["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"skill:greet") && names.contains(&"deep-research"), "{names:?}");
    let request = api.requests.recv_timeout(WAIT).unwrap();
    let sent = request["messages"].as_array().unwrap().last().unwrap()["content"].to_string();
    assert!(sent.contains("<skill name=\\\"greet\\\"") && sent.contains("Say hello in Polish.") && sent.contains("Bob"), "{sent}");
}

#[test]
fn the_editor_sets_the_permission_mode_model_and_effort_of_the_next_prompt() {
    let home = Home::new("config");
    let api = fake_api(vec![reply(&[tool_use("toolu_touch", "Bash", json!({"command": "touch made"}))], "tool_use"), reply(&[text("Done.")], "end_turn")]);
    let mut editor = Editor::start(&home, &api);
    let session = editor.request("session/new", json!({"cwd": home.project(), "mcpServers": []})).unwrap();
    let session_id = session["sessionId"].as_str().unwrap();
    let mode_ids: Vec<&str> = session["modes"]["availableModes"].as_array().unwrap().iter().map(|mode| mode["id"].as_str().unwrap()).collect();
    assert_eq!(mode_ids, ["ask", "auto", "full-access"]);
    let set = |editor: &mut Editor, config_id: &str, value: &str| editor.request("session/set_config_option", json!({"sessionId": session_id, "configId": config_id, "value": value}));

    assert!(editor.request("session/set_mode", json!({"sessionId": session_id, "modeId": "plan"})).is_err());
    editor.request("session/set_mode", json!({"sessionId": session_id, "modeId": "ask"})).unwrap();
    // Zed ignores modes once a session has config options, so the mode is one of them too.
    assert!(set(&mut editor, "mode", "plan").is_err());
    set(&mut editor, "mode", "full-access").unwrap();
    assert!(set(&mut editor, "model", "gpt-unknown").is_err());
    assert!(set(&mut editor, "effort", "extreme").is_err());
    assert!(set(&mut editor, "speed", "fast").is_err());
    set(&mut editor, "model", "claude-sonnet-9").unwrap();
    let options = set(&mut editor, "effort", "low").unwrap()["configOptions"].clone();
    editor.prompt(session_id, "Make a file.").unwrap();

    let current: Vec<&str> = options.as_array().unwrap().iter().map(|option| option["currentValue"].as_str().unwrap()).collect();
    assert_eq!(current, ["full-access", "claude-sonnet-9", "low"]);
    assert_eq!(api.requests.recv_timeout(WAIT).unwrap()["model"], "claude-sonnet-9");
    assert!(!editor.seen.iter().any(|message| message["method"] == "session/request_permission"), "full access asks nothing");
    assert!(home.project().join("made").exists());
}

#[test]
fn two_sessions_run_side_by_side_each_in_its_own_directory() {
    let home = Home::new("sessions");
    let other = home.dir.join("other");
    std::fs::create_dir(&other).unwrap();
    let api = fake_api(vec![
        // The same tool call id in both sessions, as llama.cpp gives every first call.
        reply(&[tool_use("call_0", "Bash", json!({"command": "touch b"}))], "tool_use"),
        reply(&[text("Made b.")], "end_turn"),
        reply(&[tool_use("call_0", "Bash", json!({"command": "touch a"}))], "tool_use"),
        reply(&[text("Made a.")], "end_turn"),
    ]);
    let mut editor = Editor::start(&home, &api);
    let first = editor.new_session(&home.project());
    let second = editor.new_session(&other);

    editor.prompt(&second, "Make b.").unwrap();
    editor.prompt(&first, "Make a.").unwrap();

    assert_ne!(first, second);
    assert!(other.join("b").exists() && home.project().join("a").exists());
    assert!(!home.project().join("b").exists() && !other.join("a").exists());
    // The router keys the editor's replies by id across every session's process.
    let asked: Vec<&Value> = editor.seen.iter().filter(|message| message["method"] == "session/request_permission").map(|message| &message["id"]).collect();
    assert!(asked.len() == 2 && asked[0] != asked[1], "{asked:?}");
    assert!(editor.prompt("no-such-session", "Hi.").unwrap_err().contains("Unknown session"));
    assert!(editor.request("session/new", json!({"cwd": "relative/dir", "mcpServers": []})).is_err());
    assert!(editor.request("session/new", json!({"cwd": home.project(), "mcpServers": [], "additionalDirectories": [other]})).is_ok());
    assert!(editor.request("session/new", json!({"cwd": home.project(), "mcpServers": [], "additionalDirectories": [home.dir.join("missing")]})).is_err());
}

#[test]
fn a_closed_session_is_listed_and_loads_with_its_history_replayed() {
    let home = Home::new("load");
    let api = fake_api(vec![reply(&[text("Let me look."), tool_use("toolu_ls", "Bash", json!({"command": "ls"}))], "tool_use"), reply(&[text("Nothing here.")], "end_turn")]);
    let mut editor = Editor::start(&home, &api);
    let session_id = editor.new_session(&home.project());
    editor.prompt(&session_id, "What is here?").unwrap();
    editor.request("session/close", json!({"sessionId": session_id})).unwrap();
    assert!(editor.prompt(&session_id, "Still there?").unwrap_err().contains("Unknown session"));

    let listed = editor.request("session/list", json!({"cwd": home.project()})).unwrap();
    editor.seen.clear();
    let loaded = editor.request("session/load", json!({"sessionId": session_id, "cwd": home.project(), "mcpServers": []})).unwrap();

    assert_eq!(listed["sessions"][0]["sessionId"], json!(session_id), "{listed}");
    assert!(listed["sessions"][0]["updatedAt"].as_str().unwrap().ends_with('Z'));
    assert!(loaded["modes"]["currentModeId"].is_string(), "{loaded}");
    let replayed: Vec<String> = editor
        .seen
        .iter()
        .map(|message| &message["params"]["update"])
        .filter(|update| update["sessionUpdate"] != "available_commands_update")
        .map(|update| format!("{} {}", update["sessionUpdate"].as_str().unwrap(), update["content"]["text"].as_str().or(update["toolCallId"].as_str()).unwrap_or_default()))
        .collect();
    assert_eq!(replayed, ["user_message_chunk What is here?", "agent_message_chunk Let me look.", "tool_call toolu_ls", "tool_call_update toolu_ls", "agent_message_chunk Nothing here."]);
    assert!(editor.seen.iter().all(|message| message["params"]["sessionId"] == json!(session_id)));
}

#[test]
fn a_session_loaded_closed_or_open_keeps_the_editors_directories_and_makes_no_copy() {
    let home = Home::new("reload");
    let other = home.dir.join("other");
    std::fs::create_dir(&other).unwrap();
    let api = fake_api(vec![reply(&[text("One.")], "end_turn"), reply(&[text("Two.")], "end_turn"), reply(&[text("Three.")], "end_turn")]);
    let mut editor = Editor::start(&home, &api);
    let session_id = editor.new_session(&home.project());
    editor.prompt(&session_id, "Hi.").unwrap();
    let load = json!({"sessionId": session_id, "cwd": home.project(), "mcpServers": [], "additionalDirectories": [other]});

    for close_first in [true, false] {
        if close_first {
            editor.request("session/close", json!({"sessionId": session_id})).unwrap();
        }
        editor.request("session/load", load.clone()).unwrap();
        editor.prompt(&session_id, "Again.").unwrap();
        let system = api.requests.try_iter().last().unwrap()["system"].to_string();
        assert!(system.contains(&other.display().to_string()), "closed first: {close_first}; {system}");
    }
    let listed = editor.request("session/list", json!({"cwd": home.project()})).unwrap();
    assert_eq!(listed["sessions"].as_array().unwrap().len(), 1, "{listed}");
}

#[test]
fn a_session_open_in_another_process_does_not_load_into_a_copy() {
    let home = Home::new("held");
    let api = fake_api(vec![reply(&[text("One.")], "end_turn")]);
    let mut editor = Editor::start(&home, &api);
    let session_id = editor.new_session(&home.project());
    editor.prompt(&session_id, "Hi.").unwrap();
    editor.request("session/close", json!({"sessionId": session_id})).unwrap();
    // As the TUI holds a session it has open.
    let held = std::fs::File::open(home.dir.join(format!(".agents/sessions/{session_id}.jsonl"))).unwrap();
    held.lock().unwrap();

    let error = editor.request("session/load", json!({"sessionId": session_id, "cwd": home.project(), "mcpServers": []})).unwrap_err();

    assert!(error.contains("open in another process"), "{error}");
    let listed = editor.request("session/list", json!({"cwd": home.project()})).unwrap();
    assert_eq!(listed["sessions"].as_array().unwrap().len(), 1, "{listed}");
}

#[test]
fn a_cancel_right_behind_the_prompt_still_cancels_it() {
    let home = Home::new("cancel-early");
    let api = fake_api(vec![reply(&[tool_use("toolu_touch", "Bash", json!({"command": "touch made"}))], "tool_use"), reply(&[text("Done.")], "end_turn")]);
    let mut editor = Editor::start(&home, &api);
    let session_id = editor.new_session(&home.project());

    let prompt = editor.start_request("session/prompt", json!({"sessionId": session_id, "prompt": [{"type": "text", "text": "Make a file."}]}));
    editor.send(&json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": session_id}}));

    assert_eq!(editor.answer(prompt, "session/prompt")["result"], json!({"stopReason": "cancelled"}));
    assert!(!home.project().join("made").exists());
}

#[test]
fn a_mode_change_during_a_turn_applies_to_its_next_tool_call() {
    let home = Home::new("mode-mid-turn");
    let api = fake_api(vec![
        reply(&[tool_use("toolu_sleep", "Bash", json!({"command": "sleep 2"}))], "tool_use"),
        reply(&[tool_use("toolu_touch", "Bash", json!({"command": "touch made"}))], "tool_use"),
        reply(&[text("Done.")], "end_turn"),
    ]);
    let mut editor = Editor::start(&home, &api);
    let session_id = editor.new_session(&home.project());
    editor.request("session/set_mode", json!({"sessionId": session_id, "modeId": "full-access"})).unwrap();
    let prompt = editor.start_request("session/prompt", json!({"sessionId": session_id, "prompt": [{"type": "text", "text": "Wait, then make a file."}]}));
    editor.until("the sleep's tool call", |message| message["params"]["update"]["sessionUpdate"] == "tool_call");

    let ask = editor.start_request("session/set_mode", json!({"sessionId": session_id, "modeId": "ask"}));

    editor.answer(prompt, "session/prompt");
    editor.answer(ask, "session/set_mode");
    let asked: Vec<&Value> = editor.seen.iter().filter(|message| message["method"] == "session/request_permission").map(|message| &message["params"]["toolCall"]["toolCallId"]).collect();
    assert_eq!(asked, [&json!("toolu_touch")]);
}

#[test]
fn closing_a_session_cancels_its_running_turn() {
    let home = Home::new("close-running");
    let api = fake_api(vec![reply(&[tool_use("toolu_sleep", "Bash", json!({"command": "sleep 120"}))], "tool_use")]);
    let mut editor = Editor::start(&home, &api);
    let session_id = editor.new_session(&home.project());
    let prompt = editor.start_request("session/prompt", json!({"sessionId": session_id, "prompt": [{"type": "text", "text": "Wait."}]}));
    editor.until("the sleep's tool call", |message| message["params"]["update"]["sessionUpdate"] == "tool_call");

    let close = editor.start_request("session/close", json!({"sessionId": session_id}));

    assert_eq!(editor.answer(prompt, "session/prompt")["result"], json!({"stopReason": "cancelled"}));
    assert_eq!(editor.answer(close, "session/close")["result"], json!({}));
}

#[test]
fn a_prompt_gets_an_error_when_its_session_process_dies() {
    let home = Home::new("dies");
    let api = fake_api(vec![reply(&[tool_use("toolu_kill", "Bash", json!({"command": "kill -9 $PPID"}))], "tool_use")]);
    let mut editor = Editor::start(&home, &api);
    let session_id = editor.new_session(&home.project());

    let error = editor.prompt(&session_id, "Stop.").unwrap_err();

    assert_eq!(error, "The session's process exited.");
}
#[test]
fn a_session_gets_a_title_from_its_first_prompt() {
    let home = Home::new("title");
    let api = fake_api(vec![reply(&[text("One.")], "end_turn"), reply(&[text("Two.")], "end_turn")]);
    let mut editor = Editor::start(&home, &api);
    let session_id = editor.new_session(&home.project());

    // The title arrives at the end of the first prompt the naming request finished during.
    editor.prompt(&session_id, "Count.").unwrap();
    editor.prompt(&session_id, "Again.").unwrap();

    let titles: Vec<&str> = editor.updates("session_info_update").iter().map(|update| update["title"].as_str().unwrap()).collect();
    assert_eq!(titles, ["project｜Fake title"]);
}

fn offered_tools(request: &Value) -> Vec<&str> {
    request["tools"].as_array().unwrap().iter().filter_map(|tool| tool["name"].as_str()).collect()
}

#[test]
fn an_editor_with_forms_answers_ask_user_question() {
    let home = Home::new("ask");
    let question = json!({"question": "Which color?", "header": "Color", "options": [{"label": "Red", "description": "warm"}, {"label": "Blue", "description": "cold"}]});
    let api = fake_api(vec![
        reply(&[tool_use("toolu_ask", "AskUserQuestion", json!({"questions": [question]}))], "tool_use"),
        reply(&[text("Blue it is.")], "end_turn"),
        reply(&[text("No forms.")], "end_turn"),
    ]);
    let mut editor = Editor::start_with(&home, &api, json!({"elicitation": {"form": {}}}), "sk-fake");
    editor.form_replies.push_back(json!({"action": "accept", "content": {"q0": "Blue"}}));
    let session_id = editor.new_session(&home.project());
    editor.prompt(&session_id, "Pick a color.").unwrap();
    let mut without_forms = Editor::start(&home, &api);
    let other = without_forms.new_session(&home.project());
    without_forms.prompt(&other, "Pick a color.").unwrap();

    let form = editor.seen.iter().find(|message| message["method"] == "elicitation/create").expect("a form");
    assert_eq!(form["params"]["requestedSchema"]["properties"]["q0"]["oneOf"][1]["const"], "Blue");
    let asked = api.requests.recv_timeout(WAIT).unwrap();
    let answered = api.requests.recv_timeout(WAIT).unwrap();
    let plain = api.requests.recv_timeout(WAIT).unwrap();
    assert!(offered_tools(&asked).contains(&"AskUserQuestion") && offered_tools(&asked).contains(&"EnterPlanMode"));
    assert!(!offered_tools(&plain).contains(&"AskUserQuestion") && !offered_tools(&plain).contains(&"EnterPlanMode"));
    let result = answered["messages"].as_array().unwrap().last().unwrap()["content"][0]["content"].to_string();
    assert!(result.contains("Which color?") && result.contains("Blue"), "{result}");
}

#[test]
fn an_approved_plan_is_implemented_in_the_same_prompt() {
    let home = Home::new("plan");
    let api = fake_api(vec![
        reply(&[tool_use("toolu_enter", "EnterPlanMode", json!({}))], "tool_use"),
        reply(&[tool_use("toolu_exit", "ExitPlanMode", json!({"plan": "# Plan\nTouch made."}))], "tool_use"),
        reply(&[text("Here is the plan.")], "end_turn"),
        reply(&[tool_use("toolu_touch", "Bash", json!({"command": "touch made"}))], "tool_use"),
        reply(&[text("Implemented.")], "end_turn"),
    ]);
    let mut editor = Editor::start_with(&home, &api, json!({"elicitation": {"form": {}}}), "sk-fake");
    editor.form_replies.push_back(json!({"action": "accept", "content": {"q0": "Yes, enter plan mode"}}));
    editor.form_replies.push_back(json!({"action": "accept", "content": {"next": "implement"}}));
    let session_id = editor.new_session(&home.project());

    assert_eq!(editor.prompt(&session_id, "Plan, then make a file.").unwrap(), json!({"stopReason": "end_turn"}));

    let forms: Vec<&str> = editor.seen.iter().filter(|message| message["method"] == "elicitation/create").map(|message| message["params"]["message"].as_str().unwrap()).collect();
    assert_eq!(forms.len(), 2, "{forms:?}");
    assert!(forms[1].contains("Touch made."), "{forms:?}");
    let implementing = api.requests.try_iter().nth(3).unwrap();
    assert!(implementing["messages"].to_string().contains("Implement this plan:"), "{implementing}");
    assert!(home.project().join("made").exists());
}

#[test]
fn with_no_account_a_session_needs_the_terminal_login() {
    let home = Home::new("login");
    let api = fake_api(Vec::new());
    let mut editor = Editor::start_with(&home, &api, json!({}), "");

    let plain = editor.request("initialize", json!({"protocolVersion": 1, "clientCapabilities": {}})).unwrap();
    let terminal = editor.request("initialize", json!({"protocolVersion": 1, "clientCapabilities": {"auth": {"terminal": true}}})).unwrap();
    let id = editor.start_request("session/new", json!({"cwd": home.project(), "mcpServers": []}));
    let refused = editor.answer(id, "session/new");

    assert_eq!(plain["authMethods"], json!([]));
    assert_eq!((&terminal["authMethods"][0]["type"], &terminal["authMethods"][0]["args"]), (&json!("terminal"), &json!(["--login"])));
    assert_eq!(refused["error"]["code"], -32000, "{refused}");
}

#[test]
fn a_cancelled_session_new_stops_waiting_for_a_hung_mcp_server() {
    let home = Home::new("cancel");
    let api = fake_api(Vec::new());
    let mut editor = Editor::start(&home, &api);
    let hung = json!([{"name": "hung", "command": "sleep", "args": ["100"], "env": []}]);
    let started = std::time::Instant::now();

    let id = editor.start_request("session/new", json!({"cwd": home.project(), "mcpServers": hung}));
    std::thread::sleep(Duration::from_millis(300));
    editor.send(&json!({"jsonrpc": "2.0", "method": "$/cancel_request", "params": {"requestId": id}}));
    let answer = editor.answer(id, "session/new");

    assert_eq!(answer["error"]["code"], -32800, "{answer}");
    assert!(started.elapsed() < Duration::from_secs(10));
}

/// An MCP server over stdio with one tool, `echo`.
const FAKE_MCP_SERVER: &str = r#"
import json, sys
for line in sys.stdin:
    m = json.loads(line)
    if "id" not in m:
        continue
    if m["method"] == "initialize":
        r = {"protocolVersion": m["params"]["protocolVersion"], "capabilities": {"tools": {}}, "serverInfo": {"name": "fake", "version": "1"}}
    elif m["method"] == "tools/list":
        r = {"tools": [{"name": "echo", "description": "Echoes", "inputSchema": {"type": "object"}}]}
    else:
        r = {"content": [{"type": "text", "text": "echo"}]}
    print(json.dumps({"jsonrpc": "2.0", "id": m["id"], "result": r}), flush=True)
"#;

#[test]
fn a_subagent_gets_the_mcp_servers_of_the_editor() {
    let home = Home::new("subagent-mcp");
    let api = fake_api(vec![
        reply(&[tool_use("toolu_agent", "Agent", json!({"description": "Look", "prompt": "Say hi."}))], "tool_use"),
        reply(&[text("Hi from the child.")], "end_turn"),
        reply(&[text("The child said hi.")], "end_turn"),
    ]);
    let mut editor = Editor::start(&home, &api);
    let servers = json!([{"name": "fake", "command": "python3", "args": ["-c", FAKE_MCP_SERVER], "env": []}]);
    let session = editor.request("session/new", json!({"cwd": home.project(), "mcpServers": servers})).unwrap();

    editor.prompt(session["sessionId"].as_str().unwrap(), "Ask a subagent.").unwrap();

    let parent = api.requests.recv_timeout(WAIT).unwrap();
    let child = api.requests.recv_timeout(WAIT).unwrap();
    assert!(offered_tools(&parent).contains(&"mcp__fake__echo"), "{:?}", offered_tools(&parent));
    assert!(!offered_tools(&child).contains(&"Agent"), "the second request is the child's");
    assert!(offered_tools(&child).contains(&"mcp__fake__echo"), "{:?}", offered_tools(&child));
}

#[test]
fn an_editor_with_fs_serves_reads_and_takes_edits_of_its_buffers() {
    let home = Home::new("fs");
    let file = home.project().join("a.txt");
    std::fs::write(&file, "on disk\n").unwrap();
    let api = fake_api(vec![
        reply(&[tool_use("toolu_read", "Read", json!({"file_path": "a.txt"}))], "tool_use"),
        reply(&[tool_use("toolu_edit", "Edit", json!({"file_path": "a.txt", "old_string": "unsaved", "new_string": "edited"}))], "tool_use"),
        reply(&[text("Done.")], "end_turn"),
    ]);
    let mut editor = Editor::start_with(&home, &api, json!({"fs": {"readTextFile": true, "writeTextFile": true}}), "sk-fake");
    editor.buffers.insert(file.display().to_string(), "unsaved buffer\n".into());
    let session_id = editor.new_session(&home.project());

    editor.prompt(&session_id, "Fix a.txt.").unwrap();

    let read = api.requests.try_iter().nth(1).unwrap();
    let result = read["messages"].as_array().unwrap().last().unwrap()["content"][0]["content"].to_string();
    assert!(result.contains("unsaved buffer"), "{result}");
    assert_eq!(editor.buffers[&file.display().to_string()], "edited buffer\n");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "on disk\n");
    let edited = editor.updates("tool_call_update").into_iter().find(|update| update["toolCallId"] == "toolu_edit" && update.get("content").is_some()).unwrap();
    assert_eq!((&edited["content"][0]["oldText"], &edited["content"][0]["newText"]), (&json!("unsaved buffer\n"), &json!("edited buffer\n")));
}

#[test]
fn a_file_outside_the_editors_project_is_read_and_edited_on_disk() {
    let home = Home::new("fs-outside");
    let file = home.dir.join("notes.txt");
    std::fs::write(&file, "outside\n").unwrap();
    let api = fake_api(vec![
        reply(&[tool_use("toolu_read", "Read", json!({"file_path": file}))], "tool_use"),
        reply(&[tool_use("toolu_edit", "Edit", json!({"file_path": file, "old_string": "outside", "new_string": "edited"}))], "tool_use"),
        reply(&[tool_use("toolu_write", "Write", json!({"file_path": "../written.txt", "content": "new\n"}))], "tool_use"),
        reply(&[text("Done.")], "end_turn"),
    ]);
    let mut editor = Editor::start_with(&home, &api, json!({"fs": {"readTextFile": true, "writeTextFile": true}}), "sk-fake");
    let session_id = editor.new_session(&home.project());

    editor.prompt(&session_id, "Fix notes.txt.").unwrap();

    assert_eq!(std::fs::read_to_string(&file).unwrap(), "edited\n");
    assert_eq!(std::fs::read_to_string(home.dir.join("written.txt")).unwrap(), "new\n");
    let asked_the_editor = editor.seen.iter().any(|message| message["method"].as_str().is_some_and(|method| method.starts_with("fs/")));
    assert!(!asked_the_editor, "{:#?}", editor.seen);
}

#[test]
fn bash_runs_in_the_editor_terminal_and_the_model_gets_the_same_result() {
    let home = Home::new("terminal");
    let api = fake_api(vec![reply(&[tool_use("toolu_sh", "Bash", json!({"command": "echo from-terminal; exit 3"}))], "tool_use"), reply(&[text("It failed.")], "end_turn")]);
    let mut editor = Editor::start_with(&home, &api, json!({"terminal": true}), "sk-fake");
    let session_id = editor.new_session(&home.project());

    editor.prompt(&session_id, "Run it.").unwrap();

    let created = editor.seen.iter().find(|message| message["method"] == "terminal/create").expect("a terminal");
    assert_eq!(created["params"]["cwd"], json!(home.project()));
    let result = api.requests.try_iter().nth(1).unwrap()["messages"].as_array().unwrap().last().unwrap()["content"][0].clone();
    assert_eq!(result["is_error"], true);
    assert!(result["content"].to_string().contains("from-terminal") && result["content"].to_string().contains("Command exited with code 3"), "{result}");
    // Zed rejects an update that names a released terminal; the card keeps the terminal it was given at the start.
    let released = editor.seen.iter().position(|message| message["method"] == "terminal/release").expect("a released terminal");
    assert!(editor.seen[released + 1..].iter().all(|message| !message.to_string().contains("term-0")), "{:?}", &editor.seen[released + 1..]);
    let ended = editor.updates("tool_call_update").into_iter().rfind(|update| update["toolCallId"] == "toolu_sh").unwrap();
    assert_eq!((&ended["status"], ended.get("content")), (&json!("failed"), None));
}

#[test]
fn a_refused_terminal_release_keeps_the_commands_result() {
    let home = Home::new("release-refused");
    let api = fake_api(vec![reply(&[tool_use("toolu_sh", "Bash", json!({"command": "echo ran"}))], "tool_use"), reply(&[text("Done.")], "end_turn")]);
    let mut editor = Editor::start_with(&home, &api, json!({"terminal": true}), "sk-fake");
    editor.terminal_errors = vec!["terminal/release"];
    let session_id = editor.new_session(&home.project());

    editor.prompt(&session_id, "Run it.").unwrap();

    let result = api.requests.try_iter().nth(1).unwrap()["messages"].as_array().unwrap().last().unwrap()["content"][0].clone();
    assert_eq!(result["is_error"], false, "{result}");
    assert!(result["content"].to_string().contains("ran"), "{result}");
}

#[test]
fn a_terminal_is_released_when_the_editor_fails_its_wait_or_ignores_the_kill() {
    for (errors, silent) in [(vec!["terminal/wait_for_exit"], vec![]), (vec![], vec!["terminal/wait_for_exit"])] {
        let home = Home::new("release-always");
        let api = fake_api(vec![reply(&[tool_use("toolu_sh", "Bash", json!({"command": "sleep 1", "timeout": 500}))], "tool_use"), reply(&[text("Done.")], "end_turn")]);
        let mut editor = Editor::start_with(&home, &api, json!({"terminal": true}), "sk-fake");
        (editor.terminal_errors, editor.terminal_silent) = (errors, silent);
        let session_id = editor.new_session(&home.project());

        editor.prompt(&session_id, "Run it.").unwrap();

        assert!(editor.seen.iter().any(|message| message["method"] == "terminal/release"), "{:#?}", editor.seen);
    }
}

#[test]
fn session_cancel_stops_a_running_command_and_ends_the_turn_as_cancelled() {
    let home = Home::new("cancel-running");
    let api = fake_api(vec![reply(&[tool_use("toolu_sleep", "Bash", json!({"command": "sleep 120"}))], "tool_use")]);
    let mut editor = Editor::start(&home, &api);
    let session_id = editor.new_session(&home.project());
    let prompt = editor.start_request("session/prompt", json!({"sessionId": session_id, "prompt": [{"type": "text", "text": "Wait."}]}));
    editor.until("the sleep's tool call", |message| message["params"]["update"]["sessionUpdate"] == "tool_call");

    editor.send(&json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": session_id}}));

    assert_eq!(editor.answer(prompt, "session/prompt")["result"], json!({"stopReason": "cancelled"}));
}

#[test]
fn a_rejected_tool_call_ends_the_turn_without_cancelled() {
    let home = Home::new("reject");
    let api = fake_api(vec![reply(&[tool_use("toolu_touch", "Bash", json!({"command": "touch made"}))], "tool_use")]);
    let mut editor = Editor::start(&home, &api);
    editor.permission_answer = "reject_once";
    let session_id = editor.new_session(&home.project());

    assert_eq!(editor.prompt(&session_id, "Make a file.").unwrap(), json!({"stopReason": "end_turn"}));
    assert!(!home.project().join("made").exists());
}

#[test]
fn a_refused_prompt_ends_as_refusal_and_stays_out_of_the_next_request() {
    let home = Home::new("refusal");
    let api = fake_api(vec![reply(&[text("I will not.")], "refusal"), reply(&[text("Sure.")], "end_turn")]);
    let mut editor = Editor::start(&home, &api);
    let session_id = editor.new_session(&home.project());

    assert_eq!(editor.prompt(&session_id, "Something forbidden.").unwrap(), json!({"stopReason": "refusal"}));
    editor.prompt(&session_id, "Something fine.").unwrap();

    let next = api.requests.try_iter().last().unwrap()["messages"].to_string();
    assert!(next.contains("Something fine.") && !next.contains("forbidden") && !next.contains("I will not."), "{next}");
}
