use super::*;

#[derive(Clone, Copy, PartialEq)]
pub(super) enum MidTurn {
    /// Panels, settings and local info leave the turn alone, so they run mid-turn too.
    /// Settings changes reach the agent through Command::Apply, which the worker takes after the turn.
    Runs,
    /// Waits in `queued_commands` and runs when the turn ends.
    Waits,
}

/// Built-in commands: pi's wording for pi's commands, fx's for the ones taken from fx.
pub(super) const BUILTIN_COMMANDS: &[(&str, MidTurn, &str)] = &[
    ("help", MidTurn::Runs, "Show available slash commands"),
    ("clear", MidTurn::Waits, "Start a fresh conversation"),
    ("resume", MidTurn::Runs, "Resume a saved session"),
    ("rename-title", MidTurn::Runs, "Rename the current session"),
    ("rename-title-ai", MidTurn::Waits, "Name the session again from its first prompt"),
    ("clone", MidTurn::Waits, "Duplicate the current session at the current position"),
    ("import", MidTurn::Waits, "<file.jsonl>: Import and resume a session from a JSONL file"),
    ("export", MidTurn::Runs, "[path]: Export session: .jsonl (default) or .md for plain text"),
    ("workspace", MidTurn::Runs, "[add <path> | remove <path> | list]: Extra directories this session also works in"),
    ("model", MidTurn::Runs, "[search]: Select model"),
    ("settings", MidTurn::Runs, "Open settings menu"),
    ("permissions", MidTurn::Runs, "<mode>: ask, auto or full access"),
    ("thinking", MidTurn::Runs, "<level>: Set thinking level"),
    ("effort", MidTurn::Runs, "<level>: Alias for /thinking"),
    ("fast", MidTurn::Runs, "Step speed: standard, fast, ultrafast; quicker output, billed higher, on models that offer it"),
    ("ponytail", MidTurn::Runs, "<off | lite | full | ultra>: Set the ponytail level"),
    ("caveman", MidTurn::Runs, "<off | full | ultra | wenyan>: Set the caveman level"),
    ("voice", MidTurn::Runs, "Dictate in any language with whisper.cpp, Enter to stop"),
    ("context", MidTurn::Waits, "Show what fills the context window and how well it caches"),
    ("context-window", MidTurn::Runs, "<100k to 1M | auto>: Set this model's context window"),
    ("copy", MidTurn::Runs, "Copy last agent message to clipboard"),
    ("copy-session-path", MidTurn::Runs, "Copy this session's file path to clipboard for another session to read"),
    ("session", MidTurn::Runs, "Show the session id, file and context use"),
    ("hotkeys", MidTurn::Runs, "Show all keyboard shortcuts"),
    ("compact", MidTurn::Waits, "[note]: Summarize older messages"),
    ("handoff", MidTurn::Waits, "[next task]: Summarize this conversation and continue in a fresh session"),
    ("plan", MidTurn::Waits, "[task | export [file] | implement | exit]: Plan mode, read-only until you pick what to do with the plan; ~/.agents/prompts/plan.md overrides the prompt"),
    ("todos", MidTurn::Runs, "Show and edit the todo list"),
    ("plugins", MidTurn::Runs, "Install, enable and disable plugins"),
    ("skills", MidTurn::Runs, "Enable and disable skills"),
    ("trust", MidTurn::Runs, "Load this project's .agents files and run its edit hooks"),
    ("untrust", MidTurn::Runs, "Stop loading this project's .agents files and running its edit hooks"),
    ("mcp", MidTurn::Waits, "Show MCP servers: reconnect, log in, enable and disable"),
    ("usage", MidTurn::Runs, "Show token usage, cost and subscription limits"),
    ("history", MidTurn::Runs, "Search previous prompts"),
    ("changes", MidTurn::Waits, "List files changed in this session and show their diffs"),
    ("skill-usage", MidTurn::Runs, "Show how often each skill was used, least used first"),
    ("rtk", MidTurn::Runs, "Show rtk token savings"),
    ("tasks", MidTurn::Runs, "Show background shells and subagent runs"),
    ("workflows", MidTurn::Runs, "Show workflow runs: progress, pause, stop, save as a command"),
    ("goal", MidTurn::Runs, "[objective | pause | resume | stop | done]: Set a goal the agent keeps working toward, or manage it"),
    ("btw", MidTurn::Runs, "<question>: Ask a side question without touching the session"),
    ("worktree", MidTurn::Runs, "[name]: Move this session to a git worktree or remove one: pick one, or name a branch to open or create"),
    ("login", MidTurn::Runs, "Add a Claude or ChatGPT account or an API key"),
    ("logout", MidTurn::Runs, "Remove a saved account"),
    ("account", MidTurn::Runs, "Show accounts and their usage; pick one to use until it reaches the switch threshold"),
    ("detach", MidTurn::Runs, "Close this terminal's view and leave the session running; `agent attach` returns to it"),
    ("attach", MidTurn::Runs, "Show the running sessions: switch this terminal to one, or stop it"),
    ("env", MidTurn::Runs, "[name]: Pick, add or remove the computers sessions can run on, or start a new session on one; this one keeps running"),
    ("quit", MidTurn::Runs, "Quit agent"),
];

