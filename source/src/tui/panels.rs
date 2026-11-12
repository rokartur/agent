use super::*;

/// The bottom panel that takes over the keys while it is open.
pub(super) enum Panel {
    Resume(ResumePanel),
    /// /attach: the other sessions running on this session's computer.
    Running {
        index: usize,
        sessions: Vec<Peer>,
        /// `x` was pressed once on the selected session; a second `x` stops it.
        confirm: bool,
    },
    /// `row`'s section is the open tab. Typing filters all sections by name.
    Settings {
        row: Setting,
        query: Editor,
    },
    /// Searchable model list for the settings `row` it edits; Enter and Esc return to `back`.
    Models {
        row: Setting,
        query: Editor,
        index: usize,
        back: ModelsBack,
    },
    /// Context window overrides, tab 0 per provider, tab 1 per model filtered by `query`.
    ContextWindows {
        tab: usize,
        query: Editor,
        index: usize,
    },
    /// `headroom_url` or `headroom_token` (by settings `row`) being typed; Enter saves it, Esc goes back to the row.
    Headroom {
        row: Setting,
        input: Editor,
    },
    /// Manual provider order, tab 0 model providers, tab 1 web search providers.
    ProviderOrder {
        tab: usize,
        index: usize,
    },
    /// The settings tab of skill profiles: one row per profile, then a row that makes a new one.
    Profiles {
        index: usize,
        /// The name typed for a new profile, or for the selected one when renaming.
        naming: Option<Editor>,
        /// `d` was pressed once on the selected profile; a second `d` deletes it.
        confirm: bool,
    },
    /// /env: the environments of the terminal that opened it; in a local session also a row that adds one.
    Environments {
        place: Place,
        index: usize,
        /// The Tailscale name typed for a new environment.
        naming: Option<Editor>,
        /// `d` was pressed once on the selected environment; a second `d` removes it from settings.
        confirm: bool,
    },
    /// Every skill, checked when `profile` holds it by name or by its repo.
    ProfileSkills {
        profile: String,
        query: Editor,
        index: usize,
    },
    /// fx's "Permission needed" prompt; the agent thread waits on `reply`.
    Permission {
        request: Request,
        reply: Sender<Decision>,
        index: usize,
        opened: Instant,
    },
    Ask(ask::AskPanel),
    Todos {
        index: usize,
    },
    Plugins {
        index: usize,
        /// `d` was pressed once on the selected plugin; a second `d` removes it.
        confirm: bool,
    },
    /// Every skill, including the ones `Settings.skill_enabled` leaves out of the prompt.
    Skills {
        query: Editor,
        index: usize,
    },
    /// `confirm`: space was pressed once on a project server whose entry the panel cuts.
    Mcp {
        index: usize,
        confirm: bool,
    },
    /// Token totals per `usage::SCOPES`, read when /usage opens; the last tab shows subscription limits.
    Usage {
        tab: usize,
        totals: [Vec<ModelTotal>; 4],
    },
    /// The settings a plugin declares in plugin.json; `edit` holds a string being typed.
    PluginSettings {
        plugin: String,
        index: usize,
        edit: Option<Editor>,
    },
    /// A panel a plugin opened with `ui.showPanel`; `data` is its params.
    Plugin {
        plugin: String,
        data: Value,
        index: usize,
    },
    /// The rows under the footer: `row` 0 is main, then the shells pill and the agents pill while they run, then each workflow.
    Agents {
        row: usize,
    },
    /// /tasks: the shells tab, or the subagent runs tab when `agents`; `index` selects in that tab.
    Tasks {
        agents: bool,
        index: usize,
    },
    /// /workflows with more than one run.
    Workflows {
        index: usize,
    },
    /// One run's progress view: `phase` None lists its phases, Some lists that phase's agents.
    WorkflowRun {
        id: String,
        phase: Option<usize>,
        index: usize,
    },
    /// `s` in the progress view: Tab picks the personal workflows dir over the project's.
    WorkflowSave {
        id: String,
        personal: bool,
    },
    /// One shell from /tasks, full screen and read-only. `loaded` is the (log size, width) `lines` were built for.
    Terminal {
        index: usize,
        scroll: usize,
        loaded: Option<(u64, usize)>,
        lines: Vec<Line>,
    },
    /// Shows `App::btw`, which outlives the panel so a bare /btw reopens it.
    Btw,
    /// The start-up question for an untrusted project; `files` are what /trust would load.
    Trust {
        root: PathBuf,
        files: Vec<PathBuf>,
        /// Keys wait out `PERMISSION_SETTLE` from here.
        opened: Instant,
    },
    Worktrees(WorktreesPanel),
    Login(login::Login),
    /// /account pins the chosen account, /logout removes it. Typing filters by name or provider.
    Accounts {
        logout: bool,
        query: Editor,
        index: usize,
    },
    History(HistoryPanel),
    /// Double Esc while idle: the prompts from `agent::rewind_points` to go back to.
    Rewind {
        index: usize,
        points: Vec<RewindPoint>,
    },
    /// /changes: files this session changed; Enter opens the diff.
    Changes {
        files: Vec<FileChange>,
        index: usize,
    },
    /// One /changes file's diff filling the screen; Esc goes back to the list.
    Diff {
        files: Vec<FileChange>,
        index: usize,
        scroll: usize,
    },
    /// What to do with the plan from plan_mode_complete, opened when its turn ends or by /plan.
    Plan {
        index: usize,
    },
    /// "Start fresh and implement": the model and thinking level for the new session, then Start.
    PlanFresh {
        index: usize,
    },
}

