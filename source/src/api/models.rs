use serde_json::{Value, json};

/// The Codex model list has no output limit; every Codex model takes this many.
const CODEX_MAX_TOKENS: u64 = 128_000;

/// The providers this agent can talk to.
pub const PROVIDERS: [&str; 4] = ["anthropic", "openai-codex", "openrouter", "github-copilot"];
pub struct Preset {
    pub name: &'static str,
    pub base_url: &'static str,
    /// Empty for local servers, which take no key.
    pub api_key_env: &'static str,
}

/// OpenAI-compatible providers `custom_providers` can name without a `base_url`; URLs and variables as jcode and pi use them.
pub const PRESETS: [Preset; 40] = [
    Preset { name: "groq", base_url: "https://api.groq.com/openai/v1", api_key_env: "GROQ_API_KEY" },
    Preset { name: "cerebras", base_url: "https://api.cerebras.ai/v1", api_key_env: "CEREBRAS_API_KEY" },
    Preset { name: "mistral", base_url: "https://api.mistral.ai/v1", api_key_env: "MISTRAL_API_KEY" },
    Preset { name: "xai", base_url: "https://api.x.ai/v1", api_key_env: "XAI_API_KEY" },
    Preset { name: "deepseek", base_url: "https://api.deepseek.com", api_key_env: "DEEPSEEK_API_KEY" },
    Preset { name: "together", base_url: "https://api.together.xyz/v1", api_key_env: "TOGETHER_API_KEY" },
    Preset { name: "fireworks", base_url: "https://api.fireworks.ai/inference/v1", api_key_env: "FIREWORKS_API_KEY" },
    Preset { name: "deepinfra", base_url: "https://api.deepinfra.com/v1/openai", api_key_env: "DEEPINFRA_API_KEY" },
    Preset { name: "novita", base_url: "https://api.novita.ai/openai", api_key_env: "NOVITA_API_KEY" },
    Preset { name: "perplexity", base_url: "https://api.perplexity.ai", api_key_env: "PERPLEXITY_API_KEY" },
    Preset { name: "huggingface", base_url: "https://router.huggingface.co/v1", api_key_env: "HF_TOKEN" },
    Preset { name: "nvidia", base_url: "https://integrate.api.nvidia.com/v1", api_key_env: "NVIDIA_API_KEY" },
    Preset { name: "baseten", base_url: "https://inference.baseten.co/v1", api_key_env: "BASETEN_API_KEY" },
    Preset { name: "nebius", base_url: "https://api.tokenfactory.nebius.com/v1", api_key_env: "NEBIUS_API_KEY" },
    Preset { name: "scaleway", base_url: "https://api.scaleway.ai/v1", api_key_env: "SCALEWAY_API_KEY" },
    Preset { name: "stackit", base_url: "https://api.openai-compat.model-serving.eu01.onstackit.cloud/v1", api_key_env: "STACKIT_API_KEY" },
    Preset { name: "chutes", base_url: "https://llm.chutes.ai/v1", api_key_env: "CHUTES_API_KEY" },
    Preset { name: "cortecs", base_url: "https://api.cortecs.ai/v1", api_key_env: "CORTECS_API_KEY" },
    Preset { name: "fpt", base_url: "https://mkp-api.fptcloud.com", api_key_env: "FPT_API_KEY" },
    Preset { name: "firmware", base_url: "https://app.frogbot.ai/api/v1", api_key_env: "FIRMWARE_API_KEY" },
    Preset { name: "302ai", base_url: "https://api.302.ai/v1", api_key_env: "302AI_API_KEY" },
    Preset { name: "conifer", base_url: "https://api.conifer.build/v1", api_key_env: "CONIFER_API_KEY" },
    Preset { name: "celeris", base_url: "https://inference.celeris.ai/celeris-1/v1", api_key_env: "CELERIS_API_KEY" },
    Preset { name: "orcarouter", base_url: "https://api.orcarouter.ai/v1", api_key_env: "ORCAROUTER_API_KEY" },
    Preset { name: "vercel-ai-gateway", base_url: "https://ai-gateway.vercel.sh/v1", api_key_env: "AI_GATEWAY_API_KEY" },
    Preset { name: "opencode", base_url: "https://opencode.ai/inference/v1", api_key_env: "OPENCODE_API_KEY" },
    Preset { name: "opencode-go", base_url: "https://opencode.ai/inference/go/v1", api_key_env: "OPENCODE_API_KEY" },
    Preset { name: "moonshotai", base_url: "https://api.moonshot.ai/v1", api_key_env: "MOONSHOT_API_KEY" },
    Preset { name: "moonshotai-cn", base_url: "https://api.moonshot.cn/v1", api_key_env: "MOONSHOT_API_KEY" },
    Preset { name: "kimi-coding", base_url: "https://api.kimi.com/coding/v1", api_key_env: "KIMI_API_KEY" },
    Preset { name: "zai", base_url: "https://api.z.ai/api/coding/paas/v4", api_key_env: "ZAI_API_KEY" },
    Preset { name: "zai-coding-cn", base_url: "https://open.bigmodel.cn/api/coding/paas/v4", api_key_env: "ZAI_CODING_CN_API_KEY" },
    Preset { name: "minimax", base_url: "https://api.minimax.io/v1", api_key_env: "MINIMAX_API_KEY" },
    Preset { name: "minimax-cn", base_url: "https://api.minimaxi.com/v1", api_key_env: "MINIMAX_CN_API_KEY" },
    Preset { name: "alibaba-coding-plan", base_url: "https://coding-intl.dashscope.aliyuncs.com/v1", api_key_env: "BAILIAN_CODING_PLAN_API_KEY" },
    Preset { name: "qwen-token-plan", base_url: "https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1", api_key_env: "QWEN_TOKEN_PLAN_API_KEY" },
    Preset { name: "xiaomi", base_url: "https://api.xiaomimimo.com/v1", api_key_env: "XIAOMI_API_KEY" },
    Preset { name: "ant-ling", base_url: "https://api.ant-ling.com/v1", api_key_env: "ANT_LING_API_KEY" },
    Preset { name: "ollama", base_url: "http://127.0.0.1:11434/v1", api_key_env: "" },
    Preset { name: "lmstudio", base_url: "http://127.0.0.1:1234/v1", api_key_env: "" },
];
/// For a custom provider's model whose list gives no window; Ollama's `/v1/models` never does.
pub const DEFAULT_CONTEXT_WINDOW: u64 = 32_768;
pub const DEFAULT_MAX_TOKENS: u64 = 16_384;

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

