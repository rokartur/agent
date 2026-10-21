<!--
This file IS the swarm config. Swarms are complicated, dynamic systems, so
routing policy is passed to the models as a prompt rather than as options in
a standard config file. Edit freely: override globally at
~/.agents/swarm-prompt.md or per-project at ./.agents/swarm-prompt.md.
{implementation_effort} becomes subagents.implementation_effort from settings.
-->

When to spawn:

- Independent pieces of work that can run in parallel, one child per piece.
- Reconnaissance or bulk reading that would flood your own context; ask for the
  findings, not the raw material.
- A second opinion on a plan or diff, from a child that starts without your bias.
- Not for work you can finish in one or two tool calls. Spawning costs a full
  context and the child's answer still needs your review.

Writing the prompt. A child sees none of this conversation, so its prompt must
stand alone:

- The goal, and why it matters, in one or two sentences.
- What you already know: relevant paths, decisions made, dead ends ruled out.
- The scope: what to touch and what to leave alone.
- The answer you want back: its shape, its length, and what counts as done.

A child's answer is a claim. Check what your next step depends on (open the
cited `path:line`, rerun the decisive command) before you build on it.

Model routing guidance for spawned agents. Pass `model` to choose a model for
a new child. When omitted, or with `model: "inherit"`, the child uses your model.
Pass `effort` when spawning:

- Implementation tasks: `effort: "{implementation_effort}"`.
- Design, investigation, debugging, review, and verification: default effort.
- Context fetching / bulk reading / summarization: `effort: "off"`.

Structure guidance for spawned agents:

- Always pass `description` when spawning (e.g. `description: "api reviewer"`) so
  /tasks shows what each agent is for. Spawns with a missing or blank
  description are rejected.
- Only the root session may spawn agents. Children must complete their assigned
  task directly and report back rather than creating another generation.
