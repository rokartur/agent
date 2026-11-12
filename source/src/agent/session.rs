use super::*;
use compaction::serialize;

const NAMING_SYSTEM: &str =
    "Name a coding session from the user's first message. Reply with the name only: 3 to 6 words naming the concrete task, in the language of the message, one line, no quotes, no markdown.";
const NAMING_PROMPT_CHARS: usize = 2000;
const NAMING_MAX_TOKENS: u64 = 1000;
const TITLE_CHARS: usize = 80;

#[derive(Clone)]
pub struct Session {
    pub id: String,
    pub path: Option<PathBuf>,
    /// The session this one copies because another process had it open.
    pub forked_from: Option<String>,
    /// Held while the session is open, so a second process copies the session instead of interleaving its entries.
    lock: Option<Arc<File>>,
}

impl Agent {
    pub fn save_spent(&mut self, spent: Spent) -> Result<(), String> {
        self.spent.add(spent);
        self.session.append(&spent.entry())
    }

    pub fn save_goal(&mut self) -> Result<(), String> {
        let entry = Goal::entry(self.goal.lock().unwrap().as_ref());
        self.session.append(&entry)
    }

    pub fn save_plan(&mut self) -> Result<(), String> {
        let entry = self.plan.lock().unwrap().entry();
        self.session.append(&entry)
    }

    /// Names a fresh session from its first prompt on a background thread; `prompt` emits the title once it is ready.
    pub fn start_naming(&mut self, text: &str) {
        let request = self.naming_request(text);
        let (client, session, cwd) = (self.client.clone(), self.session.clone(), self.cwd.clone());
        self.pending_title = Some(std::thread::spawn(move || name_session(&client, &session, &cwd, &request)));
    }

    /// /rename-title-ai: names the session again from its first prompt.
    pub fn rename_title(&self) -> Result<String, String> {
        let first = self.history.messages.iter().find(|m| m["role"] == "user" && m["content"][0]["type"] == "text");
        let Some(text) = first.and_then(|m| m["content"][0]["text"].as_str()) else { return Err("Nothing to name yet.".into()) };
        name_session(&self.client, &self.session, &self.cwd, &self.naming_request(text))
    }

    fn naming_request(&self, text: &str) -> Value {
        let prompt: String = text.chars().take(NAMING_PROMPT_CHARS).collect();
        api::plain_request(&self.cheap_model, NAMING_SYSTEM, &prompt, NAMING_MAX_TOKENS)
    }

    /// Starts an empty session (unsaved if the current one is unsaved).
    pub fn new_session(&mut self) -> Result<(), String> {
        self.session = Session::start(&self.sessions_dir, self.session.path.is_some(), &self.cwd, &self.model)?;
        self.load(Saved::default());
        self.announce_session();
        Ok(())
    }

    /// Switches to the saved session `id`.
    pub fn open_session(&mut self, id: &str) -> Result<Opened, String> {
        let (session, mut saved) = Session::resume(&self.sessions_dir, id)?;
        self.session = session;
        let messages = saved.history.messages.clone();
        let accounts = std::mem::take(&mut saved.accounts);
        let config = saved.config.take();
        self.load(saved);
        self.announce_session();
        Ok(Opened { messages, accounts, config })
    }

    /// --add-dir and /workspace add: `path` is relative to cwd or starts with `~`; saved so a resume restores it. Returns the notice to show.
    pub fn add_dir(&mut self, path: &str) -> Result<String, String> {
        let dir = crate::process::canonicalize(tools::hashline::resolve(&self.cwd, path)).map_err(|e| format!("Cannot open {path}: {e}"))?;
        if !dir.is_dir() {
            return Err(format!("{} is not a directory.", dir.display()));
        }
        let cwd = crate::process::canonicalize(&self.cwd).map_err(|e| format!("Cannot open {}: {e}", self.cwd.display()))?;
        // Not an error, so `agent -c --add-dir ../api` on a session that has it still starts.
        if dir.starts_with(&cwd) {
            return Ok(format!("{} is inside the working directory, so it is already in the workspace.", dir.display()));
        }
        if let Some(known) = self.extra_dirs.iter().find(|known| dir.starts_with(known)) {
            return Ok(format!("{} is already added as {}.", dir.display(), known.display()));
        }
        self.extra_dirs.push(dir.clone());
        self.save_extra_dirs()?;
        Ok(format!("Added {}.", dir.display()))
    }

