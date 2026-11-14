use serde_json::Value;

/// /context rows, in this order.
pub const PARTS: [&str; 5] = ["System prompt", "Tool schemas", "Messages", "Tool results", "Images"];
const SYSTEM: usize = 0;
const TOOLS: usize = 1;
const MESSAGES: usize = 2;
const TOOL_RESULTS: usize = 3;
const IMAGES: usize = 4;

/// One number per `PARTS` row.
pub type PerPart = [u64; PARTS.len()];

// Anthropic caps an image near 1,600 tokens; at ~4 bytes per token that is 6,400 bytes, far below its base64 size.
const IMAGE_BYTES: u64 = 6_400;

/// Splits `total` context tokens across `PARTS` in proportion to the bytes each part sends.
pub fn split(system: &str, tools: &Value, messages: &[Value], total: u64) -> PerPart {
    let bytes = bytes(system, tools, messages);
    let sum: u64 = bytes.iter().sum();
    let mut parts = bytes.map(|part| (part * total).checked_div(sum).unwrap_or(0));
    // Each part rounds down on its own; the last non-empty part takes the remainder so the rows add up to `total`.
    if let Some(last) = (0..PARTS.len()).rev().find(|&index| bytes[index] > 0) {
        parts[last] += total - parts.iter().sum::<u64>();
    }
    parts
}

const INSTRUCTION_TAGS: [(&str, &str); 6] =
    [("addendum", "APPEND_SYSTEM.md"), ("skills", "Skills list"), ("ponytail", "Ponytail mode"), ("caveman", "Caveman mode"), ("cwd", "Working directory"), ("computer", "Remote device")];

/// What the system prompt is made of, as (label, tokens) with `tokens` split by bytes: each AGENTS.md by path,
/// each tagged section from `prompt` and `modes`, and the untagged rest (SYSTEM.md or the default, output style, plan mode).
pub fn instructions(system: &str, tokens: u64) -> Vec<(String, u64)> {
    let mut sections = Vec::new();
    for block in system.split("<project_instructions path=\"").skip(1) {
        let Some((path, body)) = block.split_once("\">\n") else { continue };
        let body = body.split_once("\n</project_instructions>").map_or(body, |(body, _)| body);
        sections.push((path.to_string(), body.len()));
    }
    for (tag, label) in INSTRUCTION_TAGS {
        // The last match, since these sections follow the AGENTS.md files, whose text could quote a tag.
        let Some((_, after)) = system.rsplit_once(&format!("<{tag}>\n")) else { continue };
        let Some((body, _)) = after.split_once(&format!("\n</{tag}>")) else { continue };
        sections.push((label.to_string(), body.len()));
    }
    let listed: usize = sections.iter().map(|(_, bytes)| bytes).sum();
    sections.insert(0, ("Preamble".to_string(), system.len().saturating_sub(listed)));
    let total = system.len() as u64;
    sections.into_iter().filter(|(_, bytes)| *bytes > 0).map(|(label, bytes)| (label, (bytes as u64 * tokens).checked_div(total).unwrap_or(0))).collect()
}

pub const BYTES_PER_TOKEN: u64 = 4;

/// Context tokens a request would use, at ~4 bytes per token, for when no reply has reported them yet.
pub fn estimate(system: &str, tools: &Value, messages: &[Value]) -> u64 {
    bytes(system, tools, messages).iter().sum::<u64>() / BYTES_PER_TOKEN
}

const MIN_OUTPUT_TOKENS: u64 = 4_096;

/// `max_tokens` cut to what fits in `window` after `input`: providers reject a request whose input plus max_tokens
/// exceeds the window, and OpenRouter lists max outputs near the whole window (Kimi K3.1: 943,718 of 1,048,576).
pub fn output_tokens(max_tokens: u64, window: u64, input: u64) -> u64 {
    // `input` is counted at 4 bytes per token; code and Polish text run nearer 3.
    let room = window.saturating_sub(input + input / 3);
    max_tokens.min(room.max(MIN_OUTPUT_TOKENS))
}

/// The context limit a provider's "too long" error names: "maximum context length is N tokens" (OpenRouter, OpenAI),
/// "prompt is too long: X tokens > N maximum" and "exceed context limit: X + Y > N" (Anthropic).
pub fn limit_in_error(error: &str) -> Option<u64> {
    if let Some((_, rest)) = error.split_once("maximum context length is ") {
        return leading_number(rest);
    }
    if !error.contains("prompt is too long") && !error.contains("exceed context limit") {
        return None;
    }
    leading_number(error.split_once(" > ")?.1)
}

fn leading_number(text: &str) -> Option<u64> {
    let digits: String = text.chars().take_while(|c| c.is_ascii_digit() || *c == ',').filter(|c| *c != ',').collect();
    digits.parse().ok()
}

