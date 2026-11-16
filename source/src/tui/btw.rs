use crossterm::event::{KeyCode, KeyEvent};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use super::editor::Editor;
use super::markdown;
use super::render::{BRIGHT, Line, MUTED, PLAIN, span, wrap};
use super::transcript::prompt_rows;
use super::view::band;
use super::{App, ERROR, Item, MODELS_STILL_LOADING, Msg, Panel};
use crate::api::{self, Delta};

/// pi-btw's side-question prompt.
const SYSTEM: &str = "You answer quick side questions for a coding-agent user.

Use the provided conversation context only as background. Answer the user's side question directly and concisely. Do not claim to have changed files, run tools, or affected the main task. If the context is insufficient, say what is unknown and give the best next step.";
const MAX_CONTEXT_CHARS: usize = 40_000;
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
    /// The first body line shown; None follows the newest text. A line index stays put while an answer streams.
    pub top: Option<usize>,
}

impl Btw {
    pub fn new(context: String) -> Btw {
        Btw { context, turns: Vec::new(), input: Editor::new(), abort: Arc::default(), streaming: false, error: None, top: None }
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
        api::request(model, max_tokens, thinking, "5m", SYSTEM, "", &json!([]), &messages)
    }

    pub fn render(&self, width: usize) -> Vec<Line> {
        let body = self.body(width);
        let first = first_line(self.top, body.len());
        let end = (first + VISIBLE_LINES).min(body.len());
        let below = body.len() - end;
        let mut title = vec![span("btw", BRIGHT.bold()), span("  side question, not saved to the session", MUTED)];
        if first > 0 {
            title.push(span(format!("  ↑ {first} more"), MUTED));
        }
        let mut lines = vec![Vec::new(), title];
        lines.extend(body.into_iter().take(end).skip(first));
        if below > 0 {
            lines.push(vec![span(format!(" ↓ {below} more"), MUTED)]);
        }
        let prompt = if self.streaming { span("  answering…", MUTED) } else { span(format!("❯ {}", self.input.field(None)), BRIGHT) };
        let mut hint = if self.streaming { "esc hide, keeps answering     ctrl+c stop" } else { "enter ask     esc close" }.to_string();
        if first > 0 || below > 0 {
            hint.push_str("     ↑↓ pgup pgdn scroll");
        }
        if !self.streaming && self.turns.last().is_some_and(|(_, answer)| !answer.is_empty()) {
            hint.push_str("     tab insert answer into editor");
        }
        lines.extend([vec![prompt], vec![span(hint, MUTED)]]);
        lines
    }

    fn body(&self, width: usize) -> Vec<Line> {
        let mut body = Vec::new();
        // Styled like the transcript: a prompt band for the question, a one-column margin for the answer.
        for (question, answer) in &self.turns {
            body.extend(prompt_rows(question, BRIGHT, width).into_iter().map(|row| band(row, width)));
            for line in markdown::render(answer, width.saturating_sub(1)) {
                let mut indented = vec![span(" ", PLAIN)];
                indented.extend(line);
                body.push(indented);
            }
            body.push(Vec::new());
        }
        if let Some(error) = &self.error {
            body.extend(wrap(&[span(error.clone(), ERROR)], width, &Vec::new()));
        }
        body
    }

    /// Moves the view `rows` down (negative: up); reaching the newest text follows it again.
    pub fn scroll_by(&mut self, rows: isize, width: usize) {
        let len = self.body(width).len();
        let top = first_line(self.top, len).saturating_add_signed(rows);
        self.top = (top < len.saturating_sub(VISIBLE_LINES)).then_some(top);
    }
}

