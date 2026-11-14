use super::*;

#[test]
fn a_finished_subagent_shows_its_status_counts_and_error() {
    let run = crate::agent::subagent::scan_run();
    *run.state.lock().unwrap() = crate::agent::subagent::RunState {
        tool_calls: 1,
        elapsed: Some(Duration::from_secs(319)),
        result: Some(Ok("found".into())),
        usage: Usage { input: 108, output: 33_000, ..Usage::default() },
        ..Default::default()
    };
    let rows: Vec<String> = subagent_done_lines(&run, 80).iter().map(text).collect();
    assert_eq!(rows, ["✓ sa-1 scan · 1 tool · 5m 19s · ↑108 ↓33k cache 0"]);
    *run.state.lock().unwrap() = crate::agent::subagent::RunState { tool_calls: 31, elapsed: Some(Duration::from_secs(8)), result: Some(Err("Operation aborted".into())), ..Default::default() };
    let rows: Vec<String> = subagent_done_lines(&run, 30).iter().map(text).collect();
    assert_eq!(rows, ["✗ sa-1 scan · 31 tools · 8s ", "  · Operation aborted"]);
}

#[test]
fn unknown_tool_args_read_as_key_values() {
    let input = json!({"action": "create", "subject": "Manifest: a, b", "ids": [1, 2], "q": [{"header": "Panel"}]});
    assert_eq!(tool_args("Todo", &input, Path::new("/repo")), "action: create · ids: [1, 2] · q: [{header: Panel}] · subject: Manifest: a, b");
}

#[test]
fn done_line_leads_with_the_dim_star() {
    let item = Item::Done { took: Duration::from_secs(83), usage: Usage { input: 1200, output: 300, ..Usage::default() }, total: Duration::from_secs(600) };
    let line = &render_item(&item, 80, false, Path::new("/repo"))[0];
    assert!(text(line).starts_with("✧ 1m 23s ↑"), "{}", text(line));
    assert_eq!(line[0].style, DIM);
}

#[test]
fn prompt_rows_mark_every_row_for_copy_and_fill_no_background() {
    let rows = prompt_rows("a\nb", BRIGHT, 10);
    assert_eq!(rows.iter().map(text).collect::<Vec<_>>(), ["❯ a", "  b"]);
    assert!(rows.iter().all(|row| row[0].style == PROMPT_LEAD && row.iter().all(|s| s.style.bg.is_none())));
}

#[test]
fn user_prompt_sits_on_a_full_width_band() {
    let rows = render_item(&Item::User("a\nb".into()), 6, false, Path::new("/repo"));
    assert_eq!(rows.iter().map(text).collect::<Vec<_>>(), ["❯ a   ", "  b   "]);
    assert!(rows.iter().flatten().all(|s| s.style.bg == Some(PROMPT_BG)));
}

#[test]
fn collapsed_tool_call_is_one_line_even_for_long_bash_and_errors() {
    let ok = ("total 1\na.txt  6B\n".to_string(), false);
    let long = json!({"command": "echo aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"});
    let rows: Vec<String> = render_tool("Bash", &long, Some(&ok), 20, false, Path::new("/repo")).iter().map(text).collect();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].chars().count() <= 20, "{rows:?}");
    let failed = ("no such file\n".to_string(), true);
    assert_eq!(render_tool("Read", &json!({"file_path": "x"}), Some(&failed), 40, false, Path::new("/repo")).len(), 1);
}

#[test]
fn collapsed_tool_row_ends_with_its_result_and_keeps_its_arguments_gray() {
    let row = |name: &str, input: Value, output: &str, failed: bool| render_tool(name, &input, Some(&(output.to_string(), failed)), 80, false, Path::new("/repo")).remove(0);
    let read = row("Read", json!({"file_path": "/repo/src/a.rs"}), "1\n2\n3\n", false);
    assert_eq!(text(&read), "✓ Read src/a.rs · 3 lines");
    assert_eq!(read[2].style, TOOL_ARGS);
    let failed = row("Bash", json!({"command": "cargo test"}), "running\ntest x ... FAILED\n\n", true);
    assert_eq!(text(&failed), "✗ Bash cargo test · test x ... FAILED");
    assert_eq!(failed[0].style, ERROR);
    assert_eq!(text(&row("Bash", json!({"command": "pwd"}), "\n/repo\n", false)), "✓ Bash pwd · /repo");
    assert_eq!(text(&row("Bash", json!({"command": "true"}), "", false)), "✓ Bash true");
    assert!(running_seconds(Some(Instant::now())).is_empty());
    assert_eq!(text(&running_seconds(Instant::now().checked_sub(Duration::from_secs(4)))), " · 4s");
}

#[test]
fn expanded_tool_call_puts_the_output_under_a_hook() {
    let result = ("total 1\na.txt  6B\n".to_string(), false);
    let lines = render_tool("Bash", &json!({"command": "ls", "timeout": 10000}), Some(&result), 40, true, Path::new("/repo"));
    let rows: Vec<String> = lines.iter().map(text).collect();
    assert_eq!(rows, ["✓ Bash ls (timeout 10000 ms)", "└ total 1", "  a.txt  6B"]);
}

