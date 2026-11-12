use super::*;

pub(super) fn update_todos_lines(tasks: &[Task], width: usize) -> Vec<Line> {
    let mut lines = vec![vec![span("✓", OK), span(" Update Todos", TOOL_NAME)]];
    for (index, task) in tasks.iter().enumerate() {
        let gutter = if index == 0 { "  ⎿  " } else { "     " };
        let mut row = vec![span(gutter, MUTED)];
        row.extend(todo_spans(task.status, &task.subject));
        lines.push(truncate(&row, width));
    }
    lines
}

pub(super) fn edits_todos(name: &str, input: &Value) -> bool {
    name == "Todo" && !matches!(input["action"].as_str(), Some("list" | "get"))
}

/// The tool call glyphs: `✓` done with its text struck through, `•` in progress and bold, `○` pending and dim.
pub(super) fn todo_spans(status: Status, text: &str) -> Line {
    match status {
        Status::Completed | Status::Deleted => vec![span("✓ ", OK), span(text, Style { strikethrough: true, ..DIM })],
        Status::InProgress => vec![span(format!("• {text}"), Style { bold: true, ..BRIGHT })],
        Status::Pending => vec![span("○ ", DIM), span(text, PLAIN)],
    }
}

pub(super) fn todo_style(status: Status) -> Style {
    match status {
        Status::InProgress => BRIGHT,
        Status::Completed | Status::Deleted => DIM,
        Status::Pending => PLAIN,
    }
}

/// A prompt: `❯ ` and the text, later rows indented under it. Both leads carry PROMPT_LEAD, which copying a selection reads.
pub(super) fn prompt_rows(text: &str, style: Style, width: usize) -> Vec<Line> {
    let mut rows = wrap(&[span(text, style)], width, &vec![span("  ", PROMPT_LEAD)]);
    rows[0][0] = span("❯ ", PROMPT_LEAD);
    rows
}

pub(super) fn render_item(item: &Item, width: usize, expanded: bool, cwd: &Path) -> Vec<Line> {
    match item {
        Item::Lines(lines) => lines.clone(),
        Item::System { source, text, tone } => {
            let (icon, style) = match tone {
                Tone::Info => ("ℹ", SYSTEM),
                Tone::Warn => ("!", WARN),
                Tone::Error => ("✗", ERROR),
            };
            system_lines(icon, source, text, style, width)
        }
        Item::Subagent(run) => subagent_done_lines(run, width),
        Item::Error(text) => system_lines("✗", "error", text, ERROR, width),
        Item::User(text) => prompt_rows(text, BRIGHT, width).into_iter().map(|row| band(row, width)).collect(),
        Item::Images(tags) => tags.iter().map(|tag| image_row(tag)).collect(),
        Item::Assistant { text, thinking } => {
            let mut lines = Vec::new();
            if expanded && !thinking.trim().is_empty() {
                lines.extend(wrap(&[span(thinking.trim(), MUTED.italic())], width, &vec![span(" ", PLAIN)]));
                if !text.trim().is_empty() {
                    lines.push(Vec::new());
                }
            }
            // One column of margin, matching the thinking text above.
            for line in markdown::render(&crate::agent::without_reminders(text), width.saturating_sub(1)) {
                let mut indented = vec![span(" ", PLAIN)];
                indented.extend(line);
                lines.push(indented);
            }
            lines
        }
        Item::Tool { name, input, result, progress, .. } => {
            let mut lines = render_tool(name, input, result.as_ref(), width, expanded, cwd);
            if let Some(progress) = progress.as_ref().filter(|_| result.is_none()) {
                lines.push(truncate(&vec![span("└ ", MUTED), span(progress, MUTED)], width));
            }
            lines
        }
        Item::Todos(tasks) => update_todos_lines(tasks, width),
        Item::Done { took, usage, total } => {
            let stats = format!(" ↑{} ↓{} · total {}", tokens(usage.input), tokens(usage.output), duration(*total));
            vec![vec![star(usize::MAX), span(format!(" {}", duration(*took)), MUTED), span(stats, DIM)]]
        }
    }
}

pub(super) fn image_row(tag: &str) -> Line {
    let Some((name, path)) = tag.split_once("] ") else { return vec![span("  └ ", MUTED), span(tag, PLAIN)] };
    vec![span("  └ ", MUTED), span(format!("{name}]"), PLAIN), span(format!(" {path}"), MUTED)]
}

