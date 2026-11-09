//! Google Cloud Code Assist, signed in as the Antigravity app (Gemini and Claude): Google sign-in with a local
//! callback or a pasted address, the account's project, its model list and the Gemini wire.
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::io::BufRead;
use std::net::TcpListener;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::accounts::{ModelQuota, Usage as Windows};
use super::models::Model;
use super::oauth::{self, Pending, Tokens};
use super::{ApiError, Delta, Reply, Usage, append};

pub const ANTIGRAVITY: &str = "google-antigravity";

pub struct App {
    pub provider: &'static str,
    client_id: &'static str,
    /// The env var with the app's client secret, which stays out of the repo.
    client_secret_env: &'static str,
    redirect_uri: &'static str,
    listen: &'static str,
    callback_path: &'static str,
    scopes: &'static str,
    pub base: &'static str,
    /// Where a stream request goes when `base` fails or answers 429 or 5xx.
    pub fallback: Option<&'static str>,
}

// The Antigravity app's client and wire, as pi-ai and oh-my-pi use them (google-antigravity.ts).
pub const ANTIGRAVITY_APP: App = App {
    provider: ANTIGRAVITY,
    client_id: "1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com",
    client_secret_env: "ANTIGRAVITY_CLIENT_SECRET",
    redirect_uri: "http://localhost:51121/oauth-callback",
    listen: "127.0.0.1:51121",
    callback_path: "/oauth-callback?",
    scopes: "https://www.googleapis.com/auth/cloud-platform https://www.googleapis.com/auth/userinfo.email https://www.googleapis.com/auth/userinfo.profile https://www.googleapis.com/auth/cclog https://www.googleapis.com/auth/experimentsandconfigs",
    base: "https://daily-cloudcode-pa.googleapis.com",
    fallback: Some("https://daily-cloudcode-pa.sandbox.googleapis.com"),
};

const AUTHORIZE_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
/// Cloud Code Assist gates newer Antigravity models on the app version, so it follows the app's update manifest.
const ANTIGRAVITY_MANIFEST: &str = "https://antigravity-hub-auto-updater-974169037036.us-central1.run.app/manifest/latest-arm64-mac.yml";
/// The version when the manifest does not answer.
const ANTIGRAVITY_VERSION: &str = "2.26.0";
static ANTIGRAVITY_LATEST: OnceLock<String> = OnceLock::new();
/// Gemini 3 rejects a replayed function call without a thought signature unless the first one carries this.
const SKIP_THOUGHT_SIGNATURE: &str = "skip_thought_signature_validator";
/// Cloud Code Assist answers 400 to Claude requests over 64,000 output tokens, though its list says 65,536.
const CLAUDE_MAX_TOKENS: u64 = 64_000;
const ONBOARD_TIMEOUT: Duration = Duration::from_secs(60);

pub fn start(app: &App) -> Pending {
    let (verifier, challenge) = oauth::pkce();
    let state = oauth::random_hex(16);
    let query = [
        ("client_id", app.client_id),
        ("response_type", "code"),
        ("redirect_uri", app.redirect_uri),
        ("scope", app.scopes),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
        ("state", &state),
        ("access_type", "offline"),
        ("prompt", "consent"),
    ];
    Pending { url: format!("{AUTHORIZE_URL}?{}", oauth::form(&query)), verifier, state }
}

/// Waits for the browser's redirect on the app's local port. When the port is taken, or the browser runs on another
/// computer, only the pasted address can finish the login, so this waits for `abort` without an error.
pub fn listen(app: &App, pending: &Pending, abort: &AtomicBool) -> Result<Tokens, String> {
    let Ok(listener) = TcpListener::bind(app.listen) else {
        while !abort.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(200));
        }
        return Err("Login cancelled.".into());
    };
    let code = oauth::callback_code(&listener, app.callback_path, &pending.state, abort)?;
    exchange(app, pending, &code)
}

/// Finishes with the pasted redirect address, or the bare code from it.
pub fn finish(app: &App, pending: &Pending, pasted: &str) -> Result<Tokens, String> {
    exchange(app, pending, &pasted_code(pending, pasted)?)
}

/// Trades the code for tokens, then finds the account's project, kept as its account id.
fn exchange(app: &App, pending: &Pending, code: &str) -> Result<Tokens, String> {
    let body = oauth::form(&[
        ("client_id", app.client_id),
        ("client_secret", &client_secret(app)?),
        ("code", code),
        ("grant_type", "authorization_code"),
        ("redirect_uri", app.redirect_uri),
        ("code_verifier", &pending.verifier),
    ]);
    let data = oauth::post(TOKEN_URL, "application/x-www-form-urlencoded", &body)?;
    let (Some(access), Some(refresh), Some(expires_in)) = (data["access_token"].as_str(), data["refresh_token"].as_str(), data["expires_in"].as_u64()) else {
        return Err("The token response is missing fields.".into());
    };
    let project = project(app, access)?;
    Ok(Tokens { access: access.into(), refresh: refresh.into(), expires: oauth::expires_at(expires_in), account_id: Some(project) })
}

/// Google keeps the refresh token, so the response has none; the project stays the account's, see `fresh`.
pub fn refresh(app: &App, refresh: &str) -> Result<Tokens, String> {
    let body = oauth::form(&[("client_id", app.client_id), ("client_secret", &client_secret(app)?), ("refresh_token", refresh), ("grant_type", "refresh_token")]);
    let data = oauth::post(TOKEN_URL, "application/x-www-form-urlencoded", &body)?;
    let (Some(access), Some(expires_in)) = (data["access_token"].as_str(), data["expires_in"].as_u64()) else {
        return Err("The token response is missing fields.".into());
    };
    let refresh = data["refresh_token"].as_str().unwrap_or(refresh);
    Ok(Tokens { access: access.into(), refresh: refresh.into(), expires: oauth::expires_at(expires_in), account_id: None })
}

fn client_secret(app: &App) -> Result<String, String> {
    std::env::var(app.client_secret_env).map_err(|_| format!("Set {} to the app's OAuth client secret.", app.client_secret_env))
}

