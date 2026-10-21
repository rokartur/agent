//! `agent acp`: the Agent Client Protocol for editors like Zed, JSON-RPC 2.0 with one message per line on stdin and stdout.
//! shortcut: one session per process and no session/load, client MCP servers or client fs; add them when an editor needs them.

use std::io::{BufRead, Write};
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
    let (requests, replies) = read_stdin(agent.abort.clone());
    for message in requests {
        let result = handle(agent, &message, &replies);
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
fn read_stdin(abort: Arc<AtomicBool>) -> (Receiver<Value>, Receiver<Value>) {
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

fn handle(agent: &mut Agent, message: &Value, replies: &Receiver<Value>) -> Result<Value, (i64, String)> {
    let params = &message["params"];
    match message["method"].as_str().unwrap_or_default() {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "agentCapabilities": {"loadSession": false, "promptCapabilities": {"image": false, "audio": false, "embeddedContext": true}},
            "agentInfo": {"name": "agent", "title": "agent", "version": env!("CARGO_PKG_VERSION")},
            "authMethods": [],
        })),
        "session/new" => {
            let cwd = params["cwd"].as_str().unwrap_or_default();
            // macOS: /tmp is a link to /private/tmp, and agent.cwd is resolved.
            if std::fs::canonicalize(cwd).ok().as_deref() != Some(agent.cwd.as_path()) {
                return Err((INVALID_PARAMS, format!("agent acp serves the directory it started in, {}, not {cwd}.", agent.cwd.display())));
            }
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
            let mut last_tool_call = String::new();
            let result = agent.prompt(&text, &mut |event| match event {
                Event::Permission { request, reply } => {
                    // Tool call ids are unique, so a late reply from an earlier turn cannot answer this request.
                    let id = format!("permission-{last_tool_call}");
                    let question = format!("{} {}", request.question, request.detail);
                    send(&json!({"jsonrpc": "2.0", "id": id, "method": "session/request_permission", "params": {
                        "sessionId": session_id,
                        "toolCall": {"toolCallId": last_tool_call, "title": question},
                        "options": [
                            {"optionId": "allow_once", "name": "Allow once", "kind": "allow_once"},
                            {"optionId": "allow_always", "name": "Allow for this session", "kind": "allow_always"},
                            {"optionId": "reject_once", "name": "Reject", "kind": "reject_once"},
                        ],
                    }}));
                    let _ = reply.send(wait_for_decision(replies, &id));
                }
                event => {
                    if let Event::ToolStart { id, .. } = &event {
                        last_tool_call = id.clone();
                    }
                    if let Some(update) = session_update(&event) {
                        send(&json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session_id, "update": update}}));
                    }
                }
            });
            // The ACP spec wants `cancelled`, not an error, for a turn the client cancelled.
            if agent.abort.load(Ordering::Relaxed) {
                return Ok(json!({"stopReason": "cancelled"}));
            }
            result.map(|_| json!({"stopReason": "end_turn"})).map_err(|error| (INTERNAL_ERROR, error))
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

fn session_update(event: &Event) -> Option<Value> {
    let text = |kind: &str, text: &str| json!({"sessionUpdate": kind, "content": {"type": "text", "text": text}});
    Some(match event {
        Event::Text(chunk) => text("agent_message_chunk", chunk),
        Event::Thinking(chunk) => text("agent_thought_chunk", chunk),
        Event::ToolStart { id, name, input } => json!({
            "sessionUpdate": "tool_call",
            "toolCallId": id,
            "name": name,
            "title": format!("{name} {}", crate::preview(input)),
            "kind": tool_kind(name),
            "status": "in_progress",
            "rawInput": input,
        }),
        Event::ToolEnd { id, output, is_error } => json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": id,
            "status": if *is_error { "failed" } else { "completed" },
            "content": [{"type": "content", "content": {"type": "text", "text": output}}],
        }),
        _ => return None,
    })
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
        let start = session_update(&Event::ToolStart { id: "t1".into(), name: "Bash".into(), input: json!({"command": "ls"}) }).unwrap();
        assert_eq!((start["sessionUpdate"].as_str(), start["kind"].as_str(), start["status"].as_str()), (Some("tool_call"), Some("execute"), Some("in_progress")));
        let end = session_update(&Event::ToolEnd { id: "t1".into(), output: "x".into(), is_error: true }).unwrap();
        assert_eq!(end, json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "failed", "content": [{"type": "content", "content": {"type": "text", "text": "x"}}]}));
        assert!(session_update(&Event::Notice("n".into())).is_none());
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
