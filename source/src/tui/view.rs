use super::*;

impl App {
    /// The whole screen: transcript, then status, open panel or slash menu, editor and footer pinned to the last rows.
    pub(super) fn frame(&mut self, width: usize, height: usize) -> (Vec<Line>, (u16, u16)) {
        if matches!(self.panel, Some(Panel::Diff { .. })) {
            return (self.diff_view(width, height), (0, height.saturating_sub(1) as u16));
        }
        if matches!(self.panel, Some(Panel::Terminal { .. })) {
            return (self.terminal_view(width, height), (0, height.saturating_sub(1) as u16));
        }
        let mut bottom: Vec<Line> = Vec::new();
        let steering: Vec<String> = self.steering.lock().unwrap().iter().map(|(text, _)| text.clone()).collect();
        let queued: Vec<String> = self.queued_commands.iter().map(|(text, _)| text.clone()).collect();
        if !steering.is_empty() || !queued.is_empty() {
            bottom.push(Vec::new());
            for (name, when, messages) in [("Steering", "model reads it after the current tool", steering), ("Queued", "sent when the turn ends", queued)] {
                if messages.is_empty() {
                    continue;
                }
                bottom.push(vec![span(format!("  {name}"), TOOL_NAME.bold()), span(format!(" · {when}"), DIM)]);
                for text in &messages {
                    bottom.extend(prompt_rows(text, MUTED, width));
                }
            }
            bottom.push(vec![span(format!("  {ALT}+Up to edit"), DIM)]);
        }
        if let Some(recording) = &self.recording {
            bottom.push(Vec::new());
            bottom.push(recording_line(recording.started.elapsed()));
        } else if let Some(started) = self.transcribing {
            bottom.push(Vec::new());
            bottom.push(shimmer_line(TRANSCRIBING, (started.elapsed().as_millis() / 80) as usize));
        }
        bottom.extend(todo_lines(&self.todos.lock().unwrap()));
        let shells = self.terminals.lock().unwrap().running();
        let agents = footer_agents(&self.subagents.lock().unwrap());
        let workflows = footer_workflows(&self.workflows);
        let pills = usize::from(shells > 0) + usize::from(agents > 0);
        if pills == 0 && workflows.is_empty() && matches!(self.panel, Some(Panel::Agents { .. })) {
            self.panel = None;
        }
        let mut panel_cursor = None;
        let mut panel_view = (0, Vec::new());
        match &self.panel {
            Some(panel) => {
                let mut lines = self.render_panel(panel, width);
                panel_cursor = take_cursor(&mut lines).map(|(x, y)| (x, bottom.len() + y));
                panel_view = (bottom.len(), lines.clone());
                bottom.extend(lines);
                bottom.push(Vec::new());
            }
            None => bottom.extend(self.menu()),
        }
        // Queued prompts, todos and the menu lead with a blank line; without them the editor needs its own gap.
        if bottom.is_empty() {
            bottom.push(Vec::new());
        }
        if let Some(started) = self.working {
            if bottom.last().is_some_and(|line| !line.is_empty()) {
                bottom.push(Vec::new());
            }
            let tick = (started.elapsed().as_millis() / 80) as usize;
            // Without the margin shimmer_line leads with, so the status lines up with the transcript.
            let mut status = shimmer_line(working_label(self.entries.last().map(|e| &e.item)), tick).split_off(1);
            // shortcut: about 4 chars per token for the request still streaming; tool inputs are not counted until its usage arrives.
            let output = self.prompt_usage.output + self.streamed_chars as u64 / 4;
            self.shown_output = count_toward(self.shown_output, output);
            status.push(span(format!(" {} · ↓ {} tokens", duration(started.elapsed()), tokens(self.shown_output)), DIM));
            bottom.push(status);
            bottom.push(Vec::new());
        }
        if self.editor_width != width {
            // The selection counts wrapped rows, which a new width renumbers; input_line would index past them.
            self.input_selection = Selection::default();
            self.editor_width = width;
        }
        let (mut editor_rows, (cursor_x, cursor_row)) = self.editor.render(width);
        if self.editor.text().is_empty() {
            editor_rows[0].push(span("Ask, /command, @file", DIM));
        }
        let editor_top = bottom.len();
        let shown = editor_rows.len().min(EDITOR_ROWS);
        self.input_top = self.input_top.clamp((cursor_row + 1).saturating_sub(shown), cursor_row).min(editor_rows.len() - shown);
        let below = editor_rows.len() - self.input_top - shown;
        editor_rows.truncate(self.input_top + shown);
        editor_rows.drain(..self.input_top);
        for (row, line) in editor_rows.iter_mut().enumerate() {
            if let Some((from, to)) = self.input_selection.columns(below + shown - 1 - row) {
                *line = select(line, from, to);
            }
        }
        bottom.extend(editor_rows);
        let cursor = panel_cursor.unwrap_or((cursor_x, editor_top + cursor_row - self.input_top));
        bottom.extend(self.footer(width));
        let focus = match self.panel {
            Some(Panel::Agents { row }) => Some(row.min(pills + workflows.len())),
            _ => None,
        };
        let pill_focus = focus.filter(|row| (1..=pills).contains(row)).map(|row| row - 1);
        bottom.extend(background_rows(shells, agents, self.viewed.as_ref().map(|view| view.run.label.as_str()), pill_focus, width));
        let workflow_focus = focus.and_then(|row| row.checked_sub(1 + pills));
        bottom.extend(workflow_rows(&workflows, workflow_focus, width));

        let room = height.saturating_sub(bottom.len());
        // The last two columns are kept for a gap and the scrollbar, so text does not rewrap when it appears.
        let content_width = width.saturating_sub(2);
        let (mut transcript, mut total, mut prompts) = self.transcript_tail(content_width, room + self.scroll);
        // Scroll counts lines up from the bottom, so a scrolled-up view or a drag in progress moves up by what streamed in below it.
        let (last_total, last_width) = self.transcript_size;
        // A new width rewraps every line, so the selected lines no longer hold the selected text.
        if content_width != last_width {
            self.selection = Selection::default();
            self.edge_drag = None;
        }
        if content_width == last_width && total > last_total {
            self.selection.shift(total - last_total);
            if self.scroll > 0 || self.selection.dragging {
                self.scroll += total - last_total;
                (transcript, total, prompts) = self.transcript_tail(content_width, room + self.scroll);
            }
        }
        self.transcript_size = (total, content_width);
        self.scroll = self.scroll.min(transcript.len().saturating_sub(room));
        let end = transcript.len() - self.scroll;
        let start = end.saturating_sub(room);
        let mut rows: Vec<Line> = transcript[start..end].to_vec();
        let visible = rows.len();
        self.transcript_view = (visible, content_width);
        // Sticky header: the prompt of the turn at the top of the view, once its first line scrolls out above.
        if let Some(&(_, index)) = prompts.iter().find(|(top, _)| *top >= self.scroll + visible) {
            // shortcut: clicks and selection on the pinned rows hit the transcript lines under them.
            let prompt = self.entries[index].lines(content_width, self.expanded, &self.cwd);
            pin_prompt(&mut rows, prompt, content_width);
        }
        for (row, line) in rows.iter_mut().enumerate() {
            if let Some((from, to)) = self.selection.columns(self.scroll + visible - 1 - row) {
                *line = select(line, from, to);
            }
        }
        // Floats over the transcript top so a hint appearing or expiring does not shift the layout.
        if let (Some((hint, _)), Some(first)) = (&self.hint, rows.first_mut()) {
            *first = overlay_right(first, vec![span(format!(" {hint} "), Style { selected: true, ..PLAIN })], content_width);
        }
        if let (true, Some(last)) = (self.scroll > 0, rows.last_mut()) {
            *last = overlay_center(last, vec![span(jump_pill(), BRIGHT.on(240))], content_width);
        }
        for (row, glyph) in scrollbar(total, visible, self.scroll).into_iter().enumerate() {
            let pad = content_width.saturating_sub(line_width(&rows[row]));
            rows[row].push(span(format!("{} {glyph}", " ".repeat(pad)), MUTED));
        }
        rows.resize(room, Vec::new());
        self.input_view = (rows.len() + editor_top, shown, below);
        self.panel_view = (rows.len() + panel_view.0, panel_view.1);
        let cursor_y = rows.len() + cursor.1;
        rows.extend(bottom);
        rows.truncate(height);
        let rows = rows.iter().map(|row| truncate(row, width)).collect();
        (rows, (cursor.0 as u16, cursor_y.min(height.saturating_sub(1)) as u16))
    }