/// The `code` of a pasted redirect address, checked against our state, or a pasted bare code (Google's start `4/`).
fn pasted_code(pending: &Pending, pasted: &str) -> Result<String, String> {
    let pasted = pasted.trim();
    if !pasted.contains("code=") {
        return if pasted.is_empty() || pasted.contains("://") || pasted.contains(['?', '&', ' ']) {
            Err("Paste the whole address the browser shows after sign-in.".into())
        } else {
            Ok(pasted.into())
        };
    }
    let query = pasted.split_once('?').map_or(pasted, |(_, query)| query);
    let params = oauth::parse_query(query.split('#').next().unwrap_or_default());
    let param = |key: &str| params.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str());
    if param("state") != Some(pending.state.as_str()) {
        return Err("OAuth state mismatch. Start /login again.".into());
    }
    param("code").filter(|code| !code.is_empty()).map(String::from).ok_or_else(|| "Missing authorization code.".into())
}

/// The Cloud Code Assist project requests bill to. A new account is onboarded to the free tier; a paid (Workspace)
/// tier needs its own project in `GOOGLE_CLOUD_PROJECT`.
fn project(app: &App, access: &str) -> Result<String, String> {
    let own = ["GOOGLE_CLOUD_PROJECT", "GOOGLE_CLOUD_PROJECT_ID"].into_iter().find_map(|name| std::env::var(name).ok().filter(|id| !id.is_empty()));
    let mut metadata = metadata();
    if let Some(own) = &own {
        metadata["duetProject"] = json!(own);
    }
    let loaded = call(app, access, "loadCodeAssist", &json!({"cloudaicompanionProject": own, "metadata": metadata}))?;
    if let Some(project) = project_id(&loaded["cloudaicompanionProject"]) {
        return Ok(project);
    }
    let needs_own = || "This Google account needs its own Cloud project: set GOOGLE_CLOUD_PROJECT and /login again. See https://goo.gle/gemini-cli-auth-docs#workspace-gca".to_string();
    if !loaded["currentTier"].is_null() {
        return own.ok_or_else(needs_own);
    }
    let tiers = loaded["allowedTiers"].as_array().cloned().unwrap_or_default();
    let tier = tiers.iter().find(|tier| tier["isDefault"] == true).and_then(|tier| tier["id"].as_str()).unwrap_or("free-tier");
    if tier == "free-tier"
        && !tiers.iter().any(|t| t["id"] == "free-tier")
        && let Some(refusal) = loaded["ineligibleTiers"].as_array().into_iter().flatten().find(|t| t["tierId"] == "free-tier")
    {
        let reason = shown(refusal["reasonMessage"].as_str().unwrap_or("This Google account cannot use the free tier."));
        return Err(format!("{reason} {}", shown(refusal["validationUrl"].as_str().unwrap_or_default())).trim_end().to_string());
    }
    if tier != "free-tier" && own.is_none() {
        return Err(needs_own());
    }
    let mut body = json!({"tierId": tier, "metadata": metadata});
    if tier != "free-tier" {
        body["cloudaicompanionProject"] = json!(own);
    }
    let operation = onboard(app, access, &body)?;
    project_id(&operation["response"]["cloudaicompanionProject"]).or(own).ok_or_else(|| "Google onboarded the account but gave it no Cloud Code Assist project. Start /login again.".into())
}

fn metadata() -> Value {
    json!({"ideType": "ANTIGRAVITY"})
}

/// A project given as `"id"` or `{"id": ...}`.
fn project_id(project: &Value) -> Option<String> {
    project.as_str().or(project["id"].as_str()).filter(|id| !id.is_empty()).map(String::from)
}

/// Starts the onboarding and polls its operation until it is done.
fn onboard(app: &App, access: &str, body: &Value) -> Result<Value, String> {
    let deadline = Instant::now() + ONBOARD_TIMEOUT;
    let mut operation = call(app, access, "onboardUser", body)?;
    while operation["done"] != true {
        if Instant::now() > deadline {
            return Err("Google onboarding timed out. Start /login again.".into());
        }
        std::thread::sleep(Duration::from_secs(2));
        let name = operation["name"].as_str().filter(|name| !name.is_empty()).ok_or("Google onboarding returned no operation.")?;
        let request = super::models_agent().get(format!("{}/v1internal/{name}", app.base)).header("authorization", format!("Bearer {access}")).header("user-agent", user_agent());
        operation = read(request.call(), "onboardUser")?;
    }
    match operation["error"]["message"].as_str() {
        Some(message) => Err(format!("Google onboarding failed: {}", shown(message))),
        None => Ok(operation),
    }
}

/// POSTs to a `v1internal:{method}` endpoint.
fn call(app: &App, access: &str, method: &str, body: &Value) -> Result<Value, String> {
    let request = super::models_agent()
        .post(format!("{}/v1internal:{method}", app.base))
        .header("authorization", format!("Bearer {access}"))
        .header("content-type", "application/json")
        .header("user-agent", user_agent());
    read(request.send(body.to_string()), method)
}

fn read(response: Result<ureq::http::Response<ureq::Body>, ureq::Error>, method: &str) -> Result<Value, String> {
    let mut response = response.map_err(|e| format!("{method} failed: {e}."))?;
    let status = response.status().as_u16();
    let text = response.body_mut().read_to_string().map_err(|e| format!("{method} failed: {e}."))?;
    if status != 200 {
        return Err(format!("{method} failed ({status}): {}.", shown(&text)));
    }
    serde_json::from_str(&text).map_err(|e| format!("{method} answer is not JSON: {e}."))
}

/// Third-party text bound for the terminal, without escapes.
pub(crate) fn shown(text: &str) -> String {
    text.trim().chars().filter(|c| !c.is_control()).take(500).collect()
}