impl App {
    /// `flip` sends a mid-turn message the other way than `mid_turn` says.
    pub(super) fn submit(&mut self, text: String, flip: bool) {
        self.scroll = 0;
        let text = self.skill_command(text);
        // A side question and /goal pause work mid-turn, so they skip the steering queue.
        if let Some(question) = command_args(&text, "/btw") {
            self.open_btw(question);
            return;
        }
        if let Some(args) = command_args(&text, "/goal") {
            self.goal_command(args);
            return;
        }
        if let Some(args) = command_args(&text, "/plan")
            && self.working.is_none()
        {
            self.plan_command(args);
            return;
        }
        let local = command_args(&text, "/exit").is_some() || BUILTIN_COMMANDS.iter().any(|(name, mid_turn, _)| *mid_turn == MidTurn::Runs && command_args(&text, &format!("/{name}")).is_some());
        // Plugin commands can carry secrets such as tokens, so they never become steering text.
        let plugin = self.plugins.commands().iter().any(|(name, _)| command_args(&text, &format!("/{name}")).is_some());
        if self.working.is_some() && !local && !plugin {
            // Steering reaches the model as plain text and images, so commands wait for the turn to end and run then.
            let command = text.starts_with('!')
                || text.trim().starts_with("/skill:")
                || text.trim().starts_with("/prompt:")
                || text.trim().starts_with("/profile:")
                || self.slash_commands.iter().any(|(name, _)| command_args(&text, &format!("/{name}")).is_some());
            // Text typed after a waiting prompt waits behind it, or steering would reach the model first.
            let queued = (self.settings.mid_turn == "queued") != flip;
            if command || queued || !self.queued_commands.is_empty() {
                // The images travel with their text, so an image pasted for the next prompt starts again at #1.
                self.queued_commands.push_back((text, std::mem::take(&mut self.images)));
            } else {
                // All of them, in order: Alt+Up maps image n back to `[Image #n]`. The API rejects the dropped file's path key.
                let mut images = std::mem::take(&mut self.images);
                for image in images.iter_mut().filter_map(Value::as_object_mut) {
                    image.remove(DROPPED_PATH);
                }
                self.steering.lock().unwrap().push((text, images));
            }
            return;
        }
        let trimmed = text.trim();
        // pi's shell escape: `!cmd` runs and adds its output to the next prompt, `!!cmd` runs without that.
        if let Some(rest) = text.strip_prefix('!') {
            let keep = !rest.starts_with('!');
            let command = rest.trim_start_matches('!').trim();
            if !command.is_empty() {
                self.start(Command::Bash { command: command.to_string(), keep });
            }
            return;
        }
        // Like the official hooks, the phrase turns the mode off and still reaches the model.
        let (stop_ponytail, stop_caveman) = modes::stopped_by(trimmed);
        if stop_ponytail || stop_caveman {
            self.update_settings(|s| {
                if stop_ponytail {
                    s.ponytail = "off".into();
                }
                if stop_caveman {
                    s.caveman = "off".into();
                }
            });
        }
        let (name, args) = trimmed.strip_prefix('/').map_or(("", ""), |rest| rest.split_once(' ').map_or((rest, ""), |(name, args)| (name, args.trim())));
        let name = if name == "effort" { "thinking" } else { name };
        match name {
            "quit" | "exit" => self.quit = true,
            "detach" => self.detach(),
            "attach" => self.open_running(),
            "env" => self.switch_environment(args),
            "compact" => {
                self.system("compact", "Compacting context...");
                self.start(Command::Compact(Some(args.to_string()).filter(|focus| !focus.is_empty())));
            }
            "handoff" => {
                self.system("handoff", "Summarizing for a fresh session...");
                self.start(Command::Handoff(Some(args.to_string()).filter(|goal| !goal.is_empty())));
            }
            "clear" => {
                self.entries.truncate(self.header_len);
                self.context_tokens = 0;
                self.update_settings(Settings::reset_to_defaults);
                self.start(Command::Clear);
            }
            "rename-title" if args.is_empty() => self.push(Item::Error("Usage: /rename-title <name>".into())),
            "rename-title" => {
                let _ = self.commands.send(Command::Rename(args.to_string()));
                self.set_title(args);
                self.system("session", format!("Renamed to {args}"));
            }
            "rename-title-ai" => _ = self.commands.send(Command::RenameTitle),
            "clone" => self.start(Command::Clone),
            "import" if args.is_empty() => self.push(Item::Error("Usage: /import <file.jsonl>".into())),
            "import" => self.start(Command::Import(crate::tools::hashline::resolve(&self.cwd, args))),
            "export" => {
                let out = if args.is_empty() { format!("session-{}.jsonl", self.session_id) } else { args.to_string() };
                let _ = self.commands.send(Command::Export(crate::tools::hashline::resolve(&self.cwd, &out)));
            }
            "workspace" => _ = self.commands.send(Command::Workspace(args.to_string())),
            "model" => self.open_models(Setting::Model, args, ModelsBack::Close),
            "thinking" if args.is_empty() => self.system("thinking", format!("{}. Levels: {}", self.settings.thinking, THINKING_LEVELS.join(", "))),
            "thinking" if THINKING_LEVELS.contains(&args) => {
                self.update_settings(|s| s.thinking = args.to_string());
                self.system("thinking", format!("Level set to {args}"));
            }
            "thinking" => self.push(Item::Error(format!("Unknown thinking level {args}. Levels: {}", THINKING_LEVELS.join(", ")))),
            "fast" => {
                self.update_settings(|s| s.speed = step(&SPEEDS, &s.speed.as_str(), true).to_string());
                self.system("fast", format!("Speed: {}", self.settings.speed));
            }
            "ponytail" | "caveman" if args.is_empty() => {
                let (level, levels) = if name == "ponytail" { (&self.settings.ponytail, PONYTAIL_LEVELS) } else { (&self.settings.caveman, CAVEMAN_LEVELS) };
                self.system(name, format!("{level}. Levels: {}", levels.join(", ")));
            }
            "ponytail" if PONYTAIL_LEVELS.contains(&args) => {
                self.update_settings(|s| s.ponytail = args.to_string());
                self.system(name, format!("Level set to {args}"));
            }
            "caveman" if CAVEMAN_LEVELS.contains(&args) => {
                self.update_settings(|s| s.caveman = args.to_string());
                self.system(name, format!("Level set to {args}"));
            }
            "ponytail" => self.push(Item::Error(format!("Unknown ponytail level {args}. Levels: {}", PONYTAIL_LEVELS.join(", ")))),
            "caveman" => self.push(Item::Error(format!("Unknown caveman level {args}. Levels: {}", CAVEMAN_LEVELS.join(", ")))),
            "permissions" if args.is_empty() => {
                let modes: Vec<String> = MODES.iter().map(|m| format!("{:<12}\t{}", m.label(), m.description())).collect();
                self.system("permissions", format!("mode {}\n{}\nChange with /permissions <mode> or /settings", self.settings.permission_mode.label(), modes.join("\n")));
            }
            "permissions" => match Mode::parse(args) {
                Some(mode) => {
                    self.update_settings(|s| s.permission_mode = mode);
                    self.system("permissions", format!("mode set to {}", mode.label()));
                }
                None => self.push(Item::Error(format!("Unknown mode {args}. Modes: {}", MODES.map(Mode::label).join(", ")))),
            },
            "settings" => self.panel = Some(Panel::Settings { row: Setting::Model, query: Editor::new() }),
            "todos" => self.panel = Some(Panel::Todos { index: 0 }),
            "plugins" => self.panel = Some(Panel::Plugins { index: 0, confirm: false }),
            "skills" => self.panel = Some(Panel::Skills { query: Editor::new(), index: 0 }),
            "trust" => self.set_trust(true),
            "untrust" => self.set_trust(false),
            "mcp" => self.panel = Some(Panel::Mcp { index: 0, confirm: false }),
            "changes" => _ = self.commands.send(Command::Changes),
            "context" => _ = self.commands.send(Command::Context),
            "history" => self.open_history(),
            "usage" => {
                self.panel = Some(Panel::Usage { tab: 0, totals: usage::totals(&self.sessions_dir, &self.session_id, crate::process::now_ms()) });
                self.refresh_usage_in_background();
            }
            "skill-usage" => self.skill_usage(),
            "rtk" => self.rtk_gain(),
            "tasks" if self.terminals.lock().unwrap().list.is_empty() && self.subagents.lock().unwrap().list.is_empty() => {
                self.system("tasks", "No background tasks yet. The agent starts them with Bash run_in_background or Agent.");
            }
            "tasks" => {
                let agents = self.terminals.lock().unwrap().running() == 0 && !self.subagents.lock().unwrap().list.is_empty();
                self.open_tasks(agents);
            }
            "workflows" => self.open_workflows(),
            "login" => self.panel = Some(Panel::Login(login::Login::Kind { index: 0 })),
            "voice" => match voice::start(&self.home) {
                Ok(recording) => self.recording = Some(recording),
                Err(e) => self.push(Item::Error(e)),
            },
            "logout" | "account" if self.client.accounts().is_empty() => self.system(name, "No accounts. Add one with /login."),
            "logout" | "account" => {
                self.panel = Some(Panel::Accounts { logout: name == "logout", query: Editor::new(), index: 0 });
                self.refresh_usage_in_background();
            }
            "worktree" => {
                if let Err(error) = self.worktree(args) {
                    self.push(Item::Error(error));
                }
            }
            "context-window" if args.is_empty() => self.system("context-window", format!("{}. Set 100k to 1M or auto for this model, per provider in /settings", tokens(self.context_window()))),
            "context-window" if args == "auto" => {
                let Some(key) = self.loaded_model_key() else { return };
                self.update_settings(|s| {
                    s.context_windows.remove(&key);
                });
                self.system("context-window", format!("Back to {}", tokens(self.context_window())));
            }
            "context-window" => match parse_context_window(args) {
                Some(window) => {
                    let Some(key) = self.loaded_model_key() else { return };
                    self.system("context-window", format!("Set {key} to {}", tokens(window)));
                    self.update_settings(|s| {
                        s.context_windows.insert(key, window);
                    });
                }
                None => self.push(Item::Error(format!("Context window must be 100k to 1M, got {args}"))),
            },
            "copy" => match self.last_answer() {
                Some(answer) => {
                    self.copy(&answer, Copied::Line("Copied last agent message to clipboard".into()));
                }
                None => self.push(Item::Error("No agent message to copy yet.".into())),
            },
            "copy-session-path" => {
                let Some(path) = self.session_path.clone() else {
                    return self.push(Item::Error("This session is not saved, so there is no file to share.".into()));
                };
                self.copy(&path, Copied::Line(format!("Copied {path}")));
            }
            "session" => {
                let file = self.session_path.clone().unwrap_or_else(|| "not saved".into());
                let window = self.context_window();
                let percent = (self.context_tokens * 100).checked_div(window).unwrap_or(0);
                let info = format!("{}\nFile: {file}\nContext: {} of {} ({percent}%)", self.session_id, tokens(self.context_tokens), tokens(window));
                self.system("session", info);
            }
            "hotkeys" => self.push(Item::Lines(hotkeys_lines())),
            "help" => {
                let mut lines: Vec<String> = self.slash_commands.iter().map(|(name, description)| format!("/{name:<14}\t{description}")).collect();
                lines.push(format!("/{:<14}\trun a skill ({} loaded)", "<skill>", self.enabled_skills().count()));
                lines.push(format!("/{:<14}\trun a prompt ({} loaded)", "prompt:<name>", self.templates.len()));
                lines.push(format!("/{:<14}\tturn a skill profile on or off ({} defined)", "profile:<name>", self.settings.skill_profiles.len()));
                self.system("help", format!("{} commands\n{}", lines.len(), lines.join("\n")));
            }
            "resume" => self.open_resume(),
            skill
                if let Some(skill) = skill.strip_prefix("skill:")
                    && self.enabled_skills().any(|s| s.name == skill) =>
            {
                match crate::agent::prompt::expand_skill(trimmed, &self.skills) {
                    Ok(expanded) => {
                        if let Err(error) = skill_usage::record(&self.home.join(".agents"), skill, skill_usage::By::User, &self.cwd) {
                            self.push(Item::Error(error));
                        }
                        self.push(Item::User(text.clone()));
                        let images = self.take_images(&text);
                        self.start(Command::Prompt(expanded, images));
                    }
                    Err(error) => self.push(Item::Error(error)),
                }
            }
            name if let Some(profile) = name.strip_prefix("profile:") => self.toggle_profile(profile),
            name if self.plugins.commands().iter().any(|(command, _)| command == name) => {
                let _ = self.commands.send(Command::Plugin(name.to_string(), args.to_string()));
            }
            name if let Some(template) = name.strip_prefix("prompt:").and_then(|name| self.templates.iter().find(|t| t.name == name)) => match crate::agent::prompt::expand_template(template, args) {
                Ok(expanded) => {
                    self.push(Item::User(expanded.clone()));
                    let images = self.take_images(&text);
                    self.start(Command::Prompt(expanded, images));
                }
                Err(error) => self.push(Item::Error(error)),
            },
            name if self.saved_workflows.iter().any(|saved| saved.meta.name == name) => {
                self.push(Item::User(text.clone()));
                self.start(Command::Prompt(workflow::command_prompt(name, args), Vec::new()));
            }
            _ => {
                self.push(Item::User(text.clone()));
                let images = self.take_images(&text);
                let prompt = if mentions_ultracode(&text) { format!("{text}\n\n{ULTRACODE_NOTE}") } else { text };
                self.start(Command::Prompt(prompt, images));
            }
        }
    }

