use super::*;

/// No byte for this long means the stream died without a TCP reset; ureq's `timeout_recv_body` caps the whole body instead.
pub(super) const STREAM_IDLE: Duration = Duration::from_secs(120);

/// `body` read on its own thread, so a stalled stream fails after `idle` and Esc ends it at once.
/// A read on a stalled body that a caller gives up on stays blocked on that thread until the socket closes.
pub(super) fn watched_body(mut body: impl std::io::Read + Send + 'static, abort: &AtomicBool, idle: Duration) -> BufReader<WatchedBody<'_>> {
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

pub(super) struct WatchedBody<'a> {
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

pub(super) fn parse_stream(reader: &mut dyn BufRead, abort: &AtomicBool, on_delta: &mut dyn FnMut(Delta)) -> Result<Reply, ApiError> {
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
                    && (block["type"] == "tool_use" || block["type"] == "server_tool_use")
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
                return Err(ApiError { message: format!("API stream error: {}", event["error"]), retryable, quota, code: String::new() });
            }
            _ => {}
        }
    }
    Ok(Reply { message: json!({"role": "assistant", "content": blocks}), stop_reason, context_tokens: prompt_tokens + usage.output, usage, cache_write_1h, id, bad_tool_inputs, ..Reply::default() })
}

pub(super) fn append(field: &mut Value, text: &str) {
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
pub(super) fn parse_tool_input(id: &str, raw: &str, bad: &mut Vec<BadToolInput>) -> Value {
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
