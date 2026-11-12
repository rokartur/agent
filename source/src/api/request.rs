use super::*;

/// The tools nearly every coding turn calls. Anthropic's tool search loads any other when the model asks for it,
/// so their definitions stay out of the context until then: about 6.5k tokens for the built-in ones alone.
const CORE_TOOLS: [&str; 7] = ["Read", "Bash", "Write", "ReplaceLines", "Edit", "InsertLines", "Grep"];

pub(super) fn defer_rare_tools(body: &mut Value) {
    // `body["tools"]` would insert `"tools": null` into a plain request, which the API rejects.
    let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) else { return };
    if tools.iter().all(|tool| CORE_TOOLS.contains(&tool["name"].as_str().unwrap_or_default())) {
        return;
    }
    // A deferred tool cannot carry cache_control, so the breakpoint moves to the last tool still loaded.
    let cache_control = tools.last_mut().and_then(|tool| tool.as_object_mut()?.remove("cache_control"));
    let mut deferred = Vec::new();
    for tool in tools.iter_mut().filter(|tool| !CORE_TOOLS.contains(&tool["name"].as_str().unwrap_or_default())) {
        tool["defer_loading"] = json!(true);
        deferred.push(tool["name"].as_str().unwrap_or_default().to_string());
    }
    tools.insert(0, json!({"type": "tool_search_tool_bm25_20251119", "name": "tool_search_tool_bm25"}));
    if let Some(cache_control) = cache_control {
        let loaded = tools.iter_mut().rev().find(|tool| tool.get("defer_loading").is_none()).expect("the tool search tool is loaded");
        loaded["cache_control"] = cache_control;
    }
    // The model sees nothing of a deferred tool, not even its name, so without this list it never searches for one.
    let names = format!("Load these tools with tool_search_tool_bm25, searching by name, before their first call: {}.", deferred.join(", "));
    body["system"].as_array_mut().expect("a request with tools has a system array").push(json!({"type": "text", "text": names}));
}

/// Builds a Messages request with cache breakpoints on the system prompt, the last tool, and the last message.
/// `cache_ttl` is "5m" or "1h"; all three breakpoints share it, since a 1h one after a 5m one is a 400.
/// `per_session_system` follows the cached system block, so a value that differs per session does not rewrite it.
/// `thinking` is pi's level; "off" sends no thinking config and "minimal" maps to the lowest effort.
#[allow(clippy::too_many_arguments)]
pub fn request(model: &str, max_tokens: u64, thinking: &str, cache_ttl: &str, system: &str, per_session_system: &str, tools: &Value, messages: &[Value]) -> Value {
    // Without ttl the API caches for 5 minutes.
    let cache_control = if cache_ttl == "1h" { json!({"type": "ephemeral", "ttl": "1h"}) } else { json!({"type": "ephemeral"}) };
    let mut tools = tools.clone();
    if let Some(last) = tools.as_array_mut().and_then(|t| t.last_mut()) {
        last["cache_control"] = cache_control.clone();
    }
    let mut messages = messages.to_vec();
    if let Some(block) = messages.last_mut().and_then(|m| m["content"].as_array_mut()).and_then(|c| c.last_mut()) {
        block["cache_control"] = cache_control.clone();
    }
    let mut request = json!({
        "model": model,
        "max_tokens": max_tokens,
        "stream": true,
        "system": [{"type": "text", "text": system, "cache_control": cache_control}],
        "tools": tools,
        "messages": messages,
    });
    if !per_session_system.is_empty() {
        request["system"].as_array_mut().unwrap().push(json!({"type": "text", "text": per_session_system}));
    }
    if thinking != "off" {
        let effort = if thinking == "minimal" { "low" } else { thinking };
        // Same as pi: thinking that no longer matches the history after compaction is dropped instead of failing with 400.
        request["thinking"] = json!({"type": "adaptive", "display": "summarized", "block_binding": {"prefix_mismatch_behavior": "drop_block"}});
        request["output_config"] = json!({"effort": effort});
    }
    request
}

/// A one-shot request without tools or thinking, used for compaction summaries.
pub fn plain_request(model: &str, system: &str, prompt: &str, max_tokens: u64) -> Value {
    json!({
        "model": model,
        "max_tokens": max_tokens,
        "stream": true,
        "system": [{"type": "text", "text": system}],
        "messages": [{"role": "user", "content": [{"type": "text", "text": prompt}]}],
    })
}
