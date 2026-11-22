---
status: accepted
---

# Replies draw UI blocks in the terminal from a JSON fence

The model draws in the terminal by writing a ```ui fenced block of JSON in its reply: stacked or side-by-side nodes of Markdown text, stats, progress bars and stacked charts, each with an optional title. `markdown::render` draws the block where it would show the code, so the drawing is part of the reply text: it costs no tool call or extra turn, it is in the session file and redraws at every terminal width, and every attached terminal shows it. The format is taught in the system prompt only of interactive sessions, since `-p`, ACP and subagents have no terminal to draw in and would pass the raw JSON on. JSON that does not parse shows as code, which is also how a block looks while it streams in; a node the renderer does not know shows as code with the reason under it.

This narrows ADR 0003: native widgets were rejected there as the way to show mocks and documents, which stay `Page`s. A UI block is for a glance inside the conversation, where opening a browser costs more than the drawing is worth.

## Considered options

- A `Ui` tool whose input the TUI draws, as Claude Code's chart MCP servers do: the schema would sit in the tool list and the JSON would arrive whole, but every drawing would end a model request and start another.
- Separate fences per widget (`chart`, `stat`, `progress`): less to teach, but no layout, so a dashboard needs one block per number.
- A frame around each titled node: the mocks compared rounded boxes, rules like a code block, and bare bold titles; the bare titles were picked, they keep side-by-side columns narrow.
