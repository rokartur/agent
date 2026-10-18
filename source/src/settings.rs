use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::agent::permissions::Mode;
use crate::api::accounts::Policy;
use crate::api::models::Model;

/// Everything /settings edits, saved to ~/.agents/settings.json.
#[derive(Clone, PartialEq, Debug)]
pub struct Settings {
    pub model: String,
    pub thinking: String,
    pub permission_mode: Mode,
    pub auto_compact: bool,
    pub reserve_tokens: u64,
    pub keep_recent_tokens: u64,
    pub expand_tools: bool,
    pub condense: bool,
    /// Empty means the default summarizer for the provider.
    pub condense_model: String,
    pub condense_min_batch_chars: u64,
    pub subagent_concurrency: usize,
    /// The effort the swarm prompt tells the model to pass for implementation children.
    pub subagent_effort: String,
    pub switch_at_percent: u64,
    pub when_all_over_limit: Policy,
    /// Empty means the other provider's default model.
    pub fallback_model: String,
    /// Provider to account name, chosen in /account and used first while under the limit.
    pub default_accounts: HashMap<String, String>,
    /// Keyed by provider ("anthropic") or model ("anthropic/claude-opus-5-5"); the model key wins.
    pub context_windows: HashMap<String, u64>,
    /// One of `PROVIDER_SORTS`: the order /account, the context windows panel and web search list providers in.
    pub provider_sort: String,
    /// Providers listed first when `provider_sort` is "manual"; unlisted ones keep their default order after them.
    pub provider_order: Vec<String>,
    /// `model_key`s the model picker lists first.
    pub pinned_models: Vec<String>,
    /// Skill names left out of the prompt and /skill: completion.
    pub disabled_skills: Vec<String>,
    /// Installed plugins that are not started.
    pub disabled_plugins: Vec<String>,
    /// MCP servers from `mcp.json` that are not connected.
    pub disabled_mcp: Vec<String>,
    /// Project MCP servers the user let run, by `mcp::config::Server::approval` key.
    pub approved_mcp: Vec<String>,
    /// The user's plugin settings, `{ "<plugin>": { "<key>": value } }`; see plugins/README.md.
    pub plugin_settings: serde_json::Value,
    /// One of `modes::PONYTAIL_LEVELS`.
    pub ponytail: String,
    /// One of `modes::CAVEMAN_LEVELS`.
    pub caveman: String,
    pub rtk: bool,
    /// Send model requests through a local `headroom proxy`.
    pub headroom: bool,
    /// Desktop notification when a task ends or the agent waits for an answer while the terminal is in the background.
    pub notifications: bool,
    /// Transcript lines moved per mouse wheel step.
    pub scroll_lines: usize,
    /// Slash menu entries shown before it scrolls.
    pub menu_rows: usize,
    /// Copy a mouse selection on release; when off, Ctrl+C copies it.
    pub copy_on_select: bool,
    /// One of `update::CHANNELS`.
    pub update_channel: String,
    /// One of `PRUNE_SESSIONS`: sessions untouched this long are deleted at start.
    pub prune_sessions: String,
    /// Shell commands the TUI runs in the background at start, e.g. a global tool update.
    pub on_start: Vec<String>,
    /// Language name to the server that checks a file after each edit; an empty command turns the language off.
    pub lsp: BTreeMap<String, LspServer>,
}

#[derive(Clone, PartialEq, Debug)]
pub struct LspServer {
    pub command: String,
    pub extensions: Vec<String>,
    /// Files that mark a project root; the server starts in the nearest directory above the file that has one.
    pub roots: Vec<String>,
}

pub const PRUNE_SESSIONS: [&str; 6] = ["never", "24h", "3d", "7d", "14d", "30d"];
pub const PROVIDER_SORTS: [&str; 3] = ["default", "alphabetical", "manual"];
/// Below this a condensed batch costs more in cache rewrites than its summary saves on later cache reads.
pub const CONDENSE_MIN_BATCH_CHARS: u64 = 10_000;

