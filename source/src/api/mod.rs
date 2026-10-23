use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

pub mod accounts;
pub mod codex;
pub mod copilot;
pub mod models;
pub mod oauth;
pub mod openai_chat;

use crate::settings::{CustomApi, CustomProvider, Settings};
use accounts::{Account, Candidate, Pick, Policy, Secret, Usage as Windows};
use models::Model;

#[cfg(not(test))]
const RETRY_DELAYS_SECS: [u64; 4] = [2, 5, 15, 30];
#[cfg(test)]
const RETRY_DELAYS_SECS: [u64; 4] = [0; 4];
const THINKING_BETA: &str = "thinking-binding-controls-2026-08-01";
/// pi-ai sends OAuth requests as Claude Code: this user agent, its beta flags and its identity as the first system block.
pub const CLAUDE_CODE_USER_AGENT: &str = "claude-cli/2.1.280";
const CLAUDE_CODE_IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";
const USAGE_TTL: Duration = Duration::from_secs(60);
/// How long an account that answered 429 is skipped.
const LIMITED_FOR: Duration = Duration::from_secs(60);
/// `headroom proxy`'s default address; it serves both /v1/messages and /backend-api/codex/responses.
pub const HEADROOM_URL: &str = "http://127.0.0.1:8787";

/// Talks to Anthropic, Codex and OpenRouter. Clones share the accounts, the usage cache and the current pick.
#[derive(Clone)]
pub struct Client {
    http: ureq::Agent,
    home: PathBuf,
    anthropic_base: String,
    chatgpt_base: String,
    openrouter_base: String,
    /// Replaced by each load_catalog; clones share it.
    catalog: Arc<RwLock<Arc<[Model]>>>,
    /// Codex prompt_cache_key; one per process, like pi's session id.
    cache_key: String,
    state: Arc<Mutex<State>>,
}

#[derive(Default)]
struct State {
    accounts: Vec<Account>,
    usage: HashMap<String, (Instant, Option<Windows>)>,
    limited: HashMap<String, Instant>,
    pinned: HashMap<String, String>,
    current: HashMap<String, String>,
    threshold: f64,
    policy: Option<Policy>,
    fallback_model: String,
    /// The headroom proxy requests go through, `None` when they go straight to the provider.
    headroom: Option<String>,
    /// Sent to `headroom` as `x-headroom-proxy-token` when not empty.
    headroom_token: String,
    provider_order: Vec<&'static str>,
    custom_providers: Vec<CustomProvider>,
}

#[derive(Default)]
pub struct Reply {
    pub message: Value,
    pub stop_reason: String,
    pub usage: Usage,
    /// The part of `usage.cache_write` cached for 1h, which bills 2x input against 1.25x for 5m; None when the API gave no split.
    pub cache_write_1h: Option<u64>,
    /// Tokens the next request starts from. Differs from `usage`, which sums the iterations of a server tool loop.
    pub context_tokens: u64,
    /// The API's id for this response: Anthropic's `msg_…`, Codex's `resp_…`.
    pub id: String,
    /// Who served it, set by `Client::send`: an account switch or fallback can move a request off the asked-for account and model.
    pub provider: String,
    pub account_id: Option<String>,
    pub model: String,
    /// Tool calls whose arguments were not valid JSON; their `input` in `message` is `{}` and they must not run.
    pub bad_tool_inputs: Vec<BadToolInput>,
}

pub struct BadToolInput {
    pub id: String,
    pub error: String,
    pub raw: String,
}

