use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd, TextMergeStream};

use super::render::{Line, MUTED, PLAIN, Span, Style, line_width, span, wrap};

const BULLET: Style = Style::fg(245);
const INLINE_CODE: Style = Style::fg(245);
const CODE: Style = Style::fg(252);
const CODE_BORDER: Style = Style::rgb(0x555555);
const QUOTE: Style = Style::fg(245);
const LINK: Style = Style::fg(245).underline();

/// Renders assistant markdown to `width` columns with pi's indent of one space.
pub fn render(text: &str, width: usize) -> Vec<Line> {
    let mut out: Vec<Line> = Vec::new();
    let mut spans: Vec<Span> = Vec::new();
    let mut styles: Vec<Style> = vec![PLAIN];
    let mut lists: Vec<Option<u64>> = Vec::new();
    let mut item_prefix: Option<Span> = None;
    let mut quote_depth = 0;
    let mut in_code = false;
    let mut table: Vec<Vec<Line>> = Vec::new();
    // The open link's URL and where its text starts in `spans`.
    let mut link: Option<(String, usize)> = None;
    let base = |depth: usize, quote: usize| -> Line {
        let mut indent = vec![span(" ", PLAIN)];
        for _ in 0..quote {
            indent.push(span("│ ", QUOTE));
        }
        if depth > 1 {
            indent.push(span("  ".repeat(depth - 1), PLAIN));
        }
        indent
    };
    let flush = |out: &mut Vec<Line>, spans: &mut Vec<Span>, prefix: &mut Option<Span>, indent: Line| {
        if spans.is_empty() && prefix.is_none() {
            return;
        }
        let mut first_indent = indent.clone();
        let mut rest_indent = indent;
        if let Some(prefix) = prefix.take() {
            rest_indent.push(span(" ".repeat(super::render::width(&prefix.text)), PLAIN));
            first_indent.push(prefix);
        }
        let mut lines = wrap(spans, width, &rest_indent);
        if let Some(first) = lines.first_mut() {
            first.splice(0..rest_indent.len(), first_indent);
        }
        out.extend(lines);
        spans.clear();
    };
    let gap = |out: &mut Vec<Line>| {
        if out.last().is_some_and(|l| !l.is_empty()) {
            out.push(Vec::new());
        }
    };
    // The parser splits text at `_` and `*`, which would cut a bare URL like x.com/a_/b in two.
    let parser = Parser::new_ext(text, Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS);
    for event in TextMergeStream::new(parser) {
        let style = *styles.last().unwrap();
        match event {
            Event::Start(Tag::Paragraph) => {
                if lists.is_empty() {
                    gap(&mut out);
                }
            }
            Event::End(TagEnd::Paragraph) => flush(&mut out, &mut spans, &mut item_prefix, base(lists.len(), quote_depth)),
            Event::Start(Tag::Heading { level, .. }) => {
                gap(&mut out);
                let marker = match level {
                    HeadingLevel::H1 => "# ",
                    HeadingLevel::H2 => "## ",
                    _ => "### ",
                };
                spans.push(span(marker, Style::fg(245).bold()));
                styles.push(PLAIN.bold());
            }
            Event::End(TagEnd::Heading(_)) => {
                styles.pop();
                flush(&mut out, &mut spans, &mut item_prefix, base(0, quote_depth));
            }
            Event::Start(Tag::BlockQuote(_)) => {
                gap(&mut out);
                quote_depth += 1;
            }
            Event::End(TagEnd::BlockQuote(_)) => quote_depth -= 1,
            Event::Start(Tag::List(start)) => {
                flush(&mut out, &mut spans, &mut item_prefix, base(lists.len(), quote_depth));
                if lists.is_empty() {
                    gap(&mut out);
                }
                lists.push(start);
            }
            Event::End(TagEnd::List(_)) => {
                lists.pop();
            }
            Event::Start(Tag::Item) => {
                flush(&mut out, &mut spans, &mut item_prefix, base(lists.len(), quote_depth));
                let marker = match lists.last_mut() {
                    Some(Some(n)) => {
                        *n += 1;
                        format!("{}. ", *n - 1)
                    }
                    _ => "- ".to_string(),
                };
                item_prefix = Some(span(marker, BULLET));
            }
            Event::End(TagEnd::Item) => flush(&mut out, &mut spans, &mut item_prefix, base(lists.len(), quote_depth)),
            Event::TaskListMarker(done) => spans.push(span(if done { "[x] " } else { "[ ] " }, BULLET)),
            Event::Start(Tag::CodeBlock(kind)) => {
                flush(&mut out, &mut spans, &mut item_prefix, base(lists.len(), quote_depth));
                gap(&mut out);
                let language = match &kind {
                    CodeBlockKind::Fenced(info) => info.split_whitespace().next().unwrap_or_default(),
                    CodeBlockKind::Indented => "",
                };
                out.push(code_rule(&base(lists.len(), quote_depth), width, language));
                in_code = true;
            }
            Event::End(TagEnd::CodeBlock) => {
                out.push(code_rule(&base(lists.len(), quote_depth), width, ""));
                in_code = false;
            }
            // Inside a list item or quote pulldown-cmark sends one Text per line.
            Event::Text(text) if in_code => {
                let indent = base(lists.len(), quote_depth);
                for line in text.trim_end_matches('\n').split('\n') {
                    let line = line.replace('\t', "    ");
                    out.extend(wrap(&[span(line, CODE)], width, &indent));
                }
            }
            Event::Text(text) => spans.push(span(text.to_string(), style)),
            Event::Code(text) => spans.push(span(text.to_string(), INLINE_CODE)),
            Event::Start(Tag::Strong) => styles.push(Style { bold: true, ..style }),
            Event::Start(Tag::Emphasis) => styles.push(Style { italic: true, ..style }),
            Event::Start(Tag::Strikethrough) => styles.push(Style { strikethrough: true, ..style }),
            Event::Start(Tag::Link { dest_url, .. }) => {
                link = Some((dest_url.to_string(), spans.len()));
                styles.push(LINK);
            }
            Event::End(TagEnd::Link) => {
                styles.pop();
                let (url, start) = link.take().expect("a link ends after it starts");
                let label: String = spans[start..].iter().map(|s| s.text.as_str()).collect();
                if label != url {
                    spans.push(span(format!(" ({url})"), MUTED));
                }
            }
            Event::End(TagEnd::Strong | TagEnd::Emphasis | TagEnd::Strikethrough) => {
                styles.pop();
            }
            Event::SoftBreak => spans.push(span(" ", style)),
            Event::HardBreak => spans.push(span("\n", style)),
            Event::Rule => {
                gap(&mut out);
                out.push(vec![span(" ", PLAIN), span("─".repeat(width.saturating_sub(1)), CODE_BORDER)]);
            }
            Event::Html(html) | Event::InlineHtml(html) => spans.push(span(html.to_string(), style)),
            Event::Start(Tag::Table(_)) => gap(&mut out),
            Event::Start(Tag::TableHead | Tag::TableRow) => table.push(Vec::new()),
            Event::End(TagEnd::TableCell) => table.last_mut().expect("a cell sits in a row").push(std::mem::take(&mut spans)),
            Event::End(TagEnd::Table) => {
                let room = width.saturating_sub(line_width(&base(lists.len(), quote_depth)));
                for row in aligned(std::mem::take(&mut table), room) {
                    spans = row;
                    flush(&mut out, &mut spans, &mut item_prefix, base(lists.len(), quote_depth));
                }
            }
            _ => {}
        }
    }
    flush(&mut out, &mut spans, &mut item_prefix, base(lists.len(), quote_depth));
    while out.first().is_some_and(|l| l.is_empty()) {
        out.remove(0);
    }
    out
}