#[test]
fn a_code_call_is_headed_by_its_first_line_and_shows_the_script_when_expanded() {
    let input = json!({"code": "\n  const files = await $`ls`;\n\tif (files) {\n    return files;\n  }\n"});
    let result = ("a.txt".to_string(), false);
    let rows = |expanded: bool| render_tool("Code", &input, Some(&result), 60, expanded, Path::new("/repo")).iter().map(text).collect::<Vec<_>>();
    assert_eq!(rows(false), ["✓ Code const files = await $`ls`; · a.txt"]);
    assert_eq!(rows(true), ["✓ Code const files = await $`ls`;", "      if (files) {", "      return files;", "    }", "└ a.txt"]);
}

#[test]
fn answered_question_shows_its_answer_under_a_hook() {
    let input = json!({"questions": [{"question": "Which db?", "header": "DB", "options": []}]});
    let result = ("User has answered your questions: \"Which db?\"=\"sqlite\". You can now continue.".to_string(), false);
    let lines = render_tool("AskUserQuestion", &input, Some(&result), 40, false, Path::new("/repo"));
    let rows: Vec<String> = lines.iter().map(text).collect();
    assert_eq!(rows, ["✓ AskUserQuestion Which db?", "  └ sqlite"]);
}

#[test]
fn update_todos_marks_each_task_and_strikes_through_the_done_ones() {
    let task = |subject: &str, status| Task { id: 1, subject: subject.into(), description: None, active_form: None, status };
    let lines = update_todos_lines(&[task("review", Status::Completed), task("fix", Status::InProgress), task("test", Status::Pending)], 40);
    let rows: Vec<String> = lines.iter().map(text).collect();
    assert_eq!(rows, ["✓ Update Todos", "  ⎿ ✓ review", "    • fix", "    ○ test"]);
    let running: Vec<String> = render_tool("Todo", &json!({"action": "create", "subject": "x"}), None, 40, false, Path::new("/repo")).iter().map(text).collect();
    assert_eq!(running, ["• Update Todos"]);
    assert!(lines[1][1].style == OK && lines[1][2].style.strikethrough && lines[2][1].style.bold && lines[3][1].style == DIM && lines[3][2].style == PLAIN);
}

#[test]
fn a_running_tool_shows_its_progress_under_its_row_until_it_ends() {
    let tool = |result: Option<(String, bool)>| Item::Tool {
        id: "t1".into(),
        name: "Video".into(),
        input: json!({"source": "a.mp4"}),
        result,
        progress: Some("transcribing 0:10 / 1:00".into()),
        started: None,
    };
    let rows = |item: &Item| render_item(item, 80, false, Path::new("/repo")).iter().map(text).collect::<Vec<_>>();
    assert_eq!(rows(&tool(None)), ["• Video source: a.mp4", "└ transcribing 0:10 / 1:00"]);
    assert_eq!(rows(&tool(Some(("done".into(), false)))), ["✓ Video source: a.mp4 · done"]);
}

#[test]
fn the_collapsed_transcript_shows_each_tool_call_on_its_own_row() {
    let home = std::env::temp_dir().join(format!("agent-tui-calls-{}", std::process::id()));
    let mut app = test_app(&home);
    std::fs::remove_dir_all(&home).ok();
    for path in ["a.rs", "b.rs"] {
        app.push(Item::Tool { id: path.into(), name: "Read".into(), input: json!({"file_path": path}), result: Some(("ok".into(), false)), progress: None, started: None });
    }
    let tail = app.transcript_tail(80, 2).0.iter().map(text).collect::<Vec<_>>();
    assert_eq!(tail, ["✓ Read a.rs · ok", "✓ Read b.rs · ok"]);
}

#[test]
fn reading_a_skill_file_shows_the_skill_name() {
    let result = ("body".to_string(), false);
    let lines = render_tool("Read", &json!({"file_path": "/x/skills/verify-web-app/SKILL.md"}), Some(&result), 80, false, Path::new("/repo"));
    assert_eq!(text(&lines[0]), "✓ Skill verify-web-app (ctrl+o to expand)");
}

#[test]
fn mcp_tool_shows_its_name_and_server() {
    let lines = render_tool("mcp__vexdock__get_service", &json!({"service_id": "s1"}), None, 80, false, Path::new("/repo"));
    assert_eq!(text(&lines[0]), "• GetService service_id: s1 MCP: vexdock");
}