pub enum Delta<'a> {
    Text(&'a str),
    Thinking(&'a str),
    /// A retry or an account switch, shown as a system line.
    Notice(&'a str),
}

#[derive(Default, Clone, Copy, PartialEq, Debug)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

impl Usage {
    pub fn add(&mut self, other: Usage) {
        self.input += other.input;
        self.output += other.output;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
    }

    pub fn context_tokens(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_write
    }
}

/// One row of /account and the footer.
#[derive(Clone)]
pub struct AccountView {
    pub provider: String,
    pub name: String,
    pub oauth: bool,
    pub account_id: Option<String>,
    pub usage: Option<Windows>,
    pub current: bool,
    pub pinned: bool,
}

enum Choice {
    Use(Account),
    Fallback,
}

impl Client {
    /// Loads `~/.agents/accounts.json`; a provider without an account uses `ANTHROPIC_API_KEY` or `OPENROUTER_API_KEY` as one named `env`.
    pub fn new(home: &Path) -> Result<Client, String> {
        let base = |var: &str, default: &str| std::env::var(var).unwrap_or_else(|_| default.into()).trim_end_matches('/').to_string();
        let client = Client {
            // Every request streams, so headers come before the model thinks; a stuck server would otherwise hold the turn past Esc.
            http: ureq::Agent::config_builder().http_status_as_error(false).timeout_connect(Some(Duration::from_secs(30))).timeout_recv_response(Some(STREAM_IDLE)).build().into(),
            home: home.to_path_buf(),
            anthropic_base: base("ANTHROPIC_BASE_URL", "https://api.anthropic.com"),
            chatgpt_base: base("CHATGPT_BASE_URL", "https://chatgpt.com/backend-api"),
            openrouter_base: base("OPENROUTER_BASE_URL", "https://openrouter.ai/api"),
            catalog: Arc::default(),
            cache_key: format!("agent-{}-{}", std::process::id(), crate::process::now_ms()),
            state: Arc::new(Mutex::new(State { threshold: 90.0, ..State::default() })),
        };
        client.reload(accounts::load(home)?);
        Ok(client)
    }

    /// Builds the model catalog live from every account's model list; returns one error per list that failed.
    pub fn load_catalog(&self) -> Vec<String> {
        let accounts: Vec<Account> = self.state.lock().unwrap().accounts.iter().filter(|a| a.provider != "openrouter").cloned().collect();
        let custom_providers = self.state.lock().unwrap().custom_providers.clone();
        // One request per account, about 0.3-0.7 s each; in parallel the load waits only for the slowest.
        let (openrouter, listed, custom_listed) = std::thread::scope(|scope| {
            // Public list, so OpenRouter models show in the picker before /login.
            let openrouter = scope.spawn(|| fetch_models(models_agent().get(format!("{}/v1/models", self.openrouter_base)), models::parse_openrouter));
            let lists: Vec<_> = accounts.iter().map(|account| scope.spawn(|| self.list_models(account))).collect();
            let custom_lists: Vec<_> = custom_providers.iter().map(|provider| scope.spawn(|| custom_models(provider))).collect();
            (
                openrouter.join().expect("model list thread"),
                lists.into_iter().map(|l| l.join().expect("model list thread")).collect::<Vec<_>>(),
                custom_lists.into_iter().map(|l| l.join().expect("model list thread")).collect::<Vec<_>>(),
            )
        });
        let mut catalog = Vec::new();
        let mut errors = Vec::new();
        match openrouter {
            Ok(models) => catalog.extend(models),
            Err(error) => errors.push(format!("openrouter: {error}")),
        }
        for (account, list) in accounts.iter().zip(listed) {
            match list {
                Ok(models) => models::merge(&mut catalog, models, &account.name),
                Err(error) => errors.push(format!("{}: {error}", key(account))),
            }
        }
        for (provider, list) in custom_providers.iter().zip(custom_listed) {
            match list {
                Ok(models) => {
                    // A provider named like an OpenRouter vendor (deepseek) lists the same ids; the direct one wins.
                    catalog.retain(|known| !models.iter().any(|model| model.id == known.id));
                    catalog.extend(models);
                }
                Err(error) => errors.push(format!("{}: {error}", provider.name)),
            }
        }
        catalog.extend(models::free_routers(&catalog));
        *self.catalog.write().unwrap() = catalog.into();
        errors
    }

    /// Empty until load_catalog finishes.
    pub fn catalog(&self) -> Arc<[Model]> {
        self.catalog.read().unwrap().clone()
    }

    /// The models this account can use right now; see models-endpoints.md.
    fn list_models(&self, account: &Account) -> Result<Vec<Model>, String> {
        let account = self.fresh(account.clone())?;
        let http = models_agent();
        let request = match (&account.secret, account.provider.as_str()) {
            (Secret::ApiKey(key), "anthropic") => http.get(format!("{}/v1/models?limit=1000", self.anthropic_base)).header("anthropic-version", "2023-06-01").header("x-api-key", key),
            (Secret::OAuth(tokens), "anthropic") => http
                .get(format!("{}/v1/models?limit=1000", self.anthropic_base))
                .header("anthropic-version", "2023-06-01")
                .header("authorization", format!("Bearer {}", tokens.access))
                .header("anthropic-beta", "oauth-2025-04-20"),
            (Secret::OAuth(tokens), copilot::PROVIDER) => {
                let mut request = http.get(format!("{}/models", copilot::base_url(&tokens.access))).header("authorization", format!("Bearer {}", tokens.access));
                for (name, value) in copilot::HEADERS {
                    request = request.header(name, value);
                }
                return fetch_models(request, copilot::parse_models);
            }
            (Secret::OAuth(tokens), _) => http
                // An old client_version gets an empty list, a missing one a 400.
                .get(format!("{}/codex/models?client_version=0.160.0", self.chatgpt_base))
                .header("authorization", format!("Bearer {}", tokens.access))
                .header("chatgpt-account-id", tokens.account_id.as_deref().unwrap_or_default()),
            (Secret::ApiKey(_), provider) => return Err(format!("{provider} accounts need /login, not an API key.")),
        };
        let parse = if account.provider == "anthropic" { models::parse_anthropic } else { models::parse_codex };
        fetch_models(request, parse)
    }

    pub fn configure(&self, settings: &Settings) {
        let mut state = self.state.lock().unwrap();
        state.threshold = settings.switch_at_percent as f64;
        state.policy = Some(settings.when_all_over_limit);
        state.fallback_model = settings.fallback_model.clone();
        let state = &mut *state;
        // A new pin from /account wins over the account the last request used.
        state.current.retain(|provider, _| state.pinned.get(provider) == settings.default_accounts.get(provider));
        state.pinned = settings.default_accounts.clone();
        state.headroom = match settings.headroom_url.as_str() {
            _ if !settings.headroom => None,
            "" => crate::process::installed("headroom").then(|| HEADROOM_URL.to_string()),
            url => Some(url.to_string()),
        };
        state.headroom_token = settings.headroom_token.clone();
        state.provider_order = settings.sort_providers(&models::PROVIDERS);
        state.custom_providers = settings.custom_providers.clone();
    }

    fn reload(&self, mut saved: Vec<Account>) {
        for (provider, var) in [("anthropic", "ANTHROPIC_API_KEY"), ("openrouter", "OPENROUTER_API_KEY")] {
            let env_key = std::env::var(var).ok().filter(|k| !k.is_empty());
            if let Some(key) = env_key.filter(|_| !saved.iter().any(|a| a.provider == provider)) {
                saved.push(Account { provider: provider.into(), name: "env".into(), secret: Secret::ApiKey(key) });
            }
        }
        self.state.lock().unwrap().accounts = saved;
    }

    /// Streams one model turn into `on_delta` on a fitting account. Setting `abort` ends the stream with stop reason "aborted".
    pub fn send(&self, request: &Value, abort: &AtomicBool, on_delta: &mut dyn FnMut(Delta)) -> Result<Reply, String> {
        self.send_on_account(request, abort, on_delta, true)
    }

    /// Like `send` for callers that only read the `Reply`, so a stream that breaks off partway can still be retried.
    pub fn send_quiet(&self, request: &Value, abort: &AtomicBool) -> Result<Reply, String> {
        self.send_on_account(request, abort, &mut |_| {}, false)
    }

    fn send_on_account(&self, request: &Value, abort: &AtomicBool, on_delta: &mut dyn FnMut(Delta), shown: bool) -> Result<Reply, String> {
        let mut request = request.clone();
        let mut fell_back = false;
        let mut rate_limited: Vec<String> = Vec::new();
        let mut attempt = 0;
        loop {
            let model = self.model_of(&request)?;
            let account = match self.choose(&model, fell_back, on_delta)? {
                Choice::Use(account) => match self.fresh(account.clone()) {
                    Ok(account) => account,
                    // A failed refresh (network, revoked token) moves the request to the next account once, like a quota error.
                    Err(error) if !rate_limited.contains(&key(&account)) => {
                        on_delta(Delta::Notice(&format!("{error} Trying another account.")));
                        self.state.lock().unwrap().limited.insert(key(&account), Instant::now() + LIMITED_FOR);
                        rate_limited.push(key(&account));
                        continue;
                    }
                    Err(error) => return Err(error),
                },
                Choice::Fallback => {
                    let provider = &model.provider;
                    let fallback = self.fallback_model(provider).ok_or(format!("Every {provider} account is over its limit and accounts.fallback_model names no other provider's model."))?;
                    on_delta(Delta::Notice(&format!("Switched to {}: every {provider} account is over its limit", fallback.id)));
                    moved_to(&mut request, &fallback);
                    fell_back = true;
                    continue;
                }
            };
            let mut streamed = false;
            let sent = self.try_send(&account, &request, abort, &mut |delta| {
                streamed |= !matches!(delta, Delta::Notice(_));
                on_delta(delta);
            });
            let error = match sent {
                Ok(reply) => {
                    let model = request["model"].as_str().unwrap_or_default().to_string();
                    return Ok(Reply { provider: account.provider.clone(), account_id: account.account_id().map(String::from), model, ..reply });
                }
                Err(error) => error,
            };
            // A retry streams the whole reply again, and text already shown (stdout in --print) cannot be taken back.
            if streamed && shown {
                return Err(format!("{} The reply broke off partway and was not retried.", error.message));
            }
            // Which plans and models take a Codex tier changes often, so a rejected one retries once at standard speed.
            if account.provider == "openai-codex" && error.message.contains("service_tier") && request.as_object_mut().expect("a request is an object").remove("speed").is_some() {
                on_delta(Delta::Notice(&format!("{} Retrying at standard speed.", error.message)));
                continue;
            }
            let key = key(&account);
            // A quota error moves the same request to the next account once; a second one waits like any retry.
            if error.quota && !rate_limited.contains(&key) {
                self.state.lock().unwrap().limited.insert(key.clone(), Instant::now() + LIMITED_FOR);
                rate_limited.push(key);
                continue;
            }
            let out_of_retries = attempt == RETRY_DELAYS_SECS.len();
            // Only a fallback_model the user set: switching providers unasked could cost more.
            let failover_set = !self.state.lock().unwrap().fallback_model.is_empty();
            if error.retryable
                && out_of_retries
                && !fell_back
                && failover_set
                && !abort.load(Ordering::Relaxed)
                && let Some(fallback) = self.fallback_model(&model.provider)
            {
                on_delta(Delta::Notice(&format!("{} Switched to {} for this request.", error.message, fallback.id)));
                moved_to(&mut request, &fallback);
                fell_back = true;
                attempt = 0;
                continue;
            }
            if !error.retryable || out_of_retries || abort.load(Ordering::Relaxed) {
                return Err(error.message);
            }
            on_delta(Delta::Notice(&format!("{} Retrying in {}s.", error.message, RETRY_DELAYS_SECS[attempt])));
            let wake = Instant::now() + Duration::from_secs(RETRY_DELAYS_SECS[attempt]);
            while Instant::now() < wake {
                if abort.load(Ordering::Relaxed) {
                    return Err(error.message);
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            attempt += 1;
        }
    }

    fn custom_provider(&self, name: &str) -> Option<CustomProvider> {
        self.state.lock().unwrap().custom_providers.iter().find(|p| p.name == name).cloned()
    }

    fn model_of(&self, request: &Value) -> Result<Model, String> {
        let id = request["model"].as_str().unwrap_or_default();
        self.catalog().iter().find(|m| m.id == id).cloned().ok_or(format!("Unknown model {id}."))
    }

    /// Picks the account for this request and reports a change of account as "Switched to <name>: <reason>".
    fn choose(&self, model: &Model, fell_back: bool, on_delta: &mut dyn FnMut(Delta)) -> Result<Choice, String> {
        let provider = model.provider.as_str();
        if let Some(custom) = self.custom_provider(provider) {
            return Ok(Choice::Use(Account { provider: custom.name.clone(), name: custom.name, secret: Secret::ApiKey(custom_key(&custom.api_key_env)?) }));
        }
        let saved: Vec<Account> = self.state.lock().unwrap().accounts.iter().filter(|a| a.provider == provider).cloned().collect();
        if saved.is_empty() {
            let hint = match provider {
                "anthropic" => " or set ANTHROPIC_API_KEY",
                "openrouter" => " or set OPENROUTER_API_KEY",
                _ => "",
            };
            return Err(format!("No {provider} account. Use /login{hint}."));
        }
        let (accounts, without_access): (Vec<Account>, Vec<Account>) = saved.into_iter().partition(|a| models::serves(&self.catalog(), model, &a.name));
        if accounts.is_empty() {
            return Err(format!("No {provider} account has access to {}.", model.id));
        }
        self.refresh_usage(&accounts);
        let mut state = self.state.lock().unwrap();
        let now = Instant::now();
        let candidates: Vec<Candidate> =
            accounts.iter().map(|a| Candidate { usage: state.usage.get(&key(a)).and_then(|(_, u)| u.clone()), limited: state.limited.get(&key(a)).is_some_and(|until| *until > now) }).collect();
        let pinned = state.pinned.get(provider).and_then(|name| accounts.iter().position(|a| &a.name == name));
        let policy = state.policy.unwrap_or(Policy::LeastUsed);
        // After falling back once, the other provider's accounts are the last resort, so least-used decides.
        let policy = if fell_back && policy == Policy::OtherProvider { Policy::LeastUsed } else { policy };
        let threshold = state.threshold;
        let (index, over_limit) = match accounts::pick(&candidates, threshold, policy, pinned) {
            Pick::Account(index) => (index, false),
            Pick::OverLimit(index) => (index, true),
            Pick::OtherProvider => return Ok(Choice::Fallback),
            Pick::Blocked { resets_at } => {
                let reset = resets_at.map_or(String::new(), |at| format!(" The first window resets {}.", accounts::resets_in(at)));
                return Err(format!("Every {provider} account is over {threshold}% of its limit.{reset}"));
            }
        };
        let chosen = &accounts[index];
        let previous = state.current.insert(provider.to_string(), chosen.name.clone());
        let Some(previous) = previous.filter(|p| *p != chosen.name) else { return Ok(Choice::Use(chosen.clone())) };
        let previous_index = accounts.iter().position(|a| a.name == previous);
        let reason = match previous_index.map(|i| &candidates[i]) {
            Some(c) if c.limited => format!("{previous} hit a rate limit"),
            Some(Candidate { usage: Some(u), .. }) if u.peak() >= threshold => format!("{previous} is at {:.0}%", u.peak()),
            _ if Some(index) == pinned => "picked in /account".into(),
            None if without_access.iter().any(|a| a.name == previous) => format!("{previous} has no access to {}", model.id),
            None => format!("{previous} is gone"),
            Some(_) => "it comes first".into(),
        };
        let reason = if over_limit { format!("{reason}; every account is over {threshold}%, this one has the most room") } else { reason };
        on_delta(Delta::Notice(&format!("Switched to {}: {reason}", chosen.name)));
        Ok(Choice::Use(chosen.clone()))
    }

    /// Fetches windows older than a minute for OAuth accounts; a failed fetch counts as unknown, which does not block.
    fn refresh_usage(&self, accounts: &[Account]) {
        for account in accounts.iter().filter(|a| a.is_oauth()) {
            // Stamped before the fetch, under the same lock as the check, so concurrent sends do not fetch it too.
            let stale = {
                let mut state = self.state.lock().unwrap();
                match state.usage.get_mut(&key(account)) {
                    Some((at, _)) if at.elapsed() < USAGE_TTL => false,
                    Some((at, _)) => {
                        *at = Instant::now();
                        true
                    }
                    None => {
                        state.usage.insert(key(account), (Instant::now(), None));
                        true
                    }
                }
            };
            if !stale {
                continue;
            }
            let base = if account.provider == "anthropic" { &self.anthropic_base } else { &self.chatgpt_base };
            let usage = self.fresh(account.clone()).ok().and_then(|account| accounts::fetch_usage(&account, base).ok());
            self.state.lock().unwrap().usage.insert(key(account), (Instant::now(), usage));
        }
    }

    /// The account with tokens valid for five more minutes, refreshing and saving them when needed.
    fn fresh(&self, account: Account) -> Result<Account, String> {
        let Secret::OAuth(tokens) = &account.secret else { return Ok(account) };
        let valid = |t: &oauth::Tokens| t.expires > crate::process::now_ms() + oauth::REFRESH_MARGIN_MS;
        if valid(tokens) {
            return Ok(account);
        }
        // Refresh tokens rotate, so the check and the refresh share the lock: another process or thread may have refreshed first.
        let mut fresh = None;
        let saved = accounts::update(&self.home, |list| {
            let Some(slot) = list.iter_mut().find(|a| a.provider == account.provider && a.name == account.name) else {
                return Err(format!("{} is no longer saved. Use /login again.", account.name));
            };
            let Secret::OAuth(tokens) = &slot.secret else {
                fresh = Some(slot.clone());
                return Ok(());
            };
            if !valid(tokens) {
                let refreshed = match slot.provider.as_str() {
                    "anthropic" => oauth::refresh_claude(&tokens.refresh),
                    "openai-codex" => oauth::refresh_codex(&tokens.refresh),
                    copilot::PROVIDER => copilot::refresh(&tokens.refresh),
                    other => Err(format!("{other} has no OAuth refresh.")),
                }
                .map_err(|e| format!("Cannot refresh {}: {e} Use /login again.", slot.name))?;
                slot.secret = Secret::OAuth(oauth::Tokens { account_id: refreshed.account_id.clone().or_else(|| tokens.account_id.clone()), ..refreshed });
            }
            fresh = Some(slot.clone());
            Ok(())
        })?;
        self.reload(saved);
        Ok(fresh.expect("update ran the closure"))
    }

    /// The model `other-provider` and failover move a request to: `accounts.fallback_model`, or the other provider's default.
    fn fallback_model(&self, provider: &str) -> Option<Model> {
        let configured = self.state.lock().unwrap().fallback_model.clone();
        let catalog = self.catalog();
        let model = match configured.as_str() {
            "" if provider == "anthropic" => catalog.iter().find(|m| m.provider == "openai-codex"),
            "" => catalog.iter().find(|m| m.id == "claude-opus-5-5"),
            id => catalog.iter().find(|m| m.id == id),
        };
        model.filter(|m| m.provider != provider).cloned()
    }

    fn try_send(&self, account: &Account, request: &Value, abort: &AtomicBool, on_delta: &mut dyn FnMut(Delta)) -> Result<Reply, ApiError> {
        let (headroom, headroom_token) = {
            let state = self.state.lock().unwrap();
            (state.headroom.clone(), state.headroom_token.clone())
        };
        let anthropic_base = headroom.clone().unwrap_or_else(|| self.anthropic_base.clone());
        let chatgpt_base = headroom.as_ref().map_or_else(|| self.chatgpt_base.clone(), |url| format!("{url}/backend-api"));
        let proxy_token = headroom.as_ref().map(|_| headroom_token).filter(|token| !token.is_empty());
        let custom = self.custom_provider(&account.provider);
        let (call, body) = match (account.provider.as_str(), &account.secret) {
            (_, Secret::ApiKey(key)) if let Some(custom) = &custom => {
                let id = request["model"].as_str().unwrap_or_default();
                let id = id.split_once('/').map_or(id, |(_, id)| id);
                if custom.api == CustomApi::Anthropic {
                    let call = self.http.post(format!("{}/v1/messages", custom.base_url)).header("anthropic-version", "2023-06-01").header("x-api-key", key);
                    let mut body = request.clone();
                    body["model"] = json!(id);
                    // Adaptive thinking, its effort and speed are Anthropic-only fields that other Messages servers reject.
                    for field in ["thinking", "output_config", "speed"] {
                        body.as_object_mut().expect("a request is an object").remove(field);
                    }
                    (call, body)
                } else {
                    let call = self.http.post(format!("{}/chat/completions", custom.base_url)).header("accept", "text/event-stream");
                    let call = if key.is_empty() { call } else { call.header("authorization", format!("Bearer {key}")) };
                    (call, openai_chat::request(request, id))
                }
            }
            ("anthropic", secret) => {
                let call = with_proxy_token(self.http.post(format!("{anthropic_base}/v1/messages")), proxy_token.as_deref()).header("anthropic-version", "2023-06-01");
                let mut body = request.clone();
                // Anthropic has one fast tier.
                if body["speed"] == "ultrafast" {
                    body["speed"] = json!("fast");
                }
                match secret {
                    Secret::ApiKey(key) => (call.header("x-api-key", key).header("anthropic-beta", THINKING_BETA), body),
                    Secret::OAuth(tokens) => {
                        let mut system = vec![json!({"type": "text", "text": CLAUDE_CODE_IDENTITY})];
                        system.extend(body["system"].as_array().cloned().unwrap_or_default());
                        body["system"] = json!(system);
                        let call = call
                            .header("authorization", format!("Bearer {}", tokens.access))
                            .header("anthropic-beta", format!("claude-code-20250219,oauth-2025-04-20,{THINKING_BETA}"))
                            .header("user-agent", CLAUDE_CODE_USER_AGENT)
                            .header("x-app", "cli")
                            .header("anthropic-dangerous-direct-browser-access", "true");
                        (call, body)
                    }
                }
            }
            ("openrouter", Secret::ApiKey(key)) => {
                // OpenRouter's Anthropic-compatible Messages endpoint serves every model, so the Anthropic stream parser reads it.
                let call = self
                    .http
                    .post(format!("{}/v1/messages", self.openrouter_base))
                    .header("anthropic-version", "2023-06-01")
                    .header("anthropic-beta", THINKING_BETA)
                    .header("authorization", format!("Bearer {key}"));
                let mut body = request.clone();
                // A clef router asks for its first free model; OpenRouter moves to the next on a rate limit or outage.
                if let Some((first, rest)) = models::free_route(&self.catalog(), request["model"].as_str().unwrap_or_default()).split_first() {
                    body["model"] = json!(first.id);
                    body["fallbacks"] = rest.iter().map(|m| json!({"model": m.id})).collect();
                }
                // Speed is an Anthropic API field, unknown to OpenRouter.
                body.as_object_mut().expect("a request is an object").remove("speed");
                // OpenRouter rejects the whole request when a non-Anthropic model gets block_binding.
                if !body["model"].as_str().unwrap_or_default().starts_with("anthropic/")
                    && let Some(thinking) = body.get_mut("thinking").and_then(Value::as_object_mut)
                {
                    thinking.remove("block_binding");
                }
                (call, body)
            }
            (copilot::PROVIDER, Secret::OAuth(tokens)) => {
                let id = request["model"].as_str().unwrap_or_default();
                let body = openai_chat::request(request, id.strip_prefix("github-copilot/").unwrap_or(id));
                let mut call = self
                    .http
                    .post(format!("{}/chat/completions", copilot::base_url(&tokens.access)))
                    .header("accept", "text/event-stream")
                    .header("authorization", format!("Bearer {}", tokens.access));
                for (name, value) in copilot::HEADERS.into_iter().chain(copilot::request_headers(&body)) {
                    call = call.header(name, value);
                }
                (call, body)
            }
            ("openai-codex", Secret::OAuth(tokens)) => {
                let call = with_proxy_token(self.http.post(format!("{chatgpt_base}/codex/responses")), proxy_token.as_deref())
                    .header("authorization", format!("Bearer {}", tokens.access))
                    .header("chatgpt-account-id", tokens.account_id.as_deref().unwrap_or_default())
                    .header("OpenAI-Beta", "responses=experimental")
                    .header("originator", "pi")
                    .header("accept", "text/event-stream")
                    .header("session-id", &self.cache_key);
                (call, codex::request(request, &self.cache_key))
            }
            (provider, Secret::ApiKey(_)) => return Err(ApiError::fatal(format!("{provider} accounts need /login, not an API key."))),
            (provider, Secret::OAuth(_)) => return Err(ApiError::fatal(format!("{provider} accounts need an API key, not /login."))),
        };
        let hint = headroom.map(|url| format!(" Is `headroom proxy` running on {url}?")).unwrap_or_default();
        let response = call.header("content-type", "application/json").send(body.to_string()).map_err(|e| ApiError::retryable(format!("Request failed: {e}.{hint}")))?;
        let status = response.status().as_u16();
        let mut reader = watched_body(response.into_body().into_reader(), abort, STREAM_IDLE);
        if status != 200 {
            let mut text = String::new();
            let _ = std::io::Read::read_to_string(&mut reader, &mut text);
            let quota = status == 429 || (status == 529 && text.contains("quota"));
            let retryable = status == 408 || status == 429 || status >= 500;
            return Err(ApiError { message: format!("API error {status}: {}", text.trim()), retryable, quota });
        }
        match account.provider.as_str() {
            _ if custom.as_ref().is_some_and(|custom| custom.api == CustomApi::OpenAi) => openai_chat::parse_stream(&mut reader, abort, on_delta),
            copilot::PROVIDER => openai_chat::parse_stream(&mut reader, abort, on_delta),
            "openai-codex" => codex::parse_stream(&mut reader, abort, on_delta),
            _ => parse_stream(&mut reader, abort, on_delta),
        }
    }

    pub fn accounts(&self) -> Vec<AccountView> {
        let state = self.state.lock().unwrap();
        let mut views: Vec<AccountView> = state
            .accounts
            .iter()
            .map(|a| AccountView {
                provider: a.provider.clone(),
                name: a.name.clone(),
                oauth: a.is_oauth(),
                account_id: a.account_id().map(String::from),
                usage: state.usage.get(&key(a)).and_then(|(_, u)| u.clone()),
                current: state.current.get(&a.provider) == Some(&a.name),
                pinned: state.pinned.get(&a.provider) == Some(&a.name),
            })
            .collect();
        let rank = |provider: &str| state.provider_order.iter().position(|listed| *listed == provider).unwrap_or(usize::MAX);
        views.sort_by(|a, b| (!a.oauth, rank(&a.provider), &a.name).cmp(&(!b.oauth, rank(&b.provider), &b.name)));
        views
    }

    /// Fetches every OAuth account's windows that are older than a minute, for /account.
    pub fn refresh_all_usage(&self) {
        let accounts = self.state.lock().unwrap().accounts.clone();
        self.refresh_usage(&accounts);
    }

    /// Saves an account, replacing one with the same provider and name and dropping that one's cached usage.
    pub fn add(&self, account: Account) -> Result<(), String> {
        let usage_key = key(&account);
        let saved = accounts::update(&self.home, |list| {
            list.retain(|a| a.provider != account.provider || a.name != account.name);
            list.push(account);
            Ok(())
        })?;
        self.state.lock().unwrap().usage.remove(&usage_key);
        self.reload(saved);
        Ok(())
    }

    pub fn remove(&self, provider: &str, name: &str) -> Result<(), String> {
        let saved = accounts::update(&self.home, |list| {
            let before = list.len();
            list.retain(|a| a.provider != provider || a.name != name);
            if list.len() == before { Err(format!("{name} is not a saved account.")) } else { Ok(()) }
        })?;
        self.reload(saved);
        Ok(())
    }

    /// Name and provider account id of the account the next request is likely to use: the current pick, the pinned one, or the first.
    pub fn account(&self, provider: &str) -> Option<(String, Option<String>)> {
        let state = self.state.lock().unwrap();
        let name = state.current.get(provider).or_else(|| state.pinned.get(provider));
        let mut candidates = state.accounts.iter().filter(|a| a.provider == provider);
        let account = match name {
            Some(name) => candidates.find(|a| &a.name == name),
            None => candidates.next(),
        }?;
        Some((account.name.clone(), account.account_id().map(String::from)))
    }

    /// Provider to saved account name for each provider account id in `ids`; ids of accounts no longer saved drop out.
    pub fn account_names(&self, ids: &HashMap<String, String>) -> HashMap<String, String> {
        let state = self.state.lock().unwrap();
        let mut names = HashMap::new();
        for account in &state.accounts {
            if account.account_id().is_some_and(|id| ids.get(&account.provider).is_some_and(|wanted| wanted == id)) {
                names.insert(account.provider.clone(), account.name.clone());
            }
        }
        names
    }

    /// `agent doctor`: one line per account, `OK` with the ping time and usage or `FAIL` with the provider's error unchanged.
    pub fn doctor(&self, provider: Option<&str>) -> Vec<Result<String, String>> {
        let accounts: Vec<Account> = self.state.lock().unwrap().accounts.iter().filter(|a| provider.is_none_or(|p| a.provider == p)).cloned().collect();
        accounts.iter().map(|a| self.check(a).map(|line| format!("OK   {} {line}", key(a))).map_err(|error| format!("FAIL {} {error}", key(a)))).collect()
    }

    /// Refreshes an expired token, lists models and sends a one-token ping on the cheapest one.
    fn check(&self, account: &Account) -> Result<String, String> {
        let account = self.fresh(account.clone())?;
        let request = match account.provider.as_str() {
            "openrouter" => {
                // Every key may use the public list's free models, so the ping costs nothing; a rate-limited one moves on to the next.
                let listed = fetch_models(models_agent().get(format!("{}/v1/models", self.openrouter_base)), models::parse_openrouter)?;
                let free: Vec<Model> = listed.into_iter().filter(|m| m.id.ends_with(":free")).take(4).collect();
                let (first, rest) = free.split_first().ok_or("The model list has no free model.")?;
                let mut request = plain_request(&first.id, "Reply with one word.", "ping", 1);
                request["fallbacks"] = rest.iter().map(|m| json!({"model": m.id})).collect();
                request
            }
            _ => {
                let listed = self.list_models(&account)?;
                let newest = listed.first().ok_or("The model list is empty.")?;
                plain_request(&models::small_model(&listed, newest).id, "Reply with one word.", "ping", 1)
            }
        };
        let started = Instant::now();
        self.try_send(&account, &request, &AtomicBool::new(false), &mut |_| {}).map_err(|e| e.message)?;
        let pinged = format!("{} in {:.1}s", request["model"].as_str().unwrap_or_default(), started.elapsed().as_secs_f64());
        if !account.is_oauth() {
            return Ok(pinged);
        }
        let base = if account.provider == "anthropic" { &self.anthropic_base } else { &self.chatgpt_base };
        let usage = match accounts::fetch_usage(&account, base) {
            Ok(usage) => format!("5h {:.0}%, 7d {:.0}%", usage.five_hour, usage.seven_day),
            Err(error) => error,
        };
        Ok(format!("{pinged}, {usage}"))
    }
}

fn moved_to(request: &mut Value, model: &Model) {
    request["model"] = json!(model.id);
    request["max_tokens"] = json!(request["max_tokens"].as_u64().unwrap_or(model.max_tokens).min(model.max_tokens));
}

fn key(account: &Account) -> String {
    format!("{}/{}", account.provider, account.name)
}

pub(crate) fn models_agent() -> ureq::Agent {
    ureq::Agent::config_builder().http_status_as_error(false).timeout_global(Some(Duration::from_secs(10))).build().into()
}

/// The key in `api_key_env`; a provider without one (a local server) gets an empty key and no Authorization header.
fn custom_key(api_key_env: &str) -> Result<String, String> {
    if api_key_env.is_empty() {
        return Ok(String::new());
    }
    std::env::var(api_key_env).ok().filter(|key| !key.is_empty()).ok_or(format!("Set {api_key_env} to use this provider."))
}

/// The models `custom_providers` lists, or the server's `GET /models` when it lists none.
fn custom_models(provider: &CustomProvider) -> Result<Vec<Model>, String> {
    if provider.models.is_empty() {
        let key = custom_key(&provider.api_key_env)?;
        let request = match provider.api {
            CustomApi::OpenAi if key.is_empty() => models_agent().get(format!("{}/models", provider.base_url)),
            CustomApi::OpenAi => models_agent().get(format!("{}/models", provider.base_url)).header("authorization", format!("Bearer {key}")),
            CustomApi::Anthropic => models_agent().get(format!("{}/v1/models", provider.base_url)).header("anthropic-version", "2023-06-01").header("x-api-key", &key),
        };
        return fetch_models(request, |body| models::parse_openai(&provider.name, body));
    }
    let listed = provider.models.iter().map(|m| Model {
        provider: provider.name.clone(),
        id: format!("{}/{}", provider.name, m.id),
        name: m.id.clone(),
        context_window: m.context_window,
        max_tokens: m.max_tokens,
        accounts: Vec::new(),
        price: None,
    });
    Ok(listed.collect())
}

/// The token headroom asks of callers off loopback; only requests routed through headroom carry it.
fn with_proxy_token(call: ureq::RequestBuilder<ureq::typestate::WithBody>, token: Option<&str>) -> ureq::RequestBuilder<ureq::typestate::WithBody> {
    match token {
        Some(token) => call.header("x-headroom-proxy-token", token),
        None => call,
    }
}

fn fetch_models(request: ureq::RequestBuilder<ureq::typestate::WithoutBody>, parse: impl Fn(&Value) -> Vec<Model>) -> Result<Vec<Model>, String> {
    let mut response = request.call().map_err(|e| format!("Model list request failed: {e}."))?;
    let status = response.status().as_u16();
    if status != 200 {
        return Err(format!("Model list request failed ({status})."));
    }
    let text = response.body_mut().read_to_string().map_err(|e| format!("Model list request failed: {e}."))?;
    let body: Value = serde_json::from_str(&text).map_err(|e| format!("Model list is not JSON: {e}."))?;
    Ok(parse(&body))
}

pub struct ApiError {
    pub message: String,
    pub retryable: bool,
    /// 429, or 529 that names a quota: the account is out, another one may not be.
    pub quota: bool,
}

impl ApiError {
    pub fn retryable(message: String) -> ApiError {
        ApiError { message, retryable: true, quota: false }
    }

    pub fn fatal(message: String) -> ApiError {
        ApiError { message, retryable: false, quota: false }
    }
}

/// Builds a Messages request with cache breakpoints on the system prompt, the last tool, and the last message.
/// `cache_ttl` is "5m" or "1h"; all three breakpoints share it, since a 1h one after a 5m one is a 400.
/// `per_session_system` follows the cached system block, so a value that differs per session does not rewrite it.
/// `thinking` is pi's level; "off" sends no thinking config and "minimal" maps to the lowest effort.
#[allow(clippy::too_many_arguments)]
pub fn request(model: &str, max_tokens: u64, thinking: &str, cache_ttl: &str, system: &str, per_session_system: &str, tools: &Value, messages: &[Value]) -> Value {
    // Without ttl the API caches for 5 minutes.
    let cache_control = if cache_ttl == "1h" { json!({"type": "ephemeral", "ttl": "1h"}) } else { json!({"type": "ephemeral"}) };
    let mut tools = tools.clone();
    if let Some(last) = tools.as_array_mut().and_then(|t| t.last_mut()) {
        last["cache_control"] = cache_control.clone();
    }
    let mut messages = messages.to_vec();
    if let Some(block) = messages.last_mut().and_then(|m| m["content"].as_array_mut()).and_then(|c| c.last_mut()) {
        block["cache_control"] = cache_control.clone();
    }
    let mut request = json!({
        "model": model,
        "max_tokens": max_tokens,
        "stream": true,
        "system": [{"type": "text", "text": system, "cache_control": cache_control}],
        "tools": tools,
        "messages": messages,
    });
    if !per_session_system.is_empty() {
        request["system"].as_array_mut().unwrap().push(json!({"type": "text", "text": per_session_system}));
    }
    if thinking != "off" {
        let effort = if thinking == "minimal" { "low" } else { thinking };
        // Same as pi: thinking that no longer matches the history after compaction is dropped instead of failing with 400.
        request["thinking"] = json!({"type": "adaptive", "display": "summarized", "block_binding": {"prefix_mismatch_behavior": "drop_block"}});
        request["output_config"] = json!({"effort": effort});
    }
    request
}

/// A one-shot request without tools or thinking, used for compaction summaries.
pub fn plain_request(model: &str, system: &str, prompt: &str, max_tokens: u64) -> Value {
    json!({
        "model": model,
        "max_tokens": max_tokens,
        "stream": true,
        "system": [{"type": "text", "text": system}],
        "messages": [{"role": "user", "content": [{"type": "text", "text": prompt}]}],
    })
}

/// No byte for this long means the stream died without a TCP reset; ureq's `timeout_recv_body` caps the whole body instead.
const STREAM_IDLE: Duration = Duration::from_secs(120);

/// `body` read on its own thread, so a stalled stream fails after `idle` and Esc ends it at once.
/// A read on a stalled body that a caller gives up on stays blocked on that thread until the socket closes.
fn watched_body(mut body: impl std::io::Read + Send + 'static, abort: &AtomicBool, idle: Duration) -> BufReader<WatchedBody<'_>> {
    let (send, chunks) = std::sync::mpsc::sync_channel(4);
    std::thread::spawn(move || {
        let mut buffer = [0u8; 16 * 1024];
        loop {
            let chunk = body.read(&mut buffer).map(|read| buffer[..read].to_vec());
            let done = !matches!(&chunk, Ok(bytes) if !bytes.is_empty());
            if send.send(chunk).is_err() || done {
                return;
            }
        }
    });
    BufReader::new(WatchedBody { chunks, chunk: Vec::new(), read: 0, abort, idle })
}

struct WatchedBody<'a> {
    chunks: std::sync::mpsc::Receiver<std::io::Result<Vec<u8>>>,
    chunk: Vec<u8>,
    read: usize,
    abort: &'a AtomicBool,
    idle: Duration,
}

impl std::io::Read for WatchedBody<'_> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        use std::sync::mpsc::RecvTimeoutError;
        let deadline = Instant::now() + self.idle;
        while self.read == self.chunk.len() {
            if self.abort.load(Ordering::Relaxed) {
                return Err(std::io::Error::other("aborted"));
            }
            match self.chunks.recv_timeout(Duration::from_millis(100)) {
                Ok(chunk) => {
                    let chunk = chunk?;
                    if chunk.is_empty() {
                        return Ok(0);
                    }
                    (self.chunk, self.read) = (chunk, 0);
                }
                Err(RecvTimeoutError::Timeout) if Instant::now() < deadline => {}
                Err(RecvTimeoutError::Timeout) => return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, format!("no data for {}s", self.idle.as_secs_f32()))),
                Err(RecvTimeoutError::Disconnected) => return Ok(0),
            }
        }
        let count = out.len().min(self.chunk.len() - self.read);
        out[..count].copy_from_slice(&self.chunk[self.read..self.read + count]);
        self.read += count;
        Ok(count)
    }
}

