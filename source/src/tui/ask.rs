use crossterm::event::{KeyCode, KeyEvent};
use serde_json::Value;
use std::sync::mpsc::Sender;

use super::OK;
use super::editor::Editor;
use super::render::{BRIGHT, Line, MUTED, PLAIN, Style, span, wrap};
use crate::tools::ask::{Answer, Question, TYPE_SOMETHING};

const PREVIEW_LINES: usize = 8;

/// The ask_user_question panel: one question at a time, the agent thread waits on `reply`.
pub struct AskPanel {
    questions: Vec<Question>,
    reply: Sender<(Vec<Option<Answer>>, Vec<Value>)>,
    answers: Vec<Option<Answer>>,
    current: usize,
    drafts: Vec<Draft>,
    /// Images pasted into any question's typed answer; `[Image #n]` refers to `images[n - 1]`.
    images: Vec<Value>,
}

/// Per-question input, kept while moving between questions.
struct Draft {
    /// Focused row; `options.len()` is the "Type something." row.
    row: usize,
    picked: Vec<usize>,
    typed: Editor,
}

impl AskPanel {
    pub fn new(questions: Vec<Question>, reply: Sender<(Vec<Option<Answer>>, Vec<Value>)>) -> AskPanel {
        let drafts = questions.iter().map(|_| Draft { row: 0, picked: Vec::new(), typed: Editor::new() }).collect();
        AskPanel { answers: vec![None; questions.len()], questions, reply, current: 0, drafts, images: Vec::new() }
    }

    pub fn first_question(&self) -> &str {
        self.questions.first().map_or("", |question| question.question.as_str())
    }

    /// Pasted text can only be a typed answer, so it moves to the "Type something." row.
    pub fn paste(&mut self, text: &str) {
        let draft = &mut self.drafts[self.current];
        draft.row = self.questions[self.current].options.len();
        draft.typed.insert(text);
    }

    pub fn attach(&mut self, image: Value) {
        self.images.push(image);
        self.paste(&format!("[Image #{}] ", self.images.len()));
    }

    /// Returns true once the answers (or the cancel) went back to the agent.
    pub fn key(&mut self, key: KeyEvent) -> bool {
        let question = &self.questions[self.current];
        let draft = &mut self.drafts[self.current];
        let custom_row = question.options.len();
        let editing = draft.row == custom_row && !draft.typed.is_empty();
        match key.code {
            KeyCode::Esc => {
                let _ = self.reply.send((vec![None; self.questions.len()], Vec::new()));
                return true;
            }
            KeyCode::Left if !editing => self.current = self.current.saturating_sub(1),
            KeyCode::Right if !editing => self.current = (self.current + 1).min(self.questions.len() - 1),
            KeyCode::Up => draft.row = draft.row.saturating_sub(1),
            KeyCode::Down => draft.row = (draft.row + 1).min(custom_row),
            KeyCode::Char(' ') if question.multi && draft.row < custom_row => match draft.picked.iter().position(|i| *i == draft.row) {
                Some(at) => _ = draft.picked.remove(at),
                None => draft.picked.push(draft.row),
            },
            KeyCode::Enter => {
                let answer = match (question.multi, draft.row == custom_row) {
                    (true, _) if draft.picked.is_empty() && draft.typed.is_empty() => return false,
                    (true, _) => {
                        draft.picked.sort();
                        Answer::Multi { picked: draft.picked.clone(), typed: draft.typed.text() }
                    }
                    (false, true) if draft.typed.is_empty() => return false,
                    (false, true) => Answer::Typed(draft.typed.text()),
                    (false, false) => Answer::Picked(draft.row),
                };
                self.answers[self.current] = Some(answer);
                let count = self.questions.len();
                match (1..=count).map(|step| (self.current + step) % count).find(|&i| self.answers[i].is_none()) {
                    Some(unanswered) => self.current = unanswered,
                    None => {
                        let images = referenced_images(std::mem::take(&mut self.images), &self.answers);
                        let _ = self.reply.send((std::mem::take(&mut self.answers), images));
                        return true;
                    }
                }
            }
            _ if draft.row == custom_row => _ = draft.typed.edit_key(key),
            _ => {}
        }
        false
    }

