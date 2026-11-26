# Sessions

A session is one JSONL file, `~/.agents/sessions/<id>.jsonl`, appended to as the conversation goes. Nothing is rewritten: rewind, compaction and resets are entries too, and `Session::history` (`agent/session.rs`) replays the file top to bottom into a `History`.

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
`condense` | refs and summary from sessions before old tool results were cleared | ignored |
`goal`, `plan`, `todo` | the tools of the same name | restored state |
`extra_dirs` | `/workspace` | restored list |

Indexes in `rewind`, `tool_checkpoint` and `compaction` are message indexes. Two processes writing one file would interleave, so a second process opening a session copies it instead (`Session.forked_from`); the open file holds a lock for that.

## Resume, continue, clone, import

`-c` opens the newest session whose `cwd` is the current directory; `-r [id]` any session by id prefix or from a list. `/clone` copies the file to a new id at the current position, with the session's pages. `/import file.jsonl` reads any journal, Claude Code exports included where the block shape matches. `/export` writes `.jsonl` or `.md`.

`prune_sessions` in settings deletes files untouched this long at start, with the session's folder of pages and workflow runs; an open session holds its lock and stays.

## Rewind

Esc Esc on an idle session lists `RewindPoint`s: every user prompt with its index. Picking one truncates the conversation and, inside a git repo, restores the work tree from the checkpoint snapshots (`agent/snapshots.rs`): a private repo under `~/.agents/snapshots` holds a tree per tool call that edited files. Ignored files are not stored. Files changed since the agent last wrote them are left alone and listed (`RestoredFiles`).

## Compaction and clearing old tool results

Two mechanisms shrink the provider's view, both keep the originals in `messages`:

- **Compaction** (`Agent::compact`): one summary replaces everything before `cut`. Runs on `/compact [note]`, `/handoff`, and automatically when context tokens pass `context_window - reserve_tokens`. The summary prompt is `SUMMARY_PROMPT` in `agent/compaction.rs`; `keep_recent_tokens` stays verbatim.
- **Clearing** (`agent/condense.rs`, setting `condense.enabled`): the results of the oldest tool calls up to a cut are cleared; the model runs a call again when it needs one. The cut (`Agent::cleared_through`) moves to all but the newest 10 clearable calls only when the prompt cache is cold (`cache_ttl` since the last reply): any move rewrites the cache from the system prompt on, and on a warm cache that cost more than it saved, near compaction too. On Anthropic the request carries a `clear_tool_uses_20250919` edit whose `keep` grows with every call, so the server clears the same results each turn; a server-side edit keeps the later thinking valid, where a client rewrite would drop it. Other providers get the same edit applied in `api::request::clear_tool_uses`. `PROTECTED_TOOLS` are never cleared. Research and measurements: `docs/condense-upstream.md` at the repo root.

`/context` shows how the window splits across system prompt, tools, messages and summaries (`agent/context.rs`).