pub(super) fn system_lines(icon: &str, source: &str, text: &str, style: Style, width: usize) -> Vec<Line> {
    let hang = vec![span("  ", PLAIN)];
    let mut lines = Vec::new();
    let first_line = if style == ERROR { ERROR } else { NOTICE_TEXT };
    for (index, line) in text.lines().enumerate() {
        let spans = if index == 0 {
            vec![span(icon, style), span(" ", PLAIN), span(format!("{source}:"), style), span(" ", PLAIN), span(line, first_line)]
        } else if let Some((label, description)) = line.split_once('\t') {
            // A `label\tdescription` row wraps its description under the description column.
            let column = vec![span(" ".repeat(3 + render::width(label)), PLAIN)];
            let mut rows = wrap(&[span(description, TOOL_ARGS)], width, &column);
            rows[0][0] = span(format!("  {label} "), TOOL_ARGS);
            lines.extend(rows);
            continue;
        } else {
            vec![span(line, TOOL_ARGS)]
        };
        lines.extend(wrap(&spans, width, &hang));
    }
    // Wrapped rows hang under the source; the icon row starts at column 0 like a tool row.
    if let Some(first) = lines.first_mut() {
        first.remove(0);
    }
    lines
}

pub(super) fn subagent_done_lines(run: &Run, width: usize) -> Vec<Line> {
    let elapsed = duration(run.elapsed());
    let state = run.state.lock().unwrap();
    let plural = if state.tool_calls == 1 { "" } else { "s" };
    let mut stats = format!(" · {} tool{plural} · {elapsed}", state.tool_calls);
    if state.usage != Usage::default() {
        stats += &format!(" · {}", usage::brief(state.usage));
    }
    let (icon, icon_style) = if matches!(state.result, Some(Err(_))) { ("✗", ERROR) } else { ("✓", OK) };
    let mut spans = vec![span(icon, icon_style), span(" ", PLAIN), span(&run.id, SYSTEM), span(" ", PLAIN), span(&run.label, TOOL_NAME), span(stats, TOOL_ARGS)];
    if let Some(Err(error)) = &state.result {
        spans.extend([span(" · ", TOOL_ARGS), span(error, ERROR)]);
    }
    let mut lines = wrap(&spans, width, &vec![span("  ", PLAIN)]);
    lines[0].remove(0);
    lines
}

pub(super) fn render_tool(name: &str, input: &Value, result: Option<&(String, bool)>, width: usize, expanded: bool, cwd: &Path) -> Vec<Line> {
    // The list itself replaces this row once the call ends; see show_todos.
    if result.is_none() && edits_todos(name, input) {
        return vec![vec![span("•", MUTED), span(" Update Todos", TOOL_NAME)]];
    }
    let edits_lines = matches!(name, "ReplaceLines" | "Edit" | "InsertLines");
    let (icon, icon_style) = match result {
        None => ("•", MUTED),
        Some((_, false)) => ("✓", OK),
        Some((_, true)) => ("✗", ERROR),
    };
    if let Some(answers) = result.filter(|_| name == "AskUserQuestion").and_then(|(output, _)| crate::tools::ask::answered(input, output)) {
        return answer_lines(icon, icon_style, &answers, width);
    }
    let header = match skill_name(name, input) {
        Some(skill) => {
            let hint = if expanded { "" } else { " (ctrl+o to expand)" };
            vec![span(icon, icon_style), span(" Skill ", TOOL_NAME), span(skill, TOOL_ARGS), span(hint, DIM)]
        }
        None => {
            let (tool, server) = match mcp_tool(name) {
                Some((server, tool)) => (tool, Some(server)),
                None => (name.to_string(), None),
            };
            let mut header = vec![span(icon, icon_style), span(format!(" {tool}"), TOOL_NAME)];
            let args = tool_args(name, input, cwd);
            if !args.is_empty() {
                header.push(span(format!(" {args}"), TOOL_ARGS));
            }
            if let Some(server) = server {
                header.push(span(format!(" MCP: {server}"), DIM));
            }
            // The diff comes first; an LSP report or a truncation note follows after a blank line.
            if let Some((output, false)) = result
                && edits_lines
            {
                header.push(span(" ", PLAIN));
                header.extend(diff_stat(output.split("\n\n").next().unwrap_or_default()));
            } else if !expanded {
                header.extend(result_note(result));
            }
            header
        }
    };
    if !expanded {
        return vec![truncate(&header, width)];
    }
    let mut lines = vec![header];
    let Some((output, is_error)) = result else { return lines };
    let style = if *is_error { ERROR } else { TOOL_ARGS };
    let pretty = serde_json::from_str::<Value>(output).ok().filter(|v| v.is_object() || v.is_array()).and_then(|v| serde_json::to_string_pretty(&v).ok());
    let output = pretty.as_deref().unwrap_or(output);
    let shown = output.lines().take(TOOL_OUTPUT_LINES).map(|line| line.replace('\t', "    "));
    let is_diff = !is_error && edits_lines;
    for (index, line) in shown.enumerate() {
        let lead = if index == 0 { "└ " } else { "  " };
        let style = match line.chars().next() {
            Some('+') if is_diff => OK,
            Some('-') if is_diff => ERROR,
            _ => style,
        };
        lines.push(truncate(&vec![span(lead, MUTED), span(line, style)], width));
    }
    let hidden = output.lines().count().saturating_sub(TOOL_OUTPUT_LINES);
    if hidden > 0 {
        lines.push(vec![span(format!("  … {hidden} more lines"), MUTED)]);
    }
    lines
}