    pub fn render(&self, width: usize) -> Vec<Line> {
        let question = &self.questions[self.current];
        let draft = &self.drafts[self.current];
        let mut lines = vec![Vec::new(), self.tabs()];
        lines.extend(wrap(&[span(question.question.clone(), PLAIN.bold())], width, &Vec::new()));
        lines.push(Vec::new());
        // As wide as the widest `lead`, so wrapped text lines up under the labels.
        let indent = vec![span(" ".repeat(4 + format!("{}. ", question.options.len() + 1).len()), PLAIN)];
        for (index, option) in question.options.iter().enumerate() {
            let check = match (question.multi, draft.picked.contains(&index)) {
                (false, _) => "",
                (true, true) => "[x] ",
                (true, false) => "[ ] ",
            };
            let mut line = lead(draft.row, index);
            line.push(span(format!("{check}{}", option.label), style(draft.row, index)));
            lines.push(line);
            if !option.description.is_empty() {
                lines.extend(wrap(&[span(option.description.as_str(), MUTED)], width, &indent));
            }
        }
        let custom = question.options.len();
        let mut line = lead(draft.row, custom);
        line.push(span(TYPE_SOMETHING, style(draft.row, custom)));
        lines.push(line);
        if draft.row == custom || !draft.typed.is_empty() {
            lines.extend(wrap(&[span(draft.typed.field(None), BRIGHT)], width, &indent));
        }
        if let Some(preview) = question.options.get(draft.row).and_then(|o| o.preview.as_ref()) {
            lines.push(Vec::new());
            lines.extend(preview.lines().take(PREVIEW_LINES).map(|l| vec![span(format!("    {l}"), MUTED)]));
        }
        let switch = if self.questions.len() > 1 { "←→ question     " } else { "" };
        let action = if question.multi { "space toggle     enter submit" } else { "enter choose" };
        lines.push(vec![span(format!("↑↓ move     {switch}{action}     esc cancel"), MUTED)]);
        lines
    }

    /// One tab per question header: the current one bright, answered ones checked.
    fn tabs(&self) -> Line {
        let mut line = Vec::new();
        for (index, question) in self.questions.iter().enumerate() {
            if index > 0 {
                line.push(span("   ", PLAIN));
            }
            let mark = if self.answers[index].is_some() { "✓ " } else { "" };
            let tab = format!("{mark}{}", question.header);
            line.push(if index == self.current { span(format!("[{tab}]"), BRIGHT.bold()) } else { span(tab, MUTED) });
        }
        line
    }
}

/// The images whose `[Image #n]` is still in a typed answer, without the dropped file's path the API rejects.
fn referenced_images(images: Vec<Value>, answers: &[Option<Answer>]) -> Vec<Value> {
    let typed: Vec<&str> = answers
        .iter()
        .flatten()
        .filter_map(|answer| match answer {
            Answer::Typed(text) | Answer::Multi { typed: text, .. } => Some(text.as_str()),
            Answer::Picked(_) => None,
        })
        .collect();
    let mut kept = Vec::new();
    for (index, mut image) in images.into_iter().enumerate() {
        let tag = format!("[Image #{}]", index + 1);
        if typed.iter().any(|text| text.contains(&tag)) {
            image.as_object_mut().unwrap().remove(super::DROPPED_PATH);
            kept.push(image);
        }
    }
    kept
}

fn lead(focused: usize, row: usize) -> Line {
    vec![span("  ", PLAIN), span(if row == focused { "→ " } else { "  " }, OK), span(format!("{}. ", row + 1), MUTED)]
}

