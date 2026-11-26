use serde_json::{Value, json};

/// Results the model must keep: the user's answers, its task list and goal, and the Skill tool's instructions.
const PROTECTED_TOOLS: [&str; 4] = ["AskUserQuestion", "Todo", "Goal", "Skill"];
/// Tool results a cut leaves in place: the observation window of "The Complexity Trap" (arXiv 2508.21433).
const KEEP_RECENT: usize = 10;
/// Every request with a cut is far above this, so the server clears on each one and the cleared set stays put.
const TRIGGER_TOKENS: u64 = 1000;

/// Ids of the tool uses a cut can clear, oldest first.
pub fn clearable(messages: &[Value]) -> Vec<&str> {
    let blocks = messages.iter().flat_map(|message| message["content"].as_array().into_iter().flatten());
    blocks.filter(|block| block["type"] == "tool_use" && !PROTECTED_TOOLS.contains(&block["name"].as_str().unwrap_or_default())).filter_map(|block| block["id"].as_str()).collect()
}

/// The tool use the cut clears through, moved to all but the newest KEEP_RECENT only when the prompt cache is cold. Any move
/// rewrites the cache from the system prompt on; on a warm cache that cost more than the cheaper reads after it saved, in
/// moves near compaction too (`docs/condense-upstream.md`, "Benchmark").
pub fn advance(cut: Option<&str>, clearable: &[&str], cache_cold: bool) -> Option<String> {
    let cleared = cleared(cut, clearable);
    let target = clearable.len().saturating_sub(KEEP_RECENT);
    let moved = if cache_cold && target > cleared { target } else { cleared };
    moved.checked_sub(1).map(|last| clearable[last].to_string())
}

/// Anthropic's context editing for the cut; `api` applies the same edit itself for other providers. Server-side clearing
/// keeps later thinking valid, and `keep` counts the uses after the cut, so the cleared set holds still between moves.
pub fn edits(cut: Option<&str>, clearable: &[&str]) -> Option<Value> {
    let cleared = cleared(cut, clearable);
    if cleared == 0 {
        return None;
    }
    Some(json!({"edits": [{
        "type": "clear_tool_uses_20250919",
        "trigger": {"type": "input_tokens", "value": TRIGGER_TOKENS},
        "keep": {"type": "tool_uses", "value": clearable.len() - cleared},
        "exclude_tools": PROTECTED_TOOLS,
    }]}))
}

/// How many of `clearable` the cut clears: none once a rewind or compaction removed its tool use.
fn cleared(cut: Option<&str>, clearable: &[&str]) -> usize {
    cut.and_then(|id| clearable.iter().position(|candidate| *candidate == id)).map_or(0, |at| at + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uses(count: usize) -> Vec<String> {
        (1..=count).map(|n| format!("u{n}")).collect()
    }

    #[test]
    fn protected_tools_are_never_clearable() {
        let messages = [json!({"role": "assistant", "content": [
            {"type": "tool_use", "id": "a", "name": "Read"},
            {"type": "tool_use", "id": "b", "name": "Todo"},
            {"type": "text", "text": "x"},
            {"type": "tool_use", "id": "c", "name": "Bash"},
        ]})];
        assert_eq!(clearable(&messages), ["a", "c"]);
    }

    #[test]
    fn the_cut_moves_only_on_a_cold_cache_and_keeps_the_newest() {
        let ids = uses(25);
        let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
        assert_eq!(advance(None, &ids, false), None);
        assert_eq!(advance(Some("u5"), &ids, false).as_deref(), Some("u5"));
        assert_eq!(advance(Some("u5"), &ids, true).as_deref(), Some("u15"));
        assert_eq!(advance(Some("u15"), &ids, true).as_deref(), Some("u15"));
        assert_eq!(advance(None, &ids[..10], true), None);
        assert_eq!(advance(Some("gone"), &ids[..8], true), None);
    }

    #[test]
    fn keep_grows_with_each_use_so_the_same_oldest_results_stay_cleared() {
        let ids = uses(20);
        let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
        assert_eq!(edits(None, &ids), None);
        assert_eq!(edits(Some("u6"), &ids).unwrap()["edits"][0]["keep"]["value"], 14);
        assert_eq!(edits(Some("u6"), &ids[..18]).unwrap()["edits"][0]["keep"]["value"], 12);
        assert_eq!(edits(Some("gone"), &ids), None);
    }
}
