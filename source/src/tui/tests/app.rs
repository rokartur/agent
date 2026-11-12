use super::*;

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
fn a_permission_digit_inside_the_settle_time_is_ignored_and_esc_denies() {
    let home = std::env::temp_dir().join(format!("agent-tui-permission-{}", std::process::id()));
    let mut app = test_app(&home);
    std::fs::remove_dir_all(&home).ok();
    let (reply, decision) = mpsc::channel();
    let request = crate::agent::permissions::Request { kind: crate::agent::permissions::Kind::Command, question: "", detail: "ls".into(), grant: "bash:ls".into() };
    app.panel = Some(Panel::Permission { request, reply, index: 0, opened: Instant::now() });
    app.key(KeyEvent::from(KeyCode::Char('1')));
    app.key(KeyEvent::from(KeyCode::Down));
    let Some(Panel::Permission { index, .. }) = &app.panel else { panic!("a digit inside the settle time answered the prompt") };
    assert_eq!(*index, 1);
    app.key(KeyEvent::from(KeyCode::Esc));
    assert!(app.panel.is_none());
    assert!(matches!(decision.try_recv(), Ok(Decision::Deny)));
}

#[test]
fn ctrl_c_on_context_windows_goes_back_to_its_settings_row() {
    let home = std::env::temp_dir().join(format!("agent-tui-ctrl-c-{}", std::process::id()));
    let mut app = test_app(&home);
    std::fs::remove_dir_all(&home).ok();
    app.panel = Some(Panel::ContextWindows { tab: 0, query: Editor::new(), index: 0 });
    app.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert!(matches!(app.panel, Some(Panel::Settings { row: Setting::ContextWindow, .. })));
}

#[test]
fn esc_closes_an_empty_mcp_list_and_the_worktrees_list() {
    let home = std::env::temp_dir().join(format!("agent-tui-esc-{}", std::process::id()));
    let mut app = test_app(&home);
    std::fs::remove_dir_all(&home).ok();
    app.panel = Some(Panel::Mcp { index: 0, confirm: false });
    app.key(KeyEvent::from(KeyCode::Esc));
    assert!(app.panel.is_none());
    let worktrees = vec![crate::worktree::Worktree { path: app.cwd.clone(), branch: "main".into() }];
    app.panel = Some(Panel::Worktrees(WorktreesPanel { confirm: false, index: 0, worktrees, current: app.cwd.clone() }));
    app.key(KeyEvent::from(KeyCode::Esc));
    assert!(app.panel.is_none());
}

#[test]
fn enter_on_an_uninstalled_plugin_keeps_the_pending_confirm() {
    let home = std::env::temp_dir().join(format!("agent-tui-plugin-enter-{}", std::process::id()));
    let mut app = test_app(&home);
    std::fs::remove_dir_all(&home).ok();
    app.panel = Some(Panel::Plugins { index: 0, confirm: true });
    app.key(KeyEvent::from(KeyCode::Enter));
    assert!(matches!(app.panel, Some(Panel::Plugins { confirm: true, .. })));
}

#[test]
fn usage_tab_cycles_its_tabs_and_other_keys_do_nothing() {
    let home = std::env::temp_dir().join(format!("agent-tui-usage-{}", std::process::id()));
    let mut app = test_app(&home);
    std::fs::remove_dir_all(&home).ok();
    app.panel = Some(Panel::Usage { tab: USAGE_TABS.len() - 1, totals: Default::default() });
    app.key(KeyEvent::from(KeyCode::Tab));
    app.key(KeyEvent::from(KeyCode::Char('x')));
    assert!(matches!(app.panel, Some(Panel::Usage { tab: 0, .. })));
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
fn status_counts_reads_changed_files_and_commits_ahead_and_behind() {
    let status = "# branch.oid abc\n# branch.head main\n# branch.upstream origin/main\n# branch.ab +2 -3\n1 .M N... 100644 100644 100644 a b src/a.rs\n? new.rs\n";
    assert_eq!(status_counts(status), (2, 2, 3));
    assert_eq!(status_counts("# branch.oid abc\n# branch.head main\n"), (0, 0, 0));
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
fn ultracode_counts_only_as_a_whole_word() {
    assert!(super::mentions_ultracode("ultracode: audit src/routes"));
    assert!(super::mentions_ultracode("please UltraCode this"));
    assert!(!super::mentions_ultracode("ultracoder notes"));
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
fn a_long_turn_resends_the_progress_bar_before_the_terminal_drops_it() {
    let start = Instant::now();
    assert_eq!(progress_due(false, None, start), None);
    assert_eq!(progress_due(true, None, start), Some(true));
    assert_eq!(progress_due(true, Some(start), start + Duration::from_secs(1)), None);
    assert_eq!(progress_due(true, Some(start), start + PROGRESS_REFRESH), Some(true));
    assert_eq!(progress_due(false, Some(start), start + Duration::from_secs(1)), Some(false));
}
