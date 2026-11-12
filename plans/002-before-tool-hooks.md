# Plan 002: A before_tool hook can refuse a tool call

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in "STOP conditions" occurs, stop and report, do not
> improvise. When done, update this plan's row in `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat 52b32af..HEAD -- source/src/agent/mod.rs source/src/agent/permissions.rs source/src/settings.rs source/src/settings.schema.json`
> Plan 001 changes these files on purpose. Any other change: compare the
> "Current state" excerpts with the live code; a mismatch is a STOP condition.

## Status

- **Priority**: P2
- **Effort**: S
- **Risk**: MED
- **Depends on**: plans/001-permission-rules.md (uses its `parse_rule`, `subjects` and matcher)
- **Category**: direction
- **Planned at**: commit `52b32af`, 2026-11-12

## Why this matters

The agent already has four of the five hook points other agents offer, under
other names (table below). The missing one is a hook that runs before a tool
call and can refuse it with a reason the model reads: "no `git push` to main",
"no edits while the release freeze file exists". Plan 001's rules are static
globs; a hook is the same gate with a program behind it. This plan adds that
one hook and nothing else.

| Need | What exists at `52b32af` |
|---|---|
| after an edit, on turn end | `.agents/edit-hooks.json` `onStop` (`src/agent/edit_hooks.rs`) |
| rewrite or refuse the prompt | plugin `input` request, 5 s limit (`src/agent/mod.rs:543`, `src/plugins.rs:212`) |
| session start | `on_start` in settings (`src/tui/mod.rs:1132`), plugin event `session_start` (`src/agent/mod.rs:2134`) |
| observe tool calls and results | plugin events `tool_call`, `tool_result`, `prompt_end` (`src/agent/mod.rs:728`, `:766`, `:587`) |
| refuse a tool call | nothing |

## Current state

All paths are under `source/`.

- `src/agent/mod.rs:1629` `check_permission`: every tool call passes it. After
  plan 001 it computes `subjects` and returns early on a deny rule, above the
  `permissions::request_for(tool, input, files)?` line.
- `src/agent/mod.rs:783` `run_edit_hooks`: the pattern for running a configured
  command. It builds a `Bash` input and runs it through the Bash tool, so the
  timeout, the abort flag and the output cap come for free:

```rust
let approved = json!({"command": format!("cd {} && {}", edit_hooks::shell_quote(&hook.dir.display().to_string()), hook.command), "timeout": 120_000});
if let Some(refusal) = self.check_permission(None, "Bash", &approved, &[], emit) {
```

  Read lines 783 to 810 before you start, and copy how it calls the Bash tool
  and reads `(output, is_error)`.
- `src/agent/edit_hooks.rs`: `{file}` and `{files}` placeholders are replaced
  with `shell_quote`d values (`pub fn shell_quote`, line 119).
- `src/settings.rs`: `on_start: Vec<String>` (line 128) is the nearest
  existing key; `from_json` reads it near line 458.

## The design (decided, do not redesign)

Settings, global file only:

```json
"before_tool": [
  {"match": "Bash(git push*)", "command": "~/bin/check-push {call}"},
  {"match": "Edit", "command": "test ! -e .release-freeze"}
]
```

- `match` is a rule in plan 001's syntax, matched the way a `deny` rule is
  (any subject, the whole Bash command and each of its parts).
- When a call matches, the agent writes `{"tool": ..., "input": ..., "cwd": ...}`
  to a private temp file, replaces `{call}` in `command` with the shell-quoted
  path, runs the command in the session's cwd through the Bash tool with a
  10 s timeout, then deletes the file.
- Exit 0: the call goes on to the rules and the mode as before.
- Any other exit, or the timeout: the call is refused with
  `Blocked by the before_tool hook "<command>":\n<output, first 4000 chars>`.
  Fail closed: a hook that crashes blocks, since its job is to guard.
- Hooks run in every mode, `full access` included, and for tools that never
  ask (`Read`, `Grep`). They do not set `self.abort`.
- The hook command itself is not permission-checked: the user wrote it in
  their own settings. It must run through the Bash tool directly, the way
  `run_edit_hooks` runs a hook after its check, never through `run_tool`,
  or it would call itself.
- No hook configured: no temp file, no process. Return before any work.
- A `match` that does not parse fails `settings::load`, like a bad rule.

## Commands you will need

| Purpose | Command (in `source/`) | Expected |
|---|---|---|
| Tests | `cargo test --locked before_tool` | all pass |
| All tests | `cargo test --locked` | all pass |
| Lint | `cargo clippy --all-targets --locked -- -D warnings` | exit 0 |
| Format | `cargo fmt --check` | exit 0 |

