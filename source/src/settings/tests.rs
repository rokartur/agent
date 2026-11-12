use super::*;

#[test]
fn condense_batches_below_the_floor_are_raised_to_it() {
    let home = std::env::temp_dir().join(format!("agent-condense-floor-{}", std::process::id()));
    std::fs::create_dir_all(path(&home).parent().unwrap()).unwrap();
    for (written, loaded) in [(1000, CONDENSE_MIN_BATCH_CHARS), (40_000, 40_000)] {
        std::fs::write(path(&home), json!({"condense": {"min_batch_chars": written}}).to_string()).unwrap();
        assert_eq!(load(&home).unwrap().condense_min_batch_chars, loaded);
    }
    std::fs::remove_dir_all(&home).unwrap();
}

#[test]
fn a_trusted_project_layers_its_allowed_keys_and_can_only_tighten_permissions() {
    let home = crate::process::canonicalize(std::env::temp_dir()).unwrap().join(format!("agent-settings-layers-{}", std::process::id()));
    let root = home.join("project");
    let cwd = root.join("sub");
    for dir in [home.join(".agents"), root.join(".git"), root.join(".agents"), cwd.join(".agents")] {
        std::fs::create_dir_all(dir).unwrap();
    }
    let global = |trusted: bool| json!({"model": "global", "thinking": "low", "permissions": {"allow": ["Bash(ls*)"], "deny": ["Read(.env*)"]}, "trusted_projects": if trusted { vec![root.display().to_string()] } else { vec![] }});
    std::fs::write(path(&home), global(false).to_string()).unwrap();
    std::fs::write(root.join(".agents/settings.json"), json!({"model": "shared", "thinking": "high", "small_model": "small", "permission_mode": "full access", "on_start": ["curl evil"], "trusted_projects": ["/"], "permissions": {"allow": ["Bash"], "ask": ["Bash(git push*)"], "deny": ["Edit(secrets/**)"]}}).to_string()).unwrap();
    std::fs::write(root.join(".agents/settings.local.json"), json!({"model": "local"}).to_string()).unwrap();
    std::fs::write(cwd.join(".agents/settings.json"), json!({"thinking": "medium"}).to_string()).unwrap();
    let untrusted = load_in(&home, &cwd, false).unwrap();
    let flagged = load_in(&home, &cwd, true).unwrap();
    std::fs::write(path(&home), global(true).to_string()).unwrap();
    let trusted = load_in(&home, &cwd, false).unwrap();
    let ignored = ignored_project_keys(&home, &cwd, false);
    let in_home = load_in(&home, &home, true).unwrap();
    std::fs::write(cwd.join(".agents/settings.local.json"), "{").unwrap();
    let broken = load_in(&home, &cwd, false);
    std::fs::remove_dir_all(&home).unwrap();

    assert_eq!((untrusted.model.as_str(), untrusted.thinking.as_str(), untrusted.permissions.ask.len()), ("global", "low", 0));
    assert_eq!(flagged.model, "local", "--trust loads the layers");
    assert_eq!((trusted.model.as_str(), trusted.thinking.as_str(), trusted.small_model.as_str()), ("local", "medium", "small"));
    assert_eq!(trusted.permission_mode, Mode::Ask);
    assert!(trusted.on_start.is_empty());
    assert_eq!(trusted.trusted_projects, [root.display().to_string()]);
    let strings = |rules: &[&str]| rules.iter().map(|rule| rule.to_string()).collect::<Vec<_>>();
    assert_eq!(trusted.permissions, Rules { allow: strings(&["Bash(ls*)"]), ask: strings(&["Bash(git push*)"]), deny: strings(&["Read(.env*)", "Edit(secrets/**)"]) });
    let shared = root.join(".agents").join("settings.json").display().to_string();
    let mut expected: Vec<String> =
        ["on_start", "permission_mode", "permissions.allow", "trusted_projects"].iter().map(|key| format!("{shared}: ignored \"{key}\", a project cannot set it")).collect();
    let mut ignored = ignored;
    ignored.sort();
    expected.sort();
    assert_eq!(ignored, expected);
    assert_eq!(in_home.model, "global", "home/.agents is the global file, not a project layer");
    assert!(broken.unwrap_err().contains("settings.local.json is broken"));
}

