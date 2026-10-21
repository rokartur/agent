//! Web tools ported from pi-web-access: web_search, fetch_content and get_search_content.

mod fetch;
mod find;
mod github;
mod html;
mod providers;

pub use providers::PROVIDERS as SEARCH_PROVIDERS;

use find::Mode;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// pi's maxInlineContentChars: the most one tool result or content page carries.
const MAX_INLINE_CHARS: usize = 30_000;
const STORED_RESPONSES: usize = 20;
const MAX_FETCHED_PAGES: usize = 20;
const MAX_QUERIES: usize = 8;
const STORED_SOURCES: &str = "WebSearch or WebFetch";

#[derive(Clone, Debug)]
pub struct Hit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

#[derive(Clone, Debug)]
pub struct Page {
    pub url: String,
    pub title: String,
    pub content: String,
    pub error: Option<String>,
}

/// The lowercase hostname of an absolute URL.
pub fn host(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    let hostname = match authority.strip_prefix('[') {
        Some(ipv6) => ipv6.split(']').next()?,
        None => authority.split(':').next()?,
    };
    (!hostname.is_empty()).then(|| hostname.to_lowercase())
}

/// Redirects come back as responses: ureq would keep custom headers such as `x-api-key` on a cross-host redirect,
/// and `fetch::fetch_http` vets each hop before following it.
pub(super) fn http_agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder().http_status_as_error(false).timeout_global(Some(timeout)).max_redirects(0).build().into()
}

/// Where this run's `Web` keeps its responses; the signal handler removes it, since `Drop` does not run there.
pub fn store_dir() -> PathBuf {
    std::env::temp_dir().join(format!("agent-web-{}", std::process::id()))
}

/// Session store of tool responses: the last 20 as JSON files in `dir`, looked up by responseId.
pub struct Web {
    home: PathBuf,
    config_path: PathBuf,
    dir: PathBuf,
    ids: VecDeque<String>,
}

