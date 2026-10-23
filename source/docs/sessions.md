# Sessions

A session is one JSONL file, `~/.agents/sessions/<id>.jsonl`, appended to as the conversation goes. Nothing is rewritten: rewind, compaction and resets are entries too, and `Session::load` (`agent/mod.rs`) replays the file top to bottom into a `History`.

## Journal entries

`type` | Written by | Replay
---|---|---
`session` | first entry: `id`, `cwd`, `model`, `timestamp` | identifies the file in `/resume` |
`session_info` | `/rename-title` and the auto title | `name` |
`parent` | a subagent | hides the session from `/resume` and `--continue` |
`message` | every user and assistant message, Anthropic block shape | pushed to `messages` |
`usage` | every reply: tokens, cost, `provider`, `account` | last account per provider, so a resume keeps it |
`spent` | per user command: input, output, ms | footer totals survive a resume |
`tool_time` | per tool call: `id`, `name`, `ms` | ignored |
`bad_tool_input` | a tool call whose JSON input did not parse | ignored |
`tool_checkpoint` | `index`, `before`, `after` snapshot trees around one tool call | `checkpoints` |
`checkpoints_reset` | a worktree move | clears `checkpoints` |
`rewind` | `index` | truncates `messages`, drops later checkpoints and compactions |
`compaction` | `cut`, `summary` | the provider sees the summary instead of `messages[..cut]` |
`condense` | refs and summary of a tool-call batch | the provider sees the stub, `ContextTreeQuery` returns the original |
`goal`, `plan`, `todo` | the tools of the same name | restored state |
`extra_dirs` | `/workspace` | restored list |

Indexes in `rewind`, `tool_checkpoint` and `compaction` are message indexes. Two processes writing one file would interleave, so a second process opening a session copies it instead (`Session.forked_from`); the open file holds a lock for that.

## Resume, continue, clone, import

`-c` opens the newest session whose `cwd` is the current directory; `-r [id]` any session by id prefix or from a list. `/clone` copies the file to a new id at the current position. `/import file.jsonl` reads any journal, Claude Code exports included where the block shape matches. `/export` writes `.jsonl` or `.md`.

`prune_sessions` in settings deletes files untouched this long at start; an open session holds its lock and stays.

## Rewind

Esc Esc on an idle session lists `RewindPoint`s: every user prompt with its index. Picking one truncates the conversation and, inside a git repo, restores the work tree from the checkpoint snapshots (`agent/snapshots.rs`): a private repo under `~/.agents/snapshots` holds a tree per tool call that edited files. Ignored files are not stored. Files changed since the agent last wrote them are left alone and listed (`RestoredFiles`).

## Compaction and condense

Two mechanisms shrink the provider's view, both keep the originals in `messages`:

- **Compaction** (`Agent::compact`): one summary replaces everything before `cut`. Runs on `/compact [focus]`, `/handoff`, and automatically when context tokens pass `context_window - reserve_tokens`. The summary prompt is `SUMMARY_PROMPT` in `agent/mod.rs`; `keep_recent_tokens` stays verbatim.
- **Condense** (`agent/condense.rs`, pi-condense's design): batches of finished tool calls are summarized on a background thread by the cheap model, each call labelled `[[N:name]]` and given a short ref like `t7`. The model sees the summary; `ContextTreeQuery` fetches an original by ref. `PROTECTED_TOOLS` never condense. Settings: `condense.enabled`, `condense.model`, `condense.min_batch_chars`.

`/context` shows how the window splits across system prompt, tools, messages and summaries (`agent/context.rs`).
