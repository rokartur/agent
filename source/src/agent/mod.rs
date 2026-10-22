use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub mod condense;
pub mod context;
pub mod edit_hooks;
pub mod lsp;
pub mod modes;
pub mod permissions;
pub mod prompt;
pub mod skill_usage;
pub mod snapshots;
pub mod subagent;
pub mod usage;

use crate::api::models::Model;
use crate::api::{self, Client, Delta, Usage};
use crate::process::now_ms;
use crate::settings::Settings;
use crate::tools;
use crate::tools::ask::{self, Answer, Question};
use crate::tools::bg::{self, Terminals};
use crate::tools::conflicts;
use crate::tools::goal::{AfterTurn, Goal, Status as GoalStatus};
use crate::tools::guard;
use crate::tools::hashline::Anchors;
use crate::tools::plan::{self, Plan};
use crate::tools::rtk::Rtk;
use crate::tools::todo::Todos;
use condense::Condensed;
use permissions::{Decision, Grants, Mode, Request};
use snapshots::Snapshots;
use subagent::{Launch, Runs};

const SUMMARY_SYSTEM: &str =
    "You are a context summarization assistant. Your task is to read a conversation between a user and an AI assistant, then produce a structured summary following the exact format specified.

Do NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary.";

const SUMMARY_PROMPT: &str = "The messages above are a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work.

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

const SUMMARY_PREFIX: &str = "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
const SUMMARY_SUFFIX: &str = "\n</summary>";
/// The result of a tool call or wait that Esc stopped.
pub const ABORTED: &str = "Operation aborted";
const TOOL_RESULT_SUMMARY_CHARS: usize = 2000;
const COMPACT_MAX_TOKENS: u64 = 128_000;
const SNAPSHOT_FAILED: &str = "File snapshot failed, a rewind will not restore files from this tool call";
const BG_WAIT_SUBAGENTS_ONLY: &str = "TaskWait waits for subagents only. A background Bash task (bt-N) reports its exit as a message: keep working or end the turn, or peek with TaskOutput.";
const NAMING_SYSTEM: &str =
    "Name a coding session from the user's first message. Reply with the name only: 3 to 6 words naming the concrete task, in the language of the message, one line, no quotes, no markdown.";
const NAMING_PROMPT_CHARS: usize = 2000;
const NAMING_MAX_TOKENS: u64 = 1000;
const TITLE_CHARS: usize = 80;
/// What the agent reports while it works; the TUI renders these, print mode writes them to stdout.
pub enum Event {
    Text(String),
    Thinking(String),
    Notice(String),
    /// A foreground subagent finished; the TUI draws its status line from the run.
    SubagentDone(Arc<subagent::Run>),
    ToolStart {
        id: String,
        name: String,
        input: Value,
    },
    ToolEnd {
        id: String,
        output: String,
        is_error: bool,
    },
    Steered(String),
    /// Finished background work handed to the model.
    FollowUp {
        source: String,
        text: String,
    },
    TurnEnd {
        usage: Usage,
        context_tokens: u64,
    },
    Compacted {
        summary: String,
    },
    SessionStarted {
        id: String,
        path: Option<String>,
    },
    /// The session moved to a new session started in `cwd`.
    Moved {
        id: String,
        path: Option<String>,
        cwd: PathBuf,
    },
    /// The session's `project｜topic` name, saved to `session_info`.
    Titled(String),
    /// Ask mode: the agent waits for one Decision on `reply`.
    Permission {
        request: Request,
        reply: Sender<Decision>,
    },
    /// Auto mode: what the reviewer decided.
    Reviewed {
        allowed: bool,
        detail: String,
        reason: String,
    },
    /// ask_user_question: the agent waits for one answer slot per question; all None means declined.
    Ask {
        questions: Vec<Question>,
        reply: Sender<Vec<Option<Answer>>>,
    },
}

/// Work that finished in the background; it reaches the model after the current tool batch or turn, or starts a turn when idle.
pub struct FollowUp {
    /// What produced it, e.g. a terminal id; bg_status on that id withdraws it.
    pub source: String,
    pub text: String,
}
pub struct Agent {
    pub client: Client,
    pub model: String,
    pub effort: String,
    pub system: String,
    /// The ponytail and caveman section, sent after `system` and rebuilt when /settings changes them.
    pub modes: String,
    pub tools: Value,
    pub cwd: PathBuf,
    /// `settings.trusted_projects`, with --trust's root: their `.agents` files load and their edit hooks run.
    pub trusted_projects: Vec<String>,
    pub anchors: Anchors,
    pub rtk: Rtk,
    /// Output of `!cmd` shell escapes, sent ahead of the next prompt.
    pub shell_context: Vec<String>,
    /// Image blocks pasted into the prompt, sent with the next prompt.
    pub images: Vec<Value>,
    /// Subdirectory AGENTS.md files already attached to a read result in this session.
    pub nested_instructions: HashSet<PathBuf>,
    /// Directories outside `cwd` that belong to the task: --add-dir and /workspace.
    pub extra_dirs: Vec<PathBuf>,
    pub messages: Vec<Value>,
    pub session: Session,
    pub sessions_dir: PathBuf,
    pub context_window: u64,
    pub max_tokens: u64,
    /// The provider's cheapest model: titles and, by default, condense summaries.
    pub cheap_model: String,
    /// Auto-mode reviews: `reviewer_model` from settings, else `cheap_model`.
    pub reviewer_model: String,
    /// One of `SPEEDS`; anything but "standard" goes out as the request's `speed`, which `api` maps per provider.
    pub speed: String,
    pub cache_ttl: String,
    pub auto_compact: bool,
    pub reserve_tokens: u64,
    pub keep_recent_tokens: u64,
    pub last_context_tokens: u64,
    pub condense: condense::Config,
    /// Summarized batches; the provider sees their stubs, `messages` keeps the originals.
    pub condensed: Vec<Condensed>,
    /// Number of the next short ref (`t7`).
    pub next_ref: usize,
    pub pending_condense: Vec<JoinHandle<Result<(Condensed, api::Reply), String>>>,
    pub pending_title: Option<JoinHandle<Result<String, String>>>,
    pub usage: Usage,
    pub turns: u64,
    pub tool_calls: u64,
    /// Set from the UI thread to stop the stream and kill a running bash command.
    pub abort: Arc<AtomicBool>,
    /// Messages typed while the agent works; delivered with the next tool results.
    pub steering: Arc<Mutex<Vec<String>>>,
    /// Shared with the UI so a mode change applies to the next tool call.
    pub permission_mode: Arc<Mutex<Mode>>,
    pub grants: Grants,
    /// Shared with the UI, which shows it and lets the user edit it in /todos.
    pub todos: Arc<Mutex<Todos>>,
    /// Shared with the UI for the footer and /goal pause, which must work mid-turn.
    pub goal: Arc<Mutex<Option<Goal>>>,
    /// Shared with the UI, which opens the plan menu after the turn that completed the plan.
    pub plan: Arc<Mutex<Plan>>,
    pub follow_ups: Arc<Mutex<Vec<FollowUp>>>,
    /// Shared with the UI for /tasks and the running count above the editor.
    pub terminals: Arc<Mutex<Terminals>>,
    /// Shared with the UI for /tasks.
    pub subagents: Arc<Mutex<Runs>>,
    pub subagent_concurrency: usize,
    pub subagent_effort: String,
    /// --tools: the only tools offered, MCP and plugin tools included.
    pub only_tools: Option<Vec<String>>,
    /// Default for a subagent call without `model`; empty means `model`.
    pub subagent_model: String,
    /// Stored web_search and fetch_content responses for get_search_content.
    pub web: tools::web::Web,
    pub spent: Spent,
    /// False in print mode: tools that need the user fail instead of waiting.
    pub interactive: bool,
    /// pi's agent dir, for rebuilding the system prompt after /worktree.
    pub agent_dir: PathBuf,
    pub home: PathBuf,
    /// Shared with the UI for the footer and /plugins.
    pub plugins: crate::plugins::Plugins,
    /// Shared with the UI for /mcp.
    pub mcp: crate::mcp::Mcp,
    /// None outside a git repo and in print mode; then a rewind leaves files alone.
    pub snapshots: Option<Snapshots>,
    pub lsp: lsp::Lsp,
    /// Files changed by this session's own tool calls, oldest first.
    pub checkpoints: Vec<Checkpoint>,
    pub compactions: Vec<Compaction>,
}

/// Work tree snapshots around one tool call made while message `at` was the latest.
/// Other sessions' edits outside the call never land here.
/// The changed paths are asked from git when needed: one build can change 80k files.
#[derive(Clone, Debug, PartialEq)]
pub struct Checkpoint {
    pub at: usize,
    pub before: String,
    pub after: String,
}

/// What the provider sees in place of `messages[..cut]`. The messages stay, so a rewind can go back past the summary.
#[derive(Clone, Debug, PartialEq)]
pub struct Compaction {
    pub cut: usize,
    pub summary: String,
}

/// A user prompt the conversation can go back to.
#[derive(Debug, PartialEq)]
pub struct RewindPoint {
    /// Message index of the prompt.
    pub at: usize,
    pub text: String,
    pub images: Vec<Value>,
}

#[derive(Clone)]
pub struct Session {
    pub id: String,
    pub path: Option<PathBuf>,
    /// The session this one copies because another process had it open.
    pub forked_from: Option<String>,
    /// Held while the session is open, so a second process copies the session instead of interleaving its entries.
    lock: Option<Arc<File>>,
}

/// What a saved session restores besides its id.
#[derive(Default)]
pub struct History {
    pub messages: Vec<Value>,
    pub condensed: Vec<Condensed>,
    pub todos: Todos,
    pub goal: Option<Goal>,
    pub plan: Plan,
    pub spent: Spent,
    pub checkpoints: Vec<Checkpoint>,
    pub compactions: Vec<Compaction>,
    /// Provider to the account id that served the session's last reply, so a resume keeps the same account.
    pub accounts: HashMap<String, String>,
    pub extra_dirs: Vec<PathBuf>,
}

/// Time and tokens of the user's commands, saved per command so the footer total survives a resume.
#[derive(Default, Clone, Copy, PartialEq, Debug)]
pub struct Spent {
    pub input: u64,
    pub output: u64,
    pub time: Duration,
}

impl Spent {
    pub fn add(&mut self, other: Spent) {
        self.input += other.input;
        self.output += other.output;
        self.time += other.time;
    }

    fn entry(&self) -> Value {
        json!({"type": "spent", "input": self.input, "output": self.output, "ms": self.time.as_millis() as u64})
    }

    fn from_entry(entry: &Value) -> Spent {
        let number = |key: &str| entry[key].as_u64().unwrap_or_default();
        Spent { input: number("input"), output: number("output"), time: Duration::from_millis(number("ms")) }
    }
}

impl Agent {
    /// Runs a `!cmd` the user typed, without asking; with `keep`, pi's "Ran `cmd`" block goes into the next prompt.
    /// shortcut: output kept before a /new, /resume or quit without a prompt is dropped; save it to the session if that matters.
    pub fn run_bash(&mut self, command: &str, keep: bool, emit: &mut dyn FnMut(Event)) {
        self.abort.store(false, Ordering::Relaxed);
        let id = format!("user-bash-{}", now_ms());
        let input = json!({"command": command});
        emit(Event::ToolStart { id: id.clone(), name: "Bash".into(), input: input.clone() });
        let (output, is_error) = match tools::cd_target(command) {
            Some(dir) => match self.cd(dir, emit) {
                Ok(text) => (text, false),
                Err(text) => (text, true),
            },
            None => tools::run("Bash", &input, &self.cwd, &mut self.anchors, &Rtk::OFF, &self.abort),
        };
        if keep {
            self.shell_context.push(format!("Ran `{command}`\n```\n{}\n```", output.trim_end()));
        }
        emit(Event::ToolEnd { id, output, is_error });
    }

