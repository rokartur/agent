//! `agent acp`: the Agent Client Protocol for editors like Zed, JSON-RPC 2.0 with one message per line on stdin and stdout.
//! `router` starts a process per session; `serve` is that process, and serves one session.

mod router;
pub use router::{SESSION_ENV, route};

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::agent::permissions::{Decision, MODES, Mode, Request};
use crate::agent::prompt::{self, Skill};
use crate::agent::{Agent, Event, workflow};
use crate::api::models::{Model, PROVIDERS};
use crate::settings::THINKING_LEVELS;
use crate::tools::ask::{self, Answer, Question};
use crate::tools::hashline::EditorFiles;
use crate::tools::plan::Plan;
use crate::tools::todo::{Status, Todos};
use crate::tools::{EditorTerminal, Killed};

const PROTOCOL_VERSION: u64 = 1;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;
const AUTH_REQUIRED: i64 = -32000;
const REQUEST_CANCELLED: i64 = -32800;

pub fn serve(agent: &mut Agent) -> Result<(), String> {
    let cancelled = Arc::new(AtomicBool::new(false));
    let (requests, replies) = read_stdin(agent.abort.clone(), cancelled.clone());
    let replies = Arc::new(Mutex::new(replies));
    let mut connection = Connection { replies, cancelled, session_id: String::new(), client: Value::Null, commands: Commands::default(), editor_client: None };
    for message in requests {
        let result = handle(agent, &mut connection, &message);
        let id = &message["id"];
        if id.is_null() {
            continue;
        }
        let method = message["method"].as_str().unwrap_or_default();
        let opened_session = matches!(method, "session/new" | "session/load" | "session/resume") && result.is_ok();
        let closed_session = method == "session/close" && result.is_ok();
        send(&match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err((code, error)) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": error}}),
        });
        // After the answer, so the editor knows the session the update is for.
        if opened_session {
            send(&json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": connection.session_id, "update": connection.commands.update()}}));
        }
        if closed_session {
            break;
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
                // ACP: a session/close cancels the running turn first, which would hold the close until it ends.
                Some("session/close") => {
                    cancelled.store(true, Ordering::Relaxed);
                    abort.store(true, Ordering::Relaxed);
                    requests.send(message).map_err(|_| ())
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
    /// The editor's replies to our requests; `EditorFiles` waits on them too.
    replies: Arc<Mutex<Receiver<Value>>>,
    cancelled: Arc<AtomicBool>,
    /// The id the editor knows. A `cd` moves the agent to a new saved session, and the editor keeps this one.
    session_id: String,
    /// `clientCapabilities` from initialize.
    client: Value,
    commands: Commands,
    editor_client: Option<Arc<EditorClient>>,
}

fn handle(agent: &mut Agent, connection: &mut Connection, message: &Value) -> Result<Value, (i64, String)> {
    let Connection { replies, cancelled, session_id, client, commands, editor_client } = connection;
    let params = &message["params"];
    match message["method"].as_str().unwrap_or_default() {
        "initialize" => {
            *client = params["clientCapabilities"].clone();
            Ok(initialize_result(client))
        }
        method @ ("session/new" | "session/load" | "session/resume") => {
            let cwd = params["cwd"].as_str().unwrap_or_default();
            // macOS: /tmp is a link to /private/tmp, and agent.cwd is resolved.
            if std::fs::canonicalize(cwd).ok().as_deref() != Some(agent.cwd.as_path()) {
                return Err((INVALID_PARAMS, format!("agent acp serves the directory it started in, {}, not {cwd}.", agent.cwd.display())));
            }
            let settings = crate::settings::load(&agent.home).map_err(|error| (INTERNAL_ERROR, error))?;
            if !agent.client.catalog().iter().any(usable) {
                return Err((AUTH_REQUIRED, "No account can run a model. Log in with /login in agent.".into()));
            }
            agent.mcp.set_client_servers(&params["mcpServers"], &settings.disabled_mcp, &settings.approved_mcp).map_err(|error| (INVALID_PARAMS, error))?;
            agent.mcp.settled();
            agent.asks = !client["elicitation"]["form"].is_null();
            if method == "session/new" {
                if !agent.messages.is_empty() {
                    agent.new_session().map_err(|error| (INTERNAL_ERROR, error))?;
                }
                *session_id = agent.session.id.clone();
            } else {
                // shortcut: a session the model moved with `cd` opens as it was before the move; follow its copy when that bites.
                let asked = params["sessionId"].as_str().unwrap_or_default();
                // The router sends a load of the open session here; opening it again would fork it.
                if *session_id != asked {
                    agent.open_session(asked).map_err(|error| (INVALID_PARAMS, error))?;
                }
                if agent.session.fork_notice().is_some() {
                    // The editor cannot learn a copy's id, so its turns would land where it never looks.
                    if let Some(copy) = &agent.session.path {
                        std::fs::remove_file(copy).map_err(|error| (INTERNAL_ERROR, format!("Cannot remove {}: {error}", copy.display())))?;
                    }
                    return Err((INVALID_PARAMS, format!("Session {asked} is open in another process. Close it there first.")));
                }
                *session_id = asked.to_string();
                if method == "session/load" {
                    replay(&agent.messages, agent, session_id);
                }
            }
            // After the open: loading a session replaces its directories with the saved ones.
            for dir in params["additionalDirectories"].as_array().into_iter().flatten() {
                agent.add_dir(dir.as_str().unwrap_or_default()).map_err(|error| (INVALID_PARAMS, error))?;
            }
            *commands = Commands::load(agent).map_err(|error| (INTERNAL_ERROR, error))?;
            let files = client["fs"]["readTextFile"] == true && client["fs"]["writeTextFile"] == true;
            let terminal = client["terminal"] == true;
            // Zed's fs methods reach only its project's worktrees, the session's directories.
            let dirs = params["additionalDirectories"].as_array().into_iter().flatten().filter_map(|dir| std::fs::canonicalize(dir.as_str()?).ok());
            let roots = if files { std::iter::once(agent.cwd.clone()).chain(dirs).collect() } else { Vec::new() };
            let editor = Arc::new(EditorClient { session_id: session_id.clone(), replies: replies.clone(), next_id: AtomicU64::new(0), roots, terminals: Mutex::default() });
            agent.anchors.editor = files.then(|| editor.clone() as Arc<dyn EditorFiles>);
            agent.editor_terminal = terminal.then(|| editor.clone() as Arc<dyn EditorTerminal>);
            *editor_client = Some(editor);
            let mut answer = json!({"modes": modes(agent), "configOptions": config_options(agent)});
            if method == "session/new" {
                answer["sessionId"] = json!(session_id);
            }
            Ok(answer)
        }
        "session/close" => {
            known_session(params, session_id)?;
            Ok(json!({}))
        }
        "session/set_mode" => {
            known_session(params, session_id)?;
            *agent.permission_mode.lock().unwrap() = mode_by_id(params["modeId"].as_str().unwrap_or_default())?;
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
            if option["id"] == "mode" {
                *agent.permission_mode.lock().unwrap() = mode_by_id(value)?;
                return Ok(json!({"configOptions": config_options(agent)}));
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
            // A profile turned on in the TUI since the last prompt changes which skills exist.
            *commands = Commands::load(agent).map_err(|error| (INTERNAL_ERROR, error))?;
            send(&json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session_id, "update": commands.update()}}));
            let text = commands.expand(&prompt_text(&params["prompt"])).map_err(|error| (INTERNAL_ERROR, error))?;
            let images: Vec<Value> = image_blocks(&params["prompt"]).map(crate::image::from_base64).collect::<Result<_, _>>().map_err(|error| (INVALID_PARAMS, error))?;
            agent.images.extend(images);
            // Only the TUI's sessions name themselves, and an editor lists its sessions by title.
            if agent.messages.is_empty() {
                agent.start_naming(&text);
            }
            let first_new_message = agent.messages.len();
            let mut turn = Turn {
                cwd: agent.cwd.clone(),
                todos: agent.todos.clone(),
                context_window: agent.context_window,
                notices: !client["session"]["notices"].is_null(),
                editor: editor_client.clone(),
                ..Turn::default()
            };
            cancelled.store(false, Ordering::Relaxed);
            let mut on_event = |event: Event| match event {
                Event::Permission { call, request, reply } => {
                    let tool_call = turn.permission_tool_call(call.as_deref(), &request);
                    // The pid too: tool call ids repeat across sessions (`call_0`), and the router keys replies by id.
                    let id = format!("permission-{}-{}", std::process::id(), tool_call["toolCallId"].as_str().expect("permission_tool_call sets a toolCallId"));
                    send(&json!({"jsonrpc": "2.0", "id": id, "method": "session/request_permission", "params": {
                        "sessionId": session_id,
                        "toolCall": tool_call,
                        "options": [
                            {"optionId": "allow_once", "name": "Allow once", "kind": "allow_once"},
                            {"optionId": "allow_always", "name": "Allow for this session", "kind": "allow_always"},
                            {"optionId": "reject_once", "name": "Reject", "kind": "reject_once"},
                        ],
                    }}));
                    let decision = wait_for_decision(&replies.lock().unwrap(), &id);
                    let update = match call.as_deref().and_then(|id| turn.started.get(id)) {
                        // Zed keeps the question as the card's title, so the answer gives the call its own title back.
                        Some(started) => json!({"sessionUpdate": "tool_call_update", "toolCallId": started["toolCallId"], "title": started["title"]}),
                        // An edit hook's check has no tool call whose end would close its card.
                        None => {
                            let status = if matches!(decision, Decision::Deny) { "failed" } else { "completed" };
                            json!({"sessionUpdate": "tool_call_update", "toolCallId": tool_call["toolCallId"], "status": status})
                        }
                    };
                    send(&json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session_id, "update": update}}));
                    let _ = reply.send(decision);
                }
                Event::Ask { questions, reply } => {
                    let message = questions.iter().map(|question| question.question.as_str()).collect::<Vec<_>>().join("\n");
                    let content = elicit(&replies.lock().unwrap(), session_id, &message, question_form(&questions));
                    // No answers at all is how the TUI declines.
                    let _ = reply.send(content.map_or_else(Vec::new, |content| form_answers(&questions, &content)));
                }
                event => {
                    for update in turn.session_updates(&event) {
                        send(&json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session_id, "update": update}}));
                    }
                }
            };
            let mut result = agent.prompt(&text, &mut on_event);
            let planned = agent.plan.lock().unwrap().clone();
            if let (Ok(_), Plan::Ready(plan)) = (&result, planned)
                && agent.asks
                && !cancelled.load(Ordering::Relaxed)
            {
                let implement = approve_plan(&replies.lock().unwrap(), session_id, &plan);
                *agent.plan.lock().unwrap() = if implement { Plan::Off } else { Plan::Drafting };
                agent.save_plan().map_err(|error| (INTERNAL_ERROR, error))?;
                if implement {
                    result = agent.prompt(&format!("Implement this plan:\n\n{plan}"), &mut on_event);
                }
            }
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
/// Login is the TUI's /login in the editor's terminal, for a client that can run one.
fn initialize_result(client: &Value) -> Value {
    let login = json!({"id": "login", "name": "Log in", "description": "Opens /login in a terminal.", "type": "terminal", "args": ["--login"]});
    let auth_methods = if client["auth"]["terminal"] == true { vec![login] } else { Vec::new() };
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "agentCapabilities": {
            "loadSession": true,
            "promptCapabilities": {"image": true, "audio": false, "embeddedContext": true},
            "mcpCapabilities": {"http": true, "sse": false},
            "sessionCapabilities": {"additionalDirectories": {}, "list": {}, "resume": {}, "close": {}},
        },
        "agentInfo": {"name": "agent", "title": "agent", "version": env!("CARGO_PKG_VERSION")},
        "authMethods": auth_methods,
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

fn mode_by_id(asked: &str) -> Result<Mode, (i64, String)> {
    MODES.into_iter().find(|mode| mode_id(*mode) == asked).ok_or((INVALID_PARAMS, format!("Unknown mode {asked}.")))
}

/// The permission mode (again, as Zed ignores `modes` next to config options), the model from the models an account
/// lists, and the effort.
fn config_options(agent: &Agent) -> Vec<Value> {
    let catalog = agent.client.catalog();
    let models: Vec<Value> =
        catalog.iter().filter(|model| usable(model) || model.id == agent.model).map(|model| json!({"value": model.id, "name": model.name, "description": model.provider})).collect();
    let efforts: Vec<Value> = THINKING_LEVELS.iter().map(|level| json!({"value": level, "name": level})).collect();
    let modes: Vec<Value> = MODES.iter().map(|mode| json!({"value": mode_id(*mode), "name": mode.label(), "description": mode.description()})).collect();
    vec![
        json!({"id": "mode", "name": "Mode", "category": "mode", "type": "select", "currentValue": mode_id(*agent.permission_mode.lock().unwrap()), "options": modes}),
        json!({"id": "model", "name": "Model", "category": "model", "type": "select", "currentValue": agent.model, "options": models}),
        json!({"id": "effort", "name": "Effort", "category": "thought_level", "type": "select", "currentValue": agent.effort, "options": efforts}),
    ]
}

/// A model an account lists, or one of a custom provider, which needs no account.
fn usable(model: &Model) -> bool {
    !model.accounts.is_empty() || !PROVIDERS.contains(&model.provider.as_str())
}

/// The client's fs and terminal methods: Read, Write and the edit tools see its buffers, unsaved edits included, when
/// it offers both fs methods, and foreground Bash runs in its terminal when it offers one.
struct EditorClient {
    session_id: String,
    replies: Arc<Mutex<Receiver<Value>>>,
    next_id: AtomicU64,
    /// The directories whose files the editor reads and writes; empty when it offers no fs methods.
    roots: Vec<PathBuf>,
    /// The Bash calls that ran in a terminal, whose card keeps showing it.
    terminals: Mutex<HashSet<String>>,
}

impl EditorClient {
    fn send_request(&self, method: &str, params: Value) -> String {
        // The pid too: the router keys the editor's replies by id across every session's process.
        let id = format!("editor-{}-{}", std::process::id(), self.next_id.fetch_add(1, Ordering::Relaxed));
        send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        id
    }

    fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.send_request(method, params);
        let reply = wait_for_reply(&self.replies.lock().unwrap(), &id).ok_or("The editor closed the connection.")?;
        result_of(method, &reply)
    }
}

