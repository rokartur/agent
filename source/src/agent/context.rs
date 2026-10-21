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

pub const BYTES_PER_TOKEN: u64 = 4;

/// Context tokens a request would use, at ~4 bytes per token, for when no reply has reported them yet.
pub fn estimate(system: &str, tools: &Value, messages: &[Value]) -> u64 {
    bytes(system, tools, messages).iter().sum::<u64>() / BYTES_PER_TOKEN
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
}
