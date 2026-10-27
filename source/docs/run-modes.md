# Run modes

`main.rs` parses the flags into a `Setup` and picks one of five loops.

| Mode | Flag | Output | Tools needing the user |
|---|---|---|---|
| interactive | none | TUI in a session server, `tui::run` | ask in any attached terminal |
| print | `-p` | the answer on stdout | fail: `Agent.interactive` is false |
| json | `--json` | the answer, then one JSON summary | fail |
| headless | `--headless` | JSONL: `tool`, `session`, `usage`, `done`, `error` | fail |
| acp | `agent acp` | JSON-RPC 2.0 lines, `acp.rs` | forwarded to the editor |

`-c` and `-r` resume in every mode, and in the interactive one attach to the session when its server still runs; `--no-session` skips the journal; `--tools` narrows the tool list; `--account` pins an account; `--trust` trusts the project for the run.

## Session servers

An interactive `agent` starts `agent serve` with the same flags (`serve.rs`) and attaches to it. The server runs the session and the whole TUI, and listens on `~/.agents/serve/<pid>.sock`, mode `0600` in a private directory. Each terminal sends a hello with its size and terminal variables, then one JSON line per key, mouse, paste, resize or focus event; the server answers with `{"server": pid}`, then `{"output": ...}` frames, a `{"ping": true}` after 5 s without one and, at the end, `{"exit": ...}` with the text the terminal prints after leaving the screen (`tui/wire.rs`, `tui/server.rs`, `tui/client.rs`). What happens on the user's computer, the terminal does, since in a remote session that is not the server's: the server asks it to `{"copy": text}`, `{"open": url}` or `{"paste_image": true}` (Ctrl+V), and a pasted path of an image file arrives with the file's bytes, read where it was dropped. The server rebuilds each image block from the bytes before it reaches the model. Every attached terminal gets the same frame, sized to the smallest of them, and shares one editor; a permission prompt answered in one closes in all.

Closing a terminal detaches it, and so does `/detach`; quitting (`/quit`, Ctrl+C twice, Ctrl+D) ends the session for every terminal. `agent attach [id]` lists the running sessions from their cross-session cards and attaches to one; with an id no server holds it opens the saved session like `agent -r <id>`. A server with no terminal, no turn and no background work exits after `unload_idle_sessions_after_minutes` (default 30, 0 never). Its stdout and stderr go to `~/.agents/serve/<pid>.log`, removed on a clean exit and kept for a week after a crash.

## Remote environments

`agent serve --tailnet` (`tailnet.rs`) runs only with `serve_tailnet` on. It binds the addresses `tailscale status` gives this computer, on `tailnet_port`, and for each connection runs `tailscale whois` on the peer and admits only an untagged device of this computer's own user. The client runs the same check on the host before it sends anything. A connection sends one request line, `{"list": true}`, `{"attach": id}`, `{"start": {"cwd", "args"}}` or `{"reattach": pid}`, with its `protocol` and `version`. Every reply carries the listener's own, so on a mismatch the client names the side to update. After `{"ok": true}` the listener relays bytes between the TCP connection and the session's Unix socket, and the session wire runs through unchanged. A start runs the same find-or-start as a local `agent` (`local_session` in `main.rs`) in the requested directory, or in the host's home when that does not exist; then the session starts with `--missing-cwd <dir>` and opens with a notice that names it. Both ends turn on TCP keepalive, so a peer that sleeps without closing detaches in about a minute. A client that hears nothing for 15 s, or whose connection closes without an `exit`, keeps the screen and sends `{"reattach": pid}` with the pid from `{"server": pid}` until the host answers (`reconnect` in `serve.rs`). The new attach gets a full frame, so the terminal shows what it missed; keys typed while it reconnects are dropped, since the screen they answer is stale.

`--on`, `default_environment`, `/env` and `agent attach` are the client side (`serve.rs`). The hello names the terminal's environment and its `environments`; `/env <name>` answers `{"switch": name}`, and the client starts a new session there. A session that never got a message and has no other terminal ends at that point. `agent attach` asks every environment in parallel and saves each answer to `~/.agents/environments.json`, so an environment that does not answer still lists its last sessions, greyed out and not selectable.

`agent service` (`service.rs`) writes a launchd agent (macOS) or a systemd user unit with lingering (Linux) that runs `agent serve --tailnet` with the installing shell's `PATH` and starts it again when it exits. Session servers run in their own process group (`process::command`), and the unit sets `KillMode=process`, so a restart ends only the listener. An update calls `service::restart_if_installed`, so the listener runs the release its clients do.

## Subagents

`Agent` and `Workflow` run child processes of the same binary with `--parent <id> --headless`, the task as JSON string lines on stdin (`agent/subagent.rs`). A child has no `Agent`, `Task*`, web, `AskUserQuestion`, plan or worktree tools; `--explore` leaves only read and `rg`, `--web` keeps the web tools. The swarm prompt (`.agents/swarm-prompt.md` up to the git root, then `~/.agents/swarm-prompt.md`, then the built-in) goes ahead of every task. A child's session file carries a `parent` entry, so `/resume` hides it and `/tasks` opens it.

A background run's answer queues as a `FollowUp` for the next turn; `WaitForAgents` withdraws the ones it waited for. `SendMessage` to a finished run restarts it on its own session file.

## Workflows

`Workflow` runs a JavaScript script in an embedded QuickJS sandbox (`agent/workflow.rs`): no filesystem, shell or network, only `agent`, `parallel`, `pipeline`, `phase`, `log`, `args`. Each `agent()` call spawns a subagent as above. Runs are listed in `/workflows`, saved scripts in `~/.agents/workflows/`, and a saved one can be run by name through `/<name>`.

## Cross-session messaging

Each session server listens on `~/.agents/peers/<pid>.sock` and describes itself in `<pid>.json` (`agent/peers.rs`). `ListAgents` reads the json files; `SendMessage` to a session writes one JSON line to its socket, which arrives as a message or, with `notify_when_idle`, as a notice when that session goes idle.

## TUI

`tui/mod.rs`: `App` holds the transcript, the editor, the open `Panel` and the settings. The event loop takes `Msg` from one channel (attached terminals and their input events, agent events, worker results, plugin and MCP changes) and sends `Command`s to the worker. Slash commands are the `COMMANDS` table: each is `MidTurn::Runs` (works while the agent works) or `MidTurn::Waits` (queued until the turn ends). `/hotkeys` prints the key table from the same source the handler uses.

Trying a change without touching your own setup: `cargo build && tui run t "env HOME=/tmp/agent-home $PWD/target/debug/agent"`.