    /// /workspace remove: takes the path as listed or as typed relative to cwd. Returns the notice to show.
    pub fn remove_dir(&mut self, path: &str) -> Result<String, String> {
        let typed = tools::hashline::resolve(&self.cwd, path);
        let dir = crate::process::canonicalize(&typed).unwrap_or(typed);
        let index = self.extra_dirs.iter().position(|known| *known == dir).ok_or(format!("{} is not an extra directory.", dir.display()))?;
        let dir = self.extra_dirs.remove(index);
        self.save_extra_dirs()?;
        Ok(format!("Removed {}.", dir.display()))
    }

    fn save_extra_dirs(&self) -> Result<(), String> {
        self.session.append(&json!({"type": "extra_dirs", "dirs": self.extra_dirs}))
    }

    /// Saves the mode, model and effort, so `open_session` returns them as a `SessionConfig`.
    pub fn save_session_config(&self) -> Result<(), String> {
        let mode = self.permission_mode.lock().unwrap().label();
        self.session.append(&json!({"type": "config", "mode": mode, "model": self.model, "effort": self.effort}))
    }

    /// `cd DIR` typed after `!` or sent as the model's whole Bash command: the session moves to DIR, like /worktree.
    pub(super) fn cd(&mut self, dir: &str, emit: &mut dyn FnMut(Event)) -> Result<String, String> {
        let typed = match dir.strip_prefix('~') {
            Some(rest) if rest.is_empty() || rest.starts_with('/') => self.home.join(rest.trim_start_matches('/')),
            _ => self.cwd.join(dir),
        };
        let dir = crate::process::canonicalize(&typed).map_err(|e| format!("Cannot open {dir}: {e}"))?;
        if !dir.is_dir() {
            return Err(format!("{} is not a directory.", dir.display()));
        }
        if dir == self.cwd {
            return Ok(format!("Already in {}.", dir.display()));
        }
        self.move_to(dir.clone(), emit)?;
        Ok(format!("Session moved to {}; later commands and relative paths start there.", dir.display()))
    }

    /// /worktree and `cd`: pi pins a session to its cwd, so the history moves to a new session started in `cwd`.
    pub fn move_to(&mut self, cwd: PathBuf, emit: &mut dyn FnMut(Event)) -> Result<(), String> {
        // Everything that can fail runs before the agent changes, so an error leaves it in the old checkout.
        let settings = crate::settings::load_in(&self.home, &cwd, false)?;
        let session = self.copy_session(&cwd)?;
        // Earlier snapshots belong to the old checkout's snapshot repo.
        session.append(&json!({"type": "checkpoints_reset"}))?;
        self.grants = Grants::default();
        self.shell_context.push(format!("Session moved from {} to {}. Paths and git state from earlier messages refer to the old checkout.", self.cwd.display(), cwd.display()));
        self.trusted_projects = settings.trusted_projects.clone();
        self.permission_rules = settings.permissions.clone();
        self.before_tool = settings.before_tool.clone();
        self.system = prompt::system_prompt(&self.agent_dir, &cwd, settings.trusts(&cwd), |skill| settings.skill_enabled(skill), settings.prefer_global());
        let editor = self.anchors.editor.take();
        self.anchors = Anchors::new(&session.id);
        self.anchors.editor = editor;
        self.nested_instructions.clear();
        self.history.apply(json!({"type": "checkpoints_reset"}))?;
        if self.snapshots.is_some() {
            self.snapshots = Snapshots::open(&self.home, &cwd);
        }
        self.cwd = cwd.clone();
        self.session = session;
        self.announce_session();
        emit(Event::Moved { id: self.session.id.clone(), path: self.session.display_path(), cwd });
        Ok(())
    }

    /// /clone: continues in a copy of this session, so the original stays as it was.
    pub fn clone_session(&mut self) -> Result<(), String> {
        self.session = self.copy_session(&self.cwd)?;
        self.announce_session();
        Ok(())
    }

