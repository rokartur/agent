# agent

[![Release](https://img.shields.io/github/v/release/rokartur/agent)](https://github.com/rokartur/agent/releases/latest)
[![Downloads](https://img.shields.io/github/downloads/rokartur/agent/total)](https://github.com/rokartur/agent/releases)
[![License](https://img.shields.io/github/license/rokartur/agent)](LICENSE)
[![CI](https://github.com/rokartur/agent/actions/workflows/ci.yml/badge.svg)](https://github.com/rokartur/agent/actions/workflows/ci.yml)

simple af possible.

```sh
curl -fsSL https://agent.rokartur.com/install.sh | sh
```

A coding agent in one Rust binary. No Node. No Electron.

You already pay for Claude, ChatGPT or GitHub Copilot. Log in. That's the setup. Prefer pay as you go? Paste an Anthropic or OpenRouter API key, and every OpenRouter model is in `/model`. Any OpenAI- or Anthropic-compatible server works too, local ones included:

```json
"custom_providers": [{"name": "ollama"}, {"name": "groq"}]
```

Known names fill in the URL and the key variable: `ollama`, `lmstudio`, `groq`, `cerebras`, `mistral`, `xai`, `deepseek`, `together`, `fireworks`, `deepinfra`, `novita`, `perplexity`, `huggingface`, `nvidia`, `baseten`, `nebius`, `scaleway`, `stackit`, `chutes`, `cortecs`, `fpt`, `firmware`, `302ai`, `conifer`, `celeris`, `orcarouter`, `vercel-ai-gateway`, `opencode`, `opencode-go`, `moonshotai`, `moonshotai-cn`, `kimi-coding`, `zai`, `zai-coding-cn`, `minimax`, `minimax-cn`, `alibaba-coding-plan`, `qwen-token-plan`, `xiaomi`, `ant-ling`. For another server, set `base_url` and, if it needs a key, `api_key_env`. For an Anthropic Messages endpoint, add `"api": "anthropic"`. Their models show in `/model` as `<name>/<model id>`.

## Features

**Plugins in any language.** A plugin is a folder with `plugin.json` and a program that speaks JSON-RPC over stdin. It can add tools for the model, `/` commands and its own settings. `agent plugin install ./my-plugin`. [Docs](plugins/README.md)

**All your accounts, one agent.** Add several Claude and ChatGPT accounts, Anthropic keys and OpenRouter keys. When one hits its limit, the agent moves to the next. `/usage` shows what is left.

**One folder, nothing else.** Settings, accounts, sessions, skills, plugins, MCP servers and prompts all live in `~/.agents`. A project can add its own `.agents` next to its code. It loads, and its edit hooks run, only after you `/trust` the project. No hidden config anywhere else. Copy the folder and your whole setup moves with it.

**Edits that cannot go stale.** The agent edits by line anchors. If the file changed since it last read it, the edit is refused. No silent overwrites.

**It checks its own work.** After each edit, your language server reports new errors and the checks in `.agents/edit-hooks.json` run. The agent fixes what they find.

**Fewer tokens through headroom.** Turn on `headroom` in `/settings` and model requests go through a `headroom proxy`. The agent installs and syncs a local one, or set `headroom_url` and `headroom_token` to use your own server.

**Rewind anything.** Press Esc twice to go back to an earlier prompt. Your files go back with it.

**Subagents.** It splits big work into parallel child agents. You tune how in a plain Markdown prompt, `.agents/swarm-prompt.md`.

**MCP and skills.** Connect MCP servers, OAuth included. Drop in skills and see which ones you never use with `/skill-usage`.

**Web built in.** Search through Exa, Brave, Tavily or Perplexity. Fetch pages as Markdown. Give it a GitHub URL and it clones the repo to read.

**You choose how much it does alone.** Ask before every change, let a reviewer model approve, or `--yolo`.

**Sessions outlive the terminal.** Close it mid-turn and the turn finishes anyway. `agent attach` picks the session up again in any terminal, and two terminals on one session see the same screen.

**Sessions on your other computers.** Over Tailscale, `agent --on studio` runs the session on another of your computers: its files, shell and accounts. No cloud relay, no pairing. [Setup](docs/remote-sessions.md)

## Compared with jcode and pi

Same model (Claude Opus 5.5, medium thinking), same prompt, three large tasks, 3 runs each. On every run the three harnesses start together, each with only its login. C: write a jq interpreter in TypeScript, checked on 388 programs against jq 1.7.1, 40 minute limit. D: fix 11 reported bugs in marked, a 4,000 line Markdown renderer, where 1687 inputs must render byte for byte as the original. E: port Python's difflib to TypeScript, checked on 2916 cases from CPython.

| | agent | jcode | pi |
|---|---|---|---|
| Version | 0.1.35 | 0.99.1 | 1.4.0 |
| C: finished in time, cases passed per run | **3 of 3**, 388 · 388 · 387 | 0 of 3, 21 · 21 · 21 | 2 of 3, 388 · 386 · 21 |
| C: time, cost | **1870 s**, $6.50 | 2400 s, $4.94 | 1981 s, **$3.60** |
| D: time, tool calls, tokens, cost | 80.1 s, **10.7**, 340k, $0.41 | 142.5 s, 12.0, 715k, $0.83 | **78.5 s**, 11.7, **319k, $0.39** |
| E: time | 152.1 s | 202.4 s | **132.0 s** |
| E: tool calls, tokens, cost | **7.3, 147k, $0.45** | 11.0, 566k, $0.81 | 8.0, 179k, $0.49 |
| Reply to "OK", median of 15 | 2.78 s | 5.30 s | **1.67 s** |
| Base context | **2.3k** | 23.4k | 2.6k |
| CPU, memory when idle | **0.05 s, 27 MB** | 0.29 s, 47 MB | 0.49 s, 135 MB |

In C, jcode spent all 40 minutes trying jq and wrote no code, and one pi run waited 35 minutes on a jq command that never ended; agent stops a command after 2 minutes. Agent costs more in C because it checks its interpreter against the real jq on hundreds of its own programs, which is also why all three of its runs finished. In D one pi run and one jcode run missed one case. Run to run, times vary more than the gaps between agent and pi in D and E. The reply to "OK" varies too: in a second series of 15, agent took 3.42 s, pi 2.93 s and jcode 5.17 s.

## Commands worth knowing

```
/goal       keep going until it is done
/btw        ask aside, keep the session clean
/worktree   move the session to a git worktree, or remove one
/changes    every file it touched, with diffs
/voice      talk instead of type (whisper.cpp)
/caveman    answers without the fluff
/ponytail   writes less code
/fast       standard, fast, ultrafast: quicker output, higher price
```

## Scripts and CI

```sh
agent -p "Why is this test flaky?"     # print the answer and exit
agent --headless "Fix the lint"        # JSONL events
agent -p --tools Read,Grep "Map the API" # offer only these tools
agent acp                              # Agent Client Protocol for Zed and other editors
agent -p --trust "Fix the lint"        # load this project's .agents and run its edit hooks
```

macOS and Linux. Updates itself. `agent --help` for the rest.

## License

[GPL-3.0-or-later](LICENSE). See [CONTRIBUTING.md](CONTRIBUTING.md) to help and [SECURITY.md](SECURITY.md) to report a vulnerability.
