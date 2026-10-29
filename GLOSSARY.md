# agent

A coding agent in one Rust binary: a terminal UI that runs model sessions against your own accounts, with tools, plugins, skills and subagents.

## Sessions

**Session**:
One conversation with the model in one project directory, with its full history, saved under `~/.agents/sessions`.
_Avoid_: Thread, chat, conversation

**Turn**:
One message to the model and everything the agent does until it waits for the next one.
_Avoid_: Run, step

**Compaction**:
Replacing the older part of a session's history with a summary, keeping recent messages verbatim.
_Avoid_: Summarization, condensing

**Condensing**:
Replacing batches of old tool results with short summaries that cite the calls they cover, while the rest of the history stays.
_Avoid_: Compaction, pruning

**Rewind**:
Going back to an earlier prompt in a session, with the files the agent changed restored to that point.
_Avoid_: Undo, revert

**Goal**:
A condition the session keeps working toward across turns until it is met.
_Avoid_: Task, objective

## Models and accounts

**Provider**:
A service that serves models: Anthropic, OpenAI, GitHub Copilot, OpenRouter, or a custom provider.
_Avoid_: Vendor, backend

**Custom provider**:
An OpenAI- or Anthropic-compatible server named in `custom_providers`.
_Avoid_: Endpoint, local provider

**Account**:
One set of credentials for a provider: a subscription login or an API key. A provider can have several accounts; the agent moves to the next when one hits its limit.
_Avoid_: Key, login, profile

**Catalog**:
Every model the configured accounts can use, as listed in `/model`.
_Avoid_: Model list, registry

## Control

**Permission mode**:
How much the agent does without asking: `ask`, `auto` (a reviewer model approves or denies) or `full access`.
_Avoid_: Approval mode, autonomy level

**Reviewer**:
The model that approves or denies tool calls in `auto` permission mode.
_Avoid_: Judge, approver

**Trusted project**:
A project whose own `.agents` folder loads, and whose edit hooks run, because the user ran `/trust` in it.
_Avoid_: Allowed project, safe project

**Edit hook**:
A check from `.agents/edit-hooks.json` that runs after each file edit and reports problems back to the model.
_Avoid_: Post-edit check, linter hook

## Extensions

**Plugin**:
A folder with `plugin.json` and a program that speaks JSON-RPC over stdin, adding tools, `/` commands and settings.
_Avoid_: Extension, add-on

**Skill**:
A folder with a `SKILL.md` of instructions the model loads when a task matches its description.
_Avoid_: Prompt, recipe

**Profile**:
A named set of skills, by name or `owner/repo` source, in `skill_profiles`; its skills load only while `/profile:<name>` has it on, even ones in `disabled_skills`. Skills no profile holds load unless disabled.
_Avoid_: Mode, skill set

**Subagent**:
A child agent process a session starts for a piece of parallel work; it reports its answer back to the parent.
_Avoid_: Worker, child session

**Workflow**:
A JavaScript script that orchestrates many subagents in the background and returns one result.
_Avoid_: Pipeline, swarm

**Peer**:
Another interactive session on this or another of the user's computers, reachable with `SendMessage`; one on another computer is addressed `<environment>:<session>`.
_Avoid_: Sibling, neighbour

## Remote sessions

**Session server**:
The `agent serve` process that runs one interactive session, its TUI included; terminals attach to it as clients.
_Avoid_: Daemon, backend

**Client**:
A terminal attached to a session server: it sends its keys, mouse and size, and shows the frames the server draws.
_Avoid_: Frontend, viewer

**Attach**:
Connecting a client to a running or saved session; several clients can attach to one session at once. Closing the client detaches it and leaves the session running.
_Avoid_: Join, connect, resume

**Host**:
The computer a session runs on: its files, shell, settings and accounts are the ones the session uses.
_Avoid_: Server, remote, target

**Environment**:
A host as the TUI offers it when starting a session: `local`, a computer named in settings, or one of the user's computers found online on the tailnet, by its Tailscale name.
_Avoid_: Machine, remote, target

**Listener**:
`agent serve --tailnet`: the one process on a host that accepts its owner's other computers over Tailscale and hands each connection to a session server there.
_Avoid_: Daemon, gateway, relay
