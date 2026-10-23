# Architecture

One binary, no async runtime: threads and `mpsc` channels. HTTP is `ureq` with rustls; JSON is `serde_json::Value` throughout, no typed request or response structs. The dependency list in `Cargo.toml` is the whole third-party surface: `crossterm`, `pulldown-cmark`, `ring`, `rquickjs`, `ureq`, `unicode-width`, `libc`, `serde_json`.

## Module map

| Module | Owns |
|---|---|
| `main.rs` | CLI parsing, `Setup`, the print/json/headless loops, `plugin`, `update`, `doctor` subcommands. |
| `agent/` | `Agent`: the turn loop, messages, session journal, compaction, snapshots, permissions, subagents, workflows, cross-session messaging. |
| `agent/prompt.rs` | System prompt assembly: `SYSTEM.md`, `AGENTS.md` files, skills, prompt templates, `/skill:` and `/prompt:` expansion. |
| `api/` | `Client`: Anthropic Messages, OpenAI Codex, OpenRouter, GitHub Copilot and OpenAI-compatible custom providers; accounts, OAuth, model catalog, account choice and failover. |
| `tools/` | Built-in tools, the anchor registry (`hashline.rs`), background shells (`bg.rs`), the bash guard, rtk, web tools. |
| `tui/` | The interactive terminal: `App` state, panels, editor, markdown rendering, login flows, voice. |
| `mcp/` | MCP client over stdio and streamable HTTP, `mcp.json` config, OAuth with PKCE. |
| `plugins.rs` | JSON-RPC plugins under `~/.agents/plugins/<name>/`. Protocol in `../../plugins/README.md`. |
| `acp.rs` | `agent acp`: Agent Client Protocol server for editors. |
| `headroom.rs` | Installs and syncs the local `headroom proxy` service. |
| `image.rs` | Pasted and dropped images: ImageMagick resize to the provider's side limit. |
| `settings.rs` | `~/.agents/settings.json`, validated against the embedded `settings.schema.json`. |
| `process.rs` | Child process helpers, `installed(binary)`. |
| `update.rs` | Self-update from GitHub releases; only CI builds update themselves. |
| `worktree.rs` | `git worktree` wrappers for `/worktree`, `EnterWorktree`, `ExitWorktree`. |

`src/tui/mod.rs` is the largest file by far (6k lines). The `App` struct, the key handler and every panel's rendering live there; `panel.rs`, `editor.rs`, `render.rs`, `markdown.rs`, `selection.rs` hold the pieces that were worth pulling out.

## Threads

The TUI (`tui::run`) owns the terminal and runs the event loop on the main thread. It spawns:

- **worker**: owns the `Agent`. Receives `Command`s (prompt, compact, resume, ...) over one channel and sends `Msg::Agent(Event)` back over another. Only this thread touches `agent.messages`.
- **input**: reads crossterm events and forwards them as `Msg::Input`.
- **update**, **on_start**, **headroom sync**: one-shot threads at start.
- Per background shell, subagent, workflow, condense batch and session title: a thread that reports through the same `events` channel or through `Arc<Mutex<_>>` fields on `Agent` that the UI also reads (`todos`, `goal`, `plan`, `terminals`, `subagents`, `workflows`, `permission_mode`).

The split rule: the worker thread mutates conversation state, the UI thread reads shared state and sends commands. A mode change in `/settings` reaches the running turn through those shared fields, not through a message.

## A prompt, end to end

1. `Agent::prompt` (`agent/mod.rs`) builds the user message: `!cmd` shell output captured earlier, pending todo notes, pasted images, then the text. It is appended to `messages` and to the session journal.
2. `run_turns` loops:
   - Auto-compact when `last_context_tokens` passed `context_window - reserve_tokens`.
   - `collect_condensed` folds finished condense batches in.
   - `provider_view` is what the model sees: compaction summaries and condense stubs in place of the originals.
   - `api::request` builds the Anthropic-shaped request with cache breakpoints; `Client::send` picks an account, translates for the provider and streams deltas back as `Event::Text`, `Event::Thinking`, `Event::Notice`.
   - No tool calls: deliver queued steering messages if any, else run the edit hooks for every file edited this prompt and either return or push the hook report and loop.
   - Tool calls: each goes through `permissions::request_for` (ask, reviewer model, or allowed), a file snapshot around it, the tool itself, plugin `tool_call`/`tool_result` events, and a `tool_time` journal entry. Results go back as one user message with `tool_result` blocks.
3. The TUI renders events as they arrive and gets `Msg::Done` with the answer or the error.

Esc sets `agent.abort`; the stream reader (`api::watched_body`) and running bash commands check it. The aborted reply keeps its finished text and the journal records `Operation aborted` as each open tool call's result.

## Where state lives

| State | Memory | Disk |
|---|---|---|
| Conversation | `Agent.messages` | `~/.agents/sessions/<id>.jsonl`, see [sessions.md](sessions.md) |
| Settings | `tui::App.settings`, copied into `Agent` fields at start and on change | `~/.agents/settings.json` |
| Accounts | `api::Client` state, shared by clones | `~/.agents/accounts.json`, written through `<path>.lock` |
| Line anchors | `Agent.anchors` | none: anchors are per session and die with it |
| File snapshots | `Agent.snapshots` | a private git repo under `~/.agents/snapshots` |
| Skills, prompts, modes | rebuilt into the system prompt | `~/.agents/skills`, `~/.agents/prompts`, `~/.agents/modes`, and the project's `.agents` |