    /// Runs the model and its tool calls until it ends the turn; returns the last assistant text.
    pub fn prompt(&mut self, text: &str, emit: &mut dyn FnMut(Event)) -> Result<String, String> {
        let text = &self.plugins.input(text, &self.plugin_context())?;
        self.abort.store(false, Ordering::Relaxed);
        if self.interactive && self.messages.is_empty() {
            self.start_naming(text);
        }
        let mut content: Vec<Value> = self.shell_context.drain(..).map(|text| json!({"type": "text", "text": text})).collect();
        let mut todos = self.todos.lock().unwrap();
        let todo_notes = std::mem::take(&mut todos.user_notes);
        let todo_entry = todos.entry();
        drop(todos);
        if !todo_notes.is_empty() {
            self.session.append(&todo_entry)?;
            content.push(json!({"type": "text", "text": todo_notes.join("\n")}));
        }
        content.append(&mut self.images);
        content.push(json!({"type": "text", "text": text}));
        self.push(json!({"role": "user", "content": content}))?;
        let turns_before = self.turns;
        let result = self.run_turns(text, emit);
        if let Err(error) = &result {
            if self.turns == turns_before {
                // No reply means the API refused this prompt (e.g. a 400 on its image); resending it would fail again.
                self.rewind(self.messages.len() - 1).map_err(|e| format!("{error}; {e}"))?;
            } else {
                self.answer_unpaired_calls(error).map_err(|e| format!("{error}; {e}"))?;
            }
        }
        // A title is cosmetic: on failure /resume falls back to the first prompt line, and one still naming waits for the next prompt.
        if let Some(naming) = self.pending_title.take_if(|naming| naming.is_finished())
            && let Ok(Ok(title)) = naming.join()
        {
            emit(Event::Titled(title));
        }
        self.plugins.event(json!({"type": "prompt_end", "error": result.as_ref().err(), "context": self.plugin_context()}));
        result
    }

