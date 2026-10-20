use serde_json::{Value, json};
use std::io::BufRead;
use std::sync::atomic::{AtomicBool, Ordering};

use super::{ApiError, Delta, Reply, Usage, parse_tool_input};

/// Turns an Anthropic Messages request into a Chat Completions body for `model`, the id without the provider prefix.
/// Thinking, cache breakpoints and `speed` have no Chat Completions field and are dropped.
pub fn request(anthropic: &Value, model: &str) -> Value {
    let system: Vec<&str> = anthropic["system"].as_array().into_iter().flatten().filter_map(|b| b["text"].as_str()).collect();
    let mut messages = vec![json!({"role": "system", "content": system.join("\n\n")})];
    for message in anthropic["messages"].as_array().into_iter().flatten() {
        if message["role"] == "assistant" {
            messages.push(assistant_message(&message["content"]));
        } else {
            messages.extend(user_messages(&message["content"]));
        }
    }
    let tools: Vec<Value> = anthropic["tools"]
        .as_array()
        .into_iter()
        .flatten()
        // Anthropic server tools (web search) have a `type` and no schema; a Chat Completions endpoint cannot run them.
        .filter(|tool| tool["input_schema"].is_object())
        .map(|tool| json!({"type": "function", "function": {"name": tool["name"], "description": tool["description"], "parameters": tool["input_schema"]}}))
        .collect();
    let mut body = json!({
        "model": model,
        "messages": messages,
        "max_tokens": anthropic["max_tokens"],
        "stream": true,
        "stream_options": {"include_usage": true},
    });
    if !tools.is_empty() {
        body["tools"] = json!(tools);
    }
    body
}

fn assistant_message(content: &Value) -> Value {
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    for block in content.as_array().into_iter().flatten() {
        match block["type"].as_str() {
            Some("text") => text.push_str(block["text"].as_str().unwrap_or_default()),
            Some("tool_use") => tool_calls.push(json!({"id": block["id"], "type": "function", "function": {"name": block["name"], "arguments": block["input"].to_string()}})),
            _ => {}
        }
    }
    let mut message = json!({"role": "assistant", "content": text});
    if !tool_calls.is_empty() {
        message["tool_calls"] = json!(tool_calls);
    }
    message
}

/// Tool results become `tool` messages, which must follow the assistant's call directly; the rest of the turn,
/// images from tool results included, follows them as one user message.
fn user_messages(content: &Value) -> Vec<Value> {
    let Some(blocks) = content.as_array() else { return vec![json!({"role": "user", "content": content})] };
    let mut messages = Vec::new();
    let mut parts = Vec::new();
    for block in blocks {
        match block["type"].as_str() {
            Some("text") => parts.push(json!({"type": "text", "text": block["text"]})),
            Some("image") => parts.push(image_part(block)),
            Some("tool_result") => {
                let (text, images) = tool_output(&block["content"]);
                messages.push(json!({"role": "tool", "tool_call_id": block["tool_use_id"], "content": text}));
                parts.extend(images);
            }
            _ => {}
        }
    }
    if !parts.is_empty() {
        messages.push(json!({"role": "user", "content": parts}));
    }
    messages
}

fn tool_output(content: &Value) -> (String, Vec<Value>) {
    let Some(blocks) = content.as_array() else { return (content.as_str().unwrap_or_default().to_string(), Vec::new()) };
    let text = blocks.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n");
    let images = blocks.iter().filter(|b| b["type"] == "image").map(image_part).collect();
    (text, images)
}

fn image_part(block: &Value) -> Value {
    let source = &block["source"];
    let url = format!("data:{};base64,{}", source["media_type"].as_str().unwrap_or_default(), source["data"].as_str().unwrap_or_default());
    json!({"type": "image_url", "image_url": {"url": url}})
}

