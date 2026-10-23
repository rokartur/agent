use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;

use crate::api::models::{self, Model, Price};
use crate::api::{Reply, Usage};

/// /usage tabs that sum session records, in this order.
pub const SCOPES: [&str; 4] = ["Session", "24 hours", "7 days", "All"];

const DAY_MS: u64 = 24 * 60 * 60 * 1000;

/// The session record for one model request; `totals` sums them.
pub fn entry(model: &str, usage: Usage, timestamp: u64) -> Value {
    json!({"type": "usage", "model": model, "timestamp": timestamp, "input": usage.input, "output": usage.output, "cache_read": usage.cache_read, "cache_write": usage.cache_write})
}

/// `entry` for a reply, naming who served it: usagefleet books Claude subscription requests on `account` and dedups on `id`.
pub fn reply_entry(reply: &Reply, timestamp: u64) -> Value {
    let mut entry = entry(&reply.model, reply.usage, timestamp);
    entry["id"] = json!(reply.id);
    entry["provider"] = json!(reply.provider);
    entry["account"] = json!(reply.account_id);
    if let Some(tokens) = reply.cache_write_1h {
        entry["cache_write_1h"] = json!(tokens);
    }
    if let Some(billed) = reply.billed {
        entry["billed"] = json!(billed);
    }
    entry
}

pub fn from_entry(entry: &Value) -> Usage {
    let number = |key: &str| entry[key].as_u64().unwrap_or(0);
    Usage { input: number("input"), output: number("output"), cache_read: number("cache_read"), cache_write: number("cache_write") }
}

/// The token line /tasks shows: "↑1k ↓300 cache 40k".
pub fn brief(usage: Usage) -> String {
    format!("↑{} ↓{} cache {}", tokens(usage.input), tokens(usage.output), tokens(usage.cache_read))
}

/// Dollars at today's catalog prices, for requests the provider reported no `billed` cost for.
/// Priced per request, since a long prompt moves the whole request to the `long_prompt` rates.
pub fn cost(request: &Request, price: &Price) -> f64 {
    let usage = request.usage;
    let prompt = usage.input + usage.cache_read + usage.cache_write;
    let long = price.long_prompt.iter().filter(|(min, _)| prompt >= *min).max_by_key(|(min, _)| *min);
    let rates = long.map_or(&price.base, |(_, rates)| rates);
    let write_1h = request.cache_write_1h.min(usage.cache_write);
    let writes = (usage.cache_write - write_1h) as f64 * rates.cache_write + write_1h as f64 * 2.0 * rates.input;
    usage.input as f64 * rates.input + usage.output as f64 * rates.output + usage.cache_read as f64 * rates.cache_read + writes
}

/// None when a request has no billed cost and the catalog has no price for the model.
pub fn model_cost(total: &ModelTotal, catalog: &[Model]) -> Option<f64> {
    let price = models::price(catalog, &total.model);
    let mut sum = 0.0;
    for request in &total.requests {
        sum += match (request.billed, price) {
            (Some(billed), _) => billed,
            (None, Some(price)) => cost(request, price),
            (None, None) => return None,
        };
    }
    Some(sum)
}

/// The cost of the priced models in `totals`; None when no model has a price, so unknown never reads as $0.
pub fn total_cost(totals: &[ModelTotal], catalog: &[Model]) -> Option<f64> {
    let costs: Vec<f64> = totals.iter().filter_map(|total| model_cost(total, catalog)).collect();
    (!costs.is_empty()).then(|| costs.iter().sum())
}

/// "$0.420"
pub fn dollars(cost: f64) -> String {
    format!("${cost:.3}")
}

