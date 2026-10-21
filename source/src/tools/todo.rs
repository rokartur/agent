use serde_json::{Value, json};

/// rpiv-todo's task list: the model edits it with the `todo` tool, the user with /todos.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Todos {
    pub tasks: Vec<Task>,
    next_id: u64,
    /// Changes the user made in /todos, told to the model before the next prompt.
    pub user_notes: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Task {
    pub id: u64,
    pub subject: String,
    pub description: Option<String>,
    pub active_form: Option<String>,
    pub status: Status,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Status {
    Pending,
    InProgress,
    Completed,
    Deleted,
}

impl Status {
    fn parse(text: &str) -> Option<Status> {
        match text {
            "pending" => Some(Status::Pending),
            "in_progress" => Some(Status::InProgress),
            "completed" => Some(Status::Completed),
            "deleted" => Some(Status::Deleted),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Status::Pending => "pending",
            Status::InProgress => "in_progress",
            Status::Completed => "completed",
            Status::Deleted => "deleted",
        }
    }

    /// rpiv-todo's invariants: completed only goes to deleted, deleted is terminal.
    fn can_become(self, to: Status) -> bool {
        self == to
            || match self {
                Status::Pending | Status::InProgress => true,
                Status::Completed => to == Status::Deleted,
                Status::Deleted => false,
            }
    }
}

impl Todos {
    /// Runs one `todo` tool call; the bool is true when the list changed and must be saved.
    pub fn apply(&mut self, input: &Value) -> Result<(String, bool), String> {
        let id = input["taskId"].as_u64();
        let status = match input["status"].as_str() {
            Some(text) => Some(Status::parse(text).ok_or(format!("Error: unknown status {text}"))?),
            None => None,
        };
        let text = |key: &str| input[key].as_str().map(String::from);
        match input["action"].as_str().unwrap_or_default() {
            "create" => {
                let subject = text("subject").filter(|s| !s.trim().is_empty()).ok_or("Error: subject required for create")?;
                let id = self.next_id.max(1);
                self.next_id = id + 1;
                self.tasks.push(Task {
                    id,
                    subject: subject.clone(),
                    description: text("description").filter(|s| !s.is_empty()),
                    active_form: text("activeForm").filter(|s| !s.is_empty()),
                    status: Status::Pending,
                });
                Ok((format!("Created #{id}: {subject} (pending)"), true))
            }
            "update" => {
                let id = id.ok_or("Error: taskId required for update")?;
                let index = self.index_of(id)?;
                let fields = ["subject", "description", "activeForm", "status"];
                if fields.iter().all(|key| input.get(key).is_none_or(Value::is_null)) {
                    return Err("Error: update requires at least one mutable field: subject, description, activeForm, or status".into());
                }
                let before = self.tasks[index].clone();
                let mut task = before.clone();
                if let Some(to) = status {
                    if !before.status.can_become(to) {
                        return Err(format!("Error: illegal transition {} → {}", before.status.as_str(), to.as_str()));
                    }
                    task.status = to;
                }
                task.subject = text("subject").unwrap_or(task.subject);
                task.description = text("description").or(task.description);
                task.active_form = text("activeForm").or(task.active_form);
                if task == before {
                    return Ok((format!("No change: #{id} already matches the requested values (status: {})", task.status.as_str()), false));
                }
                let transition = if before.status != task.status { format!(" ({} → {})", before.status.as_str(), task.status.as_str()) } else { String::new() };
                self.tasks[index] = task;
                Ok((format!("Updated #{id}{transition}"), true))
            }
            "list" => {
                let lines: Vec<String> = self.tasks.iter().filter(|t| t.status != Status::Deleted).filter(|t| status.is_none_or(|s| t.status == s)).map(list_line).collect();
                Ok((if lines.is_empty() { "No tasks".into() } else { lines.join("\n") }, false))
            }
            "get" => {
                let task = &self.tasks[self.index_of(id.ok_or("Error: taskId required for get")?)?];
                Ok((Self::details(task), false))
            }
            "delete" => {
                let index = self.index_of(id.ok_or("Error: taskId required for delete")?)?;
                let task = &mut self.tasks[index];
                if task.status == Status::Deleted {
                    return Err(format!("Error: #{} is already deleted", task.id));
                }
                task.status = Status::Deleted;
                Ok((format!("Deleted #{}: {}", task.id, task.subject), true))
            }
            "clear" => {
                let count = self.tasks.len();
                self.tasks.clear();
                self.next_id = 1;
                Ok((format!("Cleared {count} tasks"), true))
            }
            other => Err(format!("Error: unknown action {other}")),
        }
    }

    /// The user's /todos edit: cycles pending → in progress → completed → pending, ignoring the model's transition rules.
    pub fn cycle(&mut self, index: usize) {
        let task = &mut self.tasks[index];
        task.status = match task.status {
            Status::Pending => Status::InProgress,
            Status::InProgress => Status::Completed,
            Status::Completed | Status::Deleted => Status::Pending,
        };
        self.user_notes.push(format!("The user set todo #{} \"{}\" to {}.", task.id, task.subject, task.status.as_str()));
    }

    pub fn delete(&mut self, index: usize) {
        let task = &mut self.tasks[index];
        task.status = Status::Deleted;
        self.user_notes.push(format!("The user deleted todo #{} \"{}\".", task.id, task.subject));
    }

    /// Tasks not deleted, in creation order.
    pub fn visible(&self) -> impl Iterator<Item = &Task> {
        self.tasks.iter().filter(|t| t.status != Status::Deleted)
    }

    /// The widget shows while some visible task is still open.
    pub fn has_open(&self) -> bool {
        self.visible().any(|t| t.status != Status::Completed)
    }

    pub fn entry(&self) -> Value {
        let tasks: Vec<Value> = self.tasks.iter().map(|t| json!({"id": t.id, "subject": t.subject, "description": t.description, "activeForm": t.active_form, "status": t.status.as_str()})).collect();
        json!({"type": "todo", "tasks": tasks, "next_id": self.next_id})
    }

    pub fn from_entry(entry: &Value) -> Todos {
        let text = |task: &Value, key: &str| task[key].as_str().map(String::from);
        let tasks = entry["tasks"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|t| Task {
                id: t["id"].as_u64().unwrap_or_default(),
                subject: text(t, "subject").unwrap_or_default(),
                description: text(t, "description"),
                active_form: text(t, "activeForm"),
                status: t["status"].as_str().and_then(Status::parse).unwrap_or(Status::Pending),
            })
            .collect();
        Todos { tasks, next_id: entry["next_id"].as_u64().unwrap_or(1), user_notes: Vec::new() }
    }

    fn index_of(&self, id: u64) -> Result<usize, String> {
        self.tasks.iter().position(|t| t.id == id).ok_or(format!("Error: #{id} not found"))
    }

    fn details(task: &Task) -> String {
        let mut lines = vec![format!("#{} [{}] {}", task.id, task.status.as_str(), task.subject)];
        if let Some(description) = &task.description {
            lines.push(format!("  description: {description}"));
        }
        if let Some(form) = &task.active_form {
            lines.push(format!("  activeForm: {form}"));
        }
        lines.join("\n")
    }
}

fn list_line(task: &Task) -> String {
    let form = match (&task.active_form, task.status) {
        (Some(form), Status::InProgress) => format!(" ({form})"),
        _ => String::new(),
    };
    format!("[{}] #{} {}{form}", task.status.as_str(), task.id, task.subject)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(todos: &mut Todos, input: Value) -> String {
        match todos.apply(&input) {
            Ok((text, _)) | Err(text) => text,
        }
    }

    #[test]
    fn transitions_follow_rpiv_todo_rules() {
        let mut todos = Todos::default();
        assert_eq!(run(&mut todos, json!({"action": "create", "subject": "a", "activeForm": "doing a"})), "Created #1: a (pending)");
        run(&mut todos, json!({"action": "create", "subject": "b"}));
        assert_eq!(run(&mut todos, json!({"action": "update", "taskId": 1, "status": "in_progress"})), "Updated #1 (pending → in_progress)");
        assert_eq!(run(&mut todos, json!({"action": "update", "taskId": 1, "status": "in_progress"})), "No change: #1 already matches the requested values (status: in_progress)");
        assert_eq!(run(&mut todos, json!({"action": "list"})), "[in_progress] #1 a (doing a)\n[pending] #2 b");
        run(&mut todos, json!({"action": "update", "taskId": 1, "status": "completed"}));
        assert_eq!(run(&mut todos, json!({"action": "update", "taskId": 1, "status": "pending"})), "Error: illegal transition completed → pending");
        assert_eq!(run(&mut todos, json!({"action": "delete", "taskId": 2})), "Deleted #2: b");
        assert_eq!(run(&mut todos, json!({"action": "list"})), "[completed] #1 a");
        assert!(!todos.has_open());
        assert_eq!(Todos::from_entry(&todos.entry()), todos);
    }
}
