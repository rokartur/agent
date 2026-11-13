use super::*;

impl App {
    pub(super) fn open_resume(&mut self) {
        let sessions: Vec<SessionInfo> = Session::list(&self.sessions_dir).into_iter().filter(|s| s.id != self.session_id).collect();
        self.panel = Some(Panel::Resume(ResumePanel { all: false, index: 0, sessions }));
    }

    pub(super) fn resume_key(&mut self, code: KeyCode) {
        let cwd = self.cwd.display().to_string();
        let Some(Panel::Resume(resume)) = self.panel.as_mut() else { return };
        if let Some(id) = resume.key(code, &cwd) {
            self.panel = None;
            self.start(Command::Resume(id));
        }
    }

    pub(super) fn resume_panel(&self, resume: &ResumePanel, width: usize) -> Vec<Line> {
        let ResumePanel { all, index, .. } = resume;
        let rows: Vec<list::Row> = resume
            .visible(&self.cwd.display().to_string())
            .into_iter()
            .enumerate()
            .map(|(row, s)| {
                let workspace = Path::new(&s.cwd).file_name().map_or(s.cwd.clone(), |n| n.to_string_lossy().into_owned());
                let turns = if s.turns == 1 { "1 turn".to_string() } else { format!("{} turns", s.turns) };
                let title: String = s.name.clone().unwrap_or_else(|| s.title.clone()).chars().take(40).collect();
                let style = if row == *index { BRIGHT.bold() } else { MUTED };
                list::Row { left: title, right: vec![span(format!("{workspace} · {} · {turns}", age(s.modified)), style)] }
            })
            .collect();
        let view = list::List {
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

    pub(super) fn open_running(&mut self) {
        let sessions = crate::serve::live(&self.home).into_iter().filter(|peer| peer.session != self.session_id).collect();
        self.panel = Some(Panel::Running { index: 0, sessions, confirm: false });
    }

    pub(super) fn running_key(&mut self, code: KeyCode) {
        let Some(Panel::Running { index, sessions, confirm }) = self.panel.as_mut() else { return };
        let stopping = std::mem::take(confirm);
        match code {
            KeyCode::Esc => self.panel = None,
            KeyCode::Up => *index = index.saturating_sub(1),
            KeyCode::Down => *index = (*index + 1).min(sessions.len().saturating_sub(1)),
            KeyCode::Enter if !sessions.is_empty() => {
                let pid = sessions[*index].pid;
                self.panel = None;
                self.release_typing_client(|client| client.attach(pid));
            }
            KeyCode::Char('x') if !sessions.is_empty() && stopping => {
                let peer = sessions.remove(*index);
                *index = (*index).min(sessions.len().saturating_sub(1));
                if let Err(e) = crate::serve::stop(&self.home, peer.pid) {
                    self.push(Item::Error(format!("Cannot stop session {}: {e}", peer.session)));
                }
            }
            KeyCode::Char('x') if !sessions.is_empty() => *confirm = true,
            _ => {}
        }
    }

    pub(super) fn running_panel(&self, index: &usize, sessions: &[Peer], confirm: &bool, width: usize) -> Vec<Line> {
        let rows: Vec<list::Row> = sessions
            .iter()
            .enumerate()
            .map(|(row, peer)| {
                let name = if peer.title.is_empty() { &peer.session } else { &peer.title };
                let state = if peer.busy { "working" } else { "idle" };
                let style = if row == *index { BRIGHT.bold() } else { MUTED };
                // Cards are files any process of this user writes, so their text could carry escape codes.
                let printable = |text: &str| -> String { text.chars().filter(|c| !c.is_control()).collect() };
                list::Row { left: printable(name).chars().take(40).collect(), right: vec![span(format!("{} · {state}", printable(&peer.cwd)), style)] }
            })
            .collect();
        let hint = if *confirm { "x again stops it     any other key keeps it" } else { "↑↓ navigate     enter attach     x stop     esc close" };
        let view = list::List { title: format!("Running {}", rows.len()), tabs: &[], tab: 0, search: None, rows: &rows, selected: *index, empty: "No other session is running.", hint };
        view.render(width, PANEL_ROWS)
    }

    pub(super) fn open_history(&mut self) {
        let mut entries = history::load(&self.home.join(".agents"));
        entries.reverse();
        self.panel = Some(Panel::History(HistoryPanel { entries, tab: 0, query: Editor::new(), index: 0 }));
    }

    pub(super) fn history_key(&mut self, key: KeyEvent) {
        let Some(Panel::History(history)) = self.panel.as_mut() else { return };
        let Some(prompt) = history.key(key, &self.cwd, &self.session_id) else { return };
        self.panel = None;
        self.editor.set(&prompt);
        // The old selection may reach rows a shorter prompt no longer has.
        self.input_selection = Selection::default();
    }

    pub(super) fn history_panel(&self, history: &HistoryPanel, width: usize) -> Vec<Line> {
        let HistoryPanel { tab, query, index, .. } = history;
        let rows: Vec<list::Row> =
            history.matches(&self.cwd, &self.session_id).iter().map(|prompt| list::Row { left: prompt.split_whitespace().collect::<Vec<_>>().join(" "), right: Vec::new() }).collect();
        let view = list::List {
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

    pub(super) fn rewind_key(&mut self, code: KeyCode) {
        let Some(Panel::Rewind { index, points }) = self.panel.as_mut() else { return };
        match code {
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
        }
    }

    pub(super) fn rewind_panel(&self, index: &usize, points: &[RewindPoint], width: usize) -> Vec<Line> {
        let rows: Vec<list::Row> = points.iter().map(|point| list::Row { left: point.text.lines().next().unwrap_or_default().chars().take(60).collect(), right: Vec::new() }).collect();
        let view = list::List {
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

    pub(super) fn changes_key(&mut self, code: KeyCode) {
        let Some(Panel::Changes { files, index }) = self.panel.as_mut() else { return };
        match code {
            KeyCode::Up => *index = index.saturating_sub(1),
            KeyCode::Down => *index = (*index + 1).min(files.len() - 1),
            KeyCode::Enter => {
                let (files, index) = (std::mem::take(files), *index);
                self.panel = Some(Panel::Diff { files, index, scroll: DIFF_TOP });
            }
            _ => {}
        }
    }

    pub(super) fn changes_panel(&self, files: &[FileChange], index: &usize, width: usize) -> Vec<Line> {
        let rows: Vec<list::Row> = files.iter().map(|file| list::Row { left: file.path.clone(), right: diff_stat(&file.diff) }).collect();
        let view = list::List { title: "Changes".into(), tabs: &[], tab: 0, search: None, rows: &rows, selected: *index, empty: "", hint: "↑↓ navigate     enter show diff     esc close" };
        view.render(width, PANEL_ROWS)
    }

    pub(super) fn worktrees_key(&mut self, code: KeyCode) {
        let Some(Panel::Worktrees(worktrees)) = self.panel.as_mut() else { return };
        match worktrees.key(code) {
            WorktreesKey::Stay => {}
            WorktreesKey::Warn(text) => self.warn("worktree", text),
            WorktreesKey::Close => self.panel = None,
            WorktreesKey::Move(path) => {
                self.panel = None;
                self.start(Command::Worktree(path));
            }
            WorktreesKey::Remove(path) => {
                self.panel = None;
                // Deleting a checkout with node_modules takes seconds, so it runs off the UI thread.
                self.system("worktree", format!("Removing {}", path.display()));
                let (cwd, events) = (self.cwd.clone(), self.events.clone());
                std::thread::spawn(move || {
                    let result = worktree::remove(&cwd, &path);
                    let _ = events.send(Msg::WorktreeRemoved(path, result));
                });
            }
        }
    }

    pub(super) fn worktrees_panel(&self, worktrees: &WorktreesPanel, width: usize) -> Vec<Line> {
        let WorktreesPanel { confirm, index, worktrees, current } = worktrees;
        let home = std::env::var("HOME").unwrap_or_default();
        let rows: Vec<list::Row> = worktrees
            .iter()
            .map(|w| {
                let mark = if w.path == *current { "✓ " } else { "  " };
                let path = w.path.display().to_string().replacen(&home, "~", 1);
                list::Row { left: format!("{mark}{}", w.branch), right: vec![span(path, MUTED)] }
            })
            .collect();
        let view = list::List {
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

    pub(super) fn todos_key(&mut self, code: KeyCode) {
        let Some(Panel::Todos { index }) = self.panel.as_mut() else { return };
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

    pub(super) fn todos_panel(&self, index: &usize, width: usize) -> Vec<Line> {
        let todos = self.todos.lock().unwrap();
        let rows: Vec<list::Row> = todos.visible().map(|t| list::Row { left: format!("#{} {}", t.id, t.subject), right: vec![span(t.status.as_str(), todo_style(t.status))] }).collect();
        let view = list::List {
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

    pub(super) fn agents_key(&mut self, code: KeyCode) {
        let Some(Panel::Agents { row }) = self.panel.as_mut() else { return };
        let workflows = footer_workflows(&self.workflows);
        let agents = footer_agents(&self.subagents.lock().unwrap());
        let pills = usize::from(self.terminals.lock().unwrap().running() > 0) + usize::from(agents > 0);
        let last = pills + workflows.len();
        let selected = (*row).min(last);
        let workflow = selected.checked_sub(1 + pills).map(|offset| workflows[offset].clone());
        match code {
            KeyCode::Up if selected > 1 => *row = selected - 1,
            KeyCode::Down => *row = (selected + 1).min(last),
            // The shells and agents pills share one line, so ←/→ walk between them too.
            KeyCode::Left if selected == 2 && pills == 2 => *row = 1,
            KeyCode::Right if selected == 1 && pills == 2 => *row = 2,
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

    /// Sorts only on open, so a task that finishes does not move under the cursor while the panel is up.
    pub(super) fn open_tasks(&mut self, agents: bool) {
        self.terminals.lock().unwrap().running_first();
        self.subagents.lock().unwrap().running_first();
        self.panel = Some(Panel::Tasks { agents, index: 0 });
    }

    pub(super) fn tasks_key(&mut self, code: KeyCode) {
        let Some(Panel::Tasks { agents, index }) = self.panel.as_mut() else { return };
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

    pub(super) fn usage_key(&mut self, code: KeyCode) {
        let Some(Panel::Usage { tab, .. }) = self.panel.as_mut() else { return };
        if !matches!(code, KeyCode::Tab | KeyCode::BackTab) {
            return;
        }
        let tabs = USAGE_TABS.len();
        *tab = if code == KeyCode::Tab { (*tab + 1) % tabs } else { (*tab + tabs - 1) % tabs };
    }

    pub(super) fn usage_panel(&self, tab: &usize, totals: &[Vec<ModelTotal>; 4], width: usize) -> Vec<Line> {
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
        let view = list::List {
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

    pub(super) fn accounts_key(&mut self, key: KeyEvent) {
        let Some(Panel::Accounts { logout, query, index }) = self.panel.as_mut() else { return };
        let code = key.code;
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

    pub(super) fn accounts_panel(&self, logout: &bool, query: &Editor, index: &usize, width: usize) -> Vec<Line> {
        let accounts = matching_accounts(self.client.accounts(), &query.text());
        let (rows, account_rows) = account_rows(&accounts, &self.plugins.account_labels());
        let view = list::List {
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

pub(super) const USAGE_TABS: [&str; 5] = ["Session", "24 hours", "7 days", "All", "Limits"];

/// A column header row, one row per model, and a total row.
pub(super) fn usage_rows(totals: &[ModelTotal], catalog: &[Model]) -> Vec<list::Row> {
    if totals.is_empty() {
        return Vec::new();
    }
    let row = |left: &str, cells: [String; 6], style| list::Row {
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
pub(super) fn account_rows(accounts: &[AccountView], labels: &[(String, HashMap<String, String>)]) -> (Vec<list::Row>, Vec<usize>) {
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
            list::Row { left, right }
        })
        .collect();
    (rows, account_rows)
}

pub(super) struct ResumePanel {
    pub(super) all: bool,
    pub(super) index: usize,
    pub(super) sessions: Vec<SessionInfo>,
}

impl ResumePanel {
    /// The sessions the open tab lists: those started in `cwd`, or all of them.
    pub(super) fn visible(&self, cwd: &str) -> Vec<&SessionInfo> {
        self.sessions.iter().filter(|s| self.all || s.cwd == cwd).collect()
    }

    /// The id of the session Enter picked.
    pub(super) fn key(&mut self, code: KeyCode, cwd: &str) -> Option<String> {
        let count = self.visible(cwd).len();
        match code {
            KeyCode::Up => self.index = self.index.saturating_sub(1),
            KeyCode::Down => self.index = (self.index + 1).min(count.saturating_sub(1)),
            KeyCode::Tab | KeyCode::BackTab => (self.all, self.index) = (!self.all, 0),
            KeyCode::Enter => return self.visible(cwd).get(self.index).map(|session| session.id.clone()),
            _ => {}
        }
        None
    }
}

/// /history: saved prompts, newest first, in the `history::SCOPES` tab `tab`. Typing filters, Enter puts the prompt in the editor.
pub(super) struct HistoryPanel {
    pub(super) entries: Vec<history::Entry>,
    pub(super) tab: usize,
    pub(super) query: Editor,
    pub(super) index: usize,
}

impl HistoryPanel {
    /// The prompts the open tab and the query leave, for a session `session` running in `cwd`.
    pub(super) fn matches(&self, cwd: &Path, session: &str) -> Vec<&str> {
        matching_prompts(&self.entries, &self.query.text(), |entry| entry.in_scope(self.tab, cwd, session))
    }

    /// The prompt Enter picked.
    pub(super) fn key(&mut self, key: KeyEvent, cwd: &Path, session: &str) -> Option<String> {
        let count = self.matches(cwd, session).len();
        match key.code {
            KeyCode::Up => self.index = self.index.saturating_sub(1),
            KeyCode::Down => self.index = (self.index + 1).min(count.saturating_sub(1)),
            KeyCode::Enter => return self.matches(cwd, session).get(self.index).map(|prompt| prompt.to_string()),
            KeyCode::Tab | KeyCode::BackTab => {
                let tabs = history::SCOPES.len();
                self.tab = if key.code == KeyCode::Tab { (self.tab + 1) % tabs } else { (self.tab + tabs - 1) % tabs };
                self.index = 0;
            }
            _ => {
                if self.query.edit_key(key) {
                    self.index = 0;
                }
            }
        }
        None
    }
}

/// `confirm`: d was pressed once, and the next d removes the selected worktree.
pub(super) struct WorktreesPanel {
    pub(super) confirm: bool,
    pub(super) index: usize,
    pub(super) worktrees: Vec<Worktree>,
    pub(super) current: PathBuf,
}

#[derive(Debug, PartialEq)]
pub(super) enum WorktreesKey {
    Stay,
    Warn(&'static str),
    /// Enter on the worktree the session already runs in.
    Close,
    Move(PathBuf),
    Remove(PathBuf),
}

impl WorktreesPanel {
    pub(super) fn key(&mut self, code: KeyCode) -> WorktreesKey {
        let confirmed = std::mem::take(&mut self.confirm);
        let selected = self.worktrees[self.index].path.clone();
        match code {
            KeyCode::Up => self.index = self.index.saturating_sub(1),
            KeyCode::Down => self.index = (self.index + 1).min(self.worktrees.len() - 1),
            KeyCode::Enter if selected == self.current => return WorktreesKey::Close,
            KeyCode::Enter => return WorktreesKey::Move(selected),
            KeyCode::Char('d') if self.index == 0 => return WorktreesKey::Warn("The main worktree cannot be removed."),
            KeyCode::Char('d') if selected == self.current => return WorktreesKey::Warn("This session runs in that worktree; move it first."),
            KeyCode::Char('d') if !confirmed => self.confirm = true,
            KeyCode::Char('d') => return WorktreesKey::Remove(selected),
            _ => {}
        }
        WorktreesKey::Stay
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str, cwd: &str) -> SessionInfo {
        SessionInfo { id: id.into(), cwd: cwd.into(), name: None, title: String::new(), turns: 1, modified: std::time::SystemTime::UNIX_EPOCH }
    }

    #[test]
    fn resume_picks_from_the_list_the_open_tab_shows() {
        let mut resume = ResumePanel { all: false, index: 0, sessions: vec![session("other", "/b"), session("here", "/a"), session("here too", "/a")] };
        assert_eq!(resume.key(KeyCode::Down, "/a"), None);
        assert_eq!(resume.key(KeyCode::Down, "/a"), None);
        assert_eq!(resume.key(KeyCode::Enter, "/a").as_deref(), Some("here too"));
        assert_eq!(resume.key(KeyCode::Tab, "/a"), None);
        assert_eq!(resume.key(KeyCode::Enter, "/a").as_deref(), Some("other"));
    }

    #[test]
    fn history_picks_from_the_prompts_the_tab_and_query_leave() {
        let entry = |prompt: &str, cwd: &str| history::Entry { prompt: prompt.into(), cwd: PathBuf::from(cwd), session: "s".into() };
        let entries = vec![entry("fix the bug", "/b"), entry("fix the test", "/a"), entry("write docs", "/a")];
        let mut history = HistoryPanel { entries, tab: 1, query: Editor::new(), index: 0 };
        for key in "fix".chars() {
            assert_eq!(history.key(KeyEvent::from(KeyCode::Char(key)), Path::new("/a"), "s"), None);
        }
        assert_eq!(history.key(KeyEvent::from(KeyCode::Down), Path::new("/a"), "s"), None);
        assert_eq!(history.key(KeyEvent::from(KeyCode::Enter), Path::new("/a"), "s").as_deref(), Some("fix the test"));
    }

    #[test]
    fn d_refuses_the_main_and_the_current_worktree_and_removes_another_on_the_second_press() {
        let worktree = |path: &str| Worktree { path: PathBuf::from(path), branch: path.into() };
        let mut panel = WorktreesPanel { confirm: false, index: 0, worktrees: vec![worktree("/main"), worktree("/here"), worktree("/other")], current: PathBuf::from("/here") };
        let d = KeyCode::Char('d');
        assert_eq!(panel.key(d), WorktreesKey::Warn("The main worktree cannot be removed."));
        panel.current = PathBuf::from("/main");
        assert_eq!(panel.key(d), WorktreesKey::Warn("The main worktree cannot be removed."));
        panel.current = PathBuf::from("/here");
        panel.key(KeyCode::Down);
        assert_eq!(panel.key(d), WorktreesKey::Warn("This session runs in that worktree; move it first."));
        assert_eq!(panel.key(KeyCode::Enter), WorktreesKey::Close);
        panel.key(KeyCode::Down);
        assert_eq!(panel.key(d), WorktreesKey::Stay);
        assert!(panel.confirm);
        assert_eq!(panel.key(d), WorktreesKey::Remove(PathBuf::from("/other")));
        panel.key(d);
        assert_eq!(panel.key(KeyCode::Up), WorktreesKey::Stay);
        assert!(!panel.confirm, "any other key cancels the pending removal");
        assert_eq!(panel.key(KeyCode::Down), WorktreesKey::Stay);
        assert_eq!(panel.key(KeyCode::Enter), WorktreesKey::Move(PathBuf::from("/other")));
    }
}
