use super::*;
use serde_json::json;

#[test]
fn a_page_published_with_no_terminal_attached_opens_in_the_next_one_that_attaches() {
    let home = std::env::temp_dir().join(format!("agent-tui-page-{}", std::process::id()));
    let cwd = home.join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let (client, _) = crate::api::images::tests::fake_codex_client(&home, &[]);
    let settings = crate::settings::load(&home).unwrap();
    let session = Session::start(&home.join("sessions"), false, &cwd, "m").unwrap();
    let agent = Agent::new(client, &settings, home.clone(), cwd, session, home.join("sessions"), None);
    let setup = Setup { home: home.clone(), settings, prompt: None, pick_session: false, modes_error: None, plugin_updates: Vec::new(), missing_cwd: None, placed: None };
    let mut app = App::new(&agent, setup, mpsc::channel().0, mpsc::channel().0);

    app.agent_event(Event::OpenPage("http://127.0.0.1:1/t/1".into()));
    let (first, first_lines) = server::Client::fake(1);
    app.add_client(first);
    let (second, second_lines) = server::Client::fake(2);
    app.add_client(second);
    app.agent_event(Event::OpenPage("http://127.0.0.1:1/t/2".into()));
    std::fs::remove_dir_all(&home).ok();

    let opened = |lines: &mpsc::Receiver<String>| lines.try_iter().filter(|line| line.contains("\"open\"")).collect::<Vec<_>>();
    assert_eq!(opened(&first_lines), [json!({"open": "http://127.0.0.1:1/t/1"}).to_string()]);
    assert_eq!(opened(&second_lines), [json!({"open": "http://127.0.0.1:1/t/2"}).to_string()], "no terminal typed yet, so the last one attached opens it");
}

fn test_app(home: &Path) -> App {
    let cwd = home.join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let (client, _) = crate::api::images::tests::fake_codex_client(home, &[]);
    let settings = crate::settings::load(home).unwrap();
    let session = Session::start(&home.join("sessions"), false, &cwd, "m").unwrap();
    let agent = Agent::new(client, &settings, home.to_path_buf(), cwd, session, home.join("sessions"), None);
    let setup = Setup { home: home.to_path_buf(), settings, prompt: None, pick_session: false, modes_error: None, plugin_updates: Vec::new(), missing_cwd: None, placed: None };
    App::new(&agent, setup, mpsc::channel().0, mpsc::channel().0)
}

#[test]
fn export_and_import_expand_a_leading_tilde_to_home() {
    let home = std::env::temp_dir().join(format!("agent-tui-tilde-{}", std::process::id()));
    let mut app = test_app(&home);
    std::fs::remove_dir_all(&home).ok();
    let (commands, sent) = mpsc::channel();
    app.commands = commands;

    app.submit("/export ~/out.jsonl".into(), false);
    app.submit("/import ~/in.jsonl".into(), false);
    app.submit("/export ../sub/out.jsonl".into(), false);

    let user_home = PathBuf::from(std::env::var("HOME").unwrap());
    let Ok(Command::Export(out)) = sent.try_recv() else { panic!("/export sent no Export") };
    let Ok(Command::Import(file)) = sent.try_recv() else { panic!("/import sent no Import") };
    assert_eq!((out, file), (user_home.join("out.jsonl"), user_home.join("in.jsonl")));
    let Ok(Command::Export(relative)) = sent.try_recv() else { panic!("/export sent no Export") };
    assert_eq!(relative, app.cwd.join("../sub/out.jsonl"));
}

