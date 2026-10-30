//! MCP servers from `mcp.json` and an ACP client, their tools offered to the model as `mcp__<server>__<tool>`.

mod client;
mod config;
mod oauth;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

pub use client::CONNECT_TIMEOUT;
use client::{Client, Launch};
use config::Server;

#[derive(Clone)]
pub struct Mcp {
    home: PathBuf,
    cwd: PathBuf,
    state: Arc<Mutex<State>>,
}

/// An `agent acp` session passes its editor's MCP servers to its subagents here, as the JSON list of session/new.
/// `process::command` drops it from every other child, since headers and env values can hold secrets.
pub const CLIENT_SERVERS_ENV: &str = "AGENT_ACP_MCP_SERVERS";

#[derive(Default)]
struct State {
    servers: Vec<Entry>,
    /// The servers an ACP client passed with session/new; `sync` runs them next to `mcp.json`'s, over one of the same name.
    client: Vec<Server>,
    /// `client` as the client sent it, which subagents get in CLIENT_SERVERS_ENV.
    client_list: Value,
    /// Why `mcp.json` could not be read, shown in /mcp.
    error: Option<String>,
    next_run: u64,
    on_change: Option<Arc<dyn Fn() + Send + Sync>>,
}

struct Entry {
    server: Server,
    approved: bool,
    /// Which connection attempt owns `connection`; 0 when none, so late answers of a replaced one are dropped.
    run: u64,
    connection: Connection,
}

enum Connection {
    Off,
    Connecting,
    LoggingIn,
    Ready { client: Arc<Client>, tools: Vec<Tool> },
    Failed(String),
}

struct Tool {
    /// The server's own name for it; `schema["name"]` holds the name the model sees.
    name: String,
    schema: Value,
}

/// One row of /mcp.
pub struct View {
    pub name: String,
    /// The command line or the URL.
    pub target: String,
    pub status: Status,
    /// Whether the server is reached over HTTP, so it can take an OAuth login.
    pub http: bool,
    pub logged_in: bool,
    /// False when `mcp.json` sets `"enabled": false`.
    pub enabled: bool,
    /// The key to add to `settings.approved_mcp` before this project server may run.
    pub approval: Option<String>,
    /// The merged entry, launch fields first, before `${VAR}` expansion: what the user approves.
    pub entry: String,
}

pub enum Status {
    Off,
    Connecting,
    LoggingIn,
    Ready { tools: usize },
    Failed(String),
}

impl Mcp {
    /// No servers until the first `sync`.
    pub fn new(home: &Path, cwd: &Path) -> Mcp {
        Mcp { home: home.to_path_buf(), cwd: cwd.to_path_buf(), state: Arc::default() }
    }

    /// Called from a connection's thread whenever a server's status changes.
    pub fn on_change(&self, callback: impl Fn() + Send + Sync + 'static) {
        self.state.lock().unwrap().on_change = Some(Arc::new(callback));
    }

    /// Rereads `mcp.json`: connects enabled servers not in `disabled` whose project entry, if any, is in
    /// `approved`, stops the rest, and keeps connections whose command or URL did not change.
    pub fn sync(&self, disabled: &[String], approved: &[String]) {
        let (mut servers, error) = config::load(&self.home, &self.cwd);
        let mut state = self.state.lock().unwrap();
        state.error = error;
        servers.retain(|server| !state.client.iter().any(|client| client.name == server.name));
        servers.extend(state.client.iter().cloned());
        let mut previous = std::mem::take(&mut state.servers);
        for server in servers {
            let is_approved = server.approval.as_ref().is_none_or(|key| approved.contains(key));
            let wanted = server.enabled && is_approved && !disabled.contains(&server.name);
            let kept = previous.iter().position(|entry| entry.server.name == server.name && entry.server.launch == server.launch);
            let mut entry = match kept {
                Some(index) => previous.remove(index),
                None => Entry { server: server.clone(), approved: false, run: 0, connection: Connection::Off },
            };
            entry.server = server;
            entry.approved = is_approved;
            let running = !matches!(entry.connection, Connection::Off);
            if !wanted {
                (entry.run, entry.connection) = (0, Connection::Off);
            }
            state.servers.push(entry);
            if wanted && !running {
                let index = state.servers.len() - 1;
                self.connect(&mut state, index);
            }
        }
    }

    /// Replaces the servers an ACP client passed before with `list`, the `mcpServers` of its session/new, and syncs.
    pub fn set_client_servers(&self, list: &Value, disabled: &[String], approved: &[String]) -> Result<(), String> {
        let client = config::client_servers(list, &self.cwd)?;
        let mut state = self.state.lock().unwrap();
        state.client = client;
        state.client_list = list.clone();
        drop(state);
        self.sync(disabled, approved);
        Ok(())
    }

