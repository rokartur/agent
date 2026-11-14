use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::agent::permissions::{Mode, Rules};
use crate::agent::prompt::Skill;
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
    /// Anthropic turns send only the new messages and the API keeps the rest (the message threads beta).
    pub message_threads: bool,
    pub permission_mode: Mode,
    /// Allow, ask and deny rules per tool; edited in the file, not in /settings.
    pub permissions: Rules,
    /// The auto-mode reviewer; empty means `small_model`.
    pub reviewer_model: String,
    /// ImageGenerate's model: a Codex one (`gpt-image-2`, `openai-codex/` optional) or `google-antigravity/<id>`; empty takes
    /// gpt-image-2 on a ChatGPT login, else the first image model an Antigravity account lists.
    pub image_model: String,
    /// Session titles, long Video descriptions and compaction, and the reviewer and condense when theirs is empty;
    /// empty means the provider's small model.
    pub small_model: String,
    pub auto_compact: bool,
    pub reserve_tokens: u64,
    pub keep_recent_tokens: u64,
    pub expand_tools: bool,
    /// The session's cost after the context tokens in the footer.
    pub footer_cost: bool,
    /// The name of the account the current model's provider uses, before the model in the footer.
    pub footer_account: bool,
    pub condense: bool,
    /// Empty means `small_model`.
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
    /// Profile name to the skill names and `owner/repo` sources it holds; those skills load only while one of their profiles is active.
    pub skill_profiles: BTreeMap<String, Vec<String>>,
    /// Profiles from `skill_profiles` that `/profile:<name>` turned on.
    pub active_profiles: Vec<String>,
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
    /// One of `modes::OUTPUT_STYLES`, as Claude Code's `outputStyle`.
    pub output_style: String,
    pub rtk: bool,
    /// Offer the Code tool: a JavaScript script that makes several tool calls in one step.
    pub code_mode: bool,
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
    /// Minutes a session with no terminal attached and nothing running stays loaded before its server exits; 0 keeps it.
    pub unload_idle_sessions_after_minutes: u64,
    /// `agent serve --tailnet` runs only with this on, so a computer that never sets it opens no network port.
    pub serve_tailnet: bool,
    /// The port `agent serve --tailnet` listens on and remote environments are reached on.
    pub tailnet_port: u16,
    /// Tailscale names of other computers to offer as environments even when they are offline, besides the ones
    /// discovered online on the tailnet.
    pub environments: Vec<String>,
    /// `local`, another environment, or `auto` for the least loaded one with the directory: where `agent` starts a
    /// session without `--on`.
    pub default_environment: String,
    /// One of `MID_TURN`: a message sent while the agent works reaches it at the next tool call, or waits for the turn to end.
    pub mid_turn: String,
    /// One of `WORKING_ANIMATIONS`: the glyph beside the working label and on a turn's done row.
    pub working_animation: String,
    /// Shell commands the TUI runs in the background at start, e.g. a global tool update.
    pub on_start: Vec<String>,
    /// Commands that run before a matching tool call; one that fails refuses the call.
    pub before_tool: Vec<ToolHook>,
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
pub const MID_TURN: [&str; 2] = ["steering", "queued"];
pub const WORKING_ANIMATIONS: [&str; 5] = ["star", "fill", "radar", "arc", "diamond"];
/// Below this a condensed batch costs more in cache rewrites than its summary saves on later cache reads.
pub const CONDENSE_MIN_BATCH_CHARS: u64 = 10_000;
/// "agent" on a phone keypad.
pub const DEFAULT_TAILNET_PORT: u16 = 24368;

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

    /// A skill no profile holds loads unless disabled; one a profile holds loads only while one of its profiles is active, disabled or not.
    pub fn skill_enabled(&self, skill: &Skill) -> bool {
        let holders: Vec<&String> = self.skill_profiles.keys().filter(|profile| self.profile_holds(profile, skill)).collect();
        if holders.is_empty() {
            return !self.disabled_skills.contains(&skill.name);
        }
        holders.iter().any(|profile| self.active_profiles.contains(profile))
    }

    pub fn profile_holds(&self, profile: &str, skill: &Skill) -> bool {
        let entries = self.skill_profiles.get(profile).map_or(&[][..], Vec::as_slice);
        entries.iter().any(|entry| *entry == skill.name || skill.source.as_ref() == Some(entry))
    }

    pub fn toggle_profile(&mut self, profile: &str) {
        if self.active_profiles.iter().any(|active| active == profile) {
            self.active_profiles.retain(|active| active != profile);
        } else {
            self.active_profiles.push(profile.to_string());
        }
    }

    /// Taking out a skill its repo brought in swaps the repo entry for the names of the repo's other `skills`.
    pub fn toggle_profile_skill(&mut self, profile: &str, skill: &Skill, skills: &[Skill]) {
        let entries = self.skill_profiles.entry(profile.to_string()).or_default();
        if let Some(source) = skill.source.as_ref().filter(|source| entries.contains(source)) {
            entries.retain(|entry| entry != source && *entry != skill.name);
            for other in skills.iter().filter(|other| other.source.as_ref() == Some(source) && other.name != skill.name) {
                if !entries.contains(&other.name) {
                    entries.push(other.name.clone());
                }
            }
        } else if entries.contains(&skill.name) {
            entries.retain(|entry| *entry != skill.name);
        } else {
            entries.push(skill.name.clone());
        }
    }

    /// Adding a repo drops the names of its `skills`, which the repo entry now covers.
    pub fn toggle_profile_repo(&mut self, profile: &str, source: &str, skills: &[Skill]) {
        let entries = self.skill_profiles.entry(profile.to_string()).or_default();
        if entries.iter().any(|entry| entry == source) {
            entries.retain(|entry| entry != source);
        } else {
            entries.retain(|entry| !skills.iter().any(|skill| skill.name == *entry && skill.source.as_deref() == Some(source)));
            entries.push(source.to_string());
        }
    }

    /// Disables every skill of the repo, or enables them all when every one is already disabled.
    pub fn toggle_disabled_repo(&mut self, source: &str, skills: &[Skill]) {
        let names: Vec<&String> = skills.iter().filter(|skill| skill.source.as_deref() == Some(source)).map(|skill| &skill.name).collect();
        let all_disabled = names.iter().all(|name| self.disabled_skills.contains(name));
        self.disabled_skills.retain(|disabled| !names.contains(&disabled));
        if !all_disabled {
            self.disabled_skills.extend(names.into_iter().cloned());
        }
    }

    /// Also renames the profile in `active_profiles`; no `from` creates it.
    pub fn rename_profile(&mut self, from: Option<&str>, to: &str) {
        let entries = from.and_then(|from| self.skill_profiles.remove(from)).unwrap_or_default();
        self.skill_profiles.insert(to.to_string(), entries);
        for active in &mut self.active_profiles {
            if Some(active.as_str()) == from {
                *active = to.to_string();
            }
        }
    }

    pub fn delete_profile(&mut self, profile: &str) {
        self.skill_profiles.remove(profile);
        self.active_profiles.retain(|active| active != profile);
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
    crate::api::accounts::locked(&path(home), || {
        let file = read(home)?;
        let mut next = to_json(&saved_after(before, after, &from_json(&file)));
        // A key this version does not know was written by a newer agent; it passes through.
        for (key, value) in file.as_object().into_iter().flatten() {
            if next.get(key).is_none() {
                next[key] = value.clone();
            }
        }
        save(home, &next)
    })
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

/// `match` is a permission rule, `Tool` or `Tool(pattern)`; `{call}` in `command` is a file holding the call as JSON.
#[derive(Clone, PartialEq, Debug)]
pub struct ToolHook {
    pub matches: String,
    pub command: String,
}

/// Defaults for a missing file; an unreadable or broken one is an error, so the next save cannot write defaults over it.
pub fn load(home: &Path) -> Result<Settings, String> {
    checked(&read(home)?, home)
}

/// Keys a project file may set. Everything else is global-only: keys that run commands (on_start, lsp, before_tool),
/// move data (custom_providers, headroom_*, accounts, environments) or loosen approval (permission_mode, reviewer_model, trusted_projects, approved_mcp).
const PROJECT_KEYS: &[&str] = &[
    "model",
    "default_model",
    "thinking",
    "default_thinking",
    "small_model",
    "output_style",
    "caveman",
    "ponytail",
    "auto_compact",
    "reserve_tokens",
    "keep_recent_tokens",
    "condense",
    "context_windows",
    "expand_tools",
    "subagents",
    "rtk",
];

/// The global settings with the trusted project's `.agents/settings.json` and `settings.local.json` over them.
/// `trust` is the --trust flag; otherwise only the global file's `trusted_projects` decides.
pub fn load_in(home: &Path, cwd: &Path, trust: bool) -> Result<Settings, String> {
    checked(&layered(home, cwd, trust)?.0, home)
}

/// One line per key a project file sets that only the global file may set.
pub fn ignored_project_keys(home: &Path, cwd: &Path, trust: bool) -> Vec<String> {
    layered(home, cwd, trust).map(|(_, ignored)| ignored).unwrap_or_default()
}

fn layered(home: &Path, cwd: &Path, trust: bool) -> Result<(Value, Vec<String>), String> {
    let mut merged = read(home)?;
    let mut ignored = Vec::new();
    let trusted = trust || crate::agent::prompt::is_trusted(&string_list(&merged["trusted_projects"]), cwd);
    let global_dir = home.join(".agents");
    // Farthest first, so the file nearest cwd wins.
    for dir in crate::agent::prompt::project_agents_dirs(cwd, trusted).iter().rev().filter(|dir| **dir != global_dir) {
        for name in ["settings.json", "settings.local.json"] {
            let file = dir.join(name);
            let Value::Object(layer) = read_file(&file)? else { continue };
            if !merged.is_object() {
                merged = json!({});
            }
            for (key, value) in layer {
                if PROJECT_KEYS.contains(&key.as_str()) {
                    merged[key] = value;
                    continue;
                }
                if key != "permissions" {
                    ignored.push(format!("{}: ignored \"{key}\", a project cannot set it", file.display()));
                    continue;
                }
                // A repository may tighten approval, never loosen it.
                if !merged["permissions"].is_object() {
                    merged["permissions"] = json!({});
                }
                for list in ["ask", "deny"] {
                    let mut rules = string_list(&merged["permissions"][list]);
                    rules.extend(string_list(&value[list]));
                    merged["permissions"][list] = json!(rules);
                }
                if !value["allow"].is_null() {
                    ignored.push(format!("{}: ignored \"permissions.allow\", a project cannot set it", file.display()));
                }
            }
        }
    }
    Ok((merged, ignored))
}

fn checked(own: &Value, home: &Path) -> Result<Settings, String> {
    let settings = from_json(own);
    crate::agent::permissions::check_rules(&settings.permissions).map_err(|e| format!("{}: {e}", path(home).display()))?;
    // A hook that could never match or run would guard nothing, silently.
    if let Some(hook) = settings.before_tool.iter().find(|hook| crate::agent::permissions::parse_rule(&hook.matches).is_none() || hook.command.trim().is_empty()) {
        return Err(format!("{}: before_tool: {{\"match\": \"{}\", \"command\": \"{}\"}} needs a match that is Tool or Tool(pattern) and a command", path(home).display(), hook.matches, hook.command));
    }
    if let Some(hook) = settings.before_tool.iter().find(|hook| crate::agent::permissions::pattern_never_matches(&hook.matches)) {
        return Err(format!("{}: before_tool: \"{}\" has a pattern, which only {} take; name the tool alone", path(home).display(), hook.matches, crate::agent::permissions::SUBJECT_TOOLS.join(", ")));
    }
    Ok(settings)
}

fn read(home: &Path) -> Result<Value, String> {
    read_file(&path(home))
}

fn read_file(path: &Path) -> Result<Value, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| format!("{} is broken: {e}. Fix or remove it by hand.", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Value::Null),
        Err(e) => Err(format!("Cannot read {}: {e}", path.display())),
    }
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
        message_threads: own["message_threads"].as_bool().unwrap_or(true),
        permission_mode: own["permission_mode"].as_str().and_then(Mode::parse).unwrap_or(Mode::Ask),
        permissions: Rules { allow: string_list(&own["permissions"]["allow"]), ask: string_list(&own["permissions"]["ask"]), deny: string_list(&own["permissions"]["deny"]) },
        reviewer_model: own["reviewer_model"].as_str().unwrap_or_default().to_string(),
        image_model: own["image_model"].as_str().unwrap_or_default().to_string(),
        small_model: own["small_model"].as_str().unwrap_or_default().to_string(),
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
        skill_profiles: own["skill_profiles"].as_object().into_iter().flatten().map(|(profile, entries)| (profile.clone(), string_list(entries))).collect(),
        active_profiles: string_list(&own["active_profiles"]),
        conflict_priority: level(&own["conflict_priority"], &CONFLICT_PRIORITIES, "project"),
        disabled_plugins: string_list(&own["disabled_plugins"]),
        disabled_mcp: string_list(&own["disabled_mcp"]),
        approved_mcp: string_list(&own["approved_mcp"]),
        trusted_projects: string_list(&own["trusted_projects"]),
        plugin_settings: if own["plugin_settings"].is_object() { own["plugin_settings"].clone() } else { serde_json::json!({}) },
        ponytail: level(&own["ponytail"], &crate::agent::modes::PONYTAIL_LEVELS, "full"),
        caveman: level(&own["caveman"], &crate::agent::modes::CAVEMAN_LEVELS, "off"),
        output_style: level(&own["output_style"], &crate::agent::modes::OUTPUT_STYLES, "default"),
        rtk: own["rtk"].as_bool().unwrap_or(true),
        code_mode: own["code_mode"].as_bool().unwrap_or(false),
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
        unload_idle_sessions_after_minutes: own["unload_idle_sessions_after_minutes"].as_u64().unwrap_or(30),
        serve_tailnet: own["serve_tailnet"].as_bool().unwrap_or(false),
        tailnet_port: own["tailnet_port"].as_u64().and_then(|port| u16::try_from(port).ok()).filter(|port| *port > 0).unwrap_or(DEFAULT_TAILNET_PORT),
        environments: string_list(&own["environments"]),
        default_environment: own["default_environment"].as_str().filter(|name| !name.is_empty()).unwrap_or(crate::tailnet::LOCAL).to_string(),
        mid_turn: level(&own["mid_turn"], &MID_TURN, "steering"),
        working_animation: level(&own["working_animation"], &WORKING_ANIMATIONS, "star"),
        on_start: string_list(&own["on_start"]),
        // An entry without its strings is kept empty, so load refuses it.
        before_tool: own["before_tool"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|hook| ToolHook { matches: hook["match"].as_str().unwrap_or_default().into(), command: hook["command"].as_str().unwrap_or_default().into() })
            .collect(),
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
fn save(home: &Path, settings: &Value) -> Result<(), String> {
    let path = path(home);
    std::fs::create_dir_all(path.parent().unwrap()).map_err(|e| format!("Cannot create {}: {e}", path.display()))?;
    // plugin_settings and headroom_token hold tokens.
    crate::api::accounts::write_private(&path, &format!("{settings:#}\n"))
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
        "message_threads": settings.message_threads,
        "permission_mode": settings.permission_mode.label(),
        "permissions": {"allow": settings.permissions.allow, "ask": settings.permissions.ask, "deny": settings.permissions.deny},
        "reviewer_model": settings.reviewer_model,
        "image_model": settings.image_model,
        "small_model": settings.small_model,
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
        "skill_profiles": settings.skill_profiles,
        "active_profiles": settings.active_profiles,
        "conflict_priority": settings.conflict_priority,
        "disabled_plugins": settings.disabled_plugins,
        "disabled_mcp": settings.disabled_mcp,
        "approved_mcp": settings.approved_mcp,
        "trusted_projects": settings.trusted_projects,
        "plugin_settings": settings.plugin_settings,
        "ponytail": settings.ponytail,
        "caveman": settings.caveman,
        "output_style": settings.output_style,
        "rtk": settings.rtk,
        "code_mode": settings.code_mode,
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
        "unload_idle_sessions_after_minutes": settings.unload_idle_sessions_after_minutes,
        "serve_tailnet": settings.serve_tailnet,
        "tailnet_port": settings.tailnet_port,
        "environments": settings.environments,
        "default_environment": settings.default_environment,
        "mid_turn": settings.mid_turn,
        "working_animation": settings.working_animation,
        "on_start": settings.on_start,
        "before_tool": settings.before_tool.iter().map(|hook| json!({"match": hook.matches, "command": hook.command})).collect::<Vec<_>>(),
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
mod tests;