/// The user-agent the Antigravity app sends.
pub fn user_agent() -> String {
    // The app's own platform names: it ships arm64 on macOS and amd64 elsewhere.
    let (os, arch) = match std::env::consts::OS {
        "macos" => ("darwin", "arm64"),
        "windows" => ("windows", "amd64"),
        _ => ("linux", "amd64"),
    };
    format!("antigravity/hub/{} (aidev_client; os_type={os}; arch={arch}; cl=1005016104)", antigravity_version())
}

/// The newest Antigravity version, read once per process; a failed read keeps `ANTIGRAVITY_VERSION`.
fn antigravity_version() -> &'static str {
    ANTIGRAVITY_LATEST.get_or_init(|| {
        let latest = || -> Option<String> {
            let mut response = super::models_agent().get(ANTIGRAVITY_MANIFEST).call().ok().filter(|r| r.status() == 200)?;
            manifest_version(&response.body_mut().read_to_string().ok()?)
        };
        latest().unwrap_or_else(|| ANTIGRAVITY_VERSION.into())
    })
}

/// The `version:` line of an electron-updater manifest.
fn manifest_version(yaml: &str) -> Option<String> {
    let line = yaml.lines().find_map(|line| line.trim().strip_prefix("version:"))?;
    let version = line.split('#').next()?.trim().trim_matches(['"', '\'']);
    (!version.is_empty() && version.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')).then(|| version.into())
}

/// The account's model list; the project is kept as the account id.
pub fn models(app: &App, tokens: &Tokens) -> Result<Vec<Model>, String> {
    let body = json!({"project": tokens.account_id});
    Ok(parse_antigravity_models(&call(app, &tokens.access, "fetchAvailableModels", &body)?))
}

/// The first image model `fetchAvailableModels` lists for an Antigravity account; `parse_antigravity_models` leaves them out.
pub fn image_model(app: &App, tokens: &Tokens) -> Result<String, String> {
    let listed = call(app, &tokens.access, "fetchAvailableModels", &json!({"project": tokens.account_id}))?;
    let mut ids = listed["imageGenerationModelIds"].as_array().into_iter().flatten().filter_map(Value::as_str);
    ids.find(|id| !id.is_empty()).map(String::from).ok_or("This Antigravity account lists no image model.".into())
}

/// The quota of each model, in percent used, from the same call as the model list.
pub fn usage(app: &App, tokens: &Tokens) -> Result<Windows, String> {
    let body = json!({"project": tokens.account_id});
    let data = call(app, &tokens.access, "fetchAvailableModels", &body)?;
    parse_usage(&data).ok_or("The quota response has no models.".into())
}

/// `fetchAvailableModels`' `models.*.quotaInfo`, each with `remainingFraction` and ISO `resetTime`.
/// Only the models the list keeps count: a spent image model does not stop the agent.
fn parse_usage(data: &Value) -> Option<Windows> {
    let listed: Vec<String> = parse_antigravity_models(data).into_iter().map(|m| m.id.split_once('/').map_or(m.id.clone(), |(_, id)| id.into())).collect();
    let infos = data["models"].as_object().into_iter().flatten().map(|(id, m)| (id.as_str(), &m["quotaInfo"]));
    let mut models: Vec<ModelQuota> = Vec::new();
    for (id, quota) in infos {
        let Some(remaining) = quota["remainingFraction"].as_f64().filter(|_| listed.iter().any(|l| l == id)) else { continue };
        let used = ((1.0 - remaining) * 100.0).clamp(0.0, 100.0);
        let resets_at = quota["resetTime"].as_str().and_then(super::accounts::iso_ms);
        // A model has a bucket per billing variant and token type; the fullest one limits it.
        match models.iter_mut().find(|q| q.model == id) {
            Some(known) if known.used >= used => {}
            Some(known) => *known = ModelQuota { model: id.into(), used, resets_at },
            None => models.push(ModelQuota { model: id.into(), used, resets_at }),
        }
    }
    models.sort_by(|a, b| a.model.cmp(&b.model));
    let fullest = models.iter().max_by(|a, b| a.used.total_cmp(&b.used))?;
    Some(Windows { five_hour: None, seven_day: fullest.used, resets_at: fullest.resets_at, models: models.clone() })
}

/// `fetchAvailableModels` as `google-antigravity/<id>`, by name; internal, disabled and image models cannot run the agent.
pub fn parse_antigravity_models(body: &Value) -> Vec<Model> {
    let images: Vec<&Value> = body["imageGenerationModelIds"].as_array().into_iter().flatten().collect();
    let mut models = Vec::new();
    for (id, m) in body["models"].as_object().into_iter().flatten() {
        // oh-my-pi's denylist: entries the list carries that do not serve agent requests.
        let denied = ["chat_20706", "chat_23310", "gemini-2.5-pro"].contains(&id.as_str());
        if denied || m["isInternal"] == true || m["disabled"] == true || images.iter().any(|image| *image == id) {
            continue;
        }
        let max_tokens = m["maxOutputTokens"].as_u64().unwrap_or(CLAUDE_MAX_TOKENS);
        let max_tokens = if id.starts_with("claude-") { max_tokens.min(CLAUDE_MAX_TOKENS) } else { max_tokens };
        let name = m["displayName"].as_str().unwrap_or(id);
        models.push(model(ANTIGRAVITY, id, name, m["maxTokens"].as_u64().unwrap_or(200_000), max_tokens));
    }
    models.sort_by(|a, b| a.name.cmp(&b.name));
    models
}

fn model(provider: &str, id: &str, name: &str, context_window: u64, max_tokens: u64) -> Model {
    Model { provider: provider.into(), id: format!("{provider}/{id}"), name: name.into(), context_window, max_tokens, accounts: Vec::new(), price: None }
}

/// Turns an Anthropic Messages request into a Cloud Code Assist `streamGenerateContent` body.
/// The model's own thoughts go back with their signatures, which Gemini 3 needs on function calls and Claude on thinking.
pub fn request(app: &App, anthropic: &Value, project: &str, session: &str) -> Value {
    let id = wire_model(anthropic);
    let owner = signature_owner(app.provider, id);
    let mut tool_names = HashMap::new();
    let mut contents = Vec::new();
    for message in anthropic["messages"].as_array().into_iter().flatten() {
        let blocks = message["content"].as_array().cloned().unwrap_or_else(|| vec![json!({"type": "text", "text": message["content"]})]);
        let mut parts = Vec::new();
        if message["role"] == "assistant" {
            // A signature-only thought signs the function call after it.
            let mut call_signature = None;
            for block in &blocks {
                match block["type"].as_str() {
                    Some("text") if block["text"] != "" => parts.push(json!({"text": block["text"]})),
                    Some("thinking") => {
                        // Another model's or another app's thoughts and signatures fail validation, so they go.
                        let Some(signature) = block["signature"].as_str().and_then(|s| s.strip_prefix(owner.as_str())) else { continue };
                        let thinking = block["thinking"].as_str().unwrap_or_default();
                        if thinking.is_empty() {
                            call_signature = Some(signature).filter(|s| !s.is_empty());
                            continue;
                        }
                        let mut part = json!({"thought": true, "text": thinking});
                        if !signature.is_empty() {
                            part["thoughtSignature"] = json!(signature);
                        }
                        parts.push(part);
                    }
                    Some("tool_use") => {
                        tool_names.insert(block["id"].as_str().unwrap_or_default().to_string(), block["name"].clone());
                        let mut part = json!({"functionCall": {"name": block["name"], "args": block["input"], "id": block["id"]}});
                        let first = !parts.iter().any(|p: &Value| p.get("functionCall").is_some());
                        // Cloud Code Assist wants the bypass only on an unsigned first call; later unsigned calls stay bare.
                        match call_signature.take() {
                            Some(signature) => part["thoughtSignature"] = json!(signature),
                            None if first => part["thoughtSignature"] = json!(SKIP_THOUGHT_SIGNATURE),
                            None => {}
                        }
                        parts.push(part);
                    }
                    _ => {}
                }
            }
            if !parts.is_empty() {
                contents.push(json!({"role": "model", "parts": parts}));
            }
            continue;
        }
        let mut images = Vec::new();
        for block in &blocks {
            match block["type"].as_str() {
                Some("tool_result") => {
                    let id = block["tool_use_id"].as_str().unwrap_or_default();
                    let (text, result_images) = tool_output(&block["content"]);
                    let key = if block["is_error"] == true { "error" } else { "output" };
                    parts.push(json!({"functionResponse": {"name": tool_names.get(id).cloned().unwrap_or_default(), "id": id, "response": {key: text}}}));
                    images.extend(result_images);
                }
                Some("text") if block["text"] != "" => parts.push(json!({"text": block["text"]})),
                Some("image") => parts.push(inline_data(block)),
                _ => {}
            }
        }
        // Function responses lead the turn, so they answer the calls before it in order.
        parts.extend(images);
        if !parts.is_empty() {
            contents.push(json!({"role": "user", "parts": parts}));
        }
    }
    let mut inner = json!({"contents": contents, "generationConfig": {"maxOutputTokens": anthropic["max_tokens"]}});
    if let Some(thinking) = anthropic["output_config"]["effort"].as_str().and_then(|effort| thinking_config(id, effort)) {
        inner["generationConfig"]["thinkingConfig"] = thinking;
    }
    let system: Vec<&str> = match &anthropic["system"] {
        Value::String(text) => vec![text.as_str()],
        system => system.as_array().into_iter().flatten().filter_map(|b| b["text"].as_str()).collect(),
    };
    if !system.is_empty() {
        inner["systemInstruction"] = json!({"parts": [{"text": system.join("\n\n")}]});
    }
    let declarations: Vec<Value> = anthropic["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|tool| tool["input_schema"].is_object())
        .map(|tool| json!({"name": tool["name"], "description": tool["description"].as_str().unwrap_or_default(), "parameters": schema(&tool["input_schema"])}))
        .collect();
    if !declarations.is_empty() {
        inner["tools"] = json!([{"functionDeclarations": declarations}]);
    }
    // Antigravity's own client tags the system prompt as a user turn and validates tool calls against the schema.
    if !system.is_empty() {
        inner["systemInstruction"]["role"] = json!("user");
    }
    if !declarations.is_empty() {
        inner["toolConfig"] = json!({"functionCallingConfig": {"mode": "VALIDATED"}});
    }
    inner["sessionId"] = json!(session);
    json!({
        "project": project,
        "model": id,
        "request": inner,
        "userAgent": "antigravity",
        "requestId": format!("agent-{}-{}", crate::process::now_ms(), oauth::random_hex(8)),
    })
}

/// A `streamGenerateContent` body for an Antigravity image model, as oh-my-pi sends it: the input images, then the
/// prompt, answered with an image and nothing else.
pub fn image_request(model: &str, project: &str, prompt: &str, inputs: &[Value]) -> Value {
    let mut parts: Vec<Value> = inputs.iter().map(inline_data).collect();
    parts.push(json!({"text": prompt}));
    json!({
        "project": project,
        "model": model,
        "request": {
            "contents": [{"role": "user", "parts": parts}],
            "systemInstruction": {"parts": [{"text": IMAGE_SYSTEM_INSTRUCTION}]},
            "generationConfig": {"responseModalities": ["IMAGE"]},
        },
        "userAgent": "antigravity",
        "requestId": format!("agent-{}-{}", crate::process::now_ms(), oauth::random_hex(8)),
    })
}

const IMAGE_SYSTEM_INSTRUCTION: &str =
    "You are an AI image generator. Generate images based on user descriptions. Focus on creating high-quality, visually appealing images that match the user's request.";

/// The model id Cloud Code Assist knows, without the provider prefix.
pub fn wire_model(anthropic: &Value) -> &str {
    let model = anthropic["model"].as_str().unwrap_or_default();
    model.split_once('/').map_or(model, |(_, id)| id)
}

/// The agent's effort as Google's thinking settings: a level for Gemini 3, a token budget for Gemini 2.5 and Claude.
/// An Antigravity tier variant (`-low`, `-high`) fixes its own level, and a Claude model without `-thinking` has none.
fn thinking_config(id: &str, effort: &str) -> Option<Value> {
    if id.starts_with("gemini-3") {
        let level = if id.ends_with("-low") {
            "LOW"
        } else if id.ends_with("-high") {
            "HIGH"
        } else {
            match effort {
                "low" => "LOW",
                // Gemini 3 Pro takes LOW and HIGH only.
                "medium" if !id.starts_with("gemini-3-pro") => "MEDIUM",
                _ => "HIGH",
            }
        };
        return Some(json!({"includeThoughts": true, "thinkingLevel": level}));
    }
    if id.starts_with("claude-") && !id.ends_with("-thinking") {
        return None;
    }
    let budget = match effort {
        "low" => 1024,
        "medium" => 8192,
        "high" => 16_384,
        "xhigh" => 24_576,
        _ => 32_768,
    };
    // Gemini 2.5 Flash takes at most 24,576 thinking tokens.
    let budget = if id.contains("flash") { budget.min(24_576) } else { budget };
    Some(json!({"includeThoughts": true, "thinkingBudget": budget}))
}

fn tool_output(content: &Value) -> (String, Vec<Value>) {
    let Some(blocks) = content.as_array() else { return (content.as_str().unwrap_or_default().into(), Vec::new()) };
    let text = blocks.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n");
    let images = blocks.iter().filter(|b| b["type"] == "image").map(inline_data).collect();
    (text, images)
}

fn inline_data(block: &Value) -> Value {
    json!({"inlineData": {"mimeType": block["source"]["media_type"], "data": block["source"]["data"]}})
}

/// The OpenAPI subset Cloud Code Assist takes as `parameters` for both Gemini and Claude: type, description, enum,
/// required, items and properties. A union becomes its first non-null member.
fn schema(value: &Value) -> Value {
    let Some(object) = value.as_object() else { return json!({"type": "object"}) };
    if let Some(member) = value["anyOf"].as_array().into_iter().flatten().find(|m| m["type"] != "null") {
        let mut picked = schema(member);
        if let Some(description) = object.get("description") {
            picked["description"] = description.clone();
        }
        return picked;
    }
    let mut out = Map::new();
    let kind = match object.get("type") {
        Some(Value::Array(kinds)) => kinds.iter().find(|k| *k != "null").cloned(),
        kind => kind.cloned(),
    };
    if let Some(kind) = kind {
        out.insert("type".into(), kind);
    }
    for key in ["description", "enum", "required"] {
        if let Some(value) = object.get(key) {
            out.insert(key.into(), value.clone());
        }
    }
    if let Some(items) = object.get("items") {
        out.insert("items".into(), schema(items));
    }
    if let Some(properties) = object.get("properties").and_then(Value::as_object) {
        out.insert("properties".into(), properties.iter().map(|(name, property)| (name.clone(), schema(property))).collect());
    }
    Value::Object(out)
}

/// A thought's `signature` starts with the app and model that signed it: only they accept it back.
pub fn signature_owner(provider: &str, model: &str) -> String {
    format!("{provider}:{model}:")
}

/// Anthropic-format endpoints reject a Google thought's signature, so they never get one.
pub fn is_google_thought(block: &Value) -> bool {
    // Sessions saved before the Gemini CLI login was removed still hold its signatures.
    block["type"] == "thinking" && block["signature"].as_str().is_some_and(|s| ["google-gemini-cli", ANTIGRAVITY].iter().any(|p| s.starts_with(&format!("{p}:"))))
}

/// Reads the `data:` chunks of `streamGenerateContent?alt=sse` into the Anthropic reply shape: thinking, text and
/// tool_use blocks. A thought keeps its signature behind `owner`; a function call's signature becomes an empty thought before it.
pub fn parse_stream(reader: &mut dyn BufRead, owner: &str, abort: &AtomicBool, on_delta: &mut dyn FnMut(Delta)) -> Result<Reply, ApiError> {
    let mut blocks: Vec<Value> = Vec::new();
    let mut usage = Usage::default();
    let mut id = String::new();
    let mut stop_reason = String::new();
    let mut line = String::new();
    loop {
        if abort.load(Ordering::Relaxed) {
            stop_reason = "aborted".into();
            break;
        }
        line.clear();
        let read = match reader.read_line(&mut line) {
            Err(_) if abort.load(Ordering::Relaxed) => continue,
            read => read.map_err(|e| ApiError::retryable(format!("Stream read failed: {e}.")))?,
        };
        if read == 0 {
            break;
        }
        let Some(data) = line.strip_prefix("data:").map(str::trim).filter(|d| !d.is_empty()) else { continue };
        let chunk: Value = serde_json::from_str(data).map_err(|e| ApiError::fatal(format!("Bad stream event: {e}.")))?;
        let response = &chunk["response"];
        if let Some(response_id) = response["responseId"].as_str() {
            id = response_id.into();
        }
        let candidate = &response["candidates"][0];
        for part in candidate["content"]["parts"].as_array().into_iter().flatten() {
            let signature = part["thoughtSignature"].as_str().filter(|s| !s.is_empty());
            if let Some(call) = part.get("functionCall") {
                if let Some(signature) = signature {
                    blocks.push(json!({"type": "thinking", "thinking": "", "signature": format!("{owner}{signature}")}));
                }
                let given = call["id"].as_str().filter(|id| !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'));
                let id = given.map_or_else(|| format!("toolu_{}", oauth::random_hex(12)), String::from);
                let input = if call["args"].is_object() { call["args"].clone() } else { json!({}) };
                blocks.push(json!({"type": "tool_use", "id": id, "name": call["name"], "input": input}));
                continue;
            }
            let Some(text) = part["text"].as_str() else { continue };
            if part["thought"] == true {
                on_delta(Delta::Thinking(text));
                match blocks.last_mut() {
                    Some(block) if block["type"] == "thinking" && block["thinking"] != "" => append(&mut block["thinking"], text),
                    _ => blocks.push(json!({"type": "thinking", "thinking": text, "signature": owner})),
                }
                if let Some(signature) = signature {
                    blocks.last_mut().expect("a thought block was just touched")["signature"] = json!(format!("{owner}{signature}"));
                }
                continue;
            }
            on_delta(Delta::Text(text));
            match blocks.last_mut() {
                Some(block) if block["type"] == "text" => append(&mut block["text"], text),
                _ => blocks.push(json!({"type": "text", "text": text})),
            }
        }
        if let Some(reason) = candidate["finishReason"].as_str() {
            stop_reason = match reason {
                "STOP" => "end_turn".into(),
                "MAX_TOKENS" => "max_tokens".into(),
                other => return Err(ApiError::fatal(format!("Google stopped the answer: {}.", shown(other)))),
            };
        }
        let meta = &response["usageMetadata"];
        if meta.is_object() {
            let cached = meta["cachedContentTokenCount"].as_u64().unwrap_or(0);
            usage = Usage {
                input: meta["promptTokenCount"].as_u64().unwrap_or(0).saturating_sub(cached),
                output: meta["candidatesTokenCount"].as_u64().unwrap_or(0) + meta["thoughtsTokenCount"].as_u64().unwrap_or(0),
                cache_read: cached,
                cache_write: 0,
            };
        }
    }
    if stop_reason.is_empty() {
        return Err(ApiError::retryable("Stream ended before its finish reason.".into()));
    }
    let blocks: Vec<Value> = blocks.into_iter().filter(|b| b["type"] != "text" || b["text"] != "").collect();
    if stop_reason == "end_turn" && blocks.iter().any(|b| b["type"] == "tool_use") {
        stop_reason = "tool_use".into();
    }
    Ok(Reply { message: json!({"role": "assistant", "content": blocks}), stop_reason, context_tokens: usage.context_tokens(), usage, id, ..Reply::default() })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::{Read, Write};

    /// The Antigravity app on local endpoints; the built-in version stands, so no request goes to the update manifest.
    pub(crate) fn app_at(base: &str, fallback: &str) -> App {
        let _ = ANTIGRAVITY_LATEST.set(ANTIGRAVITY_VERSION.into());
        App { base: base.to_string().leak(), fallback: Some(fallback.to_string().leak()), ..ANTIGRAVITY_APP }
    }

    fn pending() -> Pending {
        Pending { url: String::new(), verifier: "v".into(), state: "s1".into() }
    }

    #[test]
    fn the_pasted_address_gives_its_code_only_with_our_state() {
        assert_eq!(pasted_code(&pending(), " http://localhost:51121/oauth-callback?state=s1&code=4%2F0Ab&scope=x ").unwrap(), "4/0Ab");
        assert_eq!(pasted_code(&pending(), "4/0Ab").unwrap(), "4/0Ab");
        assert_eq!(pasted_code(&pending(), "http://localhost:51121/oauth-callback?state=other&code=c").unwrap_err(), "OAuth state mismatch. Start /login again.");
        assert_eq!(pasted_code(&pending(), "http://localhost:51121/oauth-callback?error=access_denied").unwrap_err(), "Paste the whole address the browser shows after sign-in.");
        let app = &ANTIGRAVITY_APP;
        assert!(start(app).url.contains(&format!("redirect_uri={}", oauth::percent_encode(app.redirect_uri))));
        assert!(app.redirect_uri.ends_with(app.callback_path.trim_end_matches('?')) && app.redirect_uri.contains(app.listen.split(':').nth(1).unwrap()));
    }

    #[test]
    fn a_taken_port_leaves_the_login_to_the_pasted_address() {
        let taken = TcpListener::bind("127.0.0.1:0").unwrap();
        let app = App { listen: Box::leak(taken.local_addr().unwrap().to_string().into_boxed_str()), ..ANTIGRAVITY_APP };
        let abort = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let waiting = scope.spawn(|| listen(&app, &pending(), &abort));
            std::thread::sleep(Duration::from_millis(300));
            assert!(!waiting.is_finished());
            abort.store(true, Ordering::Relaxed);
            assert_eq!(waiting.join().unwrap().unwrap_err(), "Login cancelled.");
        });
    }

    #[test]
    fn the_browser_redirect_reaches_the_listener() {
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let app = App { listen: Box::leak(format!("127.0.0.1:{port}").into_boxed_str()), ..ANTIGRAVITY_APP };
        let abort = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let waiting = scope.spawn(|| listen(&app, &pending(), &abort));
            std::thread::sleep(Duration::from_millis(300));
            let mut browser = std::net::TcpStream::connect(app.listen).unwrap();
            write!(browser, "GET {}state=s1&error=access_denied HTTP/1.1\r\n\r\n", app.callback_path).unwrap();
            let mut page = String::new();
            browser.read_to_string(&mut page).unwrap();
            assert!(page.contains("Login finished"));
            assert_eq!(waiting.join().unwrap().unwrap_err(), "Login failed: access_denied. Start /login again.");
        });
    }

    #[test]
    fn the_antigravity_list_drops_what_cannot_run_the_agent() {
        let body = json!({
            "models": {
                "gemini-3-flash": {"displayName": "Gemini 3 Flash", "maxTokens": 1048576, "maxOutputTokens": 65536},
                "claude-sonnet-4-6": {"displayName": "Claude Sonnet 4.6", "maxTokens": 200000, "maxOutputTokens": 65536},
                "chat_20706": {"displayName": "x"},
                "internal": {"isInternal": true},
                "gemini-4-argon-disabled": {"disabled": true},
                "gemini-3-pro-image": {"displayName": "Image"},
            },
            "imageGenerationModelIds": ["gemini-3-pro-image"],
        });
        let listed: Vec<_> = parse_antigravity_models(&body).into_iter().map(|m| (m.id, m.context_window, m.max_tokens)).collect();
        assert_eq!(listed, [("google-antigravity/claude-sonnet-4-6".to_string(), 200_000, 64_000), ("google-antigravity/gemini-3-flash".to_string(), 1_048_576, 65_536)]);
    }

    #[test]
    fn effort_becomes_a_level_on_gemini_3_and_a_budget_elsewhere() {
        let thinking = |id: &str, effort: &str| thinking_config(id, effort).map(|c| c.get("thinkingLevel").or(c.get("thinkingBudget")).cloned().unwrap());
        assert_eq!(thinking("gemini-3-flash-preview", "medium"), Some(json!("MEDIUM")));
        assert_eq!(thinking("gemini-3-pro-preview", "medium"), Some(json!("HIGH")));
        assert_eq!(thinking("gemini-3-pro-low", "max"), Some(json!("LOW")));
        assert_eq!(thinking("gemini-2.5-pro", "max"), Some(json!(32_768)));
        assert_eq!(thinking("gemini-2.5-flash", "max"), Some(json!(24_576)));
        assert_eq!(thinking("claude-opus-4-6-thinking", "low"), Some(json!(1024)));
        assert_eq!(thinking("claude-sonnet-4-6", "high"), None);
    }

    fn history() -> Value {
        json!({
            "model": "google-antigravity/gemini-3-flash",
            "max_tokens": 1000,
            "system": [{"type": "text", "text": "Be brief."}, {"type": "text", "text": "cwd: /x"}],
            "output_config": {"effort": "high"},
            "tools": [
                {"name": "read", "description": "Read", "input_schema": {"type": "object", "additionalProperties": false, "properties": {"pattern": {"type": ["string", "null"], "minLength": 1}, "mode": {"anyOf": [{"type": "null"}, {"type": "string", "enum": ["a"]}], "description": "How"}}, "required": ["pattern"]}},
                {"type": "web_search_20250305", "name": "web_search"},
            ],
            "messages": [
                {"role": "user", "content": "look"},
                {"role": "assistant", "content": [{"type": "thinking", "thinking": "hm", "signature": "s"}, {"type": "text", "text": "Reading."}, {"type": "tool_use", "id": "t1", "name": "read", "input": {"pattern": "a"}}, {"type": "tool_use", "id": "t2", "name": "read", "input": {"pattern": "b"}}]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": [{"type": "text", "text": "a.png"}, {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "iVBO"}}]},
                    {"type": "tool_result", "tool_use_id": "t2", "content": "gone", "is_error": true},
                    {"type": "text", "text": "and?"},
                ]},
            ],
        })
    }

    #[test]
    fn anthropic_history_becomes_gemini_contents() {
        let body = request(&ANTIGRAVITY_APP, &history(), "proj-1", "sess");
        assert_eq!((&body["project"], &body["model"], &body["userAgent"]), (&json!("proj-1"), &json!("gemini-3-flash"), &json!("antigravity")));
        let inner = &body["request"];
        assert_eq!((&inner["sessionId"], &inner["toolConfig"]["functionCallingConfig"]["mode"]), (&json!("sess"), &json!("VALIDATED")));
        assert_eq!(inner["systemInstruction"], json!({"role": "user", "parts": [{"text": "Be brief.\n\ncwd: /x"}]}));
        assert_eq!(inner["generationConfig"], json!({"maxOutputTokens": 1000, "thinkingConfig": {"includeThoughts": true, "thinkingLevel": "HIGH"}}));
        assert_eq!(
            inner["tools"],
            json!([{"functionDeclarations": [{"name": "read", "description": "Read", "parameters": {"type": "object", "required": ["pattern"], "properties": {"pattern": {"type": "string"}, "mode": {"type": "string", "enum": ["a"], "description": "How"}}}}]}])
        );
        assert_eq!(
            inner["contents"],
            json!([
                {"role": "user", "parts": [{"text": "look"}]},
                {"role": "model", "parts": [
                    {"text": "Reading."},
                    {"functionCall": {"name": "read", "args": {"pattern": "a"}, "id": "t1"}, "thoughtSignature": SKIP_THOUGHT_SIGNATURE},
                    {"functionCall": {"name": "read", "args": {"pattern": "b"}, "id": "t2"}},
                ]},
                {"role": "user", "parts": [
                    {"functionResponse": {"name": "read", "id": "t1", "response": {"output": "a.png"}}},
                    {"functionResponse": {"name": "read", "id": "t2", "response": {"error": "gone"}}},
                    {"text": "and?"},
                    {"inlineData": {"mimeType": "image/png", "data": "iVBO"}},
                ]},
            ])
        );
    }

    #[test]
    fn an_image_request_carries_the_inputs_then_the_prompt_and_asks_for_an_image_only() {
        let input = json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "iVBO"}});
        let body = image_request("gemini-3-pro-image", "proj-1", "a red dot", &[input]);
        assert_eq!((&body["project"], &body["model"], &body["userAgent"]), (&json!("proj-1"), &json!("gemini-3-pro-image"), &json!("antigravity")));
        assert!(body["requestId"].as_str().unwrap().starts_with("agent-"));
        let inner = &body["request"];
        assert_eq!(inner["contents"], json!([{"role": "user", "parts": [{"inlineData": {"mimeType": "image/png", "data": "iVBO"}}, {"text": "a red dot"}]}]));
        assert_eq!(inner["generationConfig"], json!({"responseModalities": ["IMAGE"]}));
        assert_eq!(inner["systemInstruction"]["parts"][0]["text"], IMAGE_SYSTEM_INSTRUCTION);
    }

    #[test]
    fn stream_becomes_text_and_tool_use_blocks() {
        let chunks = [
            r#"{"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"plan","thought":true}]}}]}}"#,
            r#"{"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"Hi"}]}}]}}"#,
            r#"{"response":{"candidates":[{"content":{"role":"model","parts":[{"text":" there"},{"functionCall":{"name":"read","args":{"pattern":"a"}},"thoughtSignature":"x"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":100,"cachedContentTokenCount":60,"candidatesTokenCount":5,"thoughtsTokenCount":2},"responseId":"r1"}}"#,
        ];
        let body: String = chunks.iter().map(|c| format!("data: {c}\r\n\r\n")).collect();
        let mut streamed = String::new();
        let owner = signature_owner(ANTIGRAVITY, "gemini-3-flash");
        let reply = parse_stream(&mut body.as_bytes(), &owner, &AtomicBool::new(false), &mut |delta| match delta {
            Delta::Text(t) => streamed.push_str(t),
            Delta::Thinking(t) => streamed.push_str(&format!("[{t}]")),
            Delta::Notice(_) => {}
        })
        .ok()
        .unwrap();
        assert_eq!(streamed, "[plan]Hi there");
        assert_eq!((reply.stop_reason.as_str(), reply.id.as_str()), ("tool_use", "r1"));
        assert_eq!((reply.usage.input, reply.usage.cache_read, reply.usage.output), (40, 60, 7));
        let content = &reply.message["content"];
        assert_eq!(content[0], json!({"type": "thinking", "thinking": "plan", "signature": "google-antigravity:gemini-3-flash:"}));
        assert_eq!(content[1], json!({"type": "text", "text": "Hi there"}));
        assert_eq!(content[2], json!({"type": "thinking", "thinking": "", "signature": "google-antigravity:gemini-3-flash:x"}));
        assert_eq!((&content[3]["name"], &content[3]["input"]), (&json!("read"), &json!({"pattern": "a"})));
        assert!(content[3]["id"].as_str().unwrap().starts_with("toolu_"));
        let cut = parse_stream(&mut format!("data: {}\n", chunks[1]).as_bytes(), &owner, &AtomicBool::new(false), &mut |_| {}).err().unwrap();
        assert!(cut.retryable, "{}", cut.message);
    }

    #[test]
    fn the_models_own_signatures_go_back_and_others_are_dropped() {
        let chunks = [
            r#"{"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"think","thought":true},{"text":" more","thought":true,"thoughtSignature":"c2ln"}]}}]}}"#,
            r#"{"response":{"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"read","args":{},"id":"t1"},"thoughtSignature":"Y2FsbA=="},{"functionCall":{"name":"read","args":{},"id":"t2"}}]},"finishReason":"STOP"}]}}"#,
        ];
        let body: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
        let owner = signature_owner(ANTIGRAVITY, "claude-sonnet-4-5-thinking");
        let reply = parse_stream(&mut body.as_bytes(), &owner, &AtomicBool::new(false), &mut |_| {}).ok().unwrap();
        let mut content = reply.message["content"].as_array().unwrap().clone();
        content.push(json!({"type": "thinking", "thinking": "Anthropic's", "signature": "abc"}));
        let history = |model: &str| json!({"model": model, "max_tokens": 10, "messages": [{"role": "user", "content": "go"}, {"role": "assistant", "content": content}]});

        let same = request(&ANTIGRAVITY_APP, &history("google-antigravity/claude-sonnet-4-5-thinking"), "p", "s");
        assert_eq!(
            same["request"]["contents"][1]["parts"],
            json!([
                {"thought": true, "text": "think more", "thoughtSignature": "c2ln"},
                {"functionCall": {"name": "read", "args": {}, "id": "t1"}, "thoughtSignature": "Y2FsbA=="},
                {"functionCall": {"name": "read", "args": {}, "id": "t2"}},
            ])
        );
        let other = request(&ANTIGRAVITY_APP, &history("google-antigravity/gemini-3-flash"), "p", "s");
        assert_eq!(
            other["request"]["contents"][1]["parts"],
            json!([
                {"functionCall": {"name": "read", "args": {}, "id": "t1"}, "thoughtSignature": SKIP_THOUGHT_SIGNATURE},
                {"functionCall": {"name": "read", "args": {}, "id": "t2"}},
            ])
        );
        let kept: Vec<bool> = content.iter().map(|b| !is_google_thought(b)).collect();
        assert_eq!(kept, [false, false, true, true, true]);
    }

    #[test]
    fn the_manifest_names_the_antigravity_version() {
        assert_eq!(manifest_version("version: 2.31.4\nfiles:\n  - url: a.zip\n").as_deref(), Some("2.31.4"));
        assert_eq!(manifest_version("version: \"2.31.4\" # latest\n").as_deref(), Some("2.31.4"));
        assert_eq!(manifest_version("version: 2.3 (x)\n"), None);
        assert_eq!(manifest_version("files: []\n"), None);
    }

    #[test]
    fn the_fullest_listed_model_quota_is_the_account_window() {
        let quota = json!({"imageGenerationModelIds": ["gemini-3-pro-image"], "models": {
            "gemini-3-flash": {"displayName": "Gemini 3 Flash", "quotaInfo": {"remainingFraction": 0.75, "resetTime": "2026-01-02T00:00:00Z"}},
            "claude-sonnet-4-6": {"displayName": "Claude Sonnet 4.6", "quotaInfo": {"remainingFraction": 0.5, "resetTime": "2026-01-01T00:00:00Z"}},
            "gemini-3-pro-image": {"quotaInfo": {"remainingFraction": 0.0}},
            "chat_20706": {"quotaInfo": {"remainingFraction": 0.0}},
        }});
        let usage = parse_usage(&quota).unwrap();
        let mut quotas: Vec<(&str, f64, Option<u64>)> = usage.models.iter().map(|q| (q.model.as_str(), q.used, q.resets_at)).collect();
        quotas.sort_by(|a, b| a.0.cmp(b.0));
        assert_eq!(quotas, [("claude-sonnet-4-6", 50.0, Some(1_767_225_600_000)), ("gemini-3-flash", 25.0, Some(1_767_312_000_000))]);
        assert_eq!((usage.five_hour, usage.seven_day), (None, 50.0));
        let flash = usage.for_model("google-antigravity/gemini-3-flash").unwrap();
        assert_eq!((flash.seven_day, flash.resets_at), (25.0, Some(1_767_312_000_000)));
        assert_eq!(usage.for_model("google-antigravity/gemini-3-pro"), None);
        assert_eq!(parse_usage(&json!({})), None);
    }
}
