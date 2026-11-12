use super::*;

/// One row of the settings panel. Declaration order is the search tie-break order.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum Setting {
    Model,
    Thinking,
    PermissionMode,
    AutoCompact,
    CompactionReserve,
    KeepRecent,
    ExpandTools,
    Condense,
    CondenseModel,
    CondenseMinBatch,
    SwitchAt,
    WhenAllOverLimit,
    FallbackModel,
    ContextWindow,
    Ponytail,
    Caveman,
    Rtk,
    HeadroomProxy,
    SubagentEffort,
    Notifications,
    ScrollLines,
    CopyOnSelect,
    UpdateChannel,
    MenuRows,
    PruneSessions,
    ProviderOrder,
    ConflictPriority,
    Speed,
    CacheTtl,
    ReviewerModel,
    SubagentModel,
    FooterCost,
    FooterAccount,
    ProgressBar,
    HeadroomUrl,
    HeadroomToken,
    MidTurn,
    MessageThreads,
    SmallModel,
    OutputStyle,
}

impl Setting {
    pub(super) fn name(self) -> &'static str {
        match self {
            Setting::Model => "Model",
            Setting::Thinking => "Thinking level",
            Setting::PermissionMode => "Permission mode",
            Setting::AutoCompact => "Auto-compact",
            Setting::CompactionReserve => "Compaction reserve",
            Setting::KeepRecent => "Keep recent context",
            Setting::ExpandTools => "Expand tool output",
            Setting::Condense => "Condense tool output",
            Setting::CondenseModel => "Condense model",
            Setting::CondenseMinBatch => "Condense min batch",
            Setting::SwitchAt => "Switch account at",
            Setting::WhenAllOverLimit => "When all over limit",
            Setting::FallbackModel => "Fallback model",
            Setting::ContextWindow => "Context window",
            Setting::Ponytail => "Ponytail",
            Setting::Caveman => "Caveman",
            Setting::Rtk => "rtk",
            Setting::HeadroomProxy => "Headroom proxy",
            Setting::SubagentEffort => "Subagent effort",
            Setting::Notifications => "Notifications",
            Setting::ScrollLines => "Scroll lines",
            Setting::CopyOnSelect => "Copy on select",
            Setting::UpdateChannel => "Update channel",
            Setting::MenuRows => "Menu rows",
            Setting::PruneSessions => "Auto prune sessions",
            Setting::ProviderOrder => "Provider order",
            Setting::ConflictPriority => "Name conflicts prefer",
            Setting::Speed => "Speed",
            Setting::CacheTtl => "Prompt cache TTL",
            Setting::ReviewerModel => "Reviewer model",
            Setting::SubagentModel => "Subagent model",
            Setting::FooterCost => "Cost in footer",
            Setting::FooterAccount => "Account in footer",
            Setting::ProgressBar => "Progress bar",
            Setting::HeadroomUrl => "Headroom URL",
            Setting::HeadroomToken => "Headroom token",
            Setting::MidTurn => "Messages while working",
            Setting::MessageThreads => "Message threads",
            Setting::SmallModel => "Small model",
            Setting::OutputStyle => "Output style",
        }
    }
}

