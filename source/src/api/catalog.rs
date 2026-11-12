use super::*;

impl Client {
    /// Builds the model catalog live from every account's model list; returns one error per list that failed.
    pub fn load_catalog(&self) -> Vec<String> {
        let (catalog, errors) = self.fetch_catalog();
        *self.catalog.write().unwrap() = catalog.into();
        errors
    }

    /// The live catalog and one error per list that failed, saved to the cache when complete; the catalog in use stays as it is.
    pub fn fetch_catalog(&self) -> (Vec<Model>, Vec<String>) {
        let accounts: Vec<Account> = self.state.lock().unwrap().accounts.iter().filter(|a| a.provider != "openrouter").cloned().collect();
        let custom_providers = self.state.lock().unwrap().custom_providers.clone();
        // One request per account, about 0.3-0.7 s each; in parallel the load waits only for the slowest.
        let (openrouter, listed, custom_listed) = std::thread::scope(|scope| {
            // Public list, so OpenRouter models show in the picker before /login.
            let openrouter = scope.spawn(|| fetch_models(models_agent().get(format!("{}/v1/models", self.openrouter_base)), models::parse_openrouter));
            let lists: Vec<_> = accounts.iter().map(|account| scope.spawn(|| self.list_models(account))).collect();
            let custom_lists: Vec<_> = custom_providers.iter().map(|provider| scope.spawn(|| custom_models(provider))).collect();
            (
                openrouter.join().expect("model list thread"),
                lists.into_iter().map(|l| l.join().expect("model list thread")).collect::<Vec<_>>(),
                custom_lists.into_iter().map(|l| l.join().expect("model list thread")).collect::<Vec<_>>(),
            )
        });
        let mut catalog = Vec::new();
        let mut errors = Vec::new();
        match openrouter {
            Ok(models) => catalog.extend(models),
            Err(error) => errors.push(format!("openrouter: {error}")),
        }
        for (account, list) in accounts.iter().zip(listed) {
            match list {
                Ok(models) => models::merge(&mut catalog, models, &account.name),
                Err(error) => errors.push(format!("{}: {error}", key(account))),
            }
        }
        for (provider, list) in custom_providers.iter().zip(custom_listed) {
            match list {
                Ok(models) => {
                    // A provider named like an OpenRouter vendor (deepseek) lists the same ids; the direct one wins.
                    catalog.retain(|known| !models.iter().any(|model| model.id == known.id));
                    catalog.extend(models);
                }
                Err(error) => errors.push(format!("{}: {error}", provider.name)),
            }
        }
        catalog.extend(models::free_routers(&catalog));
        // Only a complete load is saved: a list that failed would drop its account's models from the next start.
        if errors.is_empty() {
            let path = self.catalog_cache();
            let _ = std::fs::create_dir_all(path.parent().expect("the cache is under ~/.agents"));
            let _ = accounts::write_private(&path, &models::to_cache(&catalog));
        }
        (catalog, errors)
    }

    /// Sets the catalog the last complete load saved, so a one-shot run need not wait for the lists.
    /// False, leaving the catalog empty, when there is none or it does not list `model`.
    pub fn load_cached_catalog(&self, model: &str) -> bool {
        let Some(catalog) = std::fs::read_to_string(self.catalog_cache()).ok().and_then(|text| models::from_cache(&text)) else { return false };
        if !catalog.iter().any(|m| m.id == model) {
            return false;
        }
        *self.catalog.write().unwrap() = catalog.into();
        true
    }

    fn catalog_cache(&self) -> PathBuf {
        self.home.join(".agents/cache/models.json")
    }

    /// Empty until load_catalog finishes.
    pub fn catalog(&self) -> Arc<[Model]> {
        self.catalog.read().unwrap().clone()
    }

    /// The models this account can use right now; see models-endpoints.md.
    pub(super) fn list_models(&self, account: &Account) -> Result<Vec<Model>, String> {
        let account = self.fresh(account.clone())?;
        let http = models_agent();
        let request = match (&account.secret, account.provider.as_str()) {
            (Secret::ApiKey(key), "anthropic") => http.get(format!("{}/v1/models?limit=1000", self.anthropic_base)).header("anthropic-version", "2023-06-01").header("x-api-key", key),
            (Secret::OAuth(tokens), "anthropic") => http
                .get(format!("{}/v1/models?limit=1000", self.anthropic_base))
                .header("anthropic-version", "2023-06-01")
                .header("authorization", format!("Bearer {}", tokens.access))
                .header("anthropic-beta", "oauth-2025-04-20"),
            (Secret::OAuth(tokens), copilot::PROVIDER) => {
                let mut request = http.get(format!("{}/models", copilot::base_url(&tokens.access))).header("authorization", format!("Bearer {}", tokens.access));
                for (name, value) in copilot::HEADERS {
                    request = request.header(name, value);
                }
                return fetch_models(request, copilot::parse_models);
            }
            (Secret::OAuth(tokens), google::ANTIGRAVITY) => return google::models(&google::ANTIGRAVITY_APP, tokens),
            (Secret::OAuth(tokens), "openai-codex") => http
                // An old client_version gets an empty list, a missing one a 400.
                .get(format!("{}/codex/models?client_version=0.160.0", self.chatgpt_base))
                .header("authorization", format!("Bearer {}", tokens.access))
                .header("chatgpt-account-id", tokens.account_id.as_deref().unwrap_or_default()),
            (Secret::ApiKey(_), provider) => return Err(format!("{provider} accounts need /login, not an API key.")),
            (Secret::OAuth(_), provider) => return Err(format!("{provider} accounts are no longer supported. Remove this account with /logout.")),
        };
        let parse = if account.provider == "anthropic" { models::parse_anthropic } else { models::parse_codex };
        fetch_models(request, parse)
    }
}

/// The models `custom_providers` lists, or the server's `GET /models` when it lists none.
fn custom_models(provider: &CustomProvider) -> Result<Vec<Model>, String> {
    if provider.models.is_empty() {
        let key = custom_key(&provider.api_key_env)?;
        let request = match provider.api {
            CustomApi::OpenAi if key.is_empty() => models_agent().get(format!("{}/models", provider.base_url)),
            CustomApi::OpenAi => models_agent().get(format!("{}/models", provider.base_url)).header("authorization", format!("Bearer {key}")),
            CustomApi::Anthropic => models_agent().get(format!("{}/v1/models", provider.base_url)).header("anthropic-version", "2023-06-01").header("x-api-key", &key),
        };
        return fetch_models(request, |body| models::parse_openai(&provider.name, body));
    }
    let listed = provider.models.iter().map(|m| Model {
        provider: provider.name.clone(),
        id: format!("{}/{}", provider.name, m.id),
        name: m.id.clone(),
        context_window: m.context_window,
        max_tokens: m.max_tokens,
        accounts: Vec::new(),
        price: None,
    });
    Ok(listed.collect())
}

pub(super) fn fetch_models(request: ureq::RequestBuilder<ureq::typestate::WithoutBody>, parse: impl Fn(&Value) -> Vec<Model>) -> Result<Vec<Model>, String> {
    let mut response = request.call().map_err(|e| format!("Model list request failed: {e}."))?;
    let status = response.status().as_u16();
    if status != 200 {
        return Err(format!("Model list request failed ({status})."));
    }
    let text = response.body_mut().read_to_string().map_err(|e| format!("Model list request failed: {e}."))?;
    let body: Value = serde_json::from_str(&text).map_err(|e| format!("Model list is not JSON: {e}."))?;
    Ok(parse(&body))
}