#[test]
fn the_mouse_over_a_full_screen_view_leaves_the_hidden_transcript_alone() {
    let home = std::env::temp_dir().join(format!("agent-tui-hidden-mouse-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let mut app = test_app(&home);
    let wheel = MouseEvent { kind: MouseEventKind::ScrollUp, row: 0, column: 0, modifiers: KeyModifiers::NONE };
    app.panel = Some(Panel::Diff { files: Vec::new(), index: 0, scroll: 0 });
    app.mouse(wheel);
    app.panel = Some(Panel::Terminal { index: 0, scroll: 0, loaded: None, lines: Vec::new() });
    app.mouse(wheel);
    assert_eq!(app.scroll, 0);
    app.panel = None;
    app.mouse(wheel);
    assert_eq!(app.scroll, app.settings.scroll_lines);
}

#[test]
fn a_key_typed_before_the_trust_question_appeared_does_not_answer_it() {
    let home = std::env::temp_dir().join(format!("agent-tui-trust-{}", std::process::id()));
    std::fs::create_dir_all(home.join("project/.agents")).unwrap();
    std::fs::write(home.join("project/.agents/SYSTEM.md"), "x").unwrap();
    let mut app = test_app(&home);
    app.offer_trust();
    assert!(matches!(app.panel, Some(Panel::Trust { .. })));
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let trusted_at_once = app.settings.trusts(&app.cwd);
    let Some(Panel::Trust { opened, .. }) = &mut app.panel else { panic!("the early key closed the question") };
    *opened = Instant::now() - Duration::from_secs(1);
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let trusted = app.settings.trusts(&app.cwd);
    std::fs::remove_dir_all(&home).ok();
    assert_eq!((trusted_at_once, trusted), (false, true));
}

#[test]
fn alt_up_keeps_the_draft_and_a_gap_in_the_images_does_not_panic() {
    let home = std::env::temp_dir().join(format!("agent-tui-draft-{}", std::process::id()));
    let mut app = test_app(&home);
    std::fs::remove_dir_all(&home).ok();
    let image = json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AA"}, DROPPED_PATH: "/a.png"});

    app.steering.lock().unwrap().push(("queued one".into(), Vec::new()));
    app.editor.set("draft");
    app.key(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT));
    assert_eq!(app.editor.text(), "draft\n\nqueued one");

    app.settings.mid_turn = "steer".into();
    app.working = Some(Instant::now());
    app.images = vec![Value::Null, image.clone()];
    app.submit("see [Image #2]".into(), false);
    let steering = std::mem::take(&mut *app.steering.lock().unwrap());
    assert_eq!(steering.len(), 1);
    assert_eq!(steering[0].1, [Value::Null, json!({"type": "image", "source": image["source"]})]);

    app.images = vec![Value::Null, image.clone()];
    assert_eq!(app.take_images("[Image #1] typed by hand, [Image #2]").len(), 1);
}

#[test]
fn pending_messages_say_whether_they_steer_or_wait() {
    let home = std::env::temp_dir().join(format!("agent-tui-pending-{}", std::process::id()));
    let mut app = test_app(&home);
    std::fs::remove_dir_all(&home).ok();
    app.working = Some(Instant::now());
    app.settings.mid_turn = "steering".into();
    app.submit("now".into(), false);
    app.submit("later".into(), true);
    let rows: Vec<String> = app.frame(40, 20).0.iter().map(text).collect();
    let start = rows.iter().position(|row| row.starts_with("  Steering")).expect("a steering header");
    assert_eq!(rows[start + 1..start + 4], ["❯ now", "  Queued · sent when the turn ends", "❯ later"], "{rows:?}");
}

#[test]
fn bracketed_notice_prefix_becomes_the_source() {
    assert_eq!(notice_source("[lsp] Cannot read a.rs"), ("lsp", "Cannot read a.rs"));
    assert_eq!(notice_source("Operation aborted"), ("agent", "Operation aborted"));
}

#[test]
fn history_lists_a_repeated_prompt_once() {
    let entry = |prompt: &str| history::Entry { prompt: prompt.into(), cwd: "/repo".into(), session: "s1".into() };
    let entries = [entry("commits push pr merge"), entry("commits i push"), entry("commits push pr merge")];
    assert_eq!(matching_prompts(&entries, "commits", |_| true), ["commits push pr merge", "commits i push"]);
    assert_eq!(matching_prompts(&entries, "", |_| true), ["commits push pr merge", "commits i push"]);
}

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
fn step_wraps_and_starts_over_from_an_unlisted_value() {
    let models = ["a", "b", "c"];
    assert_eq!(step(&models, &"c", true), "a");
    assert_eq!(step(&models, &"x", true), "a");
}

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
fn a_failed_mode_rules_download_is_reported_in_the_transcript() {
    let home = std::env::temp_dir().join(format!("agent-modes-fetched-{}", std::process::id()));
    let mut app = test_app(&home);
    app.handle(Msg::ModesFetched(Ok(())));
    let after_success = app.entries.len();
    app.handle(Msg::ModesFetched(Err("Cannot download the ponytail rules from x: offline".into())));
    let _ = std::fs::remove_dir_all(&home);
    assert_eq!(app.entries.len(), after_success + 1);
    assert!(matches!(&app.entries.last().unwrap().item, Item::Error(error) if error.ends_with("x: offline")));
}

#[test]
fn a_jump_to_the_bottom_follows_new_output_while_a_finished_selection_is_still_highlighted() {
    let home = std::env::temp_dir().join(format!("agent-tui-follow-{}", std::process::id()));
    let mut app = test_app(&home);
    let _ = std::fs::remove_dir_all(&home);
    let add_line = |app: &mut App| {
        app.handle(Msg::ModesFetched(Err("offline".into())));
        app.frame(40, 20);
    };
    for _ in 0..30 {
        add_line(&mut app);
    }
    let mouse = |kind, row, column| MouseEvent { kind, row, column, modifiers: KeyModifiers::NONE };
    app.mouse(mouse(MouseEventKind::Down(MouseButton::Left), 2, 2));
    app.mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 3, 6));
    add_line(&mut app);
    assert!(app.scroll > 0, "a drag in progress holds the view on the text under the mouse");
    app.mouse(mouse(MouseEventKind::Up(MouseButton::Left), 3, 6));
    assert!(app.selection.range().is_some());

    app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::ALT));
    add_line(&mut app);
    assert_eq!(app.scroll, 0);
}

