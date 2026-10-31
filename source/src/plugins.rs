//! Plugins are long-lived processes in ~/.agents/plugins/<name>/ that speak JSON-RPC 2.0 over stdio,
//! one message per line. The protocol is described in this repo's `plugins/README.md`.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// How long a prompt waits for each plugin's `input` answer before it goes through unchanged.
const INPUT_TIMEOUT: Duration = Duration::from_secs(5);

/// The repo's `plugins/` directories, built in so `agent plugin install <name>` works offline.
const BUNDLED: &[(&str, &[(&str, &str)])] =
    &[("usagefleet", &[("plugin.json", include_str!("../../plugins/usagefleet/plugin.json")), ("index.ts", include_str!("../../plugins/usagefleet/index.ts"))])];

/// In an installed plugin's directory: the source directory `agent plugin update` copies it from again.
const SOURCE_FILE: &str = ".installed-from";

/// The running plugins. Clones share them.
#[derive(Clone)]
pub struct Plugins {
    dir: PathBuf,
    state: Arc<Mutex<State>>,
}

#[derive(Default)]
struct State {
    /// Installed plugins, sorted by name.
    plugins: Vec<Plugin>,
    /// Open `input` requests by id, with the plugin that owes the answer.
    pending: HashMap<u64, (String, Sender<Value>)>,
    next_id: u64,
    on_change: Option<Arc<dyn Fn() + Send + Sync>>,
    /// The last `ui.showPanel` not yet taken by the UI: plugin name and params (null closes).
    panel: Option<(String, Value)>,
}

struct Plugin {
    name: String,
    process: Option<Process>,
    /// Slash commands from the manifest the process was started with.
    commands: Vec<(String, String)>,
    /// Footer text set with `ui.setStatus`, as (text, tone) parts.
    status: Option<Vec<(String, Option<String>)>>,
    /// Text for the `/account` panel by provider account id, set with `ui.setAccountLabels`.
    account_labels: HashMap<String, String>,
    /// Why the plugin is not running: a bad manifest, a failed start, or how it exited.
    error: Option<String>,
    /// Tools from the manifest, offered to the model as `<plugin>__<name>`.
    tools: Vec<Value>,
    /// Manifest defaults overlaid with the user's values, sent as `context.settings`.
    settings: Value,
}

struct Process {
    /// Tells this process's reader thread apart from one left over by an earlier start.
    run: u64,
    child: Child,
    /// A writer thread owns stdin, so a plugin that stops reading never blocks the agent.
    stdin: Sender<String>,
}

/// One row of `agent plugin list` and /plugins.
pub struct View {
    pub name: String,
    pub version: String,
    pub description: String,
    /// GitHub login of the plugin's author.
    pub author: Option<String>,
    /// The manifest's `settings` declarations: `key`, `description`, `default`.
    pub settings: Vec<Value>,
    pub installed: bool,
    pub running: bool,
    pub error: Option<String>,
}

impl Plugins {
    /// The plugins installed in `home`, none started yet; `sync` starts them.
    pub fn new(home: &Path) -> Plugins {
        Plugins { dir: home.join(".agents/plugins"), state: Arc::default() }
    }

    /// Called after a status changes or a plugin exits, from a plugin's reader thread.
    pub fn on_change(&self, callback: impl Fn() + Send + Sync + 'static) {
        self.state.lock().unwrap().on_change = Some(Arc::new(callback));
    }

    /// Matches the processes to the disk: starts installed plugins not in `disabled`, stops the rest.
    /// `values` holds the user's settings per plugin, `{ "<plugin>": { "<key>": value } }`.
    pub fn sync(&self, disabled: &[String], values: &Value) {
        let installed = installed(&self.dir);
        let mut state = self.state.lock().unwrap();
        for index in 0..state.plugins.len() {
            let plugin = &mut state.plugins[index];
            if !installed.contains(&plugin.name) || disabled.contains(&plugin.name) {
                if let Some(mut process) = plugin.process.take() {
                    // The plugin leads its own process group, so `sh plugin.sh` starting `bun` stops as a whole.
                    crate::process::signal_group(process.child.id(), crate::process::SIGKILL);
                    let _ = process.child.wait();
                }
                // `exited` skips a stopped plugin, so its waiting calls would never get Disconnected.
                clear_run(&mut state, index);
            }
        }
        state.plugins.retain(|plugin| installed.contains(&plugin.name));
        for name in installed {
            state.next_id += 1;
            let run = state.next_id;
            let index = match state.plugins.iter().position(|plugin| plugin.name == name) {
                Some(index) => index,
                None => {
                    state.plugins.push(Plugin {
                        name: name.clone(),
                        process: None,
                        commands: Vec::new(),
                        status: None,
                        account_labels: HashMap::new(),
                        error: None,
                        tools: Vec::new(),
                        settings: json!({}),
                    });
                    state.plugins.len() - 1
                }
            };
            let plugin = &mut state.plugins[index];
            let dir = self.dir.join(&name);
            let manifest = read_manifest(&dir);
            if let Ok(manifest) = &manifest {
                plugin.settings = settings_for(&manifest.settings, &values[&name]);
            }
            if plugin.process.is_some() || disabled.contains(&name) {
                continue;
            }
            // The API takes tool names of at most 64 characters; one too long fails every model request.
            let manifest = manifest.and_then(|manifest| match manifest.tools.iter().filter_map(|tool| tool["name"].as_str()).find(|tool| name.len() + 2 + tool.len() > 64) {
                Some(tool) => Err(format!("tool `{name}__{tool}` is longer than the 64 characters the API takes.")),
                None => Ok(manifest),
            });
            match manifest.and_then(|manifest| Ok((self.spawn(&name, &dir, &manifest.command, run)?, manifest))) {
                Ok((process, manifest)) => (plugin.process, plugin.commands, plugin.tools, plugin.error) = (Some(process), manifest.commands, manifest.tools, None),
                Err(error) => plugin.error = Some(error),
            }
        }
        state.plugins.sort_by(|a, b| a.name.cmp(&b.name));
    }

