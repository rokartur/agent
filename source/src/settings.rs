use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::agent::permissions::Mode;
use crate::api::accounts::Policy;
use crate::api::models::{self, Model};

/// Everything /settings edits, saved to ~/.agents/settings.json.
#[derive(Clone, PartialEq, Debug)]
pub struct Settings {
    pub model: String,
    pub thinking: String,
    /// When set, each start and /clear begin on it, and /model picks stay out of the file.
    pub default_model: String,
    /// When set, each start and /clear begin on it, and thinking changes stay out of the file.
    pub default_thinking: String,
    /// One of `SPEEDS`: Anthropic's `speed: "fast"`, Codex's `service_tier`; faster output at a higher price.
    pub speed: String,
    /// One of `CACHE_TTLS`, for every Anthropic cache breakpoint.
    pub cache_ttl: String,
    pub permission_mode: Mode,
    /// The auto-mode reviewer; empty means the provider's small model.
    pub reviewer_model: String,
    pub auto_compact: bool,
    pub reserve_tokens: u64,
    pub keep_recent_tokens: u64,
    pub expand_tools: bool,
    /// The session's cost after the context tokens in the footer.
    pub footer_cost: bool,
    /// The name of the account the current model's provider uses, before the model in the footer.
    pub footer_account: bool,
    pub condense: bool,
    /// Empty means the default summarizer for the provider.
    pub condense_model: String,
    pub condense_min_batch_chars: u64,
    pub subagent_concurrency: usize,
    /// The effort the swarm prompt tells the model to pass for implementation children.
    pub subagent_effort: String,
    /// The model a child gets when the subagent call names none; empty means the parent's.
    pub subagent_model: String,
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
    /// `model_key`s the model picker lists first and Ctrl+P steps through.
    pub pinned_models: Vec<String>,
    /// Skill names left out of the prompt and /skill: completion.
    pub disabled_skills: Vec<String>,
    /// One of `CONFLICT_PRIORITIES`: the side whose skill or prompt wins when the project and `~/.agents` share a name.
    pub conflict_priority: String,
    /// Installed plugins that are not started.
    pub disabled_plugins: Vec<String>,
    /// MCP servers from `mcp.json` that are not connected.
    pub disabled_mcp: Vec<String>,
    /// Project MCP servers the user let run, by `mcp::config::Server::approval` key.
    pub approved_mcp: Vec<String>,
    /// Project roots (`prompt::project_root`) whose `.agents` files load and whose edit hooks run.
    pub trusted_projects: Vec<String>,
    /// The user's plugin settings, `{ "<plugin>": { "<key>": value } }`; see plugins/README.md.
    pub plugin_settings: serde_json::Value,
    /// One of `modes::PONYTAIL_LEVELS`.
    pub ponytail: String,
    /// One of `modes::CAVEMAN_LEVELS`.
    pub caveman: String,
    pub rtk: bool,
    /// Send model requests through `headroom proxy`.
    pub headroom: bool,
    /// A remote `headroom proxy` (`https://headroom.example.com`); empty means the local one, which the agent installs and syncs.
    pub headroom_url: String,
    /// The remote proxy's `HEADROOM_PROXY_TOKEN`, sent as `x-headroom-proxy-token`, the header headroom checks on a non-loopback bind.
    pub headroom_token: String,
    /// `--env` values the headroom deployment is installed with; the agent reinstalls it when one differs.
    pub headroom_env: BTreeMap<String, String>,
    /// Desktop notification when a task ends or the agent waits for an answer while the terminal is in the background.
    pub notifications: bool,
    /// OSC 9;4 busy indicator while a turn runs; Ghostty draws it as a bar at the top, others in the tab or Dock icon.
    pub progress_bar: bool,
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
    /// OpenAI- or Anthropic-compatible endpoints; a preset name (`ollama`) fills in `base_url` and `api_key_env`.
    pub custom_providers: Vec<CustomProvider>,
}

/// An endpoint whose models show as `<name>/<model id>`.
#[derive(Clone, PartialEq, Debug)]
pub struct CustomProvider {
    pub name: String,
    pub base_url: String,
    /// The variable that holds the API key, never the key itself; empty sends no Authorization header.
    pub api_key_env: String,
    pub api: CustomApi,
    /// Empty means `GET {base_url}/models` lists them.
    pub models: Vec<CustomModel>,
}

/// The wire format: Chat Completions at `{base_url}/chat/completions`, or Anthropic Messages at `{base_url}/v1/messages`.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum CustomApi {
    OpenAi,
    Anthropic,
}

#[derive(Clone, PartialEq, Debug)]
pub struct CustomModel {
    pub id: String,
    pub context_window: u64,
    pub max_tokens: u64,
}

