use super::html::strip_tags;
use super::{Hit, Page, host, http_agent};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

const CREDENTIAL_TIMEOUT: Duration = Duration::from_secs(30);
const SEARCH_TIMEOUT: Duration = Duration::from_secs(60);
const MS_PER_DAY: u64 = 86_400_000;

/// pi's auto chain order (exa, brave, tavily, jina, perplexity); xai joins last since pi only reaches it when configured.
pub const PROVIDERS: [&str; 6] = ["exa", "brave", "tavily", "jina", "perplexity", "xai"];

pub struct Query<'a> {
    pub text: &'a str,
    pub num_results: usize,
    pub recency: Option<Recency>,
    pub domains: &'a [String],
    pub include_content: bool,
    pub category: Option<&'a str>,
}

#[derive(Debug, Default)]
pub struct Response {
    pub answer: String,
    pub results: Vec<Hit>,
    pub inline: Vec<Page>,
    pub providers: Vec<String>,
}

/// `~/.agents/web-search.json` for one tool call, with each provider's key resolved at most once.
pub struct Config {
    path: PathBuf,
    /// The providers "auto" and "all" try, in order.
    order: Vec<&'static str>,
    settings: Value,
    keys: [OnceLock<Result<Option<String>, String>>; PROVIDERS.len()],
}

/// Shared with pi; a missing file is an empty config.
pub fn load_config(path: &Path, order: Vec<&'static str>) -> Result<Config, String> {
    let settings = match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| format!("{} is not valid JSON: {e}", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(e) => return Err(format!("Cannot read {}: {e}", path.display())),
    };
    Ok(Config { path: path.to_path_buf(), order, settings, keys: Default::default() })
}

/// Runs `query` against the providers `requested` names: "auto" (or absent), "all", one name, or a list.
pub fn search(config: &Config, requested: &Value, query: &Query) -> Result<Response, String> {
    let requested = match requested {
        Value::Null => Value::from("auto"),
        Value::String(s) if s.trim().is_empty() => Value::from("auto"),
        other => other.clone(),
    };
    if let Some(list) = requested.as_array() {
        let mut names = Vec::new();
        for name in list {
            names.push(known(name.as_str().unwrap_or_default())?);
        }
        if names.is_empty() {
            return Err(format!("The provider list is empty. Use auto, all, or one of: {}.", PROVIDERS.join(", ")));
        }
        return search_parallel(config, &names, query, "Selected-provider");
    }
    match requested.as_str().unwrap_or_default().trim().to_lowercase().as_str() {
        "auto" => search_auto(config, query),
        "all" => {
            let available: Vec<&str> = config.order.iter().copied().filter(|p| has_key(&config.settings, p)).collect();
            if available.is_empty() {
                return Err(format!("No configured search provider available for provider \"all\". {}", missing_keys_hint(&config.path)));
            }
            search_parallel(config, &available, query, "All-provider")
        }
        name => {
            let name = known(name)?;
            let mut response = search_one(config, name, query)?;
            response.providers = vec![name.to_string()];
            Ok(response)
        }
    }
}

fn known(name: &str) -> Result<&'static str, String> {
    let name = name.trim().to_lowercase();
    PROVIDERS.into_iter().find(|p| *p == name).ok_or_else(|| format!("Unknown provider \"{name}\". Use auto, all, or one of: {}.", PROVIDERS.join(", ")))
}

/// The provider web-search.json names goes first, then pi's chain; an empty answer keeps the chain going.
fn search_auto(config: &Config, query: &Query) -> Result<Response, String> {
    let configured = config.settings["searchProvider"].as_str().or(config.settings["provider"].as_str()).and_then(|name| known(name).ok());
    let mut order: Vec<&str> = configured.into_iter().collect();
    order.extend(config.order.iter().copied().filter(|p| Some(*p) != configured));
    let mut errors = Vec::new();
    let mut first_empty = None;
    for provider in order {
        if !has_key(&config.settings, provider) {
            continue;
        }
        match search_one(config, provider, query) {
            Ok(mut response) => {
                response.providers = vec![provider.to_string()];
                if !response.results.is_empty() || !response.answer.trim().is_empty() {
                    return Ok(response);
                }
                first_empty.get_or_insert(response);
            }
            Err(e) => errors.push(format!("{}: {e}", label(provider))),
        }
    }
    if let Some(response) = first_empty {
        return Ok(response);
    }
    if !errors.is_empty() {
        return Err(format!("Auto provider search failed:\n  - {}", errors.join("\n  - ")));
    }
    Err(format!("No search provider available. {}", missing_keys_hint(&config.path)))
}