impl Settings {
    pub fn context_window(&self, model: &Model) -> u64 {
        let overrides = &self.context_windows;
        overrides.get(&model_key(model)).or(overrides.get(&model.provider)).copied().unwrap_or(model.context_window)
    }

    /// `providers` in the order `provider_sort` picks.
    pub fn sort_providers<'a>(&self, providers: &[&'a str]) -> Vec<&'a str> {
        let mut sorted = providers.to_vec();
        match self.provider_sort.as_str() {
            "alphabetical" => sorted.sort_unstable(),
            "manual" => sorted.sort_by_key(|provider| self.provider_order.iter().position(|listed| listed.as_str() == *provider).unwrap_or(usize::MAX)),
            _ => {}
        }
        sorted
    }

    /// Moves the provider at `index` of the shown order one step and switches to manual;
    /// entries for providers outside `providers` stay listed after them.
    pub fn move_provider(&mut self, providers: &[&str], index: usize, up: bool) {
        let mut shown = self.sort_providers(providers);
        let target = if up { index.checked_sub(1) } else { Some(index + 1) };
        if let Some(target) = target.filter(|target| *target < shown.len()) {
            shown.swap(index, target);
        }
        let others = self.provider_order.iter().filter(|listed| !providers.contains(&listed.as_str()));
        self.provider_order = shown.iter().map(|provider| provider.to_string()).chain(others.cloned()).collect();
        self.provider_sort = "manual".into();
    }

    /// How long an untouched session is kept; `None` keeps it forever.
    pub fn prune_sessions_after(&self) -> Option<Duration> {
        let hours = match self.prune_sessions.as_str() {
            "24h" => 24,
            "3d" => 3 * 24,
            "7d" => 7 * 24,
            "14d" => 14 * 24,
            "30d" => 30 * 24,
            _ => return None,
        };
        Some(Duration::from_secs(hours * 3600))
    }
}

pub fn model_key(model: &Model) -> String {
    format!("{}/{}", model.provider, model.id)
}

/// Writes the change a /settings edit made from `before` to `after` over the file as it is now, under its lock,
/// so another agent's edits to other keys survive.
pub fn save_change(home: &Path, before: &Settings, after: &Settings) -> Result<(), String> {
    crate::api::accounts::locked(&path(home), || save(home, &saved_after(before, after, &load(home)?)))
}

/// `saved` with only the keys a /settings edit changed from `before` to `after`, objects merged key by key. So the
/// keys the command line sets (`--model`, `--thinking`, the permission flags, `--account`) keep their saved value too.
fn saved_after(before: &Settings, after: &Settings, saved: &Settings) -> Settings {
    let mut next = to_json(saved);
    apply_changes(&mut next, &to_json(before), &to_json(after));
    from_json(&next)
}

fn apply_changes(saved: &mut Value, before: &Value, after: &Value) {
    let (Some(before), Some(after), Some(saved)) = (before.as_object(), after.as_object(), saved.as_object_mut()) else {
        *saved = after.clone();
        return;
    };
    for key in before.keys().chain(after.keys()) {
        match (before.get(key), after.get(key)) {
            (old, new) if old == new => {}
            (_, None) => _ = saved.remove(key),
            (old, Some(new)) => apply_changes(saved.entry(key).or_insert(Value::Null), old.unwrap_or(&Value::Null), new),
        }
    }
}

pub fn path(home: &Path) -> PathBuf {
    home.join(".agents/settings.json")
}

/// Defaults for a missing file; an unreadable or broken one is an error, so the next save cannot write defaults over it.
pub fn load(home: &Path) -> Result<Settings, String> {
    let path = path(home);
    let own: Value = match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| format!("{} is broken: {e}. Fix or remove it by hand.", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Value::Null,
        Err(e) => return Err(format!("Cannot read {}: {e}", path.display())),
    };
    Ok(from_json(&own))
}

