//! `agent acp`: the Agent Client Protocol for editors like Zed, JSON-RPC 2.0 with one message per line on stdin and stdout.
//! `router` starts a process per session; `serve` is that process, and serves one session.
//! shortcut: no session/load or client fs; add them when an editor needs them.

mod router;
pub use router::{SESSION_ENV, route};

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use crate::agent::permissions::{Decision, MODES, Mode, Request};
use crate::agent::prompt::{self, Skill};
use crate::agent::{Agent, Event, workflow};
use crate::settings::THINKING_LEVELS;
use crate::tools::todo::{Status, Todos};

const PROTOCOL_VERSION: u64 = 1;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;

pub fn serve(agent: &mut Agent) -> Result<(), String> {
    let cancelled = Arc::new(AtomicBool::new(false));
    let (requests, replies) = read_stdin(agent.abort.clone(), cancelled.clone());
    let mut connection = Connection { replies, cancelled, session_id: String::new(), client: Value::Null, commands: Commands::default() };
    for message in requests {
        let result = handle(agent, &mut connection, &message);
        let id = &message["id"];
        if id.is_null() {
            continue;
        }
        let started_session = message["method"] == "session/new" && result.is_ok();
        send(&match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err((code, error)) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": error}}),
        });
        // After the answer, so the editor knows the session the update is for.
        if started_session {
            send(&json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": connection.session_id, "update": connection.commands.update()}}));
        }
    }
    Ok(())
}

/// Requests go to the first receiver, replies to our permission requests to the second.
/// session/cancel sets the abort flag here, since the main thread is busy in the turn it cancels.
/// A denied tool call aborts the turn too, so `cancelled` tells the two apart.
fn read_stdin(abort: Arc<AtomicBool>, cancelled: Arc<AtomicBool>) -> (Receiver<Value>, Receiver<Value>) {
    let (requests, request_rx) = mpsc::channel();
    let (replies, reply_rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines().map_while(Result::ok) {
            if line.trim().is_empty() {
                continue;
            }
            let message: Value = match serde_json::from_str(&line) {
                Ok(message) => message,
                Err(error) => {
                    send(&json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": format!("Parse error: {error}")}}));
                    continue;
                }
            };
            let sent = match message["method"].as_str() {
                Some("session/cancel") => {
                    cancelled.store(true, Ordering::Relaxed);
                    abort.store(true, Ordering::Relaxed);
                    Ok(())
                }
                Some(_) => requests.send(message).map_err(|_| ()),
                None => replies.send(message).map_err(|_| ()),
            };
            if sent.is_err() {
                break;
            }
        }
        // The editor or the router is gone, so no one sees the rest of a running turn.
        abort.store(true, Ordering::Relaxed);
    });
    (request_rx, reply_rx)
}

/// What the connection remembers between messages.
struct Connection {
    /// The editor's replies to our requests.
    replies: Receiver<Value>,
    cancelled: Arc<AtomicBool>,
    /// The id the editor knows. A `cd` moves the agent to a new saved session, and the editor keeps this one.
    session_id: String,
    /// `clientCapabilities` from initialize.
    client: Value,
    commands: Commands,
}