impl Drop for Web {
    fn drop(&mut self) {
        if !self.ids.is_empty() {
            _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

impl Web {
    pub fn new(home: &Path, dir: PathBuf) -> Web {
        Web { home: home.to_path_buf(), config_path: home.join(".agents/web-search.json"), dir, ids: VecDeque::new() }
    }

    /// The provider order from /settings, read per call like the other settings.
    fn provider_order(&self) -> Result<Vec<&'static str>, String> {
        Ok(crate::settings::load(&self.home)?.sort_providers(&providers::PROVIDERS))
    }

    pub fn run(&mut self, name: &str, input: &Value) -> Result<String, String> {
        match name {
            "WebSearch" => self.web_search(input),
            "WebFetch" => self.fetch_content(input),
            _ => self.get_search_content(input),
        }
    }

    /// Stores `entry` under a fresh responseId, dropping the oldest past the limit.
    fn store(&mut self, mut entry: Value) -> Result<String, String> {
        let id = format!("{}{}", base36(crate::process::now_ms()), crate::api::oauth::random_hex(3));
        entry["id"] = json!(id);
        crate::process::private_dir(&self.dir)?;
        std::fs::write(self.dir.join(format!("{id}.json")), entry.to_string()).map_err(|e| format!("Cannot store web result: {e}"))?;
        self.ids.push_back(id.clone());
        while self.ids.len() > STORED_RESPONSES {
            let old = self.ids.pop_front().expect("ids is over the limit");
            _ = std::fs::remove_file(self.dir.join(format!("{old}.json")));
        }
        Ok(id)
    }

    /// Only ids this session stored are read, so a responseId never names a path.
    fn stored(&self, id: &str) -> Option<Value> {
        if !self.ids.iter().any(|known| known == id) {
            return None;
        }
        let text = std::fs::read_to_string(self.dir.join(format!("{id}.json"))).ok()?;
        serde_json::from_str(&text).ok()
    }

    fn web_search(&mut self, input: &Value) -> Result<String, String> {
        let mut queries = search_queries(input);
        if queries.is_empty() {
            return Err("Error: No query provided. Use 'query' or 'queries' parameter.".into());
        }
        queries.truncate(MAX_QUERIES);
        let config = providers::load_config(&self.config_path, self.provider_order()?)?;
        let domains = string_list(&input["domainFilter"]);
        let include_content = input["includeContent"].as_bool().unwrap_or(false);
        let search = |text: &str| {
            let query = providers::Query {
                text,
                num_results: num_results(&input["numResults"]),
                recency: input["recencyFilter"].as_str().and_then(providers::Recency::parse),
                domains: &domains,
                include_content,
                category: input["category"].as_str(),
            };
            providers::search(&config, &input["provider"], &query)
        };
        let outcomes: Vec<Result<providers::Response, String>> = std::thread::scope(|scope| {
            let handles: Vec<_> = queries.iter().map(|text| scope.spawn(|| search(text))).collect();
            handles.into_iter().map(|handle| handle.join().unwrap_or_else(|_| Err("search thread panicked".into()))).collect()
        });
        let mut stored = Vec::new();
        let mut inline: Vec<Page> = Vec::new();
        let mut urls: Vec<String> = Vec::new();
        for (text, outcome) in queries.iter().zip(outcomes) {
            match outcome {
                Ok(response) => {
                    let hits: Vec<Value> = response.results.iter().map(|h| json!({"title": h.title, "url": h.url, "snippet": h.snippet})).collect();
                    for hit in &response.results {
                        if !urls.contains(&hit.url) {
                            urls.push(hit.url.clone());
                        }
                    }
                    for page in response.inline {
                        if !inline.iter().any(|known| known.url == page.url) {
                            inline.push(page);
                        }
                    }
                    stored.push(json!({"query": text, "answer": response.answer, "results": hits, "providers": response.providers}));
                }
                Err(error) => stored.push(json!({"query": text, "answer": "", "results": [], "providers": [], "error": error})),
            }
        }

        let output = search_presentation(&stored);
        let mut guidance = String::new();
        let missing: Vec<String> = urls.iter().filter(|u| !inline.iter().any(|p| &p.url == *u)).take(MAX_FETCHED_PAGES).cloned().collect();
        if include_content && !missing.is_empty() {
            inline.extend(fetch::fetch_all(&missing, false, false));
        }
        if include_content && !inline.is_empty() {
            let fetch_id = self.store(fetch_entry(&inline))?;
            guidance.push_str(&format!(
                "\n---\nFull content for {} sources is ready as responseId \"{fetch_id}\". Use GetSearchContent({{ responseId: \"{fetch_id}\", urlIndex: 0, offset: 0, limit: {MAX_INLINE_CHARS} }}) to retrieve the first bounded page.",
                inline.iter().filter(|p| p.error.is_none()).count()
            ));
        }
        let count = stored.len();
        let search_id = self.store(json!({"type": "search", "queries": stored}))?;
        let more = if count > 1 { format!("; repeat with queryIndex 1 through {}", count - 1) } else { String::new() };
        guidance.push_str(&format!(
            "\n---\nFull search results are stored as responseId \"{search_id}\". Use GetSearchContent({{ responseId: \"{search_id}\", queryIndex: 0, offset: 0, limit: {MAX_INLINE_CHARS} }}) to retrieve the first bounded page{more}."
        ));
        Ok(bound_presentation(&output, &guidance))
    }

    fn get_search_content(&self, input: &Value) -> Result<String, String> {
        let id = input["responseId"].as_str().unwrap_or_default();
        let find_mode = match &input["findMode"] {
            Value::Null => None,
            value => Some(value.as_str().and_then(Mode::parse).ok_or(r#"findMode must be "exact", "case-insensitive", or "fuzzy""#)?),
        };
        let find_text = match &input["findText"] {
            Value::Null => None,
            Value::String(text) => Some(vec![text.clone()]),
            other => Some(string_list(other)),
        };
        if find_mode.is_some() && find_text.is_none() {
            return Err(format!("findMode {} requires findText; provide findText or omit findMode.", input["findMode"]));
        }
        let Some(data) = self.stored(id) else {
            return Err(format!("Error: No stored results for responseId {}. Use a responseId returned by {STORED_SOURCES}.", json!(id)));
        };
        if find_text.as_ref().is_some_and(|texts| texts.iter().all(|t| t.trim().is_empty())) {
            return Err("findText must contain at least one non-empty string".into());
        }
        let find = |content: &str| find_text.as_ref().map(|texts| find::find(content, texts, find_mode.unwrap_or(Mode::CaseInsensitive)));
        let ask = Request { id, offset: &input["offset"], limit: &input["limit"] };

        match data["type"].as_str() {
            Some("search") => {
                let queries = data["queries"].as_array().cloned().unwrap_or_default();
                let index = select_query(&queries, input, id)?;
                let query = &queries[index];
                if let Some(error) = query["error"].as_str() {
                    return Err(format!(
                        "Error retrieving query {} from responseId {}: {error}. Check the stored search result and retry with another query or queryIndex if needed.",
                        query["query"],
                        json!(id)
                    ));
                }
                let full = full_results(query);
                if let Some(found) = find(&full) {
                    return Ok(found);
                }
                let subject = format!("query {}", query["query"]);
                ask.slice(&full, &subject, &format!(" in responseId {}", json!(id)), |end, limit| format!("responseId: \"{id}\", queryIndex: {index}, offset: {end}, limit: {limit}"))
            }
            _ => {
                let urls = data["urls"].as_array().cloned().unwrap_or_default();
                let index = select_url(&urls, input, id)?;
                let page = &urls[index];
                let (url, content) = (page["url"].as_str().unwrap_or_default(), page["content"].as_str().unwrap_or_default());
                if let Some(error) = page["error"].as_str() {
                    return Err(format!("Error retrieving URL {} from responseId {}: {error}. Check the stored fetch result and retry with another URL or urlIndex if needed.", json!(url), json!(id)));
                }
                let title = page["title"].as_str().filter(|t| !t.is_empty()).unwrap_or(url);
                if let Some(found) = find(content) {
                    return Ok(format!("# {title}\n\n{found}"));
                }
                ask.url_page(content, title, url, index)
            }
        }
    }
}

/// offset and limit of one get_search_content call, still unvalidated.
struct Request<'a> {
    id: &'a str,
    offset: &'a Value,
    limit: &'a Value,
}

impl Request<'_> {
    /// Validated (offset, limit) in chars; `subject` and `inside` name the content in errors.
    fn bounds(&self, length: usize, subject: &str, inside: &str) -> Result<(usize, usize), String> {
        let offset = match self.offset {
            Value::Null => 0,
            value => value.as_u64().ok_or_else(|| format!("Invalid offset: received {value} for {subject}; offset must be a non-negative integer. Use 0 or a larger integer."))? as usize,
        };
        let limit = match self.limit {
            Value::Null => MAX_INLINE_CHARS,
            value => value
                .as_u64()
                .map(|l| l as usize)
                .filter(|l| (1..=MAX_INLINE_CHARS).contains(l))
                .ok_or_else(|| format!("Invalid limit: received {value} for {subject}; limit must be an integer from 1 to {MAX_INLINE_CHARS}. Use a value in that range."))?,
        };
        if offset > length {
            return Err(format!("Offset {offset} is out of range for {subject}{inside}. Received offset {offset}; valid range is 0-{length}. Use an offset within that range."));
        }
        Ok((offset, limit))
    }

    /// pi's sliceWithContinuation: the page shrinks so it fits MAX_INLINE_CHARS together with the next-slice hint.
    fn slice(&self, content: &str, subject: &str, inside: &str, next_args: impl Fn(usize, usize) -> String) -> Result<String, String> {
        let length = content.chars().count();
        let (offset, limit) = self.bounds(length, subject, inside)?;
        let mut end = offset + limit.min(length - offset);
        let mut continuation = String::new();
        while end < length {
            continuation = format!("\n\n---\nShowing chars {offset}-{end} of {length}. Use GetSearchContent({{ {} }}) for the next slice.", next_args(end, limit));
            let overflow = (end - offset + continuation.chars().count()).saturating_sub(MAX_INLINE_CHARS);
            if overflow == 0 {
                break;
            }
            end -= overflow;
        }
        let page = char_slice(content, offset, end);
        Ok(if end < length { format!("{page}{continuation}") } else { page.to_string() })
    }

    fn url_page(&self, content: &str, title: &str, url: &str, index: usize) -> Result<String, String> {
        let length = content.chars().count();
        let (offset, limit) = self.bounds(length, &format!("URL {}", json!(url)), &format!(" in responseId {}", json!(self.id)))?;
        let end = (offset + limit).min(length);
        let mut text = format!("# {title}\n\n{}", char_slice(content, offset, end));
        if end < length || offset > 0 {
            text.push_str(&format!("\n\n---\nShowing chars {offset}-{end} of {length}."));
        }
        if end < length {
            text.push_str(&format!(" Use GetSearchContent({{ responseId: \"{}\", urlIndex: {index}, offset: {end}, limit: {limit} }}) for the next slice.", self.id));
        }
        Ok(text)
    }
}

fn select_query(queries: &[Value], input: &Value, id: &str) -> Result<usize, String> {
    let listed: Vec<String> = queries.iter().enumerate().map(|(i, q)| format!("{i}: {}", q["query"])).collect();
    let listed = if listed.is_empty() { "none".to_string() } else { listed.join(", ") };
    if let Some(query) = input["query"].as_str().filter(|q| !q.is_empty()) {
        let names: Vec<String> = queries.iter().map(|q| q["query"].to_string()).collect();
        let names = if names.is_empty() { "none".to_string() } else { names.join(", ") };
        return queries.iter().position(|q| q["query"] == query).ok_or_else(|| {
            let query = json!(query);
            format!("Query {query} was not found for responseId {}. Received query={query}. Available queries: {names}. Use one of the available queries or queryIndex.", json!(id))
        });
    }
    match &input["queryIndex"] {
        Value::Null => Err(format!("Specify query or queryIndex for responseId {}. Available queries: {listed}.", json!(id))),
        index => index.as_u64().map(|i| i as usize).filter(|i| *i < queries.len()).ok_or_else(|| {
            format!(
                "Query index {index} is out of range for responseId {}. Received queryIndex={index}; valid indexes are 0-{}. Available queries: {listed}. Use one of the available indexes.",
                json!(id),
                queries.len() as i64 - 1
            )
        }),
    }
}

fn select_url(urls: &[Value], input: &Value, id: &str) -> Result<usize, String> {
    let indexed: Vec<String> = urls.iter().enumerate().map(|(i, u)| format!("  {i}: {}", u["url"].as_str().unwrap_or_default())).collect();
    if let Some(url) = input["url"].as_str().filter(|u| !u.is_empty()) {
        return urls.iter().position(|u| u["url"] == url).ok_or_else(|| {
            let known: Vec<&str> = urls.iter().map(|u| u["url"].as_str().unwrap_or_default()).collect();
            let known = if known.is_empty() { "  none".to_string() } else { format!("  {}", known.join("\n  ")) };
            let url = json!(url);
            format!("URL {url} was not found for responseId {}. Received url={url}. Available URLs:\n{known}\nUse one of the available URLs or urlIndex.", json!(id))
        });
    }
    match &input["urlIndex"] {
        Value::Null if urls.len() == 1 => Ok(0),
        Value::Null => Err(format!("Specify url or urlIndex for responseId {}. Available URLs:\n{}", json!(id), indexed.join("\n"))),
        index => index.as_u64().map(|i| i as usize).filter(|i| *i < urls.len()).ok_or_else(|| {
            format!(
                "URL index {index} is out of range for responseId {}. Received urlIndex={index}; valid indexes are 0-{}. Available URLs:\n{}\nUse one of the available indexes.",
                json!(id),
                urls.len() as i64 - 1,
                indexed.join("\n")
            )
        }),
    }
}

/// `query` and `queries`, trimmed; a `query` holding a JSON array of strings expands into several, as in pi.
fn search_queries(input: &Value) -> Vec<String> {
    let mut queries: Vec<String> = string_list(&input["queries"]).into_iter().filter(|q| !q.trim().is_empty()).collect();
    if queries.is_empty()
        && let Some(query) = input["query"].as_str()
    {
        queries = match serde_json::from_str::<Value>(query.trim()) {
            Ok(list @ Value::Array(_)) if list.as_array().is_some_and(|items| items.iter().all(Value::is_string)) => string_list(&list),
            _ => vec![query.to_string()],
        };
    }
    queries.into_iter().map(|q| q.trim().to_string()).filter(|q| !q.is_empty()).collect()
}

fn string_list(value: &Value) -> Vec<String> {
    value.as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect()
}

fn num_results(value: &Value) -> usize {
    value.as_f64().map_or(5, |n| n.floor().clamp(1.0, 20.0) as usize)
}

fn fetch_entry(pages: &[Page]) -> Value {
    let urls: Vec<Value> = pages.iter().map(|p| json!({"url": p.url, "title": p.title, "content": p.content, "error": p.error})).collect();
    json!({"type": "fetch", "urls": urls})
}

fn search_presentation(queries: &[Value]) -> String {
    let providers: Vec<String> = queries.iter().map(|q| string_list(&q["providers"]).join(", ")).map(|p| if p.is_empty() { "unknown".into() } else { p }).collect();
    let mut output = if providers.len() == 1 {
        format!("**Provider:** {}\n\n", providers[0])
    } else {
        let used: Vec<String> = providers.iter().enumerate().map(|(i, p)| format!("Query {}: {p}", i + 1)).collect();
        format!("**Providers used:** {}\n\n", used.join("; "))
    };
    for query in queries {
        if queries.len() > 1 {
            output.push_str(&format!("## Query: \"{}\"\n\n", query["query"].as_str().unwrap_or_default()));
        }
        match query["error"].as_str() {
            Some(error) => output.push_str(&format!("Error: {error}\n\n")),
            None => output.push_str(&format!("{}\n\n", search_summary(query))),
        }
    }
    output.trim().to_string()
}

fn search_summary(query: &Value) -> String {
    let answer = query["answer"].as_str().unwrap_or_default();
    let results = query["results"].as_array().cloned().unwrap_or_default();
    if results.is_empty() {
        return if answer.is_empty() { "No results found.".into() } else { format!("{answer}\n\n---\n\n**Sources:**\nNo sources returned.") };
    }
    let sources: Vec<String> = results.iter().enumerate().map(|(i, r)| format!("{}. {}\n   {}", i + 1, r["title"].as_str().unwrap_or_default(), r["url"].as_str().unwrap_or_default())).collect();
    let header = if answer.is_empty() { String::new() } else { format!("{answer}\n\n---\n\n**Sources:**\n") };
    format!("{header}{}", sources.join("\n\n"))
}

fn full_results(query: &Value) -> String {
    let mut output = format!("## Results for: \"{}\"\n\n", query["query"].as_str().unwrap_or_default());
    let providers = string_list(&query["providers"]);
    if !providers.is_empty() {
        output.push_str(&format!("**Provider{}:** {}\n\n", if providers.len() == 1 { "" } else { "s" }, providers.join(", ")));
    }
    let answer = query["answer"].as_str().unwrap_or_default();
    if !answer.is_empty() {
        output.push_str(&format!("{answer}\n\n---\n\n"));
    }
    for result in query["results"].as_array().into_iter().flatten() {
        output.push_str(&format!("### {}\n{}", result["title"].as_str().unwrap_or_default(), result["url"].as_str().unwrap_or_default()));
        if let Some(snippet) = result["snippet"].as_str().filter(|s| !s.is_empty()) {
            output.push_str(&format!("\n\n{snippet}"));
        }
        output.push_str("\n\n");
    }
    output
}

/// pi's boundSearchPresentation: text plus guidance, or a cut text with the guidance after a truncation marker.
fn bound_presentation(text: &str, guidance: &str) -> String {
    let full = format!("{text}{guidance}");
    if full.chars().count() <= MAX_INLINE_CHARS {
        return full;
    }
    let marker = format!("\n\n---\n[Output truncated.]{guidance}");
    let keep = MAX_INLINE_CHARS.saturating_sub(marker.chars().count());
    format!("{}{marker}", char_slice(text, 0, keep))
}

/// Chars `from..to` of `text`; offsets past the end clamp to it.
fn char_slice(text: &str, from: usize, to: usize) -> &str {
    let byte = |at: usize| text.char_indices().nth(at).map_or(text.len(), |(b, _)| b);
    &text[byte(from)..byte(to.max(from))]
}

fn base36(mut n: u64) -> String {
    let mut digits = Vec::new();
    loop {
        digits.push(char::from_digit((n % 36) as u32, 36).expect("digit is below 36"));
        n /= 36;
        if n == 0 {
            break;
        }
    }
    digits.iter().rev().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn web(name: &str) -> Web {
        let dir = std::env::temp_dir().join(format!("agent-web-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Web::new(&dir, dir.join("store"))
    }

    #[test]
    fn store_keeps_the_last_twenty_and_refuses_unknown_ids() {
        let mut web = web("store");
        let first = web.store(json!({"type": "fetch", "urls": []})).unwrap();
        for _ in 0..STORED_RESPONSES {
            web.store(json!({"type": "fetch", "urls": []})).unwrap();
        }
        assert!(web.stored(&first).is_none());
        assert!(web.stored(web.ids.back().unwrap()).is_some());
        assert!(web.stored("../../etc/passwd").is_none());
    }

    #[test]
    fn get_search_content_pages_a_fetched_url() {
        let mut web = web("page");
        let content = "line\n".repeat(10_000);
        let id = web.store(fetch_entry(&[Page { url: "https://a.dev".into(), title: "A".into(), content, error: None }])).unwrap();
        let page = web.get_search_content(&json!({"responseId": id, "offset": 49_990, "limit": 100})).unwrap();
        assert_eq!(page, "# A\n\nline\nline\n\n\n---\nShowing chars 49990-50000 of 50000.");
        let first = web.get_search_content(&json!({"responseId": id, "limit": 5})).unwrap();
        assert!(first.ends_with(&format!("Showing chars 0-5 of 50000. Use GetSearchContent({{ responseId: \"{id}\", urlIndex: 0, offset: 5, limit: 5 }}) for the next slice.")), "{first}");
        let bad = web.get_search_content(&json!({"responseId": id, "offset": -1})).unwrap_err();
        assert!(bad.starts_with("Invalid offset: received -1 for URL \"https://a.dev\""), "{bad}");
    }

    #[test]
    fn get_search_content_slices_search_results_under_the_cap() {
        let mut web = web("search");
        let hits: Vec<Value> = (0..400).map(|i| json!({"title": format!("T{i}"), "url": format!("https://x.dev/{i}"), "snippet": "s".repeat(100)})).collect();
        let id = web.store(json!({"type": "search", "queries": [{"query": "q", "answer": "", "results": hits, "providers": ["exa"]}]})).unwrap();
        let page = web.get_search_content(&json!({"responseId": id, "queryIndex": 0})).unwrap();
        assert_eq!(page.chars().count(), MAX_INLINE_CHARS);
        assert!(page.contains(&format!("Use GetSearchContent({{ responseId: \"{id}\", queryIndex: 0, offset: ")), "{}", &page[page.len() - 200..]);
        let missing = web.get_search_content(&json!({"responseId": id})).unwrap_err();
        assert_eq!(missing, format!("Specify query or queryIndex for responseId \"{id}\". Available queries: 0: \"q\"."));
    }

    #[test]
    fn search_queries_expand_a_json_array_and_drop_blanks() {
        assert_eq!(search_queries(&json!({"query": r#"["a", " b ", ""]"#})), ["a", "b"]);
        assert_eq!(search_queries(&json!({"queries": [" x ", 3], "query": "ignored"})), ["x"]);
        assert_eq!(search_queries(&json!({"queries": [" "], "query": "rust"})), ["rust"]);
        assert!(search_queries(&json!({})).is_empty());
    }

    #[test]
    fn host_lowercases_and_strips_userinfo_and_port() {
        assert_eq!(host("https://user@Docs.RS:443/x?y").as_deref(), Some("docs.rs"));
        assert_eq!(host("not a url"), None);
    }
}
