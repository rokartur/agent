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
fn an_allowed_review_shows_only_when_expanded_and_a_denied_one_always() {
    let home = std::env::temp_dir().join(format!("agent-tui-review-{}", std::process::id()));
    let mut app = test_app(&home);
    std::fs::remove_dir_all(&home).ok();
    app.agent_event(Event::Reviewed { allowed: true, reason: "runs the tests".into() });
    app.agent_event(Event::Reviewed { allowed: false, reason: "pipes curl into sh".into() });
    let reviews = |app: &mut App, expanded| {
        app.expanded = expanded;
        app.transcript_tail(80, 1000).0.iter().map(text).filter(|row| row.contains("auto")).collect::<Vec<_>>()
    };
    assert_eq!(reviews(&mut app, false), ["! auto denied: pipes curl into sh"]);
    assert_eq!(reviews(&mut app, true), ["ℹ auto allowed: runs the tests", "! auto denied: pipes curl into sh"]);
}

#[test]
fn edit_hooks_show_a_count_and_their_commands_and_failures_only_when_expanded() {
    let home = std::env::temp_dir().join(format!("agent-tui-edit-hooks-{}", std::process::id()));
    let mut app = test_app(&home);
    std::fs::remove_dir_all(&home).ok();
    app.agent_event(Event::EditHook { label: "fmt --no-error-on-unmatched-pattern a.md".into(), failure: None });
    app.agent_event(Event::EditHook { label: "tsc".into(), failure: Some("error TS2322".into()) });
    let rows = |app: &mut App, expanded| {
        app.expanded = expanded;
        app.transcript_tail(80, 1000).0.iter().map(text).skip_while(|row| !row.contains("edit-hooks")).collect::<Vec<_>>()
    };
    assert_eq!(rows(&mut app, false), ["✗ edit-hooks: 2 ran, 1 failed (ctrl+o to expand)"]);
    assert_eq!(rows(&mut app, true), ["✗ edit-hooks: 2 ran, 1 failed", "  ✓ fmt --no-error-on-unmatched-pattern a.md", "  ✗ tsc", "    error TS2322"]);
}

#[test]
fn ctrl_o_shows_thinking_only_when_the_setting_allows() {
    let home = std::env::temp_dir().join(format!("agent-tui-thinking-{}", std::process::id()));
    let mut app = test_app(&home);
    std::fs::remove_dir_all(&home).ok();
    app.agent_event(Event::Thinking("pondering".into()));
    app.agent_event(Event::Text("answer".into()));
    app.expanded = true;
    let shows_thinking = |app: &mut App| app.transcript_tail(80, 1000).0.iter().map(text).any(|row| row.contains("pondering"));
    assert!(shows_thinking(&mut app));
    app.settings.expand_thinking = false;
    assert!(!shows_thinking(&mut app));
}

