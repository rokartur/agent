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
pub const CODEX_MODEL: &str = "gpt-image-2";
const CODEX: &str = "openai-codex";
/// An image answer comes whole once drawn, 20 s for a small one; `Client::http` gives up on headers after 2 minutes.
const IMAGE_TIMEOUT: Duration = Duration::from_secs(300);

impl Client {
    /// Whether an account can generate images, which is when the ImageGenerate tool is offered.
    pub fn generates_images(&self) -> bool {
        self.state.lock().unwrap().accounts.iter().any(|a| a.is_oauth() && [CODEX, google::ANTIGRAVITY].contains(&a.provider.as_str()))
    }

    /// The bytes of the image `prompt` describes, or with `inputs` (image blocks) an edit of them. `image_model` is the
    /// setting: `google-antigravity/<id>`, else a Codex image model id; empty takes `gpt-image-2` on a ChatGPT login
    /// and the first image model an Antigravity account lists otherwise. Setting `abort` ends the wait with `ABORTED`.
    pub fn generate_image(&self, image_model: &str, prompt: &str, inputs: &[Value], abort: &AtomicBool) -> Result<Vec<u8>, String> {
        let (provider, model) = match image_model.strip_prefix(&format!("{}/", google::ANTIGRAVITY)) {
            Some(id) => (google::ANTIGRAVITY, id),
            None if image_model.is_empty() => {
                let codex = self.state.lock().unwrap().accounts.iter().any(|a| a.provider == CODEX && a.is_oauth());
                if codex { (CODEX, CODEX_MODEL) } else { (google::ANTIGRAVITY, "") }
            }
            None => (CODEX, image_model),
        };
        let account = self.likely_account(provider).ok_or(format!("No {provider} account. Use /login."))?;
        let account = self.fresh(account)?;
        let Secret::OAuth(tokens) = &account.secret else { return Err(format!("{provider} accounts need /login, not an API key.")) };
        // The request blocks until the image is drawn, so it runs on its own thread and Esc ends the wait, not the thread.
        let (client, tokens, model, prompt, inputs) = (self.clone(), tokens.clone(), model.to_string(), prompt.to_string(), inputs.to_vec());
        let (sent, received) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let image = if provider == google::ANTIGRAVITY { client.antigravity_image(&tokens, &model, &prompt, &inputs) } else { client.codex_image(&tokens, &model, &prompt, &inputs) };
            let _ = sent.send(image);
        });
        loop {
            match received.recv_timeout(Duration::from_millis(100)) {
                Ok(image) => return image,
                Err(RecvTimeoutError::Timeout) if abort.load(Ordering::Relaxed) => return Err(crate::agent::ABORTED.into()),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Err("The image request ended without an answer.".into()),
            }
        }
    }

    fn image_http() -> ureq::Agent {
        ureq::Agent::config_builder().http_status_as_error(false).timeout_global(Some(IMAGE_TIMEOUT)).build().into()
    }

    /// `POST /codex/images/generations`, or `/edits` with the input images, as the Codex CLI's image extension sends them.
    fn codex_image(&self, tokens: &Tokens, model: &str, prompt: &str, inputs: &[Value]) -> Result<Vec<u8>, String> {
        let mut body = json!({"model": model, "prompt": prompt});
        let endpoint = if inputs.is_empty() { "generations" } else { "edits" };
        if !inputs.is_empty() {
            body["images"] = inputs.iter().map(|block| json!({"image_url": super::codex::data_url(block)})).collect();
        }
        let call = Self::image_http()
            .post(format!("{}/codex/images/{endpoint}", self.chatgpt_base))
            .header("authorization", format!("Bearer {}", tokens.access))
            .header("chatgpt-account-id", tokens.account_id.as_deref().unwrap_or_default())
            .header("originator", "pi")
            .header("content-type", "application/json");
        let response = call.send(body.to_string()).map_err(|e| format!("Image request failed: {e}."))?;
        let status = response.status().as_u16();
        let text = response.into_body().read_to_string().map_err(|e| format!("Image request failed: {e}."))?;
        if status != 200 {
            return Err(format!("API error {status}: {}", text.trim()));
        }
        let answer: Value = serde_json::from_str(&text).map_err(|e| format!("The image response is not JSON: {e}."))?;
        let data = answer["data"][0]["b64_json"].as_str().ok_or("The image response has no image.")?;
        crate::image::unbase64(data).ok_or("The image is not valid base64.".into())
    }

    /// Cloud Code Assist `streamGenerateContent` on the image model, answered with an image only; the sandbox endpoint
    /// takes over when the main one fails, as for a model turn.
    fn antigravity_image(&self, tokens: &Tokens, model: &str, prompt: &str, inputs: &[Value]) -> Result<Vec<u8>, String> {
        let app = &google::ANTIGRAVITY_APP;
        let model = if model.is_empty() { google::image_model(app, tokens)? } else { model.to_string() };
        let body = google::image_request(&model, tokens.account_id.as_deref().unwrap_or_default(), prompt, inputs).to_string();
        let mut sent = None;
        for base in [app.base].into_iter().chain(app.fallback) {
            let call = Self::image_http()
                .post(format!("{base}/v1internal:streamGenerateContent?alt=sse"))
                .header("authorization", format!("Bearer {}", tokens.access))
                .header("content-type", "application/json")
                .header("user-agent", google::user_agent());
            let response = call.send(body.as_str());
            let failed = response.as_ref().map_or(true, |r| r.status() == 429 || r.status().is_server_error());
            sent = Some(response);
            if !failed {
                break;
            }
        }
        let response = sent.expect("the app has a base endpoint").map_err(|e| format!("Image request failed: {e}."))?;
        let status = response.status().as_u16();
        let mut reader = BufReader::new(response.into_body().into_reader());
        if status != 200 {
            let mut text = String::new();
            let _ = reader.read_to_string(&mut text);
            return Err(format!("API error {status}: {}", text.trim()));
        }
        first_inline_image(&mut reader)
    }
}