#[derive(Clone, PartialEq, Debug)]
pub struct LspServer {
    pub command: String,
    pub extensions: Vec<String>,
    /// Files that mark a project root; the server starts in the nearest directory above the file that has one.
    pub roots: Vec<String>,
}

pub const THINKING_LEVELS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];
pub const CACHE_TTLS: [&str; 2] = ["5m", "1h"];
pub const SPEEDS: [&str; 3] = ["standard", "fast", "ultrafast"];
pub const PRUNE_SESSIONS: [&str; 6] = ["never", "24h", "3d", "7d", "14d", "30d"];
pub const PROVIDER_SORTS: [&str; 3] = ["default", "alphabetical", "manual"];
pub const CONFLICT_PRIORITIES: [&str; 2] = ["project", "global"];
/// Below this a condensed batch costs more in cache rewrites than its summary saves on later cache reads.
pub const CONDENSE_MIN_BATCH_CHARS: u64 = 10_000;

impl Settings {
    pub fn context_window(&self, model: &Model) -> u64 {
        let overrides = &self.context_windows;
        overrides.get(&model_key(model)).or(overrides.get(&model.provider)).copied().unwrap_or(model.context_window)
    }

    pub fn trusts(&self, cwd: &Path) -> bool {
        crate::agent::prompt::is_trusted(&self.trusted_projects, cwd)
    }

    /// Puts `model` and `thinking` back on `default_model` and `default_thinking` where those are set.
    pub fn reset_to_defaults(&mut self) {
        if !self.default_model.is_empty() {
            self.model = self.default_model.clone();
        }
        if !self.default_thinking.is_empty() {
            self.thinking = self.default_thinking.clone();
        }
    }

    pub fn prefer_global(&self) -> bool {
        self.conflict_priority == "global"
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
    let mut after = after.clone();
    if !after.default_model.is_empty() {
        after.model = before.model.clone();
    }
    if !after.default_thinking.is_empty() {
        after.thinking = before.thinking.clone();
    }
    let mut next = to_json(saved);
    apply_changes(&mut next, &to_json(before), &to_json(&after));
    from_json(&next)
}

fn apply_changes(saved: &mut Value, before: &Value, after: &Value) {
    if let (Some(before), Some(after), Some(saved)) = (before.as_array(), after.as_array(), saved.as_array_mut()) {
        // This edit's order and additions, minus what another agent removed, then what another agent added.
        let mut next: Vec<Value> = after.iter().filter(|item| saved.contains(item) || !before.contains(item)).cloned().collect();
        next.extend(saved.iter().filter(|item| !before.contains(item) && !after.contains(item)).cloned());
        *saved = next;
        return;
    }
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
        thinking: level(&own["thinking"], &THINKING_LEVELS, "high"),
        default_model: own["default_model"].as_str().unwrap_or_default().to_string(),
        default_thinking: level(&own["default_thinking"], &THINKING_LEVELS, ""),
        // Before `speed`, fast mode was `"fast": true`.
        speed: if own["speed"].is_null() && own["fast"] == true { "fast".into() } else { level(&own["speed"], &SPEEDS, "standard") },
        cache_ttl: level(&own["cache_ttl"], &CACHE_TTLS, "5m"),
        permission_mode: own["permission_mode"].as_str().and_then(Mode::parse).unwrap_or(Mode::Ask),
        reviewer_model: own["reviewer_model"].as_str().unwrap_or_default().to_string(),
        auto_compact: own["auto_compact"].as_bool().unwrap_or(true),
        // Compaction asks for 4/5 of it as max_tokens; 0 is an API error and a few hundred cut the summary off.
        reserve_tokens: own["reserve_tokens"].as_u64().unwrap_or(16_384).max(4_096),
        keep_recent_tokens: own["keep_recent_tokens"].as_u64().unwrap_or(20_000),
        expand_tools: own["expand_tools"].as_bool().unwrap_or(false),
        footer_cost: own["footer_cost"].as_bool().unwrap_or(true),
        footer_account: own["footer_account"].as_bool().unwrap_or(false),
        condense: own["condense"]["enabled"].as_bool().unwrap_or(true),
        condense_model: own["condense"]["model"].as_str().unwrap_or_default().to_string(),
        condense_min_batch_chars: own["condense"]["min_batch_chars"].as_u64().unwrap_or(CONDENSE_MIN_BATCH_CHARS).max(CONDENSE_MIN_BATCH_CHARS),
        subagent_concurrency: own["subagents"]["concurrency"].as_u64().unwrap_or(4) as usize,
        subagent_effort: level(&own["subagents"]["implementation_effort"], &THINKING_LEVELS, "medium"),
        subagent_model: own["subagents"]["model"].as_str().unwrap_or_default().to_string(),
        switch_at_percent: own["accounts"]["switch_at_percent"].as_u64().unwrap_or(90).min(100),
        when_all_over_limit: own["accounts"]["when_all_over_limit"].as_str().and_then(Policy::parse).unwrap_or(Policy::LeastUsed),
        fallback_model: own["accounts"]["fallback_model"].as_str().unwrap_or_default().to_string(),
        default_accounts: own["accounts"]["default"].as_object().into_iter().flatten().filter_map(|(provider, name)| Some((provider.clone(), name.as_str()?.to_string()))).collect(),
        context_windows: own["context_windows"].as_object().into_iter().flatten().filter_map(|(key, window)| Some((key.clone(), window.as_u64()?))).collect(),
        provider_sort: level(&own["provider_sort"], &PROVIDER_SORTS, "default"),
        provider_order: string_list(&own["provider_order"]),
        pinned_models: string_list(&own["pinned_models"]),
        disabled_skills: string_list(&own["disabled_skills"]),
        conflict_priority: level(&own["conflict_priority"], &CONFLICT_PRIORITIES, "project"),
        disabled_plugins: string_list(&own["disabled_plugins"]),
        disabled_mcp: string_list(&own["disabled_mcp"]),
        approved_mcp: string_list(&own["approved_mcp"]),
        trusted_projects: string_list(&own["trusted_projects"]),
        plugin_settings: if own["plugin_settings"].is_object() { own["plugin_settings"].clone() } else { serde_json::json!({}) },
        ponytail: level(&own["ponytail"], &crate::agent::modes::PONYTAIL_LEVELS, "full"),
        caveman: level(&own["caveman"], &crate::agent::modes::CAVEMAN_LEVELS, "off"),
        rtk: own["rtk"].as_bool().unwrap_or(true),
        headroom: own["headroom"].as_bool().unwrap_or(false),
        headroom_url: own["headroom_url"].as_str().unwrap_or_default().trim_end_matches('/').to_string(),
        headroom_token: own["headroom_token"].as_str().unwrap_or_default().to_string(),
        headroom_env: own["headroom_env"].as_object().into_iter().flatten().filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_string()))).collect(),
        notifications: own["notifications"].as_bool().unwrap_or(true),
        progress_bar: own["progress_bar"].as_bool().unwrap_or(true),
        scroll_lines: own["scroll_lines"].as_u64().unwrap_or(3).max(1) as usize,
        menu_rows: own["menu_rows"].as_u64().unwrap_or(6).max(1) as usize,
        copy_on_select: own["copy_on_select"].as_bool().unwrap_or(true),
        update_channel: level(&own["update_channel"], &crate::update::CHANNELS, "latest"),
        prune_sessions: own["prune_sessions"].as_str().unwrap_or("never").to_string(),
        on_start: string_list(&own["on_start"]),
        lsp: lsp_servers(&own["lsp"]),
        custom_providers: custom_providers(&own["custom_providers"]),
    }
}

