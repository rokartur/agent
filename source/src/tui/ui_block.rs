use serde_json::Value;

use super::markdown;
use super::render::{BRIGHT, DIM, Line, MUTED, PLAIN, Style, line_width, span, truncate, width};
use super::{ERROR, OK};

/// 256-color, so a cell can draw one series as foreground over the next as background.
const SERIES: [u8; 6] = [75, 179, 78, 141, 203, 80];
const AXIS: Style = Style::rgb(0x555555);
const LOWER_EIGHTHS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
const LEFT_EIGHTHS: [char; 9] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];
const COLUMN_HEIGHT: usize = 8;
const ROW_GAP: usize = 3;
const MIN_ROW_COLUMN: usize = 20;
const PROGRESS_LABEL: usize = 12;
const MAX_PROGRESS_BAR: usize = 40;

/// Draws the JSON of a ```ui block, in the shape `agent::prompt::UI_BLOCKS` teaches the model, in `width` columns.
pub fn render(node: &Value, width: usize) -> Result<Vec<Line>, String> {
    let lines = match node.get("title").and_then(Value::as_str) {
        None => body(node, width)?,
        Some(title) => {
            let mut lines = vec![vec![span(title, BRIGHT.bold())]];
            lines.extend(body(node, width.saturating_sub(2))?.into_iter().map(|line| [vec![span("  ", PLAIN)], line].concat()));
            lines
        }
    };
    Ok(lines.iter().map(|line| truncate(line, width)).collect())
}

fn body(node: &Value, width: usize) -> Result<Vec<Line>, String> {
    if let Some(nodes) = node.as_array() {
        return column(nodes, width);
    }
    if let Some(nodes) = node.get("col") {
        return column(nodes.as_array().ok_or("`col` takes an array of nodes")?, width);
    }
    if let Some(nodes) = node.get("row") {
        return row(nodes.as_array().ok_or("`row` takes an array of nodes")?, width);
    }
    if let Some(text) = node.get("text") {
        let text = text.as_str().ok_or("`text` takes a Markdown string")?;
        // markdown::render indents every line by one column, which the block already has.
        let mut lines = markdown::render(text, width + 1);
        for line in &mut lines {
            if let Some(first) = line.first_mut() {
                first.text = first.text.strip_prefix(' ').unwrap_or(&first.text).to_string();
            }
        }
        return Ok(lines);
    }
    if let Some(value) = node.get("stat") {
        return stat(node, value);
    }
    if let Some(fraction) = node.get("progress") {
        return progress(node, fraction.as_f64().ok_or("`progress` takes a fraction from 0 to 1")?, width);
    }
    if let Some(rows) = node.get("chart") {
        let table = table(rows)?;
        return match node.get("form").and_then(Value::as_str).unwrap_or("columns") {
            "columns" => Ok(columns(&table, width)),
            "bars" => Ok(bars(&table, width)),
            form => Err(format!("unknown chart form `{form}`, use `columns` or `bars`")),
        };
    }
    let mut shown = node.to_string();
    shown.truncate(shown.floor_char_boundary(60));
    Err(format!("unknown node {shown}"))
}

/// Nodes top to bottom, a blank line between two when either takes more than one line.
fn column(nodes: &[Value], width: usize) -> Result<Vec<Line>, String> {
    let mut lines = Vec::new();
    let mut last_height = 0;
    for node in nodes {
        let drawn = render(node, width)?;
        if last_height > 0 && (last_height > 1 || drawn.len() > 1) {
            lines.push(Vec::new());
        }
        last_height = drawn.len();
        lines.extend(drawn);
    }
    Ok(lines)
}

/// Nodes side by side in equal columns, or stacked when a column would be narrower than `MIN_ROW_COLUMN`.
fn row(nodes: &[Value], width: usize) -> Result<Vec<Line>, String> {
    let count = nodes.len().max(1);
    let column_width = width.saturating_sub(ROW_GAP * (count - 1)) / count;
    if column_width < MIN_ROW_COLUMN {
        return column(nodes, width);
    }
    let drawn = nodes.iter().map(|node| render(node, column_width)).collect::<Result<Vec<_>, _>>()?;
    let height = drawn.iter().map(Vec::len).max().unwrap_or(0);
    let mut lines = Vec::new();
    for index in 0..height {
        let mut line = Vec::new();
        for (column, part) in drawn.iter().enumerate() {
            let mut cell = part.get(index).cloned().unwrap_or_default();
            if column > 0 {
                line.push(span(" ".repeat(ROW_GAP), PLAIN));
            }
            if column + 1 < drawn.len() {
                cell.push(span(" ".repeat(column_width - line_width(&cell)), PLAIN));
            }
            line.extend(cell);
        }
        lines.push(line);
    }
    Ok(lines)
}

