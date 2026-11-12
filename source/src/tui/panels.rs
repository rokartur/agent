use super::*;

/// The bottom panel that takes over the keys while it is open.
pub(super) enum Panel {
    Resume {
        all: bool,
        index: usize,
        sessions: Vec<SessionInfo>,
    },
    /// /attach: the other sessions running on this session's computer.
    Running {
        index: usize,
        sessions: Vec<Peer>,
        /// `x` was pressed once on the selected session; a second `x` stops it.
        confirm: bool,
    },
    /// `row` indexes `SETTING_NAMES`; its section is the open tab. Typing filters all sections by name.
    Settings {
        row: usize,
        query: Editor,
    },
    /// Searchable model list for the settings `row` it edits; Enter and Esc return to `back`.
    Models {
        row: usize,
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
        row: usize,
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
    /// `confirm`: d was pressed once, and the next d removes the selected worktree.
    Worktrees {
        confirm: bool,
        index: usize,
        worktrees: Vec<Worktree>,
        current: PathBuf,
    },
    Login(login::Login),
    /// /account pins the chosen account, /logout removes it. Typing filters by name or provider.
    Accounts {
        logout: bool,
        query: Editor,
        index: usize,
    },
    /// /history: saved prompts, newest first, in the `history::SCOPES` tab `tab`. Typing filters, Enter puts the prompt in the editor.
    History {
        entries: Vec<history::Entry>,
        tab: usize,
        query: Editor,
        index: usize,
    },
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

pub(super) const SETTING_NAMES: [&str; 40] = [
    "Model",
    "Thinking level",
    "Permission mode",
    "Auto-compact",
    "Compaction reserve",
    "Keep recent context",
    "Expand tool output",
    "Condense tool output",
    "Condense model",
    "Condense min batch",
    "Switch account at",
    "When all over limit",
    "Fallback model",
    "Context window",
    "Ponytail",
    "Caveman",
    "rtk",
    "Headroom proxy",
    "Subagent effort",
    "Notifications",
    "Scroll lines",
    "Copy on select",
    "Update channel",
    "Menu rows",
    "Auto prune sessions",
    "Provider order",
    "Name conflicts prefer",
    "Speed",
    "Prompt cache TTL",
    "Reviewer model",
    "Subagent model",
    "Cost in footer",
    "Account in footer",
    "Progress bar",
    "Headroom URL",
    "Headroom token",
    "Messages while working",
    "Message threads",
    "Small model",
    "Output style",
];
/// The settings panel's tabs with their rows of `SETTING_NAMES`, in the order it lists them.
pub(super) const SETTING_SECTIONS: [(&str, &[usize]); 5] = [
    ("Model", &[0, 1, 27, 13, 2, 18, 30, 29, 38]),
    ("Context", &[3, 4, 5, 7, 8, 9, 28, 37]),
    ("Accounts", &[10, 11, 12, 25]),
    ("Modes", &[39, 14, 15, 16, 17, 34, 35, 26]),
    ("Interface", &[6, 31, 32, 19, 33, 20, 23, 21, 22, 24, 36]),
];
/// The settings tab after `SETTING_SECTIONS`, drawn by `Panel::Profiles`.
pub(super) const PROFILES_TAB: usize = SETTING_SECTIONS.len();
pub(super) const MODEL_ROWS: [usize; 6] = [0, 8, 12, 29, 30, 38];
pub(super) const CONTEXT_WINDOW_ROW: usize = 13;
pub(super) const PROVIDER_ORDER_ROW: usize = 25;
pub(super) const HEADROOM_URL_ROW: usize = 34;
pub(super) const HEADROOM_TOKEN_ROW: usize = 35;
pub(super) const PROVIDER_LISTS: [&[&str]; 2] = [&models::PROVIDERS, &crate::tools::web::SEARCH_PROVIDERS];
pub(super) const CONTEXT_WINDOWS: [Option<u64>; 5] = [None, Some(200_000), Some(272_000), Some(400_000), Some(1_000_000)];
pub(super) const RESERVE_TOKENS: [u64; 5] = [16_384, 32_768, 65_536, 100_000, 150_000];
pub(super) const KEEP_RECENT_TOKENS: [u64; 4] = [10_000, 20_000, 40_000, 80_000];
pub(super) const CONDENSE_MIN_CHARS: [u64; 4] = [10_000, 20_000, 40_000, 80_000];
// 0 = never switch accounts.
pub(super) const SWITCH_AT_PERCENT: [u64; 6] = [70, 80, 90, 95, 100, 0];

impl App {
    pub(super) fn plugin_settings_key(&mut self, key: KeyEvent) {
        let Some(Panel::PluginSettings { plugin, index, edit }) = self.panel.as_mut() else { return };
        let plugin = plugin.clone();
        let declared = self.plugins.list().into_iter().find(|view| view.name == plugin).map(|view| view.settings).unwrap_or_default();
        let Some(entry) = declared.get(*index).cloned() else {
            if key.code == KeyCode::Esc {
                self.panel = Some(Panel::Plugins { index: 0, confirm: false });
            }
            return;
        };
        let key_name = entry["key"].as_str().unwrap_or_default().to_string();
        let value = match (key.code, edit.as_mut()) {
            (KeyCode::Esc, Some(_)) => {
                *edit = None;
                return;
            }
            (KeyCode::Enter, Some(editor)) => {
                let text = editor.text();
                *edit = None;
                json!(text)
            }
            (_, Some(editor)) => {
                editor.edit_key(key);
                return;
            }
            (KeyCode::Esc, None) => {
                self.panel = Some(Panel::Plugins { index: 0, confirm: false });
                return;
            }
            (KeyCode::Up, None) => {
                *index = index.saturating_sub(1);
                return;
            }
            (KeyCode::Down, None) => {
                *index = (*index + 1).min(declared.len() - 1);
                return;
            }
            (KeyCode::Enter | KeyCode::Char(' '), None) => match crate::plugins::setting(&entry, &self.settings.plugin_settings[plugin.as_str()]) {
                Value::Bool(on) => json!(!on),
                value => {
                    let mut editor = Editor::new();
                    editor.set(value.as_str().unwrap_or_default());
                    *edit = Some(editor);
                    return;
                }
            },
            _ => return,
        };
        self.update_settings(|s| {
            if !s.plugin_settings[plugin.as_str()].is_object() {
                s.plugin_settings[plugin.as_str()] = json!({});
            }
            s.plugin_settings[plugin.as_str()][key_name.as_str()] = value;
        });
        self.plugins.sync(&self.settings.disabled_plugins, &self.settings.plugin_settings);
    }

    /// Keys while a panel is open: Esc and Ctrl+C close it, the rest belongs to the panel.
    pub(super) fn panel_key(&mut self, key: KeyEvent) {
        let ctrl_c = key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c');
        let key = if ctrl_c { KeyEvent::from(KeyCode::Esc) } else { key };
        let code = key.code;
        if let Some(Panel::Permission { index, opened, .. }) = self.panel.as_mut() {
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
            return;
        }
        if let Some(Panel::Plan { index }) = self.panel.as_mut() {
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
            return;
        }
        if let Some(Panel::PlanFresh { index }) = self.panel.as_mut() {
            match (code, *index) {
                (KeyCode::Up, _) => *index = index.saturating_sub(1),
                (KeyCode::Down, _) => *index = (*index + 1).min(PLAN_FRESH_ROWS.len() - 1),
                (KeyCode::Esc, _) => self.panel = Some(Panel::Plan { index: 0 }),
                (KeyCode::Enter, 0) => self.open_models(0, "", ModelsBack::PlanFresh),
                (KeyCode::Enter | KeyCode::Right | KeyCode::Left, 1) => self.change_setting(1, code != KeyCode::Left),
                (KeyCode::Enter, _) => self.start_fresh_plan(),
                _ => {}
            }
            return;
        }
        if let Some(Panel::Ask(ask)) = self.panel.as_mut() {
            if key.code == KeyCode::Char('v') && key.modifiers.contains(KeyModifiers::CONTROL) {
                self.ask_terminal(json!({"paste_image": true}));
            } else if ask.key(key) {
                self.panel = None;
            }
            return;
        }
        if let Some(Panel::Login(login)) = self.panel.as_mut() {
            if key.code == KeyCode::Enter && login.ask_overwrite(&self.client.accounts()) {
                return;
            }
            let action = login.key(key);
            self.login_action(action);
            return;
        }
        if let Some(Panel::Trust { opened, .. }) = self.panel {
            // A key typed before the question appeared must not answer it; it stays up, so even a decline waits.
            if opened.elapsed() < PERMISSION_SETTLE {
                return;
            }
            self.panel = None;
            match code {
                KeyCode::Char('y') | KeyCode::Enter => self.set_trust(true),
                _ => self.warn("trust", "Project files stay unloaded for now. /trust loads them."),
            }
            return;
        }
        match self.panel {
            Some(Panel::Btw) => return self.btw_key(key),
            Some(Panel::Models { .. }) => return self.models_key(key),
            Some(Panel::ContextWindows { .. }) => return self.context_windows_key(key),
            Some(Panel::ProviderOrder { .. }) => return self.provider_order_key(key),
            Some(Panel::Profiles { .. }) => return self.profiles_key(key),
            Some(Panel::Running { .. }) => return self.running_key(code),
            Some(Panel::Environments { .. }) => return self.environments_key(key),
            Some(Panel::ProfileSkills { .. }) => return self.profile_skills_key(key),
            Some(Panel::Headroom { .. }) => return self.headroom_key(key),
            Some(Panel::Diff { .. }) => return self.diff_key(code),
            Some(Panel::Terminal { .. }) => return self.terminal_key(code),
            Some(Panel::Workflows { .. } | Panel::WorkflowRun { .. } | Panel::WorkflowSave { .. }) => return self.workflow_key(code),
            Some(Panel::PluginSettings { .. }) => return self.plugin_settings_key(key),
            _ => {}
        }
        if let (KeyCode::Enter, Some(Panel::Plugins { index, .. })) = (code, &self.panel) {
            if let Some(view) = self.plugins.list().into_iter().nth(*index).filter(|view| view.installed) {
                self.panel = Some(Panel::PluginSettings { plugin: view.name, index: 0, edit: None });
            }
            return;
        }
        if let Some(Panel::Plugin { plugin, data, index }) = self.panel.as_mut() {
            let last = data["rows"].as_array().map_or(0, |rows| rows.len().saturating_sub(1));
            match code {
                KeyCode::Up => *index = index.saturating_sub(1),
                KeyCode::Down => *index = (*index + 1).min(last),
                KeyCode::Esc => self.panel = None,
                KeyCode::Enter => {
                    if let Some(id) = data["rows"].get(*index).map(|row| row["id"].clone()) {
                        let params = json!({"type": "panel_select", "panel": data["title"], "id": id});
                        let _ = self.commands.send(Command::PluginEvent(plugin.clone(), params));
                    }
                }
                _ => {}
            }
            return;
        }
        if code == KeyCode::Esc {
            self.panel = None;
            return;
        }
        let cwd = self.cwd.display().to_string();
        match self.panel.as_mut() {
            Some(Panel::Resume { all, index, sessions }) => {
                let visible: Vec<&SessionInfo> = sessions.iter().filter(|s| *all || s.cwd == cwd).collect();
                match code {
                    KeyCode::Up => *index = index.saturating_sub(1),
                    KeyCode::Down => *index = (*index + 1).min(visible.len().saturating_sub(1)),
                    KeyCode::Tab | KeyCode::BackTab => {
                        *all = !*all;
                        *index = 0;
                    }
                    KeyCode::Enter => {
                        let Some(session) = visible.get(*index) else { return };
                        let id = session.id.clone();
                        self.panel = None;
                        self.start(Command::Resume(id));
                    }
                    _ => {}
                }
            }
            Some(Panel::Settings { row, query }) => {
                let rows = matching_settings(*row, &query.text());
                let index = rows.iter().position(|r| r == row);
                match code {
                    KeyCode::Tab | KeyCode::BackTab => {
                        let count = PROFILES_TAB + 1;
                        let section = setting_section(*row);
                        let next = if code == KeyCode::Tab { (section + 1) % count } else { (section + count - 1) % count };
                        self.panel = Some(settings_tab(next));
                    }
                    KeyCode::Enter if *row == PROVIDER_ORDER_ROW && index.is_some() => self.panel = Some(Panel::ProviderOrder { tab: 0, index: 0 }),
                    KeyCode::Up if !rows.is_empty() => *row = rows[index.map_or(0, |i| i.saturating_sub(1))],
                    KeyCode::Down if !rows.is_empty() => *row = rows[index.map_or(0, |i| (i + 1).min(rows.len() - 1))],
                    KeyCode::Enter | KeyCode::Right | KeyCode::Left if index.is_some() => {
                        let row = *row;
                        self.change_setting(row, code != KeyCode::Left);
                    }
                    _ => {
                        if query.edit_key(key)
                            && let Some(&first) = matching_settings(*row, &query.text()).first()
                        {
                            *row = first;
                        }
                    }
                }
            }
            Some(Panel::Todos { index }) => {
                let mut todos = self.todos.lock().unwrap();
                let positions: Vec<usize> = (0..todos.tasks.len()).filter(|i| todos.tasks[*i].status != Status::Deleted).collect();
                *index = (*index).min(positions.len().saturating_sub(1));
                match code {
                    KeyCode::Up => *index = index.saturating_sub(1),
                    KeyCode::Down => *index = (*index + 1).min(positions.len().saturating_sub(1)),
                    KeyCode::Char(' ') if !positions.is_empty() => todos.cycle(positions[*index]),
                    KeyCode::Char('d') if !positions.is_empty() => todos.delete(positions[*index]),
                    _ => {}
                }
            }
            Some(Panel::Usage { tab, .. }) if matches!(code, KeyCode::Tab | KeyCode::BackTab) => {
                let tabs = USAGE_TABS.len();
                *tab = if code == KeyCode::Tab { (*tab + 1) % tabs } else { (*tab + tabs - 1) % tabs };
            }
            Some(Panel::Skills { query, index }) => match code {
                KeyCode::Up => *index = index.saturating_sub(1),
                KeyCode::Down => *index = (*index + 1).min(matching_skills(&self.skills, &query.text()).len().saturating_sub(1)),
                // Skill names and sources hold no spaces, so space stays the toggle while typing searches.
                KeyCode::Char(' ') => {
                    let Some(skill) = matching_skills(&self.skills, &query.text()).get(*index).copied() else { return };
                    let name = skill.name.clone();
                    self.update_settings(|s| {
                        if s.disabled_skills.contains(&name) {
                            s.disabled_skills.retain(|disabled| *disabled != name);
                        } else {
                            s.disabled_skills.push(name);
                        }
                    });
                }
                KeyCode::Enter => {
                    let Some(source) = matching_skills(&self.skills, &query.text()).get(*index).and_then(|skill| skill.source.clone()) else { return };
                    let mut settings = self.settings.clone();
                    settings.toggle_disabled_repo(&source, &self.skills);
                    self.update_settings(|s| *s = settings);
                }
                _ => {
                    if query.edit_key(key) {
                        *index = 0;
                    }
                }
            },
            Some(Panel::Mcp { index, confirm }) => {
                let (views, _) = self.mcp.list();
                *index = (*index).min(views.len().saturating_sub(1));
                let Some(view) = views.get(*index) else { return };
                match code {
                    KeyCode::Up => (*index, *confirm) = (index.saturating_sub(1), false),
                    KeyCode::Down => (*index, *confirm) = ((*index + 1).min(views.len() - 1), false),
                    KeyCode::Char('r') => self.mcp.reconnect(&view.name),
                    KeyCode::Char('l') if view.http => self.mcp.login(&view.name),
                    // An entry the panel cuts is approved on the second press, after the user has read the file.
                    KeyCode::Char(' ') if view.approval.is_some() && !*confirm && mcp_entry(&view.entry, self.size.0).1 => *confirm = true,
                    // A server `mcp.json` turns off stays off; only the file can turn it on.
                    KeyCode::Char(' ') if view.enabled => {
                        *confirm = false;
                        let name = view.name.clone();
                        let approval = view.approval.clone();
                        self.update_settings(|s| match approval {
                            Some(key) => {
                                s.approved_mcp.push(key);
                                s.disabled_mcp.retain(|disabled| *disabled != name);
                            }
                            None if s.disabled_mcp.contains(&name) => s.disabled_mcp.retain(|disabled| *disabled != name),
                            None => s.disabled_mcp.push(name),
                        });
                        self.mcp.sync(&self.settings.disabled_mcp, &self.settings.approved_mcp);
                    }
                    _ => {}
                }
            }
            Some(Panel::Plugins { index, confirm }) => {
                let views = self.plugins.list();
                *index = (*index).min(views.len().saturating_sub(1));
                let Some(view) = views.get(*index) else { return };
                let confirmed = std::mem::take(confirm);
                let changed = match code {
                    KeyCode::Up => {
                        *index = index.saturating_sub(1);
                        return;
                    }
                    KeyCode::Down => {
                        *index = (*index + 1).min(views.len() - 1);
                        return;
                    }
                    KeyCode::Char(' ') if !view.installed => crate::plugins::install(&self.home, &view.name).map(|_| ()),
                    KeyCode::Char(' ') => {
                        let name = view.name.clone();
                        self.update_settings(|s| {
                            if s.disabled_plugins.contains(&name) {
                                s.disabled_plugins.retain(|disabled| *disabled != name);
                            } else {
                                s.disabled_plugins.push(name);
                            }
                        });
                        Ok(())
                    }
                    // Removing deletes the state the plugin keeps next to its code, so it takes a second press.
                    KeyCode::Char('d') if view.installed && !confirmed => {
                        *confirm = true;
                        return;
                    }
                    KeyCode::Char('d') if view.installed => crate::plugins::remove(&self.home, &view.name),
                    KeyCode::Char('u') if view.installed => crate::plugins::update(&self.home, std::slice::from_ref(&view.name)).map(|notices| {
                        for notice in notices {
                            self.system("plugins", notice);
                        }
                        // `sync` starts only stopped plugins, so stop this one for the sync below to start the new copy.
                        let mut stopped = self.settings.disabled_plugins.clone();
                        stopped.push(view.name.clone());
                        self.plugins.sync(&stopped, &self.settings.plugin_settings);
                    }),
                    _ => return,
                };
                if let Err(error) = changed {
                    self.push(Item::Error(error));
                }
                self.plugins.sync(&self.settings.disabled_plugins, &self.settings.plugin_settings);
            }
            Some(Panel::Worktrees { index, worktrees, confirm, current }) => {
                let confirmed = std::mem::take(confirm);
                match code {
                    KeyCode::Up => *index = index.saturating_sub(1),
                    KeyCode::Down => *index = (*index + 1).min(worktrees.len() - 1),
                    KeyCode::Enter => {
                        let path = worktrees[*index].path.clone();
                        let moves = path != *current;
                        self.panel = None;
                        if moves {
                            self.start(Command::Worktree(path));
                        }
                    }
                    KeyCode::Char('d') if *index == 0 => self.warn("worktree", "The main worktree cannot be removed."),
                    KeyCode::Char('d') if worktrees[*index].path == *current => self.warn("worktree", "This session runs in that worktree; move it first."),
                    KeyCode::Char('d') if !confirmed => *confirm = true,
                    KeyCode::Char('d') => {
                        let path = worktrees[*index].path.clone();
                        self.panel = None;
                        // Deleting a checkout with node_modules takes seconds, so it runs off the UI thread.
                        self.system("worktree", format!("Removing {}", path.display()));
                        let (cwd, events) = (self.cwd.clone(), self.events.clone());
                        std::thread::spawn(move || {
                            let result = worktree::remove(&cwd, &path);
                            let _ = events.send(Msg::WorktreeRemoved(path, result));
                        });
                    }
                    _ => {}
                }
            }
            Some(Panel::Agents { row }) => {
                let workflows = footer_workflows(&self.workflows);
                let agents = footer_agents(&self.subagents.lock().unwrap());
                let pills = usize::from(self.terminals.lock().unwrap().running() > 0) + usize::from(agents > 0);
                let last = pills + workflows.len();
                let selected = (*row).min(last);
                let workflow = selected.checked_sub(1 + pills).map(|offset| workflows[offset].clone());
                match code {
                    KeyCode::Up if selected > 0 => *row = selected - 1,
                    KeyCode::Down => *row = (selected + 1).min(last),
                    KeyCode::Char('x') => {
                        if let Some(workflow) = workflow {
                            workflow.stop();
                        }
                    }
                    KeyCode::Enter if (1..=pills).contains(&selected) => self.open_tasks(selected == pills && agents > 0),
                    KeyCode::Enter => self.panel = workflow.map(|workflow| Panel::WorkflowRun { id: workflow.id.clone(), phase: None, index: 0 }),
                    KeyCode::Up | KeyCode::Esc => self.panel = None,
                    _ => {}
                }
            }
            Some(Panel::Tasks { agents, index }) => {
                let terminals = self.terminals.lock().unwrap().list.clone();
                let runs = self.subagents.lock().unwrap().list.clone();
                let len = if *agents { runs.len() } else { terminals.len() };
                match code {
                    KeyCode::Up => *index = index.saturating_sub(1),
                    KeyCode::Down => *index = (*index + 1).min(len.saturating_sub(1)),
                    KeyCode::Tab | KeyCode::BackTab | KeyCode::Left | KeyCode::Right => (*agents, *index) = (!*agents, 0),
                    _ if *index >= len => {}
                    KeyCode::Char('x') if *agents => runs[*index].stop(),
                    KeyCode::Char('x') => _ = terminals[*index].stop(),
                    KeyCode::Enter if *agents => {
                        let run = runs[*index].clone();
                        self.view_run(Some(run));
                    }
                    KeyCode::Enter => self.panel = Some(Panel::Terminal { index: *index, scroll: 0, loaded: None, lines: Vec::new() }),
                    _ => {}
                }
            }
            Some(Panel::Rewind { index, points }) => match code {
                KeyCode::Up => *index = index.saturating_sub(1),
                KeyCode::Down => *index = (*index + 1).min(points.len() - 1),
                KeyCode::Enter => {
                    let entry = rewind_entry(&self.entries, points, *index);
                    let point = points.swap_remove(*index);
                    self.panel = None;
                    let _ = self.commands.send(Command::Rewind { at: point.at, entry });
                    self.restore_draft(point);
                }
                _ => {}
            },
            Some(Panel::Changes { files, index }) => match code {
                KeyCode::Up => *index = index.saturating_sub(1),
                KeyCode::Down => *index = (*index + 1).min(files.len() - 1),
                KeyCode::Enter => {
                    let (files, index) = (std::mem::take(files), *index);
                    self.panel = Some(Panel::Diff { files, index, scroll: DIFF_TOP });
                }
                _ => {}
            },
            Some(Panel::History { entries, tab, query, index }) => {
                let scope = *tab;
                let matches = matching_prompts(entries, &query.text(), |entry| entry.in_scope(scope, &self.cwd, &self.session_id));
                match code {
                    KeyCode::Up => *index = index.saturating_sub(1),
                    KeyCode::Down => *index = (*index + 1).min(matches.len().saturating_sub(1)),
                    KeyCode::Enter => {
                        let Some(prompt) = matches.get(*index).map(|prompt| prompt.to_string()) else { return };
                        self.panel = None;
                        self.editor.set(&prompt);
                        // The old selection may reach rows a shorter prompt no longer has.
                        self.input_selection = Selection::default();
                    }
                    KeyCode::Tab | KeyCode::BackTab => {
                        let tabs = history::SCOPES.len();
                        *tab = if code == KeyCode::Tab { (*tab + 1) % tabs } else { (*tab + tabs - 1) % tabs };
                        *index = 0;
                    }
                    _ => {
                        if query.edit_key(key) {
                            *index = 0;
                        }
                    }
                }
            }
            Some(Panel::Accounts { logout, query, index }) => {
                let accounts = matching_accounts(self.client.accounts(), &query.text());
                let logout = *logout;
                match code {
                    KeyCode::Up | KeyCode::Down if !logout && key.modifiers.contains(KeyModifiers::SHIFT) => {
                        let Some(account) = accounts.get(*index) else { return };
                        if let Err(error) = self.client.move_account(&account.provider, &account.name, code == KeyCode::Up) {
                            return self.push(Item::Error(error));
                        }
                        let moved = matching_accounts(self.client.accounts(), &query.text());
                        *index = moved.iter().position(|a| a.provider == account.provider && a.name == account.name).unwrap_or(*index);
                    }
                    KeyCode::Up => *index = index.saturating_sub(1),
                    KeyCode::Down => *index = (*index + 1).min(accounts.len().saturating_sub(1)),
                    KeyCode::Tab if !logout => {
                        let events = self.events.clone();
                        self.client.check_accounts(move || {
                            let _ = events.send(Msg::UsageFetched);
                        });
                    }
                    KeyCode::Enter => {
                        let Some(account) = accounts.get(*index) else { return };
                        self.panel = None;
                        if !logout {
                            // For this session only, like a resumed session's account: /settings saves only the keys it changes.
                            self.settings.default_accounts.insert(account.provider.clone(), account.name.clone());
                            self.client.configure(&self.settings);
                            let _ = self.commands.send(Command::AccountPicked);
                            self.system(
                                "account",
                                format!(
                                    "This session uses {} account {}{}.",
                                    account.provider,
                                    account.name,
                                    if self.settings.switch_at_percent == 0 { String::new() } else { format!(", until it reaches {}%", self.settings.switch_at_percent) }
                                ),
                            );
                            return;
                        }
                        match self.client.remove(&account.provider, &account.name) {
                            Ok(()) => {
                                self.system("logout", format!("Removed {}.", account.name));
                                self.accounts_changed();
                            }
                            Err(error) => self.push(Item::Error(error)),
                        }
                    }
                    _ => {
                        if query.edit_key(key) {
                            *index = 0;
                        }
                    }
                }
            }
            Some(
                Panel::Permission { .. }
                | Panel::Ask(_)
                | Panel::Btw
                | Panel::Login(_)
                | Panel::Models { .. }
                | Panel::ContextWindows { .. }
                | Panel::ProviderOrder { .. }
                | Panel::Profiles { .. }
                | Panel::Running { .. }
                | Panel::Environments { .. }
                | Panel::ProfileSkills { .. }
                | Panel::Headroom { .. }
                | Panel::Terminal { .. }
                | Panel::Workflows { .. }
                | Panel::WorkflowRun { .. }
                | Panel::WorkflowSave { .. }
                | Panel::Diff { .. }
                | Panel::Plan { .. }
                | Panel::PlanFresh { .. }
                | Panel::PluginSettings { .. }
                | Panel::Plugin { .. }
                | Panel::Trust { .. }
                | Panel::Usage { .. },
            )
            | None => {}
        }
    }

    pub(super) fn models_key(&mut self, key: KeyEvent) {
        let Some(Panel::Models { row, query, index, back }) = self.panel.as_mut() else { return };
        let row = *row;
        let back = match back {
            ModelsBack::Close => None,
            ModelsBack::Settings => Some(Panel::Settings { row, query: Editor::new() }),
            ModelsBack::PlanFresh => Some(Panel::PlanFresh { index: 0 }),
        };
        let catalog = self.client.catalog();
        let matches = matching_models(&catalog, &self.settings, row, &query.text());
        match key.code {
            KeyCode::Esc => self.panel = back,
            KeyCode::Up => *index = index.saturating_sub(1),
            KeyCode::Down => *index = (*index + 1).min(matches.len().saturating_sub(1)),
            KeyCode::Enter => {
                let Some(model) = matches.get(*index) else { return };
                let id = model.map_or(String::new(), |m| m.id.clone());
                self.update_settings(|s| *model_setting(s, row) = id);
                self.panel = back;
            }
            KeyCode::Tab => {
                let Some(Some(model)) = matches.get(*index) else { return };
                let key = model_key(model);
                let query = query.text();
                self.update_settings(|s| match s.pinned_models.iter().position(|pinned| *pinned == key) {
                    Some(at) => _ = s.pinned_models.remove(at),
                    None => s.pinned_models.push(key.clone()),
                });
                let catalog = self.client.catalog();
                let matches = matching_models(&catalog, &self.settings, row, &query);
                let Some(Panel::Models { index, .. }) = self.panel.as_mut() else { return };
                *index = matches.iter().position(|m| m.is_some_and(|m| model_key(m) == key)).unwrap_or(0);
            }
            _ => {
                if query.edit_key(key) {
                    *index = 0;
                }
            }
        }
    }

    pub(super) fn open_context_windows(&mut self) {
        let Some(current) = self.loaded_model_key() else { return };
        let index = context_window_targets(&self.client.catalog(), &self.settings, 1, "").iter().position(|(key, _)| *key == current).unwrap_or(0);
        self.panel = Some(Panel::ContextWindows { tab: 1, query: Editor::new(), index });
    }

    pub(super) fn headroom_key(&mut self, key: KeyEvent) {
        let Some(Panel::Headroom { row, input }) = self.panel.as_mut() else { return };
        let row = *row;
        match key.code {
            KeyCode::Esc => {}
            KeyCode::Enter if row == HEADROOM_URL_ROW => {
                let url = input.text().trim().trim_end_matches('/').to_string();
                self.update_settings(|s| s.headroom_url = url);
            }
            KeyCode::Enter => {
                let token = input.text().trim().to_string();
                self.update_settings(|s| s.headroom_token = token);
            }
            _ => {
                input.edit_key(key);
                return;
            }
        }
        self.panel = Some(Panel::Settings { row, query: Editor::new() });
    }

    pub(super) fn context_windows_key(&mut self, key: KeyEvent) {
        let Some(Panel::ContextWindows { tab, query, index }) = self.panel.as_mut() else { return };
        let catalog = self.client.catalog();
        let targets = context_window_targets(&catalog, &self.settings, *tab, &query.text());
        match key.code {
            KeyCode::Esc => self.panel = Some(Panel::Settings { row: CONTEXT_WINDOW_ROW, query: Editor::new() }),
            KeyCode::Tab | KeyCode::BackTab => {
                *tab = 1 - *tab;
                *index = 0;
            }
            KeyCode::Up => *index = index.saturating_sub(1),
            KeyCode::Down => *index = (*index + 1).min(targets.len().saturating_sub(1)),
            KeyCode::Left | KeyCode::Right => {
                let Some((target, _)) = targets.into_iter().nth(*index) else { return };
                let forward = key.code == KeyCode::Right;
                self.update_settings(|s| {
                    match step(&CONTEXT_WINDOWS, &s.context_windows.get(&target).copied(), forward) {
                        Some(window) => s.context_windows.insert(target, window),
                        None => s.context_windows.remove(&target),
                    };
                });
            }
            _ => {
                if *tab == 1 && query.edit_key(key) {
                    *index = 0;
                }
            }
        }
    }

    pub(super) fn profiles_key(&mut self, key: KeyEvent) {
        let Some(Panel::Profiles { index, naming, confirm }) = self.panel.as_mut() else { return };
        let selected = self.settings.skill_profiles.keys().nth(*index).cloned();
        if let Some(input) = naming {
            match key.code {
                KeyCode::Esc => *naming = None,
                KeyCode::Enter => {
                    let name = input.text().split_whitespace().collect::<Vec<_>>().join("-");
                    *naming = None;
                    self.name_profile(selected, name);
                }
                _ => _ = input.edit_key(key),
            }
            return;
        }
        let deleting = std::mem::take(confirm);
        match (key.code, selected) {
            (KeyCode::Esc, _) => self.panel = None,
            (KeyCode::Tab, _) => self.panel = Some(settings_tab(0)),
            (KeyCode::BackTab, _) => self.panel = Some(settings_tab(PROFILES_TAB - 1)),
            (KeyCode::Up, _) => *index = index.saturating_sub(1),
            (KeyCode::Down, _) => *index = (*index + 1).min(self.settings.skill_profiles.len()),
            (KeyCode::Enter, None) => *naming = Some(Editor::new()),
            (KeyCode::Enter, Some(profile)) => self.panel = Some(Panel::ProfileSkills { profile, query: Editor::new(), index: 0 }),
            (KeyCode::Left | KeyCode::Right, Some(profile)) => self.update_settings(|s| s.toggle_profile(&profile)),
            (KeyCode::Char('r'), Some(profile)) => {
                let mut input = Editor::new();
                input.set(&profile);
                *naming = Some(input);
            }
            (KeyCode::Char('d'), Some(profile)) if deleting => self.update_settings(|s| s.delete_profile(&profile)),
            (KeyCode::Char('d'), Some(_)) => *confirm = true,
            _ => {}
        }
    }

    pub(super) fn environments_key(&mut self, key: KeyEvent) {
        let Some(Panel::Environments { place, index, naming, confirm }) = self.panel.as_mut() else { return };
        if let Some(input) = naming {
            match key.code {
                KeyCode::Esc => *naming = None,
                KeyCode::Enter => {
                    let name = input.text().trim().to_string();
                    *naming = None;
                    self.add_environment(name);
                }
                _ => _ = input.edit_key(key),
            }
            return;
        }
        let editable = place.environment == crate::tailnet::LOCAL;
        let selected = place.environments.get(*index).cloned();
        let saved = selected.as_ref().is_some_and(|name| place.saved.contains(name));
        let removing = std::mem::take(confirm);
        match (key.code, selected) {
            (KeyCode::Esc, _) => self.panel = None,
            (KeyCode::Up, _) => *index = index.saturating_sub(1),
            (KeyCode::Down, _) => *index = (*index + 1).min(place.environments.len() - usize::from(!editable)),
            (KeyCode::Enter, None) => *naming = Some(Editor::new()),
            (KeyCode::Enter, Some(name)) if name != place.environment => {
                self.panel = None;
                self.switch_environment(&name);
            }
            (KeyCode::Char('d'), Some(name)) if saved => {
                if !removing {
                    *confirm = true;
                } else if editable {
                    self.update_settings(|s| s.environments.retain(|saved| *saved != name));
                    self.refresh_environments();
                } else {
                    // The settings here are the session host's; the terminal's computer removes it from its own.
                    self.ask_terminal(json!({"forget": name}));
                }
            }
            _ => {}
        }
    }

    pub(super) fn add_environment(&mut self, name: String) {
        if name.is_empty() {
            return;
        }
        if name.contains(char::is_whitespace) {
            return self.push(Item::Error(format!("{name} is not a Tailscale name: those hold no spaces.")));
        }
        if !self.settings.environments.contains(&name) {
            self.update_settings(|s| s.environments.push(name.clone()));
        }
        self.refresh_environments();
        if let Some(Panel::Environments { place, index, .. }) = self.panel.as_mut() {
            *index = place.environments.iter().position(|environment| *environment == name).expect("a saved environment is listed");
        }
    }

    /// Lists the environments again after the saved ones changed here, in a local session.
    pub(super) fn refresh_environments(&mut self) {
        let Some(id) = self.typing_client else { return };
        let place = wire::Place { environment: crate::tailnet::LOCAL.into(), environments: crate::tailnet::environments(&self.settings), saved: self.settings.environments.clone() };
        self.replace_place(id, place);
    }

    /// Shows the client's new place in the panel and keeps it for its next /env.
    pub(super) fn replace_place(&mut self, id: u64, place: wire::Place) {
        if let Some(Panel::Environments { place: shown, index, .. }) = self.panel.as_mut()
            && self.typing_client == Some(id)
        {
            // Only a local session lists the row that adds one, past the environments.
            *index = (*index).min(place.environments.len() - usize::from(place.environment != crate::tailnet::LOCAL));
            *shown = place.clone();
        }
        if let Some(client) = self.clients.iter_mut().find(|client| client.id == id) {
            client.place = place;
        }
    }

    /// Creates the profile `name`, or renames `from` to it, and selects it.
    pub(super) fn name_profile(&mut self, from: Option<String>, name: String) {
        if name.is_empty() || from.as_ref() == Some(&name) {
            return;
        }
        if self.settings.skill_profiles.contains_key(&name) {
            return self.push(Item::Error(format!("A profile named {name} already exists.")));
        }
        self.update_settings(|s| s.rename_profile(from.as_deref(), &name));
        if let Some(Panel::Profiles { index, .. }) = self.panel.as_mut() {
            *index = self.settings.skill_profiles.keys().position(|profile| *profile == name).expect("the profile was just saved");
        }
    }

    pub(super) fn profile_skills_key(&mut self, key: KeyEvent) {
        let Some(Panel::ProfileSkills { profile, query, index }) = self.panel.as_mut() else { return };
        let profile = profile.clone();
        let matching = matching_skills(&self.skills, &query.text());
        let mut settings = self.settings.clone();
        match key.code {
            KeyCode::Esc => {
                let index = self.settings.skill_profiles.keys().position(|name| *name == profile).unwrap_or(0);
                self.panel = Some(Panel::Profiles { index, naming: None, confirm: false });
                return;
            }
            KeyCode::Up => *index = index.saturating_sub(1),
            KeyCode::Down => *index = (*index + 1).min(matching.len().saturating_sub(1)),
            // Skill names and sources hold no spaces, so space stays the toggle while typing searches.
            KeyCode::Char(' ') => {
                let Some(skill) = matching.get(*index) else { return };
                settings.toggle_profile_skill(&profile, skill, &self.skills);
            }
            KeyCode::Enter => {
                let Some(source) = matching.get(*index).and_then(|skill| skill.source.as_ref()) else { return };
                settings.toggle_profile_repo(&profile, source, &self.skills);
            }
            _ => {
                if query.edit_key(key) {
                    *index = 0;
                }
            }
        }
        if settings != self.settings {
            self.update_settings(|s| *s = settings);
        }
    }

    pub(super) fn provider_order_key(&mut self, key: KeyEvent) {
        let Some(Panel::ProviderOrder { tab, index }) = self.panel.as_mut() else { return };
        let providers = PROVIDER_LISTS[*tab];
        let last = providers.len() - 1;
        match key.code {
            KeyCode::Esc => self.panel = Some(Panel::Settings { row: PROVIDER_ORDER_ROW, query: Editor::new() }),
            KeyCode::Tab | KeyCode::BackTab => {
                *tab = 1 - *tab;
                *index = 0;
            }
            KeyCode::Up | KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) => {
                let (moved, up) = (*index, key.code == KeyCode::Up);
                *index = if up { moved.saturating_sub(1) } else { (moved + 1).min(last) };
                self.update_settings(|s| s.move_provider(providers, moved, up));
            }
            KeyCode::Up => *index = index.saturating_sub(1),
            KeyCode::Down => *index = (*index + 1).min(last),
            _ => {}
        }
    }

    pub(super) fn open_history(&mut self) {
        let mut entries = history::load(&self.home.join(".agents"));
        entries.reverse();
        self.panel = Some(Panel::History { entries, tab: 0, query: Editor::new(), index: 0 });
    }

    /// The model picker for settings `row`, on the current model when it matches `query`.
    pub(super) fn open_models(&mut self, row: usize, query: &str, back: ModelsBack) {
        let current = model_setting(&mut self.settings, row).clone();
        let index = matching_models(&self.client.catalog(), &self.settings, row, query).iter().position(|m| m.map_or("", |m| &m.id) == current).unwrap_or(0);
        self.panel = Some(Panel::Models { row, query: Editor::from_text(query), index, back });
    }

    /// Moves setting `row` to its next (or previous) value, then saves and applies it.
    pub(super) fn change_setting(&mut self, row: usize, forward: bool) {
        if MODEL_ROWS.contains(&row) {
            self.open_models(row, "", ModelsBack::Settings);
            return;
        }
        if row == CONTEXT_WINDOW_ROW {
            self.open_context_windows();
            return;
        }
        if row == HEADROOM_URL_ROW || row == HEADROOM_TOKEN_ROW {
            let mut input = Editor::new();
            input.set(if row == HEADROOM_URL_ROW { &self.settings.headroom_url } else { &self.settings.headroom_token });
            self.panel = Some(Panel::Headroom { row, input });
            return;
        }
        self.update_settings(|s| match row {
            1 => s.thinking = step(&THINKING_LEVELS, &s.thinking.as_str(), forward).to_string(),
            2 => s.permission_mode = step(&MODES, &s.permission_mode, forward),
            3 => s.auto_compact = !s.auto_compact,
            4 => s.reserve_tokens = step(&RESERVE_TOKENS, &s.reserve_tokens, forward),
            5 => s.keep_recent_tokens = step(&KEEP_RECENT_TOKENS, &s.keep_recent_tokens, forward),
            6 => s.expand_tools = !s.expand_tools,
            7 => s.condense = !s.condense,
            9 => s.condense_min_batch_chars = step(&CONDENSE_MIN_CHARS, &s.condense_min_batch_chars, forward),
            10 => s.switch_at_percent = step(&SWITCH_AT_PERCENT, &s.switch_at_percent, forward),
            11 => s.when_all_over_limit = step(&Policy::ALL, &s.when_all_over_limit, forward),
            14 => s.ponytail = step(&PONYTAIL_LEVELS, &s.ponytail.as_str(), forward).to_string(),
            15 => s.caveman = step(&CAVEMAN_LEVELS, &s.caveman.as_str(), forward).to_string(),
            16 => s.rtk = !s.rtk,
            17 => s.headroom = !s.headroom,
            18 => s.subagent_effort = step(&THINKING_LEVELS, &s.subagent_effort.as_str(), forward).to_string(),
            19 => s.notifications = !s.notifications,
            20 => s.scroll_lines = step(&SCROLL_LINES, &s.scroll_lines, forward),
            21 => s.copy_on_select = !s.copy_on_select,
            22 => s.update_channel = step(&crate::update::CHANNELS, &s.update_channel.as_str(), forward).to_string(),
            23 => s.menu_rows = step(&MENU_ROWS, &s.menu_rows, forward),
            24 => s.prune_sessions = step(&PRUNE_SESSIONS, &s.prune_sessions.as_str(), forward).to_string(),
            25 => s.provider_sort = step(&PROVIDER_SORTS, &s.provider_sort.as_str(), forward).to_string(),
            26 => s.conflict_priority = step(&CONFLICT_PRIORITIES, &s.conflict_priority.as_str(), forward).to_string(),
            27 => s.speed = step(&SPEEDS, &s.speed.as_str(), forward).to_string(),
            28 => s.cache_ttl = step(&CACHE_TTLS, &s.cache_ttl.as_str(), forward).to_string(),
            31 => s.footer_cost = !s.footer_cost,
            32 => s.footer_account = !s.footer_account,
            33 => s.progress_bar = !s.progress_bar,
            36 => s.mid_turn = step(&MID_TURN, &s.mid_turn.as_str(), forward).to_string(),
            37 => s.message_threads = !s.message_threads,
            39 => s.output_style = step(&OUTPUT_STYLES, &s.output_style.as_str(), forward).to_string(),
            _ => unreachable!("rows 0, 8, 12, 13, 29, 30 and 38 open their own panels"),
        });
    }

    pub(super) fn setting_values(&self) -> [Line; 40] {
        let s = &self.settings;
        let condense_model = if s.condense_model.is_empty() { "auto" } else { &s.condense_model };
        let fallback_model = if s.fallback_model.is_empty() { "auto" } else { &s.fallback_model };
        let reviewer_model = if s.reviewer_model.is_empty() { "auto" } else { &s.reviewer_model };
        let subagent_model = if s.subagent_model.is_empty() { "auto" } else { &s.subagent_model };
        let small_model = if s.small_model.is_empty() { "auto" } else { &s.small_model };
        let model = self.model();
        let model = model.as_ref();
        [
            vec![span(model.map_or(LOADING_MODELS.into(), |m| format!("{} · {}", m.provider, m.id)), BRIGHT.bold())],
            choices(THINKING_LEVELS.map(String::from), &s.thinking),
            choices(MODES.map(|m| m.label().to_string()), s.permission_mode.label()),
            on_off_choices(s.auto_compact),
            choices(RESERVE_TOKENS.map(tokens), &tokens(s.reserve_tokens)),
            choices(KEEP_RECENT_TOKENS.map(tokens), &tokens(s.keep_recent_tokens)),
            on_off_choices(s.expand_tools),
            on_off_choices(s.condense),
            vec![span(condense_model, BRIGHT.bold())],
            choices(CONDENSE_MIN_CHARS.map(|n| n.to_string()), &s.condense_min_batch_chars.to_string()),
            choices(SWITCH_AT_PERCENT.map(switch_label), &switch_label(s.switch_at_percent)),
            choices(Policy::ALL.map(|p| p.label().to_string()), s.when_all_over_limit.label()),
            vec![span(fallback_model, BRIGHT.bold())],
            match model {
                Some(model) => vec![span(tokens(s.context_window(model)), BRIGHT.bold()), span(format!("  {}", window_source(s, model)), MUTED)],
                None => vec![span(LOADING_MODELS, MUTED)],
            },
            choices(PONYTAIL_LEVELS.map(String::from), &s.ponytail),
            choices(CAVEMAN_LEVELS.map(String::from), &s.caveman),
            optional_tool(s.rtk, "rtk"),
            if s.headroom_url.is_empty() { optional_tool(s.headroom, "headroom") } else { on_off_choices(s.headroom) },
            choices(THINKING_LEVELS.map(String::from), &s.subagent_effort),
            on_off_choices(s.notifications),
            choices(SCROLL_LINES.map(|n| n.to_string()), &s.scroll_lines.to_string()),
            on_off_choices(s.copy_on_select),
            choices(crate::update::CHANNELS.map(String::from), &s.update_channel),
            choices(MENU_ROWS.map(|n| n.to_string()), &s.menu_rows.to_string()),
            choices(PRUNE_SESSIONS.map(String::from), &s.prune_sessions),
            [choices(PROVIDER_SORTS.map(String::from), &s.provider_sort), vec![span("   enter reorder", MUTED)]].concat(),
            choices(CONFLICT_PRIORITIES.map(String::from), &s.conflict_priority),
            choices(SPEEDS.map(String::from), &s.speed),
            choices(CACHE_TTLS.map(String::from), &s.cache_ttl),
            vec![span(reviewer_model, BRIGHT.bold())],
            vec![span(subagent_model, BRIGHT.bold())],
            on_off_choices(s.footer_cost),
            on_off_choices(s.footer_account),
            on_off_choices(s.progress_bar),
            match s.headroom_url.as_str() {
                "" => vec![span("local", BRIGHT.bold()), span("   enter edit", MUTED)],
                url => vec![span(url, BRIGHT.bold()), span("   enter edit", MUTED)],
            },
            vec![span(if s.headroom_token.is_empty() { "none" } else { "set" }, BRIGHT.bold()), span("   enter edit", MUTED)],
            choices(MID_TURN.map(String::from), &s.mid_turn),
            if s.message_threads && !self.client.message_threads() {
                [on_off_choices(true), vec![span("  unused behind headroom or ANTHROPIC_BASE_URL", MUTED)]].concat()
            } else {
                on_off_choices(s.message_threads)
            },
            vec![span(small_model, BRIGHT.bold())],
            choices(OUTPUT_STYLES.map(String::from), &s.output_style),
        ]
    }

    pub(super) fn render_panel(&self, panel: &Panel, width: usize) -> Vec<Line> {
        match panel {
            Panel::Resume { all, index, sessions } => {
                let cwd = self.cwd.display().to_string();
                let rows: Vec<panel::Row> = sessions
                    .iter()
                    .filter(|s| *all || s.cwd == cwd)
                    .enumerate()
                    .map(|(row, s)| {
                        let workspace = Path::new(&s.cwd).file_name().map_or(s.cwd.clone(), |n| n.to_string_lossy().into_owned());
                        let turns = if s.turns == 1 { "1 turn".to_string() } else { format!("{} turns", s.turns) };
                        let title: String = s.name.clone().unwrap_or_else(|| s.title.clone()).chars().take(40).collect();
                        let style = if row == *index { BRIGHT.bold() } else { MUTED };
                        panel::Row { left: title, right: vec![span(format!("{workspace} · {} · {turns}", age(s.modified)), style)] }
                    })
                    .collect();
                let view = panel::Panel {
                    title: format!("Sessions {}", rows.len()),
                    tabs: &["Current workspace", "All workspaces"],
                    tab: usize::from(*all),
                    search: None,
                    rows: &rows,
                    selected: *index,
                    empty: "No saved sessions.",
                    hint: "↑↓ navigate     tab scope     enter resume     esc close",
                };
                view.render(width, PANEL_ROWS)
            }
            Panel::Running { index, sessions, confirm } => {
                let rows: Vec<panel::Row> = sessions
                    .iter()
                    .enumerate()
                    .map(|(row, peer)| {
                        let name = if peer.title.is_empty() { &peer.session } else { &peer.title };
                        let state = if peer.busy { "working" } else { "idle" };
                        let style = if row == *index { BRIGHT.bold() } else { MUTED };
                        // Cards are files any process of this user writes, so their text could carry escape codes.
                        let printable = |text: &str| -> String { text.chars().filter(|c| !c.is_control()).collect() };
                        panel::Row { left: printable(name).chars().take(40).collect(), right: vec![span(format!("{} · {state}", printable(&peer.cwd)), style)] }
                    })
                    .collect();
                let hint = if *confirm { "x again stops it     any other key keeps it" } else { "↑↓ navigate     enter attach     x stop     esc close" };
                let view = panel::Panel { title: format!("Running {}", rows.len()), tabs: &[], tab: 0, search: None, rows: &rows, selected: *index, empty: "No other session is running.", hint };
                view.render(width, PANEL_ROWS)
            }
            Panel::Settings { row, query } => {
                let matches = matching_settings(*row, &query.text());
                let values = self.setting_values();
                let rows: Vec<panel::Row> = matches.iter().map(|&r| panel::Row { left: SETTING_NAMES[r].to_string(), right: values[r].clone() }).collect();
                let view = panel::Panel {
                    title: "Settings".into(),
                    tabs: &setting_tabs(),
                    tab: setting_section(*row),
                    search: Some(query.field(None)),
                    rows: &rows,
                    selected: matches.iter().position(|r| r == row).unwrap_or(0),
                    empty: "No matching settings.",
                    hint: "↑↓ navigate     ←→ change     tab section     type to search     esc close",
                };
                view.render(width, PANEL_ROWS)
            }
            Panel::ContextWindows { tab, query, index } => {
                let rows: Vec<panel::Row> = context_window_targets(&self.client.catalog(), &self.settings, *tab, &query.text())
                    .into_iter()
                    .map(|(target, model)| {
                        let mut right = choices(CONTEXT_WINDOWS.map(context_window_label), &context_window_label(self.settings.context_windows.get(&target).copied()));
                        let Some(model) = model else { return panel::Row { left: target, right } };
                        right.push(span(format!("   {} catalog {}", model.provider, tokens(model.context_window)), MUTED));
                        panel::Row { left: model.id.clone(), right }
                    })
                    .collect();
                let view = panel::Panel {
                    title: "Context window".into(),
                    tabs: &["Providers", "Models"],
                    tab: *tab,
                    search: (*tab == 1).then(|| query.field(None)),
                    rows: &rows,
                    selected: *index,
                    empty: "No matching models.",
                    hint: "↑↓ navigate     ←→ change     tab providers/models     type to search     esc back",
                };
                view.render(width, PANEL_ROWS)
            }
            Panel::Headroom { row, input } => {
                let token = *row == HEADROOM_TOKEN_ROW;
                let view = panel::Panel {
                    title: SETTING_NAMES[*row].into(),
                    tabs: &[],
                    tab: 0,
                    search: Some(input.field(token.then_some('•'))),
                    rows: &[],
                    selected: 0,
                    empty: if token { "The remote headroom's HEADROOM_PROXY_TOKEN, sent as x-headroom-proxy-token." } else { "Empty uses the local headroom." },
                    hint: "enter save     esc back",
                };
                view.render(width, PANEL_ROWS)
            }
            Panel::ProviderOrder { tab, index } => {
                let rows: Vec<panel::Row> = self.settings.sort_providers(PROVIDER_LISTS[*tab]).into_iter().map(|provider| panel::Row { left: provider.into(), right: vec![] }).collect();
                let view = panel::Panel {
                    title: "Provider order".into(),
                    tabs: &["Models", "Web search"],
                    tab: *tab,
                    search: None,
                    rows: &rows,
                    selected: *index,
                    empty: "No providers.",
                    hint: "↑↓ navigate     shift+↑↓ move     tab models/web search     esc back",
                };
                view.render(width, PANEL_ROWS)
            }
            Panel::Profiles { index, naming, confirm } => {
                let renaming = self.settings.skill_profiles.keys().nth(*index);
                if let Some(input) = naming {
                    let view = panel::Panel {
                        title: renaming.map_or("New profile".into(), |profile| format!("Rename {profile}")),
                        tabs: &[],
                        tab: 0,
                        search: Some(input.field(None)),
                        rows: &[],
                        selected: 0,
                        empty: "Its name runs it as /profile:<name>.",
                        hint: "enter save     esc cancel",
                    };
                    return view.render(width, PANEL_ROWS);
                }
                let mut rows: Vec<panel::Row> = self
                    .settings
                    .skill_profiles
                    .keys()
                    .map(|profile| {
                        let state = if self.settings.active_profiles.contains(profile) { "on" } else { "off" };
                        let mut right = choices(["off", "on"].map(String::from), state);
                        right.push(span(format!("     {}", self.profile_skills(profile)), MUTED));
                        panel::Row { left: profile.clone(), right }
                    })
                    .collect();
                rows.push(panel::Row { left: "New profile".into(), right: vec![span("enter name", MUTED)] });
                let hint = match renaming {
                    Some(profile) if *confirm => format!("d again deletes {profile}     any other key keeps it"),
                    Some(_) => "↑↓ navigate     ←→ on/off     enter edit skills     r rename     d delete     tab section     esc close".into(),
                    None => "↑↓ navigate     enter new profile     tab section     esc close".into(),
                };
                let view = panel::Panel { title: "Settings".into(), tabs: &setting_tabs(), tab: PROFILES_TAB, search: None, rows: &rows, selected: *index, empty: "", hint: &hint };
                view.render(width, PANEL_ROWS)
            }
            Panel::Environments { place, index, naming, confirm } => {
                if let Some(input) = naming {
                    let view = panel::Panel {
                        title: "New environment".into(),
                        tabs: &[],
                        tab: 0,
                        search: Some(input.field(None)),
                        rows: &[],
                        selected: 0,
                        empty: "The Tailscale name of a computer running `agent serve --tailnet`; offered even while it is offline.",
                        hint: "enter save     esc cancel",
                    };
                    return view.render(width, PANEL_ROWS);
                }
                let editable = place.environment == crate::tailnet::LOCAL;
                let saved = |environment: &String| place.saved.contains(environment);
                let mut rows: Vec<panel::Row> = place
                    .environments
                    .iter()
                    .map(|environment| {
                        let note = if *environment == place.environment {
                            "this session"
                        } else if saved(environment) {
                            "saved"
                        } else {
                            ""
                        };
                        panel::Row { left: environment.clone(), right: vec![span(note, MUTED)] }
                    })
                    .collect();
                if editable {
                    rows.push(panel::Row { left: "Add environment".into(), right: vec![span("enter name", MUTED)] });
                }
                let hint = match place.environments.get(*index) {
                    Some(environment) if *confirm => format!("d again removes {environment}     any other key keeps it"),
                    Some(environment) if *environment == place.environment => "↑↓ navigate     esc close".into(),
                    Some(environment) if saved(environment) => "↑↓ navigate     enter start a session there     d remove     esc close".into(),
                    Some(_) => "↑↓ navigate     enter start a session there     esc close".into(),
                    None => "↑↓ navigate     enter add environment     esc close".into(),
                };
                let view = panel::Panel { title: "Environments".into(), tabs: &[], tab: 0, search: None, rows: &rows, selected: *index, empty: "", hint: &hint };
                view.render(width, PANEL_ROWS)
            }
            Panel::ProfileSkills { profile, query, index } => {
                let entries = self.settings.skill_profiles.get(profile).map_or(&[][..], Vec::as_slice);
                let rows = self.skill_rows(&query.text(), width, |skill| {
                    if skill.source.as_ref().is_some_and(|source| entries.contains(source)) {
                        ("[x]", span("via repo", MUTED))
                    } else if entries.contains(&skill.name) {
                        ("[x]", span("", PLAIN))
                    } else {
                        ("[ ]", span("", PLAIN))
                    }
                });
                let view = panel::Panel {
                    title: format!("Profile {profile}"),
                    tabs: &[],
                    tab: 0,
                    search: Some(query.field(None)),
                    rows: &rows,
                    selected: *index,
                    empty: if self.skills.is_empty() { "No skills. Add one under ~/.agents/skills/<name>/SKILL.md." } else { "No matching skills." },
                    hint: "type to search name or owner/repo     ↑↓ navigate     space in/out     enter whole repo in/out     esc back",
                };
                view.render(width, PANEL_ROWS)
            }
            Panel::Models { row, query, index, .. } => {
                let rows: Vec<panel::Row> = matching_models(&self.client.catalog(), &self.settings, *row, &query.text())
                    .into_iter()
                    .map(|m| match m {
                        Some(m) => {
                            let pin = if self.settings.pinned_models.contains(&model_key(m)) { "★ " } else { "" };
                            panel::Row { left: format!("{pin}{}", m.id), right: vec![span(model_owners(m), MUTED)] }
                        }
                        None => panel::Row { left: "auto".into(), right: vec![] },
                    })
                    .collect();
                let view = panel::Panel {
                    title: SETTING_NAMES[*row].into(),
                    tabs: &[],
                    tab: 0,
                    search: Some(query.field(None)),
                    rows: &rows,
                    selected: *index,
                    empty: "No matching models.",
                    hint: "type to search     ↑↓ navigate     tab pin     enter select     esc back",
                };
                view.render(width, PANEL_ROWS)
            }
            Panel::Permission { request, index, .. } => permission_lines(request, *index, width),
            Panel::Plan { index } => plan_lines(*index),
            Panel::PlanFresh { index } => {
                let [model, effort, ..] = self.setting_values();
                let rights = [model, effort, Vec::new()];
                let rows: Vec<panel::Row> = PLAN_FRESH_ROWS.iter().zip(rights).map(|(left, right)| panel::Row { left: left.to_string(), right }).collect();
                let view = panel::Panel {
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
            Panel::Ask(ask) => ask.render(width),
            Panel::Agents { .. } => Vec::new(),
            Panel::Workflows { .. } | Panel::WorkflowRun { .. } | Panel::WorkflowSave { .. } => self.workflow_panel(width),
            Panel::Tasks { agents, index } => tasks_panel(&self.terminals.lock().unwrap().list, &self.subagents.lock().unwrap().list, &self.client.catalog(), *agents, *index, width),
            Panel::Todos { index } => {
                let todos = self.todos.lock().unwrap();
                let rows: Vec<panel::Row> = todos.visible().map(|t| panel::Row { left: format!("#{} {}", t.id, t.subject), right: vec![span(t.status.as_str(), todo_style(t.status))] }).collect();
                let view = panel::Panel {
                    title: format!("Todos {}", rows.len()),
                    tabs: &[],
                    tab: 0,
                    search: None,
                    rows: &rows,
                    selected: *index,
                    empty: "No todos.",
                    hint: "↑↓ navigate     space status     d delete     esc close",
                };
                view.render(width, PANEL_ROWS)
            }
            Panel::Usage { tab, totals } => {
                let (rows, selected) = match totals.get(*tab) {
                    Some(totals) => {
                        let rows = usage_rows(totals, &self.client.catalog());
                        let total_row = rows.len().saturating_sub(1);
                        (rows, total_row)
                    }
                    None => {
                        let subscriptions: Vec<AccountView> = self.client.accounts().into_iter().filter(|a| a.oauth).collect();
                        let (rows, account_rows) = account_rows(&subscriptions, &[]);
                        (rows, account_rows.first().copied().unwrap_or(0))
                    }
                };
                let view = panel::Panel {
                    title: "Usage".to_string(),
                    tabs: &USAGE_TABS,
                    tab: *tab,
                    search: None,
                    rows: &rows,
                    selected,
                    empty: if *tab < usage::SCOPES.len() { "No usage recorded yet." } else { "No subscription accounts." },
                    hint: "tab scope     $ billed by OpenRouter, else at list prices; subscriptions do not bill     esc close",
                };
                view.render(width, PANEL_ROWS)
            }
            Panel::Skills { query, index } => {
                let rows = self.skill_rows(&query.text(), width, |skill| {
                    if self.settings.disabled_skills.contains(&skill.name) {
                        ("[ ]", span("disabled", MUTED))
                    } else if !self.settings.skill_enabled(skill) {
                        ("[ ]", span("profile off", MUTED))
                    } else {
                        ("[x]", span("enabled", PLAIN))
                    }
                });
                let view = panel::Panel {
                    title: "Skills".into(),
                    tabs: &[],
                    tab: 0,
                    search: Some(query.field(None)),
                    rows: &rows,
                    selected: *index,
                    empty: if self.skills.is_empty() { "No skills. Add one under ~/.agents/skills/<name>/SKILL.md." } else { "No matching skills." },
                    hint: "type to search name or owner/repo     ↑↓ navigate     space enable/disable     enter whole repo on/off     esc close",
                };
                view.render(width, PANEL_ROWS)
            }
            Panel::Mcp { index, confirm } => {
                let (views, error) = self.mcp.list();
                let name_width = views.iter().map(|view| view.name.chars().count()).max().unwrap_or(0);
                let mut rows = Vec::new();
                for view in &views {
                    let on = view.enabled && view.approval.is_none() && !self.settings.disabled_mcp.contains(&view.name);
                    let mut right = vec![match &view.status {
                        crate::mcp::Status::Off if view.approval.is_some() => span("project server, space to approve", WARN),
                        crate::mcp::Status::Off if !view.enabled => span("off in mcp.json", MUTED),
                        crate::mcp::Status::Off => span("disabled", MUTED),
                        crate::mcp::Status::Connecting => span("connecting", MUTED),
                        crate::mcp::Status::LoggingIn => span("logging in (browser)", WARN),
                        crate::mcp::Status::Ready { tools } => span(format!("{tools} tools"), PLAIN),
                        crate::mcp::Status::Failed(error) => span(error, ERROR),
                    }];
                    if view.logged_in {
                        right.push(span("  logged in", MUTED));
                    }
                    let mark = if on { "[x]" } else { "[ ]" };
                    rows.push(panel::Row { left: format!("{mark} {:<name_width$}  {}", view.name, view.target), right });
                }
                let empty = match &error {
                    Some(error) => error.as_str(),
                    None => "No MCP servers. Add one to ~/.agents/mcp.json or .mcp.json under \"mcpServers\".",
                };
                let view = panel::Panel {
                    title: "MCP".into(),
                    tabs: &[],
                    tab: 0,
                    search: None,
                    rows: &rows,
                    selected: *index,
                    empty,
                    hint: "↑↓ navigate     space enable/disable     r reconnect     l log in     esc close",
                };
                let mut lines = view.render(width, PANEL_ROWS);
                // `env`, `cwd` and `headers` decide what a project server runs as much as its command does.
                if let Some(selected) = views.get(*index).filter(|view| view.approval.is_some()) {
                    let (mut entry, cut) = mcp_entry(&selected.entry, width);
                    if cut && *confirm {
                        entry.push(vec![span("  read the whole entry in the file, then press space again to approve it", WARN)]);
                    }
                    let above_hint = lines.len() - 1;
                    lines.splice(above_hint..above_hint, entry);
                }
                lines
            }
            Panel::Plugins { index, confirm } => {
                let views = self.plugins.list();
                let name_width = views.iter().map(|view| view.name.len()).max().unwrap_or(0);
                let update_width = views.iter().filter_map(|view| view.update.as_ref()).map(|version| "update ".len() + version.len()).max().unwrap_or(0);
                let rows: Vec<panel::Row> = views
                    .into_iter()
                    .map(|view| {
                        let author = view.author.map(|author| format!("  @{author}")).unwrap_or_default();
                        let about = format!("{}{author}", view.description);
                        let (status, tone, about) = match (view.installed, self.settings.disabled_plugins.contains(&view.name), view.error) {
                            (false, _, _) => ("available", MUTED, about),
                            (true, true, _) => ("disabled", MUTED, about),
                            (true, false, Some(error)) => ("error", ERROR, error),
                            (true, false, None) if view.running => ("running", OK, about),
                            (true, false, None) => ("stopped", MUTED, about),
                        };
                        // Fixed columns before the description, so a narrow terminal cuts only the description.
                        let mut right = Vec::new();
                        if update_width > 0 {
                            let update = view.update.map(|version| format!("update {version}")).unwrap_or_default();
                            right.push(span(format!("{update:<update_width$}   "), WARN));
                        }
                        right.extend([span(format!("{status:<9}"), tone), span(format!("  {about}"), DIM)]);
                        panel::Row { left: format!("{:<name_width$}  {}", view.name, view.version), right }
                    })
                    .collect();
                let view = panel::Panel {
                    title: "Plugins".into(),
                    tabs: &[],
                    tab: 0,
                    search: None,
                    rows: &rows,
                    selected: *index,
                    empty: "No plugins. Install one with `agent plugin install <path>`.",
                    hint: if *confirm {
                        "d again removes the plugin and the state it saved     any other key cancels"
                    } else {
                        "↑↓ navigate     space install/enable/disable     u update     enter settings     d remove     esc close"
                    },
                };
                view.render(width, PANEL_ROWS)
            }
            Panel::PluginSettings { plugin, index, edit } => {
                let declared = self.plugins.list().into_iter().find(|view| &view.name == plugin).map(|view| view.settings).unwrap_or_default();
                let rows: Vec<panel::Row> = declared
                    .iter()
                    .enumerate()
                    .map(|(row, entry)| {
                        let key = entry["key"].as_str().unwrap_or_default();
                        let value = crate::plugins::setting(entry, &self.settings.plugin_settings[plugin.as_str()]);
                        let right = match (edit, value) {
                            (Some(editor), _) if row == *index => format!("{}▏", editor.text()),
                            (_, Value::Bool(on)) => (if on { "on" } else { "off" }).to_string(),
                            (_, value) => value.as_str().unwrap_or_default().to_string(),
                        };
                        panel::Row { left: format!("{key}  {}", entry["description"].as_str().unwrap_or_default()), right: vec![span(right, PLAIN)] }
                    })
                    .collect();
                let view = panel::Panel {
                    title: format!("{plugin} settings"),
                    tabs: &[],
                    tab: 0,
                    search: None,
                    rows: &rows,
                    selected: *index,
                    empty: "This plugin has no settings.",
                    hint: if edit.is_some() { "enter save     esc cancel" } else { "↑↓ navigate     space/enter change     esc back" },
                };
                view.render(width, PANEL_ROWS)
            }
            Panel::Plugin { plugin, data, index } => {
                let rows: Vec<panel::Row> = data["rows"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|row| panel::Row { left: row["left"].as_str().unwrap_or_default().to_string(), right: vec![span(row["right"].as_str().unwrap_or_default().to_string(), PLAIN)] })
                    .collect();
                let hint = format!("↑↓ navigate     {}     esc close", data["hint"].as_str().unwrap_or("enter select"));
                let view =
                    panel::Panel { title: data["title"].as_str().unwrap_or(plugin).to_string(), tabs: &[], tab: 0, search: None, rows: &rows, selected: *index, empty: "Nothing here.", hint: &hint };
                view.render(width, PANEL_ROWS)
            }
            Panel::Btw => self.btw.as_ref().expect("the btw panel opens with a thread").render(width),
            Panel::Terminal { .. } | Panel::Diff { .. } => unreachable!("frame draws the full-screen view instead of a panel"),
            Panel::Trust { root, files, .. } => {
                let rows: Vec<panel::Row> = files.iter().map(|file| panel::Row { left: file.strip_prefix(root).unwrap_or(file).display().to_string(), right: Vec::new() }).collect();
                let view = panel::Panel {
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
            Panel::Worktrees { confirm, index, worktrees, current } => {
                let home = std::env::var("HOME").unwrap_or_default();
                let rows: Vec<panel::Row> = worktrees
                    .iter()
                    .map(|w| {
                        let mark = if w.path == *current { "✓ " } else { "  " };
                        let path = w.path.display().to_string().replacen(&home, "~", 1);
                        panel::Row { left: format!("{mark}{}", w.branch), right: vec![span(path, MUTED)] }
                    })
                    .collect();
                let view = panel::Panel {
                    title: if *confirm { format!("Remove {}? Its ignored files go with it.", worktrees[*index].branch) } else { "Worktrees".into() },
                    tabs: &[],
                    tab: 0,
                    search: None,
                    rows: &rows,
                    selected: *index,
                    empty: "",
                    hint: if *confirm { "d again removes it, the branch stays     any other key cancels" } else { "↑↓ navigate     enter move session     d remove     esc close" },
                };
                view.render(width, PANEL_ROWS)
            }
            Panel::Login(login) => login.render(width, PANEL_ROWS),
            Panel::Rewind { index, points } => {
                let rows: Vec<panel::Row> = points.iter().map(|point| panel::Row { left: point.text.lines().next().unwrap_or_default().chars().take(60).collect(), right: Vec::new() }).collect();
                let view = panel::Panel {
                    title: "Rewind".into(),
                    tabs: &[],
                    tab: 0,
                    search: None,
                    rows: &rows,
                    selected: *index,
                    empty: "",
                    hint: "↑↓ navigate     enter rewind here, restoring the files it changed     esc close",
                };
                view.render(width, PANEL_ROWS)
            }
            Panel::Changes { files, index } => {
                let rows: Vec<panel::Row> = files.iter().map(|file| panel::Row { left: file.path.clone(), right: diff_stat(&file.diff) }).collect();
                let view =
                    panel::Panel { title: "Changes".into(), tabs: &[], tab: 0, search: None, rows: &rows, selected: *index, empty: "", hint: "↑↓ navigate     enter show diff     esc close" };
                view.render(width, PANEL_ROWS)
            }
            Panel::History { entries, tab, query, index } => {
                let rows: Vec<panel::Row> = matching_prompts(entries, &query.text(), |entry| entry.in_scope(*tab, &self.cwd, &self.session_id))
                    .iter()
                    .map(|prompt| panel::Row { left: prompt.split_whitespace().collect::<Vec<_>>().join(" "), right: Vec::new() })
                    .collect();
                let view = panel::Panel {
                    title: "History".into(),
                    tabs: &history::SCOPES,
                    tab: *tab,
                    search: Some(query.field(None)),
                    rows: &rows,
                    selected: *index,
                    empty: "No matching prompts.",
                    hint: "↑↓ navigate     enter insert     tab scope     esc close",
                };
                view.render(width, PANEL_ROWS)
            }
            Panel::Accounts { logout, query, index } => {
                let accounts = matching_accounts(self.client.accounts(), &query.text());
                let (rows, account_rows) = account_rows(&accounts, &self.plugins.account_labels());
                let view = panel::Panel {
                    title: if *logout { "Remove account" } else { "Accounts" }.into(),
                    tabs: &[],
                    tab: 0,
                    search: Some(query.field(None)),
                    rows: &rows,
                    selected: account_rows.get(*index).copied().unwrap_or(0),
                    empty: "No matching accounts.",
                    hint: if *logout { "↑↓ navigate     enter remove     esc close" } else { "↑↓ navigate     shift+↑↓ failover order     tab check all     enter use     esc close" },
                };
                view.render(width, PANEL_ROWS)
            }
        }
    }
}

pub(super) const USAGE_TABS: [&str; 5] = ["Session", "24 hours", "7 days", "All", "Limits"];

/// A column header row, one row per model, and a total row.
pub(super) fn usage_rows(totals: &[ModelTotal], catalog: &[Model]) -> Vec<panel::Row> {
    if totals.is_empty() {
        return Vec::new();
    }
    let row = |left: &str, cells: [String; 6], style| panel::Row {
        left: left.to_string(),
        right: vec![span(format!("{:>8}  {:>8}  {:>8}  {:>10}  {:>11}  {:>9}", cells[0], cells[1], cells[2], cells[3], cells[4], cells[5]), style)],
    };
    let counts = |requests: usize, u: &Usage, cost: Option<f64>| {
        [requests.to_string(), tokens(u.input), tokens(u.output), tokens(u.cache_read), tokens(u.cache_write), cost.map(usage::dollars).unwrap_or_default()]
    };
    let mut rows = vec![row("Model", ["Requests", "Input", "Output", "Cache read", "Cache write", "Cost"].map(String::from), MUTED)];
    let (mut requests, mut sum) = (0, Usage::default());
    for total in totals {
        let used = total.usage();
        rows.push(row(&total.model, counts(total.requests.len(), &used, usage::model_cost(total, catalog)), PLAIN));
        requests += total.requests.len();
        sum.add(used);
    }
    rows.push(row("Total", counts(requests, &sum, usage::total_cost(totals, catalog)), BRIGHT));
    rows
}

/// The `/account` table: subscriptions, then API keys, each under a heading row, with columns shared by both groups.
/// Also returns each account's row index, since headings and the gap between groups cannot be selected.
pub(super) fn account_rows(accounts: &[AccountView], labels: &[(String, HashMap<String, String>)]) -> (Vec<panel::Row>, Vec<usize>) {
    // Each row's cells and the style of its Status cell.
    let mut table: Vec<(String, Vec<String>, Style)> = Vec::new();
    let mut account_rows = Vec::new();
    let mut group = None;
    for a in accounts {
        if group != Some(a.oauth) {
            if group.is_some() {
                table.push((String::new(), Vec::new(), MUTED));
            }
            group = Some(a.oauth);
            let (title, five_hour, seven_day) = if a.oauth { ("Subscription", "5h", "7d") } else { ("API key", "", "") };
            let mut heading: Vec<String> = ["Provider", five_hour, seven_day, "Pinned"].map(String::from).into();
            heading.extend(labels.iter().map(|(plugin, _)| if a.oauth { plugin.clone() } else { String::new() }));
            // Last, so a long error runs off the edge instead of pushing columns.
            heading.push("Status".into());
            table.push((format!("  {title}"), heading, MUTED));
        }
        let percent = |share: Option<f64>| match (a.oauth, share) {
            (true, Some(share)) => format!("{share:.0}%"),
            (true, None) => "?".to_string(),
            (false, _) => String::new(),
        };
        let mark = if a.current { "✓ " } else { "  " };
        let quotas = a.usage.as_ref().map_or(&[][..], |u| u.models.as_slice());
        let no_five_hour = a.usage.as_ref().is_some_and(|u| u.five_hour.is_none());
        let five_hour = if no_five_hour { "-".to_string() } else { percent(a.usage.as_ref().and_then(|u| u.five_hour)) };
        let seven_day = if quotas.is_empty() { percent(a.usage.as_ref().map(|u| u.seven_day)) } else { "-".to_string() };
        let (status, status_style) = match &a.status {
            AccountStatus::Unchecked => (String::new(), MUTED),
            AccountStatus::Checking => ("checking…".to_string(), MUTED),
            AccountStatus::Working => ("ok".to_string(), OK),
            AccountStatus::Limited => ("rate limited".to_string(), WARN),
            AccountStatus::Failed(error) => (error.lines().next().unwrap_or_default().to_string(), ERROR),
        };
        let mut cells = vec![a.provider.clone(), five_hour, seven_day, if a.pinned { "pinned" } else { "" }.to_string()];
        for (_, by_account) in labels {
            cells.push(a.account_id.as_ref().and_then(|id| by_account.get(id)).cloned().unwrap_or_default());
        }
        cells.push(status);
        account_rows.push(table.len());
        table.push((format!("{mark}{}", a.name), cells, status_style));
        // Google's quotas are per model, with their own resets, so each gets a line under its account.
        for quota in quotas {
            let reset = quota.resets_at.map(|at| format!(", resets {}", crate::api::accounts::resets_in(at))).unwrap_or_default();
            table.push((format!("      {} {:.0}%{reset}", quota.model, quota.used), Vec::new(), MUTED));
        }
    }
    let columns = table.iter().map(|(_, cells, _)| cells.len()).max().unwrap_or(0);
    let widths: Vec<usize> = (0..columns).map(|c| table.iter().filter_map(|(_, cells, _)| cells.get(c)).map(|cell| width(cell)).max().unwrap_or(0)).collect();
    let rows = table
        .into_iter()
        .map(|(left, cells, status_style)| {
            let mut right = Vec::new();
            for (c, cell) in cells.iter().enumerate() {
                let pad = " ".repeat(widths[c] - width(cell));
                // The 5h and 7d percentages line up on the right.
                let padded = if c == 1 || c == 2 { format!("{pad}{cell}  ") } else { format!("{cell}{pad}  ") };
                right.push(span(padded, if c == cells.len() - 1 { status_style } else { MUTED }));
            }
            panel::Row { left, right }
        })
        .collect();
    (rows, account_rows)
}

pub(super) fn context_window_label(window: Option<u64>) -> String {
    window.map_or("auto".to_string(), tokens)
}

/// The rows of `row`'s section, or with a query the rows of every section whose name matches it, see `panel::search`.
pub(super) fn matching_settings(row: usize, query: &str) -> Vec<usize> {
    if query.is_empty() {
        return SETTING_SECTIONS[setting_section(row)].1.to_vec();
    }
    panel::search(SETTING_SECTIONS.iter().flat_map(|(_, rows)| rows.iter().copied()), query, |&r| SETTING_NAMES[r].to_string())
}

pub(super) fn setting_tabs() -> Vec<&'static str> {
    SETTING_SECTIONS.iter().map(|(name, _)| *name).chain(["Profiles"]).collect()
}

/// The settings panel opened on tab `tab` of `setting_tabs`.
pub(super) fn settings_tab(tab: usize) -> Panel {
    match SETTING_SECTIONS.get(tab) {
        Some((_, rows)) => Panel::Settings { row: rows[0], query: Editor::new() },
        None => Panel::Profiles { index: 0, naming: None, confirm: false },
    }
}

pub(super) fn setting_section(row: usize) -> usize {
    SETTING_SECTIONS.iter().position(|(_, rows)| rows.contains(&row)).expect("every setting row is in a section")
}

/// Override keys for the context windows panel: providers on tab 0, models matching `query` on tab 1.
pub(super) fn context_window_targets<'a>(catalog: &'a [Model], settings: &Settings, tab: usize, query: &str) -> Vec<(String, Option<&'a Model>)> {
    if tab == 0 {
        return settings.sort_providers(&models::PROVIDERS).into_iter().map(|provider| (provider.to_string(), None)).collect();
    }
    matching_models(catalog, settings, 0, query).into_iter().flatten().map(|model| (model_key(model), Some(model))).collect()
}

/// Where the window in effect for `model` comes from.
pub(super) fn window_source(settings: &Settings, model: &Model) -> &'static str {
    if settings.context_windows.contains_key(&model_key(model)) {
        "model"
    } else if settings.context_windows.contains_key(&model.provider) {
        "provider"
    } else {
        "catalog"
    }
}

/// Picker rows for settings `row` whose provider or id matches `query` (see `panel::search`): pinned models first, each part grouped in provider order.
/// `None` is "auto", offered for the optional condense and fallback models.
pub(super) fn matching_models<'a>(catalog: &'a [Model], settings: &Settings, row: usize, query: &str) -> Vec<Option<&'a Model>> {
    let auto = (row != 0 && panel::search_rank("auto", query).is_some()).then_some(None);
    let providers = settings.sort_providers(&models::PROVIDERS);
    let mut matches = panel::search(catalog, query, |m| format!("{} {}", model_owners(m), m.id));
    matches.sort_by_key(|m| (!settings.pinned_models.contains(&model_key(m)), providers.iter().position(|provider| *provider == m.provider).unwrap_or(usize::MAX)));
    auto.into_iter().chain(matches.into_iter().map(Some)).collect()
}

