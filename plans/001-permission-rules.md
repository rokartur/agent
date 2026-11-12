# Plan 001: Settings hold allow, ask and deny rules per tool

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in "STOP conditions" occurs, stop and report, do not
> improvise. When done, update this plan's row in `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat 52b32af..HEAD -- source/src/agent/permissions.rs source/src/agent/mod.rs source/src/agent/edit_hooks.rs source/src/settings.rs source/src/settings.schema.json`
> If any of these changed, compare the "Current state" excerpts with the live
> code before you proceed. A mismatch is a STOP condition.

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: MED (it sits in the one function every tool call passes)
- **Depends on**: none
- **Category**: security
- **Planned at**: commit `52b32af`, 2026-11-12

## Why this matters

Today the only lasting permission choice is the mode (`ask`, `auto`,
`full access`). "Don't ask again" lives in `Grants`, a `HashSet<String>` that
dies with the session and matches one exact command. A user cannot say "never
read `.env`", "always allow `cargo test`", or "always ask before `git push`".
After this plan a `permissions` key in `~/.agents/settings.json` holds three
lists of rules, and `deny` holds in every mode, `full access` included.

## Current state

All paths are under `source/`. Run cargo there.

- `src/agent/permissions.rs`: `Mode`, `Kind`, `Request`, `request_for`
  (line 99), `Grants` (line 247), tests from line 337.
- `src/agent/mod.rs:1629`: `check_permission`, the single gate. `run_tool`
  (line 837), `run_tool_with_checkpoint` (line 1748) and `run_edit_hooks`
  (line 798, with tool `"Bash"`) call it.
- `src/agent/edit_hooks.rs:124`: `fn matches(pattern, path) -> bool`, a glob
  over `/`-separated text: `*` and `?` stay in a segment, `**` crosses
  segments, `{a,b}` picks one. Private today.
- `src/settings.rs`: `Settings` struct (line 13), `from_json` (about line
  396), `to_json` (about line 540), `load` (line 380).
- `src/settings.schema.json`: one entry per settings key.

`check_permission`, the part this plan changes (`src/agent/mod.rs:1652`):

```rust
let files = if tool == "Workflow" { &saved_file } else { edited_files };
let request = permissions::request_for(tool, input, files)?;
let mode = *self.permission_mode.lock().unwrap();
if mode == Mode::FullAccess || self.grants.allows(&request) {
    return None;
}
if mode == Mode::Auto {
    // ImageGenerate counts as an edit but also uploads its prompt and images, so it stays reviewed.
    if tool != "ImageGenerate" && request.kind == permissions::Kind::Edit && permissions::edits_inside_workspace(files, &self.cwd, &self.extra_dirs) {
        return None;
    }
    return self.review(&request, input, emit);
}
let (reply, answer) = mpsc::channel();
emit(Event::Permission { call: call.map(String::from), request: request.clone(), reply });
```

`request_for` returns `None` for `Read`, `Grep` and every other tool that
never asks, and the `?` returns at once. So a deny rule for `Read` must be
checked above that line.

`load` cannot fail on content today:

```rust
pub fn load(home: &Path) -> Result<Settings, String> {
    Ok(from_json(&read(home)?))
}
```

Conventions: functions and plain data, no traits; rustfmt with very long
lines (`rustfmt.toml`); comments only for a why, one line; tests in a
`#[cfg(test)] mod tests` at the end of the same file; error text names the
file and the fix (`"{} is broken: {e}. Fix or remove it by hand."`).

## The design (decided, do not redesign)

Settings:

```json
"permissions": {
  "allow": ["Bash(cargo test*)", "mcp__github__*"],
  "ask":   ["Bash(git push*)"],
  "deny":  ["Read(.env*)", "Edit(**/secrets/**)", "WebFetch"]
}
```

A rule is `Tool` or `Tool(pattern)`. `Tool` is matched against the tool name
with `edit_hooks::matches` (so `mcp__github__*` works). A rule with no pattern
covers every call of the tool.

Subjects, what a pattern is matched against:

| Tool | Subjects |
|---|---|
| `Bash` | the command |
| `Write`, `Edit`, `ReplaceLines`, `InsertLines`, `ImageGenerate` | each edited file (the `files` argument, absolute) |
| `Read` | `file_path`, and every entry of `paths`, resolved with `tools::hashline::resolve(cwd, path)` |
| `WebFetch` | each URL (`urls` and `url`) |
| `Video` | `source` |
| anything else | none: only a rule without a pattern can match |

Pattern matching:

- File subjects use the edit-hooks convention: a pattern with no `/` is
  matched against the file name, a pattern that starts with `/` against the
  absolute path, `~/` is replaced with the home directory first, any other
  pattern against the path relative to `cwd` (no match when the file is
  outside `cwd`).