    /// /import: copies an exported session into the sessions dir and resumes it.
    pub fn import_session(&mut self, file: &Path) -> Result<Opened, String> {
        let id = Session::import(&self.sessions_dir, file)?;
        let target = self.sessions_dir.join(format!("{id}.jsonl"));
        self.open_session(&id).inspect_err(|_| {
            let _ = std::fs::remove_file(&target);
        })
    }

    /// A new session in `cwd` holding this session's entries.
    fn copy_session(&self, cwd: &Path) -> Result<Session, String> {
        match &self.session.path {
            Some(path) => Session::copy(&self.sessions_dir, path, cwd, &self.model),
            None => Session::start(&self.sessions_dir, false, cwd, &self.model),
        }
    }

    /// /export: the session file as is, or for a `.md` path the conversation as plain text.
    pub fn export_session(&self, out: &Path) -> Result<(), String> {
        let text = if out.extension().is_some_and(|ext| ext == "md") {
            serialize(&self.history.messages)
        } else {
            let path = self.session.path.as_ref().ok_or("This session is not saved (--no-session); export to a .md path.")?;
            std::fs::read_to_string(path).map_err(|e| format!("Cannot read {}: {e}", path.display()))?
        };
        // create_new: `/export README.md` must not replace a project file that no snapshot covers. 0600 like the session file it copies.
        write_new_private(out, &text).map_err(|e| match e.kind() {
            std::io::ErrorKind::AlreadyExists => format!("{} exists.", out.display()),
            _ => format!("Cannot write {}: {e}", out.display()),
        })
    }

    /// Replaces the conversation state with a saved (or empty) session's.
    pub fn load(&mut self, saved: Saved) {
        self.grants = Grants::default();
        self.history = saved.history;
        self.next_ref = condense::next_ref(&self.history.condensed);
        crate::image::fit_history(&mut self.history.messages);
        *self.todos.lock().unwrap() = saved.todos;
        *self.goal.lock().unwrap() = saved.goal;
        *self.plan.lock().unwrap() = saved.plan;
        self.tools = self.tool_schemas();
        self.spent = saved.spent;
        self.pending_condense.clear();
        self.pending_title = None;
        self.nested_instructions.clear();
        self.extra_dirs = saved.extra_dirs;
        self.message_thread = api::MessageThread::default();
        self.estimate_context_tokens();
    }

    /// Tells plugins the session started; needs the settled model for the context's provider.
    pub fn announce_session(&self) {
        // `agent attach` and `agent -r` find a running session by the id on its card.
        self.describe_to_peers(false);
        self.plugins.event(json!({"type": "session_start", "context": self.plugin_context()}));
    }
}

pub struct SessionInfo {
    pub id: String,
    pub cwd: String,
    pub name: Option<String>,
    /// First line of the first prompt.
    pub title: String,
    pub turns: usize,
    pub modified: std::time::SystemTime,
}

impl SessionInfo {
    /// None for the null summary of a session /resume leaves out.
    fn from_summary(id: &str, summary: &Value, modified: std::time::SystemTime) -> Option<SessionInfo> {
        Some(SessionInfo {
            id: id.to_string(),
            cwd: summary["cwd"].as_str()?.to_string(),
            name: summary["name"].as_str().map(String::from),
            title: summary["title"].as_str()?.to_string(),
            turns: summary["turns"].as_u64()? as usize,
            modified,
        })
    }
}

impl Session {
    /// The session file for display; `None` when the session is not saved.
    pub fn display_path(&self) -> Option<String> {
        self.path.as_ref().map(|p| p.display().to_string())
    }

    pub fn start(dir: &Path, save: bool, cwd: &Path, model: &str) -> Result<Session, String> {
        // 8 hex chars from the clock and pid: short enough to type after `--resume`.
        let mut seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() ^ ((std::process::id() as u128) << 64);
        // A colliding id would append this session's header to another session's file.
        let id = loop {
            let id = format!("{:08x}", (seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 64) as u32);
            if !save || !dir.join(format!("{id}.jsonl")).exists() {
                break id;
            }
            seed += 1;
        };
        let mut session = Session { path: save.then(|| dir.join(format!("{id}.jsonl"))), id, forked_from: None, lock: None };
        if save {
            std::fs::create_dir_all(dir).map_err(|e| format!("Cannot create {}: {e}", dir.display()))?;
        }
        session.append(&json!({"type": "session", "id": session.id, "cwd": cwd, "model": model, "timestamp": now_ms()}))?;
        if let Some(path) = &session.path {
            let file = lock_file(path).map_err(|e| format!("Cannot open {}: {e}", path.display()))?;
            file.try_lock().map_err(|e| format!("Cannot lock {}: {e}", path.display()))?;
            session.lock = Some(Arc::new(file));
        }
        Ok(session)
    }

