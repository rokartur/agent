# Tools

Built-in tools are defined in `tools/mod.rs` (`schemas` gives the JSON schema list the model sees, `run` dispatches by name). MCP tools arrive as `mcp__<server>__<tool>`, plugin tools as `<plugin>__<tool>`.

| Group | Tools | Code |
|---|---|---|
| Files | `Read`, `Write`, `Edit`, `ReplaceLines`, `InsertLines`, `Grep` | `tools/mod.rs`, anchors in `tools/hashline.rs` |
| Shell | `Bash`, `TaskOutput`, `TaskStop` | `tools/bg.rs`, `tools/rtk.rs`, `tools/guard.rs` |
| Agents | `Agent`, `WaitForAgents`, `SendMessage`, `ListAgents`, `StartSession`, `Workflow` | `agent/subagent.rs`, `agent/workflow.rs`, `agent/peers.rs`, `serve.rs` |
| Planning | `Todo`, `Goal`, `EnterPlanMode`, `ExitPlanMode`, `Skill` | `tools/todo.rs`, `tools/goal.rs`, `tools/plan.rs` |
| User | `AskUserQuestion`, `PushNotification`, `Page` | `tools/ask.rs`, `tui/notify.rs`, `tools/page.rs` |
| Time | `ScheduleWakeup`, `CronCreate`, `CronDelete`, `CronList` | `tools/cron.rs` |
| Web | `WebSearch`, `WebFetch`, `GetSearchContent` | `tools/web/` |
| Git | `EnterWorktree`, `ExitWorktree` | `worktree.rs` |
| Scripts | `Code` | `agent/code.rs` |

`--tools a,b` (print, headless, acp) and `--parent` (subagents) narrow the list; `Agent.only_tools` is the filter.

## Line anchors

`Read` and `Grep` serve each line with a 4-letter anchor (`ABCD│text`). `Edit`, `ReplaceLines` and `InsertLines` address lines by anchor, never by number or content. The registry (`tools/hashline.rs`) is session-wide: an anchor names one line of one file at one version, and an edit that replaces lines retires their anchors. An edit naming a retired or unknown anchor is refused, which is how a file changed since the last read cannot be overwritten silently.

Every anchor is four ASCII letters so a long session can hold one per line read without growing the context much.

## PDF

`Read` on a file that starts with `%PDF-` returns its text through poppler's `pdftotext -layout` (`tools/pdf.rs`), with no anchors: 20 pages per call, `pages` (`"3"`, `"21-40"`) picks others, 50KB at most. A scan has no text, so the result names the `pdftoppm` command that renders its pages to PNG files, which `Read` shows as images.

## Bash

`Bash` runs `sh -c` with the command first passed through `rtk rewrite` when `rtk` is on and installed (`tools/rtk.rs`): rtk exits 0 or 3 with a replacement, 1 when it has none, 2 when it refuses. Output loses ANSI sequences and keeps its last `max_bytes`; the full log lands in a temp file named at the end.

`tools/guard.rs` refuses three command shapes before they run: a blind `sleep` loop, `gh pr merge` chained after `gh pr checks` with `;` or a pipe (drops the exit status), and plain file reads (`cat`, `head`, `sed -n`) mixed with other commands. A command that is only file reads runs as `Read` in the same turn so the lines come back with anchors.

`run_in_background: true` starts the command in its own process group tracked by `tools/bg.rs`; its exit arrives as a follow-up message, `TaskOutput` peeks, or with `block` waits for the exit up to `timeout` ms, `TaskStop` sends SIGTERM then SIGKILL; on Windows it ends the process tree with taskkill at once.

## Code mode

Experimental, off by default: `code_mode` in `settings.json` ("Code mode" in `/settings`) offers the `Code` tool. On Anthropic its definition stays loaded where other rare tools wait behind tool search (`CORE_TOOLS` in `api/request.rs`): a model that sees only the name never writes a script. A project's settings file cannot turn it on. The model sends a JavaScript script, the body of an async function, and `agent/code.rs` runs it in an embedded QuickJS sandbox with two globals (`agent/code-harness.js`): `tool(name, input)` and `` $`cmd` ``, which is `tool("Bash", ...)` with interpolated values shell-quoted; a value that is `undefined`, `null`, an object, a function or a promise the script did not await throws a `TypeError` and no command runs. The tool result is the lines the script printed with `console.log`, then its `return` value; nothing the calls print reaches the model unless the script passes it on.

Each `tool()` call goes through `Agent::run_call`, the function a call from the model's reply goes through, so it gets the same permission check, `before_tool` hooks, plan-mode block, rewind checkpoint, LSP report and edit hooks, and shows in the transcript as its own row under the `Code` row (id `<code call id>-<n>`). A script reaches only the tools offered to the model, so `--tools` and a subagent's shorter list hold inside it, and it cannot call `Code`. Calls run one at a time on the agent's thread, in the order made, with one exception: foreground `Bash` calls the script makes together, before it awaits any of them (`Promise.all`), run at the same time (`Agent::bash_together`). Each is checked in turn first, guards, rules, hooks and prompts, then those let through run on their own threads, 8 at once, between one pair of rewind snapshots. A "No" at any of the prompts runs none of them. `cd DIR`, a background command and every call under an editor's terminal (ACP) stay sequential.