fn from_json(own: &Value) -> Settings {
    Settings {
        model: own["model"].as_str().unwrap_or("claude-opus-5-5").to_string(),
        thinking: level(&own["thinking"], &crate::tui::THINKING_LEVELS, "high"),
        permission_mode: own["permission_mode"].as_str().and_then(Mode::parse).unwrap_or(Mode::Ask),
        auto_compact: own["auto_compact"].as_bool().unwrap_or(true),
        // Compaction asks for 4/5 of it as max_tokens; 0 is an API error and a few hundred cut the summary off.
        reserve_tokens: own["reserve_tokens"].as_u64().unwrap_or(16_384).max(4_096),
        keep_recent_tokens: own["keep_recent_tokens"].as_u64().unwrap_or(20_000),
        expand_tools: own["expand_tools"].as_bool().unwrap_or(false),
        condense: own["condense"]["enabled"].as_bool().unwrap_or(true),
        condense_model: own["condense"]["model"].as_str().unwrap_or_default().to_string(),
        condense_min_batch_chars: own["condense"]["min_batch_chars"].as_u64().unwrap_or(CONDENSE_MIN_BATCH_CHARS).max(CONDENSE_MIN_BATCH_CHARS),
        subagent_concurrency: own["subagents"]["concurrency"].as_u64().unwrap_or(4) as usize,
        subagent_effort: level(&own["subagents"]["implementation_effort"], &crate::tui::THINKING_LEVELS, "medium"),
        switch_at_percent: own["accounts"]["switch_at_percent"].as_u64().unwrap_or(90),
        when_all_over_limit: own["accounts"]["when_all_over_limit"].as_str().and_then(Policy::parse).unwrap_or(Policy::LeastUsed),
        fallback_model: own["accounts"]["fallback_model"].as_str().unwrap_or_default().to_string(),
        default_accounts: own["accounts"]["default"].as_object().into_iter().flatten().filter_map(|(provider, name)| Some((provider.clone(), name.as_str()?.to_string()))).collect(),
        context_windows: own["context_windows"].as_object().into_iter().flatten().filter_map(|(key, window)| Some((key.clone(), window.as_u64()?))).collect(),
        provider_sort: level(&own["provider_sort"], &PROVIDER_SORTS, "default"),
        provider_order: string_list(&own["provider_order"]),
        pinned_models: string_list(&own["pinned_models"]),
        disabled_skills: string_list(&own["disabled_skills"]),
        disabled_plugins: string_list(&own["disabled_plugins"]),
        disabled_mcp: string_list(&own["disabled_mcp"]),
        approved_mcp: string_list(&own["approved_mcp"]),
        plugin_settings: if own["plugin_settings"].is_object() { own["plugin_settings"].clone() } else { serde_json::json!({}) },
        ponytail: level(&own["ponytail"], &crate::agent::modes::PONYTAIL_LEVELS, "full"),
        caveman: level(&own["caveman"], &crate::agent::modes::CAVEMAN_LEVELS, "off"),
        rtk: own["rtk"].as_bool().unwrap_or(true),
        headroom: own["headroom"].as_bool().unwrap_or(false),
        notifications: own["notifications"].as_bool().unwrap_or(true),
        scroll_lines: own["scroll_lines"].as_u64().unwrap_or(3) as usize,
        menu_rows: own["menu_rows"].as_u64().unwrap_or(6) as usize,
        copy_on_select: own["copy_on_select"].as_bool().unwrap_or(true),
        update_channel: level(&own["update_channel"], &crate::update::CHANNELS, "latest"),
        prune_sessions: own["prune_sessions"].as_str().unwrap_or("never").to_string(),
        on_start: string_list(&own["on_start"]),
        lsp: lsp_servers(&own["lsp"]),
    }
}