    /// A turn that failed between a reply's tool calls and their results leaves calls the API requires answers to,
    /// so every later prompt would get a 400. Answering them with the error keeps the files the calls changed.
    fn answer_unpaired_calls(&mut self, error: &str) -> Result<(), String> {
        let Some(last) = self.messages.last().filter(|message| message["role"] == "assistant") else { return Ok(()) };
        let results: Vec<Value> = last["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|block| block["type"] == "tool_use")
            .map(|call| json!({"type": "tool_result", "tool_use_id": call["id"], "content": format!("Interrupted: {error}"), "is_error": true}))
            .collect();
        if results.is_empty() {
            return Ok(());
        }
        self.push(json!({"role": "user", "content": results}))
    }

    /// Sent with every plugin message; see plugins/README.md.
    pub fn plugin_context(&self) -> Value {
        // None when every model list failed at startup.
        let catalog = self.client.catalog();
        let provider = catalog.iter().find(|m| m.id == self.model).map(|m| m.provider.as_str());
        let (account_name, account_id) = provider.and_then(|provider| self.client.account(provider)).unzip();
        json!({
            "provider": provider,
            "model": self.model,
            "accountId": account_id.flatten(),
            "accountName": account_name,
            "cwd": self.cwd,
            "sessionId": self.session.id,
        })
    }

    fn run_turns(&mut self, text: &str, emit: &mut dyn FnMut(Event)) -> Result<String, String> {
        let mut edited = Vec::new();
        let mut fix_rounds = 0;
        loop {
            if self.auto_compact && self.last_context_tokens > self.context_window.saturating_sub(self.reserve_tokens) {
                emit(Event::Notice("Compacting context...".into()));
                if let Some(summary) = self.compact(None)? {
                    emit(Event::Compacted { summary });
                }
            }
            self.collect_condensed(emit)?;
            let messages = self.provider_view();
            let (system, tools) = self.system_and_tools();
            let mut request = api::request(&self.model, self.max_tokens, &self.effort, &self.cache_ttl, &system, &prompt::cwd_section(&self.cwd, &self.extra_dirs), &tools, &messages);
            if self.speed != "standard" {
                request["speed"] = json!(self.speed);
            }
            let mut reply = self.client.send(&request, &self.abort, &mut |delta| match delta {
                Delta::Text(text) => emit(Event::Text(text.to_string())),
                Delta::Thinking(text) => emit(Event::Thinking(text.to_string())),
                Delta::Notice(text) => emit(Event::Notice(text.to_string())),
            })?;
            self.turns += 1;
            self.usage.add(reply.usage);
            self.session.append(&usage::reply_entry(&reply, now_ms()))?;
            if let Some(goal) = self.goal.lock().unwrap().as_mut().filter(|g| g.status == GoalStatus::Active) {
                // Cache reads are left out: they re-read context an earlier request already paid for.
                goal.tokens_used += reply.usage.input + reply.usage.cache_write + reply.usage.output;
            }
            self.last_context_tokens = reply.context_tokens;
            emit(Event::TurnEnd { usage: reply.usage, context_tokens: reply.context_tokens });
            let aborted = reply.stop_reason == "aborted";
            if aborted {
                keep_finished_text(&mut reply.message);
            }
            let calls: Vec<Value> = reply.message["content"].as_array().into_iter().flatten().filter(|b| b["type"] == "tool_use").cloned().collect();
            let answer = text_of(&reply.message);
            if reply.message["content"].as_array().is_some_and(|c| !c.is_empty()) {
                self.push(reply.message)?;
            }
            if aborted {
                emit(Event::Notice(ABORTED.into()));
                return Ok(answer);
            }
            if calls.is_empty() {
                if reply.stop_reason == "max_tokens" || reply.stop_reason == "refusal" {
                    emit(Event::Notice(format!("Stopped: {}", reply.stop_reason)));
                    return Ok(answer);
                }
                let queued = self.take_queued(emit);
                if queued.is_empty() {
                    // Every file edited this prompt, so a fix round that leaves a failing file alone still sees it fail.
                    let failures = self.run_edit_hooks(edited.clone(), text, emit);
                    if failures.is_empty() || self.abort.load(Ordering::Relaxed) {
                        return Ok(answer);
                    }
                    fix_rounds += 1;
                    let (message, retry) = edit_hooks::report(&failures, fix_rounds);
                    emit(Event::Notice(message.clone()));
                    if !retry {
                        return Ok(answer);
                    }
                    self.push(json!({"role": "user", "content": [{"type": "text", "text": message}]}))?;
                    continue;
                }
                self.push(json!({"role": "user", "content": queued}))?;
                continue;
            }
            let mut results = Vec::new();
            let mut finished = Vec::new();
            for call in calls {
                let id = call["id"].as_str().unwrap_or_default().to_string();
                let name = call["name"].as_str().unwrap_or_default().to_string();
                let input = call["input"].clone();
                let targets = edit_hooks::edited_files(&name, &input, &self.cwd, &self.anchors);
                let mut shown = input.clone();
                // Anchors mean nothing to the user, show the file they resolve to.
                if let Some(file) = targets.first() {
                    shown["file_path"] = file.strip_prefix(&self.cwd).unwrap_or(file).display().to_string().into();
                }
                emit(Event::ToolStart { id: id.clone(), name: name.clone(), input: shown });
                self.plugins.event(json!({"type": "tool_call", "name": name, "input": input, "context": self.plugin_context()}));
                let started = Instant::now();
                let (mut output, is_error, image) = match self.requested_image(&name, &input) {
                    Some(path) => match crate::image::block(&path) {
                        Ok(block) => (format!("[{} is attached as an image.]", path.display()), false, Some(block)),
                        Err(error) => (error, true, None),
                    },
                    None => {
                        let (output, is_error) = self.run_tool_with_checkpoint(&name, &input, &targets, text, emit)?;
                        (output, is_error, None)
                    }
                };
                if !is_error {
                    for file in &targets {
                        match self.lsp.after_edit(file, &self.cwd, &self.abort) {
                            Ok(Some(report)) => output = format!("{output}\n\n{report}"),
                            Ok(None) => {}
                            Err(notice) => emit(Event::Notice(notice)),
                        }
                    }
                    edited.extend(targets);
                }
                self.tool_calls += 1;
                // Includes the LSP check after an edit: the whole wait before the next tool.
                self.session.append(&json!({"type": "tool_time", "id": id, "name": name, "ms": started.elapsed().as_millis() as u64}))?;
                self.plugins.event(json!({"type": "tool_result", "name": name, "output": output, "isError": is_error, "context": self.plugin_context()}));
                emit(Event::ToolEnd { id: id.clone(), output: output.clone(), is_error });
                let content = match image {
                    Some(block) => json!([{"type": "text", "text": output}, block]),
                    None => json!(output),
                };
                results.push(json!({"type": "tool_result", "tool_use_id": id, "content": content, "is_error": is_error}));
                finished.push(condense::Call { id, name, input, output, is_error });
            }
            results.extend(self.take_queued(emit));
            self.push(json!({"role": "user", "content": results}))?;
            if self.abort.load(Ordering::Relaxed) {
                emit(Event::Notice(ABORTED.into()));
                return Ok(answer);
            }
            self.start_condense(&answer, finished);
        }
    }

    /// Runs the edit hooks for the files this prompt changed and returns their failures.
    /// The commands come from a repo file, so each one passes the same permission check as bash.
    fn run_edit_hooks(&mut self, files: Vec<PathBuf>, user_request: &str, emit: &mut dyn FnMut(Event)) -> Vec<String> {
        if files.is_empty() {
            return Vec::new();
        }
        let (hooks, mut failures, skipped) = edit_hooks::hooks(&files, &self.trusted_projects);
        if skipped > 0 {
            emit(Event::Notice(format!("[edit-hooks] {skipped} skipped: their project is not trusted, /trust in it runs them")));
        }
        for hook in hooks {
            if self.abort.load(Ordering::Relaxed) {
                break;
            }
            let input = json!({"command": hook.command, "timeout": 120_000});
            // The directory is part of what gets approved, so a grant does not also cover the command in cwd.
            let approved = json!({"command": format!("cd {} && {}", edit_hooks::shell_quote(&hook.dir.display().to_string()), hook.command), "timeout": 120_000});
            if let Some(refusal) = self.check_permission("Bash", &approved, &[], user_request, emit) {
                emit(Event::Notice(format!("[edit-hooks] {} skipped: {refusal}", hook.command)));
                continue;
            }
            emit(Event::Notice(format!("[edit-hooks] {}", hook.command)));
            let (output, is_error) = tools::run("Bash", &input, &hook.dir, &mut self.anchors, &self.rtk, &self.abort);
            if is_error {
                failures.push(edit_hooks::failure(&hook.command, &output));
            }
        }
        failures
    }

    /// Runs one tool call after the permission check; agent-level tools are handled here, file and shell tools in `tools`.
    fn run_tool(&mut self, name: &str, input: &Value, targets: &[PathBuf], user_request: &str, emit: &mut dyn FnMut(Event)) -> (String, bool) {
        if self.abort.load(Ordering::Relaxed) {
            return (ABORTED.into(), true);
        }
        if *self.plan.lock().unwrap() != Plan::Off
            && let Some(reason) = plan::blocked(name, input)
        {
            return (reason, true);
        }
        if name == "Bash"
            && let Some(read) = guard::file_read(input["command"].as_str().unwrap_or_default())
        {
            let (output, is_error) = self.run_tool("Read", &read, &[], user_request, emit);
            return (format!("Ran as read, so these anchors are what edits use.\n{output}"), is_error);
        }
        if name == "Bash"
            && let Some(reason) = guard::bash(input["command"].as_str().unwrap_or_default())
        {
            return (reason, true);
        }
        if name == "Bash"
            && let Some(reason) = conflicts::commit_blocked(input["command"].as_str().unwrap_or_default(), &self.cwd)
        {
            return (reason, true);
        }
        if let Some(reason) = self.check_permission(name, input, targets, user_request, emit) {
            return (reason, true);
        }
        // After the permission check: starting a language server runs the repo's build scripts.
        // Files outside the project get no baseline, so after_edit skips them too: no server starts for /tmp or node_modules elsewhere.
        for file in targets.iter().filter(|file| file.starts_with(&self.cwd) || self.extra_dirs.iter().any(|dir| file.starts_with(dir))) {
            if let Err(notice) = self.lsp.before_edit(file, &self.abort) {
                emit(Event::Notice(notice));
            }
        }
        let result = match name {
            "ContextTreeQuery" => condense::query(&self.messages, &self.condensed, input),
            "Todo" => self.todo(input),
            "ExitPlanMode" => self.complete_plan(input),
            "Goal" => self.goal_tool(input),
            "Bash" if input["run_in_background"] == true => self.terminals.lock().unwrap().start(input, &self.cwd, &self.follow_ups),
            "TaskOutput" => self.terminals.lock().unwrap().status_of(input, &self.follow_ups),
            "TaskStop" => {
                let targets = self.terminals.lock().unwrap().for_kill(input);
                targets.map(|targets| bg::kill(&targets, &self.follow_ups, &self.abort))
            }
            "Agent" => self.subagent(input, emit),
            "TaskWait" => self.bg_wait(input),
            "WebSearch" | "WebFetch" | "GetSearchContent" => self.web.run(name, input),
            "AskUserQuestion" => self.ask_user(input, emit),
            _ => return self.plugins.tool(name, input, &self.plugin_context(), &self.abort).or_else(|| self.mcp.tool(name, input, &self.abort)).unwrap_or_else(|| self.run_file_tool(name, input)),
        };
        match result {
            Ok(text) => (text, false),
            Err(text) => (text, true),
        }
    }

    fn run_file_tool(&mut self, name: &str, input: &Value) -> (String, bool) {
        let (mut output, is_error) = tools::run(name, input, &self.cwd, &mut self.anchors, &self.rtk, &self.abort);
        if name == "Bash"
            && let Some(hint) = conflicts::hint(input["command"].as_str().unwrap_or_default(), &self.cwd)
        {
            output = format!("{output}\n\n{hint}");
        }
        if name == "Read" && !is_error {
            let paths = match input["paths"].as_array() {
                Some(paths) => paths.iter().filter_map(|entry| entry.as_str().or(entry["path"].as_str())).collect(),
                None => vec![input["path"].as_str().unwrap_or_default()],
            };
            for path in paths {
                self.record_skill_read(&self.cwd.join(path));
                let nested = prompt::nested_context(&self.cwd, &self.extra_dirs, &self.cwd.join(path), &mut self.nested_instructions);
                if !nested.is_empty() {
                    output = format!("{output}\n\n{nested}");
                }
            }
        }
        (output, is_error)
    }

    /// The image file a single-path `Read` names; it goes to the model as an image block, not as anchored text.
    fn requested_image(&self, name: &str, input: &Value) -> Option<PathBuf> {
        if name != "Read" {
            return None;
        }
        let path = tools::hashline::resolve(&self.cwd, input["file_path"].as_str()?);
        crate::image::media_type(&path).map(|_| path)
    }

    /// A read SKILL.md counts as the agent using that skill.
    fn record_skill_read(&self, path: &Path) {
        if path.file_name().is_none_or(|name| name != "SKILL.md") {
            return;
        }
        let Some(skill) = prompt::parse_skill(path) else { return };
        // Usage stats must not fail the read that triggered them.
        let _ = skill_usage::record(&self.agent_dir, &skill.name, skill_usage::By::Agent, &self.cwd);
    }

    fn todo(&mut self, input: &Value) -> Result<String, String> {
        let (text, changed) = self.todos.lock().unwrap().apply(input)?;
        if changed {
            self.session.append(&self.todos.lock().unwrap().entry())?;
        }
        Ok(text)
    }

    /// Steering and finished background work, taken after each tool batch and at turn end.
    fn take_queued(&mut self, emit: &mut dyn FnMut(Event)) -> Vec<Value> {
        let steering = std::mem::take(&mut *self.steering.lock().unwrap());
        let follow_ups = std::mem::take(&mut *self.follow_ups.lock().unwrap());
        let mut blocks = Vec::new();
        for text in steering {
            emit(Event::Steered(text.clone()));
            blocks.push(json!({"type": "text", "text": text}));
        }
        for FollowUp { source, text } in follow_ups {
            blocks.push(json!({"type": "text", "text": text}));
            emit(Event::FollowUp { source, text });
        }
        blocks
    }

    /// Starts a turn from queued follow-ups while idle; Ok("") when another turn already took them.
    pub fn run_follow_ups(&mut self, emit: &mut dyn FnMut(Event)) -> Result<String, String> {
        let blocks = self.take_queued(emit);
        let texts: Vec<&str> = blocks.iter().filter_map(|b| b["text"].as_str()).collect();
        if texts.is_empty() {
            return Ok(String::new());
        }
        self.pursue(&texts.join("\n\n"), emit)
    }

    fn bg_wait(&self, input: &Value) -> Result<String, String> {
        let runs: Vec<_> = {
            let subagents = self.subagents.lock().unwrap();
            match input["task_ids"].as_array() {
                Some(ids) => ids
                    .iter()
                    .map(|id| {
                        let id = id.as_str().unwrap_or_default();
                        subagents.list.iter().find(|r| r.id == id && r.background).cloned().ok_or_else(|| format!("No background subagent {id}. {BG_WAIT_SUBAGENTS_ONLY}"))
                    })
                    .collect::<Result<_, _>>()?,
                None => subagents.list.iter().filter(|r| r.background && r.is_running()).cloned().collect(),
            }
        };
        if runs.is_empty() {
            return Err(format!("No background subagents are running. {BG_WAIT_SUBAGENTS_ONLY}"));
        }
        let timeout = Duration::from_millis(input["timeout"].as_u64().unwrap_or(600_000).min(86_400_000));
        subagent::wait_background(&runs, timeout, &self.abort, &self.follow_ups)
    }

    /// Runs child agents: in the foreground `subagent_concurrency` at a time, or all in the background.
    fn subagent(&mut self, input: &Value, emit: &mut dyn FnMut(Event)) -> Result<String, String> {
        let tasks = subagent::tasks(input)?;
        // A child with an unknown model dies before its first JSONL line, so reject it here.
        if let Some(model) = tasks.iter().filter_map(|t| t.model.as_deref()).find(|m| !self.client.catalog().iter().any(|c| c.id == *m)) {
            return Err(format!("Unknown model {model}."));
        }
        let cwd = match input["cwd"].as_str() {
            Some(dir) => self.cwd.join(dir),
            None => self.cwd.clone(),
        };
        let background = input["run_in_background"].as_bool() == Some(true);
        let batch = if background { tasks.len() } else { self.subagent_concurrency.max(1) };
        let mode = *self.permission_mode.lock().unwrap();
        let mut runs = Vec::new();
        for chunk in tasks.chunks(batch) {
            let mut started = Vec::new();
            for task in chunk {
                let default_model = if self.subagent_model.is_empty() { &self.model } else { &self.subagent_model };
                let model = task.model.clone().unwrap_or_else(|| default_model.clone());
                let thinking = task.effort.clone().unwrap_or_else(|| self.effort.clone());
                let provider = self.client.catalog().iter().find(|c| c.id == model).map(|c| c.provider.clone());
                let account = provider.and_then(|provider| Some(format!("{provider}={}", self.client.account(&provider)?.0)));
                let launch = Launch {
                    label: task.label.clone(),
                    task: task.prompt.clone(),
                    cwd: cwd.clone(),
                    model,
                    thinking,
                    mode,
                    parent_session: self.session.id.clone(),
                    explore: task.explore,
                    account,
                    extra_dirs: self.extra_dirs.clone(),
                    trusted: prompt::is_trusted(&self.trusted_projects, &cwd),
                };
                // The tool reports failure, so this chunk's runs must not keep going; earlier chunks have finished.
                match self.subagents.lock().unwrap().spawn(launch, background, &self.follow_ups) {
                    Ok(run) => started.push(run),
                    Err(error) => return Err(stop_after_failed_spawn(error, &started, &runs)),
                }
            }
            if !background {
                subagent::wait(&started, &self.abort);
                for run in &started {
                    emit(Event::SubagentDone(run.clone()));
                }
            }
            runs.extend(started);
            if self.abort.load(Ordering::Relaxed) {
                return Err(ABORTED.into());
            }
        }
        if background {
            let names: Vec<String> = runs.iter().map(|r| format!("{} ({})", r.id, r.label)).collect();
            return Ok(format!("Started {} in the background. Each answer arrives as a message when it finishes, or TaskWait waits for them; /tasks shows progress.", names.join(", ")));
        }
        if let [run] = runs.as_slice() {
            return Ok(run.result_text());
        }
        Ok(result_sections(&runs))
    }

    fn complete_plan(&mut self, input: &Value) -> Result<String, String> {
        let plan = input["plan"].as_str().map(str::trim).unwrap_or_default();
        if plan.is_empty() {
            return Err("Error: plan required".into());
        }
        *self.plan.lock().unwrap() = Plan::Ready(plan.to_string());
        self.save_plan()?;
        Ok("Plan submitted. End your turn now; the user picks what happens next.".into())
    }

    fn goal_tool(&mut self, input: &Value) -> Result<String, String> {
        let mut goal = self.goal.lock().unwrap();
        let text = match input["action"].as_str().unwrap_or_default() {
            "create" => {
                let objective = input["objective"].as_str().filter(|o| !o.trim().is_empty()).ok_or("Objective required.")?;
                *goal = Some(Goal::new(objective, input["token_budget"].as_u64()));
                format!("Goal created: {objective}")
            }
            "complete" => {
                let goal = goal.as_mut().ok_or("No goal.")?;
                goal.status = GoalStatus::Complete;
                goal.evidence = input["evidence"].as_str().map(String::from);
                "Goal marked complete.".into()
            }
            "get" => return Ok(goal.as_ref().map_or("No goal.".into(), Goal::summary)),
            other => return Err(format!("Unknown action {other:?}; use create, get or complete.")),
        };
        self.session.append(&Goal::entry(goal.as_ref()))?;
        Ok(text)
    }

    /// Runs `text`, then keeps prompting while an active goal is not complete; Esc or a spent budget pause it.
    pub fn pursue(&mut self, text: &str, emit: &mut dyn FnMut(Event)) -> Result<String, String> {
        let mut answer = self.prompt(text, emit)?;
        loop {
            let next = match self.goal.lock().unwrap().as_mut() {
                Some(goal) => goal.after_turn(self.abort.load(Ordering::Relaxed)),
                None => AfterTurn::Idle,
            };
            match next {
                AfterTurn::Idle => return Ok(answer),
                AfterTurn::Paused(note) => {
                    self.save_goal()?;
                    emit(Event::Notice(note));
                    return Ok(answer);
                }
                AfterTurn::Continue(prompt) => {
                    self.save_goal()?;
                    emit(Event::Steered(prompt.clone()));
                    answer = self.prompt(&prompt, emit)?;
                }
            }
        }
    }

    pub fn save_spent(&mut self, spent: Spent) -> Result<(), String> {
        self.spent.add(spent);
        self.session.append(&spent.entry())
    }

    pub fn save_goal(&mut self) -> Result<(), String> {
        let entry = Goal::entry(self.goal.lock().unwrap().as_ref());
        self.session.append(&entry)
    }

    pub fn save_plan(&mut self) -> Result<(), String> {
        let entry = self.plan.lock().unwrap().entry();
        self.session.append(&entry)
    }

    fn ask_user(&mut self, input: &Value, emit: &mut dyn FnMut(Event)) -> Result<String, String> {
        if !self.interactive {
            return Err(ask::NO_UI.into());
        }
        let questions = ask::parse(input)?;
        let (reply, answers) = mpsc::channel();
        emit(Event::Ask { questions: questions.clone(), reply });
        Ok(ask::response(&questions, &answers.recv().unwrap_or_default()))
    }

    /// Names a fresh session from its first prompt on a background thread; `prompt` emits the title once it is ready.
    fn start_naming(&mut self, text: &str) {
        let request = self.naming_request(text);
        let (client, session, cwd) = (self.client.clone(), self.session.clone(), self.cwd.clone());
        self.pending_title = Some(std::thread::spawn(move || name_session(&client, &session, &cwd, &request)));
    }

    /// /rename-title-ai: names the session again from its first prompt.
    pub fn rename_title(&self) -> Result<String, String> {
        let first = self.messages.iter().find(|m| m["role"] == "user" && m["content"][0]["type"] == "text");
        let Some(text) = first.and_then(|m| m["content"][0]["text"].as_str()) else { return Err("Nothing to name yet.".into()) };
        name_session(&self.client, &self.session, &self.cwd, &self.naming_request(text))
    }

    fn naming_request(&self, text: &str) -> Value {
        let prompt: String = text.chars().take(NAMING_PROMPT_CHARS).collect();
        api::plain_request(&self.cheap_model, NAMING_SYSTEM, &prompt, NAMING_MAX_TOKENS)
    }

    /// Summarizes the turn's tool results on a background thread; the next request picks the summary up.
    fn start_condense(&mut self, assistant_text: &str, calls: Vec<condense::Call>) {
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
    fn collect_condensed(&mut self, emit: &mut dyn FnMut(Event)) -> Result<(), String> {
        let (finished, running) = std::mem::take(&mut self.pending_condense).into_iter().partition(|handle| handle.is_finished());
        self.pending_condense = running;
        for handle in finished {
            match handle.join().unwrap_or_else(|_| Err("the summarizer thread panicked".into())) {
                Ok((batch, reply)) => {
                    self.usage.add(reply.usage);
                    self.session.append(&usage::reply_entry(&reply, now_ms()))?;
                    self.session.append(&condense::entry(&batch))?;
                    self.condensed.push(batch);
                }
                Err(error) => emit(Event::Notice(format!("Condense failed, kept the original tool outputs: {error}"))),
            }
        }
        Ok(())
    }

    /// None when the call may run; otherwise the tool result that explains the refusal.
    /// A user's "No" also stops the turn, like Esc.
    fn check_permission(&mut self, tool: &str, input: &Value, edited_files: &[PathBuf], user_request: &str, emit: &mut dyn FnMut(Event)) -> Option<String> {
        let request = permissions::request_for(tool, input, edited_files)?;
        let mode = *self.permission_mode.lock().unwrap();
        if mode == Mode::FullAccess || self.grants.allows(&request) {
            return None;
        }
        if mode == Mode::Auto {
            return self.review(user_request, &request, input, emit);
        }
        let (reply, answer) = mpsc::channel();
        emit(Event::Permission { request: request.clone(), reply });
        match answer.recv().unwrap_or(Decision::Deny) {
            Decision::Allow => None,
            Decision::AllowAlways => {
                self.grants.add(&request);
                None
            }
            Decision::Deny => {
                self.abort.store(true, Ordering::Relaxed);
                Some("The user denied this tool call.".into())
            }
        }
    }

    /// Auto mode: a small model allows or denies the call; a failed review or a call too long to show whole denies it.
    fn review(&mut self, user_request: &str, request: &Request, input: &Value, emit: &mut dyn FnMut(Event)) -> Option<String> {
        let (allowed, reason) = match permissions::review_prompt(user_request, &self.cwd, request, input) {
            Some(prompt) => self.ask_reviewer(&prompt),
            None => (false, format!("the call is over {} characters, too long to review", permissions::REVIEW_DETAIL_CHARS)),
        };
        emit(Event::Reviewed { allowed, detail: request.detail.clone(), reason: reason.clone() });
        if allowed { None } else { Some(format!("Denied by the auto-mode reviewer: {reason}. Try another way or ask the user.")) }
    }

    fn ask_reviewer(&mut self, prompt: &str) -> (bool, String) {
        // Adaptive thinking counts against max_tokens; at 200 a long think left no verdict text.
        match self.client.send_quiet(&api::plain_request(&self.reviewer_model, permissions::REVIEW_SYSTEM, prompt, 2000), &self.abort) {
            Ok(reply) => {
                self.usage.add(reply.usage);
                if let Err(error) = self.session.append(&usage::reply_entry(&reply, now_ms())) {
                    return (false, format!("the review could not be saved: {error}"));
                }
                permissions::parse_review(&text_of(&reply.message))
            }
            Err(error) => (false, format!("review failed: {error}")),
        }
    }

    fn push(&mut self, message: Value) -> Result<(), String> {
        self.session.append(&json!({"type": "message", "message": message}))?;
        self.messages.push(message);
        Ok(())
    }

    /// Files this session changed, with diffs; None when the work tree is not snapshotted.
    pub fn changes(&self) -> Result<Option<Vec<FileChange>>, String> {
        let Some(snapshots) = &self.snapshots else { return Ok(None) };
        session_changes(snapshots, &self.checkpoints).map(Some)
    }

    /// Drops the message at `index` and everything after it, and puts back the files those prompts changed.
    /// None: the work tree is not snapshotted, so files stay as they are.
    pub fn rewind(&mut self, index: usize) -> Result<Option<RestoredFiles>, String> {
        let files = self.snapshots.as_ref().map(|snapshots| restore_files(snapshots, &self.checkpoints, index)).transpose()?;
        self.checkpoints.retain(|checkpoint| checkpoint.at < index);
        self.compactions.retain(|compaction| compaction.cut <= index);
        self.messages.truncate(index);
        self.session.append(&json!({"type": "rewind", "index": index}))?;
        self.estimate_context_tokens();
        Ok(files)
    }

    /// Snapshots around a tool that can change files, so a rewind touches only what this session changed.
    /// A file another session edits while this call runs still counts as this session's.
    fn run_tool_with_checkpoint(&mut self, name: &str, input: &Value, targets: &[PathBuf], user_request: &str, emit: &mut dyn FnMut(Event)) -> Result<(String, bool), String> {
        const CHANGES_FILES: [&str; 6] = ["Write", "ReplaceLines", "Edit", "InsertLines", "Bash", "Agent"];
        // Before the snapshot: the move swaps the snapshot repo, so before and after would come from different checkouts.
        if name == "Bash"
            && let Some(dir) = tools::cd_target(input["command"].as_str().unwrap_or_default())
        {
            // The auto reviewer trusts calls inside the working directory, so moving it is a call to review.
            if let Some(reason) = self.check_permission(name, input, targets, user_request, emit) {
                return Ok((reason, true));
            }
            return Ok(match self.cd(dir, emit) {
                Ok(text) => (text, false),
                Err(text) => (text, true),
            });
        }
        let Some(snapshots) = self.snapshots.as_ref().filter(|_| CHANGES_FILES.contains(&name)) else {
            return Ok(self.run_tool(name, input, targets, user_request, emit));
        };
        let before = match snapshots.take() {
            Ok(before) => before,
            Err(error) => {
                emit(Event::Notice(format!("{SNAPSHOT_FAILED}: {error}")));
                return Ok(self.run_tool(name, input, targets, user_request, emit));
            }
        };
        let result = self.run_tool(name, input, targets, user_request, emit);
        let snapshots = self.snapshots.as_ref().expect("checked at the top of run_tool_with_checkpoint");
        match snapshots.take() {
            Ok(after) if after == before => {}
            Ok(after) => {
                let at = self.messages.len();
                self.session.append(&json!({"type": "tool_checkpoint", "index": at, "before": before, "after": after}))?;
                self.checkpoints.push(Checkpoint { at, before, after });
            }
            Err(error) => emit(Event::Notice(format!("{SNAPSHOT_FAILED}: {error}"))),
        }
        Ok(result)
    }

    /// Summarizes everything before a recent assistant message and keeps the rest verbatim, like pi.
    /// Returns None when there is nothing old enough to summarize.
    pub fn compact(&mut self, focus: Option<&str>) -> Result<Option<String>, String> {
        let view = self.compacted();
        let Some(first_kept) = cut_point(&view, self.keep_recent_tokens) else {
            return Ok(None);
        };
        let prompt = compaction_prompt(&serialize(&view[..first_kept]), focus);
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
            return Err(format!("Compaction summary stopped with {}.", reply.stop_reason));
        }
        let summary = text_of(&reply.message);
        if summary.trim().is_empty() {
            return Err("The compaction summary came back empty.".into());
        }
        let cut = message_index(&self.compactions, first_kept);
        self.session.append(&json!({"type": "compaction", "summary": summary, "cut": cut}))?;
        self.compactions.push(Compaction { cut, summary: summary.clone() });
        self.last_context_tokens = 0;
        Ok(Some(summary))
    }

    /// Applies settings changed in /settings; `model` is the catalog entry for `settings.model`, `cheap_model` its provider's cheapest.
    fn apply(&mut self, settings: &Settings, model: &Model, cheap_model: &str) {
        self.trusted_projects = settings.trusted_projects.clone();
        self.model = settings.model.clone();
        self.effort = settings.thinking.clone();
        self.speed = settings.speed.clone();
        self.cache_ttl = settings.cache_ttl.clone();
        self.context_window = settings.context_window(model);
        self.max_tokens = model.max_tokens;
        self.cheap_model = cheap_model.to_string();
        self.reviewer_model = if settings.reviewer_model.is_empty() { cheap_model.to_string() } else { settings.reviewer_model.clone() };
        self.auto_compact = settings.auto_compact;
        self.reserve_tokens = settings.reserve_tokens;
        self.keep_recent_tokens = settings.keep_recent_tokens;
        self.condense = condense::Config::new(settings, cheap_model);
        self.subagent_concurrency = settings.subagent_concurrency;
        self.subagent_effort = settings.subagent_effort.clone();
        self.subagent_model = settings.subagent_model.clone();
        self.tools = self.tool_schemas();
        *self.permission_mode.lock().unwrap() = settings.permission_mode;
        self.rtk = if settings.rtk { Rtk::ON } else { Rtk::OFF };
        self.modes = modes::prompt(&self.home, &settings.ponytail, &settings.caveman);
        self.system = prompt::system_prompt(&self.agent_dir, &self.cwd, settings.trusts(&self.cwd), &settings.disabled_skills, settings.prefer_global());
    }

    /// Applies `settings`, swapping a model no account lists for one an account lists, then for OpenRouter's public list.
    /// Returns the notice for a swapped model.
    pub fn settle_model(&mut self, settings: &mut Settings) -> Result<Option<String>, String> {
        let catalog = self.client.catalog();
        let model = catalog.iter().find(|m| m.id == settings.model).or_else(|| catalog.iter().find(|m| !m.accounts.is_empty())).or_else(|| catalog.first());
        let Some(model) = model.cloned() else {
            return Err("No provider listed any model.".into());
        };
        let notice = (model.id != settings.model).then(|| format!("{} is not on any account's model list; using {}.", settings.model, model.id));
        settings.model = model.id.clone();
        let cheap_model = api::models::small_model(&catalog, &model).id.clone();
        self.apply(settings, &model, &cheap_model);
        Ok(notice)
    }

    /// Starts an empty session (unsaved if the current one is unsaved).
    pub fn new_session(&mut self) -> Result<(), String> {
        self.session = Session::start(&self.sessions_dir, self.session.path.is_some(), &self.cwd, &self.model)?;
        self.load(History::default());
        self.announce_session();
        Ok(())
    }

    /// Switches to the saved session `id`; returns its messages for the transcript and the accounts that served it.
    pub fn open_session(&mut self, id: &str) -> Result<(Vec<Value>, HashMap<String, String>), String> {
        let (session, mut history) = Session::resume(&self.sessions_dir, id)?;
        self.session = session;
        let messages = history.messages.clone();
        let accounts = std::mem::take(&mut history.accounts);
        self.load(history);
        self.announce_session();
        Ok((messages, accounts))
    }

    /// --add-dir and /workspace add: `path` is relative to cwd; saved so a resume restores it. Returns the notice to show.
    pub fn add_dir(&mut self, path: &str) -> Result<String, String> {
        let dir = self.cwd.join(path).canonicalize().map_err(|e| format!("Cannot open {path}: {e}"))?;
        if !dir.is_dir() {
            return Err(format!("{} is not a directory.", dir.display()));
        }
        let cwd = self.cwd.canonicalize().map_err(|e| format!("Cannot open {}: {e}", self.cwd.display()))?;
        // Not an error, so `agent -c --add-dir ../api` on a session that has it still starts.
        if dir.starts_with(&cwd) {
            return Ok(format!("{} is inside the working directory, so it is already in the workspace.", dir.display()));
        }
        if let Some(known) = self.extra_dirs.iter().find(|known| dir.starts_with(known)) {
            return Ok(format!("{} is already added as {}.", dir.display(), known.display()));
        }
        self.extra_dirs.push(dir.clone());
        self.save_extra_dirs()?;
        Ok(format!("Added {}.", dir.display()))
    }

    /// /workspace remove: takes the path as listed or as typed relative to cwd. Returns the notice to show.
    pub fn remove_dir(&mut self, path: &str) -> Result<String, String> {
        let typed = self.cwd.join(path);
        let dir = typed.canonicalize().unwrap_or(typed);
        let index = self.extra_dirs.iter().position(|known| *known == dir).ok_or(format!("{} is not an extra directory.", dir.display()))?;
        let dir = self.extra_dirs.remove(index);
        self.save_extra_dirs()?;
        Ok(format!("Removed {}.", dir.display()))
    }

    fn save_extra_dirs(&self) -> Result<(), String> {
        self.session.append(&json!({"type": "extra_dirs", "dirs": self.extra_dirs}))
    }

    /// `cd DIR` typed after `!` or sent as the model's whole Bash command: the session moves to DIR, like /worktree.
    fn cd(&mut self, dir: &str, emit: &mut dyn FnMut(Event)) -> Result<String, String> {
        let typed = match dir.strip_prefix('~') {
            Some(rest) if rest.is_empty() || rest.starts_with('/') => self.home.join(rest.trim_start_matches('/')),
            _ => self.cwd.join(dir),
        };
        let dir = typed.canonicalize().map_err(|e| format!("Cannot open {dir}: {e}"))?;
        if !dir.is_dir() {
            return Err(format!("{} is not a directory.", dir.display()));
        }
        if dir == self.cwd {
            return Ok(format!("Already in {}.", dir.display()));
        }
        self.move_to(dir.clone(), emit)?;
        Ok(format!("Session moved to {}; later commands and relative paths start there.", dir.display()))
    }

    /// /worktree and `cd`: pi pins a session to its cwd, so the history moves to a new session started in `cwd`.
    pub fn move_to(&mut self, cwd: PathBuf, emit: &mut dyn FnMut(Event)) -> Result<(), String> {
        // Everything that can fail runs before the agent changes, so an error leaves it in the old checkout.
        let settings = crate::settings::load(&self.home)?;
        let session = self.copy_session(&cwd)?;
        // Earlier snapshots belong to the old checkout's snapshot repo.
        session.append(&json!({"type": "checkpoints_reset"}))?;
        self.grants = Grants::default();
        self.shell_context.push(format!("Session moved from {} to {}. Paths and git state from earlier messages refer to the old checkout.", self.cwd.display(), cwd.display()));
        self.trusted_projects = settings.trusted_projects.clone();
        self.system = prompt::system_prompt(&self.agent_dir, &cwd, settings.trusts(&cwd), &settings.disabled_skills, settings.prefer_global());
        self.anchors = Anchors::new(&session.id);
        self.nested_instructions.clear();
        self.checkpoints.clear();
        if self.snapshots.is_some() {
            self.snapshots = Snapshots::open(&self.home, &cwd);
        }
        self.cwd = cwd.clone();
        self.session = session;
        self.announce_session();
        emit(Event::Moved { id: self.session.id.clone(), path: self.session.display_path(), cwd });
        Ok(())
    }

    /// /clone: continues in a copy of this session, so the original stays as it was.
    pub fn clone_session(&mut self) -> Result<(), String> {
        self.session = self.copy_session(&self.cwd)?;
        self.announce_session();
        Ok(())
    }

    /// /import: copies an exported session into the sessions dir and resumes it.
    pub fn import_session(&mut self, file: &Path) -> Result<(Vec<Value>, HashMap<String, String>), String> {
        let id = Session::import(&self.sessions_dir, file)?;
        let target = self.sessions_dir.join(format!("{id}.jsonl"));
        self.open_session(&id).inspect_err(|_| {
            let _ = std::fs::remove_file(&target);
        })
    }

    /// A new session in `cwd` holding this session's entries.
    fn copy_session(&self, cwd: &Path) -> Result<Session, String> {
        match &self.session.path {
            Some(path) => Session::copy(&self.sessions_dir, path, cwd, &self.model),
            None => Session::start(&self.sessions_dir, false, cwd, &self.model),
        }
    }

    /// /export: the session file as is, or for a `.md` path the conversation as plain text.
    pub fn export_session(&self, out: &Path) -> Result<(), String> {
        let text = if out.extension().is_some_and(|ext| ext == "md") {
            serialize(&self.messages)
        } else {
            let path = self.session.path.as_ref().ok_or("This session is not saved (--no-session); export to a .md path.")?;
            std::fs::read_to_string(path).map_err(|e| format!("Cannot read {}: {e}", path.display()))?
        };
        // create_new: `/export README.md` must not replace a project file that no snapshot covers. 0600 like the session file it copies.
        write_new_private(out, &text).map_err(|e| match e.kind() {
            std::io::ErrorKind::AlreadyExists => format!("{} exists.", out.display()),
            _ => format!("Cannot write {}: {e}", out.display()),
        })
    }

    /// Offers only `names`; an error names one no built-in, plugin or MCP tool has.
    pub fn limit_tools(&mut self, names: &[String]) -> Result<(), String> {
        self.only_tools = None;
        let (_, offered) = self.system_and_tools();
        let offered = offered.as_array().expect("tool schemas are an array");
        if let Some(unknown) = names.iter().find(|name| !offered.iter().any(|tool| tool["name"] == name.as_str())) {
            return Err(format!("--tools: no tool is named {unknown}."));
        }
        self.only_tools = Some(names.to_vec());
        Ok(())
    }

    /// The system prompt and tool schemas the next request sends, with modes, plugin tools and plan mode applied.
    fn system_and_tools(&self) -> (String, Value) {
        let mut system = if self.modes.is_empty() { self.system.clone() } else { format!("{}\n\n{}", self.system, self.modes) };
        let mut tools = self.tools.clone();
        let list = tools.as_array_mut().expect("tool schemas are an array");
        list.extend(self.plugins.tools());
        list.extend(self.mcp.tools());
        if *self.plan.lock().unwrap() != Plan::Off {
            system = format!("{system}\n\n{}", plan::PROMPT);
            list.push(plan::tool());
        }
        if let Some(only) = &self.only_tools {
            list.retain(|tool| only.iter().any(|name| tool["name"] == name.as_str()));
        }
        (system, tools)
    }

    /// The last request's context tokens split across `context::PARTS`.
    pub fn context_split(&self) -> context::PerPart {
        let (system, tools) = self.system_and_tools();
        let system = format!("{system}{}", prompt::cwd_section(&self.cwd, &self.extra_dirs));
        context::split(&system, &tools, &self.provider_view(), self.last_context_tokens)
    }

    /// Set after a resume or rewind, so auto-compact runs before a first request that would not fit.
    fn estimate_context_tokens(&mut self) {
        let (system, tools) = self.system_and_tools();
        let system = format!("{system}{}", prompt::cwd_section(&self.cwd, &self.extra_dirs));
        self.last_context_tokens = context::estimate(&system, &tools, &self.provider_view());
    }

    fn tool_schemas(&self) -> Value {
        // Condensed batches of a resumed session keep their originals reachable after condense is turned off.
        let context_tree_query = self.condense.enabled || !self.condensed.is_empty();
        tools::schemas(&subagent::swarm_prompt(&self.agent_dir, &self.cwd, prompt::is_trusted(&self.trusted_projects, &self.cwd), &self.subagent_effort), context_tree_query)
    }

    /// Replaces the conversation state with a saved (or empty) session's.
    pub fn load(&mut self, history: History) {
        self.grants = Grants::default();
        self.next_ref = condense::next_ref(&history.condensed);
        self.messages = history.messages;
        self.condensed = history.condensed;
        *self.todos.lock().unwrap() = history.todos;
        *self.goal.lock().unwrap() = history.goal;
        *self.plan.lock().unwrap() = history.plan;
        self.tools = self.tool_schemas();
        self.spent = history.spent;
        self.pending_condense.clear();
        self.pending_title = None;
        self.nested_instructions.clear();
        self.checkpoints = history.checkpoints;
        self.compactions = history.compactions;
        self.extra_dirs = history.extra_dirs;
        self.estimate_context_tokens();
    }

    /// The messages with everything before the latest compaction replaced by its summary.
    fn compacted(&self) -> Vec<Value> {
        compacted(&self.messages, &self.compactions, self.session.path.as_deref())
    }

    fn provider_view(&self) -> Vec<Value> {
        condense::view(&self.compacted(), &self.condensed)
    }

    /// Tells plugins the session started; needs the settled model for the context's provider.
    pub fn announce_session(&self) {
        self.plugins.event(json!({"type": "session_start", "context": self.plugin_context()}));
    }

    pub fn summary(&self, answer: &str) -> Value {
        json!({
            "session_id": self.session.id,
            "model": self.model,
            "turns": self.turns,
            "tool_calls": self.tool_calls,
            "usage": {
                "input_tokens": self.usage.input,
                "output_tokens": self.usage.output,
                "cache_read_input_tokens": self.usage.cache_read,
                "cache_creation_input_tokens": self.usage.cache_write,
            },
            "final_output": answer,
        })
    }
}

/// An aborted stream can end inside a tool call or a thinking block; only finished text is valid history.
fn keep_finished_text(message: &mut Value) {
    if let Some(content) = message["content"].as_array_mut() {
        content.retain(|block| block["type"] == "text" && block["text"].as_str().is_some_and(|t| !t.is_empty()));
    }
}

pub struct SessionInfo {
    pub id: String,
    pub cwd: String,
    pub name: Option<String>,
    /// First line of the first prompt.
    pub title: String,
    pub turns: usize,
    pub modified: std::time::SystemTime,
}

impl Session {
    /// The session file for display; `None` when the session is not saved.
    pub fn display_path(&self) -> Option<String> {
        self.path.as_ref().map(|p| p.display().to_string())
    }