/// The panel the model picker returns to: none for /model, the settings list, or the start-fresh panel.
#[derive(Clone, Copy)]
pub(super) enum ModelsBack {
    Close,
    Settings,
    PlanFresh,
}

impl App {
    /// Keys while a panel is open: Esc and Ctrl+C close it, the rest belongs to the panel.
    pub(super) fn panel_key(&mut self, key: KeyEvent) {
        let ctrl_c = key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c');
        let key = if ctrl_c { KeyEvent::from(KeyCode::Esc) } else { key };
        let Some(panel) = &self.panel else { return };
        match panel {
            Panel::Permission { .. } => self.permission_key(key.code),
            Panel::Plan { .. } => self.plan_key(key.code),
            Panel::PlanFresh { .. } => self.plan_fresh_key(key.code),
            Panel::Ask(_) => self.ask_key(key),
            Panel::Login(_) => self.login_key(key),
            Panel::Trust { .. } => self.trust_key(key.code),
            Panel::Btw => self.btw_key(key),
            Panel::Models { .. } => self.models_key(key),
            Panel::ContextWindows { .. } => self.context_windows_key(key),
            Panel::ProviderOrder { .. } => self.provider_order_key(key),
            Panel::Profiles { .. } => self.profiles_key(key),
            Panel::Running { .. } => self.running_key(key.code),
            Panel::Environments { .. } => self.environments_key(key),
            Panel::ProfileSkills { .. } => self.profile_skills_key(key),
            Panel::Headroom { .. } => self.headroom_key(key),
            Panel::Diff { .. } => self.diff_key(key.code),
            Panel::Terminal { .. } => self.terminal_key(key.code),
            Panel::Workflows { .. } | Panel::WorkflowRun { .. } | Panel::WorkflowSave { .. } => self.workflow_key(key.code),
            Panel::PluginSettings { .. } => self.plugin_settings_key(key),
            Panel::Plugin { .. } => self.plugin_key(key.code),
            Panel::Resume(_)
            | Panel::Settings { .. }
            | Panel::Todos { .. }
            | Panel::Usage { .. }
            | Panel::Skills { .. }
            | Panel::Mcp { .. }
            | Panel::Plugins { .. }
            | Panel::Worktrees(_)
            | Panel::Agents { .. }
            | Panel::Tasks { .. }
            | Panel::Rewind { .. }
            | Panel::Changes { .. }
            | Panel::History(_)
            | Panel::Accounts { .. }
                if key.code == KeyCode::Esc =>
            {
                self.panel = None
            }
            Panel::Resume(_) => self.resume_key(key.code),
            Panel::Settings { .. } => self.settings_key(key),
            Panel::Todos { .. } => self.todos_key(key.code),
            Panel::Usage { .. } => self.usage_key(key.code),
            Panel::Skills { .. } => self.skills_key(key),
            Panel::Mcp { .. } => self.mcp_key(key),
            Panel::Plugins { .. } => self.plugins_key(key.code),
            Panel::Worktrees(_) => self.worktrees_key(key.code),
            Panel::Agents { .. } => self.agents_key(key.code),
            Panel::Tasks { .. } => self.tasks_key(key.code),
            Panel::Rewind { .. } => self.rewind_key(key.code),
            Panel::Changes { .. } => self.changes_key(key.code),
            Panel::History(_) => self.history_key(key),
            Panel::Accounts { .. } => self.accounts_key(key),
        }
    }

