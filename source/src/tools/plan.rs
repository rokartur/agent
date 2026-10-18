use serde_json::{Value, json};

#[derive(Clone, Debug, Default, PartialEq)]
pub enum Plan {
    #[default]
    Off,
    Drafting,
    Ready(String),
}

impl Plan {
    pub fn entry(&self) -> Value {
        match self {
            Plan::Off => json!({"type": "plan", "mode": "off"}),
            Plan::Drafting => json!({"type": "plan", "mode": "drafting"}),
            Plan::Ready(plan) => json!({"type": "plan", "mode": "ready", "plan": plan}),
        }
    }

    pub fn from_entry(entry: &Value) -> Plan {
        match (entry["mode"].as_str(), entry["plan"].as_str()) {
            (Some("drafting"), _) => Plan::Drafting,
            (Some("ready"), Some(plan)) => Plan::Ready(plan.to_string()),
            _ => Plan::Off,
        }
    }
}

pub const PROMPT: &str = "<plan_mode>
Plan mode is on. Investigate and design, change nothing. Edits, subagents, plugin tools and state-changing commands are blocked; bash and bg_start run only read-only commands (ls, cat, rg, fd, git status/log/diff/show, ...). Ask with ask_user_question when a decision cannot be inferred. When the plan is ready, call plan_mode_complete with the whole plan in Markdown as your last action. The user then picks: implement here, implement in a fresh session, export, or keep planning.
</plan_mode>";

pub const IMPLEMENT: &str = "Implement the plan.";

const EDITING_TOOLS: [&str; 8] = ["write", "replace", "replace_match", "insert", "copy", "move", "undo_last_change", "subagent"];

const READ_ONLY_COMMANDS: [&str; 18] = ["ls", "cat", "head", "tail", "wc", "rg", "fd", "pwd", "tree", "file", "stat", "which", "echo", "jq", "diff", "du", "cd", "true"];
const READ_ONLY_GIT: [&str; 7] = ["status", "log", "diff", "show", "blame", "ls-files", "rev-parse"];
// Matched as prefixes, so attached values (`--pre=sh`) count too; `--output` writes a file.
const RUNS_OTHER_COMMANDS: [&str; 3] = ["--exec", "--pre", "--output"];

pub fn tool() -> Value {
    json!({
        "name": "plan_mode_complete",
        "description": "Submit the finished plan. Call it alone, as your last action in plan mode.",
        "input_schema": {
            "type": "object",
            "properties": { "plan": { "type": "string", "description": "The whole plan in Markdown" } },
            "required": ["plan"]
        }
    })
}

/// Why plan mode blocks this tool call, or None when it may run.
pub fn blocked(tool: &str, input: &Value) -> Option<String> {
    // Plugin and MCP tools (`<plugin>__<name>`) may write anything, so they count as editing.
    if EDITING_TOOLS.contains(&tool) || tool.contains("__") {
        return Some(format!("Plan mode blocks {tool}: it can change files. Finish the plan with plan_mode_complete."));
    }
    let command = input["command"].as_str().unwrap_or_default();
    if matches!(tool, "bash" | "bg_start") && !read_only(command) {
        return Some(format!("Plan mode allows {tool} only read-only commands (ls, cat, rg, fd, git status/log/diff/show, ...) without redirects or $(...)."));
    }
    None
}

// shortcut: splits on | ; & and newlines even inside quotes, so `rg 'a|b'` is refused; a shell parser would fix it.
fn read_only(command: &str) -> bool {
    if command.contains(['>', '`']) || command.contains("$(") || command.contains("<(") {
        return false;
    }
    let mut segments = command.split(['|', ';', '&', '\n']).map(str::trim).filter(|segment| !segment.is_empty()).peekable();
    segments.peek().is_some() && segments.all(read_only_segment)
}

fn read_only_segment(segment: &str) -> bool {
    let words: Vec<&str> = segment.split_whitespace().collect();
    // Short forms of RUNS_OTHER_COMMANDS: fd's `-x`/`-X` run a command, tree's `-o` writes a file and `-R` writes 00Tree.html per directory.
    // Searched in the whole cluster, since `fd -Hx rm` bundles them.
    let risky: &[char] = match words.first() {
        Some(&"fd") => &['x', 'X'],
        Some(&"tree") => &['o', 'R'],
        _ => &[],
    };
    let short_cluster = |word: &str| word.starts_with('-') && !word.starts_with("--") && word.contains(risky);
    if words.iter().any(|word| short_cluster(word) || RUNS_OTHER_COMMANDS.iter().any(|flag| word.starts_with(flag))) {
        return false;
    }
    match words[0] {
        "git" => words.get(1).is_some_and(|sub| READ_ONLY_GIT.contains(sub)),
        program => READ_ONLY_COMMANDS.contains(&program),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_edits_and_state_changing_commands() {
        for tool in ["write", "replace", "undo_last_change", "subagent", "my-plugin__lookup"] {
            assert!(blocked(tool, &json!({"path": "a"})).is_some(), "{tool}");
        }
        for tool in ["read", "create_goal", "add_directory", "bg_kill", "source_check"] {
            assert!(blocked(tool, &json!({"path": "a"})).is_none(), "{tool}");
        }
        assert!(blocked("bg_start", &json!({"command": "rm -rf x"})).is_some());
        assert!(blocked("bg_start", &json!({"command": "git log"})).is_none());
        for command in ["rg -n plan src", "git log --oneline | head -5 && git status", "cd . && ls", "ls -lX", "du -xh", "rg -o x"] {
            assert!(blocked("bash", &json!({ "command": command })).is_none(), "{command}");
        }
        for command in [
            "rm -rf x",
            "cat a > b",
            "git commit -m x",
            "fd -x rm",
            "fd -Hx rm",
            "fd -HX sh",
            "ls; touch x",
            "echo $(rm x)",
            "cat <(rm x)",
            "rg --pre=sh x",
            "fd --exec=rm",
            "git diff --output=f",
            "tree -o f",
            "tree -fo f",
            "tree -RL 1 -H . .",
            "",
        ] {
            assert!(blocked("bash", &json!({ "command": command })).is_some(), "{command}");
        }
    }

    #[test]
    fn entry_round_trips() {
        for plan in [Plan::Off, Plan::Drafting, Plan::Ready("# Plan".into())] {
            assert_eq!(Plan::from_entry(&plan.entry()), plan);
        }
    }
}
