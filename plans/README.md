# Implementation Plans

Written on 2026-11-12 against commit `52b32af`. Each plan is self-contained:
read it fully before starting, honor its STOP conditions, and update your row
when done.

## Execution order & status

| Plan | Title | Priority | Effort | Depends on | Status |
|------|-------|----------|--------|------------|--------|
| [001](001-permission-rules.md) | Settings hold allow, ask and deny rules per tool | P1 | M | none | TODO |
| [002](002-before-tool-hooks.md) | A before_tool hook can refuse a tool call | P2 | S | 001 | TODO |
| [003](003-project-and-local-settings.md) | A trusted project overrides settings from its `.agents` | P2 | M | none (step 5 needs 001) | TODO |
| [004](004-read-pdf.md) | Read returns the text of a PDF | P3 | S | none | TODO |

Status values: TODO | IN PROGRESS | DONE | BLOCKED (with one-line reason) | REJECTED (with one-line rationale)

## Dependency notes

- 002 requires 001: a hook's `match` is a rule in 001's syntax and uses its subjects and matcher.
- 003 is independent, but run it after 001 so its step 5 (a project may add `ask` and `deny` rules, never `allow`) applies. 001, 002 and 003 all add keys to `source/src/settings.rs`: run them one after another, not in parallel.
- 004 touches only `source/src/tools/` and can run at any time, in parallel with the others.

## Decisions the maintainer accepted (2026-11-12)

- 003: a project file cannot set `permissions.allow`, `permission_mode`, `on_start`, `lsp`, `before_tool` or providers. Loosening any of these makes `/trust` mean "this repository may run commands without asking".
- 003: `/settings` does not show that a project overrides a key.
- 004: CI does not install poppler, so the tests that run pdftotext run only on a developer machine.
- 004: text through pdftotext, not the Anthropic `document` block.

## Considered and rejected

- `after_tool`, `stop`, `prompt` and `session_start` shell hooks: covered by `.agents/edit-hooks.json` `onStop`, the plugin `input` request, `on_start` and the plugin events `tool_call`, `tool_result`, `prompt_end`, `session_start`.
- A PDF crate: `Cargo.toml` has eight dependencies; an optional external program follows what `Video` does with `ffmpeg`.
- Persisting "don't ask again" as a rule: the answer names one exact command, a rule needs a pattern the user chose.