fn search_parallel(config: &Config, providers: &[&'static str], query: &Query, kind: &str) -> Result<Response, String> {
    let outcomes: Vec<(&str, Result<Response, String>)> = std::thread::scope(|scope| {
        let handles: Vec<_> = providers.iter().map(|p| (*p, scope.spawn(move || search_one(config, p, query)))).collect();
        handles.into_iter().map(|(p, handle)| (p, handle.join().unwrap_or_else(|_| Err("search thread panicked".into())))).collect()
    });
    let mut merged = Response::default();
    let mut sections = Vec::new();
    let mut failures = Vec::new();
    for (provider, outcome) in outcomes {
        match outcome {
            Ok(response) => {
                let answer = if response.answer.is_empty() { "(No answer text returned.)" } else { &response.answer };
                sections.push(format!("## {}\n\n{answer}", label(provider)));
                for hit in response.results {
                    if !merged.results.iter().any(|h| h.url == hit.url) {
                        merged.results.push(hit);
                    }
                }
                for page in response.inline {
                    if !merged.inline.iter().any(|p| p.url == page.url) {
                        merged.inline.push(page);
                    }
                }
                merged.providers.push(provider.to_string());
            }
            Err(e) => failures.push(format!("{}: {e}", label(provider))),
        }
    }
    if merged.providers.is_empty() {
        return Err(format!("{kind} search failed:\n  - {}", failures.join("\n  - ")));
    }
    if !failures.is_empty() {
        let lines: Vec<String> = failures.iter().map(|f| format!("- {f}")).collect();
        sections.push(format!("## Provider errors\n\n{}", lines.join("\n")));
    }
    merged.answer = sections.join("\n\n");
    Ok(merged)
}

fn search_one(config: &Config, provider: &str, query: &Query) -> Result<Response, String> {
    let Some(key) = api_key(config, provider)? else {
        return Err(format!("Providers without credentials: {} (set {} or {}ApiKey in {}).", label(provider), env_name(provider), provider, config.path.display()));
    };
    let result = match provider {
        "exa" => exa(&config.settings, &key, query),
        "brave" => brave(&config.settings, &key, query),
        "tavily" => tavily(&config.settings, &key, query),
        "jina" => jina(&key, query),
        "perplexity" => perplexity(&key, query),
        "xai" => xai(&config.settings, &key, query),
        other => unreachable!("{other} is not in PROVIDERS"),
    };
    result.map_err(|e| e.replace(&key, "[redacted]").chars().take(400).collect())
}

pub fn label(provider: &str) -> &'static str {
    match provider {
        "exa" => "Exa",
        "brave" => "Brave",
        "tavily" => "Tavily",
        "jina" => "Jina",
        "perplexity" => "Perplexity",
        "xai" => "xAI",
        other => unreachable!("{other} is not in PROVIDERS"),
    }
}

fn env_name(provider: &str) -> String {
    format!("{}_API_KEY", provider.to_uppercase())
}

fn missing_keys_hint(config_path: &Path) -> String {
    let envs: Vec<String> = PROVIDERS.iter().map(|p| env_name(p)).collect();
    let fields: Vec<String> = PROVIDERS.iter().map(|p| format!("{p}ApiKey")).collect();
    format!("Providers without credentials: set {} or {} in {}.", envs.join(", "), fields.join(", "), config_path.display())
}

/// Blank values on either side count as unset, so an empty `exaApiKey` does not hide `EXA_API_KEY`.
fn configured_key<'a>(config: &'a Value, provider: &str) -> Option<&'a str> {
    config[format!("{provider}ApiKey")].as_str().map(str::trim).filter(|key| !key.is_empty())
}

fn env_key(provider: &str) -> Option<String> {
    std::env::var(env_name(provider)).ok().filter(|key| !key.trim().is_empty())
}

fn has_key(config: &Value, provider: &str) -> bool {
    configured_key(config, provider).is_some() || env_key(provider).is_some()
}

/// Only web-search.json values use the credential syntax; an env value is the key itself.
/// The key is resolved once per `Config`, so a `!cmd` with an unlock prompt asks once per tool call.
fn api_key(config: &Config, provider: &str) -> Result<Option<String>, String> {
    let slot = PROVIDERS.iter().position(|p| *p == provider).expect("provider comes from PROVIDERS");
    let resolve = || match configured_key(&config.settings, provider) {
        Some(raw) => resolve_credential(raw, label(provider)).map(Some),
        None => Ok(env_key(provider)),
    };
    config.keys[slot].get_or_init(resolve).clone()
}