    pub fn start(dir: &Path, save: bool, cwd: &Path, model: &str) -> Result<Session, String> {
        // 8 hex chars from the clock and pid: short enough to type after `--resume`.
        let mut seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() ^ ((std::process::id() as u128) << 64);
        // A colliding id would append this session's header to another session's file.
        let id = loop {
            let id = format!("{:08x}", (seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 64) as u32);
            if !save || !dir.join(format!("{id}.jsonl")).exists() {
                break id;
            }
            seed += 1;
        };
        let mut session = Session { path: save.then(|| dir.join(format!("{id}.jsonl"))), id, forked_from: None, lock: None };
        if save {
            std::fs::create_dir_all(dir).map_err(|e| format!("Cannot create {}: {e}", dir.display()))?;
        }
        session.append(&json!({"type": "session", "id": session.id, "cwd": cwd, "model": model, "timestamp": now_ms()}))?;
        if let Some(path) = &session.path {
            let file = File::open(path).map_err(|e| format!("Cannot open {}: {e}", path.display()))?;
            file.try_lock().map_err(|e| format!("Cannot lock {}: {e}", path.display()))?;
            session.lock = Some(Arc::new(file));
        }
        Ok(session)
    }

    /// Opens session `id`, or the one saved session whose id starts with `id`.
    /// When another process has it open, continues in a copy so the two conversations do not interleave in one file.
    pub fn resume(dir: &Path, id: &str) -> Result<(Session, History), String> {
        let id = resolve_id(dir, id)?;
        let path = dir.join(format!("{id}.jsonl"));
        let history = Session::history(&path)?;
        let file = File::open(&path).map_err(|e| format!("Cannot open {}: {e}", path.display()))?;
        match file.try_lock() {
            Ok(()) => Ok((Session { id, path: Some(path), forked_from: None, lock: Some(Arc::new(file)) }, history)),
            Err(TryLockError::WouldBlock) => Ok((Session::fork(dir, &path, id)?, history)),
            Err(TryLockError::Error(e)) => Err(format!("Cannot lock {}: {e}", path.display())),
        }
    }