    /// Sends an `event` notification to every running plugin; `params` carries its `type`.
    pub fn event(&self, params: Value) {
        for plugin in &self.state.lock().unwrap().plugins {
            if let Some(process) = &plugin.process {
                let _ = process.stdin.send(message(plugin, None, "event", &params));
            }
        }
    }

    /// Sends an `event` notification to one running plugin, e.g. `panel_select` from its panel.
    pub fn event_to(&self, name: &str, params: Value) {
        for plugin in &self.state.lock().unwrap().plugins {
            if let (true, Some(process)) = (plugin.name == name, &plugin.process) {
                let _ = process.stdin.send(message(plugin, None, "event", &params));
            }
        }
    }

    /// Tool schemas of the running plugins, named `<plugin>__<tool>`.
    pub fn tools(&self) -> Vec<Value> {
        let state = self.state.lock().unwrap();
        let running = state.plugins.iter().filter(|plugin| plugin.process.is_some());
        running
            .flat_map(|plugin| {
                plugin.tools.iter().map(|tool| {
                    let mut tool = tool.clone();
                    tool["name"] = json!(format!("{}__{}", plugin.name, tool["name"].as_str().unwrap_or_default()));
                    tool
                })
            })
            .collect()
    }

    /// Runs a plugin tool: None when `name` is no running plugin's tool, else the text and
    /// whether it is an error. Waits for the answer as long as it takes, until `abort` is set.
    pub fn tool(&self, name: &str, input: &Value, context: &Value, abort: &AtomicBool) -> Option<(String, bool)> {
        let (owner, tool) = name.split_once("__")?;
        let offered = self.state.lock().unwrap().plugins.iter().any(|plugin| plugin.name == owner && plugin.process.is_some() && plugin.tools.iter().any(|t| t["name"] == tool));
        if !offered {
            return None;
        }
        let params = json!({"name": tool, "input": input, "context": context});
        Some(match self.request_until(owner, "tool", params, None, abort) {
            None if abort.load(Ordering::Relaxed) => (crate::agent::ABORTED.into(), true),
            None => (format!("{owner} stopped before answering {name}."), true),
            Some(reply) => match reply["error"]["message"].as_str() {
                Some(message) => (message.to_string(), true),
                None => (reply["result"]["text"].as_str().unwrap_or_default().to_string(), reply["result"]["isError"] == true),
            },
        })
    }

    /// The panel a plugin last asked for with `ui.showPanel`, once: plugin name and params, null to close.
    pub fn take_panel(&self) -> Option<(String, Value)> {
        self.state.lock().unwrap().panel.take()
    }

    /// Asks each running plugin about a prompt, in name order: Ok with the text, rewritten by any
    /// `transform`, or Err with the reason of the first `block`. Silence, errors and crashes let it through.
    pub fn input(&self, text: &str, context: &Value) -> Result<String, String> {
        let mut text = text.to_string();
        for name in self.running() {
            let Some(answer) = self.request(&name, "input", json!({"text": text, "context": context})) else { continue };
            let result = &answer["result"];
            match result["action"].as_str() {
                Some("block") => return Err(result["reason"].as_str().map_or_else(|| format!("{name} blocked the prompt."), String::from)),
                Some("transform") => {
                    if let Some(rewritten) = result["text"].as_str() {
                        text = rewritten.to_string();
                    }
                }
                _ => {}
            }
        }
        Ok(text)
    }

    /// The footer lines set with `ui.setStatus`, in name order, as (text, tone) parts.
    pub fn statuses(&self) -> Vec<Vec<(String, Option<String>)>> {
        self.state.lock().unwrap().plugins.iter().filter_map(|plugin| plugin.status.clone()).collect()
    }

    /// `/account` panel labels as (plugin name, account id to text), one entry per plugin that set any.
    pub fn account_labels(&self) -> Vec<(String, HashMap<String, String>)> {
        let state = self.state.lock().unwrap();
        state.plugins.iter().filter(|plugin| !plugin.account_labels.is_empty()).map(|plugin| (plugin.name.clone(), plugin.account_labels.clone())).collect()
    }

    /// Slash commands of running plugins as (name, description), in plugin name order.
    pub fn commands(&self) -> Vec<(String, String)> {
        let state = self.state.lock().unwrap();
        state.plugins.iter().filter(|plugin| plugin.process.is_some()).flat_map(|plugin| plugin.commands.clone()).collect()
    }

