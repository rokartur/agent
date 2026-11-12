use super::*;

impl App {
    pub(super) fn skills_key(&mut self, key: KeyEvent) {
        let Some(Panel::Skills { query, index }) = self.panel.as_mut() else { return };
        let code = key.code;
        match code {
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
        }
    }

    pub(super) fn skills_panel(&self, query: &Editor, index: &usize, width: usize) -> Vec<Line> {
        let rows = self.skill_rows(&query.text(), width, |skill| {
            if self.settings.disabled_skills.contains(&skill.name) {
                ("[ ]", span("disabled", MUTED))
            } else if !self.settings.skill_enabled(skill) {
                ("[ ]", span("profile off", MUTED))
            } else {
                ("[x]", span("enabled", PLAIN))
            }
        });
        let view = list::List {
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

    pub(super) fn mcp_key(&mut self, key: KeyEvent) {
        let Some(Panel::Mcp { index, confirm }) = self.panel.as_mut() else { return };
        let code = key.code;
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

    pub(super) fn mcp_panel(&self, index: &usize, confirm: &bool, width: usize) -> Vec<Line> {
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
            rows.push(list::Row { left: format!("{mark} {:<name_width$}  {}", view.name, view.target), right });
        }
        let empty = match &error {
            Some(error) => error.as_str(),
            None => "No MCP servers. Add one to ~/.agents/mcp.json or .mcp.json under \"mcpServers\".",
        };
        let view = list::List {
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

    pub(super) fn plugins_key(&mut self, code: KeyCode) {
        let Some(Panel::Plugins { index, confirm }) = self.panel.as_mut() else { return };
        if code == KeyCode::Enter {
            if let Some(view) = self.plugins.list().into_iter().nth(*index).filter(|view| view.installed) {
                self.panel = Some(Panel::PluginSettings { plugin: view.name, index: 0, edit: None });
            }
            return;
        }
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
            KeyCode::Char('u') if view.installed => crate::plugins::update(&self.home, std::slice::from_ref(&view.name)).map(|updates| {
                for update in updates {
                    self.system("plugins", update.notice());
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

    pub(super) fn plugins_panel(&self, index: &usize, confirm: &bool, width: usize) -> Vec<Line> {
        let views = self.plugins.list();
        let name_width = views.iter().map(|view| view.name.len()).max().unwrap_or(0);
        let update_width = views.iter().filter_map(|view| view.update.as_ref()).map(|version| "update ".len() + version.len()).max().unwrap_or(0);
        let rows: Vec<list::Row> = views
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
                list::Row { left: format!("{:<name_width$}  {}", view.name, view.version), right }
            })
            .collect();
        let view = list::List {
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

    pub(super) fn plugin_settings_panel(&self, plugin: &str, index: &usize, edit: &Option<Editor>, width: usize) -> Vec<Line> {
        let declared = self.plugins.list().into_iter().find(|view| view.name == plugin).map(|view| view.settings).unwrap_or_default();
        let rows: Vec<list::Row> = declared
            .iter()
            .enumerate()
            .map(|(row, entry)| {
                let key = entry["key"].as_str().unwrap_or_default();
                let value = crate::plugins::setting(entry, &self.settings.plugin_settings[plugin]);
                let right = match (edit, value) {
                    (Some(editor), _) if row == *index => format!("{}▏", editor.text()),
                    (_, Value::Bool(on)) => (if on { "on" } else { "off" }).to_string(),
                    (_, value) => value.as_str().unwrap_or_default().to_string(),
                };
                list::Row { left: format!("{key}  {}", entry["description"].as_str().unwrap_or_default()), right: vec![span(right, PLAIN)] }
            })
            .collect();
        let view = list::List {
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

    pub(super) fn plugin_key(&mut self, code: KeyCode) {
        let Some(Panel::Plugin { plugin, data, index }) = self.panel.as_mut() else { return };
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
    }

    pub(super) fn plugin_panel(&self, plugin: &str, data: &Value, index: &usize, width: usize) -> Vec<Line> {
        let rows: Vec<list::Row> = data["rows"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|row| list::Row { left: row["left"].as_str().unwrap_or_default().to_string(), right: vec![span(row["right"].as_str().unwrap_or_default().to_string(), PLAIN)] })
            .collect();
        let hint = format!("↑↓ navigate     {}     esc close", data["hint"].as_str().unwrap_or("enter select"));
        let view = list::List { title: data["title"].as_str().unwrap_or(plugin).to_string(), tabs: &[], tab: 0, search: None, rows: &rows, selected: *index, empty: "Nothing here.", hint: &hint };
        view.render(width, PANEL_ROWS)
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

    pub(super) fn environments_panel(&self, place: &Place, index: &usize, naming: &Option<Editor>, confirm: &bool, width: usize) -> Vec<Line> {
        if let Some(input) = naming {
            let view = list::List {
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
        let mut rows: Vec<list::Row> = place
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
                list::Row { left: environment.clone(), right: vec![span(note, MUTED)] }
            })
            .collect();
        if editable {
            rows.push(list::Row { left: "Add environment".into(), right: vec![span("enter name", MUTED)] });
        }
        let hint = match place.environments.get(*index) {
            Some(environment) if *confirm => format!("d again removes {environment}     any other key keeps it"),
            Some(environment) if *environment == place.environment => "↑↓ navigate     esc close".into(),
            Some(environment) if saved(environment) => "↑↓ navigate     enter start a session there     d remove     esc close".into(),
            Some(_) => "↑↓ navigate     enter start a session there     esc close".into(),
            None => "↑↓ navigate     enter add environment     esc close".into(),
        };
        let view = list::List { title: "Environments".into(), tabs: &[], tab: 0, search: None, rows: &rows, selected: *index, empty: "", hint: &hint };
        view.render(width, PANEL_ROWS)
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
}
