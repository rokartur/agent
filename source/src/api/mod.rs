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
pub mod google;
pub mod images;
pub mod models;
pub mod oauth;
pub mod openai_chat;

mod catalog;
mod request;
mod stream;
mod thread;

pub use request::{plain_request, request};
pub use thread::MessageThread;

use catalog::fetch_models;
use request::defer_rare_tools;
use stream::{STREAM_IDLE, append, parse_stream, parse_tool_input, watched_body};
use thread::THREAD_LOST;

use crate::settings::{CustomApi, CustomProvider, Settings};
use accounts::{Account, Candidate, Pick, Policy, Secret, Usage as Windows};
use models::Model;

#[cfg(not(test))]
const RETRY_DELAYS_SECS: [u64; 4] = [2, 5, 15, 30];
#[cfg(test)]
const RETRY_DELAYS_SECS: [u64; 4] = [0; 4];
const THINKING_BETA: &str = "thinking-binding-controls-2026-08-01";
const MESSAGE_THREADS_BETA: &str = "message-threads-2026-08-12";
const ANTHROPIC_API: &str = "https://api.anthropic.com";
/// pi-ai sends OAuth requests as Claude Code: this user agent, its beta flags and its identity as the first system block.
pub const CLAUDE_CODE_USER_AGENT: &str = "claude-cli/2.1.321";
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
    /// Why the last usage fetch failed, until one succeeds.
    usage_errors: HashMap<String, String>,
    /// The outcome of the last /account check, kept until the next one.
    checks: HashMap<String, AccountStatus>,
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
    /// The setting is on and Anthropic requests go straight to its API, which a thread needs.
    message_threads: bool,
}

#[derive(Default)]
pub struct Reply {
    pub message: Value,
    pub stop_reason: String,
    pub usage: Usage,
    /// The part of `usage.cache_write` cached for 1h, which bills 2x input against 1.25x for 5m; None when the API gave no split.
    pub cache_write_1h: Option<u64>,
    /// Dollars the provider says it billed (OpenRouter's `usage.cost`); None when it does not say.
    pub billed: Option<f64>,
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
    pub status: AccountStatus,
}

