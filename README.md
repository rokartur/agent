# agent

A coding agent in one Rust binary. No Node. No Electron.

```sh
curl -fsSL https://agent.rokartur.com/install.sh | sh
```

<p>
<a href="https://github.com/rokartur/agent/releases/latest"><img alt="Release" src="https://img.shields.io/github/v/release/rokartur/agent.svg?style=for-the-badge&labelColor=000000&color=0a0a0a" height="28"></a>
<a href="https://github.com/rokartur/agent/releases"><img alt="Downloads" src="https://img.shields.io/github/downloads/rokartur/agent/total.svg?style=for-the-badge&labelColor=000000&color=0a0a0a" height="28"></a>
<a href="LICENSE"><img alt="License" src="https://img.shields.io/github/license/rokartur/agent.svg?style=for-the-badge&labelColor=000000&color=0a0a0a" height="28"></a>
<a href="https://github.com/rokartur/agent/actions/workflows/ci.yml"><img alt="CI" src="https://img.shields.io/github/actions/workflow/status/rokartur/agent/ci.yml.svg?style=for-the-badge&labelColor=000000&label=ci" height="28"></a>
</p>

## Quick start

```sh
cd your-project
agent
```

Type `/login` and pick Claude, ChatGPT or GitHub Copilot. You already pay for one; that's the setup. Prefer pay as you go? Paste an Anthropic or OpenRouter API key, and every OpenRouter model is in `/model`.

macOS and Linux. Updates itself. `agent --help` for the rest.

## Switch computers with /env

Run a session on another of your computers over Tailscale: its files, its shell, its accounts. No cloud relay, no pairing.

1. `agent service install` on the other computer, or `agent service install --on <computer_name>` from this one.
2. `/env <computer_name>` here. A new session starts there, in this terminal, and the session you leave keeps running.

`/env` alone lists your environments. `agent --on <computer_name>` starts a session there from the shell, and `agent --on auto` picks the least loaded computer. [Setup](docs/remote-sessions.md).

## Features

