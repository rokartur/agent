use serde_json::Value;

/// The Codex model list has no output limit; every Codex model takes this many.
const CODEX_MAX_TOKENS: u64 = 128_000;

/// The providers this agent can talk to.
pub const PROVIDERS: [&str; 3] = ["anthropic", "openai-codex", "openrouter"];
/// OpenRouter's routers (`openrouter/auto`) list no output limit.
const OPENROUTER_MAX_TOKENS: u64 = 32_000;

#[derive(Clone, Default)]
pub struct Model {
    pub provider: String,
    pub id: String,
    pub name: String,
    pub context_window: u64,
    pub max_tokens: u64,
    /// Names of the accounts whose live model list has this model; empty for OpenRouter's public list.
    pub accounts: Vec<String>,
    /// OpenRouter's list price; None elsewhere, since Anthropic and OpenAI list none (see `price`).
    pub price: Option<Price>,
}

#[derive(Clone, PartialEq, Debug)]
pub struct Price {
    pub base: Rates,
    /// OpenRouter's `overrides` as (min prompt tokens, rates): Haiku 5.5 costs 5x from 100k, GPT models 2x from 272k.
    pub long_prompt: Vec<(u64, Rates)>,
}

/// Dollars per token.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Rates {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

/// Adds one account's live list to the catalog, so each model knows every account that can use it.
pub fn merge(catalog: &mut Vec<Model>, listed: Vec<Model>, account: &str) {
    for model in listed {
        match catalog.iter_mut().find(|m| m.provider == model.provider && m.id == model.id) {
            Some(known) => known.accounts.push(account.into()),
            None => catalog.push(Model { accounts: vec![account.into()], ..model }),
        }
    }
}

/// Whether the account may get requests for this model. An account no list came from (a failed fetch,
/// a /login after startup, any OpenRouter key) is unchecked, so the API decides.
pub fn serves(catalog: &[Model], model: &Model, account: &str) -> bool {
    let checked = catalog.iter().any(|m| m.provider == model.provider && m.accounts.iter().any(|a| a == account));
    !checked || model.accounts.iter().any(|a| a == account)
}

/// Anthropic's `GET /v1/models`, newest first.
pub fn parse_anthropic(body: &Value) -> Vec<Model> {
    let mut models = Vec::new();
    for m in body["data"].as_array().into_iter().flatten() {
        let (Some(id), Some(context_window), Some(max_tokens)) = (m["id"].as_str(), m["max_input_tokens"].as_u64(), m["max_tokens"].as_u64()) else { continue };
        let name = m["display_name"].as_str().unwrap_or(id);
        models.push(Model { provider: "anthropic".into(), id: id.into(), name: name.into(), context_window, max_tokens, accounts: Vec::new(), price: None });
    }
    models
}

/// Codex's `GET /codex/models`, in picker order; hidden models are internal.
pub fn parse_codex(body: &Value) -> Vec<Model> {
    let mut models = Vec::new();
    for m in body["models"].as_array().into_iter().flatten() {
        let (Some(id), Some(context_window)) = (m["slug"].as_str(), m["context_window"].as_u64()) else { continue };
        if m["visibility"] != "list" {
            continue;
        }
        let name = m["display_name"].as_str().unwrap_or(id);
        models.push(Model { provider: "openai-codex".into(), id: id.into(), name: name.into(), context_window, max_tokens: CODEX_MAX_TOKENS, accounts: Vec::new(), price: None });
    }
    models
}

/// OpenRouter's `GET /v1/models`, newest first; models without tool calling cannot run the agent.
pub fn parse_openrouter(body: &Value) -> Vec<Model> {
    let mut models = Vec::new();
    for m in body["data"].as_array().into_iter().flatten() {
        let (Some(id), Some(context_window)) = (m["id"].as_str(), m["context_length"].as_u64()) else { continue };
        if !m["supported_parameters"].as_array().is_some_and(|p| p.iter().any(|p| p == "tools")) {
            continue;
        }
        let name = m["name"].as_str().unwrap_or(id);
        let max_tokens = m["top_provider"]["max_completion_tokens"].as_u64().unwrap_or(OPENROUTER_MAX_TOKENS);
        models.push(Model { provider: "openrouter".into(), id: id.into(), name: name.into(), context_window, max_tokens, accounts: Vec::new(), price: openrouter_price(&m["pricing"]) });
    }
    models
}

/// None for routers like `openrouter/auto`, which list -1 since the routed model decides.
fn openrouter_price(pricing: &Value) -> Option<Price> {
    let mut long_prompt = Vec::new();
    for tier in pricing["overrides"].as_array().into_iter().flatten() {
        long_prompt.push((tier["min_prompt_tokens"].as_u64()?, openrouter_rates(tier)?));
    }
    Some(Price { base: openrouter_rates(pricing)?, long_prompt })
}