#[test]
fn a_click_on_the_jump_pill_returns_to_the_bottom() {
    let home = std::env::temp_dir().join(format!("agent-tui-pill-{}", std::process::id()));
    let mut app = test_app(&home);
    let _ = std::fs::remove_dir_all(&home);
    for _ in 0..30 {
        app.handle(Msg::ModesFetched(Err("offline".into())));
    }
    let click = |app: &mut App, row: usize, column: usize| {
        app.scroll = 5;
        let rows: Vec<String> = app.frame(80, 20).0.iter().map(text).collect();
        assert_eq!(app.scroll, 5);
        app.mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), row: row as u16, column: column as u16, modifiers: KeyModifiers::NONE });
        app.mouse(MouseEvent { kind: MouseEventKind::Up(MouseButton::Left), row: row as u16, column: column as u16, modifiers: KeyModifiers::NONE });
        (rows, app.scroll)
    };
    let (rows, _) = click(&mut app, 0, 0);
    let pill = jump_pill();
    let row = rows.iter().position(|row| row.contains(&pill)).expect("the pill is drawn");
    let first = width(&rows[row][..rows[row].find(&pill).unwrap()]);
    let last = first + width(&pill) - 1;
    assert_eq!(click(&mut app, row, first - 1).1, 5, "left of the pill");
    assert_eq!(click(&mut app, row, last + 1).1, 5, "right of the pill");
    assert_eq!(click(&mut app, row, first).1, 0);
    assert_eq!(click(&mut app, row, last).1, 0);
}

#[test]
fn a_click_on_a_shown_editor_row_moves_the_cursor_there() {
    let home = std::env::temp_dir().join(format!("agent-tui-editor-click-{}", std::process::id()));
    let mut app = test_app(&home);
    let _ = std::fs::remove_dir_all(&home);
    let draft: Vec<String> = (1..=12).map(|n| format!("l{n}")).collect();
    app.editor.set(&draft.join("\n"));
    let rows: Vec<String> = app.frame(80, 30).0.iter().map(text).collect();
    assert!(!rows.iter().any(|row| row.trim_end() == "  l1"), "12 rows do not fit in the editor: {rows:?}");
    let row = rows.iter().position(|row| row.trim_end() == "  l5").expect("l5 is shown") as u16;
    app.mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), row, column: 4, modifiers: KeyModifiers::NONE });
    app.mouse(MouseEvent { kind: MouseEventKind::Up(MouseButton::Left), row, column: 4, modifiers: KeyModifiers::NONE });
    app.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
    assert!(app.editor.text().contains("\nl5x\n"), "{}", app.editor.text());
}