    /// /goal: no argument shows the goal, a keyword manages it, anything else sets a new goal and starts on it.
    pub(super) fn goal_command(&mut self, args: &str) {
        let mut slot = self.goal.lock().unwrap();
        if args.is_empty() {
            let text = slot.as_ref().map_or("No goal. Set one with /goal <objective>.".into(), Goal::summary);
            drop(slot);
            self.system("goal", text);
            return;
        }
        let change = match (args, slot.as_mut()) {
            ("pause" | "resume" | "stop" | "done", None) => None,
            ("pause", Some(goal)) => {
                goal.status = GoalStatus::Paused;
                Some(("Goal paused.", false))
            }
            ("resume", Some(goal)) => {
                goal.status = GoalStatus::Active;
                Some(("Goal resumed.", true))
            }
            ("done", Some(goal)) => {
                goal.status = GoalStatus::Complete;
                Some(("Goal marked done.", false))
            }
            ("stop", Some(_)) => {
                *slot = None;
                Some(("Goal stopped.", false))
            }
            (objective, _) => {
                *slot = Some(Goal::new(objective, None));
                Some(("Goal set.", true))
            }
        };
        let prompt = slot.as_ref().map(Goal::prompt);
        drop(slot);
        let Some((note, run)) = change else {
            self.push(Item::Error("No goal. Set one with /goal <objective>.".into()));
            return;
        };
        self.system("goal", note);
        let _ = self.commands.send(Command::SaveGoal);
        // A running turn picks the active goal up when it ends.
        if let Some(prompt) = prompt.filter(|_| run && self.working.is_none()) {
            self.push(Item::User(prompt.clone()));
            self.start(Command::Prompt(prompt, Vec::new()));
        }
    }