/// The settings panel's tabs with their rows, in the order it lists them.
pub(super) const SETTING_SECTIONS: [(&str, &[Setting]); 5] = {
    use Setting::*;
    [
        ("Model", &[Model, Thinking, Speed, ContextWindow, PermissionMode, SubagentEffort, SubagentModel, ReviewerModel, SmallModel]),
        ("Context", &[AutoCompact, CompactionReserve, KeepRecent, Condense, CondenseModel, CondenseMinBatch, CacheTtl, MessageThreads]),
        ("Accounts", &[SwitchAt, WhenAllOverLimit, FallbackModel, ProviderOrder]),
        ("Modes", &[OutputStyle, Ponytail, Caveman, Rtk, HeadroomProxy, HeadroomUrl, HeadroomToken, ConflictPriority]),
        ("Interface", &[ExpandTools, FooterCost, FooterAccount, Notifications, ProgressBar, ScrollLines, MenuRows, CopyOnSelect, UpdateChannel, PruneSessions, MidTurn]),
    ]
};
/// The settings tab after `SETTING_SECTIONS`, drawn by `Panel::Profiles`.
pub(super) const PROFILES_TAB: usize = SETTING_SECTIONS.len();
pub(super) const PROVIDER_LISTS: [&[&str]; 2] = [&models::PROVIDERS, &crate::tools::web::SEARCH_PROVIDERS];
pub(super) const CONTEXT_WINDOWS: [Option<u64>; 5] = [None, Some(200_000), Some(272_000), Some(400_000), Some(1_000_000)];
pub(super) const RESERVE_TOKENS: [u64; 5] = [16_384, 32_768, 65_536, 100_000, 150_000];
pub(super) const KEEP_RECENT_TOKENS: [u64; 4] = [10_000, 20_000, 40_000, 80_000];
pub(super) const CONDENSE_MIN_CHARS: [u64; 4] = [10_000, 20_000, 40_000, 80_000];
// 0 = never switch accounts.
pub(super) const SWITCH_AT_PERCENT: [u64; 6] = [70, 80, 90, 95, 100, 0];

