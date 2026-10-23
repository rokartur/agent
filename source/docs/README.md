# Source docs

How `agent` is built. User-facing material lives one level up: [`../../README.md`](../../README.md) for features, `agent --help` for flags, `/help` and `/hotkeys` inside a session for commands and keys, [`../../plugins/README.md`](../../plugins/README.md) for the plugin protocol.

| Doc | Read it when |
|---|---|
| [architecture.md](architecture.md) | You need the module map, the threads, or what happens to a prompt from Enter to the answer. |
| [sessions.md](sessions.md) | You touch the session journal, resume, rewind, compaction, condense or file snapshots. |
| [tools.md](tools.md) | You add or change a tool, its permission class, or the line anchors edits rely on. |
| [providers.md](providers.md) | You touch accounts, model lists, failover between accounts, custom providers or headroom. |
| [configuration.md](configuration.md) | You touch settings, the system prompt, `.agents` files, skills, prompts, edit hooks or trust. |
| [run-modes.md](run-modes.md) | You touch the TUI, `-p`, `--json`, `--headless`, subagents (`--parent`) or `acp`. |

Every doc names the file that owns each fact. The code's `///` comments carry the gotchas; these docs carry the shape.