    /// The ACP client's servers in its own shape, as its session/new sent them; null when no client passed a list.
    pub fn client_list(&self) -> Value {
        self.state.lock().unwrap().client_list.clone()
    }

    /// Waits until no server is still connecting or `limit` passes, so a run without a UI starts with every tool.
    /// A server still connecting after it offers its tools once it connects.
    pub fn settled(&self, limit: Duration) {
        let start = Instant::now();
        let connecting = || self.state.lock().unwrap().servers.iter().any(|entry| matches!(entry.connection, Connection::Connecting));
        while connecting() && start.elapsed() < limit {
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Drops the server's connection and starts a new one, unless the server is off.
    pub fn reconnect(&self, name: &str) {
        let mut state = self.state.lock().unwrap();
        let Some(index) = state.servers.iter().position(|entry| entry.server.name == name) else { return };
        if !matches!(state.servers[index].connection, Connection::Off) {
            self.connect(&mut state, index);
        }
    }

    /// Opens the browser to log in to an enabled HTTP server, then reconnects with the new token.
    pub fn login(&self, name: &str) {
        let mut state = self.state.lock().unwrap();
        state.next_run += 1;
        let run = state.next_run;
        let Some(entry) = state.servers.iter_mut().find(|entry| entry.server.name == name) else { return };
        let Launch::Http { url, .. } = &entry.server.launch else { return };
        // The first login waits up to its timeout for the browser; a second one would open another tab.
        if matches!(entry.connection, Connection::Off | Connection::LoggingIn) {
            return;
        }
        let url = url.clone();
        (entry.run, entry.connection) = (run, Connection::LoggingIn);
        let (mcp, name) = (self.clone(), name.to_string());
        drop(state);
        self.changed();
        std::thread::spawn(move || match oauth::login(&mcp.home, &url, &AtomicBool::new(false)) {
            Ok(()) => mcp.reconnect(&name),
            Err(error) => mcp.finish(&name, run, Connection::Failed(error)),
        });
    }

    /// Tool schemas of the ready servers. Sanitized or cut names can clash; the first one wins.
    pub fn tools(&self) -> Vec<Value> {
        let state = self.state.lock().unwrap();
        let mut seen = HashSet::new();
        let mut tools = Vec::new();
        for entry in &state.servers {
            let Connection::Ready { tools: offered, .. } = &entry.connection else { continue };
            for tool in offered {
                if seen.insert(tool.schema["name"].clone()) {
                    tools.push(tool.schema.clone());
                }
            }
        }
        tools
    }

    /// Runs a tool: None when `name` is no ready server's tool, else the text and whether it is an error.
    pub fn tool(&self, name: &str, input: &Value, abort: &AtomicBool) -> Option<(String, bool)> {
        let (client, tool) = {
            let state = self.state.lock().unwrap();
            state.servers.iter().find_map(|entry| {
                let Connection::Ready { client, tools } = &entry.connection else { return None };
                let tool = tools.iter().find(|tool| tool.schema["name"] == name)?;
                Some((client.clone(), tool.name.clone()))
            })?
        };
        Some(client.call(&tool, input, abort))
    }

    /// The servers for /mcp, and why `mcp.json` could not be read.
    pub fn list(&self) -> (Vec<View>, Option<String>) {
        // Read before the lock: connection threads wait on it, and the panel calls this every frame.
        // A broken store shows as logged out; `bearer` reports it.
        let tokens = oauth::read(&self.home).unwrap_or_default();
        let state = self.state.lock().unwrap();
        let views = state.servers.iter().map(|entry| {
            let approval = entry.server.approval.clone().filter(|_| !entry.approved);
            let (target, http, logged_in) = match &entry.server.launch {
                // The command only: args may hold an expanded `${VAR}` secret.
                Launch::Stdio { command, .. } => (command.clone(), false, false),
                // The origin only: a `${VAR}` secret expanded into the path or query stays off screen.
                Launch::Http { url, .. } => (oauth::origin(url).to_string(), true, oauth::logged_in(&tokens, url)),
            };
            let status = match &entry.connection {
                Connection::Off => Status::Off,
                Connection::Connecting => Status::Connecting,
                Connection::LoggingIn => Status::LoggingIn,
                Connection::Ready { tools, .. } => Status::Ready { tools: tools.len() },
                Connection::Failed(error) => Status::Failed(error.clone()),
            };
            View { name: entry.server.name.clone(), target, status, http, logged_in, enabled: entry.server.enabled, approval, entry: entry.server.raw.clone() }
        });
        (views.collect(), state.error.clone())
    }

    /// Starts a connection on its own thread; replacing the old one stops its process.
    fn connect(&self, state: &mut State, index: usize) {
        state.next_run += 1;
        let run = state.next_run;
        let entry = &mut state.servers[index];
        (entry.run, entry.connection) = (run, Connection::Connecting);
        let (mcp, server) = (self.clone(), entry.server.clone());
        std::thread::spawn(move || {
            let (watcher, name) = (mcp.clone(), server.name.clone());
            let exited = move |line: String| {
                let error = if line.is_empty() { "Exited.".to_string() } else { format!("Exited: {line}") };
                watcher.finish(&name, run, Connection::Failed(error));
            };
            let connection = match Client::connect(&server.launch, &mcp.home, exited) {
                Ok((client, tools)) => Connection::Ready { client: Arc::new(client), tools: offered(&server.name, tools) },
                Err(error) => Connection::Failed(error),
            };
            mcp.finish(&server.name, run, connection);
        });
    }

    /// Records the outcome of attempt `run`. A failure the process already reported by exiting is kept,
    /// since its stderr line says more than the missing answer.
    fn finish(&self, name: &str, run: u64, connection: Connection) {
        let mut state = self.state.lock().unwrap();
        let Some(entry) = state.servers.iter_mut().find(|entry| entry.server.name == name && entry.run == run) else { return };
        if matches!((&entry.connection, &connection), (Connection::Failed(_), Connection::Failed(_)) | (Connection::Failed(_), Connection::Ready { .. })) {
            return;
        }
        entry.connection = connection;
        drop(state);
        self.changed();
    }

    fn changed(&self) {
        let on_change = self.state.lock().unwrap().on_change.clone();
        if let Some(on_change) = on_change {
            on_change();
        }
    }
}

fn offered(server: &str, tools: Vec<Value>) -> Vec<Tool> {
    let mut offered = Vec::new();
    for tool in tools {
        let Some(name) = tool["name"].as_str() else { continue };
        // Anthropic's BM25 tool search does not split `mcp__zed-echo__echo`, so a search for "echo" misses it without these words.
        let description = format!("MCP server {server}, tool {name}. {}", tool["description"].as_str().unwrap_or_default());
        let schema = json!({
            "name": tool_name(server, name),
            "description": description.trim_end(),
            "input_schema": if tool["inputSchema"].is_object() { tool["inputSchema"].clone() } else { json!({"type": "object", "properties": {}}) },
        });
        offered.push(Tool { name: name.to_string(), schema });
    }
    offered
}

/// Model APIs take `[A-Za-z0-9_-]{1,64}` as tool names.
fn tool_name(server: &str, tool: &str) -> String {
    let name = format!("mcp__{server}__{tool}");
    name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).take(64).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    /// Answers initialize, lists `echo` on a first page and `fail` on a second, and runs both.
    const FAKE_SERVER: &str = r#"
import json, sys
for line in sys.stdin:
    m = json.loads(line)
    if "id" not in m:
        continue
    p = m.get("params", {})
    if m["method"] == "initialize":
        r = {"protocolVersion": p["protocolVersion"], "capabilities": {"tools": {}}, "serverInfo": {"name": "fake", "version": "1"}}
    elif m["method"] == "tools/list" and "cursor" not in p:
        r = {"tools": [{"name": "echo", "description": "Echoes", "inputSchema": {"type": "object"}}], "nextCursor": "2"}
    elif m["method"] == "tools/list":
        r = {"tools": [{"name": "fail"}]}
    elif p["name"] == "echo":
        r = {"content": [{"type": "text", "text": p["arguments"]["text"]}]}
    else:
        r = {"content": [{"type": "text", "text": "boom"}], "isError": True}
    print(json.dumps({"jsonrpc": "2.0", "id": m["id"], "result": r}), flush=True)
"#;

    #[test]
    fn tools_of_a_stdio_server_are_offered_and_run() {
        let home = std::env::temp_dir().join(format!("agent-mcp-{}", std::process::id()));
        std::fs::create_dir_all(home.join(".agents")).unwrap();
        let config = json!({"mcpServers": {"fake": {"command": "python3", "args": ["-c", FAKE_SERVER]}, "off": {"command": "false", "enabled": false}}});
        std::fs::write(home.join(".agents/mcp.json"), config.to_string()).unwrap();
        let mcp = Mcp::new(&home, &home);
        let (changed, change) = mpsc::channel();
        mcp.on_change(move || {
            let _ = changed.send(());
        });
        mcp.sync(&[], &[]);
        mcp.settled(CONNECT_TIMEOUT);
        assert!(change.try_recv().is_ok());

        let names: Vec<_> = mcp.tools().iter().map(|tool| tool["name"].clone()).collect();
        assert_eq!(names, ["mcp__fake__echo", "mcp__fake__fail"]);
        assert_eq!(mcp.tools()[1]["input_schema"], json!({"type": "object", "properties": {}}));
        assert_eq!((mcp.tools()[0]["description"].as_str(), mcp.tools()[1]["description"].as_str()), (Some("MCP server fake, tool echo. Echoes"), Some("MCP server fake, tool fail.")));
        let never = AtomicBool::new(false);
        assert_eq!(mcp.tool("mcp__fake__echo", &json!({"text": "hi"}), &never), Some(("hi".into(), false)));
        assert_eq!(mcp.tool("mcp__fake__fail", &json!({}), &never), Some(("boom".into(), true)));
        assert_eq!(mcp.tool("mcp__off__x", &json!({}), &never), None);
        let (views, error) = mcp.list();
        assert!(error.is_none());
        assert!(matches!(views[0].status, Status::Ready { tools: 2 }));
        assert!(matches!(views[1].status, Status::Off));

        mcp.sync(&["fake".into()], &[]);
        let (views, _) = mcp.list();
        std::fs::remove_dir_all(&home).unwrap();
        assert!(mcp.tools().is_empty());
        assert!(matches!(views[0].status, Status::Off));
    }

    #[test]
    fn a_project_server_runs_only_once_approved() {
        let root = std::env::temp_dir().join(format!("agent-mcp-project-{}", std::process::id()));
        let (home, cwd) = (root.join("home"), root.join("repo"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        let config = json!({"mcpServers": {"fake": {"command": "python3", "args": ["-c", FAKE_SERVER]}}});
        std::fs::write(cwd.join(".mcp.json"), config.to_string()).unwrap();
        let mcp = Mcp::new(&home, &cwd);
        let (changed, change) = mpsc::channel();
        mcp.on_change(move || {
            let _ = changed.send(());
        });

        mcp.sync(&[], &[]);
        let (views, _) = mcp.list();
        assert!(matches!(views[0].status, Status::Off));
        let key = views[0].approval.clone().unwrap();
        assert_eq!(serde_json::from_str::<Value>(&views[0].entry).unwrap(), config["mcpServers"]["fake"]);

        mcp.sync(&[], &[key]);
        change.recv_timeout(Duration::from_secs(10)).unwrap();
        let (views, _) = mcp.list();
        std::fs::remove_dir_all(&root).unwrap();
        assert!(matches!(views[0].status, Status::Ready { tools: 2 }));
        assert_eq!(views[0].approval, None);
    }

    #[test]
    fn an_acp_client_server_runs_over_the_same_named_mcp_json_one_until_the_client_drops_it() {
        let home = std::env::temp_dir().join(format!("agent-mcp-client-{}", std::process::id()));
        std::fs::create_dir_all(home.join(".agents")).unwrap();
        std::fs::write(home.join(".agents/mcp.json"), json!({"mcpServers": {"fake": {"command": "false"}}}).to_string()).unwrap();
        let mcp = Mcp::new(&home, &home);
        mcp.sync(&[], &[]);

        mcp.set_client_servers(&json!([{"name": "fake", "command": "python3", "args": ["-c", FAKE_SERVER], "env": []}]), &[], &[]).unwrap();
        mcp.settled(CONNECT_TIMEOUT);
        let names: Vec<_> = mcp.tools().iter().map(|tool| tool["name"].clone()).collect();
        let (views, _) = mcp.list();
        mcp.set_client_servers(&json!([]), &[], &[]).unwrap();
        let (views_after, _) = mcp.list();
        std::fs::remove_dir_all(&home).unwrap();

        assert_eq!(names, ["mcp__fake__echo", "mcp__fake__fail"]);
        let targets = |views: &[View]| views.iter().map(|view| view.target.clone()).collect::<Vec<_>>();
        assert_eq!(targets(&views), ["python3"]);
        assert_eq!(targets(&views_after), ["false"]);
    }

    #[test]
    fn tool_names_are_sanitized_and_cut_to_64() {
        assert_eq!(tool_name("my server", "get.page"), "mcp__my_server__get_page");
        assert_eq!(tool_name("s", &"x".repeat(80)).len(), 64);
    }
}
