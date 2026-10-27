use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::process::now_ms;

// Client ids, endpoints and scopes are pi-ai's (auth/oauth/anthropic.js and openai-codex.js).
const CLAUDE_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const CLAUDE_AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
const CLAUDE_TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const CLAUDE_REDIRECT_URI: &str = "https://platform.claude.com/oauth/code/callback";
const CLAUDE_SCOPES: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";
const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CODEX_AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const CODEX_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const CODEX_REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const CODEX_CALLBACK_ADDRESS: &str = "127.0.0.1:1455";
const CODEX_SCOPES: &str = "openid profile email offline_access";
const CODEX_JWT_CLAIM: &str = "https://api.openai.com/auth";
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);
/// Tokens are refreshed this long before they expire.
pub const REFRESH_MARGIN_MS: u64 = 5 * 60 * 1000;

#[derive(Clone, PartialEq)]
pub struct Tokens {
    pub access: String,
    pub refresh: String,
    /// Unix milliseconds.
    pub expires: u64,
    /// ChatGPT account read from the Codex access token, or the Claude account uuid from the token response.
    pub account_id: Option<String>,
}

/// Redacted, so a failing assert or a `{:?}` in a log never prints a token.
impl std::fmt::Debug for Tokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tokens").field("expires", &self.expires).field("account_id", &self.account_id).finish_non_exhaustive()
    }
}

/// A started login: the URL to open and what the code exchange needs.
pub struct Pending {
    pub url: String,
    verifier: String,
    state: String,
}

pub fn start_claude() -> Pending {
    let (verifier, challenge) = pkce();
    let state = random_hex(32);
    let query = [
        ("code", "true"),
        ("client_id", CLAUDE_CLIENT_ID),
        ("response_type", "code"),
        ("redirect_uri", CLAUDE_REDIRECT_URI),
        ("scope", CLAUDE_SCOPES),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
        ("state", &state),
    ];
    Pending { url: format!("{CLAUDE_AUTHORIZE_URL}?{}", form(&query)), verifier, state }
}

/// Exchanges the `code#state` Anthropic shows after sign-in.
pub fn finish_claude(pending: &Pending, pasted: &str) -> Result<Tokens, String> {
    let (code, state) = pasted.trim().split_once('#').ok_or("Paste the whole code#state Anthropic shows.")?;
    if state != pending.state {
        return Err("OAuth state mismatch. Start /login again.".into());
    }
    if code.is_empty() {
        return Err("Missing authorization code.".into());
    }
    let body = json!({"grant_type": "authorization_code", "client_id": CLAUDE_CLIENT_ID, "code": code, "state": state, "redirect_uri": CLAUDE_REDIRECT_URI, "code_verifier": pending.verifier});
    claude_tokens(post(CLAUDE_TOKEN_URL, "application/json", &body.to_string())?)
}

pub fn refresh_claude(refresh: &str) -> Result<Tokens, String> {
    let body = json!({"grant_type": "refresh_token", "client_id": CLAUDE_CLIENT_ID, "refresh_token": refresh});
    claude_tokens(post(CLAUDE_TOKEN_URL, "application/json", &body.to_string())?)
}

fn claude_tokens(data: Value) -> Result<Tokens, String> {
    let (Some(access), Some(refresh), Some(expires_in)) = (data["access_token"].as_str(), data["refresh_token"].as_str(), data["expires_in"].as_u64()) else {
        return Err("The token response is missing fields.".into());
    };
    let account_id = data["account"]["uuid"].as_str().map(String::from);
    Ok(Tokens { access: access.into(), refresh: refresh.into(), expires: expires_at(expires_in), account_id })
}

/// `fresh` in api/mod.rs applies `REFRESH_MARGIN_MS`, so the stored expiry is the server's.
pub(crate) fn expires_at(expires_in_secs: u64) -> u64 {
    now_ms().saturating_add(expires_in_secs.saturating_mul(1000))
}

pub fn start_codex() -> Pending {
    let (verifier, challenge) = pkce();
    let state = random_hex(16);
    let query = [
        ("response_type", "code"),
        ("client_id", CODEX_CLIENT_ID),
        ("redirect_uri", CODEX_REDIRECT_URI),
        ("scope", CODEX_SCOPES),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
        ("state", &state),
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
        ("originator", "pi"),
    ];
    Pending { url: format!("{CODEX_AUTHORIZE_URL}?{}", form(&query)), verifier, state }
}

