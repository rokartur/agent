use super::*;

/// Where the last frame put each part of the screen, so mouse and keys hit what is drawn.
#[derive(Default)]
pub(super) struct Layout {
    pub width: usize,
    /// Transcript wrap width: `width` less the gap and scrollbar columns.
    pub content_width: usize,
    /// Transcript rows on screen.
    pub transcript_rows: usize,
    /// Transcript line count at `content_width`.
    pub transcript_total: usize,
    /// Screen row of the first shown editor row.
    pub input_top: usize,
    pub input_rows: usize,
    /// Editor rows hidden below the shown ones.
    pub input_below: usize,
    /// First editor row shown when the editor is taller than `EDITOR_ROWS`.
    pub editor_first: usize,
    /// Screen row of the open panel's first row.
    pub panel_top: usize,
    pub panel_lines: Vec<Line>,
}

impl Layout {
    pub fn on_input(&self, row: usize) -> bool {
        (self.input_top..self.input_top + self.input_rows).contains(&row)
    }

    /// Transcript point under screen `row`, its line counted up from the last one.
    pub fn transcript_point(&self, scroll: usize, row: usize, column: u16) -> Point {
        Point { line: scroll + self.transcript_rows - 1 - row, column: column as usize }
    }

    /// Editor point under screen `row`, counted up from the last editor row; a row outside the editor clamps to its edge row.
    pub fn input_point(&self, row: usize, column: u16) -> Point {
        let row = row.clamp(self.input_top, self.input_top + self.input_rows - 1) - self.input_top;
        Point { line: self.input_below + self.input_rows - 1 - row, column: column as usize }
    }

    /// The pill sits centered on the last transcript row while the view is scrolled up.
    pub fn on_jump_pill(&self, scroll: usize, row: usize, column: usize) -> bool {
        let pill = line_width(&vec![span(jump_pill(), PLAIN)]);
        let start = self.content_width.saturating_sub(pill) / 2;
        scroll > 0 && self.transcript_rows > 0 && row == self.transcript_rows - 1 && (start..start + pill).contains(&column)
    }