fn parse_stream(reader: &mut dyn BufRead, abort: &AtomicBool, on_delta: &mut dyn FnMut(Delta)) -> Result<Reply, ApiError> {
    let mut blocks: Vec<Value> = Vec::new();
    let mut tool_json: Vec<String> = Vec::new();
    let mut bad_tool_inputs = Vec::new();
    let mut usage = Usage::default();
    let mut cache_write_1h = None;
    let mut stop_reason = String::new();
    let mut id = String::new();
    // message_delta sums the input of every server tool iteration (tool_search); message_start holds the first request's.
    let mut prompt_tokens = 0;
    let mut line = String::new();
    let cut_off = |what: &str| ApiError::retryable(format!("Stream ended {what}."));
    loop {
        if abort.load(Ordering::Relaxed) {
            stop_reason = "aborted".into();
            break;
        }
        line.clear();
        let read = match reader.read_line(&mut line) {
            Err(_) if abort.load(Ordering::Relaxed) => {
                stop_reason = "aborted".into();
                break;
            }
            read => read.map_err(|e| ApiError::retryable(format!("Stream read failed: {e}.")))?,
        };
        if read == 0 {
            return Err(cut_off("before message_stop"));
        }
        let Some(data) = line.strip_prefix("data:") else { continue };
        let event: Value = serde_json::from_str(data.trim()).map_err(|e| ApiError::fatal(format!("Bad stream event: {e}.")))?;
        match event["type"].as_str().unwrap_or_default() {
            "message_start" => {
                id = event["message"]["id"].as_str().unwrap_or_default().to_string();
                read_usage(&event["message"]["usage"], &mut usage, &mut cache_write_1h);
                prompt_tokens = usage.input + usage.cache_read + usage.cache_write;
            }
            "content_block_start" => {
                blocks.push(event["content_block"].clone());
                tool_json.push(String::new());
            }
            "content_block_delta" => {
                let index = event["index"].as_u64().unwrap_or(0) as usize;
                let (Some(block), Some(partial)) = (blocks.get_mut(index), tool_json.get_mut(index)) else {
                    return Err(cut_off("with a delta for an unknown block"));
                };
                let delta = &event["delta"];
                match delta["type"].as_str().unwrap_or_default() {
                    "text_delta" => {
                        let text = delta["text"].as_str().unwrap_or_default();
                        on_delta(Delta::Text(text));
                        append(&mut block["text"], text);
                    }
                    "thinking_delta" => {
                        let text = delta["thinking"].as_str().unwrap_or_default();
                        on_delta(Delta::Thinking(text));
                        append(&mut block["thinking"], text);
                    }
                    "signature_delta" => append(&mut block["signature"], delta["signature"].as_str().unwrap_or_default()),
                    "input_json_delta" => partial.push_str(delta["partial_json"].as_str().unwrap_or_default()),
                    _ => {}
                }
            }
            "content_block_stop" => {
                let index = event["index"].as_u64().unwrap_or(0) as usize;
                if let (Some(block), Some(partial)) = (blocks.get_mut(index), tool_json.get(index))
                    && block["type"] == "tool_use"
                    && !partial.is_empty()
                {
                    block["input"] = parse_tool_input(block["id"].as_str().unwrap_or_default(), partial, &mut bad_tool_inputs);
                }
            }
            "message_delta" => {
                stop_reason = event["delta"]["stop_reason"].as_str().unwrap_or_default().to_string();
                read_usage(&event["usage"], &mut usage, &mut cache_write_1h);
            }
            "message_stop" => break,
            "error" => {
                let kind = event["error"]["type"].as_str().unwrap_or_default();
                let retryable = matches!(kind, "overloaded_error" | "api_error" | "rate_limit_error");
                let quota = kind == "rate_limit_error";
                return Err(ApiError { message: format!("API stream error: {}", event["error"]), retryable, quota });
            }
            _ => {}
        }
    }
    Ok(Reply { message: json!({"role": "assistant", "content": blocks}), stop_reason, context_tokens: prompt_tokens + usage.output, usage, cache_write_1h, id, bad_tool_inputs, ..Reply::default() })
}

