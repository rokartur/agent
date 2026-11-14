use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{File, TryLockError};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
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

mod agent_tools;
mod code;
mod compaction;
mod rewind;
mod session;
mod tool_run;

pub use rewind::{FileChange, RestoredFiles, RewindPoint, rewind_points};
pub use session::{Session, SessionInfo};

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
use permissions::{Decision, Grants, Mode, Request, Step, Verdict};
use snapshots::Snapshots;
use subagent::{Launch, Runs};

/// The result of a tool call or wait that Esc stopped.
pub const ABORTED: &str = "Operation aborted";
/// The refused input stays in the context, so "continue" or any retry is refused again.
const REFUSED: &str = "Stopped: the model refused this request, and every retry will refuse too while it stays in the context. Rewind (Esc Esc) to before the message that caused it, or start a new session. A screenshot of the model's own reasoning is a known cause.";
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
    /// What a long call is doing now, such as Video's `transcribing 2:10 / 14:32`.
    ToolProgress {
        id: String,
        text: String,
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
    /// A page the Page tool published, for the terminal that typed to open in its browser.
    OpenPage(String),
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
    /// ask_user_question: the agent waits for one answer slot per question, plus the images the typed answers name; all None means declined.
    Ask {
        questions: Vec<Question>,
        reply: Sender<(Vec<Option<Answer>>, Vec<Value>)>,
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
    /// Streamed text goes to stdout (--print, ACP), where a retried reply would print twice, so a cut-off reply fails instead.
    pub text_to_stdout: bool,
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
    pub history: History,
    pub session: Session,
    pub sessions_dir: PathBuf,
    pub context_window: u64,
    /// What the provider takes, input plus output: the catalog's window, lowered by a limit a "too long" error named.
    /// `context_window` can be a smaller /context-window budget for compaction, which should not cut the output.
    provider_window: u64,
    pub max_tokens: u64,
    /// `small_model` from settings, else the provider's cheapest: titles, Video descriptions, and by default condense summaries.
    pub cheap_model: String,
    /// Auto-mode reviews: `reviewer_model` from settings, else `cheap_model`.
    pub reviewer_model: String,
    /// ImageGenerate's model, the `image_model` setting; empty lets `Client::generate_image` pick.
    image_model: String,
    /// One of `SPEEDS`; anything but "standard" goes out as the request's `speed`, which `api` maps per provider.
    pub speed: String,
    pub cache_ttl: String,
    pub auto_compact: bool,
    pub reserve_tokens: u64,
    pub keep_recent_tokens: u64,
    pub last_context_tokens: u64,
    pub condense: condense::Config,
    /// Number of the next short ref (`t7`).
    pub next_ref: usize,
    pub pending_condense: Vec<JoinHandle<Result<(Condensed, api::Reply), String>>>,
    pub pending_title: Option<JoinHandle<Result<String, String>>>,
    pub usage: Usage,
    pub turns: u64,
    pub tool_calls: u64,
    /// What the Anthropic API holds of this conversation, so a turn that only appends to it sends just the new messages.
    message_thread: api::MessageThread,
    /// Set from the UI thread to stop the stream and kill a running bash command.
    pub abort: Arc<AtomicBool>,
    /// Messages typed while the agent works; delivered with the next tool results.
    pub steering: Steering,
    /// Blocks the running call returns after its text: AskUserQuestion's pasted images, or Video's frames and the speech between them.
    result_blocks: Vec<Value>,
    /// Files the tool calls of the prompt in progress edited, for the edit hooks at its end.
    edited: Vec<PathBuf>,
    /// The `code_mode` setting: the Code tool is offered.
    code_mode: bool,
    /// A Code script is making the call in progress, so its Bash commands run as written.
    in_script: bool,
    /// Time spent at permission prompts and AskUserQuestion; a Code script's time limit leaves it out.
    waited_on_user: Duration,
    /// The Page tool's server, started by its first call.
    pages: Option<tools::page::Pages>,
    /// Shared with the UI so a mode change applies to the next tool call.
    pub permission_mode: Arc<Mutex<Mode>>,
    pub grants: Grants,
    /// `settings.permissions`: a deny rule refuses a call in every mode.
    pub permission_rules: permissions::Rules,
    pub before_tool: Vec<crate::settings::ToolHook>,
    /// Shared with the UI, which shows it and lets the user edit it in /todos.
    pub todos: Arc<Mutex<Todos>>,
    /// Shared with the UI for the footer and /goal pause, which must work mid-turn.
    pub goal: Arc<Mutex<Option<Goal>>>,
    /// The last `REVIEW_REQUESTS` texts the user typed, oldest first; the auto-mode reviewer judges calls against them.
    user_requests: Vec<String>,
    /// The text of the turn in progress when a peer, background command or scheduled task started it, not the user.
    follow_up: Option<String>,
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
    /// Someone answers `Event::Ask`: the TUI, or an editor with form elicitation. AskUserQuestion and EnterPlanMode need it.
    pub asks: bool,
    /// An ACP editor's terminal, where foreground Bash runs; background runs stay in `terminals`.
    pub editor_terminal: Option<Arc<dyn tools::EditorTerminal>>,
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

/// The conversation a session file replays to: what `History::apply` builds from its entries.
#[derive(Default, Debug, PartialEq)]
pub struct History {
    pub messages: Vec<Value>,
    /// Summarized batches; the provider sees their stubs, `messages` keeps the originals.
    pub condensed: Vec<Condensed>,
    /// Files changed by this session's own tool calls, oldest first.
    pub checkpoints: Vec<Checkpoint>,
    pub file_checkpoints: Vec<FileCheckpoint>,
    pub compactions: Vec<Compaction>,
}

impl History {
    /// Applies one session entry of these five types; other types are left to the caller.
    fn apply(&mut self, mut entry: Value) -> Result<(), String> {
        let kind = entry["type"].as_str().unwrap_or_default().to_string();
        let missing = |field: &str| format!("{kind} without {field}");
        match kind.as_str() {
            "message" => self.messages.push(entry["message"].take()),
            "rewind" => {
                let index = saved_index(&entry, "index", "to", &self.compactions).ok_or_else(|| missing("index"))?;
                self.messages.truncate(index);
                self.checkpoints.retain(|checkpoint| checkpoint.at < index);
                self.file_checkpoints.retain(|checkpoint| checkpoint.at < index);
                self.compactions.retain(|compaction| compaction.cut <= index);
            }
            "tool_checkpoint" => {
                let text = |field: &str| entry[field].as_str().map(String::from).ok_or_else(|| missing(field));
                let at = saved_index(&entry, "index", "at", &self.compactions).ok_or_else(|| missing("index"))?;
                self.checkpoints.push(Checkpoint { at, before: text("before")?, after: text("after")? });
            }
            "file_checkpoint" => {
                let at = saved_index(&entry, "index", "at", &self.compactions).ok_or_else(|| missing("index"))?;
                let path = entry["path"].as_str().map(PathBuf::from).ok_or_else(|| missing("path"))?;
                let blob = |field: &str| entry[field].as_str().map(String::from);
                self.file_checkpoints.push(FileCheckpoint { at, path, before: blob("before"), after: blob("after") });
            }
            "checkpoints_reset" => {
                self.checkpoints.clear();
                self.file_checkpoints.clear();
            }
            "compaction" => {
                let cut = saved_index(&entry, "cut", "first_kept", &self.compactions).ok_or_else(|| missing("cut"))?;
                let summary = entry["summary"].as_str().ok_or_else(|| missing("summary"))?;
                // Two processes appending to one session file interleave their messages, so the cut can point past them.
                if cut > self.messages.len() {
                    return Err(format!("compaction keeps from message {cut} of {}", self.messages.len()));
                }
                self.compactions.push(Compaction { cut, summary: summary.to_string() });
            }
            "condense" => self.condensed.push(condense::from_entry(&entry).ok_or_else(|| missing("refs or summary"))?),
            _ => {}
        }
        Ok(())
    }
}

/// What a saved session restores besides its id.
#[derive(Default)]
pub struct Saved {
    pub history: History,
    pub todos: Todos,
    pub goal: Option<Goal>,
    pub plan: Plan,
    pub spent: Spent,
    /// Provider to the account id that served the session's last reply, so a resume keeps the same account.
    pub accounts: HashMap<String, String>,
    pub extra_dirs: Vec<PathBuf>,
    pub config: Option<SessionConfig>,
}

/// What an opened session brings besides its state: messages for the transcript, the accounts that served it and the
/// config an editor last saved for it.
pub struct Opened {
    pub messages: Vec<Value>,
    pub accounts: HashMap<String, String>,
    pub config: Option<SessionConfig>,
}

/// The mode, model and effort an editor picked for one session over ACP, which leaves settings.json as it is.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionConfig {
    pub mode: Mode,
    pub model: String,
    pub effort: String,
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
            text_to_stdout: false,
            system: prompt::system_prompt(&agent_dir, &cwd, settings.trusts(&cwd), |skill| settings.skill_enabled(skill), settings.prefer_global()),
            trusted_projects: settings.trusted_projects.clone(),
            modes: modes::prompt(&home, &settings.ponytail, &settings.caveman, &settings.output_style),
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
            provider_window: 0,
            max_tokens: 0,
            cheap_model: String::new(),
            reviewer_model: String::new(),
            image_model: String::new(),
            speed: settings.speed.clone(),
            cache_ttl: settings.cache_ttl.clone(),
            auto_compact: settings.auto_compact,
            reserve_tokens: settings.reserve_tokens,
            keep_recent_tokens: settings.keep_recent_tokens,
            last_context_tokens: 0,
            condense: condense::Config::new(settings, ""),
            next_ref: 1,
            pending_condense: Vec::new(),
            pending_title: None,
            usage: Usage::default(),
            turns: 0,
            tool_calls: 0,
            message_thread: api::MessageThread::default(),
            abort: Arc::default(),
            steering: Arc::new(Mutex::new(Vec::new())),
            result_blocks: Vec::new(),
            edited: Vec::new(),
            code_mode: settings.code_mode,
            in_script: false,
            waited_on_user: Duration::ZERO,
            pages: None,
            permission_mode: Arc::new(Mutex::new(settings.permission_mode)),
            grants: Grants::default(),
            permission_rules: settings.permissions.clone(),
            before_tool: settings.before_tool.clone(),
            todos: Arc::default(),
            goal: Arc::default(),
            user_requests: Vec::new(),
            follow_up: None,
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
            asks: false,
            editor_terminal: None,
            plugins: crate::plugins::Plugins::new(&home),
            mcp: crate::mcp::Mcp::new(&home, &cwd),
            snapshots: None,
            lsp: lsp::Lsp::new(settings.lsp.clone()),
            agent_dir,
            home,
            cwd,
            history: History::default(),
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
    /// Leaves the abort flag to the caller, so a cancel that came while the caller set up this turn still stops it.
    pub fn prompt(&mut self, text: &str, emit: &mut dyn FnMut(Event)) -> Result<String, String> {
        self.turn(text, true, emit)
    }

    /// `typed` is false for a prompt the agent wrote itself, as a goal continuation is.
    fn turn(&mut self, text: &str, typed: bool, emit: &mut dyn FnMut(Event)) -> Result<String, String> {
        let text = &self.plugin_input(text, emit)?;
        // The reviewer judges calls against this, so only the user's own words may set it.
        if typed && self.follow_up.is_none() {
            self.user_requests.push(text.to_string());
            let extra = self.user_requests.len().saturating_sub(permissions::REVIEW_REQUESTS);
            self.user_requests.drain(..extra);
        }
        self.describe_to_peers(true);
        if self.interactive && self.history.messages.is_empty() {
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
        let result = self.run_turns(emit);
        if let Err(error) = &result {
            if self.turns == turns_before {
                // No reply means the API refused this prompt (e.g. a 400 on its image); resending it would fail again.
                self.rewind(self.history.messages.len() - 1).map_err(|e| format!("{error}; {e}"))?;
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
        let Some(last) = self.history.messages.last().filter(|message| message["role"] == "assistant") else { return Ok(()) };
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

    /// The prompt as the plugins let it through. A block on the session's account moves the session to the next
    /// account and asks the plugins again; each account is tried once.
    fn plugin_input(&self, text: &str, emit: &mut dyn FnMut(Event)) -> Result<String, String> {
        let mut tried = Vec::new();
        loop {
            let context = self.plugin_context();
            let block = match self.plugins.input(text, &context) {
                Ok(text) => return Ok(text),
                Err(block) => block,
            };
            tried.push(context["accountName"].clone());
            let Some(until) = block.account_until else { return Err(block.reason) };
            if !self.client.switch_account_until(&self.model, until) {
                return Err(block.reason);
            }
            let next = self.plugin_context()["accountName"].clone();
            if tried.contains(&next) {
                return Err(block.reason);
            }
            emit(Event::Notice(format!("Switched to {}: {}", next.as_str().unwrap_or_default(), block.reason)));
        }
    }

    /// Sent with every plugin message; see plugins/README.md.
    pub fn plugin_context(&self) -> Value {
        // None when every model list failed at startup.
        let catalog = self.client.catalog();
        let provider = catalog.iter().find(|m| m.id == self.model).map(|m| m.provider.as_str());
        let (account_name, account_id) = provider.and_then(|provider| self.client.account(provider)).unzip();
        let usage = provider.zip(account_name.as_deref()).and_then(|(provider, name)| self.client.account_usage(provider, name)?.for_model(&self.model));
        json!({
            "provider": provider,
            "model": self.model,
            "accountId": account_id.flatten(),
            "accountName": account_name,
            "accountUsage": usage.map(|u| json!({"fiveHourPct": u.five_hour, "sevenDayPct": u.seven_day})),
            "cwd": self.cwd,
            "sessionId": self.session.id,
        })
    }

    fn run_turns(&mut self, emit: &mut dyn FnMut(Event)) -> Result<String, String> {
        self.edited.clear();
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
            let input = context::estimate(&format!("{system}{}", self.cwd_section()), &tools, &messages);
            let max_tokens = context::output_tokens(self.max_tokens, self.provider_window, input);
            let mut request = api::request(&self.model, max_tokens, &self.effort, &self.cache_ttl, &system, &self.cwd_section(), &tools, &messages);
            if self.speed != "standard" {
                request["speed"] = json!(self.speed);
            }
            let sent = self.client.send_in_thread(&request, &mut self.message_thread, &self.abort, self.text_to_stdout, &mut |delta| match delta {
                Delta::Text(text) => emit(Event::Text(text.to_string())),
                Delta::Thinking(text) => emit(Event::Thinking(text.to_string())),
                Delta::Notice(text) => emit(Event::Notice(text.to_string())),
            });
            let mut reply = match sent {
                Ok(reply) => reply,
                Err(error) => {
                    // Each retry needs a strictly lower limit, so a provider repeating the same one ends the turn.
                    let Some(limit) = context::limit_in_error(&error).filter(|&limit| limit < self.provider_window) else { return Err(error) };
                    self.provider_window = limit;
                    self.context_window = self.context_window.min(limit);
                    emit(Event::Notice(format!("{} takes at most {} tokens of context; retrying within that.", self.model, limit)));
                    continue;
                }
            };
            drop_reminders(&mut reply.message);
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
                    let failures = self.run_edit_hooks(self.edited.clone(), emit);
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
                let bad_input = reply.bad_tool_inputs.iter().find(|bad| bad.id == id).map(|bad| bad.error.as_str());
                let (content, output, is_error) = self.run_call(&id, &name, &input, bad_input, emit)?;
                results.push(json!({"type": "tool_result", "tool_use_id": id, "content": content, "is_error": is_error}));
                finished.push(condense::Call { id, name, input, output, is_error });
            }
            // An aborted turn leaves steering queued, so the idle TUI sends it as the next turn.
            let stopped = self.abort.load(Ordering::Relaxed);
            if !stopped {
                results.extend(self.take_queued(emit));
            }
            self.push(json!({"role": "user", "content": results}))?;
            if stopped {
                emit(Event::Notice(ABORTED.into()));
                return Ok(answer);
            }
            self.start_condense(&answer, finished);
        }
    }

    /// Steering and finished background work, taken after each tool batch and at turn end.
    fn take_queued(&mut self, emit: &mut dyn FnMut(Event)) -> Vec<Value> {
        let steering = std::mem::take(&mut *self.steering.lock().unwrap());
        let follow_ups = std::mem::take(&mut *self.follow_ups.lock().unwrap());
        let mut blocks = Vec::new();
        for (text, images) in steering {
            emit(Event::Steered(text.clone()));
            // A null holds the place of an image whose tag was deleted.
            blocks.extend(images.into_iter().filter(|image| !image.is_null()));
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
        self.follow_up = Some(texts.join("\n\n"));
        let result = self.pursue(&texts.join("\n\n"), emit);
        self.follow_up = None;
        result
    }

    pub fn describe_to_peers(&self, busy: bool) {
        if let Some(inbox) = &self.inbox {
            inbox.describe(&self.session.id, &self.title, &self.cwd, busy);
        }
    }

    fn push(&mut self, message: Value) -> Result<(), String> {
        self.record(json!({"type": "message", "message": message}))
    }

    /// Saves `entry` to the session file, then applies it to `history`, so memory matches what a resume rebuilds.
    fn record(&mut self, entry: Value) -> Result<(), String> {
        self.session.append(&entry)?;
        self.history.apply(entry)
    }

    /// Applies settings changed in /settings; `model` is the catalog entry for `settings.model`, `cheap_model` the `small_model` setting or its provider's cheapest.
    fn apply(&mut self, settings: &Settings, model: &Model, cheap_model: &str) {
        self.trusted_projects = settings.trusted_projects.clone();
        self.permission_rules = settings.permissions.clone();
        self.before_tool = settings.before_tool.clone();
        self.model = settings.model.clone();
        self.effort = settings.thinking.clone();
        self.speed = settings.speed.clone();
        self.cache_ttl = settings.cache_ttl.clone();
        self.context_window = settings.context_window(model);
        self.provider_window = model.context_window;
        self.max_tokens = model.max_tokens;
        self.cheap_model = cheap_model.to_string();
        self.reviewer_model = if settings.reviewer_model.is_empty() { cheap_model.to_string() } else { settings.reviewer_model.clone() };
        self.image_model = settings.image_model.clone();
        self.auto_compact = settings.auto_compact;
        self.reserve_tokens = settings.reserve_tokens;
        self.keep_recent_tokens = settings.keep_recent_tokens;
        self.condense = condense::Config::new(settings, cheap_model);
        self.subagent_concurrency = settings.subagent_concurrency;
        self.subagent_effort = settings.subagent_effort.clone();
        self.subagent_model = settings.subagent_model.clone();
        self.code_mode = settings.code_mode;
        self.tools = self.tool_schemas();
        *self.permission_mode.lock().unwrap() = settings.permission_mode;
        self.rtk = if settings.rtk { Rtk::ON } else { Rtk::OFF };
        self.modes = modes::prompt(&self.home, &settings.ponytail, &settings.caveman, &settings.output_style);
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
        let cheap_model = if settings.small_model.is_empty() { api::models::small_model(&catalog, &model).id.clone() } else { settings.small_model.clone() };
        self.apply(settings, &model, &cheap_model);
        Ok(notice)
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
        // A one-shot run (-p, --headless, acp, a child) has no UI to notify or schedule in, and asks only through an editor: these tools only fail there.
        if !self.interactive {
            list.retain(|tool| !tools::facts(tool["name"].as_str().unwrap_or_default()).is_some_and(|facts| facts.needs_ui));
        }
        if !self.asks {
            list.retain(|tool| tool["name"] != "AskUserQuestion" && tool["name"] != "EnterPlanMode");
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
        let context_tree_query = self.condense.enabled || !self.history.condensed.is_empty();
        tools::schemas(
            &subagent::swarm_prompt(&self.agent_dir, &self.cwd, prompt::is_trusted(&self.trusted_projects, &self.cwd), &self.subagent_effort),
            context_tree_query,
            self.client.generates_images(&self.image_model),
            self.code_mode,
        )
    }

    fn provider_view(&self) -> Vec<Value> {
        answer_missing_results(condense::view(&self.compacted(), &self.history.condensed))
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
/// A tool search the model asked for after a client tool call ends the reply unrun, with no result to answer it, so it goes.
fn answer_missing_results(mut messages: Vec<Value>) -> Vec<Value> {
    let mut index = 0;
    while index < messages.len() {
        if let Some(content) = messages[index]["content"].as_array_mut() {
            let answered: Vec<Value> = content.iter().map(|b| b["tool_use_id"].clone()).collect();
            content.retain(|b| b["type"] != "server_tool_use" || answered.contains(&b["id"]));
        }
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

/// The source of a text the agent added to a user message itself (finished background work, `!cmd` output, a move),
/// matched by the openings their producers write. shortcut: a new producer counts as a typed prompt until it is listed here.
pub fn harness_source(text: &str) -> Option<&'static str> {
    const OPENINGS: [(&str, &str); 11] = [
        ("Background terminal ", "terminal"),
        ("Subagent ", "subagent"),
        ("Answer from ", "subagent"),
        ("Message from subagent ", "subagent"),
        ("Message from another agent session ", "peer"),
        ("Workflow ", "workflow"),
        ("Scheduled task ", "cron"),
        ("Ran `", "shell"),
        ("Session moved from ", "session"),
        ("The user set todo #", "todo"),
        ("The user deleted todo #", "todo"),
    ];
    OPENINGS.iter().find(|(opening, _)| text.starts_with(opening)).map(|(_, source)| *source)
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

/// Models trained on Claude Code sometimes write a harness `<system-reminder>` themselves, e.g. a made-up terminal exit.
pub fn without_reminders(text: &str) -> String {
    const OPEN: &str = "<system-reminder>";
    const CLOSE: &str = "</system-reminder>";
    let mut kept = String::new();
    let mut rest = text;
    while let Some(start) = rest.find(OPEN) {
        kept.push_str(&rest[..start]);
        // An unclosed one is still streaming, or the reply stopped inside it.
        rest = rest[start..].find(CLOSE).map_or("", |end| &rest[start + end + CLOSE.len()..]);
    }
    kept.push_str(rest);
    kept
}

fn drop_reminders(message: &mut Value) {
    let Some(content) = message["content"].as_array_mut() else { return };
    for block in content.iter_mut() {
        if let Some(text) = block["text"].as_str().filter(|t| t.contains("<system-reminder>")) {
            block["text"] = json!(without_reminders(text).trim());
        }
    }
    // The API rejects an empty text block.
    content.retain(|block| block["type"] != "text" || block["text"] != "");
}

fn text_of(message: &Value) -> String {
    let blocks = message["content"].as_array().into_iter().flatten();
    blocks.filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n")
}

#[cfg(test)]
mod tests;