fn openrouter_rates(pricing: &Value) -> Option<Rates> {
    let rate = |key: &str| pricing[key].as_str()?.parse::<f64>().ok().filter(|rate| rate.is_finite());
    let (input, output) = (rate("prompt")?, rate("completion")?);
    if input < 0.0 || output < 0.0 {
        return None;
    }
    // Models without cache rates report no cache tokens, so the fallback only matters if that changes.
    Some(Rates { input, output, cache_read: rate("input_cache_read").unwrap_or(input), cache_write: rate("input_cache_write").unwrap_or(input) })
}

/// The price of a model id from usage records. Native Anthropic and Codex models take their OpenRouter twin's,
/// `claude-opus-5-5` from `anthropic/claude-opus-5.5`, since their own model lists have no prices.
pub fn price<'a>(catalog: &'a [Model], id: &str) -> Option<&'a Price> {
    let dashed = id.replace('.', "-");
    let twin = |listed: &str| ["anthropic/", "openai/"].iter().any(|vendor| listed.strip_prefix(vendor).is_some_and(|name| name.replace('.', "-") == dashed));
    catalog.iter().find(|m| m.provider == "openrouter" && (m.id == id || twin(&m.id)))?.price.as_ref()
}

/// Our OpenRouter routers as (id, name): each sends a request to the `:free` models of `free_route`.
const FREE_ROUTERS: [(&str, &str); 2] = [("clef/auto", "clef/auto (free)"), ("clef-flash/auto", "clef-flash/auto (free)")];
/// The id words that mark a small, fast model, like `nemotron-3.5-lightning` or `laguna-xs-2.1`.
const FLASH_WORDS: [&str; 7] = ["flash", "lightning", "lite", "mini", "nano", "small", "xs"];

/// A catalog entry per router that has free models, sized to fit every model it can land on.
pub fn free_routers(catalog: &[Model]) -> Vec<Model> {
    let mut routers = Vec::new();
    for (id, name) in FREE_ROUTERS {
        let route = free_route(catalog, id);
        let (Some(context_window), Some(max_tokens)) = (route.iter().map(|m| m.context_window).min(), route.iter().map(|m| m.max_tokens).min()) else { continue };
        routers.push(Model { provider: "openrouter".into(), id: id.into(), name: name.into(), context_window, max_tokens, accounts: Vec::new(), price: None });
    }
    routers
}

/// The free models a router tries, biggest context first: `clef-flash` the small fast ones, `clef` the rest.
/// Empty for a model that is no router. Four, since OpenRouter's Messages `fallbacks` takes at most 3 after the first.
pub fn free_route<'a>(catalog: &'a [Model], router: &str) -> Vec<&'a Model> {
    if !FREE_ROUTERS.iter().any(|(id, _)| *id == router) {
        return Vec::new();
    }
    let flash = router == "clef-flash/auto";
    let mut route: Vec<&Model> = catalog.iter().filter(|m| m.provider == "openrouter" && m.id.ends_with(":free") && is_flash(m) == flash).collect();
    route.sort_by_key(|m| std::cmp::Reverse(m.context_window));
    route.truncate(4);
    route
}

fn is_flash(model: &Model) -> bool {
    let name = model.id.rsplit('/').next().unwrap_or_default().trim_end_matches(":free");
    name.split('-').any(|word| FLASH_WORDS.contains(&word))
}

/// The provider's newest small model (haiku, luna), used for summaries and permission reviews; `model` itself when its provider lists none.
pub fn small_model<'a>(catalog: &'a [Model], model: &'a Model) -> &'a Model {
    catalog.iter().find(|m| m.provider == model.provider && is_small(m)).unwrap_or(model)
}

fn is_small(model: &Model) -> bool {
    model.id.contains("haiku") || model.id.contains("luna")
}