/// Waits for the browser to reach the local callback, then exchanges its code. `abort` (Esc) gives up.
pub fn finish_codex(pending: &Pending, abort: &AtomicBool) -> Result<Tokens, String> {
    let listener = TcpListener::bind(CODEX_CALLBACK_ADDRESS).map_err(|e| format!("Cannot listen on {CODEX_CALLBACK_ADDRESS} ({e}). Close the Codex CLI login and try again."))?;
    let code = callback_code(&listener, "/auth/callback?", &pending.state, abort)?;
    let body = form(&[("grant_type", "authorization_code"), ("client_id", CODEX_CLIENT_ID), ("code", &code), ("code_verifier", &pending.verifier), ("redirect_uri", CODEX_REDIRECT_URI)]);
    codex_tokens(post(CODEX_TOKEN_URL, "application/x-www-form-urlencoded", &body)?)
}

pub fn refresh_codex(refresh: &str) -> Result<Tokens, String> {
    let body = form(&[("grant_type", "refresh_token"), ("refresh_token", refresh), ("client_id", CODEX_CLIENT_ID)]);
    codex_tokens(post(CODEX_TOKEN_URL, "application/x-www-form-urlencoded", &body)?)
}

/// Waits on `listener` for the browser's redirect to `path` (ending in `?`) and returns its `code`.
pub fn callback_code(listener: &TcpListener, path: &str, state: &str, abort: &AtomicBool) -> Result<String, String> {
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let deadline = Instant::now() + LOGIN_TIMEOUT;
    loop {
        if abort.load(Ordering::Relaxed) {
            return Err("Login cancelled.".into());
        }
        if Instant::now() > deadline {
            return Err("Login timed out.".into());
        }
        let Ok((stream, _)) = listener.accept() else {
            std::thread::sleep(Duration::from_millis(100));
            continue;
        };
        stream.set_nonblocking(false).map_err(|e| e.to_string())?;
        // A connection that sends nothing must not hold the login past abort or its deadline.
        stream.set_read_timeout(Some(Duration::from_secs(5))).map_err(|e| e.to_string())?;
        let mut stream = stream;
        let mut line = String::new();
        if BufReader::new((&stream).take(8192)).read_line(&mut line).is_err() {
            continue;
        }
        let target = line.split_whitespace().nth(1).unwrap_or_default();
        let Some(query) = target.strip_prefix(path) else {
            let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n");
            continue;
        };
        let params = parse_query(query);
        let param = |key: &str| params.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
        // Any local page can request this URL, so only the redirect carrying our state ends the login.
        if param("state").as_deref() != Some(state) {
            let _ = stream.write_all(b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\n\r\n");
            continue;
        }
        let page = "<!doctype html><p>Login finished. You can close this tab.</p>";
        let _ = write!(stream, "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: {}\r\n\r\n{page}", page.len());
        if let Some(error) = param("error") {
            let shown: String = error.chars().filter(|c| !c.is_control()).take(200).collect();
            return Err(format!("Login failed: {shown}. Start /login again."));
        }
        return param("code").ok_or_else(|| "Missing authorization code.".into());
    }
}

fn codex_tokens(data: Value) -> Result<Tokens, String> {
    let (Some(access), Some(refresh), Some(expires_in)) = (data["access_token"].as_str(), data["refresh_token"].as_str(), data["expires_in"].as_u64()) else {
        return Err("The token response is missing fields.".into());
    };
    let account_id = codex_account_id(access).ok_or("The token has no ChatGPT account id.")?;
    Ok(Tokens { access: access.into(), refresh: refresh.into(), expires: expires_at(expires_in), account_id: Some(account_id) })
}

/// The ChatGPT account id from the access token's JWT payload.
fn codex_account_id(access: &str) -> Option<String> {
    let payload = access.split('.').nth(1)?;
    let claims: Value = serde_json::from_slice(&base64url_decode(payload)?).ok()?;
    claims[CODEX_JWT_CLAIM]["chatgpt_account_id"].as_str().filter(|id| !id.is_empty()).map(String::from)
}

pub fn open_browser(url: &str) {
    // MCP servers supply the authorization URL; a file:// or app scheme would open something local.
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return;
    }
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    // Reaped on a thread, not waited for: xdg-open runs a browser without a desktop handler in the foreground.
    if let Ok(mut child) = crate::process::command(opener).arg(url).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn() {
        std::thread::spawn(move || child.wait());
    }
}

