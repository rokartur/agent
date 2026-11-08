//! OAuth for HTTP servers, per the MCP authorization spec: metadata discovery, dynamic client
//! registration, PKCE through the browser, and refresh. Tokens live in `~/.agents/mcp-auth.json`.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use serde_json::{Map, Value, json};

use crate::api::oauth::{REFRESH_MARGIN_MS, callback_code, form, open_browser, pkce, post, random_hex};
use crate::process::now_ms;

/// `l` in /mcp exists only in the interactive agent, not in `--print`, `--headless` or subagent runs.
const LOG_IN_AGAIN: &str = "Open /mcp in the interactive agent and press l to log in again.";

/// The access token stored for `url`, refreshed first when it is about to expire or when it is
/// the `rejected` token a server just answered 401 to.
pub fn bearer(home: &Path, url: &str, rejected: Option<&str>) -> Result<Option<String>, String> {
    let fresh = |entry: &Value| {
        let expiring = entry["expires"].as_u64().is_some_and(|expires| now_ms() + REFRESH_MARGIN_MS >= expires);
        !expiring && (rejected.is_none() || entry["access"].as_str() != rejected)
    };
    let Some(entry) = read(home)?.remove(url) else { return Ok(None) };
    if fresh(&entry) {
        return Ok(entry["access"].as_str().map(String::from));
    }
    update(home, |store| {
        let Some(entry) = store.get(url).cloned() else { return Ok(None) };
        // Another agent may have refreshed it while this one waited for the lock.
        if fresh(&entry) {
            return Ok(entry["access"].as_str().map(String::from));
        }
        let Some(refresh) = entry["refresh"].as_str() else { return Ok(entry["access"].as_str().map(String::from)) };
        let client_id = entry["client_id"].as_str().unwrap_or_default();
        let mut fields = vec![("grant_type", "refresh_token"), ("refresh_token", refresh), ("client_id", client_id), ("resource", url)];
        if let Some(secret) = entry["client_secret"].as_str() {
            fields.push(("client_secret", secret));
        }
        let endpoint = entry["token_endpoint"].as_str().ok_or(format!("The stored login has no token endpoint. {LOG_IN_AGAIN}"))?;
        let answer = post(endpoint, "application/x-www-form-urlencoded", &form(&fields)).map_err(|e| format!("Refreshing the login failed: {e} {LOG_IN_AGAIN}"))?;
        let updated = with_tokens(entry, &answer)?;
        let access = updated["access"].as_str().map(String::from);
        store.insert(url.into(), updated);
        Ok(access)
    })
}

/// Whether `url` has a login in `store` that still works or can be refreshed.
pub fn logged_in(store: &Map<String, Value>, url: &str) -> bool {
    let usable = |entry: &Value| entry["refresh"].is_string() || entry["expires"].as_u64().is_none_or(|expires| now_ms() < expires);
    store.get(url).is_some_and(usable)
}

/// Runs the browser login for the server at `url` and stores its tokens.
pub fn login(home: &Path, url: &str, abort: &AtomicBool) -> Result<(), String> {
    let www_authenticate = challenge(url);
    // MCP 2025-06-18: the 401's `resource_metadata` comes first, then the well-known locations.
    let resource_urls = auth_param(&www_authenticate, "resource_metadata").map_or_else(|| metadata_urls(url, "oauth-protected-resource"), |found| vec![found]);
    let resource = fetch_first(&resource_urls);
    // Without resource metadata the 2025-03-26 rule applies: the server's origin is the issuer.
    let issuer = resource.as_ref().and_then(|meta| meta["authorization_servers"][0].as_str()).unwrap_or(origin(url)).to_string();
    let endpoints = Endpoints::new(fetch_first(&server_metadata_urls(&issuer)), &issuer);
    endpoints.check()?;

    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| format!("Cannot listen for the login callback: {e}"))?;
    let redirect = format!("http://127.0.0.1:{}/callback", listener.local_addr().map_err(|e| e.to_string())?.port());
    let client = register(&endpoints, &redirect)?;
    let (verifier, challenge) = pkce();
    let state = random_hex(16);
    let scope = auth_param(&www_authenticate, "scope").or_else(|| resource.as_ref().map(scopes)).unwrap_or_default();
    let client_id = client["client_id"].as_str().expect("register stores client_id");
    open_browser(&authorize_url(
        &endpoints.authorization,
        &[("client_id", client_id), ("redirect_uri", &redirect), ("code_challenge", &challenge), ("state", &state), ("resource", url), ("scope", &scope)],
    ));
    let code = callback_code(&listener, "/callback?", &state, abort)?;

    let mut fields = vec![("grant_type", "authorization_code"), ("code", &code), ("redirect_uri", &redirect), ("client_id", client_id), ("code_verifier", &verifier), ("resource", url)];
    if let Some(secret) = client["client_secret"].as_str() {
        fields.push(("client_secret", secret));
    }
    let answer = post(&endpoints.token, "application/x-www-form-urlencoded", &form(&fields))?;
    let mut entry = client.clone();
    entry["token_endpoint"] = json!(endpoints.token);
    let entry = with_tokens(entry, &answer)?;
    update(home, |store| {
        store.insert(url.into(), entry);
        Ok(())
    })
}

