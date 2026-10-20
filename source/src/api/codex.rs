use serde_json::{Value, json};
use std::io::BufRead;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::api::{ApiError, Delta, Reply, Usage};

/// Turns an Anthropic Messages request into a Codex Responses body, like pi-ai's openai-codex-responses.
/// shortcut: reasoning items are not kept in history, so they are never replayed; keep them if answers lose the thread.
pub fn request(anthropic: &Value, cache_key: &str) -> Value {
    let system: Vec<&str> = anthropic["system"].as_array().into_iter().flatten().filter_map(|b| b["text"].as_str()).collect();
    let mut input = Vec::new();
    for (index, message) in anthropic["messages"].as_array().into_iter().flatten().enumerate() {
        let blocks = message["content"].as_array().cloned().unwrap_or_else(|| vec![json!({"type": "text", "text": message["content"]})]);
        if message["role"] == "assistant" {
            for block in &blocks {
                match block["type"].as_str() {
                    Some("text") => input.push(json!({
                        "type": "message", "role": "assistant", "id": format!("msg_{index}"), "status": "completed",
                        "content": [{"type": "output_text", "text": block["text"], "annotations": []}],
                    })),
                    Some("tool_use") => input.push(json!({"type": "function_call", "call_id": block["id"], "name": block["name"], "arguments": block["input"].to_string()})),
                    _ => {}
                }
            }
            continue;
        }
        let mut parts = Vec::new();
        for block in &blocks {
            match block["type"].as_str() {
                Some("tool_result") => input.push(json!({"type": "function_call_output", "call_id": block["tool_use_id"], "output": tool_output(&block["content"])})),
                Some("text") => parts.push(json!({"type": "input_text", "text": block["text"]})),
                Some("image") => parts.push(input_image(block)),
                _ => {}
            }
        }
        if !parts.is_empty() {
            input.push(json!({"role": "user", "content": parts}));
        }
    }
    let tools: Vec<Value> = anthropic["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|tool| json!({"type": "function", "name": tool["name"], "description": tool["description"], "parameters": tool["input_schema"]}))
        .collect();
    let effort = match anthropic["output_config"]["effort"].as_str() {
        None => "none",
        Some("max") => "xhigh",
        Some(effort) => effort,
    };
    let reasoning = if effort == "none" { json!({"effort": "none"}) } else { json!({"effort": effort, "summary": "auto"}) };
    let mut body = json!({
        "model": anthropic["model"],
        "store": false,
        "stream": true,
        "instructions": system.join("\n\n"),
        "input": input,
        "text": {"verbosity": "low"},
        "prompt_cache_key": cache_key,
        "tool_choice": "auto",
        "parallel_tool_calls": true,
        "reasoning": reasoning,
    });
    if !tools.is_empty() {
        body["tools"] = json!(tools);
    }
    match anthropic["speed"].as_str() {
        Some("fast") => body["service_tier"] = json!("priority"),
        Some("ultrafast") => body["service_tier"] = json!("ultrafast"),
        _ => {}
    }
    body
}

/// Plain text unless the result carries an image, which Responses takes as an `input_image` part, like pi-ai.
fn tool_output(content: &Value) -> Value {
    let Some(blocks) = content.as_array() else { return json!(content.as_str().unwrap_or_default()) };
    if !blocks.iter().any(|b| b["type"] == "image") {
        return json!(blocks.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n"));
    }
    let mut parts = Vec::new();
    for block in blocks {
        match block["type"].as_str() {
            Some("text") => parts.push(json!({"type": "input_text", "text": block["text"]})),
            Some("image") => parts.push(input_image(block)),
            _ => {}
        }
    }
    json!(parts)
}

fn input_image(block: &Value) -> Value {
    let source = &block["source"];
    let url = format!("data:{};base64,{}", source["media_type"].as_str().unwrap_or_default(), source["data"].as_str().unwrap_or_default());
    json!({"type": "input_image", "image_url": url})
}

/// Reads Responses SSE into the Anthropic reply shape the agent stores: text and tool_use blocks.
pub fn parse_stream(reader: &mut dyn BufRead, abort: &AtomicBool, on_delta: &mut dyn FnMut(Delta)) -> Result<Reply, ApiError> {
    let mut slots: Vec<Option<Value>> = Vec::new();
    let mut arguments: Vec<String> = Vec::new();
    let mut usage = Usage::default();
    let mut id = String::new();
    let mut stop_reason: String;
    let mut line = String::new();
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
            return Err(ApiError::retryable("Stream ended before response.completed.".into()));
        }
        let Some(data) = line.strip_prefix("data:").map(str::trim).filter(|d| *d != "[DONE]") else { continue };
        let event: Value = serde_json::from_str(data).map_err(|e| ApiError::fatal(format!("Bad stream event: {e}.")))?;
        // Bounded, so a bad index cannot make resize allocate gigabytes.
        let index = event["output_index"].as_u64().unwrap_or(0).min(1023) as usize;
        if slots.len() <= index {
            slots.resize(index + 1, None);
            arguments.resize(index + 1, String::new());
        }
        let item = &event["item"];
        match event["type"].as_str().unwrap_or_default() {
            "response.output_item.added" => {
                slots[index] = match item["type"].as_str() {
                    Some("message") => Some(json!({"type": "text", "text": ""})),
                    // A call joins the reply only once output_item.done brings its arguments; a cut stream must not run it with `{}`.
                    _ => None,
                };
            }
            "response.output_text.delta" => {
                let text = event["delta"].as_str().unwrap_or_default();
                on_delta(Delta::Text(text));
                if let Some(Value::String(joined)) = slots[index].as_mut().and_then(|block| block.get_mut("text")) {
                    joined.push_str(text);
                }
            }
            "response.reasoning_summary_text.delta" => on_delta(Delta::Thinking(event["delta"].as_str().unwrap_or_default())),
            "response.function_call_arguments.delta" => arguments[index].push_str(event["delta"].as_str().unwrap_or_default()),
            "response.output_item.done" => match item["type"].as_str() {
                Some("function_call") => {
                    let raw = item["arguments"].as_str().filter(|a| !a.is_empty()).unwrap_or(&arguments[index]);
                    let input = super::parse_tool_input(if raw.is_empty() { "{}" } else { raw })?;
                    slots[index] = Some(json!({"type": "tool_use", "id": item["call_id"], "name": item["name"], "input": input}));
                }
                Some("message") => {
                    let parts = item["content"].as_array().into_iter().flatten();
                    let text: String = parts.filter_map(|p| p["text"].as_str().or(p["refusal"].as_str())).collect();
                    slots[index] = Some(json!({"type": "text", "text": text}));
                }
                _ => {}
            },
            "response.completed" | "response.incomplete" => {
                let response = &event["response"];
                id = response["id"].as_str().unwrap_or_default().to_string();
                let cached = response["usage"]["input_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0);
                usage.input = response["usage"]["input_tokens"].as_u64().unwrap_or(0).saturating_sub(cached);
                usage.cache_read = cached;
                usage.output = response["usage"]["output_tokens"].as_u64().unwrap_or(0);
                stop_reason = match response["incomplete_details"]["reason"].as_str() {
                    Some("max_output_tokens") => "max_tokens".into(),
                    Some(reason) => return Err(ApiError::fatal(format!("Response incomplete: {reason}"))),
                    None => "end_turn".into(),
                };
                break;
            }
            "response.failed" => return Err(stream_error(&event["response"]["error"])),
            "error" => return Err(stream_error(&event)),
            _ => {}
        }
    }
    let blocks: Vec<Value> = slots.into_iter().flatten().filter(|b| b["type"] != "text" || b["text"] != "").collect();
    if stop_reason == "end_turn" && blocks.iter().any(|b| b["type"] == "tool_use") {
        stop_reason = "tool_use".into();
    }
    Ok(Reply { message: json!({"role": "assistant", "content": blocks}), stop_reason, context_tokens: usage.context_tokens(), usage, id, ..Reply::default() })
}

/// Classified like the Anthropic stream's `error`: a rate limit switches accounts, an overloaded server retries.
fn stream_error(error: &Value) -> ApiError {
    let code = error["code"].as_str().unwrap_or("unknown");
    let message = format!("Codex error {code}: {}", error["message"].as_str().unwrap_or("no message"));
    let quota = matches!(code, "rate_limit_exceeded" | "usage_limit_reached" | "insufficient_quota");
    let retryable = quota || matches!(code, "server_error" | "server_is_overloaded" | "slow_down");
    ApiError { message, retryable, quota }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anthropic_history_becomes_responses_input() {
        let anthropic = json!({
            "model": "gpt-6-sol",
            "system": [{"type": "text", "text": "Be brief.", "cache_control": {"type": "ephemeral"}}],
            "tools": [{"name": "bash", "description": "Run", "input_schema": {"type": "object"}, "cache_control": {"type": "ephemeral"}}],
            "messages": [
                {"role": "user", "content": [{"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "iVBO"}}, {"type": "text", "text": "hi"}]},
                {"role": "assistant", "content": [{"type": "thinking", "thinking": "hm", "signature": "s"}, {"type": "text", "text": "ok"}, {"type": "tool_use", "id": "toolu_1", "name": "bash", "input": {"command": "ls"}}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_1", "content": [{"type": "text", "text": "a.txt"}]}, {"type": "text", "text": "[summary]"}]},
            ],
            "output_config": {"effort": "max"},
        });
        let body = request(&anthropic, "key");
        assert_eq!(body["instructions"], "Be brief.");
        assert_eq!(body["reasoning"], json!({"effort": "xhigh", "summary": "auto"}));
        assert_eq!(body["tools"], json!([{"type": "function", "name": "bash", "description": "Run", "parameters": {"type": "object"}}]));
        assert_eq!(
            body["input"],
            json!([
                {"role": "user", "content": [{"type": "input_image", "image_url": "data:image/png;base64,iVBO"}, {"type": "input_text", "text": "hi"}]},
                {"type": "message", "role": "assistant", "id": "msg_1", "status": "completed", "content": [{"type": "output_text", "text": "ok", "annotations": []}]},
                {"type": "function_call", "call_id": "toolu_1", "name": "bash", "arguments": "{\"command\":\"ls\"}"},
                {"type": "function_call_output", "call_id": "toolu_1", "output": "a.txt"},
                {"role": "user", "content": [{"type": "input_text", "text": "[summary]"}]},
            ])
        );
        assert_eq!(request(&json!({"model": "m", "messages": []}), "k")["reasoning"], json!({"effort": "none"}));
    }

    #[test]
    fn speed_becomes_the_service_tier() {
        let tier = |speed: Value| request(&json!({"model": "m", "messages": [], "speed": speed}), "k")["service_tier"].clone();
        assert_eq!(tier(json!("fast")), "priority");
        assert_eq!(tier(json!("ultrafast")), "ultrafast");
        assert_eq!(tier(Value::Null), Value::Null);
    }

    #[test]
    fn an_image_tool_result_keeps_its_image() {
        let content = json!([{"type": "text", "text": "a.png"}, {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "iVBO"}}]);
        assert_eq!(tool_output(&content), json!([{"type": "input_text", "text": "a.png"}, {"type": "input_image", "image_url": "data:image/png;base64,iVBO"}]));
    }

    #[test]
    fn stream_becomes_text_and_tool_use_blocks() {
        let events = [
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_1"}}"#,
            r#"{"type":"response.reasoning_summary_text.delta","output_index":0,"delta":"think"}"#,
            r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"message","id":"msg_1"}}"#,
            r#"{"type":"response.output_text.delta","output_index":1,"delta":"Hi"}"#,
            r#"{"type":"response.output_item.added","output_index":2,"item":{"type":"function_call","call_id":"call_1","name":"bash","arguments":""}}"#,
            r#"{"type":"response.function_call_arguments.delta","output_index":2,"delta":"{\"command\":\"ls\"}"}"#,
            r#"{"type":"response.output_item.done","output_index":2,"item":{"type":"function_call","call_id":"call_1","name":"bash","arguments":""}}"#,
            r#"{"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":100,"output_tokens":7,"input_tokens_details":{"cached_tokens":60}}}}"#,
        ];
        let body: String = events.iter().map(|e| format!("event: x\ndata: {e}\n\n")).collect();
        let mut streamed = String::new();
        let on_delta = &mut |delta: Delta| match delta {
            Delta::Text(t) => streamed.push_str(t),
            Delta::Thinking(t) => streamed.push_str(&format!("[{t}]")),
            Delta::Notice(_) => {}
        };
        let reply = parse_stream(&mut body.as_bytes(), &AtomicBool::new(false), on_delta).ok().unwrap();
        assert_eq!(streamed, "[think]Hi");
        assert_eq!(reply.stop_reason, "tool_use");
        assert_eq!((reply.usage.input, reply.usage.cache_read, reply.usage.output), (40, 60, 7));
        assert_eq!(reply.message["content"], json!([{"type": "text", "text": "Hi"}, {"type": "tool_use", "id": "call_1", "name": "bash", "input": {"command": "ls"}}]));
    }

    #[test]
    fn an_aborted_stream_drops_a_tool_call_whose_arguments_never_arrived() {
        struct Cut;
        impl std::io::Read for Cut {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::ConnectionReset.into())
            }
        }
        let added = r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"call_1","name":"bash","arguments":""}}"#;
        let body = format!("data: {added}\n\n");
        let mut reader = std::io::BufReader::new(std::io::Read::chain(body.as_bytes(), Cut));
        let reply = parse_stream(&mut reader, &AtomicBool::new(true), &mut |_| {}).ok().unwrap();
        assert_eq!((reply.stop_reason.as_str(), &reply.message["content"]), ("aborted", &json!([])));
    }

    #[test]
    fn stream_errors_retry_only_when_the_server_is_busy() {
        let parse = |event: &str| parse_stream(&mut format!("data: {event}\n\n").as_bytes(), &AtomicBool::new(false), &mut |_| {}).err().unwrap();
        let busy = parse(r#"{"type":"response.failed","response":{"error":{"code":"server_is_overloaded","message":"busy"}}}"#);
        assert!(busy.retryable && !busy.quota, "{}", busy.message);
        let limited = parse(r#"{"type":"error","code":"rate_limit_exceeded","message":"slow"}"#);
        assert!(limited.retryable && limited.quota, "{}", limited.message);
        let bad = parse(r#"{"type":"error","code":"invalid_prompt","message":"no"}"#);
        assert!(!bad.retryable, "{}", bad.message);
    }
}