/// A value from settings.json limited to `levels`; an unknown one loads as the default, like an unknown permission mode.
fn level(value: &Value, levels: &[&str], default: &str) -> String {
    value.as_str().filter(|level| levels.contains(level)).unwrap_or(default).to_string()
}

fn string_list(value: &Value) -> Vec<String> {
    value.as_array().into_iter().flatten().filter_map(|item| Some(item.as_str()?.to_string())).collect()
}

/// Entries without a name, with a built-in provider's name or without a base URL are left out.
fn custom_providers(own: &Value) -> Vec<CustomProvider> {
    let mut providers = Vec::new();
    for entry in own.as_array().into_iter().flatten() {
        let Some(name) = entry["name"].as_str().filter(|name| !name.is_empty() && !name.contains('/') && !models::PROVIDERS.contains(name)) else { continue };
        let preset = models::PRESETS.iter().find(|preset| preset.name == name);
        let Some(base_url) = entry["base_url"].as_str().or(preset.map(|p| p.base_url)).filter(|url| !url.is_empty()) else { continue };
        let api_key_env = entry["api_key_env"].as_str().or(preset.map(|p| p.api_key_env)).unwrap_or_default();
        let api = if entry["api"] == "anthropic" { CustomApi::Anthropic } else { CustomApi::OpenAi };
        let mut listed = Vec::new();
        for model in entry["models"].as_array().into_iter().flatten() {
            let Some(id) = model["id"].as_str() else { continue };
            let context_window = model["context_window"].as_u64().unwrap_or(models::DEFAULT_CONTEXT_WINDOW);
            listed.push(CustomModel { id: id.into(), context_window, max_tokens: model["max_tokens"].as_u64().unwrap_or(models::DEFAULT_MAX_TOKENS) });
        }
        providers.push(CustomProvider { name: name.into(), base_url: base_url.trim_end_matches('/').into(), api_key_env: api_key_env.into(), api, models: listed });
    }
    providers
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
    // plugin_settings and headroom_token hold tokens.
    crate::api::accounts::write_private(&path, &format!("{:#}\n", to_json(settings)))
}