/// An OpenAI-compatible `GET /models` as `<provider>/<id>`; `context_length` is a common extension, not the standard.
pub fn parse_openai(provider: &str, body: &Value) -> Vec<Model> {
    let mut models = Vec::new();
    for m in body["data"].as_array().into_iter().flatten() {
        let Some(id) = m["id"].as_str() else { continue };
        let context_window = m["context_length"].as_u64().or(m["context_window"].as_u64()).unwrap_or(DEFAULT_CONTEXT_WINDOW);
        models.push(Model { provider: provider.into(), id: format!("{provider}/{id}"), name: id.into(), context_window, max_tokens: DEFAULT_MAX_TOKENS, accounts: Vec::new(), price: None });
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

/// The catalog as the cache file stores it.
pub fn to_cache(catalog: &[Model]) -> String {
    let rates = |r: &Rates| json!([r.input, r.output, r.cache_read, r.cache_write]);
    let models: Vec<Value> = catalog
        .iter()
        .map(|m| {
            let price = m.price.as_ref().map(|p| json!({"base": rates(&p.base), "long_prompt": p.long_prompt.iter().map(|(from, r)| json!([from, rates(r)])).collect::<Vec<_>>()}));
            json!({"provider": m.provider, "id": m.id, "name": m.name, "context_window": m.context_window, "max_tokens": m.max_tokens, "accounts": m.accounts, "price": price})
        })
        .collect();
    Value::Array(models).to_string()
}

/// The catalog `to_cache` wrote; None when the file is not one.
pub fn from_cache(text: &str) -> Option<Vec<Model>> {
    let rates = |v: &Value| Some(Rates { input: v[0].as_f64()?, output: v[1].as_f64()?, cache_read: v[2].as_f64()?, cache_write: v[3].as_f64()? });
    let mut catalog = Vec::new();
    for m in serde_json::from_str::<Value>(text).ok()?.as_array()? {
        let price = match &m["price"] {
            Value::Null => None,
            p => Some(Price { base: rates(&p["base"])?, long_prompt: p["long_prompt"].as_array()?.iter().map(|tier| Some((tier[0].as_u64()?, rates(&tier[1])?))).collect::<Option<_>>()? }),
        };
        catalog.push(Model {
            provider: m["provider"].as_str()?.into(),
            id: m["id"].as_str()?.into(),
            name: m["name"].as_str()?.into(),
            context_window: m["context_window"].as_u64()?,
            max_tokens: m["max_tokens"].as_u64()?,
            accounts: m["accounts"].as_array()?.iter().map(|a| a.as_str().map(String::from)).collect::<Option<_>>()?,
            price,
        });
    }
    Some(catalog)
}

/// The provider's newest small model (haiku, luna), used for summaries and permission reviews; `model` itself when its provider lists none.
pub fn small_model<'a>(catalog: &'a [Model], model: &'a Model) -> &'a Model {
    catalog.iter().find(|m| m.provider == model.provider && is_small(m)).unwrap_or(model)
}

fn is_small(model: &Model) -> bool {
    model.id.contains("haiku") || model.id.contains("luna")
}

/// Footer label: "Claude Opus 5.5" becomes "anthropic/opus-5-5", since one model can come from several providers.
/// OpenRouter shows the id, since names like "Auto Router" hide whose model it is.
pub fn short_label(model: &Model) -> String {
    format!("{}/{}", model.provider, short_name(model).replace([' ', '.'], "-"))
}

/// "Claude Opus 5.5" becomes "opus 5.5"; OpenRouter keeps the id, as in `short_label`.
pub fn short_name(model: &Model) -> String {
    if model.provider == "openrouter" {
        return model.id.clone();
    }
    let lower = model.name.to_lowercase();
    lower.strip_prefix("claude ").unwrap_or(&lower).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(provider: &str, id: &str) -> Model {
        Model { provider: provider.into(), id: id.into(), name: id.into(), context_window: 1, max_tokens: 1, accounts: Vec::new(), price: None }
    }

    #[test]
    fn the_cached_catalog_reads_back_as_it_was_saved() {
        let rates = Rates { input: 1e-7, output: 5e-7, cache_read: 1e-8, cache_write: 1.25e-7 };
        let priced = Model { accounts: vec!["work".into()], price: Some(Price { base: rates, long_prompt: vec![(100_000, rates)] }), ..model("openrouter", "a/b") };
        let catalog = vec![priced, model("anthropic", "claude-haiku-5-5")];
        let back = from_cache(&to_cache(&catalog)).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!((back[0].id.as_str(), back[0].accounts.as_slice(), back[0].price.as_ref()), ("a/b", &["work".to_string()][..], catalog[0].price.as_ref()));
        assert!(back[1].price.is_none());
        assert!(from_cache("{\"not\": \"a list\"}").is_none());
    }

    #[test]
    fn an_openai_model_list_prefixes_ids_and_reads_either_context_field() {
        let body = json!({"data": [{"id": "a", "context_length": 8192}, {"id": "b", "context_window": 4096}, {"id": "c"}, {"object": "model"}]});
        let models = parse_openai("groq", &body);
        let listed: Vec<_> = models.iter().map(|m| (m.id.as_str(), m.name.as_str(), m.context_window)).collect();
        assert_eq!(listed, [("groq/a", "a", 8192), ("groq/b", "b", 4096), ("groq/c", "c", DEFAULT_CONTEXT_WINDOW)]);
    }

    #[test]
    fn small_model_falls_back_to_the_model_itself_and_labels_are_short() {
        let opus = Model { name: "Claude Opus 5.5".into(), ..model("anthropic", "claude-opus-5-5") };
        let terra = model("openai-codex", "gpt-5.6-terra");
        let models = [opus.clone(), model("anthropic", "claude-haiku-5-5"), terra.clone()];
        assert_eq!(short_label(&opus), "anthropic/opus-5-5");
        assert_eq!(small_model(&models, &opus).id, "claude-haiku-5-5");
        assert_eq!(small_model(&models, &terra).id, "gpt-5.6-terra");
        let router = Model { name: "Auto Router".into(), ..model("openrouter", "openrouter/auto") };
        assert_eq!(short_label(&router), "openrouter/openrouter/auto");
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