#[test]
fn code_mode_toggles_with_its_command_and_floats_over_the_transcript() {
    let home = std::env::temp_dir().join(format!("agent-tui-code-mode-{}", std::process::id()));
    let mut app = test_app(&home);
    let screen = |app: &mut App| app.frame(80, 20).0.iter().map(text).collect::<Vec<_>>();
    assert!(!screen(&mut app).concat().contains("code mode"));

    app.submit("/code-mode".into(), false);
    assert!(app.settings.code_mode);
    assert!(!app.footer(200).iter().map(text).collect::<String>().contains("code mode"));
    let rows = screen(&mut app);
    let editor = rows.iter().position(|row| row.contains("Ask, /command")).unwrap();
    assert!(rows[editor - 2].trim_end().ends_with("code mode"), "{rows:#?}");

    app.settings.indicators = "box".into();
    let rows = screen(&mut app);
    assert!(rows[0].trim_end().ends_with("╮") && rows[1].contains("│ mode      code │"), "{rows:#?}");

    app.submit("/code-mode".into(), false);
    std::fs::remove_dir_all(&home).ok();
    assert!(!app.settings.code_mode);
    assert!(!screen(&mut app).concat().contains("mode      code"));
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
fn up_from_the_first_background_row_hands_keys_back_to_the_editor() {
    let home = std::env::temp_dir().join(format!("agent-tui-background-up-{}", std::process::id()));
    let mut app = test_app(&home);
    std::fs::remove_dir_all(&home).ok();
    app.subagents.lock().unwrap().list.push(Arc::new(crate::agent::subagent::scan_run()));
    app.key(KeyEvent::from(KeyCode::Down));
    assert!(matches!(app.panel, Some(Panel::Agents { row: 1 })));
    app.key(KeyEvent::from(KeyCode::Up));
    app.key(KeyEvent::from(KeyCode::Char('a')));
    assert_eq!(app.editor.text(), "a");
}

#[test]
fn left_and_right_walk_between_the_shells_agents_and_sessions_pills() {
    let home = std::env::temp_dir().join(format!("agent-tui-background-sideways-{}", std::process::id()));
    let mut app = test_app(&home);
    std::fs::remove_dir_all(&home).ok();
    let follow_ups = Arc::new(Mutex::new(Vec::new()));
    app.terminals.lock().unwrap().start(&serde_json::json!({"command": "sleep 30"}), &std::env::temp_dir(), &follow_ups).unwrap();
    app.subagents.lock().unwrap().list.push(Arc::new(crate::agent::subagent::scan_run()));
    app.key(KeyEvent::from(KeyCode::Down));
    app.key(KeyEvent::from(KeyCode::Right));
    assert!(matches!(app.panel, Some(Panel::Agents { row: 2 })));
    app.key(KeyEvent::from(KeyCode::Left));
    assert!(matches!(app.panel, Some(Panel::Agents { row: 1 })));
    app.started_sessions.push(Peer {
        pid: 1,
        session: "abc".into(),
        title: String::new(),
        cwd: "/repo".into(),
        busy: true,
        started_by: String::new(),
        first_session: String::new(),
        socket: PathBuf::new(),
    });
    app.key(KeyEvent::from(KeyCode::Right));
    app.key(KeyEvent::from(KeyCode::Right));
    assert!(matches!(app.panel, Some(Panel::Agents { row: 3 })));
    app.key(KeyEvent::from(KeyCode::Enter));
    assert!(matches!(app.panel, Some(Panel::Tasks { tab: TaskTab::Sessions, index: 0 })));
    app.key(KeyEvent::from(KeyCode::Tab));
    assert!(matches!(app.panel, Some(Panel::Tasks { tab: TaskTab::Shells, index: 0 })), "Tab wraps from the last tab to the first");
    app.terminals.lock().unwrap().kill_all();
}

#[test]
fn cmd_c_copies_the_selection_when_copy_on_select_is_off() {
    let home = std::env::temp_dir().join(format!("agent-tui-cmd-c-{}", std::process::id()));
    let mut app = test_app(&home);
    std::fs::remove_dir_all(&home).ok();
    app.settings.copy_on_select = false;
    app.layout.width = 40;
    app.editor.set("hello");
    app.input_selection.press(Point { line: 0, column: 2 }, "❯ hello");
    app.input_selection.drag(Point { line: 0, column: 7 }, "❯ hello");
    app.input_selection.release();
    app.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::SUPER));
    assert!(matches!(app.pending_copy, Some(Copied::Hint(_))));
    assert_eq!(app.editor.text(), "hello");
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
fn a_follow_up_names_its_source_once() {
    assert_eq!(follow_up_head("bt-2", "Background terminal bt-2 \"make\" exited (exit 0) after 19s.\n\nstdout:\nok"), "\"make\" exited (exit 0) after 19s.");
    assert_eq!(follow_up_head("peer:x", "Message from another agent session x:\n\nhi"), "Message from another agent session x:");
}