    pub(super) fn diff_view(&mut self, width: usize, height: usize) -> Vec<Line> {
        let Some(Panel::Diff { files, index, scroll }) = self.panel.as_mut() else { unreachable!("frame checks the panel") };
        let lines: Vec<Line> = diff_lines(&files[*index]).iter().map(|line| truncate(line, width)).collect();
        let header = screen_header(vec![span(&files[*index].path, BRIGHT.bold())], *index, files.len());
        full_screen(header, &lines, scroll, height, "↑↓ scroll     ←→ file     esc back")
    }

    pub(super) fn diff_key(&mut self, code: KeyCode) {
        let Some(Panel::Diff { files, index, scroll }) = self.panel.as_mut() else { unreachable!("panel_key checks the panel") };
        match code {
            KeyCode::Up => *scroll = scroll.saturating_add(1),
            KeyCode::Down => *scroll = scroll.saturating_sub(1),
            KeyCode::PageUp => *scroll = scroll.saturating_add(10),
            KeyCode::PageDown => *scroll = scroll.saturating_sub(10),
            KeyCode::Left => (*index, *scroll) = (index.saturating_sub(1), DIFF_TOP),
            KeyCode::Right => (*index, *scroll) = ((*index + 1).min(files.len() - 1), DIFF_TOP),
            KeyCode::Esc => {
                let (files, index) = (std::mem::take(files), *index);
                self.panel = Some(Panel::Changes { files, index });
            }
            _ => {}
        }
    }