    pub fn fork_notice(&self) -> Option<String> {
        let from = self.forked_from.as_ref()?;
        Some(format!("Session {from} is open in another process, continuing in copy {}", self.id))
    }

    fn fork(dir: &Path, path: &Path, id: String) -> Result<Session, String> {
        let cannot_read = |e: std::io::Error| format!("Cannot read {}: {e}", path.display());
        let first_line = BufReader::new(File::open(path).map_err(cannot_read)?).lines().next().transpose().map_err(cannot_read)?;
        let header: Value = serde_json::from_str(&first_line.unwrap_or_default()).map_err(|e| format!("{}:1: {e}", path.display()))?;
        let (Some(cwd), Some(model)) = (header["cwd"].as_str(), header["model"].as_str()) else {
            return Err(format!("{}:1: session header without cwd or model", path.display()));
        };
        let mut copy = Session::copy(dir, path, Path::new(cwd), model)?;
        copy.forked_from = Some(id);
        Ok(copy)
    }

    /// A new saved session holding every entry of `from` after its header.
    fn copy(dir: &Path, from: &Path, cwd: &Path, model: &str) -> Result<Session, String> {
        let session = Session::start(dir, true, cwd, model)?;
        let text = std::fs::read_to_string(from).map_err(|e| format!("Cannot read {}: {e}", from.display()))?;
        for line in text.lines().skip(1) {
            let entry: Value = serde_json::from_str(line).map_err(|e| format!("{}: {e}", from.display()))?;
            session.append(&entry)?;
        }
        Ok(session)
    }