fn stat(node: &Value, value: &Value) -> Result<Vec<Line>, String> {
    let value = match value {
        Value::Number(n) => number(n.as_f64().expect("a JSON number reads as an f64")),
        Value::String(text) => text.clone(),
        _ => return Err("`stat` takes a number or a string".into()),
    };
    let mut lines = vec![vec![span(value, BRIGHT.bold())]];
    let mut caption = Vec::new();
    if let Some(label) = node.get("label").and_then(Value::as_str) {
        caption.push(span(format!("{label}  "), MUTED));
    }
    if let Some(delta) = node.get("delta").and_then(Value::as_str) {
        caption.push(span(delta, if delta.starts_with(['-', '−']) { ERROR } else { OK }));
    }
    if !caption.is_empty() {
        lines.push(caption);
    }
    Ok(lines)
}

/// `Migrated    ██████████▍░░░░░░░  58%`, filled to an eighth of a cell.
fn progress(node: &Value, fraction: f64, width: usize) -> Result<Vec<Line>, String> {
    let fraction = fraction.clamp(0.0, 1.0);
    let label = node.get("label").and_then(Value::as_str).unwrap_or_default();
    let label_width = if label.is_empty() { 0 } else { PROGRESS_LABEL.max(self::width(label) + 2) };
    let cells = width.saturating_sub(label_width + 5).min(MAX_PROGRESS_BAR);
    let eighths = (fraction * (cells * 8) as f64).round() as usize;
    let mut bar = "█".repeat(eighths / 8);
    if !eighths.is_multiple_of(8) {
        bar.push(LEFT_EIGHTHS[eighths % 8]);
    }
    let empty = cells - eighths.div_ceil(8);
    let percent = format!("{}%", (fraction * 100.0).round());
    Ok(vec![vec![
        span(format!("{label}{}", " ".repeat(label_width - self::width(label))), MUTED),
        span(bar, Style::fg(SERIES[0])),
        span("░".repeat(empty), DIM),
        span(format!("{percent:>5}"), BRIGHT),
    ]])
}

/// A chart's rows: the header names the label column and then each series, every other row is a label and its values.
struct Table {
    series: Vec<String>,
    labels: Vec<String>,
    values: Vec<Vec<f64>>,
}

fn table(rows: &Value) -> Result<Table, String> {
    let rows = rows.as_array().ok_or("`chart` takes an array of rows")?;
    let Some((header, rows)) = rows.split_first().filter(|(_, rows)| !rows.is_empty()) else {
        return Err("`chart` takes a header row and at least one row of values".into());
    };
    let header = header.as_array().ok_or("a chart's header is an array of names")?;
    let series: Vec<String> = header.iter().skip(1).map(cell_text).collect();
    let mut table = Table { series, labels: Vec::new(), values: Vec::new() };
    for row in rows {
        let row = row.as_array().ok_or("each chart row is an array")?;
        table.labels.push(row.first().map(cell_text).unwrap_or_default());
        let mut values = Vec::new();
        for index in 1..=table.series.len() {
            values.push(match row.get(index) {
                None | Some(Value::Null) => 0.0,
                Some(value) => value.as_f64().ok_or_else(|| format!("chart values are numbers, not {value}"))?,
            });
        }
        table.values.push(values);
    }
    Ok(table)
}