fn to_json(settings: &Settings) -> Value {
    json!({
        "$schema": SCHEMA_URL,
        "model": settings.model,
        "thinking": settings.thinking,
        "default_model": settings.default_model,
        "default_thinking": settings.default_thinking,
        "speed": settings.speed,
        "cache_ttl": settings.cache_ttl,
        "permission_mode": settings.permission_mode.label(),
        "reviewer_model": settings.reviewer_model,
        "auto_compact": settings.auto_compact,
        "reserve_tokens": settings.reserve_tokens,
        "keep_recent_tokens": settings.keep_recent_tokens,
        "expand_tools": settings.expand_tools,
        "footer_cost": settings.footer_cost,
        "footer_account": settings.footer_account,
        "context_windows": settings.context_windows,
        "provider_sort": settings.provider_sort,
        "provider_order": settings.provider_order,
        "pinned_models": settings.pinned_models,
        "disabled_skills": settings.disabled_skills,
        "conflict_priority": settings.conflict_priority,
        "disabled_plugins": settings.disabled_plugins,
        "disabled_mcp": settings.disabled_mcp,
        "approved_mcp": settings.approved_mcp,
        "trusted_projects": settings.trusted_projects,
        "plugin_settings": settings.plugin_settings,
        "ponytail": settings.ponytail,
        "caveman": settings.caveman,
        "rtk": settings.rtk,
        "headroom": settings.headroom,
        "headroom_url": settings.headroom_url,
        "headroom_token": settings.headroom_token,
        "headroom_env": settings.headroom_env,
        "notifications": settings.notifications,
        "progress_bar": settings.progress_bar,
        "scroll_lines": settings.scroll_lines,
        "menu_rows": settings.menu_rows,
        "copy_on_select": settings.copy_on_select,
        "update_channel": settings.update_channel,
        "prune_sessions": settings.prune_sessions,
        "on_start": settings.on_start,
        "custom_providers": settings.custom_providers.iter().map(|p| json!({
            "name": p.name,
            "base_url": p.base_url,
            "api_key_env": p.api_key_env,
            "api": if p.api == CustomApi::Anthropic { "anthropic" } else { "openai" },
            "models": p.models.iter().map(|m| json!({"id": m.id, "context_window": m.context_window, "max_tokens": m.max_tokens})).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "lsp": settings.lsp.iter().map(|(language, server)| (language.clone(), json!({"command": server.command, "extensions": server.extensions, "roots": server.roots}))).collect::<serde_json::Map<_, _>>(),
        "condense": {"enabled": settings.condense, "model": settings.condense_model, "min_batch_chars": settings.condense_min_batch_chars},
        "subagents": {"concurrency": settings.subagent_concurrency, "implementation_effort": settings.subagent_effort, "model": settings.subagent_model},
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
        settings.subagent_model = "claude-sonnet-5-5".into();
        settings.reviewer_model = "claude-haiku-5-5".into();
        settings.speed = "ultrafast".into();
        settings.custom_providers = vec![CustomProvider {
            name: "local".into(),
            base_url: "http://127.0.0.1:8080/v1".into(),
            api_key_env: String::new(),
            api: CustomApi::Anthropic,
            models: vec![CustomModel { id: "qwen".into(), context_window: 65_536, max_tokens: 8_192 }],
        }];
        settings.cache_ttl = "1h".into();
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
        assert_eq!(properties["thinking"]["enum"], json!(THINKING_LEVELS));
        assert_eq!(properties["cache_ttl"]["enum"], json!(CACHE_TTLS));
        assert_eq!(properties["speed"]["enum"], json!(SPEEDS));
        assert_eq!(properties["subagents"]["properties"]["implementation_effort"]["enum"], json!(THINKING_LEVELS));
        assert_eq!(properties["permission_mode"]["enum"], json!(crate::agent::permissions::MODES.map(|mode| mode.label())));
        assert_eq!(properties["accounts"]["properties"]["when_all_over_limit"]["enum"], json!(Policy::ALL.map(|policy| policy.label())));
        assert_eq!(properties["ponytail"]["enum"], json!(crate::agent::modes::PONYTAIL_LEVELS));
        assert_eq!(properties["caveman"]["enum"], json!(crate::agent::modes::CAVEMAN_LEVELS));
        assert_eq!(properties["update_channel"]["enum"], json!(crate::update::CHANNELS));
        assert_eq!(properties["prune_sessions"]["enum"], json!(PRUNE_SESSIONS));
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
}
