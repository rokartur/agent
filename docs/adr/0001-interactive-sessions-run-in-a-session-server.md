---
status: proposed
---

# Interactive sessions run in a session server, the TUI is a client

Every interactive session runs in `agent serve`, local ones included; the TUI attaches to it over a Unix socket, or over the tailnet for another computer. We chose this over keeping local sessions in the TUI process with a server only for remote ones, so there is one code path, sessions survive closing the terminal, and any session can be attached from another computer. `-p`, `--json`, `--headless` and `agent acp` stay in-process, since nothing attaches to them. Client and server speak the ACP JSON-RPC that `agent acp` already implements, extended with the methods attach and the TUI's project reads need, instead of a protocol of our own. A session belongs to its host: it uses the host's settings, accounts, MCP servers, plugins and skills, and the client's settings only shape the TUI. See #60 and #61.