- `Bash`, `WebFetch` and `Video` subjects are free text where `/` is not a
  separator: turn every `*` of the pattern into `**` before calling `matches`
  (`pattern.replace("**", "*").replace('*', "**")`).
- A call with several subjects: `deny` and `ask` match when any subject
  matches, `allow` only when every subject matches.
- `Bash` and compound commands: split the command on `;`, `&`, `|` and
  newline, trim the parts. `deny` and `ask` test the whole command and each
  part. `allow` never matches a command that contains any of `;`, `&`, `|`,
  a newline, a backtick, `$(`, `>` or `<`. So `Bash(cargo test*)` in `allow`
  cannot approve `cargo test; rm -rf ~`.

Verdict order: `deny`, then `ask`, then `allow`.

| Verdict | Effect in `check_permission` |
|---|---|
| deny | return `Some("Denied by the permission rule \"<rule>\" in settings.json.")` in every mode. Do not set `self.abort`: the model may try another way. |
| ask | in `ask` and `auto` mode skip the grants and the reviewer and prompt the user. In `full access` it does nothing. Has no effect on tools for which `request_for` is `None`. |
| allow | in `ask` and `auto` mode return `None` (run). |

A rule that does not parse (no tool name, `(` without a final `)`) makes
`settings::load` fail with
`"<path>: permissions.<list>: \"<rule>\" is not Tool or Tool(pattern)"`. A
deny rule that silently matched nothing would fail open.

This is a rule list, not a sandbox: `deny Read(.env*)` does not stop
`Bash(cat .env)`. The docs must say so.

## Commands you will need

| Purpose | Command (in `source/`) | Expected |
|---|---|---|
| Tests | `cargo test --locked permissions` | all pass |
| All tests | `cargo test --locked` | all pass |
| Lint | `cargo clippy --all-targets --locked -- -D warnings` | exit 0 |
| Format | `cargo fmt --check` | exit 0 |

On Artur's MacBook every cargo command goes through `offload '<command>'`.

## Scope

**In scope**:
- `source/src/agent/permissions.rs`
- `source/src/agent/mod.rs` (`check_permission`, the `Agent` field, the places that copy settings into the agent)
- `source/src/agent/edit_hooks.rs` (only: `fn matches` becomes `pub(crate)`)
- `source/src/settings.rs`, `source/src/settings.schema.json`
- `source/docs/tools.md` (Permissions section), `README.md` (one sentence)

**Out of scope**:
- The `/permissions` command and any TUI panel for rules. Rules are edited in `settings.json`.
- Writing a rule from the "don't ask again" prompt answer. `Grants` stays as it is.
- Project-level rules. Plan 003 owns that.
- `REVIEW_SYSTEM` and the reviewer.

## Git workflow

- Branch `feat/permission-rules`.
- Conventional Commits, as in `git log`: `fix(permissions): show the reviewer the user's last three requests`. Suggested: `feat(permissions): allow, ask and deny rules in settings`.
- Do not push or open a PR unless told to.

## Steps

### Step 1: the rule type and matcher in `permissions.rs`

Add, above `Grants`:

```rust
#[derive(Clone, Default, PartialEq, Debug)]
pub struct Rules {
    pub allow: Vec<String>,
    pub ask: Vec<String>,
    pub deny: Vec<String>,
}

#[derive(PartialEq, Debug)]
pub enum Verdict {
    Deny(String), // the rule that matched
    Ask,
    Allow,
}

/// None when a rule is not `Tool` or `Tool(pattern)`.
fn parse_rule(rule: &str) -> Option<(&str, Option<&str>)>
pub fn check_rules(rules: &Rules) -> Result<(), String>
pub fn subjects(tool: &str, input: &Value, files: &[PathBuf], cwd: &Path) -> Vec<String>
pub fn verdict(rules: &Rules, tool: &str, subjects: &[String], cwd: &Path, home: &Path) -> Option<Verdict>
```

Make `edit_hooks::matches` `pub(crate)` and call it. Do not copy the glob code.

For `Read` with `paths`, open `Anchors::read_many` in
`src/tools/hashline.rs` and accept exactly the entry shapes it accepts.

**Verify**: `cargo test --locked permissions` passes with the tests from "Test plan" for this step.

### Step 2: the settings key

In `settings.rs` add `pub permissions: crate::agent::permissions::Rules` to
`Settings`, read it in `from_json` (missing key gives `Rules::default()`),
write it in `to_json`, and validate in `load`:

```rust
pub fn load(home: &Path) -> Result<Settings, String> {
    let settings = from_json(&read(home)?);
    crate::agent::permissions::check_rules(&settings.permissions).map_err(|e| format!("{}: {e}", path(home).display()))?;
    Ok(settings)
}
```

