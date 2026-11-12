use super::*;

#[test]
fn plan_file_name_comes_from_the_first_heading() {
    assert_eq!(plan_file_name("intro\n# Add /plan export, v2!\n## Steps"), "ADD_PLAN_EXPORT_V2_PLAN.md");
    assert_eq!(plan_file_name("no heading"), "PLAN.md");
}

#[test]
fn token_counter_counts_up_through_a_jump_and_drops_with_the_estimate() {
    let mut shown = 0;
    let mut frames = Vec::new();
    while shown < 10 {
        shown = count_toward(shown, 10);
        frames.push(shown);
    }
    assert_eq!(frames, [2, 4, 5, 6, 7, 8, 9, 10]);
    assert_eq!(count_toward(500, 300), 300);
}

#[test]
fn duration_rolls_minutes_into_hours() {
    assert_eq!(duration(Duration::from_secs(59)), "59s");
    assert_eq!(duration(Duration::from_secs(27811)), "7h 43m 31s");
}

#[test]
fn transcript_points_count_up_from_the_last_line() {
    let layout = Layout { transcript_rows: 10, ..Layout::default() };
    assert_eq!(layout.transcript_point(0, 9, 3), Point { line: 0, column: 3 });
    assert_eq!(layout.transcript_point(0, 0, 3), Point { line: 9, column: 3 });
    assert_eq!(layout.transcript_point(4, 9, 3), Point { line: 4, column: 3 });
}

#[test]
fn a_subagent_on_another_computer_shows_its_task_not_its_session_file_path_there() {
    let shown = |run: crate::agent::subagent::Run| {
        run.state.lock().unwrap().session = Some("/nonexistent/agent-remote-session.jsonl".into());
        let mut view = RunView { run: Arc::new(run), loaded: None, built: None, lines: Vec::new() };
        view.lines(80, Path::new("/")).iter().map(text).collect::<Vec<_>>().join("\n")
    };
    assert!(shown(crate::agent::subagent::scan_run()).starts_with("✗"));
    assert_eq!(shown(crate::agent::subagent::scan_run_on("arturs-mac-studio")), "· task: scan");
}

