---
status: accepted
---

# Interactive sessions run in a session server, the terminal only shows them

Every interactive session runs in its own `agent serve` process, local ones included, and a terminal attaches to it over a Unix socket, `~/.agents/serve/<pid>.sock`. The server runs the whole TUI: it draws one frame, sized to the smallest attached terminal, and sends it to every client; a client only forwards its key, mouse, paste, resize and focus events and writes the frames it gets, like tmux. The wire format is our own newline-delimited JSON (`tui/wire.rs`), not ACP: the TUI needs no protocol method per panel, and a remote session (#62) shows the host's files, shell and git with no extra methods, because the TUI already runs there.

We chose this over a TUI that speaks ACP to the server, which the first draft of this record proposed. That design needed a protocol method and a synced copy of state for every panel (`/mcp`, `/plugins`, `/account`, `/settings`, `/tasks`, `/workflows`, rewind, worktrees) and more for the project reads of #63, a rewrite of most of `tui/mod.rs`. The cost of the choice: terminals attached to one session share one editor, one scroll position and one open panel, and the TUI follows the host's settings, not the client's.

One server per session, not one per user: each session keeps the process-wide state it always had (working directory, environment, the per-pid spill, web and peer files) and the session ends with its process. `agent` starts the server through `sh` so it is no child of the terminal's process and outlives it; `agent attach` and `agent -r` find a running one by its cross-session card in `~/.agents/peers`. `-p`, `--json`, `--headless` and `agent acp` stay in-process, since nothing attaches to them. See #60 and #61.