/// pi's credential syntax: `$NAME`/`${NAME}` reads the env, `!cmd` runs a shell command, `$$x`/`$!x` escape a literal.
fn resolve_credential(raw: &str, provider: &str) -> Result<String, String> {
    if raw.starts_with("$$") || raw.starts_with("$!") {
        return Ok(raw[1..].to_string());
    }
    if let Some(command) = raw.strip_prefix('!') {
        // From `/`, so a relative command never runs out of the project the agent was opened in.
        let mut shell = crate::process::command("sh");
        shell.arg("-c").arg(command).current_dir("/");
        let output = crate::process::output_within(&mut shell, CREDENTIAL_TIMEOUT)
            .map_err(|e| format!("{provider} credential command failed: {e}"))?
            .ok_or_else(|| format!("{provider} credential command timed out after {} seconds.", CREDENTIAL_TIMEOUT.as_secs()))?;
        let key = String::from_utf8_lossy(&output.stdout).trim().to_string();
        // Its stderr is not shown: it may hold the secret or a long dump.
        if !output.status.success() || key.is_empty() {
            return Err(format!("{provider} credential command produced no key ({}).", output.status));
        }
        return Ok(key);
    }
    if let Some(name) = raw.strip_prefix('$') {
        let name = name.strip_prefix('{').and_then(|n| n.strip_suffix('}')).unwrap_or(name);
        return std::env::var(name).ok().filter(|v| !v.is_empty()).ok_or_else(|| format!("{provider} credential references unset environment variable {name}."));
    }
    Ok(raw.to_string())
}

/// `EXA_BASE_URL` beats `exaBaseUrl` in web-search.json; both must be absolute http(s) URLs.
fn base_url(config: &Value, provider: &str, default: &str) -> Result<String, String> {
    let env = format!("{}_BASE_URL", provider.to_uppercase());
    let field = format!("{provider}BaseUrl");
    let (source, value) = match std::env::var(&env).ok().filter(|v| !v.trim().is_empty()) {
        Some(value) => (env, value),
        None => match config[&field].as_str() {
            Some(value) => (field, value.to_string()),
            None => return Ok(default.to_string()),
        },
    };
    let value = value.trim().trim_end_matches('/');
    if !(value.starts_with("http://") || value.starts_with("https://")) || host(value).is_none() {
        return Err(format!("{source} must be an absolute HTTP(S) URL"));
    }
    Ok(value.to_string())
}

fn send(response: Result<ureq::http::Response<ureq::Body>, ureq::Error>, name: &str) -> Result<Value, String> {
    let mut response = response.map_err(|e| format!("{name} request failed: {e}"))?;
    let status = response.status().as_u16();
    let text = response.body_mut().with_config().limit(10 * 1024 * 1024).lossy_utf8(true).read_to_string().map_err(|e| format!("{name} response unreadable: {e}"))?;
    if !(200..300).contains(&status) {
        // Untruncated: `search_one` redacts the key first, then shortens it, so no partial key survives the cut.
        return Err(format!("{name} API error {status}: {text}"));
    }
    serde_json::from_str(&text).map_err(|e| format!("{name} API returned invalid JSON: {e}"))
}

/// A trimmed, lowercased hostname from a domain filter entry such as `-https://Docs.rs/x`; None when it is not a domain.
pub fn normalize_domain(raw: &str) -> Option<String> {
    let lowered = raw.trim().to_lowercase();
    let value = lowered.strip_prefix('-').unwrap_or(&lowered);
    let hostname = if value.contains("://") { host(value)? } else { value.split(['/', ':']).next().unwrap_or_default().to_string() };
    let domain = hostname.trim_matches('.');
    let (name, tld) = domain.rsplit_once('.')?;
    let valid = name.starts_with(|c: char| c.is_ascii_alphanumeric())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
        && tld.len() >= 2
        && tld.chars().all(|c| c.is_ascii_lowercase());
    valid.then(|| domain.to_string())
}

/// (allowed, blocked) domains, normalized and deduplicated; a leading `-` blocks.
fn split_domains(domains: &[String]) -> (Vec<String>, Vec<String>) {
    let (mut allowed, mut blocked) = (Vec::new(), Vec::new());
    for raw in domains {
        let Some(domain) = normalize_domain(raw) else { continue };
        let target = if raw.trim().starts_with('-') { &mut blocked } else { &mut allowed };
        if !target.contains(&domain) {
            target.push(domain);
        }
    }
    (allowed, blocked)
}