/// A PKCE authorization request; an empty value (no scopes) is left out.
fn authorize_url(endpoint: &str, fields: &[(&str, &str)]) -> String {
    let mut query = vec![("response_type", "code"), ("code_challenge_method", "S256")];
    query.extend(fields.iter().filter(|(_, value)| !value.is_empty()));
    let joiner = if endpoint.contains('?') { '&' } else { '?' };
    format!("{endpoint}{joiner}{}", form(&query))
}

#[derive(Debug, PartialEq)]
struct Endpoints {
    authorization: String,
    token: String,
    registration: Option<String>,
}

impl Endpoints {
    /// From authorization server metadata, or the spec's default paths on the issuer's origin.
    /// Metadata for another issuer is ignored (RFC 8414 §3.3).
    fn new(metadata: Option<Value>, issuer: &str) -> Endpoints {
        let same_issuer = |meta: &Value| meta["issuer"].as_str().is_some_and(|found| found.trim_end_matches('/') == issuer.trim_end_matches('/'));
        let metadata = metadata.filter(same_issuer);
        let origin = origin(issuer);
        let field = |key: &str| metadata.as_ref().and_then(|meta| meta[key].as_str()).map(String::from);
        Endpoints {
            authorization: field("authorization_endpoint").unwrap_or_else(|| format!("{origin}/authorize")),
            token: field("token_endpoint").unwrap_or_else(|| format!("{origin}/token")),
            registration: if metadata.is_some() { field("registration_endpoint") } else { Some(format!("{origin}/register")) },
        }
    }

    /// The MCP authorization spec wants https; a loopback server is a local test or a tunnel.
    fn check(&self) -> Result<(), String> {
        let loopback = |authority: &str| {
            let port_only = |rest: &str| rest.is_empty() || rest.strip_prefix(':').is_some_and(|port| port.bytes().all(|byte| byte.is_ascii_digit()));
            ["127.0.0.1", "localhost", "[::1]"].iter().any(|host| authority.strip_prefix(host).is_some_and(port_only))
        };
        let secure = |url: &str| url.starts_with("https://") || origin(url).strip_prefix("http://").is_some_and(loopback);
        let mut urls = [Some(&self.authorization), Some(&self.token), self.registration.as_ref()].into_iter().flatten();
        if urls.all(|url| secure(url)) {
            return Ok(());
        }
        Err("The server's login endpoints must use https.".into())
    }
}

/// A client for this agent: `client_id` and maybe `client_secret`.
fn register(endpoints: &Endpoints, redirect: &str) -> Result<Value, String> {
    let Some(registration) = &endpoints.registration else {
        return Err("The server does not offer client registration, so the agent cannot log in to it.".into());
    };
    let request = json!({
        "client_name": "agent",
        "redirect_uris": [redirect],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    });
    let client = post(registration, "application/json", &request.to_string()).map_err(|e| format!("Client registration failed: {e}"))?;
    let mut kept = json!({"client_id": client["client_id"].as_str().ok_or("Client registration returned no client_id.")?});
    if let Some(secret) = client["client_secret"].as_str() {
        kept["client_secret"] = json!(secret);
    }
    Ok(kept)
}