    /// Runs `/name args` on the running plugin that offers it: None when none does,
    /// else the text to show or the plugin's error.
    pub fn command(&self, name: &str, args: &str, context: &Value) -> Option<Result<String, String>> {
        let owner = {
            let state = self.state.lock().unwrap();
            let plugin = state.plugins.iter().find(|plugin| plugin.process.is_some() && plugin.commands.iter().any(|(command, _)| command == name))?;
            plugin.name.clone()
        };
        let Some(answer) = self.request(&owner, "command", json!({"name": name, "args": args, "context": context})) else {
            return Some(Err(format!("{owner} did not answer /{name}.")));
        };
        if let Some(message) = answer["error"]["message"].as_str() {
            return Some(Err(message.to_string()));
        }
        Some(Ok(answer["result"]["text"].as_str().unwrap_or_default().to_string()))
    }

    /// Installed plugins as on disk, with how their processes run, then the bundled ones not installed yet.
    pub fn list(&self) -> Vec<View> {
        let state = self.state.lock().unwrap();
        let mut views = Vec::new();
        for name in installed(&self.dir) {
            let plugin = state.plugins.iter().find(|plugin| plugin.name == name);
            let (manifest, manifest_error) = match read_manifest(&self.dir.join(&name)) {
                Ok(manifest) => (Some(manifest), None),
                Err(error) => (None, Some(error)),
            };
            let running = plugin.is_some_and(|plugin| plugin.process.is_some());
            let error = plugin.and_then(|plugin| plugin.error.clone()).or(manifest_error);
            views.push(view(name, manifest, true, running, error));
        }
        for (name, files) in BUNDLED {
            if views.iter().any(|view| view.name == *name) {
                continue;
            }
            let manifest = files.iter().find(|(file, _)| *file == "plugin.json").and_then(|(_, text)| parse_manifest(text).ok());
            views.push(view(name.to_string(), manifest, false, false, None));
        }
        views
    }

    fn running(&self) -> Vec<String> {
        self.state.lock().unwrap().plugins.iter().filter(|plugin| plugin.process.is_some()).map(|plugin| plugin.name.clone()).collect()
    }

    /// The plugin's answer, or None when it is not running, exits, or misses INPUT_TIMEOUT.
    fn request(&self, name: &str, method: &str, params: Value) -> Option<Value> {
        self.request_until(name, method, params, Some(INPUT_TIMEOUT), &AtomicBool::new(false))
    }

    /// The plugin's answer, or None when it is not running, exits, misses `timeout`, or `abort` is set.
    fn request_until(&self, name: &str, method: &str, params: Value, timeout: Option<Duration>, abort: &AtomicBool) -> Option<Value> {
        let (reply, answer) = mpsc::channel();
        let id = {
            let mut state = self.state.lock().unwrap();
            state.next_id += 1;
            let id = state.next_id;
            let plugin = state.plugins.iter().find(|plugin| plugin.name == name)?;
            plugin.process.as_ref()?.stdin.send(message(plugin, Some(id), method, &params)).ok()?;
            state.pending.insert(id, (name.to_string(), reply));
            id
        };
        let start = Instant::now();
        let answer = loop {
            match answer.recv_timeout(Duration::from_millis(100)) {
                Ok(answer) => break Some(answer),
                Err(mpsc::RecvTimeoutError::Disconnected) => break None,
                Err(mpsc::RecvTimeoutError::Timeout) if abort.load(Ordering::Relaxed) || timeout.is_some_and(|t| start.elapsed() >= t) => break None,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        };
        self.state.lock().unwrap().pending.remove(&id);
        answer
    }

    fn spawn(&self, name: &str, dir: &Path, command: &[String], run: u64) -> Result<Process, String> {
        let mut child = crate::process::command(&command[0])
            .args(&command[1..])
            .current_dir(dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("Cannot start {}: {e}", command[0]))?;
        let (mut stdin, stdout, stderr) = (child.stdin.take().unwrap(), child.stdout.take().unwrap(), child.stderr.take().unwrap());
        let (lines, outbox) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            for line in outbox {
                if writeln!(stdin, "{line}").is_err() {
                    break;
                }
            }
        });
        // Raw lines, so one non-UTF-8 line neither stops a reader nor leaves a pipe undrained.
        let last_error = std::thread::spawn(move || {
            let lines = BufReader::new(stderr).split(b'\n').map_while(Result::ok);
            lines.map(|line| String::from_utf8_lossy(&line).trim().to_string()).filter(|line| !line.is_empty()).last()
        });
        let (plugins, name) = (self.clone(), name.to_string());
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).split(b'\n').map_while(Result::ok) {
                let line = String::from_utf8_lossy(&line);
                plugins.receive(&name, run, &line);
            }
            plugins.exited(&name, run, last_error.join().unwrap_or_default());
        });
        Ok(Process { run, child, stdin: lines })
    }

    fn receive(&self, name: &str, run: u64, line: &str) {
        let Ok(message) = serde_json::from_str::<Value>(line) else { return };
        let mut state = self.state.lock().unwrap();
        if let Some(id) = message["id"].as_u64()
            && (message.get("result").is_some() || message.get("error").is_some())
        {
            // Ids are sequential and easy to guess: only the plugin that was asked may answer.
            if state.pending.get(&id).is_some_and(|(owner, _)| owner == name)
                && let Some((_, reply)) = state.pending.remove(&id)
            {
                let _ = reply.send(message);
            }
            return;
        }
        let Some(plugin) = state.plugins.iter_mut().find(|plugin| plugin.name == name && plugin.process.as_ref().is_some_and(|p| p.run == run)) else { return };
        match message["method"].as_str() {
            Some(method @ ("ui.setStatus" | "ui.setAccountLabels" | "ui.showPanel")) => {
                if method == "ui.setStatus" {
                    plugin.status = status_parts(&message["params"]["text"]);
                } else if method == "ui.setAccountLabels" {
                    let labels = message["params"]["labels"].as_object().into_iter().flatten();
                    plugin.account_labels = labels.filter_map(|(id, text)| Some((id.clone(), text.as_str()?.to_string()))).collect();
                } else {
                    state.panel = Some((name.to_string(), message["params"].clone()));
                }
                let on_change = state.on_change.clone();
                drop(state);
                if let Some(on_change) = on_change {
                    on_change();
                }
            }
            _ if message.get("id").is_some() => {
                let error = json!({"jsonrpc": "2.0", "id": message["id"], "error": {"code": -32601, "message": "Method not found"}});
                let _ = plugin.process.as_ref().unwrap().stdin.send(error.to_string());
            }
            _ => {}
        }
    }

    fn exited(&self, name: &str, run: u64, last_error: Option<String>) {
        let mut state = self.state.lock().unwrap();
        let Some(index) = state.plugins.iter().position(|plugin| plugin.name == name && plugin.process.as_ref().is_some_and(|p| p.run == run)) else { return };
        let mut process = state.plugins[index].process.take().unwrap();
        clear_run(&mut state, index);
        drop(state);
        // A plugin that closed stdout can no longer answer; waiting under the lock would freeze every plugin call.
        let code = process.child.wait().map(|status| status.to_string()).unwrap_or_else(|e| e.to_string());
        let mut state = self.state.lock().unwrap();
        // Skip when the plugin was removed or started again while it was waited on.
        if let Some(plugin) = state.plugins.iter_mut().find(|plugin| plugin.name == name && plugin.process.is_none()) {
            plugin.error = Some(match last_error {
                Some(line) => format!("Exited ({code}): {line}"),
                None => format!("Exited ({code})."),
            });
        }
        let on_change = state.on_change.clone();
        drop(state);
        if let Some(on_change) = on_change {
            on_change();
        }
    }
}