fn host_matches(url: &str, domain: &str) -> bool {
    host(url).is_some_and(|h| h == domain || h.ends_with(&format!(".{domain}")))
}

fn str_of<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap_or_default()
}

fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// pi's "snippet\nSource: title (url)" answer that search-only providers return.
fn answer_from_hits(hits: &[Hit]) -> String {
    let parts: Vec<String> = hits.iter().map(|h| if h.snippet.is_empty() { format!("Source: {} ({})", h.title, h.url) } else { format!("{}\nSource: {} ({})", h.snippet, h.title, h.url) }).collect();
    parts.join("\n\n")
}

fn exa(config: &Value, key: &str, query: &Query) -> Result<Response, String> {
    let base = base_url(config, "exa", "https://api.exa.ai")?;
    let mut body = json!({
        "query": query.text,
        "type": "auto",
        "numResults": query.num_results,
        "contents": if query.include_content { json!({"text": true, "highlights": true}) } else { json!({"highlights": true}) },
    });
    let (include, exclude) = split_domains(query.domains);
    if !include.is_empty() {
        body["includeDomains"] = json!(include);
    }
    if !exclude.is_empty() {
        body["excludeDomains"] = json!(exclude);
    }
    if let Some(category) = query.category {
        body["category"] = json!(category);
    }
    if let Some(recency) = query.recency {
        body["startPublishedDate"] = json!(iso_days_ago(recency.days()));
    }
    let agent = http_agent(SEARCH_TIMEOUT);
    let data = send(agent.post(format!("{base}/search")).header("x-api-key", key).header("content-type", "application/json").send(body.to_string()), "Exa")?;
    let (mut results, mut parts, mut inline) = (Vec::new(), Vec::new(), Vec::new());
    for (i, item) in data["results"].as_array().into_iter().flatten().enumerate() {
        let url = str_of(item, "url");
        if url.is_empty() {
            continue;
        }
        let title = match str_of(item, "title") {
            "" => host(url).unwrap_or_else(|| format!("Source {}", i + 1)),
            title => title.to_string(),
        };
        let highlights: Vec<&str> = item["highlights"].as_array().into_iter().flatten().filter_map(Value::as_str).filter(|h| !h.trim().is_empty()).collect();
        let text = str_of(item, "text");
        let content = if highlights.is_empty() { text.trim().chars().take(1000).collect() } else { highlights.join(" ") };
        if !content.is_empty() {
            parts.push(format!("{content}\nSource: {title} ({url})"));
        }
        if !text.is_empty() {
            inline.push(Page { url: url.into(), title: title.clone(), content: text.into(), error: None });
        }
        results.push(Hit { title, url: url.into(), snippet: String::new() });
    }
    Ok(Response { answer: parts.join("\n\n"), results, inline, providers: Vec::new() })
}

fn brave(config: &Value, key: &str, query: &Query) -> Result<Response, String> {
    let base = base_url(config, "brave", "https://api.search.brave.com/res/v1")?;
    let (allowed, blocked) = split_domains(query.domains);
    let mut q = vec![query.text.to_string()];
    if !allowed.is_empty() {
        q.push(allowed.iter().map(|d| format!("site:{d}")).collect::<Vec<_>>().join(" OR "));
    }
    q.extend(blocked.iter().map(|d| format!("NOT site:{d}")));
    let count = if query.domains.is_empty() { query.num_results } else { 20 };
    let mut request = http_agent(Duration::from_secs(30))
        .get(format!("{base}/web/search"))
        .query("q", q.join(" "))
        .query("count", count.to_string())
        .header("accept", "application/json")
        .header("x-subscription-token", key);
    if let Some(freshness) = query.recency.map(Recency::brave) {
        request = request.query("freshness", freshness);
    }
    let data = send(request.call(), "Brave Search")?;
    let mut results = Vec::new();
    for item in data["web"]["results"].as_array().into_iter().flatten() {
        let url = str_of(item, "url");
        let filtered_out = (!allowed.is_empty() && !allowed.iter().any(|d| host_matches(url, d))) || blocked.iter().any(|d| host_matches(url, d));
        if url.is_empty() || filtered_out {
            continue;
        }
        let title = super::html::decode(&strip_tags(str_of(item, "title"))).trim().to_string();
        let snippet = super::html::decode(&strip_tags(str_of(item, "description"))).trim().to_string();
        results.push(Hit { title: if title.is_empty() { url.into() } else { title }, url: url.into(), snippet });
        if results.len() >= query.num_results {
            break;
        }
    }
    Ok(Response { answer: answer_from_hits(&results), results, ..Default::default() })
}

