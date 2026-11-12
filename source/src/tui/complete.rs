use super::*;

impl App {
    pub(super) fn menu_matches(&self) -> Vec<(String, String)> {
        if let Some(query) = self.workspace_query() {
            return self.workspace_matches(query);
        }
        match self.file_query() {
            Some(query) => self.listed_paths(query),
            None => {
                // A closed menu forgets its listing, so typing `@` again sees files created since.
                self.path_matches.take();
                let mut commands = self.commands_without_skills();
                // A skill named like a command stays reachable as `/skill:name`.
                let skills: Vec<_> = self
                    .enabled_skills()
                    .map(|skill| {
                        let name = if commands.iter().any(|(command, _)| *command == skill.name) { format!("skill:{}", skill.name) } else { skill.name.clone() };
                        let description = skill.description.split_whitespace().collect::<Vec<_>>().join(" ");
                        let description = match &skill.argument_hint {
                            Some(hint) => format!("{hint} {description}"),
                            None => description,
                        };
                        (name, description)
                    })
                    .collect();
                if let Some(word) = self.skill_query() {
                    return matching_commands(&skills, &format!("/{word}"));
                }
                commands.extend(skills);
                matching_commands(&commands, slash_query(&self.editor.text(), self.editor.before_cursor()))
            }
        }
    }

    pub(super) fn commands_without_skills(&self) -> Vec<(String, String)> {
        let mut commands = self.slash_commands.clone();
        commands.extend(self.templates.iter().map(|template| (format!("prompt:{}", template.name), template.description.clone())));
        commands.extend(self.settings.skill_profiles.keys().map(|profile| {
            let action = if self.settings.active_profiles.contains(profile) { "turn off" } else { "turn on" };
            (format!("profile:{profile}"), format!("{action} {}", self.profile_skills(profile)))
        }));
        commands.extend(self.plugins.commands());
        commands.extend(self.saved_workflows.iter().map(|saved| (saved.meta.name.clone(), saved.meta.description.clone())));
        commands
    }

    /// The `@` token right before the cursor, without the `@`.
    pub(super) fn file_query(&self) -> Option<&str> {
        self.editor.before_cursor().rsplit(char::is_whitespace).next()?.strip_prefix('@')
    }

    /// The path typed after `/workspace add ` or `/workspace remove `, up to the cursor.
    pub(super) fn workspace_query(&self) -> Option<&str> {
        let before = self.editor.before_cursor();
        before.strip_prefix("/workspace add ").or_else(|| before.strip_prefix("/workspace remove "))
    }

    /// Remove offers the added directories; add offers directories, led by the typed one once it ends in `/` so Enter can add it.
    pub(super) fn workspace_matches(&self, query: &str) -> Vec<(String, String)> {
        if self.editor.before_cursor().starts_with("/workspace remove ") {
            let added = self.extra_dirs.iter().map(|dir| dir.display().to_string());
            return added.filter(|dir| dir.starts_with(query)).map(|dir| (dir, String::new())).collect();
        }
        let mut dirs = Vec::new();
        if query.ends_with('/') && crate::tools::hashline::resolve(&self.cwd, query).is_dir() {
            dirs.push((query.to_string(), "this directory".to_string()));
        }
        dirs.extend(self.listed_paths(query).into_iter().filter(|(path, _)| path.ends_with('/')));
        dirs
    }

    /// Redraws repeat every 80 ms while the agent works, so the listing runs once per typed query.
    /// Keyed by the text, not a `Path`: `..` and `../` compare equal as paths but list different entries.
    pub(super) fn listed_paths(&self, query: &str) -> Vec<(String, String)> {
        let mut cached = self.path_matches.borrow_mut();
        let key = format!("{}\n{query}", self.cwd.display());
        if cached.0 != key {
            *cached = (key, matching_paths(&self.cwd, query));
        }
        cached.1.clone()
    }

    pub(super) fn skill_query(&self) -> Option<&str> {
        mid_text_skill(self.editor.before_cursor())
    }

    /// A leading `/name` of an enabled skill that no command takes becomes the `/skill:name` command.
    pub(super) fn skill_command(&self, text: String) -> String {
        let commands = self.commands_without_skills();
        as_skill_command(text, |name| self.enabled_skills().any(|skill| skill.name == name) && !commands.iter().any(|(command, _)| command == name))
    }

    pub(super) fn accept_skill(&mut self, name: &str) {
        let typed = self.skill_query().map_or(0, |word| word.chars().count() + 1);
        for _ in 0..typed {
            self.editor.backspace();
        }
        // Only a leading `/name` expands on submit, so the pick moves to the front.
        let text = self.editor.text();
        self.editor.set(&format!("/{name} {}", text.trim_start()));
        self.menu_index = 0;
    }

