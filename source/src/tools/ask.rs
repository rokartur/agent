use serde_json::Value;

/// The row rpiv-ask-user-question appends to every question for a typed answer.
pub const TYPE_SOMETHING: &str = "Type something.";
pub const NO_UI: &str = "Error: UI not available (running in non-interactive mode)";
const DECLINED: &str = "User declined to answer questions";
const RESERVED: [&str; 3] = ["Other", TYPE_SOMETHING, "Next"];

#[derive(Clone, Debug)]
pub struct Question {
    pub question: String,
    pub header: String,
    pub options: Vec<Choice>,
    pub multi: bool,
}

#[derive(Clone, Debug)]
pub struct Choice {
    pub label: String,
    pub description: String,
    pub preview: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Answer {
    Picked(usize),
    Typed(String),
    Multi { picked: Vec<usize>, typed: String },
}

/// Reads and validates the tool input with rpiv-ask-user-question's rules and messages.
pub fn parse(input: &Value) -> Result<Vec<Question>, String> {
    let text = |value: &Value, key: &str| value[key].as_str().unwrap_or_default().to_string();
    let questions: Vec<Question> = input["questions"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|q| Question {
            question: text(q, "question"),
            header: text(q, "header"),
            multi: q["multiSelect"].as_bool() == Some(true),
            options: q["options"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|o| Choice { label: text(o, "label"), description: text(o, "description"), preview: o["preview"].as_str().map(String::from) })
                .collect(),
        })
        .collect();
    if questions.is_empty() {
        return Err("Error: At least one question is required".into());
    }
    if questions.len() > 4 {
        return Err("Error: At most 4 questions are allowed per invocation".into());
    }
    for (index, q) in questions.iter().enumerate() {
        if questions[..index].iter().any(|earlier| earlier.question == q.question) {
            return Err("Error: Question text must be unique within an invocation".into());
        }
    }
    for q in &questions {
        if q.question.is_empty() {
            return Err("Error: Each question requires question text".into());
        }
        if q.header.chars().count() > 12 {
            return Err("Error: header must be at most 12 characters".into());
        }
        if q.options.len() < 2 {
            return Err("Error: Each question requires at least 2 options".into());
        }
        if q.options.len() > 4 {
            return Err("Error: Each question allows at most 4 options".into());
        }
        for (index, option) in q.options.iter().enumerate() {
            if option.label.trim().is_empty() {
                return Err("Error: Each option requires a label".into());
            }
            if option.label.chars().count() > 60 {
                return Err("Error: Option label must be at most 60 characters".into());
            }
            if RESERVED.contains(&option.label.as_str()) {
                return Err(format!("Error: Option label is reserved ({})", RESERVED.join(", ")));
            }
            if q.options[..index].iter().any(|earlier| earlier.label == option.label) {
                return Err("Error: Option labels must be unique within a question".into());
            }
        }
    }
    Ok(questions)
}

/// rpiv's envelope: `"Question"="Answer".` per answered question; nothing answered means declined.
pub fn response(questions: &[Question], answers: &[Option<Answer>]) -> String {
    let mut segments = Vec::new();
    let mut unanswered = Vec::new();
    for (index, question) in questions.iter().enumerate() {
        let Some(Some(answer)) = answers.get(index) else {
            unanswered.push(format!("\"{}\"", question.question));
            continue;
        };
        let label = |i: &usize| question.options[*i].label.clone();
        let (value, preview) = match answer {
            Answer::Picked(i) => (label(i), question.options[*i].preview.clone()),
            Answer::Typed(text) => (text.clone(), None),
            Answer::Multi { picked, typed } => {
                let mut values: Vec<String> = picked.iter().map(label).collect();
                if !typed.is_empty() {
                    values.push(typed.clone());
                }
                (values.join(", "), None)
            }
        };
        let value = if value.is_empty() { "(no input)".to_string() } else { value };
        let mut segment = format!("\"{}\"=\"{value}\"", question.question);
        if let Some(preview) = preview.filter(|p| !p.is_empty()) {
            segment.push_str(&format!(". selected preview: {preview}"));
        }
        segments.push(format!("{segment}."));
    }
    if segments.is_empty() {
        return DECLINED.into();
    }
    if !unanswered.is_empty() {
        segments.push(format!("unanswered: {}.", unanswered.join(", ")));
    }
    format!("User has answered your questions: {} You can now continue with the user's answers in mind.", segments.join(" "))
}

/// Reads `response`'s envelope back as `header: answer` for the transcript row; None when declined.
pub fn answered(input: &Value, output: &str) -> Option<String> {
    let mut shown = Vec::new();
    for question in input["questions"].as_array()? {
        let key = format!("\"{}\"=\"", question["question"].as_str().unwrap_or_default());
        let Some((_, rest)) = output.split_once(&key) else { continue };
        let value = rest.split_once("\".").map_or(rest, |(value, _)| value);
        shown.push(format!("{}: {value}", question["header"].as_str().unwrap_or_default()));
    }
    (!shown.is_empty()).then(|| shown.join(" · "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn input(labels: [&str; 2]) -> Value {
        json!({"questions": [
            {"question": "Which db?", "header": "DB", "options": [{"label": labels[0], "description": ""}, {"label": labels[1], "description": "", "preview": "P"}]},
            {"question": "Which parts?", "header": "Parts", "multiSelect": true, "options": [{"label": "a", "description": ""}, {"label": "b", "description": ""}]},
        ]})
    }

    #[test]
    fn validation_and_envelope_match_rpiv() {
        assert_eq!(parse(&input(["x", "x"])).unwrap_err(), "Error: Option labels must be unique within a question");
        assert_eq!(parse(&input([" ", "x"])).unwrap_err(), "Error: Each option requires a label");
        assert_eq!(parse(&input(["Other", "x"])).unwrap_err(), "Error: Option label is reserved (Other, Type something., Next)");
        assert_eq!(parse(&input([&"ł".repeat(61), "x"])).unwrap_err(), "Error: Option label must be at most 60 characters");
        assert!(parse(&input([&"ł".repeat(60), "x"])).is_ok());
        let mut long_header = input(["pg", "sqlite"]);
        long_header["questions"][0]["header"] = "Thirteen char".into();
        assert_eq!(parse(&long_header).unwrap_err(), "Error: header must be at most 12 characters");
        let mut no_question = input(["pg", "sqlite"]);
        no_question["questions"][0].as_object_mut().unwrap().remove("question");
        assert_eq!(parse(&no_question).unwrap_err(), "Error: Each question requires question text");
        let questions = parse(&input(["pg", "sqlite"])).unwrap();
        assert_eq!(response(&questions, &[None, None]), DECLINED);
        let answers = [Some(Answer::Picked(1)), Some(Answer::Multi { picked: vec![0, 1], typed: "c".into() })];
        assert_eq!(
            response(&questions, &answers),
            "User has answered your questions: \"Which db?\"=\"sqlite\". selected preview: P. \"Which parts?\"=\"a, b, c\". You can now continue with the user's answers in mind."
        );
        assert_eq!(
            response(&questions, &[Some(Answer::Typed("mysql".into())), None]),
            "User has answered your questions: \"Which db?\"=\"mysql\". unanswered: \"Which parts?\". You can now continue with the user's answers in mind."
        );
        let both = response(&questions, &answers);
        assert_eq!(answered(&input(["pg", "sqlite"]), &both).unwrap(), "DB: sqlite · Parts: a, b, c");
        let one = response(&questions, &[Some(Answer::Typed("mysql".into())), None]);
        assert_eq!(answered(&input(["pg", "sqlite"]), &one).unwrap(), "DB: mysql");
        assert_eq!(answered(&input(["pg", "sqlite"]), DECLINED), None);
    }
}