    /// Opens session `id`, or the one saved session whose id starts with `id`.
    /// When another process has it open, continues in a copy so the two conversations do not interleave in one file.
    pub fn resume(dir: &Path, id: &str) -> Result<(Session, Saved), String> {
        let id = resolve_id(dir, id)?;
        let path = dir.join(format!("{id}.jsonl"));
        let saved = Session::history(&path)?;
        let file = lock_file(&path).map_err(|e| format!("Cannot open {}: {e}", path.display()))?;
        match file.try_lock() {
            Ok(()) => Ok((Session { id, path: Some(path), forked_from: None, lock: Some(Arc::new(file)) }, saved)),
            Err(TryLockError::WouldBlock) => Ok((Session::fork(dir, &path, id)?, saved)),
            Err(TryLockError::Error(e)) => Err(format!("Cannot lock {}: {e}", path.display())),
        }
    }

    pub fn fork_notice(&self) -> Option<String> {
        let from = self.forked_from.as_ref()?;
        Some(format!("Session {from} is open in another process, continuing in copy {}", self.id))
    }

    fn fork(dir: &Path, path: &Path, id: String) -> Result<Session, String> {
        let cannot_read = |e: std::io::Error| format!("Cannot read {}: {e}", path.display());
        let first_line = BufReader::new(File::open(path).map_err(cannot_read)?).lines().next().transpose().map_err(cannot_read)?;
        let header: Value = serde_json::from_str(&first_line.unwrap_or_default()).map_err(|e| format!("{}:1: {e}", path.display()))?;
        let (Some(cwd), Some(model)) = (header["cwd"].as_str(), header["model"].as_str()) else {
            return Err(format!("{}:1: session header without cwd or model", path.display()));
        };
        let mut copy = Session::copy(dir, path, Path::new(cwd), model)?;
        copy.forked_from = Some(id);
        Ok(copy)
    }