fn cell_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// Vertical columns, series stacked, on a y axis with four ticks.
fn columns(table: &Table, width: usize) -> Vec<Line> {
    let top = nice_step(largest_total(table) / 4.0) * 4.0;
    let axis = number(top).len() + 1;
    let slot = width.saturating_sub(axis + 1) / table.labels.len();
    if slot < 2 {
        return bars(table, width);
    }
    let bar = slot - (slot / 3).max(1);
    let mut lines: Vec<Line> = legend(table).into_iter().map(|line| [vec![span(" ".repeat(axis + 1), PLAIN)], line].concat()).collect();
    let ends: Vec<Vec<usize>> = table.values.iter().map(|values| stacked_ends(values, top, COLUMN_HEIGHT)).collect();
    for row in (0..COLUMN_HEIGHT).rev() {
        let tick = (row + 1).is_multiple_of(COLUMN_HEIGHT / 4);
        let tick_label = if tick { number((row + 1) as f64 * top / COLUMN_HEIGHT as f64) } else { String::new() };
        let mut line = vec![span(format!("{tick_label:>0$} ", axis - 1), MUTED), span(if tick { "┤" } else { "│" }, AXIS)];
        for ends in &ends {
            let (glyph, style) = cell(ends, row * 8, &LOWER_EIGHTHS);
            line.push(span(" ".repeat(slot - bar), PLAIN));
            line.push(span(glyph.to_string().repeat(bar), style));
        }
        lines.push(line);
    }
    lines.push(vec![span(format!("{:>1$} ", "0", axis - 1), MUTED), span(format!("┴{}", "─".repeat(table.labels.len() * slot)), AXIS)]);
    // Every label would overlap its neighbours when the labels are wider than a column, so only every n-th shows.
    let longest = table.labels.iter().map(|label| self::width(label)).max().unwrap_or(0);
    let mut labels = String::new();
    let mut used = 0;
    for (index, label) in table.labels.iter().enumerate().step_by((longest + 1).div_ceil(slot)) {
        let at = axis + 1 + index * slot + slot - bar;
        labels.push_str(&" ".repeat(at - used));
        labels.push_str(label);
        used = at + self::width(label);
    }
    lines.push(vec![span(labels, MUTED)]);
    lines
}

/// One horizontal bar per label, series stacked, its total after it.
fn bars(table: &Table, width: usize) -> Vec<Line> {
    let top = largest_total(table).max(f64::MIN_POSITIVE);
    let label_width = table.labels.iter().map(|label| self::width(label)).max().unwrap_or(0).min(width / 3);
    let totals: Vec<String> = table.values.iter().map(|values| number(values.iter().sum())).collect();
    let total_width = totals.iter().map(String::len).max().unwrap_or(0);
    let cells = width.saturating_sub(label_width + total_width + 2);
    let mut lines: Vec<Line> = legend(table).into_iter().collect();
    for ((label, values), total) in table.labels.iter().zip(&table.values).zip(totals) {
        let ends = stacked_ends(values, top, cells);
        let mut line = truncate(&vec![span(label, PLAIN)], label_width);
        line.push(span(" ".repeat(label_width - line_width(&line) + 1), PLAIN));
        for index in 0..ends.last().copied().unwrap_or(0).div_ceil(8) {
            let (glyph, style) = cell(&ends, index * 8, &LEFT_EIGHTHS);
            match line.last_mut() {
                Some(last) if last.style == style => last.text.push(glyph),
                _ => line.push(span(glyph.to_string(), style)),
            }
        }
        line.push(span(format!(" {total}"), MUTED));
        lines.push(line);
    }
    lines
}

/// `■ Person   ■ AI agent`, or nothing for a single series.
fn legend(table: &Table) -> Option<Line> {
    if table.series.len() < 2 {
        return None;
    }
    let mut line = Vec::new();
    for (index, name) in table.series.iter().enumerate() {
        if index > 0 {
            line.push(span("   ", PLAIN));
        }
        line.push(span("■ ", Style::fg(SERIES[index % SERIES.len()])));
        line.push(span(name, PLAIN));
    }
    Some(line)
}

fn largest_total(table: &Table) -> f64 {
    table.values.iter().map(|values| values.iter().map(|value| value.max(0.0)).sum()).fold(0.0, f64::max)
}

/// Where each stacked series ends, in eighths of a cell, on a bar `cells` long that `top` fills.
fn stacked_ends(values: &[f64], top: f64, cells: usize) -> Vec<usize> {
    let mut total = 0.0;
    values
        .iter()
        .map(|value| {
            total += value.max(0.0);
            (total / top * (cells * 8) as f64).round() as usize
        })
        .collect()
}

/// The cell over eighths `from..from + 8`: the series at its start, as many eighths as that series fills, drawn over
/// the series at its end as the background. `glyphs[n]` fills n eighths from the bar's base.
fn cell(ends: &[usize], from: usize, glyphs: &[char; 9]) -> (char, Style) {
    let series_at = |eighth: usize| ends.iter().position(|end| *end > eighth);
    let Some(low) = series_at(from) else { return (' ', PLAIN) };
    let style = Style::fg(SERIES[low % SERIES.len()]);
    match series_at(from + 7) {
        Some(high) if high == low => (glyphs[8], style),
        Some(high) => (glyphs[ends[low] - from], style.on(SERIES[high % SERIES.len()])),
        None => (glyphs[ends[low] - from], style),
    }
}