#[derive(Clone, Debug, PartialEq)]
pub enum AccountStatus {
    Unchecked,
    Checking,
    Working,
    /// A recent 429 or failed token refresh moved requests off it for a while.
    Limited,
    Failed(String),
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
            anthropic_base: base("ANTHROPIC_BASE_URL", ANTHROPIC_API),
            chatgpt_base: base("CHATGPT_BASE_URL", "https://chatgpt.com/backend-api"),
            openrouter_base: base("OPENROUTER_BASE_URL", "https://openrouter.ai/api"),
            catalog: Arc::default(),
            cache_key: format!("agent-{}-{}", std::process::id(), crate::process::now_ms()),
            state: Arc::new(Mutex::new(State { threshold: 90.0, ..State::default() })),
        };
        client.reload(accounts::load(home)?);
        Ok(client)
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
        // Headroom rewrites each request's whole history, and a proxy at ANTHROPIC_BASE_URL may drop the thread field or its beta.
        state.message_threads = settings.message_threads && state.headroom.is_none() && self.anthropic_base == ANTHROPIC_API;
        state.provider_order = settings.sort_providers(&models::PROVIDERS);
        state.custom_providers = settings.custom_providers.clone();
    }

    /// Whether Anthropic turns go in message threads: the setting is on and nothing sits between this client and the API.
    pub fn message_threads(&self) -> bool {
        self.state.lock().unwrap().message_threads
    }

    fn reload(&self, mut saved: Vec<Account>) {
        for (provider, var) in [("anthropic", "ANTHROPIC_API_KEY"), ("openrouter", "OPENROUTER_API_KEY")] {
            // Unit tests share the developer's environment; a key there must not become an account.
            let env_key = std::env::var(var).ok().filter(|k| !k.is_empty() && !cfg!(test));
            if let Some(key) = env_key.filter(|_| !saved.iter().any(|a| a.provider == provider)) {
                saved.push(Account { provider: provider.into(), name: "env".into(), secret: Secret::ApiKey(key) });
            }
        }
        self.state.lock().unwrap().accounts = saved;
    }

    /// Streams one model turn into `on_delta` on a fitting account. Setting `abort` ends the stream with stop reason "aborted".
    pub fn send(&self, request: &Value, abort: &AtomicBool, on_delta: &mut dyn FnMut(Delta)) -> Result<Reply, String> {
        self.send_on_account(request, None, abort, on_delta, true)
    }

    /// Like `send`, inside `thread` when `message_threads` is on and the request goes straight to Anthropic.
    /// `shown` false: streamed text went nowhere it cannot be taken back (stdout), so a reply cut off partway is retried.
    pub fn send_in_thread(&self, request: &Value, thread: &mut MessageThread, abort: &AtomicBool, shown: bool, on_delta: &mut dyn FnMut(Delta)) -> Result<Reply, String> {
        self.send_on_account(request, Some(thread), abort, on_delta, shown)
    }

    /// Like `send` for callers that only read the `Reply`, so a stream that breaks off partway can still be retried.
    pub fn send_quiet(&self, request: &Value, abort: &AtomicBool) -> Result<Reply, String> {
        self.send_on_account(request, None, abort, &mut |_| {}, false)
    }

    fn send_on_account(&self, request: &Value, mut thread: Option<&mut MessageThread>, abort: &AtomicBool, on_delta: &mut dyn FnMut(Delta), shown: bool) -> Result<Reply, String> {
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
            let mut watched = |delta: Delta| {
                streamed |= !matches!(delta, Delta::Notice(_));
                on_delta(delta);
            };
            let threaded = account.provider == "anthropic" && self.state.lock().unwrap().message_threads;
            let sent = match thread.as_deref_mut().filter(|thread| threaded && thread.unsupported_model != request["model"]) {
                Some(thread) => self.try_send_in_thread(&account, &request, thread, abort, &mut watched),
                None => self.try_send(&account, &request, abort, &mut watched),
            };
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
            on_delta(Delta::Notice(&format!("{}. Retrying in {}s.", error.message.trim_end_matches('.'), RETRY_DELAYS_SECS[attempt])));
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
        let candidates: Vec<Candidate> = accounts
            .iter()
            .map(|a| Candidate { usage: state.usage.get(&key(a)).and_then(|(_, u)| u.as_ref()?.for_model(&model.id)), limited: state.limited.get(&key(a)).is_some_and(|until| *until > now) })
            .collect();
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
        // Each stale account is claimed (stamped) under the lock, so concurrent sends do not fetch it too.
        let stale: Vec<&Account> = accounts
            .iter()
            .filter(|account| {
                if !accounts::has_usage(account) {
                    return false;
                }
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
            })
            .collect();
        // In the background: this pick uses the windows already known (unknown does not block), the next one the fresh ones.
        // Waiting cost every run's first request 0.2-0.6 s; an account over its limit still answers 429 and is switched.
        for account in stale {
            let (client, account) = (self.clone(), account.clone());
            std::thread::spawn(move || {
                let base = if account.provider == "anthropic" { &client.anthropic_base } else { &client.chatgpt_base };
                let fetched = client.fresh(account.clone()).and_then(|fresh| accounts::fetch_usage(&fresh, base));
                let mut state = client.state.lock().unwrap();
                match &fetched {
                    Ok(_) => state.usage_errors.remove(&key(&account)),
                    Err(error) => state.usage_errors.insert(key(&account), error.clone()),
                };
                state.usage.insert(key(&account), (Instant::now(), fetched.ok()));
            });
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
                    google::ANTIGRAVITY => google::refresh(&google::ANTIGRAVITY_APP, &tokens.refresh),
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

    /// `try_send` in `thread`: a continue request with only the new messages when the API holds the rest, else a create.
    fn try_send_in_thread(&self, account: &Account, request: &Value, thread: &mut MessageThread, abort: &AtomicBool, on_delta: &mut dyn FnMut(Delta)) -> Result<Reply, ApiError> {
        let owner = key(account);
        let (body, continued) = match thread.continue_request(&owner, request) {
            Some(body) => (body, true),
            None => {
                let mut body = request.clone();
                body["thread"] = json!({"type": "create"});
                (body, false)
            }
        };
        let error = match self.try_send(account, &body, abort, on_delta) {
            Ok(reply) => {
                thread.record(owner, request, &reply);
                return Ok(reply);
            }
            Err(error) => error,
        };
        // A 404 right after a replay: the API may be unable to store a conversation this large.
        if error.code == "thread_unsupported_request" || (continued && error.code == "thread_not_found" && thread.replayed) {
            thread.unsupported_model = request["model"].clone();
            return self.try_send(account, request, abort, on_delta);
        }
        // A 400 the create names no thread code for: the beta header or the model may not take threads, which
        // the same turn without a thread tells apart from a request the API refuses either way.
        if !continued && error.code == "invalid_request_error" {
            let reply = self.try_send(account, request, abort, on_delta)?;
            thread.unsupported_model = request["model"].clone();
            return Ok(reply);
        }
        // An unknown 400 on a continue gets a replay too, as Claude Code does: a create shows whether the thread caused it.
        if !continued || !(THREAD_LOST.contains(&error.code.as_str()) || error.code == "invalid_request_error") {
            return Err(error);
        }
        // The API no longer holds the thread, or a dropped stream already continued it: send the whole conversation again.
        thread.last = None;
        let reply = self.try_send_in_thread(account, request, thread, abort, on_delta)?;
        thread.replayed = true;
        Ok(reply)
    }

    /// The request `account` sends for `request`, and the parser its reply stream needs.
    fn outgoing(&self, account: &Account, request: &Value) -> Result<Outgoing, ApiError> {
        let (headroom, headroom_token) = {
            let state = self.state.lock().unwrap();
            (state.headroom.clone(), state.headroom_token.clone())
        };
        let anthropic_base = headroom.clone().unwrap_or_else(|| self.anthropic_base.clone());
        let chatgpt_base = headroom.as_ref().map_or_else(|| self.chatgpt_base.clone(), |url| format!("{url}/backend-api"));
        let proxy_token = headroom.as_ref().map(|_| headroom_token).filter(|token| !token.is_empty());
        let custom = self.custom_provider(&account.provider);
        let without_google_thoughts;
        let has_google_thoughts = || request["messages"].as_array().into_iter().flatten().filter_map(|m| m["content"].as_array()).flatten().any(google::is_google_thought);
        let request = if account.provider == google::ANTIGRAVITY || !has_google_thoughts() {
            request
        } else {
            without_google_thoughts = drop_google_thoughts(request);
            &without_google_thoughts
        };
        let mut fallback = None;
        let (call, body, parser) = match (account.provider.as_str(), &account.secret) {
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
                    (call, body, Parser::Messages)
                } else {
                    let call = self.http.post(format!("{}/chat/completions", custom.base_url)).header("accept", "text/event-stream");
                    let call = if key.is_empty() { call } else { call.header("authorization", format!("Bearer {key}")) };
                    (call, openai_chat::request(request, id), Parser::OpenAiChat)
                }
            }
            ("anthropic", secret) => {
                let call = with_proxy_token(self.http.post(format!("{anthropic_base}/v1/messages")), proxy_token.as_deref()).header("anthropic-version", "2023-06-01");
                let mut body = request.clone();
                defer_rare_tools(&mut body);
                // Anthropic has one fast tier.
                if body["speed"] == "ultrafast" {
                    body["speed"] = json!("fast");
                }
                // Every request of a thread names its beta; the API compares the set with the create's.
                let thread_beta = if body.get("thread").is_some() { format!(",{MESSAGE_THREADS_BETA}") } else { String::new() };
                match secret {
                    Secret::ApiKey(key) => (call.header("x-api-key", key).header("anthropic-beta", format!("{THINKING_BETA}{thread_beta}")), body, Parser::Messages),
                    Secret::OAuth(tokens) => {
                        // A continue carries no system: the thread keeps the create's, identity block included.
                        if body["thread"]["type"] != "continue" {
                            let mut system = vec![json!({"type": "text", "text": CLAUDE_CODE_IDENTITY})];
                            system.extend(body["system"].as_array().cloned().unwrap_or_default());
                            body["system"] = json!(system);
                        }
                        let call = call
                            .header("authorization", format!("Bearer {}", tokens.access))
                            .header("anthropic-beta", format!("claude-code-20250219,oauth-2025-04-20,{THINKING_BETA}{thread_beta}"))
                            .header("user-agent", CLAUDE_CODE_USER_AGENT)
                            .header("x-app", "cli")
                            .header("anthropic-dangerous-direct-browser-access", "true");
                        (call, body, Parser::Messages)
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
                (call, body, Parser::Messages)
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
                (call, body, Parser::OpenAiChat)
            }
            (google::ANTIGRAVITY, Secret::OAuth(tokens)) => {
                let app = &google::ANTIGRAVITY_APP;
                let call_to = |base: &str| {
                    self.http
                        .post(format!("{base}/v1internal:streamGenerateContent?alt=sse"))
                        .header("authorization", format!("Bearer {}", tokens.access))
                        .header("accept", "text/event-stream")
                        .header("user-agent", google::user_agent())
                };
                fallback = app.fallback.map(call_to);
                let body = google::request(app, request, tokens.account_id.as_deref().unwrap_or_default(), &self.cache_key);
                (call_to(app.base), body, Parser::Google { owner: google::signature_owner(google::ANTIGRAVITY, google::wire_model(request)) })
            }
            ("openai-codex", Secret::OAuth(tokens)) => {
                let call = with_proxy_token(self.http.post(format!("{chatgpt_base}/codex/responses")), proxy_token.as_deref())
                    .header("authorization", format!("Bearer {}", tokens.access))
                    .header("chatgpt-account-id", tokens.account_id.as_deref().unwrap_or_default())
                    .header("OpenAI-Beta", "responses=experimental")
                    .header("originator", "pi")
                    .header("accept", "text/event-stream")
                    .header("session-id", &self.cache_key);
                (call, codex::request(request, &self.cache_key), Parser::Codex)
            }
            (provider, Secret::ApiKey(_)) => return Err(ApiError::fatal(format!("{provider} accounts need /login, not an API key."))),
            (provider, Secret::OAuth(_)) => return Err(ApiError::fatal(format!("{provider} accounts need an API key, not /login."))),
        };
        let hint = headroom.map(|url| format!(" Is `headroom proxy` running on {url}?")).unwrap_or_default();
        Ok(Outgoing { call, fallback, body, parser, hint })
    }

    fn try_send(&self, account: &Account, request: &Value, abort: &AtomicBool, on_delta: &mut dyn FnMut(Delta)) -> Result<Reply, ApiError> {
        let Outgoing { call, fallback, body, parser, hint } = self.outgoing(account, request)?;
        let body = body.to_string();
        let send = |call: ureq::RequestBuilder<ureq::typestate::WithBody>| call.header("content-type", "application/json").send(body.as_str());
        let mut sent = send(call);
        let failed = sent.as_ref().map_or(true, |response| response.status() == 429 || response.status().is_server_error());
        if failed && let Some(fallback) = fallback {
            sent = send(fallback);
        }
        let response = sent.map_err(|e| ApiError::retryable(format!("Request failed: {e}.{hint}")))?;
        let status = response.status().as_u16();
        let mut reader = watched_body(response.into_body().into_reader(), abort, STREAM_IDLE);
        if status != 200 {
            let mut text = String::new();
            let _ = std::io::Read::read_to_string(&mut reader, &mut text);
            let quota = status == 429 || (status == 529 && text.contains("quota"));
            let retryable = status == 408 || status == 429 || status >= 500;
            let error: Value = serde_json::from_str(&text).unwrap_or_default();
            let code = error["error"]["details"]["error_code"].as_str().or(error["error"]["type"].as_str()).unwrap_or_default().to_string();
            let message = match text.trim() {
                "" => format!("API error {status}."),
                body => format!("API error {status}: {body}"),
            };
            return Err(ApiError { message, retryable, quota, code });
        }
        match parser {
            Parser::Messages => parse_stream(&mut reader, abort, on_delta),
            Parser::OpenAiChat => openai_chat::parse_stream(&mut reader, abort, on_delta),
            Parser::Codex => codex::parse_stream(&mut reader, abort, on_delta),
            Parser::Google { owner } => google::parse_stream(&mut reader, &owner, abort, on_delta),
        }
    }

    /// Subscriptions, then API keys, by provider order, each provider's accounts in the priority order failover tries them.
    pub fn accounts(&self) -> Vec<AccountView> {
        let state = self.state.lock().unwrap();
        let now = Instant::now();
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
                status: status(&state, a, now),
            })
            .collect();
        let rank = |provider: &str| state.provider_order.iter().position(|listed| *listed == provider).unwrap_or(usize::MAX);
        views.sort_by_key(|a| (!a.oauth, rank(&a.provider)));
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
        let mut state = self.state.lock().unwrap();
        state.usage.remove(&usage_key);
        state.usage_errors.remove(&usage_key);
        state.checks.remove(&usage_key);
        drop(state);
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

    /// Swaps the account with the one before (`up`) or after it among its provider's accounts; at either end nothing moves.
    pub fn move_account(&self, provider: &str, name: &str, up: bool) -> Result<(), String> {
        let saved = accounts::update(&self.home, |list| {
            let same: Vec<usize> = (0..list.len()).filter(|i| list[*i].provider == provider).collect();
            let at = same.iter().position(|i| list[*i].name == name).ok_or(format!("{name} is not a saved account."))?;
            let neighbor = if up { at.checked_sub(1) } else { Some(at + 1) };
            if let Some(&neighbor) = neighbor.and_then(|n| same.get(n)) {
                list.swap(same[at], neighbor);
            }
            Ok(())
        })?;
        self.reload(saved);
        Ok(())
    }

    /// Name and provider account id of the account `likely_account` names.
    pub fn account(&self, provider: &str) -> Option<(String, Option<String>)> {
        let state = self.state.lock().unwrap();
        let account = likely_account(&state, provider)?;
        Some((account.name.clone(), account.account_id().map(String::from)))
    }

    /// After a plugin refused the account `model_id` would use until `until_ms`: marks it limited until then and picks
    /// the next account. False when the pick stays on it, as with switching off or every account over its limit.
    pub fn switch_account_until(&self, model_id: &str, until_ms: u64) -> bool {
        let Some(model) = self.catalog().iter().find(|m| m.id == model_id).cloned() else { return false };
        let blocked = {
            let mut state = self.state.lock().unwrap();
            let Some(blocked) = likely_account(&state, &model.provider).map(key) else { return false };
            let until = Instant::now() + Duration::from_millis(until_ms.saturating_sub(crate::process::now_ms()));
            state.limited.insert(blocked.clone(), until);
            blocked
        };
        matches!(self.choose(&model, false, &mut |_| {}), Ok(Choice::Use(next)) if key(&next) != blocked)
    }

    /// The account's last fetched subscription windows; None for API keys and before the first fetch.
    pub fn account_usage(&self, provider: &str, name: &str) -> Option<accounts::Usage> {
        self.state.lock().unwrap().usage.get(&format!("{provider}/{name}")).and_then(|(_, usage)| usage.clone())
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

    /// Runs `doctor`'s check on every account, each on its own thread, as their /account status; `on_done` runs after each.
    pub fn check_accounts(&self, on_done: impl Fn() + Clone + Send + 'static) {
        let accounts = self.state.lock().unwrap().accounts.clone();
        for account in accounts {
            self.state.lock().unwrap().checks.insert(key(&account), AccountStatus::Checking);
            let (client, on_done) = (self.clone(), on_done.clone());
            std::thread::spawn(move || {
                let status = match client.check(&account) {
                    Ok(_) => AccountStatus::Working,
                    Err(error) => AccountStatus::Failed(error),
                };
                let mut state = client.state.lock().unwrap();
                // A ping that went through outweighs an older 429.
                if status == AccountStatus::Working {
                    state.limited.remove(&key(&account));
                }
                state.checks.insert(key(&account), status);
                drop(state);
                on_done();
            });
        }
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
            Ok(usage) => match (usage.for_model(request["model"].as_str().unwrap_or_default()), usage.five_hour) {
                (Some(model), _) if !usage.models.is_empty() => format!("quota {:.0}%", model.seven_day),
                (None, _) => "no quota for this model".into(),
                (_, Some(five_hour)) => format!("5h {five_hour:.0}%, 7d {:.0}%", usage.seven_day),
                (_, None) => format!("7d {:.0}%", usage.seven_day),
            },
            Err(error) => error,
        };
        Ok(format!("{pinged}, {usage}"))
    }
}