fn first_line(top: Option<usize>, len: usize) -> usize {
    let last_page = len.saturating_sub(VISIBLE_LINES);
    top.map_or(last_page, |top| top.min(last_page))
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

impl App {
    /// `/btw question` starts a new side thread on the current transcript; a bare `/btw` reopens the last one.
    pub(super) fn open_btw(&mut self, question: &str) {
        if self.btw.as_ref().is_some_and(|btw| btw.streaming) {
            self.panel = Some(Panel::Btw);
            self.hint = Some(("btw is still answering".into(), Instant::now()));
            return;
        }
        if question.is_empty() && self.btw.is_some() {
            self.panel = Some(Panel::Btw);
            return;
        }
        self.btw = Some(Btw::new(self.btw_context()));
        self.panel = Some(Panel::Btw);
        if !question.is_empty() {
            self.ask_btw(question.to_string());
        }
    }

    /// Esc only hides the panel, so an answer can stream while the main editor is in use; Ctrl+C stops it.
    pub(super) fn btw_key(&mut self, key: KeyEvent, ctrl_c: bool) {
        let btw = self.btw.as_mut().expect("the btw panel opens with a thread");
        match key.code {
            KeyCode::Esc if ctrl_c && btw.streaming => btw.abort.store(true, Ordering::Relaxed),
            KeyCode::Esc => self.panel = None,
            KeyCode::Up => btw.scroll_by(-1, self.layout.width),
            KeyCode::Down => btw.scroll_by(1, self.layout.width),
            KeyCode::PageUp => btw.scroll_by(-(VISIBLE_LINES as isize), self.layout.width),
            KeyCode::PageDown => btw.scroll_by(VISIBLE_LINES as isize, self.layout.width),
            KeyCode::Tab => {
                let Some((_, answer)) = btw.turns.last().filter(|(_, answer)| !answer.is_empty()) else { return };
                let answer = answer.clone();
                self.editor.insert(&answer);
                self.panel = None;
            }
            KeyCode::Enter if !btw.streaming && !btw.input.text().trim().is_empty() => {
                // Checked before the question leaves the input, so it is not lost.
                if self.model().is_none() {
                    self.system("btw", MODELS_STILL_LOADING);
                    return;
                }
                let question = self.btw.as_mut().expect("key went to an open btw thread").input.take();
                self.ask_btw(question.trim().to_string());
            }
            _ => _ = btw.input.edit_key(key),
        }
    }

    /// Streams the answer on its own thread with the current model and thinking level, no tools.
    fn ask_btw(&mut self, question: String) {
        let Some(max_tokens) = self.model().map(|m| m.max_tokens) else {
            self.system("btw", MODELS_STILL_LOADING);
            return;
        };
        let (model, thinking) = (self.settings.model.clone(), self.settings.thinking.clone());
        let btw = self.btw.as_mut().expect("asked from an open btw thread");
        btw.turns.push((question, String::new()));
        btw.streaming = true;
        btw.error = None;
        btw.top = None;
        btw.abort.store(false, Ordering::Relaxed);
        let request = btw.request(&model, max_tokens, &thinking);
        let (client, abort, events) = (self.client.clone(), btw.abort.clone(), self.events.clone());
        std::thread::spawn(move || {
            let result = client.send(&request, &abort, &mut |delta| {
                if let Delta::Text(text) = delta {
                    let _ = events.send(Msg::BtwDelta(text.to_string()));
                }
            });
            let _ = events.send(Msg::BtwDone(result.map(|_| ())));
        });
    }

    /// pi-btw's context: user and assistant text plus the assistant's tool calls, without thinking or tool results.
    fn btw_context(&self) -> String {
        let mut sections: Vec<String> = Vec::new();
        let mut assistant_open = false;
        for entry in &self.entries[self.header_len..] {
            match &entry.item {
                Item::User(text) => {
                    sections.push(format!("User: {}", text.trim()));
                    assistant_open = false;
                }
                Item::Assistant { text, .. } if !text.trim().is_empty() => {
                    sections.push(format!("Assistant: {}", text.trim()));
                    assistant_open = true;
                }
                Item::Tool { name, input, .. } => {
                    let call = format!("Tool call: {name}({input})");
                    match sections.last_mut() {
                        Some(last) if assistant_open => *last = format!("{last}\n{call}"),
                        _ => sections.push(format!("Assistant: {call}")),
                    }
                    assistant_open = true;
                }
                _ => {}
            }
        }
        context(&sections)
    }
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
    fn a_scrolled_view_stays_put_while_the_answer_grows_and_follows_again_at_the_end() {
        let mut btw = Btw::new(String::new());
        btw.turns = vec![("q".into(), (0..30).map(|n| format!("line{n}\n\n")).collect())];
        btw.scroll_by(-100, 40);
        assert_eq!(btw.top, Some(0));
        btw.turns[0].1.push_str("more\n\nmore\n\n");
        assert_eq!(btw.render(40)[2].iter().map(|s| s.text.as_str()).collect::<String>().trim(), "❯ q");
        btw.scroll_by(1000, 40);
        assert_eq!(btw.top, None);
    }

    #[test]
    fn a_failed_answer_is_never_sent_as_an_empty_text_block() {
        let mut btw = Btw::new(String::new());
        btw.turns = vec![("why?".into(), String::new()), ("again?".into(), String::new())];
        assert_eq!(btw.request("m", 100, "off")["messages"][1]["content"][0]["text"], "(no answer)");
    }
}