fn result_of(method: &str, reply: &Value) -> Result<Value, String> {
    match reply.get("error") {
        Some(error) => Err(format!("The editor answered {method} with: {}", error["message"].as_str().unwrap_or_default())),
        None => Ok(reply["result"].clone()),
    }
}

impl EditorFiles for EditorClient {
    fn serves(&self, path: &Path) -> bool {
        self.roots.iter().any(|root| path.starts_with(root))
    }

    fn read(&self, path: &Path) -> Result<String, String> {
        let result = self.request("fs/read_text_file", json!({"sessionId": self.session_id, "path": path}))?;
        result["content"].as_str().map(String::from).ok_or_else(|| format!("The editor sent no text for {}.", path.display()))
    }

    fn write(&self, path: &Path, text: &str) -> Result<(), String> {
        self.request("fs/write_text_file", json!({"sessionId": self.session_id, "path": path, "content": text})).map(|_| ())
    }
}

/// What the editor's terminal keeps of a command's output; bash's own capture spills past its limit the same way.
const TERMINAL_OUTPUT_BYTES: u64 = 4 << 20;

impl EditorTerminal for EditorClient {
    fn run(&self, call: &str, script: &str, cwd: &Path, abort: &AtomicBool, deadline: Option<Instant>) -> Result<(String, i32, Option<Killed>), String> {
        let create = json!({"sessionId": self.session_id, "command": "bash", "args": ["-c", script], "cwd": cwd, "outputByteLimit": TERMINAL_OUTPUT_BYTES});
        let created = self.request("terminal/create", create)?;
        let terminal_id = created["terminalId"].as_str().ok_or("The editor sent no terminal id.")?.to_string();
        self.terminals.lock().unwrap().insert(call.to_string());
        let shown = json!({"sessionUpdate": "tool_call_update", "toolCallId": call, "content": [{"type": "terminal", "terminalId": terminal_id}]});
        send(&json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": self.session_id, "update": shown}}));
        let terminal = json!({"sessionId": self.session_id, "terminalId": terminal_id});
        let waiting = self.send_request("terminal/wait_for_exit", terminal.clone());
        let mut killed = None;
        let exited = loop {
            match self.replies.lock().unwrap().recv_timeout(Duration::from_millis(10)) {
                Ok(reply) if reply["id"] == waiting.as_str() => break reply,
                // The answer to our terminal/kill, which nothing waits for.
                Ok(_) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Err("The editor closed the connection.".into()),
            }
            let kill = if abort.load(Ordering::Relaxed) { Some(Killed::Aborted) } else { deadline.filter(|deadline| Instant::now() >= *deadline).map(|_| Killed::Timeout) };
            if killed.is_none() && kill.is_some() {
                killed = kill;
                self.send_request("terminal/kill", terminal.clone());
            }
        };
        let status = result_of("terminal/wait_for_exit", &exited)?;
        let output = self.request("terminal/output", terminal.clone())?;
        self.request("terminal/release", terminal)?;
        // A signal ended it: 128 like a shell, as the editor sends the signal's name and not its number.
        let code = status["exitCode"].as_i64().map_or(128, |code| code as i32);
        Ok((output["output"].as_str().unwrap_or_default().to_string(), code, killed))
    }
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

/// Sends a loaded session's history as the updates its turns sent, before the answer to session/load.
fn replay(messages: &[Value], agent: &Agent, session_id: &str) {
    let mut turn = Turn { cwd: agent.cwd.clone(), todos: agent.todos.clone(), ..Turn::default() };
    let update = |update: Value| send(&json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session_id, "update": update}}));
    for message in messages {
        let user = message["role"] == "user";
        let blocks = match &message["content"] {
            Value::String(text) => vec![json!({"type": "text", "text": text})],
            content => content.as_array().cloned().unwrap_or_default(),
        };
        for block in blocks {
            let text = |key: &str| block[key].as_str().unwrap_or_default().to_string();
            let event = match block["type"].as_str() {
                Some("text") if user => {
                    update(json!({"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": block["text"]}}));
                    continue;
                }
                Some("text") => Event::Text(text("text")),
                Some("thinking") => Event::Thinking(text("thinking")),
                Some("tool_use") => Event::ToolStart { id: text("id"), name: text("name"), input: block["input"].clone() },
                Some("tool_result") => Event::ToolEnd { id: text("tool_use_id"), output: result_text(&block["content"]), is_error: block["is_error"] == true },
                _ => continue,
            };
            turn.session_updates(&event).into_iter().for_each(update);
            // A diff needs the file as it was before the edit, which is gone by now.
            turn.before_edit.clear();
        }
    }
}