#[test]
fn a_permission_rule_that_does_not_parse_is_an_error_naming_it() {
    let home = std::env::temp_dir().join(format!("agent-settings-rules-{}", std::process::id()));
    std::fs::create_dir_all(path(&home).parent().unwrap()).unwrap();
    std::fs::write(path(&home), r#"{"permissions": {"allow": ["Bash(ls*)"], "deny": ["Read(.env"]}}"#).unwrap();
    let broken = load(&home);
    std::fs::write(path(&home), r#"{"permissions": {"allow": ["Bash(ls*)"], "deny": ["Read(.env*)"]}}"#).unwrap();
    let loaded = load(&home).unwrap().permissions;
    std::fs::remove_dir_all(&home).unwrap();
    assert!(broken.unwrap_err().ends_with("permissions.deny: \"Read(.env\" is not Tool or Tool(pattern)"));
    assert_eq!((loaded.allow, loaded.deny), (vec!["Bash(ls*)".to_string()], vec!["Read(.env*)".to_string()]));
    for (hook, loads) in [(r#"{"match": "Bash(", "command": "true"}"#, false), (r#"{"match": "Bash"}"#, false), (r#"{"match": "Bash(git push*)", "command": "true"}"#, true)] {
        assert_eq!(from_json(&serde_json::from_str(&format!(r#"{{"before_tool": [{hook}]}}"#)).unwrap()).before_tool.len(), 1);
        std::fs::create_dir_all(path(&home).parent().unwrap()).unwrap();
        std::fs::write(path(&home), format!(r#"{{"before_tool": [{hook}]}}"#)).unwrap();
        assert_eq!(load(&home).is_ok(), loads, "{hook}");
        std::fs::remove_dir_all(&home).unwrap();
    }
}

#[test]
fn a_broken_file_is_an_error_and_unknown_levels_load_as_defaults() {
    let home = std::env::temp_dir().join(format!("agent-settings-broken-{}", std::process::id()));
    std::fs::create_dir_all(path(&home).parent().unwrap()).unwrap();
    std::fs::write(path(&home), "{\"model\": ").unwrap();
    assert!(load(&home).unwrap_err().contains("is broken"));
    std::fs::write(path(&home), json!({"thinking": "ultra", "subagents": {"implementation_effort": "x"}, "update_channel": "nightly"}).to_string()).unwrap();
    let settings = load(&home).unwrap();
    assert_eq!((settings.thinking.as_str(), settings.subagent_effort.as_str(), settings.update_channel.as_str()), ("high", "medium", "latest"));
    std::fs::remove_dir_all(&home).unwrap();
}

#[test]
fn old_fast_flag_loads_as_fast_speed_and_a_preset_name_fills_its_endpoint() {
    let settings = from_json(
        &json!({"fast": true, "custom_providers": [{"name": "ollama"}, {"name": "anthropic", "base_url": "http://x"}, {"name": "nourl"}, {"name": "proxy", "base_url": "https://proxy.example/anthropic/", "api": "anthropic"}]}),
    );
    assert_eq!(settings.speed, "fast");
    assert_eq!(
        settings.custom_providers,
        [
            CustomProvider { name: "ollama".into(), base_url: "http://127.0.0.1:11434/v1".into(), api_key_env: String::new(), api: CustomApi::OpenAi, models: Vec::new() },
            CustomProvider { name: "proxy".into(), base_url: "https://proxy.example/anthropic".into(), api_key_env: String::new(), api: CustomApi::Anthropic, models: Vec::new() },
        ]
    );
    assert_eq!(from_json(&json!({"fast": true, "speed": "ultrafast"})).speed, "ultrafast");
}

#[test]
fn command_line_values_stay_out_of_the_file_until_settings_change_them() {
    let mut saved = load(Path::new("/nonexistent")).unwrap();
    let mut flagged = saved.clone();
    flagged.model = "gpt-5.5".into();
    flagged.permission_mode = Mode::FullAccess;
    flagged.default_accounts.insert("anthropic".into(), "work".into());
    let mut edited = flagged.clone();
    edited.rtk = false;
    edited.thinking = "low".into();
    edited.default_accounts.insert("openai".into(), "home".into());
    saved.ponytail = "ultra".into();
    let next = saved_after(&flagged, &edited, &saved);
    assert_eq!(next.ponytail, "ultra", "another agent's edit to a key this one did not change survives");
    assert_eq!((next.model.as_str(), next.permission_mode, next.thinking.as_str(), next.rtk), (saved.model.as_str(), saved.permission_mode, "low", false));
    assert_eq!(next.default_accounts, HashMap::from([("openai".to_string(), "home".to_string())]));
}

#[test]
fn with_defaults_set_model_and_thinking_changes_stay_out_of_the_file() {
    let mut before = load(Path::new("/nonexistent")).unwrap();
    before.default_model = "gpt-5.5".into();
    before.default_thinking = "low".into();
    let mut edited = before.clone();
    edited.model = "claude-haiku".into();
    edited.thinking = "max".into();
    let next = saved_after(&before, &edited, &before);
    assert_eq!((next.model.as_str(), next.thinking.as_str()), (before.model.as_str(), before.thinking.as_str()));
}

#[test]
fn list_edits_from_two_agents_both_survive() {
    let before = load(Path::new("/nonexistent")).unwrap();
    let mut saved = before.clone();
    saved.disabled_plugins = vec!["theirs".into()];
    let mut edited = before.clone();
    edited.disabled_plugins = vec!["mine".into()];
    let next = saved_after(&before, &edited, &saved);
    assert_eq!(next.disabled_plugins, ["mine", "theirs"]);
    let mut enabled_again = edited.clone();
    enabled_again.disabled_plugins.clear();
    assert_eq!(saved_after(&edited, &enabled_again, &next).disabled_plugins, ["theirs"]);
}

#[test]
fn a_skill_a_profile_holds_loads_only_while_one_of_its_profiles_is_active() {
    let skill = |name: &str, source: Option<&str>| Skill {
        name: name.into(),
        description: String::new(),
        path: PathBuf::new(),
        model_invocable: true,
        shadows: None,
        source: source.map(Into::into),
        arguments: Vec::new(),
        argument_hint: None,
    };
    let (seo, copywriting, tdd) = (skill("seo-audit", Some("coreyhaines31/marketingskills")), skill("copywriting", None), skill("tdd", Some("mattpocock/skills")));
    let mut settings = load(Path::new("/nonexistent")).unwrap();
    settings.skill_profiles.insert("marketing".into(), vec!["coreyhaines31/marketingskills".into()]);
    settings.skill_profiles.insert("writing".into(), vec!["copywriting".into(), "seo-audit".into()]);
    let enabled = |settings: &Settings| [&seo, &copywriting, &tdd].map(|skill| settings.skill_enabled(skill));
    assert_eq!(enabled(&settings), [false, false, true]);
    settings.active_profiles = vec!["marketing".into()];
    assert_eq!(enabled(&settings), [true, false, true]);
    settings.active_profiles = vec!["writing".into()];
    assert_eq!(enabled(&settings), [true, true, true]);
    settings.disabled_skills = vec!["seo-audit".into(), "tdd".into()];
    assert_eq!(enabled(&settings), [true, true, false]);
    settings.active_profiles.clear();
    assert_eq!(enabled(&settings), [false, false, false]);
}

#[test]
fn a_profile_takes_skills_by_name_and_by_repo() {
    let skill = |name: &str, source: &str| Skill {
        name: name.into(),
        description: String::new(),
        path: PathBuf::new(),
        model_invocable: true,
        shadows: None,
        source: Some(source.into()),
        arguments: Vec::new(),
        argument_hint: None,
    };
    let skills = [skill("seo-audit", "coreyhaines31/marketingskills"), skill("ads", "coreyhaines31/marketingskills"), skill("tdd", "mattpocock/skills")];
    let mut settings = load(Path::new("/nonexistent")).unwrap();
    let entries = |settings: &Settings| settings.skill_profiles["work"].clone();
    settings.rename_profile(None, "work");
    settings.toggle_profile_skill("work", &skills[0], &skills);
    settings.toggle_profile_skill("work", &skills[2], &skills);
    assert_eq!(entries(&settings), ["seo-audit", "tdd"]);
    settings.toggle_profile_repo("work", "coreyhaines31/marketingskills", &skills);
    assert_eq!(entries(&settings), ["tdd", "coreyhaines31/marketingskills"]);
    settings.toggle_profile_skill("work", &skills[1], &skills);
    assert_eq!(entries(&settings), ["tdd", "seo-audit"]);
    settings.toggle_profile("work");
    settings.disabled_skills = vec!["seo-audit".into()];
    settings.toggle_disabled_repo("coreyhaines31/marketingskills", &skills);
    assert_eq!(settings.disabled_skills, ["seo-audit", "ads"]);
    settings.toggle_disabled_repo("coreyhaines31/marketingskills", &skills);
    assert!(settings.disabled_skills.is_empty());
    settings.rename_profile(Some("work"), "writing");
    assert_eq!((settings.skill_profiles.keys().map(String::as_str).collect::<Vec<_>>(), settings.active_profiles.clone()), (vec!["writing"], vec!["writing".to_string()]));
    settings.delete_profile("writing");
    assert!(settings.skill_profiles.is_empty() && settings.active_profiles.is_empty());
}

#[test]
fn settings_survive_a_save() {
    let home = std::env::temp_dir().join(format!("agent-settings-{}", std::process::id()));
    let mut settings = load(&home).unwrap();
    assert_eq!((settings.model.as_str(), settings.thinking.as_str(), settings.reserve_tokens), ("claude-opus-5-5", "high", 16_384));
    assert!(settings.on_start.is_empty());
    settings.thinking = "max".into();
    settings.permission_mode = Mode::Auto;
    settings.when_all_over_limit = Policy::OtherProvider;
    settings.context_windows.insert("anthropic".into(), 500_000);
    settings.context_windows.insert("openai-codex/gpt-5.5".into(), 400_000);
    settings.default_accounts.insert("anthropic".into(), "work".into());
    settings.provider_sort = "manual".into();
    settings.provider_order = vec!["openrouter".into(), "anthropic".into()];
    settings.pinned_models = vec!["anthropic/claude-haiku-5-5".into()];
    settings.disabled_skills = vec!["remotion-render".into()];
    settings.skill_profiles.insert("marketing".into(), vec!["coreyhaines31/marketingskills".into(), "copywriting".into()]);
    settings.active_profiles = vec!["marketing".into()];
    settings.disabled_plugins = vec!["usagefleet".into()];
    settings.disabled_mcp = vec!["github".into()];
    settings.plugin_settings = serde_json::json!({"usagefleet": {"token": "x"}});
    settings.ponytail = "ultra".into();
    settings.caveman = "wenyan".into();
    settings.output_style = "Concise".into();
    settings.rtk = false;
    settings.subagent_effort = "low".into();
    settings.subagent_model = "claude-sonnet-5-5".into();
    settings.reviewer_model = "claude-haiku-5-5".into();
    settings.image_model = "google-antigravity/gemini-3-pro-image".into();
    settings.small_model = "claude-haiku-4-5".into();
    settings.speed = "ultrafast".into();
    settings.custom_providers = vec![CustomProvider {
        name: "local".into(),
        base_url: "http://127.0.0.1:8080/v1".into(),
        api_key_env: String::new(),
        api: CustomApi::Anthropic,
        models: vec![CustomModel { id: "qwen".into(), context_window: 65_536, max_tokens: 8_192 }],
    }];
    settings.cache_ttl = "1h".into();
    settings.message_threads = false;
    settings.headroom = true;
    settings.headroom_url = "https://headroom.example.com".into();
    settings.headroom_token = "t0ken".into();
    settings.headroom_env = BTreeMap::from([("HEADROOM_DISABLE_KOMPRESS".into(), "1".into())]);
    settings.notifications = false;
    settings.progress_bar = false;
    settings.scroll_lines = 10;
    settings.menu_rows = 10;
    settings.copy_on_select = false;
    settings.update_channel = "pre-release".into();
    settings.prune_sessions = "7d".into();
    settings.mid_turn = "queued".into();
    settings.working_animation = "radar".into();
    settings.on_start = vec!["echo started".into()];
    settings.lsp.get_mut("rust").unwrap().command = "ra-multiplex".into();
    save(&home, &to_json(&settings)).unwrap();
    assert_eq!(load(&home).unwrap(), settings);
    std::fs::remove_dir_all(&home).unwrap();
}

#[test]
fn a_save_keeps_keys_this_version_does_not_know() {
    let home = std::env::temp_dir().join(format!("agent-settings-unknown-{}", std::process::id()));
    std::fs::create_dir_all(path(&home).parent().unwrap()).unwrap();
    std::fs::write(path(&home), r#"{"thinking": "low", "added_by_a_newer_agent": true, "another": {"nested": [1, 2]}}"#).unwrap();
    let before = load(&home).unwrap();
    let mut after = before.clone();
    after.prune_sessions = "7d".into();
    save_change(&home, &before, &after).unwrap();
    let written: Value = serde_json::from_str(&std::fs::read_to_string(path(&home)).unwrap()).unwrap();
    std::fs::remove_dir_all(&home).unwrap();
    assert_eq!(written["added_by_a_newer_agent"], true);
    assert_eq!(written["another"], json!({"nested": [1, 2]}));
    assert_eq!((written["prune_sessions"].as_str(), written["thinking"].as_str()), (Some("7d"), Some("low")));
}

#[test]
fn settings_schema_matches_what_save_writes() {
    let home = std::env::temp_dir().join(format!("agent-settings-schema-{}", std::process::id()));
    save(&home, &to_json(&load(&home).unwrap())).unwrap();
    let written: Value = serde_json::from_str(&std::fs::read_to_string(path(&home)).unwrap()).unwrap();
    std::fs::remove_dir_all(&home).unwrap();
    let schema: Value = serde_json::from_str(include_str!("../settings.schema.json")).unwrap();
    assert_eq!(written["$schema"], SCHEMA_URL);
    assert_same_keys(&written, &schema, "settings");
    assert_same_keys(&written["lsp"]["rust"], &schema["properties"]["lsp"]["additionalProperties"], "lsp.rust");
    let properties = &schema["properties"];
    assert_eq!(properties["thinking"]["enum"], json!(THINKING_LEVELS));
    assert_eq!(properties["cache_ttl"]["enum"], json!(CACHE_TTLS));
    assert_eq!(properties["speed"]["enum"], json!(SPEEDS));
    assert_eq!(properties["subagents"]["properties"]["implementation_effort"]["enum"], json!(THINKING_LEVELS));
    assert_eq!(properties["permission_mode"]["enum"], json!(crate::agent::permissions::MODES.map(|mode| mode.label())));
    assert_eq!(properties["accounts"]["properties"]["when_all_over_limit"]["enum"], json!(Policy::ALL.map(|policy| policy.label())));
    assert_eq!(properties["ponytail"]["enum"], json!(crate::agent::modes::PONYTAIL_LEVELS));
    assert_eq!(properties["caveman"]["enum"], json!(crate::agent::modes::CAVEMAN_LEVELS));
    assert_eq!(properties["output_style"]["enum"], json!(crate::agent::modes::OUTPUT_STYLES));
    assert_eq!(properties["update_channel"]["enum"], json!(crate::update::CHANNELS));
    assert_eq!(properties["prune_sessions"]["enum"], json!(PRUNE_SESSIONS));
    assert_eq!(properties["mid_turn"]["enum"], json!(MID_TURN));
    assert_eq!(properties["working_animation"]["enum"], json!(WORKING_ANIMATIONS));
    assert_eq!(properties["provider_sort"]["enum"], json!(PROVIDER_SORTS));
    assert_eq!(properties["conflict_priority"]["enum"], json!(CONFLICT_PRIORITIES));
}

/// Every object `save` writes has exactly the properties its schema lists, nested objects included.
fn assert_same_keys(written: &Value, schema: &Value, at: &str) {
    let properties = schema["properties"].as_object().unwrap();
    let written_keys: Vec<&String> = written.as_object().unwrap().keys().collect();
    let schema_keys: Vec<&String> = properties.keys().collect();
    assert_eq!(written_keys, schema_keys, "{at}");
    for (key, property) in properties {
        if property["properties"].is_object() {
            assert_same_keys(&written[key], property, &format!("{at}.{key}"));
        }
    }
}

#[test]
fn model_window_beats_provider_window_beats_catalog() {
    let model = |id: &str| Model { provider: "anthropic".into(), id: id.into(), context_window: 1_000_000, ..Model::default() };
    let (opus, sonnet) = (&model("claude-opus-4-6"), &model("claude-sonnet-4-6"));
    let mut settings = load(Path::new("/nonexistent")).unwrap();
    assert_eq!(settings.context_window(opus), opus.context_window);
    settings.context_windows.insert("anthropic".into(), 400_000);
    settings.context_windows.insert("anthropic/claude-opus-4-6".into(), 200_000);
    assert_eq!((settings.context_window(opus), settings.context_window(sonnet)), (200_000, 400_000));
}

#[test]
fn provider_sort_applies_default_alphabetical_and_manual_orders() {
    let mut settings = load(Path::new("/nonexistent")).unwrap();
    let providers = ["exa", "brave", "tavily"];
    assert_eq!(settings.sort_providers(&providers), ["exa", "brave", "tavily"]);
    settings.provider_sort = "alphabetical".into();
    assert_eq!(settings.sort_providers(&providers), ["brave", "exa", "tavily"]);
    settings.provider_sort = "manual".into();
    settings.provider_order = vec!["tavily".into(), "exa".into()];
    assert_eq!(settings.sort_providers(&providers), ["tavily", "exa", "brave"]);
}

#[test]
fn moving_a_provider_keeps_the_shown_order_and_switches_to_manual() {
    let mut settings = load(Path::new("/nonexistent")).unwrap();
    settings.provider_sort = "alphabetical".into();
    settings.provider_order = vec!["anthropic".into()];
    settings.move_provider(&["exa", "brave", "tavily"], 2, true);
    assert_eq!(settings.provider_sort, "manual");
    assert_eq!(settings.provider_order, ["brave", "tavily", "exa", "anthropic"]);
    settings.move_provider(&["exa", "brave", "tavily"], 2, false);
    assert_eq!(settings.provider_order, ["brave", "tavily", "exa", "anthropic"]);
}

#[test]
fn lsp_entries_override_single_fields_and_add_languages() {
    let servers = lsp_servers(&json!({"rust": {"command": "ra-multiplex"}, "zig": {"command": "zls", "extensions": ["zig"]}, "go": {"command": ""}}));
    assert_eq!(servers["rust"], LspServer { command: "ra-multiplex".into(), extensions: vec!["rs".into()], roots: vec!["Cargo.toml".into()] });
    assert_eq!(servers["zig"], LspServer { command: "zls".into(), extensions: vec!["zig".into()], roots: Vec::new() });
    assert_eq!(servers["go"].command, "");
    assert_eq!(servers["typescript"].command, "tsc-rs --lsp --stdio");
}