fn bytes(system: &str, tools: &Value, messages: &[Value]) -> PerPart {
    let mut bytes = [0; PARTS.len()];
    bytes[SYSTEM] = system.len() as u64;
    bytes[TOOLS] = tools.to_string().len() as u64;
    for message in messages {
        let Some(blocks) = message["content"].as_array() else {
            bytes[MESSAGES] += message["content"].to_string().len() as u64;
            continue;
        };
        for block in blocks {
            match block["type"].as_str() {
                Some("image") => bytes[IMAGES] += IMAGE_BYTES,
                Some("tool_result") => add_tool_result(&mut bytes, &block["content"]),
                _ => bytes[MESSAGES] += block.to_string().len() as u64,
            }
        }
    }
    bytes
}

fn add_tool_result(bytes: &mut PerPart, content: &Value) {
    let Some(blocks) = content.as_array() else {
        bytes[TOOL_RESULTS] += content.to_string().len() as u64;
        return;
    };
    for block in blocks {
        if block["type"] == "image" {
            bytes[IMAGES] += IMAGE_BYTES;
        } else {
            bytes[TOOL_RESULTS] += block.to_string().len() as u64;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn split_scales_each_part_by_its_bytes_and_counts_images_by_estimate() {
        let image = json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "A".repeat(100_000)}});
        let messages = [
            json!({"role": "user", "content": "x".repeat(98)}),
            json!({"role": "assistant", "content": [{"type": "tool_use", "id": "t1", "name": "Read", "input": {}}]}),
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": [{"type": "text", "text": "y"}, image]}]}),
        ];
        let tool_use_bytes = messages[1]["content"][0].to_string().len() as u64;
        let result_bytes = json!({"type": "text", "text": "y"}).to_string().len() as u64;
        let tools = json!([]);
        let bytes = [100, 2, 100 + tool_use_bytes, result_bytes, IMAGE_BYTES];
        let sum: u64 = bytes.iter().sum();

        assert_eq!(split(&"s".repeat(100), &tools, &messages, sum * 2), bytes.map(|part| part * 2));
        assert_eq!(split("", &tools, &[], 0), [0; 5]);
        assert_eq!(split(&"s".repeat(100), &tools, &messages, 7).iter().sum::<u64>(), 7);
        assert_eq!(estimate(&"s".repeat(100), &tools, &messages), sum / 4, "the base64 image counts by estimate, not its 100 KB");
    }

    #[test]
    fn instructions_list_each_agents_md_and_tagged_section() {
        let agents = "a".repeat(40);
        let system = format!(
            "{}\n\n<project_context>\n<project_instructions path=\"/p/AGENTS.md\">\n{agents}\n</project_instructions>\n</project_context>\n\n<skills>\n{}\n</skills>\n<cwd>\n/p\n</cwd>",
            "p".repeat(20),
            "s".repeat(10)
        );
        let rows = instructions(&system, system.len() as u64);
        let labels: Vec<&str> = rows.iter().map(|(label, _)| label.as_str()).collect();
        assert_eq!(labels, ["Preamble", "/p/AGENTS.md", "Skills list", "Working directory"]);
        assert_eq!(rows[1].1, 40);
        assert_eq!(rows[2].1, 10);
        assert_eq!(rows.iter().map(|(_, tokens)| tokens).sum::<u64>(), system.len() as u64);
        assert_eq!(instructions("", 0), []);
    }

    #[test]
    fn output_tokens_leave_the_input_room_in_the_window() {
        assert_eq!(output_tokens(943_718, 1_048_576, 30_000), 943_718);
        assert_eq!(output_tokens(943_718, 1_048_576, 90_000), 928_576, "the session 419cc652 overflow");
        assert_eq!(output_tokens(128_000, 200_000, 150_000), 4_096, "a full window still asks for a usable reply");
        assert_eq!(output_tokens(2_000, 200_000, 150_000), 2_000);
    }

    #[test]
    fn limit_in_error_reads_each_providers_wording() {
        let openrouter = r#"API error 400: {"error":{"message":"This endpoint's maximum context length is 262144 tokens. However, you requested about 1063412 tokens (119694 of text input, 943718 in the output).","code":400}}"#;
        let anthropic = r#"API error 400: {"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 205000 tokens > 200000 maximum"}}"#;
        let anthropic_output = "API error 400: input length and `max_tokens` exceed context limit: 188240 + 21333 > 200000, decrease input length or `max_tokens` and try again";
        assert_eq!(limit_in_error(openrouter), Some(262_144));
        assert_eq!(limit_in_error("API error 400: This model's maximum context length is 128,000 tokens."), Some(128_000));
        assert_eq!(limit_in_error(anthropic), Some(200_000));
        assert_eq!(limit_in_error(anthropic_output), Some(200_000));
        assert_eq!(limit_in_error("API error 400: max_tokens: 5 > 4 is not allowed"), None);
    }
}