/// A rule across the code block, with the fence's language on the opening one: `── rust ─────`.
fn code_rule(indent: &Line, width: usize, language: &str) -> Line {
    let rule_width = width.saturating_sub(line_width(indent)).max(1);
    let mut rule = indent.clone();
    if language.is_empty() {
        rule.push(span("─".repeat(rule_width), CODE_BORDER));
        return rule;
    }
    let label = format!(" {language} ");
    let rest = rule_width.saturating_sub(2 + super::render::width(&label));
    rule.extend([span("──", CODE_BORDER), span(label, MUTED), span("─".repeat(rest), CODE_BORDER)]);
    rule
}

/// Fits the columns into `room`, wraps each cell inside its column, splits columns with `│` and rules off every row from the next.
fn aligned(rows: Vec<Vec<Line>>, room: usize) -> Vec<Line> {
    let mut natural: Vec<usize> = Vec::new();
    for row in &rows {
        for (column, cell) in row.iter().enumerate() {
            if column == natural.len() {
                natural.push(0);
            }
            natural[column] = natural[column].max(line_width(cell));
        }
    }
    let widths = fit(&natural, room.saturating_sub(3 * natural.len().saturating_sub(1)));
    let rule: Vec<String> = widths.iter().map(|w| "─".repeat(*w)).collect();
    let mut lines = Vec::new();
    let row_count = rows.len();
    for (index, row) in rows.into_iter().enumerate() {
        let last = row.len().saturating_sub(1);
        let cells: Vec<Vec<Line>> = row.iter().enumerate().map(|(column, cell)| wrap(cell, widths[column], &Vec::new())).collect();
        let height = cells.iter().map(Vec::len).max().unwrap_or(0);
        for row_line in 0..height {
            let mut line = Vec::new();
            for (column, cell) in cells.iter().enumerate() {
                let part = cell.get(row_line).cloned().unwrap_or_default();
                let pad = widths[column].saturating_sub(line_width(&part));
                line.extend(part);
                if column != last {
                    line.push(span(" ".repeat(pad), PLAIN));
                    line.push(span(" │ ", CODE_BORDER));
                }
            }
            lines.push(line);
        }
        if index + 1 < row_count {
            lines.push(vec![span(rule.join("─┼─"), CODE_BORDER)]);
        }
    }
    lines
}

