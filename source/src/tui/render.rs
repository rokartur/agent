use std::io::Write;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Clone, Copy, PartialEq, Default, Debug)]
pub enum Color {
    #[default]
    Default,
    Ansi(u8),
    Rgb(u32),
}

#[derive(Clone, Copy, PartialEq, Default, Debug)]
pub struct Style {
    pub fg: Color,
    pub bg: Option<u8>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
    pub selected: bool,
}

impl Style {
    pub const fn fg(n: u8) -> Style {
        Style { fg: Color::Ansi(n), ..PLAIN }
    }
    pub const fn rgb(rgb: u32) -> Style {
        Style { fg: Color::Rgb(rgb), ..PLAIN }
    }
    pub const fn bold(self) -> Style {
        Style { bold: true, ..self }
    }
    pub const fn italic(self) -> Style {
        Style { italic: true, ..self }
    }
    pub const fn underline(self) -> Style {
        Style { underline: true, ..self }
    }
    pub const fn on(self, bg: u8) -> Style {
        Style { bg: Some(bg), ..self }
    }

    fn sgr(&self) -> String {
        let mut codes = vec!["0".to_string()];
        match self.fg {
            _ if self.selected => codes.push("38;2;0;0;0;48;2;255;255;255".into()),
            Color::Default => {}
            Color::Ansi(n) => codes.push(format!("38;5;{n}")),
            Color::Rgb(rgb) => codes.push(format!("38;2;{};{};{}", rgb >> 16, (rgb >> 8) & 0xff, rgb & 0xff)),
        }
        if let (Some(bg), false) = (self.bg, self.selected) {
            codes.push(format!("48;5;{bg}"));
        }
        if self.bold {
            codes.push("1".into());
        }
        if self.italic {
            codes.push("3".into());
        }
        if self.underline {
            codes.push("4".into());
        }
        if self.strikethrough {
            codes.push("9".into());
        }
        format!("\x1b[{}m", codes.join(";"))
    }
}

pub const PLAIN: Style = Style { fg: Color::Default, bg: None, bold: false, italic: false, underline: false, strikethrough: false, selected: false };
pub const BRIGHT: Style = Style::fg(255);
pub const MUTED: Style = Style::fg(245);

#[derive(Clone, Debug, PartialEq)]
pub struct Span {
    pub text: String,
    pub style: Style,
    /// The whole URL this text is part of, set by `wrap` so a URL split across rows opens in full.
    pub link: Option<String>,
    /// Set only on the empty span `wrap` puts after the indent of a row that continues the row above: the text the break removed.
    pub run_on: Option<&'static str>,
}

pub type Line = Vec<Span>;

pub fn span(text: impl Into<String>, style: Style) -> Span {
    Span { text: text.into(), style, link: None, run_on: None }
}

pub fn width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

pub fn line_width(line: &Line) -> usize {
    line.iter().map(|s| width(&s.text)).sum()
}

/// Marks where the terminal cursor sits in a rendered line. Zero-width (deprecated LANGUAGE TAG), so wrapping and truncation ignore it.
pub const CURSOR: char = '\u{E0001}';

/// Removes the first `CURSOR` marker from `lines` and returns its (column, row).
pub fn take_cursor(lines: &mut [Line]) -> Option<(usize, usize)> {
    for (row, line) in lines.iter_mut().enumerate() {
        let mut column = 0;
        for span in line.iter_mut() {
            if let Some(at) = span.text.find(CURSOR) {
                span.text.remove(at);
                return Some((column + width(&span.text[..at]), row));
            }
            column += width(&span.text);
        }
    }
    None
}