fn style(focused: usize, row: usize) -> Style {
    if row == focused { BRIGHT.bold() } else { PLAIN }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::mpsc;

    #[test]
    fn a_long_typed_answer_wraps_inside_the_panel() {
        let input = json!({"questions": [{"question": "Q?", "header": "H", "options": [{"label": "a", "description": ""}, {"label": "b", "description": ""}]}]});
        let (reply, _answers) = mpsc::channel();
        let mut panel = AskPanel::new(crate::tools::ask::parse(&input).unwrap(), reply);
        panel.paste("one two three four five six seven eight nine ten");
        let text: Vec<String> = panel.render(24).iter().map(|l| l.iter().map(|s| s.text.as_str()).collect()).collect();
        let answer: Vec<&String> = text.iter().filter(|row| ["one", "five", "ten"].iter().any(|word| row.contains(word))).collect();
        assert!(answer.len() > 1 && answer.iter().all(|row| super::super::render::width(row) <= 24), "{text:#?}");
    }

    #[test]
    fn a_long_description_wraps_below_its_label() {
        let input = json!({"questions": [{"question": "Q?", "header": "H", "options": [{"label": "a", "description": "one two three four five six seven eight nine ten"}, {"label": "b", "description": ""}]}]});
        let (reply, _answers) = mpsc::channel();
        let panel = AskPanel::new(crate::tools::ask::parse(&input).unwrap(), reply);
        let text: Vec<String> = panel.render(24).iter().map(|l| l.iter().map(|s| s.text.as_str()).collect()).collect();
        let label = text.iter().position(|row| row.ends_with("1. a")).expect("label on its own row");
        assert!(text[label + 1].trim_start().starts_with("one"), "{text:#?}");
        assert_eq!(text[label + 4], "       ten", "{text:#?}");
    }

    #[test]
    fn answers_each_question_in_turn_then_replies() {
        let input = json!({"questions": [
            {"question": "Q1?", "header": "One", "options": [{"label": "a", "description": ""}, {"label": "b", "description": ""}]},
            {"question": "Q2?", "header": "Two", "multiSelect": true, "options": [{"label": "c", "description": ""}, {"label": "d", "description": ""}]},
        ]});
        let (reply, answers) = mpsc::channel();
        let mut panel = AskPanel::new(crate::tools::ask::parse(&input).unwrap(), reply);
        let keys = [KeyCode::Down, KeyCode::Down, KeyCode::Char('x'), KeyCode::Enter, KeyCode::Down, KeyCode::Char(' '), KeyCode::Up, KeyCode::Char(' ')];
        assert!(keys.into_iter().all(|code| !panel.key(code.into())));
        assert!(panel.key(KeyCode::Enter.into()));
        assert_eq!(answers.recv().unwrap().0, [Some(Answer::Typed("x".into())), Some(Answer::Multi { picked: vec![0, 1], typed: String::new() })]);
    }

    #[test]
    fn a_pasted_image_goes_back_with_the_answer_that_still_names_it() {
        let input = json!({"questions": [{"question": "Q?", "header": "H", "options": [{"label": "a", "description": ""}, {"label": "b", "description": ""}]}]});
        let (reply, answers) = mpsc::channel();
        let mut panel = AskPanel::new(crate::tools::ask::parse(&input).unwrap(), reply);
        panel.attach(json!({"type": "image", "n": 1, super::super::DROPPED_PATH: "/a.png"}));
        panel.attach(json!({"type": "image", "n": 2}));
        // The space, then the whole `[Image #2]` tag.
        panel.key(KeyCode::Backspace.into());
        panel.key(KeyCode::Backspace.into());
        assert!(panel.key(KeyCode::Enter.into()));
        let (answers, images) = answers.recv().unwrap();
        assert_eq!(answers, [Some(Answer::Typed("[Image #1] ".into()))]);
        assert_eq!(images, [json!({"type": "image", "n": 1})]);
    }

    #[test]
    fn going_back_keeps_and_changes_an_answer() {
        let input = json!({"questions": [
            {"question": "Q1?", "header": "One", "options": [{"label": "a", "description": ""}, {"label": "b", "description": ""}]},
            {"question": "Q2?", "header": "Two", "multiSelect": true, "options": [{"label": "c", "description": ""}, {"label": "d", "description": ""}]},
        ]});
        let (reply, answers) = mpsc::channel();
        let mut panel = AskPanel::new(crate::tools::ask::parse(&input).unwrap(), reply);
        let keys = [KeyCode::Enter, KeyCode::Char(' '), KeyCode::Left, KeyCode::Down, KeyCode::Enter];
        assert!(keys.into_iter().all(|code| !panel.key(code.into())));
        assert!(panel.key(KeyCode::Enter.into()));
        assert_eq!(answers.recv().unwrap().0, [Some(Answer::Picked(1)), Some(Answer::Multi { picked: vec![0], typed: String::new() })]);
    }

    #[test]
    fn tabs_name_every_question_and_mark_the_answered() {
        let input = json!({"questions": [
            {"question": "Q1?", "header": "One", "options": [{"label": "a", "description": ""}, {"label": "b", "description": ""}]},
            {"question": "Q2?", "header": "Two", "options": [{"label": "c", "description": ""}, {"label": "d", "description": ""}]},
        ]});
        let (reply, _answers) = mpsc::channel();
        let mut panel = AskPanel::new(crate::tools::ask::parse(&input).unwrap(), reply);
        panel.key(KeyCode::Enter.into());
        let text: Vec<String> = panel.render(40).iter().map(|l| l.iter().map(|s| s.text.as_str()).collect()).collect();
        assert_eq!(text[1], "✓ One   [Two]", "{text:#?}");
    }
}
