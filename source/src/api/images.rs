//! Images from another account's image model, which the agent's catalog never lists: the Codex images API behind a
//! ChatGPT login (`gpt-image-2`), or the image model an Antigravity account advertises.
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;

use super::accounts::Secret;
use super::oauth::Tokens;
use super::{Client, google};

/// The Codex images API's model; the model list behind a ChatGPT login does not name it.
const CODEX_MODEL: &str = "gpt-image-2";
const CODEX: &str = "openai-codex";
/// An image answer comes whole once drawn, 20 s for a small one; `Client::http` gives up on headers after 2 minutes.
const IMAGE_TIMEOUT: Duration = Duration::from_secs(300);

impl Client {
    /// Whether a login can draw with `image_model`, which is when the ImageGenerate tool is offered.
    pub fn generates_images(&self, image_model: &str) -> bool {
        self.has_login(self.image_provider(image_model).0)
    }

    /// The bytes of the image `prompt` describes, or with `inputs` (image blocks) an edit of them. Setting `abort` ends
    /// the wait with `ABORTED`.
    pub fn generate_image(&self, image_model: &str, prompt: &str, inputs: &[Value], abort: &AtomicBool) -> Result<Vec<u8>, String> {
        let (provider, model) = self.image_provider(image_model);
        let account = super::likely_account(&self.state.lock().unwrap(), provider).cloned().ok_or(format!("No {provider} account. Use /login."))?;
        let account = self.fresh(account)?;
        let Secret::OAuth(tokens) = &account.secret else { return Err(format!("{provider} accounts need /login, not an API key.")) };
        // The request blocks until the image is drawn, so it runs on its own thread and Esc ends the wait, not the thread.
        let (client, tokens, model, prompt, inputs) = (self.clone(), tokens.clone(), model.to_string(), prompt.to_string(), inputs.to_vec());
        let (sent, received) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let image = if provider == CODEX { client.codex_image(&tokens, &model, &prompt, &inputs) } else { antigravity_image(&google::ANTIGRAVITY_APP, &tokens, &model, &prompt, &inputs) };
            let _ = sent.send(image);
        });
        loop {
            match received.recv_timeout(Duration::from_millis(100)) {
                Ok(image) => return image,
                Err(RecvTimeoutError::Timeout) if abort.load(Ordering::Relaxed) => return Err(crate::agent::ABORTED.into()),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Err("The image request thread panicked.".into()),
            }
        }
    }

    /// An empty Antigravity model is the first image model the account lists, which only a request can tell.
    fn image_provider<'a>(&self, image_model: &'a str) -> (&'static str, &'a str) {
        match image_model.split_once('/') {
            Some((google::ANTIGRAVITY, id)) => (google::ANTIGRAVITY, id),
            Some((CODEX, id)) => (CODEX, id),
            _ if !image_model.is_empty() => (CODEX, image_model),
            _ if self.has_login(CODEX) => (CODEX, CODEX_MODEL),
            _ => (google::ANTIGRAVITY, ""),
        }
    }

    fn has_login(&self, provider: &str) -> bool {
        self.state.lock().unwrap().accounts.iter().any(|a| a.provider == provider && a.is_oauth())
    }

    /// `POST /codex/images/generations`, or `/edits` with the input images, as the Codex CLI's image extension sends them.
    fn codex_image(&self, tokens: &Tokens, model: &str, prompt: &str, inputs: &[Value]) -> Result<Vec<u8>, String> {
        let mut body = json!({"model": model, "prompt": prompt});
        let endpoint = if inputs.is_empty() { "generations" } else { "edits" };
        if !inputs.is_empty() {
            body["images"] = inputs.iter().map(|block| json!({"image_url": super::codex::data_url(block)})).collect();
        }
        let call = image_http()
            .post(format!("{}/codex/images/{endpoint}", self.chatgpt_base))
            .header("authorization", format!("Bearer {}", tokens.access))
            .header("chatgpt-account-id", tokens.account_id.as_deref().unwrap_or_default())
            .header("originator", "pi")
            .header("content-type", "application/json");
        let mut response = call.send(body.to_string()).map_err(|e| format!("Image request failed: {e}."))?;
        let status = response.status().as_u16();
        // ureq stops a body at 10 MB by default, and a large PNG passes that in base64.
        let text = response.body_mut().with_config().limit(64 * 1024 * 1024).read_to_string().map_err(|e| format!("Image request failed: {e}."))?;
        if status != 200 {
            // An error page (Cloudflare answers HTML) would otherwise land in the model's context whole.
            return Err(format!("API error {status}: {}", google::shown(&text)));
        }
        let answer: Value = serde_json::from_str(&text).map_err(|e| format!("The image response is not JSON: {e}."))?;
        let data = answer["data"][0]["b64_json"].as_str().ok_or("The image response has no image.")?;
        crate::image::unbase64(data).ok_or("The image is not valid base64.".into())
    }
}

