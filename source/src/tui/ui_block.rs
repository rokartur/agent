use serde_json::Value;

use super::markdown;
use super::render::{BRIGHT, DIM, Line, MUTED, PLAIN, Span, Style, line_width, span, truncate, width};
use super::{ERROR, OK};

/// 256-color, so a cell can draw one series as foreground over the next as background.
const SERIES: [u8; 6] = [75, 179, 78, 141, 203, 80];
/// A heatmap's five levels, empty to largest.
const HEAT: [u8; 5] = [236, 22, 28, 34, 40];
const AXIS: Style = Style::rgb(0x555555);
const LOWER_EIGHTHS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
const LEFT_EIGHTHS: [char; 9] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];
/// The bit of each dot in a braille cell, by row and column.
const BRAILLE_DOTS: [[u8; 2]; 4] = [[0x01, 0x08], [0x02, 0x10], [0x04, 0x20], [0x40, 0x80]];
const COLUMN_HEIGHT: usize = 8;
const ROW_GAP: usize = 3;
const MIN_ROW_COLUMN: usize = 20;
const LABEL_WIDTH: usize = 12;
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
    if let Some(values) = node.get("spark") {
        return spark(node, values, width);
    }
    if let Some(rows) = node.get("timeline") {
        return timeline(rows, width);
    }
    if let Some(rows) = node.get("chart") {
        let table = table(rows)?;
        return match node.get("form").and_then(Value::as_str).unwrap_or("columns") {
            "columns" => Ok(columns(&table, width)),
            "bars" => Ok(bars(&table, width)),
            "line" => Ok(line(&table, width)),
            "heatmap" => Ok(heatmap(&table, width)),
            form => Err(format!("unknown chart form `{form}`, use `columns`, `bars`, `line` or `heatmap`")),
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
    let label = label(node);
    let cells = width.saturating_sub(self::width(&label.text) + 5).min(MAX_PROGRESS_BAR);
    let eighths = (fraction * (cells * 8) as f64).round() as usize;
    let mut bar = "█".repeat(eighths / 8);
    if !eighths.is_multiple_of(8) {
        bar.push(LEFT_EIGHTHS[eighths % 8]);
    }
    let empty = cells - eighths.div_ceil(8);
    let percent = format!("{}%", (fraction * 100.0).round());
    Ok(vec![vec![label, span(bar, Style::fg(SERIES[0])), span("░".repeat(empty), DIM), span(format!("{percent:>5}"), BRIGHT)]])
}

/// `p50         ▂▃▅▇▆▂ 12`: a cell per value from the lowest to the highest, the newest that fit, the last value after.
fn spark(node: &Value, values: &Value, width: usize) -> Result<Vec<Line>, String> {
    let mut numbers = Vec::new();
    for value in values.as_array().ok_or("`spark` takes an array of numbers")? {
        numbers.push(value.as_f64().ok_or_else(|| format!("spark values are numbers, not {value}"))?);
    }
    let Some(last) = numbers.last() else { return Err("`spark` takes at least one number".into()) };
    let label = label(node);
    let last = format!(" {}", number(*last));
    let cells = width.saturating_sub(self::width(&label.text) + last.len());
    let shown = &numbers[numbers.len().saturating_sub(cells)..];
    let low = shown.iter().copied().fold(f64::INFINITY, f64::min);
    let high = shown.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let range = (high - low).max(f64::MIN_POSITIVE);
    let bars: String = shown.iter().map(|value| LOWER_EIGHTHS[1 + ((value - low) / range * 7.0).round() as usize]).collect();
    Ok(vec![vec![label, span(bars, Style::fg(SERIES[0])), span(last, BRIGHT)]])
}

/// A progress bar's or a sparkline's label, padded so a column of them lines up.
fn label(node: &Value) -> Span {
    let label = node.get("label").and_then(Value::as_str).unwrap_or_default();
    let padded = if label.is_empty() { 0 } else { LABEL_WIDTH.max(width(label) + 2) };
    span(format!("{label}{}", " ".repeat(padded - width(label))), MUTED)
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
    let top = nice_step(largest_total(table, 1.0) / 4.0) * 4.0;
    let axis = axis_width(0.0, top);
    let slot = width.saturating_sub(axis + 1) / table.labels.len();
    if slot < 2 || table.values.iter().flatten().any(|value| *value < 0.0) {
        return bars(table, width);
    }
    let bar = slot - (slot / 3).max(1);
    let mut lines = legend_over_plot(table, axis);
    let ends: Vec<Vec<usize>> = table.values.iter().map(|values| stacked_ends(values, 1.0, top, COLUMN_HEIGHT)).collect();
    for row in (0..COLUMN_HEIGHT).rev() {
        let mut line = y_axis(row, 0.0, top, axis);
        for ends in &ends {
            let (filled, style) = cell(ends, row * 8, false);
            line.push(span(" ".repeat(slot - bar), PLAIN));
            line.push(span(LOWER_EIGHTHS[filled].to_string().repeat(bar), style));
        }
        lines.push(line);
    }
    lines.push(x_axis(0.0, axis, table.labels.len() * slot));
    lines.push(x_labels(&table.labels, slot, |index| axis + 1 + index * slot + slot - bar));
    lines
}

/// One horizontal bar per label, series stacked, negative values left of a shared zero, the total after it.
fn bars(table: &Table, width: usize) -> Vec<Line> {
    let (left_top, right_top) = (largest_total(table, -1.0), largest_total(table, 1.0));
    let top = (left_top + right_top).max(f64::MIN_POSITIVE);
    let label_width = table.labels.iter().map(|label| self::width(label)).max().unwrap_or(0).min(width / 3);
    let totals: Vec<String> = table.values.iter().map(|values| number(values.iter().sum())).collect();
    let total_width = totals.iter().map(String::len).max().unwrap_or(0);
    let cells = width.saturating_sub(label_width + total_width + 2);
    let left_cells = ((left_top / top * (cells * 8) as f64).round() as usize).div_ceil(8);
    let mut lines: Vec<Line> = legend(table).into_iter().collect();
    for ((label, values), total) in table.labels.iter().zip(&table.values).zip(totals) {
        let left = stacked_ends(values, -1.0, top, cells);
        let right = stacked_ends(values, 1.0, top, cells);
        let mut line = truncate(&vec![span(label, PLAIN)], label_width);
        line.push(span(" ".repeat(label_width - line_width(&line) + 1), PLAIN));
        for index in (0..left_cells).rev() {
            let (filled, style) = cell(&left, index * 8, true);
            push_glyph(&mut line, LEFT_EIGHTHS[filled], style);
        }
        for index in 0..right.last().copied().unwrap_or(0).div_ceil(8) {
            let (filled, style) = cell(&right, index * 8, false);
            push_glyph(&mut line, LEFT_EIGHTHS[filled], style);
        }
        line.push(span(format!(" {total}"), MUTED));
        lines.push(line);
    }
    lines
}

/// Each series as a braille line, 2 by 4 dots a cell, on a y axis fitted to the values. A cell two lines cross takes
/// the later series' color.
fn line(table: &Table, width: usize) -> Vec<Line> {
    let low = table.values.iter().flatten().copied().fold(f64::INFINITY, f64::min);
    let high = table.values.iter().flatten().copied().fold(f64::NEG_INFINITY, f64::max);
    let (bottom, top) = fitted_axis(low, high);
    let axis = axis_width(bottom, top);
    let cells = width.saturating_sub(axis + 1);
    if cells < 2 {
        return bars(table, width);
    }
    let (dots_wide, dots_high) = (cells * 2, COLUMN_HEIGHT * 4);
    let points = table.labels.len();
    let x = |index: usize| if points == 1 { 0 } else { index * (dots_wide - 1) / (points - 1) };
    let y = |value: f64| dots_high - 1 - ((value - bottom) / (top - bottom) * (dots_high - 1) as f64).round() as usize;
    let mut grid = vec![vec![(0u8, 0usize); cells]; COLUMN_HEIGHT];
    for series in 0..table.series.len() {
        let mut previous = None;
        for (index, values) in table.values.iter().enumerate() {
            let point = (x(index), y(values[series]));
            let (from_x, from_y) = previous.unwrap_or(point);
            let steps = point.0.abs_diff(from_x).max(point.1.abs_diff(from_y)).max(1);
            for step in 0..=steps {
                let along = step as f64 / steps as f64;
                let dot_x = (from_x as f64 + (point.0 as f64 - from_x as f64) * along).round() as usize;
                let dot_y = (from_y as f64 + (point.1 as f64 - from_y as f64) * along).round() as usize;
                let cell = &mut grid[dot_y / 4][dot_x / 2];
                cell.0 |= BRAILLE_DOTS[dot_y % 4][dot_x % 2];
                cell.1 = series;
            }
            previous = Some(point);
        }
    }
    let mut lines = legend_over_plot(table, axis);
    for (row, cells) in grid.iter().enumerate() {
        let mut line = y_axis(COLUMN_HEIGHT - 1 - row, bottom, top, axis);
        for (dots, series) in cells {
            let glyph = char::from_u32(0x2800 + u32::from(*dots)).expect("U+2800 to U+28FF are braille");
            push_glyph(&mut line, glyph, if *dots == 0 { PLAIN } else { Style::fg(SERIES[series % SERIES.len()]) });
        }
        lines.push(line);
    }
    lines.push(x_axis(bottom, axis, cells));
    lines.push(x_labels(&table.labels, cells / points.saturating_sub(1).max(1), |index| axis + 1 + x(index) / 2));
    lines
}

/// The table as a grid, a cell per value, brighter as it nears the largest.
fn heatmap(table: &Table, width: usize) -> Vec<Line> {
    let largest = table.values.iter().flatten().copied().fold(0.0, f64::max);
    let label_width = table.labels.iter().map(|label| self::width(label)).max().unwrap_or(0).min(width / 3);
    let longest_name = table.series.iter().map(|name| self::width(name)).max().unwrap_or(0);
    let room = width.saturating_sub(label_width + 1) / table.series.len().max(1);
    let cell = longest_name.max(2).min(room.saturating_sub(1)).max(1);
    let mut header = vec![span(" ".repeat(label_width + 1), PLAIN)];
    for name in &table.series {
        let mut name = truncate(&vec![span(name, MUTED)], cell);
        name.push(span(" ".repeat(cell + 1 - line_width(&name)), PLAIN));
        header.extend(name);
    }
    let mut lines = vec![header];
    for (label, values) in table.labels.iter().zip(&table.values) {
        let mut line = truncate(&vec![span(label, PLAIN)], label_width);
        line.push(span(" ".repeat(label_width - line_width(&line) + 1), PLAIN));
        for value in values {
            // shortcut: zero and negative values draw as empty, a diverging color scale would show them.
            let level = if *value <= 0.0 { 0 } else { (value / largest * 4.0).ceil() as usize };
            line.push(span("█".repeat(cell), Style::fg(HEAT[level])));
            line.push(span(" ", PLAIN));
        }
        lines.push(line);
    }
    let mut scale = vec![span(format!("{}0 ", " ".repeat(label_width + 1)), MUTED)];
    scale.extend(HEAT.map(|color| span("■", Style::fg(color))));
    scale.push(span(format!(" {}", number(largest)), MUTED));
    lines.push(scale);
    lines
}

/// One bar per row from its start to its end on an axis every row shares, its length after it.
fn timeline(rows: &Value, width: usize) -> Result<Vec<Line>, String> {
    let mut phases = Vec::new();
    for row in rows.as_array().ok_or("`timeline` takes an array of [label, start, end] rows")? {
        let Some([label, start, end]) = row.as_array().map(Vec::as_slice) else {
            return Err(format!("a timeline row is [label, start, end], not {row}"));
        };
        let (Some(start), Some(end)) = (start.as_f64(), end.as_f64()) else {
            return Err(format!("a timeline row starts and ends at numbers, not {row}"));
        };
        if end < start {
            return Err(format!("a timeline row ends before it starts: {row}"));
        }
        phases.push((cell_text(label), start, end));
    }
    if phases.is_empty() {
        return Err("`timeline` takes at least one row".into());
    }
    let low = phases.iter().map(|phase| phase.1).fold(f64::INFINITY, f64::min);
    let high = phases.iter().map(|phase| phase.2).fold(f64::NEG_INFINITY, f64::max);
    let length = (high - low).max(f64::MIN_POSITIVE);
    let label_width = phases.iter().map(|(label, ..)| self::width(label)).max().unwrap_or(0).min(width / 3);
    let lengths: Vec<String> = phases.iter().map(|(_, start, end)| number(end - start)).collect();
    let length_width = lengths.iter().map(String::len).max().unwrap_or(0);
    let cells = width.saturating_sub(label_width + length_width + 2);
    let eighth = |value: f64| ((value - low) / length * (cells * 8) as f64).round() as usize;
    let mut lines = Vec::new();
    for (index, ((label, start, end), length)) in phases.iter().zip(lengths).enumerate() {
        let style = Style::fg(SERIES[index % SERIES.len()]);
        let to = eighth(*end);
        let from = eighth(*start).min(to.saturating_sub(1));
        let to = to.max(from + 1);
        let mut line = truncate(&vec![span(label, PLAIN)], label_width);
        line.push(span(" ".repeat(label_width - line_width(&line) + 1), PLAIN));
        for column in 0..to.div_ceil(8) {
            let (cell_start, cell_end) = (column * 8, column * 8 + 8);
            if to <= cell_start || from >= cell_end {
                push_glyph(&mut line, ' ', PLAIN);
            } else if from <= cell_start {
                push_glyph(&mut line, LEFT_EIGHTHS[to.min(cell_end) - cell_start], style);
            } else {
                // Starts inside the cell: reversed, the glyph's eighths are the gap before the bar, which runs to the cell's end.
                push_glyph(&mut line, LEFT_EIGHTHS[from - cell_start], Style { reverse: true, ..style });
            }
        }
        line.push(span(format!(" {length}"), MUTED));
        lines.push(line);
    }
    let first = number(low);
    let last = format!("{:>1$}", number(high), cells.saturating_sub(first.len()));
    lines.push(vec![span(format!("{} {first}{last}", " ".repeat(label_width)), MUTED)]);
    Ok(lines)
}

/// Appends `glyph`, into the last span when it has the same style.
fn push_glyph(line: &mut Line, glyph: char, style: Style) {
    match line.last_mut() {
        Some(last) if last.style == style => last.text.push(glyph),
        _ => line.push(span(glyph.to_string(), style)),
    }
}

/// The legend, indented past a y axis `axis` wide.
fn legend_over_plot(table: &Table, axis: usize) -> Vec<Line> {
    legend(table).into_iter().map(|line| [vec![span(" ".repeat(axis + 1), PLAIN)], line].concat()).collect()
}

/// The width of a y axis's tick values, from `bottom` to `top` in quarters, and a space.
fn axis_width(bottom: f64, top: f64) -> usize {
    (0..=4).map(|quarter| number(bottom + quarter as f64 * (top - bottom) / 4.0).len()).max().unwrap_or(0) + 1
}

/// The y axis left of plot row `row`, counted up from the bottom: a tick and its value every quarter of the height.
fn y_axis(row: usize, bottom: f64, top: f64, axis: usize) -> Line {
    let tick = (row + 1).is_multiple_of(COLUMN_HEIGHT / 4);
    let value = if tick { number(bottom + (row + 1) as f64 * (top - bottom) / COLUMN_HEIGHT as f64) } else { String::new() };
    vec![span(format!("{value:>0$} ", axis - 1), MUTED), span(if tick { "┤" } else { "│" }, AXIS)]
}

/// The x axis under a plot `cells` wide, `bottom` at its corner.
fn x_axis(bottom: f64, axis: usize, cells: usize) -> Line {
    vec![span(format!("{:>1$} ", number(bottom), axis - 1), MUTED), span(format!("┴{}", "─".repeat(cells)), AXIS)]
}

/// The labels under a plot, each at the column `at` gives. Labels wider than the `gap` between two would overlap, so
/// only every n-th shows.
fn x_labels(labels: &[String], gap: usize, at: impl Fn(usize) -> usize) -> Line {
    let longest = labels.iter().map(|label| self::width(label)).max().unwrap_or(0);
    let mut text = String::new();
    let mut used = 0;
    for (index, label) in labels.iter().enumerate().step_by((longest + 1).div_ceil(gap.max(1))) {
        let at = at(index);
        text.push_str(&" ".repeat(at.saturating_sub(used)));
        text.push_str(label);
        used = at.max(used) + self::width(label);
    }
    vec![span(text, MUTED)]
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

/// The largest sum of one row's values on the `sign` side of zero, as a positive number.
fn largest_total(table: &Table, sign: f64) -> f64 {
    table.values.iter().map(|values| values.iter().map(|value| (value * sign).max(0.0)).sum()).fold(0.0, f64::max)
}

/// Where each series ends, stacking the values on the `sign` side of zero, in eighths of a cell, on a bar `cells`
/// long that `top` fills.
fn stacked_ends(values: &[f64], sign: f64, top: f64, cells: usize) -> Vec<usize> {
    let mut total = 0.0;
    values
        .iter()
        .map(|value| {
            total += (value * sign).max(0.0);
            (total / top * (cells * 8) as f64).round() as usize
        })
        .collect()
}

/// The cell over eighths `from..from + 8` counted from the bar's base: how many eighths the series at its start fills,
/// drawn over the series at its end as the background. A `mirrored` bar grows leftwards, so it returns the eighths
/// `LEFT_EIGHTHS` must fill from the cell's left edge.
fn cell(ends: &[usize], from: usize, mirrored: bool) -> (usize, Style) {
    let series_at = |eighth: usize| ends.iter().position(|end| *end > eighth);
    let color = |series: usize| SERIES[series % SERIES.len()];
    let Some(low) = series_at(from) else { return (0, PLAIN) };
    let filled = ends[low].saturating_sub(from);
    match series_at(from + 7) {
        Some(high) if high == low => (8, Style::fg(color(low))),
        Some(high) if mirrored => (8 - filled, Style::fg(color(high)).on(color(low))),
        Some(high) => (filled, Style::fg(color(low)).on(color(high))),
        // Reversed, the glyph's eighths show the terminal's background, the rest the series.
        None if mirrored => (8 - filled, Style { reverse: true, ..Style::fg(color(low)) }),
        None => (filled, Style::fg(color(low))),
    }
}

/// A y axis from `bottom` to `top`, four equal steps of a `nice_step` that hold every value from `low` to `high`.
fn fitted_axis(low: f64, high: f64) -> (f64, f64) {
    let mut step = nice_step((high - low) / 4.0);
    // A range wider than f64::MAX makes the step infinite and the bottom NaN, which no step would ever fit.
    while step.is_finite() {
        let bottom = (low / step).floor() * step;
        if bottom + 4.0 * step >= high {
            return (bottom, bottom + 4.0 * step);
        }
        step = nice_step(step * 1.01);
    }
    (low, high)
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

    #[test]
    fn spark_spans_lowest_to_highest_and_keeps_the_newest_values_that_fit() {
        assert_eq!(text(&render(&json!({"spark": [1, 5, 3, 9], "label": "p50"}), 30).unwrap()), ["p50         ▁▅▃█ 9"]);
        assert_eq!(text(&render(&json!({"spark": [1, 5, 3, 9]}), 4).unwrap()), ["▁█ 9"]);
    }

    #[test]
    fn negative_values_grow_left_of_a_shared_zero() {
        let chart = json!({"chart": [["t", "x", "y"], ["a", 3, -1], ["c", -0.25, 0]], "form": "bars"});
        let lines = render(&chart, 18).unwrap();
        assert_eq!(text(&lines), ["■ x   ■ y", "a ▌█████████▌ 2", "c   ▍ -0.25"]);
        let edge = &lines[2][2];
        assert_eq!((edge.text.as_str(), edge.style), ("▍", Style { reverse: true, ..Style::fg(SERIES[0]) }), "reversed, the bar is the glyph's right side");
        let columns = json!({"chart": [["t", "n"], ["a", 2], ["b", -1]]});
        assert_eq!(text(&render(&columns, 20).unwrap()), ["a      ██████████ 2", "b █████ -1"], "columns with a negative value draw as bars");
    }

    #[test]
    fn a_y_axis_fits_its_widest_tick() {
        let lines = render(&json!({"chart": [["x", "n"], ["a", 10], ["b", 3]]}), 20).unwrap();
        assert_eq!(text(&lines)[..3], [" 10 ┤  █████", "    │  █████", "7.5 ┤  █████"]);
    }

    #[test]
    #[rustfmt::skip]
    fn line_draws_each_series_in_braille_on_a_fitted_axis() {
        let lines = render(&json!({"chart": [["x", "a", "b"], ["1", 0, 3], ["2", 4, 1], ["3", 2, 2]], "form": "line"}), 20).unwrap();
        assert_eq!(text(&lines), [
            "   ■ a   ■ b",
            "4 ┤⠀⠀⠀⠀⠀⠀⠀⡰⠑⢄⠀⠀⠀⠀⠀⠀⠀",
            "  │⠀⠀⠀⠀⠀⠀⡰⠁⠀⠀⠉⠢⡀⠀⠀⠀⠀",
            "3 ┤⠑⢄⠀⠀⠀⡰⠁⠀⠀⠀⠀⠀⠈⠢⣀⠀⠀",
            "  │⠀⠀⠑⢄⡰⠁⠀⠀⠀⠀⠀⠀⠀⠀⠀⠑⣄",
            "2 ┤⠀⠀⠀⡰⠉⠢⡀⠀⠀⠀⠀⠀⣀⠤⠒⠉⠀",
            "  │⠀⠀⡰⠁⠀⠀⠈⠢⣀⠤⠒⠉⠀⠀⠀⠀⠀",
            "1 ┤⠀⡰⠁⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
            "  │⡰⠁⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
            "0 ┴─────────────────",
            "   1       2       3",
        ]);
        assert_eq!([fitted_axis(203.0, 251.0), fitted_axis(11.0, 19.0)], [(200.0, 280.0), (10.0, 20.0)], "a step of 2 from 10 stops at 18");
        assert_eq!(fitted_axis(-1e308, 1e308), (-1e308, 1e308), "a range past f64::MAX returns instead of looping");
    }

    #[test]
    fn heatmap_brightens_toward_the_largest_value() {
        let lines = render(&json!({"chart": [["day", "Mon", "Tue"], ["W1", 0, 4], ["W2", 2, 1]], "form": "heatmap"}), 30).unwrap();
        assert_eq!(text(&lines), ["   Mon Tue", "W1 ███ ███", "W2 ███ ███", "   0 ■■■■■ 4"]);
        let colors = |line: &Line| line.iter().filter(|s| s.text == "███").map(|s| s.style).collect::<Vec<_>>();
        assert_eq!(colors(&lines[1]), [Style::fg(HEAT[0]), Style::fg(HEAT[4])]);
        assert_eq!(colors(&lines[2]), [Style::fg(HEAT[2]), Style::fg(HEAT[1])]);
    }

    #[test]
    fn timeline_bars_start_and_end_in_an_eighth_of_a_cell() {
        let lines = render(&json!({"timeline": [["Build", 0, 4], ["Test", 4.5, 10], ["Ship", 10, 12]]}), 24).unwrap();
        assert_eq!(text(&lines), ["Build ████▋ 4", "Test       ▎█████▋ 5.5", "Ship             ▋██ 2", "      0           12"]);
        assert!(lines[1][2].style.reverse, "a bar starting inside a cell draws the gap before it");
        assert_eq!(render(&json!({"timeline": [["x", 3, 1]]}), 20).unwrap_err(), "a timeline row ends before it starts: [\"x\",3,1]");
    }
}