#[test]
fn a_drag_outside_the_editor_clamps_to_its_edge_row() {
    let layout = Layout { input_top: 20, input_rows: 3, input_below: 2, ..Layout::default() };
    assert_eq!(layout.input_point(0, 7), Point { line: 4, column: 7 }, "above the editor: its first shown row");
    assert_eq!(layout.input_point(22, 7), Point { line: 2, column: 7 });
    assert_eq!(layout.input_point(40, 7), Point { line: 2, column: 7 }, "below the editor: its last shown row");
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
fn plugin_rows_keep_the_status_when_a_narrow_terminal_cuts_the_description() {
    let home = std::env::temp_dir().join(format!("agent-tui-plugins-{}", std::process::id()));
    let app = test_app(&home);
    std::fs::remove_dir_all(&home).ok();
    let lines: Vec<String> = app.render_panel(&Panel::Plugins { index: 0, confirm: false }, 40).iter().map(text).collect();
    let version = crate::plugins::Plugins::new(&home).list()[0].version.clone();
    assert!(lines[2].starts_with(&format!("→ usagefleet  {version}    available  ")), "{lines:?}");
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
fn unknown_tool_args_read_as_key_values() {
    let input = json!({"action": "create", "subject": "Manifest: a, b", "ids": [1, 2], "q": [{"header": "Panel"}]});
    assert_eq!(tool_args("Todo", &input, Path::new("/repo")), "action: create · ids: [1, 2] · q: [{header: Panel}] · subject: Manifest: a, b");
}

fn text(line: &Line) -> String {
    line.iter().map(|s| s.text.as_str()).collect()
}

#[test]
fn status_counts_reads_changed_files_and_commits_ahead_and_behind() {
    let status = "# branch.oid abc\n# branch.head main\n# branch.upstream origin/main\n# branch.ab +2 -3\n1 .M N... 100644 100644 100644 a b src/a.rs\n? new.rs\n";
    assert_eq!(status_counts(status), (2, 2, 3));
    assert_eq!(status_counts("# branch.oid abc\n# branch.head main\n"), (0, 0, 0));
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
fn working_glyph_fills_only_while_the_band_passes_it() {
    let lit = shimmer_line(WORKING, 3);
    assert_eq!(text(&lit), " ✦ Working");
    assert_eq!(lit[1].style, Style::fg(255));

    let later = shimmer_line(WORKING, 10);
    assert_eq!(text(&later), " ✧ Working");
    assert_eq!(later[1].style, DIM);
    assert_eq!(later[3 + 5].style, Style::fg(255));
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
fn recording_line_shows_minutes_and_seconds() {
    let line = recording_line(Duration::from_secs(75));
    assert_eq!(text(&line), " ● 1:15  Enter to transcribe · Esc to cancel");
    assert_eq!(line[0].style, ERROR);
}

#[test]
fn changing_a_settings_row_changes_that_row_and_no_other() {
    let home = std::env::temp_dir().join(format!("agent-settings-rows-{}", std::process::id()));
    let mut app = test_app(&home);
    // The first settings change configures the client, which is what the Message threads row reads.
    app.client.configure(&app.settings);
    let all = all_settings();
    use Setting::*;
    // These open their own panel; Ponytail and Caveman would download their rules.
    let skipped = [Model, CondenseModel, FallbackModel, ReviewerModel, SubagentModel, SmallModel, ContextWindow, HeadroomUrl, HeadroomToken, Ponytail, Caveman];
    let values = |app: &App| all.iter().map(|&row| text(&app.setting_value(row))).collect::<Vec<_>>();
    let mut changed = Vec::new();
    for &row in all.iter().filter(|row| !skipped.contains(row)) {
        let before = values(&app);
        app.change_setting(row, true);
        let after = values(&app);
        changed.push((row, (0..after.len()).filter(|&index| before[index] != after[index]).map(|index| all[index]).collect::<Vec<_>>()));
    }
    let _ = std::fs::remove_dir_all(&home);
    let own: Vec<(Setting, Vec<Setting>)> = all.iter().filter(|row| !skipped.contains(row)).map(|&row| (row, vec![row])).collect();
    assert_eq!(changed, own);
}

/// Every settings row, in declaration order.
fn all_settings() -> Vec<Setting> {
    let mut rows: Vec<Setting> = SETTING_SECTIONS.iter().flat_map(|(_, rows)| rows.iter().copied()).collect();
    rows.sort_by_key(|&row| row as usize);
    rows
}

#[test]
fn settings_rows_keep_their_labels() {
    let home = std::env::temp_dir().join(format!("agent-settings-labels-{}", std::process::id()));
    type Field = fn(&Settings) -> String;
    let fields: [(&str, Field); 15] = [
        ("Thinking level", |s| s.thinking.to_string()),
        ("Auto-compact", |s| s.auto_compact.to_string()),
        ("Compaction reserve", |s| s.reserve_tokens.to_string()),
        ("Keep recent context", |s| s.keep_recent_tokens.to_string()),
        ("Condense tool output", |s| s.condense.to_string()),
        ("rtk", |s| s.rtk.to_string()),
        ("Headroom proxy", |s| s.headroom.to_string()),
        ("Notifications", |s| s.notifications.to_string()),
        ("Copy on select", |s| s.copy_on_select.to_string()),
        ("Update channel", |s| s.update_channel.to_string()),
        ("Speed", |s| s.speed.to_string()),
        ("Prompt cache TTL", |s| s.cache_ttl.to_string()),
        ("Cost in footer", |s| s.footer_cost.to_string()),
        ("Message threads", |s| s.message_threads.to_string()),
        ("Output style", |s| s.output_style.to_string()),
    ];
    let mut unchanged = Vec::new();
    for (label, field) in fields {
        let mut app = test_app(&home);
        let before = field(&app.settings);
        app.change_setting(*all_settings().iter().find(|row| row.name() == label).unwrap(), true);
        if field(&app.settings) == before {
            unchanged.push(label);
        }
    }
    let _ = std::fs::remove_dir_all(&home);
    assert_eq!(unchanged, [""; 0], "these rows change another setting");
}

#[test]
fn settings_sections_hold_every_row_once_and_search_spans_them() {
    let rows: Vec<usize> = all_settings().into_iter().map(|row| row as usize).collect();
    assert_eq!(rows, (0..=Setting::OutputStyle as usize).collect::<Vec<_>>());
    assert_eq!(matching_settings(Setting::HeadroomUrl, ""), SETTING_SECTIONS[3].1);
    assert_eq!(matching_settings(Setting::Model, "CONTEXT WIN"), [Setting::ContextWindow]);
    assert_eq!(matching_settings(Setting::Model, "headroom"), [Setting::HeadroomProxy, Setting::HeadroomUrl, Setting::HeadroomToken]);
    assert!(matching_settings(Setting::Model, "no such setting").is_empty());
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
fn context_window_accepts_100k_to_1m() {
    assert_eq!(parse_context_window("100k"), Some(100_000));
    assert_eq!(parse_context_window("1M"), Some(1_000_000));
    assert_eq!(parse_context_window("250000"), Some(250_000));
    assert_eq!(parse_context_window("99k"), None);
    assert_eq!(parse_context_window("2m"), None);
    assert_eq!(parse_context_window("lots"), None);
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
    assert_eq!(rows, ["✓ Update Todos", "  ⎿  ✓ review", "     • fix", "     ○ test"]);
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
fn a_run_of_tool_calls_is_one_row_that_counts_them_by_kind() {
    let call = |name: &str, input: Value, output: Option<&str>| Item::Tool {
        id: "t".into(),
        name: name.into(),
        input,
        result: output.map(|output| (output.to_string(), false)),
        progress: None,
        started: None,
    };
    let mut calls = vec![
        call("Read", json!({"file_path": "a.rs"}), Some("ok")),
        call("Read", json!({"file_path": "a.rs", "offset": 40}), Some("ok")),
        call("Read", json!({"paths": ["b.rs", {"file_path": "c.rs"}]}), Some("ok")),
        call("Grep", json!({"pattern": "x"}), Some("ok")),
        call("Edit", json!({"file_path": "a.rs"}), Some("ok")),
        call("Bash", json!({"command": "cargo test"}), Some("ok")),
    ];
    let rows = |calls: &[Item]| tool_run_lines(&calls.iter().collect::<Vec<_>>(), 120, Path::new("/repo"), None).iter().map(text).collect::<Vec<_>>();
    assert_eq!(rows(&calls), ["✓ Read 3 files, searched for 1 pattern, ran 1 command, edited 1 file (ctrl+o to expand)"]);
    calls.push(call("Bash", json!({"command": "cargo clippy"}), None));
    assert_eq!(rows(&calls), ["• Read 3 files, searched for 1 pattern, running 2 commands, edited 1 file… (ctrl+o to expand)", "└ Bash cargo clippy"]);
}

#[test]
fn folding_spans_collapsed_thinking_and_leaves_failed_and_lone_calls_their_rows() {
    let entry = |item: Item| Entry { item, cache: None };
    let call =
        |name: &str, failed: bool| entry(Item::Tool { id: "t".into(), name: name.into(), input: json!({"file_path": "a.rs"}), result: Some(("ok".into(), failed)), progress: None, started: None });
    let thinking = entry(Item::Assistant { text: String::new(), thinking: "hm".into() });
    let mut entries = [call("Read", false), thinking, call("Edit", false), call("Read", true), call("Write", false)];
    let folded = folded_tools(&mut entries, 80, Path::new("/repo"), None);
    assert_eq!(text(&folded[&0][0]), "✓ Read 1 file, edited 1 file (ctrl+o to expand)");
    assert!(folded[&2].is_empty());
    assert_eq!(folded.len(), 2);
}

#[test]
fn the_transcript_folds_a_run_of_tool_calls_until_expanded() {
    let home = std::env::temp_dir().join(format!("agent-tui-fold-{}", std::process::id()));
    let mut app = test_app(&home);
    std::fs::remove_dir_all(&home).ok();
    for path in ["a.rs", "b.rs"] {
        app.push(Item::Tool { id: path.into(), name: "Read".into(), input: json!({"file_path": path}), result: Some(("ok".into(), false)), progress: None, started: None });
    }
    let tail = |app: &mut App, rows: usize| app.transcript_tail(80, rows).0.iter().map(text).collect::<Vec<_>>();
    assert_eq!(tail(&mut app, 1), ["✓ Read 2 files (ctrl+o to expand)"]);
    app.expanded = true;
    assert_eq!(tail(&mut app, 4), ["✓ Read a.rs", "└ ok", "✓ Read b.rs", "└ ok"]);
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
fn context_use_turns_yellow_at_70_and_red_at_90_percent() {
    assert_eq!([69, 70, 89, 90].map(context_style), [MUTED, WARN, WARN, ERROR]);
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
fn slash_menu_lists_prefix_matches_first_and_closes_after_a_space() {
    let commands: Vec<(String, String)> = ["compact", "skill:code-review", "copy"].iter().map(|n| (n.to_string(), String::new())).collect();
    let names = |text: &str| matching_commands(&commands, text).into_iter().map(|c| c.0).collect::<Vec<_>>();
    assert_eq!(names("/co"), ["compact", "copy", "skill:code-review"]);
    assert_eq!(names("/"), ["compact", "skill:code-review", "copy"]);
    assert!(names("/copy now").is_empty());
    assert!(names("copy").is_empty());
}

#[test]
fn ultracode_counts_only_as_a_whole_word() {
    assert!(super::mentions_ultracode("ultracode: audit src/routes"));
    assert!(super::mentions_ultracode("please UltraCode this"));
    assert!(!super::mentions_ultracode("ultracoder notes"));
}

#[test]
fn slash_menu_completes_a_slash_typed_in_front_of_existing_text() {
    assert_eq!(slash_query("/skfix the bug", "/sk"), "/sk");
    assert_eq!(slash_query("/thinking m", "/thinking m"), "/thinking m");
    assert_eq!(slash_query("fix the bug /sk", "fix the bug /sk"), "fix the bug /sk");
}

#[test]
fn a_slash_word_after_text_offers_skills_but_a_path_or_shell_line_does_not() {
    assert_eq!(mid_text_skill("fix the bug /co"), Some("co"));
    assert_eq!(mid_text_skill("fix the bug /"), Some(""));
    assert_eq!(mid_text_skill("/co"), None);
    assert_eq!(mid_text_skill("look at /tmp/x"), None);
    assert_eq!(mid_text_skill("!ls /tm"), None);
    assert_eq!(mid_text_skill("fix /co the bug"), None);
}

#[test]
fn a_leading_slash_skill_runs_as_skill_and_a_command_name_stays_a_command() {
    let is_skill = |name: &str| name == "code-review";
    assert_eq!(as_skill_command("/code-review src/".into(), is_skill), "/skill:code-review src/");
    assert_eq!(as_skill_command("/code-review".into(), is_skill), "/skill:code-review");
    assert_eq!(as_skill_command("/compact".into(), is_skill), "/compact");
    assert_eq!(as_skill_command("fix /code-review".into(), is_skill), "fix /code-review");
}

#[test]
fn slash_menu_lists_known_arguments_after_the_name() {
    let names = |text: &str| matching_commands(&[], text).into_iter().map(|c| c.0).collect::<Vec<_>>();
    assert_eq!(names("/thinking m"), ["thinking minimal", "thinking medium", "thinking max"]);
    assert_eq!(names("/permissions "), ["permissions ask", "permissions auto", "permissions full access"]);
    assert_eq!(names("/plan e"), ["plan export", "plan exit"]);
    assert_eq!(names("/workspace r"), ["workspace remove"]);
    assert!(names("/thinking medium ").is_empty());
}

#[test]
fn at_menu_lists_a_directory_then_nested_names_with_folders_first_and_dotfiles_without_the_dot() {
    let root = std::env::temp_dir().join(format!("agent-at-{}", std::process::id()));
    std::fs::create_dir_all(root.join("src/tui")).unwrap();
    std::fs::create_dir_all(root.join(".git")).unwrap();
    for file in ["b.md", ".env", "src/main.rs", "src/.envrc"] {
        std::fs::write(root.join(file), "").unwrap();
    }
    let paths = |query: &str| matching_paths(&root, query).into_iter().map(|p| p.0).collect::<Vec<_>>();
    assert_eq!(paths(""), ["src/", ".env", "b.md"]);
    assert_eq!(paths("."), [".git/", ".env", "src/.envrc"]);
    assert_eq!(paths("env"), [".env", "src/.envrc"]);
    assert_eq!(paths("src/e"), ["src/.envrc"]);
    assert_eq!(paths("src/"), ["src/tui/", "src/.envrc", "src/main.rs"]);
    assert_eq!(paths("src/M"), ["src/main.rs"]);
    assert_eq!(paths("tu"), ["src/tui/"]);
    assert_eq!(paths("m"), ["src/main.rs"]);
    assert!(paths("nope/").is_empty());
    std::fs::remove_dir_all(root).unwrap();
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
fn model_search_matches_provider_account_or_id_ignoring_case() {
    let model = |provider: &str, id: &str, accounts: &[&str]| Model { provider: provider.into(), id: id.into(), accounts: accounts.iter().map(|a| a.to_string()).collect(), ..Model::default() };
    let catalog = [model("anthropic", "claude-haiku-5-5", &["work"]), model("openai-codex", "gpt-6-luna", &["pro", "plus"]), model("openai-codex", "gpt-6-sol", &["pro"])];
    let mut settings = settings::load(Path::new("/nonexistent")).unwrap();
    let ids = |settings: &Settings, row, query| matching_models(&catalog, settings, row, query).iter().map(|m| m.map_or("auto", |m| m.id.as_str()).to_string()).collect::<Vec<_>>();
    assert_eq!(ids(&settings, Setting::Model, "HAIKU"), ["claude-haiku-5-5"]);
    assert_eq!(ids(&settings, Setting::Model, "codex"), ["gpt-6-luna", "gpt-6-sol"]);
    assert_eq!(ids(&settings, Setting::Model, "plus"), ["gpt-6-luna"]);
    assert_eq!(model_owners(&catalog[1]), "openai-codex · pro, plus");
    assert_eq!(ids(&settings, Setting::Model, "").len(), catalog.len());
    assert_eq!(ids(&settings, Setting::CondenseModel, "").len(), catalog.len() + 1);
    assert_eq!(ids(&settings, Setting::FallbackModel, "aut"), ["auto", "gpt-6-luna"]);
    assert!(ids(&settings, Setting::CondenseModel, "xyz").is_empty());
    settings.provider_sort = "manual".into();
    settings.provider_order = vec!["openai-codex".into()];
    assert_eq!(ids(&settings, Setting::CondenseModel, ""), ["auto", "gpt-6-luna", "gpt-6-sol", "claude-haiku-5-5"]);
    settings.pinned_models = vec!["openai-codex/gpt-6-sol".into()];
    assert_eq!(ids(&settings, Setting::CondenseModel, ""), ["auto", "gpt-6-sol", "gpt-6-luna", "claude-haiku-5-5"]);
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
fn rewind_entry_finds_the_chosen_prompt_among_repeats_and_bash_output() {
    let entry = |item: Item| Entry { item, cache: None };
    let user = |text: &str| entry(Item::User(text.into()));
    let entries = [user("go"), entry(Item::Lines(Vec::new())), user("Ran `ls`"), user("go"), user("done?")];
    let point = |at: usize, text: &str| RewindPoint { at, text: text.into(), images: Vec::new() };
    let points = [point(0, "go"), point(2, "go"), point(4, "done?")];
    assert_eq!(rewind_entry(&entries, &points, 0), Some(0));
    assert_eq!(rewind_entry(&entries, &points, 1), Some(3));
}

#[test]
fn only_notices_after_the_prompt_leave_it_unanswered() {
    let entry = |item: Item| Entry { item, cache: None };
    let prompt = entry(Item::User("go".into()));
    let notice = entry(Item::System { source: "models".into(), text: "retrying".into(), tone: Tone::Info });
    let thinking = entry(Item::Assistant { text: String::new(), thinking: "hm".into() });
    assert!(!answered(&[prompt, entry(Item::Images(Vec::new())), notice]));
    let prompt = entry(Item::User("go".into()));
    assert!(answered(&[prompt, thinking]));
}

#[test]
fn image_slots_put_kept_images_back_under_their_tags() {
    let (two, three) = (json!("png2"), json!("png3"));
    let slots = image_slots("see [Image #2] and [Image #3]", vec![two.clone(), three.clone()]);
    assert_eq!(slots, [Value::Null, two, three]);
}

#[test]
fn image_tags_sit_right_under_their_prompt() {
    let prompt = Item::User("see [Image #1]".into());
    let images = Item::Images(vec!["[Image #1]".into()]);
    assert!(!blank_between(&prompt, &images));
    assert!(blank_between(&images, &Item::User("next".into())));
    assert!(blank_between(&prompt, &Item::User("next".into())));
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
fn a_long_turn_resends_the_progress_bar_before_the_terminal_drops_it() {
    let start = Instant::now();
    assert_eq!(progress_due(false, None, start), None);
    assert_eq!(progress_due(true, None, start), Some(true));
    assert_eq!(progress_due(true, Some(start), start + Duration::from_secs(1)), None);
    assert_eq!(progress_due(true, Some(start), start + PROGRESS_REFRESH), Some(true));
    assert_eq!(progress_due(false, Some(start), start + Duration::from_secs(1)), Some(false));
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

#[test]
fn account_rows_group_subscriptions_and_api_keys_in_shared_columns() {
    let account = |name: &str, oauth: bool, id: Option<&str>, usage: Option<(Option<f64>, f64)>| AccountView {
        provider: "anthropic".into(),
        name: name.into(),
        oauth,
        account_id: id.map(String::from),
        usage: usage.map(|(five_hour, seven_day)| crate::api::accounts::Usage { five_hour, seven_day, resets_at: None, models: Vec::new() }),
        current: name == "claudescom",
        pinned: name == "rokartur",
        status: AccountStatus::Unchecked,
    };
    let quota = |model: &str, used| crate::api::accounts::ModelQuota { model: model.into(), used, resets_at: None };
    let gemini = AccountView {
        provider: "google-antigravity".into(),
        usage: Some(crate::api::accounts::Usage { five_hour: None, seven_day: 80.0, resets_at: None, models: vec![quota("gemini-2.5-flash", 10.0), quota("gemini-2.5-pro", 80.0)] }),
        ..account("gemini", true, Some("proj-1"), None)
    };
    let accounts = [
        AccountView { status: AccountStatus::Working, ..account("claudescom", true, Some("uuid-1"), Some((Some(21.0), 73.0))) },
        AccountView { status: AccountStatus::Failed("Usage request failed (401).\nbody".into()), ..account("rokartur", true, Some("uuid-2"), None) },
        AccountView { status: AccountStatus::Limited, ..account("ultra", true, Some("uuid-3"), Some((None, 41.0))) },
        AccountView { status: AccountStatus::Checking, ..gemini },
        account("claudescom-api", false, None, None),
    ];
    let labels = [("usagefleet".to_string(), HashMap::from([("uuid-1".to_string(), "uf_tVdC…".to_string())]))];
    let (rows, account_rows) = account_rows(&accounts, &labels);
    let table: Vec<(&str, String)> = rows.iter().map(|row| (row.left.as_str(), text(&row.right).trim_end().to_string())).collect();
    assert_eq!(
        table,
        [
            ("  Subscription", "Provider             5h   7d  Pinned  usagefleet  Status".to_string()),
            ("✓ claudescom", "anthropic           21%  73%          uf_tVdC…    ok".to_string()),
            ("  rokartur", "anthropic             ?    ?  pinned              Usage request failed (401).".to_string()),
            ("  ultra", "anthropic             -  41%                      rate limited".to_string()),
            ("  gemini", "google-antigravity    -    -                      checking…".to_string()),
            ("      gemini-2.5-flash 10%", String::new()),
            ("      gemini-2.5-pro 80%", String::new()),
            ("", String::new()),
            ("  API key", "Provider                      Pinned              Status".to_string()),
            ("  claudescom-api", "anthropic".to_string()),
        ]
    );
    assert_eq!(account_rows, [1, 2, 3, 4, 9]);
    assert_eq!([1, 2].map(|row| rows[row].right.last().unwrap().style), [OK, ERROR]);
    let names = |query| matching_accounts(accounts.to_vec(), query).into_iter().map(|a| a.name).collect::<Vec<_>>();
    assert_eq!(names("API"), ["claudescom-api", "claudescom", "rokartur", "ultra"]);
    assert_eq!(names("anthropic").len(), 4);
}