fn handle(agent: &mut Agent, connection: &mut Connection, message: &Value) -> Result<Value, (i64, String)> {
    let Connection { replies, cancelled, session_id, client, commands } = connection;
    let params = &message["params"];
    match message["method"].as_str().unwrap_or_default() {
        "initialize" => {
            *client = params["clientCapabilities"].clone();
            Ok(initialize_result())
        }
        "session/new" => {
            let cwd = params["cwd"].as_str().unwrap_or_default();
            // macOS: /tmp is a link to /private/tmp, and agent.cwd is resolved.
            if std::fs::canonicalize(cwd).ok().as_deref() != Some(agent.cwd.as_path()) {
                return Err((INVALID_PARAMS, format!("agent acp serves the directory it started in, {}, not {cwd}.", agent.cwd.display())));
            }
            let settings = crate::settings::load(&agent.home).map_err(|error| (INTERNAL_ERROR, error))?;
            agent.mcp.set_client_servers(&params["mcpServers"], &settings.disabled_mcp, &settings.approved_mcp).map_err(|error| (INVALID_PARAMS, error))?;
            agent.mcp.settled();
            for dir in params["additionalDirectories"].as_array().into_iter().flatten() {
                agent.add_dir(dir.as_str().unwrap_or_default()).map_err(|error| (INVALID_PARAMS, error))?;
            }
            if !agent.messages.is_empty() {
                agent.new_session().map_err(|error| (INTERNAL_ERROR, error))?;
            }
            *session_id = agent.session.id.clone();
            *commands = Commands::load(agent).map_err(|error| (INTERNAL_ERROR, error))?;
            Ok(json!({"sessionId": session_id, "modes": modes(agent), "configOptions": config_options(agent)}))
        }
        "session/set_mode" => {
            known_session(params, session_id)?;
            let asked = params["modeId"].as_str().unwrap_or_default();
            let mode = MODES.into_iter().find(|mode| mode_id(*mode) == asked).ok_or((INVALID_PARAMS, format!("Unknown mode {asked}.")))?;
            *agent.permission_mode.lock().unwrap() = mode;
            Ok(json!({}))
        }
        "session/set_config_option" => {
            known_session(params, session_id)?;
            let value = params["value"].as_str().unwrap_or_default();
            let option = config_options(agent).into_iter().find(|option| option["id"] == params["configId"]);
            let Some(option) = option else { return Err((INVALID_PARAMS, format!("Unknown config option {}.", params["configId"]))) };
            if !option["options"].as_array().into_iter().flatten().any(|offered| offered["value"] == value) {
                return Err((INVALID_PARAMS, format!("{} cannot be {value}.", option["name"].as_str().unwrap_or_default())));
            }
            let mut settings = crate::settings::load(&agent.home).map_err(|error| (INTERNAL_ERROR, error))?;
            // settle_model applies every setting, so the session keeps what it changed and only this option moves.
            settings.model = agent.model.clone();
            settings.thinking = agent.effort.clone();
            settings.permission_mode = *agent.permission_mode.lock().unwrap();
            match option["id"].as_str() {
                Some("model") => settings.model = value.to_string(),
                _ => settings.thinking = value.to_string(),
            }
            agent.settle_model(&mut settings).map_err(|error| (INTERNAL_ERROR, error))?;
            Ok(json!({"configOptions": config_options(agent)}))
        }
        "session/prompt" => {
            known_session(params, session_id)?;
            let text = commands.expand(&prompt_text(&params["prompt"])).map_err(|error| (INTERNAL_ERROR, error))?;
            let images: Vec<Value> = image_blocks(&params["prompt"]).map(crate::image::from_base64).collect::<Result<_, _>>().map_err(|error| (INVALID_PARAMS, error))?;
            agent.images.extend(images);
            let first_new_message = agent.messages.len();
            let mut turn = Turn { cwd: agent.cwd.clone(), todos: agent.todos.clone(), context_window: agent.context_window, notices: !client["session"]["notices"].is_null(), ..Turn::default() };
            cancelled.store(false, Ordering::Relaxed);
            let result = agent.prompt(&text, &mut |event| match event {
                Event::Permission { call, request, reply } => {
                    let tool_call = turn.permission_tool_call(call.as_deref(), &request);
                    // Tool call ids are unique, so a late reply from an earlier turn cannot answer this request.
                    let id = format!("permission-{}", tool_call["toolCallId"].as_str().unwrap_or_default());
                    send(&json!({"jsonrpc": "2.0", "id": id, "method": "session/request_permission", "params": {
                        "sessionId": session_id,
                        "toolCall": tool_call,
                        "options": [
                            {"optionId": "allow_once", "name": "Allow once", "kind": "allow_once"},
                            {"optionId": "allow_always", "name": "Allow for this session", "kind": "allow_always"},
                            {"optionId": "reject_once", "name": "Reject", "kind": "reject_once"},
                        ],
                    }}));
                    let decision = wait_for_decision(replies, &id);
                    // An edit hook's check has no tool call whose end would close its card.
                    if call.is_none() {
                        let status = if matches!(decision, Decision::Deny) { "failed" } else { "completed" };
                        let update = json!({"sessionUpdate": "tool_call_update", "toolCallId": tool_call["toolCallId"], "status": status});
                        send(&json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session_id, "update": update}}));
                    }
                    let _ = reply.send(decision);
                }
                event => {
                    for update in turn.session_updates(&event) {
                        send(&json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session_id, "update": update}}));
                    }
                }
            });
            let stop_reason = turn.stop_reason(cancelled.load(Ordering::Relaxed));
            // The ACP spec wants `cancelled`, not an error, for a turn the client cancelled.
            if stop_reason != "cancelled" {
                result.map_err(|error| (INTERNAL_ERROR, error))?;
            }
            // ACP leaves a refused prompt and what followed it out of later prompts, and Zed drops them from its thread.
            if stop_reason == "refusal" {
                agent.forget_from(first_new_message).map_err(|error| (INTERNAL_ERROR, error))?;
            }
            Ok(json!({"stopReason": stop_reason}))
        }
        method => Err((METHOD_NOT_FOUND, format!("Method not found: {method}"))),
    }
}