/// `ui.setStatus` text: a string, or an array of `{ "text", "tone" }` parts. None when nothing would show.
fn status_parts(text: &Value) -> Option<Vec<(String, Option<String>)>> {
    let parts: Vec<_> = match text {
        Value::String(text) => vec![(text.clone(), None)],
        Value::Array(parts) => parts.iter().filter_map(|part| Some((part["text"].as_str()?.to_string(), part["tone"].as_str().map(String::from)))).collect(),
        _ => Vec::new(),
    };
    parts.iter().any(|(text, _)| !text.is_empty()).then_some(parts)
}

/// Forgets what a run left behind: its UI state and the calls still waiting on it.
fn clear_run(state: &mut State, index: usize) {
    let plugin = &mut state.plugins[index];
    plugin.status = None;
    plugin.account_labels.clear();
    let name = plugin.name.clone();
    state.pending.retain(|_, (owner, _)| *owner != name);
}

/// Copies a bundled plugin by name, or a plugin directory by path, into ~/.agents/plugins; returns its name.
/// Files already there and not in the source stay, since a plugin may keep state next to its code.
pub fn install(home: &Path, source: &str) -> Result<String, String> {
    let plugins = home.join(".agents/plugins");
    if source.contains('/') {
        let from = Path::new(source).canonicalize().map_err(|e| format!("Cannot read {source}: {e}"))?;
        let name = from.file_name().unwrap_or_default().to_string_lossy().to_string();
        check_name(&name)?;
        read_manifest(&from)?;
        let target = plugins.join(&name);
        // Copying the installed directory onto itself would truncate every file to zero bytes.
        if target.canonicalize().is_ok_and(|target| target == from) {
            return Ok(name);
        }
        copy_dir(&from, &target)?;
        let marker = target.join(SOURCE_FILE);
        std::fs::write(&marker, from.display().to_string()).map_err(|e| format!("Cannot write {}: {e}", marker.display()))?;
        return Ok(name);
    }
    check_name(source)?;
    let (_, files) = BUNDLED.iter().find(|(name, _)| *name == source).ok_or_else(|| format!("No bundled plugin named {source}. Pass a directory path to install your own."))?;
    let target = plugins.join(source);
    std::fs::create_dir_all(&target).map_err(|e| format!("Cannot create {}: {e}", target.display()))?;
    for (file, text) in *files {
        let path = target.join(file);
        std::fs::write(&path, text).map_err(|e| format!("Cannot write {}: {e}", path.display()))?;
    }
    // The bundled copy replaces a directory install, so `plugin update` must not copy that directory back.
    let marker = target.join(SOURCE_FILE);
    match std::fs::remove_file(&marker) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(format!("Cannot remove {}: {e}", marker.display())),
        _ => {}
    }
    Ok(source.to_string())
}