fn tavily(config: &Value, key: &str, query: &Query) -> Result<Response, String> {
    let base = base_url(config, "tavily", "https://api.tavily.com")?;
    let mut body = json!({
        "query": query.text,
        "search_depth": "basic",
        "max_results": query.num_results,
        "include_answer": "basic",
        "include_raw_content": if query.include_content { json!("markdown") } else { json!(false) },
    });
    if let Some(recency) = query.recency {
        body["time_range"] = json!(recency.as_str());
    }
    let (allowed, blocked) = split_domains(query.domains);
    if !allowed.is_empty() {
        body["include_domains"] = json!(allowed);
    }
    if !blocked.is_empty() {
        body["exclude_domains"] = json!(blocked);
    }
    let agent = http_agent(SEARCH_TIMEOUT);
    let data = send(agent.post(format!("{base}/search")).header("authorization", format!("Bearer {key}")).header("content-type", "application/json").send(body.to_string()), "Tavily")?;
    let (mut results, mut inline) = (Vec::new(), Vec::new());
    for item in data["results"].as_array().into_iter().flatten() {
        let url = str_of(item, "url");
        if url.is_empty() {
            continue;
        }
        let title = match str_of(item, "title") {
            "" => format!("Source {}", results.len() + 1),
            title => title.to_string(),
        };
        let raw = str_of(item, "raw_content");
        if !raw.is_empty() {
            inline.push(Page { url: url.into(), title: title.clone(), content: raw.into(), error: None });
        }
        results.push(Hit { title, url: url.into(), snippet: collapse(str_of(item, "content")) });
        if results.len() >= query.num_results {
            break;
        }
    }
    Ok(Response { answer: str_of(&data, "answer").to_string(), results, inline, providers: Vec::new() })
}

fn jina(key: &str, query: &Query) -> Result<Response, String> {
    let (allowed, blocked) = split_domains(query.domains);
    let mut text = query.text.to_string();
    for domain in &blocked {
        text.push_str(&format!(" -site:{domain}"));
    }
    if let Some(recency) = query.recency {
        text.push_str(&format!(" published in the past {}", recency.as_str()));
    }
    let mut request = http_agent(SEARCH_TIMEOUT)
        .get(format!("https://s.jina.ai/{}", crate::api::oauth::percent_encode(&text)))
        .query("count", query.num_results.to_string())
        .header("accept", "application/json")
        .header("authorization", format!("Bearer {key}"))
        .header("user-agent", "pi-web-access")
        .header("x-respond-with", if query.include_content { "content" } else { "no-content" });
    for domain in &allowed {
        request = request.query("site", domain);
    }
    let data = send(request.call(), "Jina Search")?;
    if data["code"].as_i64().is_some_and(|code| code != 200) {
        return Err(format!("Jina Search API error {}: response envelope reported failure", data["code"]));
    }
    let items = if data.is_array() { &data } else { &data["data"] };
    let Some(items) = items.as_array() else {
        return Err("Jina Search API returned invalid response: expected data array".into());
    };
    let (mut results, mut inline) = (Vec::new(), Vec::<Page>::new());
    for item in items {
        let url = str_of(item, "url").trim();
        let filtered_out = (!allowed.is_empty() && !allowed.iter().any(|d| host_matches(url, d))) || blocked.iter().any(|d| host_matches(url, d));
        if url.is_empty() || filtered_out || results.iter().any(|h: &Hit| h.url == url) {
            continue;
        }
        let title = match str_of(item, "title").trim() {
            "" => format!("Source {}", results.len() + 1),
            title => title.to_string(),
        };
        let content = str_of(item, "content");
        if !content.is_empty() {
            inline.push(Page { url: url.into(), title: title.clone(), content: content.into(), error: None });
        }
        results.push(Hit { title, url: url.into(), snippet: collapse(str_of(item, "description")) });
        if results.len() >= query.num_results {
            break;
        }
    }
    Ok(Response { answer: answer_from_hits(&results), results, inline, providers: Vec::new() })
}