    /// A new saved session holding every entry of `from` after its header, and the pages it published.
    fn copy(dir: &Path, from: &Path, cwd: &Path, model: &str) -> Result<Session, String> {
        let session = Session::start(dir, true, cwd, model)?;
        let text = std::fs::read_to_string(from).map_err(|e| format!("Cannot read {}: {e}", from.display()))?;
        for line in complete_lines(&text).lines().skip(1) {
            let entry: Value = serde_json::from_str(line).map_err(|e| format!("{}: {e}", from.display()))?;
            session.append(&entry)?;
        }
        let pages = from.with_extension("").join("pages");
        if let Ok(files) = std::fs::read_dir(&pages) {
            let copied = session.path.as_ref().expect("saved above").with_extension("").join("pages");
            std::fs::create_dir_all(copied.parent().expect("a session file has a folder")).map_err(|e| format!("Cannot create {}: {e}", copied.display()))?;
            crate::process::private_dir(&copied)?;
            for file in files.flatten().filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file())) {
                // std::fs::copy keeps the 0600 of the page.
                std::fs::copy(file.path(), copied.join(file.file_name())).map_err(|e| format!("Cannot copy {}: {e}", file.path().display()))?;
            }
        }
        Ok(session)
    }

    /// Replays a session file without opening the session, e.g. to show a subagent's transcript.
    pub fn history(path: &Path) -> Result<Saved, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("Cannot read {}: {e}", path.display()))?;
        let mut saved = Saved::default();
        for (number, line) in complete_lines(&text).lines().enumerate() {
            let at = format!("{}:{}", path.display(), number + 1);
            let entry: Value = serde_json::from_str(line).map_err(|e| format!("{at}: {e}"))?;
            let missing = |field: &str| format!("{at}: {} without {field}", entry["type"].as_str().unwrap_or_default());
            match entry["type"].as_str() {
                Some("goal") => saved.goal = Goal::from_entry(&entry),
                Some("plan") => saved.plan = Plan::from_entry(&entry),
                Some("todo") => saved.todos = Todos::from_entry(&entry),
                Some("spent") => saved.spent.add(Spent::from_entry(&entry)),
                Some("extra_dirs") => saved.extra_dirs = serde_json::from_value(entry["dirs"].clone()).map_err(|_| missing("dirs"))?,
                Some("config") => {
                    let text = |field: &str| entry[field].as_str().map(String::from).ok_or_else(|| missing(field));
                    let mode = entry["mode"].as_str().and_then(Mode::parse).ok_or_else(|| missing("mode"))?;
                    saved.config = Some(SessionConfig { mode, model: text("model")?, effort: text("effort")? });
                }
                Some("usage") => {
                    if let (Some(provider), Some(account)) = (entry["provider"].as_str(), entry["account"].as_str()) {
                        saved.accounts.insert(provider.to_string(), account.to_string());
                    }
                }
                _ => saved.history.apply(entry).map_err(|e| format!("{at}: {e}"))?,
            }
        }
        Ok(saved)
    }

    /// Saved sessions in `dir`, newest first. Unreadable files are skipped.
    /// Reads only the files changed since the last list; `dir/index.json` keeps the summary of the rest by size and mtime.
    pub fn list(dir: &Path) -> Vec<SessionInfo> {
        let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
        let index_path = dir.join("index.json");
        let old_index = read_session_index(&index_path);
        let mut index = serde_json::Map::new();
        let mut sessions = Vec::new();
        for path in entries.filter_map(|e| e.ok()).map(|e| e.path()) {
            let Some(id) = path.file_name().and_then(|name| name.to_str()).and_then(|name| name.strip_suffix(".jsonl")) else { continue };
            // Stat before reading: a file that grows in between is read again next time instead of kept stale.
            let Ok(metadata) = path.metadata() else { continue };
            let Ok(modified) = metadata.modified() else { continue };
            let stamp = json!([metadata.len(), modified.duration_since(std::time::UNIX_EPOCH).map_or(0, |since| since.as_nanos() as u64)]);
            let summary = match old_index.get(id) {
                Some(cached) if cached["stamp"] == stamp => cached["summary"].clone(),
                _ => match std::fs::read_to_string(&path) {
                    Ok(text) => summarize(&text),
                    Err(_) => continue,
                },
            };
            if let Some(info) = SessionInfo::from_summary(id, &summary, modified) {
                sessions.push(info);
            }
            index.insert(id.to_string(), json!({"stamp": stamp, "summary": summary}));
        }
        if index != old_index {
            // Only a cache: when it cannot be written, the next list reads the files again.
            let _ = write_session_index(&index_path, index);
        }
        sessions.sort_by_key(|s| std::cmp::Reverse(s.modified));
        sessions
    }

    /// One saved session file; None when unreadable, a subagent's, or without a prompt yet.
    pub fn info(path: &Path) -> Option<SessionInfo> {
        let id = path.file_name()?.to_str()?.strip_suffix(".jsonl")?;
        let text = std::fs::read_to_string(path).ok()?;
        let modified = path.metadata().and_then(|m| m.modified()).ok()?;
        SessionInfo::from_summary(id, &summarize(&text), modified)
    }

    /// Deletes saved sessions untouched for longer than `max_age`. A session open in any process, this one included, holds its lock and stays.
    pub fn prune(dir: &Path, max_age: Duration) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        let cutoff = std::time::SystemTime::now() - max_age;
        for path in entries.filter_map(|e| e.ok()).map(|e| e.path()) {
            if path.extension().is_none_or(|extension| extension != "jsonl") {
                continue;
            }
            let Ok(modified) = path.metadata().and_then(|m| m.modified()) else { continue };
            if modified >= cutoff {
                continue;
            }
            let Ok(file) = lock_file(&path) else { continue };
            if file.try_lock().is_ok() {
                // A file that cannot be removed now is tried again at the next start.
                let _ = std::fs::remove_file(&path);
                // The session's folder: its pages and workflow runs.
                let _ = std::fs::remove_dir_all(path.with_extension(""));
                // Removed while still locked, so no resume can lock a session whose transcript is gone.
                #[cfg(windows)]
                let _ = std::fs::remove_file(path.with_extension("lock"));
                drop(file);
            }
        }
    }

    /// Id of the newest saved session started in `cwd`.
    pub fn latest(dir: &Path, cwd: &Path) -> Result<String, String> {
        let cwd = cwd.display().to_string();
        let newest = Session::list(dir).into_iter().find(|s| s.cwd == cwd);
        newest.map(|s| s.id).ok_or_else(|| format!("No session for {cwd} yet."))
    }

    pub fn rename(&self, name: &str) -> Result<(), String> {
        self.append(&json!({"type": "session_info", "name": name}))
    }

    pub fn append(&self, entry: &Value) -> Result<(), String> {
        let Some(path) = &self.path else { return Ok(()) };
        let mut file = crate::process::private_file().create(true).read(true).append(true).open(path).map_err(|e| format!("Cannot open {}: {e}", path.display()))?;
        drop_torn_tail(&mut file, path)?;
        // One write call, so a signal exit on another thread cannot leave half a line; writeln! on a File writes piecewise.
        file.write_all(format!("{entry}\n").as_bytes()).map_err(|e| format!("Cannot write {}: {e}", path.display()))
    }

    /// /import: copies an exported session file into `dir` and returns its id for `resume`.
    pub fn import(dir: &Path, file: &Path) -> Result<String, String> {
        let text = std::fs::read_to_string(file).map_err(|e| format!("Cannot read {}: {e}", file.display()))?;
        let header: Value = text.lines().next().and_then(|line| serde_json::from_str(line).ok()).unwrap_or_default();
        let id = header["id"].as_str().unwrap_or_default();
        // The id becomes a file name, so it must not carry a path.
        if header["type"] != "session" || id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Err(format!("{} is not an exported session.", file.display()));
        }
        let target = dir.join(format!("{id}.jsonl"));
        std::fs::create_dir_all(dir).map_err(|e| format!("Cannot create {}: {e}", dir.display()))?;
        // 0600 like `append`: a transcript holds file contents and command output.
        write_new_private(&target, &text).map_err(|e| match e.kind() {
            std::io::ErrorKind::AlreadyExists => format!("Session {id} is already here; /resume {id}."),
            _ => format!("Cannot write {}: {e}", target.display()),
        })?;
        Ok(id.to_string())
    }
}