    /// Replays a session file without opening the session, e.g. to show a subagent's transcript.
    pub fn history(path: &Path) -> Result<History, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("Cannot read {}: {e}", path.display()))?;
        let mut history = History::default();
        for (number, line) in text.lines().enumerate() {
            let entry: Value = serde_json::from_str(line).map_err(|e| format!("{}:{}: {e}", path.display(), number + 1))?;
            let missing = |field: &str| format!("{}:{}: {} without {field}", path.display(), number + 1, entry["type"].as_str().unwrap_or_default());
            match entry["type"].as_str() {
                Some("message") => history.messages.push(entry["message"].clone()),
                Some("rewind") => {
                    let index = saved_index(&entry, "index", "to", &history.compactions).ok_or_else(|| missing("index"))?;
                    history.messages.truncate(index);
                    history.checkpoints.retain(|checkpoint| checkpoint.at < index);
                    history.compactions.retain(|compaction| compaction.cut <= index);
                }
                Some("tool_checkpoint") => {
                    let text = |field: &str| entry[field].as_str().map(String::from).ok_or_else(|| missing(field));
                    let at = saved_index(&entry, "index", "at", &history.compactions).ok_or_else(|| missing("index"))?;
                    history.checkpoints.push(Checkpoint { at, before: text("before")?, after: text("after")? });
                }
                Some("checkpoints_reset") => history.checkpoints.clear(),
                Some("compaction") => {
                    let cut = saved_index(&entry, "cut", "first_kept", &history.compactions).ok_or_else(|| missing("cut"))?;
                    let summary = entry["summary"].as_str().ok_or_else(|| missing("summary"))?;
                    // Two processes appending to one session file interleave their messages, so the cut can point past them.
                    if cut > history.messages.len() {
                        return Err(format!("{}:{}: compaction keeps from message {cut} of {}", path.display(), number + 1, history.messages.len()));
                    }
                    history.compactions.push(Compaction { cut, summary: summary.to_string() });
                }
                Some("condense") => history.condensed.push(condense::from_entry(&entry).ok_or_else(|| missing("refs or summary"))?),
                Some("goal") => history.goal = Goal::from_entry(&entry),
                Some("plan") => history.plan = Plan::from_entry(&entry),
                Some("todo") => history.todos = Todos::from_entry(&entry),
                Some("spent") => history.spent.add(Spent::from_entry(&entry)),
                Some("extra_dirs") => history.extra_dirs = serde_json::from_value(entry["dirs"].clone()).map_err(|_| missing("dirs"))?,
                Some("usage") => {
                    if let (Some(provider), Some(account)) = (entry["provider"].as_str(), entry["account"].as_str()) {
                        history.accounts.insert(provider.to_string(), account.to_string());
                    }
                }
                _ => {}
            }
        }
        Ok(history)
    }

    /// Saved sessions in `dir`, newest first. Unreadable files are skipped.
    pub fn list(dir: &Path) -> Vec<SessionInfo> {
        let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
        let mut sessions = Vec::new();
        for path in entries.filter_map(|e| e.ok()).map(|e| e.path()) {
            let Some(id) = path.file_name().and_then(|n| n.to_str()).and_then(|n| n.strip_suffix(".jsonl")) else { continue };
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            let Ok(modified) = path.metadata().and_then(|m| m.modified()) else { continue };
            let mut info = SessionInfo { id: id.to_string(), cwd: String::new(), name: None, title: String::new(), turns: 0, modified };
            let mut subagent = false;
            for entry in text.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()) {
                match entry["type"].as_str() {
                    Some("session") => info.cwd = entry["cwd"].as_str().unwrap_or_default().to_string(),
                    Some("session_info") => info.name = entry["name"].as_str().map(String::from),
                    // Subagent sessions stay out of /resume and --continue; /tasks opens them.
                    Some("parent") => subagent = true,
                    Some("message") if entry["message"]["role"] == "user" && entry["message"]["content"][0]["type"] == "text" => {
                        info.turns += 1;
                        if info.title.is_empty() {
                            info.title = entry["message"]["content"][0]["text"].as_str().unwrap_or_default().lines().next().unwrap_or_default().to_string();
                        }
                    }
                    _ => {}
                }
            }
            if info.turns > 0 && !subagent {
                sessions.push(info);
            }
        }
        sessions.sort_by_key(|s| std::cmp::Reverse(s.modified));
        sessions
    }

    /// Deletes saved sessions untouched for longer than `max_age`. A session open in any process, this one included, holds its lock and stays.
    pub fn prune(dir: &Path, max_age: Duration) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        let cutoff = std::time::SystemTime::now() - max_age;
        for path in entries.filter_map(|e| e.ok()).map(|e| e.path()) {
            if path.extension().is_none_or(|extension| extension != "jsonl") {
                continue;
            }
            let Ok(modified) = path.metadata().and_then(|m| m.modified()) else { continue };
            if modified >= cutoff {
                continue;
            }
            let Ok(file) = File::open(&path) else { continue };
            if file.try_lock().is_ok() {
                // A file that cannot be removed now is tried again at the next start.
                let _ = std::fs::remove_file(&path);
            }
        }
    }

    /// Id of the newest saved session started in `cwd`.
    pub fn latest(dir: &Path, cwd: &Path) -> Result<String, String> {
        let cwd = cwd.display().to_string();
        let newest = Session::list(dir).into_iter().find(|s| s.cwd == cwd);
        newest.map(|s| s.id).ok_or_else(|| format!("No session for {cwd} yet."))
    }

    pub fn rename(&self, name: &str) -> Result<(), String> {
        self.append(&json!({"type": "session_info", "name": name}))
    }

    pub fn append(&self, entry: &Value) -> Result<(), String> {
        let Some(path) = &self.path else { return Ok(()) };
        let mut file = OpenOptions::new().create(true).append(true).mode(0o600).open(path).map_err(|e| format!("Cannot open {}: {e}", path.display()))?;
        // One write call, so a signal exit on another thread cannot leave half a line; writeln! on a File writes piecewise.
        file.write_all(format!("{entry}\n").as_bytes()).map_err(|e| format!("Cannot write {}: {e}", path.display()))
    }

    /// /import: copies an exported session file into `dir` and returns its id for `resume`.
    pub fn import(dir: &Path, file: &Path) -> Result<String, String> {
        let text = std::fs::read_to_string(file).map_err(|e| format!("Cannot read {}: {e}", file.display()))?;
        let header: Value = text.lines().next().and_then(|line| serde_json::from_str(line).ok()).unwrap_or_default();
        let id = header["id"].as_str().unwrap_or_default();
        // The id becomes a file name, so it must not carry a path.
        if header["type"] != "session" || id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Err(format!("{} is not an exported session.", file.display()));
        }
        let target = dir.join(format!("{id}.jsonl"));
        std::fs::create_dir_all(dir).map_err(|e| format!("Cannot create {}: {e}", dir.display()))?;
        // 0600 like `append`: a transcript holds file contents and command output.
        write_new_private(&target, &text).map_err(|e| match e.kind() {
            std::io::ErrorKind::AlreadyExists => format!("Session {id} is already here; /resume {id}."),
            _ => format!("Cannot write {}: {e}", target.display()),
        })?;
        Ok(id.to_string())
    }
}

/// Writes `text` to a new 0600 file; a failed write removes it, since a partial file would pass as "already here" on the next import.
fn write_new_private(path: &Path, text: &str) -> std::io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
    file.write_all(text.as_bytes()).inspect_err(|_| {
        let _ = std::fs::remove_file(path);
    })
}

/// The full id for an exact id or a unique prefix of a saved session's id.
fn resolve_id(dir: &Path, prefix: &str) -> Result<String, String> {
    // Ids are ascii alphanumeric (see `import`); anything else could name a file outside `dir`.
    if prefix.is_empty() || !prefix.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(format!("{prefix:?} is not a session id."));
    }
    if dir.join(format!("{prefix}.jsonl")).is_file() {
        return Ok(prefix.to_string());
    }
    let entries = std::fs::read_dir(dir).map_err(|e| format!("Cannot read {}: {e}", dir.display()))?;
    let ids: Vec<String> = entries.filter_map(|e| e.ok()).filter_map(|e| e.file_name().to_str().and_then(|n| n.strip_suffix(".jsonl")).map(String::from)).filter(|id| id.starts_with(prefix)).collect();
    match ids.as_slice() {
        [id] => Ok(id.clone()),
        [] => Err(format!("No saved session {prefix}.")),
        _ => Err(format!("{prefix} matches {} sessions; use more characters.", ids.len())),
    }
}