fn perplexity(key: &str, query: &Query) -> Result<Response, String> {
    let mut body = json!({
        "query": query.text,
        "max_results": query.num_results,
        "search_type": "fast",
        "search_context_size": "medium",
    });
    if let Some(recency) = query.recency {
        body["search_recency_filter"] = json!(recency.as_str());
    }
    let domains: Vec<String> = query
        .domains
        .iter()
        .filter_map(|raw| {
            let raw = raw.trim();
            let (sign, domain) = raw.strip_prefix('-').map_or(("", raw), |d| ("-", d));
            normalize_domain(domain).map(|d| format!("{sign}{d}"))
        })
        .collect();
    if !domains.is_empty() {
        body["search_domain_filter"] = json!(domains);
    }
    let agent = http_agent(SEARCH_TIMEOUT);
    let data = send(agent.post("https://api.perplexity.ai/search").header("authorization", format!("Bearer {key}")).header("content-type", "application/json").send(body.to_string()), "Perplexity")?;
    let mut results = Vec::new();
    for item in data["results"].as_array().into_iter().flatten() {
        let url = str_of(item, "url");
        if url.is_empty() {
            continue;
        }
        let title = match str_of(item, "title") {
            "" => format!("Source {}", results.len() + 1),
            title => title.to_string(),
        };
        results.push(Hit { title, url: url.into(), snippet: collapse(str_of(item, "snippet")) });
    }
    Ok(Response { answer: answer_from_hits(&results), results, ..Default::default() })
}

/// Filters ride in the prompt, not as tool parameters: xAI's filter field names are unverified and an unknown one 400s.
fn xai(config: &Value, key: &str, query: &Query) -> Result<Response, String> {
    let mut lines = vec!["Search the web and answer using only what the web results say.".to_string(), "Cite your sources inline.".to_string()];
    if let Some(recency) = query.recency {
        lines.push(format!("Prefer sources from the {}.", recency.phrase()));
    }
    lines.push(format!("Prefer around {} distinct sources.", query.num_results));
    let (allowed, blocked) = split_domains(query.domains);
    if !allowed.is_empty() {
        lines.push(format!("Only use sources from: {}.", allowed.iter().take(100).cloned().collect::<Vec<_>>().join(", ")));
    }
    if !blocked.is_empty() {
        lines.push(format!("Do not use sources from: {}.", blocked.iter().take(100).cloned().collect::<Vec<_>>().join(", ")));
    }
    let body = json!({
        "model": config["xaiSearchModel"].as_str().unwrap_or("grok-4.5"),
        "input": format!("{}\n\n{}", lines.join(" "), query.text),
        "tools": [{"type": "web_search"}],
    });
    let agent = http_agent(SEARCH_TIMEOUT);
    let data = send(agent.post("https://api.x.ai/v1/responses").header("authorization", format!("Bearer {key}")).header("content-type", "application/json").send(body.to_string()), "xAI")?;
    let response = xai_response(&data, query.num_results);
    if response.answer.is_empty() && response.results.is_empty() {
        return Err("xAI web_search returned no answer or sources".into());
    }
    Ok(response)
}

/// Annotation sources come first: they carry titles and must win URL deduplication over the bare `citations` list.
fn xai_response(data: &Value, num_results: usize) -> Response {
    let mut answer = Vec::new();
    let mut results: Vec<Hit> = Vec::new();
    let add = |results: &mut Vec<Hit>, url: &Value, title: &Value, snippet: String| {
        let Some(url) = url.as_str().filter(|u| !u.trim().is_empty()) else { return };
        if results.iter().any(|h| h.url == url) {
            return;
        }
        let title = title.as_str().filter(|t| !t.trim().is_empty()).unwrap_or(url);
        results.push(Hit { title: title.into(), url: url.into(), snippet });
    };
    let output = data["output"].as_array().map(Vec::as_slice).unwrap_or_default();
    for part in output.iter().filter(|item| item["type"] == "message").flat_map(|item| item["content"].as_array().into_iter().flatten()) {
        let text = str_of(part, "text");
        if !text.trim().is_empty() {
            answer.push(text);
        }
        for annotation in part["annotations"].as_array().into_iter().flatten().filter(|a| a["type"] == "url_citation") {
            let snippet = snippet_around(text, annotation["start_index"].as_u64(), annotation["end_index"].as_u64());
            add(&mut results, &annotation["url"], &annotation["title"], snippet);
        }
    }
    let calls = output.iter().filter(|item| item["type"] == "web_search_call" || item["type"] == "x_search_call");
    let call_sources = calls.flat_map(|item| [&item["action"]["sources"], &item["sources"], &item["results"]]);
    for source in call_sources.chain([&data["citations"]]).flat_map(|group| group.as_array().into_iter().flatten()) {
        if source.is_string() {
            add(&mut results, source, &Value::Null, String::new());
        } else {
            let url = if source["url"].is_null() { &source["source_website_url"] } else { &source["url"] };
            let title = if source["title"].is_null() { &source["caption"] } else { &source["title"] };
            add(&mut results, url, title, String::new());
        }
    }
    results.truncate(num_results);
    Response { answer: answer.join("\n").trim().to_string(), results, ..Default::default() }
}