    /// Swaps the typed query for the path; a directory keeps the menu open for its entries.
    pub(super) fn accept_path(&mut self, path: &str) {
        let typed = self.file_query().or(self.workspace_query()).map_or(0, |query| query.chars().count());
        for _ in 0..typed {
            self.editor.backspace();
        }
        self.editor.insert(path);
        if !path.ends_with('/') {
            self.editor.insert(" ");
        }
        self.menu_index = 0;
    }

    /// Fills the editor with the selected command; commands without arguments run right away on Enter.
    /// A `/` typed in front of existing text keeps that text as the command's arguments, or in the editor for a command without them.
    pub(super) fn accept_slash(&mut self, run: bool) {
        let matches = self.menu_matches();
        let name = matches[self.menu_index.min(matches.len() - 1)].0.clone();
        let takes_args = matches!(name.as_str(), "thinking" | "effort" | "rename-title" | "btw" | "goal" | "import" | "workspace add" | "workspace remove")
            || name.starts_with("prompt:")
            || name.starts_with("skill:")
            || self.skill_command(format!("/{name}")).starts_with("/skill:")
            || self.saved_workflows.iter().any(|saved| saved.meta.name == name);
        let text = self.editor.text();
        let rest = text[slash_query(&text, self.editor.before_cursor()).len()..].trim().to_string();
        if takes_args {
            self.editor.set(&format!("/{name} {rest}"));
        } else if run || !rest.is_empty() {
            self.editor.set(&rest);
            self.submit(format!("/{name}"), false);
        } else {
            self.editor.set(&format!("/{name}"));
        }
    }

    pub(super) fn menu(&self) -> Vec<Line> {
        let matches = self.menu_matches();
        if matches.is_empty() {
            return Vec::new();
        }
        let selected = self.menu_index.min(matches.len() - 1);
        let menu_rows = self.settings.menu_rows;
        let first = (selected + 1).saturating_sub(menu_rows);
        let mut rows = vec![Vec::new()];
        for (index, (name, description)) in matches.iter().enumerate().skip(first).take(menu_rows) {
            let label = format!("{name:<31} {description}");
            if index == selected {
                rows.push(vec![span("→", OK), span(format!(" {label}"), TOOL_NAME)]);
            } else {
                rows.push(vec![span(format!("  {label}"), TOOL_ARGS)]);
            }
        }
        if matches.len() > menu_rows {
            rows.extend([Vec::new(), vec![span(format!("  {}/{}", selected + 1, matches.len()), DIM)]]);
        }
        rows
    }
}

pub(super) fn matching_skills<'a>(skills: &'a [Skill], query: &str) -> Vec<&'a Skill> {
    panel::search(skills, query, |skill| format!("{} {}", skill.name, skill.source.as_deref().unwrap_or_default()))
}

/// Accounts whose name or provider matches the query, see `panel::search`.
pub(super) fn matching_accounts(accounts: Vec<AccountView>, query: &str) -> Vec<AccountView> {
    panel::search(accounts, query, |a| format!("{} {}", a.name, a.provider))
}

/// The prompts in scope that match `query`, each repeated prompt once at its best-ranked place.
pub(super) fn matching_prompts<'a>(entries: &'a [history::Entry], query: &str, in_scope: impl Fn(&history::Entry) -> bool) -> Vec<&'a str> {
    let mut seen = HashSet::new();
    let matches = panel::search(entries.iter().filter(|entry| in_scope(entry)), query, |entry| entry.prompt.clone());
    matches.into_iter().map(|entry| entry.prompt.as_str()).filter(|prompt| seen.insert(*prompt)).collect()
}

/// What the slash menu completes: the `/word` up to the cursor when the cursor sits in the text's first word, else the whole text.
pub(super) fn slash_query<'a>(text: &'a str, before_cursor: &'a str) -> &'a str {
    if text.starts_with(before_cursor) && !before_cursor.contains(char::is_whitespace) { before_cursor } else { text }
}

/// A `/word` right before the cursor with text in front of it, without the `/`: there the menu offers only skills.
/// A shell line and a path like `/tmp/x` open no menu.
pub(super) fn mid_text_skill(before: &str) -> Option<&str> {
    let word = before.rsplit(char::is_whitespace).next()?;
    let name = word.strip_prefix('/')?;
    let in_front = before[..before.len() - word.len()].trim();
    (!in_front.is_empty() && !before.starts_with('!') && !name.contains('/')).then_some(name)
}

/// `/name args` becomes `/skill:name args` when `is_skill(name)`.
pub(super) fn as_skill_command(text: String, is_skill: impl Fn(&str) -> bool) -> String {
    let Some(rest) = text.trim_start().strip_prefix('/') else { return text };
    let name = rest.split_once(char::is_whitespace).map_or(rest, |(name, _)| name);
    if is_skill(name) { format!("/skill:{rest}") } else { text }
}