    /// Shows `run`'s session in place of the transcript, or the main transcript again for None.
    pub(super) fn view_run(&mut self, run: Option<Arc<Run>>) {
        self.viewed = run.map(|run| RunView { run, loaded: None, built: None, lines: Vec::new() });
        self.panel = None;
        self.scroll = 0;
        self.selection = Selection::default();
    }

    /// The open terminal's output filling the screen, reread when its logs grow.
    pub(super) fn terminal_view(&mut self, width: usize, height: usize) -> Vec<Line> {
        let Some(Panel::Terminal { index, scroll, loaded, lines }) = self.panel.as_mut() else { unreachable!("frame checks the panel") };
        let terminal = self.terminals.lock().unwrap().list[*index].clone();
        let size = terminal.log_size();
        if *loaded != Some((size, width)) {
            *loaded = Some((size, width));
            *lines = log_lines(&terminal.log(), width);
        }
        let status = terminal.status();
        let frames = crate::spinner::FRAMES;
        let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
        let mark = match status {
            TerminalStatus::Running => span(frames[(now.as_millis() / ANIMATION_TICK.as_millis()) as usize % frames.len()], MUTED),
            TerminalStatus::Done => span("✓", OK),
            TerminalStatus::Failed => span("✗", ERROR),
            TerminalStatus::Killed => span("✗", WARN),
        };
        let about = format!(" {} · {} {} · {}", terminal.id, status.as_str(), terminal.elapsed(), short_path(&terminal.cwd.display().to_string()));
        let label = vec![mark, span(" ", PLAIN), span(&terminal.title, BRIGHT.bold()), span(about, DIM)];
        let count = self.terminals.lock().unwrap().list.len();
        let header = screen_header(label, *index, count);
        full_screen(header, lines, scroll, height, "↑↓ scroll     ←→ shell     x stop     esc back")
    }

    pub(super) fn terminal_key(&mut self, code: KeyCode) {
        let terminals = self.terminals.lock().unwrap();
        let last = terminals.list.len() - 1;
        let Some(Panel::Terminal { index, scroll, loaded, .. }) = self.panel.as_mut() else { unreachable!("panel_key checks the panel") };
        match code {
            KeyCode::Up => *scroll += 1,
            KeyCode::Down => *scroll = scroll.saturating_sub(1),
            KeyCode::PageUp => *scroll += 10,
            KeyCode::PageDown => *scroll = scroll.saturating_sub(10),
            KeyCode::Char('x') => _ = terminals.list[*index].stop(),
            KeyCode::Left | KeyCode::Right => {
                let next = if code == KeyCode::Left { index.saturating_sub(1) } else { (*index + 1).min(last) };
                if next != *index {
                    (*index, *scroll, *loaded) = (next, 0, None);
                }
            }
            KeyCode::Esc => {
                let index = *index;
                drop(terminals);
                self.panel = Some(Panel::Tasks { agents: false, index });
            }
            _ => {}
        }
    }