/// Writes `text` to a new 0600 file; a failed write removes it, since a partial file would pass as "already here" on the next import.
fn write_new_private(path: &Path, text: &str) -> std::io::Result<()> {
    let mut file = crate::process::private_file().write(true).create_new(true).open(path)?;
    file.write_all(text.as_bytes()).inspect_err(|_| {
        let _ = std::fs::remove_file(path);
    })
}

/// Bump when `summarize` changes what it keeps, so `Session::list` reads every file again instead of trusting an older index.
const SESSION_INDEX_VERSION: u64 = 1;

/// What /resume shows of one session file; null for a subagent's session or one without a prompt yet.
fn summarize(text: &str) -> Value {
    let (mut cwd, mut name, mut title, mut turns) = (String::new(), None, String::new(), 0);
    let mut subagent = false;
    for entry in text.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()) {
        match entry["type"].as_str() {
            Some("session") => cwd = entry["cwd"].as_str().unwrap_or_default().to_string(),
            Some("session_info") => name = entry["name"].as_str().map(String::from),
            // Subagent sessions stay out of /resume and --continue; /tasks opens them.
            Some("parent") => subagent = true,
            Some("message") if entry["message"]["role"] == "user" && entry["message"]["content"][0]["type"] == "text" => {
                turns += 1;
                if title.is_empty() {
                    title = entry["message"]["content"][0]["text"].as_str().unwrap_or_default().lines().next().unwrap_or_default().to_string();
                }
            }
            _ => {}
        }
    }
    if turns == 0 || subagent {
        return Value::Null;
    }
    json!({"cwd": cwd, "name": name, "title": title, "turns": turns})
}

/// The summaries `Session::list` saved by session id; empty when missing, unreadable or from another index version.
fn read_session_index(path: &Path) -> serde_json::Map<String, Value> {
    let Ok(text) = std::fs::read_to_string(path) else { return serde_json::Map::new() };
    let Ok(mut index) = serde_json::from_str::<Value>(&text) else { return serde_json::Map::new() };
    match index["sessions"].take() {
        Value::Object(sessions) if index["version"] == SESSION_INDEX_VERSION => sessions,
        _ => serde_json::Map::new(),
    }
}