fn image_http() -> ureq::Agent {
    ureq::Agent::config_builder().http_status_as_error(false).timeout_connect(Some(Duration::from_secs(30))).timeout_global(Some(IMAGE_TIMEOUT)).build().into()
}

/// Cloud Code Assist `streamGenerateContent` on the image model, answered with an image only; the fallback endpoint
/// takes over when the main one fails, as for a model turn.
fn antigravity_image(app: &google::App, tokens: &Tokens, model: &str, prompt: &str, inputs: &[Value]) -> Result<Vec<u8>, String> {
    let model = if model.is_empty() { google::image_model(app, tokens)? } else { model.to_string() };
    let body = google::image_request(&model, tokens.account_id.as_deref().unwrap_or_default(), prompt, inputs).to_string();
    let http = image_http();
    let send = |base: &str| {
        http.post(format!("{base}/v1internal:streamGenerateContent?alt=sse"))
            .header("authorization", format!("Bearer {}", tokens.access))
            .header("content-type", "application/json")
            .header("user-agent", google::user_agent())
            .send(body.as_str())
    };
    let mut sent = send(app.base);
    let failed = sent.as_ref().map_or(true, |response| response.status() == 429 || response.status().is_server_error());
    if failed && let Some(fallback) = app.fallback {
        sent = send(fallback);
    }
    let response = sent.map_err(|e| format!("Image request failed: {e}."))?;
    let status = response.status().as_u16();
    let mut reader = BufReader::new(response.into_body().into_reader());
    if status != 200 {
        let mut text = String::new();
        let _ = reader.read_to_string(&mut text);
        return Err(format!("API error {status}: {}", google::shown(&text)));
    }
    first_inline_image(&mut reader)
}

