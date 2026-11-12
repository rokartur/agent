use super::*;

impl App {
    pub(super) fn footer(&self, width: usize) -> Vec<Line> {
        let home = std::env::var("HOME").unwrap_or_default();
        let tilde = |path: &Path| {
            let path = path.display().to_string();
            match path.strip_prefix(&home) {
                Some(rest) if !home.is_empty() => short_path(&format!("~{rest}")),
                _ => short_path(&path),
            }
        };
        let mut left = Vec::new();
        // Every remote client names the same host, the computer this session server runs on.
        if let Some(host) = self.clients.iter().map(|client| &client.place.environment).find(|environment| *environment != crate::tailnet::LOCAL) {
            left.push(span(host, SYSTEM.bold()));
            left.push(span(FOOTER_SEPARATOR, DIM));
        }
        left.push(span(tilde(&self.cwd), PLAIN.bold()));
        if let Some(branch) = &self.git.branch {
            left.push(span(" ", PLAIN));
            left.push(span(branch, MUTED));
        }
        if self.git.changed > 0 {
            left.push(span(" ", PLAIN));
            left.push(span(format!("{} changed", self.git.changed), WARN));
        }
        if self.git.ahead > 0 {
            left.push(span(format!(" ↑{} to push", self.git.ahead), WARN));
        }
        if self.git.behind > 0 {
            left.push(span(format!(" ↓{} to pull", self.git.behind), WARN));
        }
        if let Some(main) = &self.git.main {
            left.push(span(format!(" → {} {}", main.branch, tilde(&main.path)), WARN));
        }
        if self.git.others > 0 {
            left.push(span(format!(" +{} wt", self.git.others), MUTED));
        }
        for dir in &self.extra_dirs {
            let name = dir.file_name().map_or(dir.display().to_string(), |name| name.to_string_lossy().into_owned());
            left.push(span(format!(" + {name}"), MUTED));
        }
        if let Some(goal) = self.goal.lock().unwrap().as_ref() {
            left.push(span(FOOTER_SEPARATOR, DIM));
            left.push(span(format!("goal {}", goal.status.as_str()), MUTED));
        }
        let scheduled = self.crons.lock().unwrap().len();
        if scheduled > 0 {
            left.push(span(FOOTER_SEPARATOR, DIM));
            left.push(span(format!("{scheduled} scheduled"), MUTED));
        }
        match *self.plan.lock().unwrap() {
            Plan::Off => {}
            Plan::Drafting => left.extend([span(FOOTER_SEPARATOR, DIM), span("plan", WARN)]),
            Plan::Ready(_) => left.extend([span(FOOTER_SEPARATOR, DIM), span("plan ready", WARN)]),
        }
        let profiles: Vec<&str> = self.settings.active_profiles.iter().filter(|profile| self.settings.skill_profiles.contains_key(*profile)).map(String::as_str).collect();
        if !profiles.is_empty() {
            left.push(span(FOOTER_SEPARATOR, DIM));
            left.push(span(format!("profile {}", profiles.join(",")), MUTED));
        }
        let percent = (self.context_tokens * 100).checked_div(self.context_window()).unwrap_or(0);
        let mut right = Line::default();
        for status in self.plugins.statuses() {
            for (text, tone) in status {
                let style = match tone.as_deref() {
                    Some("ok") => OK,
                    Some("warn") => WARN,
                    Some("error") => ERROR,
                    _ => MUTED,
                };
                right.push(span(text, style));
            }
            right.push(span("  ", MUTED));
        }
        if self.settings.footer_account
            && let Some((name, _)) = self.model().and_then(|m| self.client.account(&m.provider))
        {
            right.push(span(format!("{name} "), MUTED));
        }
        right.push(span(self.model().as_ref().map_or(LOADING_MODELS.into(), models::short_label), MUTED));
        right.push(span(" ", PLAIN));
        right.push(span(&self.settings.thinking, thinking_style(&self.settings.thinking)));
        // Fast tiers bill up to 6x, so the footer names the one in use.
        if self.settings.speed != "standard" {
            right.push(span(format!(" {}", self.settings.speed), WARN));
        }
        if self.context_tokens > 0 {
            right.push(span(format!(" {} {percent}%", tokens(self.context_tokens)), context_style(percent)));
        }
        if self.settings.footer_cost
            && let Some(cost) = usage::total_cost(&self.session_usage, &self.client.catalog())
        {
            right.push(span(format!(" {}", usage::dollars(cost)), MUTED));
        }
        footer_rows(left, right, width)
    }
}

