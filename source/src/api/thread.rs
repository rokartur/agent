use super::*;

/// The API drops a thread 5 minutes after it stored the last response; 10 s less covers that response's trip here.
pub(super) const THREAD_IDLE: Duration = Duration::from_secs(290);

/// A conversation the Anthropic API keeps between turns (the message threads beta), so a turn sends only what follows `held`.
#[derive(Default)]
pub struct MessageThread {
    /// The response a continue names, and when it arrived; None when the next turn creates a thread.
    pub(super) last: Option<(String, Instant)>,
    /// `key` of the account that created it: the API answers another account's continue with 404.
    account: String,
    /// The last request without its messages: a continue sends no system or tools, and the API refuses another model or thinking.
    fixed: Value,
    /// What the API holds: the messages sent so far, without the breakpoint `request` puts on the last one, then the reply.
    held: Vec<Value>,
    /// The thread comes from a replay after a lost one.
    pub(super) replayed: bool,
    /// The model whose turns go without a thread, as Claude Code keeps it per model: after `thread_unsupported_request`
    /// (a HIPAA organization, for one), a 404 right after a replay, or a create 400 the same turn without a thread passed.
    pub(super) unsupported_model: Value,
}

/// Continue errors that a create with the whole conversation fixes.
pub(super) const THREAD_LOST: [&str; 4] = ["thread_not_found", "thread_already_continued", "thread_continue_in_flight", "thread_fingerprint_mismatch"];

impl MessageThread {
    /// `request` from `account` as a continue: its new messages without system and tools, which a continue must not send.
    /// None when the API cannot hold its conversation: another account, other fixed fields, an edited history,
    /// a reply the thread never saw, or an expired thread.
    pub(super) fn continue_request(&self, account: &str, request: &Value) -> Option<Value> {
        let (last_id, stored_at) = self.last.as_ref()?;
        let messages = request["messages"].as_array()?;
        let held = self.held.len();
        // A reply after `held` came from a turn sent outside the thread, which the API does not hold.
        let appended = messages.len() > held && messages[..held] == self.held[..] && messages[held..].iter().all(|message| message["role"] == "user");
        let mut continued = without_messages(request);
        if stored_at.elapsed() >= THREAD_IDLE || self.account != account || !appended || continued != self.fixed {
            return None;
        }
        let fields = continued.as_object_mut().expect("a request is an object");
        fields.remove("system");
        fields.remove("tools");
        fields.insert("messages".into(), json!(messages[held..]));
        fields.insert("thread".into(), json!({"type": "continue", "previous_message_id": last_id}));
        Some(continued)
    }

    /// Keeps the turn the API stored; after a reply it may not hold, the next turn creates a thread.
    pub(super) fn record(&mut self, account: String, request: &Value, reply: &Reply) {
        // No continue after a refusal, a paused turn takes no new message, an aborted stream ends early,
        // and a tool input that did not parse is `{}` here but not in the API's copy.
        let stored = matches!(reply.stop_reason.as_str(), "end_turn" | "tool_use") && reply.bad_tool_inputs.is_empty() && !reply.id.is_empty();
        if !stored {
            self.last = None;
            return;
        }
        let mut held = request["messages"].as_array().cloned().unwrap_or_default();
        if let Some(block) = held.last_mut().and_then(|message| message.get_mut("content")?.as_array_mut()?.last_mut()?.as_object_mut()) {
            block.remove("cache_control");
        }
        held.push(reply.message.clone());
        self.last = Some((reply.id.clone(), Instant::now()));
        self.account = account;
        self.fixed = without_messages(request);
        self.held = held;
        self.replayed = false;
    }
}

fn without_messages(request: &Value) -> Value {
    let fields = request.as_object().expect("a request is an object");
    Value::Object(fields.iter().filter(|(name, _)| name.as_str() != "messages").map(|(name, value)| (name.clone(), value.clone())).collect())
}