/// Collapsed, two or more tool calls in a row are one row. Maps each of their entries to the lines it draws: the row for the first, none for the rest.
pub(super) fn folded_tools(entries: &mut [Entry], width: usize, cwd: &Path, spinner: Option<&str>) -> HashMap<usize, Vec<Line>> {
    let mut folded = HashMap::new();
    let mut run: Vec<usize> = Vec::new();
    // The pass past the last entry ends the last run.
    for index in 0..=entries.len() {
        if let Some(entry) = entries.get_mut(index) {
            if folds_into_run(&entry.item) {
                run.push(index);
                continue;
            }
            // Collapsed thinking draws nothing, so it does not end the run.
            if entry.lines(width, false, cwd).is_empty() {
                continue;
            }
        }
        if run.len() > 1 {
            let calls: Vec<&Item> = run.iter().map(|&member| &entries[member].item).collect();
            folded.insert(run[0], tool_run_lines(&calls, width, cwd, spinner));
            folded.extend(run[1..].iter().map(|&member| (member, Vec::new())));
        }
        run.clear();
    }
    folded
}

/// A failed call, a question, a skill and a todo edit keep their own rows.
pub(super) fn folds_into_run(item: &Item) -> bool {
    let Item::Tool { name, input, result, .. } = item else { return false };
    !matches!(result, Some((_, true))) && name != "AskUserQuestion" && !edits_todos(name, input) && skill_name(name, input).is_none()
}

/// "Read 3 files, searched for 2 patterns, running 1 command… (ctrl+o to expand)", then the call still running under a hook.
pub(super) fn tool_run_lines(calls: &[&Item], width: usize, cwd: &Path, spinner: Option<&str>) -> Vec<Line> {
    const KINDS: [(&str, &str, &str); 5] =
        [("read", "reading", "file"), ("searched for", "searching for", "pattern"), ("ran", "running", "command"), ("edited", "editing", "file"), ("called", "calling", "tool")];
    let mut counts = [0; 5];
    let mut running = [false; 5];
    let mut files = Vec::new();
    let mut unfinished = None;
    for call in calls {
        let Item::Tool { name, input, result, progress, started, .. } = call else { unreachable!("folds_into_run checks the item") };
        let kind = match name.as_str() {
            "Read" => 0,
            "Grep" => 1,
            "Bash" => 2,
            "Write" | "ReplaceLines" | "Edit" | "InsertLines" => 3,
            _ => 4,
        };
        if result.is_none() {
            running[kind] = true;
            unfinished = Some((name, input, progress, started));
        }
        if KINDS[kind].2 != "file" {
            counts[kind] += 1;
            continue;
        }
        // A file read or edited twice counts once.
        let paths = input["paths"].as_array().into_iter().flatten().filter_map(|entry| entry.as_str().or(entry["file_path"].as_str())).chain(input["file_path"].as_str());
        for path in paths {
            if !files.contains(&(kind, path)) {
                files.push((kind, path));
                counts[kind] += 1;
            }
        }
    }
    let mut parts = Vec::new();
    for (kind, (done, doing, unit)) in KINDS.into_iter().enumerate() {
        if counts[kind] > 0 {
            let verb = if running[kind] { doing } else { done };
            let plural = if counts[kind] == 1 { "" } else { "s" };
            parts.push(format!("{verb} {} {unit}{plural}", counts[kind]));
        }
    }
    let mut summary = parts.join(", ");
    // Empty when the only calls are file calls without a path.
    if let Some(first) = summary.get_mut(..1) {
        first.make_ascii_uppercase();
    }
    let Some((name, input, progress, started)) = unfinished else {
        return vec![truncate(&vec![span("✓", OK), span(format!(" {summary}"), TOOL_NAME), span(" (ctrl+o to expand)", DIM)], width)];
    };
    let tool = mcp_tool(name).map_or(name.to_string(), |(_, tool)| tool);
    let about = progress.clone().unwrap_or_else(|| tool_args(name, input, cwd));
    let mut hook = truncate(&vec![span(format!("└ {tool} {about}"), MUTED)], width);
    if spinner.is_some() {
        hook.extend(running_seconds(*started));
    }
    vec![truncate(&vec![span(spinner.unwrap_or("•"), MUTED), span(format!(" {summary}…"), TOOL_NAME), span(" (ctrl+o to expand)", DIM)], width), hook]
}