/// pi's token counts: 950, 14k, 1.2M.
pub fn tokens(count: u64) -> String {
    match count {
        n if n < 1000 => n.to_string(),
        n if n < 999_500 => format!("{}k", (n + 500) / 1000),
        n => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

#[derive(Default, Clone, PartialEq, Debug)]
pub struct ModelTotal {
    pub model: String,
    /// One per request, kept apart for `cost`.
    pub requests: Vec<Request>,
}

#[derive(Default, Clone, Copy, PartialEq, Debug)]
pub struct Request {
    pub usage: Usage,
    /// The part of `usage.cache_write` cached for 1h.
    pub cache_write_1h: u64,
    pub billed: Option<f64>,
}

impl ModelTotal {
    pub fn usage(&self) -> Usage {
        let mut sum = Usage::default();
        for request in &self.requests {
            sum.add(request.usage);
        }
        sum
    }
}

/// Sums the `usage` records of every session in `dir` for each of `SCOPES`, sorted by model.
pub fn totals(dir: &Path, session_id: &str, now_ms: u64) -> [Vec<ModelTotal>; SCOPES.len()] {
    let mut by_model: [BTreeMap<String, ModelTotal>; SCOPES.len()] = Default::default();
    let Ok(files) = std::fs::read_dir(dir) else { return Default::default() };
    for path in files.flatten().map(|file| file.path()) {
        let current = path.file_stem().is_some_and(|stem| stem == session_id);
        for (model, request, timestamp) in records(&path) {
            let age = now_ms.saturating_sub(timestamp);
            for (scope, inside) in by_model.iter_mut().zip([current, age < DAY_MS, age < 7 * DAY_MS, true]) {
                if inside {
                    add(scope, &model, request);
                }
            }
        }
    }
    by_model.map(|scope| scope.into_values().collect())
}

/// The Session scope of `totals` for one session file, cheap enough to read after every turn.
pub fn session_totals(path: &Path) -> Vec<ModelTotal> {
    let mut by_model = BTreeMap::new();
    for (model, request, _) in records(path) {
        add(&mut by_model, &model, request);
    }
    by_model.into_values().collect()
}

fn add(by_model: &mut BTreeMap<String, ModelTotal>, model: &str, request: Request) {
    let total = by_model.entry(model.to_string()).or_insert_with(|| ModelTotal { model: model.to_string(), ..Default::default() });
    total.requests.push(request);
}

/// (model, request, timestamp) of each `usage` record in a session file; none when it cannot be read.
fn records(path: &Path) -> Vec<(String, Request, u64)> {
    let Ok(text) = std::fs::read_to_string(path) else { return Vec::new() };
    let mut records = Vec::new();
    // Skips the JSON parse of the far more common message lines; serde_json writes keys without spaces.
    for line in text.lines().filter(|line| line.contains(r#""type":"usage""#)) {
        let Ok(entry) = serde_json::from_str::<Value>(line) else { continue };
        if entry["type"] != "usage" {
            continue;
        }
        let model = entry["model"].as_str().unwrap_or("unknown").to_string();
        let request = Request { usage: from_entry(&entry), cache_write_1h: entry["cache_write_1h"].as_u64().unwrap_or(0), billed: entry["billed"].as_f64() };
        records.push((model, request, entry["timestamp"].as_u64().unwrap_or(0)));
    }
    records
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::models::Rates;

    #[test]
    fn sums_usage_records_per_model_for_each_scope() {
        let dir = std::env::temp_dir().join(format!("agent-usage-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let now = 100 * DAY_MS;
        let used = |input| Usage { input, output: 1, cache_read: 2, cache_write: 3 };
        let current = [json!({"type": "message", "message": {"role": "user", "content": "\"type\":\"usage\""}}), entry("opus", used(10), now - 1000), entry("opus", used(20), now - 2 * DAY_MS)];
        let other = [entry("haiku", used(5), now - 1000), entry("opus", used(40), now - 8 * DAY_MS)];
        for (name, lines) in [("cur", &current[..]), ("old", &other[..])] {
            std::fs::write(dir.join(format!("{name}.jsonl")), lines.iter().map(|l| format!("{l}\n")).collect::<String>()).unwrap();
        }

        let [session, day, week, all] = totals(&dir, "cur", now);
        std::fs::remove_dir_all(&dir).unwrap();

        // read_dir order differs between file systems, so requests compare as sorted inputs.
        let inputs = |totals: &[ModelTotal]| -> Vec<(String, Vec<u64>)> {
            let sorted = |t: &ModelTotal| {
                let mut inputs: Vec<u64> = t.requests.iter().map(|r| r.usage.input).collect();
                inputs.sort();
                inputs
            };
            totals.iter().map(|t| (t.model.clone(), sorted(t))).collect()
        };
        let expect = |rows: &[(&str, &[u64])]| -> Vec<(String, Vec<u64>)> { rows.iter().map(|(m, i)| (m.to_string(), i.to_vec())).collect() };
        assert_eq!(inputs(&session), expect(&[("opus", &[10, 20])]));
        assert_eq!(inputs(&day), expect(&[("haiku", &[5]), ("opus", &[10])]));
        assert_eq!(inputs(&week), expect(&[("haiku", &[5]), ("opus", &[10, 20])]));
        assert_eq!(inputs(&all), expect(&[("haiku", &[5]), ("opus", &[10, 20, 40])]));
        assert_eq!(all[1].usage(), Usage { input: 70, output: 3, cache_read: 6, cache_write: 9 });
    }

    const OPUS: Rates = Rates { input: 4e-6, output: 20e-6, cache_read: 0.2e-6, cache_write: 5e-6 };

    fn priced(id: &str, price: Price) -> Model {
        Model { provider: "openrouter".into(), id: id.into(), price: Some(price), ..Model::default() }
    }

    #[test]
    fn costs_every_token_kind_and_leaves_unpriced_models_without_a_cost() {
        let used = Usage { input: 1_000_000, output: 100_000, cache_read: 10_000_000, cache_write: 200_000 };
        let catalog = [priced("anthropic/claude-opus-5.5", Price { base: OPUS, long_prompt: Vec::new() })];
        let total = |model: &str| ModelTotal { model: model.into(), requests: vec![Request { usage: used, ..Request::default() }] };
        assert_eq!(total_cost(&[total("unknown-model")], &catalog), None);
        assert!((total_cost(&[total("claude-opus-5-5"), total("unknown-model")], &catalog).unwrap() - 9.0).abs() < 1e-9);
    }

    #[test]
    fn prices_each_request_at_the_rates_of_its_prompt_length() {
        let double = Rates { input: 8e-6, output: 40e-6, cache_read: 0.4e-6, cache_write: 10e-6 };
        let catalog = [priced("openai/gpt-6-sol", Price { base: OPUS, long_prompt: vec![(272_000, double)] })];
        let request = |cache_read| Request { usage: Usage { input: 1_000, output: 1_000, cache_read, cache_write: 1_000 }, ..Request::default() };
        let total = ModelTotal { model: "gpt-6-sol".into(), requests: vec![request(100_000), request(300_000)] };
        let short = 1_000.0 * (4e-6 + 20e-6 + 5e-6) + 100_000.0 * 0.2e-6;
        let long = 1_000.0 * (8e-6 + 40e-6 + 10e-6) + 300_000.0 * 0.4e-6;
        assert!((model_cost(&total, &catalog).unwrap() - (short + long)).abs() < 1e-9);
    }

    #[test]
    fn bills_1h_cache_writes_at_twice_input_and_prefers_the_billed_cost() {
        let price = Price { base: OPUS, long_prompt: Vec::new() };
        let writes = Request { usage: Usage { cache_write: 1_000, ..Usage::default() }, cache_write_1h: 400, billed: None };
        assert!((cost(&writes, &price) - (600.0 * 5e-6 + 400.0 * 8e-6)).abs() < 1e-12);
        let billed = Request { billed: Some(0.5), ..writes };
        let total = ModelTotal { model: "unpriced".into(), requests: vec![billed, billed] };
        assert_eq!(model_cost(&total, &[]), Some(1.0));
        assert_eq!(model_cost(&ModelTotal { requests: vec![writes], ..total }, &[]), None);
    }
}