/// xAI annotation offsets count characters; 100 chars either side, markdown links flattened, capped at 300.
fn snippet_around(text: &str, start: Option<u64>, end: Option<u64>) -> String {
    let (Some(start), Some(end)) = (start, end) else { return String::new() };
    let chars: Vec<char> = text.chars().collect();
    let from = (start as usize).saturating_sub(100).min(chars.len());
    let to = (end as usize).saturating_add(100).min(chars.len()).max(from);
    let snippet = flatten_links(&chars[from..to].iter().collect::<String>()).trim().to_string();
    if snippet.chars().count() > 300 { format!("{}...", snippet.chars().take(297).collect::<String>()) } else { snippet }
}

/// `[label](url)` becomes `label`.
fn flatten_links(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let link = after.find("](").filter(|close| !after[..*close].contains('[')).and_then(|close| after[close + 2..].find(')').map(|paren| (close, close + 2 + paren)));
        match link {
            Some((close, paren)) => {
                out.push_str(&after[..close]);
                rest = &after[paren + 1..];
            }
            None => {
                out.push('[');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Recency {
    Day,
    Week,
    Month,
    Year,
}

impl Recency {
    pub fn parse(value: &str) -> Option<Recency> {
        match value {
            "day" => Some(Recency::Day),
            "week" => Some(Recency::Week),
            "month" => Some(Recency::Month),
            "year" => Some(Recency::Year),
            _ => None,
        }
    }

    /// The `recencyFilter` word, which Tavily, Perplexity and research artifacts take as is.
    pub fn as_str(self) -> &'static str {
        match self {
            Recency::Day => "day",
            Recency::Week => "week",
            Recency::Month => "month",
            Recency::Year => "year",
        }
    }

    fn days(self) -> u64 {
        match self {
            Recency::Day => 1,
            Recency::Week => 7,
            Recency::Month => 30,
            Recency::Year => 365,
        }
    }

    fn brave(self) -> &'static str {
        match self {
            Recency::Day => "pd",
            Recency::Week => "pw",
            Recency::Month => "pm",
            Recency::Year => "py",
        }
    }

    fn phrase(self) -> &'static str {
        match self {
            Recency::Day => "past 24 hours",
            Recency::Week => "past week",
            Recency::Month => "past month",
            Recency::Year => "past year",
        }
    }
}

/// `YYYY-MM-DDTHH:MM:SS.mmmZ` for now minus `days`, like JS `toISOString`.
fn iso_days_ago(days: u64) -> String {
    iso_from_ms(crate::process::now_ms() - days * MS_PER_DAY)
}