/// Commands matching a typed `/name`: prefix matches first, then names containing it. Past the name, its known arguments.
pub(super) fn matching_commands(commands: &[(String, String)], text: &str) -> Vec<(String, String)> {
    let Some(query) = text.strip_prefix('/') else { return Vec::new() };
    if let Some((name, arg)) = query.split_once(' ') {
        return matching_args(name, arg);
    }
    let query = query.to_lowercase();
    let (mut matches, mut inner, mut fuzzy) = (Vec::new(), Vec::new(), Vec::new());
    for command in commands {
        if command.0.starts_with(&query) {
            matches.push(command.clone());
            continue;
        }
        match panel::search_rank(&command.0, &query) {
            Some(0) => inner.push(command.clone()),
            Some(_) => fuzzy.push(command.clone()),
            None => {}
        }
    }
    matches.extend(inner);
    matches.extend(fuzzy);
    matches
}

/// Fixed argument values of `name` starting with `arg`, as whole `name value` entries so accepting one fills or runs the command.
pub(super) fn matching_args(name: &str, arg: &str) -> Vec<(String, String)> {
    let values: Vec<(&str, &str)> = match name {
        "thinking" | "effort" => THINKING_LEVELS.iter().map(|level| (*level, "")).collect(),
        "ponytail" => PONYTAIL_LEVELS.iter().map(|level| (*level, "")).collect(),
        "caveman" => CAVEMAN_LEVELS.iter().map(|level| (*level, "")).collect(),
        "permissions" => MODES.iter().map(|mode| (mode.label(), mode.description())).collect(),
        "goal" => vec![("pause", "Pause the goal"), ("resume", "Resume the goal"), ("stop", "Stop the goal"), ("done", "Mark the goal done")],
        "plan" => vec![
            ("export", "Write the ready plan to <TITLE>_PLAN.md or the file named after it"),
            ("implement", "Pick model and effort, then implement the ready plan in a new session"),
            ("exit", "Leave plan mode, dropping a ready plan"),
        ],
        "context-window" => vec![("auto", "Drop this model's override")],
        "workspace" => vec![("add", "Add a directory"), ("remove", "Remove an added directory"), ("list", "List the added directories")],
        _ => return Vec::new(),
    };
    let arg = arg.to_lowercase();
    let mut matches = Vec::new();
    for (value, description) in values {
        if value.starts_with(&arg) {
            matches.push((format!("{name} {value}"), description.to_string()));
        }
    }
    matches
}

/// Entries of the query's directory whose names start with its last part, directories first with a trailing `/`.
/// A dotfile matches with or without its dot typed; `.git` shows only once the dot is typed.
/// A query without a `/` also lists deeper paths whose names start with it, after the top-level ones.
pub(super) fn matching_paths(cwd: &Path, query: &str) -> Vec<(String, String)> {
    let (dir, prefix) = query.split_at(query.rfind('/').map_or(0, |slash| slash + 1));
    let Ok(entries) = std::fs::read_dir(crate::tools::hashline::resolve(cwd, dir)) else { return Vec::new() };
    let prefix = prefix.to_lowercase();
    let mut paths = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let lower = name.to_lowercase();
        let undotted = lower.strip_prefix('.').unwrap_or(&lower);
        if !(lower.starts_with(&prefix) || undotted.starts_with(&prefix)) || (name == ".git" && !prefix.starts_with('.')) {
            continue;
        }
        let is_dir = entry.path().is_dir();
        paths.push((!is_dir, format!("{dir}{name}{}", if is_dir { "/" } else { "" })));
    }
    paths.sort();
    if dir.is_empty() && !prefix.is_empty() {
        paths.extend(nested_paths(cwd, &prefix));
    }
    paths.into_iter().map(|(_, path)| (path, String::new())).collect()
}

/// Paths below the top level whose names start with `prefix`, from `fd` so ignored trees like target/ stay out.
/// fd is killed after 150 ms, so a huge tree (a home directory takes 7 s) lists nothing deeper instead of freezing typing.
pub(super) fn nested_paths(cwd: &Path, prefix: &str) -> Vec<(bool, String)> {
    let mut fd = crate::process::command("fd");
    fd.args(["--color=never", "--ignore-case", "--glob", "--hidden", "--exclude=.git", "--min-depth=2", "--max-results=50", "--path-separator=/"]).current_dir(cwd);
    fd.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null());
    let Ok(mut child) = fd.arg(format!("{{{prefix}*,.{prefix}*}}")).spawn() else { return Vec::new() };
    let started = Instant::now();
    while child.try_wait().is_ok_and(|status| status.is_none()) {
        // Tests get longer: a loaded 3-core CI runner took over 150 ms to start fd.
        if started.elapsed() > Duration::from_millis(if cfg!(test) { 5000 } else { 150 }) {
            let _ = child.kill();
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let Ok(output) = child.wait_with_output() else { return Vec::new() };
    if !output.status.success() {
        return Vec::new();
    }
    let mut paths: Vec<(bool, String)> = String::from_utf8_lossy(&output.stdout).lines().map(|path| (!path.ends_with('/'), path.to_string())).collect();
    paths.sort();
    paths
}