    /// /plan: no argument turns plan mode on, or off when it is on; a ready plan reopens its menu.
    /// Text starts planning on it through the plan prompt. A ready plan also takes `export [file]` and `implement`; `exit` leaves plan mode.
    pub(super) fn plan_command(&mut self, args: &str) {
        let ready = matches!(*self.plan.lock().unwrap(), Plan::Ready(_));
        match args.split_once(' ').map_or((args, ""), |(word, rest)| (word, rest.trim())) {
            ("exit", "") => {
                self.set_plan(Plan::Off);
                return self.system("plan", "Plan mode off.");
            }
            ("export", file) if ready => return self.export_plan(file),
            ("implement", "") if ready => return self.plan_action(0),
            _ => {}
        }
        if args.is_empty() {
            let current = self.plan.lock().unwrap().clone();
            match current {
                Plan::Off => {
                    self.set_plan(Plan::Drafting);
                    self.system("plan", "Plan mode on: read-only until you pick what to do with the plan.");
                }
                Plan::Drafting => {
                    self.set_plan(Plan::Off);
                    self.system("plan", "Plan mode off.");
                }
                Plan::Ready(_) => self.panel = Some(Panel::Plan { index: 0 }),
            }
            return;
        }
        let prompt = match crate::agent::prompt::expand_plan(&self.templates, args) {
            Ok(prompt) => prompt,
            Err(error) => return self.push(Item::Error(error)),
        };
        self.set_plan(Plan::Drafting);
        self.push(Item::User(format!("/plan {args}")));
        let images = self.take_images(args);
        self.start(Command::Prompt(prompt, images));
    }