/// Index of the assistant message where the kept tail starts: the latest one with at least `keep_tokens` after it, else the latest assistant message.
fn cut_point(messages: &[Value], keep_tokens: u64) -> Option<usize> {
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

/// User prompts that can be rewound to, oldest first.
pub fn rewind_points(messages: &[Value]) -> Vec<RewindPoint> {
    let mut points = Vec::new();
    for (at, message) in messages.iter().enumerate() {
        let Some(blocks) = message["content"].as_array().filter(|_| message["role"] == "user") else { continue };
        if blocks.iter().any(|block| block["type"] == "tool_result") {
            continue;
        }
        // The typed text is the last block; shell output and todo notes come before it.
        let Some(text) = blocks.iter().rev().find_map(|block| block["text"].as_str()) else { continue };
        let images = blocks.iter().filter(|block| block["type"] == "image").cloned().collect();
        points.push(RewindPoint { at, text: text.to_string(), images });
    }
    points
}

/// A file this session's tool calls changed, with its diff from before the first of them to now.
#[derive(Debug)]
pub struct FileChange {
    pub path: String,
    pub diff: String,
}

/// Files this session's tool calls changed that still differ from how they were before the first change.
fn session_changes(snapshots: &Snapshots, checkpoints: &[Checkpoint]) -> Result<Vec<FileChange>, String> {
    let changed = changed_paths(snapshots, checkpoints)?;
    if changed.is_empty() {
        return Ok(Vec::new());
    }
    let now = snapshots.take()?;
    let mut changes = Vec::new();
    for (path, (before, _)) in changed {
        let diff = snapshots.diff(before, &now, &path)?;
        if !diff.is_empty() {
            changes.push(FileChange { path, diff });
        }
    }
    Ok(changes)
}

/// Files a rewind put back, and files it left alone because they changed after the agent last wrote them.
#[derive(Debug, Default, PartialEq)]
pub struct RestoredFiles {
    pub restored: Vec<String>,
    pub kept: Vec<String>,
}

/// Puts each file this session's tool calls from message `index` on changed back as it was before the first of them.
fn restore_files(snapshots: &Snapshots, checkpoints: &[Checkpoint], index: usize) -> Result<RestoredFiles, String> {
    let mut files = RestoredFiles::default();
    let changed = changed_paths(snapshots, checkpoints.iter().filter(|checkpoint| checkpoint.at >= index))?;
    if changed.is_empty() {
        return Ok(files);
    }
    let now = snapshots.take()?;
    let mut deletions = Vec::new();
    for (path, (before, after)) in changed {
        let current = snapshots.blob(&now, &path);
        let before_blob = snapshots.blob(before, &path);
        if current != snapshots.blob(after, &path) {
            files.kept.push(path);
        } else if current != before_blob {
            if before_blob.is_none() {
                deletions.push((path, before));
            } else {
                snapshots.restore(before, &path)?;
                files.restored.push(path);
            }
        }
    }
    // Snapshots skip ignored files, so a path missing from `before` may have been ignored then (an unignored `.env`).
    // Asked after the restores above, so the ignore files are as they were before.
    for (path, before) in deletions {
        if snapshots.ignored(&path)? {
            files.kept.push(path);
        } else {
            snapshots.restore(before, &path)?;
            files.restored.push(path);
        }
    }
    Ok(files)
}

/// Each path `checkpoints` changed, with the snapshot from before the first change and after the last.
fn changed_paths<'a>(snapshots: &Snapshots, checkpoints: impl IntoIterator<Item = &'a Checkpoint>) -> Result<BTreeMap<String, (&'a str, &'a str)>, String> {
    let mut changed = BTreeMap::new();
    for checkpoint in checkpoints {
        for path in snapshots.changed(&checkpoint.before, &checkpoint.after)? {
            let (_, last_after) = changed.entry(path).or_insert((checkpoint.before.as_str(), checkpoint.after.as_str()));
            *last_after = checkpoint.after.as_str();
        }
    }
    Ok(changed)
}

/// The message index of position `view_index` in the compacted view, where the summary takes index 0.
fn message_index(compactions: &[Compaction], view_index: usize) -> usize {
    match compactions.last() {
        Some(compaction) => compaction.cut + view_index - 1,
        None => view_index,
    }
}

/// A message index saved under `key`. Sessions written before 0.2 saved positions in the compacted view under `view_key`.
fn saved_index(entry: &Value, key: &str, view_key: &str, compactions: &[Compaction]) -> Option<usize> {
    if let Some(index) = entry[key].as_u64() {
        return Some(index as usize);
    }
    entry[view_key].as_u64().map(|position| message_index(compactions, position as usize))
}

fn compaction_prompt(conversation: &str, focus: Option<&str>) -> String {
    let prompt = format!("<conversation>\n{conversation}\n</conversation>\n\n{SUMMARY_PROMPT}");
    match focus {
        Some(focus) => format!("{prompt}\n\n<focus>\n{focus}\n</focus>\nGive extra detail to what the focus names."),
        None => prompt,
    }
}