#[test]
fn a_watched_subagent_is_not_rebuilt_on_every_append() {
    use std::io::Write as _;
    let dir = std::env::temp_dir().join(format!("agent-run-view-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let session = dir.join("abc123.jsonl");
    let append = |text: &str| {
        let entry = json!({"type": "message", "message": {"role": "assistant", "content": [{"type": "text", "text": text}]}});
        writeln!(std::fs::OpenOptions::new().create(true).append(true).open(&session).unwrap(), "{entry}").unwrap();
    };
    let run = crate::agent::subagent::scan_run();
    run.state.lock().unwrap().session = Some(session.to_str().unwrap().into());
    let mut view = RunView { run: Arc::new(run), loaded: None, built: None, lines: Vec::new() };
    let shows = |view: &mut RunView, width: usize, text: &str| view.lines(width, Path::new("/")).iter().map(self::text).any(|line| line.contains(text));
    append("first");
    let first = shows(&mut view, 80, "first");
    append("second");
    let held = !shows(&mut view, 80, "second");
    view.built = Some(Instant::now() - Duration::from_secs(1));
    let after_the_cap = shows(&mut view, 80, "second");
    append("third");
    let at_a_new_width = shows(&mut view, 60, "third");
    append("fourth");
    view.run.state.lock().unwrap().result = Some(Ok("done".into()));
    let once_finished = shows(&mut view, 60, "fourth");
    std::fs::remove_dir_all(&dir).unwrap();
    assert_eq!([first, held, after_the_cap, at_a_new_width, once_finished], [true; 5]);
}

#[test]
fn tasks_list_shells_and_agents_on_separate_tabs() {
    use crate::agent::subagent::{RunState, scan_run};
    let done = scan_run();
    *done.state.lock().unwrap() = RunState { elapsed: Some(Duration::from_secs(165)), result: Some(Ok("found".into())), usage: Usage { output: 11_000, ..Usage::default() }, ..Default::default() };
    let mut running = scan_run();
    (running.label, running.model, running.thinking) = ("recon 1a".into(), "claude-sonnet-5".into(), "medium".into());
    let sonnet = Model { provider: "anthropic".into(), id: "claude-sonnet-5".into(), name: "Claude Sonnet 5".into(), context_window: 1, max_tokens: 1, accounts: Vec::new(), price: None };
    let runs = [Arc::new(done), Arc::new(running)];
    let rows: Vec<String> = tasks_panel(&[], &runs, &[sonnet], true, 1, 80).iter().map(text).collect();
    assert_eq!(rows[1..3], ["Background  Shells 0  [Agents 2]", "1 active agent"]);
    assert_eq!(rows[4], "  ● scan       done · 0 tools · opus high");
    assert_eq!(rows[5], "❯ ● recon 1a   running · 0 tools · sonnet 5 medium");
    assert_eq!(rows.last().unwrap(), "↑/↓ to select · Tab to switch · Enter to view · x to stop · Esc to close");
    let shells: Vec<String> = tasks_panel(&[], &runs, &[], false, 0, 80).iter().map(text).collect();
    assert_eq!(shells[1], "Background  [Shells 0]  Agents 2");
    assert_eq!(shells[4], "  No shells yet.");
}

#[test]
fn background_rows_show_a_shells_pill_and_an_agents_pill() {
    let rows = |shells, agents, viewed, focus| background_rows(shells, agents, viewed, focus, 40).iter().map(text).collect::<Vec<_>>();
    assert_eq!(rows(2, 13, None, None), ["2 shells · 13 agents · ↓ to view"]);
    assert_eq!(rows(0, 1, None, Some(0)), ["1 agent · Enter to view"]);
    assert_eq!(rows(1, 0, Some("scan"), None), ["1 shell · ↓ to view", "viewing scan · Esc back"]);
    assert_eq!(rows(0, 0, Some("scan"), None), ["viewing scan · Esc back"]);
    assert!(rows(0, 0, None, None).is_empty());
    let focused = background_rows(2, 13, None, Some(1), 40);
    assert_eq!(focused[0][0].style, Style::fg(45));
    assert_eq!(focused[0][2].style, Style::fg(16).on(45));
}

#[test]
fn short_path_cuts_every_folder_but_the_last() {
    assert_eq!(short_path("~/Developer/adescom/vapp-v2-frontend"), "~/Dev/ade/vapp-v2-frontend");
    assert_eq!(short_path("/private/tmp"), "/pri/tmp");
    assert_eq!(short_path("~"), "~");
    assert_eq!(short_path("~/Żółtość/x"), "~/Żół/x");
}

#[test]
fn footer_wraps_by_word_when_the_row_is_full() {
    let rows = |width| footer_rows(vec![span("~/r/app-pr detached → main ~/r/app", PLAIN)], vec![span("opus 12%", MUTED)], width).iter().map(text).collect::<Vec<_>>();
    assert_eq!(rows(44), ["~/r/app-pr detached → main ~/r/app  opus 12%"]);
    assert_eq!(rows(30), ["~/r/app-pr detached → main ", "~/r/app               opus 12%"]);
    assert_eq!(rows(15), ["~/r/app-pr ", "detached → main", "~/r/app", "       opus 12%"]);
}

#[test]
fn star_fills_only_while_the_band_passes_it() {
    let lit = shimmer_line(WORKING, 3, "star");
    assert_eq!(text(&lit), " ✦ Working");
    assert_eq!(lit[1].style, Style::fg(255));

    let later = shimmer_line(WORKING, 10, "star");
    assert_eq!(text(&later), " ✧ Working");
    assert_eq!(later[1].style, DIM);
    assert_eq!(later[3 + 5].style, Style::fg(255));
}

#[test]
fn every_working_animation_but_the_star_cycles_one_cell_frames() {
    for animation in WORKING_ANIMATIONS.iter().filter(|name| **name != "star") {
        let marks: Vec<Span> = (0..40).map(|tick| shimmer_line(WORKING, tick, animation).remove(1)).collect();
        assert!(marks.iter().all(|mark| line_width(&vec![mark.clone()]) == 1), "{animation}");
        assert!(marks.iter().any(|mark| mark.text != marks[0].text), "{animation} never moves");
    }
    // 110 ms frames on an 80 ms tick: tick 7 is 560 ms in, the sixth frame.
    assert_eq!(text(&shimmer_line(WORKING, 7, "radar")), " ● Working");
}

#[test]
fn done_mark_flashes_bright_then_rests_gray() {
    assert_eq!((done_mark("radar", Duration::ZERO).text.as_str(), done_mark("radar", Duration::ZERO).style), ("◉", Style::fg(255)));
    assert_eq!((done_mark("radar", DONE_FLASH).text.as_str(), done_mark("radar", DONE_FLASH).style), ("●", MUTED));
    assert_eq!((done_mark("star", Duration::ZERO).text.as_str(), done_mark("star", Duration::ZERO).style), ("✧", DIM));
}

#[test]
fn done_rows_follow_the_working_animation_setting() {
    let home = std::env::temp_dir().join(format!("agent-tui-done-mark-{}", std::process::id()));
    let mut app = test_app(&home);
    std::fs::remove_dir_all(&home).ok();
    app.push(Item::Done { took: Duration::from_secs(83), usage: Usage::default(), total: Duration::from_secs(600) });
    let first = |app: &mut App| text(&app.transcript_tail(80, 1).0[0]);
    assert!(first(&mut app).starts_with("✧ 1m 23s"), "{}", first(&mut app));
    app.settings.working_animation = "diamond".into();
    assert!(first(&mut app).starts_with("◆ 1m 23s"), "{}", first(&mut app));
}

#[test]
fn working_label_says_thinking_only_before_the_answer_starts() {
    let assistant = |text: &str, thinking: &str| Item::Assistant { text: text.into(), thinking: thinking.into() };
    assert_eq!(working_label(Some(&assistant("", "hm"))), THINKING);
    assert_eq!(working_label(Some(&assistant("ok", "hm"))), WORKING);
    assert_eq!(working_label(Some(&assistant("", ""))), WORKING);
    assert_eq!(working_label(None), WORKING);
}

#[test]
fn recording_line_shows_minutes_and_seconds() {
    let line = recording_line(Duration::from_secs(75));
    assert_eq!(text(&line), " ● 1:15  Enter to transcribe · Esc to cancel");
    assert_eq!(line[0].style, ERROR);
}

#[test]
fn hotkeys_list_groups_with_keys_in_one_column() {
    let rows: Vec<String> = hotkeys_lines().iter().map(text).collect();
    assert_eq!(rows[0], "ℹ hotkeys");
    assert_eq!(rows[1], "  Send");
    assert!(rows[2].starts_with("    Enter  "), "{rows:?}");
    let column = |row: &String| row.trim_start().len() - row.trim_start().split("  ").skip(1).collect::<Vec<_>>().join("  ").trim_start().len();
    let starts: Vec<usize> = rows.iter().filter(|row| row.starts_with("    ")).map(|row| row.len() - row.trim_start().len() + column(row)).collect();
    assert!(starts.windows(2).all(|pair| pair[0] == pair[1]), "{rows:?}");
}

#[test]
fn context_use_turns_yellow_at_70_and_red_at_90_percent() {
    assert_eq!([69, 70, 89, 90].map(context_style), [MUTED, WARN, WARN, ERROR]);
}

#[test]
fn thinking_level_brightens_from_grey_to_pure_white() {
    let grays: Vec<Style> = THINKING_LEVELS.iter().map(|l| thinking_style(l)).collect();
    assert_eq!(grays[0], Style::rgb(0x767676));
    assert_eq!(grays[THINKING_LEVELS.len() - 1], Style::rgb(0xffffff));
    assert!(grays.windows(2).all(|pair| pair[0] != pair[1]));
}

#[test]
fn token_counts_round_like_pi() {
    assert_eq!([tokens(950), tokens(14_400), tokens(999_600), tokens(1_250_000)], ["950", "14k", "1.0M", "1.2M"]);
}

#[test]
fn scrollbar_thumb_tracks_the_position_and_hides_when_everything_fits() {
    assert!(scrollbar(4, 4, 0).is_empty());
    assert_eq!(scrollbar(8, 4, 0), [" ", " ", "╷", "╵"]);
    assert_eq!(scrollbar(8, 4, 4), ["╷", "╵", " ", " "]);
    assert_eq!(scrollbar(100, 3, 48), [" ", "•", " "]);
}

#[test]
fn pinned_prompt_keeps_three_rows_and_marks_the_cut() {
    let text = |rows: &[Line]| rows.iter().map(|row| row.iter().map(|s| s.text.as_str()).collect::<String>()).collect::<Vec<_>>();
    let mut rows = vec![vec![span("x", PLAIN)]; 5];
    pin_prompt(&mut rows, &render_item(&Item::User("a\nb".into()), 10, false, Path::new("/repo")), 10);
    assert_eq!(text(&rows), ["❯ a       ", "  b       ", "x", "x", "x"]);
    pin_prompt(&mut rows, &render_item(&Item::User("1\n2\n3\n4".into()), 10, false, Path::new("/repo")), 10);
    assert_eq!(text(&rows), ["❯ 1       ", "  2       ", "  3…      ", "x", "x"]);
    assert!(rows[2].iter().all(|s| s.style.bg == Some(PROMPT_BG)));
}

#[test]
fn subagent_lines_render_its_session_file() {
    let dir = std::env::temp_dir().join(format!("agent-subagent-lines-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let session = dir.join("abc123.jsonl");
    let messages = [json!({"role": "user", "content": [{"type": "text", "text": "review auth"}]}), json!({"role": "assistant", "content": [{"type": "text", "text": "Token stays cached."}]})];
    let entries: Vec<String> = messages.iter().map(|message| json!({"type": "message", "message": message}).to_string()).collect();
    std::fs::write(&session, entries.join("\n") + "\n").unwrap();
    let lines = subagent_lines(&session, 60, Path::new("/repo"));
    std::fs::remove_dir_all(&dir).unwrap();
    let lines: Vec<String> = lines.unwrap().iter().map(text).collect();
    assert!(lines.iter().any(|line| line.contains("review auth")));
    assert!(lines.iter().any(|line| line.contains("Token stays cached.")));
}

#[test]
fn subagent_lines_fold_consecutive_tool_calls_and_keep_tool_rows_together() {
    let dir = std::env::temp_dir().join(format!("agent-subagent-tools-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let session = dir.join("abc123.jsonl");
    let tool = |id: &str, path: &str| json!({"type": "tool_use", "id": id, "name": "Read", "input": {"file_path": path}});
    let result = |id: &str, failed: bool| json!({"type": "tool_result", "tool_use_id": id, "content": "ok", "is_error": failed});
    let messages = [
        json!({"role": "assistant", "content": [tool("a", "one.rs"), tool("b", "two.rs"), tool("c", "three.rs")]}),
        json!({"role": "user", "content": [result("a", false), result("b", false), result("c", true)]}),
        json!({"role": "assistant", "content": [{"type": "text", "text": "Done."}]}),
    ];
    let entries: Vec<String> = messages.iter().map(|message| json!({"type": "message", "message": message}).to_string()).collect();
    std::fs::write(&session, entries.join("\n") + "\n").unwrap();
    let lines = subagent_lines(&session, 60, Path::new("/repo"));
    std::fs::remove_dir_all(&dir).unwrap();
    let lines: Vec<String> = lines.unwrap().iter().map(text).collect();
    let folded = lines.iter().position(|line| line.contains("Read 2 files")).unwrap();
    let failed = lines.iter().position(|line| line.contains("three.rs")).unwrap();
    let done = lines.iter().position(|line| line.contains("Done.")).unwrap();
    assert_eq!(failed, folded + 1, "{lines:#?}");
    assert!(lines[failed..done].iter().any(|line| line.trim().is_empty()), "{lines:#?}");
}

#[test]
fn log_lines_drop_colors_and_keep_the_last_progress_redraw() {
    let lines: Vec<String> = log_lines("\x1b[32mok\x1b[0m\n10%\r55%\r100%\nabcdefghij", 6).iter().map(text).collect();
    assert_eq!(lines, [" ok", " 100%", " abcde", " fghij"]);
}

#[test]
fn full_screen_starts_short_output_under_the_header() {
    let lines = vec![vec![span("a", PLAIN)], vec![span("b", PLAIN)]];
    let rows: Vec<String> = full_screen(vec![span("head", PLAIN)], &lines, &mut 0, 7, "esc back").iter().map(text).collect();
    assert_eq!(rows, ["head", "a", "b", "", "", "", "esc back"]);
}

#[test]
fn screen_header_shows_the_position_only_among_several() {
    assert_eq!(text(&screen_header(vec![span("ls", PLAIN)], 1, 3)), "ls · 2/3");
    assert_eq!(text(&screen_header(vec![span("ls", PLAIN)], 0, 1)), "ls");
}