/// What this agent supports; the router answers initialize with it and passes the client's capabilities on.
fn initialize_result() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "agentCapabilities": {
            "loadSession": false,
            "promptCapabilities": {"image": true, "audio": false, "embeddedContext": true},
            "mcpCapabilities": {"http": true, "sse": false},
            "sessionCapabilities": {"additionalDirectories": {}},
        },
        "agentInfo": {"name": "agent", "title": "agent", "version": env!("CARGO_PKG_VERSION")},
        "authMethods": [],
    })
}

/// Errs unless `params` names the session this connection started.
fn known_session(params: &Value, session_id: &str) -> Result<(), (i64, String)> {
    let asked = params["sessionId"].as_str().unwrap_or_default();
    if session_id.is_empty() || asked != session_id {
        return Err((INVALID_PARAMS, format!("Unknown session {asked}.")));
    }
    Ok(())
}

/// Permission modes as ACP session modes. Plan mode stays out until plan approval can reach the editor.
fn modes(agent: &Agent) -> Value {
    let available: Vec<Value> = MODES.iter().map(|mode| json!({"id": mode_id(*mode), "name": mode.label(), "description": mode.description()})).collect();
    json!({"currentModeId": mode_id(*agent.permission_mode.lock().unwrap()), "availableModes": available})
}

fn mode_id(mode: Mode) -> String {
    mode.label().replace(' ', "-")
}

/// The model, from the models an account lists, and the effort.
fn config_options(agent: &Agent) -> Vec<Value> {
    let catalog = agent.client.catalog();
    let models: Vec<Value> =
        catalog.iter().filter(|model| !model.accounts.is_empty() || model.id == agent.model).map(|model| json!({"value": model.id, "name": model.name, "description": model.provider})).collect();
    let efforts: Vec<Value> = THINKING_LEVELS.iter().map(|level| json!({"value": level, "name": level})).collect();
    vec![
        json!({"id": "model", "name": "Model", "category": "model", "type": "select", "currentValue": agent.model, "options": models}),
        json!({"id": "effort", "name": "Effort", "category": "thought_level", "type": "select", "currentValue": agent.effort, "options": efforts}),
    ]
}

/// The editor's `/` commands: skills as `skill:<name>`, like the TUI, and saved workflows by name.
#[derive(Default)]
struct Commands {
    skills: Vec<Skill>,
    workflows: Vec<workflow::Saved>,
}