/// `transcript` is the session file, which keeps every message the summary replaces.
fn compacted(messages: &[Value], compactions: &[Compaction], transcript: Option<&Path>) -> Vec<Value> {
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
fn serialize(messages: &[Value]) -> String {
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

fn text_of(message: &Value) -> String {
    let blocks = message["content"].as_array().into_iter().flatten();
    blocks.filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n")
}

fn result_sections(runs: &[Arc<subagent::Run>]) -> String {
    let sections: Vec<String> = runs.iter().map(|r| format!("## {} ({})\n\n{}", r.label, r.id, r.result_text())).collect();
    sections.join("\n\n")
}

/// Stops the runs of the chunk whose spawn failed, and keeps the answers of earlier chunks, which have finished.
fn stop_after_failed_spawn(error: String, started: &[Arc<subagent::Run>], finished: &[Arc<subagent::Run>]) -> String {
    let mut message = error;
    if !started.is_empty() {
        for run in started {
            run.stop();
        }
        // A stopped background run still reports as a follow-up, so the model needs its id.
        let ids: Vec<&str> = started.iter().map(|run| run.id.as_str()).collect();
        message = format!("{message} Stopped the ones already started: {}.", ids.join(", "));
    }
    if !finished.is_empty() {
        message = format!("{message}\n\nEarlier subagents finished:\n\n{}", result_sections(finished));
    }
    message
}

/// Asks `request`'s model for a topic and saves `project｜topic` as the session name.
fn name_session(client: &Client, session: &Session, cwd: &Path, request: &Value) -> Result<String, String> {
    let reply = client.send_quiet(request, &AtomicBool::new(false))?;
    let text = text_of(&reply.message);
    let topic = text.lines().next().unwrap_or_default().trim().trim_matches('"');
    if topic.is_empty() {
        return Err("The model gave no title.".into());
    }
    let title = session_title(cwd, topic);
    session.rename(&title)?;
    Ok(title)
}

fn session_title(cwd: &Path, topic: &str) -> String {
    let project = cwd.file_name().unwrap_or_default().to_string_lossy();
    format!("{project}｜{topic}").chars().take(TITLE_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    use snapshots::tests::{SNAPSHOT_INDEX, git_work};

    fn assistant(text: &str) -> Value {
        json!({"role": "assistant", "content": [{"type": "text", "text": text}]})
    }

    fn user(text: &str) -> Value {
        json!({"role": "user", "content": [{"type": "text", "text": text}]})
    }

    #[test]
    fn cut_point_keeps_recent_tokens_and_starts_at_an_assistant_message() {
        let big = "x".repeat(400);
        let messages = [user("a"), assistant(&big), user(&big), assistant(&big), user("b")];
        assert_eq!(cut_point(&messages, 100), Some(3));
        assert_eq!(cut_point(&messages, 300), Some(1));
        assert_eq!(cut_point(&messages, 10_000), Some(3), "a tail shorter than the budget still compacts up to the latest reply");
        assert_eq!(cut_point(&[user("only")], 1), None);
    }

    #[test]
    fn compacted_history_starts_with_the_summary() {
        let messages = [user("a"), assistant("b"), user("c")];
        let kept = compacted(&messages, &[Compaction { cut: 1, summary: "S".into() }], None);
        assert_eq!(text_of(&json!({"content": kept[0]["content"]})), format!("{SUMMARY_PREFIX}S{SUMMARY_SUFFIX}"));
        assert_eq!(kept[1..], messages[1..]);
    }

    #[test]
    fn compacted_summary_points_at_the_saved_transcript() {
        let kept = compacted(&[user("a"), user("b")], &[Compaction { cut: 1, summary: "S".into() }], Some(Path::new("/s/abc.jsonl")));
        assert!(
            text_of(&json!({"content": kept[0]["content"]}))
                .ends_with("</summary>\n\nThe full conversation before this summary is in /s/abc.jsonl, one JSON entry per line. Search it with rg when the summary lacks a detail you need.")
        );
    }

    #[test]
    fn compaction_prompt_appends_the_focus() {
        assert!(compaction_prompt("c", None).ends_with(SUMMARY_PROMPT));
        assert!(compaction_prompt("c", Some("API decisions")).ends_with("<focus>\nAPI decisions\n</focus>\nGive extra detail to what the focus names."));
    }

    #[test]
    fn serialize_matches_pi_format() {
        let messages = [
            user("fix it"),
            json!({"role": "assistant", "content": [{"type": "thinking", "thinking": "t"}, {"type": "text", "text": "ok"}, {"type": "tool_use", "id": "1", "name": "Bash", "input": {"command": "ls"}}]}),
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "1", "content": "a.rs"}]}),
        ];
        assert_eq!(serialize(&messages), "[User]: fix it\n\n[Assistant]: ok\n\n[Assistant tool calls]: Bash(command=\"ls\")\n\n[Tool result]: a.rs");
    }

    #[test]
    fn resumed_session_gives_the_provider_the_same_condensed_view() {
        let dir = std::env::temp_dir().join(format!("agent-condense-{}", std::process::id()));
        let session = Session::start(&dir, true, Path::new("/w"), "m").unwrap();
        let messages = [
            json!({"role": "assistant", "content": [{"type": "tool_use", "id": "a", "name": "Bash", "input": {}}]}),
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "a", "content": "raw", "is_error": false}]}),
        ];
        for message in &messages {
            session.append(&json!({"type": "message", "message": message})).unwrap();
        }
        let batch = Condensed { refs: vec![("t1".into(), "a".into())], summary: "`t1` ran".into() };
        session.append(&condense::entry(&batch)).unwrap();
        let (_, history) = Session::resume(&dir, &session.id).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(history.messages, messages);
        assert_eq!(condense::view(&history.messages, &history.condensed), condense::view(&messages, &[batch]));
    }

    #[test]
    fn resumed_session_sums_the_time_and_tokens_spent() {
        let dir = std::env::temp_dir().join(format!("agent-spent-{}", std::process::id()));
        let session = Session::start(&dir, true, Path::new("/w"), "m").unwrap();
        session.append(&Spent { input: 100, output: 20, time: Duration::from_millis(1500) }.entry()).unwrap();
        session.append(&Spent { input: 50, output: 5, time: Duration::from_secs(61) }.entry()).unwrap();
        let (_, history) = Session::resume(&dir, &session.id).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(history.spent, Spent { input: 150, output: 25, time: Duration::from_millis(62_500) });
    }

    #[test]
    fn resumed_session_remembers_the_last_account_per_provider() {
        let dir = std::env::temp_dir().join(format!("agent-accounts-{}", std::process::id()));
        let session = Session::start(&dir, true, Path::new("/w"), "m").unwrap();
        session.append(&json!({"type": "usage", "provider": "anthropic", "account": "uuid-home"})).unwrap();
        session.append(&json!({"type": "usage", "provider": "openai-codex", "account": "chatgpt-1"})).unwrap();
        session.append(&json!({"type": "usage", "provider": "anthropic", "account": "uuid-work"})).unwrap();
        session.append(&json!({"type": "usage", "provider": "anthropic", "account": null})).unwrap();
        let (_, history) = Session::resume(&dir, &session.id).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        let expected = HashMap::from([("anthropic".to_string(), "uuid-work".to_string()), ("openai-codex".to_string(), "chatgpt-1".to_string())]);
        assert_eq!(history.accounts, expected);
    }

    #[test]
    fn resumed_session_keeps_the_last_plan_mode() {
        let dir = std::env::temp_dir().join(format!("agent-plan-{}", std::process::id()));
        let session = Session::start(&dir, true, Path::new("/w"), "m").unwrap();
        session.append(&Plan::Drafting.entry()).unwrap();
        session.append(&Plan::Ready("# Plan".into()).entry()).unwrap();
        let (_, history) = Session::resume(&dir, &session.id).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(history.plan, Plan::Ready("# Plan".into()));
    }

    #[test]
    fn resumed_session_drops_rewound_messages_and_their_checkpoints() {
        let dir = std::env::temp_dir().join(format!("agent-rewind-{}", std::process::id()));
        let session = Session::start(&dir, true, Path::new("/w"), "m").unwrap();
        let checkpoint = |at: usize| json!({"type": "tool_checkpoint", "at": at, "before": "t0", "after": "t1"});
        session.append(&json!({"type": "message", "message": user("hi")})).unwrap();
        session.append(&checkpoint(1)).unwrap();
        session.append(&json!({"type": "message", "message": assistant("hello")})).unwrap();
        session.append(&json!({"type": "message", "message": user("look at this image")})).unwrap();
        session.append(&checkpoint(3)).unwrap();
        session.append(&json!({"type": "message", "message": assistant("nice")})).unwrap();
        session.append(&json!({"type": "rewind", "to": 2})).unwrap();
        let (_, history) = Session::resume(&dir, &session.id).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(history.messages, [user("hi"), assistant("hello")]);
        assert_eq!(history.checkpoints, [Checkpoint { at: 1, before: "t0".into(), after: "t1".into() }]);
    }

    #[test]
    fn a_resumed_session_can_rewind_to_a_prompt_the_compaction_summarized() {
        let dir = std::env::temp_dir().join(format!("agent-rewind-compacted-{}", std::process::id()));
        let session = Session::start(&dir, true, Path::new("/w"), "m").unwrap();
        session.append(&json!({"type": "message", "message": user("hi")})).unwrap();
        session.append(&json!({"type": "message", "message": assistant("hello")})).unwrap();
        session.append(&json!({"type": "message", "message": user("more")})).unwrap();
        session.append(&json!({"type": "message", "message": assistant("ok")})).unwrap();
        session.append(&json!({"type": "compaction", "summary": "S", "first_kept": 3})).unwrap();
        let (_, history) = Session::resume(&dir, &session.id).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        let texts: Vec<String> = rewind_points(&history.messages).into_iter().map(|point| point.text).collect();
        assert_eq!(texts, ["hi", "more"]);
    }

    #[test]
    fn resuming_a_compaction_past_the_saved_messages_fails_instead_of_panicking() {
        let dir = std::env::temp_dir().join(format!("agent-bad-compaction-{}", std::process::id()));
        let session = Session::start(&dir, true, Path::new("/w"), "m").unwrap();
        session.append(&json!({"type": "message", "message": user("hi")})).unwrap();
        session.append(&json!({"type": "compaction", "summary": "S", "first_kept": 2})).unwrap();
        let error = Session::resume(&dir, &session.id).err().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(error.ends_with(":3: compaction keeps from message 2 of 1"), "{error}");
    }

    #[test]
    fn resume_restores_the_last_saved_extra_dirs() {
        let dir = std::env::temp_dir().join(format!("agent-extra-dirs-{}", std::process::id()));
        let session = Session::start(&dir, true, Path::new("/w"), "m").unwrap();
        session.append(&json!({"type": "extra_dirs", "dirs": ["/api", "/web"]})).unwrap();
        session.append(&json!({"type": "extra_dirs", "dirs": ["/api"]})).unwrap();
        let history = Session::history(session.path.as_ref().unwrap()).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(history.extra_dirs, [PathBuf::from("/api")]);
    }

    #[test]
    fn resuming_a_session_open_in_another_process_continues_in_a_copy() {
        let dir = std::env::temp_dir().join(format!("agent-open-elsewhere-{}", std::process::id()));
        let open = Session::start(&dir, true, Path::new("/w"), "m").unwrap();
        open.append(&json!({"type": "message", "message": user("hi")})).unwrap();
        let (copy, history) = Session::resume(&dir, &open.id).unwrap();
        let copied = Session::history(copy.path.as_ref().unwrap()).unwrap();
        let id = open.id.clone();
        drop(open);
        // A process other tests spawn holds a copy of the lock fd between its fork and exec on Linux, so retry briefly.
        let mut again = Session::resume(&dir, &id).unwrap().0;
        for _ in 0..400 {
            if again.forked_from.is_none() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
            again = Session::resume(&dir, &id).unwrap().0;
        }
        std::fs::remove_dir_all(&dir).unwrap();
        assert_ne!(copy.id, id);
        assert_eq!(copy.forked_from.as_deref(), Some(id.as_str()));
        assert_eq!((history.messages, copied.messages), (vec![user("hi")], vec![user("hi")]));
        assert_eq!((again.id, again.forked_from), (id, None));
    }

    #[test]
    fn prune_deletes_only_old_sessions_no_process_has_open() {
        let dir = std::env::temp_dir().join(format!("agent-prune-{}", std::process::id()));
        let open = Session::start(&dir, true, Path::new("/w"), "m").unwrap();
        let closed = Session::start(&dir, true, Path::new("/w"), "m").unwrap();
        let (open_path, closed_path) = (open.path.clone().unwrap(), closed.path.clone().unwrap());
        drop(closed);
        let fresh = dir.join("fresh.jsonl");
        std::fs::write(&fresh, "").unwrap();
        let month_ago = std::time::SystemTime::now() - Duration::from_secs(30 * 24 * 3600);
        for path in [&open_path, &closed_path] {
            File::options().write(true).open(path).unwrap().set_modified(month_ago).unwrap();
        }
        Session::prune(&dir, Duration::from_secs(7 * 24 * 3600));
        let left = (open_path.exists(), closed_path.exists(), fresh.exists());
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(left, (true, false, true));
    }

    #[test]
    fn rewind_points_are_typed_prompts_not_tool_results() {
        let tool_result = json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "1", "content": "x"}, {"type": "text", "text": "steer"}]});
        let image = json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AA"}});
        let with_context = json!({"role": "user", "content": [{"type": "text", "text": "$ ls"}, image, {"type": "text", "text": "fix [Image #1]"}]});
        let messages = [user("hi"), assistant("b"), tool_result, with_context];
        let hi = RewindPoint { at: 0, text: "hi".into(), images: Vec::new() };
        let fix = RewindPoint { at: 3, text: "fix [Image #1]".into(), images: vec![image] };
        assert_eq!(rewind_points(&messages), [hi, fix]);
    }

    #[test]
    fn rewind_restores_only_what_this_session_changed() {
        let _index = SNAPSHOT_INDEX.lock().unwrap();
        let (root, work) = git_work("agent-restore");
        let write = |name: &str, text: &str| std::fs::write(work.join(name), text).unwrap();
        let read = |name: &str| std::fs::read_to_string(work.join(name)).ok();
        let snapshots = Snapshots::open(&root, &work).unwrap();
        let tool_call = |at: usize, writes: &[(&str, &str)]| {
            let before = snapshots.take().unwrap();
            for (name, text) in writes {
                write(name, text);
            }
            let after = snapshots.take().unwrap();
            Checkpoint { at, before, after }
        };
        write("a.txt", "v1");
        let first = tool_call(1, &[("a.txt", "v2")]);
        let store = std::fs::read_dir(root.join(".agents/snapshots")).unwrap().next().unwrap().unwrap();
        assert_eq!(std::os::unix::fs::PermissionsExt::mode(&store.metadata().unwrap().permissions()) & 0o777, 0o700, "snapshots hold file contents");
        write("between.txt", "user");
        let second = tool_call(3, &[("a.txt", "v3"), ("new.txt", "agent"), ("shared.txt", "agent")]);
        write("other.txt", "another session");
        let third = tool_call(3, &[("a.txt", "v4")]);
        write("shared.txt", "mine");

        let files = restore_files(&snapshots, &[first, second, third], 2).unwrap();
        let state = [read("a.txt"), read("new.txt"), read("shared.txt"), read("between.txt"), read("other.txt")];
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(files, RestoredFiles { restored: vec!["a.txt".into(), "new.txt".into()], kept: vec!["shared.txt".into()] });
        assert_eq!(state, [Some("v2".into()), None, Some("mine".into()), Some("user".into()), Some("another session".into())]);
    }

    #[test]
    fn rewind_keeps_a_file_that_was_ignored_before_the_tool_call() {
        let _index = SNAPSHOT_INDEX.lock().unwrap();
        let (root, work) = git_work("agent-restore-ignored");
        std::fs::write(work.join(".gitignore"), ".env\n").unwrap();
        std::fs::write(work.join(".env"), "local").unwrap();
        let snapshots = Snapshots::open(&root, &work).unwrap();
        let before = snapshots.take().unwrap();
        std::fs::write(work.join(".gitignore"), "").unwrap();
        let after = snapshots.take().unwrap();
        let checkpoint = Checkpoint { at: 1, before, after };

        let files = restore_files(&snapshots, &[checkpoint], 0).unwrap();
        let env = std::fs::read_to_string(work.join(".env")).ok();
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(files, RestoredFiles { restored: vec![".gitignore".into()], kept: vec![".env".into()] });
        assert_eq!(env, Some("local".into()));
    }

    #[test]
    fn session_title_prefixes_the_project_and_cuts_at_80_code_points() {
        assert_eq!(session_title(Path::new("/src/agent"), "Fix /changes diff view"), "agent｜Fix /changes diff view");
        let title = session_title(Path::new("/src/zażółć"), &"ę".repeat(100));
        assert_eq!(title, format!("zażółć｜{}", "ę".repeat(73)));
    }

    #[test]
    fn changes_list_files_this_session_left_different_with_their_diff() {
        let _index = SNAPSHOT_INDEX.lock().unwrap();
        let (root, work) = git_work("agent-changes");
        let write = |name: &str, text: &str| std::fs::write(work.join(name), text).unwrap();
        let snapshots = Snapshots::open(&root, &work).unwrap();
        let tool_call = |writes: &[(&str, &str)]| {
            let before = snapshots.take().unwrap();
            for (name, text) in writes {
                write(name, text);
            }
            let after = snapshots.take().unwrap();
            Checkpoint { at: 1, before, after }
        };
        write("a.txt", "v1\n");
        write("back.txt", "same\n");
        write("user.txt", "untouched\n");
        let first = tool_call(&[("a.txt", "v2\n"), ("back.txt", "changed\n")]);
        let second = tool_call(&[("back.txt", "same\n")]);
        write("user.txt", "user edit\n");

        let changes = session_changes(&snapshots, &[first, second]).unwrap();
        std::fs::remove_dir_all(&root).unwrap();
        let paths: Vec<&str> = changes.iter().map(|change| change.path.as_str()).collect();
        assert_eq!(paths, ["a.txt"]);
        assert!(changes[0].diff.contains("-v1\n+v2"), "{}", changes[0].diff);
    }

    #[test]
    fn imported_session_resumes_and_a_non_session_file_is_refused() {
        let root = std::env::temp_dir().join(format!("agent-import-{}", std::process::id()));
        let session = Session::start(&root.join("a"), true, Path::new("/repo"), "m").unwrap();
        session.append(&json!({"type": "message", "message": {"role": "user", "content": "hi"}})).unwrap();

        let id = Session::import(&root.join("b"), session.path.as_ref().unwrap()).unwrap();
        let (_, history) = Session::resume(&root.join("b"), &id).unwrap();
        assert_eq!(history.messages, vec![json!({"role": "user", "content": "hi"})]);
        assert!(Session::import(&root.join("b"), session.path.as_ref().unwrap()).is_err());

        let forged = root.join("forged.jsonl");
        std::fs::write(&forged, "{\"type\": \"session\", \"id\": \"../x\"}\n").unwrap();
        assert!(Session::import(&root.join("b"), &forged).is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn resume_refuses_an_id_that_is_not_alphanumeric() {
        let root = std::env::temp_dir().join(format!("agent-resume-id-{}", std::process::id()));
        std::fs::create_dir_all(root.join("sessions")).unwrap();
        let outside = Session::start(&root, true, Path::new("/repo"), "m").unwrap();
        for id in ["", "../", &format!("../{}", outside.id)] {
            assert_eq!(Session::resume(&root.join("sessions"), id).err().unwrap(), format!("{id:?} is not a session id."));
        }
        std::fs::remove_dir_all(&root).unwrap();
    }
}