/// Column widths that fit `available`: narrow columns keep their natural width, wide ones split what is left evenly.
fn fit(natural: &[usize], mut available: usize) -> Vec<usize> {
    let mut by_width: Vec<usize> = (0..natural.len()).collect();
    by_width.sort_by_key(|&column| natural[column]);
    let mut widths = vec![0; natural.len()];
    for (sized, &column) in by_width.iter().enumerate() {
        let share = available / (natural.len() - sized);
        widths[column] = natural[column].min(share).max(1);
        available = available.saturating_sub(widths[column]);
    }
    widths
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|l| l.iter().map(|s| s.text.as_str()).collect::<String>().trim_end().to_string()).collect()
    }

    #[test]
    fn list_and_code_block_match_pi_layout() {
        let lines = render("- one `a.txt`\n- two\n\n```\nhello\n```", 10);
        assert_eq!(text(&lines), [" - one", "   a.txt", " - two", "", " ─────────", " hello", " ─────────"]);
        assert_eq!(lines[1].last().unwrap().style, INLINE_CODE);
    }

    #[test]
    fn a_code_block_in_a_list_item_is_one_box() {
        assert_eq!(text(&render("- step\n\n  ```\n  a\n  b\n  ```\n", 12)), [" - step", "", " ───────────", " a", " b", " ───────────"]);
    }

    #[test]
    fn a_fenced_code_block_names_its_language_on_the_opening_rule() {
        assert_eq!(text(&render("```rust ignore\nfn a() {}\n```", 14)), [" ── rust ─────", " fn a() {}", " ─────────────"]);
    }

    #[test]
    fn paragraphs_are_separated_by_one_blank_line() {
        assert_eq!(text(&render("a\n\nb", 20)), [" a", "", " b"]);
    }

    #[test]
    fn table_columns_line_up() {
        let lines = render("text\n\n| a | bb |\n|---|---|\n| ccc | d |\n| e | f |", 40);
        assert_eq!(text(&lines), [" text", "", " a   │ bb", " ────┼───", " ccc │ d", " ────┼───", " e   │ f"]);
    }

    #[test]
    fn table_too_wide_wraps_inside_its_columns() {
        let lines = render("| a | b |\n|---|---|\n| one two three | x |", 12);
        assert_eq!(text(&lines), [" a       │ b", " ────────┼──", " one two │ x", " three   │"]);
    }

    #[test]
    fn links_show_their_url_and_strikethrough_is_styled() {
        assert_eq!(text(&render("[docs](https://a.dev) <https://b.dev>", 60)), [" docs (https://a.dev) https://b.dev"]);
        let lines = render("~~old~~", 20);
        assert!(lines[0].iter().any(|s| s.text == "old" && s.style.strikethrough));
    }

    #[test]
    fn a_bare_url_with_an_underscore_is_one_link() {
        let lines = render("https://x.com/a_/status/1 (x)", 60);
        assert_eq!(crate::tui::render::link_at(&lines[0], 20).as_deref(), Some("https://x.com/a_/status/1"));
    }
}
