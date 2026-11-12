# Configuration

Everything lives under `~/.agents` (the agent dir) and, per project, under `.agents` next to the code. Copying `~/.agents` moves the whole setup.

## `~/.agents`

| Path | Owner |
|---|---|
| `settings.json` | `settings.rs`; schema and defaults in `src/settings.schema.json`, embedded in the binary. `jq '.properties \| keys' src/settings.schema.json` lists every key. |
| `accounts.json` | `api/accounts.rs`, see [providers.md](providers.md) |
| `sessions/<id>.jsonl` | `agent/mod.rs`, see [sessions.md](sessions.md) |
| `sessions/<id>/` | the session's folder: `pages/` (`tools/page.rs`) and `workflows/` (`agent/workflow.rs`) |
| `snapshots/` | `agent/snapshots.rs`: a private git repo for rewind |
| `skills/<name>/SKILL.md` | `agent/prompt.rs` |
| `prompts/<name>.md` | prompt templates, `/prompt:name args`; `plan.md` overrides the `/plan` prompt |
| `SYSTEM.md`, `APPEND_SYSTEM.md` | replace or extend the built-in preamble |
| `swarm-prompt.md` | the subagent prompt, `agent/subagent.rs` |
| `modes/` | cached ponytail and caveman rule files, `agent/modes.rs` |
| `plugins/<name>/` | `plugins.rs` |
| `mcp.json`, `mcp-auth.json` | `mcp/config.rs`, `mcp/oauth.rs` |
| `web-search.json` | web search provider keys, `tools/web/providers.rs` |
| `workflows/` | saved workflow scripts and run logs, `agent/workflow.rs` |
| `history.jsonl`, `skill-usage.jsonl` | prompt history, `/skill-usage` |
| `peers/<pid>.sock`, `<pid>.json` | cross-session messaging, `agent/peers.rs` |

## A project's `.agents`

Looked up in `cwd` and its parents up to the git root, nearest first. A project is **untrusted** until `/trust` or `--trust` adds its root to `trusted_projects`; an untrusted project contributes only its `AGENTS.md` files. Trusted, it also contributes `SYSTEM.md`, `APPEND_SYSTEM.md`, `skills/`, `prompts/`, `swarm-prompt.md`, `edit-hooks.json`, `workflows/` and runs the hooks. `conflict_priority` decides whether a project or global skill wins a name clash.

A trusted project also layers `settings.json` and `settings.local.json` (yours, keep it out of git) from each of those `.agents` dirs over the global file: git root first, cwd last, `settings.local.json` over `settings.json`, each key replacing the earlier value (`settings::load_in`). A project file may set only `model`, `default_model`, `thinking`, `default_thinking`, `small_model`, `output_style`, `caveman`, `ponytail`, `auto_compact`, `reserve_tokens`, `keep_recent_tokens`, `condense`, `context_windows`, `expand_tools`, `subagents` and `rtk` (`PROJECT_KEYS`). Its `permissions.ask` and `permissions.deny` are added to the global lists; `permissions.allow` and every other key are ignored and named in a notice at start, so a repository can tighten approval but never loosen it, run a command or grant itself trust. `/settings` writes the global file, so a key the project sets stays overridden. Layers apply to sessions started after `/trust`.

`.mcp.json` in the project is read in every project but each server runs only once approved in `/mcp` (`approved_mcp`).

## System prompt

`agent/prompt::system` assembles, in order: the preamble (`SYSTEM.md` or the built-in), `APPEND_SYSTEM.md`, the project context (`AGENTS.override.md`, `AGENTS.md` or `AGENTS.MD` in cwd, wrapped in `<project_instructions path=...>`), the skills list, then the output style (`output_style`: Claude Code's built-in `outputStyle` prompts in `agent/output-styles/`), ponytail and caveman section. The cwd section goes after the cached system block so a `/worktree` move keeps the prompt cache.

An `AGENTS.md` below cwd is attached to the first `Read` result that reaches its directory, once per session (`nested_instructions`): the file closest to the code applies.

Skills are the `SKILL.md` frontmatter (`name`, `description`, `disable-model-invocation`) listed in the prompt; `/skill:name args` or the `Skill` tool expands the body in a `<skill>` block. In the TUI, `/name args` runs the skill unless a command has that name, and `/name` typed after prompt text offers skills: Tab moves the pick to the front. Templates use pi's placeholders: `$1`, `$@`, `$ARGUMENTS`, `${1:-default}`, `${@:2}`.

## `.agents/edit-hooks.json`

```json
{"onStop": {"*.ts": ["fmt {files}", "check {file}", "tsc"], "*.md": "fmt {files}", "src/**": "never"}}
```

Each edited file goes to the first glob it matches; a glob with a slash matches the path from the config dir. Commands run in the directory holding `.agents`, with `{file}` or `{files}` substituted, when the turn ends. The nearest config above each file wins; the walk stops at the repository root. A broken config errors for its directory only.

## Settings that are not in `/settings`

`/settings` covers the common keys. The rest, `context_windows`, `custom_providers`, `accounts.*`, `condense.*`, `lsp`, `plugin_settings`, `headroom_env`, `on_start`, `permissions`, `before_tool`, are edited in `settings.json`; the schema's `description` is the documentation. `save_change` writes the file 0600 and keeps keys it does not know.
