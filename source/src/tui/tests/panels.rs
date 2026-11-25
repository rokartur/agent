use super::*;

#[test]
fn step_wraps_and_starts_over_from_an_unlisted_value() {
    let models = ["a", "b", "c"];
    assert_eq!(step(&models, &"c", true), "a");
    assert_eq!(step(&models, &"x", true), "a");
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
    let fields: [(&str, Field); 16] = [
        ("Working animation", |s| s.working_animation.to_string()),
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
    assert_eq!(rows, (0..=Setting::MaxSessions as usize).collect::<Vec<_>>());
    assert_eq!(matching_settings(Setting::HeadroomUrl, ""), SETTING_SECTIONS[3].1);
    assert_eq!(matching_settings(Setting::Model, "CONTEXT WIN"), [Setting::ContextWindow]);
    assert_eq!(matching_settings(Setting::Model, "headroom"), [Setting::HeadroomProxy, Setting::HeadroomUrl, Setting::HeadroomToken]);
    assert!(matching_settings(Setting::Model, "no such setting").is_empty());
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
fn account_rows_group_subscriptions_and_api_keys_in_shared_columns() {
    let account = |name: &str, oauth: bool, id: Option<&str>, usage: Option<(Option<f64>, f64)>| AccountView {
        provider: "anthropic".into(),
        name: name.into(),
        oauth,
        account_id: id.map(String::from),
        usage: usage.map(|(five_hour, seven_day)| crate::api::accounts::Usage { five_hour, seven_day, resets_at: None, models: Vec::new(), windows: Vec::new() }),
        current: name == "claudescom",
        pinned: name == "rokartur",
        status: AccountStatus::Unchecked,
    };
    let quota = |model: &str, used| crate::api::accounts::ModelQuota { model: model.into(), used, resets_at: None };
    let gemini = AccountView {
        provider: "google-antigravity".into(),
        usage: Some(crate::api::accounts::Usage {
            five_hour: None,
            seven_day: 80.0,
            resets_at: None,
            models: vec![quota("gemini-2.5-flash", 10.0), quota("gemini-2.5-pro", 80.0)],
            windows: Vec::new(),
        }),
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

#[test]
fn limits_draw_claude_codes_bars_and_reset_times() {
    use crate::api::accounts::{Usage, Window};
    let windows = vec![Window { title: "Current session", used: 29.6, resets_at: None }, Window { title: "Current week (all models)", used: 100.0, resets_at: None }];
    let usage = Usage { five_hour: Some(29.6), seven_day: 100.0, resets_at: None, models: Vec::new(), windows };
    let claude = AccountView { provider: "anthropic".into(), name: "a".into(), oauth: true, account_id: None, usage: Some(usage), current: true, pinned: false, status: AccountStatus::Working };
    let lines: Vec<String> = limits_panel(std::slice::from_ref(&claude), 100, 0).iter().map(text).collect();
    let bar = format!("{}▉{}", "█".repeat(14), " ".repeat(35));
    let full = "█".repeat(50);
    let expected = [
        "",
        "Usage   Limits   Overview  Daily  Session  24 hours  7 days  All",
        "",
        "Current session",
        &format!("{bar} 29% used"),
        "",
        "Current week (all models)",
        &format!("{full} 100% used"),
        "",
        "tab scope     esc close",
    ];
    assert_eq!(lines, expected.map(String::from));
    let narrow: Vec<String> = limits_panel(std::slice::from_ref(&claude), 30, 0).iter().map(text).collect();
    assert_eq!(narrow[3..6], ["Current session", &format!("{}▎{}", "█".repeat(8), " ".repeat(19)), "29% used"]);
    let loading = AccountView { usage: None, status: AccountStatus::Unchecked, ..claude.clone() };
    let failed = AccountView { usage: None, status: AccountStatus::Failed("Usage request failed (401).".into()), ..claude };
    let lines: Vec<String> = limits_panel(&[loading, failed], 100, 0).iter().map(text).collect();
    assert_eq!(lines[2..8], ["", "a (anthropic)", "", "Loading usage data…", "", "a (anthropic)"]);
    assert_eq!(lines[9], "Error: Usage request failed (401).");
    assert_eq!(text(&limits_panel(&[], 100, 0)[3]), "/usage is only available for subscription plans.");
}

#[test]
fn heatmap_draws_codexs_year_of_weeks() {
    let now_ms = 1_790_000_000_000;
    let today = crate::process::local_time(now_ms / 1000).tm_wday as usize;
    let day_ms = 24 * 60 * 60 * 1000;
    let days = usage::Days::from([(usage::local_day(now_ms / 1000), 100), (usage::local_day((now_ms - day_ms) / 1000), 40)]);
    let lines = heatmap(&days, 56, now_ms);
    let texts: Vec<String> = lines.iter().map(text).collect();
    assert_eq!(texts.len(), 10);
    assert!(texts[0].starts_with("    ") && texts[0].chars().count() <= 56, "{texts:?}");
    assert!(texts[1].starts_with(" Su ") && texts[7].starts_with(" Sa "));
    assert!(texts[1..8].iter().all(|row| row.chars().count() == 4 + 26 * 2 - 1), "{texts:?}");
    let shade = |weekday: usize, week: usize| lines[1 + weekday][1 + week * 2].clone();
    assert_eq!(shade(today, 25).style, Style::rgb(0xffe4a8));
    let (yesterday, week) = if today == 0 { (6, 24) } else { (today - 1, 25) };
    assert_eq!(shade(yesterday, week).style, Style::rgb(0x6b6047), "40 of 100 is the second shade");
    assert_eq!((shade(today, 0).text.as_str(), shade(today, 0).style), ("■", Style::rgb(0x242424)));
    if today < 6 {
        assert_eq!(shade(today + 1, 25).text, " ", "future days stay blank");
    }
    assert_eq!(texts[9], "   Less ■ ■ ■ ■ ■ More");
}

#[test]
fn overview_counts_tokens_and_streaks() {
    let now_ms = 1_790_000_000_000;
    let day = |ago: u64| usage::local_day((now_ms - ago * 24 * 60 * 60 * 1000) / 1000);
    // Today is empty, so the current streak runs from yesterday; the longest is the older three days.
    let days = usage::Days::from([(day(1), 1_500), (day(2), 2_000_000), (day(5), 10), (day(6), 10), (day(7), 10)]);
    let lines: Vec<String> = overview_panel(&days, 80, now_ms).iter().map(text).collect();
    let words = |line: &str| line.split("  ").map(str::trim).filter(|word| !word.is_empty()).map(String::from).collect::<Vec<_>>();
    assert_eq!(words(&lines[3]), ["2.0M", "2.0M", "2 days", "3 days"]);
    assert_eq!(words(&lines[4]), ["Lifetime tokens", "Peak tokens", "Current streak", "Longest streak"]);
    assert_eq!(lines[6..8], ["Token activity", "Last 12 months"]);
    assert_eq!(text(&overview_panel(&usage::Days::new(), 80, now_ms)[3]), "No usage recorded yet.");
}

#[test]
fn daily_chart_draws_a_bar_per_day_of_the_week() {
    let now_ms = 1_790_000_000_000;
    let day = |ago: u64| usage::local_day((now_ms - ago * 24 * 60 * 60 * 1000) / 1000);
    let days = usage::Days::from([(day(0), 1_000), (day(3), 500)]);
    let lines: Vec<String> = daily_panel(&days, 80, now_ms).iter().map(text).collect();
    // Axis 5 wide ("500" + 2), 75 / 7 = 10 per day, bars 6 wide.
    let column = |row: &str, day: usize| row.chars().skip(5 + day * 10).take(10).collect::<String>();
    let (top, middle, baseline) = (&lines[7], &lines[12], &lines[17]);
    assert_eq!(column(top, 6), "  ██████  ", "{lines:#?}");
    assert!(top.starts_with(" 1k  "));
    assert_eq!(column(&lines[6], 6), "    1k    ");
    assert_eq!(column(&lines[11], 3), "   500    ");
    assert_eq!(column(middle, 3), "  ██████  ");
    assert!(middle.starts_with("500  "));
    assert_eq!(column(&lines[16], 0), "    0     ");
    assert!(baseline.starts_with("  0  ─") && baseline.ends_with('─'));
    let dates: Vec<&str> = lines[19].split("  ").map(str::trim).filter(|date| !date.is_empty()).collect();
    assert_eq!(dates.len(), 7);
    let year = crate::process::local_time(now_ms / 1000).tm_year + 1900;
    assert_eq!(lines[3], format!("{}-{}, {year}", dates[0], dates[6]));
}

#[test]
fn reset_times_read_like_claude_codes() {
    let tm = |year, month, day, hour, minute| {
        // SAFETY: a zeroed tm is a valid value of that C struct.
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        (tm.tm_year, tm.tm_mon, tm.tm_mday, tm.tm_hour, tm.tm_min) = (year - 1900, month - 1, day, hour, minute);
        tm
    };
    let now = tm(2026, 10, 1, 9, 0);
    assert_eq!(clock_label(&tm(2026, 10, 1, 17, 0), &now, false), "5pm");
    assert_eq!(clock_label(&tm(2026, 10, 1, 0, 30), &now, false), "12:30am");
    assert_eq!(clock_label(&tm(2026, 10, 9, 14, 5), &now, true), "Oct 9, 2:05pm");
    assert_eq!(clock_label(&tm(2027, 1, 2, 12, 0), &now, true), "Jan 2, 2027, 12pm");
}