/// The first `inlineData` part of a Gemini SSE stream; a refusal comes as text, which the error carries.
fn first_inline_image(reader: &mut dyn BufRead) -> Result<Vec<u8>, String> {
    let mut texts = Vec::new();
    for line in reader.lines() {
        let line = line.map_err(|e| format!("Stream read failed: {e}."))?;
        let Some(data) = line.strip_prefix("data:").map(str::trim).filter(|d| !d.is_empty()) else { continue };
        let chunk: Value = serde_json::from_str(data).map_err(|e| format!("Bad stream event: {e}."))?;
        for part in chunk["response"]["candidates"][0]["content"]["parts"].as_array().into_iter().flatten() {
            if let Some(data) = part["inlineData"]["data"].as_str() {
                return crate::image::unbase64(data).ok_or("The image is not valid base64.".into());
            }
            texts.extend(part["text"].as_str().map(str::to_string));
        }
    }
    let text = texts.join(" ");
    if text.trim().is_empty() {
        return Err("Google answered without an image.".into());
    }
    Err(format!("Google answered without an image: {}", text.trim()))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::api::accounts::Account;
    use crate::image::tests::PIXEL;
    use std::io::Write;
    use std::sync::mpsc::Receiver;

    /// A client whose ChatGPT login talks to a server answering every request with `reply`; each request arrives on the
    /// receiver as its path, its headers and its JSON body.
    pub(crate) fn fake_codex_client(home: &std::path::Path, reply: &str) -> (Client, Receiver<(String, Value, Value)>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (sent, received) = std::sync::mpsc::channel();
        let reply = reply.to_string();
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
                let length = headers["content-length"].as_str().unwrap().parse().unwrap();
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let _ = sent.send((path, Value::Object(headers), serde_json::from_slice(&body).unwrap()));
                write!(stream, "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}", reply.len()).unwrap();
            }
        });
        let mut client = Client::new(home).unwrap();
        client.chatgpt_base = base;
        let tokens = Tokens { access: "acc3ss".into(), refresh: "r".into(), expires: u64::MAX, account_id: Some("acct-1".into()) };
        client.reload(vec![Account { provider: CODEX.into(), name: "work".into(), secret: Secret::OAuth(tokens) }]);
        (client, received)
    }

    fn pixel_reply() -> String {
        json!({"created": 1, "data": [{"b64_json": crate::image::base64(&PIXEL), "generation_id": "g1"}], "output_format": "png"}).to_string()
    }

    #[test]
    fn a_chatgpt_login_generates_on_gpt_image_2_and_returns_the_png() {
        let home = std::env::temp_dir().join(format!("agent-image-gen-{}", std::process::id()));
        let (client, requests) = fake_codex_client(&home, &pixel_reply());
        assert!(client.generates_images());

        let bytes = client.generate_image("", "a red dot", &[], &AtomicBool::new(false));
        std::fs::remove_dir_all(&home).ok();

        assert_eq!(bytes.unwrap(), PIXEL);
        let (path, headers, body) = requests.recv().unwrap();
        assert_eq!(path, "/codex/images/generations");
        assert_eq!(body, json!({"model": "gpt-image-2", "prompt": "a red dot"}));
        assert_eq!((&headers["authorization"], &headers["chatgpt-account-id"], &headers["originator"]), (&json!("Bearer acc3ss"), &json!("acct-1"), &json!("pi")));
    }

    #[test]
    fn input_images_make_it_an_edit_with_data_urls_on_the_named_model() {
        let home = std::env::temp_dir().join(format!("agent-image-edit-{}", std::process::id()));
        let (client, requests) = fake_codex_client(&home, &pixel_reply());
        let input = json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "iVBO"}});

        let bytes = client.generate_image("gpt-image-1.5", "add a hat", &[input], &AtomicBool::new(false));
        std::fs::remove_dir_all(&home).ok();

        assert_eq!(bytes.unwrap(), PIXEL);
        let (path, _, body) = requests.recv().unwrap();
        assert_eq!(path, "/codex/images/edits");
        assert_eq!(body, json!({"model": "gpt-image-1.5", "prompt": "add a hat", "images": [{"image_url": "data:image/png;base64,iVBO"}]}));
    }

    #[test]
    fn a_rejected_request_and_a_missing_account_are_errors() {
        let home = std::env::temp_dir().join(format!("agent-image-errors-{}", std::process::id()));
        let (client, _requests) = fake_codex_client(&home, r#"{"error": {"message": "nope"}}"#);
        let no_image = client.generate_image("", "x", &[], &AtomicBool::new(false));
        client.reload(Vec::new());
        let no_account = client.generate_image("", "x", &[], &AtomicBool::new(false));
        std::fs::remove_dir_all(&home).ok();

        assert_eq!(no_image.unwrap_err(), "The image response has no image.");
        assert!(!client.generates_images());
        assert_eq!(no_account.unwrap_err(), "No google-antigravity account. Use /login.");
    }

    #[test]
    fn esc_ends_the_wait_for_an_image_that_is_still_being_drawn() {
        let home = std::env::temp_dir().join(format!("agent-image-abort-{}", std::process::id()));
        // Bound but never accepting: the request goes out and no answer ever comes.
        let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let (mut client, _requests) = fake_codex_client(&home, "{}");
        client.chatgpt_base = format!("http://{}", silent.local_addr().unwrap());
        let started = std::time::Instant::now();

        let aborted = client.generate_image("", "a red dot", &[], &AtomicBool::new(true));
        std::fs::remove_dir_all(&home).ok();

        assert_eq!(aborted.unwrap_err(), crate::agent::ABORTED);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn the_antigravity_stream_gives_its_first_inline_image_or_the_refusal() {
        let data = crate::image::base64(&PIXEL);
        let stream = format!(
            "data: {}\n\ndata: {}\n\n",
            json!({"response": {"candidates": [{"content": {"parts": [{"text": "Here it is"}]}}]}}),
            json!({"response": {"candidates": [{"content": {"parts": [{"inlineData": {"mimeType": "image/png", "data": data}}]}}]}})
        );
        assert_eq!(first_inline_image(&mut stream.as_bytes()).unwrap(), PIXEL);
        let refusal = format!("data: {}\n\n", json!({"response": {"candidates": [{"content": {"parts": [{"text": "I cannot draw that."}]}}], "finishReason": "STOP"}}));
        assert_eq!(first_inline_image(&mut refusal.as_bytes()).unwrap_err(), "Google answered without an image: I cannot draw that.");
        assert_eq!(first_inline_image(&mut "".as_bytes()).unwrap_err(), "Google answered without an image.");
    }
}