impl Commands {
    fn load(agent: &Agent) -> Result<Commands, String> {
        let settings = crate::settings::load(&agent.home)?;
        let skills = prompt::load_skills(&agent.agent_dir, &agent.cwd, settings.trusts(&agent.cwd), |skill| settings.skill_enabled(skill), settings.prefer_global());
        Ok(Commands { skills, workflows: agent.saved_workflows() })
    }

    fn update(&self) -> Value {
        let skills = self.skills.iter().map(|skill| json!({"name": format!("skill:{}", skill.name), "description": skill.description, "input": {"hint": "what to do"}}));
        let workflows = self.workflows.iter().map(|saved| json!({"name": saved.meta.name, "description": saved.meta.description, "input": {"hint": "input"}}));
        json!({"sessionUpdate": "available_commands_update", "availableCommands": skills.chain(workflows).collect::<Vec<_>>()})
    }

    /// `text` as the model gets it: the skill for `/skill:<name> args`, a Workflow call for `/<workflow> input`.
    fn expand(&self, text: &str) -> Result<String, String> {
        if text.starts_with("/skill:") {
            return prompt::expand_skill(text, &self.skills);
        }
        let Some(command) = text.strip_prefix('/') else { return Ok(text.to_string()) };
        let (name, input) = command.split_once(char::is_whitespace).unwrap_or((command, ""));
        if self.workflows.iter().any(|saved| saved.meta.name == name) {
            return Ok(workflow::command_prompt(name, input));
        }
        Ok(text.to_string())
    }
}

/// Text blocks as they are, links and embedded resources as their uri and text.
fn prompt_text(blocks: &Value) -> String {
    let mut parts = Vec::new();
    for block in blocks.as_array().into_iter().flatten() {
        match block["type"].as_str() {
            Some("text") => parts.push(block["text"].as_str().unwrap_or_default().to_string()),
            Some("resource_link") => parts.push(format!("@{}", block["uri"].as_str().unwrap_or_default())),
            Some("resource") => {
                let resource = &block["resource"];
                let uri = resource["uri"].as_str().unwrap_or_default();
                parts.push(format!("<file uri=\"{uri}\">\n{}\n</file>", resource["text"].as_str().unwrap_or_default()));
            }
            _ => {}
        }
    }
    parts.join("\n\n")
}

/// The base64 data of each image block; `prompt_text` leaves them out.
fn image_blocks(blocks: &Value) -> impl Iterator<Item = &str> {
    blocks.as_array().into_iter().flatten().filter(|block| block["type"] == "image").map(|block| block["data"].as_str().unwrap_or_default())
}

/// The client's answer to our request `id`; a cancelled request or a closed stdin denies.
fn wait_for_decision(replies: &Receiver<Value>, id: &str) -> Decision {
    for reply in replies.iter() {
        if reply["id"] != id {
            continue;
        }
        return match reply["result"]["outcome"]["optionId"].as_str() {
            Some("allow_once") => Decision::Allow,
            Some("allow_always") => Decision::AllowAlways,
            _ => Decision::Deny,
        };
    }
    Decision::Deny
}

/// What one session/prompt remembers between the agent's events.
#[derive(Default)]
struct Turn {
    /// Where relative tool paths resolve; a `cd` moves it mid-turn.
    cwd: PathBuf,
    /// The `tool_call` update of each call that started, so its permission request shows the same card.
    started: HashMap<String, Value>,
    /// Each running edit's file and its text before the edit, None for a new file, so its end can show a diff.
    before_edit: HashMap<String, (PathBuf, Option<String>)>,
    /// The last model reply's, in Anthropic's names.
    stop_reason: String,
    todos: Arc<Mutex<Todos>>,
    context_window: u64,
    /// The client shows `notice` updates.
    notices: bool,
}