/// The first `inlineData` part of a Gemini SSE stream that is not a draft thought; a refusal comes as text or a
/// `finishReason`, which the error carries.
fn first_inline_image(reader: &mut dyn BufRead) -> Result<Vec<u8>, String> {
    let mut texts = Vec::new();
    for line in reader.lines() {
        let line = line.map_err(|e| format!("Stream read failed: {e}."))?;
        let Some(data) = line.strip_prefix("data:").map(str::trim).filter(|d| !d.is_empty()) else { continue };
        let chunk: Value = serde_json::from_str(data).map_err(|e| format!("Bad stream event: {e}."))?;
        let candidate = &chunk["response"]["candidates"][0];
        for part in candidate["content"]["parts"].as_array().into_iter().flatten() {
            if part["thought"] != true
                && let Some(data) = part["inlineData"]["data"].as_str()
            {
                return crate::image::unbase64(data).ok_or("The image is not valid base64.".into());
            }
            texts.extend(part["text"].as_str().map(str::to_string));
        }
        // A safety block (IMAGE_SAFETY, PROHIBITED_CONTENT) names itself only here.
        if let Some(reason) = candidate["finishReason"].as_str().filter(|reason| *reason != "STOP") {
            texts.push(format!("(finishReason {reason})"));
        }
    }
    match texts.join(" ").trim() {
        "" => Err("Google answered without an image.".into()),
        text => Err(format!("Google answered without an image: {text}")),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::api::accounts::Account;
    use crate::image::tests::PIXEL;
    use std::io::Write;
    use std::sync::mpsc::Receiver;

    /// A server answering its requests with `replies` in order, the last one from then on; each request arrives on the
    /// receiver as its path, its headers and its JSON body.
    fn fake_server(replies: &[(u16, &str)]) -> (String, Receiver<(String, Value, Value)>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (sent, received) = std::sync::mpsc::channel();
        let mut replies: Vec<(u16, String)> = replies.iter().map(|(status, body)| (*status, body.to_string())).collect();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
                reader.read_line(&mut request_line).unwrap();
                let path = request_line.split(' ').nth(1).unwrap_or_default().to_string();
                let mut headers = serde_json::Map::new();
                let mut header = String::new();
                while reader.read_line(&mut header).unwrap() > 2 {
                    if let Some((name, value)) = header.split_once(':') {
                        headers.insert(name.to_lowercase(), json!(value.trim()));
                    }
                    header.clear();
                }
                // A GET, such as the Codex usage check, has no body.
                let length = headers.get("content-length").map_or(0, |length| length.as_str().unwrap().parse().unwrap());
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let _ = sent.send((path, Value::Object(headers), serde_json::from_slice(&body).unwrap_or_default()));
                let (status, reply) = if replies.len() > 1 { replies.remove(0) } else { replies[0].clone() };
                write!(stream, "HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}", reply.len()).unwrap();
            }
        });
        (base, received)
    }

    /// A client whose ChatGPT login talks to a `fake_server` answering with `replies`.
    pub(crate) fn fake_codex_client(home: &std::path::Path, replies: &[(u16, &str)]) -> (Client, Receiver<(String, Value, Value)>) {
        let (base, received) = fake_server(replies);
        let mut client = Client::new(home).unwrap();
        client.chatgpt_base = base;
        let tokens = Tokens { access: "acc3ss".into(), refresh: "r".into(), expires: u64::MAX, account_id: Some("acct-1".into()) };
        client.reload(vec![Account { provider: CODEX.into(), name: "work".into(), secret: Secret::OAuth(tokens) }]);
        (client, received)
    }

    pub(crate) fn pixel_reply() -> String {
        json!({"created": 1, "data": [{"b64_json": crate::image::base64(&PIXEL), "generation_id": "g1"}], "output_format": "png"}).to_string()
    }

    #[test]
    fn a_chatgpt_login_generates_on_gpt_image_2_and_returns_the_png() {
        let home = std::env::temp_dir().join(format!("agent-image-gen-{}", std::process::id()));
        let (client, requests) = fake_codex_client(&home, &[(200, &pixel_reply())]);
        let offered = [client.generates_images(""), client.generates_images("openai-codex/gpt-image-2"), client.generates_images("google-antigravity/gemini-3-pro-image")];

        let bytes = client.generate_image("", "a red dot", &[], &AtomicBool::new(false));
        std::fs::remove_dir_all(&home).ok();

        assert_eq!(offered, [true, true, false]);
        assert_eq!(bytes.unwrap(), PIXEL);
        let (path, headers, body) = requests.recv().unwrap();
        assert_eq!(path, "/codex/images/generations");
        assert_eq!(body, json!({"model": "gpt-image-2", "prompt": "a red dot"}));
        assert_eq!((&headers["authorization"], &headers["chatgpt-account-id"], &headers["originator"]), (&json!("Bearer acc3ss"), &json!("acct-1"), &json!("pi")));
    }

    #[test]
    fn input_images_make_it_an_edit_with_data_urls_on_the_named_model() {
        let home = std::env::temp_dir().join(format!("agent-image-edit-{}", std::process::id()));
        let (client, requests) = fake_codex_client(&home, &[(200, &pixel_reply())]);
        let input = json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "iVBO"}});

        let bytes = client.generate_image("openai-codex/gpt-image-1.5", "add a hat", &[input], &AtomicBool::new(false));
        std::fs::remove_dir_all(&home).ok();

        assert_eq!(bytes.unwrap(), PIXEL);
        let (path, _, body) = requests.recv().unwrap();
        assert_eq!(path, "/codex/images/edits");
        assert_eq!(body, json!({"model": "gpt-image-1.5", "prompt": "add a hat", "images": [{"image_url": "data:image/png;base64,iVBO"}]}));
    }

    #[test]
    fn a_rejected_request_and_a_missing_account_are_errors() {
        let home = std::env::temp_dir().join(format!("agent-image-errors-{}", std::process::id()));
        let (client, _requests) = fake_codex_client(&home, &[(400, r#"{"error": {"message": "nope"}}"#), (200, "{}")]);
        let rejected = client.generate_image("", "x", &[], &AtomicBool::new(false));
        let no_image = client.generate_image("", "x", &[], &AtomicBool::new(false));
        client.reload(Vec::new());
        let no_account = client.generate_image("", "x", &[], &AtomicBool::new(false));
        std::fs::remove_dir_all(&home).ok();

        assert_eq!(rejected.unwrap_err(), r#"API error 400: {"error": {"message": "nope"}}"#);
        assert_eq!(no_image.unwrap_err(), "The image response has no image.");
        assert!(!client.generates_images(""));
        assert_eq!(no_account.unwrap_err(), "No google-antigravity account. Use /login.");
    }

    #[test]
    fn an_image_over_ten_megabytes_in_base64_still_arrives() {
        let home = std::env::temp_dir().join(format!("agent-image-large-{}", std::process::id()));
        let large = vec![7; 8 << 20];
        let (client, _requests) = fake_codex_client(&home, &[(200, &json!({"data": [{"b64_json": crate::image::base64(&large)}]}).to_string())]);

        let bytes = client.generate_image("", "a huge red dot", &[], &AtomicBool::new(false));
        std::fs::remove_dir_all(&home).ok();

        assert_eq!(bytes.unwrap().len(), large.len());
    }

    #[test]
    fn esc_ends_the_wait_for_an_image_that_is_still_being_drawn() {
        let home = std::env::temp_dir().join(format!("agent-image-abort-{}", std::process::id()));
        // Bound but never accepting: the request goes out and no answer ever comes.
        let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let (mut client, _requests) = fake_codex_client(&home, &[(200, "{}")]);
        client.chatgpt_base = format!("http://{}", silent.local_addr().unwrap());
        let started = std::time::Instant::now();

        let aborted = client.generate_image("", "a red dot", &[], &AtomicBool::new(true));
        std::fs::remove_dir_all(&home).ok();

        assert_eq!(aborted.unwrap_err(), crate::agent::ABORTED);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn antigravity_looks_up_its_image_model_and_falls_back_when_the_main_endpoint_is_limited() {
        let listed = json!({"imageGenerationModelIds": ["", "gemini-3-pro-image"]}).to_string();
        let (main, main_requests) = fake_server(&[(200, &listed), (429, "{}")]);
        let image = json!({"response": {"candidates": [{"content": {"parts": [{"inlineData": {"mimeType": "image/png", "data": crate::image::base64(&PIXEL)}}]}}]}});
        let (fallback, fallback_requests) = fake_server(&[(200, &format!("data: {image}\n\n"))]);
        let tokens = Tokens { access: "acc3ss".into(), refresh: "r".into(), expires: u64::MAX, account_id: Some("proj-1".into()) };

        let bytes = antigravity_image(&google::tests::app_at(&main, &fallback), &tokens, "", "a red dot", &[]);

        assert_eq!(bytes.unwrap(), PIXEL);
        let (path, headers, body) = main_requests.recv().unwrap();
        assert_eq!((path.as_str(), body), ("/v1internal:fetchAvailableModels", json!({"project": "proj-1"})));
        assert_eq!(headers["authorization"], "Bearer acc3ss");
        let (path, _, limited) = main_requests.recv().unwrap();
        assert_eq!(path, "/v1internal:streamGenerateContent?alt=sse");
        assert_eq!(limited["model"], "gemini-3-pro-image");
        assert_eq!(fallback_requests.recv().unwrap().2, limited);
    }

    #[test]
    fn the_antigravity_stream_gives_its_first_final_image_or_the_refusal() {
        let data = crate::image::base64(&PIXEL);
        let stream = format!(
            "data: {}\n\ndata: {}\n\n",
            json!({"response": {"candidates": [{"content": {"parts": [{"text": "Here it is"}, {"thought": true, "inlineData": {"mimeType": "image/png", "data": "ZHJhZnQ="}}]}}]}}),
            json!({"response": {"candidates": [{"content": {"parts": [{"inlineData": {"mimeType": "image/png", "data": data}}]}}]}})
        );
        assert_eq!(first_inline_image(&mut stream.as_bytes()).unwrap(), PIXEL);
        let refusal = format!("data: {}\n\n", json!({"response": {"candidates": [{"content": {"parts": [{"text": "I cannot draw that."}]}, "finishReason": "STOP"}]}}));
        assert_eq!(first_inline_image(&mut refusal.as_bytes()).unwrap_err(), "Google answered without an image: I cannot draw that.");
        let blocked = format!("data: {}\n\n", json!({"response": {"candidates": [{"content": {"parts": []}, "finishReason": "IMAGE_SAFETY"}]}}));
        assert_eq!(first_inline_image(&mut blocked.as_bytes()).unwrap_err(), "Google answered without an image: (finishReason IMAGE_SAFETY)");
        assert_eq!(first_inline_image(&mut "".as_bytes()).unwrap_err(), "Google answered without an image.");
    }
}
