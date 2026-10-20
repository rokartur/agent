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

**Rewind anything.** Press Esc twice to go back to an earlier prompt. Your files go back with it.

**Subagents.** It splits big work into parallel child agents. You tune how in a plain Markdown prompt, `.agents/swarm-prompt.md`.

**MCP and skills.** Connect MCP servers, OAuth included. Drop in skills and see which ones you never use with `/skill-usage`.

**Web built in.** Search through Exa, Brave, Tavily or Perplexity. Fetch pages as Markdown. Give it a GitHub URL and it clones the repo to read.

**You choose how much it does alone.** Ask before every change, let a reviewer model approve, or `--yolo`.

## Compared with jcode and pi

Same model (Claude Opus 5.5, medium thinking), same prompt, two coding tasks, 6 runs each, averages. Every run of every harness passed all hidden tests. A: fix a CSV parser and write a CLI (39 tests). B: port semver range matching with no dependencies (2550 cases).

| | agent | jcode | pi |
|---|---|---|---|
| Version | 0.1.0 | 0.98.1 | 1.3.0 |
| A: time, tool calls, tokens, cost | **35.0 s, 3.2, 85k, $0.11** | 40.5 s, 3.8, 122k, $0.14 | 47.4 s, 4.3, 113k, $0.17 |
| B: time | **187.3 s** (143–279) | 244.5 s (165–344) | 224.4 s (135–365) |
| B: tool calls | **9.5**, median 6 | 14.7, median 14 | 13.0, median 9.5 |
| B: tokens, cost | **386k, $0.61** | 619k, $0.84 | 536k, $0.68 |
| Reply to "OK", median of 15 | **2.30 s** | 3.09 s | 4.20 s |
| Base context | **17.8k** | 24.2k | 19.4k |
| CPU, memory when idle | **0.07 s, 27 MB** | 0.2 s, 48 MB | 2 s, 310 MB |

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
agent -p --tools read,rg "Map the API" # offer only these tools
agent acp                              # Agent Client Protocol for Zed and other editors
agent -p --trust "Fix the lint"        # load this project's .agents and run its edit hooks
```

macOS and Linux. Updates itself. `agent --help` for the rest.

## License

[GPL-3.0-or-later](LICENSE). See [CONTRIBUTING.md](CONTRIBUTING.md) to help and [SECURITY.md](SECURITY.md) to report a vulnerability.