impl Turn {
    /// The session/updates the editor shows for `event`.
    fn session_updates(&mut self, event: &Event) -> Vec<Value> {
        let text = |kind: &str, text: &str| json!({"sessionUpdate": kind, "content": {"type": "text", "text": text}});
        vec![match event {
            Event::Text(chunk) => text("agent_message_chunk", chunk),
            Event::Thinking(chunk) => text("agent_thought_chunk", chunk),
            Event::ToolStart { id, name, input } => {
                let path = input["file_path"].as_str().map(|path| self.cwd.join(path));
                if let Some(path) = path.as_ref().filter(|_| tool_kind(name) == "edit") {
                    self.before_edit.insert(id.clone(), (path.clone(), std::fs::read_to_string(path).ok()));
                }
                let locations: Vec<Value> = path.iter().map(|path| json!({"path": path, "line": input["offset"].as_u64()})).collect();
                let update = json!({
                    "sessionUpdate": "tool_call",
                    "toolCallId": id,
                    "name": name,
                    "title": format!("{name} {}", crate::preview(input)),
                    "kind": tool_kind(name),
                    "status": "in_progress",
                    "rawInput": input,
                    "locations": locations,
                });
                self.started.insert(id.clone(), update.clone());
                update
            }
            Event::ToolEnd { id, output, is_error } => {
                let mut content = vec![json!({"type": "content", "content": {"type": "text", "text": output}})];
                if let Some((path, old_text)) = self.before_edit.remove(id)
                    && !is_error
                    && let Ok(new_text) = std::fs::read_to_string(&path)
                {
                    content.insert(0, json!({"type": "diff", "path": path, "oldText": old_text, "newText": new_text}));
                }
                let update = json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": id,
                    "status": if *is_error { "failed" } else { "completed" },
                    "content": content,
                });
                if !is_error && self.started.get(id).is_some_and(|call| call["name"] == "Todo") {
                    return vec![update, plan(&self.todos.lock().unwrap())];
                }
                update
            }
            Event::Moved { cwd, .. } => {
                self.cwd = cwd.clone();
                return Vec::new();
            }
            Event::TurnEnd { stop_reason, context_tokens, .. } => {
                self.stop_reason = stop_reason.clone();
                json!({"sessionUpdate": "usage_update", "used": context_tokens, "size": self.context_window})
            }
            Event::Titled(title) => json!({"sessionUpdate": "session_info_update", "title": title}),
            Event::Notice(notice) if self.notices && !notice.is_empty() => json!({"sessionUpdate": "notice", "severity": "info", "title": notice}),
            _ => return Vec::new(),
        }]
    }

    /// The tool call a permission request is about, titled with the question: the started call, or a new card for an
    /// edit hook's command, which no tool call runs.
    fn permission_tool_call(&self, call: Option<&str>, request: &Request) -> Value {
        let title = format!("{} {}", request.question, request.detail);
        let Some(mut tool_call) = call.and_then(|id| self.started.get(id)).cloned() else {
            let id = format!("edit-hook-{}", crate::process::now_ms());
            return json!({"toolCallId": id, "title": title, "kind": "execute", "status": "pending"});
        };
        if let Some(fields) = tool_call.as_object_mut() {
            fields.remove("sessionUpdate");
        }
        tool_call["title"] = title.into();
        tool_call
    }

    /// ACP's name for how the turn ended. A denied tool call aborts it too, but only session/cancel makes it `cancelled`.
    fn stop_reason(&self, cancelled: bool) -> &'static str {
        if cancelled {
            return "cancelled";
        }
        match self.stop_reason.as_str() {
            "max_tokens" => "max_tokens",
            "refusal" => "refusal",
            _ => "end_turn",
        }
    }
}

/// ACP's plan replaces the whole list each time; deleted tasks are left out.
fn plan(todos: &Todos) -> Value {
    let entries: Vec<Value> =
        todos.tasks.iter().filter(|task| task.status != Status::Deleted).map(|task| json!({"content": task.subject, "status": task.status.as_str(), "priority": "medium"})).collect();
    json!({"sessionUpdate": "plan", "entries": entries})
}