    pub fn panel_line(&self, row: usize) -> Option<&Line> {
        row.checked_sub(self.panel_top).and_then(|at| self.panel_lines.get(at))
    }
}

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
            bottom.push(shimmer_line(TRANSCRIBING, (started.elapsed().as_millis() / 80) as usize, &self.settings.working_animation));
        }
        let todos = todo_lines(&self.todos.lock().unwrap());
        let has_todos = !todos.is_empty();
        bottom.extend(todos);
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
            None => {
                let menu = self.menu();
                // Todos keep one blank line above the editor; the menu brings its own.
                if has_todos && menu.is_empty() {
                    bottom.push(Vec::new());
                }
                bottom.extend(menu);
            }
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
            let mut status = shimmer_line(working_label(self.entries.last().map(|e| &e.item)), tick, &self.settings.working_animation).split_off(1);
            // shortcut: about 4 chars per token for the request still streaming; tool inputs are not counted until its usage arrives.
            let output = self.prompt_usage.output + self.streamed_chars as u64 / 4;
            self.shown_output = count_toward(self.shown_output, output);
            status.push(span(format!(" {} · ↓ {} tokens", duration(started.elapsed()), tokens(self.shown_output)), DIM));
            bottom.push(status);
            bottom.push(Vec::new());
        }
        if self.layout.width != width {
            // The selection counts wrapped rows, which a new width renumbers; input_line would index past them.
            self.input_selection = Selection::default();
        }
        let (mut editor_rows, (cursor_x, cursor_row)) = self.editor.render(width);
        if self.editor.text().is_empty() {
            editor_rows[0].push(span("Ask, /command, @file", DIM));
        }
        let editor_top = bottom.len();
        let shown = editor_rows.len().min(EDITOR_ROWS);
        let editor_first = self.layout.editor_first.clamp((cursor_row + 1).saturating_sub(shown), cursor_row).min(editor_rows.len() - shown);
        let below = editor_rows.len() - editor_first - shown;
        editor_rows.truncate(editor_first + shown);
        editor_rows.drain(..editor_first);
        for (row, line) in editor_rows.iter_mut().enumerate() {
            if let Some((from, to)) = self.input_selection.columns(below + shown - 1 - row) {
                *line = select(line, from, to);
            }
        }
        bottom.extend(editor_rows);
        let cursor = panel_cursor.unwrap_or((cursor_x, editor_top + cursor_row - editor_first));
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
        let (last_total, last_width) = (self.layout.transcript_total, self.layout.content_width);
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
        self.scroll = self.scroll.min(transcript.len().saturating_sub(room));
        let end = transcript.len() - self.scroll;
        let start = end.saturating_sub(room);
        let mut rows: Vec<Line> = transcript[start..end].to_vec();
        let visible = rows.len();
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
        let (panel_top, panel_lines) = panel_view;
        self.layout = Layout {
            width,
            content_width,
            transcript_rows: visible,
            transcript_total: total,
            input_top: rows.len() + editor_top,
            input_rows: shown,
            input_below: below,
            editor_first,
            panel_top: rows.len() + panel_top,
            panel_lines,
        };
        let cursor_y = rows.len() + cursor.1;
        rows.extend(bottom);
        rows.truncate(height);
        let rows = rows.iter().map(|row| truncate(row, width)).collect();
        (rows, (cursor.0 as u16, cursor_y.min(height.saturating_sub(1)) as u16))
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
        let mut chunks: Vec<Vec<Line>> = Vec::new();
        let mut count = 0;
        let mut prompts = Vec::new();
        // Nearest rendered entry below; empty ones (collapsed thinking) must not split tool or log runs.
        let mut below = None;
        for index in (0..self.entries.len()).rev() {
            let gap = below.is_some_and(|below: usize| blank_between(&self.entries[index].item, &self.entries[below].item));
            let is_prompt = matches!(self.entries[index].item, Item::User(_));
            let running = match &self.entries[index].item {
                Item::Tool { result: None, started, .. } => Some(*started),
                _ => None,
            };
            let last = index == self.entries.len() - 1;
            let done_for = matches!(self.entries[index].item, Item::Done { .. }).then(|| self.turn_ended.filter(|_| last).map_or(DONE_FLASH, |ended| ended.elapsed()));
            let lines = self.entries[index].lines(width, expanded, &self.cwd);
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
                // Drawn here so a changed working_animation setting reaches rows already cached.
                if let Some(since_end) = done_for {
                    lines[0][0] = done_mark(&self.settings.working_animation, since_end);
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

pub(super) fn context_lines(split: agent::context::PerPart, instructions: &[(String, u64)], window: u64, session: Usage, last: Usage) -> Vec<Line> {
    let used: u64 = split.iter().sum();
    let percent = |part: u64, of: u64| format!("{}%", (part * 100).checked_div(of).unwrap_or(0));
    let mut lines = vec![vec![span("Context ", TOOL_NAME), span(format!("{} of {} ({})", tokens(used), tokens(window), percent(used, window)), PLAIN)]];
    for (name, part) in agent::context::PARTS.iter().zip(split) {
        lines.push(vec![span(format!("  {name:<16}"), MUTED), span(format!("{:>7} {:>5}", tokens(part), percent(part, used)), PLAIN)]);
    }
    let sources = instructions.iter().map(|(source, part)| vec![span(format!("    {:>7}  ", tokens(*part)), MUTED), span(source.clone(), TOOL_ARGS)]);
    // Under the "System prompt" row, the first after the header.
    lines.splice(2..2, sources);
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
    let tab = |label: String, active: bool| if active { span(format!(" {label} "), ACTIVE_TAB) } else { span(label, MUTED) };
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
            let mut row = vec![status_mark(status.as_str())];
            row.extend(name(&terminal.title, index, status == TerminalStatus::Running));
            row.push(span(status.as_str(), DIM));
            rows.push(row);
        }
    } else {
        for (index, run) in runs.iter().enumerate() {
            let state = run.state.lock().unwrap();
            let (status, mark) = match state.result {
                None if state.last_tool.is_empty() => ("running".to_string(), status_mark("running")),
                None => (format!("running {}", state.last_tool), status_mark("running")),
                Some(Ok(_)) => ("done".to_string(), status_mark("done")),
                Some(Err(_)) => ("failed".to_string(), status_mark("failed")),
            };
            let mut row = vec![mark];
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

/// A distinct glyph per state, so it reads without color too.
fn status_mark(status: &str) -> Span {
    match status {
        "running" => span("● ", SYSTEM),
        "done" => span("✓ ", OK),
        "failed" => span("✗ ", ERROR),
        _ => span("■ ", WARN),
    }
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

/// `tick` counts 80 ms steps; `animation` is one of WORKING_ANIMATIONS.
pub(super) fn shimmer_line(label: &str, tick: usize, animation: &str) -> Line {
    let head = (tick % (label.len() + 10)) as isize - 3;
    let level = |column: usize| match (column as isize - head).unsigned_abs() {
        distance @ 0..=2 => 255 - 4 * distance as u8,
        _ => 245,
    };
    let mark = match frames(animation) {
        Some((frames, frame_ms, _)) => {
            let (glyph, gray) = frames[tick * 80 / frame_ms % frames.len()];
            span(glyph, Style::fg(gray))
        }
        // The star is lit only while the shimmer band passes over it.
        None => match head.unsigned_abs() {
            distance @ 0..=2 => span("✦", Style::fg(255 - 4 * distance as u8)),
            _ => span("✧", DIM),
        },
    };
    let mut line = vec![span(" ", PLAIN), mark, span(" ", PLAIN)];
    for (index, letter) in label.chars().enumerate() {
        line.push(span(letter.to_string(), Style::fg(level(index + 2))));
    }
    line
}

/// The done row's glyph: bright as the turn ends, then fading to its resting gray within DONE_FLASH. The star only rests.
pub(super) fn done_mark(animation: &str, since_end: Duration) -> Span {
    let Some((_, _, [opening, resting])) = frames(animation) else {
        return span("✧", DIM);
    };
    match since_end.as_millis() / 100 {
        0 => span(opening, Style::fg(255)),
        1 => span(resting, Style::fg(255)),
        2 => span(resting, Style::fg(252)),
        3 => span(resting, Style::fg(248)),
        _ => span(resting, MUTED),
    }
}

type Frames = (&'static [(&'static str, u8)], usize, [&'static str; 2]);

/// An animation's working frames as (glyph, 256-color gray), the ms each one shows, and the done row's opening and resting glyphs.
fn frames(animation: &str) -> Option<Frames> {
    Some(match animation {
        "fill" => {
            (&[("⡀", 241), ("⣀", 243), ("⣄", 245), ("⣤", 247), ("⣦", 249), ("⣶", 251), ("⣷", 253), ("⣿", 255), ("⣷", 253), ("⣶", 251), ("⣦", 249), ("⣤", 247), ("⣄", 245), ("⣀", 243)], 90, ["⣿", "⣿"])
        }
        "radar" => (&[("·", 242), ("◦", 245), ("○", 248), ("◎", 250), ("◉", 253), ("●", 255), ("◉", 253), ("◎", 250), ("○", 248), ("◦", 245)], 110, ["◉", "●"]),
        "arc" => (&[("◜", 255), ("◝", 254), ("◞", 251), ("◟", 248), ("◜", 246), ("◝", 248), ("◞", 251), ("◟", 254)], 100, ["○", "●"]),
        "diamond" => (&[("⋄", 242), ("◇", 245), ("◇", 247), ("◈", 250), ("◈", 252), ("◆", 255), ("◆", 255), ("◈", 252), ("◈", 250), ("◇", 246)], 110, ["◈", "◆"]),
        _ => return None,
    })
}

pub(super) fn recording_line(elapsed: Duration) -> Line {
    let seconds = elapsed.as_secs();
    vec![span(" ●", ERROR), span(format!(" {}:{:02}", seconds / 60, seconds % 60), BRIGHT), span("  Enter to transcribe · Esc to cancel", DIM)]
}