    fn permission_key(&mut self, code: KeyCode) {
        let Some(Panel::Permission { index, opened, .. }) = self.panel.as_mut() else { return };
        // An Enter or digit typed for the prompt just before the panel opened must not approve the call.
        let settling = opened.elapsed() < PERMISSION_SETTLE;
        let choice = match code {
            KeyCode::Up => {
                *index = index.saturating_sub(1);
                return;
            }
            KeyCode::Down => {
                *index = (*index + 1).min(2);
                return;
            }
            KeyCode::Char('3') | KeyCode::Esc => 2,
            _ if settling => return,
            KeyCode::Char('1') => 0,
            KeyCode::Char('2') => 1,
            KeyCode::Enter => *index,
            _ => return,
        };
        let Some(Panel::Permission { request, reply, .. }) = self.panel.take() else { unreachable!() };
        let decision = match choice {
            0 => Decision::Allow,
            1 => Decision::AllowAlways,
            _ => Decision::Deny,
        };
        if matches!(decision, Decision::Deny) {
            self.warn("permissions", format!("Denied {}", request.detail));
        }
        let _ = reply.send(decision);
    }

    fn plan_key(&mut self, code: KeyCode) {
        let Some(Panel::Plan { index }) = self.panel.as_mut() else { return };
        let choice = match code {
            KeyCode::Up => {
                *index = index.saturating_sub(1);
                return;
            }
            KeyCode::Down => {
                *index = (*index + 1).min(PLAN_OPTIONS.len() - 1);
                return;
            }
            KeyCode::Char(digit @ '1'..='3') => digit as usize - '1' as usize,
            KeyCode::Esc => 2,
            KeyCode::Enter => *index,
            _ => return,
        };
        self.plan_action(choice);
    }

    fn plan_fresh_key(&mut self, code: KeyCode) {
        let Some(Panel::PlanFresh { index }) = self.panel.as_mut() else { return };
        match (code, *index) {
            (KeyCode::Up, _) => *index = index.saturating_sub(1),
            (KeyCode::Down, _) => *index = (*index + 1).min(PLAN_FRESH_ROWS.len() - 1),
            (KeyCode::Esc, _) => self.panel = Some(Panel::Plan { index: 0 }),
            (KeyCode::Enter, 0) => self.open_models(Setting::Model, "", ModelsBack::PlanFresh),
            (KeyCode::Enter | KeyCode::Right | KeyCode::Left, 1) => self.change_setting(Setting::Thinking, code != KeyCode::Left),
            (KeyCode::Enter, _) => self.start_fresh_plan(),
            _ => {}
        }
    }