fn write_session_index(path: &Path, sessions: serde_json::Map<String, Value>) -> std::io::Result<()> {
    // Titles are prompt lines, so 0600 like the sessions; the rename keeps another process from reading half a file.
    let temporary = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let mut file = crate::process::private_file().create(true).write(true).truncate(true).open(&temporary)?;
    file.write_all(json!({"version": SESSION_INDEX_VERSION, "sessions": sessions}).to_string().as_bytes())?;
    std::fs::rename(&temporary, path)
}

/// The file whose lock marks a session as open in a process.
#[cfg(unix)]
fn lock_file(session: &Path) -> std::io::Result<File> {
    File::open(session)
}

/// Windows locks are mandatory: a lock on the session itself would block its own appends and every reader, so it sits beside it.
#[cfg(windows)]
fn lock_file(session: &Path) -> std::io::Result<File> {
    // NotFound for a pruned session, as the Unix open gives, so a resume does not lock a fresh sidecar.
    File::open(session)?;
    File::options().create(true).write(true).truncate(false).open(session.with_extension("lock"))
}

/// The full id for an exact id or a unique prefix of a saved session's id.
fn resolve_id(dir: &Path, prefix: &str) -> Result<String, String> {
    // Ids are ascii alphanumeric (see `import`); anything else could name a file outside `dir`.
    if prefix.is_empty() || !prefix.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(format!("{prefix:?} is not a session id."));
    }
    if dir.join(format!("{prefix}.jsonl")).is_file() {
        return Ok(prefix.to_string());
    }
    let entries = std::fs::read_dir(dir).map_err(|e| format!("Cannot read {}: {e}", dir.display()))?;
    let ids: Vec<String> = entries.filter_map(|e| e.ok()).filter_map(|e| e.file_name().to_str().and_then(|n| n.strip_suffix(".jsonl")).map(String::from)).filter(|id| id.starts_with(prefix)).collect();
    match ids.as_slice() {
        [id] => Ok(id.clone()),
        [] => Err(format!("No saved session {prefix}.")),
        _ => Err(format!("{prefix} matches {} sessions; use more characters.", ids.len())),
    }
}

/// The complete entries of a session file. Every entry ends with a newline, so a tail without one is a write that was cut short.
fn complete_lines(text: &str) -> &str {
    if text.ends_with('\n') { text } else { text.rsplit_once('\n').map_or("", |(complete, _)| complete) }
}

/// Cuts off a last line without a newline, the half entry `complete_lines` skips, so the next entry is not glued onto it.
fn drop_torn_tail(file: &mut File, path: &Path) -> Result<(), String> {
    let failed = |e: std::io::Error| format!("Cannot repair {}: {e}", path.display());
    let mut last = *b"\n";
    if file.metadata().map_err(failed)?.len() > 0 {
        file.seek(SeekFrom::End(-1)).map_err(failed)?;
        file.read_exact(&mut last).map_err(failed)?;
    }
    if last == *b"\n" {
        return Ok(());
    }
    let bytes = std::fs::read(path).map_err(failed)?;
    let complete = bytes.iter().rposition(|&byte| byte == b'\n').map_or(0, |at| at + 1);
    // A second handle: Windows refuses set_len on one opened only for append.
    std::fs::OpenOptions::new().write(true).open(path).and_then(|file| file.set_len(complete as u64)).map_err(failed)
}

/// Asks `request`'s model for a topic and saves `project｜topic` as the session name.
fn name_session(client: &Client, session: &Session, cwd: &Path, request: &Value) -> Result<String, String> {
    let reply = client.send_quiet(request, &AtomicBool::new(false))?;
    let text = text_of(&reply.message);
    let topic = text.lines().next().unwrap_or_default().trim().trim_matches('"');
    if topic.is_empty() {
        return Err("The model gave no title.".into());
    }
    let title = session_title(cwd, topic);
    session.rename(&title)?;
    Ok(title)
}

pub(super) fn session_title(cwd: &Path, topic: &str) -> String {
    let project = cwd.file_name().unwrap_or_default().to_string_lossy();
    format!("{project}｜{topic}").chars().take(TITLE_CHARS).collect()
}