/// POSTs to a token or client registration endpoint. The error carries the status and body, never the request,
/// which holds the secrets.
pub fn post(url: &str, content_type: &str, body: &str) -> Result<Value, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).timeout_global(Some(Duration::from_secs(30))).build().into();
    let mut response = agent.post(url).header("content-type", content_type).header("accept", "application/json").send(body).map_err(|e| format!("Token request failed: {e}."))?;
    let status = response.status();
    let text = response.body_mut().read_to_string().map_err(|e| format!("Token response unreadable: {e}."))?;
    if !status.is_success() {
        let body: Value = serde_json::from_str(&text).unwrap_or_default();
        // The body comes from a third-party server and ends up on the terminal, so escapes are dropped.
        let shown: String = body["error_description"].as_str().unwrap_or(&text).trim().chars().filter(|c| !c.is_control()).take(500).collect();
        return Err(format!("Token request failed ({}): {shown}.", status.as_u16()));
    }
    serde_json::from_str(&text).map_err(|e| format!("Token response is not JSON: {e}."))
}

/// RFC 7636 S256: a random 32-byte verifier and the base64url SHA-256 of it.
pub fn pkce() -> (String, String) {
    let verifier = base64url_encode(&random_bytes::<32>());
    let challenge = challenge(&verifier);
    (verifier, challenge)
}

fn challenge(verifier: &str) -> String {
    base64url_encode(ring::digest::digest(&ring::digest::SHA256, verifier.as_bytes()).as_ref())
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut bytes).expect("the system random source works");
    bytes
}

pub fn random_hex(len: usize) -> String {
    random_bytes::<32>()[..len].iter().map(|b| format!("{b:02x}")).collect()
}