In `Auto` the reviewer judges the script once, before it starts, and its calls then skip the reviewer, as in Claude Code: one review where a 50-call script took 50. The reviewer sees the script's text, not the values it computes, so it is told to deny a script whose commands or edits cannot be told from that text, and that a value `$` interpolates is quoted as one argument, so a path read at run time does not make the command unknown. Deny rules, ask rules and `before_tool` hooks still see every call with its real input, and a call asks the user again once the mode leaves `Auto`. A script over `REVIEW_DETAIL_CHARS` is not reviewed as a whole; each of its calls is, as before.

A script's `Bash` calls run as written, because the script parses their output: no `rtk` rewrite, and `cat FILE` prints the file where a direct call would run as `Read` and return anchored rows; a `Read` deny rule on the file still refuses it. A `before_tool` hook matching `Code` reads the script from `input.code`.

Limits: 10 minutes of the script's own work, its JavaScript and its tool calls, not counting the time a permission prompt or `AskUserQuestion` waits for the user; 512 MB of JavaScript memory; no state kept between scripts; the session history holds only the `Code` call and its result, so a resumed transcript shows the script's row without its calls, as it shows an `Agent` row without the subagent's. The row is headed by the script's first line and, expanded, shows the whole script.

`run_in_background` starts the script in a child process, `agent --headless --parent <id> --script`, with the script on stdin where a subagent gets its task (`Agent::script_in_background`, `Agent::background_script`). The `Code` call returns the run's id (`sa-N`) at once, so the model answers the user while the script's commands run; the value arrives as a follow-up message when the script ends, `WaitForAgents` waits for it, `TaskStop` stops it, and `/tasks` lists it with its call count. The limit there is 2 hours. Nobody answers a prompt in the child, so the whole script is allowed first: full access, or `Auto` with the reviewer's one verdict; `Ask`, plan mode, `--tools` and a script too long to review refuse `run_in_background`. Deny rules, ask rules and `before_tool` hooks still see each call, and a call an ask rule matches ends the script. A mode or plan change after the start does not reach the child. The child has a subagent's tools plus the web tools, runs the edit hooks itself and adds their failures to the value, and writes no rewind checkpoints, as a background subagent writes none. shortcut: the model picks the background when it writes the script; a script already running in the foreground cannot move there, so a message typed meanwhile waits for it.

## Permissions

`agent/permissions.rs`. Three modes: `Ask`, `Auto` (a reviewer model approves), `FullAccess`. `request_for(tool, input, files)` decides which calls need approval and under which `Kind`: `Command` (Bash), `Edit` (every file an edit call touches), `Subagent`, `Workflow`, `Web` (WebFetch, WebSearch with `includeContent`), `Mcp`, `Plugin`, and worktree moves. Everything else runs in every mode.

"Don't ask again" stores a `Grant` keyed by the request's key (a command prefix, a file, a workflow path) for the session. In `Auto` the reviewer sees the request capped at `REVIEW_DETAIL_CHARS`; a longer call is denied unreviewed, since a cut would hide its tail.

`permissions` in `settings.json` holds rules that outlive the session:

```json
"permissions": {"allow": ["Bash(cargo test*)", "mcp__github__*"], "ask": ["Bash(git push*)"], "deny": ["Read(.env*)", "Edit(**/secrets/**)"]}
```

A rule is `Tool` or `Tool(pattern)`; `*` in the tool name matches any run of characters. The pattern is matched against the command (`Bash`), the URLs (`WebFetch`, `Video`) or the files (`Read`, the edit tools, and for `Grep` its `path` and every file a folder search reads, as `rg --files` lists them with the call's `hidden`, `glob` and `type`). In a command or URL `*` matches anything. A file pattern is an edit-hooks glob: without a slash it matches the file name, otherwise the path from the working directory, or an absolute one when it starts with `/` or `~/`. Deny beats ask beats allow. Deny refuses in every mode, `FullAccess` included; ask prompts the user in `Ask` and `Auto` whatever was granted; allow runs the call in `Ask` and `Auto`. Deny and ask also test each part of a chained command, and allow never covers a command that chains, substitutes or redirects, so `Bash(cargo test*)` does not approve `cargo test; rm -rf ~`. A rule that does not parse stops the agent at start (`permissions::check_rules`). Rules are not a sandbox: a deny rule for `Read` does not stop a shell command that reads the same file, or a `Grep` over the directory that holds it, and a command pattern does not see through `sh -c`, a variable or `$(...)`.

### before_tool hooks

`before_tool` in `settings.json` runs a command before a tool call and can refuse it:

```json
"before_tool": [
  {"match": "Bash(git push*)", "command": "~/bin/check-push {call}"},
  {"match": "Edit", "command": "test ! -e .release-freeze"}
]
```

`match` is a permission rule, matched the way a deny rule is. The command runs in the session's directory with a 10 s limit, in every mode and for tools that never ask. `{call}` is the path of a private file holding `{"tool", "input", "cwd"}`, removed when the command ends. Exit 0 lets the call go on to the rules and the mode. Any other exit, or the time limit, refuses it: the model reads `Blocked by the before_tool hook "<command>":` and the first 4000 characters of the output. The hook's own command is not permission-checked (`Agent::before_tool_refusal`).

## Edit hooks and language servers

After each edit the result carries two kinds of feedback: new diagnostics from a language server started for the file's language (`agent/lsp.rs`, configured in settings), and the output of the `onStop` commands in the nearest `.agents/edit-hooks.json` (`agent/edit_hooks.rs`, format in [configuration.md](configuration.md)). Hooks for every file edited this prompt run again when the turn ends; failures go back to the model for a bounded number of fix rounds (`edit_hooks::report`).