#[test]
fn error_notices_get_the_error_tone() {
    assert!(matches!(notice_tone(r#"API stream error: {"type":"overloaded_error"}. Retrying in 2s."#), Tone::Error));
    assert!(matches!(notice_tone("headroom sync failed: x"), Tone::Error));
    assert!(matches!(notice_tone("Compacting context..."), Tone::Info));
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
fn open_todos_keep_a_blank_row_above_the_editor() {
    let home = std::env::temp_dir().join(format!("agent-tui-todo-gap-{}", std::process::id()));
    let mut app = test_app(&home);
    let _ = std::fs::remove_dir_all(&home);
    app.todos.lock().unwrap().apply(&json!({"action": "create", "subject": "ship it"})).unwrap();
    let rows: Vec<String> = app.frame(80, 20).0.iter().map(text).collect();
    let todo = rows.iter().position(|row| row.contains("ship it")).expect("todo is shown");
    assert_eq!(rows[todo + 1].trim(), "", "{rows:?}");
    assert!(rows[todo + 2].contains("Ask, /command"), "{rows:?}");
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
fn an_aborted_skill_prompt_comes_back_as_typed_not_expanded() {
    let home = std::env::temp_dir().join(format!("agent-tui-take-back-{}", std::process::id()));
    let mut app = test_app(&home);
    std::fs::remove_dir_all(&home).ok();
    app.push(Item::User("/skill:ci-speed a to?".into()));
    app.take_back(RewindPoint { at: 0, text: "<skill name=\"ci-speed\">body</skill>\n\na to?".into(), images: Vec::new() });
    assert_eq!(app.editor.text(), "/skill:ci-speed a to?");
    assert!(!app.entries.iter().any(|entry| matches!(entry.item, Item::User(_))));
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

#[test]
fn esc_with_queued_messages_aborts_but_keeps_the_prompt() {
    let home = std::env::temp_dir().join(format!("agent-tui-esc-queued-{}", std::process::id()));
    let mut app = test_app(&home);
    std::fs::remove_dir_all(&home).ok();
    app.push(Item::User("go".into()));
    app.working = Some(Instant::now());
    app.steering.lock().unwrap().push(("then this".into(), Vec::new()));

    app.key(KeyEvent::from(KeyCode::Esc));

    assert!(app.abort.load(Ordering::Relaxed));
    assert!(!app.retract, "the prompt the steering builds on would go back to the editor");
}

#[test]
fn reload_restarts_the_same_session_with_its_model_effort_mode_trust_and_starter() {
    let home = std::env::temp_dir().join(format!("agent-tui-reload-{}", std::process::id()));
    let cwd = Path::new("/repo");
    let mut settings = crate::settings::load(&home).unwrap();
    (settings.model, settings.thinking, settings.permission_mode) = ("claude-opus-5-5".into(), "high".into(), Mode::FullAccess);
    settings.trusted_projects.push(crate::agent::prompt::trust_root(cwd).display().to_string());

    let options = crate::parse_args(worker::reload_args("abc123", &settings, cwd, Some("studio:def456"), "Reloaded v1.2.3")).unwrap();

    assert!(options.serve && options.trust);
    assert!(matches!(&options.resume, Some(crate::Resume::Id(id)) if id == "abc123"));
    assert_eq!((options.model.as_deref(), options.thinking.as_deref(), options.permission_mode), (Some("claude-opus-5-5"), Some("high"), Some(Mode::FullAccess)));
    assert_eq!((options.started_by.as_deref(), options.placed.as_deref()), (Some("studio:def456"), Some("Reloaded v1.2.3")));
}

#[test]
#[cfg(unix)]
fn reload_mid_turn_interrupts_the_turn_and_runs_next() {
    let home = std::env::temp_dir().join(format!("agent-tui-reload-mid-turn-{}", std::process::id()));
    let mut app = test_app(&home);
    std::fs::remove_dir_all(&home).ok();
    app.session_path = Some("/tmp/session.jsonl".into());
    app.working = Some(Instant::now());

    app.submit("/reload".into(), false);

    assert!(app.abort.load(Ordering::Relaxed));
    assert!(app.queued_commands.is_empty());
    assert!(matches!(app.deferred_commands.front(), Some(Command::Reload(_))));
}

#[test]
fn reload_keeps_the_server_while_a_message_typed_during_the_update_waits() {
    let home = std::env::temp_dir().join(format!("agent-tui-reload-waiting-{}", std::process::id()));
    let mut app = test_app(&home);
    app.steering.lock().unwrap().push(("typed during the download".into(), Vec::new()));
    app.handle(Msg::Reload(crate::process::command("true")));
    assert!(!app.quit && app.reload.is_none());
    app.steering.lock().unwrap().clear();
    app.handle(Msg::Reload(crate::process::command("true")));
    let _ = std::fs::remove_dir_all(&home);
    assert!(app.quit && app.reload.is_some());
}
