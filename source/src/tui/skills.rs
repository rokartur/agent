use super::*;

impl App {
    pub(super) fn enabled_skills(&self) -> impl Iterator<Item = &Skill> {
        self.skills.iter().filter(|skill| self.settings.skill_enabled(skill))
    }

    /// "1 skill" or "N skills" the profile holds among the loaded ones.
    pub(super) fn profile_skills(&self, profile: &str) -> String {
        let count = self.skills.iter().filter(|skill| self.settings.profile_holds(profile, skill)).count();
        if count == 1 { "1 skill".into() } else { format!("{count} skills") }
    }

    /// The skills matching `query` as checkbox rows with aligned name, repo and description columns.
    /// The description gives way so the state on the right fits in `columns`.
    pub(super) fn skill_rows(&self, query: &str, columns: usize, state: impl Fn(&Skill) -> (&'static str, Span)) -> Vec<list::Row> {
        let name_width = self.skills.iter().map(|skill| skill.name.chars().count()).max().unwrap_or(0);
        let source_width = self.skills.iter().filter_map(|skill| skill.source.as_ref()).map(|source| source.chars().count()).max().unwrap_or(0);
        let skills: Vec<(&Skill, &str, Span)> = matching_skills(&self.skills, query)
            .into_iter()
            .map(|skill| {
                let (mark, right) = state(skill);
                (skill, mark, right)
            })
            .collect();
        // The panel puts `→ ` before a row and four spaces before its right side.
        let room = columns.saturating_sub(2 + 4 + skills.iter().map(|(_, _, right)| render::width(&right.text)).max().unwrap_or(0));
        let mut rows = Vec::new();
        for (skill, mark, right) in skills {
            let description = skill.description.split_whitespace().collect::<Vec<_>>().join(" ");
            let source = skill.source.as_deref().unwrap_or_default();
            let left = format!("{mark} {:<name_width$}  {source:<source_width$}  {description}", skill.name);
            let left = render::truncate(&vec![span(left, PLAIN)], room).into_iter().map(|cut| cut.text).collect();
            rows.push(list::Row { left, right: vec![right] });
        }
        rows
    }

    pub(super) fn toggle_profile(&mut self, profile: &str) {
        if !self.settings.skill_profiles.contains_key(profile) {
            return self.push(Item::Error(format!("No profile {profile}. Add it to skill_profiles in ~/.agents/settings.json.")));
        }
        self.update_settings(|s| s.toggle_profile(profile));
        let on = self.settings.active_profiles.iter().any(|active| active == profile);
        self.system("profile", format!("{profile} {}: {}", if on { "on" } else { "off" }, self.profile_skills(profile)));
    }

    /// Every skill, the disabled ones too: /skills lists them, `enabled_skills` filters by `Settings.skill_enabled`.
    pub(super) fn load_skills_and_prompts(&mut self) {
        let agent_dir = self.home.join(".agents");
        let trusted = self.settings.trusts(&self.cwd);
        self.skills = crate::agent::prompt::load_skills(&agent_dir, &self.cwd, trusted, |_| true, self.settings.prefer_global());
        self.templates = crate::agent::prompt::load_templates(&agent_dir, &self.cwd, trusted, self.settings.prefer_global());
        self.saved_workflows = workflow::saved(&crate::agent::prompt::project_agents_dirs(&self.cwd, trusted), &agent_dir);
    }

    /// Asks whether to trust a project with `.agents` files to load; another open panel gets a notice instead.
    pub(super) fn offer_trust(&mut self) {
        let files = crate::agent::prompt::untrusted_files(&self.home.join(".agents"), &self.cwd);
        if self.settings.trusts(&self.cwd) {
            for ignored in settings::ignored_project_keys(&self.home, &self.cwd, true) {
                self.warn("settings", ignored);
            }
            return;
        }
        if files.is_empty() {
            return;
        }
        let root = crate::agent::prompt::project_root(&self.cwd);
        if self.panel.is_some() {
            self.warn("trust", format!("{} is not trusted, so its .agents files stay unloaded and its edit hooks do not run. /trust loads them.", root.display()));
            return;
        }
        self.panel = Some(Panel::Trust { root, files, opened: Instant::now() });
    }

    /// /trust and /untrust; the agent rebuilds its system prompt once, before the next turn.
    pub(super) fn set_trust(&mut self, trusted: bool) {
        let root = crate::agent::prompt::project_root(&self.cwd).display().to_string();
        if trusted {
            // A --trust root is already in the list; dropping it first makes the save see /trust as a change.
            self.settings.trusted_projects.retain(|project| *project != root);
            self.update_settings(|s| s.trusted_projects.push(root.clone()));
        } else {
            self.update_settings(|s| s.trusted_projects.retain(|project| *project != root));
        }
        let text = if trusted { "trusted: its .agents files load and its edit hooks run" } else { "not trusted: its .agents files stay unloaded and its edit hooks do not run" };
        self.system("trust", format!("{root} {text}"));
    }

    /// Names each skill and prompt defined both in the project and in `~/.agents`; `conflict_priority` says which copy loads.
    pub(super) fn report_conflicts(&mut self) {
        let skills: Vec<&str> = self.skills.iter().filter(|s| s.shadows.is_some()).map(|s| s.name.as_str()).collect();
        let prompts: Vec<&str> = self.templates.iter().filter(|t| t.shadows.is_some()).map(|t| t.name.as_str()).collect();
        let count = skills.len() + prompts.len();
        if count == 0 {
            return;
        }
        let winner = if self.settings.prefer_global() { "~/.agents" } else { "project" };
        let mut text = format!("{count} names are in both the project and ~/.agents, the {winner} copy is used (/settings: Name conflicts prefer)");
        for (kind, names) in [("skills ", skills), ("prompts", prompts)] {
            if !names.is_empty() {
                text.push_str(&format!("\n{kind}  {}", names.join(", ")));
            }
        }
        self.system("conflicts", text);
    }

    pub(super) fn skill_usage(&mut self) {
        let log = std::fs::read_to_string(self.home.join(".agents/skill-usage.jsonl")).unwrap_or_default();
        let names: Vec<&str> = self.skills.iter().map(|skill| skill.name.as_str()).collect();
        let lines: Vec<String> = skill_usage::counts(&log, &names).iter().map(|(name, count)| format!("{count:>5}  {name}")).collect();
        self.system("skill-usage", format!("uses per skill, least used first\n{}", lines.join("\n")));
    }
}
