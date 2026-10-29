//! `agent acp` starts each session in its own `agent acp` process, in the session's directory, and passes messages
//! between the editor and those processes, so the working directory and the per-pid files stay one session's (ADR 0001).

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Stdio};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::{INTERNAL_ERROR, INVALID_PARAMS, METHOD_NOT_FOUND, send};
use crate::agent::Session;
use crate::tools::web::providers::iso_from_ms;

/// Set for a session's process, so its `agent acp` serves one session instead of routing.
pub const SESSION_ENV: &str = "AGENT_ACP_SESSION";

type Writer = Arc<Mutex<ChildStdin>>;

#[derive(Default)]
struct Routes {
    /// Each session's process, by the session id the editor knows.
    sessions: HashMap<String, Writer>,
    /// Requests a session's process sent the editor, by id, so the reply goes back to that process.
    asked: HashMap<String, Writer>,
}

/// `args` start each session's process the way this one started, `acp` and its flags included.
pub fn route(args: &[String]) -> Result<(), String> {
    let routes = Arc::new(Mutex::new(Routes::default()));
    let mut initialize = Value::Null;
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
        let Some(method) = message["method"].as_str() else {
            let asking = routes.lock().unwrap().asked.remove(&message["id"].to_string());
            if let Some(writer) = asking {
                let _ = write(&writer, &message);
            }
            continue;
        };
        let answer = match method {
            "initialize" => {
                initialize = message["params"].clone();
                Ok(Some(super::initialize_result()))
            }
            "session/new" => start(&message, None, &initialize, args, &routes).map(|()| None),
            "session/load" | "session/resume" => {
                let session_id = message["params"]["sessionId"].as_str().map(String::from);
                start(&message, session_id, &initialize, args, &routes).map(|()| None)
            }
            "session/list" => list(&message["params"]).map(Some),
            // Routed no more right away, so a prompt sent while the process exits gets an error, not silence.
            "session/close" => forward(&message, &routes).map(|()| {
                routes.lock().unwrap().sessions.remove(message["params"]["sessionId"].as_str().unwrap_or_default());
                None
            }),
            _ => forward(&message, &routes).map(|()| None),
        };
        let id = &message["id"];
        match answer {
            _ if id.is_null() => {}
            Ok(None) => {}
            Ok(Some(result)) => send(&json!({"jsonrpc": "2.0", "id": id, "result": result})),
            Err((code, error)) => send(&json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": error}})),
        }
    }
    Ok(())
}

/// Starts a process for the session `message` opens; it answers `message` itself. `session_id` is None for a new
/// session, whose id comes in the answer.
fn start(message: &Value, session_id: Option<String>, initialize: &Value, args: &[String], routes: &Arc<Mutex<Routes>>) -> Result<(), (i64, String)> {
    let cwd = message["params"]["cwd"].as_str().unwrap_or_default();
    if !Path::new(cwd).is_absolute() || !Path::new(cwd).is_dir() {
        return Err((INVALID_PARAMS, format!("cwd must be the absolute path of a directory, not {cwd:?}.")));
    }
    let exe = std::env::current_exe().map_err(|e| (INTERNAL_ERROR, format!("Cannot find the agent binary: {e}")))?;
    let mut child = crate::process::command(exe)
        .args(args)
        .current_dir(cwd)
        .env(SESSION_ENV, "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| (INTERNAL_ERROR, format!("Cannot start a session process: {e}")))?;
    let writer = Arc::new(Mutex::new(child.stdin.take().expect("piped stdin")));
    let stdout = child.stdout.take().expect("piped stdout");
    // No id: the process keeps the client's capabilities, and the editor already has our answer.
    write(&writer, &json!({"jsonrpc": "2.0", "method": "initialize", "params": initialize}))?;
    write(&writer, message)?;
    let (starting, routes) = (message["id"].clone(), routes.clone());
    std::thread::spawn(move || relay(child, stdout, writer, starting, session_id, routes));
    Ok(())
}

/// Passes a session process's messages to the editor until it exits. The session is routed to it once it answers
/// `starting`, before the editor reads that answer and sends the session's first prompt.
fn relay(mut child: Child, stdout: ChildStdout, writer: Writer, starting: Value, session_id: Option<String>, routes: Arc<Mutex<Routes>>) {
    let mut answered = false;
    for line in BufReader::new(stdout).lines().map_while(Result::ok) {
        let Ok(message) = serde_json::from_str::<Value>(&line) else { continue };
        if message["method"].is_string() && !message["id"].is_null() {
            routes.lock().unwrap().asked.insert(message["id"].to_string(), writer.clone());
        } else if !answered && message["id"] == starting {
            answered = true;
            let started = session_id.clone().or_else(|| message["result"]["sessionId"].as_str().map(String::from));
            let Some(started) = started.filter(|_| message.get("error").is_none()) else {
                send(&message);
                break;
            };
            routes.lock().unwrap().sessions.insert(started, writer.clone());
        }
        send(&message);
    }
    let mut routes = routes.lock().unwrap();
    routes.sessions.retain(|_, routed| !Arc::ptr_eq(routed, &writer));
    routes.asked.retain(|_, routed| !Arc::ptr_eq(routed, &writer));
    drop(routes);
    if !answered && !starting.is_null() {
        send(&json!({"jsonrpc": "2.0", "id": starting, "error": {"code": INTERNAL_ERROR, "message": "The session's process exited before it started the session."}}));
    }
    drop(writer);
    let _ = child.wait();
}

/// The saved sessions, newest first, of `cwd` when the editor names one.
fn list(params: &Value) -> Result<Value, (i64, String)> {
    let sessions_dir = crate::home().map_err(|error| (INTERNAL_ERROR, error))?.join(".agents/sessions");
    // macOS: /tmp is a link to /private/tmp, and sessions record the resolved directory.
    let cwd = params["cwd"].as_str().map(|cwd| std::fs::canonicalize(cwd).map_or(cwd.to_string(), |cwd| cwd.display().to_string()));
    let sessions: Vec<Value> = Session::list(&sessions_dir)
        .into_iter()
        .filter(|info| cwd.as_ref().is_none_or(|cwd| info.cwd == *cwd))
        .map(|info| {
            let updated = info.modified.duration_since(std::time::UNIX_EPOCH).map_or(0, |since| since.as_millis() as u64);
            json!({"sessionId": info.id, "cwd": info.cwd, "title": info.name.unwrap_or(info.title), "updatedAt": iso_from_ms(updated)})
        })
        .collect();
    Ok(json!({"sessions": sessions}))
}

/// Passes a request or notification to the process of the session it names.
fn forward(message: &Value, routes: &Arc<Mutex<Routes>>) -> Result<(), (i64, String)> {
    let Some(session_id) = message["params"]["sessionId"].as_str() else {
        return Err((METHOD_NOT_FOUND, format!("Method not found: {}", message["method"])));
    };
    let writer = routes.lock().unwrap().sessions.get(session_id).cloned();
    write(&writer.ok_or((INVALID_PARAMS, format!("Unknown session {session_id}.")))?, message)
}

fn write(writer: &Writer, message: &Value) -> Result<(), (i64, String)> {
    writeln!(writer.lock().unwrap(), "{message}").map_err(|e| (INTERNAL_ERROR, format!("The session's process is gone: {e}")))
}