On Artur's MacBook every cargo command goes through `offload '<command>'`.

## Scope

**In scope**:
- `source/src/agent/mod.rs` (`check_permission`, one new method, the `Agent` field)
- `source/src/agent/permissions.rs` (only if `verdict`'s matcher needs to be exposed as a function)
- `source/src/settings.rs`, `source/src/settings.schema.json`
- `source/docs/tools.md`, `source/docs/configuration.md`

**Out of scope**:
- `after_tool`, `stop`, `prompt` and `session_start` hooks: the table above shows what covers them.
- `src/plugins.rs` and the plugin protocol.
- Reading `before_tool` from a project file. A project hook would run a command with no prompt.
- `.agents/edit-hooks.json` and `edit_hooks.rs` behaviour.

## Git workflow

- Branch `feat/before-tool-hooks`, cut from the branch or commit that holds plan 001.
- Commit: `feat(hooks): before_tool hooks can refuse a tool call`.
- Do not push or open a PR unless told to.

## Steps

### Step 1: the settings key

Add to `Settings`:

```rust
pub struct ToolHook {
    pub matches: String, // the "match" key
    pub command: String,
}
pub before_tool: Vec<ToolHook>,
```

Read and write it in `from_json` and `to_json` (an entry without both string
keys is an error, not skipped), validate each `match` in `load` next to plan
001's `check_rules` call, add `before_tool` to `settings.schema.json`.

**Verify**: `cargo test --locked settings` passes.

### Step 2: run the hooks

Add `before_tool: Vec<settings::ToolHook>` to `Agent`, set wherever plan 001
set `permission_rules`. Add a method:

```rust
/// The refusal of the first before_tool hook that matches and fails.
fn before_tool_refusal(&mut self, tool: &str, input: &Value, subjects: &[String]) -> Option<String>
```

Call it in `check_permission` right after the deny-rule return and before
`request_for`. Write the temp file with `crate::api::accounts::write_private`
(`src/api/accounts.rs:96`, `fn write_private(path: &Path, text: &str) -> Result<(), String>`, the function `settings::save` uses) under `std::env::temp_dir()`, named
`agent-call-<pid>-<counter>.json`. Remove it on every path.

**Verify**: `cargo test --locked before_tool` passes.

### Step 3: docs

- `source/docs/tools.md`: a section "before_tool hooks" with the JSON example,
  the exit rule and the payload keys.
- `source/docs/configuration.md`, "Settings that are not in `/settings`": add
  `before_tool` and `permissions` to the list.

**Verify**: `cargo fmt --check` and clippy exit 0.

## Test plan

In `agent/mod.rs` tests, modelled on `check_permission_asks_once_per_grant_and_a_closed_reply_denies` (line 3274). Unix-only commands need `#[cfg(unix)]` or must work in Git Bash; look at how neighbouring tests that run Bash handle Windows.

- a hook `{"match": "Bash", "command": "false"}` refuses in `FullAccess`, and the reason names the command.
- `"command": "true"` lets the call through to the normal prompt.
- a hook for `Read` refuses a `Read` call.
- `{call}`: a hook `grep -q '"tool":"Bash"' {call}` passes for Bash; after the call no `agent-call-*` file is left in the temp dir.
- a hook for `Edit` does not run for a `Bash` call (use a command that would fail).
- `settings::load` errors on `{"match": "Bash("}`.

## Done criteria

- [ ] `cargo test --locked` exits 0, with the new tests
- [ ] clippy and `cargo fmt --check` exit 0
- [ ] `jq '.properties.before_tool' source/src/settings.schema.json` prints an object
- [ ] `rg -n 'before_tool' source/src/plugins.rs` has no matches
- [ ] `git status --porcelain` lists only in-scope files
- [ ] `plans/README.md` row updated

## STOP conditions

- Plan 001 is not in the tree (`rg -n 'pub fn subjects' source/src/agent/permissions.rs` has no match).
- The Bash tool cannot be called from `check_permission` without `run_tool` (borrow or signature trouble): report, do not write a second process runner.
- `write_private` does not take a path and text: report its signature.
- A step's verification fails twice.

## Maintenance notes

- Every matching tool call pays one shell start. A hook on `Read` or `Grep` slows a search-heavy turn; the docs should say so.
- Edit-hook commands pass `check_permission` as `Bash`, so `before_tool` hooks for `Bash` see them too. That is intended.
- Deferred, with the reason: an `after_tool` hook (plugins get `tool_result`), hooks in project files (needs a trust decision).
