# Tools

Built-in tools are defined in `tools/mod.rs` (`schemas` gives the JSON schema list the model sees, `run` dispatches by name). MCP tools arrive as `mcp__<server>__<tool>`, plugin tools as `<plugin>__<tool>`.

| Group | Tools | Code |
|---|---|---|
| Files | `Read`, `Write`, `Edit`, `ReplaceLines`, `InsertLines`, `Grep` | `tools/mod.rs`, anchors in `tools/hashline.rs` |
| Shell | `Bash`, `TaskOutput`, `TaskStop` | `tools/bg.rs`, `tools/rtk.rs`, `tools/guard.rs` |
| Agents | `Agent`, `WaitForAgents`, `SendMessage`, `ListAgents`, `Workflow` | `agent/subagent.rs`, `agent/workflow.rs`, `agent/peers.rs` |
| Planning | `Todo`, `Goal`, `EnterPlanMode`, `ExitPlanMode`, `Skill` | `tools/todo.rs`, `tools/goal.rs`, `tools/plan.rs` |
| User | `AskUserQuestion`, `PushNotification` | `tools/ask.rs`, `tui/notify.rs` |
| Time | `ScheduleWakeup`, `CronCreate`, `CronDelete`, `CronList` | `tools/cron.rs` |
| Web | `WebSearch`, `WebFetch`, `GetSearchContent` | `tools/web/` |
| Git | `EnterWorktree`, `ExitWorktree` | `worktree.rs` |
| Context | `ContextTreeQuery` | `agent/condense.rs` |

`--tools a,b` (print, headless, acp) and `--parent` (subagents) narrow the list; `Agent.only_tools` is the filter.

## Line anchors

`Read` and `Grep` serve each line with a 4-letter anchor (`ABCD│text`). `Edit`, `ReplaceLines` and `InsertLines` address lines by anchor, never by number or content. The registry (`tools/hashline.rs`) is session-wide: an anchor names one line of one file at one version, and an edit that replaces lines retires their anchors. An edit naming a retired or unknown anchor is refused, which is how a file changed since the last read cannot be overwritten silently.

Every anchor is four ASCII letters so a long session can hold one per line read without growing the context much.

## Bash

`Bash` runs `sh -c` with the command first passed through `rtk rewrite` when `rtk` is on and installed (`tools/rtk.rs`): rtk exits 0 or 3 with a replacement, 1 when it has none, 2 when it refuses. Output loses ANSI sequences and keeps its last `max_bytes`; the full log lands in a temp file named at the end.

`tools/guard.rs` refuses three command shapes before they run: a blind `sleep` loop, `gh pr merge` chained after `gh pr checks` with `;` or a pipe (drops the exit status), and plain file reads (`cat`, `head`, `sed -n`) mixed with other commands. A command that is only file reads runs as `Read` in the same turn so the lines come back with anchors.

`run_in_background: true` starts the command in its own process group tracked by `tools/bg.rs`; its exit arrives as a follow-up message, `TaskOutput` peeks, `TaskStop` sends SIGTERM then SIGKILL.

## Permissions

`agent/permissions.rs`. Three modes: `Ask`, `Auto` (a reviewer model approves), `FullAccess`. `request_for(tool, input, files)` decides which calls need approval and under which `Kind`: `Command` (Bash), `Edit` (every file an edit call touches), `Subagent`, `Workflow`, `Web` (WebFetch, WebSearch with `includeContent`), `Mcp`, `Plugin`, and worktree moves. Everything else runs in every mode.

"Don't ask again" stores a `Grant` keyed by the request's key (a command prefix, a file, a workflow path) for the session. In `Auto` the reviewer sees the request capped at `REVIEW_DETAIL_CHARS`; a longer call is denied unreviewed, since a cut would hide its tail.

## Edit hooks and language servers

After each edit the result carries two kinds of feedback: new diagnostics from a language server started for the file's language (`agent/lsp.rs`, configured in settings), and the output of the `onStop` commands in the nearest `.agents/edit-hooks.json` (`agent/edit_hooks.rs`, format in [configuration.md](configuration.md)). Hooks for every file edited this prompt run again when the turn ends; failures go back to the model for a bounded number of fix rounds (`edit_hooks::report`).