fn append(field: &mut Value, text: &str) {
    let mut joined = field.as_str().unwrap_or_default().to_string();
    joined.push_str(text);
    *field = Value::String(joined);
}

/// Usage fields arrive in message_start and again (cumulative) in message_delta; later values win.
fn read_usage(raw: &Value, usage: &mut Usage, cache_write_1h: &mut Option<u64>) {
    let fields =
        [("input_tokens", &mut usage.input), ("output_tokens", &mut usage.output), ("cache_read_input_tokens", &mut usage.cache_read), ("cache_creation_input_tokens", &mut usage.cache_write)];
    for (name, slot) in fields {
        if let Some(value) = raw[name].as_u64() {
            *slot = value;
        }
    }
    if let Some(value) = raw["cache_creation"]["ephemeral_1h_input_tokens"].as_u64() {
        *cache_write_1h = Some(value);
    }
}

/// Models sometimes write a raw tab or newline inside a string (tab-indented code); strict JSON rejects it, so escape it first.
/// Invalid JSON becomes `{}` plus an entry in `bad`, so the agent answers that call with an error instead of losing the turn.
fn parse_tool_input(id: &str, raw: &str, bad: &mut Vec<BadToolInput>) -> Value {
    let mut escaped = String::with_capacity(raw.len());
    let (mut in_string, mut after_backslash) = (false, false);
    for c in raw.chars() {
        if in_string && c < ' ' {
            escaped.push_str(&format!("\\u{:04x}", c as u32));
        } else {
            escaped.push(c);
        }
        if c == '"' && !after_backslash {
            in_string = !in_string;
        }
        after_backslash = in_string && c == '\\' && !after_backslash;
    }
    serde_json::from_str(&escaped).unwrap_or_else(|e| {
        bad.push(BadToolInput { id: id.to_string(), error: format!("Bad tool input JSON: {e}."), raw: raw.to_string() });
        json!({})
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picking_an_account_reports_it_before_the_next_request() {
        let home = std::env::temp_dir().join(format!("agent-pick-{}", std::process::id()));
        let client = Client::new(&home).unwrap();
        let account = |name: &str| Account { provider: "anthropic".into(), name: name.into(), secret: Secret::ApiKey("sk".into()) };
        client.reload(vec![account("home"), account("work")]);
        client.state.lock().unwrap().current.insert("anthropic".into(), "home".into());

        let mut settings = crate::settings::load(&home).unwrap();
        settings.default_accounts.insert("anthropic".into(), "work".into());
        client.configure(&settings);

        assert_eq!(client.account("anthropic"), Some(("work".into(), None)));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn a_remote_headroom_gets_the_requests_with_the_proxy_token() {
        let home = std::env::temp_dir().join(format!("agent-remote-headroom-{}", std::process::id()));
        let mut client = Client::new(&home).unwrap();
        client.anthropic_base = "http://127.0.0.1:9".into();
        let (headroom, requests) = mock_provider(200, PONG);
        client.reload(vec![Account { provider: "anthropic".into(), name: "work".into(), secret: Secret::ApiKey("sk".into()) }]);
        *client.catalog.write().unwrap() = vec![Model { provider: "anthropic".into(), id: "claude".into(), ..Model::default() }].into();
        let mut settings = crate::settings::load(&home).unwrap();
        (settings.headroom, settings.headroom_url, settings.headroom_token) = (true, headroom, "t0ken".into());
        client.configure(&settings);

        let sent = client.send(&json!({"model": "claude", "max_tokens": 10}), &AtomicBool::new(false), &mut |_| {});
        std::fs::remove_dir_all(&home).ok();

        assert!(sent.is_ok(), "{:?}", sent.err());
        assert_eq!(requests.recv().unwrap()["x-headroom-proxy-token"], "t0ken");
    }

    #[test]
    fn a_failed_token_refresh_moves_the_request_to_the_next_account() {
        let home = std::env::temp_dir().join(format!("agent-refresh-{}", std::process::id()));
        let client = Client::new(&home).unwrap();
        let expired = oauth::Tokens { access: "a".into(), refresh: "r".into(), expires: 0, account_id: None };
        let account = |name: &str| Account { provider: "anthropic".into(), name: name.into(), secret: Secret::OAuth(expired.clone()) };
        client.reload(vec![account("home"), account("work")]);
        *client.catalog.write().unwrap() = vec![Model { provider: "anthropic".into(), id: "claude".into(), ..Model::default() }].into();

        let mut notices = Vec::new();
        let error = client.send(&json!({"model": "claude"}), &AtomicBool::new(false), &mut |delta| {
            if let Delta::Notice(text) = delta {
                notices.push(text.to_string());
            }
        });
        std::fs::remove_dir_all(&home).ok();

        // Neither account is in the accounts file, so each refresh fails without a network call.
        assert!(error.is_err());
        assert!(notices.iter().any(|n| n.starts_with("home ")) && notices.iter().any(|n| n.starts_with("work ")), "{notices:?}");
    }

    #[test]
    fn account_names_maps_saved_account_ids_to_names() {
        let home = std::env::temp_dir().join(format!("agent-names-{}", std::process::id()));
        let client = Client::new(&home).unwrap();
        let saved = |name: &str, id: &str| Account {
            provider: "anthropic".into(),
            name: name.into(),
            secret: Secret::OAuth(oauth::Tokens { access: "a".into(), refresh: "r".into(), expires: 5, account_id: Some(id.into()) }),
        };
        client.reload(vec![saved("home", "uuid-home"), saved("work", "uuid-work")]);

        let ids = |id: &str| HashMap::from([("anthropic".to_string(), id.to_string())]);
        assert_eq!(client.account_names(&ids("uuid-work")), HashMap::from([("anthropic".to_string(), "work".to_string())]));
        assert_eq!(client.account_names(&ids("uuid-gone")), HashMap::new());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn stream_builds_text_thinking_and_tool_blocks() {
        let events = [
            r#"{"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":10,"cache_read_input_tokens":5,"output_tokens":1}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hm"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig"}}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Hi"}}"#,
            r#"{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"t1","name":"bash","input":{}}}"#,
            r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"command\":"}}"#,
            r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"\"ls\"}"}}"#,
            r#"{"type":"content_block_stop","index":2}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":7}}"#,
            r#"{"type":"message_stop"}"#,
        ];
        let body: String = events.iter().map(|e| format!("event: x\ndata: {e}\n\n")).collect();
        let mut streamed = String::new();
        let on_delta = &mut |delta: Delta| match delta {
            Delta::Text(t) => streamed.push_str(t),
            Delta::Thinking(t) => streamed.push_str(&format!("[{t}]")),
            Delta::Notice(_) => {}
        };
        let reply = parse_stream(&mut body.as_bytes(), &AtomicBool::new(false), on_delta).ok().unwrap();
        assert_eq!(streamed, "[hm]Hi");
        assert_eq!(reply.stop_reason, "tool_use");
        assert_eq!((reply.usage.input, reply.usage.output, reply.usage.cache_read), (10, 7, 5));
        assert_eq!(reply.id, "msg_1");
        let content = &reply.message["content"];
        assert_eq!(content[0], json!({"type": "thinking", "thinking": "hm", "signature": "sig"}));
        assert_eq!(content[2]["input"], json!({"command": "ls"}));
    }

    #[test]
    fn server_tool_iterations_bill_their_sum_but_the_context_is_one_request() {
        let events = [
            r#"{"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":4,"cache_creation_input_tokens":2769,"cache_read_input_tokens":0,"output_tokens":2}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"srvtoolu_1","name":"tool_search_tool_regex","input":{}}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"input_tokens":641,"cache_creation_input_tokens":2769,"cache_read_input_tokens":2769,"output_tokens":211}}"#,
            r#"{"type":"message_stop"}"#,
        ];
        let body: String = events.iter().map(|e| format!("event: x\ndata: {e}\n\n")).collect();
        let reply = parse_stream(&mut body.as_bytes(), &AtomicBool::new(false), &mut |_| {}).ok().unwrap();
        assert_eq!((reply.usage.input, reply.usage.cache_read, reply.usage.cache_write), (641, 2769, 2769));
        assert_eq!(reply.context_tokens, 4 + 2769 + 211);
    }

    #[test]
    fn the_1h_cache_write_split_survives_to_the_reply() {
        let start = |usage: &str| format!(r#"{{"type":"message_start","message":{{"id":"msg_1","usage":{usage}}}}}"#);
        let parse = |usage: &str| {
            let body = format!("data: {}\n\ndata: {{\"type\":\"message_stop\"}}\n\n", start(usage));
            parse_stream(&mut body.as_bytes(), &AtomicBool::new(false), &mut |_| {}).ok().unwrap().cache_write_1h
        };
        let split = r#"{"input_tokens":4,"cache_creation_input_tokens":300,"cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":300}}"#;
        assert_eq!(parse(split), Some(300));
        assert_eq!(parse(r#"{"input_tokens":4,"cache_creation_input_tokens":300}"#), None);
    }

    #[test]
    fn tool_input_keeps_a_raw_tab_inside_a_string() {
        let events = [
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t1","name":"write","input":{}}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"text\": \"\\\"\tx\\n\"}"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"message_stop"}"#,
        ];
        let body: String = events.iter().map(|e| format!("event: x\ndata: {e}\n\n")).collect();
        let reply = parse_stream(&mut body.as_bytes(), &AtomicBool::new(false), &mut |_| {}).ok().unwrap();
        assert_eq!(reply.message["content"][0]["input"], json!({"text": "\"\tx\n"}));
    }

    #[test]
    fn invalid_tool_input_is_reported_not_fatal() {
        let events = [
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t1","name":"write","input":{}}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"text\": \"a\"b\"}"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"message_stop"}"#,
        ];
        let body: String = events.iter().map(|e| format!("event: x\ndata: {e}\n\n")).collect();
        let reply = parse_stream(&mut body.as_bytes(), &AtomicBool::new(false), &mut |_| {}).ok().unwrap();
        assert_eq!(reply.message["content"][0]["input"], json!({}));
        let [bad] = reply.bad_tool_inputs.as_slice() else { panic!("expected one bad input") };
        assert_eq!((bad.id.as_str(), bad.raw.as_str()), ("t1", r#"{"text": "a"b"}"#));
        assert!(bad.error.starts_with("Bad tool input JSON: expected `,` or `}`"), "{}", bad.error);
    }

    #[test]
    fn request_marks_cache_breakpoints() {
        let tools = json!([{"name": "a"}, {"name": "b"}]);
        let messages = [json!({"role": "user", "content": [{"type": "text", "text": "1"}, {"type": "text", "text": "2"}]})];
        let body = request("m", 1000, "high", "5m", "sys", "<cwd>/a</cwd>", &tools, &messages);
        assert!(body["tools"][0].get("cache_control").is_none());
        assert_eq!(body["tools"][1]["cache_control"]["type"], "ephemeral");
        assert!(body["messages"][0]["content"][0].get("cache_control").is_none());
        assert_eq!(body["messages"][0]["content"][1]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["system"][1], json!({"type": "text", "text": "<cwd>/a</cwd>"}));
        assert_eq!(request("m", 1000, "high", "5m", "sys", "", &tools, &messages)["system"].as_array().unwrap().len(), 1);
        let hour = request("m", 1000, "high", "1h", "sys", "", &tools, &messages);
        for breakpoint in [&hour["tools"][1], &hour["messages"][0]["content"][1], &hour["system"][0]] {
            assert_eq!(breakpoint["cache_control"], json!({"type": "ephemeral", "ttl": "1h"}));
        }
    }

    #[test]
    fn a_stalled_stream_times_out_and_esc_ends_it_as_aborted() {
        use std::io::Write;
        use std::os::unix::net::UnixStream;
        let (mut server, client) = UnixStream::pair().unwrap();
        server.write_all(b"data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{}}}\n").unwrap();
        let not_aborted = AtomicBool::new(false);
        let error = parse_stream(&mut watched_body(client.try_clone().unwrap(), &not_aborted, Duration::from_millis(200)), &not_aborted, &mut |_| {}).err().unwrap();
        assert!(error.retryable && error.message.contains("no data"), "{}", error.message);

        let abort = Arc::new(AtomicBool::new(false));
        let setter = Arc::clone(&abort);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            setter.store(true, Ordering::Relaxed);
        });
        let started = Instant::now();
        let reply = parse_stream(&mut watched_body(client, &abort, Duration::from_secs(60)), &abort, &mut |_| {}).ok().unwrap();
        assert_eq!(reply.stop_reason, "aborted");
        assert!(started.elapsed() < Duration::from_secs(5));
        drop(server);
    }

    /// Serves a models list that both Anthropic and OpenRouter parse; messages get `messages_status` and `messages_body`
    /// and their request bodies go to the returned receiver.
    fn mock_provider(messages_status: u16, messages_body: &'static str) -> (String, std::sync::mpsc::Receiver<Value>) {
        use std::io::{BufRead, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (sent, received) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
                reader.read_line(&mut request_line).unwrap();
                // Read the whole request, so closing the socket does not reset it under the client.
                let mut length = 0;
                let mut proxy_token = None;
                let mut header = String::new();
                while reader.read_line(&mut header).unwrap() > 2 {
                    if let Some(value) = header.to_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap();
                    }
                    if let Some(value) = header.to_lowercase().strip_prefix("x-headroom-proxy-token:") {
                        proxy_token = Some(value.trim().to_string());
                    }
                    header.clear();
                }
                let mut request = vec![0; length];
                std::io::Read::read_exact(&mut reader, &mut request).unwrap();
                let (status, body) = if request_line.contains("/v1/models") {
                    (
                        200,
                        r#"{"data":[{"id":"claude-haiku-9","max_input_tokens":200000,"max_tokens":8192},{"id":"a/one:free","context_length":1000,"supported_parameters":["tools"]},{"id":"b/two:free","context_length":1000,"supported_parameters":["tools"]}]}"#,
                    )
                } else if request_line.starts_with("GET") {
                    // Codex and Claude usage windows; unknown usage does not block a request.
                    (404, "{}")
                } else {
                    let mut request: Value = serde_json::from_slice(&request).unwrap();
                    if let Some(token) = proxy_token {
                        request["x-headroom-proxy-token"] = json!(token);
                    }
                    // A test that only reads the doctor lines has dropped the receiver.
                    let _ = sent.send(request);
                    (messages_status, messages_body)
                };
                write!(stream, "HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        });
        (base, received)
    }

    const PONG: &str = concat!("data: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"usage\":{}}}\n\n", "data: {\"type\":\"message_stop\"}\n\n");

    #[test]
    fn doctor_reports_ok_for_a_working_account_and_the_provider_error_for_a_rejected_one() {
        let home = std::env::temp_dir().join(format!("agent-doctor-{}", std::process::id()));
        let check = |messages_status, messages_body| {
            let mut client = Client::new(&home).unwrap();
            client.anthropic_base = mock_provider(messages_status, messages_body).0;
            client.reload(vec![Account { provider: "anthropic".into(), name: "work".into(), secret: Secret::ApiKey("sk".into()) }]);
            client.doctor(Some("anthropic"))
        };

        let ok = check(200, PONG);
        let rejected = check(401, r#"{"error":{"message":"invalid x-api-key"}}"#);
        std::fs::remove_dir_all(&home).ok();

        assert!(matches!(&ok[..], [Ok(line)] if line.contains("anthropic/work")), "{ok:?}");
        assert!(matches!(&rejected[..], [Err(line)] if line.contains("anthropic/work") && line.contains("401") && line.contains("invalid x-api-key")), "{rejected:?}");
    }

    #[test]
    fn doctor_pings_openrouter_on_free_models_without_a_thinking_field() {
        let home = std::env::temp_dir().join(format!("agent-doctor-or-{}", std::process::id()));
        let mut client = Client::new(&home).unwrap();
        let (base, requests) = mock_provider(200, PONG);
        client.openrouter_base = base;
        client.reload(vec![Account { provider: "openrouter".into(), name: "key".into(), secret: Secret::ApiKey("sk".into()) }]);

        let lines = client.doctor(Some("openrouter"));
        std::fs::remove_dir_all(&home).ok();

        assert!(matches!(&lines[..], [Ok(_)]), "{lines:?}");
        let ping = requests.recv().unwrap();
        assert!(ping.get("thinking").is_none(), "{ping}");
        assert_eq!(ping["model"], "a/one:free");
        // A free model hits its rate limit often, so OpenRouter moves the ping to the next one.
        assert_eq!(ping["fallbacks"], json!([{"model": "b/two:free"}]));
    }

    #[test]
    fn a_custom_provider_model_replaces_the_openrouter_model_with_the_same_id() {
        let home = std::env::temp_dir().join(format!("agent-custom-clash-{}", std::process::id()));
        let mut client = Client::new(&home).unwrap();
        let (base, _) = mock_provider(200, PONG);
        client.openrouter_base = base;
        let model = crate::settings::CustomModel { id: "one:free".into(), context_window: 1, max_tokens: 1 };
        client.state.lock().unwrap().custom_providers = vec![CustomProvider { name: "a".into(), base_url: "http://x".into(), api_key_env: String::new(), api: CustomApi::OpenAi, models: vec![model] }];

        let errors = client.load_catalog();
        std::fs::remove_dir_all(&home).ok();

        assert!(errors.is_empty(), "{errors:?}");
        let catalog = client.catalog();
        let same_id: Vec<&str> = catalog.iter().filter(|m| m.id == "a/one:free").map(|m| m.provider.as_str()).collect();
        assert_eq!(same_id, ["a"]);
    }

    #[test]
    fn loading_the_catalog_again_lists_the_models_of_an_account_added_since() {
        let home = std::env::temp_dir().join(format!("agent-catalog-reload-{}", std::process::id()));
        let mut client = Client::new(&home).unwrap();
        let (base, _) = mock_provider(200, PONG);
        (client.anthropic_base, client.openrouter_base) = (base.clone(), base);
        client.reload(Vec::new());
        client.load_catalog();
        let has_claude = |client: &Client| client.catalog().iter().any(|m| m.provider == "anthropic" && m.id == "claude-haiku-9");
        assert!(!has_claude(&client));

        client.reload(vec![Account { provider: "anthropic".into(), name: "work".into(), secret: Secret::ApiKey("sk".into()) }]);
        let errors = client.load_catalog();
        std::fs::remove_dir_all(&home).ok();

        assert!(errors.is_empty(), "{errors:?}");
        assert!(has_claude(&client));
    }

    /// An Anthropic account answering `status` on every attempt, with an OpenRouter account and `a/one:free` as `fallback_model`.
    fn send_with_failover(status: u16, body: &'static str) -> (Result<Reply, String>, Vec<String>, std::sync::mpsc::Receiver<Value>) {
        let home = std::env::temp_dir().join(format!("agent-failover-{status}-{}", std::process::id()));
        let mut client = Client::new(&home).unwrap();
        client.anthropic_base = mock_provider(status, body).0;
        let (openrouter_base, fallback_requests) = mock_provider(200, PONG);
        client.openrouter_base = openrouter_base;
        client.reload(vec![
            Account { provider: "anthropic".into(), name: "work".into(), secret: Secret::ApiKey("sk".into()) },
            Account { provider: "openrouter".into(), name: "key".into(), secret: Secret::ApiKey("sk".into()) },
        ]);
        let models = vec![Model { provider: "anthropic".into(), id: "claude".into(), ..Model::default() }, Model { provider: "openrouter".into(), id: "a/one:free".into(), ..Model::default() }];
        *client.catalog.write().unwrap() = models.into();
        client.state.lock().unwrap().fallback_model = "a/one:free".into();

        let mut notices = Vec::new();
        let sent = client.send(&json!({"model": "claude", "max_tokens": 10}), &AtomicBool::new(false), &mut |delta| {
            if let Delta::Notice(text) = delta {
                notices.push(text.to_string());
            }
        });
        std::fs::remove_dir_all(&home).ok();
        (sent, notices, fallback_requests)
    }

    #[test]
    fn an_overloaded_provider_fails_over_to_the_fallback_model_after_its_retries() {
        let (sent, notices, fallback_requests) = send_with_failover(529, r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#);

        assert_eq!(sent.unwrap().model, "a/one:free");
        assert_eq!(fallback_requests.recv().unwrap()["model"], "a/one:free");
        assert!(notices.last().is_some_and(|n| n.contains("Switched to a/one:free for this request")), "{notices:?}");
    }

    #[test]
    fn a_rejected_request_does_not_fail_over() {
        let (sent, notices, fallback_requests) = send_with_failover(401, r#"{"error":{"message":"invalid x-api-key"}}"#);

        assert!(sent.is_err_and(|error| error.contains("401")));
        assert!(fallback_requests.try_recv().is_err());
        assert!(notices.iter().all(|n| !n.contains("Switched")), "{notices:?}");
    }

    #[test]
    fn a_custom_provider_model_goes_to_its_chat_completions_endpoint_without_the_name_prefix() {
        let home = std::env::temp_dir().join(format!("agent-custom-{}", std::process::id()));
        let client = Client::new(&home).unwrap();
        let (base, requests) = mock_provider(200, "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"pong\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n");
        let local = CustomProvider { name: "ollama".into(), base_url: format!("{base}/v1"), api_key_env: String::new(), api: CustomApi::OpenAi, models: Vec::new() };
        *client.catalog.write().unwrap() = vec![Model { provider: "ollama".into(), id: "ollama/qwen3:8b".into(), ..Model::default() }].into();
        client.state.lock().unwrap().custom_providers = vec![local];

        let reply = client.send(&plain_request("ollama/qwen3:8b", "Be brief.", "ping", 10), &AtomicBool::new(false), &mut |_| {});
        std::fs::remove_dir_all(&home).ok();

        let reply = reply.unwrap();
        assert_eq!(reply.message["content"], json!([{"type": "text", "text": "pong"}]));
        assert_eq!((reply.provider.as_str(), reply.model.as_str()), ("ollama", "ollama/qwen3:8b"));
        assert_eq!(requests.recv().unwrap()["model"], "qwen3:8b");
    }

    #[test]
    fn an_anthropic_compatible_provider_gets_a_messages_request_without_anthropic_only_fields() {
        let home = std::env::temp_dir().join(format!("agent-custom-anthropic-{}", std::process::id()));
        let client = Client::new(&home).unwrap();
        let (base, requests) = mock_provider(200, PONG);
        let minimax = CustomProvider { name: "minimax".into(), base_url: base, api_key_env: String::new(), api: CustomApi::Anthropic, models: Vec::new() };
        *client.catalog.write().unwrap() = vec![Model { provider: "minimax".into(), id: "minimax/MiniMax-M2".into(), ..Model::default() }].into();
        client.state.lock().unwrap().custom_providers = vec![minimax];
        let mut request = plain_request("minimax/MiniMax-M2", "Be brief.", "ping", 10);
        request["thinking"] = json!({"type": "adaptive"});
        request["speed"] = json!("fast");

        let reply = client.send(&request, &AtomicBool::new(false), &mut |_| {});
        std::fs::remove_dir_all(&home).ok();

        assert_eq!(reply.unwrap().provider, "minimax");
        let sent = requests.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(sent["model"], "MiniMax-M2");
        assert!(sent.get("thinking").is_none() && sent.get("speed").is_none(), "{sent}");
    }

    #[test]
    fn a_rejected_codex_tier_retries_once_at_standard_speed() {
        let home = std::env::temp_dir().join(format!("agent-tier-{}", std::process::id()));
        let mut client = Client::new(&home).unwrap();
        let (base, requests) = mock_provider(400, r#"{"error":{"message":"Unsupported service_tier: ultrafast"}}"#);
        client.chatgpt_base = base;
        let tokens = oauth::Tokens { access: "a".into(), refresh: "r".into(), expires: u64::MAX, account_id: None };
        client.reload(vec![Account { provider: "openai-codex".into(), name: "pro".into(), secret: Secret::OAuth(tokens) }]);
        *client.catalog.write().unwrap() = vec![Model { provider: "openai-codex".into(), id: "gpt".into(), ..Model::default() }].into();
        let mut request = plain_request("gpt", "Be brief.", "ping", 10);
        request["speed"] = json!("ultrafast");

        let mut notices = Vec::new();
        let sent = client.send(&request, &AtomicBool::new(false), &mut |delta| {
            if let Delta::Notice(text) = delta {
                notices.push(text.to_string());
            }
        });
        std::fs::remove_dir_all(&home).ok();

        assert!(sent.is_err_and(|error| error.contains("service_tier")));
        assert_eq!(requests.recv().unwrap()["service_tier"], "ultrafast");
        assert_eq!(requests.recv_timeout(std::time::Duration::from_secs(5)).unwrap().get("service_tier"), None);
        assert!(requests.try_recv().is_err());
        assert!(notices.iter().any(|n| n.contains("Retrying at standard speed")), "{notices:?}");
    }
}