    fn ask_key(&mut self, key: KeyEvent) {
        let Some(Panel::Ask(ask)) = self.panel.as_mut() else { return };
        if key.code == KeyCode::Char('v') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.ask_terminal(json!({"paste_image": true}));
        } else if ask.key(key) {
            self.panel = None;
        }
    }

    fn login_key(&mut self, key: KeyEvent) {
        let Some(Panel::Login(login)) = self.panel.as_mut() else { return };
        if key.code == KeyCode::Enter && login.ask_overwrite(&self.client.accounts()) {
            return;
        }
        let action = login.key(key);
        self.login_action(action);
    }

    fn trust_key(&mut self, code: KeyCode) {
        let Some(Panel::Trust { opened, .. }) = self.panel else { return };
        // A key typed before the question appeared must not answer it; it stays up, so even a decline waits.
        if opened.elapsed() < PERMISSION_SETTLE {
            return;
        }
        self.panel = None;
        match code {
            KeyCode::Char('y') | KeyCode::Enter => self.set_trust(true),
            _ => self.warn("trust", "Project files stay unloaded for now. /trust loads them."),
        }
    }

    pub(super) fn render_panel(&self, panel: &Panel, width: usize) -> Vec<Line> {
        match panel {
            Panel::Resume(resume) => self.resume_panel(resume, width),
            Panel::Running { index, sessions, confirm } => self.running_panel(index, sessions, confirm, width),
            Panel::Settings { row, query } => self.settings_panel(row, query, width),
            Panel::Models { row, query, index, .. } => self.models_panel(row, query, index, width),
            Panel::ContextWindows { tab, query, index } => self.context_windows_panel(tab, query, index, width),
            Panel::Headroom { row, input } => self.headroom_panel(row, input, width),
            Panel::ProviderOrder { tab, index } => self.provider_order_panel(tab, index, width),
            Panel::Profiles { index, naming, confirm } => self.profiles_panel(index, naming, confirm, width),
            Panel::Environments { place, index, naming, confirm } => self.environments_panel(place, index, naming, confirm, width),
            Panel::ProfileSkills { profile, query, index } => self.profile_skills_panel(profile, query, index, width),
            Panel::Permission { request, index, .. } => permission_lines(request, *index, width),
            Panel::Ask(ask) => ask.render(width),
            Panel::Todos { index } => self.todos_panel(index, width),
            Panel::Plugins { index, confirm } => self.plugins_panel(index, confirm, width),
            Panel::Skills { query, index } => self.skills_panel(query, index, width),
            Panel::Mcp { index, confirm } => self.mcp_panel(index, confirm, width),
            Panel::Usage { tab, totals } => self.usage_panel(tab, totals, width),
            Panel::PluginSettings { plugin, index, edit } => self.plugin_settings_panel(plugin, index, edit, width),
            Panel::Plugin { plugin, data, index } => self.plugin_panel(plugin, data, index, width),
            Panel::Agents { .. } => Vec::new(),
            Panel::Tasks { agents, index } => tasks_panel(&self.terminals.lock().unwrap().list, &self.subagents.lock().unwrap().list, &self.client.catalog(), *agents, *index, width),
            Panel::Workflows { .. } | Panel::WorkflowRun { .. } | Panel::WorkflowSave { .. } => self.workflow_panel(width),
            Panel::Terminal { .. } | Panel::Diff { .. } => unreachable!("frame draws the full-screen view instead of a panel"),
            Panel::Btw => self.btw.as_ref().expect("the btw panel opens with a thread").render(width),
            Panel::Trust { root, files, .. } => self.trust_panel(root, files, width),
            Panel::Worktrees(worktrees) => self.worktrees_panel(worktrees, width),
            Panel::Login(login) => login.render(width, PANEL_ROWS),
            Panel::Accounts { logout, query, index } => self.accounts_panel(logout, query, index, width),
            Panel::History(history) => self.history_panel(history, width),
            Panel::Rewind { index, points } => self.rewind_panel(index, points, width),
            Panel::Changes { files, index } => self.changes_panel(files, index, width),
            Panel::Plan { index } => plan_lines(*index),
            Panel::PlanFresh { index } => self.plan_fresh_panel(index, width),
        }
    }

    fn plan_fresh_panel(&self, index: &usize, width: usize) -> Vec<Line> {
        let [model, effort] = [Setting::Model, Setting::Thinking].map(|row| self.setting_value(row));
        let rights = [model, effort, Vec::new()];
        let rows: Vec<list::Row> = PLAN_FRESH_ROWS.iter().zip(rights).map(|(left, right)| list::Row { left: left.to_string(), right }).collect();
        let view = list::List {
            title: PLAN_OPTIONS[0].into(),
            tabs: &[],
            tab: 0,
            search: None,
            rows: &rows,
            selected: *index,
            empty: "",
            hint: "↑↓ navigate     enter pick model / start     ←→ change effort     esc back",
        };
        view.render(width, PANEL_ROWS)
    }

    fn trust_panel(&self, root: &Path, files: &[PathBuf], width: usize) -> Vec<Line> {
        let rows: Vec<list::Row> = files.iter().map(|file| list::Row { left: file.strip_prefix(root).unwrap_or(file).display().to_string(), right: Vec::new() }).collect();
        let view = list::List {
            title: "Trust this project? Its files can instruct the model and run commands.".into(),
            tabs: &[],
            tab: 0,
            search: None,
            rows: &rows,
            selected: 0,
            empty: "",
            hint: "y or enter trust     any other key not now",
        };
        view.render(width, PANEL_ROWS)
    }
}
