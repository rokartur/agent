# Fetching the model catalog from the providers

Question: can the built-in model list be replaced by asking each provider which models exist, so new models appear without a release?

Answer: yes. Since 344267f the agent has no built-in list and no cache. At each start it asks every account for its models. See [What the agent does](#what-the-agent-does).

Checked 2026-10 with live requests. Credentials came from `~/.agents/accounts.json` (Anthropic) and `~/.codex/auth.json` (Codex). They were never printed.

## Anthropic: `GET https://api.anthropic.com/v1/models`

- Works with an API key (`x-api-key`) and with the OAuth token (`authorization: Bearer`, `anthropic-beta: oauth-2025-04-20`). Both returned 200.
- `?limit=100` returns everything in one page (`has_more: false`, 14 models). Pagination uses `has_more` / `last_id`. The agent asks for `limit=1000`, the API maximum, and does not page.
- Fields per model: `id, display_name, created_at, line, lifecycle, deprecated_at, retires_at, max_input_tokens, max_tokens, capabilities, type`.
- `max_input_tokens` and `max_tokens` matched the old built-in list for all 14 ids.
- The endpoint lists only the dated ids, for example `claude-opus-4-5-20251101`. The old list also had the aliases `claude-haiku-4-5`, `claude-opus-4-5` and `claude-sonnet-4-5`.
- No price.
- The lists differ per account. On 2026-10 `claude-sonnet-4-5-20250929` was on three accounts and missing on the fourth (`claudescom-api`).

## Codex (ChatGPT subscription): `GET https://chatgpt.com/backend-api/codex/models?client_version=<v>`

- This is the endpoint Codex CLI uses. It caches the response in `~/.codex/models_cache.json` with `fetched_at`, `etag` and `client_version`.
- Auth is the agent's own `openai-codex` account: `authorization: Bearer <access>` and `chatgpt-account-id`. The answer is 200 with a weak `etag`.
- `client_version` is required. Without it the answer is 400. With `0.1.0` the list is empty. With `0.160.0`, `0.200.0` and `9.0.0` the list is the same.
- Fields per model: `slug, display_name, context_window, max_context_window, visibility, priority, supported_in_api, supported_reasoning_levels, input_modalities, ...`.
- `context_window` is 272000. `max_context_window` is 872000.
- `visibility` is `list` or `hide`. The live answer had 5 models and 3 of them listed: `gpt-6-luna`, `gpt-5.6-terra`, `gpt-5.6-luna`. `gpt-reserve` and `codex-auto-review` are hidden.
- No price. `priority` looks like picker order.
- The list is authoritative. A one-word request to `/codex/responses` gives `response.completed` for `gpt-6-luna`. For `gpt-6-sol`, `gpt-5.5` and `gpt-5.3-codex-spark` it gives 400 `The '<model>' model is not supported when using Codex with a ChatGPT account.` The old built-in list had 6 Codex models that are not on the live list.

## What the agent does

- `Client::load_catalog` in `source/src/api/mod.rs` fetches OpenRouter's public list, then the list of each Anthropic and Codex account. A list that fails becomes a startup notice. A catalog with no failed list is saved to `~/.agents/cache/models.json`.
- A one-shot run (`-p`) starts on that cached catalog when it lists the configured model, and the lists only refresh the cache behind the request. The lists take 1-3 s of network.
- Each entry in `custom_providers` (settings) is an OpenAI-compatible server: `GET {base_url}/models`, or the `models` list from settings when it is set. Ids get the prefix `<name>/`, which the agent removes before it sends a request to `{base_url}/chat/completions`. With `"api": "anthropic"` the list is `GET {base_url}/v1/models` and requests go to `{base_url}/v1/messages` with `x-api-key`, without `thinking`, `output_config` and `speed`. A custom model with the same id as an OpenRouter model replaces it. The key comes from the env var named in `api_key_env`. If that is empty, no `authorization` header is sent. A missing key or a failed list becomes a startup notice. Missing context sizes default to 32768 / 16384.
- A `github-copilot` account (device login in `/login`) lists `GET {base}/models` and sends Chat Completions to `{base}/chat/completions`. `base` comes from `proxy-ep` in the Copilot token. The token lasts about 30 minutes and is refreshed with the saved GitHub token.
- `models::merge` keeps one entry per model and records in `Model.accounts` which accounts listed it. `/model` shows these accounts next to the provider.
- `choose` sends a request only to an account that listed the model. An account with no list (a failed fetch, a `/login` after startup, an OpenRouter key) is not checked, so the API decides.
- If the configured model id is not in the merged catalog, the agent uses the first model an account lists and shows a notice.
- There is no price. `models::small_model` picks the provider's `haiku` or `luna` model for summaries and reviews, or the main model if there is none.
- Without network or accounts only OpenRouter's list is left. If that fails too, the TUI opens with an error and print mode exits.
