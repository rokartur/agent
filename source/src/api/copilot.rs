//! GitHub Copilot: device-flow login, the short-lived Copilot token, its model list and request headers.
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::models::Model;
use super::oauth::{self, Tokens};

pub const PROVIDER: &str = "github-copilot";
/// The VS Code Copilot Chat app, the client id pi and opencode sign in with.
const CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
const DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
const ACCESS_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
const COPILOT_TOKEN_URL: &str = "https://api.github.com/copilot_internal/v2/token";
const DEFAULT_BASE: &str = "https://api.individual.githubcopilot.com";
/// Copilot only serves editors it knows.
pub const HEADERS: [(&str, &str); 5] = [
    ("user-agent", "GitHubCopilotChat/0.35.0"),
    ("editor-version", "vscode/1.107.0"),
    ("editor-plugin-version", "copilot-chat/0.35.0"),
    ("copilot-integration-id", "vscode-chat"),
    ("x-github-api-version", "2026-06-01"),
];

/// A started device login: the user enters `user_code` at `verification_uri`.
pub struct Device {
    pub user_code: String,
    pub verification_uri: String,
    device_code: String,
    interval: u64,
    expires_in: u64,
}

pub fn start() -> Result<Device, String> {
    let data = oauth::post(DEVICE_CODE_URL, "application/x-www-form-urlencoded", &oauth::form(&[("client_id", CLIENT_ID), ("scope", "read:user")]))?;
    let field = |key: &str| data[key].as_str().map(String::from).ok_or("The device code response is missing fields.".to_string());
    let verification_uri = field("verification_uri")?;
    // Opened in the browser and shown on the terminal, so only a web page passes.
    if !verification_uri.starts_with("https://") {
        return Err("The device code response has no https sign-in page.".into());
    }
    Ok(Device {
        user_code: field("user_code")?,
        verification_uri,
        device_code: field("device_code")?,
        interval: data["interval"].as_u64().unwrap_or(5),
        expires_in: data["expires_in"].as_u64().unwrap_or(900),
    })
}