/// A tool result's text, whether the API got a string or blocks.
fn result_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        blocks => blocks.as_array().into_iter().flatten().filter_map(|block| block["text"].as_str()).collect::<Vec<_>>().join("\n"),
    }
}

/// The client's answer to our request `id`; a cancelled request or a closed stdin denies.
fn wait_for_decision(replies: &Receiver<Value>, id: &str) -> Decision {
    let reply = wait_for_reply(replies, id).unwrap_or_default();
    match reply["result"]["outcome"]["optionId"].as_str() {
        Some("allow_once") => Decision::Allow,
        Some("allow_always") => Decision::AllowAlways,
        _ => Decision::Deny,
    }
}

/// The client's reply to our request `id`, None once stdin closes. Replies to earlier requests are dropped.
fn wait_for_reply(replies: &Receiver<Value>, id: &str) -> Option<Value> {
    replies.iter().find(|reply| reply["id"] == id)
}

/// Asks the editor to fill in a form; the content, or None when the user declines or cancels.
fn elicit(replies: &Receiver<Value>, session_id: &str, message: &str, schema: Value) -> Option<Value> {
    // The pid too: the router keys the editor's replies by id across every session's process.
    let id = format!("elicitation-{}-{}", std::process::id(), crate::process::now_ms());
    let params = json!({"sessionId": session_id, "mode": "form", "message": message, "requestedSchema": schema});
    send(&json!({"jsonrpc": "2.0", "id": id, "method": "elicitation/create", "params": params}));
    let reply = wait_for_reply(replies, &id)?;
    (reply["result"]["action"] == "accept").then(|| reply["result"]["content"].clone())
}