/// What a collapsed call says about its result: an error's last line, a one-line result itself, or how many lines came back.
pub(super) fn result_note(result: Option<&(String, bool)>) -> Line {
    let Some((output, is_error)) = result else { return Line::new() };
    if *is_error {
        let Some(last) = output.lines().rfind(|line| !line.trim().is_empty()) else { return Line::new() };
        return vec![span(" · ", DIM), span(last.trim(), ERROR)];
    }
    let mut lines = output.lines().filter(|line| !line.trim().is_empty());
    match (lines.next(), lines.count()) {
        (None, _) => Line::new(),
        (Some(only), 0) => vec![span(format!(" · {}", only.trim()), DIM)],
        (Some(_), rest) => vec![span(format!(" · {} lines", rest + 1), DIM)],
    }
}

/// " · 4s" for a call that has run a second or longer; shorter calls would only flash it.
pub(super) fn running_seconds(started: Option<Instant>) -> Line {
    match started.map(|started| started.elapsed()) {
        Some(elapsed) if elapsed >= Duration::from_secs(1) => vec![span(format!(" · {}", duration(elapsed)), DIM)],
        _ => Line::new(),
    }
}

/// Each question, then its answer under a hook like an attached image's row.
pub(super) fn answer_lines(icon: &str, icon_style: Style, answers: &[(String, String)], width: usize) -> Vec<Line> {
    let mut lines = Vec::new();
    for (index, (question, answer)) in answers.iter().enumerate() {
        let mut spans = vec![span(question, TOOL_ARGS)];
        if index == 0 {
            spans.splice(0..0, [span(icon, icon_style), span(" ", PLAIN), span("AskUserQuestion ", TOOL_NAME)]);
        }
        let mut rows = wrap(&spans, width, &vec![span("  ", PLAIN)]);
        if index == 0 {
            rows[0].remove(0);
        }
        lines.extend(rows);
        let mut rows = wrap(&[span(answer, PLAIN)], width, &vec![span("    ", PLAIN)]);
        rows[0][0] = span("  └ ", MUTED);
        lines.extend(rows);
    }
    lines
}

/// `mcp__vexdock__get_service` as (`vexdock`, `GetService`).
pub(super) fn mcp_tool(name: &str) -> Option<(&str, String)> {
    let (server, tool) = name.strip_prefix("mcp__")?.split_once("__")?;
    let pascal = tool.split(['_', '-']).map(|word| {
        let mut chars = word.chars();
        chars.next().map(|first| first.to_uppercase().chain(chars).collect::<String>()).unwrap_or_default()
    });
    Some((server, pascal.collect()))
}

pub(super) fn skill_name(name: &str, input: &Value) -> Option<String> {
    let path = Path::new(input["file_path"].as_str()?);
    if name != "Read" || path.file_name()? != "SKILL.md" {
        return None;
    }
    Some(path.parent()?.file_name()?.to_string_lossy().into_owned())
}