    /// The last `needed` transcript lines (or all of them if there are fewer), the transcript's total line count,
    /// and every user prompt as (its top line counted up from the last line, its entry index), nearest the bottom first.
    /// While a subagent is viewed, its session stands in for the transcript, so scrolling, selection and copy follow it.
    pub(super) fn transcript_tail(&mut self, width: usize, needed: usize) -> (Vec<Line>, usize, Vec<(usize, usize)>) {
        if let Some(view) = &mut self.viewed {
            let lines = view.lines(width, &self.cwd);
            return (lines[lines.len().saturating_sub(needed)..].to_vec(), lines.len(), Vec::new());
        }
        let expanded = self.expanded;
        let spinner = self.working.map(|started| crate::spinner::FRAMES[(started.elapsed().as_millis() / ANIMATION_TICK.as_millis()) as usize % crate::spinner::FRAMES.len()]);
        let folded = if expanded { HashMap::new() } else { folded_tools(&mut self.entries, width, &self.cwd, spinner) };
        let mut chunks: Vec<Vec<Line>> = Vec::new();
        let mut count = 0;
        let mut prompts = Vec::new();
        // Nearest rendered entry below; empty ones (collapsed thinking) must not split tool or log runs.
        let mut below = None;
        for index in (0..self.entries.len()).rev() {
            let gap = below.is_some_and(|below: usize| blank_between(&self.entries[index].item, &self.entries[below].item));
            let is_prompt = matches!(self.entries[index].item, Item::User(_));
            let running = match &self.entries[index].item {
                Item::Tool { result: None, started, .. } if !folded.contains_key(&index) => Some(*started),
                _ => None,
            };
            let lines = match folded.get(&index) {
                Some(lines) => lines,
                None => self.entries[index].lines(width, expanded, &self.cwd),
            };
            if lines.is_empty() {
                continue;
            }
            if gap {
                if count < needed {
                    chunks.push(vec![Vec::new()]);
                }
                count += 1;
            }
            if count < needed {
                let mut lines = lines.clone();
                // Rows are cached, so the running tool's icon turns here, on every frame.
                if let (Some(started), Some(frame)) = (running, spinner) {
                    lines[0][0] = span(frame, MUTED);
                    lines[0].extend(running_seconds(started));
                }
                chunks.push(lines);
            }
            count += lines.len();
            if is_prompt {
                prompts.push((count - 1, index));
            }
            below = Some(index);
        }
        (chunks.into_iter().rev().flatten().collect(), count, prompts)
    }

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

/// "+3 -1" for the /changes rows.
pub(super) fn diff_stat(diff: &str) -> Line {
    let body = diff.lines().filter(|line| !line.starts_with("+++") && !line.starts_with("---"));
    let (mut added, mut removed) = (0, 0);
    for line in body {
        match line.chars().next() {
            Some('+') => added += 1,
            Some('-') => removed += 1,
            _ => {}
        }
    }
    vec![span(format!("+{added}"), OK), span(" ", PLAIN), span(format!("-{removed}"), ERROR)]
}

/// `full_screen` counts scroll up from the bottom and clamps it, so this opens a diff at its first line.
pub(super) const DIFF_TOP: usize = usize::MAX;

/// A file's /changes diff without git's header lines, added lines green and removed red.
pub(super) fn diff_lines(file: &FileChange) -> Vec<Line> {
    let mut lines = Vec::new();
    let hunks = file.diff.lines().skip_while(|line| !line.starts_with("@@"));
    for line in hunks {
        let style = match line.chars().next() {
            Some('+') => OK,
            Some('-') => ERROR,
            Some('@') => MUTED,
            _ => PLAIN,
        };
        lines.push(vec![span(format!("  {}", line.replace('\t', "    ")), style)]);
    }
    lines
}

pub(super) const TODO_ROWS: usize = 5;
pub(super) const PINNED_PROMPT_ROWS: usize = 3;

/// Draws a prompt over the top transcript rows, cut to three rows with "…".
pub(super) fn pin_prompt(rows: &mut [Line], prompt: &[Line], width: usize) {
    let pinned = PINNED_PROMPT_ROWS.min(rows.len()).min(prompt.len());
    for index in 0..pinned {
        let mut row = prompt[index].clone();
        if index + 1 == pinned && prompt.len() > pinned {
            row.pop();
            row.push(span("…", PLAIN));
            row = band(truncate(&row, width), width);
        }
        rows[index] = row;
    }
}

/// `row` padded to `width` and set on the prompt background; its last span is the padding.
pub(super) fn band(mut row: Line, width: usize) -> Line {
    row.push(span(" ".repeat(width.saturating_sub(line_width(&row))), PLAIN));
    for span in &mut row {
        span.style.bg = Some(PROMPT_BG);
    }
    row
}

/// The widget above the editor: up to five tasks from the first open one, gone once nothing is open.
pub(super) fn todo_lines(todos: &Todos) -> Vec<Line> {
    if !todos.has_open() {
        return Vec::new();
    }
    let tasks: Vec<_> = todos.visible().skip_while(|t| t.status == Status::Completed).collect();
    let mut lines = vec![Vec::new()];
    for task in tasks.iter().take(TODO_ROWS) {
        let text = match (task.status, &task.active_form) {
            (Status::InProgress, Some(form)) => form,
            _ => &task.subject,
        };
        lines.push(todo_spans(task.status, text));
    }
    if tasks.len() > TODO_ROWS {
        lines.push(vec![span(format!("  +{} more, /todos", tasks.len() - TODO_ROWS), DIM)]);
    }
    lines
}

/// pj's position-only scrollbar for `visible` rows of a `total`-line transcript scrolled `scroll` lines up from the end.
/// Empty when everything fits.
pub(super) fn scrollbar(total: usize, visible: usize, scroll: usize) -> Vec<&'static str> {
    if visible == 0 || total <= visible {
        return Vec::new();
    }
    let thumb = (visible * visible).div_ceil(total).clamp(1, visible);
    let max_scroll = total - visible;
    let top = (max_scroll - scroll.min(max_scroll)) * (visible - thumb) / max_scroll;
    (0..visible)
        .map(|row| match row {
            _ if row < top || row >= top + thumb => " ",
            _ if thumb == 1 => "•",
            _ if row == top => "╷",
            _ if row + 1 == top + thumb => "╵",
            _ => "│",
        })
        .collect()
}