fn moved_to(request: &mut Value, model: &Model) {
    request["model"] = json!(model.id);
    request["max_tokens"] = json!(request["max_tokens"].as_u64().unwrap_or(model.max_tokens).min(model.max_tokens));
}

/// A running check first, then a rate limit, the last check, the last usage fetch.
fn status(state: &State, account: &Account, now: Instant) -> AccountStatus {
    let key = key(account);
    let check = state.checks.get(&key);
    if check == Some(&AccountStatus::Checking) {
        return AccountStatus::Checking;
    }
    if state.limited.get(&key).is_some_and(|until| *until > now) {
        return AccountStatus::Limited;
    }
    if let Some(check) = check {
        return check.clone();
    }
    if let Some(error) = state.usage_errors.get(&key) {
        return AccountStatus::Failed(error.clone());
    }
    match state.usage.get(&key) {
        Some((_, Some(_))) => AccountStatus::Working,
        _ => AccountStatus::Unchecked,
    }
}

fn key(account: &Account) -> String {
    format!("{}/{}", account.provider, account.name)
}

/// The account the next request to `provider` is likely to use: the current pick, the pinned one, or the first.
fn likely_account<'a>(state: &'a State, provider: &str) -> Option<&'a Account> {
    let name = state.current.get(provider).or_else(|| state.pinned.get(provider));
    let mut candidates = state.accounts.iter().filter(|a| a.provider == provider);
    match name {
        Some(name) => candidates.find(|a| &a.name == name),
        None => candidates.next(),
    }
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

