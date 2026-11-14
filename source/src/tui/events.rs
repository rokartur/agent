use super::*;

impl App {
    pub(super) fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Attached(client) => self.add_client(client),
            Msg::Detached(id) => {
                self.clients.retain(|client| client.id != id);
                self.invalidate_screens();
            }
            Msg::Input(id, input) => self.input(id, input),
            Msg::Image { id, paste, image } => {
                self.typing_client = Some(id);
                match paste {
                    // A dropped path is plain text to a panel's field; a question's typed answer takes the image.
                    Some(text) if self.panel.is_some() && !matches!(self.panel, Some(Panel::Ask(_))) => self.paste(&text),
                    _ => self.attach(image),
                }
            }
            Msg::TerminalCopied => match self.pending_copy.take() {
                Some(Copied::Hint(text)) => self.hint = Some((text, Instant::now())),
                Some(Copied::Line(text)) => self.system("copy", text),
                None => {}
            },
            Msg::TerminalFailed(error) => self.push(Item::Error(error)),
            Msg::Place(id, place) => self.replace_place(id, place),
            Msg::Agent(event) => self.agent_event(event),
            Msg::Done(result) => self.done(result),
            Msg::ModelSettled { model, failures } => {
                for failure in failures {
                    self.push(Item::System { source: "models".into(), text: failure, tone: Tone::Error });
                }
                match model {
                    Ok((model, swapped)) => {
                        self.settings.model = model;
                        if let Some(notice) = swapped {
                            self.system("models", notice);
                        }
                    }
                    Err(error) => self.push(Item::Error(error)),
                }
            }
            Msg::Workspace(dirs) => self.extra_dirs = dirs,
            Msg::Resumed { id, path, messages, spent, notice, accounts } => {
                self.working = None;
                self.spent = spent;
                self.entries.truncate(self.header_len);
                self.push_history(&messages);
                let title = self.saved_title(&id).unwrap_or_else(|| id.clone());
                self.set_title(&title);
                self.system("session", format!("Resumed {title}"));
                if let Some(notice) = notice {
                    self.system("session", notice);
                }
                self.session_id = id;
                self.session_path = path;
                self.context_tokens = 0;
                self.read_session_usage();
                let names = self.client.account_names(&accounts);
                if !names.is_empty() {
                    // For this run only, like --account: /settings saves only the keys it changes.
                    self.settings.default_accounts.extend(names);
                    self.client.configure(&self.settings);
                    let _ = self.commands.send(Command::AccountPicked);
                }
            }
            Msg::BtwDelta(text) => {
                if let Some((_, answer)) = self.btw.as_mut().and_then(|btw| btw.turns.last_mut()) {
                    answer.push_str(&text);
                }
            }
            Msg::BtwDone(result) => {
                if let Some(btw) = self.btw.as_mut() {
                    btw.streaming = false;
                    btw.error = result.err();
                }
            }
            Msg::HandedOff(prompt) => {
                self.entries.truncate(self.header_len);
                self.context_tokens = 0;
                self.push(Item::User(prompt));
            }
            Msg::Cloned { id, path } => {
                self.working = None;
                self.system("session", format!("Cloned {} into {id}", self.session_id));
                self.session_id = id;
                self.session_path = path;
            }
            Msg::LoginFinished { provider, cancel, result } => {
                let Some(Panel::Login(login)) = self.panel.as_mut() else { return };
                if let Err(error) = login.finished(provider, &cancel, result) {
                    self.panel = None;
                    self.push(Item::Error(error));
                }
            }
            Msg::CopilotDevice { cancel, device } => {
                let Some(Panel::Login(login)) = self.panel.as_mut() else { return };
                let action = login.device(&cancel, device);
                self.login_action(action);
            }
            Msg::UsageFetched | Msg::McpChanged => {}
            Msg::Git(cwd, git, took) => {
                // git status walks the tree: a slow repo is polled less, so git takes at most ~5% of a core.
                self.git_due = Some(Instant::now() + GIT_REFRESH.max(took * 20));
                if cwd == self.cwd {
                    self.git = git;
                }
            }
            Msg::WorktreeRemoved(path, result) => match result {
                Ok(()) => {
                    self.git_due = Some(Instant::now());
                    self.system("worktree", format!("Removed {}", path.display()));
                }
                Err(error) => self.push(Item::Error(error)),
            },
            Msg::PluginsChanged => match self.plugins.take_panel() {
                // A plugin closes only its own panel, and never replaces one the user is in.
                Some((plugin, data)) if data.is_null() => {
                    if matches!(&self.panel, Some(Panel::Plugin { plugin: open, .. }) if *open == plugin) {
                        self.panel = None;
                    }
                }
                Some((plugin, data)) if self.panel.is_none() || matches!(&self.panel, Some(Panel::Plugin { plugin: open, .. }) if *open == plugin) => {
                    let index = match &self.panel {
                        Some(Panel::Plugin { plugin: open, index, .. }) if *open == plugin => *index,
                        _ => 0,
                    };
                    let index = index.min(data["rows"].as_array().map_or(0, |rows| rows.len().saturating_sub(1)));
                    self.panel = Some(Panel::Plugin { plugin, data, index });
                }
                _ => {}
            },
            Msg::Transcribed(result) => {
                self.transcribing = None;
                match result {
                    Ok(text) => self.editor.insert(&text),
                    Err(e) => self.push(Item::Error(e)),
                }
            }
            Msg::RewindPoints(points) if points.is_empty() => self.hint = Some(("Nothing to rewind yet".into(), Instant::now())),
            Msg::RewindPoints(points) => self.replace_panel(Panel::Rewind { index: points.len() - 1, points }),
            Msg::Changes(Ok(None)) => self.system("changes", "File snapshots are off outside a git repo."),
            Msg::Changes(Ok(Some(files))) if files.is_empty() => self.system("changes", "No files changed in this session."),
            Msg::Changes(Ok(Some(files))) => self.replace_panel(Panel::Changes { files, index: 0 }),
            Msg::Changes(Err(error)) => self.push(Item::Error(error)),
            Msg::ModesFetched(Err(error)) => self.push(Item::Error(error)),
            Msg::ModesFetched(Ok(())) => {}
            Msg::Context { split, instructions, session } => {
                let window = self.context_window();
                self.push(Item::Lines(context_lines(split, &instructions, window, session, self.last_usage)));
            }
            Msg::Retracted(point) => self.take_back(point),
            Msg::Rewound { messages, files, entry } => {
                match entry {
                    Some(entry) => self.entries.truncate(entry),
                    None => {
                        self.entries.truncate(self.header_len);
                        self.push_history(&messages);
                    }
                }
                self.context_tokens = 0;
                self.system("rewind", rewind_note(files));
            }
        }
    }

    pub(super) fn start(&mut self, command: Command) {
        self.scroll = 0;
        if self.working.is_some() {
            self.deferred_commands.push_back(command);
            self.hint = Some(("Runs when the turn ends".into(), Instant::now()));
            return;
        }
        if self.commands.send(command).is_err() {
            self.push(Item::Error("The agent thread stopped.".into()));
            // Otherwise the event loop would retry FollowUps on every tick.
            self.steering.lock().unwrap().clear();
            self.follow_ups.lock().unwrap().clear();
            return;
        }
        self.streamed_chars = 0;
        self.shown_output = 0;
        self.working = Some(Instant::now());
    }

    pub(super) fn agent_event(&mut self, event: Event) {
        match event {
            Event::Text(delta) => {
                self.streamed_chars += delta.chars().count();
                self.append_assistant(&delta, false);
            }
            Event::Thinking(delta) => {
                self.streamed_chars += delta.chars().count();
                self.append_assistant(&delta, true);
            }
            Event::Notice(text) => {
                let (source, text) = notice_source(&text);
                let tone = notice_tone(text);
                self.push(Item::System { source: source.into(), text: text.into(), tone });
            }
            Event::SubagentDone(run) => self.push(Item::Subagent(run)),
            Event::ToolStart { id, name, input } => self.push(Item::Tool { id, name, input, result: None, progress: None, started: Some(Instant::now()) }),
            Event::ToolProgress { id, text } => {
                let Some(entry) = self.entries.iter_mut().rev().find(|e| matches!(&e.item, Item::Tool { id: tool_id, .. } if *tool_id == id)) else { return };
                let Item::Tool { progress, .. } = &mut entry.item else { return };
                *progress = Some(text);
                entry.cache = None;
            }
            Event::ToolEnd { id, output, is_error } => {
                let Some(index) = self.entries.iter().rposition(|e| matches!(&e.item, Item::Tool { id: tool_id, .. } if *tool_id == id)) else { return };
                let entry = &mut self.entries[index];
                let Item::Tool { name, input, result, .. } = &mut entry.item else { return };
                let edits_todos = !is_error && edits_todos(name, input);
                *result = Some((output, is_error));
                entry.cache = None;
                if edits_todos {
                    self.show_todos(index);
                }
            }
            Event::Steered(text) => self.push(Item::User(text)),
            // Only headless subagents emit it.
            Event::Outbox { .. } => {}
            Event::Push(text) => self.notify("", &text),
            // The typing terminal may have detached, while a cron or wakeup turn still opens a page.
            Event::OpenPage(url) => match self.clients.iter().find(|client| Some(client.id) == self.typing_client).or(self.clients.last()) {
                Some(client) => client.send(json!({"open": url})),
                None => self.pending_page = Some(url),
            },
            // The output tail went to the model; /tasks shows it in full.
            Event::FollowUp { source, text } => self.system(&source, text.split_once('\n').map_or(text.as_str(), |(head, _)| head)),
            Event::TurnEnd { usage, context_tokens, .. } => {
                self.context_tokens = context_tokens;
                self.last_usage = usage;
                self.prompt_usage.add(usage);
                self.streamed_chars = 0;
                self.read_session_usage();
            }
            Event::Titled(title) => self.set_title(&title),
            Event::SessionStarted { id, path } => {
                self.session_id = id;
                self.session_path = path;
                self.spent = Spent::default();
                self.session_usage = Vec::new();
                self.session_usage_read = Default::default();
                self.set_title(&unnamed_title(&self.cwd));
                self.system("session", "Started a fresh conversation");
            }
            Event::Moved { id, path, cwd } => {
                self.system("session", format!("Session moved from {} to {}", self.cwd.display(), cwd.display()));
                let sequence = cwd_sequence(&cwd);
                for client in &mut self.clients {
                    client.queue(&sequence);
                }
                // Blank until the refresh lands: the old directory's branch must not show for the new one.
                self.git = Git::default();
                self.git_due = Some(Instant::now());
                self.cwd = cwd;
                self.session_id = id;
                self.session_path = path;
            }
            Event::Permission { request, reply, .. } => {
                self.notify("Permission", &format!("{} {}", request.question, request.detail));
                self.open_agent_panel(Panel::Permission { request, reply, index: 0, opened: Instant::now() });
            }
            Event::Ask { questions, reply } => {
                self.notify("Waiting", &questions.first().map_or(String::new(), |question| question.question.clone()));
                self.open_agent_panel(Panel::Ask(ask::AskPanel::new(questions, reply)));
            }
            Event::Reviewed { allowed, detail, reason } => {
                let verdict = if allowed { "allowed" } else { "denied" };
                self.system("auto", format!("{verdict} {detail}: {reason}"));
            }
            Event::Compacted { summary } => {
                self.context_tokens = 0;
                let lines = summary.lines().count();
                self.system("compact", format!("Compacted context into a {lines}-line summary"));
            }
        }
    }

    /// The agent thread blocks on this panel's reply, so it replaces any open one; a waiting /login loses its tokens.
    fn open_agent_panel(&mut self, panel: Panel) {
        if matches!(self.panel, Some(Panel::Login(_))) {
            self.push(Item::Error("Login cancelled: the agent needs an answer first. Run /login again.".into()));
        }
        self.replace_panel(panel);
    }

    /// A reply opens its panel over any open one; a waiting /login it replaces frees its callback port.
    fn replace_panel(&mut self, panel: Panel) {
        if let Some(Panel::Login(login)) = &self.panel {
            login.cancel();
        }
        self.panel = Some(panel);
    }

    fn append_assistant(&mut self, delta: &str, is_thinking: bool) {
        if !matches!(self.entries.last().map(|e| &e.item), Some(Item::Assistant { .. })) {
            self.push(Item::Assistant { text: String::new(), thinking: String::new() });
        }
        let entry = self.entries.last_mut().unwrap();
        if let Item::Assistant { text, thinking } = &mut entry.item {
            if is_thinking { thinking } else { text }.push_str(delta);
        }
        entry.cache = None;
    }

    fn done(&mut self, result: Result<String, String>) {
        self.outcome = match &result {
            _ if self.abort.load(Ordering::Relaxed) => notify::Status { state: "idle", kind: None, msg: String::new() },
            Ok(answer) => notify::Status { state: "done", kind: None, msg: if answer.trim().is_empty() { "Done".to_string() } else { answer.clone() } },
            Err(error) => notify::Status { state: "error", kind: None, msg: format!("Failed: {error}") },
        };
        match &result {
            // Esc aborted it, so the user is at the keyboard already; an aborted turn often still returns Ok.
            _ if self.abort.load(Ordering::Relaxed) => {}
            Ok(answer) if answer.trim().is_empty() => self.notify("Completed", "Done"),
            Ok(answer) => self.notify("Completed", answer),
            Err(error) => self.notify("Error", &format!("Failed: {error}")),
        }
        if std::mem::take(&mut self.retract) {
            match &result {
                Ok(_) => _ = self.commands.send(Command::Retract),
                // prompt() already dropped a prompt that got no reply; its pasted images went with it.
                Err(_) => {
                    if let Some(Item::User(text)) = self.entries.iter().rev().map(|entry| &entry.item).find(|item| matches!(item, Item::User(_))) {
                        let point = RewindPoint { at: 0, text: text.clone(), images: Vec::new() };
                        self.take_back(point);
                    }
                }
            }
        }
        if let Err(error) = result {
            self.push(Item::Error(error));
        }
        if let Some(started) = self.working.take() {
            let usage = std::mem::take(&mut self.prompt_usage);
            // /clear, /resume and !bash finish without a model call, so there is nothing to report.
            if usage.input + usage.output > 0 {
                let took = started.elapsed();
                let spent = Spent { input: usage.input, output: usage.output, time: took };
                self.spent.add(spent);
                let _ = self.commands.send(Command::SaveSpent(spent));
                self.push(Item::Done { took, usage, total: self.spent.time });
                self.turn_ended = Some(Instant::now());
            }
        }
        if matches!(*self.plan.lock().unwrap(), Plan::Ready(_)) && self.panel.is_none() {
            self.panel = Some(Panel::Plan { index: 0 });
        }
        self.git_due = Some(Instant::now());
    }
}