/// The smallest of 1, 2, 2.5 and 5 times a power of ten that is at least `at_least`.
fn nice_step(at_least: f64) -> f64 {
    if at_least <= 0.0 {
        return 1.0;
    }
    let power = 10f64.powf(at_least.log10().floor());
    [1.0, 2.0, 2.5, 5.0, 10.0].into_iter().map(|multiple| multiple * power).find(|step| *step >= at_least).unwrap_or(10.0 * power)
}

/// `n` with thousands separators and at most two decimals: `8,000`, `0.25`.
fn number(n: f64) -> String {
    let text = format!("{n:.2}");
    let (whole, fraction) = text.split_once('.').unwrap_or((&text, ""));
    let (sign, digits) = whole.strip_prefix('-').map_or(("", whole), |digits| ("-", digits));
    let mut grouped = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    let fraction = fraction.trim_end_matches('0');
    if fraction.is_empty() { format!("{sign}{grouped}") } else { format!("{sign}{grouped}.{fraction}") }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn text(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|l| l.iter().map(|s| s.text.as_str()).collect::<String>().trim_end().to_string()).collect()
    }

    #[test]
    #[rustfmt::skip]
    fn stacked_columns_split_a_cell_between_two_series() {
        let lines = render(&json!({"chart": [["day", "a", "b"], ["x", 2.75, 1.25], ["y", 1, 0]]}), 12).unwrap();
        assert_eq!(text(&lines), [
            "   ■ a   ■ b",
            "4 ┤ ███",
            "  │ ███",
            "3 ┤ ▄▄▄",
            "  │ ███",
            "2 ┤ ███",
            "  │ ███",
            "1 ┤ ███ ███",
            "  │ ███ ███",
            "0 ┴────────",
            "    x   y",
        ]);
        let split = lines[3].iter().find(|s| s.text == "▄▄▄").unwrap();
        assert_eq!(split.style, Style::fg(SERIES[0]).on(SERIES[1]), "series a fills half the cell over series b");
    }

    #[test]
    fn bars_end_in_an_eighth_of_a_cell_and_their_total() {
        let lines = render(&json!({"chart": [["lang", "n"], ["Rust", 3], ["TypeScript", 1]], "form": "bars"}), 30).unwrap();
        assert_eq!(text(&lines), ["Rust       █████████████████ 3", "TypeScript █████▋ 1"]);
        let narrow = render(&json!({"chart": [["lang", "n"], ["Rust", 3], ["TypeScript", 1]]}), 6).unwrap();
        assert_eq!(text(&narrow), ["R… █ 3", "T… ▍ 1"], "columns too narrow for their labels draw as bars");
    }

    #[test]
    fn a_row_too_narrow_for_its_columns_stacks_them() {
        let node = json!({"title": "Rollout", "row": [{"stat": 42914, "label": "Downloads", "delta": "+18%"}, {"text": "**done**"}]});
        assert_eq!(text(&render(&node, 45).unwrap()), ["Rollout", "  42,914                 done", "  Downloads  +18%"]);
        assert_eq!(text(&render(&node, 44).unwrap()), ["Rollout", "  42,914", "  Downloads  +18%", "", "  done"]);
    }

    #[test]
    fn progress_fills_to_an_eighth_of_a_cell() {
        assert_eq!(text(&render(&json!({"progress": 0.53, "label": "Migrated"}), 37).unwrap()), ["Migrated    ██████████▋░░░░░░░░░  53%"]);
    }

    #[test]
    fn a_ui_fence_draws_and_falls_back_to_code_when_it_cannot() {
        assert_eq!(text(&markdown::render("```ui\n{\"stat\": 7}\n```", 20)), [" 7"]);
        assert_eq!(text(&markdown::render("```ui\n{\"stat\": 7\n```", 14)), [" ── ui ───────", " {\"stat\": 7", " ─────────────"], "JSON cut off mid-stream");
        assert_eq!(text(&markdown::render("```ui\n{\"map\": 1}\n```", 40)).last().unwrap(), " ui block: unknown node {\"map\":1}");
    }

    #[test]
    fn numbers_group_thousands_and_keep_two_decimals() {
        assert_eq!([8000.0, 1_234_567.0, 0.25, -1500.5, 2.0 / 3.0].map(number), ["8,000", "1,234,567", "0.25", "-1,500.5", "0.67"]);
    }
}