pub(super) fn tool_args(name: &str, input: &Value, cwd: &Path) -> String {
    let field = |key: &str| input[key].as_str().unwrap_or_default().to_string();
    let relative = |path: &str| match Path::new(path).strip_prefix(cwd) {
        Ok(rest) if rest.as_os_str().is_empty() => ".".to_string(),
        Ok(rest) => rest.display().to_string(),
        Err(_) => path.to_string(),
    };
    match name {
        "Bash" => {
            let command = field("command").lines().next().unwrap_or_default().to_string();
            match input["timeout"].as_f64() {
                Some(ms) if ms > 0.0 => format!("{command} (timeout {ms} ms)"),
                _ => command,
            }
        }
        "Read" => {
            if let Some(paths) = input["paths"].as_array() {
                return paths.iter().filter_map(|entry| entry.as_str().or(entry["file_path"].as_str())).map(relative).collect::<Vec<_>>().join(", ");
            }
            let mut text = relative(&field("file_path"));
            if let Some(offset) = input["offset"].as_u64().map(|n| n.to_string()).or(input["offset"].as_str().map(String::from)) {
                text.push_str(&format!(" from {offset}"));
            }
            if let Some(limit) = input["limit"].as_u64() {
                text.push_str(&format!(" ({limit} lines)"));
            }
            text
        }
        // Anchors mean nothing to the user; render_tool adds the +/- line counts instead.
        "Write" | "ReplaceLines" | "Edit" | "InsertLines" => relative(&field("file_path")),
        "Page" => field("title"),
        "Grep" => match input["path"].as_str() {
            Some(path) => format!("{} in {}", field("pattern"), relative(path)),
            None => field("pattern"),
        },
        _ => match input {
            Value::Object(map) => map.iter().map(|(k, v)| format!("{k}: {}", plain_json(v))).collect::<Vec<_>>().join(" · "),
            _ => plain_json(input),
        },
    }
}

/// JSON for a one-line header: strings unquoted, objects as `key: value`.
pub(super) fn plain_json(value: &Value) -> String {
    match value {
        Value::String(s) => s.lines().next().unwrap_or_default().to_string(),
        Value::Array(items) => format!("[{}]", items.iter().map(plain_json).collect::<Vec<_>>().join(", ")),
        Value::Object(map) => format!("{{{}}}", map.iter().map(|(k, v)| format!("{k}: {}", plain_json(v))).collect::<Vec<_>>().join(", ")),
        other => other.to_string(),
    }
}

/// Consecutive items of the same run render without a blank line between them, and image tags stay under their prompt.
pub(super) fn blank_between(item: &Item, below: &Item) -> bool {
    let run = transcript_run(item);
    !matches!(below, Item::Images(_)) && (run.is_none() || run != transcript_run(below))
}

pub(super) fn transcript_run(item: &Item) -> Option<&'static str> {
    match item {
        Item::Tool { .. } | Item::Todos(_) => Some("tool"),
        Item::System { .. } | Item::Subagent(_) | Item::Error(_) => Some("log"),
        _ => None,
    }
}

/// Transcript items for a resumed session.
pub(super) fn history_items(messages: &[Value]) -> Vec<Item> {
    let mut items = Vec::new();
    for message in messages {
        for block in message["content"].as_array().into_iter().flatten() {
            match (message["role"].as_str(), block["type"].as_str()) {
                (Some("user"), Some("text")) => {
                    let text = block["text"].as_str().unwrap_or_default();
                    items.push(match crate::agent::harness_source(text) {
                        // The live transcript shows these as a one-line notice too, not as a prompt.
                        Some(source) => Item::System { source: source.into(), text: text.lines().next().unwrap_or_default().into(), tone: Tone::Info },
                        None => Item::User(text.to_string()),
                    });
                }
                (Some("assistant"), Some("text")) => items.push(Item::Assistant { text: block["text"].as_str().unwrap_or_default().to_string(), thinking: String::new() }),
                (Some("assistant"), Some("tool_use")) => items.push(Item::Tool {
                    id: block["id"].as_str().unwrap_or_default().to_string(),
                    name: block["name"].as_str().unwrap_or_default().to_string(),
                    input: block["input"].clone(),
                    result: None,
                    progress: None,
                    started: None,
                }),
                (Some("user"), Some("tool_result")) => {
                    let id = block["tool_use_id"].as_str().unwrap_or_default();
                    let result = (crate::acp::result_text(&block["content"]), block["is_error"].as_bool().unwrap_or(false));
                    for item in items.iter_mut().rev() {
                        if let Item::Tool { id: tool_id, result: slot, .. } = item
                            && tool_id == id
                        {
                            *slot = Some(result);
                            break;
                        }
                    }
                }
                _ => {}
            }
        }
    }
    items
}
