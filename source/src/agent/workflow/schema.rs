use super::*;

/// The JSON in an agent's answer: the whole answer, a fenced block, or the outermost object or array in it.
pub(super) fn parse_json_answer(answer: &str) -> Result<Value, String> {
    let text = answer.trim();
    let unfenced = text.strip_prefix("```").map(|rest| rest.trim_start_matches(|c: char| c.is_ascii_alphabetic()).trim().trim_end_matches("```").trim());
    if let Some(value) = [Some(text), unfenced].into_iter().flatten().find_map(|candidate| serde_json::from_str(candidate).ok()) {
        return Ok(value);
    }
    let start = text.find(['{', '[']).ok_or("the answer has no JSON")?;
    let end = text.rfind(['}', ']']).filter(|end| *end > start).ok_or("the answer has no JSON")?;
    serde_json::from_str(&text[start..=end]).map_err(|e| format!("the answer is not valid JSON: {e}"))
}

/// A subset of JSON Schema: type, required, properties, additionalProperties, items, enum, minItems, maxItems.
pub(super) fn validate(value: &Value, schema: &Value, at: &str) -> Result<(), String> {
    if let Some(choices) = schema["enum"].as_array()
        && !choices.contains(value)
    {
        return Err(format!("{at} must be one of {}", Value::from(choices.clone())));
    }
    let types: Vec<&str> = match &schema["type"] {
        Value::String(one) => vec![one.as_str()],
        Value::Array(many) => many.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    if !types.is_empty() && !types.iter().any(|kind| has_type(value, kind)) {
        return Err(format!("{at} must be {}, got {}", types.join(" or "), type_name(value)));
    }
    if let Value::Object(object) = value {
        for key in schema["required"].as_array().into_iter().flatten().filter_map(Value::as_str) {
            if !object.contains_key(key) {
                return Err(format!("{at} is missing the required key `{key}`"));
            }
        }
        for (key, item) in object {
            match schema["properties"].get(key) {
                Some(property) => validate(item, property, &format!("{at}.{key}"))?,
                None if schema["additionalProperties"] == false => return Err(format!("{at} has the key `{key}`, which the schema does not allow")),
                None => {}
            }
        }
    }
    if let Value::Array(items) = value {
        if let Some(min) = schema["minItems"].as_u64().filter(|min| (items.len() as u64) < *min) {
            return Err(format!("{at} needs at least {min} items"));
        }
        if let Some(max) = schema["maxItems"].as_u64().filter(|max| items.len() as u64 > *max) {
            return Err(format!("{at} takes at most {max} items"));
        }
        if schema["items"].is_object() {
            for (index, item) in items.iter().enumerate() {
                validate(item, &schema["items"], &format!("{at}[{index}]"))?;
            }
        }
    }
    Ok(())
}

fn has_type(value: &Value, kind: &str) -> bool {
    match kind {
        "integer" => value.is_i64() || value.is_u64(),
        _ => type_name(value) == kind || (kind == "number" && value.is_number()),
    }
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// A schema no value can satisfy, found before the agent starts: a required key that `additionalProperties: false` rules out.
pub(super) fn contradiction(schema: &Value) -> Option<String> {
    if schema["additionalProperties"] == false {
        let ruled_out = schema["required"].as_array().into_iter().flatten().filter_map(Value::as_str).find(|key| schema["properties"].get(*key).is_none());
        if let Some(key) = ruled_out {
            return Some(format!("`{key}` is required, but `additionalProperties: false` rules it out"));
        }
    }
    let nested = schema["properties"].as_object().into_iter().flat_map(|properties| properties.values()).chain([&schema["items"]]);
    nested.filter(|inner| inner.is_object()).find_map(contradiction)
}