fn with_tokens(mut entry: Value, answer: &Value) -> Result<Value, String> {
    entry["access"] = json!(answer["access_token"].as_str().ok_or("The token answer has no access_token.")?);
    if let Some(refresh) = answer["refresh_token"].as_str() {
        entry["refresh"] = json!(refresh);
    }
    let expires_in = answer["expires_in"].as_u64().or_else(|| answer["expires_in"].as_str()?.parse().ok());
    entry["expires"] = expires_in.map_or(Value::Null, |seconds| json!(crate::api::oauth::expires_at(seconds)));
    Ok(entry)
}

fn scopes(resource: &Value) -> String {
    let scopes = resource["scopes_supported"].as_array().into_iter().flatten().filter_map(Value::as_str);
    scopes.collect::<Vec<_>>().join(" ")
}

/// RFC 8414 first, then OpenID Connect, whose Discovery §4 also appends the segment to the issuer.
fn server_metadata_urls(issuer: &str) -> Vec<String> {
    let mut urls = metadata_urls(issuer, "oauth-authorization-server");
    urls.extend(metadata_urls(issuer, "openid-configuration"));
    urls.push(format!("{}/.well-known/openid-configuration", issuer.trim_end_matches('/')));
    urls.dedup();
    urls
}
/// RFC 8414 and 9728 put the well-known segment between the origin and the path; the root comes last.
fn metadata_urls(url: &str, kind: &str) -> Vec<String> {
    let origin = origin(url);
    let path = url[origin.len()..].split(['?', '#']).next().unwrap_or_default().trim_end_matches('/');
    let mut urls = vec![format!("{origin}/.well-known/{kind}{path}")];
    if !path.is_empty() {
        urls.push(format!("{origin}/.well-known/{kind}"));
    }
    urls
}

pub(super) fn origin(url: &str) -> &str {
    let after_scheme = url.find("://").map_or(0, |at| at + 3);
    let end = url[after_scheme..].find(['/', '?', '#']).map_or(url.len(), |at| after_scheme + at);
    &url[..end]
}

/// The `WWW-Authenticate` header of an unauthenticated request to the server, or "".
fn challenge(url: &str) -> String {
    let response = http().post(url).header("content-type", "application/json").header("accept", "application/json, text/event-stream").send("{}");
    let header = response.ok().and_then(|response| Some(response.headers().get("www-authenticate")?.to_str().ok()?.to_string()));
    header.unwrap_or_default()
}

/// One `name=value` or `name="value"` parameter of a `WWW-Authenticate` header.
fn auth_param(header: &str, name: &str) -> Option<String> {
    let key = format!("{name}=");
    let (at, _) = header.match_indices(&key).find(|(at, _)| header[..*at].ends_with([' ', ',']))?;
    let rest = &header[at + key.len()..];
    let value = match rest.strip_prefix('"') {
        Some(quoted) => quoted.split('"').next(),
        None => rest.split([',', ' ']).next(),
    };
    value.map(String::from)
}

/// The first of `urls` that answers 200 with JSON.
fn fetch_first(urls: &[String]) -> Option<Value> {
    let agent = http();
    urls.iter().find_map(|url| {
        let mut response = agent.get(url).header("accept", "application/json").call().ok()?;
        if response.status().as_u16() != 200 {
            return None;
        }
        serde_json::from_str(&response.body_mut().read_to_string().ok()?).ok()
    })
}

fn http() -> ureq::Agent {
    ureq::Agent::config_builder().http_status_as_error(false).timeout_global(Some(Duration::from_secs(30))).build().into()
}

fn path(home: &Path) -> PathBuf {
    home.join(".agents/mcp-auth.json")
}

pub fn read(home: &Path) -> Result<Map<String, Value>, String> {
    let path = path(home);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(e) => return Err(format!("Cannot read {}: {e}", path.display())),
    };
    serde_json::from_str(&text).map_err(|e| format!("{} is broken: {e}. Fix or remove it by hand.", path.display()))
}

