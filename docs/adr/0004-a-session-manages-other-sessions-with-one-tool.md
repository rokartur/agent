---
status: accepted
---

# A session manages other sessions with one tool and the message path, not a mode

A session becomes an orchestrator by calling `StartSession`, which starts a managed session: a whole session server (ADR 0001) with no terminal, here or on another computer (ADR 0002). The managed session is started with `--started-by <address>` and sends each answer, and each wait for the user's approval or answer, to that address over the path `SendMessage` already uses. `SendMessage`, `ListAgents` and `TaskStop` cover the rest, so there is no orchestrating mode, no setting and no new panel. The cost: a report is a message, so an orchestrator that has exited or sleeps misses it and has to read `ListAgents` when it is back.

The rules the model cannot change:

- Permission mode: `auto`, or `full access` only when the orchestrator has it. The orchestrator never approves a tool call or answers a question for the user; it is told the managed session waits and passes that on.
- Directory: inside a git project whose root is not the home directory, checked on the computer that runs it. A session in `auto` edits its directory unreviewed, so the model cannot point it at `~` or at a folder of projects.
- One generation: a managed session is not offered `StartSession`, and a call to it is refused.
- At most 8 managed sessions per orchestrator on one computer. The limit is per computer because a count across computers would ask every listener before each start.
- `TaskStop` stops only a session whose card names the caller as its orchestrator. Over the tailnet the host takes the computer in that name from the peer `tailscale whois` reported; the session id comes from the request.
- The orchestrator's address is the session id its process began with, so `/clear` and `/handoff`, which give the process a new id, keep its managed sessions while that process runs. An orchestrator reopened under another id is a new one: the sessions keep running and show in `ListAgents`, and the user stops them from `agent attach`.
- A managed session outlives its orchestrator, and stopping it keeps its saved history, worktree and branch.

## Considered options

- A subscription on the managed session's socket, like `notify_when_idle`: it dies with the orchestrator's process and does not cross computers.
- An orchestrating mode or setting: it would only gate tools that are safe to offer always.
- Letting the orchestrator approve permissions: in `auto` the reviewer model already decides, and the calls it passes to the user are the ones no model should approve.
- Pushing worktree branches to `origin` to move work between computers: visible to everyone with the repository. `git fetch <computer>:<path> worktree-<name>:worktree-<name>` over ssh moves the branch between the user's own computers only.
- Refusing `StartSession` in `ask`, the default mode: the feature would be off for most users. The approval prompt says the session runs in `auto`, and "always allow" covers that exact call only.
- A notice to the orchestrator when an idle managed session unloads: it would wake the orchestrator for a turn 30 minutes after each session finishes. A `SendMessage` that no longer reaches it says so, and a new session in its directory continues the work.
- Passing the orchestrator's account: `/account` pins are saved settings, so a managed session reads the same ones and switches accounts on its own limits, as any session does.
- Removing the worktree on stop: it would delete work the orchestrator has not merged.
- Keeping `started_by` in the session file: a managed session the user reopens after it stopped or unloaded is the user's own session, so it reports to nobody and may start sessions.
