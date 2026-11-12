use super::*;

pub(super) const SUMMARY_SYSTEM: &str =
    "You are a context summarization assistant. Your task is to read a conversation between a user and an AI assistant, then produce a structured summary following the exact format specified.

Do NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary.";

pub(super) const SUMMARY_PROMPT: &str = "The messages above are a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work.

Use this EXACT format:

## Goal
[What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]

## Constraints & Preferences
- [Any constraints, preferences, or requirements mentioned by user]
- [Or \"(none)\" if none were mentioned]

## Progress
### Done
- [x] [Completed tasks/changes]

### In Progress
- [ ] [Current work]

### Blocked
- [Issues preventing progress, if any]

## Key Decisions
- **[Decision]**: [Brief rationale]

## Next Steps
1. [Ordered list of what should happen next]

## Critical Context
- [Any data, examples, or references needed to continue]
- [Or \"(none)\" if not applicable]

Keep each section concise. Preserve exact file paths, function names, and error messages.";

pub(super) const SUMMARY_PREFIX: &str = "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
pub(super) const SUMMARY_SUFFIX: &str = "\n</summary>";
const TOOL_RESULT_SUMMARY_CHARS: usize = 2000;
const COMPACT_MAX_TOKENS: u64 = 128_000;

impl Agent {
    /// Summarizes the turn's tool results on a background thread; the next request picks the summary up.
    pub(super) fn start_condense(&mut self, assistant_text: &str, calls: Vec<condense::Call>) {
        if !self.condense.enabled {
            return;
        }
        let calls = condense::select(calls, self.condense.min_batch_chars, self.last_context_tokens);
        if calls.is_empty() {
            return;
        }
        let (system, prompt) = condense::summarizer_prompt(assistant_text, &calls);
        let request = api::plain_request(&self.condense.model, system, &prompt, condense::SUMMARY_MAX_TOKENS);
        let client = self.client.clone();
        let first_ref = self.next_ref;
        self.next_ref += calls.len();
        self.pending_condense.push(std::thread::spawn(move || {
            let reply = client.send_quiet(&request, &AtomicBool::new(false))?;
            let summary = text_of(&reply.message);
            if summary.trim().is_empty() || reply.stop_reason != "end_turn" {
                return Err("the summary came back empty or cut off".to_string());
            }
            Ok((condense::condensed(&summary, &calls, first_ref), reply))
        }));
    }

    /// Applies finished summaries; a failed one leaves its originals in place.
    pub(super) fn collect_condensed(&mut self, emit: &mut dyn FnMut(Event)) -> Result<(), String> {
        let (finished, running) = std::mem::take(&mut self.pending_condense).into_iter().partition(|handle| handle.is_finished());
        self.pending_condense = running;
        for handle in finished {
            match handle.join().unwrap_or_else(|_| Err("the summarizer thread panicked".into())) {
                Ok((batch, reply)) => {
                    self.usage.add(reply.usage);
                    self.session.append(&usage::reply_entry(&reply, now_ms()))?;
                    self.record(condense::entry(&batch))?;
                }
                Err(error) => emit(Event::Notice(format!("Condense failed, kept the original tool outputs: {error}"))),
            }
        }
        Ok(())
    }

    /// Summarizes everything before a recent assistant message and keeps the rest verbatim, like pi.
    /// Returns None when there is nothing old enough to summarize.
    pub fn compact(&mut self, focus: Option<&str>) -> Result<Option<String>, String> {
        let view = self.compacted();
        let Some(first_kept) = cut_point(&view, self.keep_recent_tokens) else {
            return Ok(None);
        };
        let summary = self.summarize(&view[..first_kept], focus)?;
        let cut = message_index(&self.history.compactions, first_kept);
        self.record(json!({"type": "compaction", "summary": summary, "cut": cut}))?;
        self.last_context_tokens = 0;
        // Turns after a thread the API could not store go without one; the shorter conversation gets another try.
        self.message_thread = api::MessageThread::default();
        Ok(Some(summary))
    }

    /// /handoff: summarizes the whole conversation into the first prompt of a fresh session; `goal` names its next task.
    pub fn handoff_prompt(&mut self, goal: Option<&str>) -> Result<String, String> {
        let view = self.compacted();
        if view.is_empty() {
            return Err("Nothing to hand off yet.".into());
        }
        let summary = self.summarize(&view, goal)?;
        Ok(handoff_prompt(&summary, self.session.path.as_deref(), goal))
    }

    fn summarize(&mut self, messages: &[Value], focus: Option<&str>) -> Result<String, String> {
        let prompt = compaction_prompt(&serialize(messages), focus);
        // The cheap model summarizes only when the conversation fits its window, so a 1M-context session keeps the main model.
        let catalog = self.client.catalog();
        let cheap = catalog.iter().find(|model| model.id == self.cheap_model && model.context_window >= self.context_window);
        let (model, output_limit) = cheap.map_or((self.model.clone(), u64::MAX), |model| (model.id.clone(), model.max_tokens));
        let max_tokens = (self.reserve_tokens * 4 / 5).min(COMPACT_MAX_TOKENS).min(output_limit);
        let request = api::plain_request(&model, SUMMARY_SYSTEM, &prompt, max_tokens);
        let reply = self.client.send_quiet(&request, &self.abort)?;
        self.usage.add(reply.usage);
        self.session.append(&usage::reply_entry(&reply, now_ms()))?;
        if reply.stop_reason != "end_turn" {
            return Err(format!("Summary stopped with {}.", reply.stop_reason));
        }
        let summary = text_of(&reply.message);
        if summary.trim().is_empty() {
            return Err("The summary came back empty.".into());
        }
        Ok(summary)
    }

    /// The messages with everything before the latest compaction replaced by its summary.
    pub(super) fn compacted(&self) -> Vec<Value> {
        compacted(&self.history.messages, &self.history.compactions, self.session.path.as_deref())
    }
}

/// Index of the assistant message where the kept tail starts: the latest one with at least `keep_tokens` after it, else the latest assistant message.
pub(super) fn cut_point(messages: &[Value], keep_tokens: u64) -> Option<usize> {
    let mut kept_chars = 0;
    let mut latest_assistant = None;
    for index in (1..messages.len()).rev() {
        kept_chars += messages[index].to_string().len() as u64;
        if messages[index]["role"] != "assistant" {
            continue;
        }
        latest_assistant.get_or_insert(index);
        if kept_chars / context::BYTES_PER_TOKEN >= keep_tokens {
            return Some(index);
        }
    }
    latest_assistant
}

pub(super) fn compaction_prompt(conversation: &str, focus: Option<&str>) -> String {
    let prompt = format!("<conversation>\n{conversation}\n</conversation>\n\n{SUMMARY_PROMPT}");
    match focus {
        Some(focus) => format!("{prompt}\n\n<focus>\n{focus}\n</focus>\nGive extra detail to what the focus names."),
        None => prompt,
    }
}

/// `transcript` is the handed-off session's file.
pub(super) fn handoff_prompt(summary: &str, transcript: Option<&Path>, goal: Option<&str>) -> String {
    let mut prompt = format!("This session continues an earlier one, handed off with this summary:\n\n<summary>\n{summary}\n</summary>");
    if let Some(path) = transcript {
        prompt.push_str(&format!("\n\nThe full earlier conversation is in {}, one JSON entry per line. Search it with rg when the summary lacks a detail you need.", path.display()));
    }
    prompt.push_str(&match goal {
        Some(goal) => format!("\n\nNext: {goal}"),
        None => "\n\nContinue with the next steps.".into(),
    });
    prompt
}

/// `transcript` is the session file, which keeps every message the summary replaces.
pub(super) fn compacted(messages: &[Value], compactions: &[Compaction], transcript: Option<&Path>) -> Vec<Value> {
    let Some(compaction) = compactions.last() else { return messages.to_vec() };
    let mut text = format!("{SUMMARY_PREFIX}{}{SUMMARY_SUFFIX}", compaction.summary);
    if let Some(path) = transcript {
        text.push_str(&format!("\n\nThe full conversation before this summary is in {}, one JSON entry per line. Search it with rg when the summary lacks a detail you need.", path.display()));
    }
    let mut kept = vec![json!({"role": "user", "content": [{"type": "text", "text": text}]})];
    kept.extend_from_slice(&messages[compaction.cut..]);
    kept
}

/// pi's serializeConversation: plain text, so the summarizer does not continue the conversation.
pub(super) fn serialize(messages: &[Value]) -> String {
    let mut parts = Vec::new();
    for message in messages {
        let blocks = message["content"].as_array().into_iter().flatten();
        if message["role"] == "assistant" {
            let text = text_of(message);
            if !text.is_empty() {
                parts.push(format!("[Assistant]: {text}"));
            }
            let calls: Vec<String> = blocks.filter(|b| b["type"] == "tool_use").map(|b| format!("{}({})", b["name"].as_str().unwrap_or_default(), call_args(&b["input"]))).collect();
            if !calls.is_empty() {
                parts.push(format!("[Assistant tool calls]: {}", calls.join("; ")));
            }
            continue;
        }
        for block in blocks {
            match block["type"].as_str() {
                Some("text") => parts.push(format!("[User]: {}", block["text"].as_str().unwrap_or_default())),
                Some("tool_result") => parts.push(format!("[Tool result]: {}", truncate_chars(block["content"].as_str().unwrap_or_default(), TOOL_RESULT_SUMMARY_CHARS))),
                _ => {}
            }
        }
    }
    parts.join("\n\n")
}

fn call_args(input: &Value) -> String {
    let fields = input.as_object().into_iter().flatten();
    fields.map(|(key, value)| format!("{key}={value}")).collect::<Vec<_>>().join(", ")
}

fn truncate_chars(text: &str, max: usize) -> String {
    let total = text.chars().count();
    if total <= max {
        return text.to_string();
    }
    let kept: String = text.chars().take(max).collect();
    format!("{kept}\n\n[... {} more characters truncated]", total - max)
}