/// The token headroom asks of callers off loopback; only requests routed through headroom carry it.
fn with_proxy_token(call: ureq::RequestBuilder<ureq::typestate::WithBody>, token: Option<&str>) -> ureq::RequestBuilder<ureq::typestate::WithBody> {
    match token {
        Some(token) => call.header("x-headroom-proxy-token", token),
        None => call,
    }
}

pub struct ApiError {
    pub message: String,
    pub retryable: bool,
    /// 429, or 529 that names a quota: the account is out, another one may not be.
    pub quota: bool,
    /// Anthropic's `error.details.error_code`, such as `thread_not_found`, else its `error.type`
    /// (`invalid_request_error` for a 400 without a code); empty when the error has neither.
    pub code: String,
}

impl ApiError {
    pub fn retryable(message: String) -> ApiError {
        ApiError { message, retryable: true, quota: false, code: String::new() }
    }

    pub fn fatal(message: String) -> ApiError {
        ApiError { message, retryable: false, quota: false, code: String::new() }
    }
}

/// How a provider's reply stream is read.
#[derive(Debug, PartialEq)]
enum Parser {
    Messages,
    OpenAiChat,
    Codex,
    Google { owner: String },
}

/// One request ready to send: its call, a second endpoint tried on failure, its body and its reply parser.
struct Outgoing {
    call: ureq::RequestBuilder<ureq::typestate::WithBody>,
    fallback: Option<ureq::RequestBuilder<ureq::typestate::WithBody>>,
    body: Value,
    parser: Parser,
    /// Appended to a connection error.
    hint: String,
}

/// The request without Google's signed thoughts, which other providers reject or cannot use.
fn drop_google_thoughts(request: &Value) -> Value {
    let mut request = request.clone();
    for message in request["messages"].as_array_mut().into_iter().flatten() {
        if let Some(content) = message["content"].as_array_mut() {
            content.retain(|block| !google::is_google_thought(block));
        }
    }
    request
}

#[cfg(test)]
mod tests;
