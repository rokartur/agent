use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use super::editor::Editor;
use super::markdown;
use super::render::{BRIGHT, Line, MUTED, PLAIN, Style, span, wrap};
use crate::api;

/// pi-btw's side-question prompt.
const SYSTEM: &str = "You answer quick side questions for a coding-agent user.

Use the provided conversation context only as background. Answer the user's side question directly and concisely. Do not claim to have changed files, run tools, or affected the main task. If the context is insufficient, say what is unknown and give the best next step.";
const MAX_CONTEXT_CHARS: usize = 40_000;
const ERROR: Style = Style::rgb(0xff7b72);
const VISIBLE_LINES: usize = 14;

/// A /btw side thread. It lives in memory until the process exits and never reaches the session.
pub struct Btw {
    /// The main conversation as text, frozen when the thread started.
    pub context: String,
    /// (question, answer) pairs; the last answer grows while it streams.
    pub turns: Vec<(String, String)>,
    pub input: Editor,
    pub abort: Arc<AtomicBool>,
    pub streaming: bool,
    pub error: Option<String>,
}

impl Btw {
    pub fn new(context: String) -> Btw {
        Btw { context, turns: Vec::new(), input: Editor::new(), abort: Arc::default(), streaming: false, error: None }
    }

    /// pi-btw's messages: the first question carries the context, later ones continue the thread.
    pub fn request(&self, model: &str, max_tokens: u64, thinking: &str) -> Value {
        let mut messages = Vec::new();
        for (index, (question, answer)) in self.turns.iter().enumerate() {
            let prompt = if index == 0 {
                let context = if self.context.is_empty() { "No prior conversation context was available." } else { &self.context };
                format!(
                    "Answer this side question without modifying the main conversation.\n\n<side_question>\n{question}\n</side_question>\n\n<conversation_context>\n{context}\n</conversation_context>"
                )
            } else {
                format!("Continue the same side conversation.\n\n<side_question>\n{question}\n</side_question>")
            };
            messages.push(json!({"role": "user", "content": [{"type": "text", "text": prompt}]}));
            if index + 1 < self.turns.len() {
                // A failed turn keeps an empty answer, and the API rejects an empty text block.
                let answer = if answer.trim().is_empty() { "(no answer)" } else { answer.as_str() };
                messages.push(json!({"role": "assistant", "content": [{"type": "text", "text": answer}]}));
            }
        }
        api::request(model, max_tokens, thinking, SYSTEM, "", &json!([]), &messages)
    }

    pub fn render(&self, width: usize) -> Vec<Line> {
        let mut body = Vec::new();
        for (question, answer) in &self.turns {
            body.extend(wrap(&[span(format!("> {question}"), BRIGHT.bold())], width, &vec![span("  ", PLAIN)]));
            body.extend(markdown::render(answer, width));
            body.push(Vec::new());
        }
        if let Some(error) = &self.error {
            body.extend(wrap(&[span(error.clone(), ERROR)], width, &Vec::new()));
        }
        let first = body.len().saturating_sub(VISIBLE_LINES);
        let mut lines = vec![Vec::new(), vec![span("btw", BRIGHT.bold()), span("  side question, not saved to the session", MUTED)]];
        lines.extend(body.into_iter().skip(first));
        let prompt = if self.streaming { span("  answering…", MUTED) } else { span(format!("> {}", self.input.field(None)), BRIGHT) };
        lines.extend([vec![prompt], vec![span("enter ask     tab insert answer into editor     esc close", MUTED)]]);
        lines
    }
}

/// pi-btw's `User: …` / `Assistant: …` context, cut from the start to its last MAX_CONTEXT_CHARS characters.
pub fn context(sections: &[String]) -> String {
    let text = sections.join("\n\n");
    let total = text.chars().count();
    if total <= MAX_CONTEXT_CHARS {
        return text;
    }
    let tail: String = text.chars().skip(total - MAX_CONTEXT_CHARS).collect();
    format!("[Earlier context omitted; showing the last {MAX_CONTEXT_CHARS} characters.]\n{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follow_ups_keep_the_first_context_and_earlier_answers() {
        let mut btw = Btw::new("User: hi".into());
        btw.turns = vec![("why?".into(), "because".into()), ("and?".into(), String::new())];
        let request = btw.request("m", 100, "off");
        let messages = request["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert!(messages[0]["content"][0]["text"].as_str().unwrap().contains("<side_question>\nwhy?\n</side_question>\n\n<conversation_context>\nUser: hi"));
        assert_eq!(messages[1]["content"][0]["text"], "because");
        assert!(messages[2]["content"][0]["text"].as_str().unwrap().starts_with("Continue the same side conversation."));
        assert_eq!(request["tools"], json!([]));
    }

    #[test]
    fn a_failed_answer_is_never_sent_as_an_empty_text_block() {
        let mut btw = Btw::new(String::new());
        btw.turns = vec![("why?".into(), String::new()), ("again?".into(), String::new())];
        assert_eq!(btw.request("m", 100, "off")["messages"][1]["content"][0]["text"], "(no answer)");
    }
}