/// AskUserQuestion as a form: per question a pick of its options and a field to type an answer, like the TUI's "Type something.".
fn question_form(questions: &[Question]) -> Value {
    let mut properties = serde_json::Map::new();
    for (index, question) in questions.iter().enumerate() {
        let options: Vec<Value> = question.options.iter().map(|choice| json!({"const": choice.label, "title": choice.label, "description": choice.description})).collect();
        let pick = if question.multi {
            json!({"type": "array", "title": question.header, "description": question.question, "items": {"anyOf": options}})
        } else {
            json!({"type": "string", "title": question.header, "description": question.question, "oneOf": options})
        };
        properties.insert(format!("q{index}"), pick);
        properties.insert(format!("q{index}_typed"), json!({"type": "string", "title": ask::TYPE_SOMETHING}));
    }
    json!({"type": "object", "properties": properties})
}

/// The filled-in `question_form` as answers; None for a question left empty, and a typed answer over a pick.
fn form_answers(questions: &[Question], content: &Value) -> Vec<Option<Answer>> {
    let mut answers = Vec::new();
    for (index, question) in questions.iter().enumerate() {
        let typed = content[format!("q{index}_typed")].as_str().unwrap_or_default().trim().to_string();
        let position = |label: &Value| question.options.iter().position(|choice| Some(choice.label.as_str()) == label.as_str());
        let picked = &content[format!("q{index}")];
        answers.push(if question.multi {
            let picked: Vec<usize> = picked.as_array().into_iter().flatten().filter_map(position).collect();
            (!picked.is_empty() || !typed.is_empty()).then_some(Answer::Multi { picked, typed })
        } else if !typed.is_empty() {
            Some(Answer::Typed(typed))
        } else {
            position(picked).map(Answer::Picked)
        });
    }
    answers
}