pub(crate) fn iso_from_ms(ms: u64) -> String {
    let (day_count, ms_of_day) = (ms / MS_PER_DAY, ms % MS_PER_DAY);
    // Hinnant's civil_from_days.
    let z = day_count as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let (h, m, s, milli) = (ms_of_day / 3_600_000, ms_of_day / 60_000 % 60, ms_of_day / 1000 % 60, ms_of_day % 1000);
    format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}.{milli:03}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_follow_pi_syntax() {
        unsafe { std::env::set_var("WEB_TEST_KEY", "from-env") };
        assert_eq!(resolve_credential("$WEB_TEST_KEY", "Exa").unwrap(), "from-env");
        assert_eq!(resolve_credential("${WEB_TEST_KEY}", "Exa").unwrap(), "from-env");
        assert_eq!(resolve_credential("$$literal", "Exa").unwrap(), "$literal");
        assert_eq!(resolve_credential("$!literal", "Exa").unwrap(), "!literal");
        assert_eq!(resolve_credential("!printf ' k1 '", "Exa").unwrap(), "k1");
        assert_eq!(resolve_credential("plain", "Exa").unwrap(), "plain");
        assert!(resolve_credential("$WEB_TEST_UNSET_KEY", "Exa").is_err());
    }

    #[test]
    fn domains_normalize_and_split() {
        assert_eq!(normalize_domain(" -https://Docs.RS/serde "), Some("docs.rs".into()));
        assert_eq!(normalize_domain("example.com/path"), Some("example.com".into()));
        assert_eq!(normalize_domain("localhost"), None);
        let (allowed, blocked) = split_domains(&["a.com".into(), "-b.org".into(), "a.com".into()]);
        assert_eq!((allowed, blocked), (vec!["a.com".to_string()], vec!["b.org".to_string()]));
    }

    #[test]
    fn base_url_rejects_relative_values() {
        let config = json!({"braveBaseUrl": "api.local/v1"});
        assert_eq!(base_url(&config, "brave", "https://x").unwrap_err(), "braveBaseUrl must be an absolute HTTP(S) URL");
        let config = json!({"braveBaseUrl": "http://127.0.0.1:9/v1/"});
        assert_eq!(base_url(&config, "brave", "https://x").unwrap(), "http://127.0.0.1:9/v1");
    }

    #[test]
    fn xai_results_prefer_annotations_then_citations() {
        let data = json!({
            "output": [{"type": "message", "content": [{"type": "output_text", "text": "Rust is fast [docs](https://a.dev).", "annotations": [{"type": "url_citation", "url": "https://a.dev", "title": "A", "start_index": 0, "end_index": 4}]}]}],
            "citations": ["https://a.dev", {"source_website_url": "https://b.dev", "caption": "B"}],
        });
        let response = xai_response(&data, 5);
        assert_eq!(response.answer, "Rust is fast [docs](https://a.dev).");
        let hits: Vec<(&str, &str, &str)> = response.results.iter().map(|h| (h.title.as_str(), h.url.as_str(), h.snippet.as_str())).collect();
        assert_eq!(hits, [("A", "https://a.dev", "Rust is fast docs."), ("B", "https://b.dev", "")]);
    }

    #[test]
    fn iso_dates_match_js() {
        assert_eq!(iso_from_ms(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso_from_ms(1_709_210_096_789), "2024-02-29T12:34:56.789Z");
    }

    #[test]
    fn auto_without_keys_names_the_missing_credentials() {
        let query = Query { text: "q", num_results: 5, recency: None, domains: &[], include_content: false, category: None };
        for provider in PROVIDERS {
            if std::env::var(env_name(provider)).is_ok() {
                return;
            }
        }
        let error = search(&load_config(Path::new("/cfg.json"), PROVIDERS.to_vec()).unwrap(), &Value::Null, &query).unwrap_err();
        assert!(error.starts_with("No search provider available. Providers without credentials: set EXA_API_KEY"), "{error}");
    }

    #[test]
    fn a_credential_command_runs_once_and_env_keys_are_literal() {
        let dir = std::env::temp_dir().join(format!("agent-web-keys-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("runs");
        let settings = json!({"exaApiKey": format!("!echo run >> {}; echo k1", log.display()), "tavilyApiKey": " "});
        std::fs::write(dir.join("web-search.json"), settings.to_string()).unwrap();
        unsafe { std::env::set_var("TAVILY_API_KEY", "!from-env") };
        let config = load_config(&dir.join("web-search.json"), PROVIDERS.to_vec()).unwrap();
        let keys = [api_key(&config, "exa"), api_key(&config, "exa"), api_key(&config, "tavily")];
        let runs = std::fs::read_to_string(&log).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(keys, [Ok(Some("k1".into())), Ok(Some("k1".into())), Ok(Some("!from-env".into()))]);
        assert_eq!(runs, "run\n");
    }

    #[test]
    fn a_provider_redirect_is_not_followed_with_the_key() {
        let leak = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let api = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let location = format!("http://{}/search", leak.local_addr().unwrap());
        let api_base = format!("http://{}", api.local_addr().unwrap());
        std::thread::spawn(move || {
            let (mut stream, _) = api.accept().unwrap();
            // Read the whole request: closing a socket with its body unread resets it, and macOS then fails the client with EINVAL.
            let mut reader = std::io::BufReader::new(&stream);
            let mut length = 0;
            let mut header = String::new();
            while std::io::BufRead::read_line(&mut reader, &mut header).unwrap() > 2 {
                if let Some(value) = header.to_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap();
                }
                header.clear();
            }
            std::io::Read::read_exact(&mut reader, &mut vec![0; length]).unwrap();
            let reply = format!("HTTP/1.1 302 Found\r\nlocation: {location}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
            _ = std::io::Write::write_all(&mut stream, reply.as_bytes());
        });
        leak.set_nonblocking(true).unwrap();
        let query = Query { text: "q", num_results: 5, recency: None, domains: &[], include_content: false, category: None };
        let error = exa(&json!({"exaBaseUrl": api_base}), "SECRETKEY", &query).unwrap_err();
        assert!(error.starts_with("Exa API error 302"), "{error}");
        assert!(leak.accept().is_err(), "the redirect target got the request");
    }
}