/// Reads Chat Completions SSE into the Anthropic reply shape the agent stores: one text block, then tool_use blocks.
pub fn parse_stream(reader: &mut dyn BufRead, abort: &AtomicBool, on_delta: &mut dyn FnMut(Delta)) -> Result<Reply, ApiError> {
    let mut text = String::new();
    // (id, name, arguments) by the chunk's `index`; arguments arrive in pieces.
    let mut calls: Vec<(String, String, String)> = Vec::new();
    let mut usage = Usage::default();
    let mut stop_reason = String::new();
    let mut id = String::new();
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
        // Some servers close the stream after the last chunk without `[DONE]`.
        if read == 0 && !stop_reason.is_empty() {
            break;
        }
        if read == 0 {
            return Err(ApiError::retryable("Stream ended before finish_reason.".into()));
        }
        let Some(data) = line.strip_prefix("data:").map(str::trim) else { continue };
        if data == "[DONE]" {
            break;
        }
        let chunk: Value = serde_json::from_str(data).map_err(|e| ApiError::fatal(format!("Bad stream event: {e}.")))?;
        if !chunk["error"].is_null() {
            return Err(ApiError::fatal(format!("API stream error: {}", chunk["error"])));
        }
        if let Some(chunk_id) = chunk["id"].as_str() {
            id = chunk_id.to_string();
        }
        if chunk["usage"].is_object() {
            let cached = chunk["usage"]["prompt_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0);
            usage.input = chunk["usage"]["prompt_tokens"].as_u64().unwrap_or(0).saturating_sub(cached);
            usage.cache_read = cached;
            usage.output = chunk["usage"]["completion_tokens"].as_u64().unwrap_or(0);
        }
        let Some(choice) = chunk["choices"].get(0) else { continue };
        let delta = &choice["delta"];
        if let Some(piece) = delta["content"].as_str() {
            on_delta(Delta::Text(piece));
            text.push_str(piece);
        }
        for call in delta["tool_calls"].as_array().into_iter().flatten() {
            let index = call["index"].as_u64().unwrap_or(calls.len() as u64) as usize;
            if calls.len() <= index {
                calls.resize(index + 1, (String::new(), String::new(), String::new()));
            }
            let (call_id, name, arguments) = &mut calls[index];
            call_id.push_str(call["id"].as_str().unwrap_or_default());
            name.push_str(call["function"]["name"].as_str().unwrap_or_default());
            arguments.push_str(call["function"]["arguments"].as_str().unwrap_or_default());
        }
        stop_reason = match choice["finish_reason"].as_str() {
            Some("tool_calls") => "tool_use".into(),
            Some("length") => "max_tokens".into(),
            Some(_) => "end_turn".into(),
            None => stop_reason,
        };
    }
    let mut blocks = Vec::new();
    if !text.is_empty() {
        blocks.push(json!({"type": "text", "text": text}));
    }
    if stop_reason != "aborted" {
        for (index, (call_id, name, arguments)) in calls.into_iter().enumerate() {
            let input = if arguments.trim().is_empty() { json!({}) } else { parse_tool_input(&arguments)? };
            // llama.cpp and some Ollama versions send no call id.
            let call_id = if call_id.is_empty() { format!("call_{index}") } else { call_id };
            blocks.push(json!({"type": "tool_use", "id": call_id, "name": name, "input": input}));
        }
    }
    // Some servers finish tool calls with "stop".
    if stop_reason == "end_turn" && blocks.iter().any(|b| b["type"] == "tool_use") {
        stop_reason = "tool_use".into();
    }
    let context_tokens = usage.context_tokens();
    Ok(Reply { message: json!({"role": "assistant", "content": blocks}), stop_reason, usage, context_tokens, id, ..Reply::default() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anthropic_history_becomes_chat_messages() {
        let anthropic = json!({
            "model": "ollama/qwen3",
            "max_tokens": 1000,
            "system": [{"type": "text", "text": "Be brief."}, {"type": "text", "text": "cwd: /x"}],
            "thinking": {"type": "adaptive"},
            "speed": "fast",
            "tools": [{"name": "read", "description": "Read a file", "input_schema": {"type": "object"}}, {"type": "web_search_20250305", "name": "web_search"}],
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "look"}, {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "iVBO"}}]},
                {"role": "assistant", "content": [{"type": "thinking", "thinking": "hm"}, {"type": "text", "text": "Reading."}, {"type": "tool_use", "id": "call_1", "name": "read", "input": {"path": "a.png"}}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "call_1", "content": [{"type": "text", "text": "a.png"}, {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "QUJD"}}]}, {"type": "text", "text": "and?"}]},
            ],
        });
        assert_eq!(
            request(&anthropic, "qwen3"),
            json!({
                "model": "qwen3",
                "max_tokens": 1000,
                "stream": true,
                "stream_options": {"include_usage": true},
                "tools": [{"type": "function", "function": {"name": "read", "description": "Read a file", "parameters": {"type": "object"}}}],
                "messages": [
                    {"role": "system", "content": "Be brief.\n\ncwd: /x"},
                    {"role": "user", "content": [{"type": "text", "text": "look"}, {"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBO"}}]},
                    {"role": "assistant", "content": "Reading.", "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "read", "arguments": "{\"path\":\"a.png\"}"}}]},
                    {"role": "tool", "tool_call_id": "call_1", "content": "a.png"},
                    {"role": "user", "content": [{"type": "image_url", "image_url": {"url": "data:image/png;base64,QUJD"}}, {"type": "text", "text": "and?"}]},
                ],
            })
        );
    }

    #[test]
    fn tool_call_arguments_split_across_chunks_join_into_one_input() {
        let stream = [
            r#"data: {"id":"c1","choices":[{"index":0,"delta":{"content":"Let me look."}}]}"#,
            r#"data: {"id":"c1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_9","type":"function","function":{"name":"read","arguments":"{\"pa"}}]}}]}"#,
            r#"data: {"id":"c1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"a.rs\"}"}}]},"finish_reason":"tool_calls"}]}"#,
            r#"data: {"id":"c1","choices":[],"usage":{"prompt_tokens":120,"completion_tokens":30,"prompt_tokens_details":{"cached_tokens":100}}}"#,
            "data: [DONE]",
        ]
        .join("\n\n");
        let mut shown = String::new();
        let reply = parse_stream(&mut stream.as_bytes(), &AtomicBool::new(false), &mut |delta| {
            if let Delta::Text(text) = delta {
                shown.push_str(text);
            }
        })
        .unwrap_or_else(|e| panic!("{}", e.message));
        assert_eq!(shown, "Let me look.");
        assert_eq!(reply.stop_reason, "tool_use");
        assert_eq!(reply.message["content"], json!([{"type": "text", "text": "Let me look."}, {"type": "tool_use", "id": "call_9", "name": "read", "input": {"path": "a.rs"}}]));
        assert_eq!(reply.usage, Usage { input: 20, output: 30, cache_read: 100, cache_write: 0 });
        assert_eq!(reply.context_tokens, 150);
    }
}