/// "openai-codex · pro, plus": the provider and the accounts whose model list has the model.
pub(super) fn model_owners(model: &Model) -> String {
    if model.accounts.is_empty() {
        return model.provider.clone();
    }
    format!("{} · {}", model.provider, model.accounts.join(", "))
}

/// The model id settings `row` edits: 0 model, 8 condense, 12 fallback, 29 reviewer, 30 subagent, 38 small.
pub(super) fn model_setting(settings: &mut Settings, row: usize) -> &mut String {
    match row {
        0 => &mut settings.model,
        8 => &mut settings.condense_model,
        12 => &mut settings.fallback_model,
        29 => &mut settings.reviewer_model,
        38 => &mut settings.small_model,
        _ => &mut settings.subagent_model,
    }
}

/// The option after (or before) `current`, wrapping around; the first option if `current` is not listed.
pub(super) fn step<T: PartialEq + Clone>(options: &[T], current: &T, forward: bool) -> T {
    let position = options.iter().position(|o| o == current);
    let index = match (position, forward) {
        (None, _) => 0,
        (Some(i), true) => (i + 1) % options.len(),
        (Some(i), false) => (i + options.len() - 1) % options.len(),
    };
    options[index].clone()
}

pub(super) fn switch_label(percent: u64) -> String {
    if percent == 0 { "never".into() } else { format!("{percent}%") }
}

/// fx's inline setting values: every option, the current one checked and bright. A current value missing from the options is appended.
pub(super) fn choices(options: impl IntoIterator<Item = String>, current: &str) -> Line {
    let mut line = Vec::new();
    let mut listed = false;
    for option in options {
        if !line.is_empty() {
            line.push(span("  ", PLAIN));
        }
        listed |= option == current;
        line.push(if option == current { span(format!("✓ {option}"), BRIGHT.bold()) } else { span(option, MUTED) });
    }
    if !listed {
        line.push(span(format!("  ✓ {current}"), BRIGHT.bold()));
    }
    line
}

/// An on/off row for a tool the user installs separately; the setting does nothing until `binary` is on PATH.
pub(super) fn on_off_choices(on: bool) -> Line {
    choices(["off", "on"].map(String::from), if on { "on" } else { "off" })
}

pub(super) fn optional_tool(on: bool, binary: &str) -> Line {
    let mut line = on_off_choices(on);
    if !crate::process::installed(binary) {
        line.push(span(format!("  {binary} is not installed"), MUTED));
    }
    line
}
