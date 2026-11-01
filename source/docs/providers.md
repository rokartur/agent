# Providers and accounts

`api/mod.rs` owns one `Client`. Clones share the account list, the usage cache and the current pick. The request shape is Anthropic Messages everywhere; `api/codex.rs`, `api/openai_chat.rs` and `api/copilot.rs` translate it on the way out and the stream on the way back.

| Provider id | Auth | Code |
|---|---|---|
| `anthropic` | OAuth as Claude Code, or `ANTHROPIC_API_KEY` | `api/oauth.rs` |
| `openai-codex` | ChatGPT OAuth | `api/codex.rs`, `api/oauth.rs` |
| `openrouter` | `OPENROUTER_API_KEY` or a saved key | `api/openai_chat.rs` |
| `github-copilot` | device flow, short-lived Copilot token | `api/copilot.rs` |
| custom | `custom_providers` in settings: a `PRESETS` name, or `base_url` plus `api_key_env` | `api/models.rs`, `api/openai_chat.rs` |

`PROVIDERS` in `api/models.rs` is the fixed list; `PRESETS` the 40 OpenAI-compatible servers a custom provider can name without a URL.

## Accounts

`~/.agents/accounts.json` (`api/accounts.rs`): every saved account in priority order per provider, with its secret. `/account` lists them in that order and Shift+↑↓ moves the selected one (`Client::move_account`). Writes go through a 0600 temp file and a rename, under `<path>.lock`, so two agent processes saving at once keep each other's accounts. A key in `ANTHROPIC_API_KEY` or `OPENROUTER_API_KEY` becomes an account named `env` when no saved account exists for that provider (`Client::reload`). This is why the tests run with `env -u ANTHROPIC_API_KEY`.

OAuth accounts report subscription windows (5h, 7d) in percent; `/usage` and `/account` show them. API keys have none.

## Choosing an account, failover

`send_on_account` loops until one attempt returns:

1. `choose` picks the pinned account while it is under `accounts.switch_at_percent` and not rate-limited, else the first such account, else what `accounts.when_all_over_limit` says (`least-used`, `block`, `other-provider`).
2. A failed token refresh or a quota error (429) marks the account limited for `LIMITED_FOR` and moves the request to the next account, once per account per request.
3. `other-provider` with every account over limit moves the request to `accounts.fallback_model`, which must name another provider's model.
4. A Codex `service_tier` rejection retries once without `speed`.
5. A reply that already streamed text to a `--print` stdout is not retried: the text cannot be taken back.

A `--account p=name` flag or `/account` pins one account for the session; the last account used per provider is saved in the journal so a resume keeps it.

## Models

`load_catalog` fetches each provider's model list once per start and `/model` filters it. `context_windows` in settings overrides a model's window by provider or by `provider/model`; `/context-window` sets it for the session. `pinned_models` go first in `/model` and `Ctrl+P` cycles them.

## Headroom

With `headroom` on, requests go to a `headroom proxy` instead of the provider, `HEADROOM_URL` locally. `headroom_url` names a remote one; `headroom_token` is sent as `x-headroom-proxy-token`, the header headroom requires off loopback, and the proxy strips it before forwarding. The agent installs and syncs the local service at start (`headroom.rs`) only when `headroom_url` is empty; `headroom_env` is the service environment the sync enforces.

## Cost

`agent/usage.rs`. OpenRouter replies carry what was billed and that amount wins; other providers are priced at list prices from the catalog. The footer shows the exact figure, never an estimate.