impl App {
    pub(super) fn settings_key(&mut self, key: KeyEvent) {
        let Some(Panel::Settings { row, query }) = self.panel.as_mut() else { return };
        let code = key.code;
        let rows = matching_settings(*row, &query.text());
        let index = rows.iter().position(|r| r == row);
        match code {
            KeyCode::Tab | KeyCode::BackTab => {
                let count = PROFILES_TAB + 1;
                let section = setting_section(*row);
                let next = if code == KeyCode::Tab { (section + 1) % count } else { (section + count - 1) % count };
                self.panel = Some(settings_tab(next));
            }
            KeyCode::Enter if *row == Setting::ProviderOrder && index.is_some() => self.panel = Some(Panel::ProviderOrder { tab: 0, index: 0 }),
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

    pub(super) fn settings_panel(&self, row: &Setting, query: &Editor, width: usize) -> Vec<Line> {
        let matches = matching_settings(*row, &query.text());
        let rows: Vec<list::Row> = matches.iter().map(|&r| list::Row { left: r.name().to_string(), right: self.setting_value(r) }).collect();
        let view = list::List {
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

    /// Moves setting `row` to its next (or previous) value, then saves and applies it.
    pub(super) fn change_setting(&mut self, row: Setting, forward: bool) {
        use Setting::*;
        match row {
            Model | CondenseModel | FallbackModel | ReviewerModel | SubagentModel | SmallModel => return self.open_models(row, "", ModelsBack::Settings),
            ContextWindow => return self.open_context_windows(),
            HeadroomUrl | HeadroomToken => {
                let mut input = Editor::new();
                input.set(if row == HeadroomUrl { &self.settings.headroom_url } else { &self.settings.headroom_token });
                self.panel = Some(Panel::Headroom { row, input });
                return;
            }
            _ => {}
        }
        self.update_settings(|s| match row {
            Thinking => s.thinking = step(&THINKING_LEVELS, &s.thinking.as_str(), forward).to_string(),
            PermissionMode => s.permission_mode = step(&MODES, &s.permission_mode, forward),
            AutoCompact => s.auto_compact = !s.auto_compact,
            CompactionReserve => s.reserve_tokens = step(&RESERVE_TOKENS, &s.reserve_tokens, forward),
            KeepRecent => s.keep_recent_tokens = step(&KEEP_RECENT_TOKENS, &s.keep_recent_tokens, forward),
            ExpandTools => s.expand_tools = !s.expand_tools,
            Condense => s.condense = !s.condense,
            CondenseMinBatch => s.condense_min_batch_chars = step(&CONDENSE_MIN_CHARS, &s.condense_min_batch_chars, forward),
            SwitchAt => s.switch_at_percent = step(&SWITCH_AT_PERCENT, &s.switch_at_percent, forward),
            WhenAllOverLimit => s.when_all_over_limit = step(&Policy::ALL, &s.when_all_over_limit, forward),
            Ponytail => s.ponytail = step(&PONYTAIL_LEVELS, &s.ponytail.as_str(), forward).to_string(),
            Caveman => s.caveman = step(&CAVEMAN_LEVELS, &s.caveman.as_str(), forward).to_string(),
            Rtk => s.rtk = !s.rtk,
            HeadroomProxy => s.headroom = !s.headroom,
            SubagentEffort => s.subagent_effort = step(&THINKING_LEVELS, &s.subagent_effort.as_str(), forward).to_string(),
            Notifications => s.notifications = !s.notifications,
            ScrollLines => s.scroll_lines = step(&SCROLL_LINES, &s.scroll_lines, forward),
            CopyOnSelect => s.copy_on_select = !s.copy_on_select,
            UpdateChannel => s.update_channel = step(&crate::update::CHANNELS, &s.update_channel.as_str(), forward).to_string(),
            MenuRows => s.menu_rows = step(&MENU_ROWS, &s.menu_rows, forward),
            PruneSessions => s.prune_sessions = step(&PRUNE_SESSIONS, &s.prune_sessions.as_str(), forward).to_string(),
            ProviderOrder => s.provider_sort = step(&PROVIDER_SORTS, &s.provider_sort.as_str(), forward).to_string(),
            ConflictPriority => s.conflict_priority = step(&CONFLICT_PRIORITIES, &s.conflict_priority.as_str(), forward).to_string(),
            Speed => s.speed = step(&SPEEDS, &s.speed.as_str(), forward).to_string(),
            CacheTtl => s.cache_ttl = step(&CACHE_TTLS, &s.cache_ttl.as_str(), forward).to_string(),
            FooterCost => s.footer_cost = !s.footer_cost,
            FooterAccount => s.footer_account = !s.footer_account,
            ProgressBar => s.progress_bar = !s.progress_bar,
            MidTurn => s.mid_turn = step(&MID_TURN, &s.mid_turn.as_str(), forward).to_string(),
            MessageThreads => s.message_threads = !s.message_threads,
            OutputStyle => s.output_style = step(&OUTPUT_STYLES, &s.output_style.as_str(), forward).to_string(),
            Model | CondenseModel | FallbackModel | ReviewerModel | SubagentModel | SmallModel | ContextWindow | HeadroomUrl | HeadroomToken => {
                unreachable!("{row:?} opens its own panel")
            }
        });
    }

    pub(super) fn setting_value(&self, row: Setting) -> Line {
        let s = &self.settings;
        let auto = |model: &str| vec![span(if model.is_empty() { "auto" } else { model }, BRIGHT.bold())];
        match row {
            Setting::Model => vec![span(self.model().map_or(LOADING_MODELS.into(), |m| format!("{} · {}", m.provider, m.id)), BRIGHT.bold())],
            Setting::Thinking => choices(THINKING_LEVELS.map(String::from), &s.thinking),
            Setting::PermissionMode => choices(MODES.map(|m| m.label().to_string()), s.permission_mode.label()),
            Setting::AutoCompact => on_off_choices(s.auto_compact),
            Setting::CompactionReserve => choices(RESERVE_TOKENS.map(tokens), &tokens(s.reserve_tokens)),
            Setting::KeepRecent => choices(KEEP_RECENT_TOKENS.map(tokens), &tokens(s.keep_recent_tokens)),
            Setting::ExpandTools => on_off_choices(s.expand_tools),
            Setting::Condense => on_off_choices(s.condense),
            Setting::CondenseModel => auto(&s.condense_model),
            Setting::CondenseMinBatch => choices(CONDENSE_MIN_CHARS.map(|n| n.to_string()), &s.condense_min_batch_chars.to_string()),
            Setting::SwitchAt => choices(SWITCH_AT_PERCENT.map(switch_label), &switch_label(s.switch_at_percent)),
            Setting::WhenAllOverLimit => choices(Policy::ALL.map(|p| p.label().to_string()), s.when_all_over_limit.label()),
            Setting::FallbackModel => auto(&s.fallback_model),
            Setting::ContextWindow => match self.model() {
                Some(model) => vec![span(tokens(s.context_window(&model)), BRIGHT.bold()), span(format!("  {}", window_source(s, &model)), MUTED)],
                None => vec![span(LOADING_MODELS, MUTED)],
            },
            Setting::Ponytail => choices(PONYTAIL_LEVELS.map(String::from), &s.ponytail),
            Setting::Caveman => choices(CAVEMAN_LEVELS.map(String::from), &s.caveman),
            Setting::Rtk => optional_tool(s.rtk, "rtk"),
            Setting::HeadroomProxy => {
                if s.headroom_url.is_empty() {
                    optional_tool(s.headroom, "headroom")
                } else {
                    on_off_choices(s.headroom)
                }
            }
            Setting::SubagentEffort => choices(THINKING_LEVELS.map(String::from), &s.subagent_effort),
            Setting::Notifications => on_off_choices(s.notifications),
            Setting::ScrollLines => choices(SCROLL_LINES.map(|n| n.to_string()), &s.scroll_lines.to_string()),
            Setting::CopyOnSelect => on_off_choices(s.copy_on_select),
            Setting::UpdateChannel => choices(crate::update::CHANNELS.map(String::from), &s.update_channel),
            Setting::MenuRows => choices(MENU_ROWS.map(|n| n.to_string()), &s.menu_rows.to_string()),
            Setting::PruneSessions => choices(PRUNE_SESSIONS.map(String::from), &s.prune_sessions),
            Setting::ProviderOrder => [choices(PROVIDER_SORTS.map(String::from), &s.provider_sort), vec![span("   enter reorder", MUTED)]].concat(),
            Setting::ConflictPriority => choices(CONFLICT_PRIORITIES.map(String::from), &s.conflict_priority),
            Setting::Speed => choices(SPEEDS.map(String::from), &s.speed),
            Setting::CacheTtl => choices(CACHE_TTLS.map(String::from), &s.cache_ttl),
            Setting::ReviewerModel => auto(&s.reviewer_model),
            Setting::SubagentModel => auto(&s.subagent_model),
            Setting::FooterCost => on_off_choices(s.footer_cost),
            Setting::FooterAccount => on_off_choices(s.footer_account),
            Setting::ProgressBar => on_off_choices(s.progress_bar),
            Setting::HeadroomUrl => match s.headroom_url.as_str() {
                "" => vec![span("local", BRIGHT.bold()), span("   enter edit", MUTED)],
                url => vec![span(url, BRIGHT.bold()), span("   enter edit", MUTED)],
            },
            Setting::HeadroomToken => vec![span(if s.headroom_token.is_empty() { "none" } else { "set" }, BRIGHT.bold()), span("   enter edit", MUTED)],
            Setting::MidTurn => choices(MID_TURN.map(String::from), &s.mid_turn),
            Setting::MessageThreads => {
                if s.message_threads && !self.client.message_threads() {
                    [on_off_choices(true), vec![span("  unused behind headroom or ANTHROPIC_BASE_URL", MUTED)]].concat()
                } else {
                    on_off_choices(s.message_threads)
                }
            }
            Setting::SmallModel => auto(&s.small_model),
            Setting::OutputStyle => choices(OUTPUT_STYLES.map(String::from), &s.output_style),
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

    pub(super) fn models_panel(&self, row: &Setting, query: &Editor, index: &usize, width: usize) -> Vec<Line> {
        let rows: Vec<list::Row> = matching_models(&self.client.catalog(), &self.settings, *row, &query.text())
            .into_iter()
            .map(|m| match m {
                Some(m) => {
                    let pin = if self.settings.pinned_models.contains(&model_key(m)) { "★ " } else { "" };
                    list::Row { left: format!("{pin}{}", m.id), right: vec![span(model_owners(m), MUTED)] }
                }
                None => list::Row { left: "auto".into(), right: vec![] },
            })
            .collect();
        let view = list::List {
            title: row.name().into(),
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

    /// The model picker for settings `row`, on the current model when it matches `query`.
    pub(super) fn open_models(&mut self, row: Setting, query: &str, back: ModelsBack) {
        let current = model_setting(&mut self.settings, row).clone();
        let index = matching_models(&self.client.catalog(), &self.settings, row, query).iter().position(|m| m.map_or("", |m| &m.id) == current).unwrap_or(0);
        self.panel = Some(Panel::Models { row, query: Editor::from_text(query), index, back });
    }

    pub(super) fn open_context_windows(&mut self) {
        let Some(current) = self.loaded_model_key() else { return };
        let index = context_window_targets(&self.client.catalog(), &self.settings, 1, "").iter().position(|(key, _)| *key == current).unwrap_or(0);
        self.panel = Some(Panel::ContextWindows { tab: 1, query: Editor::new(), index });
    }

    pub(super) fn context_windows_key(&mut self, key: KeyEvent) {
        let Some(Panel::ContextWindows { tab, query, index }) = self.panel.as_mut() else { return };
        let catalog = self.client.catalog();
        let targets = context_window_targets(&catalog, &self.settings, *tab, &query.text());
        match key.code {
            KeyCode::Esc => self.panel = Some(Panel::Settings { row: Setting::ContextWindow, query: Editor::new() }),
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

    pub(super) fn context_windows_panel(&self, tab: &usize, query: &Editor, index: &usize, width: usize) -> Vec<Line> {
        let rows: Vec<list::Row> = context_window_targets(&self.client.catalog(), &self.settings, *tab, &query.text())
            .into_iter()
            .map(|(target, model)| {
                let mut right = choices(CONTEXT_WINDOWS.map(context_window_label), &context_window_label(self.settings.context_windows.get(&target).copied()));
                let Some(model) = model else { return list::Row { left: target, right } };
                right.push(span(format!("   {} catalog {}", model.provider, tokens(model.context_window)), MUTED));
                list::Row { left: model.id.clone(), right }
            })
            .collect();
        let view = list::List {
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

    pub(super) fn headroom_key(&mut self, key: KeyEvent) {
        let Some(Panel::Headroom { row, input }) = self.panel.as_mut() else { return };
        let row = *row;
        match key.code {
            KeyCode::Esc => {}
            KeyCode::Enter if row == Setting::HeadroomUrl => {
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

    pub(super) fn headroom_panel(&self, row: &Setting, input: &Editor, width: usize) -> Vec<Line> {
        let token = *row == Setting::HeadroomToken;
        let view = list::List {
            title: row.name().into(),
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

    pub(super) fn provider_order_key(&mut self, key: KeyEvent) {
        let Some(Panel::ProviderOrder { tab, index }) = self.panel.as_mut() else { return };
        let providers = PROVIDER_LISTS[*tab];
        let last = providers.len() - 1;
        match key.code {
            KeyCode::Esc => self.panel = Some(Panel::Settings { row: Setting::ProviderOrder, query: Editor::new() }),
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

    pub(super) fn provider_order_panel(&self, tab: &usize, index: &usize, width: usize) -> Vec<Line> {
        let rows: Vec<list::Row> = self.settings.sort_providers(PROVIDER_LISTS[*tab]).into_iter().map(|provider| list::Row { left: provider.into(), right: vec![] }).collect();
        let view = list::List {
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

    pub(super) fn profiles_panel(&self, index: &usize, naming: &Option<Editor>, confirm: &bool, width: usize) -> Vec<Line> {
        let renaming = self.settings.skill_profiles.keys().nth(*index);
        if let Some(input) = naming {
            let view = list::List {
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
        let mut rows: Vec<list::Row> = self
            .settings
            .skill_profiles
            .keys()
            .map(|profile| {
                let state = if self.settings.active_profiles.contains(profile) { "on" } else { "off" };
                let mut right = choices(["off", "on"].map(String::from), state);
                right.push(span(format!("     {}", self.profile_skills(profile)), MUTED));
                list::Row { left: profile.clone(), right }
            })
            .collect();
        rows.push(list::Row { left: "New profile".into(), right: vec![span("enter name", MUTED)] });
        let hint = match renaming {
            Some(profile) if *confirm => format!("d again deletes {profile}     any other key keeps it"),
            Some(_) => "↑↓ navigate     ←→ on/off     enter edit skills     r rename     d delete     tab section     esc close".into(),
            None => "↑↓ navigate     enter new profile     tab section     esc close".into(),
        };
        let view = list::List { title: "Settings".into(), tabs: &setting_tabs(), tab: PROFILES_TAB, search: None, rows: &rows, selected: *index, empty: "", hint: &hint };
        view.render(width, PANEL_ROWS)
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

    pub(super) fn profile_skills_panel(&self, profile: &str, query: &Editor, index: &usize, width: usize) -> Vec<Line> {
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
        let view = list::List {
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
}

pub(super) fn context_window_label(window: Option<u64>) -> String {
    window.map_or("auto".to_string(), tokens)
}

/// The rows of `row`'s section, or with a query the rows of every section whose name matches it, see `list::search`.
pub(super) fn matching_settings(row: Setting, query: &str) -> Vec<Setting> {
    if query.is_empty() {
        return SETTING_SECTIONS[setting_section(row)].1.to_vec();
    }
    list::search(SETTING_SECTIONS.iter().flat_map(|(_, rows)| rows.iter().copied()), query, |r| r.name().to_string())
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

pub(super) fn setting_section(row: Setting) -> usize {
    SETTING_SECTIONS.iter().position(|(_, rows)| rows.contains(&row)).expect("every setting row is in a section")
}

/// Override keys for the context windows panel: providers on tab 0, models matching `query` on tab 1.
pub(super) fn context_window_targets<'a>(catalog: &'a [Model], settings: &Settings, tab: usize, query: &str) -> Vec<(String, Option<&'a Model>)> {
    if tab == 0 {
        return settings.sort_providers(&models::PROVIDERS).into_iter().map(|provider| (provider.to_string(), None)).collect();
    }
    matching_models(catalog, settings, Setting::Model, query).into_iter().flatten().map(|model| (model_key(model), Some(model))).collect()
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

/// Picker rows for settings `row` whose provider or id matches `query` (see `list::search`): pinned models first, each part grouped in provider order.
/// `None` is "auto", offered for the optional condense and fallback models.
pub(super) fn matching_models<'a>(catalog: &'a [Model], settings: &Settings, row: Setting, query: &str) -> Vec<Option<&'a Model>> {
    let auto = (row != Setting::Model && list::search_rank("auto", query).is_some()).then_some(None);
    let providers = settings.sort_providers(&models::PROVIDERS);
    let mut matches = list::search(catalog, query, |m| format!("{} {}", model_owners(m), m.id));
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

/// The model id settings `row` edits.
pub(super) fn model_setting(settings: &mut Settings, row: Setting) -> &mut String {
    match row {
        Setting::Model => &mut settings.model,
        Setting::CondenseModel => &mut settings.condense_model,
        Setting::FallbackModel => &mut settings.fallback_model,
        Setting::ReviewerModel => &mut settings.reviewer_model,
        Setting::SubagentModel => &mut settings.subagent_model,
        Setting::SmallModel => &mut settings.small_model,
        _ => unreachable!("{row:?} is not a model setting"),
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