#[test]
fn tool_rows_show_paths_relative_to_the_working_directory() {
    let ok = ("body".to_string(), false);
    let row = |name: &str, input: Value| text(&render_tool(name, &input, Some(&ok), 80, false, Path::new("/repo"))[0]);
    assert_eq!(row("Read", json!({"file_path": "/repo/src/a.rs"})), "✓ Read src/a.rs · body");
    assert_eq!(row("Read", json!({"paths": ["/repo/a.rs", "/etc/hosts"]})), "✓ Read a.rs, /etc/hosts · body");
    assert_eq!(row("Grep", json!({"pattern": "fn", "path": "/repo"})), "✓ Grep fn in . · body");
    assert_eq!(row("Write", json!({"file_path": "/repository/a.rs"})), "✓ Write /repository/a.rs · body");
}

#[test]
fn edit_rows_count_the_changed_lines_in_place_of_anchors() {
    let result = (" AAAA│keep\n-BBBB│old\n+CCCC│new\n+DDDD│more\n\n[lsp] 1 new error in a.rs:\n+1 more".to_string(), false);
    let row = |input: Value| text(&render_tool("ReplaceLines", &input, Some(&result), 80, false, Path::new("/repo"))[0]);
    assert_eq!(row(json!({"file_path": "/repo/a.rs", "remove_from": "BBBB", "remove_to": "BBBB"})), "✓ ReplaceLines a.rs +2 -1");
    assert_eq!(row(json!({"remove_from": "BBBB", "remove_to": "BBBB"})), "✓ ReplaceLines +2 -1");
}

#[test]
fn log_row_wraps_under_its_source() {
    let lines = system_lines("✗", "error", "aaa bbb ccc\nddd", ERROR, 14);
    let rows: Vec<String> = lines.iter().map(text).collect();
    assert_eq!(rows, ["✗ error: aaa", "  bbb ccc", "  ddd"]);
}

#[test]
fn a_tabbed_log_row_wraps_its_description_under_the_description_column() {
    let lines = system_lines("·", "help", "2 commands\n/plan\taaa bbb ccc\n/btw\t", DIM, 16);
    let rows: Vec<String> = lines.iter().map(text).collect();
    assert_eq!(rows, ["· help: 2 ", "  commands", "  /plan aaa bbb ", "        ccc", "  /btw "]);
}

#[test]
fn a_system_line_takes_its_mark_and_color_from_its_tone() {
    let row = |tone| render_item(&Item::System { source: "trust".into(), text: "x".into(), tone }, 40, false, Path::new("/repo")).remove(0);
    let look = |line: &Line| (text(line), line[0].style, line.last().unwrap().style);
    assert_eq!(look(&row(Tone::Info)), ("ℹ trust: x".to_string(), SYSTEM, NOTICE_TEXT));
    assert_eq!(look(&row(Tone::Warn)), ("! trust: x".to_string(), WARN, NOTICE_TEXT));
    assert_eq!(look(&row(Tone::Error)), ("✗ trust: x".to_string(), ERROR, ERROR));
}

#[test]
fn edit_diff_colors_added_green_removed_red_context_gray() {
    let result = (" AAAA│keep\n-BBBB│old\n+CCCC│new".to_string(), false);
    let lines = render_tool("ReplaceLines", &json!({"file_path": "a.rs"}), Some(&result), 40, true, Path::new("/repo"));
    let colors: Vec<Style> = lines[1..].iter().map(|line| line[1].style).collect();
    assert_eq!(colors, [TOOL_ARGS, ERROR, OK]);
}

#[test]
fn history_pairs_tool_results_with_their_calls() {
    let messages = [
        json!({"role": "assistant", "content": [{"type": "tool_use", "id": "t1", "name": "Read", "input": {"file_path": "a"}}]}),
        json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "x", "is_error": true}]}),
        json!({"role": "assistant", "content": [{"type": "tool_use", "id": "t2", "name": "Video", "input": {"source": "a.mp4"}}]}),
        json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t2", "content": [{"type": "text", "text": "frames"}, {"type": "image", "source": {}}]}]}),
    ];
    let items = history_items(&messages);
    assert!(matches!(&items[0], Item::Tool { result: Some((out, true)), .. } if out == "x"));
    assert!(matches!(&items[1], Item::Tool { result: Some((out, false)), .. } if out == "frames"));
}

#[test]
fn history_shows_agent_added_texts_as_notices() {
    let shell = json!({"type": "text", "text": "Ran `cd ..`\n```\nmoved\n```"});
    let messages = [json!({"role": "user", "content": [shell, {"type": "text", "text": "go"}]})];
    let items = history_items(&messages);
    assert!(matches!(&items[0], Item::System { source, text, .. } if source == "shell" && text == "Ran `cd ..`"));
    assert!(matches!(&items[1], Item::User(text) if text == "go"));
}

#[test]
fn image_tags_sit_right_under_their_prompt() {
    let prompt = Item::User("see [Image #1]".into());
    let images = Item::Images(vec!["[Image #1]".into()]);
    assert!(!blank_between(&prompt, &images));
    assert!(blank_between(&images, &Item::User("next".into())));
    assert!(blank_between(&prompt, &Item::User("next".into())));
}