/// Footer label: "Claude Opus 5.5" becomes "anthropic · opus 5.5", since one model can come from several providers.
/// OpenRouter shows the id, since names like "Auto Router" hide whose model it is.
pub fn short_label(model: &Model) -> String {
    let lower = model.name.to_lowercase();
    let name = if model.provider == "openrouter" { &model.id } else { lower.strip_prefix("claude ").unwrap_or(&lower) };
    format!("{} · {name}", model.provider)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn model(provider: &str, id: &str) -> Model {
        Model { provider: provider.into(), id: id.into(), name: id.into(), context_window: 1, max_tokens: 1, accounts: Vec::new(), price: None }
    }

    #[test]
    fn small_model_falls_back_to_the_model_itself_and_labels_are_short() {
        let opus = Model { name: "Claude Opus 5.5".into(), ..model("anthropic", "claude-opus-5-5") };
        let terra = model("openai-codex", "gpt-5.6-terra");
        let models = [opus.clone(), model("anthropic", "claude-haiku-5-5"), terra.clone()];
        assert_eq!(short_label(&opus), "anthropic · opus 5.5");
        assert_eq!(small_model(&models, &opus).id, "claude-haiku-5-5");
        assert_eq!(small_model(&models, &terra).id, "gpt-5.6-terra");
        let router = Model { name: "Auto Router".into(), ..model("openrouter", "openrouter/auto") };
        assert_eq!(short_label(&router), "openrouter · openrouter/auto");
    }

    #[test]
    fn only_accounts_that_list_a_model_serve_it() {
        let mut catalog = Vec::new();
        merge(&mut catalog, vec![model("openai-codex", "gpt-6-luna"), model("openai-codex", "gpt-6-sol")], "pro");
        merge(&mut catalog, vec![model("openai-codex", "gpt-6-luna")], "plus");
        let (luna, sol) = (&catalog[0], &catalog[1]);
        assert_eq!(luna.accounts, ["pro", "plus"]);
        assert!(serves(&catalog, sol, "pro"));
        assert!(!serves(&catalog, sol, "plus"));
        assert!(serves(&catalog, sol, "logged-in-later"));
    }

    #[test]
    fn openrouter_list_keeps_tool_models_and_defaults_a_missing_output_limit() {
        let body = json!({"data": [
            {"id": "anthropic/claude-haiku-5.5", "name": "Anthropic: Claude Haiku 5.5", "context_length": 1000000, "top_provider": {"max_completion_tokens": 128000}, "supported_parameters": ["tools"], "pricing": {"prompt": "0.0000001", "completion": "0.0000005", "input_cache_read": "0.00000001", "input_cache_write": "0.000000125", "overrides": [{"min_prompt_tokens": 100000, "prompt": "0.0000005", "completion": "0.0000025", "input_cache_read": "0.00000005", "input_cache_write": "0.000000625"}]}},
            {"id": "openrouter/auto", "name": "Auto Router", "context_length": 2000000, "top_provider": {"max_completion_tokens": null}, "supported_parameters": ["tools"], "pricing": {"prompt": "-1", "completion": "-1"}},
            {"id": "moondream/moondream3-preview", "context_length": 32000, "top_provider": {}, "supported_parameters": ["temperature"]}
        ]});
        let models = parse_openrouter(&body);
        let ids: Vec<(&str, u64)> = models.iter().map(|m| (m.id.as_str(), m.max_tokens)).collect();
        assert_eq!(ids, [("anthropic/claude-haiku-5.5", 128000), ("openrouter/auto", OPENROUTER_MAX_TOKENS)]);
        assert!(models.iter().all(|m| m.provider == "openrouter"));

        let haiku = Price {
            base: Rates { input: 1e-7, output: 5e-7, cache_read: 1e-8, cache_write: 1.25e-7 },
            long_prompt: vec![(100_000, Rates { input: 5e-7, output: 2.5e-6, cache_read: 5e-8, cache_write: 6.25e-7 })],
        };
        assert_eq!(price(&models, "claude-haiku-5-5"), Some(&haiku));
        assert_eq!(price(&models, "anthropic/claude-haiku-5.5"), Some(&haiku));
        assert_eq!(price(&models, "openrouter/auto"), None);
        assert_eq!(price(&models, "claude-haiku-5"), None);
    }

    #[test]
    fn free_routers_split_free_models_into_flash_and_the_rest() {
        let free = |id: &str, context_window: u64| Model { context_window, ..model("openrouter", id) };
        let catalog = [
            free("liquid/lfm-2.5-2.6b:free", 65536),
            free("nvidia/nemotron-3-ultra:free", 1000000),
            free("nvidia/nemotron-3.5-lightning:free", 1000000),
            free("poolside/laguna-xs-2.1:free", 262144),
            model("openrouter", "anthropic/claude-haiku-5.5"),
            model("anthropic", "claude-mini:free"),
        ];
        let ids = |router: &str| free_route(&catalog, router).iter().map(|m| m.id.as_str()).collect::<Vec<_>>();
        assert_eq!(ids("clef/auto"), ["nvidia/nemotron-3-ultra:free", "liquid/lfm-2.5-2.6b:free"]);
        assert_eq!(ids("clef-flash/auto"), ["nvidia/nemotron-3.5-lightning:free", "poolside/laguna-xs-2.1:free"]);
        assert!(ids("anthropic/claude-haiku-5.5").is_empty());
        let clef = &free_routers(&catalog)[0];
        assert_eq!((clef.id.as_str(), clef.name.as_str(), clef.context_window), ("clef/auto", "clef/auto (free)", 65536));
    }

    #[test]
    fn codex_list_skips_hidden_models() {
        let body = json!({"models": [
            {"slug": "gpt-6-luna", "display_name": "GPT-6-Luna", "context_window": 272000, "visibility": "list"},
            {"slug": "codex-auto-review", "context_window": 272000, "visibility": "hide"},
        ]});
        let ids: Vec<String> = parse_codex(&body).into_iter().map(|m| m.id).collect();
        assert_eq!(ids, ["gpt-6-luna"]);
    }
}