/// A value from settings.json limited to `levels`; an unknown one loads as the default, like an unknown permission mode.
fn level(value: &Value, levels: &[&str], default: &str) -> String {
    value.as_str().filter(|level| levels.contains(level)).unwrap_or(default).to_string()
}

fn string_list(value: &Value) -> Vec<String> {
    value.as_array().into_iter().flatten().filter_map(|item| Some(item.as_str()?.to_string())).collect()
}

/// The built-in servers, each field replaced by the user's value where one is set; unknown names add a language.
fn lsp_servers(own: &Value) -> BTreeMap<String, LspServer> {
    let strings = |value: &Value| -> Option<Vec<String>> { value.as_array()?.iter().map(|item| Some(item.as_str()?.to_string())).collect() };
    let mut servers = default_lsp_servers();
    for (language, value) in own.as_object().into_iter().flatten() {
        let server = servers.entry(language.clone()).or_insert(LspServer { command: String::new(), extensions: Vec::new(), roots: Vec::new() });
        if let Some(command) = value["command"].as_str() {
            server.command = command.to_string();
        }
        if let Some(extensions) = strings(&value["extensions"]) {
            server.extensions = extensions;
        }
        if let Some(roots) = strings(&value["roots"]) {
            server.roots = roots;
        }
    }
    servers
}

fn default_lsp_servers() -> BTreeMap<String, LspServer> {
    let servers = [
        ("typescript", "tsc-rs --lsp --stdio", &["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"][..], &["tsconfig.json", "jsconfig.json", "package.json"][..]),
        ("rust", "rust-analyzer", &["rs"], &["Cargo.toml"]),
        ("swift", "sourcekit-lsp", &["swift"], &["buildServer.json", "Package.swift"]),
        ("go", "gopls serve", &["go"], &["go.work", "go.mod"]),
        ("python", "pyright-langserver --stdio", &["py"], &["pyrightconfig.json", "pyproject.toml", "setup.py", "requirements.txt"]),
        ("cpp", "clangd --log=error", &["c", "h", "cc", "cpp", "cxx", "hpp", "hxx"], &["compile_commands.json", "CMakeLists.txt", ".clangd"]),
        ("php", "intelephense --stdio", &["php"], &["composer.json"]),
    ];
    let owned = |items: &[&str]| items.iter().map(|item| item.to_string()).collect();
    servers.into_iter().map(|(language, command, extensions, roots)| (language.to_string(), LspServer { command: command.to_string(), extensions: owned(extensions), roots: owned(roots) })).collect()
}

/// Editors fetch it for completion; `settings_schema_matches_what_save_writes` keeps it in step with `save`.
const SCHEMA_URL: &str = "https://raw.githubusercontent.com/rokartur/agent/main/source/src/settings.schema.json";

/// Callers hold the file lock; see `save_change`.
fn save(home: &Path, settings: &Settings) -> Result<(), String> {
    let path = path(home);
    std::fs::create_dir_all(path.parent().unwrap()).map_err(|e| format!("Cannot create {}: {e}", path.display()))?;
    // plugin_settings can hold tokens.
    crate::api::accounts::write_private(&path, &format!("{:#}\n", to_json(settings)))
}

