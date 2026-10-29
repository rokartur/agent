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
pub mod peers;
pub mod permissions;
pub mod prompt;
pub mod skill_usage;
pub mod snapshots;
pub mod subagent;
pub mod usage;
pub mod workflow;

use crate::api::models::Model;
use crate::api::{self, Client, Delta, Usage};
use crate::process::now_ms;
use crate::settings::Settings;
use crate::tailnet;
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
/// The refused input stays in the context, so "continue" or any retry is refused again.
const REFUSED: &str = "Stopped: the model refused this request, and every retry will refuse too while it stays in the context. Rewind (Esc Esc) to before the message that caused it, or start a new session. A screenshot of the model's own reasoning is a known cause.";
const TOOL_RESULT_SUMMARY_CHARS: usize = 2000;
const COMPACT_MAX_TOKENS: u64 = 128_000;
const SNAPSHOT_FAILED: &str = "File snapshot failed, a rewind will not restore files from this tool call";
const BG_WAIT_SUBAGENTS_ONLY: &str = "WaitForAgents waits for subagents only. A background Bash task (bt-N) reports its exit as a message: keep working or end the turn, or peek with TaskOutput.";
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
    /// A subagent's SendMessage, which the parent reads from the headless JSONL and routes.
    Outbox {
        to: String,
        text: String,
    },
    /// PushNotification: a desktop notification the model asked for.
    Push(String),
    /// Finished background work handed to the model.
    FollowUp {
        source: String,
        text: String,
    },
    TurnEnd {
        usage: Usage,
        context_tokens: u64,
        /// Anthropic's names whatever the provider: `end_turn`, `tool_use`, `max_tokens`, `refusal`, `aborted`.
        stop_reason: String,
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
    /// Ask mode: the agent waits for one Decision on `reply`. `call` is the tool call asking, None for an edit hook's command.
    Permission {
        call: Option<String>,
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

/// Messages typed mid-turn with their pasted images, read after each tool batch.
pub type Steering = Arc<Mutex<Vec<(String, Vec<Value>)>>>;

/// Work that finished in the background; it reaches the model after the current tool batch or turn, or starts a turn when idle.
pub struct FollowUp {
    /// What produced it, e.g. a terminal id; bg_status on that id withdraws it.
    pub source: String,
    pub text: String,
}

#[derive(Clone)]
pub struct WorktreeSession {
    original_cwd: PathBuf,
    /// None for a worktree entered by `path`, which ExitWorktree never removes.
    created: Option<crate::worktree::Created>,
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
    /// Set by EnterWorktree until ExitWorktree; only this process knows it, so a resumed session has none.
    pub worktree_session: Option<WorktreeSession>,
    /// Directories outside `cwd` that belong to the task: --add-dir and /workspace.
    pub extra_dirs: Vec<PathBuf>,
    /// The computer the user's terminal is on, when it is not this one: `serve::USER_DEVICE`.
    pub user_device: Option<String>,
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
    pub steering: Steering,
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
    pub crons: Arc<Mutex<tools::cron::Crons>>,
    /// Shared with the UI for /tasks and the running count above the editor.
    pub terminals: Arc<Mutex<Terminals>>,
    /// Shared with the UI for /tasks.
    pub subagents: Arc<Mutex<Runs>>,
    /// Shared with the UI for /workflows and the task panel.
    pub workflows: Arc<Mutex<Vec<Arc<workflow::Workflow>>>>,
    /// The parent session's id when this agent is a subagent.
    pub parent: Option<String>,
    /// Interactive sessions listen for cross-session messages here.
    pub inbox: Option<peers::Inbox>,
    /// The session's `project｜topic` name, shown to other sessions by ListAgents.
    pub title: String,
    /// The run each subagent label reached in SendMessage, so a newer run reusing the label is not messaged by mistake.
    pub addressed: HashMap<String, String>,
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
    pub file_checkpoints: Vec<FileCheckpoint>,
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

/// One file an edit tool wrote that the work tree snapshots leave out: outside the work tree, or ignored.
/// The blob ids are None when the file did not exist.
#[derive(Clone, Debug, PartialEq)]
pub struct FileCheckpoint {
    pub at: usize,
    pub path: PathBuf,
    pub before: Option<String>,
    pub after: Option<String>,
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
    pub file_checkpoints: Vec<FileCheckpoint>,
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
    /// The agent before plugins, MCP servers and the session's history are loaded into it.
    pub fn new(client: Client, settings: &Settings, home: PathBuf, cwd: PathBuf, session: Session, sessions_dir: PathBuf, parent: Option<String>) -> Agent {
        let agent_dir = home.join(".agents");
        Agent {
            client,
            system: prompt::system_prompt(&agent_dir, &cwd, settings.trusts(&cwd), |skill| settings.skill_enabled(skill), settings.prefer_global()),
            trusted_projects: settings.trusted_projects.clone(),
            modes: modes::prompt(&home, &settings.ponytail, &settings.caveman),
            // agent.load fills it, with the session's swarm prompt.
            tools: Value::Null,
            anchors: Anchors::new(&session.id),
            rtk: if settings.rtk { Rtk::ON } else { Rtk::OFF },
            nested_instructions: HashSet::new(),
            worktree_session: None,
            extra_dirs: Vec::new(),
            user_device: std::env::var(crate::serve::USER_DEVICE).ok(),
            shell_context: Vec::new(),
            images: Vec::new(),
            model: settings.model.clone(),
            effort: settings.thinking.clone(),
            // settle_model fills these four and condense once the model lists arrive.
            context_window: 0,
            max_tokens: 0,
            cheap_model: String::new(),
            reviewer_model: String::new(),
            speed: settings.speed.clone(),
            cache_ttl: settings.cache_ttl.clone(),
            auto_compact: settings.auto_compact,
            reserve_tokens: settings.reserve_tokens,
            keep_recent_tokens: settings.keep_recent_tokens,
            last_context_tokens: 0,
            condense: condense::Config::new(settings, ""),
            condensed: Vec::new(),
            next_ref: 1,
            pending_condense: Vec::new(),
            pending_title: None,
            usage: Usage::default(),
            turns: 0,
            tool_calls: 0,
            abort: Arc::default(),
            steering: Arc::new(Mutex::new(Vec::new())),
            permission_mode: Arc::new(Mutex::new(settings.permission_mode)),
            grants: Grants::default(),
            todos: Arc::default(),
            goal: Arc::default(),
            plan: Arc::default(),
            follow_ups: Arc::default(),
            crons: Arc::default(),
            terminals: Arc::default(),
            web: tools::web::Web::new(&home, tools::web::store_dir()),
            subagents: Arc::default(),
            workflows: Arc::default(),
            parent,
            inbox: None,
            title: String::new(),
            addressed: HashMap::new(),
            subagent_concurrency: settings.subagent_concurrency,
            subagent_effort: settings.subagent_effort.clone(),
            subagent_model: settings.subagent_model.clone(),
            only_tools: None,
            spent: Spent::default(),
            interactive: false,
            plugins: crate::plugins::Plugins::new(&home),
            mcp: crate::mcp::Mcp::new(&home, &cwd),
            snapshots: None,
            lsp: lsp::Lsp::new(settings.lsp.clone()),
            checkpoints: Vec::new(),
            file_checkpoints: Vec::new(),
            compactions: Vec::new(),
            agent_dir,
            home,
            cwd,
            messages: Vec::new(),
            session,
            sessions_dir,
        }
    }

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
        self.describe_to_peers(true);
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
            self.title = title.clone();
            emit(Event::Titled(title));
        }
        self.describe_to_peers(false);
        if let Some(inbox) = &self.inbox {
            inbox.went_idle(&self.session.id, &self.cwd);
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
            let mut request = api::request(&self.model, self.max_tokens, &self.effort, &self.cache_ttl, &system, &self.cwd_section(), &tools, &messages);
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
            emit(Event::TurnEnd { usage: reply.usage, context_tokens: reply.context_tokens, stop_reason: reply.stop_reason.clone() });
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
                if reply.stop_reason == "refusal" {
                    emit(Event::Notice(REFUSED.into()));
                    return Ok(answer);
                }
                if reply.stop_reason == "max_tokens" {
                    emit(Event::Notice("Stopped: max_tokens".into()));
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
            for bad in &reply.bad_tool_inputs {
                self.session.append(&json!({"type": "bad_tool_input", "id": bad.id, "error": bad.error, "raw": bad.raw}))?;
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
                let bad_input = reply.bad_tool_inputs.iter().find(|bad| bad.id == id);
                let (mut output, is_error, image) = if let Some(bad) = bad_input {
                    (format!("{} The call did not run; send it again with valid JSON.", bad.error), true, None)
                } else if let Some(error) = tools::undeclared_params(&self.tools, &name, &input) {
                    (format!("{error} The call did not run; send it again with only the parameters its schema lists."), true, None)
                } else if let Some(path) = self.requested_image(&name, &input) {
                    match crate::image::block(&path) {
                        Ok(block) => (format!("[{} is attached as an image.]", path.display()), false, Some(block)),
                        Err(error) => (error, true, None),
                    }
                } else {
                    let (output, is_error) = self.run_tool_with_checkpoint(&id, &name, &input, &targets, text, emit)?;
                    (output, is_error, None)
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
            if let Some(refusal) = self.check_permission(None, "Bash", &approved, &[], user_request, emit) {
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
    fn run_tool(&mut self, id: &str, name: &str, input: &Value, targets: &[PathBuf], user_request: &str, emit: &mut dyn FnMut(Event)) -> (String, bool) {
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
            let (output, is_error) = self.run_tool(id, "Read", &read, &[], user_request, emit);
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
        if let Some(reason) = self.check_permission(Some(id), name, input, targets, user_request, emit) {
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
            "EnterPlanMode" => self.enter_plan(emit),
            "ExitPlanMode" => self.complete_plan(input),
            "Skill" => self.skill(input),
            "EnterWorktree" => self.enter_worktree(input, emit),
            "ExitWorktree" => self.exit_worktree(input, emit),
            "Goal" => self.goal_tool(input),
            "Bash" if input["run_in_background"] == true => self.terminals.lock().unwrap().start(input, &self.cwd, &self.follow_ups),
            "TaskOutput" => self.terminals.lock().unwrap().status_of(input, &self.follow_ups),
            "TaskStop" => self.task_stop(input),
            "SendMessage" => self.send_message(input, emit),
            "ListAgents" => self.list_agents(),
            "Agent" => self.subagent(input, emit),
            "Workflow" => self.workflow(input),
            "WaitForAgents" => self.bg_wait(input),
            "CronCreate" if self.interactive => tools::cron::create(&self.crons, input, &self.follow_ups),
            "ScheduleWakeup" if self.interactive => tools::cron::wakeup(&self.crons, input, &self.follow_ups),
            "CronCreate" | "ScheduleWakeup" => Err("Scheduling needs the interactive UI; this process ends with its prompt.".into()),
            "CronDelete" => self.crons.lock().unwrap().delete(input),
            "CronList" => Ok(self.crons.lock().unwrap().list()),
            "PushNotification" => self.push_notification(input, emit),
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
        for (text, images) in steering {
            emit(Event::Steered(text.clone()));
            blocks.extend(images);
            blocks.push(json!({"type": "text", "text": text}));
        }
        for FollowUp { source, text } in follow_ups {
            blocks.push(json!({"type": "text", "text": text}));
            emit(Event::FollowUp { source, text });
        }
        blocks
    }

    /// Work whose result has yet to reach the model. Follow-ups are read last: a run queues its own while it flips to done.
    pub fn has_background_work(&self) -> bool {
        self.workflows.lock().unwrap().iter().any(|run| run.is_running())
            || self.subagents.lock().unwrap().list.iter().any(|run| run.background && run.is_running())
            || !self.follow_ups.lock().unwrap().is_empty()
    }

    /// Starts a turn from queued follow-ups while idle; Ok("") when another turn already took them.
    pub fn run_follow_ups(&mut self, emit: &mut dyn FnMut(Event)) -> Result<String, String> {
        let mut texts = Vec::new();
        for block in self.take_queued(emit) {
            match block["text"].as_str() {
                Some(text) => texts.push(text.to_string()),
                None => self.images.push(block),
            }
        }
        if texts.is_empty() {
            return Ok(String::new());
        }
        self.pursue(&texts.join("\n\n"), emit)
    }

    pub fn describe_to_peers(&self, busy: bool) {
        if let Some(inbox) = &self.inbox {
            inbox.describe(&self.session.id, &self.title, &self.cwd, busy);
        }
    }

    /// TaskStop: `sa-N` stops a subagent, `wf-N` a workflow, anything else names a background terminal.
    fn task_stop(&self, input: &Value) -> Result<String, String> {
        let ids: Vec<&str> = input["task_ids"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        let (workflows, ids): (Vec<&str>, Vec<&str>) = ids.iter().partition(|id| id.starts_with("wf-"));
        let (agents, terminals): (Vec<&str>, Vec<&str>) = ids.iter().partition(|id| id.starts_with("sa-"));
        let mut lines = Vec::new();
        for id in workflows {
            let run = self.workflows.lock().unwrap().iter().find(|run| run.id == id).cloned().ok_or_else(|| format!("No workflow {id}."))?;
            run.stop();
            lines.push(format!("Stopped {} ({}) and its agents. Relaunch with resumeFromRunId {} to reuse its finished agents.", run.id, run.name, run.run_id));
        }
        for id in agents {
            let run = self.subagents.lock().unwrap().named(id).ok_or_else(|| format!("No subagent {id}."))?;
            run.stop();
            lines.push(format!("Stopped {} ({}). SendMessage resumes it once it has exited.", run.id, run.label));
        }
        if !terminals.is_empty() || lines.is_empty() {
            let targets = self.terminals.lock().unwrap().for_kill(&json!({"task_ids": terminals}))?;
            lines.push(bg::kill(&targets, &self.follow_ups, &self.abort));
        }
        Ok(lines.join("\n"))
    }

    fn send_message(&mut self, input: &Value, emit: &mut dyn FnMut(Event)) -> Result<String, String> {
        let to = input["to"].as_str().map(|to| to.trim().trim_start_matches('@')).filter(|to| !to.is_empty()).ok_or("SendMessage needs `to`.")?;
        let text = input["message"].as_str().unwrap_or_default();
        let notify_when_idle = input["notify_when_idle"] == true;
        if text.is_empty() && !notify_when_idle {
            return Err("SendMessage needs a message.".into());
        }
        if self.parent.is_some() {
            emit(Event::Outbox { to: to.to_string(), text: text.to_string() });
            return Ok(format!("Sent to {to}."));
        }
        if to == "main" {
            return Err("You are main, the agent the user talks to.".into());
        }
        let run = self.subagents.lock().unwrap().named(to);
        if let Some(run) = run {
            if text.is_empty() {
                return Err("SendMessage needs a message; notify_when_idle is for other sessions.".into());
            }
            if run.id != to {
                if let Some(earlier) = self.addressed.get(to).filter(|earlier| **earlier != run.id) {
                    return Err(format!(
                        "{to} now reaches {}, a newer agent than {earlier}, which you messaged before. Send to {earlier} for the earlier one or to {} for the newer one.",
                        run.id, run.id
                    ));
                }
                self.addressed.insert(to.to_string(), run.id.clone());
            }
            return subagent::send(&self.subagents, &run.id, text, None, &self.follow_ups);
        }
        if let Some((environment, session)) = to.split_once(':') {
            let settings = crate::settings::load(&self.home)?;
            if environment != tailnet::LOCAL && tailnet::environments(&settings).iter().any(|known| known == environment) {
                if notify_when_idle {
                    return Err("notify_when_idle works only for sessions on this computer.".into());
                }
                return tailnet::message(&settings, environment, session, &self.session.id, &self.cwd, text);
            }
        }
        let peer = peers::find(&self.home, to)?.ok_or_else(|| format!("No subagent or session named {to}. ListAgents lists the ones you can message."))?;
        let reply_to = self.inbox.as_ref().map(peers::Inbox::socket);
        if notify_when_idle && reply_to.is_none() {
            return Err("Only an interactive session can get idle notices.".into());
        }
        peers::send(&peer, &self.session.id, &self.cwd, text, reply_to.as_deref().filter(|_| notify_when_idle))
    }

    fn list_agents(&self) -> Result<String, String> {
        let mut sections = Vec::new();
        let subagents: Vec<String> = self.subagents.lock().unwrap().list.iter().filter(|run| run.workflow.is_none()).map(|run| format!("- {} ({}), {}", run.id, run.label, run.status())).collect();
        if !subagents.is_empty() {
            sections.push(format!("Subagents (`to`: id or label; a finished one resumes in the background):\n{}", subagents.join("\n")));
        }
        let sessions: Vec<String> = peers::list(&self.home).iter().map(peers::Peer::line).collect();
        if !sessions.is_empty() {
            sections.push(format!("Other sessions on this machine (`to`: session id or its prefix, title, or pid):\n{}", sessions.join("\n")));
        }
        let (remote, _) = tailnet::listing(&self.home, &crate::settings::load(&self.home)?);
        let remote: Vec<String> = remote
            .iter()
            .filter(|listed| listed.reachable)
            .map(|listed| {
                let state = if listed.busy { "working" } else { "idle" };
                let title = if listed.title.is_empty() { String::new() } else { format!(" \"{}\"", listed.title) };
                format!("- session {}:{}{title}, {state}, in {}", listed.environment, listed.session, listed.cwd)
            })
            .collect();
        if !remote.is_empty() {
            sections.push(format!("Sessions on your other computers (`to`: <computer>:<session id or its prefix or title>):\n{}", remote.join("\n")));
        }
        if sections.is_empty() {
            return Ok("No other agents. Subagents you start with Agent and other interactive sessions on this and your other computers show here.".into());
        }
        Ok(sections.join("\n\n"))
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
                    web: false,
                };
                // The tool reports failure, so this chunk's runs must not keep going; earlier chunks have finished.
                match subagent::spawn(&self.subagents, launch, background, None, &self.follow_ups) {
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
            return Ok(format!("Started {} in the background. Each answer arrives as a message when it finishes, or WaitForAgents waits for them; /tasks shows progress.", names.join(", ")));
        }
        if let [run] = runs.as_slice() {
            return Ok(run.result_text());
        }
        Ok(result_sections(&runs))
    }

    /// Workflow: resolves the script (scriptPath, then script, then a saved or bundled name) and starts it in the background.
    fn workflow(&mut self, input: &Value) -> Result<String, String> {
        let script = match (input["scriptPath"].as_str(), input["script"].as_str(), input["name"].as_str()) {
            (Some(path), _, _) => {
                let path = self.cwd.join(path);
                std::fs::read_to_string(&path).map_err(|e| format!("Cannot read {}: {e}", path.display()))?
            }
            (None, Some(script), _) => script.to_string(),
            (None, None, Some(name)) => {
                let name = name.trim_start_matches('/');
                self.saved_workflow(name).ok_or_else(|| format!("No workflow named {name}."))?.script
            }
            (None, None, None) => match input["resumeFromRunId"].as_str() {
                Some(run_id) => {
                    let path = workflow::run_dir(&self.workflow_runs_dir(), run_id)?.join("script.js");
                    std::fs::read_to_string(&path).map_err(|_| format!("Run {run_id}: nothing to resume. Start the workflow over as a new run."))?
                }
                None => return Err("Workflow needs `script`, `name`, `scriptPath` or `resumeFromRunId`.".into()),
            },
        };
        let mode = *self.permission_mode.lock().unwrap();
        let default_model = if self.subagent_model.is_empty() { &self.model } else { &self.subagent_model };
        let models: Vec<(String, Option<String>)> =
            self.client.catalog().iter().map(|model| (model.id.clone(), self.client.account(&model.provider).map(|account| format!("{}={}", model.provider, account.0)))).collect();
        let account = models.iter().find(|(id, _)| id == default_model).and_then(|(_, account)| account.clone());
        let base = Launch {
            label: String::new(),
            task: String::new(),
            cwd: self.cwd.clone(),
            model: default_model.clone(),
            thinking: self.effort.clone(),
            mode,
            parent_session: self.session.id.clone(),
            explore: false,
            account,
            extra_dirs: self.extra_dirs.clone(),
            trusted: prompt::is_trusted(&self.trusted_projects, &self.cwd),
            web: true,
        };
        let host = workflow::Host { runs: self.subagents.clone(), follow_ups: self.follow_ups.clone(), base, models };
        let runs_dir = self.workflow_runs_dir();
        let run = workflow::launch(&self.workflows, script, input["args"].clone(), input["resumeFromRunId"].as_str(), &runs_dir, host)?;
        Ok(json!({
            "status": "async_launched",
            "taskId": run.id,
            "taskType": "local_workflow",
            "workflowName": run.name,
            "runId": run.run_id,
            "summary": run.description,
            "transcriptDir": run.script_path.parent(),
            "scriptPath": run.script_path,
            "note": "The run's result arrives as a message when it finishes. /workflows shows its progress; TaskStop stops it."
        })
        .to_string())
    }

    /// Saved workflows this session can run by name: the project's when it is trusted, the personal ones, the bundled ones.
    pub fn saved_workflows(&self) -> Vec<workflow::Saved> {
        let trusted = prompt::is_trusted(&self.trusted_projects, &self.cwd);
        workflow::saved(&prompt::project_agents_dirs(&self.cwd, trusted), &self.agent_dir)
    }

    fn saved_workflow(&self, name: &str) -> Option<workflow::Saved> {
        self.saved_workflows().into_iter().find(|saved| saved.meta.name == name)
    }

    /// One directory per run next to the session file, so a resumed session can still relaunch its runs.
    fn workflow_runs_dir(&self) -> PathBuf {
        match &self.session.path {
            Some(path) => path.with_extension("").join("workflows"),
            None => std::env::temp_dir().join("agent-workflows").join(&self.session.id),
        }
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

    /// Claude Code's EnterPlanMode: the user consents before plan mode starts.
    fn enter_plan(&mut self, emit: &mut dyn FnMut(Event)) -> Result<String, String> {
        if *self.plan.lock().unwrap() != Plan::Off {
            return Err("Already in plan mode.".into());
        }
        if !self.interactive {
            return Err(ask::NO_UI.into());
        }
        let questions = ask::parse(&json!({"questions": [{
            "question": "Enter plan mode? The agent explores and designs an approach for your approval before changing anything.",
            "header": "Plan mode",
            "options": [
                {"label": "Yes, enter plan mode", "description": "Read-only until you approve a plan"},
                {"label": "No, start implementing", "description": "Skip planning and work on the task now"}
            ]
        }]}))?;
        let (reply, answers) = mpsc::channel();
        emit(Event::Ask { questions, reply });
        match answers.recv().unwrap_or_default().first() {
            Some(Some(ask::Answer::Picked(0))) => {}
            Some(Some(ask::Answer::Typed(text))) => return Err(format!("User declined to enter plan mode and said: {text}")),
            _ => return Err("User declined to enter plan mode. Continue with the task without planning.".into()),
        }
        *self.plan.lock().unwrap() = Plan::Drafting;
        self.save_plan()?;
        Ok("Entered plan mode. You should now focus on exploring the codebase and designing an implementation approach.\n\nIn plan mode, you should:\n1. Thoroughly explore the codebase to understand existing patterns\n2. Identify similar features and architectural approaches\n3. Consider multiple approaches and their trade-offs\n4. Use AskUserQuestion if you need to clarify the approach\n5. Design a concrete implementation strategy\n6. When ready, use ExitPlanMode to present your plan for approval\n\nRemember: DO NOT write or edit any files yet. This is a read-only exploration and planning phase.".into())
    }

    /// Looks the skill up the way the system prompt lists it, so a name outside that list never reads a file.
    fn skill(&self, input: &Value) -> Result<String, String> {
        let name = input["skill"].as_str().unwrap_or_default().trim().trim_start_matches('/');
        let args = input["args"].as_str().unwrap_or_default();
        let settings = crate::settings::load(&self.home)?;
        let mut skills = prompt::load_skills(&self.agent_dir, &self.cwd, settings.trusts(&self.cwd), |skill| settings.skill_enabled(skill), settings.prefer_global());
        skills.retain(|skill| skill.model_invocable);
        let skill = skills.iter().find(|skill| skill.name == name).ok_or_else(|| format!("Unknown skill: {name}"))?;
        self.record_skill_read(&skill.path);
        prompt::expand_skill(&format!("/skill:{name} {args}"), &skills)
    }

    fn enter_worktree(&mut self, input: &Value, emit: &mut dyn FnMut(Event)) -> Result<String, String> {
        match (input["name"].as_str(), input["path"].as_str()) {
            (Some(_), Some(_)) => Err("name and path are mutually exclusive.".into()),
            (None, Some(path)) => self.switch_worktree(path, emit),
            (name, None) => self.create_worktree(name, emit),
        }
    }

    fn create_worktree(&mut self, name: Option<&str>, emit: &mut dyn FnMut(Event)) -> Result<String, String> {
        if self.worktree_session.is_some() {
            return Err("Already in a worktree session. Use ExitWorktree first, or pass path to switch into another existing worktree.".into());
        }
        let name = name.map_or_else(|| format!("wt-{:x}", now_ms()), str::to_string);
        // The name becomes a directory and a branch: no `/` to leave .agents/worktrees, no leading `-` for git to read as an option.
        let valid = (1..=64).contains(&name.len()) && !name.starts_with(['-', '.']) && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
        if !valid {
            return Err("name must be 1-64 letters, digits, dots, underscores or dashes, not starting with a dot or dash.".into());
        }
        let created = crate::worktree::create(&self.cwd, &name)?;
        let original_cwd = self.cwd.clone();
        self.move_to(created.path.clone(), emit)?;
        let text = format!("Created worktree at {} on branch {}. The session is now working in the worktree. Use ExitWorktree to leave it.", created.path.display(), created.branch);
        self.worktree_session = Some(WorktreeSession { original_cwd, created: Some(created) });
        Ok(text)
    }

    fn switch_worktree(&mut self, path: &str, emit: &mut dyn FnMut(Event)) -> Result<String, String> {
        let typed = self.cwd.join(path);
        let path = typed.canonicalize().map_err(|e| format!("Cannot open {}: {e}", typed.display()))?;
        let (worktrees, current) = crate::worktree::list(&self.cwd)?;
        if !worktrees.iter().any(|worktree| worktree.path.canonicalize().is_ok_and(|known| known == path)) {
            return Err(format!("{} is not in `git worktree list` of this repository.", path.display()));
        }
        if path == current {
            return Ok(format!("Already in {}.", path.display()));
        }
        let original_cwd = self.worktree_session.as_ref().map_or_else(|| self.cwd.clone(), |session| session.original_cwd.clone());
        self.move_to(path.clone(), emit)?;
        self.worktree_session = Some(WorktreeSession { original_cwd, created: None });
        Ok(format!("Switched to worktree at {}. ExitWorktree will not remove it; use action \"keep\" to return.", path.display()))
    }

    fn exit_worktree(&mut self, input: &Value, emit: &mut dyn FnMut(Event)) -> Result<String, String> {
        let remove = match input["action"].as_str() {
            Some("keep") => false,
            Some("remove") => true,
            _ => return Err("action must be \"keep\" or \"remove\".".into()),
        };
        let Some(session) = self.worktree_session.clone() else {
            return Ok("No active worktree session to exit. ExitWorktree only operates on worktrees created by EnterWorktree in this session; nothing was changed.".into());
        };
        let worktree = self.cwd.clone();
        let created = match (&session.created, remove) {
            (_, false) => None,
            (None, true) => return Err("This worktree was entered by path, so ExitWorktree will not remove it. Use action \"keep\".".into()),
            (Some(created), true) => Some(created),
        };
        if let Some(created) = created
            && input["discard_changes"] != true
        {
            let changes = crate::worktree::changes(created)?;
            if !changes.is_empty() {
                return Err(format!(
                    "The worktree has uncommitted files or commits not on the original branch:\n{changes}\nConfirm with the user, then call again with discard_changes: true, or use action \"keep\"."
                ));
            }
        }
        self.move_to(session.original_cwd.clone(), emit)?;
        self.worktree_session = None;
        let back = format!("Exited worktree. The session is back in {}.", session.original_cwd.display());
        let Some(created) = created else {
            return Ok(format!("{back} The worktree at {} was kept.", worktree.display()));
        };
        match crate::worktree::discard(&session.original_cwd, created) {
            Ok(()) => Ok(format!("{back} Removed the worktree at {} and its branch {}.", created.path.display(), created.branch)),
            Err(error) => Err(format!("{back} Removing the worktree at {} failed: {error}", created.path.display())),
        }
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

    fn push_notification(&self, input: &Value, emit: &mut dyn FnMut(Event)) -> Result<String, String> {
        if !self.interactive {
            return Err(ask::NO_UI.into());
        }
        let message = input["message"].as_str().map(str::trim).filter(|m| !m.is_empty()).ok_or("message required.")?;
        emit(Event::Push(message.to_string()));
        Ok("Sent. It shows only while the terminal is in the background and notifications are on in /settings.".into())
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
    fn check_permission(&mut self, call: Option<&str>, tool: &str, input: &Value, edited_files: &[PathBuf], user_request: &str, emit: &mut dyn FnMut(Event)) -> Option<String> {
        // A saved workflow is granted by its file, so a project workflow that later shadows the same name asks again.
        let saved_file: Vec<PathBuf> = match (tool, input["name"].as_str()) {
            ("Workflow", Some(name)) => self.saved_workflow(name.trim_start_matches('/')).and_then(|saved| saved.path).into_iter().collect(),
            _ => Vec::new(),
        };
        let files = if tool == "Workflow" { &saved_file } else { edited_files };
        let request = permissions::request_for(tool, input, files)?;
        let mode = *self.permission_mode.lock().unwrap();
        if mode == Mode::FullAccess || self.grants.allows(&request) {
            return None;
        }
        if mode == Mode::Auto {
            return self.review(user_request, &request, input, emit);
        }
        let (reply, answer) = mpsc::channel();
        emit(Event::Permission { call: call.map(String::from), request: request.clone(), reply });
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
        let files = self.snapshots.as_ref().map(|snapshots| restore_files(snapshots, &self.checkpoints, &self.file_checkpoints, index)).transpose()?;
        self.forget_from(index)?;
        Ok(files)
    }

    /// Drops message `index` and every later one from the context, and leaves the files as they are.
    pub fn forget_from(&mut self, index: usize) -> Result<(), String> {
        self.checkpoints.retain(|checkpoint| checkpoint.at < index);
        self.file_checkpoints.retain(|checkpoint| checkpoint.at < index);
        self.compactions.retain(|compaction| compaction.cut <= index);
        self.messages.truncate(index);
        self.session.append(&json!({"type": "rewind", "index": index}))?;
        self.estimate_context_tokens();
        Ok(())
    }

    /// Snapshots around a tool that can change files, so a rewind touches only what this session changed.
    /// A file another session edits while this call runs still counts as this session's.
    fn run_tool_with_checkpoint(&mut self, id: &str, name: &str, input: &Value, targets: &[PathBuf], user_request: &str, emit: &mut dyn FnMut(Event)) -> Result<(String, bool), String> {
        const CHANGES_FILES: [&str; 6] = ["Write", "ReplaceLines", "Edit", "InsertLines", "Bash", "Agent"];
        // Before the snapshot: the move swaps the snapshot repo, so before and after would come from different checkouts.
        if name == "Bash"
            && let Some(dir) = tools::cd_target(input["command"].as_str().unwrap_or_default())
        {
            // The auto reviewer trusts calls inside the working directory, so moving it is a call to review.
            if let Some(reason) = self.check_permission(Some(id), name, input, targets, user_request, emit) {
                return Ok((reason, true));
            }
            return Ok(match self.cd(dir, emit) {
                Ok(text) => (text, false),
                Err(text) => (text, true),
            });
        }
        let Some(snapshots) = self.snapshots.as_ref().filter(|_| CHANGES_FILES.contains(&name)) else {
            return Ok(self.run_tool(id, name, input, targets, user_request, emit));
        };
        let files_before = match save_files_left_out(snapshots, targets) {
            Ok(files) => files,
            Err(error) => {
                emit(Event::Notice(format!("{SNAPSHOT_FAILED}: {error}")));
                Vec::new()
            }
        };
        let before = match snapshots.take() {
            Ok(before) => before,
            Err(error) => {
                emit(Event::Notice(format!("{SNAPSHOT_FAILED}: {error}")));
                return Ok(self.run_tool(id, name, input, targets, user_request, emit));
            }
        };
        let result = self.run_tool(id, name, input, targets, user_request, emit);
        let snapshots = self.snapshots.as_ref().expect("checked at the top of run_tool_with_checkpoint");
        let at = self.messages.len();
        for (path, before) in files_before {
            match snapshots.save_file(&path) {
                Ok(after) if after == before => {}
                Ok(after) => {
                    self.session.append(&json!({"type": "file_checkpoint", "index": at, "path": path, "before": before, "after": after}))?;
                    self.file_checkpoints.push(FileCheckpoint { at, path, before, after });
                }
                Err(error) => emit(Event::Notice(format!("{SNAPSHOT_FAILED}: {error}"))),
            }
        }
        match snapshots.take() {
            Ok(after) if after == before => {}
            Ok(after) => {
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
        let summary = self.summarize(&view[..first_kept], focus)?;
        let cut = message_index(&self.compactions, first_kept);
        self.session.append(&json!({"type": "compaction", "summary": summary, "cut": cut}))?;
        self.compactions.push(Compaction { cut, summary: summary.clone() });
        self.last_context_tokens = 0;
        Ok(Some(summary))
    }

    /// /handoff: summarizes the whole conversation into the first prompt of a fresh session; `goal` names its next task.
    pub fn handoff_prompt(&mut self, goal: Option<&str>) -> Result<String, String> {
        let view = self.compacted();
        if view.is_empty() {
            return Err("Nothing to hand off yet.".into());
        }
        let summary = self.summarize(&view, goal)?;
        Ok(handoff_prompt(&summary, self.session.path.as_deref(), goal))
    }

    fn summarize(&mut self, messages: &[Value], focus: Option<&str>) -> Result<String, String> {
        let prompt = compaction_prompt(&serialize(messages), focus);
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
            return Err(format!("Summary stopped with {}.", reply.stop_reason));
        }
        let summary = text_of(&reply.message);
        if summary.trim().is_empty() {
            return Err("The summary came back empty.".into());
        }
        Ok(summary)
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
        self.system = prompt::system_prompt(&self.agent_dir, &self.cwd, settings.trusts(&self.cwd), |skill| settings.skill_enabled(skill), settings.prefer_global());
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

    /// --add-dir and /workspace add: `path` is relative to cwd or starts with `~`; saved so a resume restores it. Returns the notice to show.
    pub fn add_dir(&mut self, path: &str) -> Result<String, String> {
        let dir = tools::hashline::resolve(&self.cwd, path).canonicalize().map_err(|e| format!("Cannot open {path}: {e}"))?;
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
        let typed = tools::hashline::resolve(&self.cwd, path);
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
        self.system = prompt::system_prompt(&self.agent_dir, &cwd, settings.trusts(&cwd), |skill| settings.skill_enabled(skill), settings.prefer_global());
        self.anchors = Anchors::new(&session.id);
        self.nested_instructions.clear();
        self.checkpoints.clear();
        self.file_checkpoints.clear();
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
        // A one-shot run (-p, --headless, acp, a child) has no UI to ask, plan, notify or schedule in: these tools only fail there.
        if !self.interactive {
            const NEEDS_UI: [&str; 7] = ["AskUserQuestion", "EnterPlanMode", "PushNotification", "CronCreate", "CronDelete", "CronList", "ScheduleWakeup"];
            list.retain(|tool| !NEEDS_UI.contains(&tool["name"].as_str().unwrap_or_default()));
        }
        // prompt::system_prompt lists the skills under this tag; with none, Skill has nothing to load.
        if !self.system.contains("<skills>\n") {
            list.retain(|tool| tool["name"] != "Skill");
        }
        if let Some(only) = &self.only_tools {
            list.retain(|tool| only.iter().any(|name| tool["name"] == name.as_str()));
        }
        (system, tools)
    }

    fn cwd_section(&self) -> String {
        prompt::cwd_section(&self.cwd, &self.extra_dirs, self.user_device.as_deref())
    }

    /// The last request's context tokens split across `context::PARTS`.
    pub fn context_split(&self) -> context::PerPart {
        let (system, tools) = self.system_and_tools();
        let system = format!("{system}{}", self.cwd_section());
        context::split(&system, &tools, &self.provider_view(), self.last_context_tokens)
    }

    /// Set after a resume or rewind, so auto-compact runs before a first request that would not fit.
    fn estimate_context_tokens(&mut self) {
        let (system, tools) = self.system_and_tools();
        let system = format!("{system}{}", self.cwd_section());
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
        crate::image::fit_history(&mut self.messages);
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
        self.file_checkpoints = history.file_checkpoints;
        self.compactions = history.compactions;
        self.extra_dirs = history.extra_dirs;
        self.estimate_context_tokens();
    }

    /// The messages with everything before the latest compaction replaced by its summary.
    fn compacted(&self) -> Vec<Value> {
        compacted(&self.messages, &self.compactions, self.session.path.as_deref())
    }

    fn provider_view(&self) -> Vec<Value> {
        answer_missing_results(condense::view(&self.compacted(), &self.condensed))
    }

    /// Tells plugins the session started; needs the settled model for the context's provider.
    pub fn announce_session(&self) {
        // `agent attach` and `agent -r` find a running session by the id on its card.
        self.describe_to_peers(false);
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

/// The API rejects a whole request when any tool call lacks a result in the next message, so one call cut off
/// mid-run (the process quit or crashed before its result was saved) would make every later prompt a 400.
fn answer_missing_results(mut messages: Vec<Value>) -> Vec<Value> {
    let mut index = 0;
    while index < messages.len() {
        let calls: Vec<Value> = match messages[index]["role"].as_str() {
            Some("assistant") => messages[index]["content"].as_array().into_iter().flatten().filter(|b| b["type"] == "tool_use").map(|b| b["id"].clone()).collect(),
            _ => Vec::new(),
        };
        index += 1;
        if calls.is_empty() {
            continue;
        }
        if messages.get(index).is_none_or(|next| next["role"] != "user") {
            messages.insert(index, json!({"role": "user", "content": []}));
        }
        let next = &mut messages[index]["content"];
        if let Some(text) = next.as_str() {
            *next = json!([{"type": "text", "text": text}]);
        }
        let content = next.as_array_mut().expect("user content is a string or an array");
        let missing: Vec<Value> = calls
            .into_iter()
            .filter(|id| !content.iter().any(|b| b["type"] == "tool_result" && b["tool_use_id"] == *id))
            .map(|id| json!({"type": "tool_result", "tool_use_id": id, "content": "Interrupted: no result was saved for this call.", "is_error": true}))
            .collect();
        // The API wants tool results ahead of any text in the message.
        content.splice(0..0, missing);
    }
    messages
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

impl SessionInfo {
    /// None for the null summary of a session /resume leaves out.
    fn from_summary(id: &str, summary: &Value, modified: std::time::SystemTime) -> Option<SessionInfo> {
        Some(SessionInfo {
            id: id.to_string(),
            cwd: summary["cwd"].as_str()?.to_string(),
            name: summary["name"].as_str().map(String::from),
            title: summary["title"].as_str()?.to_string(),
            turns: summary["turns"].as_u64()? as usize,
            modified,
        })
    }
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
                    history.file_checkpoints.retain(|checkpoint| checkpoint.at < index);
                    history.compactions.retain(|compaction| compaction.cut <= index);
                }
                Some("tool_checkpoint") => {
                    let text = |field: &str| entry[field].as_str().map(String::from).ok_or_else(|| missing(field));
                    let at = saved_index(&entry, "index", "at", &history.compactions).ok_or_else(|| missing("index"))?;
                    history.checkpoints.push(Checkpoint { at, before: text("before")?, after: text("after")? });
                }
                Some("file_checkpoint") => {
                    let at = saved_index(&entry, "index", "at", &history.compactions).ok_or_else(|| missing("index"))?;
                    let path = entry["path"].as_str().map(PathBuf::from).ok_or_else(|| missing("path"))?;
                    let blob = |field: &str| entry[field].as_str().map(String::from);
                    history.file_checkpoints.push(FileCheckpoint { at, path, before: blob("before"), after: blob("after") });
                }
                Some("checkpoints_reset") => {
                    history.checkpoints.clear();
                    history.file_checkpoints.clear();
                }
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
    /// Reads only the files changed since the last list; `dir/index.json` keeps the summary of the rest by size and mtime.
    pub fn list(dir: &Path) -> Vec<SessionInfo> {
        let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
        let index_path = dir.join("index.json");
        let old_index = read_session_index(&index_path);
        let mut index = serde_json::Map::new();
        let mut sessions = Vec::new();
        for path in entries.filter_map(|e| e.ok()).map(|e| e.path()) {
            let Some(id) = path.file_name().and_then(|name| name.to_str()).and_then(|name| name.strip_suffix(".jsonl")) else { continue };
            // Stat before reading: a file that grows in between is read again next time instead of kept stale.
            let Ok(metadata) = path.metadata() else { continue };
            let Ok(modified) = metadata.modified() else { continue };
            let stamp = json!([metadata.len(), modified.duration_since(std::time::UNIX_EPOCH).map_or(0, |since| since.as_nanos() as u64)]);
            let summary = match old_index.get(id) {
                Some(cached) if cached["stamp"] == stamp => cached["summary"].clone(),
                _ => match std::fs::read_to_string(&path) {
                    Ok(text) => summarize(&text),
                    Err(_) => continue,
                },
            };
            if let Some(info) = SessionInfo::from_summary(id, &summary, modified) {
                sessions.push(info);
            }
            index.insert(id.to_string(), json!({"stamp": stamp, "summary": summary}));
        }
        if index != old_index {
            // Only a cache: when it cannot be written, the next list reads the files again.
            let _ = write_session_index(&index_path, index);
        }
        sessions.sort_by_key(|s| std::cmp::Reverse(s.modified));
        sessions
    }

    /// One saved session file; None when unreadable, a subagent's, or without a prompt yet.
    pub fn info(path: &Path) -> Option<SessionInfo> {
        let id = path.file_name()?.to_str()?.strip_suffix(".jsonl")?;
        let text = std::fs::read_to_string(path).ok()?;
        let modified = path.metadata().and_then(|m| m.modified()).ok()?;
        SessionInfo::from_summary(id, &summarize(&text), modified)
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

/// Bump when `summarize` changes what it keeps, so `Session::list` reads every file again instead of trusting an older index.
const SESSION_INDEX_VERSION: u64 = 1;

/// What /resume shows of one session file; null for a subagent's session or one without a prompt yet.
fn summarize(text: &str) -> Value {
    let (mut cwd, mut name, mut title, mut turns) = (String::new(), None, String::new(), 0);
    let mut subagent = false;
    for entry in text.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()) {
        match entry["type"].as_str() {
            Some("session") => cwd = entry["cwd"].as_str().unwrap_or_default().to_string(),
            Some("session_info") => name = entry["name"].as_str().map(String::from),
            // Subagent sessions stay out of /resume and --continue; /tasks opens them.
            Some("parent") => subagent = true,
            Some("message") if entry["message"]["role"] == "user" && entry["message"]["content"][0]["type"] == "text" => {
                turns += 1;
                if title.is_empty() {
                    title = entry["message"]["content"][0]["text"].as_str().unwrap_or_default().lines().next().unwrap_or_default().to_string();
                }
            }
            _ => {}
        }
    }
    if turns == 0 || subagent {
        return Value::Null;
    }
    json!({"cwd": cwd, "name": name, "title": title, "turns": turns})
}

/// The summaries `Session::list` saved by session id; empty when missing, unreadable or from another index version.
fn read_session_index(path: &Path) -> serde_json::Map<String, Value> {
    let Ok(text) = std::fs::read_to_string(path) else { return serde_json::Map::new() };
    let Ok(mut index) = serde_json::from_str::<Value>(&text) else { return serde_json::Map::new() };
    match index["sessions"].take() {
        Value::Object(sessions) if index["version"] == SESSION_INDEX_VERSION => sessions,
        _ => serde_json::Map::new(),
    }
}

fn write_session_index(path: &Path, sessions: serde_json::Map<String, Value>) -> std::io::Result<()> {
    // Titles are prompt lines, so 0600 like the sessions; the rename keeps another process from reading half a file.
    let temporary = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let mut file = OpenOptions::new().create(true).write(true).truncate(true).mode(0o600).open(&temporary)?;
    file.write_all(json!({"version": SESSION_INDEX_VERSION, "sessions": sessions}).to_string().as_bytes())?;
    std::fs::rename(&temporary, path)
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
fn restore_files(snapshots: &Snapshots, checkpoints: &[Checkpoint], file_checkpoints: &[FileCheckpoint], index: usize) -> Result<RestoredFiles, String> {
    let mut files = restore_files_left_out(snapshots, file_checkpoints, index)?;
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

/// Saves each target the work tree snapshots leave out, with its blob id from before the tool call.
fn save_files_left_out(snapshots: &Snapshots, targets: &[PathBuf]) -> Result<Vec<(PathBuf, Option<String>)>, String> {
    let mut saved = Vec::new();
    for path in targets {
        if !snapshots.stores(path)? {
            saved.push((path.clone(), snapshots.save_file(path)?));
        }
    }
    Ok(saved)
}

/// Like the work tree part of `restore_files`, for the files `save_files_left_out` saved.
fn restore_files_left_out(snapshots: &Snapshots, file_checkpoints: &[FileCheckpoint], index: usize) -> Result<RestoredFiles, String> {
    let mut changed: BTreeMap<&Path, (Option<&str>, Option<&str>)> = BTreeMap::new();
    for checkpoint in file_checkpoints.iter().filter(|checkpoint| checkpoint.at >= index) {
        let (_, last_after) = changed.entry(&checkpoint.path).or_insert((checkpoint.before.as_deref(), checkpoint.after.as_deref()));
        *last_after = checkpoint.after.as_deref();
    }
    let mut files = RestoredFiles::default();
    for (path, (before, after)) in changed {
        let current = snapshots.file_id(path)?;
        if current.as_deref() != after {
            files.kept.push(path.display().to_string());
        } else if current.as_deref() != before {
            snapshots.restore_file(path, before)?;
            files.restored.push(path.display().to_string());
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

/// `transcript` is the handed-off session's file.
fn handoff_prompt(summary: &str, transcript: Option<&Path>, goal: Option<&str>) -> String {
    let mut prompt = format!("This session continues an earlier one, handed off with this summary:\n\n<summary>\n{summary}\n</summary>");
    if let Some(path) = transcript {
        prompt.push_str(&format!("\n\nThe full earlier conversation is in {}, one JSON entry per line. Search it with rg when the summary lacks a detail you need.", path.display()));
    }
    prompt.push_str(&match goal {
        Some(goal) => format!("\n\nNext: {goal}"),
        None => "\n\nContinue with the next steps.".into(),
    });
    prompt
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
    fn handoff_prompt_points_at_the_old_transcript_and_names_the_next_task() {
        let prompt = handoff_prompt("S", Some(Path::new("/s/abc.jsonl")), Some("ship it"));
        assert!(prompt.starts_with("This session continues an earlier one, handed off with this summary:\n\n<summary>\nS\n</summary>"));
        assert!(prompt.contains("The full earlier conversation is in /s/abc.jsonl"));
        assert!(prompt.ends_with("\n\nNext: ship it"));
        let unsaved = handoff_prompt("S", None, None);
        assert!(!unsaved.contains("earlier conversation is in"));
        assert!(unsaved.ends_with("\n\nContinue with the next steps."));
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
    fn a_call_cut_off_before_its_result_gets_an_error_result() {
        let call = |id: &str| json!({"type": "tool_use", "id": id, "name": "Bash", "input": {}});
        let result = |id: &str| json!({"type": "tool_result", "tool_use_id": id, "content": "ok"});
        let interrupted = |id: &str| json!({"type": "tool_result", "tool_use_id": id, "content": "Interrupted: no result was saved for this call.", "is_error": true});
        let messages = vec![
            json!({"role": "assistant", "content": [call("a"), call("b")]}),
            json!({"role": "user", "content": [result("a")]}),
            json!({"role": "assistant", "content": [call("c")]}),
            json!({"role": "user", "content": "continue"}),
            json!({"role": "assistant", "content": [call("d")]}),
        ];
        assert_eq!(
            answer_missing_results(messages),
            vec![
                json!({"role": "assistant", "content": [call("a"), call("b")]}),
                json!({"role": "user", "content": [interrupted("b"), result("a")]}),
                json!({"role": "assistant", "content": [call("c")]}),
                json!({"role": "user", "content": [interrupted("c"), {"type": "text", "text": "continue"}]}),
                json!({"role": "assistant", "content": [call("d")]}),
                json!({"role": "user", "content": [interrupted("d")]}),
            ]
        );
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
    fn workspace_add_expands_a_tilde_to_the_home_directory() {
        let home = std::env::temp_dir().join(format!("agent-add-tilde-{}", std::process::id()));
        let cwd = home.join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let settings = crate::settings::load(&home).unwrap();
        let session = Session::start(&home.join("sessions"), false, &cwd, "m").unwrap();
        let mut agent = Agent::new(Client::new(&home).unwrap(), &settings, home.clone(), cwd, session, home.join("sessions"), None);
        let added = agent.add_dir("~");
        std::fs::remove_dir_all(&home).unwrap();
        let real_home = PathBuf::from(std::env::var("HOME").unwrap()).canonicalize().unwrap();
        assert_eq!(added, Ok(format!("Added {}.", real_home.display())));
        assert_eq!(agent.extra_dirs, [real_home]);
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
    fn session_info_titles_one_file_by_its_name_then_first_prompt() {
        let dir = std::env::temp_dir().join(format!("agent-info-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let prompt = |text: &str| json!({"type": "message", "message": {"role": "user", "content": [{"type": "text", "text": text}]}}).to_string();
        let write = |id: &str, lines: &[String]| {
            let path = dir.join(format!("{id}.jsonl"));
            std::fs::write(&path, lines.join("\n")).unwrap();
            path
        };
        let titled = write("a", &[json!({"type": "session", "cwd": "/w"}).to_string(), prompt("fix login\nmore"), prompt("again")]);
        let named = write("b", &[prompt("x"), json!({"type": "session_info", "name": "Login fix"}).to_string()]);
        let child = write("c", &[json!({"type": "parent"}).to_string(), prompt("x")]);
        let empty = write("d", &[]);
        let infos = [&titled, &named, &child, &empty].map(|path| Session::info(path).map(|s| (s.title, s.name, s.turns)));
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(infos, [Some(("fix login".into(), None, 2)), Some(("x".into(), Some("Login fix".into()), 1)), None, None]);
    }

    #[test]
    fn list_reads_again_only_the_session_files_changed_since_the_last_list() {
        let dir = std::env::temp_dir().join(format!("agent-list-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let prompt = |text: &str| json!({"type": "message", "message": {"role": "user", "content": [{"type": "text", "text": text}]}}).to_string() + "\n";
        let path = dir.join("a.jsonl");
        let header = json!({"type": "session", "cwd": "/w"}).to_string() + "\n";
        std::fs::write(&path, header.clone() + &prompt("first")).unwrap();
        let listed = || Session::list(&dir).into_iter().map(|s| (s.title, s.turns)).collect::<Vec<_>>();
        let read = listed();
        let modified = path.metadata().unwrap().modified().unwrap();
        std::fs::write(&path, header + &prompt("other")).unwrap();
        File::options().write(true).open(&path).unwrap().set_modified(modified).unwrap();
        let same_size_and_mtime = listed();
        OpenOptions::new().append(true).open(&path).unwrap().write_all(prompt("again").as_bytes()).unwrap();
        let appended = listed();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(read, [("first".to_string(), 1)]);
        assert_eq!(same_size_and_mtime, read, "an unchanged size and mtime keep the summary from the index");
        assert_eq!(appended, [("other".to_string(), 2)]);
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

        let files = restore_files(&snapshots, &[first, second, third], &[], 2).unwrap();
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

        let files = restore_files(&snapshots, &[checkpoint], &[], 0).unwrap();
        let env = std::fs::read_to_string(work.join(".env")).ok();
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(files, RestoredFiles { restored: vec![".gitignore".into()], kept: vec![".env".into()] });
        assert_eq!(env, Some("local".into()));
    }

    #[test]
    fn rewind_restores_files_edited_outside_the_work_tree() {
        let _index = SNAPSHOT_INDEX.lock().unwrap();
        let (root, work) = git_work("agent-restore-outside");
        let (edited, created, shared) = (root.join("edited.txt"), root.join("created.txt"), root.join("shared.txt"));
        std::fs::write(&edited, "v1").unwrap();
        std::fs::write(&shared, "v1").unwrap();
        let snapshots = Snapshots::open(&root, &work).unwrap();
        assert!(!snapshots.stores(&edited).unwrap());
        let tool_call = |path: &PathBuf, text: &str| {
            let before = snapshots.save_file(path).unwrap();
            std::fs::write(path, text).unwrap();
            FileCheckpoint { at: 1, path: path.clone(), before, after: snapshots.save_file(path).unwrap() }
        };
        let checkpoints = [tool_call(&edited, "v2"), tool_call(&created, "agent"), tool_call(&shared, "agent")];
        std::fs::write(&shared, "mine").unwrap();

        let files = restore_files(&snapshots, &[], &checkpoints, 0).unwrap();
        let state = [&edited, &created, &shared].map(|path| std::fs::read_to_string(path).ok());
        std::fs::remove_dir_all(&root).unwrap();
        let shown = |path: &PathBuf| path.display().to_string();
        assert_eq!(files, RestoredFiles { restored: vec![shown(&created), shown(&edited)], kept: vec![shown(&shared)] });
        assert_eq!(state, [Some("v1".into()), None, Some("mine".into())]);
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
