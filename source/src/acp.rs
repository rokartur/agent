//! `agent acp`: the Agent Client Protocol for editors like Zed, JSON-RPC 2.0 with one message per line on stdin and stdout.
//! shortcut: one session per process and no session/load or client fs; add them when an editor needs them.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};

use serde_json::{Value, json};

use crate::agent::permissions::Decision;
use crate::agent::{Agent, Event};

const PROTOCOL_VERSION: u64 = 1;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;

pub fn serve(agent: &mut Agent) -> Result<(), String> {
    let cancelled = Arc::new(AtomicBool::new(false));
    let (requests, replies) = read_stdin(agent.abort.clone(), cancelled.clone());
    for message in requests {
        let result = handle(agent, &message, &replies, &cancelled);
        let id = &message["id"];
        if id.is_null() {
            continue;
        }
        send(&match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err((code, error)) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": error}}),
        });
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
    });
    (request_rx, reply_rx)
}

fn handle(agent: &mut Agent, message: &Value, replies: &Receiver<Value>, cancelled: &AtomicBool) -> Result<Value, (i64, String)> {
    let params = &message["params"];
    match message["method"].as_str().unwrap_or_default() {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "agentCapabilities": {
                "loadSession": false,
                "promptCapabilities": {"image": false, "audio": false, "embeddedContext": true},
                "mcpCapabilities": {"http": true, "sse": false},
            },
            "agentInfo": {"name": "agent", "title": "agent", "version": env!("CARGO_PKG_VERSION")},
            "authMethods": [],
        })),
        "session/new" => {
            let cwd = params["cwd"].as_str().unwrap_or_default();
            // macOS: /tmp is a link to /private/tmp, and agent.cwd is resolved.
            if std::fs::canonicalize(cwd).ok().as_deref() != Some(agent.cwd.as_path()) {
                return Err((INVALID_PARAMS, format!("agent acp serves the directory it started in, {}, not {cwd}.", agent.cwd.display())));
            }
            let settings = crate::settings::load(&agent.home).map_err(|error| (INTERNAL_ERROR, error))?;
            agent.mcp.set_client_servers(&params["mcpServers"], &settings.disabled_mcp, &settings.approved_mcp).map_err(|error| (INVALID_PARAMS, error))?;
            agent.mcp.settled();
            if !agent.messages.is_empty() {
                agent.new_session().map_err(|error| (INTERNAL_ERROR, error))?;
            }
            Ok(json!({"sessionId": agent.session.id}))
        }
        "session/prompt" => {
            let session_id = params["sessionId"].as_str().unwrap_or_default().to_string();
            if session_id != agent.session.id {
                return Err((INVALID_PARAMS, format!("Unknown session {session_id}.")));
            }
            let text = prompt_text(&params["prompt"]);
            let first_new_message = agent.messages.len();
            let mut turn = Turn { cwd: agent.cwd.clone(), ..Turn::default() };
            cancelled.store(false, Ordering::Relaxed);
            let result = agent.prompt(&text, &mut |event| match event {
                Event::Permission { request, reply } => {
                    // Tool call ids are unique, so a late reply from an earlier turn cannot answer this request.
                    let id = format!("permission-{}", turn.last_tool_call);
                    let question = format!("{} {}", request.question, request.detail);
                    send(&json!({"jsonrpc": "2.0", "id": id, "method": "session/request_permission", "params": {
                        "sessionId": session_id,
                        "toolCall": {"toolCallId": turn.last_tool_call, "title": question},
                        "options": [
                            {"optionId": "allow_once", "name": "Allow once", "kind": "allow_once"},
                            {"optionId": "allow_always", "name": "Allow for this session", "kind": "allow_always"},
                            {"optionId": "reject_once", "name": "Reject", "kind": "reject_once"},
                        ],
                    }}));
                    let _ = reply.send(wait_for_decision(replies, &id));
                }
                event => {
                    if let Some(update) = turn.session_update(&event) {
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
    last_tool_call: String,
    /// Each running edit's file and its text before the edit, None for a new file, so its end can show a diff.
    before_edit: HashMap<String, (PathBuf, Option<String>)>,
    /// The last model reply's, in Anthropic's names.
    stop_reason: String,
}

impl Turn {
    /// The session/update the editor shows for `event`, if any.
    fn session_update(&mut self, event: &Event) -> Option<Value> {
        let text = |kind: &str, text: &str| json!({"sessionUpdate": kind, "content": {"type": "text", "text": text}});
        Some(match event {
            Event::Text(chunk) => text("agent_message_chunk", chunk),
            Event::Thinking(chunk) => text("agent_thought_chunk", chunk),
            Event::ToolStart { id, name, input } => {
                self.last_tool_call = id.clone();
                let path = input["file_path"].as_str().map(|path| self.cwd.join(path));
                if let Some(path) = path.as_ref().filter(|_| tool_kind(name) == "edit") {
                    self.before_edit.insert(id.clone(), (path.clone(), std::fs::read_to_string(path).ok()));
                }
                let locations: Vec<Value> = path.iter().map(|path| json!({"path": path, "line": input["offset"].as_u64()})).collect();
                json!({
                    "sessionUpdate": "tool_call",
                    "toolCallId": id,
                    "name": name,
                    "title": format!("{name} {}", crate::preview(input)),
                    "kind": tool_kind(name),
                    "status": "in_progress",
                    "rawInput": input,
                    "locations": locations,
                })
            }
            Event::ToolEnd { id, output, is_error } => {
                let mut content = vec![json!({"type": "content", "content": {"type": "text", "text": output}})];
                if let Some((path, old_text)) = self.before_edit.remove(id)
                    && !is_error
                    && let Ok(new_text) = std::fs::read_to_string(&path)
                {
                    content.insert(0, json!({"type": "diff", "path": path, "oldText": old_text, "newText": new_text}));
                }
                json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": id,
                    "status": if *is_error { "failed" } else { "completed" },
                    "content": content,
                })
            }
            Event::Moved { cwd, .. } => {
                self.cwd = cwd.clone();
                return None;
            }
            Event::TurnEnd { stop_reason, .. } => {
                self.stop_reason = stop_reason.clone();
                return None;
            }
            _ => return None,
        })
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
        let start = turn.session_update(&Event::ToolStart { id: "t1".into(), name: "Bash".into(), input: json!({"command": "ls"}) }).unwrap();
        assert_eq!((start["sessionUpdate"].as_str(), start["kind"].as_str(), start["status"].as_str()), (Some("tool_call"), Some("execute"), Some("in_progress")));
        assert_eq!(start["locations"], json!([]));
        let end = turn.session_update(&Event::ToolEnd { id: "t1".into(), output: "x".into(), is_error: true }).unwrap();
        assert_eq!(end, json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "failed", "content": [{"type": "content", "content": {"type": "text", "text": "x"}}]}));
        assert!(turn.session_update(&Event::Notice("n".into())).is_none());
    }

    #[test]
    fn an_edit_shows_where_it_is_and_its_diff() {
        let dir = std::env::temp_dir().join(format!("agent-acp-edit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), "old").unwrap();
        let mut turn = Turn { cwd: dir.clone(), ..Turn::default() };
        let mut run = |id: &str, name: &str, file: &str| {
            let start = turn.session_update(&Event::ToolStart { id: id.into(), name: name.into(), input: json!({"file_path": file}) }).unwrap();
            std::fs::write(dir.join(file), "new").unwrap();
            let end = turn.session_update(&Event::ToolEnd { id: id.into(), output: "ok".into(), is_error: false }).unwrap();
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
        assert!(turn.session_update(&reply_stopped("aborted")).is_none());
        assert_eq!((turn.stop_reason(false), turn.stop_reason(true)), ("end_turn", "cancelled"));
        turn.session_update(&reply_stopped("max_tokens"));
        assert_eq!(turn.stop_reason(false), "max_tokens");
        turn.session_update(&reply_stopped("refusal"));
        assert_eq!(turn.stop_reason(false), "refusal");
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
