use serde_json::{Value, json};

/// A minimal pi-goal-x goal: the agent keeps prompting itself until the model reports it complete.
#[derive(Clone, Debug, PartialEq)]
pub struct Goal {
    pub objective: String,
    pub status: Status,
    pub token_budget: Option<u64>,
    pub tokens_used: u64,
    pub evidence: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Status {
    Active,
    Paused,
    Complete,
}

#[derive(Debug, PartialEq)]
pub enum AfterTurn {
    Continue(String),
    Paused(String),
    Idle,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Active => "active",
            Status::Paused => "paused",
            Status::Complete => "complete",
        }
    }
}

impl Goal {
    pub fn new(objective: &str, token_budget: Option<u64>) -> Goal {
        Goal { objective: objective.to_string(), status: Status::Active, token_budget, tokens_used: 0, evidence: None }
    }

    pub fn prompt(&self) -> String {
        format!("Continue working toward the goal: {} Call update_goal when done criteria are met.", self.objective)
    }

    /// What follows a finished turn; Esc and a spent budget pause the goal.
    pub fn after_turn(&mut self, aborted: bool) -> AfterTurn {
        if self.status != Status::Active {
            return AfterTurn::Idle;
        }
        if aborted {
            self.status = Status::Paused;
            return AfterTurn::Paused("Goal paused. Resume with /goal resume.".into());
        }
        if let Some(budget) = self.token_budget.filter(|budget| self.tokens_used >= *budget) {
            self.status = Status::Paused;
            return AfterTurn::Paused(format!("Goal paused: its token budget of {budget} is used up ({} tokens).", self.tokens_used));
        }
        AfterTurn::Continue(self.prompt())
    }

    pub fn summary(&self) -> String {
        let budget = self.token_budget.map_or(String::new(), |budget| format!(" of {budget}"));
        let mut text = format!("Goal ({}): {}\nTokens used: {}{budget}", self.status.as_str(), self.objective, self.tokens_used);
        if let Some(evidence) = &self.evidence {
            text.push_str(&format!("\nEvidence: {evidence}"));
        }
        text
    }

    pub fn entry(goal: Option<&Goal>) -> Value {
        let goal = goal.map(|g| json!({"objective": g.objective, "status": g.status.as_str(), "token_budget": g.token_budget, "tokens_used": g.tokens_used, "evidence": g.evidence}));
        json!({"type": "goal", "goal": goal})
    }

    pub fn from_entry(entry: &Value) -> Option<Goal> {
        // A `Value`, not its `Map`: indexing a Map panics on a key an older session lacks.
        let goal = entry.get("goal").filter(|goal| goal.is_object())?;
        let status = match goal["status"].as_str() {
            Some("paused") => Status::Paused,
            Some("complete") => Status::Complete,
            _ => Status::Active,
        };
        Some(Goal {
            objective: goal["objective"].as_str().unwrap_or_default().to_string(),
            status,
            token_budget: goal["token_budget"].as_u64(),
            tokens_used: goal["tokens_used"].as_u64().unwrap_or_default(),
            evidence: goal["evidence"].as_str().map(String::from),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn continues_until_paused_by_esc_or_budget() {
        let mut goal = Goal::new("ship it.", Some(100));
        assert_eq!(goal.after_turn(false), AfterTurn::Continue("Continue working toward the goal: ship it. Call update_goal when done criteria are met.".into()));
        assert_eq!(goal.after_turn(true), AfterTurn::Paused("Goal paused. Resume with /goal resume.".into()));
        assert_eq!(goal.after_turn(false), AfterTurn::Idle);
        goal.status = Status::Active;
        goal.tokens_used = 120;
        assert_eq!(goal.after_turn(false), AfterTurn::Paused("Goal paused: its token budget of 100 is used up (120 tokens).".into()));
        assert_eq!(Goal::from_entry(&Goal::entry(Some(&goal))), Some(goal));
        assert_eq!(Goal::from_entry(&Goal::entry(None)), None);
        assert_eq!(Goal::from_entry(&json!({"type": "goal", "goal": {"objective": "old", "status": "paused"}})).unwrap().evidence, None);
    }
}
