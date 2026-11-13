use super::OK;
use super::render::{ACTIVE_TAB, BRIGHT, DIM, Line, MUTED, PLAIN, span, truncate, width};

pub struct Row {
    pub left: String,
    pub right: Line,
}

/// Items whose text matches the search `query`, ignoring case: substring matches first, then fuzzy ones.
/// Fuzzy means the query's non-space characters appear in the text in order, so "cl max" finds "Claude Pro/Max".
pub fn search<T>(items: impl IntoIterator<Item = T>, query: &str, text: impl Fn(&T) -> String) -> Vec<T> {
    let mut ranked: Vec<(u8, T)> = items.into_iter().filter_map(|item| Some((search_rank(&text(&item), query)?, item))).collect();
    ranked.sort_by_key(|(rank, _)| *rank);
    ranked.into_iter().map(|(_, item)| item).collect()
}

/// 0 when `text` contains `query`, 1 when it matches only fuzzily, ignoring case.
pub fn search_rank(text: &str, query: &str) -> Option<u8> {
    let (text, query) = (text.to_lowercase(), query.to_lowercase());
    if text.contains(&query) {
        return Some(0);
    }
    let mut chars = text.chars();
    query.chars().filter(|c| !c.is_whitespace()).all(|wanted| chars.any(|c| c == wanted)).then_some(1)
}

/// fx's bottom panel: a blank line, `Title   Tab   Tab` with the active tab on a background, `> search` when it filters, rows with the selected one bright, key hints.
pub struct List<'a> {
    pub title: String,
    pub tabs: &'a [&'a str],
    pub tab: usize,
    /// The filter field, `Editor::field` output; `None` for panels that do not filter.
    pub search: Option<String>,
    pub rows: &'a [Row],
    pub selected: usize,
    pub empty: &'a str,
    pub hint: &'a str,
}

impl List<'_> {
    pub fn render(&self, columns: usize, max_rows: usize) -> Vec<Line> {
        let mut header = vec![span(self.title.clone(), BRIGHT.bold())];
        for (index, tab) in self.tabs.iter().enumerate() {
            header.push(span("  ", PLAIN));
            header.push(if index == self.tab { span(format!(" {tab} "), ACTIVE_TAB) } else { span(*tab, MUTED) });
        }
        let mut head = vec![header];
        if let Some(search) = &self.search {
            head.push(vec![span(format!("> {search}"), BRIGHT.bold())]);
        }
        if self.rows.is_empty() {
            head.push(vec![span(format!("  {}", self.empty), MUTED)]);
        }
        let left_width = self.rows.iter().map(|r| width(&r.left)).max().unwrap_or(0);
        // A row deleted under the selection leaves it past the end until the next key; mark the last row instead.
        let selected = self.selected.min(self.rows.len().saturating_sub(1));
        frame(head, self.rows.len(), selected, self.hint, columns, max_rows, |index| {
            let row = &self.rows[index];
            let style = if index == selected { BRIGHT.bold() } else { MUTED };
            let pad = " ".repeat(left_width - width(&row.left));
            let mut line = vec![span(format!("{}{pad}", row.left), style), span("    ", PLAIN)];
            line.extend(row.right.iter().cloned());
            line
        })
    }
}

/// A panel around rows the caller styles: a blank line, `head`, the `max_rows` rows around `selected` behind a `→`, `n/len` when they scroll, key hints.
pub fn frame(head: Vec<Line>, len: usize, selected: usize, hint: &str, columns: usize, max_rows: usize, row: impl Fn(usize) -> Line) -> Vec<Line> {
    let mut lines = vec![Vec::new()];
    lines.extend(head);
    let first = (selected + 1).saturating_sub(max_rows);
    for index in first..len.min(first + max_rows) {
        let marker = if index == selected { "→ " } else { "  " };
        let mut line = vec![span(marker, OK)];
        line.extend(row(index));
        lines.push(truncate(&line, columns));
    }
    if len > max_rows {
        lines.extend([Vec::new(), vec![span(format!("  {}/{len}", selected + 1), DIM)]]);
    }
    lines.extend([Vec::new(), vec![span(hint, MUTED)]]);
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(line: &Line) -> String {
        line.iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn rows_align_and_the_selected_one_is_marked_and_bright() {
        let rows = [Row { left: "a".into(), right: vec![span("x", MUTED)] }, Row { left: "abc".into(), right: vec![span("y", MUTED)] }];
        let panel = List { title: "Sessions 2".into(), tabs: &["One", "Two"], tab: 0, search: None, rows: &rows, selected: 1, empty: "", hint: "esc close" };
        let lines = panel.render(20, 5);
        assert_eq!(text(&lines[1]), "Sessions 2   One   Two");
        assert_eq!(lines[1][2].style, ACTIVE_TAB);
        assert_eq!(text(&lines[2]), "  a      x");
        assert_eq!(text(&lines[3]), "→ abc    y");
        assert_eq!(lines[3][0].style, OK);
        assert_eq!(lines[3][1].style, BRIGHT.bold());
        assert_eq!(text(&lines[5]), "esc close");
    }

    #[test]
    fn a_list_that_scrolls_shows_the_selected_position_under_it() {
        let rows: Vec<Row> = (0..5).map(|i| Row { left: i.to_string(), right: Vec::new() }).collect();
        let panel = |selected| List { title: "Settings".into(), tabs: &[], tab: 0, search: None, rows: &rows, selected, empty: "", hint: "" };
        let lines: Vec<String> = panel(3).render(40, 2).iter().map(text).collect();
        assert_eq!(lines[..6], ["", "Settings", "  2    ", "→ 3    ", "", "  4/5"]);
        assert_eq!(panel(3).render(40, 2)[5][0].style, DIM);
        assert_eq!(panel(3).render(40, 5).len(), 9);
    }

    #[test]
    fn a_selection_past_a_shrunk_list_marks_its_last_row() {
        let rows = [Row { left: "a".into(), right: Vec::new() }, Row { left: "b".into(), right: Vec::new() }];
        let panel = List { title: "Todos 2".into(), tabs: &[], tab: 0, search: None, rows: &rows, selected: 2, empty: "", hint: "" };
        let lines = panel.render(40, 5);
        assert_eq!(text(&lines[3]), "→ b    ");
        assert_eq!(lines[3][1].style, BRIGHT.bold());
    }

    #[test]
    fn search_sits_on_its_own_line_under_the_tabs() {
        let rows = [Row { left: "a".into(), right: Vec::new() }];
        let panel = List { title: "History".into(), tabs: &["All", "Here"], tab: 0, search: Some("fo".into()), rows: &rows, selected: 0, empty: "", hint: "" };
        let lines: Vec<String> = panel.render(40, 5).iter().map(text).collect();
        assert_eq!(lines[1..4], ["History   All   Here", "> fo", "→ a    "]);
    }

    #[test]
    fn search_lists_substring_matches_before_fuzzy_ones() {
        let labels = ["Claude Pro/Max", "Max mode", "OpenAI API key"];
        assert_eq!(search(labels, "max", |l| l.to_string()), ["Claude Pro/Max", "Max mode"]);
        assert_eq!(search(labels, "cl max", |l| l.to_string()), ["Claude Pro/Max"]);
        assert_eq!(search(labels, "oak", |l| l.to_string()), ["OpenAI API key"]);
        assert_eq!(search(["Merge order", "Max mode"], "mode", |l| l.to_string()), ["Max mode", "Merge order"]);
        assert!(search(labels, "xyz", |l| l.to_string()).is_empty());
    }
}