- **Plugins in any language**: a plugin is a folder with `plugin.json` and a program that speaks JSON-RPC over stdin. It can add tools for the model, `/` commands and its own settings. `agent plugin install ./my-plugin` ([docs](plugins/README.md)).
- **All your accounts, one agent**: add several Claude and ChatGPT accounts, Anthropic keys and OpenRouter keys. When one hits its limit, the agent moves to the next. `/usage` shows what is left.
- **One folder, nothing else**: settings, accounts, sessions, skills, plugins, MCP servers and prompts all live in `~/.agents`. A project can add its own `.agents` next to its code; it loads, and its edit hooks run, only after you `/trust` the project. Copy the folder and your whole setup moves with it.
- **Edits that cannot go stale**: the agent edits by line anchors. If the file changed since it last read it, the edit is refused.
- **It checks its own work**: after each edit, your language server reports new errors and the checks in `.agents/edit-hooks.json` run. The agent fixes what they find.
- **Fewer tokens through headroom**: turn on `headroom` in `/settings` and model requests go through a `headroom proxy`. The agent installs and syncs a local one, or set `headroom_url` and `headroom_token` to use your own server.
- **Rewind anything**: press Esc twice to go back to an earlier prompt. Your files go back with it.
- **Subagents**: it splits big work into parallel child agents. You tune how in a plain Markdown prompt, `.agents/swarm-prompt.md`.
- **MCP and skills**: connect MCP servers, OAuth included. Drop in skills and see which ones you never use with `/skill-usage`.
- **Web built in**: search through Exa, Brave, Tavily or Perplexity. Fetch pages as Markdown. Give it a GitHub URL and it clones the repo to read.
- **You choose how much it does alone**: ask before every change, let a reviewer model approve, or `--yolo`.
- **Sessions outlive the terminal**: close it mid-turn and the turn finishes anyway. `agent attach` picks the session up again in any terminal, and two terminals on one session see the same screen.

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
agent -p "Why is this test flaky?"       # print the answer and exit
agent --headless "Fix the lint"          # JSONL events
agent -p --tools Read,Grep "Map the API" # offer only these tools
agent acp                                # Agent Client Protocol for Zed and other editors
agent -p --trust "Fix the lint"          # load this project's .agents and run its edit hooks
```

## Other providers

Any OpenAI- or Anthropic-compatible server works, local ones included:

```json
"custom_providers": [{"name": "ollama"}, {"name": "groq"}]
```

Their models show in `/model` as `<name>/<model id>`. For an unknown server, set `base_url` and, if it needs a key, `api_key_env`. For an Anthropic Messages endpoint, add `"api": "anthropic"`.

<details>
<summary>Known names fill in the URL and the key variable</summary>

`ollama`, `lmstudio`, `groq`, `cerebras`, `mistral`, `xai`, `deepseek`, `together`, `fireworks`, `deepinfra`, `novita`, `perplexity`, `huggingface`, `nvidia`, `baseten`, `nebius`, `scaleway`, `stackit`, `chutes`, `cortecs`, `fpt`, `firmware`, `302ai`, `conifer`, `celeris`, `orcarouter`, `vercel-ai-gateway`, `opencode`, `opencode-go`, `moonshotai`, `moonshotai-cn`, `kimi-coding`, `zai`, `zai-coding-cn`, `minimax`, `minimax-cn`, `alibaba-coding-plan`, `qwen-token-plan`, `xiaomi`, `ant-ling`.

</details>

## Compared with Claude Code, jcode and pi

| | agent | Claude Code | jcode | pi |
|---|---|---|---|---|
| Version | 0.1.36 | 2.1.309 | 0.99.1 | 1.4.0 |
| C: finished in time, cases passed per run | **3 of 3**, 388 · 387 · 388 | 2 of 3, 388 · 388 · 388 | 0 of 3, 21 · 21 · 21 | 0 of 3, 387 · 388 · 387 |
| C: time, cost | **1208 s, $4.08** | 1955 s, $7.39 | 2400 s, $4.92 | 2400 s, $5.12 |
| D: time, tool calls, tokens, cost | **69.6 s**, 9.7, **260k, $0.35** | 94.9 s, 13.3, 645k, $0.58 | 197.3 s, 17.7, 954k, $0.89 | 70.7 s, **9.3**, 289k, $0.38 |
| E: time | 161.3 s | **128.6 s** | 224.0 s | 229.4 s |
| E: tool calls, tokens, cost | **7.0, 144k, $0.43** | 8.7, 348k, $0.56 | 12.0, 575k, $0.79 | 10.3, 202k, $0.49 |
| Reply to "OK", median of 15 | **1.10 s** | 3.08 s | 5.24 s | 1.44 s |
| Base context | 3.2k | 20.2k | 23.4k | **2.6k** |
| CPU, memory when idle | **0.04 s, 25 MB** | 0.82 s, 299 MB | 0.31 s, 45 MB | 0.49 s, 135 MB |

<details>
<summary>How it was measured</summary>

Same model (Claude Opus 5.5, medium thinking), same prompt, three large tasks, 3 runs each. On every run agent, jcode and pi start together, each with only its login. Claude Code ran the same tasks a day later on its own, on a MacBook with the same chip (M1 Max), also with only its login. C: write a jq interpreter in TypeScript, checked on 388 programs against jq 1.7.1, 40 minute limit. D: fix 11 reported bugs in marked, a 4,000 line Markdown renderer, where 1687 inputs must render byte for byte as the original. E: port Python's difflib to TypeScript, checked on 2916 cases from CPython.

In C, jcode spent all 40 minutes trying jq and wrote no code. pi had a working interpreter in all three runs, but two were still checking it when time ran out, and one waited 16 minutes on a command that never ended; agent stops a command after 2 minutes. Claude Code passed every case in every run; in C one of its runs was still checking its interpreter against jq when time ran out. In D one pi run and one jcode run missed one case. Run to run, times vary more than the gaps between agent and pi in D and E. The reply to "OK" varies from series to series too: in the previous one, on another Mac, agent took 2.78 s and pi 1.67 s.

</details>

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Report vulnerabilities as described in [SECURITY.md](SECURITY.md), not in a public issue.

## License

[GPL-3.0-or-later](LICENSE).