/// A project server's entry wrapped to `width` and cut to 6 rows; true when cut.
pub(super) fn mcp_entry(entry: &str, width: usize) -> (Vec<Line>, bool) {
    const ROWS: usize = 6;
    let mut lines = wrap(&[span(entry, WARN)], width, &vec![span("  ", PLAIN)]);
    let cut = lines.len() > ROWS;
    if cut {
        lines.truncate(ROWS - 1);
        lines.push(vec![span("  … cut here, read the rest in .mcp.json or .agents/mcp.json", MUTED)]);
    }
    (lines, cut)
}

/// Pads between `left` and `right` so `right` ends at the last column.
pub(super) fn spread(left: Line, right: Line, width: usize) -> Line {
    let gap = width.saturating_sub(line_width(&left) + line_width(&right)).max(1);
    [left, vec![span(" ".repeat(gap), PLAIN)], right].concat()
}

pub(super) fn jump_pill() -> String {
    format!(" ↓ Jump to latest message · {ALT}+Down ")
}

/// `~/Developer/adescom/web` becomes `~/Dev/ade/web`: every folder but the last cut to 3 characters.
pub(super) fn short_path(path: &str) -> String {
    let mut parts: Vec<String> = path.split('/').map(String::from).collect();
    let last = parts.len() - 1;
    for part in &mut parts[..last] {
        *part = part.chars().take(3).collect();
    }
    parts.join("/")
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

pub(super) fn context_lines(split: agent::context::PerPart, window: u64, session: Usage, last: Usage) -> Vec<Line> {
    let used: u64 = split.iter().sum();
    let percent = |part: u64, of: u64| format!("{}%", (part * 100).checked_div(of).unwrap_or(0));
    let mut lines = vec![vec![span("Context ", TOOL_NAME), span(format!("{} of {} ({})", tokens(used), tokens(window), percent(used, window)), PLAIN)]];
    for (name, part) in agent::context::PARTS.iter().zip(split) {
        lines.push(vec![span(format!("  {name:<16}"), MUTED), span(format!("{:>7} {:>5}", tokens(part), percent(part, used)), PLAIN)]);
    }
    lines.push(vec![span(format!("  {:<16}{:>7} {:>7} {:>5}", "Cache", "read", "write", "hit"), MUTED)]);
    for (name, usage) in [("Session", session), ("Last request", last)] {
        let hit = percent(usage.cache_read, usage.input + usage.cache_read + usage.cache_write);
        lines.push(vec![span(format!("  {name:<16}"), MUTED), span(format!("{:>7} {:>7} {:>5}", tokens(usage.cache_read), tokens(usage.cache_write), hit), PLAIN)]);
    }
    lines
}

pub(super) fn hotkeys_lines() -> Vec<Line> {
    let key_width = HOTKEYS.iter().flat_map(|(_, keys)| keys.iter()).map(|(key, _)| key.len()).max().unwrap_or(0);
    let mut lines = vec![vec![span("ℹ", SYSTEM), span(" ", PLAIN), span("hotkeys", SYSTEM)]];
    for (group, keys) in HOTKEYS {
        lines.push(vec![span(format!("  {group}"), TOOL_NAME.bold())]);
        for (key, action) in keys {
            lines.push(vec![span(format!("    {key:key_width$}   "), TOOL_NAME), span(*action, TOOL_ARGS)]);
        }
    }
    lines
}

pub(super) fn banner() -> Vec<Line> {
    vec![vec![span("░▒▓", DIM), span("█▶ agent", BRIGHT.bold()), span(format!(" v{} · Run /help for commands", env!("CARGO_PKG_VERSION")), MUTED)]]
}

/// A full-screen view's top row: `label`, then "2/3" when there is more than one.
pub(super) fn screen_header(mut label: Line, index: usize, count: usize) -> Line {
    if count > 1 {
        label.push(span(format!(" · {}/{count}", index + 1), DIM));
    }
    label
}

/// A full-screen read-only view: `header`, the last screenful of `lines` scrolled up by `scroll`, a blank row and `hint`, styled like a panel's.
/// Lines shorter than the screen start under the header, like a pager.
pub(super) fn full_screen(header: Line, lines: &[Line], scroll: &mut usize, height: usize, hint: &str) -> Vec<Line> {
    let room = height.saturating_sub(3);
    *scroll = (*scroll).min(lines.len().saturating_sub(room));
    let end = lines.len() - *scroll;
    let start = end.saturating_sub(room);
    let mut rows = vec![header];
    rows.extend_from_slice(&lines[start..end]);
    rows.resize(room + 2, Vec::new());
    rows.push(vec![span(hint, MUTED)]);
    rows
}

/// A terminal log as plain wrapped rows: colors dropped, a line redrawn with `\r` (a progress bar) keeps its last state.
pub(super) fn log_lines(text: &str, width: usize) -> Vec<Line> {
    let indent = vec![span(" ", PLAIN)];
    let mut lines = Vec::new();
    for line in strip_ansi(text).lines() {
        let last = line.rfind('\r').map_or(line, |at| &line[at + 1..]);
        lines.extend(wrap(&[span(last.replace('\t', "    "), PLAIN)], width, &indent));
    }
    lines
}

/// Running subagents counted in the footer pill; workflow agents show on their workflow's line instead.
pub(super) fn footer_agents(runs: &Runs) -> usize {
    runs.list.iter().filter(|run| run.is_running() && run.workflow.is_none()).count()
}

/// The subagent shown in place of the transcript. `loaded` is the (file size, width) `lines` were built for, `built` when.
pub(super) struct RunView {
    pub(super) run: Arc<Run>,
    pub(super) loaded: Option<(u64, usize)>,
    pub(super) built: Option<Instant>,
    pub(super) lines: Vec<Line>,
}

impl RunView {
    /// The run's session as transcript lines, reread when its file grows, at most every `RUN_VIEW_REFRESH` while the run works.
    pub(super) fn lines(&mut self, width: usize, cwd: &Path) -> &[Line] {
        let session = self.run.state.lock().unwrap().session.clone().filter(|_| self.run.environment().is_none());
        let size = session.as_ref().and_then(|path| std::fs::metadata(path).ok()).map_or(0, |meta| meta.len());
        let held = self.run.is_running() && self.loaded.is_some_and(|(_, loaded_width)| loaded_width == width) && self.built.is_some_and(|at| at.elapsed() < RUN_VIEW_REFRESH);
        if self.loaded != Some((size, width)) && !held {
            self.loaded = Some((size, width));
            self.built = Some(Instant::now());
            self.lines = match session {
                None => system_lines("·", "task", &self.run.task, DIM, width),
                Some(path) => subagent_lines(Path::new(&path), width, cwd).unwrap_or_else(|error| system_lines("✗", "error", &error, ERROR, width)),
            };
        }
        &self.lines
    }
}

/// A subagent's session rendered like the main transcript: a blank line between items, except inside a tool or log run.
pub(super) fn subagent_lines(path: &Path, width: usize, cwd: &Path) -> Result<Vec<Line>, String> {
    let saved = Session::history(path)?;
    let mut entries: Vec<Entry> = history_items(&saved.history.messages).into_iter().map(|item| Entry { item, cache: None }).collect();
    let mut folded = folded_tools(&mut entries, width, cwd, None);
    let mut lines = Vec::new();
    let mut previous_run = None;
    for (index, entry) in entries.iter_mut().enumerate() {
        let item_lines = match folded.remove(&index) {
            Some(lines) => lines,
            None => entry.lines(width, false, cwd).clone(),
        };
        if item_lines.is_empty() {
            continue;
        }
        let run = transcript_run(&entry.item);
        if run.is_none() || run != previous_run {
            lines.push(Vec::new());
        }
        lines.extend(item_lines);
        previous_run = run;
    }
    Ok(lines)
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

/// The id until the model lists load, then the catalog name with its version: "opus 5.5".
pub(super) fn model_name(catalog: &[Model], id: &str) -> String {
    catalog.iter().find(|model| model.id == id).map_or(id.to_string(), models::short_name)
}

/// A phase row in the workflow progress view: "3 agents · 2 done · 1 running".
pub(super) fn step_counts(steps: &[workflow::Step]) -> String {
    let mut parts = vec![format!("{} {}", steps.len(), if steps.len() == 1 { "agent" } else { "agents" })];
    for status in [StepStatus::Running, StepStatus::Queued, StepStatus::Done, StepStatus::Cached, StepStatus::Failed] {
        let count = steps.iter().filter(|step| step.status == status).count();
        if count > 0 {
            parts.push(format!("{count} {}", status.word()));
        }
    }
    parts.join(" · ")
}

/// An agent row in a workflow phase: its label, then status, attempt and work done.
pub(super) fn step_row(step: &workflow::Step) -> (String, String) {
    let mut right = step.status.word().to_string();
    if step.attempt > 1 {
        right.push_str(&format!(" · attempt {}", step.attempt));
    }
    if let Some(run) = &step.run {
        let elapsed = duration(run.elapsed());
        let state = run.state.lock().unwrap();
        right.push_str(&format!(" · {elapsed} · {} tools", state.tool_calls));
        if state.usage.output > 0 {
            right.push_str(&format!(" · ↓ {} tokens", usage::tokens(state.usage.output)));
        }
    }
    (step.label.clone(), right)
}

/// /tasks, as in Claude Code, with shells and local agents on separate tabs, each row with its status.
pub(super) fn tasks_panel(terminals: &[Arc<Terminal>], runs: &[Arc<Run>], catalog: &[Model], agents: bool, selected: usize, columns: usize) -> Vec<Line> {
    let active_shells = terminals.iter().filter(|t| t.status() == TerminalStatus::Running).count();
    let active_agents = runs.iter().filter(|run| run.is_running()).count();
    let mut counts = Vec::new();
    if active_shells > 0 {
        counts.push(format!("{active_shells} active {}", if active_shells == 1 { "shell" } else { "shells" }));
    }
    if active_agents > 0 {
        counts.push(format!("{active_agents} active {}", if active_agents == 1 { "agent" } else { "agents" }));
    }
    let summary = if counts.is_empty() { "nothing running".to_string() } else { counts.join(" · ") };
    let tab = |label: String, active: bool| if active { span(format!("[{label}]"), BRIGHT.bold()) } else { span(label, MUTED) };
    let header = vec![span("Background", SYSTEM.bold()), span("  ", PLAIN), tab(format!("Shells {}", terminals.len()), !agents), span("  ", PLAIN), tab(format!("Agents {}", runs.len()), agents)];
    let names: Vec<&str> = if agents { runs.iter().map(|run| run.label.as_str()).collect() } else { terminals.iter().map(|t| t.title.as_str()).collect() };
    let name_width = names.iter().map(|name| width(name)).max().unwrap_or(0).min(TASK_NAME_WIDTH);
    let name = |text: &str, index: usize, running: bool| {
        let style = if index == selected {
            SYSTEM
        } else if running {
            PLAIN
        } else {
            MUTED
        };
        let cell = truncate(&vec![span(text, style)], name_width);
        let gap = name_width - line_width(&cell) + 3;
        [cell, vec![span(" ".repeat(gap), PLAIN)]].concat()
    };
    let mut rows: Vec<Line> = Vec::new();
    if !agents {
        for (index, terminal) in terminals.iter().enumerate() {
            let status = terminal.status();
            let dot = match status {
                TerminalStatus::Running | TerminalStatus::Killed => MUTED,
                TerminalStatus::Done => OK,
                TerminalStatus::Failed => ERROR,
            };
            let mut row = vec![span("● ", dot)];
            row.extend(name(&terminal.title, index, status == TerminalStatus::Running));
            row.push(span(status.as_str(), DIM));
            rows.push(row);
        }
    } else {
        for (index, run) in runs.iter().enumerate() {
            let state = run.state.lock().unwrap();
            let (status, dot) = match state.result {
                None if state.last_tool.is_empty() => ("running".to_string(), MUTED),
                None => (format!("running {}", state.last_tool), MUTED),
                Some(Ok(_)) => ("done".to_string(), OK),
                Some(Err(_)) => ("failed".to_string(), ERROR),
            };
            let mut row = vec![span("● ", dot)];
            row.extend(name(&run.label, index, state.result.is_none()));
            row.push(span(format!("{status} · {} tools · {} {}", state.tool_calls, model_name(catalog, &run.model), run.thinking), DIM));
            rows.push(row);
        }
    }
    let mut lines = vec![Vec::new(), header, vec![span(summary, MUTED)], Vec::new()];
    if rows.is_empty() {
        lines.push(vec![span(if agents { "  No agents yet." } else { "  No shells yet." }, MUTED)]);
    }
    let shown = PANEL_ROWS + 3;
    let first = (selected + 1).saturating_sub(shown);
    for (index, row) in rows.iter().enumerate().skip(first).take(shown) {
        let marker = if index == selected { span("❯ ", SYSTEM) } else { span("  ", PLAIN) };
        let mut line = vec![marker];
        line.extend(row.iter().cloned());
        lines.push(truncate(&line, columns));
    }
    if rows.len() > shown {
        lines.extend([Vec::new(), vec![span(format!("  {}/{}", selected + 1, rows.len()), DIM)]]);
    }
    lines.extend([Vec::new(), vec![span("↑/↓ to select · Tab to switch · Enter to view · x to stop · Esc to close", MUTED.italic())]]);
    lines
}

/// One frame of the token counter: a quarter of the gap, at least 1, so a jump plays as a quick count.
pub(super) fn count_toward(shown: u64, target: u64) -> u64 {
    if shown >= target {
        return target;
    }
    shown + ((target - shown) / 4).max(1)
}

pub(super) fn duration(d: Duration) -> String {
    let secs = d.as_secs();
    match (secs / 3600, secs / 60 % 60, secs % 60) {
        (0, 0, s) => format!("{s}s"),
        (0, m, s) => format!("{m}m {s}s"),
        (h, m, s) => format!("{h}h {m}m {s}s"),
    }
}

/// fx's "Permission needed" prompt.
pub(super) fn permission_lines(request: &Request, selected: usize, width: usize) -> Vec<Line> {
    let title = vec![span("  ", PLAIN), span("Permission needed · Choose one", PLAIN.bold())];
    let always = request.kind.always();
    let mut lines = vec![Vec::new(), spread(title, vec![span(request.kind.label(), MUTED)], width), Vec::new(), vec![span(format!("  {}", request.question), PLAIN.bold())], Vec::new()];
    let prefix = if request.kind == Kind::Command { "$ " } else { "" };
    for (index, line) in request.detail.lines().enumerate() {
        let lead = if index == 0 { prefix } else { "  " };
        lines.extend(wrap(&[span(format!("{lead}{line}"), BRIGHT.bold())], width, &vec![span("  ", PLAIN)]));
    }
    lines.push(Vec::new());
    for (index, option) in ["Yes", always, "No"].iter().enumerate() {
        let label = format!("{}. {option}", index + 1);
        lines.push(if index == selected { vec![span("  ", PLAIN), span("→ ", OK), span(label, BRIGHT.bold())] } else { vec![span(format!("    {label}"), PLAIN)] });
    }
    lines.push(vec![span("↑↓ move     enter choose     1-3 pick     esc deny", MUTED)]);
    lines
}

/// `<TITLE>_PLAN.md` from the plan's first `# ` heading: letters and digits upper-cased, runs of anything else become one `_`.
pub(super) fn plan_file_name(plan: &str) -> String {
    let title = plan.lines().find_map(|line| line.strip_prefix("# ")).unwrap_or_default();
    let mut name = String::new();
    for word in title.split(|c: char| !c.is_alphanumeric()).filter(|word| !word.is_empty()) {
        name.push_str(&word.to_uppercase());
        name.push('_');
    }
    format!("{name}PLAN.md")
}

pub(super) fn plan_lines(selected: usize) -> Vec<Line> {
    let mut lines = vec![Vec::new(), vec![span("  ", PLAIN), span("Plan ready · Choose one", PLAIN.bold())], Vec::new()];
    for (index, option) in PLAN_OPTIONS.iter().enumerate() {
        let label = format!("{}. {option}", index + 1);
        lines.push(if index == selected { vec![span("  ", PLAIN), span("→ ", OK), span(label, BRIGHT.bold())] } else { vec![span(format!("    {label}"), PLAIN)] });
    }
    lines.push(vec![span("↑↓ move     enter choose     1-3 pick     esc keep planning", MUTED)]);
    lines
}

/// fx's session age: 45s, 3m, 5h, 2d.
pub(super) fn age(time: SystemTime) -> String {
    let secs = time.elapsed().map_or(0, |d| d.as_secs());
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

/// Thinking only while reasoning streams, before the answer text or a tool call starts.
pub(super) fn working_label(last: Option<&Item>) -> &'static str {
    match last {
        Some(Item::Assistant { text, thinking }) if text.is_empty() && !thinking.is_empty() => THINKING,
        _ => WORKING,
    }
}

pub(super) fn shimmer_line(label: &str, tick: usize) -> Line {
    let head = (tick % (label.len() + 10)) as isize - 3;
    let level = |column: usize| match (column as isize - head).unsigned_abs() {
        distance @ 0..=2 => 255 - 4 * distance as u8,
        _ => 245,
    };
    let mut line = vec![span(" ", PLAIN), star(head.unsigned_abs()), span(" ", PLAIN)];
    for (index, letter) in label.chars().enumerate() {
        line.push(span(letter.to_string(), Style::fg(level(index + 2))));
    }
    line
}

/// Lit ✦ while a shimmer band `distance` columns away passes over it, dim ✧ otherwise.
pub(super) fn star(distance: usize) -> Span {
    match distance {
        0..=2 => span("✦", Style::fg(255 - 4 * distance as u8)),
        _ => span("✧", DIM),
    }
}

pub(super) fn recording_line(elapsed: Duration) -> Line {
    let seconds = elapsed.as_secs();
    vec![span(" ●", ERROR), span(format!(" {}:{:02}", seconds / 60, seconds % 60), BRIGHT), span("  Enter to transcribe · Esc to cancel", DIM)]
}
