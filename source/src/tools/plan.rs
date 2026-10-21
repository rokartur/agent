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
Plan mode is on. Investigate and design, change nothing. Edits, subagents, plugin and MCP tools and state-changing commands are blocked; Bash runs only read-only commands (ls, cat, rg, fd, git status/log/diff/show, ...). Ask with AskUserQuestion when a decision cannot be inferred. When the plan is ready, call ExitPlanMode with the whole plan in Markdown as your last action. Start the plan with a `# Title` heading. The user then picks: implement in a fresh session, export, or keep planning.
</plan_mode>";

const EDITING_TOOLS: [&str; 5] = ["Write", "ReplaceLines", "Edit", "InsertLines", "Agent"];

const READ_ONLY_COMMANDS: [&str; 18] = ["ls", "cat", "head", "tail", "wc", "rg", "fd", "pwd", "tree", "file", "stat", "which", "echo", "jq", "diff", "du", "cd", "true"];
const READ_ONLY_GIT: [&str; 7] = ["status", "log", "diff", "show", "blame", "ls-files", "rev-parse"];
// Matched as prefixes, so attached values (`--pre=sh`) count too; `--output` writes a file; rg runs `--hostname-bin` for hyperlinks.
const RUNS_OTHER_COMMANDS: [&str; 4] = ["--exec", "--pre", "--output", "--hostname-bin"];

pub fn tool() -> Value {
    json!({
        "name": "ExitPlanMode",
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
    // Plugin and MCP tools (`<plugin>__<name>`, `mcp__<server>__<name>`) may write anything, so they count as editing.
    if EDITING_TOOLS.contains(&tool) || tool.contains("__") {
        return Some(format!("Plan mode blocks {tool}: it can change files. Finish the plan with ExitPlanMode."));
    }
    let command = input["command"].as_str().unwrap_or_default();
    if tool == "Bash" && !read_only(command) {
        return Some("Plan mode allows Bash only read-only commands (ls, cat, rg, fd, git status/log/diff/show, ...) without redirects or $(...).".to_string());
    }
    None
}

fn read_only(command: &str) -> bool {
    let Some(segments) = unquoted_segments(command) else { return false };
    let mut segments = segments.iter().map(|segment| segment.trim()).filter(|segment| !segment.is_empty()).peekable();
    segments.peek().is_some() && segments.all(read_only_segment)
}

/// Splits on `|`, `;`, `&` and newlines outside quotes and drops the quotes, so `'--pre'` is still seen as `--pre`.
/// None when the command substitutes (`$(`, backtick), redirects to a file (`>`, `<(`), escapes with `\` outside single quotes, or leaves a quote open.
fn unquoted_segments(command: &str) -> Option<Vec<String>> {
    let mut segments = vec![String::new()];
    let mut quote = None;
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        let next = chars.peek().copied();
        match (quote, c) {
            (Some('\''), '\'') | (Some('"'), '"') => quote = None,
            (Some('\''), _) => segments.last_mut().expect("starts with one segment").push(c),
            (_, '`' | '\\') => return None,
            (_, '$') if next == Some('(') => return None,
            (None, '>') => return None,
            (None, '<') if next == Some('(') => return None,
            (None, '\'' | '"') => quote = Some(c),
            (None, '|' | ';' | '&' | '\n') => segments.push(String::new()),
            _ => segments.last_mut().expect("starts with one segment").push(c),
        }
    }
    quote.is_none().then_some(segments)
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
        for tool in ["Write", "Edit", "Agent", "my-plugin__lookup", "mcp__github__create_issue"] {
            assert!(blocked(tool, &json!({"path": "a"})).is_some(), "{tool}");
        }
        for tool in ["Read", "Grep", "Goal", "TaskStop", "AskUserQuestion"] {
            assert!(blocked(tool, &json!({"path": "a"})).is_none(), "{tool}");
        }
        assert!(blocked("Bash", &json!({"command": "rm -rf x", "run_in_background": true})).is_some());
        assert!(blocked("Bash", &json!({"command": "git log", "run_in_background": true})).is_none());
        for command in ["rg -n plan src", "git log --oneline | head -5 && git status", "cd . && ls", "ls -lX", "du -xh", "rg -o x", "rg 'a|b' src", "rg \"a; b>c\" src", "echo '$(x)'"] {
            assert!(blocked("Bash", &json!({ "command": command })).is_none(), "{command}");
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
            "rg '--pre=sh' x",
            "rg \"--pre\"=sh x",
            "echo \"$(rm x)\"",
            "echo \\' ; rm x ; echo \\'",
            "echo 'open",
            "rg --hostname-bin=sh --hyperlink-format=default x",
            "fd --exec=rm",
            "git diff --output=f",
            "tree -o f",
            "tree -fo f",
            "tree -RL 1 -H . .",
            "",
        ] {
            assert!(blocked("Bash", &json!({ "command": command })).is_some(), "{command}");
        }
    }

    #[test]
    fn entry_round_trips() {
        for plan in [Plan::Off, Plan::Drafting, Plan::Ready("# Plan".into())] {
            assert_eq!(Plan::from_entry(&plan.entry()), plan);
        }
    }
}
