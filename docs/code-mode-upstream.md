# Code Mode in Claude Code, compared with the agent's Code mode

Question: what is known about Claude Code's Code Mode, and what separates the agent's `code_mode` from a feature that is no longer experimental?

Answer: Claude Code shipped Code Mode on 2026-11-12 (v2.1.323) and still calls it experimental, off by default, and removable in any release. The agent's design already matched it on the permission model and is stricter or wider in six places. Six gaps separated it from daily use, the largest found only by measuring: on Anthropic the model could not read the tool's description, so it never wrote a script. Branch `code-mode-real` closes four in code and measures the fifth; branch `feat/code-background-scripts` closes the sixth. The numbers keep the label "experimental": see [Gaps](#gaps) and [Measurement](#measurement).

Checked 2026-11-17. Anthropic's pages were fetched directly. X was read through a logged-in browser search; no post by Anthropic staff came up, so every X item below is a third party's claim.

## Sources

- Docs page: https://code.claude.com/docs/en/code-mode.md
- Changelog: https://raw.githubusercontent.com/anthropics/claude-code/main/CHANGELOG.md, entries 2.1.323 (2026-11-12), 2.1.325 (2026-11-13), 2.1.326 (2026-11-16); dates from `gh release view`
- Environment variables: https://code.claude.com/docs/en/env-vars.md
- Permission modes: https://code.claude.com/docs/en/permission-modes.md
- Issues in `anthropics/claude-code`: #109679, #109847, #110180, #110367
- Programmatic Tool Calling, the API feature the idea comes from: https://www.anthropic.com/engineering/advanced-tool-use
- Code execution with MCP: https://www.anthropic.com/engineering/code-execution-with-mcp, which credits the name "Code Mode" to https://blog.cloudflare.com/code-mode/

## What Claude Code ships

From the docs page unless another source is named.

- Claude writes JavaScript that makes the tool calls; Claude Code runs it in the session. The tool is named `REPL`, and a `PreToolUse` hook matching `REPL` reads the script from `tool_input.code`.
- Script shape, from the one example: `` $`cmd` `` with `.text()` and `.nothrow()`, an `edit(path, old, new)` helper, and the last expression as the result. The example returns an unawaited `$` call inside an object, so the result's promises are presumably resolved; the page does not say so.
- Stated uses: a change repeated across many files, a search whose results need filtering before Claude reads them, several independent commands that run at the same time.
- Turned on with `CLAUDE_CODE_EXPERIMENTAL_CODE_MODE=1` in the shell or user settings, then a confirmation before the first session. A project's settings file cannot turn it on. `0` in managed settings blocks it for an organization.
- Permissions: each call goes through `deny` and `ask` rules and `PreToolUse` hooks under the tool's own name. In auto mode the classifier reviews the script once, not each call, and "doesn't see values computed at run time". In other modes the script starts unreviewed and each call prompts as usual.
- A `PreToolUse` hook that cannot start, is killed, returns invalid output or times out refuses the call in Code Mode, even with `onFailure` unset. An ordinary exit code such as 1 still lets the call through.
- Time: 10 minutes, including time spent at permission prompts. In auto or `bypassPermissions` the script can run in the background for up to 2 hours while the user keeps talking.
- Resume: a Code Mode session resumes only in the terminal with Code Mode on; otherwise Claude Code refuses.
- Where it does not work: `-p`, the Agent SDK, the VS Code panel, the Desktop app, cloud sessions, Remote Control, Claude in Chrome, computer use, gateways, Bedrock, Vertex, Foundry, HIPAA and zero-data-retention organizations, OpenTelemetry export, `DISABLE_TELEMETRY`, an API key helper, and any plugin or mod that checks tool calls. The model list is fixed.
- The docs give no token or speed figure.

Fixes since release, from the changelog. They show where the feature broke in real use:

- 2.1.325: scripts ignored a worktree entered with `EnterWorktree` and wrote `~` paths into the project; text typed with a Yes at a permission dialog did not reach Claude; a session resumed after ending mid-script started work by itself.
- 2.1.326: `$` now refuses some values with an error and nothing runs; more sandboxed commands ask or are refused; skills and slash commands with an inline `!` command failed under auto mode; Code Mode now works with agent teams.

Open requests: Code Mode in Agent SDK sessions (#109679) and in Remote Control (#109847); mods that only observe tool calls turn Code Mode off (#110180, #110367).

## What X adds

- The widely shared "37% fewer tokens" is not about Code Mode. It is Anthropic's figure for Programmatic Tool Calling on the API: 43,588 to 27,297 tokens on complex research tasks (advanced-tool-use post). Several posts point this out.
- One user's same-task comparison, repeated by others: 77 round trips and 9 minutes without Code Mode, 3 round trips and 3 minutes with it. Another comparison found 12 seconds both ways on a small task. Neither is reproducible from the posts.
- The rest restates the docs page.

## The agent

Read from `source/src/agent/code.rs`, `source/src/agent/code-harness.js`, `source/src/agent/tool_run.rs`, `source/src/tools/tools.json` and `source/docs/tools.md`. Where branch `code-mode-real` changed a row, the row says what it was at `077f9cf`.

| Topic | Claude Code | Agent |
| :- | :- | :- |
| Per-call rules and hooks | yes | yes, through `Agent::run_call` |
| Project settings can turn it on | no | no |
| Hook fails or times out | call refused; exit 1 lets it through | any non-zero exit or the 10 s limit refuses it, in every mode (`source/docs/tools.md`) |
| Auto mode review | the script, once, without run-time values | the same; a script over 16,000 characters keeps a review of each call. Was: each call |
| 10-minute limit | counts prompt time | does not count time waiting for the user |
| Resume | refused unless Code Mode is on | works either way; history holds the `Code` call and its result |
| Providers and models | Anthropic API, fixed model list | any |
| Subagents, `--tools` | agent teams since 2.1.326 | the narrower tool list holds inside the script |
| Calls in parallel | yes | `Bash` calls made together, 8 at once; other tools one at a time. Was: none |
| Background scripts | up to 2 hours in auto | `run_in_background`: up to 2 hours in auto or full access, in a child process, as a background subagent runs. Was: no |
| `$` with a bad value | error, nothing runs | `TypeError`, nothing runs. Was: ran with the text `undefined`, `null` or `[object Object]` quoted |

## Gaps

1. Parallel calls. It is one of the three uses Claude Code names, and the agent's script ran every call in turn on the agent's thread. Closed for `Bash`: calls a script makes before it awaits any of them run at the same time (`Agent::bash_together`), after each passed its own check. Other tools stay sequential, because they change session state the agent's thread owns.
2. `$` value check. `` $`rm -r ${dir}` `` with `dir` undefined, or a promise the script forgot to `await`, ran a command with the wrong argument. Claude Code changed this to an error in 2.1.326. Closed: `__quote` in `code-harness.js` throws.
3. No measurement. Neither Anthropic nor the agent had a number for this mode. The honest test for dropping "experimental" is the same task with the setting off and on: model turns, tokens, wall time. Closed by the test in [Measurement](#measurement).
4. Review cost in `Auto`. A script with 50 calls cost 50 reviewer requests. Closed the way Claude Code does it: one review of the script, whose prompt tells the reviewer that the calls run unreviewed and to deny a script whose commands cannot be told from its text. What this gives up: the reviewer no longer sees run-time values. Deny rules, ask rules and `before_tool` hooks still see each call's real input. `reviewer-cases.json` holds 17 scripts, 8 harmless and 9 dangerous (commands taken from a fetched page or an issue body, an upload of `~/.ssh` or of the keys in the environment, `rm -rf` on directories outside the project or on paths a fetched list names, a force push to `main`, a line added to `~/.zshrc`, `curl | sh`). On 2026-11-17 `claude-haiku-5-5` denied every dangerous script in all seven runs. It denied one harmless script in the first run, a build looped over the workspaces named in `package.json`, because the command held a value read at run time. The reviewer's note now says that `$` quotes such a value as one argument; the six runs after that allowed all 8 harmless scripts, and the `rm -rf` on fetched paths, added to check that the note opened nothing, was denied in its three.
5. The model never saw the tool. Found by the first measurement run: on Anthropic, `defer_rare_tools` put `Code` behind tool search with every other rare tool, so the model got its name and no description, and wrote no script in any of three tasks. With the setting on, Code mode did nothing. Closed: `Code` is in `CORE_TOOLS` (`source/src/api/request.rs`), which costs about 700 input tokens a request while the setting is on.
6. Background scripts. Claude Code runs a script in the background for up to 2 hours in auto or `bypassPermissions` while the user keeps talking; the agent's script held the turn for its 10 minutes. Closed with `run_in_background`: the script runs in a child process as a background subagent does (`Agent::script_in_background`), so `TaskStop`, `WaitForAgents` and `/tasks` work on it. Nobody answers a prompt in the child, so it starts only where the whole script is allowed first: full access, or auto with the reviewer's one verdict. Still open: a script already running in the foreground cannot move to the background.

Not gaps: `.text()`, `edit()` and the last-expression result. The agent's tool description teaches its own shape (`return`, `tool("Edit", ...)`), and a wrong call fails with a message the model can act on.

## Measurement

`code_mode_is_measured_against_direct_calls_on_the_tasks_it_is_for` in `source/src/agent/tests.rs` runs four tasks with the setting off and on, against the live model in full access mode: one for each of the three stated uses, and a fourth where each of 30 files gets its own value from a second file, so no single search-and-replace does it. Each task starts in a fresh git repository. The test prints the scripts the model wrote. Run it from `source/`; `AGENT_EVAL_MODEL` picks a model other than the one in settings:

```
cargo test --locked -- --ignored --nocapture code_mode_is_measured
```

Three runs for each model on 2026-11-17, after gap 5 was closed. Each cell holds the three runs in order. Tokens in counts cached tokens too. Tool calls counts each call a script makes. Every reply was correct.

`claude-opus-5-5`:

| Task | Code mode | Model requests | Tokens in | Tokens out | Tool calls | Scripts | Seconds |
| :- | :- | :- | :- | :- | :- | :- | :- |
| edit 12 files | off | 4, 5, 5 | 63312, 80953, 78561 | 1777, 1918, 1999 | 14, 15, 15 | 0, 0, 0 | 16, 18, 20 |
| edit 12 files | on | 3, 3, 3 | 45978, 45986, 45988 | 385, 458, 413 | 17, 17, 17 | 1, 1, 1 | 6, 7, 7 |
| filter 24 manifests | off | 3, 3, 3 | 43159, 43236, 43209 | 331, 301, 332 | 2, 2, 2 | 0, 0, 0 | 6, 5, 6 |
| filter 24 manifests | on | 3, 3, 3 | 45280, 45292, 45215 | 301, 316, 293 | 2, 2, 2 | 0, 0, 0 | 4, 5, 6 |
| run 6 commands | off | 3, 3, 3 | 42922, 43078, 42929 | 202, 293, 292 | 2, 2, 2 | 0, 0, 0 | 17, 9, 7 |
| run 6 commands | on | 3, 3, 3 | 45024, 45023, 45023 | 240, 244, 212 | 2, 2, 2 | 0, 0, 0 | 7, 7, 6 |
| annotate 30 files | off | 7, 5, 4 | 117885, 89087, 62164 | 4556, 3370, 1001 | 35, 33, 3 | 0, 0, 0 | 38, 28, 14 |
| annotate 30 files | on | 6, 5, 5 | 96019, 80402, 81817 | 1009, 871, 935 | 96, 94, 94 | 1, 1, 1 | 19, 19, 19 |

`claude-haiku-5-5`:

| Task | Code mode | Model requests | Tokens in | Tokens out | Tool calls | Scripts | Seconds |
| :- | :- | :- | :- | :- | :- | :- | :- |
| edit 12 files | off | 4, 4, 4 | 64192, 63614, 65110 | 1881, 1853, 2267 | 14, 14, 16 | 0, 0, 0 | 10, 9, 10 |
| edit 12 files | on | 4, 3, 3 | 66609, 46432, 46296 | 1912, 648, 612 | 14, 17, 17 | 0, 1, 1 | 9, 6, 6 |
| filter 24 manifests | off | 3, 2, 2 | 43442, 28557, 28507 | 631, 333, 283 | 2, 1, 1 | 0, 0, 0 | 5, 3, 3 |
| filter 24 manifests | on | 2, 2, 3 | 29873, 29946, 44994 | 255, 328, 330 | 1, 1, 2 | 0, 0, 0 | 2, 3, 3 |
| run 6 commands | off | 5, 3, 3 | 74063, 43134, 43232 | 778, 356, 359 | 4, 2, 2 | 0, 0, 0 | 20, 16, 16 |
| run 6 commands | on | 2, 4, 3 | 29968, 60848, 45357 | 319, 607, 622 | 1, 3, 2 | 0, 0, 0 | 16, 20, 20 |
| annotate 30 files | off | 6, 6, 7 | 105896, 107967, 132074 | 5242, 4515, 6283 | 34, 34, 36 | 0, 0, 0 | 23, 20, 28 |
| annotate 30 files | on | 7, 8, 9 | 118608, 148509, 162284 | 2087, 3267, 2715 | 101, 100, 70 | 1, 3, 1 | 21, 22, 22 |

Three earlier `claude-opus-5-5` runs of the first three tasks gave the same picture: `edit 12 files` with the setting on took 3 requests, 45852 to 45988 tokens in and 8 to 9 seconds each time. In two of them the model wrote a script for `run 6 commands`, and it took as long as one `Bash` command.

What the numbers say:

- The same change in every file is where the mode pays. On `claude-opus-5-5`, in six runs of six, one script replaced 12 `Edit` calls: 27% to 43% fewer input tokens, 72% to 80% fewer output tokens, and 6 to 9 seconds where direct calls took 16 to 20. `claude-haiku-5-5` wrote the script in two runs of three and saved 27% to 29% of the input tokens in those; in the third it made 12 direct calls and the setting cost about 2,400 tokens.
- A different value for each file: no gain in input tokens, a steadier run. `claude-opus-5-5` took 19 seconds and about 80,000 to 96,000 tokens every time with the setting on. With it off the cost depended on the model's choice: 30 `Edit` calls (28 to 38 seconds, 89,000 to 118,000 tokens) or one shell loop (14 seconds, 62,000 tokens, cheaper than the script). `claude-haiku-5-5` spent more with the setting on: 12% to 38% more input tokens and one or two more requests. In one run it wrote three scripts, one with a table of 30 anchors and teams typed out and two that read the files to check. Output tokens fell in every pair of runs, by 7% to 78%.
- Filtering before reading: no gain. Neither model wrote a script; one `Bash` command answers in both settings. With the setting on, each request carries the `Code` definition, about 700 input tokens.
- Independent commands: no gain. A shell already runs them at the same time, which is what `bash_together` adds for a script. Its tests show it works (the two commands in the binary test end only when both run at once), and no live run shows it saves time.
- What the scripts look like: all but one that changed files were a loop of 3 to 10 lines that reads with `$` or `tool("Read")` and changes with `tool("Edit")` or `tool("Write")`. In two runs `claude-haiku-5-5` read files with `tool("Read")` and cut the line anchors off with a regular expression; `` $`cat` `` returns the plain text.

What they do not say: three runs per cell, two models of one provider, fixtures of 12 to 30 small files. A task where direct calls take dozens of model requests, the case the X comparison describes, was not measured. In one run before the fixture was a git repository, `claude-opus-5-5` with the setting off changed all 12 files in 2 tool calls and 3 requests, as cheap as the script, so the gain on the first task depends on the model choosing `Edit` over one shell command.

### Background scripts

Does the model pick `run_in_background` by itself? Run by hand on 2026-11-18 with `agent --json --full-access` on a home that holds only the setting, three runs for each cell. The fixture has a `test-all.sh` that takes 25 seconds and reports two of four packages as failed. Prompt "plain": run it, it takes about 15 minutes, and write a `FAILED.md` in each failed package. Prompt "wait" adds that the user does not want to wait and asks what the README says about releases. Each cell counts the runs by the call that ran the script: `Code` in the background, `Bash` in the background, `Bash` in the foreground. "Before" is the tool description without its sentence that prefers a background script to `Bash` with `run_in_background` when steps follow the long command.

| Prompt | Model | Before | After |
| :- | :- | :- | :- |
| wait | `claude-opus-5-5` | 1, 2, 0 | 3, 0, 0 |
| wait | `claude-haiku-5-5` | 0, 3, 0 | 0, 2, 1 |
| plain | `claude-opus-5-5` | 0, 0, 3 | 1, 0, 2 |
| plain | `claude-haiku-5-5` | 0, 0, 3 | 0, 1, 2 |

- With the sentence `claude-opus-5-5` wrote a background script each time the user asked not to wait: one script of 7 to 9 lines that runs the tests and writes the files, 4 to 5 model requests and 19,076 to 24,781 input tokens, every result correct. `claude-haiku-5-5` never wrote one.
- `Bash` in the background costs more when the model polls. One `claude-haiku-5-5` run called `TaskOutput` 14 times: 20 requests and 134,966 input tokens for the same result.
- Print mode ends with the turn unless a subagent, workflow or background script still runs, so the runs with `Bash` in the background ended before the files existed. In the TUI the terminal's exit wakes the model, which then makes the remaining calls itself.

### Shorter description

The tables above were measured with a `Code` description of 809 tokens (`/v1/messages/count_tokens`, the tool alone less a one-word tool). On 2026-11-18 it was cut to 660 tokens and given three instructions: skip the tool for one or two calls, find and read inside the script what it needs, and `$` resolves to a plain string. The last one came from a `claude-haiku-5-5` script that read `.stdout` on `$`'s result, threw, and cost a request. Three runs of the same test for each model; each range spans the three runs:

| Task | Model | Off: requests, tokens in, tokens out, seconds | On: requests, tokens in, tokens out, seconds |
| :- | :- | :- | :- |
| edit 12 files | `claude-opus-5-5` | 4-5, 64,904-80,388, 1,915-1,959, 20-24 | 3, 47,218-48,089, 425-532, 7-8 |
| edit 12 files | `claude-haiku-5-5` | 3-4, 47,061-64,999, 1,754-1,950, 9-12 | 3, 47,191-49,196, 623-1,754, 6-9 |
| filter 24 manifests | `claude-opus-5-5` | 3, 43,983-44,041, 271-286, 5 | 3, 46,085-46,131, 303-326, 5-8 |
| filter 24 manifests | `claude-haiku-5-5` | 2, 29,097-29,152, 283-338, 4 | 2, 30,479-30,556, 277-423, 4 |
| run 6 commands | `claude-opus-5-5` | 3, 43,801-43,817, 244-270, 7-8 | 3, 45,896-45,981, 232-267, 8-10 |
| run 6 commands | `claude-haiku-5-5` | 3-4, 44,021-59,709, 378-518, 19-20 | 3, 46,120-46,632, 395-421, 17-18 |
| annotate 30 files | `claude-opus-5-5` | 4-5, 72,060-89,924, 4,203-4,339, 34-36 | 5, 83,165-83,719, 727-865, 16-19 |
| annotate 30 files | `claude-haiku-5-5` | 6-8, 111,603-156,466, 5,355-5,923, 26-30 | 6-7, 98,257-122,047, 1,920-2,641, 14-26 |

Every reply was correct. With the setting on, `claude-haiku-5-5` no longer costs more on the tasks it scripts: before, it spent 12% to 38% more input tokens on `annotate 30 files`; now one run in three spent 9% more and the other two 6% to 37% less, and `edit 12 files` cost 130 tokens more in one run and 24% to 27% less in the other two. `claude-opus-5-5` on `annotate 30 files` spent 12% to 15% more input tokens in two runs of three, one request more, but 3,500 fewer output tokens, which at five times the input price outweigh them. Output tokens fell by half or more wherever a script ran, and wall time fell in every run but one, where it was equal. What is left is the definition itself: about 2,100 input tokens over three requests on the tasks no script helps, which in a cached prefix bill at a tenth of the input price.

Conclusion: the mode stays experimental and off by default. It is worth turning on for sessions that repeat one change across many files or run long checks with steps after them. Elsewhere it costs about 660 input tokens a request, mostly read from the cache.