Add `permissions` to `settings.schema.json`: an object with `allow`, `ask`,
`deny`, each an array of strings, default empty, with a `description` that
gives the rule syntax in two sentences. Copy the shape of a neighbouring
entry. Do not add it to `/settings` (`SETTING_NAMES` in `tui/mod.rs`).

**Verify**: `cargo test --locked settings` passes. If a test compares the schema keys with `to_json`, it must pass too.

### Step 3: `check_permission`

Add `permission_rules: permissions::Rules` to `Agent`. Find every place the
agent copies a setting: `rg -n 'trusted_projects = settings|trusted_projects: settings' src/agent/mod.rs`.
Set `permission_rules` at each of those places (one is `move_to`, line 1972).

In `check_permission`, between the `let files = ...` line and the
`request_for` line, compute the verdict once and apply the table above:

```rust
let verdict = permissions::verdict(&self.permission_rules, tool, &permissions::subjects(tool, input, files, &self.cwd), &self.cwd, &self.home);
if let Some(Verdict::Deny(rule)) = &verdict {
    return Some(format!("Denied by the permission rule \"{rule}\" in settings.json."));
}
let request = permissions::request_for(tool, input, files)?;
let mode = *self.permission_mode.lock().unwrap();
let ask = verdict == Some(Verdict::Ask);
if mode == Mode::FullAccess || (!ask && (verdict == Some(Verdict::Allow) || self.grants.allows(&request))) {
    return None;
}
if mode == Mode::Auto && !ask {
    ...unchanged...
}
```

The rest of the function stays as it is.

**Verify**: `cargo test --locked check_permission` passes, old and new tests.

### Step 4: docs

- `source/docs/tools.md`, section "Permissions": add one paragraph with the
  JSON example, the verdict order, the compound-command rule and the sentence
  "Rules are not a sandbox: a deny rule for Read does not stop a shell command
  that reads the same file."
- `README.md`, bullet "You choose how much it does alone": add one sentence
  that `permissions` in `settings.json` allows, asks or denies per tool.

**Verify**: `cargo fmt --check` and `cargo clippy --all-targets --locked -- -D warnings` exit 0.

## Test plan

In `permissions.rs` tests (pattern: `review_denies_unless_its_first_word_is_allow`, table-style asserts):

- `parse_rule`: `Bash`, `Bash(cargo test*)`, `mcp__github__*` parse; `Bash(`, `(x)`, `` fail.
- deny beats ask beats allow when all three match.
- `allow Bash(cargo test*)` matches `cargo test -q`, not `cargo test; rm -rf ~`, not `cargo test > /etc/x`, not `cargo test $(x)`.
- `deny Bash(rm -rf*)` matches `cd x && rm -rf y`.
- `deny Read(.env*)` matches `/p/.env.local` by name; `Edit(src/**)` matches `/p/src/a/b.rs` with cwd `/p` and not `/q/src/a.rs`.
- an edit of two files: `allow` needs both to match, `deny` needs one.
- `WebFetch(https://docs.rs/*)` matches `https://docs.rs/a/b`.
- a rule with a pattern never matches a tool with no subjects.

In `agent/mod.rs` tests (pattern: `check_permission_asks_once_per_grant_and_a_closed_reply_denies`, line 3274):

- deny refuses in `FullAccess` and emits no `Event::Permission`.
- allow runs in `Ask` mode with no event.
- ask prompts in `Auto` mode and prompts again after an `AllowAlways` answer.
- a `Read` deny refuses although `request_for` is `None` for `Read`.

In `settings.rs` tests: a bad rule makes `load` return `Err` that names the rule.

Each new test must fail when its rule is removed: check one by hand (delete the deny branch, see the test fail, restore it).

## Done criteria

- [ ] `cargo test --locked` exits 0, with the new tests
- [ ] `cargo clippy --all-targets --locked -- -D warnings` exits 0
- [ ] `cargo fmt --check` exits 0
- [ ] `jq '.properties.permissions' source/src/settings.schema.json` prints an object
- [ ] `rg -n 'fn matches' source/src` shows one definition
- [ ] `git status --porcelain` lists only in-scope files
- [ ] `plans/README.md` row updated

## STOP conditions

- `check_permission` does not match the excerpt.
- Some caller runs a tool without `check_permission` (`rg -n 'tools::run\(' src`): report the list instead of adding a second gate. `run_edit_hooks` running the hook command after its own check is expected.
- Subagent processes turn out not to call `settings::load` (then rules would not reach them): report.
- A step's verification fails twice.

## Maintenance notes

- A new tool with a path or URL argument needs a row in `subjects`, else only pattern-less rules cover it.
- Reviewers: the compound-command guard on `allow`, and that deny is tested above `request_for`.
- Deferred: a TUI editor for rules, saving "don't ask again" as a rule, project rules (plan 003), `ask` for tools that never prompt.
