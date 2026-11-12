use super::*;

impl App {
    /// pj's mouse selection over the transcript: release copies, a drag past its top or bottom scrolls.
    pub(super) fn mouse(&mut self, mouse: MouseEvent) {
        // frame draws no transcript under these, so the layout it left is not on screen.
        if matches!(self.panel, Some(Panel::Diff { .. } | Panel::Terminal { .. })) {
            return;
        }
        let (visible, _) = self.transcript_view;
        let row = mouse.row as usize;
        self.edge_drag = None;
        if mouse.kind == MouseEventKind::Down(MouseButton::Left) && self.on_jump_pill(row, mouse.column as usize) {
            self.scroll = 0;
            return;
        }
        if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
            let (top, rows, _) = self.input_view;
            self.selecting_input = (top..top + rows).contains(&row);
            // A press inside keeps the selection: `press` needs its last click to count double and triple clicks.
            if !self.selecting_input {
                self.input_selection = Selection::default();
            }
        }
        let wheel = matches!(mouse.kind, MouseEventKind::ScrollUp | MouseEventKind::ScrollDown);
        let (top, rows, _) = self.input_view;
        if wheel && (top..top + rows).contains(&row) {
            let step = self.settings.scroll_lines as isize;
            self.editor.scroll(self.editor_width, if mouse.kind == MouseEventKind::ScrollUp { -step } else { step });
            return;
        }
        if self.selecting_input && !wheel {
            self.input_mouse(mouse.kind, row, mouse.column);
            return;
        }
        match mouse.kind {
            MouseEventKind::ScrollUp => self.scroll += self.settings.scroll_lines,
            MouseEventKind::ScrollDown => self.scroll = self.scroll.saturating_sub(self.settings.scroll_lines),
            MouseEventKind::Down(MouseButton::Left) if row < visible => {
                let point = self.point_at(row, mouse.column);
                let text = self.transcript_text(point.line);
                self.selection.press(point, &text);
            }
            MouseEventKind::Down(MouseButton::Left) => self.selection = Selection::default(),
            MouseEventKind::Drag(MouseButton::Left) if visible > 0 => {
                // Edge scroll stops at either end, so a drag whose button-up never arrives cannot scroll forever.
                let max_scroll = self.transcript_size.0.saturating_sub(visible);
                if self.selection.dragging && row == 0 && self.scroll < max_scroll {
                    self.scroll += 1;
                    self.edge_drag = Some((mouse, Instant::now()));
                } else if self.selection.dragging && row >= visible && self.scroll > 0 {
                    self.scroll -= 1;
                    self.edge_drag = Some((mouse, Instant::now()));
                }
                let point = self.point_at(row.min(visible - 1), mouse.column);
                let text = self.transcript_text(point.line);
                self.selection.drag(point, &text);
            }
            MouseEventKind::Up(MouseButton::Left) if self.selection.release() => {
                if self.settings.copy_on_select {
                    let text = self.selected_text();
                    self.copy_selection(&text);
                }
            }
            MouseEventKind::Up(MouseButton::Left) if row < visible => {
                let point = self.point_at(row, mouse.column);
                let line = self.transcript_line(point.line);
                let Some(url) = link_at(&line, point.column) else { return };
                self.ask_terminal(json!({"open": url}));
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let (top, lines) = &self.panel_view;
                let Some(line) = row.checked_sub(*top).and_then(|at| lines.get(at)) else { return };
                let Some(url) = link_at(line, mouse.column as usize) else { return };
                self.ask_terminal(json!({"open": url}));
            }
            _ => {}
        }
    }

    /// Same selection as the transcript, over the editor rows; a drag outside them clamps to the edge row.
    pub(super) fn input_mouse(&mut self, kind: MouseEventKind, row: usize, column: u16) {
        let (top, rows, below) = self.input_view;
        let row = row.clamp(top, top + rows - 1) - top;
        let point = Point { line: below + rows - 1 - row, column: column as usize };
        let text = text_in(&self.input_line(point.line), 0, usize::MAX);
        match kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.selection = Selection::default();
                self.input_selection.press(point, &text);
                let total = self.editor.render(self.editor_width).0.len();
                if let Some(row) = total.checked_sub(point.line + 1) {
                    self.editor.click(self.editor_width, row, point.column);
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => self.input_selection.drag(point, &text),
            MouseEventKind::Up(MouseButton::Left) if self.input_selection.release() && self.settings.copy_on_select => {
                let text = self.input_selected_text();
                self.copy_selection(&text);
            }
            _ => {}
        }
    }

    pub(super) fn input_selected_text(&self) -> String {
        let Some((start, end)) = self.input_selection.range() else { return String::new() };
        let mut lines = Vec::new();
        for line in (end.line..=start.line).rev() {
            let (from, to) = self.input_selection.columns(line).expect("line is inside the range");
            // Every editor row starts with a 2-column prompt ("❯ ", "$ " or "  ").
            lines.push(text_in(&self.input_line(line), from.max(2), to).trim_end().to_string());
        }
        lines.join("\n")
    }

    /// Editor row `line`, counted up from the last row, prompt included like transcript gutters.
    /// Empty when a paste, slash command or transcript shrank the editor under a selection made earlier.
    pub(super) fn input_line(&self, line: usize) -> Line {
        let (mut rows, _) = self.editor.render(self.editor_width);
        let Some(index) = rows.len().checked_sub(line + 1) else { return Line::new() };
        rows.swap_remove(index)
    }

    pub(super) fn copy_selection(&mut self, text: &str) {
        if text.trim().is_empty() {
            self.hint = Some(("Selection is empty".into(), Instant::now()));
            return;
        }
        self.copy(text, Copied::Hint("Copied selection".into()));
    }

    pub(super) fn copy(&mut self, text: &str, copied: Copied) {
        self.ask_terminal(json!({"copy": text}));
        self.pending_copy = Some(copied);
    }

    /// The pill sits centered on the last transcript row while the view is scrolled up.
    pub(super) fn on_jump_pill(&self, row: usize, column: usize) -> bool {
        let (visible, width) = self.transcript_view;
        let pill = line_width(&vec![span(jump_pill(), PLAIN)]);
        let start = width.saturating_sub(pill) / 2;
        self.scroll > 0 && visible > 0 && row == visible - 1 && (start..start + pill).contains(&column)
    }

    pub(super) fn point_at(&self, row: usize, column: u16) -> Point {
        let (visible, _) = self.transcript_view;
        Point { line: self.scroll + visible - 1 - row, column: column as usize }
    }

    /// Plain text of transcript line `line` (counted up from the last one) as rendered at the last frame's width.
    pub(super) fn transcript_text(&mut self, line: usize) -> String {
        self.transcript_line(line).iter().map(|s| s.text.as_str()).collect()
    }

    pub(super) fn transcript_line(&mut self, line: usize) -> Line {
        let (_, width) = self.transcript_view;
        let (mut tail, _, _) = self.transcript_tail(width, line + 1);
        let Some(index) = tail.len().checked_sub(line + 1) else { return Line::new() };
        tail.swap_remove(index)
    }

    /// shortcut: copies gutters at the start of each source line, except a user prompt's `❯ ` and indent; strip them if that gets in the way.
    pub(super) fn selected_text(&mut self) -> String {
        let Some((start, end)) = self.selection.range() else { return String::new() };
        let (_, width) = self.transcript_view;
        let (tail, _, _) = self.transcript_tail(width, start.line + 1);
        let mut rows = Vec::new();
        for line in (end.line..=start.line).rev() {
            let Some(index) = tail.len().checked_sub(line + 1) else { continue };
            let row = &tail[index];
            let (from, to) = self.selection.columns(line).expect("line is inside the range");
            let from = if row.first().is_some_and(|s| Style { bg: None, ..s.style } == PROMPT_LEAD) { from.max(2) } else { from };
            rows.push((row, from, to));
        }
        plain_text(&rows)
    }

    /// Paste lands in the input that has focus: an open panel's field, or the editor when no panel is open.
    pub(super) fn paste(&mut self, text: &str) {
        let line = text.replace(['\r', '\n'], " ");
        match self.panel.as_mut() {
            Some(Panel::Login(login)) => login.paste(text),
            Some(Panel::Ask(ask)) => ask.paste(&line),
            Some(
                Panel::Models { query, index, .. }
                | Panel::ContextWindows { query, index, .. }
                | Panel::Accounts { query, index, .. }
                | Panel::History { query, index, .. }
                | Panel::Skills { query, index },
            ) => {
                query.insert(&line);
                *index = 0;
            }
            Some(Panel::Settings { row, query }) => {
                query.insert(&line);
                if let Some(&first) = matching_settings(*row, &query.text()).first() {
                    *row = first;
                }
            }
            Some(Panel::Btw) => self.btw.as_mut().expect("the btw panel opens with a thread").input.insert(&line),
            Some(Panel::Headroom { input, .. } | Panel::Profiles { naming: Some(input), .. } | Panel::Environments { naming: Some(input), .. }) => input.insert(line.trim()),
            Some(Panel::ProfileSkills { query, index, .. }) => {
                query.insert(line.trim());
                *index = 0;
            }
            Some(_) => {}
            None => self.editor.paste(text),
        }
    }

    pub(super) fn key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        if self.panel.is_some() {
            self.panel_key(key);
            return;
        }
        if self.recording.is_some() {
            match key.code {
                KeyCode::Enter => self.stop_recording(),
                KeyCode::Esc => self.recording.take().unwrap().cancel(),
                KeyCode::Char('c') if ctrl => self.recording.take().unwrap().cancel(),
                _ => {}
            }
            return;
        }
        let matches = self.menu_matches();
        let menu_len = matches.len();
        if menu_len > 0 && !(ctrl || alt || shift) {
            match key.code {
                KeyCode::Up => self.menu_index = (self.menu_index.min(menu_len - 1) + menu_len - 1) % menu_len,
                KeyCode::Down => self.menu_index = (self.menu_index.min(menu_len - 1) + 1) % menu_len,
                KeyCode::Tab if self.workspace_query().is_some() => {
                    let path = matches[self.menu_index.min(menu_len - 1)].0.clone();
                    self.accept_path(&path);
                }
                KeyCode::Enter if self.workspace_query().is_some() => {
                    let path = matches[self.menu_index.min(menu_len - 1)].0.clone();
                    self.accept_path(&path);
                    self.edit(key, ctrl, alt, shift);
                }
                KeyCode::Tab | KeyCode::Enter if self.file_query().is_some() => {
                    let path = &matches[self.menu_index.min(menu_len - 1)].0;
                    self.accept_path(path);
                }
                // Prose holds `/api` and `/tmp`, so Enter there sends the prompt and only Tab picks a skill.
                KeyCode::Tab if self.skill_query().is_some() => {
                    let name = &matches[self.menu_index.min(menu_len - 1)].0;
                    self.accept_skill(name);
                }
                KeyCode::Enter if self.skill_query().is_some() => self.edit(key, ctrl, alt, shift),
                KeyCode::Tab => self.accept_slash(false),
                KeyCode::Enter => self.accept_slash(true),
                _ => self.edit(key, ctrl, alt, shift),
            }
            return;
        }
        self.edit(key, ctrl, alt, shift);
    }

    pub(super) fn edit(&mut self, key: KeyEvent, ctrl: bool, alt: bool, shift: bool) {
        let before = self.editor.text();
        self.edit_key(key, ctrl, alt, shift);
        if self.editor.text() != before {
            self.menu_index = 0;
            self.input_selection = Selection::default();
        }
    }

    pub(super) fn edit_key(&mut self, key: KeyEvent, ctrl: bool, alt: bool, shift: bool) {
        match key.code {
            KeyCode::Enter if shift || (alt && self.working.is_none()) => self.editor.newline(),
            KeyCode::Char('j') if ctrl => self.editor.newline(),
            KeyCode::Enter => {
                if self.editor.text().trim().is_empty() {
                    return;
                }
                let text = self.editor.take();
                if let Some(view) = &self.viewed {
                    match view.run.message(&text) {
                        Ok(()) => self.hint = Some((format!("Sent to {}", view.run.label), Instant::now())),
                        Err(error) => {
                            self.editor.set(&text);
                            self.hint = Some((error, Instant::now()));
                        }
                    }
                    return;
                }
                if let Err(error) = history::append(&self.home.join(".agents"), history::Entry { prompt: text.clone(), cwd: self.cwd.clone(), session: self.session_id.clone() }) {
                    self.push(Item::Error(error));
                }
                self.submit(text, alt);
            }
            KeyCode::Esc if self.viewed.is_some() => self.view_run(None),
            KeyCode::Esc if self.working.is_some() && self.editor.is_empty() && !answered(&self.entries) => {
                self.retract = true;
                self.abort.store(true, Ordering::Relaxed);
            }
            KeyCode::Esc if self.working.is_some() => {
                if !self.abort.swap(true, Ordering::Relaxed) {
                    self.system("agent", "Interrupting...");
                }
            }
            KeyCode::Esc if self.hint.as_ref().is_some_and(|(text, _)| text == REWIND_HINT) => {
                self.hint = None;
                let _ = self.commands.send(Command::RewindPoints);
            }
            KeyCode::Esc => self.hint = Some((REWIND_HINT.into(), Instant::now())),
            KeyCode::Char('c') if ctrl => self.ctrl_c(),
            KeyCode::Char('d') if ctrl && self.editor.is_empty() => self.quit = true,
            KeyCode::Char('o') if ctrl => {
                self.expanded = !self.expanded;
            }
            KeyCode::Char('l') if ctrl => self.invalidate_screens(),
            KeyCode::Char('r') if ctrl => self.open_history(),
            KeyCode::BackTab => self.cycle_thinking(),
            KeyCode::Char('p') if ctrl => self.cycle_model(),
            KeyCode::Up if alt => {
                let steering = std::mem::take(&mut *self.steering.lock().unwrap());
                let draft = self.editor.text();
                let mut queued: Vec<String> = if draft.trim().is_empty() { Vec::new() } else { vec![draft] };
                let had_draft = !queued.is_empty();
                for (mut text, images) in steering.into_iter().chain(std::mem::take(&mut self.queued_commands)) {
                    let offset = self.images.len();
                    // Highest first: with offset 1, #1 becomes #2 only after the old #2 became #3.
                    for number in (1..=images.len()).rev() {
                        text = text.replace(&format!("[Image #{number}]"), &format!("[Image #{}]", number + offset));
                    }
                    self.images.extend(images);
                    queued.push(text);
                }
                if queued.len() > usize::from(had_draft) {
                    self.editor.set(&queued.join("\n\n"));
                }
            }
            KeyCode::Down if alt => self.scroll = 0,
            KeyCode::PageUp => self.scroll += self.transcript_view.0.max(1),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(self.transcript_view.0.max(1)),
            KeyCode::Up => self.editor.up(self.editor_width),
            KeyCode::Down
                if self.editor.is_empty() && (footer_agents(&self.subagents.lock().unwrap()) > 0 || self.terminals.lock().unwrap().running() > 0 || !footer_workflows(&self.workflows).is_empty()) =>
            {
                self.panel = Some(Panel::Agents { row: 1 });
            }
            KeyCode::Down => self.editor.down(self.editor_width),
            KeyCode::Char('v') if ctrl => self.ask_terminal(json!({"paste_image": true})),
            _ => _ = self.editor.edit_key(key),
        }
    }

    pub(super) fn attach(&mut self, image: Result<Value, String>) {
        match (image, self.panel.as_mut()) {
            (Ok(block), Some(Panel::Ask(ask))) => ask.attach(block),
            (Ok(block), _) => {
                self.images.push(block);
                self.editor.insert(&format!("[Image #{}] ", self.images.len()));
            }
            (Err(error), _) => self.hint = Some((error, Instant::now())),
        }
    }

    /// The pasted images whose `[Image #n]` is still in `text`, each shown as a row under the prompt; the rest are dropped.
    pub(super) fn take_images(&mut self, text: &str) -> Vec<Value> {
        let mut kept = Vec::new();
        let mut tags = Vec::new();
        for (index, mut block) in std::mem::take(&mut self.images).into_iter().enumerate() {
            let tag = format!("[Image #{}]", index + 1);
            // A null holds the place of an image whose tag was deleted.
            if text.contains(&tag) && !block.is_null() {
                // The API rejects unknown keys, so the dropped file's path leaves the block here.
                match block.as_object_mut().unwrap().remove(DROPPED_PATH) {
                    Some(Value::String(path)) => tags.push(format!("{tag} {path}")),
                    _ => tags.push(tag),
                }
                kept.push(block);
            }
        }
        if !tags.is_empty() {
            self.push(Item::Images(tags));
        }
        kept
    }

    /// Drops the last prompt from the transcript and puts it back in the editor.
    pub(super) fn take_back(&mut self, point: RewindPoint) {
        if let Some(prompt) = self.entries.iter().rposition(|entry| matches!(entry.item, Item::User(_))) {
            self.entries.truncate(prompt);
        }
        self.restore_draft(point);
    }

    /// Puts a rewound prompt back in an empty editor, with its images under their `[Image #n]` numbers again.
    pub(super) fn restore_draft(&mut self, point: RewindPoint) {
        if !self.editor.is_empty() {
            return;
        }
        self.editor.set(&point.text);
        self.images = image_slots(&point.text, point.images);
    }

    pub(super) fn stop_recording(&mut self) {
        let recording = self.recording.take().unwrap();
        let events = self.events.clone();
        self.transcribing = Some(Instant::now());
        std::thread::spawn(move || {
            let _ = events.send(Msg::Transcribed(recording.transcribe()));
        });
    }

    pub(super) fn ctrl_c(&mut self) {
        if !self.settings.copy_on_select {
            let text = if self.selection.range().is_some() { self.selected_text() } else { self.input_selected_text() };
            if !text.trim().is_empty() {
                self.copy_selection(&text);
                self.selection = Selection::default();
                self.input_selection = Selection::default();
                return;
            }
        }
        if !self.editor.is_empty() {
            self.editor.set("");
            return;
        }
        if self.hint.as_ref().is_some_and(|(text, _)| text == EXIT_HINT) {
            self.abort.store(true, Ordering::Relaxed);
            self.quit = true;
            return;
        }
        self.hint = Some((EXIT_HINT.into(), Instant::now()));
    }

    pub(super) fn cycle_thinking(&mut self) {
        self.update_settings(|s| s.thinking = step(&THINKING_LEVELS, &s.thinking.as_str(), true).to_string());
        self.hint = Some((format!("Thinking level: {}", self.settings.thinking), Instant::now()));
    }

    /// Switches to the next pinned model, skipping pins missing from the catalog.
    pub(super) fn cycle_model(&mut self) {
        if self.settings.pinned_models.is_empty() {
            self.hint = Some(("Pin models with Tab in /model to switch between them with Ctrl+P".into(), Instant::now()));
            return;
        }
        let catalog = self.client.catalog();
        if catalog.is_empty() {
            self.hint = Some((MODELS_STILL_LOADING.into(), Instant::now()));
            return;
        }
        let mut available = Vec::new();
        let mut missing = Vec::new();
        for key in &self.settings.pinned_models {
            match catalog.iter().find(|m| model_key(m) == *key) {
                Some(m) if !available.contains(&m.id) => available.push(m.id.clone()),
                Some(_) => {}
                None => missing.push(key.clone()),
            }
        }
        if !missing.is_empty() {
            self.warn("models", format!("Not available, skipped: {}", missing.join(", ")));
        }
        if available.is_empty() {
            return;
        }
        let model = step(&available, &self.settings.model, true);
        self.update_settings(|s| s.model = model);
        self.hint = Some((format!("Model: {}", self.settings.model), Instant::now()));
    }
}