/// Word-wraps styled spans to `max` columns. Each output line starts with `indent`; long words break mid-word.
pub fn wrap(spans: &[Span], max: usize, indent: &Line) -> Vec<Line> {
    let room = max.saturating_sub(line_width(indent)).max(1);
    let mut rows: Vec<(Option<&'static str>, Line)> = Vec::new();
    let mut run_on = None;
    let mut current: Line = Vec::new();
    let mut used = 0;
    for piece in spans.iter().flat_map(split_words).flat_map(mark_links) {
        if piece.text == "\n" {
            rows.push((run_on.take(), std::mem::take(&mut current)));
            used = 0;
            continue;
        }
        let piece_width = width(&piece.text);
        let is_space = piece.text.trim().is_empty();
        if used + piece_width > room && used > 0 {
            let ends_in_space = current.last().is_some_and(|s| s.text.ends_with(' '));
            rows.push((run_on, std::mem::take(&mut current)));
            run_on = Some(if is_space || ends_in_space { " " } else { "" });
            used = 0;
            if is_space {
                continue;
            }
        }
        if piece_width <= room {
            used += piece_width;
            push_span(&mut current, piece);
            continue;
        }
        for ch in piece.text.chars() {
            let w = ch.width().unwrap_or(0);
            if used + w > room {
                rows.push((run_on, std::mem::take(&mut current)));
                run_on = Some("");
                used = 0;
            }
            used += w;
            push_span(&mut current, Span { text: ch.to_string(), ..piece.clone() });
        }
    }
    if !current.is_empty() || rows.is_empty() {
        rows.push((run_on, current));
    }
    let mut lines = Vec::new();
    for (run_on, line) in rows {
        let mut row = indent.clone();
        if run_on.is_some() {
            row.push(Span { run_on, ..span("", PLAIN) });
        }
        row.extend(line);
        lines.push(row);
    }
    lines
}

fn split_words(s: &Span) -> Vec<Span> {
    let mut out = Vec::new();
    let mut word = String::new();
    for ch in s.text.chars() {
        if ch == ' ' || ch == '\n' {
            if !word.is_empty() {
                out.push(span(std::mem::take(&mut word), s.style));
            }
            out.push(span(ch.to_string(), s.style));
        } else {
            word.push(ch);
        }
    }
    if !word.is_empty() {
        out.push(span(word, s.style));
    }
    out
}

/// Splits a word so each http(s) URL in it is its own underlined span carrying the URL.
fn mark_links(word: Span) -> Vec<Span> {
    let mut out = Vec::new();
    let mut rest = word.text.as_str();
    while let Some((before, url, after)) = split_link(rest) {
        if !before.is_empty() {
            out.push(span(before, word.style));
        }
        out.push(Span { text: url.to_string(), style: word.style.underline(), link: Some(url.to_string()), run_on: None });
        rest = after;
    }
    if !rest.is_empty() || out.is_empty() {
        out.push(span(rest, word.style));
    }
    out
}

fn push_span(line: &mut Line, piece: Span) {
    match line.last_mut() {
        Some(last) if last.style == piece.style && last.link == piece.link => last.text.push_str(&piece.text),
        _ => line.push(piece),
    }
}

/// Cuts a line to `max` columns, ending with "…" when it had to cut.
pub fn truncate(line: &Line, max: usize) -> Line {
    if line_width(line) <= max {
        return line.clone();
    }
    let mut out = Vec::new();
    let mut used = 0;
    for s in line {
        let mut text = String::new();
        for ch in s.text.chars() {
            let w = ch.width().unwrap_or(0);
            if used + w + 1 > max {
                out.push(span(text, s.style));
                out.push(span("…", s.style));
                return out;
            }
            used += w;
            text.push(ch);
        }
        out.push(span(text, s.style));
    }
    out
}

/// Splits `line` into the parts before, inside and after display columns `from..to`.
fn split_columns(line: &Line, from: usize, to: usize) -> [Line; 3] {
    let mut parts: [Line; 3] = Default::default();
    let mut column = 0;
    for s in line {
        for ch in s.text.chars() {
            let w = ch.width().unwrap_or(0);
            // A wide glyph that crosses `from` goes in the middle, or the part before would end past `from`.
            let part = if column + w <= from {
                0
            } else if column < to {
                1
            } else {
                2
            };
            match parts[part].last_mut() {
                Some(last) if last.style == s.style => last.text.push(ch),
                _ => parts[part].push(span(ch.to_string(), s.style)),
            }
            column += w;
        }
    }
    parts
}

/// `line` with display columns `from..to` drawn as selected (black on white).
pub fn select(line: &Line, from: usize, to: usize) -> Line {
    let [mut out, mut inside, after] = split_columns(line, from, to);
    for s in &mut inside {
        s.style.selected = true;
    }
    out.extend(inside);
    out.extend(after);
    out
}

/// The plain text in display columns `from..to` of `line`.
pub fn text_in(line: &Line, from: usize, to: usize) -> String {
    let [_, inside, _] = split_columns(line, from, to);
    inside.iter().map(|s| s.text.as_str()).collect()
}

/// The text of each row's display columns `from..to`, top to bottom; a row `wrap` broke off rejoins the row above, without its indent.
pub fn plain_text(rows: &[(&Line, usize, usize)]) -> String {
    let mut lines: Vec<String> = Vec::new();
    for &(row, from, to) in rows {
        let run_on = row.iter().enumerate().find_map(|(at, s)| Some((at, s.run_on?)));
        match (run_on, lines.last_mut()) {
            (Some((at, joiner)), Some(last)) => {
                let indent = row[..at].iter().map(|s| width(&s.text)).sum();
                last.push_str(joiner);
                last.push_str(text_in(row, from.max(indent), to).trim_end());
            }
            _ => lines.push(text_in(row, from, to).trim_end().to_string()),
        }
    }
    lines.join("\n")
}

/// `line` with `overlay` drawn over its last columns up to `width`: a longer line is cut with "…", a shorter one padded.
pub fn overlay_right(line: &Line, overlay: Line, width: usize) -> Line {
    let start = width.saturating_sub(line_width(&overlay));
    let mut left = truncate(line, start);
    left.push(span(" ".repeat(start.saturating_sub(line_width(&left))), PLAIN));
    left.extend(overlay);
    left
}

/// `line` with `overlay` drawn over its middle columns, padding a line too short to reach them.
pub fn overlay_center(line: &Line, overlay: Line, width: usize) -> Line {
    let start = width.saturating_sub(line_width(&overlay)) / 2;
    let [mut out, _, after] = split_columns(line, start, start + line_width(&overlay));
    out.push(span(" ".repeat(start.saturating_sub(line_width(&out))), PLAIN));
    out.extend(overlay);
    out.extend(after);
    out
}

/// ANSI for `line`; http(s) URLs are underlined wherever they appear.
fn to_ansi(line: &Line) -> String {
    let mut out = String::new();
    for s in line {
        let text: String = s.text.replace('\t', "    ").chars().filter(|c| !c.is_control()).collect();
        let mut rest = text.as_str();
        while let Some((before, url, after)) = split_link(rest) {
            push_styled(&mut out, s.style, before);
            push_styled(&mut out, s.style.underline(), url);
            rest = after;
        }
        push_styled(&mut out, s.style, rest);
    }
    out.push_str("\x1b[0m");
    out
}

fn push_styled(out: &mut String, style: Style, text: &str) {
    if !text.is_empty() {
        out.push_str(&style.sgr());
        out.push_str(text);
    }
}

/// `text` split around its first http(s) URL, without trailing punctuation or an unmatched `)`.
fn split_link(text: &str) -> Option<(&str, &str, &str)> {
    let start = [text.find("https://"), text.find("http://")].into_iter().flatten().min()?;
    let tail = &text[start..];
    let mut url = &tail[..tail.find(|c: char| c.is_whitespace() || "<>\"'`".contains(c)).unwrap_or(tail.len())];
    while let Some(last) = url.chars().last() {
        let unmatched_paren = last == ')' && url.matches(')').count() > url.matches('(').count();
        if !".,;:!?".contains(last) && !unmatched_paren {
            break;
        }
        url = &url[..url.len() - 1];
    }
    Some((&text[..start], url, &text[start + url.len()..]))
}

/// The http(s) URL covering display column `column` of `line`, whole even when `wrap` split it across rows.
pub fn link_at(line: &Line, column: usize) -> Option<String> {
    let mut col = 0;
    for s in line {
        let start = col;
        col += s.text.width();
        if !(start..col).contains(&column) {
            continue;
        }
        if let Some(link) = &s.link {
            return Some(link.clone());
        }
        return url_at(&s.text, column - start).map(str::to_string);
    }
    None
}

fn url_at(text: &str, column: usize) -> Option<&str> {
    let mut rest = text;
    let mut col = 0;
    while let Some((before, url, after)) = split_link(rest) {
        let start = col + before.width();
        col = start + url.width();
        if (start..col).contains(&column) {
            return Some(url);
        }
        rest = after;
    }
    None
}

/// Keeps the last frame and rewrites only the rows that changed.
pub struct Screen {
    previous: Vec<String>,
}

impl Screen {
    pub fn new() -> Screen {
        Screen { previous: Vec::new() }
    }

    pub fn invalidate(&mut self) {
        self.previous.clear();
    }

    pub fn draw(&mut self, out: &mut impl Write, rows: Vec<Line>, cursor: Option<(u16, u16)>) -> std::io::Result<()> {
        let rows: Vec<String> = rows.iter().map(to_ansi).collect();
        let mut buffer = String::from("\x1b[?2026h\x1b[?25l");
        // Full redraw (first frame, resize, Ctrl+L): rows below the frame may hold stale or reflowed text.
        if self.previous.is_empty() || rows.len() < self.previous.len() {
            buffer.push_str(&format!("\x1b[{};1H\x1b[J", rows.len() + 1));
        }
        for (y, row) in rows.iter().enumerate() {
            if self.previous.get(y) == Some(row) {
                continue;
            }
            // Erase before writing: EL after a full-width row hits the pending-wrap cell and erases the last glyph.
            buffer.push_str(&format!("\x1b[{};1H\x1b[2K{row}", y + 1));
        }
        if let Some((x, y)) = cursor {
            buffer.push_str(&format!("\x1b[{};{}H\x1b[?25h", y + 1, x + 1));
        }
        buffer.push_str("\x1b[?2026l");
        out.write_all(buffer.as_bytes())?;
        out.flush()?;
        self.previous = rows;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redraw_after_invalidate_clears_below_the_frame() {
        let mut screen = Screen::new();
        screen.draw(&mut Vec::new(), vec![vec![span("a", PLAIN)]], None).unwrap();
        screen.invalidate();
        let mut out = Vec::new();
        screen.draw(&mut out, vec![vec![span("a", PLAIN)]], None).unwrap();
        let out = String::from_utf8(out).unwrap();
        let clear = out.find("\x1b[2;1H\x1b[J").unwrap();
        // CUP clamps on a full-height frame, so a clear after the rows would erase the last one.
        assert!(clear < out.find('a').unwrap());
    }

    #[test]
    fn overlay_cuts_the_line_before_it_and_pads_a_short_line() {
        let text = |line: Line| line.iter().map(|s| s.text.as_str()).collect::<String>();
        assert_eq!(text(overlay_right(&vec![span("abcdefgh", PLAIN)], vec![span("XY", PLAIN)], 6)), "abc…XY");
        assert_eq!(text(overlay_right(&vec![span("ab", PLAIN)], vec![span("XY", PLAIN)], 6)), "ab  XY");
    }

    #[test]
    fn center_overlay_keeps_both_sides_of_the_line_and_pads_a_short_line() {
        let text = |line: Line| line.iter().map(|s| s.text.as_str()).collect::<String>();
        assert_eq!(text(overlay_center(&vec![span("abcdefgh", PLAIN)], vec![span("XY", PLAIN)], 8)), "abcXYfgh");
        assert_eq!(text(overlay_center(&vec![span("a", PLAIN)], vec![span("XY", PLAIN)], 8)), "a  XY");
        assert_eq!(text(overlay_center(&vec![span("ab字defg", PLAIN)], vec![span("XY", PLAIN)], 8)), "ab XYefg");
    }

    #[test]
    fn cursor_marker_becomes_a_column_and_is_removed() {
        let mut lines = vec![vec![span("title", PLAIN)], vec![span("> ", PLAIN), span(format!("日本{CURSOR}x"), PLAIN)]];
        assert_eq!(take_cursor(&mut lines), Some((6, 1)));
        assert_eq!(lines[1][1].text, "日本x");
    }

    #[test]
    fn urls_are_underlined_without_trailing_punctuation() {
        assert_eq!(split_link("see https://x.dev/a_(b)). ok"), Some(("see ", "https://x.dev/a_(b)", "). ok")));
        assert_eq!(split_link("plain text"), None);
        let ansi = to_ansi(&vec![span("go http://a.b, now", PLAIN)]);
        assert_eq!(ansi, "\x1b[0mgo \x1b[0;4mhttp://a.b\x1b[0m, now\x1b[0m");
    }

    #[test]
    fn control_characters_never_reach_the_terminal() {
        assert_eq!(to_ansi(&vec![span("a\x1b]52;c;eA==\x07b\r\u{9b}2Jc", PLAIN)]), "\x1b[0ma]52;c;eA==b2Jc\x1b[0m");
    }

    #[test]
    fn link_at_finds_the_url_under_a_column() {
        let line = vec![span("世界 https://a.dev, http://b.dev", PLAIN)];
        assert_eq!(link_at(&line, 5).as_deref(), Some("https://a.dev"));
        assert_eq!(link_at(&line, 18), None);
        assert_eq!(link_at(&line, 20).as_deref(), Some("http://b.dev"));
        assert_eq!(link_at(&line, 2), None);
    }

    #[test]
    fn a_wrapped_url_is_underlined_and_opens_whole_on_every_row() {
        let rows = wrap(&[span("go https://a.dev/long", PLAIN)], 10, &vec![]);
        assert_eq!(text(&rows), ["go ", "https://a.", "dev/long"]);
        assert!(to_ansi(&rows[2]).starts_with("\x1b[0;4mdev/long"));
        assert_eq!(link_at(&rows[2], 0).as_deref(), Some("https://a.dev/long"));
    }

    fn text(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|l| l.iter().map(|s| s.text.as_str()).collect()).collect()
    }

    #[test]
    fn wrap_breaks_on_words_and_drops_the_break_space() {
        let lines = wrap(&[span("aaa bbb ccc", PLAIN)], 7, &vec![]);
        assert_eq!(text(&lines), ["aaa bbb", "ccc"]);
    }

    #[test]
    fn wrap_keeps_indent_and_splits_long_words() {
        let lines = wrap(&[span("abcdefgh", PLAIN)], 5, &vec![span(" ", PLAIN)]);
        assert_eq!(text(&lines), [" abcd", " efgh"]);
    }

    #[test]
    fn plain_text_rejoins_the_rows_wrap_broke() {
        let rows = wrap(&[span("go https://a.dev/long\nwide text\nabcd efg h", PLAIN)], 10, &vec![span("│ ", PLAIN)]);
        let selected: Vec<_> = rows.iter().map(|row| (row, 0, usize::MAX)).collect();
        assert_eq!(plain_text(&selected), "│ go https://a.dev/long\n│ wide text\n│ abcd efg h");
    }

    #[test]
    fn truncate_counts_wide_chars() {
        let line = vec![span("ąę中文abc", PLAIN)];
        assert_eq!(text(&[truncate(&line, 6)]), ["ąę中…"]);
    }
}