/// ExitPlanMode leaves a plan for the user to approve, which the TUI does in a panel. True to implement it now.
fn approve_plan(replies: &Receiver<Value>, session_id: &str, plan: &str) -> bool {
    let next = json!({"type": "string", "title": "Next", "oneOf": [{"const": "implement", "title": "Implement the plan"}, {"const": "keep", "title": "Keep planning"}]});
    let schema = json!({"type": "object", "properties": {"next": next}, "required": ["next"]});
    elicit(replies, session_id, &format!("The plan is ready.\n\n{plan}"), schema).is_some_and(|content| content["next"] == "implement")
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
    /// The editor's buffers, which edits go to when the client offers fs, and the terminals Bash ran in.
    editor: Option<Arc<EditorClient>>,
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
                    self.before_edit.insert(id.clone(), (path.clone(), self.read_text(path)));
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
                // A Bash terminal is released by now and Zed rejects an update naming it; with no content the card keeps it.
                let in_terminal = self.editor.as_ref().is_some_and(|editor| editor.terminals.lock().unwrap().remove(id));
                let mut content = if in_terminal { Vec::new() } else { vec![json!({"type": "content", "content": {"type": "text", "text": output}})] };
                if let Some((path, old_text)) = self.before_edit.remove(id)
                    && !is_error
                    && let Some(new_text) = self.read_text(&path)
                {
                    content.insert(0, json!({"type": "diff", "path": path, "oldText": old_text, "newText": new_text}));
                }
                let mut update = json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": id,
                    "status": if *is_error { "failed" } else { "completed" },
                });
                if !content.is_empty() {
                    update["content"] = content.into();
                }
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

    fn read_text(&self, path: &Path) -> Option<String> {
        match &self.editor {
            Some(editor) if editor.serves(path) => editor.read(path).ok(),
            // Before the tool's own checks: /dev/zero or a FIFO never ends.
            _ => std::fs::metadata(path).is_ok_and(|meta| meta.is_file()).then(|| std::fs::read_to_string(path).ok()).flatten(),
        }
    }

    /// The tool call a permission request is about, titled with the question: the started call, or a new card for an
    /// edit hook's command, which no tool call runs.
    fn permission_tool_call(&self, call: Option<&str>, request: &Request) -> Value {
        let title = format!("{} {}", request.question, request.detail);
        let Some(mut tool_call) = call.and_then(|id| self.started.get(id)).cloned() else {
            let id = format!("edit-hook-{}-{}", std::process::id(), crate::process::now_ms());
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
    fn an_edit_of_a_fifo_gets_no_diff_and_does_not_block() {
        let fifo = std::env::temp_dir().join(format!("agent-acp-fifo-{}", std::process::id()));
        let _ = std::fs::remove_file(&fifo);
        assert!(crate::process::command("mkfifo").arg(&fifo).status().unwrap().success());
        let (sent, read) = mpsc::channel();
        let path = fifo.clone();
        std::thread::spawn(move || sent.send(Turn::default().read_text(&path)).unwrap());
        let text = read.recv_timeout(Duration::from_secs(5));
        std::fs::remove_file(&fifo).unwrap();
        assert_eq!(text, Ok(None));
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
    fn form_answers_read_picks_typed_text_and_multi_select() {
        let questions = ask::parse(&json!({"questions": [
            {"question": "One?", "header": "One", "options": [{"label": "A"}, {"label": "B"}]},
            {"question": "Many?", "header": "Many", "multiSelect": true, "options": [{"label": "X"}, {"label": "Y"}]},
            {"question": "Own?", "header": "Own", "options": [{"label": "C"}, {"label": "D"}]},
            {"question": "Skip?", "header": "Skip", "options": [{"label": "E"}, {"label": "F"}]},
        ]}))
        .unwrap();
        let form = question_form(&questions);
        assert_eq!(form["properties"]["q1"]["items"]["anyOf"][1]["const"], "Y");

        let answers = form_answers(&questions, &json!({"q0": "B", "q1": ["X", "Y"], "q1_typed": "Z", "q2": "C", "q2_typed": "mine"}));

        assert_eq!(answers, [Some(Answer::Picked(1)), Some(Answer::Multi { picked: vec![0, 1], typed: "Z".into() }), Some(Answer::Typed("mine".into())), None]);
        assert!(ask::response(&questions, &answers).contains("\"Many?\"=\"X, Y, Z\""));
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