/// `agent plugin update [name...]`, every installed plugin when `names` is empty: a directory install is copied again
/// from its directory, a bundled one updates when the agent has a newer version. Returns one line per plugin.
pub fn update(home: &Path, names: &[String]) -> Result<Vec<String>, String> {
    let plugins = home.join(".agents/plugins");
    let names = if names.is_empty() { installed(&plugins) } else { names.to_vec() };
    let mut current = Vec::new();
    for name in names {
        check_name(&name)?;
        let manifest = read_manifest(&plugins.join(&name)).map_err(|_| format!("{name} is not installed."))?;
        current.push((name, manifest.version));
    }
    // One failed copy, say a moved source directory, must not stop the plugins after it.
    Ok(current.into_iter().map(|(name, version)| update_one(home, &name, &version).unwrap_or_else(|error| format!("{name}: {error}"))).collect())
}

fn update_one(home: &Path, name: &str, version: &str) -> Result<String, String> {
    let dir = home.join(".agents/plugins").join(name);
    if let Ok(source) = std::fs::read_to_string(dir.join(SOURCE_FILE)) {
        install(home, &source)?;
        return Ok(format!("{name}: updated to {} from {source}.", read_manifest(&dir)?.version));
    }
    let Some(bundled) = bundled_version(name) else {
        return Ok(format!("{name}: not installed from a directory; install it again with agent plugin install <path>."));
    };
    if !crate::update::is_newer(&bundled, version) {
        return Ok(format!("{name}: {version} is the newest."));
    }
    install(home, name)?;
    Ok(format!("{name}: updated to {bundled}."))
}

fn bundled_version(name: &str) -> Option<String> {
    let (_, files) = BUNDLED.iter().find(|(bundled, _)| *bundled == name)?;
    let (_, manifest) = files.iter().find(|(file, _)| *file == "plugin.json").expect("bundled plugins ship a plugin.json");
    Some(parse_manifest(manifest).expect("bundled plugin.json is valid").version)
}

/// Reinstalls each installed bundled plugin whose built-in version is newer; returns one notice per update or failure.
/// A copy with the same or a higher version stays, so a local edit with a bumped version survives.
pub fn update_bundled(home: &Path) -> Vec<String> {
    let mut notices = Vec::new();
    for (name, _) in BUNDLED {
        let Ok(installed) = read_manifest(&home.join(".agents/plugins").join(name)) else { continue };
        let bundled = bundled_version(name).expect("BUNDLED lists it");
        if !crate::update::is_newer(&bundled, &installed.version) {
            continue;
        }
        notices.push(match install(home, name) {
            Ok(_) => format!("Updated plugin {name} to {bundled}."),
            Err(error) => format!("Cannot update plugin {name}: {error}"),
        });
    }
    notices
}

/// Deletes ~/.agents/plugins/<name> with everything in it.
pub fn remove(home: &Path, name: &str) -> Result<(), String> {
    check_name(name)?;
    let dir = home.join(".agents/plugins").join(name);
    if !dir.is_dir() {
        return Err(format!("{name} is not installed."));
    }
    std::fs::remove_dir_all(&dir).map_err(|e| format!("Cannot remove {}: {e}", dir.display()))
}

/// Names double as directory names, so nothing that could leave ~/.agents/plugins gets through.
fn check_name(name: &str) -> Result<(), String> {
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
        return Err(format!("{name} is not a plugin name: use lowercase letters, digits and dashes."));
    }
    Ok(())
}

fn copy_dir(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::create_dir_all(to).map_err(|e| format!("Cannot create {}: {e}", to.display()))?;
    for entry in std::fs::read_dir(from).map_err(|e| format!("Cannot read {}: {e}", from.display()))? {
        let entry = entry.map_err(|e| format!("Cannot read {}: {e}", from.display()))?;
        let path = entry.path();
        let target = to.join(entry.file_name());
        // file_type does not follow links, so a link loop cannot recurse forever.
        if entry.file_type().map_err(|e| format!("Cannot read {}: {e}", path.display()))?.is_dir() {
            copy_dir(&path, &target)?;
        } else {
            std::fs::copy(&path, &target).map_err(|e| format!("Cannot copy {}: {e}", path.display()))?;
        }
    }
    Ok(())
}

fn installed(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut names: Vec<String> =
        entries.filter_map(|entry| entry.ok()).filter(|entry| entry.path().is_dir()).map(|entry| entry.file_name().to_string_lossy().to_string()).filter(|name| check_name(name).is_ok()).collect();
    names.sort();
    names
}

struct Manifest {
    version: String,
    description: String,
    author: Option<String>,
    command: Vec<String>,
    /// Slash commands as (name, description), offered while the plugin runs.
    commands: Vec<(String, String)>,
    /// Tool schemas (`name`, `description`, `input_schema`), offered to the model while the plugin runs.
    tools: Vec<Value>,
    /// Setting declarations: `key`, `description`, `default` (a boolean or a string).
    settings: Vec<Value>,
}

fn read_manifest(dir: &Path) -> Result<Manifest, String> {
    let path = dir.join("plugin.json");
    let text = std::fs::read_to_string(&path).map_err(|e| format!("Cannot read {}: {e}", path.display()))?;
    parse_manifest(&text).map_err(|e| format!("{}: {e}", path.display()))
}

