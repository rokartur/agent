# Run modes

`main.rs` parses the flags into a `Setup` and picks one of five loops.

| Mode | Flag | Output | Tools needing the user |
|---|---|---|---|
| interactive | none | TUI, `tui::run` | ask in the terminal |
| print | `-p` | the answer on stdout | fail: `Agent.interactive` is false |
| json | `--json` | the answer, then one JSON summary | fail |
| headless | `--headless` | JSONL: `tool`, `session`, `usage`, `done`, `error` | fail |
| acp | `agent acp` | JSON-RPC 2.0 lines, `acp.rs` | forwarded to the editor |

`-c` and `-r` resume in every mode; `--no-session` skips the journal; `--tools` narrows the tool list; `--account` pins an account; `--trust` trusts the project for the run.

## Subagents

`Agent` and `Workflow` run child processes of the same binary with `--parent <id> --headless`, the task as JSON string lines on stdin (`agent/subagent.rs`). A child has no `Agent`, `Task*`, web, `AskUserQuestion`, plan or worktree tools; `--explore` leaves only read and `rg`, `--web` keeps the web tools. The swarm prompt (`.agents/swarm-prompt.md` up to the git root, then `~/.agents/swarm-prompt.md`, then the built-in) goes ahead of every task. A child's session file carries a `parent` entry, so `/resume` hides it and `/tasks` opens it.

A background run's answer queues as a `FollowUp` for the next turn; `WaitForAgents` withdraws the ones it waited for. `SendMessage` to a finished run restarts it on its own session file.

## Workflows

`Workflow` runs a JavaScript script in an embedded QuickJS sandbox (`agent/workflow.rs`): no filesystem, shell or network, only `agent`, `parallel`, `pipeline`, `phase`, `log`, `args`. Each `agent()` call spawns a subagent as above. Runs are listed in `/workflows`, saved scripts in `~/.agents/workflows/`, and a saved one can be run by name through `/<name>`.

## Cross-session messaging

Each interactive session listens on `~/.agents/peers/<pid>.sock` and describes itself in `<pid>.json` (`agent/peers.rs`). `ListAgents` reads the json files; `SendMessage` to a session writes one JSON line to its socket, which arrives as a message or, with `notify_when_idle`, as a notice when that session goes idle.

## TUI

`tui/mod.rs`: `App` holds the transcript, the editor, the open `Panel` and the settings. The event loop takes `Msg` from one channel (input events, agent events, worker results, plugin and MCP changes) and sends `Command`s to the worker. Slash commands are the `COMMANDS` table: each is `MidTurn::Runs` (works while the agent works) or `MidTurn::Waits` (queued until the turn ends). `/hotkeys` prints the key table from the same source the handler uses.

Trying a change without touching your own setup: `cargo build && tui run t "env HOME=/tmp/agent-home $PWD/target/debug/agent"`.