    pub(super) fn plan_action(&mut self, choice: usize) {
        self.panel = None;
        let Plan::Ready(_) = *self.plan.lock().unwrap() else { return };
        match choice {
            0 => self.panel = Some(Panel::PlanFresh { index: PLAN_FRESH_ROWS.len() - 1 }),
            1 => self.export_plan(""),
            _ => {
                self.set_plan(Plan::Drafting);
                self.system("plan", "Still planning. Say what to change.");
            }
        }
    }

    /// Clears the transcript and implements the ready plan in a new session with the picked model and thinking level.
    pub(super) fn start_fresh_plan(&mut self) {
        self.panel = None;
        let Plan::Ready(text) = self.plan.lock().unwrap().clone() else { return };
        self.set_plan(Plan::Off);
        self.entries.truncate(self.header_len);
        self.context_tokens = 0;
        let prompt = format!("Implement this plan:\n\n{text}");
        self.push(Item::User(prompt.clone()));
        self.start(Command::Fresh(prompt));
    }

    /// Sets plan mode and writes it to the session, so resuming it comes back in the same mode.
    pub(super) fn set_plan(&mut self, plan: Plan) {
        *self.plan.lock().unwrap() = plan;
        let _ = self.commands.send(Command::SavePlan);
    }

    /// Writes the ready plan to `file` under the cwd, or to `<TITLE>_PLAN.md` when `file` is empty.
    /// A failure keeps the plan ready so `/plan export <file>` can retry.
    pub(super) fn export_plan(&mut self, file: &str) {
        let Plan::Ready(text) = self.plan.lock().unwrap().clone() else { return };
        let path = crate::tools::hashline::resolve(&self.cwd, &if file.is_empty() { plan_file_name(&text) } else { file.to_string() });
        // create_new: an existing file is the user's, never overwrite it.
        let written = std::fs::File::create_new(&path).and_then(|mut file| std::io::Write::write_all(&mut file, format!("{text}\n").as_bytes()));
        match written {
            Ok(()) => {
                self.set_plan(Plan::Off);
                self.system("plan", format!("Exported to {}", path.display()));
            }
            Err(error) => self.push(Item::Error(format!("Cannot export to {}: {error}. /plan export <file> writes it elsewhere.", path.display()))),
        }
    }

    /// pi's worktree extension: no name picks from the list, a name opens or creates that worktree.
    pub(super) fn worktree(&mut self, name: &str) -> Result<(), String> {
        let (worktrees, current) = worktree::list(&self.cwd)?;
        if name.is_empty() {
            self.panel = Some(Panel::Worktrees { confirm: false, index: 0, worktrees, current });
            return Ok(());
        }
        let path = worktree::open(&self.cwd, &worktrees, name)?;
        self.start(Command::Worktree(path));
        Ok(())
    }
}