fn parse_manifest(text: &str) -> Result<Manifest, String> {
    let json: Value = serde_json::from_str(text).map_err(|e| format!("not valid JSON: {e}"))?;
    let command: Vec<String> = json["command"].as_array().into_iter().flatten().filter_map(|part| Some(part.as_str()?.to_string())).collect();
    if command.is_empty() {
        return Err("`command` needs at least the program to run.".into());
    }
    let mut commands = Vec::new();
    for entry in json["commands"].as_array().into_iter().flatten() {
        let name = entry["name"].as_str().ok_or("every entry in `commands` needs a `name`.")?;
        commands.push((name.to_string(), entry["description"].as_str().unwrap_or_default().to_string()));
    }
    let mut tools = Vec::new();
    for entry in json["tools"].as_array().into_iter().flatten() {
        // The API takes 64 characters of [a-zA-Z0-9_-] per name, and `__` separates the plugin.
        let name = entry["name"].as_str().filter(|name| !name.is_empty() && !name.contains("__") && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'));
        let name = name.ok_or("every entry in `tools` needs a `name` of letters, digits, `_` and `-`, without `__`.")?;
        let schema = if entry["input_schema"].is_object() { entry["input_schema"].clone() } else { json!({"type": "object", "properties": {}}) };
        tools.push(json!({"name": name, "description": entry["description"].as_str().unwrap_or_default(), "input_schema": schema}));
    }
    let settings: Vec<Value> = json["settings"].as_array().cloned().unwrap_or_default();
    if settings.iter().any(|entry| !entry["key"].is_string() || !(entry["default"].is_boolean() || entry["default"].is_string())) {
        return Err("every entry in `settings` needs a `key` and a boolean or string `default`.".into());
    }
    let text = |key: &str| json[key].as_str().unwrap_or_default().to_string();
    let author = json["author"].as_str().filter(|author| !author.is_empty()).map(String::from);
    Ok(Manifest { version: text("version"), description: text("description"), author, command, commands, tools, settings })
}

/// The values a plugin sees: each declared default, replaced by the user's value of the same type.
fn settings_for(declared: &[Value], values: &Value) -> Value {
    Value::Object(declared.iter().map(|entry| (entry["key"].as_str().unwrap_or_default().to_string(), setting(entry, values))).collect())
}

/// One declared setting's value: the user's value from `values` when it has the default's type, else the default.
pub fn setting(entry: &Value, values: &Value) -> Value {
    let value = &values[entry["key"].as_str().unwrap_or_default()];
    let default = &entry["default"];
    if value.is_boolean() == default.is_boolean() && value.is_string() == default.is_string() && !value.is_null() { value.clone() } else { default.clone() }
}

/// A JSON-RPC message to `plugin`, with its settings added to `params.context` when there is one.
fn message(plugin: &Plugin, id: Option<u64>, method: &str, params: &Value) -> String {
    let mut params = params.clone();
    if params["context"].is_object() {
        params["context"]["settings"] = plugin.settings.clone();
    }
    let mut message = json!({"jsonrpc": "2.0", "method": method, "params": params});
    if let Some(id) = id {
        message["id"] = json!(id);
    }
    message.to_string()
}

fn view(name: String, manifest: Option<Manifest>, installed: bool, running: bool, error: Option<String>) -> View {
    let manifest =
        manifest.unwrap_or(Manifest { version: String::new(), description: String::new(), author: None, command: Vec::new(), commands: Vec::new(), tools: Vec::new(), settings: Vec::new() });
    View { name, version: manifest.version, description: manifest.description, author: manifest.author, settings: manifest.settings, installed, running, error }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// A home with one plugin per (name, script); each script runs as `sh plugin.sh`.
    fn home_with(test: &str, plugins: &[(&str, &str)]) -> PathBuf {
        let home = std::env::temp_dir().join(format!("agent-plugins-{}-{test}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        for (name, script) in plugins {
            let dir = home.join(".agents/plugins").join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("plugin.json"), r#"{"version": "1.0.0", "command": ["sh", "plugin.sh"]}"#).unwrap();
            std::fs::write(dir.join("plugin.sh"), script).unwrap();
        }
        home
    }

    /// Answers every `input` request with `result`.
    fn answering(result: &str) -> String {
        format!(
            r#"while read -r line; do
  case "$line" in *'"method":"input"'*)
    id=$(echo "$line" | sed 's/.*"id":\([0-9]*\).*/\1/')
    echo '{{"jsonrpc":"2.0","id":'"$id"',"result":{result}}}';;
  esac
done"#
        )
    }

    fn eventually(check: impl Fn() -> bool) {
        let started = Instant::now();
        while !check() {
            assert!(started.elapsed() < Duration::from_secs(5), "timed out");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn first_block_refuses_the_prompt_after_earlier_rewrites() {
        let home = home_with("block", &[("a-rewrite", &answering(r#"{"action":"transform","text":"rewritten"}"#)), ("b-block", &answering(r#"{"action":"block","reason":"over budget"}"#))]);
        let plugins = Plugins::new(&home);
        plugins.sync(&[], &json!({}));
        assert_eq!(plugins.input("hi", &json!({})), Err("over budget".into()));
        plugins.sync(&["b-block".into()], &json!({}));
        assert_eq!(plugins.input("hi", &json!({})), Ok("rewritten".into()));
    }

    #[test]
    fn a_crashed_plugin_lets_prompts_through_and_reports_why() {
        let home = home_with("crash", &[("broken", "echo 'no bun here' >&2; exit 3")]);
        let plugins = Plugins::new(&home);
        plugins.sync(&[], &json!({}));
        eventually(|| !plugins.list()[0].running);
        assert_eq!(plugins.list()[0].error.as_deref(), Some("Exited (exit status: 3): no bun here"));
        assert_eq!(plugins.input("hi", &json!({})), Ok("hi".into()));
    }

    #[test]
    fn a_tool_name_over_the_api_limit_keeps_the_plugin_stopped() {
        let home = home_with("long-tool", &[("long", "cat >/dev/null")]);
        let tool = "t".repeat(59);
        std::fs::write(home.join(".agents/plugins/long/plugin.json"), format!(r#"{{"command": ["sh", "plugin.sh"], "tools": [{{"name": "{tool}"}}]}}"#)).unwrap();
        let plugins = Plugins::new(&home);
        plugins.sync(&[], &json!({}));
        assert!(!plugins.list()[0].running);
        assert_eq!(plugins.list()[0].error, Some(format!("tool `long__{tool}` is longer than the 64 characters the API takes.")));
    }

    #[test]
    fn disabling_a_plugin_ends_its_waiting_calls() {
        let home = home_with("disable-pending", &[("silent", "cat >/dev/null")]);
        let plugins = Plugins::new(&home);
        plugins.sync(&[], &json!({}));
        let (done, finished) = mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(|| done.send(plugins.request_until("silent", "tool", json!({}), None, &AtomicBool::new(false))).unwrap());
            eventually(|| !plugins.state.lock().unwrap().pending.is_empty());
            plugins.sync(&["silent".into()], &json!({}));
            assert_eq!(finished.recv_timeout(Duration::from_secs(5)), Ok(None));
        });
    }

    #[test]
    fn a_plugin_cannot_answer_another_plugins_request() {
        let forger = r#"sleep 0.3; for id in 1 2 3 4 5 6 7 8 9 10; do echo '{"jsonrpc":"2.0","id":'$id',"result":{"action":"block"}}'; done; cat >/dev/null"#;
        let home = home_with("forged-reply", &[("forger", forger), ("silent", "cat >/dev/null")]);
        let plugins = Plugins::new(&home);
        plugins.sync(&[], &json!({}));
        assert_eq!(plugins.request_until("silent", "input", json!({}), Some(Duration::from_secs(1)), &AtomicBool::new(false)), None);
    }

    #[test]
    fn status_and_account_labels_reach_the_ui() {
        let script = r#"echo '{"jsonrpc":"2.0","method":"ui.setStatus","params":{"text":[{"text":"5h "},{"text":"9%","tone":"ok"}]}}'; echo '{"jsonrpc":"2.0","method":"ui.setAccountLabels","params":{"labels":{"uuid-1":"uf_tVdC…"}}}'; cat >/dev/null"#;
        let home = home_with("status", &[("fleet", script)]);
        let plugins = Plugins::new(&home);
        let woken = Arc::new(Mutex::new(false));
        let flag = woken.clone();
        plugins.on_change(move || *flag.lock().unwrap() = true);
        plugins.sync(&[], &json!({}));
        eventually(|| *woken.lock().unwrap() && !plugins.account_labels().is_empty());
        assert_eq!(plugins.statuses(), [[("5h ".to_string(), None), ("9%".to_string(), Some("ok".to_string()))]]);
        assert_eq!(plugins.account_labels(), [("fleet".to_string(), HashMap::from([("uuid-1".to_string(), "uf_tVdC…".to_string())]))]);
        plugins.sync(&["fleet".into()], &json!({}));
        assert!(plugins.statuses().is_empty());
        assert!(plugins.account_labels().is_empty());
    }

    #[test]
    fn a_running_plugin_offers_and_answers_its_slash_commands() {
        let script = r#"while read -r line; do
  case "$line" in *'"method":"command"'*)
    id=$(echo "$line" | sed 's/.*"id":\([0-9]*\).*/\1/')
    echo '{"jsonrpc":"2.0","id":'"$id"',"result":{"text":"Saved."}}';;
  esac
done"#;
        let home = home_with("command", &[("fleet", script)]);
        let manifest = r#"{"command": ["sh", "plugin.sh"], "commands": [{"name": "fleet", "description": "Set the token"}]}"#;
        std::fs::write(home.join(".agents/plugins/fleet/plugin.json"), manifest).unwrap();
        let plugins = Plugins::new(&home);
        plugins.sync(&[], &json!({}));
        assert_eq!(plugins.commands(), [("fleet".to_string(), "Set the token".to_string())]);
        assert_eq!(plugins.command("fleet", "uf_x", &json!({})), Some(Ok("Saved.".into())));
        assert_eq!(plugins.command("other", "", &json!({})), None);
        plugins.sync(&["fleet".into()], &json!({}));
        assert!(plugins.commands().is_empty());
    }

    #[test]
    fn tools_settings_and_panels_reach_the_agent() {
        // Echoes the setting it was sent back as the tool's answer, and opens a panel on start.
        let script = r#"echo '{"jsonrpc":"2.0","method":"ui.showPanel","params":{"title":"Fleet","rows":[{"id":"a","left":"one"}]}}'
while read -r line; do
  case "$line" in *'"method":"tool"'*)
    id=$(echo "$line" | sed 's/.*"id":\([0-9]*\).*/\1/')
    greeting=$(echo "$line" | sed 's/.*"greeting":"\([^"]*\)".*/\1/')
    echo '{"jsonrpc":"2.0","id":'"$id"',"result":{"text":"'"$greeting"'"}}';;
  esac
done"#;
        let home = home_with("tools", &[("fleet", script)]);
        let manifest = r#"{"command": ["sh", "plugin.sh"], "author": "octocat",
            "tools": [{"name": "hello", "description": "Greets"}],
            "settings": [{"key": "greeting", "default": "hi"}, {"key": "loud", "default": false}]}"#;
        std::fs::write(home.join(".agents/plugins/fleet/plugin.json"), manifest).unwrap();
        let plugins = Plugins::new(&home);
        plugins.sync(&[], &json!({"fleet": {"greeting": "hey", "loud": "yes"}}));
        assert_eq!(plugins.tools()[0]["name"], "fleet__hello");
        assert_eq!(plugins.list()[0].author.as_deref(), Some("octocat"));
        let abort = AtomicBool::new(false);
        assert_eq!(plugins.tool("fleet__hello", &json!({}), &json!({}), &abort), Some(("hey".into(), false)));
        assert_eq!(plugins.tool("fleet__nope", &json!({}), &json!({}), &abort), None);
        // A value of the wrong type falls back to the default.
        assert_eq!(plugins.state.lock().unwrap().plugins[0].settings, json!({"greeting": "hey", "loud": false}));
        eventually(|| plugins.state.lock().unwrap().panel.is_some());
        assert_eq!(plugins.take_panel().unwrap().1["rows"][0]["id"], "a");
        assert!(plugins.take_panel().is_none());
        assert!(parse_manifest(r#"{"command": ["x"], "tools": [{"name": "a__b"}]}"#).is_err());
    }

    #[test]
    fn install_and_remove_stay_inside_the_plugins_directory() {
        let home = home_with("install", &[]);
        assert_eq!(install(&home, "usagefleet"), Ok("usagefleet".into()));
        assert!(home.join(".agents/plugins/usagefleet/plugin.json").is_file());
        assert!(install(&home, "nope").unwrap_err().starts_with("No bundled plugin named nope."));
        assert!(remove(&home, "..").is_err());
        assert_eq!(remove(&home, "usagefleet"), Ok(()));
        assert!(!home.join(".agents/plugins/usagefleet").exists());
    }

    #[test]
    fn installing_an_installed_plugin_from_its_own_directory_keeps_its_files() {
        let home = home_with("reinstall", &[]);
        install(&home, "usagefleet").unwrap();
        let dir = home.join(".agents/plugins/usagefleet");
        let manifest = std::fs::read(dir.join("plugin.json")).unwrap();
        assert_eq!(install(&home, dir.to_str().unwrap()), Ok("usagefleet".into()));
        assert_eq!(std::fs::read(dir.join("plugin.json")).unwrap(), manifest);
    }

    #[test]
    fn bundled_plugins_update_only_over_an_older_version() {
        let home = home_with("update", &[]);
        assert!(update_bundled(&home).is_empty());
        install(&home, "usagefleet").unwrap();
        let manifest = home.join(".agents/plugins/usagefleet/plugin.json");
        let bundled = read_manifest(manifest.parent().unwrap()).unwrap().version;
        std::fs::write(&manifest, r#"{"version": "99.0.0", "command": ["bun", "index.ts"]}"#).unwrap();
        assert!(update_bundled(&home).is_empty());
        std::fs::write(&manifest, r#"{"version": "0.0.1", "command": ["bun", "index.ts"]}"#).unwrap();
        assert_eq!(update_bundled(&home), [format!("Updated plugin usagefleet to {bundled}.")]);
        assert_eq!(read_manifest(manifest.parent().unwrap()).unwrap().version, bundled);
    }

    #[test]
    fn update_copies_each_plugin_again_from_where_it_was_installed() {
        let home = home_with("update-command", &[("old", "")]);
        let source = home.join("src/mine");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("plugin.json"), r#"{"version": "1.0.0", "command": ["sh", "plugin.sh"]}"#).unwrap();
        install(&home, source.to_str().unwrap()).unwrap();
        install(&home, "usagefleet").unwrap();
        std::fs::write(source.join("plugin.json"), r#"{"version": "1.1.0", "command": ["sh", "plugin.sh"]}"#).unwrap();

        let bundled = read_manifest(&home.join(".agents/plugins/usagefleet")).unwrap().version;
        assert_eq!(
            update(&home, &[]),
            Ok(vec![
                format!("mine: updated to 1.1.0 from {}.", source.canonicalize().unwrap().display()),
                "old: not installed from a directory; install it again with agent plugin install <path>.".to_string(),
                format!("usagefleet: {bundled} is the newest."),
            ])
        );
        assert_eq!(update(&home, &["nope".into()]), Err("nope is not installed.".into()));
    }
}