const BASE64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn base64url_encode(bytes: &[u8]) -> String {
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | (*b as u32) << (16 - 8 * i));
        for i in 0..=chunk.len() {
            out.push(BASE64URL[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

fn base64url_decode(text: &str) -> Option<Vec<u8>> {
    let mut bits = 0u32;
    let mut count = 0;
    let mut out = Vec::new();
    for ch in text.trim_end_matches('=').bytes() {
        let ch = match ch {
            b'+' => b'-',
            b'/' => b'_',
            other => other,
        };
        bits = bits << 6 | BASE64URL.iter().position(|c| *c == ch)? as u32;
        count += 6;
        if count >= 8 {
            count -= 8;
            out.push((bits >> count) as u8);
        }
    }
    Some(out)
}

/// `application/x-www-form-urlencoded`, also used for query strings.
pub fn form(pairs: &[(&str, &str)]) -> String {
    pairs.iter().map(|(key, value)| format!("{}={}", percent_encode(key), percent_encode(value))).collect::<Vec<_>>().join("&")
}

/// Every byte but RFC 3986 unreserved characters as `%XX`, like `encodeURIComponent`.
pub fn percent_encode(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn parse_query(query: &str) -> Vec<(String, String)> {
    query.split('&').filter_map(|pair| pair.split_once('=')).map(|(k, v)| (form_decode(k), form_decode(v))).collect()
}

/// `+` is a space and `%XY` a byte; a `%` without two hex digits after it stays as it is.
fn form_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(byte) = hex_byte(&bytes[i + 1..])
        {
            out.push(byte);
            i += 3;
        } else {
            out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The byte the first two hex digits of `pair` spell. Not `from_str_radix`, which also takes a sign: `+1`.
pub(crate) fn hex_byte(pair: &[u8]) -> Option<u8> {
    let digit = |index: usize| char::from(*pair.get(index)?).to_digit(16);
    Some((digit(0)? * 16 + digit(1)?) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pasted_code_without_its_state_is_refused() {
        let pending = Pending { url: String::new(), verifier: "v".into(), state: "s".into() };
        assert_eq!(finish_claude(&pending, "abc").unwrap_err(), "Paste the whole code#state Anthropic shows.");
        assert_eq!(finish_claude(&pending, "abc#other").unwrap_err(), "OAuth state mismatch. Start /login again.");
    }

    #[test]
    fn pkce_matches_the_rfc_7636_vector() {
        assert_eq!(challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
        assert_eq!(base64url_encode(b"f"), "Zg");
        assert_eq!(base64url_encode(b"foob"), "Zm9vYg");
        assert_eq!(base64url_decode("Zm9vYmE").unwrap(), b"fooba");
        let (verifier, challenge_text) = pkce();
        assert_eq!((verifier.len(), challenge_text), (43, challenge(&verifier)));
    }

    #[test]
    fn codex_account_id_comes_from_the_jwt_claim() {
        let claims = json!({"https://api.openai.com/auth": {"chatgpt_account_id": "acc-1"}});
        let token = format!("e30.{}.sig", base64url_encode(claims.to_string().as_bytes()));
        assert_eq!(codex_account_id(&token).as_deref(), Some("acc-1"));
        assert_eq!(codex_account_id("not-a-jwt"), None);
        assert_eq!(parse_query("code=a%2Fb+c&state=x"), [("code".to_string(), "a/b c".to_string()), ("state".to_string(), "x".to_string())]);
        assert_eq!(parse_query("code=%+1%4"), [("code".to_string(), "% 1%4".to_string())]);
        assert_eq!(form(&[("scope", "a b"), ("u", "http://x/y")]), "scope=a%20b&u=http%3A%2F%2Fx%2Fy");
    }

    /// Answers one HTTP request on a local port with `response` and returns the port's URL.
    fn serve_once(response: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_request(&mut BufReader::new(&stream));
            stream.write_all(response.as_bytes()).unwrap();
        });
        url
    }

    /// Reads the whole request: closing a socket with its body unread resets it, and macOS then fails the client with EINVAL.
    fn read_request(reader: &mut impl BufRead) {
        let mut length = 0;
        let mut header = String::new();
        while reader.read_line(&mut header).unwrap() > 2 {
            if let Some(value) = header.to_lowercase().strip_prefix("content-length:") {
                length = value.trim().parse().unwrap();
            }
            header.clear();
        }
        reader.read_exact(&mut vec![0; length]).unwrap();
    }

    #[test]
    fn post_accepts_201_created_from_client_registration() {
        let url = serve_once("HTTP/1.1 201 Created\r\ncontent-length: 17\r\n\r\n{\"client_id\":\"c\"}");
        assert_eq!(post(&url, "application/json", "{}").unwrap(), json!({"client_id": "c"}));
    }

    #[test]
    fn a_failed_post_reports_the_body_without_terminal_escapes() {
        let url = serve_once("HTTP/1.1 400 Bad Request\r\ncontent-length: 14\r\n\r\nbad\x1b]0;x\x07grant");
        assert_eq!(post(&url, "application/json", "{}").unwrap_err(), "Token request failed (400): bad]0;xgrant.");
    }

    #[test]
    fn a_failed_post_reports_the_oauth_error_description() {
        let body = r#"{"error": "invalid_grant", "error_description": "Refresh token not found or invalid"}"#;
        let url = serve_once(format!("HTTP/1.1 400 Bad Request\r\ncontent-length: {}\r\n\r\n{body}", body.len()).leak());
        assert_eq!(post(&url, "application/json", "{}").unwrap_err(), "Token request failed (400): Refresh token not found or invalid.");
    }

    #[test]
    fn the_callback_ignores_a_request_without_the_state_and_reports_an_oauth_error() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let browser = std::thread::spawn(move || {
            let mut answers = Vec::new();
            for target in ["/cb?code=forged", "/cb?error=access_denied&state=s"] {
                let mut stream = std::net::TcpStream::connect(address).unwrap();
                write!(stream, "GET {target} HTTP/1.1\r\n\r\n").unwrap();
                let mut answer = String::new();
                let _ = stream.read_to_string(&mut answer);
                answers.push(answer.lines().next().unwrap_or_default().to_string());
            }
            answers
        });
        let result = callback_code(&listener, "/cb?", "s", &AtomicBool::new(false));
        assert_eq!(result.unwrap_err(), "Login failed: access_denied. Start /login again.");
        assert_eq!(browser.join().unwrap(), ["HTTP/1.1 400 Bad Request", "HTTP/1.1 200 OK"]);
    }
}
