# Shrinking the context: what upstream agents do, compared with the agent's condense

Question: which open-source designs for pruning, condensing and compacting a coding agent's context give the best accuracy per token spent, judged on prompt-cache economics, and what should the agent's condense become?

Answer: no upstream agent that measures cost summarizes tool outputs with a second model the way the agent's condense does. The designs with published numbers all do three things the agent does not: they replace old outputs with a fixed placeholder instead of a summary (masking), they rewrite the prefix rarely and in large steps (a cutoff that only advances at a pressure threshold or when the cache is cold), and they keep a way back to the original. JetBrains measured masking at the same solve rate as LLM summarization for about half the raw agent's cost, with summary calls alone at up to 7.2% of the bill. condensed-milk-pi, a pi extension built on that result, reports about 3 tokens of cheaper reads for each token of extra cache writes in shared field data; the agent's 868 sessions show about 1.16 (148M saved against 128M written). A finding that changes the design on Anthropic: on models with preserved thinking (Claude Opus 5.5, Sonnet 5.5, Haiku 5.5), any client-side rewrite of an earlier tool result makes every later thinking block fail the prefix check, and the agent's `drop_block` setting then removes all of that reasoning from the model's view. Anthropic's server-side `clear_tool_uses_20250919` does not count as a change. See [Recommendations](#recommendations).

Checked 2026-11-26. Code was read from shallow clones at the commits below; Anthropic and Claude Code pages were fetched as Markdown. Numbers from papers that only reach this page through another project's notes are marked "secondary".

## Sources

- pi-condense (`jjuraszek/pi-condense`, `8b9d3e8`, 2026-11-25), the design the agent's condense came from: `PRUNING.md`, `src/types.ts`
- pi (`badlogic/pi-mono`, `db79712`): `packages/coding-agent/src/core/compaction/compaction.ts`, `compaction/utils.ts`, `docs/compaction.md`
- condensed-milk-pi (`tomooshi/condensed-milk-pi`, `81dce5d`, 2026-11-19): `README.md`, `index.ts`, `docs/research/2026-11-01-addressable-recall.md`, `docs/research/2026-11-18-field-data.md`
- Codex CLI (`openai/codex`, `af71942`): `codex-rs/core/src/compact.rs`, `core/src/session/context_window.rs`, `core/src/session/async_compaction.rs`, `core/src/context_manager/history.rs`, `protocol/src/openai_models.rs`, `prompts/templates/compact/prompt.md`
- opencode (`sst/opencode`, `314842d`): `packages/core/src/session/compaction.ts`, `packages/core/src/config/normalize.ts`; the removed prune at `8c94e9005f`: `packages/opencode/src/session/compaction.ts`, `message-v2.ts`
- Cline (`cline/cline`): classic extension at tag `v3.30.0`, `src/core/context/context-management/ContextManager.ts` and `context-window-utils.ts`; current SDK at `2a0f048`, `sdk/packages/core/src/extensions/context/`
- Roo Code (`RooCodeInc/Roo-Code`, `b867ec9`, last commit 2026-05-15): `src/core/context-management/index.ts`, `src/core/condense/index.ts`, `src/core/condense/foldedFileContext.ts`
- OpenHands: V0 condensers at tag `0.59.0` of `OpenHands/OpenHands`, `openhands/memory/condenser/impl/`, `openhands/core/config/condenser_config.py`; V1 SDK (`OpenHands/software-agent-sdk`, `d726333`), `openhands-sdk/openhands/sdk/context/condenser/`; blog https://openhands.dev/blog/openhands-context-condensensation-for-more-efficient-ai-agents
- SWE-agent (`SWE-agent/SWE-agent`, `3ea751c`): `sweagent/agent/history_processors.py`, `config/default.yaml`, `config/sweagent_0_7/07.yaml`
- "The Complexity Trap" (Lindenbauer et al., NeurIPS 2025 DL4Code workshop): https://arxiv.org/abs/2508.21433 (HTML v3), repo `JetBrains-Research/the-complexity-trap` (`bf15b5f`), `config/`
- Anthropic API: https://platform.claude.com/docs/en/build-with-claude/context-editing.md, `.../compaction.md`, `.../preserved-thinking/prefix-checks.md`, `.../preserved-thinking/change-tools-or-instructions.md`, `.../preserved-thinking/fix-bound-to-a-different-conversation.md`
- Claude Code: https://code.claude.com/docs/en/prompt-caching.md, `costs.md`, `settings-reference.md` (`idleCompaction`)

## What each one does

### pi-condense (current upstream of the agent's design)

- Trigger: default `pruneOn: "agent-message"` (`src/types.ts:588`). Tool-call batches accumulate untouched for the whole run and are summarized once, 1.5 s after the run settles on its final reply, off the critical path (`PRUNING.md:476-596`). Optional mid-run triggers: a context fraction capped at 300k tokens, a per-turn jump, and an absolute un-pruned tail (`PRUNING.md:892-925`). The agent's condense summarizes after every turn instead.
- Before any summarizer call, a zero-LLM pipeline (`PRUNING.md:598-760`): protected tools and paths never enter a batch (skills by default, `src/types.ts:596`); a single result over 65,536 chars is spilled to a sidecar file and replaced by a head preview plus ref; identical outputs (hash of tool name and normalized text) become aliases of the earlier ref; batches under 2,000 chars are skipped (they compress to about 44% against about 7% for a typical batch, `PRUNING.md:720-726`); a summary longer than its input is thrown away.
- Re-reads: an older read of a path that a later read fully covers becomes a one-line "superseded" stub, but only when the pruner is already rewriting at or before it, or on a cold-cache event (`PRUNING.md:667-690`).
- Chain compression: once more than 3 user-to-final-reply chains have closed, the oldest one loses all its middle assistant turns, tool calls and results; one `<compressed-chain>` message keeps the batch summaries and the refs (`PRUNING.md:926-1040`). Long string arguments of summarized calls are replaced by `<purged-args size=... ref=...>` in the same rewrite (`PRUNING.md:1159-1198`).
- Idle cleanup: the first prompt after `idleCleanupMinutes` (default 180, or the model's declared cache lifetime if longer) compresses every closed chain, the newest three included, with no model call (`PRUNING.md:1000-1013`).
- Way back: `context_tree_query` by `tN` ref; superseded reads say "re-read the file".
- Numbers: none for cost or accuracy. The rationale cites SUPO, ReSum and ACON (secondary).

### pi (badlogic/pi-mono) compaction

- Trigger: `contextTokens > contextWindow - reserveTokens`, defaults 16,384 reserve and 20,000 recent tokens kept (`compaction.ts:128-129`, `267-270`).
- Summarizes from the previous kept boundary to the cut into a structured summary (Goal, Constraints, Progress, Key Decisions, Next Steps, Critical Context, read and modified files), passing the previous summary in (`docs/compaction.md`). Tool results are cut to 2,000 chars in the transcript (`utils.ts:95`).
- Cache: the summary request is a text transcript sent with `cacheRetention: "none"` (`compaction.ts:626-631`), so it reads nothing from the conversation's cache.

### condensed-milk-pi

- At landing: command-aware filters shrink bash output before the model first sees it (test runs that all passed become one line, `git status`, `ls`, `grep`, JSON shape), and never hide a failure (`README.md`).
- Masking: a cutoff index advances only when context use crosses 30%, 45% or 60%, masking 60%, 80% or 95% of messages (`index.ts:80-100`); between advances the prefix is byte-identical. The idle trigger advances the cutoff when the cache is already dead; a 32 KB floor skips advances that would free little.
- Placeholder: `[cm-masked bash #id] <command> (N lines, SIZE; cm_recall returns it)`, id from a hash of the call id, so it never changes; an opt-in digest adds one outcome line of at most 120 chars.
- Way back: `cm_recall` by id, with `grep` and `offset`/`limit`, capped at 2,000 lines and 5% of the window.
- Numbers (`docs/research/2026-11-18-field-data.md`, one shared export of 1,223 sessions on Sonnet 5.5, Opus 5.5 and GPT-6.1): about 170M input-token equivalents of cache reads avoided against about 49M of extra writes, a ratio of about 3 (9 in the maintainer's own log). 17% of sessions that masked used recall; 11.7% of masked reads were read again, median 7 turns later. On simulated masking of 40 sessions, only 0.2% of masked results were fetched again with the identical command.
- Secondary, from its research note: in 2,848 billed Claude Code runs, cutting tool output by 38.4% raised the bill 6.8%; per-turn masking cut the cache hit rate from 84% to 23% while rewrites every 3 steps kept it; deleting output raised re-fetches up to 7.5 times in 9,400 SWE-bench runs. Not verified here.

### Codex CLI

- No tool-output pruning in history. Every tool output is truncated once, when it is recorded, before it is first sent (`context_manager/history.rs:819-828`), to 10,000 bytes by default (`models-manager/src/model_info.rs:128`).
- Auto-compaction at 90% of the window, or a lower configured limit (`protocol/src/openai_models.rs:575-586`); an optional turn-end threshold (`core/src/config/mod.rs:693-698`, `context_window.rs:114-120`); an experimental background compaction that starts at a configurable share and is adopted at a checkpoint (`session/async_compaction.rs:1-3`, `55-82`).
- The compaction request is the unchanged history plus the prompt "CONTEXT CHECKPOINT COMPACTION... handoff summary" (`prompts/templates/compact/prompt.md`, `compact.rs:123-136`, `300-330`), so it reuses the cached prefix. On overflow it drops the oldest item and retries (`compact.rs:365`).
- Kept after compaction: always-retained items, the newest user messages up to 20,000 tokens, then the summary (`compact.rs:69`, `733-866`). No way back to originals.

### opencode

- The old `prune` (removed with the rewrite; the current config rejects the key, `packages/core/src/config/normalize.ts:373`): walking back from the newest, skip the last 2 user turns and the newest 40,000 tokens of tool output, then mark older outputs as compacted, but only if that frees more than 20,000 tokens; `skill` is never pruned; stop at the first already-pruned part (`8c94e9005f:packages/opencode/src/session/compaction.ts:28-31`, `241-286`). Pruned outputs render as `[Old tool result content cleared]` (`message-v2.ts:293-294`). No way back.
- Current compaction (`packages/core/src/session/compaction.ts`): keeps 15% of the window free (at least 16k) because single steps grew by up to about 22k tokens at p99 (`129-137`); keeps 15,000 recent tokens verbatim (`275`). The summary request sends the whole conversation unchanged plus the prompt, "Keeping the normal request's prefix intact lets the summary reuse its prompt cache" (`478-495`, `550-566`).
- Background precompute: within 60,000 tokens of the ceiling, summarize in the background from the prefix the provider just cached, hold the summary, and swap it in only when compaction is due, "swapping it in now would rewrite the prefix and throw away the cache for every step until compaction is actually due" (`344-356`, `160`). It starts only while at least 30 s of the 5-minute cache lifetime remain (`161-164`).

### Cline

- Classic extension (`v3.30.0`): over `contextWindow - 40,000` on a 200k model (`context-window-utils.ts:19-31`), first replace every older read of a file that was read again with a "duplicate file read" notice (`ContextManager.ts:669`, `741-745`); if that saves 30% of the characters, stop there (`217`); else hide half (or three quarters) of the messages after the first user-assistant pair (`197-235`, `263-280`). The hidden range moves only at that threshold.
- Current SDK: compaction at 90% of the input budget, 20,000 recent tokens kept, tool results cut to 2,000 chars for the summarizer (`sdk/packages/core/src/extensions/context/compaction-shared.ts:15-37`).

### Roo Code

- Auto-condense when the previous context passes a configurable percent of the window (default 100) or 90% of the window minus reserved output; otherwise a sliding window hides half the messages (`src/core/context-management/index.ts:24`, `280-331`).
- "Fresh start": after condensing, the model sees only the summary; originals stay stored and tagged (`src/core/condense/index.ts:239-242`, `460-475`). The summary is followed by tree-sitter definitions of the files read, up to 50,000 chars (`foldedFileContext.ts`).

### OpenHands

- V0 condensers (`0.59.0`): `observation_masking` replaces observations older than a window with `<MASKED>` (config default 100 events, `condenser_config.py:19-30`; `observation_masking_condenser.py`); `recent`, `llm` (summarize the forgotten half, 100 events, observations cut to 10,000 chars, `61-86`), `amortized`, `llm_attention`, `structured`, and a pipeline of these.
- V1 SDK default: `LLMSummarizingCondenser`, `max_size` 240 events, `keep_first` 2 (`llm_summarizing_condenser.py:64-67`). The README gives the cache reasoning: the first half of the events becomes one summary, "condensation destroys the prompt cache, but doing so regularly keeps the cost of rebuilding the prompt cache low" (`condenser/README.md`).
- Published numbers (blog): up to 2x lower cost per turn, cost per turn flat instead of growing, and 54% against 53% solved on a SWE-bench Verified subset.

### SWE-agent

- `last_n_observations`: older observations become `Old environment output: (n lines omitted)`; `polling` updates the cut only every k steps because "every time we change the history, we need to cache everything again" (`history_processors.py:85-177`). The 0.7 config used `n: 5` (`config/sweagent_0_7/07.yaml:99-101`); the current default only places `cache_control` on the last 2 user messages (`config/default.yaml:67-69`, `history_processors.py:261-300`).
- `closed_window` replaces every older view of a file that was shown again later (`history_processors.py:215-258`).

### The Complexity Trap (JetBrains)

- Setup: SWE-agent on SWE-bench Verified, 250-turn limit, five model configurations. Masking keeps the last 10 observations (M=10, the best of the window sizes they tried); LLM-Summary summarizes 21 turns and keeps the last 10 (`config/default_no_demo_N=1_M=10.yaml`, `config/default_no_demo_checkpoint_same_model_openhands_N=21_M=10.yaml`).
- Observations are about 84% of an average SWE-agent turn.
- Table 1: masking cut cost 50.9%, 9.8%, 52.7%, 56.1% and 57.1% against the raw agent; summary cut 55.4%, 0%, 50.4%, 41.5% and 55.4%. Solve rates of the two were within each other's confidence intervals in four of five setups; on Gemini 2.5 Flash with thinking both lost against the raw agent (masking -9.9%, summary -22.3%).
- Table 2: the summary calls alone cost 0.65% to 7.2% of each instance, and they read almost nothing from the cache because each one processes a unique sequence of turns.
- Hybrid (mask with a window of 10, summarize every 43 turns): 7% cheaper than masking alone and 11% cheaper than summary alone.
- The paper reports that summary runs take more turns ("trajectory elongation"); condensed-milk's note puts it at about 15% (secondary, not checked in the paper text).

### Anthropic API

- `clear_tool_uses_20250919` (beta header `context-management-2025-06-27`): once the prompt exceeds `trigger` (default 100,000 input tokens, or a count of tool uses), the server replaces the oldest tool results with a placeholder, keeping the newest `keep` (default 3) tool uses; `clear_at_least` skips the edit unless it frees that many tokens; `exclude_tools` is never cleared; `clear_tool_inputs` also clears the call's arguments. The client keeps the full history. The response's `context_management.applied_edits` reports `cleared_tool_uses` and `cleared_input_tokens`.
- Caching, from the docs: clearing "invalidates cached prompt prefixes when content is cleared... clear enough tokens to make the cache invalidation worthwhile. Use the `clear_at_least` parameter".
- Preserved thinking (Opus 5.5, Sonnet 5.5, Haiku 5.5, Fable and Mythos 5.x): each thinking block's signature records its prefix. "If your code clears, shortens, or re-encodes something it already sent, the thinking after it becomes invalid. To clear old tool results later... use context editing", which changes "only what Claude reads, so [it] never count[s]" (prefix-checks, change-tools-or-instructions). With `drop_block`, the invalid blocks are dropped on every later request that sends them; "Claude can spend more output tokens working out reasoning it can no longer read. Dropping the blocks also invalidates the prompt cache from the first dropped block onward" (fix-bound-to-a-different-conversation). For summaries written by your own code: "cut rarely, in large steps".
- Server-side compaction: on demand (`compact-2026-09-04`, can keep recent turns and their thinking, can run in the background) or at a token threshold (`compaction.md`).

### Claude Code

- Compaction request: "repeats your conversation's system prompt, tools, and the history being summarized... While the cache is warm, that request reads your prefix from the cache" (`prompt-caching.md`, Compacting the conversation).
- Idle compaction: on the 1-hour TTL, a conversation over 200,000 tokens (minimum 100,000) that sits idle for about an hour is compacted "before the prompt cache expired", and the next message sends the summary (`settings-reference.md`, `idleCompaction`).
- Tool-result clearing exists: `/cost` counts "expected rebuilds (compaction or tool-result clearing)" (`costs.md`). Its trigger, placeholder and thresholds are not documented publicly. condensed-milk's note says the placeholder is `[Old tool result content cleared]` with no way back (secondary).

## Comparison

| | Trigger | Removes | Keeps | Cache handling | Way back | Measured |
| :- | :- | :- | :- | :- | :- | :- |
| The agent's condense | each turn past 60k tokens, batch of at least 10k chars | outputs of the turn's calls, replaced by a Haiku summary | protected tools, newest summary held | applied when cold or past 3/4 of compaction (pending change) | `ContextTreeQuery` by ref | 1.16 reads saved per extra write; median 88k written to remove 19k |
| pi-condense | once per run, after the final reply; idle prompt | outputs (summarized), long arguments, middle turns of old chains, superseded reads | 3 newest chains, protected tools and paths | one rewrite per run; dedup and supersede only at rewrite or cold moments | `context_tree_query` | none |
| condensed-milk-pi | context at 30/45/60%, or idle | outputs, masked with a fixed placeholder | 40/20/5% of messages | static cutoff, few advances, idle advance | `cm_recall` with grep and ranges | reads saved about 3x extra writes |
| Codex | 90% of window; optional turn end; background | everything but retained items and 20k tokens of user messages | user messages, summary | output truncated at landing; summary request reuses cache | none | none |
| opencode prune (removed) | each turn | outputs older than the newest 40k tokens | 2 last turns, `skill` | only if it frees more than 20k | none | none |
| opencode compaction | 15% (at least 16k) before the window | everything before the newest 15k tokens | recent 15k tokens | summary from the cached prefix, precomputed, swapped in when due | none | p99 step 22k |
| Cline classic | window minus 40k | older reads of a re-read file, then half the messages | first pair | hidden range moves only at the threshold | none | none |
| Roo Code | percent of window | everything (summary), or half (sliding window) | summary plus file outlines | none | none | none |
| OpenHands V1 | 240 events | first half of the events, summarized | first 2 events, second half | rare large rewrites | none | up to 2x cheaper per turn, 54% vs 53% solved |
| SWE-agent | every step | observations beyond the newest n | n observations | `polling` | none | Complexity Trap below |
| Complexity Trap masking | every step | observations beyond the newest 10 | 10 observations | none | none | 10% to 57% cheaper, solve rate on par with summary |
| Anthropic `clear_tool_uses` | input tokens past `trigger` | oldest tool results (and inputs if asked) | `keep` newest, `exclude_tools` | `clear_at_least`; thinking stays valid | client keeps originals | none |

## Cache economics

In input-price units on a warm cache: a rewrite that starts W tokens before the end of the prompt and removes R of them makes the next request write W - R tokens at 1.25 that it would have read at 0.1, an extra 1.15 (W - R). Each later request then reads 0.1 R less. It pays off after 11.5 (W - R) / R requests.

- The agent's median swap (W = 88k, R = 19k): 42 requests at full context before the next compaction or idle break. That matches the measured break-even.
- A rewrite that removes half of what it rewrites (R = W / 2), as a cutoff advance over all old outputs does: 11.5 requests.
- A rewrite on a cold cache: the whole prompt is written anyway, so the rewrite is free and saves 1.25 R on that first write.

So R / W and the cache state decide the cost; the quality of the summary does not enter it. A summary also adds its own tokens back into R. Every source above that thinks about caching lands on the same two levers: rewrite when the cache is cold, and otherwise rewrite seldom with a large R / W (pi-condense's once-per-run flush, condensed-milk's zones, SWE-agent's `polling`, opencode's 20k minimum, Anthropic's `clear_at_least`, OpenHands' half-history).

On preserved-thinking models there is a second cost that the formula above leaves out: after a client-side rewrite at position p, every thinking block after p is dropped on every later request, so the model works on without its reasoning for those turns.

## Recommendations

1. Mask, do not summarize. Replace condensed outputs with a fixed placeholder that names the tool, its input in one line, the size and the `tN` ref, plus at most one deterministic outcome line (exit code, last line of a test run). Keep `ContextTreeQuery`, and give it `grep`/`offset` like `cm_recall`. Evidence: masking matched summary solve rates at equal or lower cost (Complexity Trap Table 1), summary calls cost up to 7.2% (Table 2), and condensed-milk's masking returned about 3 tokens per extra write against the agent's 1.16. Effect for the agent: R becomes nearly the whole output instead of output minus summary, the Haiku calls go away, and the background thread with them.
2. One cutoff, advanced rarely, over all old outputs at once. Move the cutoff when the cache is cold (the pending change) or when the context crosses a pressure point, and then mask every unprotected output older than the newest N turns in one rewrite, not "all but the newest summary". Fold duplicate and superseded reads into the same rewrite (Cline, pi-condense, SWE-agent `closed_window`). Effect: R / W rises from about 0.2 to 0.5 or more, so a warm rewrite pays off in about 11 requests instead of 42.
3. On Anthropic with a preserved-thinking model, prefer `clear_tool_uses_20250919` with `exclude_tools` set to the protected tools, a large `clear_at_least` (for example 40k) and a `keep` that matches item 2. Recovery still works: the agent keeps the originals, and the `tool_use` ids stay visible unless `clear_tool_inputs` is set. Effect: the same cache cost as a client rewrite, without losing the thinking of every later turn. Not verified: how the server keeps the cleared set stable between activations (whether a request below `clear_at_least` un-clears), which needs a live run that logs `cache_read_input_tokens`, `cache_creation_input_tokens` and `applied_edits` across consecutive requests before it replaces item 2 on that provider.
4. Compaction: request the summary from the main model with the unchanged conversation plus the prompt, so the input is read from the cache (Codex, opencode, Claude Code); start it in the background within about 60k tokens of the limit while at least 30 s of the cache lifetime remain, and swap it in when compaction is due or the cache goes cold (opencode, Claude Code's idle compaction). On Anthropic, on-demand compaction (`compact-2026-09-04`) does the same and keeps the kept turns' thinking valid. Effect: the agent now sends a serialized transcript uncached to Haiku (`agent/compaction.rs`, `summarize`); whether the cached main-model read is cheaper depends on the two models' prices, but the precompute removes the wait at the threshold.
5. Shrink at landing, which costs no cache at all: Codex truncates every output to 10,000 bytes when it is recorded, against the agent's 50 KB and 2,000 lines (`source/src/tools/mod.rs:30-31`); pi-condense spills single results over 65,536 chars to a file with a head preview and a ref.

What to drop from the current design: the per-turn Haiku batch summary and everything that exists for it (`SUMMARIZER_SYSTEM`, the `[[N:name]]` label protocol in `condensed`, `summary_block`, `SUMMARY_MAX_TOKENS`, `min_batch_chars`, the held queue in `release`), and the 60k-token `MIN_CONTEXT_TOKENS` gate, which the cold and pressure triggers replace. Keep the refs, `view`'s stubs, `ContextTreeQuery`, `PROTECTED_TOOLS` and the skill-file protection.

Not checked: whether masking keeps the solve rate on Opus-class models in the agent's own tasks (the Complexity Trap's strongest model was Qwen3-Coder 480B); Claude Code's tool-result clearing rules; the secondary numbers quoted from condensed-milk's research note.

## Measured on the live API

Run on 2026-11-26 with `claude-haiku-5-5`, a fixture of small text files read one `Read` call per request, and the usage the session journal logs (`cleared` is `cleared_input_tokens` from `context_management.applied_edits`).

- `clear_tool_uses_20250919` with a fixed `keep` slides: once the prompt passes `trigger`, every request clears one more result, and `clear_at_least` counts the results cleared before too, so it does not stop that. Each slide missed the cache back to the tools breakpoint: `cache_read` stayed at 15,214 (system and tools) and every request rewrote the whole message history.
- A `keep` the client raises by one with each new call holds the same oldest results cleared, when each call starts a turn of its own, as in this fixture. Calls inside one turn do not count; see the turn in progress below. After the one rewrite where clearing began, `cache_read` grew again request by request, with writes of one file's size.
- `keep` counts only the tool uses `exclude_tools` does not name. Counted over all uses, the cleared set shrank as excluded calls came in, and the cache missed each time.
- `keep` also leaves out the turn in progress: the tool uses after the last user message that holds no `tool_result` (a message with tool results and text still continues the turn). The server never clears them. Found 2026-11-27 in the session logs: since the cold-cache cut shipped, 1,160 of 11,528 requests rewrote the cache from the start of the messages with the cache still warm, 171M of 235M cache-write tokens, about 40% of the spend in that window; 1,147 of them sent a clearing edit. Each call in a turn raised `keep` by one while the server's count did not move, so one more old result came back every request. Replayed against the API: a request with 31 uses, 1 of them in the turn, cleared 20 with `keep` 10 and 19 with `keep` 11; with 32 and 33 uses and `keep` 10 it still cleared 20 and read 66k tokens from the cache. Fixed by counting `keep` and placing the cut over the uses before the turn in progress only (`api::turn_start`). A text-only user message right after a tool result does not open a turn: the API merges consecutive user messages, and a replay with such a note inserted between two calls of one turn still cleared 30 - `keep` of 30 earlier uses (checked 2026-11-27).
- A message thread refuses the edit: the create fails with `thread_unsupported_request` ("`thread` cannot be combined with `context_management` edits other than `clear_thinking_20251015`"). Requests that clear go without a thread.
- Any change before the last cache breakpoint costs a write of everything after the tools breakpoint, not just the part after the change, because the cache is only read at breakpoints. So a cut is worth moving only in large steps.
- A cut that moves to all but the newest 10 clearable calls: with a 104k compaction point and 45 reads of about 1,600 tokens, one move at 79k context cleared 43,930 tokens, one rewrite of 19,414 tokens, then cache hits on every later request. Resuming that session after it went cold cleared 49,773 tokens on its first request.

## Benchmark

Run on 2026-11-26 with `claude-haiku-5-5`, two runs per cell. The fixture is 36 files of 9 to 22 KB from `source/src` at `HEAD`, read one `Read` call per request into a 200k window (16k reserve). Then five questions on constants from the first five files, tools allowed. Cost is in thousands of input-token equivalents: input + 0.1 cache read + 1.25 cache write + 5 output, the summarizer and compaction calls included. "Old" is `HEAD` with the Haiku summaries; "off" and "clearing" are this change with `condense.enabled` false and true. Every run answered 5 of 5.

| Session | Old | Off | Clearing |
| :- | -: | -: | -: |
| 36 reads with a warm cache throughout | 1,600, 1,611 | 757, 756 | 758, 756; 1,065, 1,073 while the cut also moved near compaction |
| 20 reads, a 5-minute break, 10 reads | 1,203, 1,018 | 830, 815 | 749, 752 |

- The Haiku summaries cost 1.3 to 2.1 times as much as no condensing at all: each swap rewrote the cache from the start of the messages, and the summarizer added 143k to 237k.
- Moves near compaction lost money: with the newest 10 results kept, the context climbed back past 3/4 of the compaction point within about 7 requests, and each move rewrote about 84k tokens to free about 56k. So the cut moves only on a cold cache.
- After the break, clearing wrote 99k tokens where off wrote the whole conversation, and the context stayed under the compaction point, so off paid a compaction there and clearing did not: 8 to 10% less than off, 26 to 38% less than old.
- The warm runs use a second process for the questions (`agent -c`). Before the resume read the last reply's time from the session journal, that resume counted as a cold cache, cleared 45k tokens and cost 838; now it waits, as the cache is still warm.