fn tool_kind(name: &str) -> &'static str {
    match name {
        "Read" => "read",
        "Grep" => "search",
        "Bash" => "execute",
        "Write" | "ReplaceLines" | "Edit" | "InsertLines" => "edit",
        "WebSearch" | "WebFetch" | "GetSearchContent" => "fetch",
        _ => "other",
    }
}

fn send(message: &Value) {
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{message}");
    let _ = stdout.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_blocks_become_one_message() {
        let blocks = json!([
            {"type": "text", "text": "fix this"},
            {"type": "resource_link", "uri": "file:///a.rs", "name": "a.rs"},
            {"type": "resource", "resource": {"uri": "file:///b.rs", "text": "fn b() {}"}},
            {"type": "image", "data": "...", "mimeType": "image/png"},
        ]);
        assert_eq!(prompt_text(&blocks), "fix this\n\n@file:///a.rs\n\n<file uri=\"file:///b.rs\">\nfn b() {}\n</file>");
    }

    #[test]
    fn tool_events_become_tool_call_updates() {
        let mut turn = Turn::default();
        let start = turn.session_updates(&Event::ToolStart { id: "t1".into(), name: "Bash".into(), input: json!({"command": "ls"}) }).remove(0);
        assert_eq!((start["sessionUpdate"].as_str(), start["kind"].as_str(), start["status"].as_str()), (Some("tool_call"), Some("execute"), Some("in_progress")));
        assert_eq!(start["locations"], json!([]));
        let end = turn.session_updates(&Event::ToolEnd { id: "t1".into(), output: "x".into(), is_error: true }).remove(0);
        assert_eq!(end, json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "failed", "content": [{"type": "content", "content": {"type": "text", "text": "x"}}]}));
        assert!(turn.session_updates(&Event::Notice("n".into())).is_empty());
    }

    #[test]
    fn an_edit_shows_where_it_is_and_its_diff() {
        let dir = std::env::temp_dir().join(format!("agent-acp-edit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), "old").unwrap();
        let mut turn = Turn { cwd: dir.clone(), ..Turn::default() };
        let mut run = |id: &str, name: &str, file: &str| {
            let start = turn.session_updates(&Event::ToolStart { id: id.into(), name: name.into(), input: json!({"file_path": file}) }).remove(0);
            std::fs::write(dir.join(file), "new").unwrap();
            let end = turn.session_updates(&Event::ToolEnd { id: id.into(), output: "ok".into(), is_error: false }).remove(0);
            (start["locations"].clone(), end["content"].clone())
        };
        let (edited_at, edit) = run("t1", "Edit", "a.rs");
        let (_, write) = run("t2", "Write", "b.rs");
        let (_, read) = run("t3", "Read", "a.rs");
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(edited_at, json!([{"path": dir.join("a.rs"), "line": null}]));
        assert_eq!(edit[0], json!({"type": "diff", "path": dir.join("a.rs"), "oldText": "old", "newText": "new"}));
        assert_eq!(edit[1]["content"]["text"], "ok");
        assert_eq!(write[0], json!({"type": "diff", "path": dir.join("b.rs"), "oldText": null, "newText": "new"}));
        assert_eq!(read.as_array().unwrap().len(), 1);
    }

    #[test]
    fn only_session_cancel_ends_a_turn_as_cancelled() {
        let mut turn = Turn::default();
        let reply_stopped = |reason: &str| Event::TurnEnd { usage: Default::default(), context_tokens: 0, stop_reason: reason.into() };
        turn.session_updates(&reply_stopped("aborted"));
        assert_eq!((turn.stop_reason(false), turn.stop_reason(true)), ("end_turn", "cancelled"));
        turn.session_updates(&reply_stopped("max_tokens"));
        assert_eq!(turn.stop_reason(false), "max_tokens");
        turn.session_updates(&reply_stopped("refusal"));
        assert_eq!(turn.stop_reason(false), "refusal");
    }

    #[test]
    fn a_permission_request_shows_the_card_of_the_call_that_asks() {
        let mut turn = Turn::default();
        turn.session_updates(&Event::ToolStart { id: "t1".into(), name: "Bash".into(), input: json!({"command": "rm x"}) });
        let request = Request { kind: crate::agent::permissions::Kind::Command, question: "Run this command?", detail: "rm x".into(), grant: "rm x".into() };

        let asked = turn.permission_tool_call(Some("t1"), &request);
        let hook = turn.permission_tool_call(None, &request);

        assert_eq!((&asked["toolCallId"], &asked["kind"], &asked["rawInput"]), (&json!("t1"), &json!("execute"), &json!({"command": "rm x"})));
        assert_eq!((asked["title"].as_str(), asked.get("sessionUpdate")), (Some("Run this command? rm x"), None));
        assert!(hook["toolCallId"].as_str().unwrap().starts_with("edit-hook-"), "{hook}");
    }

    #[test]
    fn todo_calls_send_the_whole_task_list_as_a_plan() {
        let mut turn = Turn::default();
        let mut run = |id: &str, input: Value| {
            turn.session_updates(&Event::ToolStart { id: id.into(), name: "Todo".into(), input: input.clone() });
            turn.todos.lock().unwrap().apply(&input).unwrap();
            turn.session_updates(&Event::ToolEnd { id: id.into(), output: "ok".into(), is_error: false })
        };
        run("t1", json!({"action": "create", "subject": "Read"}));
        run("t2", json!({"action": "create", "subject": "Write"}));
        run("t3", json!({"action": "create", "subject": "Drop"}));
        run("t4", json!({"action": "update", "taskId": 1, "status": "completed"}));
        run("t5", json!({"action": "update", "taskId": 2, "status": "in_progress"}));
        let updates = run("t6", json!({"action": "delete", "taskId": 3}));

        assert_eq!(updates[0]["sessionUpdate"], "tool_call_update");
        assert_eq!(
            updates[1],
            json!({"sessionUpdate": "plan", "entries": [
                {"content": "Read", "status": "completed", "priority": "medium"},
                {"content": "Write", "status": "in_progress", "priority": "medium"},
            ]})
        );
    }

    #[test]
    fn context_use_title_and_notices_reach_the_editor() {
        let mut turn = Turn { context_window: 200_000, ..Turn::default() };
        let ended = Event::TurnEnd { usage: Default::default(), context_tokens: 1234, stop_reason: "end_turn".into() };
        assert_eq!(turn.session_updates(&ended), [json!({"sessionUpdate": "usage_update", "used": 1234, "size": 200_000})]);
        assert_eq!(turn.session_updates(&Event::Titled("agent | acp".into())), [json!({"sessionUpdate": "session_info_update", "title": "agent | acp"})]);
        // Only a client that advertised session.notices gets them.
        assert!(turn.session_updates(&Event::Notice("Compacted.".into())).is_empty());
        turn.notices = true;
        assert_eq!(turn.session_updates(&Event::Notice("Compacted.".into())), [json!({"sessionUpdate": "notice", "severity": "info", "title": "Compacted."})]);
    }

    #[test]
    fn a_permission_reply_maps_to_a_decision() {
        let (tx, rx) = mpsc::channel();
        tx.send(json!({"id": "permission-0", "result": {"outcome": {"outcome": "selected", "optionId": "allow_once"}}})).unwrap();
        tx.send(json!({"id": "permission-1", "result": {"outcome": {"outcome": "selected", "optionId": "allow_always"}}})).unwrap();
        tx.send(json!({"id": "permission-2", "result": {"outcome": {"outcome": "cancelled"}}})).unwrap();
        assert!(matches!(wait_for_decision(&rx, "permission-1"), Decision::AllowAlways));
        assert!(matches!(wait_for_decision(&rx, "permission-2"), Decision::Deny));
        drop(tx);
        assert!(matches!(wait_for_decision(&rx, "permission-3"), Decision::Deny));
    }
}
