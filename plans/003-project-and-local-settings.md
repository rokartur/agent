# Plan 003: A trusted project overrides settings from its `.agents`

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in "STOP conditions" occurs, stop and report, do not
> improvise. When done, update this plan's row in `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat 52b32af..HEAD -- source/src/settings.rs source/src/agent/prompt.rs source/src/main.rs source/src/acp.rs source/src/agent/mod.rs`
> Plans 001 and 002 add keys to `settings.rs` on purpose. Any other change:
> compare the "Current state" excerpts with the live code; a mismatch is a
> STOP condition.

## Status

- **Priority**: P2
- **Effort**: M
- **Risk**: HIGH (a settings file inside a cloned repository is attacker-controlled input)
- **Depends on**: none (step 5 applies only when plan 001 is in the tree)
- **Category**: direction
- **Planned at**: commit `52b32af`, 2026-11-12

## Why this matters

Settings are one global file, `~/.agents/settings.json`. A project cannot say
"this repo uses the cheaper model" or "condense is off here", and a user
cannot keep a private override for one checkout. After this plan a trusted
project contributes `.agents/settings.json` (committed, shared with the team)
and `.agents/settings.local.json` (the user's own, kept out of git), layered
over the global file.

## Current state

All paths are under `source/`.

`src/settings.rs`:

```rust
pub fn load(home: &Path) -> Result<Settings, String> {
    Ok(from_json(&read(home)?))
}
```

`read(home)` returns the file as a `serde_json::Value`, `Value::Null` when it
is missing, and `Err("{} is broken: {e}. Fix or remove it by hand.")` when it
does not parse. `path(home)` is `home.join(".agents/settings.json")`.

Saving writes only what changed, to the global file (`src/settings.rs:325`):

```rust
/// Writes the change a /settings edit made from `before` to `after` over the file as it is now, under its lock,
/// so another agent's edits to other keys survive.
pub fn save_change(home: &Path, before: &Settings, after: &Settings) -> Result<(), String> {
    crate::api::accounts::locked(&path(home), || {
        let file = read(home)?;
        let mut next = to_json(&saved_after(before, after, &from_json(&file)));
```

Because it diffs `before` against `after`, a value that came from a project
layer and was not edited is never written to the global file. Keep it so.

`src/agent/prompt.rs` already knows the project's `.agents` dirs and trust:

```rust
/// `.agents` in cwd and its parents up to the git root, nearest first; none for an untrusted project.
pub fn project_agents_dirs(cwd: &Path, trusted: bool) -> Vec<PathBuf> {
```

```rust
pub fn untrusted_files(agent_dir: &Path, cwd: &Path) -> Vec<PathBuf> {
    let names = ["SYSTEM.md", "APPEND_SYSTEM.md", "skills", "prompts", "swarm-prompt.md", "edit-hooks.json", "workflows"];
    let dirs = project_agents_dirs(cwd, true).into_iter().filter(|dir| dir != agent_dir);
```

`is_trusted(trusted_projects, dir)` (line 78) says whether the project root of
`dir` is in the list. `untrusted_files` feeds the notice the TUI shows for an
untrusted project (`src/tui/mod.rs:2322`).

`settings::load` callers at `52b32af` (`rg -n 'settings::load\(' src`):
`main.rs` 176, 211, 434, 518; `acp.rs` 174, 252, 532; `agent/mod.rs` 1035,
1061, 1115, 1299, 1966; `serve.rs` 166, 255; `service.rs` 74;
`tui/client.rs` 126; plus tests.

`permissions::is_protected` (`src/agent/permissions.rs:198`) already lists
`.agents`, so in `auto` mode the agent's own edit of a project settings file
goes to the reviewer. Nothing to add there.

## The design (decided, do not redesign)

Layers, later wins: global file, then for each project `.agents` dir from the
git root down to cwd: `settings.json`, then `settings.local.json`.

- Layers load only when the project is trusted, judged by the **global**
  file's `trusted_projects`. A project layer can never grant trust.
- The dir equal to `home/.agents` is skipped (as `untrusted_files` does), so
  the global file is not applied twice when cwd is the home directory.
- Merge is per top-level key: the layer's value replaces the earlier one.
- Only keys in an allowlist are taken from a project layer:

```rust
/// Keys a project file may set. Everything else is global-only: keys that run commands (on_start, lsp, before_tool),
/// move data (custom_providers, headroom_*, accounts, environments) or loosen approval (permission_mode, reviewer_model, trusted_projects, approved_mcp).
const PROJECT_KEYS: &[&str] = &["model", "default_model", "thinking", "default_thinking", "small_model", "output_style", "caveman", "ponytail", "auto_compact", "reserve_tokens", "keep_recent_tokens", "condense", "context_windows", "expand_tools", "subagents", "rtk"];
```

  An allowlist, so a key added later is global-only until someone decides
  otherwise. `settings.local.json` follows the same list: it sits in the
  checkout, so a repository can ship one.
- A key outside the list is ignored and reported once at session start:
  `<path>: ignored "<key>", a project cannot set it`.
- A project file that does not parse is an error with the same text as a
  broken global file, naming its path.
- `/settings` keeps writing to the global file. A key a project overrides
  stays overridden; the docs say so.
- Trust given with `/trust` applies the layers to sessions started after it.

## Commands you will need

| Purpose | Command (in `source/`) | Expected |
|---|---|---|
| Tests | `cargo test --locked settings` | all pass |
| All tests | `cargo test --locked` | all pass |
| Lint | `cargo clippy --all-targets --locked -- -D warnings` | exit 0 |
| Format | `cargo fmt --check` | exit 0 |

On Artur's MacBook every cargo command goes through `offload '<command>'`.

## Scope

**In scope**:
- `source/src/settings.rs`
- `source/src/agent/prompt.rs` (the `names` list in `untrusted_files`)
- the `settings::load` call sites named in step 3
- `source/src/tui/mod.rs` (only the notice in step 4)
- `source/docs/configuration.md`, `README.md` (one clause in "One folder, nothing else")

**Out of scope**:
- `save_change`, `saved_after`, `apply_changes`: saving stays global.
- A `/settings` view that marks overridden keys.
- Writing `.gitignore` entries for `settings.local.json`.
- Any key outside `PROJECT_KEYS`, however harmless it looks. Report it instead.

## Git workflow

- Branch `feat/project-settings`.
- Commit: `feat(settings): project and local settings layers`.
- Do not push or open a PR unless told to.

## Steps

### Step 1: `load_in` in `settings.rs`

```rust
/// The global settings with the trusted project's `.agents/settings.json` and `settings.local.json` over them.
pub fn load_in(home: &Path, cwd: &Path) -> Result<Settings, String>
/// One line per key a project file sets that only the global file may set.
pub fn ignored_project_keys(home: &Path, cwd: &Path) -> Vec<String>
```

`load_in`: read the global JSON; take `trusted_projects` from it; ask
`prompt::is_trusted`; list `prompt::project_agents_dirs(cwd, trusted)`,
drop `home/.agents`, reverse (farthest first); for each dir and each of the
two file names read the JSON if the file exists and copy the `PROJECT_KEYS`
it has over the merged value; then `from_json`. Keep whatever validation
`load` does at that point. Reading a layer reuses the parse-and-error code
of `read`: factor `read`'s body into a helper that takes a path, do not copy it.

**Verify**: `cargo test --locked settings` passes with the step's tests from "Test plan".

### Step 2: untrusted notice

Add `"settings.json"` and `"settings.local.json"` to `names` in
`prompt::untrusted_files`, so an untrusted project's notice lists them.

**Verify**: `cargo test --locked prompt` passes; if a test pins the list, extend it.

### Step 3: switch the session call sites

A call switches to `load_in(home, cwd)` when it builds or refreshes the
settings a session runs with and a session cwd is in scope. A call that reads
one global-only key stays on `load`.

| Call | Action |
|---|---|
| `agent/mod.rs:1966` (`move_to`) | `load_in` with the new `cwd` argument: verified, the session moves to another checkout |
| `agent/mod.rs:1035` | stays: reads `environments`, verified |
| `main.rs:176` (`update_channel`), `main.rs:211` (`disabled_plugins`), `service.rs:74`, `serve.rs:166`, `serve.rs:255`, `tui/client.rs:126` | stay: global-only keys or no session cwd. Confirm by reading each |
| `main.rs:434`, `main.rs:518`, `acp.rs:174`, `acp.rs:252`, `acp.rs:532`, `agent/mod.rs:1061`, `1115`, `1299` | read each; apply the rule above |

For every call you switch whose result is `mut` and later saved: confirm the
save goes through `save_change(home, &before, &after)`. See STOP conditions.

**Verify**: `cargo test --locked` passes. `rg -n 'settings::load_in\(' src` lists at least `agent/mod.rs` (`move_to`) and the session start in `main.rs`.

### Step 4: report ignored keys

In `src/tui/mod.rs`, where the untrusted-files notice is built (line 2322),
also show the lines of `settings::ignored_project_keys` for a trusted
project, using the same notice call the neighbouring code uses.

**Verify**: `cargo test --locked` passes.

### Step 5: only if plan 001 landed

Check: `jq -e '.properties.permissions' src/settings.schema.json`. If it
exits 0: a project layer's `permissions.ask` and `permissions.deny` are
**appended** to the merged lists; its `permissions.allow` is ignored and
reported like any global-only key. A repository may tighten approval, never
loosen it. If the check fails, skip this step and say so in your report.

**Verify**: the two extra tests in "Test plan" pass.

### Step 6: docs

`source/docs/configuration.md`, section "A project's `.agents`": add the two
files, the layer order, the `PROJECT_KEYS` list, the sentence that `/settings`
writes the global file, and the advice to add `.agents/settings.local.json`
to `.gitignore`. `README.md`: in "One folder, nothing else" add the project
settings to what a trusted project can add.

**Verify**: clippy and `cargo fmt --check` exit 0.

## Test plan

In `settings.rs` tests, modelled on `a_broken_file_is_an_error_and_unknown_levels_load_as_defaults` (line 622): a temp home under `std::env::temp_dir()` named with the process id, plus a temp project dir holding an empty `.git` directory.

- untrusted project: its `model` is not applied.
- trusted: `model` from `.agents/settings.json` wins over global.
- `settings.local.json` wins over `settings.json` in the same dir.
- nested: `<root>/sub/.agents/settings.json` wins over `<root>/.agents/settings.json` when cwd is `sub`.
- `permission_mode`, `on_start`, `trusted_projects`, `custom_providers` in a project file do not change the result, and `ignored_project_keys` names each.
- a project file listing its own root in `trusted_projects` does not become trusted.
- a broken project file makes `load_in` return `Err` naming that file.
- cwd equal to home: the global file is applied once (no ignored-key lines).
- `save_change` after `load_in`: change one unrelated key, reload with `load`, and the project's `model` is not in the global file.
- step 5: a project `deny` rule is in the merged rules; a project `allow` rule is not.

Check by hand that the allowlist test fails when `permission_mode` is added to `PROJECT_KEYS`, then remove it again.

## Done criteria

- [ ] `cargo test --locked` exits 0, with the new tests
- [ ] clippy and `cargo fmt --check` exit 0
- [ ] `rg -n 'PROJECT_KEYS' source/src/settings.rs` shows the const and its use
- [ ] `rg -n '"permission_mode"|"on_start"|"trusted_projects"|"approved_mcp"|"custom_providers"|"lsp"|"accounts"' source/src/settings.rs` shows none of them inside `PROJECT_KEYS`
- [ ] `git status --porcelain` lists only in-scope files
- [ ] `plans/README.md` row updated

## STOP conditions

- A switched call site saves the whole `Settings` (a `save(` or `to_json(&settings)` written to disk) instead of `save_change`: a project value would leak into the global file. Report the site.
- A call site has no session cwd but clearly feeds a session.
- You want to add a key to `PROJECT_KEYS`. Report the key and why.
- `prompt::is_trusted` or `project_agents_dirs` does not match the excerpts.
- A step's verification fails twice.

## Maintenance notes

- Every new settings key is global-only until it is added to `PROJECT_KEYS`; that decision belongs in review, with the question "what can a hostile repository do with this key".
- Reviewers: the trust check reads the global file only; `load` stays for global callers.
- Deferred: marking overridden keys in `/settings`, applying layers right after `/trust`, a per-key merge for objects such as `condense` (today the project's object replaces the global one).