/// Polls until the user approves the device, then trades the GitHub token for a Copilot one.
pub fn finish(device: &Device, abort: &AtomicBool) -> Result<Tokens, String> {
    let deadline = Instant::now() + Duration::from_secs(device.expires_in);
    let mut interval = device.interval;
    let body = oauth::form(&[("client_id", CLIENT_ID), ("device_code", &device.device_code), ("grant_type", "urn:ietf:params:oauth:grant-type:device_code")]);
    loop {
        let wake = Instant::now() + Duration::from_secs(interval);
        while Instant::now() < wake {
            if abort.load(Ordering::Relaxed) {
                return Err("Login cancelled.".into());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if Instant::now() > deadline {
            return Err("The device code expired. Start /login again.".into());
        }
        let data = oauth::post(ACCESS_TOKEN_URL, "application/x-www-form-urlencoded", &body)?;
        match poll(&data, interval) {
            Poll::Token(github_token) => return refresh(&github_token),
            Poll::Wait(next) => interval = next,
            Poll::Failed(error) => return Err(error),
        }
    }
}

#[derive(Debug, PartialEq)]
enum Poll {
    Token(String),
    /// Seconds until the next poll.
    Wait(u64),
    Failed(String),
}

/// RFC 8628 3.5: `slow_down` adds 5 seconds unless GitHub names the new interval.
fn poll(data: &Value, interval: u64) -> Poll {
    if let Some(token) = data["access_token"].as_str() {
        return Poll::Token(token.into());
    }
    match data["error"].as_str() {
        Some("authorization_pending") => Poll::Wait(interval),
        Some("slow_down") => Poll::Wait(data["interval"].as_u64().unwrap_or(interval + 5)),
        Some(error) => Poll::Failed(format!("Login failed: {}. Start /login again.", error.chars().filter(|c| !c.is_control()).take(100).collect::<String>())),
        None => Poll::Failed("The token response is missing fields.".into()),
    }
}

/// A Copilot token for the GitHub token kept as `refresh`; it lasts about 30 minutes.
pub fn refresh(github_token: &str) -> Result<Tokens, String> {
    let mut request = super::models_agent().get(COPILOT_TOKEN_URL).header("authorization", format!("Bearer {github_token}")).header("accept", "application/json");
    for (name, value) in HEADERS {
        request = request.header(name, value);
    }
    let mut response = request.call().map_err(|e| format!("Copilot token request failed: {e}."))?;
    let status = response.status().as_u16();
    let text = response.body_mut().read_to_string().map_err(|e| format!("Copilot token response unreadable: {e}."))?;
    if status != 200 {
        return Err(format!("Copilot token request failed ({status}). Does this GitHub account have Copilot?"));
    }
    let data: Value = serde_json::from_str(&text).map_err(|e| format!("Copilot token response is not JSON: {e}."))?;
    let (Some(access), Some(expires_at)) = (data["token"].as_str(), data["expires_at"].as_u64()) else { return Err("The Copilot token response is missing fields.".into()) };
    Ok(Tokens { access: access.into(), refresh: github_token.into(), expires: expires_at * 1000, account_id: None })
}

/// The API host the token names as `proxy-ep=proxy.…`, served at `api.…`.
pub fn base_url(token: &str) -> String {
    let proxy = token.split(';').find_map(|field| field.strip_prefix("proxy-ep="));
    // A hostname only: the token comes from GitHub, but the API key is sent wherever it points.
    match proxy.filter(|host| !host.is_empty() && host.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')) {
        Some(host) => format!("https://{}", host.strip_prefix("proxy.").map_or(host.to_string(), |rest| format!("api.{rest}"))),
        None => DEFAULT_BASE.into(),
    }
}

/// `X-Initiator: agent` for a turn after tool results, which Copilot does not bill as a premium request.
pub fn request_headers(body: &Value) -> Vec<(&'static str, &'static str)> {
    let messages = body["messages"].as_array().map(Vec::as_slice).unwrap_or_default();
    let initiator = if messages.last().is_some_and(|m| m["role"] == "user") { "user" } else { "agent" };
    let mut headers = vec![("x-initiator", initiator), ("openai-intent", "conversation-edits")];
    let has_image = messages.iter().any(|m| m["content"].as_array().is_some_and(|parts| parts.iter().any(|p| p["type"] == "image_url")));
    if has_image {
        headers.push(("copilot-vision-request", "true"));
    }
    headers
}

/// `GET {base}/models` as `github-copilot/<id>`: chat models from the picker that call tools over Chat Completions.
pub fn parse_models(body: &Value) -> Vec<Model> {
    let mut models = Vec::new();
    for m in body["data"].as_array().into_iter().flatten() {
        let Some(id) = m["id"].as_str() else { continue };
        let limits = &m["capabilities"]["limits"];
        let Some(context_window) = limits["max_context_window_tokens"].as_u64().or(limits["max_prompt_tokens"].as_u64()) else { continue };
        let chat = m["supported_endpoints"].as_array().is_none_or(|endpoints| endpoints.iter().any(|e| e == "/chat/completions"));
        let usable = m["model_picker_enabled"] == true && m["policy"]["state"] != "disabled" && m["capabilities"]["supports"]["tool_calls"] == true && chat;
        if !usable {
            continue;
        }
        let max_tokens = limits["max_output_tokens"].as_u64().unwrap_or(super::models::DEFAULT_MAX_TOKENS);
        models.push(Model { provider: PROVIDER.into(), id: format!("{PROVIDER}/{id}"), name: m["name"].as_str().unwrap_or(id).into(), context_window, max_tokens, accounts: Vec::new(), price: None });
    }
    models
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn polling_waits_slows_down_and_stops_on_a_denial() {
        assert_eq!(poll(&json!({"access_token": "gho_1"}), 5), Poll::Token("gho_1".into()));
        assert_eq!(poll(&json!({"error": "authorization_pending"}), 5), Poll::Wait(5));
        assert_eq!(poll(&json!({"error": "slow_down"}), 5), Poll::Wait(10));
        assert_eq!(poll(&json!({"error": "slow_down", "interval": 15}), 5), Poll::Wait(15));
        assert_eq!(poll(&json!({"error": "access_denied"}), 5), Poll::Failed("Login failed: access_denied. Start /login again.".into()));
    }

    #[test]
    fn the_base_url_comes_from_the_token_and_only_as_a_hostname() {
        assert_eq!(base_url("tid=1;exp=2;proxy-ep=proxy.business.githubcopilot.com;st=dotcom"), "https://api.business.githubcopilot.com");
        assert_eq!(base_url("tid=1;exp=2"), DEFAULT_BASE);
        assert_eq!(base_url("tid=1;proxy-ep=evil.com/x?"), DEFAULT_BASE);
    }

    #[test]
    fn a_turn_after_tool_results_is_agent_initiated_and_images_ask_for_vision() {
        let user = json!({"messages": [{"role": "user", "content": "hi"}]});
        assert_eq!(request_headers(&user), [("x-initiator", "user"), ("openai-intent", "conversation-edits")]);
        let tool = json!({"messages": [{"role": "user", "content": [{"type": "image_url", "image_url": {"url": "data:"}}]}, {"role": "tool", "content": "ok"}]});
        assert_eq!(request_headers(&tool), [("x-initiator", "agent"), ("openai-intent", "conversation-edits"), ("copilot-vision-request", "true")]);
    }

    #[test]
    fn the_model_list_keeps_enabled_tool_calling_chat_models() {
        let model = |id: &str, extra: Value| {
            let mut m = json!({"id": id, "name": id, "model_picker_enabled": true, "policy": {"state": "enabled"}, "capabilities": {"limits": {"max_context_window_tokens": 128000, "max_output_tokens": 16000}, "supports": {"tool_calls": true}}});
            for (key, value) in extra.as_object().unwrap() {
                m[key] = value.clone();
            }
            m
        };
        let body = json!({"data": [
            model("gpt-5", json!({})),
            model("hidden", json!({"model_picker_enabled": false})),
            model("disabled", json!({"policy": {"state": "disabled"}})),
            model("no-tools", json!({"capabilities": {"limits": {"max_context_window_tokens": 1}, "supports": {}}})),
            model("responses-only", json!({"supported_endpoints": ["/responses"]})),
        ]});
        let listed = parse_models(&body);
        assert_eq!(listed.iter().map(|m| (m.id.as_str(), m.context_window, m.max_tokens)).collect::<Vec<_>>(), [("github-copilot/gpt-5", 128000, 16000)]);
    }
}