fn to_json(settings: &Settings) -> Value {
    json!({
        "$schema": SCHEMA_URL,
        "model": settings.model,
        "thinking": settings.thinking,
        "permission_mode": settings.permission_mode.label(),
        "auto_compact": settings.auto_compact,
        "reserve_tokens": settings.reserve_tokens,
        "keep_recent_tokens": settings.keep_recent_tokens,
        "expand_tools": settings.expand_tools,
        "context_windows": settings.context_windows,
        "provider_sort": settings.provider_sort,
        "provider_order": settings.provider_order,
        "pinned_models": settings.pinned_models,
        "disabled_skills": settings.disabled_skills,
        "disabled_plugins": settings.disabled_plugins,
        "disabled_mcp": settings.disabled_mcp,
        "approved_mcp": settings.approved_mcp,
        "plugin_settings": settings.plugin_settings,
        "ponytail": settings.ponytail,
        "caveman": settings.caveman,
        "rtk": settings.rtk,
        "headroom": settings.headroom,
        "notifications": settings.notifications,
        "scroll_lines": settings.scroll_lines,
        "menu_rows": settings.menu_rows,
        "copy_on_select": settings.copy_on_select,
        "update_channel": settings.update_channel,
        "prune_sessions": settings.prune_sessions,
        "on_start": settings.on_start,
        "lsp": settings.lsp.iter().map(|(language, server)| (language.clone(), json!({"command": server.command, "extensions": server.extensions, "roots": server.roots}))).collect::<serde_json::Map<_, _>>(),
        "condense": {"enabled": settings.condense, "model": settings.condense_model, "min_batch_chars": settings.condense_min_batch_chars},
        "subagents": {"concurrency": settings.subagent_concurrency, "implementation_effort": settings.subagent_effort},
        "accounts": {"switch_at_percent": settings.switch_at_percent, "when_all_over_limit": settings.when_all_over_limit.label(), "fallback_model": settings.fallback_model, "default": settings.default_accounts},
    })
}

#[cfg(test)]
mod tests {
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
        settings.disabled_plugins = vec!["usagefleet".into()];
        settings.disabled_mcp = vec!["github".into()];
        settings.plugin_settings = serde_json::json!({"usagefleet": {"token": "x"}});
        settings.ponytail = "ultra".into();
        settings.caveman = "wenyan".into();
        settings.rtk = false;
        settings.subagent_effort = "low".into();
        settings.headroom = true;
        settings.notifications = false;
        settings.scroll_lines = 10;
        settings.menu_rows = 10;
        settings.copy_on_select = false;
        settings.update_channel = "pre-release".into();
        settings.prune_sessions = "7d".into();
        settings.on_start = vec!["echo started".into()];
        settings.lsp.get_mut("rust").unwrap().command = "ra-multiplex".into();
        save(&home, &settings).unwrap();
        assert_eq!(load(&home).unwrap(), settings);
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn settings_schema_matches_what_save_writes() {
        let home = std::env::temp_dir().join(format!("agent-settings-schema-{}", std::process::id()));
        save(&home, &load(&home).unwrap()).unwrap();
        let written: Value = serde_json::from_str(&std::fs::read_to_string(path(&home)).unwrap()).unwrap();
        std::fs::remove_dir_all(&home).unwrap();
        let schema: Value = serde_json::from_str(include_str!("settings.schema.json")).unwrap();
        assert_eq!(written["$schema"], SCHEMA_URL);
        assert_same_keys(&written, &schema, "settings");
        assert_same_keys(&written["lsp"]["rust"], &schema["properties"]["lsp"]["additionalProperties"], "lsp.rust");
        let properties = &schema["properties"];
        assert_eq!(properties["thinking"]["enum"], json!(crate::tui::THINKING_LEVELS));
        assert_eq!(properties["subagents"]["properties"]["implementation_effort"]["enum"], json!(crate::tui::THINKING_LEVELS));
        assert_eq!(properties["permission_mode"]["enum"], json!(crate::agent::permissions::MODES.map(|mode| mode.label())));
        assert_eq!(properties["accounts"]["properties"]["when_all_over_limit"]["enum"], json!(Policy::ALL.map(|policy| policy.label())));
        assert_eq!(properties["ponytail"]["enum"], json!(crate::agent::modes::PONYTAIL_LEVELS));
        assert_eq!(properties["caveman"]["enum"], json!(crate::agent::modes::CAVEMAN_LEVELS));
        assert_eq!(properties["update_channel"]["enum"], json!(crate::update::CHANNELS));
        assert_eq!(properties["prune_sessions"]["enum"], json!(PRUNE_SESSIONS));
        assert_eq!(properties["provider_sort"]["enum"], json!(PROVIDER_SORTS));
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
}