/// Read, change and save under a lock on `mcp-auth.json.lock`, so parallel agents never spend one
/// refresh token twice or write over each other's logins.
fn update<T>(home: &Path, change: impl FnOnce(&mut Map<String, Value>) -> Result<T, String>) -> Result<T, String> {
    crate::api::accounts::locked(&path(home), || {
        let mut store = read(home)?;
        let result = change(&mut store)?;
        crate::api::accounts::write_private(&path(home), &serde_json::to_string_pretty(&store).unwrap())?;
        Ok(result)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_looks_under_the_path_then_at_the_root() {
        assert_eq!(
            metadata_urls("https://mcp.example.com/v1/mcp/?x=1", "oauth-protected-resource"),
            ["https://mcp.example.com/.well-known/oauth-protected-resource/v1/mcp", "https://mcp.example.com/.well-known/oauth-protected-resource"]
        );
        assert_eq!(metadata_urls("https://auth.example.com", "oauth-authorization-server"), ["https://auth.example.com/.well-known/oauth-authorization-server"]);
        assert_eq!(server_metadata_urls("https://kc.example/realms/x").last().unwrap(), "https://kc.example/realms/x/.well-known/openid-configuration");
    }

    #[test]
    fn endpoints_come_from_metadata_or_default_paths() {
        let metadata = json!({"issuer": "https://a/tenant", "authorization_endpoint": "https://a/auth", "token_endpoint": "https://a/tok"});
        let found = Endpoints::new(Some(metadata), "https://a/tenant");
        assert_eq!(found, Endpoints { authorization: "https://a/auth".into(), token: "https://a/tok".into(), registration: None });
        let defaults = Endpoints::new(None, "https://a/tenant");
        let expected = Endpoints { authorization: "https://a/authorize".into(), token: "https://a/token".into(), registration: Some("https://a/register".into()) };
        assert_eq!(defaults, expected);
        assert_eq!(scopes(&json!({"scopes_supported": ["read", "write"]})), "read write");
    }

    #[test]
    fn the_authorization_url_carries_pkce_and_the_resource() {
        let url = authorize_url("https://a/authorize", &[("client_id", "c1"), ("redirect_uri", "http://127.0.0.1:5/callback"), ("resource", "https://x/mcp"), ("scope", "")]);
        let expected = "https://a/authorize?response_type=code&code_challenge_method=S256&client_id=c1&redirect_uri=http%3A%2F%2F127.0.0.1%3A5%2Fcallback&resource=https%3A%2F%2Fx%2Fmcp";
        assert_eq!(url, expected);
        assert!(authorize_url("https://a/authorize?tenant=t", &[]).starts_with("https://a/authorize?tenant=t&response_type=code"));
    }

    #[test]
    fn the_challenge_names_the_resource_metadata_and_scope() {
        let header = r#"Bearer error="invalid_token", resource_metadata="https://x/.well-known/oauth-protected-resource", scope="read write""#;
        assert_eq!(auth_param(header, "resource_metadata").as_deref(), Some("https://x/.well-known/oauth-protected-resource"));
        assert_eq!(auth_param(header, "scope").as_deref(), Some("read write"));
        assert_eq!(auth_param("Bearer realm=x, error=y", "error").as_deref(), Some("y"));
        assert_eq!(auth_param(header, "metadata"), None);
    }

    #[test]
    fn login_endpoints_must_be_https_or_loopback() {
        let endpoints = |token: &str| Endpoints { authorization: "https://a/auth".into(), token: token.into(), registration: None };
        assert!(endpoints("https://a/token").check().is_ok());
        assert!(endpoints("http://127.0.0.1:8080/token").check().is_ok());
        assert!(endpoints("http://localhost/token").check().is_ok());
        assert!(endpoints("http://a/token").check().is_err());
        assert!(endpoints("http://127.0.0.1.evil.com/token").check().is_err());
        assert!(endpoints("http://localhost:80@evil.com/token").check().is_err());
    }

    #[test]
    fn metadata_for_another_issuer_is_ignored() {
        let issued = |issuer: &str| Some(json!({"issuer": issuer, "token_endpoint": "https://evil/token"}));
        assert_eq!(Endpoints::new(issued("https://evil"), "https://a").token, "https://a/token");
        assert_eq!(Endpoints::new(issued("https://a/"), "https://a").token, "https://evil/token");
    }

    #[test]
    fn an_expiring_token_is_refreshed_and_stored() {
        let (endpoint, server) = token_server();
        let home = std::env::temp_dir().join(format!("agent-mcp-auth-refresh-{}", std::process::id()));
        store_login(&home, json!({"access": "old", "refresh": "r1", "expires": now_ms(), "client_id": "c", "token_endpoint": endpoint}));
        let token = bearer(&home, "https://x/mcp", None);
        let stored = read(&home).unwrap()["https://x/mcp"].clone();
        std::fs::remove_dir_all(&home).unwrap();
        assert_eq!(token.unwrap().as_deref(), Some("new"));
        assert!(server.join().unwrap().contains("grant_type=refresh_token&refresh_token=r1"));
        assert_eq!(stored["refresh"], "r2");
        assert!(stored["expires"].as_u64().is_some_and(|expires| expires > now_ms() + 3_000_000));
    }

    #[test]
    fn a_rejected_token_is_refreshed_before_it_expires() {
        let (endpoint, server) = token_server();
        let home = std::env::temp_dir().join(format!("agent-mcp-auth-rejected-{}", std::process::id()));
        store_login(&home, json!({"access": "old", "refresh": "r1", "client_id": "c", "token_endpoint": endpoint}));
        let kept = bearer(&home, "https://x/mcp", Some("older"));
        let refreshed = bearer(&home, "https://x/mcp", Some("old"));
        std::fs::remove_dir_all(&home).unwrap();
        assert_eq!(kept.unwrap().as_deref(), Some("old"));
        assert_eq!(refreshed.unwrap().as_deref(), Some("new"));
        server.join().unwrap();
    }

    fn store_login(home: &Path, entry: Value) {
        update(home, |store| {
            store.insert("https://x/mcp".into(), entry);
            Ok(())
        })
        .unwrap();
    }

    /// Answers one refresh request for `https://x/mcp` with new tokens and returns the request.
    fn token_server() -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/token", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            while !String::from_utf8_lossy(&request).contains("resource=https%3A%2F%2Fx%2Fmcp") {
                let mut chunk = [0; 4096];
                let length = std::io::Read::read(&mut stream, &mut chunk).unwrap();
                assert!(length > 0, "the request ended early");
                request.extend_from_slice(&chunk[..length]);
            }
            let body = "{\"access_token\":\"new\",\"refresh_token\":\"r2\",\"expires_in\":\"3600\"}";
            std::io::Write::write_all(&mut stream, format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{body}", body.len()).as_bytes()).unwrap();
            String::from_utf8_lossy(&request).into_owned()
        });
        (endpoint, server)
    }

    #[test]
    #[cfg(unix)]
    fn a_fresh_token_is_used_without_refreshing() {
        let home = std::env::temp_dir().join(format!("agent-mcp-auth-{}", std::process::id()));
        let mut store = Map::new();
        store.insert("https://x/mcp".into(), json!({"access": "tok", "refresh": "r", "expires": now_ms() + 3_600_000, "token_endpoint": "http://127.0.0.1:9/"}));
        store.insert("https://z/mcp".into(), json!({"access": "gone", "expires": now_ms() - 1}));
        update(&home, |stored| {
            *stored = store;
            Ok(())
        })
        .unwrap();
        let mode = std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(path(&home)).unwrap().permissions());
        let token = bearer(&home, "https://x/mcp", None);
        let other = bearer(&home, "https://y/mcp", None);
        assert!(logged_in(&read(&home).unwrap(), "https://x/mcp"));
        assert!(!logged_in(&read(&home).unwrap(), "https://z/mcp"));
        std::fs::remove_dir_all(&home).unwrap();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(token.unwrap().as_deref(), Some("tok"));
        assert_eq!(other.unwrap(), None);
    }

    #[test]
    fn a_broken_store_is_reported_and_kept() {
        let home = std::env::temp_dir().join(format!("agent-mcp-auth-broken-{}", std::process::id()));
        std::fs::create_dir_all(home.join(".agents")).unwrap();
        std::fs::write(path(&home), "{\"https://x/mcp\": {\"acc").unwrap();
        let token = bearer(&home, "https://x/mcp", None);
        let saved = update(&home, |store| Ok(store.insert("https://y/mcp".into(), json!({}))));
        let text = std::fs::read_to_string(path(&home)).unwrap();
        std::fs::remove_dir_all(&home).unwrap();
        assert!(token.unwrap_err().contains("mcp-auth.json is broken"));
        assert!(saved.is_err());
        assert_eq!(text, "{\"https://x/mcp\": {\"acc");
    }
}