/// `left` word-wrapped to `width`, with `right` at the end of its last row, or on a row of its own when that one is full.
pub(super) fn footer_rows(left: Line, right: Line, width: usize) -> Vec<Line> {
    let mut rows = wrap(&left, width, &Vec::new());
    let mut last = rows.pop().expect("wrap returns at least one row");
    if line_width(&last) + 1 + line_width(&right) > width {
        rows.push(std::mem::take(&mut last));
    }
    rows.push(spread(last, right, width));
    rows
}

/// Running subagents counted in the footer pill; workflow agents show on their workflow's line instead.
pub(super) fn footer_agents(runs: &Runs) -> usize {
    runs.list.iter().filter(|run| run.is_running() && run.workflow.is_none()).count()
}

pub(super) fn context_style(percent: u64) -> Style {
    match percent {
        90.. => ERROR,
        70.. => WARN,
        _ => MUTED,
    }
}

/// Grey for "off" brightening linearly to pure white for "max".
pub(super) fn thinking_style(level: &str) -> Style {
    let index = THINKING_LEVELS.iter().position(|l| *l == level).unwrap_or(0) as u32;
    let (darkest, last) = (0x76, THINKING_LEVELS.len() as u32 - 1);
    let gray = darkest + index * (0xff - darkest) / last;
    Style::rgb(gray * 0x010101)
}

/// The rows under the footer, as in Claude Code: a shells pill and an agents pill, each opening its /tasks tab,
/// then the subagent viewed in place of the transcript. `focus` indexes the pills shown, walked to with ↓.
pub(super) fn background_rows(shells: usize, agents: usize, viewed: Option<&str>, focus: Option<usize>, columns: usize) -> Vec<Line> {
    let mut rows = Vec::new();
    let mut counts = Vec::new();
    if shells > 0 {
        counts.push(format!("{shells} {}", if shells == 1 { "shell" } else { "shells" }));
    }
    if agents > 0 {
        counts.push(format!("{agents} {}", if agents == 1 { "agent" } else { "agents" }));
    }
    if !counts.is_empty() {
        let mut row = Vec::new();
        for (index, count) in counts.into_iter().enumerate() {
            if index > 0 {
                row.push(span(" · ", DIM));
            }
            row.push(span(count, if focus == Some(index) { Style::fg(16).on(45) } else { Style::fg(45) }));
        }
        row.push(span(if focus.is_some() { " · Enter to view" } else { " · ↓ to view" }, DIM));
        rows.push(row);
    }
    if let Some(label) = viewed {
        rows.push(truncate(&vec![span("viewing ", MUTED), span(label, BRIGHT.bold()), span(" · Esc back", DIM)], columns));
    }
    rows
}

/// Workflows with a progress line under the footer: running ones, and stopped ones until their agents exit.
pub(super) fn footer_workflows(workflows: &Mutex<Vec<Arc<Workflow>>>) -> Vec<Arc<Workflow>> {
    workflows.lock().unwrap().iter().filter(|run| run.is_running() || run.live_agents() > 0).cloned().collect()
}

/// One progress line per workflow under the agent rows; ↓ walks to them, `focus` indexes `workflows`.
pub(super) fn workflow_rows(workflows: &[Arc<Workflow>], focus: Option<usize>, columns: usize) -> Vec<Line> {
    let mut rows = Vec::new();
    for (index, run) in workflows.iter().enumerate() {
        let focused = focus == Some(index);
        let (glyph, style, hint) = if focused { ("● ", BRIGHT.bold(), " · Enter to open · x to stop") } else { ("○ ", PLAIN, "") };
        rows.push(truncate(&vec![span(glyph, style), span(&run.name, style), span(format!("  {}{hint}", run.summary()), MUTED)], columns));
    }
    rows
}
