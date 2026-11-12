use super::*;

impl App {
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
    let mut lines = Vec::new();
    let mut previous_run = None;
    for entry in &mut entries {
        let item_lines = entry.lines(width, false, cwd).clone();
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
