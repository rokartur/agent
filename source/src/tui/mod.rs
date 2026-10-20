mod ask;
mod btw;
mod editor;
mod history;
mod login;
mod markdown;
mod notify;
mod panel;
mod render;
mod selection;
mod voice;

use crossterm::event::{
    self, DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste, EnableFocusChange, EnableMouseCapture, Event as Input, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
    KeyboardEnhancementFlags, MouseButton, MouseEvent, MouseEventKind, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::io::{Stdout, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::agent::FollowUp;
use crate::agent::modes::{self, CAVEMAN_LEVELS, PONYTAIL_LEVELS};
use crate::agent::permissions::{Decision, Kind, MODES, Mode, Request};
use crate::agent::prompt::{Skill, Template};
use crate::agent::skill_usage;
use crate::agent::subagent::{Run, Runs};
use crate::agent::usage::{self, ModelTotal, tokens};
use crate::agent::{self, Agent, Event, FileChange, RestoredFiles, RewindPoint, Session, SessionInfo, Spent};
use crate::api::accounts::Policy;
use crate::api::models::{self, Model};
use crate::api::oauth::Tokens;
use crate::api::{AccountView, Client, Delta, Usage};
use crate::image;
use crate::settings::THINKING_LEVELS;
use crate::settings::{self, CACHE_TTLS, CONFLICT_PRIORITIES, PROVIDER_SORTS, PRUNE_SESSIONS, Settings, model_key};
use crate::tools::bg::{Status as TerminalStatus, Terminal, Terminals};
use crate::tools::goal::{Goal, Status as GoalStatus};
use crate::tools::rtk::strip_ansi;
use crate::tools::todo::{Status, Todos};
use crate::worktree::{self, Worktree};
use editor::Editor;
use render::{BRIGHT, Line, MUTED, PLAIN, Screen, Span, Style, line_width, link_at, overlay_center, overlay_right, plain_text, select, span, take_cursor, text_in, truncate, width, wrap};
use selection::{Point, Selection};

const WORKING: &str = "Working";
const TRANSCRIBING: &str = "Transcribing";
const LOADING_MODELS: &str = "loading models";
// Matched whole: an error hint that mentions Ctrl+C must not arm quitting.
const EXIT_HINT: &str = "Press Ctrl+C again to exit";
const REWIND_HINT: &str = "Press Esc again to rewind";
const MODELS_STILL_LOADING: &str = "The model lists are still loading.";
const TOOL_OUTPUT_LINES: usize = 20;
/// One line per tick while a selection drag rests on the transcript edge.
const ANIMATION_TICK: Duration = Duration::from_millis(80);
const HINT_LIFETIME: Duration = Duration::from_millis(1500);
const EDGE_SCROLL: Duration = Duration::from_millis(40);
const PERMISSION_SETTLE: Duration = Duration::from_millis(400);

const DIM: Style = Style::fg(241);
const WARN: Style = Style::fg(179);
const SYSTEM: Style = Style::rgb(0x79c0ff);
const OK: Style = Style::rgb(0x7ee787);
const ERROR: Style = Style::rgb(0xff7b72);
const TOOL_NAME: Style = Style::rgb(0xffffff);
const TOOL_ARGS: Style = Style::rgb(0x858585);

enum Msg {
    Input(Input),
    Agent(Event),
    Done(Result<String, String>),
    /// The model the worker settled on with the notice of a swapped model, and the failed model lists.
    ModelSettled {
        model: Result<(String, Option<String>), String>,
        failures: Vec<String>,
    },
    Resumed {
        id: String,
        path: Option<String>,
        messages: Vec<Value>,
        spent: Spent,
        notice: Option<String>,
        /// Provider to the account id that served the session's last reply.
        accounts: HashMap<String, String>,
    },
    BtwDelta(String),
    BtwDone(Result<(), String>),
    Moved {
        id: String,
        path: Option<String>,
        cwd: PathBuf,
    },
    Cloned {
        id: String,
        path: Option<String>,
    },
    LoginFinished {
        provider: &'static str,
        /// The exchange's cancel flag, which tells it apart from a newer /login.
        cancel: Arc<AtomicBool>,
        result: Result<Tokens, String>,
    },
    /// Account usage arrived for /account.
    UsageFetched,
    WorktreeRemoved(PathBuf, Result<(), String>),
    /// A plugin set its status or exited: redraw the footer and /plugins.
    PluginsChanged,
    /// An MCP server connected, failed or exited: redraw /mcp.
    McpChanged,
    Transcribed(Result<String, String>),
    RewindPoints(Vec<RewindPoint>),
    /// `entry` is where the rewound prompt sits on screen; None redraws from `messages`.
    Rewound {
        messages: Vec<Value>,
        files: Option<RestoredFiles>,
        entry: Option<usize>,
    },
    Changes(Result<Option<Vec<FileChange>>, String>),
    Context {
        split: agent::context::PerPart,
        session: Usage,
    },
}

/// The bottom panel that takes over the keys while it is open.
enum Panel {
    Resume {
        all: bool,
        index: usize,
        sessions: Vec<SessionInfo>,
    },
    /// `row` indexes `SETTING_NAMES`. Typing filters the list by name.
    Settings {
        row: usize,
        query: Editor,
    },
    /// Searchable model list for the settings `row` it edits; Enter and Esc return to `back`.
    Models {
        row: usize,
        query: Editor,
        index: usize,
        back: ModelsBack,
    },
    /// Context window overrides, tab 0 per provider, tab 1 per model filtered by `query`.
    ContextWindows {
        tab: usize,
        query: Editor,
        index: usize,
    },
    /// Manual provider order, tab 0 model providers, tab 1 web search providers.
    ProviderOrder {
        tab: usize,
        index: usize,
    },
    /// fx's "Permission needed" prompt; the agent thread waits on `reply`.
    Permission {
        request: Request,
        reply: Sender<Decision>,
        index: usize,
        opened: Instant,
    },
    Ask(ask::AskPanel),
    Todos {
        index: usize,
    },
    Plugins {
        index: usize,
        /// `d` was pressed once on the selected plugin; a second `d` removes it.
        confirm: bool,
    },
    /// Every skill, including the ones `Settings.disabled_skills` leaves out of the prompt.
    Skills {
        index: usize,
    },
    /// `confirm`: space was pressed once on a project server whose entry the panel cuts.
    Mcp {
        index: usize,
        confirm: bool,
    },
    /// Token totals per `usage::SCOPES`, read when /usage opens; the last tab shows subscription limits.
    Usage {
        tab: usize,
        totals: [Vec<ModelTotal>; 4],
    },
    /// The settings a plugin declares in plugin.json; `edit` holds a string being typed.
    PluginSettings {
        plugin: String,
        index: usize,
        edit: Option<Editor>,
    },
    /// A panel a plugin opened with `ui.showPanel`; `data` is its params.
    Plugin {
        plugin: String,
        data: Value,
        index: usize,
    },
    Terminals {
        index: usize,
    },
    /// One terminal from /ps, full screen and read-only. `loaded` is the (log size, width) `lines` were built for.
    Terminal {
        index: usize,
        scroll: usize,
        loaded: Option<(u64, usize)>,
        lines: Vec<Line>,
    },
    Subagents {
        index: usize,
    },
    /// One run from /subagents, full screen and read-only. `loaded` is the (file size, width) `lines` were built for.
    Subagent {
        index: usize,
        scroll: usize,
        loaded: Option<(u64, usize)>,
        lines: Vec<Line>,
    },
    /// Shows `App::btw`, which outlives the panel so a bare /btw reopens it.
    Btw,
    /// The start-up question for an untrusted project; `files` are what /trust would load.
    Trust {
        root: PathBuf,
        files: Vec<PathBuf>,
    },
    /// `confirm`: d was pressed once, and the next d removes the selected worktree.
    Worktrees {
        confirm: bool,
        index: usize,
        worktrees: Vec<Worktree>,
        current: PathBuf,
    },
    Login(login::Login),
    /// /account pins the chosen account, /logout removes it. Typing filters by name or provider.
    Accounts {
        logout: bool,
        query: Editor,
        index: usize,
    },
    /// /history: saved prompts, newest first, in the `history::SCOPES` tab `tab`. Typing filters, Enter puts the prompt in the editor.
    History {
        entries: Vec<history::Entry>,
        tab: usize,
        query: Editor,
        index: usize,
    },
    /// Double Esc while idle: the prompts from `agent::rewind_points` to go back to.
    Rewind {
        index: usize,
        points: Vec<RewindPoint>,
    },
    /// /changes: files this session changed; Enter opens the diff.
    Changes {
        files: Vec<FileChange>,
        index: usize,
    },
    /// One /changes file's diff filling the screen; Esc goes back to the list.
    Diff {
        files: Vec<FileChange>,
        index: usize,
        scroll: usize,
    },
}

/// The panel the model picker returns to: none for /model, or the settings list.
#[derive(Clone, Copy)]
enum ModelsBack {
    Close,
    Settings,
}

enum Command {
    /// The prompt text and the image blocks pasted into it.
    Prompt(String, Vec<Value>),
    /// `!cmd` typed by the user; `keep` adds its output to the next prompt.
    Bash {
        command: String,
        keep: bool,
    },
    Compact(Option<String>),
    /// New settings; the worker settles their model and answers with Msg::ModelSettled.
    Apply(Box<Settings>),
    Clear,
    Rename(String),
    RenameTitle,
    Resume(String),
    Worktree(PathBuf),
    Clone,
    Import(PathBuf),
    Export(PathBuf),
    /// `/workspace` and its arguments: add, remove or list the extra directories.
    Workspace(String),
    /// A plugin's slash command: name and args.
    Plugin(String, String),
    /// A row picked in a plugin's panel: plugin name and the `panel_select` event, context added by the worker.
    PluginEvent(String, Value),
    /// Tells plugins the account picked in /account, so footer usage follows it.
    AccountPicked,
    /// Tells plugins a /login overwrite gave a saved account a new provider account id, so per-id data can follow it.
    AccountReplaced {
        previous_id: String,
        account_id: String,
    },
    /// Writes the goal the UI changed to the session.
    SaveGoal,
    SaveSpent(Spent),
    /// Starts a turn from follow-ups that arrived while idle.
    FollowUps,
    RewindPoints,
    /// Drops the prompt at message `at` and everything after it; `entry` passes through to `Msg::Rewound`.
    Rewind {
        at: usize,
        entry: Option<usize>,
    },
    Changes,
    Context,
}

#[derive(Clone, Copy, PartialEq)]
enum MidTurn {
    /// Panels, settings and local info leave the turn alone, so they run mid-turn too.
    /// Settings changes reach the agent through Command::Apply, which the worker takes after the turn.
    Runs,
    /// Waits in `queued_commands` and runs when the turn ends.
    Waits,
}

/// Built-in commands: pi's wording for pi's commands, fx's for the ones taken from fx.
const BUILTIN_COMMANDS: &[(&str, MidTurn, &str)] = &[
    ("help", MidTurn::Runs, "show available slash commands"),
    ("clear", MidTurn::Waits, "start a fresh conversation"),
    ("resume", MidTurn::Runs, "resume a saved session"),
    ("rename-title", MidTurn::Runs, "rename the current session"),
    ("rename-title-ai", MidTurn::Waits, "Name the session again from its first prompt"),
    ("clone", MidTurn::Waits, "Duplicate the current session at the current position"),
    ("import", MidTurn::Waits, "<file.jsonl>: Import and resume a session from a JSONL file"),
    ("export", MidTurn::Runs, "[path]: Export session: .jsonl (default) or .md for plain text"),
    ("workspace", MidTurn::Runs, "[add <path> | remove <path> | list]: Extra directories this session also works in"),
    ("model", MidTurn::Runs, "[search]: Select model"),
    ("settings", MidTurn::Runs, "Open settings menu"),
    ("permissions", MidTurn::Runs, "<mode>: ask, auto or full access"),
    ("thinking", MidTurn::Runs, "<level>: Set thinking level"),
    ("fast", MidTurn::Runs, "Toggle fast output: quicker, billed higher, Anthropic models with fast mode only"),
    ("ponytail", MidTurn::Runs, "<off | lite | full | ultra>: Set the ponytail level"),
    ("caveman", MidTurn::Runs, "<off | full | ultra | wenyan>: Set the caveman level"),
    ("voice", MidTurn::Runs, "Dictate in any language with whisper.cpp, Enter to stop"),
    ("context", MidTurn::Waits, "Show what fills the context window and how well it caches"),
    ("context-window", MidTurn::Runs, "<100k to 1M | auto>: Set this model's context window"),
    ("copy", MidTurn::Runs, "Copy last agent message to clipboard"),
    ("copy-session-path", MidTurn::Runs, "Copy this session's file path to clipboard for another session to read"),
    ("session", MidTurn::Runs, "Show the session id, file and context use"),
    ("hotkeys", MidTurn::Runs, "Show all keyboard shortcuts"),
    ("compact", MidTurn::Waits, "[focus]: Summarize older messages"),
    ("todos", MidTurn::Runs, "Show and edit the todo list"),
    ("plugins", MidTurn::Runs, "Install, enable and disable plugins"),
    ("skills", MidTurn::Runs, "Enable and disable skills"),
    ("trust", MidTurn::Runs, "Load this project's .agents files and run its edit hooks"),
    ("untrust", MidTurn::Runs, "Stop loading this project's .agents files and running its edit hooks"),
    ("mcp", MidTurn::Waits, "Show MCP servers: reconnect, log in, enable and disable"),
    ("usage", MidTurn::Runs, "Show token usage, estimated cost and subscription limits"),
    ("history", MidTurn::Runs, "Search previous prompts"),
    ("changes", MidTurn::Waits, "List files changed in this session and show their diffs"),
    ("skill-usage", MidTurn::Runs, "Show how often each skill was used, least used first"),
    ("rtk", MidTurn::Runs, "Show rtk token savings"),
    ("ps", MidTurn::Runs, "Show background terminals"),
    ("subagents", MidTurn::Runs, "Show subagent runs"),
    ("goal", MidTurn::Runs, "[objective | pause | resume | stop | done]: Set a goal the agent keeps working toward, or manage it"),
    ("btw", MidTurn::Runs, "<question>: Ask a side question without touching the session"),
    ("worktree", MidTurn::Runs, "[name]: Move this session to a git worktree or remove one: pick one, or name a branch to open or create"),
    ("login", MidTurn::Runs, "Add a Claude or ChatGPT account or an API key"),
    ("logout", MidTurn::Runs, "Remove a saved account"),
    ("account", MidTurn::Runs, "Show accounts and their usage; pick one to use until it reaches the switch threshold"),
    ("quit", MidTurn::Runs, "Quit agent"),
];
const MENU_ROWS: [usize; 5] = [4, 6, 8, 10, 12];
const PANEL_ROWS: usize = 8;
const EDITOR_ROWS: usize = 10;
const SCROLL_LINES: [usize; 5] = [1, 2, 3, 5, 10];
const SETTING_NAMES: [&str; 32] = [
    "Model",
    "Thinking level",
    "Permission mode",
    "Auto-compact",
    "Compaction reserve",
    "Keep recent context",
    "Expand tool output",
    "Condense tool output",
    "Condense model",
    "Condense min batch",
    "Switch account at",
    "When all over limit",
    "Fallback model",
    "Context window",
    "Ponytail",
    "Caveman",
    "rtk",
    "Headroom proxy",
    "Subagent effort",
    "Notifications",
    "Scroll lines",
    "Copy on select",
    "Update channel",
    "Menu rows",
    "Auto prune sessions",
    "Provider order",
    "Name conflicts prefer",
    "Fast mode",
    "Prompt cache TTL",
    "Reviewer model",
    "Subagent model",
    "Cost in footer",
];
/// Rows of `SETTING_NAMES` in the order the settings panel lists them.
const SETTING_ORDER: [usize; 32] = [0, 1, 27, 18, 30, 13, 2, 29, 3, 4, 5, 6, 31, 7, 8, 9, 10, 11, 12, 25, 28, 14, 15, 16, 17, 26, 19, 20, 23, 21, 22, 24];
const MODEL_ROWS: [usize; 5] = [0, 8, 12, 29, 30];
const CONTEXT_WINDOW_ROW: usize = 13;
const PROVIDER_ORDER_ROW: usize = 25;
const PROVIDER_LISTS: [&[&str]; 2] = [&models::PROVIDERS, &crate::tools::web::SEARCH_PROVIDERS];
const CONTEXT_WINDOWS: [Option<u64>; 5] = [None, Some(200_000), Some(272_000), Some(400_000), Some(1_000_000)];
const RESERVE_TOKENS: [u64; 5] = [16_384, 32_768, 65_536, 100_000, 150_000];
const KEEP_RECENT_TOKENS: [u64; 4] = [10_000, 20_000, 40_000, 80_000];
const CONDENSE_MIN_CHARS: [u64; 4] = [10_000, 20_000, 40_000, 80_000];
// 0 = never switch accounts.
const SWITCH_AT_PERCENT: [u64; 6] = [70, 80, 90, 95, 100, 0];

const ALT: &str = if cfg!(target_os = "macos") { "Option" } else { "Alt" };

const HOTKEYS: [(&str, &[(&str, &str)]); 4] = [
    (
        "Send",
        &[
            ("Enter", "send; while working, steers the turn (commands and images wait for it to end)"),
            ("Shift+Enter", "new line (also Ctrl+J, and Alt+Enter while idle)"),
            (if cfg!(target_os = "macos") { "Option+Enter" } else { "Alt+Enter" }, "while working, queue a follow-up that runs after the turn"),
            ("Tab", "complete a slash command or an @path"),
            ("Up, Down", "history, menu selection"),
            ("Ctrl+R", "search previous prompts"),
            (if cfg!(target_os = "macos") { "Option+Up" } else { "Alt+Up" }, "edit queued messages"),
        ],
    ),
    ("Edit", &[(if cfg!(target_os = "macos") { "Cmd+Z, Ctrl+Z" } else { "Ctrl+Z" }, "undo the last edit"), ("Ctrl+A E U K W", "line editing"), ("Ctrl+C", "clear the editor")]),
    (
        "View",
        &[
            ("Ctrl+O", "expand tool output and thinking"),
            ("Shift+Tab", "cycle thinking level"),
            ("Ctrl+P", "cycle models listed in cycle_models"),
            ("PageUp, PageDown", "scroll the transcript"),
            (if cfg!(target_os = "macos") { "Option+Down" } else { "Alt+Down" }, "jump to the latest message"),
            ("Down", "open running subagents (empty editor)"),
        ],
    ),
    ("Stop", &[("Esc", "abort the running turn"), ("Esc Esc", "rewind to an earlier prompt and its files (idle)"), ("Ctrl+C Ctrl+C", "quit"), ("Ctrl+D", "quit on an empty editor")]),
];

enum Item {
    Lines(Vec<Line>),
    /// fx's system line: `* source: text`, for agent, command and extension messages.
    System {
        source: String,
        text: String,
        failed: bool,
    },
    /// A foreground subagent's end: status, label, tool calls, time, tokens.
    Subagent(Arc<Run>),
    Error(String),
    User(String),
    Assistant {
        text: String,
        thinking: String,
    },
    Tool {
        id: String,
        name: String,
        input: Value,
        result: Option<(String, bool)>,
    },
    /// fx's line after each answer: elapsed time, the prompt's uncached input and output tokens, and the session's total time.
    Done {
        took: Duration,
        usage: Usage,
        total: Duration,
    },
}

/// A transcript item with its rendered lines cached for one (width, expanded) pair.
struct Entry {
    item: Item,
    cache: Option<(usize, bool, Vec<Line>)>,
}

struct Git {
    branch: Option<String>,
    changed: usize,
    /// The main checkout, set only when the session runs in a linked worktree.
    main: Option<Worktree>,
    /// Worktrees of this repository besides the one the session runs in.
    others: usize,
}

struct App {
    entries: Vec<Entry>,
    editor: Editor,
    commands: Sender<Command>,
    working: Option<Instant>,
    recording: Option<voice::Recording>,
    transcribing: Option<Instant>,
    /// None until the terminal reports focus or a key arrives; notifications go out unless it is Some(true).
    focused: Option<bool>,
    prompt_usage: Usage,
    spent: Spent,
    expanded: bool,
    scroll: usize,
    home: PathBuf,
    settings: Settings,
    context_tokens: u64,
    /// The last request's usage, for /context's cache line.
    last_usage: Usage,
    /// This session's usage per model, priced at render so the footer cost appears once the catalog loads.
    session_usage: Vec<ModelTotal>,
    cwd: PathBuf,
    /// The `@` menu's last listing and the cwd-joined query it answers.
    path_matches: std::cell::RefCell<(PathBuf, Vec<(String, String)>)>,
    git: Git,
    plugins: crate::plugins::Plugins,
    mcp: crate::mcp::Mcp,
    steering: Arc<Mutex<Vec<String>>>,
    /// Slash commands, `!bash` and prompts with pasted images typed while the agent works, with those images; they run one by one once it is idle.
    queued_commands: VecDeque<(String, Vec<Value>)>,
    /// Worker commands picked mid-turn, like a worktree or session from a panel; the worker is busy until the turn ends.
    deferred_commands: VecDeque<Command>,
    abort: Arc<AtomicBool>,
    permission_mode: Arc<Mutex<Mode>>,
    hint: Option<(String, Instant)>,
    /// Image blocks pasted into the editor; `[Image #n]` in the text refers to `images[n - 1]`.
    images: Vec<Value>,
    quit: bool,
    skills: Vec<Skill>,
    templates: Vec<Template>,
    /// (name, description) shown under the editor while a slash command is typed.
    slash_commands: Vec<(String, String)>,
    menu_index: usize,
    /// Banner entries kept by /new.
    header_len: usize,
    session_id: String,
    session_path: Option<String>,
    sessions_dir: PathBuf,
    panel: Option<Panel>,
    selection: Selection,
    /// A selection drag held on the transcript's top row or below it, replayed on a timer so the view keeps scrolling without mouse motion.
    edge_drag: Option<(MouseEvent, Instant)>,
    /// Transcript rows on screen and the screen width, from the last frame.
    transcript_view: (usize, usize),
    /// Transcript line count and the width it was wrapped to, from the last frame.
    transcript_size: (usize, usize),
    /// Width the editor wrapped to in the last frame, so Up/Down move by screen rows.
    editor_width: usize,
    input_selection: Selection,
    /// The last left press landed on the editor, so drags and the release belong to `input_selection`.
    selecting_input: bool,
    /// Screen row of the first shown editor row, the shown row count, and the editor rows hidden below them, in the last frame.
    input_view: (usize, usize, usize),
    /// First editor row shown when the editor is taller than `EDITOR_ROWS`.
    input_top: usize,
    todos: Arc<Mutex<Todos>>,
    goal: Arc<Mutex<Option<Goal>>>,
    follow_ups: Arc<Mutex<Vec<FollowUp>>>,
    terminals: Arc<Mutex<Terminals>>,
    subagents: Arc<Mutex<Runs>>,
    client: Client,
    events: Sender<Msg>,
    /// The last /btw thread, kept in memory until exit.
    btw: Option<btw::Btw>,
}

/// What the TUI needs besides the agent itself.
pub struct Setup {
    pub home: PathBuf,
    /// With the command-line flags applied; /settings writes only the keys it changes to the file.
    pub settings: Settings,
    pub prompt: Option<String>,
    /// `--resume` without an id: start with the /resume list open.
    pub pick_session: bool,
    /// Why the ponytail or caveman rules could not be downloaded at startup.
    pub modes_error: Option<String>,
    /// One line per bundled plugin updated, or failed to update, at startup.
    pub plugin_updates: Vec<String>,
}

/// `model_lists` is the startup load of the account model lists; the worker settles the model on it before its first command.
pub fn run(agent: Agent, setup: Setup, model_lists: JoinHandle<Vec<String>>) -> Result<(), String> {
    let prompt = setup.prompt.clone();
    let pick_session = setup.pick_session;
    let (events, inbox) = mpsc::channel();
    let (commands, command_inbox) = mpsc::channel();
    let mut app = App::new(&agent, setup, commands, events.clone());
    let wake = events.clone();
    app.plugins.on_change(move || {
        let _ = wake.send(Msg::PluginsChanged);
    });
    let wake = events.clone();
    app.mcp.on_change(move || {
        let _ = wake.send(Msg::McpChanged);
    });
    spawn_worker(agent, app.settings.clone(), model_lists, command_inbox, events.clone());
    if crate::update::enabled() {
        spawn_update(app.home.clone(), app.settings.update_channel.clone(), events.clone());
    }
    spawn_on_start(app.settings.on_start.clone(), events.clone());
    if app.settings.headroom && crate::process::installed("headroom") {
        spawn_headroom_sync(app.home.clone(), app.settings.headroom_env.clone(), events.clone());
    }
    spawn_input(events);
    let mut out = std::io::stdout();
    enter(&mut out).map_err(|e| format!("Cannot set up the terminal: {e}"))?;
    report_cwd(&app.cwd);
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // `panic = "abort"` skips Drop, so ffmpeg and bg terminals in their own sessions would outlive the agent.
        crate::process::hang_up_children();
        leave(&mut std::io::stdout());
        previous_hook(info);
    }));
    if let Some(prompt) = prompt {
        app.submit(prompt);
    }
    if pick_session {
        app.open_resume();
    }
    app.offer_trust();
    let result = event_loop(&mut app, &inbox, &mut out);
    app.terminals.lock().unwrap().kill_all();
    app.subagents.lock().unwrap().stop_all();
    crate::tools::remove_spills();
    if let Some(recording) = app.recording.take() {
        recording.cancel();
    }
    leave(&mut out);
    if let Some(path) = &app.session_path {
        let has_messages = std::fs::read_to_string(path).is_ok_and(|text| text.contains(r#""type":"message""#));
        if has_messages {
            println!("\x1b[38;5;245mContinue session with: agent --resume {}\x1b[0m", app.session_id);
        }
    }
    result
}

fn enter(out: &mut Stdout) -> std::io::Result<()> {
    terminal::enable_raw_mode()?;
    execute!(out, EnterAlternateScreen, EnableBracketedPaste, EnableMouseCapture, EnableFocusChange, PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES))
}

pub(crate) fn leave(out: &mut Stdout) {
    let _ = execute!(out, PopKeyboardEnhancementFlags, DisableMouseCapture, DisableFocusChange, DisableBracketedPaste, LeaveAlternateScreen);
    let _ = terminal::disable_raw_mode();
}

fn spawn_worker(mut agent: Agent, mut settings: Settings, model_lists: JoinHandle<Vec<String>>, commands: Receiver<Command>, events: Sender<Msg>) {
    std::thread::spawn(move || {
        let notices = model_lists.join().expect("the model lists thread");
        let _ = events.send(settle_model(&mut agent, &mut settings, notices));
        agent.announce_session();
        loop {
            agent.lsp.stop_idle();
            let command = match commands.recv_timeout(Duration::from_secs(60)) {
                Ok(command) => command,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => return,
            };
            let result = match command {
                Command::Prompt(text, images) => {
                    agent.images = images;
                    agent.pursue(&text, &mut |event| {
                        let _ = events.send(Msg::Agent(event));
                    })
                }
                Command::Bash { command, keep } => {
                    agent.run_bash(&command, keep, &mut |event| {
                        let _ = events.send(Msg::Agent(event));
                    });
                    Ok(String::new())
                }
                Command::Compact(focus) => match agent.compact(focus.as_deref()) {
                    Ok(Some(summary)) => {
                        let _ = events.send(Msg::Agent(Event::Compacted { summary }));
                        Ok(String::new())
                    }
                    Ok(None) => Err("Nothing to compact yet.".into()),
                    Err(error) => Err(error),
                },
                Command::Apply(mut settings) => {
                    let _ = events.send(settle_model(&mut agent, &mut settings, Vec::new()));
                    continue;
                }
                Command::Clear => agent.new_session().map(|()| {
                    let path = agent.session.display_path();
                    let _ = events.send(Msg::Agent(Event::SessionStarted { id: agent.session.id.clone(), path }));
                    String::new()
                }),
                Command::Rename(name) => {
                    if let Err(error) = agent.session.rename(&name) {
                        let _ = events.send(Msg::Agent(Event::Notice(error)));
                    }
                    continue;
                }
                Command::RenameTitle => {
                    match agent.rename_title() {
                        Ok(title) => {
                            let _ = events.send(Msg::Agent(Event::Notice(format!("Renamed to {title}"))));
                            let _ = events.send(Msg::Agent(Event::Titled(title)));
                        }
                        Err(error) => _ = events.send(Msg::Agent(Event::Notice(error))),
                    }
                    continue;
                }
                Command::Resume(_) | Command::Import(_) => {
                    let opened = match command {
                        Command::Resume(id) => agent.open_session(&id),
                        Command::Import(file) => agent.import_session(&file),
                        _ => unreachable!("the outer arm matched Resume or Import"),
                    };
                    match opened {
                        Ok((messages, accounts)) => {
                            let path = agent.session.display_path();
                            let _ = events.send(Msg::Resumed { id: agent.session.id.clone(), path, messages, spent: agent.spent, notice: agent.session.fork_notice(), accounts });
                            continue;
                        }
                        Err(error) => Err(error),
                    }
                }
                Command::Context => {
                    let _ = events.send(Msg::Context { split: agent.context_split(), session: agent.usage });
                    continue;
                }
                Command::Changes => {
                    let _ = events.send(Msg::Changes(agent.changes()));
                    continue;
                }
                Command::RewindPoints => {
                    let _ = events.send(Msg::RewindPoints(agent::rewind_points(&agent.messages)));
                    continue;
                }
                Command::Rewind { at, entry } => match agent.rewind(at) {
                    Ok(files) => {
                        let _ = events.send(Msg::Rewound { messages: agent.messages.clone(), files, entry });
                        continue;
                    }
                    Err(error) => Err(error),
                },
                Command::Worktree(cwd) => match agent.move_to(cwd.clone()) {
                    Ok(()) => {
                        let path = agent.session.display_path();
                        let _ = events.send(Msg::Moved { id: agent.session.id.clone(), path, cwd });
                        continue;
                    }
                    Err(error) => Err(error),
                },
                Command::Clone => match agent.clone_session() {
                    Ok(()) => {
                        let path = agent.session.display_path();
                        let _ = events.send(Msg::Cloned { id: agent.session.id.clone(), path });
                        continue;
                    }
                    Err(error) => Err(error),
                },
                Command::Export(out) => {
                    let notice = match agent.export_session(&out) {
                        Ok(()) => format!("Exported to {}", out.display()),
                        Err(error) => error,
                    };
                    let _ = events.send(Msg::Agent(Event::Notice(notice)));
                    continue;
                }
                Command::Workspace(args) => {
                    let _ = events.send(Msg::Agent(Event::Notice(workspace(&mut agent, &args))));
                    continue;
                }
                Command::AccountPicked => {
                    agent.plugins.event(json!({"type": "account_change", "context": agent.plugin_context()}));
                    continue;
                }
                Command::AccountReplaced { previous_id, account_id } => {
                    let params = json!({"type": "account_replace", "previousAccountId": previous_id, "accountId": account_id, "context": agent.plugin_context()});
                    agent.plugins.event(params);
                    continue;
                }
                Command::PluginEvent(name, mut params) => {
                    params["context"] = agent.plugin_context();
                    agent.plugins.event_to(&name, params);
                    continue;
                }
                Command::Plugin(name, args) => {
                    let notice = match agent.plugins.command(&name, &args, &agent.plugin_context()) {
                        Some(Ok(text) | Err(text)) => text,
                        None => format!("No running plugin offers /{name}."),
                    };
                    let _ = events.send(Msg::Agent(Event::Notice(notice)));
                    continue;
                }
                Command::FollowUps => agent.run_follow_ups(&mut |event| {
                    let _ = events.send(Msg::Agent(event));
                }),
                Command::SaveSpent(_) | Command::SaveGoal => {
                    let saved = match command {
                        Command::SaveSpent(spent) => agent.save_spent(spent),
                        _ => agent.save_goal(),
                    };
                    if let Err(error) = saved {
                        let _ = events.send(Msg::Agent(Event::Notice(error)));
                    }
                    continue;
                }
            };
            if events.send(Msg::Done(result)).is_err() {
                return;
            }
        }
    });
}

/// The notice /workspace shows: the result of add or remove, then the list.
fn workspace(agent: &mut Agent, args: &str) -> String {
    let changed = match args.split_once(' ').map(|(action, path)| (action, path.trim())) {
        Some(("add", path)) => agent.add_dir(path).map(|notice| notice + "\n"),
        Some(("remove", path)) => agent.remove_dir(path).map(|notice| notice + "\n"),
        None if args.is_empty() || args == "list" => Ok(String::new()),
        _ => return "Usage: /workspace [add <path> | remove <path> | list]".into(),
    };
    let changed = match changed {
        Ok(changed) => changed,
        Err(error) => return error,
    };
    let dirs: Vec<String> = agent.extra_dirs.iter().map(|dir| format!("  {}", dir.display())).collect();
    let list = if dirs.is_empty() { "No extra directories. Add one with /workspace add <path>.".to_string() } else { format!("Extra directories:\n{}", dirs.join("\n")) };
    format!("{changed}{list}")
}

fn settle_model(agent: &mut Agent, settings: &mut Settings, failures: Vec<String>) -> Msg {
    let model = agent.settle_model(settings).map(|swapped| (settings.model.clone(), swapped));
    Msg::ModelSettled { model, failures }
}

fn spawn_update(home: PathBuf, channel: String, events: Sender<Msg>) {
    std::thread::spawn(move || {
        let notice = match crate::update::install_latest(&home, &channel) {
            Ok(Some(version)) => format!("Updated to v{version}. Restart agent to use it."),
            Ok(None) => return,
            Err(error) => format!("Update failed: {error}"),
        };
        let _ = events.send(Msg::Agent(Event::Notice(notice)));
    });
}

/// One thread per command, so a long-running entry such as a dev server does not hold back the rest.
fn spawn_on_start(commands: Vec<String>, events: Sender<Msg>) {
    for command in commands {
        let events = events.clone();
        std::thread::spawn(move || {
            let failure = match crate::process::command("sh").args(["-c", &command]).output() {
                Ok(output) if output.status.success() => return,
                Ok(output) => tail(&String::from_utf8_lossy(&output.stderr)),
                Err(e) => e.to_string(),
            };
            let _ = events.send(Msg::Agent(Event::Notice(format!("on_start \"{command}\" failed: {failure}"))));
        });
    }
}

/// `install apply` restarts the proxy, so a reinstall at start can fail the first request or two.
fn spawn_headroom_sync(home: PathBuf, env: std::collections::BTreeMap<String, String>, events: Sender<Msg>) {
    std::thread::spawn(move || {
        if let Err(error) = crate::headroom::sync(&home, &env) {
            let _ = events.send(Msg::Agent(Event::Notice(format!("headroom sync failed: {}", tail(&error)))));
        }
    });
}

fn tail(text: &str) -> String {
    let lines: Vec<&str> = text.trim_end().lines().collect();
    lines[lines.len().saturating_sub(5)..].join("\n")
}

fn spawn_input(events: Sender<Msg>) {
    std::thread::spawn(move || {
        while let Ok(input) = event::read() {
            if events.send(Msg::Input(input)).is_err() {
                return;
            }
        }
    });
}

fn event_loop(app: &mut App, inbox: &Receiver<Msg>, out: &mut Stdout) -> Result<(), String> {
    let mut screen = Screen::new();
    loop {
        let (width, height) = terminal::size().map_err(|e| e.to_string())?;
        let (rows, cursor) = app.frame(width as usize, height as usize);
        screen.draw(out, rows, Some(cursor)).map_err(|e| e.to_string())?;
        let wait = if app.edge_drag.is_some() {
            EDGE_SCROLL
        } else if app.working.is_some()
            || app.hint.is_some()
            || app.transcribing.is_some()
            || (matches!(app.panel, Some(Panel::Subagents { .. })) && app.subagents.lock().unwrap().running() > 0)
            || (matches!(app.panel, Some(Panel::Terminals { .. })) && app.terminals.lock().unwrap().running() > 0)
        {
            ANIMATION_TICK
        } else if app.recording.is_some() {
            Duration::from_secs(1)
        } else if app.terminals.lock().unwrap().running() > 0 || app.subagents.lock().unwrap().running() > 0 {
            // A terminal or background subagent that ends while idle queues a follow-up that must start a turn.
            Duration::from_secs(1)
        } else {
            Duration::from_secs(3600)
        };
        match inbox.recv_timeout(wait) {
            Ok(msg) => {
                app.handle(msg, &mut screen);
                while let Ok(msg) = inbox.try_recv() {
                    app.handle(msg, &mut screen);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Err("The agent thread stopped.".into()),
        }
        if let Some((mouse, at)) = app.edge_drag
            && at.elapsed() >= EDGE_SCROLL
        {
            app.mouse(mouse);
        }
        if app.hint.as_ref().is_some_and(|(_, at)| at.elapsed() > HINT_LIFETIME) {
            app.hint = None;
        }
        // Steering typed after the turn's last check, e.g. while edit hooks ran, still needs a turn.
        if app.working.is_none() && (!app.follow_ups.lock().unwrap().is_empty() || !app.steering.lock().unwrap().is_empty()) {
            app.start(Command::FollowUps);
        }
        if app.working.is_none()
            && let Some(command) = app.deferred_commands.pop_front()
        {
            app.start(command);
        }
        while app.working.is_none() && !app.queued_commands.is_empty() {
            let (text, images) = app.queued_commands.pop_front().expect("checked non-empty above");
            // Images pasted into the editor meanwhile belong to the next prompt, so they sit out this one.
            let pasted = std::mem::replace(&mut app.images, images);
            app.submit(text);
            app.images = pasted;
        }
        if app.quit {
            return Ok(());
        }
    }
}

impl App {
    fn new(agent: &Agent, setup: Setup, commands: Sender<Command>, events: Sender<Msg>) -> App {
        let Setup { home, settings, modes_error, plugin_updates, .. } = setup;
        let mut recent_prompts: Vec<String> = history::load(&home.join(".agents")).into_iter().map(|entry| entry.prompt).collect();
        recent_prompts.drain(..recent_prompts.len().saturating_sub(100));
        let slash_commands: Vec<(String, String)> = BUILTIN_COMMANDS.iter().map(|(name, _, description)| (name.to_string(), description.to_string())).collect();
        let mut app = App {
            entries: Vec::new(),
            editor: Editor::with_history(recent_prompts),
            commands,
            working: None,
            recording: None,
            transcribing: None,
            focused: None,
            prompt_usage: Usage::default(),
            spent: agent.spent,
            expanded: settings.expand_tools,
            scroll: 0,
            home,
            settings,
            context_tokens: 0,
            last_usage: Usage::default(),
            session_usage: agent.session.path.as_deref().map(usage::session_totals).unwrap_or_default(),
            git: git_info(&agent.cwd),
            plugins: agent.plugins.clone(),
            mcp: agent.mcp.clone(),
            cwd: agent.cwd.clone(),
            path_matches: Default::default(),
            steering: agent.steering.clone(),
            queued_commands: VecDeque::new(),
            deferred_commands: VecDeque::new(),
            abort: agent.abort.clone(),
            permission_mode: agent.permission_mode.clone(),
            hint: None,
            images: Vec::new(),
            quit: false,
            skills: Vec::new(),
            templates: Vec::new(),
            slash_commands,
            menu_index: 0,
            header_len: 0,
            session_id: agent.session.id.clone(),
            session_path: agent.session.display_path(),
            sessions_dir: agent.sessions_dir.clone(),
            panel: None,
            selection: Selection::default(),
            edge_drag: None,
            transcript_view: (0, 0),
            transcript_size: (0, 0),
            editor_width: 0,
            input_selection: Selection::default(),
            selecting_input: false,
            input_view: (0, 0, 0),
            input_top: 0,
            todos: agent.todos.clone(),
            goal: agent.goal.clone(),
            follow_ups: agent.follow_ups.clone(),
            terminals: agent.terminals.clone(),
            subagents: agent.subagents.clone(),
            client: agent.client.clone(),
            events,
            btw: None,
        };
        app.push(Item::Lines(banner()));
        app.load_skills_and_prompts();
        app.system("skills", format!("{} loaded", app.enabled_skills().count()));
        app.report_conflicts();
        let levels = [("ponytail", modes::PONYTAIL_BANNER, app.settings.ponytail.clone()), ("caveman", modes::CAVEMAN_BANNER, app.settings.caveman.clone())];
        for (name, banner, level) in levels {
            if agent.modes.contains(banner) {
                app.system(name, format!("loaded, level {level}"));
            }
        }
        if let Some(error) = modes_error {
            app.push(Item::Error(error));
        }
        for notice in plugin_updates {
            app.system("plugins", notice);
        }
        app.header_len = app.entries.len();
        app.push_history(&agent.messages);
        if let Some(notice) = agent.session.fork_notice() {
            app.system("session", notice);
        }
        app
    }

    /// A resumed transcript has no per-answer lines, since those are not messages, so the session's total time follows it.
    fn push_history(&mut self, messages: &[Value]) {
        for item in history_items(messages) {
            self.push(item);
        }
        if !self.spent.time.is_zero() {
            self.push(Item::Lines(vec![vec![span(format!(" total {}", duration(self.spent.time)), MUTED)]]));
        }
    }

    fn push(&mut self, item: Item) {
        self.entries.push(Entry { item, cache: None });
    }

    fn system(&mut self, source: &str, text: impl Into<String>) {
        self.push(Item::System { source: source.to_string(), text: text.into(), failed: false });
    }

    /// pj's mouse selection over the transcript: release copies, a drag past its top or bottom scrolls.
    fn mouse(&mut self, mouse: MouseEvent) {
        let (visible, _) = self.transcript_view;
        let row = mouse.row as usize;
        self.edge_drag = None;
        if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
            self.input_selection = Selection::default();
            let (top, rows, _) = self.input_view;
            self.selecting_input = (top..top + rows).contains(&row);
        }
        let wheel = matches!(mouse.kind, MouseEventKind::ScrollUp | MouseEventKind::ScrollDown);
        let (top, rows, _) = self.input_view;
        if wheel && (top..top + rows).contains(&row) {
            let step = self.settings.scroll_lines as isize;
            self.editor.scroll(self.editor_width, if mouse.kind == MouseEventKind::ScrollUp { -step } else { step });
            return;
        }
        if self.selecting_input && !wheel {
            self.input_mouse(mouse.kind, row, mouse.column);
            return;
        }
        match mouse.kind {
            MouseEventKind::ScrollUp => self.scroll += self.settings.scroll_lines,
            MouseEventKind::ScrollDown => self.scroll = self.scroll.saturating_sub(self.settings.scroll_lines),
            MouseEventKind::Down(MouseButton::Left) if row < visible => {
                let point = self.point_at(row, mouse.column);
                let text = self.transcript_text(point.line);
                self.selection.press(point, &text);
            }
            MouseEventKind::Down(MouseButton::Left) => self.selection = Selection::default(),
            MouseEventKind::Drag(MouseButton::Left) if visible > 0 => {
                // Edge scroll stops at either end, so a drag whose button-up never arrives cannot scroll forever.
                let max_scroll = self.transcript_size.0.saturating_sub(visible);
                if self.selection.dragging && row == 0 && self.scroll < max_scroll {
                    self.scroll += 1;
                    self.edge_drag = Some((mouse, Instant::now()));
                } else if self.selection.dragging && row >= visible && self.scroll > 0 {
                    self.scroll -= 1;
                    self.edge_drag = Some((mouse, Instant::now()));
                }
                let point = self.point_at(row.min(visible - 1), mouse.column);
                let text = self.transcript_text(point.line);
                self.selection.drag(point, &text);
            }
            MouseEventKind::Up(MouseButton::Left) if self.selection.release() => {
                if self.settings.copy_on_select {
                    let text = self.selected_text();
                    self.copy_selection(&text);
                }
            }
            MouseEventKind::Up(MouseButton::Left) if row < visible => {
                let point = self.point_at(row, mouse.column);
                let line = self.transcript_line(point.line);
                let Some(url) = link_at(&line, point.column) else { return };
                if let Err(error) = open_in_browser(&url) {
                    self.push(Item::Error(error));
                }
            }
            _ => {}
        }
    }

    /// Same selection as the transcript, over the editor rows; a drag outside them clamps to the edge row.
    fn input_mouse(&mut self, kind: MouseEventKind, row: usize, column: u16) {
        let (top, rows, below) = self.input_view;
        let row = row.clamp(top, top + rows - 1) - top;
        let point = Point { line: below + rows - 1 - row, column: column as usize };
        let text = text_in(&self.input_line(point.line), 0, usize::MAX);
        match kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.selection = Selection::default();
                self.input_selection.press(point, &text);
            }
            MouseEventKind::Drag(MouseButton::Left) => self.input_selection.drag(point, &text),
            MouseEventKind::Up(MouseButton::Left) if self.input_selection.release() && self.settings.copy_on_select => {
                let text = self.input_selected_text();
                self.copy_selection(&text);
            }
            _ => {}
        }
    }

    fn input_selected_text(&self) -> String {
        let Some((start, end)) = self.input_selection.range() else { return String::new() };
        let mut lines = Vec::new();
        for line in (end.line..=start.line).rev() {
            let (from, to) = self.input_selection.columns(line).expect("line is inside the range");
            // Every editor row starts with a 2-column prompt ("❯ ", "$ " or "  ").
            lines.push(text_in(&self.input_line(line), from.max(2), to).trim_end().to_string());
        }
        lines.join("\n")
    }

    /// Editor row `line`, counted up from the last row, prompt included like transcript gutters.
    /// Empty when a paste, slash command or transcript shrank the editor under a selection made earlier.
    fn input_line(&self, line: usize) -> Line {
        let (mut rows, _) = self.editor.render(self.editor_width);
        let Some(index) = rows.len().checked_sub(line + 1) else { return Line::new() };
        rows.swap_remove(index)
    }

    fn copy_selection(&mut self, text: &str) {
        if text.trim().is_empty() {
            self.hint = Some(("Selection is empty".into(), Instant::now()));
            return;
        }
        match copy_to_clipboard(text) {
            Ok(()) => self.hint = Some(("Copied selection".into(), Instant::now())),
            Err(error) => self.push(Item::Error(error)),
        }
    }

    fn point_at(&self, row: usize, column: u16) -> Point {
        let (visible, _) = self.transcript_view;
        Point { line: self.scroll + visible - 1 - row, column: column as usize }
    }

    /// Plain text of transcript line `line` (counted up from the last one) as rendered at the last frame's width.
    fn transcript_text(&mut self, line: usize) -> String {
        self.transcript_line(line).iter().map(|s| s.text.as_str()).collect()
    }

    fn transcript_line(&mut self, line: usize) -> Line {
        let (_, width) = self.transcript_view;
        let (mut tail, _, _) = self.transcript_tail(width, line + 1);
        let Some(index) = tail.len().checked_sub(line + 1) else { return Line::new() };
        tail.swap_remove(index)
    }

    /// shortcut: copies gutters at the start of each source line, except the user prompt bar; strip them if that gets in the way.
    fn selected_text(&mut self) -> String {
        let Some((start, end)) = self.selection.range() else { return String::new() };
        let (_, width) = self.transcript_view;
        let (tail, _, _) = self.transcript_tail(width, start.line + 1);
        let mut rows = Vec::new();
        for line in (end.line..=start.line).rev() {
            let Some(index) = tail.len().checked_sub(line + 1) else { continue };
            let row = &tail[index];
            let (from, to) = self.selection.columns(line).expect("line is inside the range");
            let from = if text_in(row, 0, 2) == "┃ " { from.max(2) } else { from };
            rows.push((row, from, to));
        }
        plain_text(&rows)
    }

    fn handle(&mut self, msg: Msg, screen: &mut Screen) {
        match msg {
            Msg::Input(Input::Key(key)) if key.kind != KeyEventKind::Release => {
                // Typing proves focus, also in terminals that never report it.
                self.focused = Some(true);
                // A drag released outside the window sends no button-up, so a key stops its edge scroll.
                self.edge_drag = None;
                self.key(key, screen);
            }
            Msg::Input(Input::Paste(text)) => self.paste(&text),
            Msg::Input(Input::Resize(..)) => screen.invalidate(),
            Msg::Input(Input::Mouse(mouse)) => self.mouse(mouse),
            Msg::Input(Input::FocusGained) => self.focused = Some(true),
            Msg::Input(Input::FocusLost) => self.focused = Some(false),
            Msg::Input(_) => {}
            Msg::Agent(event) => self.agent_event(event),
            Msg::Done(result) => self.done(result),
            Msg::ModelSettled { model, failures } => {
                for failure in failures {
                    self.push(Item::System { source: "models".into(), text: failure, failed: true });
                }
                match model {
                    Ok((model, swapped)) => {
                        self.settings.model = model;
                        if let Some(notice) = swapped {
                            self.system("models", notice);
                        }
                    }
                    Err(error) => self.push(Item::Error(error)),
                }
            }
            Msg::Resumed { id, path, messages, spent, notice, accounts } => {
                self.working = None;
                self.spent = spent;
                self.entries.truncate(self.header_len);
                self.push_history(&messages);
                self.system("session", format!("Resumed {}", self.session_title(&id)));
                if let Some(notice) = notice {
                    self.system("session", notice);
                }
                self.session_id = id;
                self.session_path = path;
                self.context_tokens = 0;
                self.read_session_usage();
                let names = self.client.account_names(&accounts);
                if !names.is_empty() {
                    // For this run only, like --account: /settings saves only the keys it changes.
                    self.settings.default_accounts.extend(names);
                    self.client.configure(&self.settings);
                    let _ = self.commands.send(Command::AccountPicked);
                }
            }
            Msg::BtwDelta(text) => {
                if let Some((_, answer)) = self.btw.as_mut().and_then(|btw| btw.turns.last_mut()) {
                    answer.push_str(&text);
                }
            }
            Msg::BtwDone(result) => {
                if let Some(btw) = self.btw.as_mut() {
                    btw.streaming = false;
                    btw.error = result.err();
                }
            }
            Msg::Moved { id, path, cwd } => {
                self.working = None;
                self.system("worktree", format!("Session moved from {} to {}", self.cwd.display(), cwd.display()));
                report_cwd(&cwd);
                self.git = git_info(&cwd);
                self.cwd = cwd;
                self.session_id = id;
                self.session_path = path;
            }
            Msg::Cloned { id, path } => {
                self.working = None;
                self.system("session", format!("Cloned {} into {id}", self.session_id));
                self.session_id = id;
                self.session_path = path;
            }
            Msg::LoginFinished { provider, cancel, result } => {
                let Some(Panel::Login(login)) = self.panel.as_mut() else { return };
                if let Err(error) = login.finished(provider, &cancel, result) {
                    self.panel = None;
                    self.push(Item::Error(error));
                }
            }
            Msg::UsageFetched | Msg::McpChanged => {}
            Msg::WorktreeRemoved(path, result) => match result {
                Ok(()) => {
                    self.git = git_info(&self.cwd);
                    self.system("worktree", format!("Removed {}", path.display()));
                }
                Err(error) => self.push(Item::Error(error)),
            },
            Msg::PluginsChanged => match self.plugins.take_panel() {
                // A plugin closes only its own panel, and never replaces one the user is in.
                Some((plugin, data)) if data.is_null() => {
                    if matches!(&self.panel, Some(Panel::Plugin { plugin: open, .. }) if *open == plugin) {
                        self.panel = None;
                    }
                }
                Some((plugin, data)) if self.panel.is_none() || matches!(&self.panel, Some(Panel::Plugin { plugin: open, .. }) if *open == plugin) => {
                    let index = match &self.panel {
                        Some(Panel::Plugin { plugin: open, index, .. }) if *open == plugin => *index,
                        _ => 0,
                    };
                    let index = index.min(data["rows"].as_array().map_or(0, |rows| rows.len().saturating_sub(1)));
                    self.panel = Some(Panel::Plugin { plugin, data, index });
                }
                _ => {}
            },
            Msg::Transcribed(result) => {
                self.transcribing = None;
                match result {
                    Ok(text) => self.editor.insert(&text),
                    Err(e) => self.push(Item::Error(e)),
                }
            }
            Msg::RewindPoints(points) if points.is_empty() => self.hint = Some(("Nothing to rewind yet".into(), Instant::now())),
            Msg::RewindPoints(points) => self.replace_panel(Panel::Rewind { index: points.len() - 1, points }),
            Msg::Changes(Ok(None)) => self.system("changes", "File snapshots are off outside a git repo."),
            Msg::Changes(Ok(Some(files))) if files.is_empty() => self.system("changes", "No files changed in this session."),
            Msg::Changes(Ok(Some(files))) => self.replace_panel(Panel::Changes { files, index: 0 }),
            Msg::Changes(Err(error)) => self.push(Item::Error(error)),
            Msg::Context { split, session } => {
                let window = self.context_window();
                self.push(Item::Lines(context_lines(split, window, session, self.last_usage)));
            }
            Msg::Rewound { messages, files, entry } => {
                match entry {
                    Some(entry) => self.entries.truncate(entry),
                    None => {
                        self.entries.truncate(self.header_len);
                        self.push_history(&messages);
                    }
                }
                self.context_tokens = 0;
                self.system("rewind", rewind_note(files));
            }
        }
    }

    /// Paste lands in the input that has focus: an open panel's field, or the editor when no panel is open.
    fn paste(&mut self, text: &str) {
        let line = text.replace(['\r', '\n'], " ");
        match self.panel.as_mut() {
            Some(Panel::Login(login)) => login.paste(text),
            Some(Panel::Ask(ask)) => ask.paste(&line),
            Some(Panel::Models { query, index, .. } | Panel::ContextWindows { query, index, .. } | Panel::Accounts { query, index, .. } | Panel::History { query, index, .. }) => {
                query.insert(&line);
                *index = 0;
            }
            Some(Panel::Settings { row, query }) => {
                query.insert(&line);
                if let Some(&first) = matching_settings(&query.text()).first() {
                    *row = first;
                }
            }
            Some(Panel::Btw) => self.btw.as_mut().expect("the btw panel opens with a thread").input.insert(&line),
            Some(_) => {}
            None => match image::pasted_path(text) {
                Some(path) => self.attach(image::block(&path)),
                None => self.editor.paste(text),
            },
        }
    }

    fn key(&mut self, key: KeyEvent, screen: &mut Screen) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        if self.panel.is_some() {
            self.panel_key(key);
            return;
        }
        if self.recording.is_some() {
            match key.code {
                KeyCode::Enter => self.stop_recording(),
                KeyCode::Esc => self.recording.take().unwrap().cancel(),
                KeyCode::Char('c') if ctrl => self.recording.take().unwrap().cancel(),
                _ => {}
            }
            return;
        }
        let matches = self.menu_matches();
        let menu_len = matches.len();
        if menu_len > 0 && !(ctrl || alt || shift) {
            match key.code {
                KeyCode::Up => self.menu_index = (self.menu_index.min(menu_len - 1) + menu_len - 1) % menu_len,
                KeyCode::Down => self.menu_index = (self.menu_index.min(menu_len - 1) + 1) % menu_len,
                KeyCode::Tab | KeyCode::Enter if self.file_query().is_some() => {
                    let path = &matches[self.menu_index.min(menu_len - 1)].0;
                    self.accept_path(path);
                }
                KeyCode::Tab => self.accept_slash(false),
                KeyCode::Enter => self.accept_slash(true),
                _ => self.edit(key, ctrl, alt, shift, screen),
            }
            return;
        }
        self.edit(key, ctrl, alt, shift, screen);
    }

    fn edit(&mut self, key: KeyEvent, ctrl: bool, alt: bool, shift: bool, screen: &mut Screen) {
        let before = self.editor.text();
        self.edit_key(key, ctrl, alt, shift, screen);
        if self.editor.text() != before {
            self.menu_index = 0;
            self.input_selection = Selection::default();
        }
    }

    fn edit_key(&mut self, key: KeyEvent, ctrl: bool, alt: bool, shift: bool, screen: &mut Screen) {
        match key.code {
            KeyCode::Enter if shift || (alt && self.working.is_none()) => self.editor.newline(),
            KeyCode::Char('j') if ctrl => self.editor.newline(),
            KeyCode::Enter => {
                if self.editor.text().trim().is_empty() {
                    return;
                }
                let text = self.editor.take();
                if let Err(error) = history::append(&self.home.join(".agents"), history::Entry { prompt: text.clone(), cwd: self.cwd.clone(), session: self.session_id.clone() }) {
                    self.push(Item::Error(error));
                }
                // pi's follow-up: Alt+Enter while working waits for the turn to end instead of steering it.
                if alt {
                    self.queued_commands.push_back((text, std::mem::take(&mut self.images)));
                } else {
                    self.submit(text);
                }
            }
            KeyCode::Esc if self.working.is_some() => {
                if !self.abort.swap(true, Ordering::Relaxed) {
                    self.system("agent", "Interrupting...");
                }
            }
            KeyCode::Esc if self.hint.as_ref().is_some_and(|(text, _)| text == REWIND_HINT) => {
                self.hint = None;
                let _ = self.commands.send(Command::RewindPoints);
            }
            KeyCode::Esc => self.hint = Some((REWIND_HINT.into(), Instant::now())),
            KeyCode::Char('c') if ctrl => self.ctrl_c(),
            KeyCode::Char('d') if ctrl && self.editor.is_empty() => self.quit = true,
            KeyCode::Char('o') if ctrl => {
                self.expanded = !self.expanded;
            }
            KeyCode::Char('l') if ctrl => screen.invalidate(),
            KeyCode::Char('r') if ctrl => self.open_history(),
            KeyCode::BackTab => self.cycle_thinking(),
            KeyCode::Char('p') if ctrl => self.cycle_model(),
            KeyCode::Up if alt => {
                let mut queued = std::mem::take(&mut *self.steering.lock().unwrap());
                for (mut text, images) in std::mem::take(&mut self.queued_commands) {
                    let offset = self.images.len();
                    // Highest first: with offset 1, #1 becomes #2 only after the old #2 became #3.
                    for number in (1..=images.len()).rev() {
                        text = text.replace(&format!("[Image #{number}]"), &format!("[Image #{}]", number + offset));
                    }
                    self.images.extend(images);
                    queued.push(text);
                }
                if !queued.is_empty() {
                    self.editor.set(&queued.join("\n\n"));
                }
            }
            KeyCode::Down if alt => self.scroll = 0,
            KeyCode::PageUp => self.scroll += self.transcript_view.0.max(1),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(self.transcript_view.0.max(1)),
            KeyCode::Up => self.editor.up(self.editor_width),
            KeyCode::Down if self.editor.is_empty() && self.subagents.lock().unwrap().running() > 0 => {
                self.open_subagents();
            }
            KeyCode::Down => self.editor.down(self.editor_width),
            KeyCode::Char('v') if ctrl => self.attach(image::from_clipboard()),
            _ => _ = self.editor.edit_key(key),
        }
    }

    fn attach(&mut self, image: Result<Value, String>) {
        match image {
            Ok(block) => {
                self.images.push(block);
                self.editor.insert(&format!("[Image #{}] ", self.images.len()));
            }
            Err(error) => self.hint = Some((error, Instant::now())),
        }
    }

    /// The pasted images whose `[Image #n]` is still in `text`, shown as `read_image` rows; the rest are dropped.
    fn take_images(&mut self, text: &str) -> Vec<Value> {
        let mut kept = Vec::new();
        for (index, block) in std::mem::take(&mut self.images).into_iter().enumerate() {
            if text.contains(&format!("[Image #{}]", index + 1)) {
                self.push(Item::Tool { id: String::new(), name: "read_image".into(), input: json!({"image": image_note(index + 1, &block)}), result: Some((String::new(), false)) });
                kept.push(block);
            }
        }
        kept
    }

    /// Puts a rewound prompt back in an empty editor, with its images under their `[Image #n]` numbers again.
    fn restore_draft(&mut self, point: RewindPoint) {
        if !self.editor.is_empty() {
            return;
        }
        self.editor.set(&point.text);
        self.images = image_slots(&point.text, point.images);
    }

    fn stop_recording(&mut self) {
        let recording = self.recording.take().unwrap();
        let events = self.events.clone();
        self.transcribing = Some(Instant::now());
        std::thread::spawn(move || {
            let _ = events.send(Msg::Transcribed(recording.transcribe()));
        });
    }

    fn ctrl_c(&mut self) {
        if !self.settings.copy_on_select {
            let text = if self.selection.range().is_some() { self.selected_text() } else { self.input_selected_text() };
            if !text.trim().is_empty() {
                self.copy_selection(&text);
                self.selection = Selection::default();
                self.input_selection = Selection::default();
                return;
            }
        }
        if !self.editor.is_empty() {
            self.editor.set("");
            return;
        }
        if self.hint.as_ref().is_some_and(|(text, _)| text == EXIT_HINT) {
            self.abort.store(true, Ordering::Relaxed);
            self.quit = true;
            return;
        }
        self.hint = Some((EXIT_HINT.into(), Instant::now()));
    }

    fn cycle_thinking(&mut self) {
        self.update_settings(|s| s.thinking = step(&THINKING_LEVELS, &s.thinking.as_str(), true).to_string());
        self.hint = Some((format!("Thinking level: {}", self.settings.thinking), Instant::now()));
    }

    /// Switches to the next `cycle_models` entry, skipping ids missing from the catalog.
    fn cycle_model(&mut self) {
        if self.settings.cycle_models.is_empty() {
            self.hint = Some(("Set cycle_models in settings.json to switch models with Ctrl+P".into(), Instant::now()));
            return;
        }
        let catalog = self.client.catalog();
        if catalog.is_empty() {
            self.hint = Some((MODELS_STILL_LOADING.into(), Instant::now()));
            return;
        }
        let (available, missing): (Vec<String>, Vec<String>) = self.settings.cycle_models.iter().cloned().partition(|id| catalog.iter().any(|m| m.id == *id));
        if !missing.is_empty() {
            self.system("models", format!("Not available, skipped: {}", missing.join(", ")));
        }
        if available.is_empty() {
            return;
        }
        let model = step(&available, &self.settings.model, true);
        self.update_settings(|s| s.model = model);
        self.hint = Some((format!("Model: {}", self.settings.model), Instant::now()));
    }

    fn plugin_settings_key(&mut self, key: KeyEvent) {
        let Some(Panel::PluginSettings { plugin, index, edit }) = self.panel.as_mut() else { return };
        let plugin = plugin.clone();
        let declared = self.plugins.list().into_iter().find(|view| view.name == plugin).map(|view| view.settings).unwrap_or_default();
        let Some(entry) = declared.get(*index).cloned() else {
            if key.code == KeyCode::Esc {
                self.panel = Some(Panel::Plugins { index: 0, confirm: false });
            }
            return;
        };
        let key_name = entry["key"].as_str().unwrap_or_default().to_string();
        let value = match (key.code, edit.as_mut()) {
            (KeyCode::Esc, Some(_)) => {
                *edit = None;
                return;
            }
            (KeyCode::Enter, Some(editor)) => {
                let text = editor.text();
                *edit = None;
                json!(text)
            }
            (_, Some(editor)) => {
                editor.edit_key(key);
                return;
            }
            (KeyCode::Esc, None) => {
                self.panel = Some(Panel::Plugins { index: 0, confirm: false });
                return;
            }
            (KeyCode::Up, None) => {
                *index = index.saturating_sub(1);
                return;
            }
            (KeyCode::Down, None) => {
                *index = (*index + 1).min(declared.len() - 1);
                return;
            }
            (KeyCode::Enter | KeyCode::Char(' '), None) => match crate::plugins::setting(&entry, &self.settings.plugin_settings[plugin.as_str()]) {
                Value::Bool(on) => json!(!on),
                value => {
                    let mut editor = Editor::new();
                    editor.set(value.as_str().unwrap_or_default());
                    *edit = Some(editor);
                    return;
                }
            },
            _ => return,
        };
        self.update_settings(|s| {
            if !s.plugin_settings[plugin.as_str()].is_object() {
                s.plugin_settings[plugin.as_str()] = json!({});
            }
            s.plugin_settings[plugin.as_str()][key_name.as_str()] = value;
        });
        self.plugins.sync(&self.settings.disabled_plugins, &self.settings.plugin_settings);
    }

    /// Changes, saves and applies settings; the agent thread picks them up before its next turn.
    fn update_settings(&mut self, change: impl FnOnce(&mut Settings)) {
        let old = self.settings.clone();
        change(&mut self.settings);
        if self.working.is_some() && (self.settings.model != old.model || self.settings.thinking != old.thinking) {
            self.system("settings", "Model and thinking changes apply from the next turn.");
        }
        self.expanded = self.settings.expand_tools;
        *self.permission_mode.lock().unwrap() = self.settings.permission_mode;
        self.client.configure(&self.settings);
        // Startup already refreshed the rules; a new level downloads only its missing file, which can take seconds offline.
        if (self.settings.ponytail != old.ponytail || self.settings.caveman != old.caveman)
            && let Err(error) = modes::fetch(&self.home, &self.settings.ponytail, &self.settings.caveman, false)
        {
            self.push(Item::Error(error));
        }
        if self.settings.conflict_priority != old.conflict_priority || self.settings.trusted_projects != old.trusted_projects {
            self.load_skills_and_prompts();
            self.report_conflicts();
        }
        let _ = self.commands.send(Command::Apply(Box::new(self.settings.clone())));
        if let Err(error) = settings::save_change(&self.home, &old, &self.settings) {
            self.push(Item::Error(error));
        }
    }

    /// None until the model lists load.
    fn model(&self) -> Option<&Model> {
        self.client.catalog().iter().find(|m| m.id == self.settings.model)
    }

    /// Rereads the session file; an unsaved session has none, so its footer shows no cost.
    fn read_session_usage(&mut self) {
        self.session_usage = usage::session_totals(&self.sessions_dir.join(format!("{}.jsonl", self.session_id)));
    }

    /// The settings key of the current model; says so when the model lists are still loading.
    fn loaded_model_key(&mut self) -> Option<String> {
        let key = self.model().map(model_key);
        if key.is_none() {
            self.system("models", MODELS_STILL_LOADING);
        }
        key
    }

    fn context_window(&self) -> u64 {
        self.model().map_or(0, |model| self.settings.context_window(model))
    }

    fn enabled_skills(&self) -> impl Iterator<Item = &Skill> {
        self.skills.iter().filter(|skill| !self.settings.disabled_skills.contains(&skill.name))
    }

    /// Every skill, the disabled ones too: /skills lists them, `enabled_skills` filters by `Settings.disabled_skills`.
    fn load_skills_and_prompts(&mut self) {
        let agent_dir = self.home.join(".agents");
        let trusted = self.settings.trusts(&self.cwd);
        self.skills = crate::agent::prompt::load_skills(&agent_dir, &self.cwd, trusted, &[], self.settings.prefer_global());
        self.templates = crate::agent::prompt::load_templates(&agent_dir, &self.cwd, trusted, self.settings.prefer_global());
    }

    /// Asks whether to trust a project with `.agents` files to load; another open panel gets a notice instead.
    fn offer_trust(&mut self) {
        let files = crate::agent::prompt::untrusted_files(&self.home.join(".agents"), &self.cwd);
        if self.settings.trusts(&self.cwd) || files.is_empty() {
            return;
        }
        let root = crate::agent::prompt::project_root(&self.cwd);
        if self.panel.is_some() {
            self.system("trust", format!("{} is not trusted, so its .agents files stay unloaded and its edit hooks do not run. /trust loads them.", root.display()));
            return;
        }
        self.panel = Some(Panel::Trust { root, files });
    }

    /// /trust and /untrust; the agent rebuilds its system prompt once, before the next turn.
    fn set_trust(&mut self, trusted: bool) {
        let root = crate::agent::prompt::project_root(&self.cwd).display().to_string();
        if trusted {
            // A --trust root is already in the list; dropping it first makes the save see /trust as a change.
            self.settings.trusted_projects.retain(|project| *project != root);
            self.update_settings(|s| s.trusted_projects.push(root.clone()));
        } else {
            self.update_settings(|s| s.trusted_projects.retain(|project| *project != root));
        }
        let text = if trusted { "trusted: its .agents files load and its edit hooks run" } else { "not trusted: its .agents files stay unloaded and its edit hooks do not run" };
        self.system("trust", format!("{root} {text}"));
    }

    /// Names each skill and prompt defined both in the project and in `~/.agents`; `conflict_priority` says which copy loads.
    fn report_conflicts(&mut self) {
        let skills: Vec<&str> = self.skills.iter().filter(|s| s.shadows.is_some()).map(|s| s.name.as_str()).collect();
        let prompts: Vec<&str> = self.templates.iter().filter(|t| t.shadows.is_some()).map(|t| t.name.as_str()).collect();
        let count = skills.len() + prompts.len();
        if count == 0 {
            return;
        }
        let winner = if self.settings.prefer_global() { "~/.agents" } else { "project" };
        let mut text = format!("{count} names are in both the project and ~/.agents, the {winner} copy is used (/settings: Name conflicts prefer)");
        for (kind, names) in [("skills ", skills), ("prompts", prompts)] {
            if !names.is_empty() {
                text.push_str(&format!("\n{kind}  {}", names.join(", ")));
            }
        }
        self.system("conflicts", text);
    }

    fn skill_usage(&mut self) {
        let log = std::fs::read_to_string(self.home.join(".agents/skill-usage.jsonl")).unwrap_or_default();
        let names: Vec<&str> = self.skills.iter().map(|skill| skill.name.as_str()).collect();
        let lines: Vec<String> = skill_usage::counts(&log, &names).iter().map(|(name, count)| format!("{count:>5}  {name}")).collect();
        self.system("skill-usage", format!("uses per skill, least used first\n{}", lines.join("\n")));
    }

    fn rtk_gain(&mut self) {
        match crate::process::command("rtk").arg("gain").output() {
            Ok(output) if output.status.success() => self.system("rtk", String::from_utf8_lossy(&output.stdout).trim_end()),
            Ok(output) => self.push(Item::Error(format!("rtk gain failed: {}", String::from_utf8_lossy(&output.stderr).trim_end()))),
            Err(error) => self.push(Item::Error(format!("Cannot run rtk: {error}"))),
        }
    }

    fn open_resume(&mut self) {
        let sessions: Vec<SessionInfo> = Session::list(&self.sessions_dir).into_iter().filter(|s| s.id != self.session_id).collect();
        self.panel = Some(Panel::Resume { all: false, index: 0, sessions });
    }

    fn submit(&mut self, text: String) {
        self.scroll = 0;
        // A side question and /goal pause work mid-turn, so they skip the steering queue.
        if let Some(question) = command_args(&text, "/btw") {
            self.open_btw(question);
            return;
        }
        if let Some(args) = command_args(&text, "/goal") {
            self.goal_command(args);
            return;
        }
        let local = command_args(&text, "/exit").is_some() || BUILTIN_COMMANDS.iter().any(|(name, mid_turn, _)| *mid_turn == MidTurn::Runs && command_args(&text, &format!("/{name}")).is_some());
        // Plugin commands can carry secrets such as tokens, so they never become steering text.
        let plugin = self.plugins.commands().iter().any(|(name, _)| command_args(&text, &format!("/{name}")).is_some());
        if self.working.is_some() && !local && !plugin {
            // Steering reaches the model as plain text, so commands and prompts with pasted images wait for the turn to end and run then.
            let command = text.starts_with('!')
                || text.trim().starts_with("/skill:")
                || text.trim().starts_with("/prompt:")
                || self.slash_commands.iter().any(|(name, _)| command_args(&text, &format!("/{name}")).is_some());
            if command || !self.images.is_empty() {
                // The images travel with their text, so an image pasted for the next prompt starts again at #1.
                self.queued_commands.push_back((text, std::mem::take(&mut self.images)));
            } else {
                self.steering.lock().unwrap().push(text);
            }
            return;
        }
        let trimmed = text.trim();
        // pi's shell escape: `!cmd` runs and adds its output to the next prompt, `!!cmd` runs without that.
        if let Some(rest) = text.strip_prefix('!') {
            let keep = !rest.starts_with('!');
            let command = rest.trim_start_matches('!').trim();
            if !command.is_empty() {
                self.start(Command::Bash { command: command.to_string(), keep });
            }
            return;
        }
        // Like the official hooks, the phrase turns the mode off and still reaches the model.
        let (stop_ponytail, stop_caveman) = modes::stopped_by(trimmed);
        if stop_ponytail || stop_caveman {
            self.update_settings(|s| {
                if stop_ponytail {
                    s.ponytail = "off".into();
                }
                if stop_caveman {
                    s.caveman = "off".into();
                }
            });
        }
        let (name, args) = trimmed.strip_prefix('/').map_or(("", ""), |rest| rest.split_once(' ').map_or((rest, ""), |(name, args)| (name, args.trim())));
        match name {
            "quit" | "exit" => self.quit = true,
            "compact" => {
                self.system("compact", "Compacting context...");
                self.start(Command::Compact(Some(args.to_string()).filter(|focus| !focus.is_empty())));
            }
            "clear" => {
                self.entries.truncate(self.header_len);
                self.context_tokens = 0;
                self.start(Command::Clear);
            }
            "rename-title" if args.is_empty() => self.push(Item::Error("Usage: /rename-title <name>".into())),
            "rename-title" => {
                let _ = self.commands.send(Command::Rename(args.to_string()));
                set_terminal_title(args);
                self.system("session", format!("Renamed to {args}"));
            }
            "rename-title-ai" => _ = self.commands.send(Command::RenameTitle),
            "clone" => self.start(Command::Clone),
            "import" if args.is_empty() => self.push(Item::Error("Usage: /import <file.jsonl>".into())),
            "import" => self.start(Command::Import(self.cwd.join(args))),
            "export" => {
                let out = if args.is_empty() { format!("session-{}.jsonl", self.session_id) } else { args.to_string() };
                let _ = self.commands.send(Command::Export(self.cwd.join(out)));
            }
            "workspace" => _ = self.commands.send(Command::Workspace(args.to_string())),
            "model" => self.open_models(0, args, ModelsBack::Close),
            "thinking" if args.is_empty() => self.system("thinking", format!("{}. Levels: {}", self.settings.thinking, THINKING_LEVELS.join(", "))),
            "thinking" if THINKING_LEVELS.contains(&args) => {
                self.update_settings(|s| s.thinking = args.to_string());
                self.system("thinking", format!("Level set to {args}"));
            }
            "thinking" => self.push(Item::Error(format!("Unknown thinking level {args}. Levels: {}", THINKING_LEVELS.join(", ")))),
            "fast" => {
                self.update_settings(|s| s.fast = !s.fast);
                self.system("fast", if self.settings.fast { "Fast mode on" } else { "Fast mode off" });
            }
            "ponytail" | "caveman" if args.is_empty() => {
                let (level, levels) = if name == "ponytail" { (&self.settings.ponytail, PONYTAIL_LEVELS) } else { (&self.settings.caveman, CAVEMAN_LEVELS) };
                self.system(name, format!("{level}. Levels: {}", levels.join(", ")));
            }
            "ponytail" if PONYTAIL_LEVELS.contains(&args) => {
                self.update_settings(|s| s.ponytail = args.to_string());
                self.system(name, format!("Level set to {args}"));
            }
            "caveman" if CAVEMAN_LEVELS.contains(&args) => {
                self.update_settings(|s| s.caveman = args.to_string());
                self.system(name, format!("Level set to {args}"));
            }
            "ponytail" => self.push(Item::Error(format!("Unknown ponytail level {args}. Levels: {}", PONYTAIL_LEVELS.join(", ")))),
            "caveman" => self.push(Item::Error(format!("Unknown caveman level {args}. Levels: {}", CAVEMAN_LEVELS.join(", ")))),
            "permissions" if args.is_empty() => {
                let modes: Vec<String> = MODES.iter().map(|m| format!("{:<12}\t{}", m.label(), m.description())).collect();
                self.system("permissions", format!("mode {}\n{}\nChange with /permissions <mode> or /settings", self.settings.permission_mode.label(), modes.join("\n")));
            }
            "permissions" => match Mode::parse(args) {
                Some(mode) => {
                    self.update_settings(|s| s.permission_mode = mode);
                    self.system("permissions", format!("mode set to {}", mode.label()));
                }
                None => self.push(Item::Error(format!("Unknown mode {args}. Modes: {}", MODES.map(Mode::label).join(", ")))),
            },
            "settings" => self.panel = Some(Panel::Settings { row: 0, query: Editor::new() }),
            "todos" => self.panel = Some(Panel::Todos { index: 0 }),
            "plugins" => self.panel = Some(Panel::Plugins { index: 0, confirm: false }),
            "skills" => self.panel = Some(Panel::Skills { index: 0 }),
            "trust" => self.set_trust(true),
            "untrust" => self.set_trust(false),
            "mcp" => self.panel = Some(Panel::Mcp { index: 0, confirm: false }),
            "changes" => _ = self.commands.send(Command::Changes),
            "context" => _ = self.commands.send(Command::Context),
            "history" => self.open_history(),
            "usage" => {
                self.panel = Some(Panel::Usage { tab: 0, totals: usage::totals(&self.sessions_dir, &self.session_id, crate::process::now_ms()) });
                self.refresh_usage_in_background();
            }
            "skill-usage" => self.skill_usage(),
            "rtk" => self.rtk_gain(),
            "ps" if self.terminals.lock().unwrap().list.is_empty() => self.system("ps", "No background terminals yet. The agent starts them with bg_start."),
            "ps" => self.panel = Some(Panel::Terminals { index: 0 }),
            "subagents" if self.subagents.lock().unwrap().list.is_empty() => self.system("subagents", "No subagent runs yet."),
            "subagents" => self.open_subagents(),
            "login" => self.panel = Some(Panel::Login(login::Login::Kind { index: 0 })),
            "voice" => match voice::start(&self.home) {
                Ok(recording) => self.recording = Some(recording),
                Err(e) => self.push(Item::Error(e)),
            },
            "logout" | "account" if self.client.accounts().is_empty() => self.system(name, "No accounts. Add one with /login."),
            "logout" | "account" => {
                self.panel = Some(Panel::Accounts { logout: name == "logout", query: Editor::new(), index: 0 });
                self.refresh_usage_in_background();
            }
            "worktree" => {
                if let Err(error) = self.worktree(args) {
                    self.push(Item::Error(error));
                }
            }
            "context-window" if args.is_empty() => self.system("context-window", format!("{}. Set 100k to 1M or auto for this model, per provider in /settings", tokens(self.context_window()))),
            "context-window" if args == "auto" => {
                let Some(key) = self.loaded_model_key() else { return };
                self.update_settings(|s| {
                    s.context_windows.remove(&key);
                });
                self.system("context-window", format!("Back to {}", tokens(self.context_window())));
            }
            "context-window" => match parse_context_window(args) {
                Some(window) => {
                    let Some(key) = self.loaded_model_key() else { return };
                    self.system("context-window", format!("Set {key} to {}", tokens(window)));
                    self.update_settings(|s| {
                        s.context_windows.insert(key, window);
                    });
                }
                None => self.push(Item::Error(format!("Context window must be 100k to 1M, got {args}"))),
            },
            "copy" => match self.copy_last_answer() {
                Ok(()) => self.system("copy", "Copied last agent message to clipboard"),
                Err(error) => self.push(Item::Error(error)),
            },
            "copy-session-path" => {
                let Some(path) = self.session_path.clone() else {
                    return self.push(Item::Error("This session is not saved, so there is no file to share.".into()));
                };
                match copy_to_clipboard(&path) {
                    Ok(()) => self.system("copy", format!("Copied {path}")),
                    Err(error) => self.push(Item::Error(error)),
                }
            }
            "session" => {
                let file = self.session_path.clone().unwrap_or_else(|| "not saved".into());
                let window = self.context_window();
                let percent = (self.context_tokens * 100).checked_div(window).unwrap_or(0);
                let info = format!("{}\nFile: {file}\nContext: {} of {} ({percent}%)", self.session_id, tokens(self.context_tokens), tokens(window));
                self.system("session", info);
            }
            "hotkeys" => self.push(Item::Lines(hotkeys_lines())),
            "help" => {
                let mut lines: Vec<String> = self.slash_commands.iter().map(|(name, description)| format!("/{name:<14}\t{description}")).collect();
                lines.push(format!("/{:<14}\trun a skill ({} loaded)", "skill:<name>", self.enabled_skills().count()));
                lines.push(format!("/{:<14}\trun a prompt ({} loaded)", "prompt:<name>", self.templates.len()));
                self.system("help", format!("{} commands\n{}", lines.len(), lines.join("\n")));
            }
            "resume" => self.open_resume(),
            skill
                if let Some(skill) = skill.strip_prefix("skill:")
                    && self.enabled_skills().any(|s| s.name == skill) =>
            {
                match crate::agent::prompt::expand_skill(trimmed, &self.skills) {
                    Ok(expanded) => {
                        if let Err(error) = skill_usage::record(&self.home.join(".agents"), skill, skill_usage::By::User, &self.cwd) {
                            self.push(Item::Error(error));
                        }
                        self.push(Item::User(text.clone()));
                        let images = self.take_images(&text);
                        self.start(Command::Prompt(expanded, images));
                    }
                    Err(error) => self.push(Item::Error(error)),
                }
            }
            name if self.plugins.commands().iter().any(|(command, _)| command == name) => {
                let _ = self.commands.send(Command::Plugin(name.to_string(), args.to_string()));
            }
            name if let Some(template) = name.strip_prefix("prompt:").and_then(|name| self.templates.iter().find(|t| t.name == name)) => match crate::agent::prompt::expand_template(template, args) {
                Ok(expanded) => {
                    self.push(Item::User(expanded.clone()));
                    let images = self.take_images(&text);
                    self.start(Command::Prompt(expanded, images));
                }
                Err(error) => self.push(Item::Error(error)),
            },
            _ => {
                self.push(Item::User(text.clone()));
                let images = self.take_images(&text);
                self.start(Command::Prompt(text, images));
            }
        }
    }

    fn refresh_usage_in_background(&self) {
        let (client, events) = (self.client.clone(), self.events.clone());
        std::thread::spawn(move || {
            client.refresh_all_usage();
            let _ = events.send(Msg::UsageFetched);
        });
    }
    /// Keys while a panel is open: Esc and Ctrl+C close it, the rest belongs to the panel.
    fn panel_key(&mut self, key: KeyEvent) {
        let ctrl_c = key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c');
        let key = if ctrl_c { KeyEvent::from(KeyCode::Esc) } else { key };
        let code = key.code;
        if let Some(Panel::Permission { index, opened, .. }) = self.panel.as_mut() {
            // An Enter or digit typed for the prompt just before the panel opened must not approve the call.
            let settling = opened.elapsed() < PERMISSION_SETTLE;
            let choice = match code {
                KeyCode::Up => {
                    *index = index.saturating_sub(1);
                    return;
                }
                KeyCode::Down => {
                    *index = (*index + 1).min(2);
                    return;
                }
                KeyCode::Char('3') | KeyCode::Esc => 2,
                _ if settling => return,
                KeyCode::Char('1') => 0,
                KeyCode::Char('2') => 1,
                KeyCode::Enter => *index,
                _ => return,
            };
            let Some(Panel::Permission { request, reply, .. }) = self.panel.take() else { unreachable!() };
            let decision = match choice {
                0 => Decision::Allow,
                1 => Decision::AllowAlways,
                _ => Decision::Deny,
            };
            if matches!(decision, Decision::Deny) {
                self.system("permissions", format!("Denied {}", request.detail));
            }
            let _ = reply.send(decision);
            return;
        }
        if let Some(Panel::Ask(ask)) = self.panel.as_mut() {
            if ask.key(key) {
                self.panel = None;
            }
            return;
        }
        if let Some(Panel::Login(login)) = self.panel.as_mut() {
            if key.code == KeyCode::Enter && login.ask_overwrite(&self.client.accounts()) {
                return;
            }
            let action = login.key(key);
            self.login_action(action);
            return;
        }
        if let Some(Panel::Trust { .. }) = self.panel {
            self.panel = None;
            match code {
                KeyCode::Char('y') | KeyCode::Enter => self.set_trust(true),
                _ => self.system("trust", "Project files stay unloaded for now. /trust loads them."),
            }
            return;
        }
        match self.panel {
            Some(Panel::Btw) => return self.btw_key(key),
            Some(Panel::Models { .. }) => return self.models_key(key),
            Some(Panel::ContextWindows { .. }) => return self.context_windows_key(key),
            Some(Panel::ProviderOrder { .. }) => return self.provider_order_key(key),
            Some(Panel::Diff { .. }) => return self.diff_key(code),
            Some(Panel::Subagent { .. }) => return self.subagent_key(code),
            Some(Panel::Terminal { .. }) => return self.terminal_key(code),
            Some(Panel::PluginSettings { .. }) => return self.plugin_settings_key(key),
            _ => {}
        }
        if let (KeyCode::Enter, Some(Panel::Plugins { index, .. })) = (code, &self.panel) {
            if let Some(view) = self.plugins.list().into_iter().nth(*index).filter(|view| view.installed) {
                self.panel = Some(Panel::PluginSettings { plugin: view.name, index: 0, edit: None });
            }
            return;
        }
        if let Some(Panel::Plugin { plugin, data, index }) = self.panel.as_mut() {
            let last = data["rows"].as_array().map_or(0, |rows| rows.len().saturating_sub(1));
            match code {
                KeyCode::Up => *index = index.saturating_sub(1),
                KeyCode::Down => *index = (*index + 1).min(last),
                KeyCode::Esc => self.panel = None,
                KeyCode::Enter => {
                    if let Some(id) = data["rows"].get(*index).map(|row| row["id"].clone()) {
                        let params = json!({"type": "panel_select", "panel": data["title"], "id": id});
                        let _ = self.commands.send(Command::PluginEvent(plugin.clone(), params));
                    }
                }
                _ => {}
            }
            return;
        }
        if code == KeyCode::Esc {
            self.panel = None;
            return;
        }
        let cwd = self.cwd.display().to_string();
        match self.panel.as_mut() {
            Some(Panel::Resume { all, index, sessions }) => {
                let visible: Vec<&SessionInfo> = sessions.iter().filter(|s| *all || s.cwd == cwd).collect();
                match code {
                    KeyCode::Up => *index = index.saturating_sub(1),
                    KeyCode::Down => *index = (*index + 1).min(visible.len().saturating_sub(1)),
                    KeyCode::Tab | KeyCode::BackTab => {
                        *all = !*all;
                        *index = 0;
                    }
                    KeyCode::Enter => {
                        let Some(session) = visible.get(*index) else { return };
                        let id = session.id.clone();
                        self.panel = None;
                        self.start(Command::Resume(id));
                    }
                    _ => {}
                }
            }
            Some(Panel::Settings { row, query }) => {
                let rows = matching_settings(&query.text());
                let index = rows.iter().position(|r| r == row);
                match code {
                    KeyCode::Enter if *row == PROVIDER_ORDER_ROW && index.is_some() => self.panel = Some(Panel::ProviderOrder { tab: 0, index: 0 }),
                    KeyCode::Up if !rows.is_empty() => *row = rows[index.map_or(0, |i| i.saturating_sub(1))],
                    KeyCode::Down if !rows.is_empty() => *row = rows[index.map_or(0, |i| (i + 1).min(rows.len() - 1))],
                    KeyCode::Enter | KeyCode::Right | KeyCode::Left if index.is_some() => {
                        let row = *row;
                        self.change_setting(row, code != KeyCode::Left);
                    }
                    _ => {
                        if query.edit_key(key)
                            && let Some(&first) = matching_settings(&query.text()).first()
                        {
                            *row = first;
                        }
                    }
                }
            }
            Some(Panel::Todos { index }) => {
                let mut todos = self.todos.lock().unwrap();
                let positions: Vec<usize> = (0..todos.tasks.len()).filter(|i| todos.tasks[*i].status != Status::Deleted).collect();
                *index = (*index).min(positions.len().saturating_sub(1));
                match code {
                    KeyCode::Up => *index = index.saturating_sub(1),
                    KeyCode::Down => *index = (*index + 1).min(positions.len().saturating_sub(1)),
                    KeyCode::Char(' ') if !positions.is_empty() => todos.cycle(positions[*index]),
                    KeyCode::Char('d') if !positions.is_empty() => todos.delete(positions[*index]),
                    _ => {}
                }
            }
            Some(Panel::Usage { tab, .. }) if matches!(code, KeyCode::Tab | KeyCode::BackTab) => {
                let tabs = USAGE_TABS.len();
                *tab = if code == KeyCode::Tab { (*tab + 1) % tabs } else { (*tab + tabs - 1) % tabs };
            }
            Some(Panel::Skills { index }) => match code {
                KeyCode::Up => *index = index.saturating_sub(1),
                KeyCode::Down => *index = (*index + 1).min(self.skills.len().saturating_sub(1)),
                KeyCode::Char(' ') => {
                    let Some(skill) = self.skills.get(*index) else { return };
                    let name = skill.name.clone();
                    self.update_settings(|s| {
                        if s.disabled_skills.contains(&name) {
                            s.disabled_skills.retain(|disabled| *disabled != name);
                        } else {
                            s.disabled_skills.push(name);
                        }
                    });
                }
                _ => {}
            },
            Some(Panel::Mcp { index, confirm }) => {
                let (views, _) = self.mcp.list();
                *index = (*index).min(views.len().saturating_sub(1));
                let Some(view) = views.get(*index) else { return };
                match code {
                    KeyCode::Up => (*index, *confirm) = (index.saturating_sub(1), false),
                    KeyCode::Down => (*index, *confirm) = ((*index + 1).min(views.len() - 1), false),
                    KeyCode::Char('r') => self.mcp.reconnect(&view.name),
                    KeyCode::Char('l') if view.http => self.mcp.login(&view.name),
                    // An entry the panel cuts is approved on the second press, after the user has read the file.
                    KeyCode::Char(' ') if view.approval.is_some() && !*confirm && terminal::size().map_or(true, |(width, _)| mcp_entry(&view.entry, width as usize).1) => *confirm = true,
                    // A server `mcp.json` turns off stays off; only the file can turn it on.
                    KeyCode::Char(' ') if view.enabled => {
                        *confirm = false;
                        let name = view.name.clone();
                        let approval = view.approval.clone();
                        self.update_settings(|s| match approval {
                            Some(key) => {
                                s.approved_mcp.push(key);
                                s.disabled_mcp.retain(|disabled| *disabled != name);
                            }
                            None if s.disabled_mcp.contains(&name) => s.disabled_mcp.retain(|disabled| *disabled != name),
                            None => s.disabled_mcp.push(name),
                        });
                        self.mcp.sync(&self.settings.disabled_mcp, &self.settings.approved_mcp);
                    }
                    _ => {}
                }
            }
            Some(Panel::Plugins { index, confirm }) => {
                let views = self.plugins.list();
                *index = (*index).min(views.len().saturating_sub(1));
                let Some(view) = views.get(*index) else { return };
                let confirmed = std::mem::take(confirm);
                let changed = match code {
                    KeyCode::Up => {
                        *index = index.saturating_sub(1);
                        return;
                    }
                    KeyCode::Down => {
                        *index = (*index + 1).min(views.len() - 1);
                        return;
                    }
                    KeyCode::Char(' ') if !view.installed => crate::plugins::install(&self.home, &view.name).map(|_| ()),
                    KeyCode::Char(' ') => {
                        let name = view.name.clone();
                        self.update_settings(|s| {
                            if s.disabled_plugins.contains(&name) {
                                s.disabled_plugins.retain(|disabled| *disabled != name);
                            } else {
                                s.disabled_plugins.push(name);
                            }
                        });
                        Ok(())
                    }
                    // Removing deletes the state the plugin keeps next to its code, so it takes a second press.
                    KeyCode::Char('d') if view.installed && !confirmed => {
                        *confirm = true;
                        return;
                    }
                    KeyCode::Char('d') if view.installed => crate::plugins::remove(&self.home, &view.name),
                    _ => return,
                };
                if let Err(error) = changed {
                    self.push(Item::Error(error));
                }
                self.plugins.sync(&self.settings.disabled_plugins, &self.settings.plugin_settings);
            }
            Some(Panel::Worktrees { index, worktrees, confirm, current }) => {
                let confirmed = std::mem::take(confirm);
                match code {
                    KeyCode::Up => *index = index.saturating_sub(1),
                    KeyCode::Down => *index = (*index + 1).min(worktrees.len() - 1),
                    KeyCode::Enter => {
                        let path = worktrees[*index].path.clone();
                        let moves = path != *current;
                        self.panel = None;
                        if moves {
                            self.start(Command::Worktree(path));
                        }
                    }
                    KeyCode::Char('d') if *index == 0 => self.system("worktree", "The main worktree cannot be removed."),
                    KeyCode::Char('d') if worktrees[*index].path == *current => self.system("worktree", "This session runs in that worktree; move it first."),
                    KeyCode::Char('d') if !confirmed => *confirm = true,
                    KeyCode::Char('d') => {
                        let path = worktrees[*index].path.clone();
                        self.panel = None;
                        // Deleting a checkout with node_modules takes seconds, so it runs off the UI thread.
                        self.system("worktree", format!("Removing {}", path.display()));
                        let (cwd, events) = (self.cwd.clone(), self.events.clone());
                        std::thread::spawn(move || {
                            let result = worktree::remove(&cwd, &path);
                            let _ = events.send(Msg::WorktreeRemoved(path, result));
                        });
                    }
                    _ => {}
                }
            }
            Some(Panel::Subagents { index }) => {
                let runs = self.subagents.lock().unwrap();
                let run = runs.list[*index].clone();
                let last = runs.list.len() - 1;
                drop(runs);
                match code {
                    KeyCode::Up => *index = index.saturating_sub(1),
                    KeyCode::Down => *index = (*index + 1).min(last),
                    KeyCode::Char('k') => run.stop(),
                    KeyCode::Enter => self.panel = Some(Panel::Subagent { index: *index, scroll: 0, loaded: None, lines: Vec::new() }),
                    _ => {}
                }
            }
            Some(Panel::Terminals { index }) => {
                let terminals = self.terminals.lock().unwrap();
                if terminals.list.is_empty() {
                    return;
                }
                let last = terminals.list.len() - 1;
                match code {
                    KeyCode::Up => *index = index.saturating_sub(1),
                    KeyCode::Down => *index = (*index + 1).min(last),
                    KeyCode::Char('k') => _ = terminals.list[*index].stop(),
                    KeyCode::Enter => {
                        let index = *index;
                        drop(terminals);
                        self.panel = Some(Panel::Terminal { index, scroll: 0, loaded: None, lines: Vec::new() });
                    }
                    _ => {}
                }
            }
            Some(Panel::Rewind { index, points }) => match code {
                KeyCode::Up => *index = index.saturating_sub(1),
                KeyCode::Down => *index = (*index + 1).min(points.len() - 1),
                KeyCode::Enter => {
                    let entry = rewind_entry(&self.entries, points, *index);
                    let point = points.swap_remove(*index);
                    self.panel = None;
                    let _ = self.commands.send(Command::Rewind { at: point.at, entry });
                    self.restore_draft(point);
                }
                _ => {}
            },
            Some(Panel::Changes { files, index }) => match code {
                KeyCode::Up => *index = index.saturating_sub(1),
                KeyCode::Down => *index = (*index + 1).min(files.len() - 1),
                KeyCode::Enter => {
                    let (files, index) = (std::mem::take(files), *index);
                    self.panel = Some(Panel::Diff { files, index, scroll: DIFF_TOP });
                }
                _ => {}
            },
            Some(Panel::History { entries, tab, query, index }) => {
                let scope = *tab;
                let matches = matching_prompts(entries, &query.text(), |entry| entry.in_scope(scope, &self.cwd, &self.session_id));
                match code {
                    KeyCode::Up => *index = index.saturating_sub(1),
                    KeyCode::Down => *index = (*index + 1).min(matches.len().saturating_sub(1)),
                    KeyCode::Enter => {
                        let Some(prompt) = matches.get(*index).map(|prompt| prompt.to_string()) else { return };
                        self.panel = None;
                        self.editor.set(&prompt);
                        // The old selection may reach rows a shorter prompt no longer has.
                        self.input_selection = Selection::default();
                    }
                    KeyCode::Tab | KeyCode::BackTab => {
                        let tabs = history::SCOPES.len();
                        *tab = if code == KeyCode::Tab { (*tab + 1) % tabs } else { (*tab + tabs - 1) % tabs };
                        *index = 0;
                    }
                    _ => {
                        if query.edit_key(key) {
                            *index = 0;
                        }
                    }
                }
            }
            Some(Panel::Accounts { logout, query, index }) => {
                let accounts = matching_accounts(self.client.accounts(), &query.text());
                let logout = *logout;
                match code {
                    KeyCode::Up => *index = index.saturating_sub(1),
                    KeyCode::Down => *index = (*index + 1).min(accounts.len().saturating_sub(1)),
                    KeyCode::Enter => {
                        let Some(account) = accounts.get(*index) else { return };
                        self.panel = None;
                        if !logout {
                            let (provider, name) = (account.provider.clone(), account.name.clone());
                            self.update_settings(|s| {
                                s.default_accounts.insert(provider, name);
                            });
                            let _ = self.commands.send(Command::AccountPicked);
                            self.system(
                                "account",
                                format!(
                                    "Default {} account: {}{}.",
                                    account.provider,
                                    account.name,
                                    if self.settings.switch_at_percent == 0 { String::new() } else { format!(", until it reaches {}%", self.settings.switch_at_percent) }
                                ),
                            );
                            return;
                        }
                        match self.client.remove(&account.provider, &account.name) {
                            Ok(()) => self.system("logout", format!("Removed {}.", account.name)),
                            Err(error) => self.push(Item::Error(error)),
                        }
                    }
                    _ => {
                        if query.edit_key(key) {
                            *index = 0;
                        }
                    }
                }
            }
            Some(
                Panel::Permission { .. }
                | Panel::Ask(_)
                | Panel::Btw
                | Panel::Login(_)
                | Panel::Models { .. }
                | Panel::ContextWindows { .. }
                | Panel::ProviderOrder { .. }
                | Panel::Subagent { .. }
                | Panel::Terminal { .. }
                | Panel::Diff { .. }
                | Panel::PluginSettings { .. }
                | Panel::Plugin { .. }
                | Panel::Trust { .. }
                | Panel::Usage { .. },
            )
            | None => {}
        }
    }

    fn login_action(&mut self, action: login::Action) {
        match action {
            login::Action::None => {}
            login::Action::Close => self.panel = None,
            login::Action::Copy(url) => match copy_to_clipboard(&url) {
                Ok(()) => self.hint = Some(("Copied login link".into(), Instant::now())),
                Err(error) => self.push(Item::Error(error)),
            },
            login::Action::Exchange { provider, cancel, exchange } => {
                let events = self.events.clone();
                std::thread::spawn(move || {
                    let result = exchange(&cancel);
                    let _ = events.send(Msg::LoginFinished { provider, cancel, result });
                });
            }
            login::Action::Save(account) => {
                self.panel = None;
                let name = account.name.clone();
                let previous_id = self.client.accounts().into_iter().find(|saved| saved.provider == account.provider && saved.name == name).and_then(|saved| saved.account_id);
                let account_id = account.account_id().map(String::from);
                match self.client.add(account) {
                    Ok(()) => self.system("login", format!("Saved {name}.")),
                    Err(error) => return self.push(Item::Error(error)),
                }
                if let (Some(previous_id), Some(account_id)) = (previous_id, account_id)
                    && previous_id != account_id
                {
                    let _ = self.commands.send(Command::AccountReplaced { previous_id, account_id });
                }
            }
        }
    }

    /// /goal: no argument shows the goal, a keyword manages it, anything else sets a new goal and starts on it.
    fn goal_command(&mut self, args: &str) {
        let mut slot = self.goal.lock().unwrap();
        if args.is_empty() {
            let text = slot.as_ref().map_or("No goal. Set one with /goal <objective>.".into(), Goal::summary);
            drop(slot);
            self.system("goal", text);
            return;
        }
        let change = match (args, slot.as_mut()) {
            ("pause" | "resume" | "stop" | "done", None) => None,
            ("pause", Some(goal)) => {
                goal.status = GoalStatus::Paused;
                Some(("Goal paused.", false))
            }
            ("resume", Some(goal)) => {
                goal.status = GoalStatus::Active;
                Some(("Goal resumed.", true))
            }
            ("done", Some(goal)) => {
                goal.status = GoalStatus::Complete;
                Some(("Goal marked done.", false))
            }
            ("stop", Some(_)) => {
                *slot = None;
                Some(("Goal stopped.", false))
            }
            (objective, _) => {
                *slot = Some(Goal::new(objective, None));
                Some(("Goal set.", true))
            }
        };
        let prompt = slot.as_ref().map(Goal::prompt);
        drop(slot);
        let Some((note, run)) = change else {
            self.push(Item::Error("No goal. Set one with /goal <objective>.".into()));
            return;
        };
        self.system("goal", note);
        let _ = self.commands.send(Command::SaveGoal);
        // A running turn picks the active goal up when it ends.
        if let Some(prompt) = prompt.filter(|_| run && self.working.is_none()) {
            self.push(Item::User(prompt.clone()));
            self.start(Command::Prompt(prompt, Vec::new()));
        }
    }

    /// pi's worktree extension: no name picks from the list, a name opens or creates that worktree.
    fn worktree(&mut self, name: &str) -> Result<(), String> {
        let (worktrees, current) = worktree::list(&self.cwd)?;
        if name.is_empty() {
            self.panel = Some(Panel::Worktrees { confirm: false, index: 0, worktrees, current });
            return Ok(());
        }
        let path = worktree::open(&self.cwd, &worktrees, name)?;
        self.start(Command::Worktree(path));
        Ok(())
    }

    /// `/btw question` starts a new side thread on the current transcript; a bare `/btw` reopens the last one.
    fn open_btw(&mut self, question: &str) {
        if self.btw.as_ref().is_some_and(|btw| btw.streaming) {
            self.panel = Some(Panel::Btw);
            self.hint = Some(("btw is still answering".into(), Instant::now()));
            return;
        }
        if question.is_empty() && self.btw.is_some() {
            self.panel = Some(Panel::Btw);
            return;
        }
        self.btw = Some(btw::Btw::new(self.btw_context()));
        self.panel = Some(Panel::Btw);
        if !question.is_empty() {
            self.ask_btw(question.to_string());
        }
    }

    fn models_key(&mut self, key: KeyEvent) {
        let Some(Panel::Models { row, query, index, back }) = self.panel.as_mut() else { return };
        let row = *row;
        let back = match back {
            ModelsBack::Close => None,
            ModelsBack::Settings => Some(Panel::Settings { row, query: Editor::new() }),
        };
        let matches = matching_models(self.client.catalog(), &self.settings, row, &query.text());
        match key.code {
            KeyCode::Esc => self.panel = back,
            KeyCode::Up => *index = index.saturating_sub(1),
            KeyCode::Down => *index = (*index + 1).min(matches.len().saturating_sub(1)),
            KeyCode::Enter => {
                let Some(model) = matches.get(*index) else { return };
                let id = model.map_or(String::new(), |m| m.id.clone());
                self.update_settings(|s| *model_setting(s, row) = id);
                self.panel = back;
            }
            KeyCode::Tab => {
                let Some(Some(model)) = matches.get(*index) else { return };
                let key = model_key(model);
                let query = query.text();
                self.update_settings(|s| match s.pinned_models.iter().position(|pinned| *pinned == key) {
                    Some(at) => _ = s.pinned_models.remove(at),
                    None => s.pinned_models.push(key.clone()),
                });
                let matches = matching_models(self.client.catalog(), &self.settings, row, &query);
                let Some(Panel::Models { index, .. }) = self.panel.as_mut() else { return };
                *index = matches.iter().position(|m| m.is_some_and(|m| model_key(m) == key)).unwrap_or(0);
            }
            _ => {
                if query.edit_key(key) {
                    *index = 0;
                }
            }
        }
    }

    fn open_context_windows(&mut self) {
        let Some(current) = self.loaded_model_key() else { return };
        let index = context_window_targets(self.client.catalog(), &self.settings, 1, "").iter().position(|(key, _)| *key == current).unwrap_or(0);
        self.panel = Some(Panel::ContextWindows { tab: 1, query: Editor::new(), index });
    }

    fn context_windows_key(&mut self, key: KeyEvent) {
        let Some(Panel::ContextWindows { tab, query, index }) = self.panel.as_mut() else { return };
        let targets = context_window_targets(self.client.catalog(), &self.settings, *tab, &query.text());
        match key.code {
            KeyCode::Esc => self.panel = Some(Panel::Settings { row: CONTEXT_WINDOW_ROW, query: Editor::new() }),
            KeyCode::Tab | KeyCode::BackTab => {
                *tab = 1 - *tab;
                *index = 0;
            }
            KeyCode::Up => *index = index.saturating_sub(1),
            KeyCode::Down => *index = (*index + 1).min(targets.len().saturating_sub(1)),
            KeyCode::Left | KeyCode::Right => {
                let Some((target, _)) = targets.into_iter().nth(*index) else { return };
                let forward = key.code == KeyCode::Right;
                self.update_settings(|s| {
                    match step(&CONTEXT_WINDOWS, &s.context_windows.get(&target).copied(), forward) {
                        Some(window) => s.context_windows.insert(target, window),
                        None => s.context_windows.remove(&target),
                    };
                });
            }
            _ => {
                if *tab == 1 && query.edit_key(key) {
                    *index = 0;
                }
            }
        }
    }

    fn provider_order_key(&mut self, key: KeyEvent) {
        let Some(Panel::ProviderOrder { tab, index }) = self.panel.as_mut() else { return };
        let providers = PROVIDER_LISTS[*tab];
        let last = providers.len() - 1;
        match key.code {
            KeyCode::Esc => self.panel = Some(Panel::Settings { row: PROVIDER_ORDER_ROW, query: Editor::new() }),
            KeyCode::Tab | KeyCode::BackTab => {
                *tab = 1 - *tab;
                *index = 0;
            }
            KeyCode::Up | KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) => {
                let (moved, up) = (*index, key.code == KeyCode::Up);
                *index = if up { moved.saturating_sub(1) } else { (moved + 1).min(last) };
                self.update_settings(|s| s.move_provider(providers, moved, up));
            }
            KeyCode::Up => *index = index.saturating_sub(1),
            KeyCode::Down => *index = (*index + 1).min(last),
            _ => {}
        }
    }

    fn btw_key(&mut self, key: KeyEvent) {
        let btw = self.btw.as_mut().expect("the btw panel opens with a thread");
        match key.code {
            KeyCode::Esc => {
                btw.abort.store(true, Ordering::Relaxed);
                self.panel = None;
            }
            KeyCode::Tab => {
                let Some((_, answer)) = btw.turns.last().filter(|(_, answer)| !answer.is_empty()) else { return };
                let answer = answer.clone();
                self.editor.insert(&answer);
                self.panel = None;
            }
            KeyCode::Enter if !btw.streaming && !btw.input.text().trim().is_empty() => {
                let question = btw.input.take();
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
        btw::context(&sections)
    }

    fn open_history(&mut self) {
        let mut entries = history::load(&self.home.join(".agents"));
        entries.reverse();
        self.panel = Some(Panel::History { entries, tab: 0, query: Editor::new(), index: 0 });
    }

    /// The model picker for settings `row`, on the current model when it matches `query`.
    fn open_models(&mut self, row: usize, query: &str, back: ModelsBack) {
        let current = model_setting(&mut self.settings, row).clone();
        let index = matching_models(self.client.catalog(), &self.settings, row, query).iter().position(|m| m.map_or("", |m| &m.id) == current).unwrap_or(0);
        self.panel = Some(Panel::Models { row, query: Editor::from_text(query), index, back });
    }

    /// Moves setting `row` to its next (or previous) value, then saves and applies it.
    fn change_setting(&mut self, row: usize, forward: bool) {
        if MODEL_ROWS.contains(&row) {
            self.open_models(row, "", ModelsBack::Settings);
            return;
        }
        if row == CONTEXT_WINDOW_ROW {
            self.open_context_windows();
            return;
        }
        self.update_settings(|s| match row {
            1 => s.thinking = step(&THINKING_LEVELS, &s.thinking.as_str(), forward).to_string(),
            2 => s.permission_mode = step(&MODES, &s.permission_mode, forward),
            3 => s.auto_compact = !s.auto_compact,
            4 => s.reserve_tokens = step(&RESERVE_TOKENS, &s.reserve_tokens, forward),
            5 => s.keep_recent_tokens = step(&KEEP_RECENT_TOKENS, &s.keep_recent_tokens, forward),
            6 => s.expand_tools = !s.expand_tools,
            7 => s.condense = !s.condense,
            9 => s.condense_min_batch_chars = step(&CONDENSE_MIN_CHARS, &s.condense_min_batch_chars, forward),
            10 => s.switch_at_percent = step(&SWITCH_AT_PERCENT, &s.switch_at_percent, forward),
            11 => s.when_all_over_limit = step(&Policy::ALL, &s.when_all_over_limit, forward),
            14 => s.ponytail = step(&PONYTAIL_LEVELS, &s.ponytail.as_str(), forward).to_string(),
            15 => s.caveman = step(&CAVEMAN_LEVELS, &s.caveman.as_str(), forward).to_string(),
            16 => s.rtk = !s.rtk,
            17 => s.headroom = !s.headroom,
            18 => s.subagent_effort = step(&THINKING_LEVELS, &s.subagent_effort.as_str(), forward).to_string(),
            19 => s.notifications = !s.notifications,
            20 => s.scroll_lines = step(&SCROLL_LINES, &s.scroll_lines, forward),
            21 => s.copy_on_select = !s.copy_on_select,
            22 => s.update_channel = step(&crate::update::CHANNELS, &s.update_channel.as_str(), forward).to_string(),
            23 => s.menu_rows = step(&MENU_ROWS, &s.menu_rows, forward),
            24 => s.prune_sessions = step(&PRUNE_SESSIONS, &s.prune_sessions.as_str(), forward).to_string(),
            25 => s.provider_sort = step(&PROVIDER_SORTS, &s.provider_sort.as_str(), forward).to_string(),
            26 => s.conflict_priority = step(&CONFLICT_PRIORITIES, &s.conflict_priority.as_str(), forward).to_string(),
            27 => s.fast = !s.fast,
            28 => s.cache_ttl = step(&CACHE_TTLS, &s.cache_ttl.as_str(), forward).to_string(),
            31 => s.footer_cost = !s.footer_cost,
            _ => unreachable!("rows 0, 8, 12, 13, 29 and 30 open their own panels"),
        });
    }

    fn setting_values(&self) -> [Line; 32] {
        let s = &self.settings;
        let condense_model = if s.condense_model.is_empty() { "auto" } else { &s.condense_model };
        let fallback_model = if s.fallback_model.is_empty() { "auto" } else { &s.fallback_model };
        let reviewer_model = if s.reviewer_model.is_empty() { "auto" } else { &s.reviewer_model };
        let subagent_model = if s.subagent_model.is_empty() { "auto" } else { &s.subagent_model };
        let model = self.model();
        [
            vec![span(model.map_or(LOADING_MODELS.into(), |m| format!("{} · {}", m.provider, m.id)), BRIGHT.bold())],
            choices(THINKING_LEVELS.map(String::from), &s.thinking),
            choices(MODES.map(|m| m.label().to_string()), s.permission_mode.label()),
            on_off_choices(s.auto_compact),
            choices(RESERVE_TOKENS.map(tokens), &tokens(s.reserve_tokens)),
            choices(KEEP_RECENT_TOKENS.map(tokens), &tokens(s.keep_recent_tokens)),
            on_off_choices(s.expand_tools),
            on_off_choices(s.condense),
            vec![span(condense_model, BRIGHT.bold())],
            choices(CONDENSE_MIN_CHARS.map(|n| n.to_string()), &s.condense_min_batch_chars.to_string()),
            choices(SWITCH_AT_PERCENT.map(switch_label), &switch_label(s.switch_at_percent)),
            choices(Policy::ALL.map(|p| p.label().to_string()), s.when_all_over_limit.label()),
            vec![span(fallback_model, BRIGHT.bold())],
            match model {
                Some(model) => vec![span(tokens(s.context_window(model)), BRIGHT.bold()), span(format!("  {}", window_source(s, model)), MUTED)],
                None => vec![span(LOADING_MODELS, MUTED)],
            },
            choices(PONYTAIL_LEVELS.map(String::from), &s.ponytail),
            choices(CAVEMAN_LEVELS.map(String::from), &s.caveman),
            optional_tool(s.rtk, "rtk"),
            optional_tool(s.headroom, "headroom"),
            choices(THINKING_LEVELS.map(String::from), &s.subagent_effort),
            on_off_choices(s.notifications),
            choices(SCROLL_LINES.map(|n| n.to_string()), &s.scroll_lines.to_string()),
            on_off_choices(s.copy_on_select),
            choices(crate::update::CHANNELS.map(String::from), &s.update_channel),
            choices(MENU_ROWS.map(|n| n.to_string()), &s.menu_rows.to_string()),
            choices(PRUNE_SESSIONS.map(String::from), &s.prune_sessions),
            [choices(PROVIDER_SORTS.map(String::from), &s.provider_sort), vec![span("   enter reorder", MUTED)]].concat(),
            choices(CONFLICT_PRIORITIES.map(String::from), &s.conflict_priority),
            on_off_choices(s.fast),
            choices(CACHE_TTLS.map(String::from), &s.cache_ttl),
            vec![span(reviewer_model, BRIGHT.bold())],
            vec![span(subagent_model, BRIGHT.bold())],
            on_off_choices(s.footer_cost),
        ]
    }

    fn render_panel(&self, panel: &Panel, width: usize) -> Vec<Line> {
        match panel {
            Panel::Resume { all, index, sessions } => {
                let cwd = self.cwd.display().to_string();
                let rows: Vec<panel::Row> = sessions
                    .iter()
                    .filter(|s| *all || s.cwd == cwd)
                    .enumerate()
                    .map(|(row, s)| {
                        let workspace = Path::new(&s.cwd).file_name().map_or(s.cwd.clone(), |n| n.to_string_lossy().into_owned());
                        let turns = if s.turns == 1 { "1 turn".to_string() } else { format!("{} turns", s.turns) };
                        let title: String = s.name.clone().unwrap_or_else(|| s.title.clone()).chars().take(40).collect();
                        let style = if row == *index { BRIGHT.bold() } else { MUTED };
                        panel::Row { left: title, right: vec![span(format!("{workspace} · {} · {turns}", age(s.modified)), style)] }
                    })
                    .collect();
                let view = panel::Panel {
                    title: format!("Sessions {}", rows.len()),
                    tabs: &["Current workspace", "All workspaces"],
                    tab: usize::from(*all),
                    search: None,
                    rows: &rows,
                    selected: *index,
                    empty: "No saved sessions.",
                    hint: "↑↓ navigate     tab scope     enter resume     esc close",
                };
                view.render(width, PANEL_ROWS)
            }
            Panel::Settings { row, query } => {
                let matches = matching_settings(&query.text());
                let values = self.setting_values();
                let rows: Vec<panel::Row> = matches.iter().map(|&r| panel::Row { left: SETTING_NAMES[r].to_string(), right: values[r].clone() }).collect();
                let view = panel::Panel {
                    title: "Settings".into(),
                    tabs: &[],
                    tab: 0,
                    search: Some(query.field(None)),
                    rows: &rows,
                    selected: matches.iter().position(|r| r == row).unwrap_or(0),
                    empty: "No matching settings.",
                    hint: "↑↓ navigate     ←→ change     type to search     esc close",
                };
                view.render(width, PANEL_ROWS)
            }
            Panel::ContextWindows { tab, query, index } => {
                let rows: Vec<panel::Row> = context_window_targets(self.client.catalog(), &self.settings, *tab, &query.text())
                    .into_iter()
                    .map(|(target, model)| {
                        let mut right = choices(CONTEXT_WINDOWS.map(context_window_label), &context_window_label(self.settings.context_windows.get(&target).copied()));
                        let Some(model) = model else { return panel::Row { left: target, right } };
                        right.push(span(format!("   {} catalog {}", model.provider, tokens(model.context_window)), MUTED));
                        panel::Row { left: model.id.clone(), right }
                    })
                    .collect();
                let view = panel::Panel {
                    title: "Context window".into(),
                    tabs: &["Providers", "Models"],
                    tab: *tab,
                    search: (*tab == 1).then(|| query.field(None)),
                    rows: &rows,
                    selected: *index,
                    empty: "No matching models.",
                    hint: "↑↓ navigate     ←→ change     tab providers/models     type to search     esc back",
                };
                view.render(width, PANEL_ROWS)
            }
            Panel::ProviderOrder { tab, index } => {
                let rows: Vec<panel::Row> = self.settings.sort_providers(PROVIDER_LISTS[*tab]).into_iter().map(|provider| panel::Row { left: provider.into(), right: vec![] }).collect();
                let view = panel::Panel {
                    title: "Provider order".into(),
                    tabs: &["Models", "Web search"],
                    tab: *tab,
                    search: None,
                    rows: &rows,
                    selected: *index,
                    empty: "No providers.",
                    hint: "↑↓ navigate     shift+↑↓ move     tab models/web search     esc back",
                };
                view.render(width, PANEL_ROWS)
            }
            Panel::Models { row, query, index, .. } => {
                let rows: Vec<panel::Row> = matching_models(self.client.catalog(), &self.settings, *row, &query.text())
                    .into_iter()
                    .map(|m| match m {
                        Some(m) => {
                            let pin = if self.settings.pinned_models.contains(&model_key(m)) { "★ " } else { "" };
                            panel::Row { left: format!("{pin}{}", m.id), right: vec![span(model_owners(m), MUTED)] }
                        }
                        None => panel::Row { left: "auto".into(), right: vec![] },
                    })
                    .collect();
                let view = panel::Panel {
                    title: SETTING_NAMES[*row].into(),
                    tabs: &[],
                    tab: 0,
                    search: Some(query.field(None)),
                    rows: &rows,
                    selected: *index,
                    empty: "No matching models.",
                    hint: "type to search     ↑↓ navigate     tab pin     enter select     esc back",
                };
                view.render(width, PANEL_ROWS)
            }
            Panel::Permission { request, index, .. } => permission_lines(request, *index, width),
            Panel::Ask(ask) => ask.render(width),
            Panel::Subagents { index } => subagents_panel(&self.subagents.lock().unwrap().list, *index, width),
            Panel::Terminals { index } => terminals_panel(&self.terminals.lock().unwrap().list, *index, width),
            Panel::Todos { index } => {
                let todos = self.todos.lock().unwrap();
                let rows: Vec<panel::Row> = todos.visible().map(|t| panel::Row { left: format!("#{} {}", t.id, t.subject), right: vec![span(t.status.as_str(), todo_style(t.status))] }).collect();
                let view = panel::Panel {
                    title: format!("Todos {}", rows.len()),
                    tabs: &[],
                    tab: 0,
                    search: None,
                    rows: &rows,
                    selected: *index,
                    empty: "No todos.",
                    hint: "↑↓ navigate     space status     d delete     esc close",
                };
                view.render(width, PANEL_ROWS)
            }
            Panel::Usage { tab, totals } => {
                let (rows, selected) = match totals.get(*tab) {
                    Some(totals) => {
                        let rows = usage_rows(totals, self.client.catalog());
                        let total_row = rows.len().saturating_sub(1);
                        (rows, total_row)
                    }
                    None => {
                        let subscriptions: Vec<AccountView> = self.client.accounts().into_iter().filter(|a| a.oauth).collect();
                        let (rows, account_rows) = account_rows(&subscriptions, &[]);
                        (rows, account_rows.first().copied().unwrap_or(0))
                    }
                };
                let view = panel::Panel {
                    title: "Usage".to_string(),
                    tabs: &USAGE_TABS,
                    tab: *tab,
                    search: None,
                    rows: &rows,
                    selected,
                    empty: if *tab < usage::SCOPES.len() { "No usage recorded yet." } else { "No subscription accounts." },
                    hint: "tab scope     ~$ at OpenRouter list prices, not billed on subscriptions     esc close",
                };
                view.render(width, PANEL_ROWS)
            }
            Panel::Skills { index } => {
                let name_width = self.skills.iter().map(|skill| skill.name.chars().count()).max().unwrap_or(0);
                let rows: Vec<panel::Row> = self
                    .skills
                    .iter()
                    .map(|skill| {
                        let disabled = self.settings.disabled_skills.contains(&skill.name);
                        let (mark, right) = if disabled { ("[ ]", span("disabled", MUTED)) } else { ("[x]", span("enabled", PLAIN)) };
                        let description = skill.description.split_whitespace().collect::<Vec<_>>().join(" ");
                        panel::Row { left: format!("{mark} {:<name_width$}  {description}", skill.name), right: vec![right] }
                    })
                    .collect();
                let view = panel::Panel {
                    title: "Skills".into(),
                    tabs: &[],
                    tab: 0,
                    search: None,
                    rows: &rows,
                    selected: *index,
                    empty: "No skills. Add one under ~/.agents/skills/<name>/SKILL.md.",
                    hint: "↑↓ navigate     space enable/disable     esc close",
                };
                view.render(width, PANEL_ROWS)
            }
            Panel::Mcp { index, confirm } => {
                let (views, error) = self.mcp.list();
                let name_width = views.iter().map(|view| view.name.chars().count()).max().unwrap_or(0);
                let mut rows = Vec::new();
                for view in &views {
                    let on = view.enabled && view.approval.is_none() && !self.settings.disabled_mcp.contains(&view.name);
                    let mut right = vec![match &view.status {
                        crate::mcp::Status::Off if view.approval.is_some() => span("project server, space to approve", WARN),
                        crate::mcp::Status::Off if !view.enabled => span("off in mcp.json", MUTED),
                        crate::mcp::Status::Off => span("disabled", MUTED),
                        crate::mcp::Status::Connecting => span("connecting", MUTED),
                        crate::mcp::Status::LoggingIn => span("logging in (browser)", WARN),
                        crate::mcp::Status::Ready { tools } => span(format!("{tools} tools"), PLAIN),
                        crate::mcp::Status::Failed(error) => span(error, ERROR),
                    }];
                    if view.logged_in {
                        right.push(span("  logged in", MUTED));
                    }
                    let mark = if on { "[x]" } else { "[ ]" };
                    rows.push(panel::Row { left: format!("{mark} {:<name_width$}  {}", view.name, view.target), right });
                }
                let empty = match &error {
                    Some(error) => error.as_str(),
                    None => "No MCP servers. Add one to ~/.agents/mcp.json or .mcp.json under \"mcpServers\".",
                };
                let view = panel::Panel {
                    title: "MCP".into(),
                    tabs: &[],
                    tab: 0,
                    search: None,
                    rows: &rows,
                    selected: *index,
                    empty,
                    hint: "↑↓ navigate     space enable/disable     r reconnect     l log in     esc close",
                };
                let mut lines = view.render(width, PANEL_ROWS);
                // `env`, `cwd` and `headers` decide what a project server runs as much as its command does.
                if let Some(selected) = views.get(*index).filter(|view| view.approval.is_some()) {
                    let (mut entry, cut) = mcp_entry(&selected.entry, width);
                    if cut && *confirm {
                        entry.push(vec![span("  read the whole entry in the file, then press space again to approve it", WARN)]);
                    }
                    let above_hint = lines.len() - 1;
                    lines.splice(above_hint..above_hint, entry);
                }
                lines
            }
            Panel::Plugins { index, confirm } => {
                let rows: Vec<panel::Row> = self
                    .plugins
                    .list()
                    .into_iter()
                    .map(|view| {
                        let right = match (view.installed, &view.error, self.settings.disabled_plugins.contains(&view.name)) {
                            (false, _, _) => span("available", MUTED),
                            (true, _, true) => span("disabled", MUTED),
                            (true, Some(error), false) => span(error.clone(), WARN),
                            (true, None, false) => span(if view.running { "running" } else { "stopped" }, PLAIN),
                        };
                        let author = view.author.map(|author| format!("  @{author}")).unwrap_or_default();
                        panel::Row { left: format!("{} {}  {}{author}", view.name, view.version, view.description), right: vec![right] }
                    })
                    .collect();
                let view = panel::Panel {
                    title: "Plugins".into(),
                    tabs: &[],
                    tab: 0,
                    search: None,
                    rows: &rows,
                    selected: *index,
                    empty: "No plugins. Install one with `agent plugin install <path>`.",
                    hint: if *confirm {
                        "d again removes the plugin and the state it saved     any other key cancels"
                    } else {
                        "↑↓ navigate     space install/enable/disable     enter settings     d remove     esc close"
                    },
                };
                view.render(width, PANEL_ROWS)
            }
            Panel::PluginSettings { plugin, index, edit } => {
                let declared = self.plugins.list().into_iter().find(|view| &view.name == plugin).map(|view| view.settings).unwrap_or_default();
                let rows: Vec<panel::Row> = declared
                    .iter()
                    .enumerate()
                    .map(|(row, entry)| {
                        let key = entry["key"].as_str().unwrap_or_default();
                        let value = crate::plugins::setting(entry, &self.settings.plugin_settings[plugin.as_str()]);
                        let right = match (edit, value) {
                            (Some(editor), _) if row == *index => format!("{}▏", editor.text()),
                            (_, Value::Bool(on)) => (if on { "on" } else { "off" }).to_string(),
                            (_, value) => value.as_str().unwrap_or_default().to_string(),
                        };
                        panel::Row { left: format!("{key}  {}", entry["description"].as_str().unwrap_or_default()), right: vec![span(right, PLAIN)] }
                    })
                    .collect();
                let view = panel::Panel {
                    title: format!("{plugin} settings"),
                    tabs: &[],
                    tab: 0,
                    search: None,
                    rows: &rows,
                    selected: *index,
                    empty: "This plugin has no settings.",
                    hint: if edit.is_some() { "enter save     esc cancel" } else { "↑↓ navigate     space/enter change     esc back" },
                };
                view.render(width, PANEL_ROWS)
            }
            Panel::Plugin { plugin, data, index } => {
                let rows: Vec<panel::Row> = data["rows"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|row| panel::Row { left: row["left"].as_str().unwrap_or_default().to_string(), right: vec![span(row["right"].as_str().unwrap_or_default().to_string(), PLAIN)] })
                    .collect();
                let hint = format!("↑↓ navigate     {}     esc close", data["hint"].as_str().unwrap_or("enter select"));
                let view =
                    panel::Panel { title: data["title"].as_str().unwrap_or(plugin).to_string(), tabs: &[], tab: 0, search: None, rows: &rows, selected: *index, empty: "Nothing here.", hint: &hint };
                view.render(width, PANEL_ROWS)
            }
            Panel::Btw => self.btw.as_ref().expect("the btw panel opens with a thread").render(width),
            Panel::Subagent { .. } | Panel::Terminal { .. } | Panel::Diff { .. } => unreachable!("frame draws the full-screen view instead of a panel"),
            Panel::Trust { root, files } => {
                let rows: Vec<panel::Row> = files.iter().map(|file| panel::Row { left: file.strip_prefix(root).unwrap_or(file).display().to_string(), right: Vec::new() }).collect();
                let view = panel::Panel {
                    title: "Trust this project? Its files can instruct the model and run commands.".into(),
                    tabs: &[],
                    tab: 0,
                    search: None,
                    rows: &rows,
                    selected: 0,
                    empty: "",
                    hint: "y or enter trust     any other key not now",
                };
                view.render(width, PANEL_ROWS)
            }
            Panel::Worktrees { confirm, index, worktrees, current } => {
                let home = std::env::var("HOME").unwrap_or_default();
                let rows: Vec<panel::Row> = worktrees
                    .iter()
                    .map(|w| {
                        let mark = if w.path == *current { "✓ " } else { "  " };
                        let path = w.path.display().to_string().replacen(&home, "~", 1);
                        panel::Row { left: format!("{mark}{}", w.branch), right: vec![span(path, MUTED)] }
                    })
                    .collect();
                let view = panel::Panel {
                    title: if *confirm { format!("Remove {}? Its ignored files go with it.", worktrees[*index].branch) } else { "Worktrees".into() },
                    tabs: &[],
                    tab: 0,
                    search: None,
                    rows: &rows,
                    selected: *index,
                    empty: "",
                    hint: if *confirm { "d again removes it, the branch stays     any other key cancels" } else { "↑↓ navigate     enter move session     d remove     esc close" },
                };
                view.render(width, PANEL_ROWS)
            }
            Panel::Login(login) => login.render(width, PANEL_ROWS),
            Panel::Rewind { index, points } => {
                let rows: Vec<panel::Row> = points.iter().map(|point| panel::Row { left: point.text.lines().next().unwrap_or_default().chars().take(60).collect(), right: Vec::new() }).collect();
                let view = panel::Panel {
                    title: "Rewind".into(),
                    tabs: &[],
                    tab: 0,
                    search: None,
                    rows: &rows,
                    selected: *index,
                    empty: "",
                    hint: "↑↓ navigate     enter rewind here, restoring the files it changed     esc close",
                };
                view.render(width, PANEL_ROWS)
            }
            Panel::Changes { files, index } => {
                let rows: Vec<panel::Row> = files.iter().map(|file| panel::Row { left: file.path.clone(), right: diff_stat(&file.diff) }).collect();
                let view =
                    panel::Panel { title: "Changes".into(), tabs: &[], tab: 0, search: None, rows: &rows, selected: *index, empty: "", hint: "↑↓ navigate     enter show diff     esc close" };
                view.render(width, PANEL_ROWS)
            }
            Panel::History { entries, tab, query, index } => {
                let rows: Vec<panel::Row> = matching_prompts(entries, &query.text(), |entry| entry.in_scope(*tab, &self.cwd, &self.session_id))
                    .iter()
                    .map(|prompt| panel::Row { left: prompt.split_whitespace().collect::<Vec<_>>().join(" "), right: Vec::new() })
                    .collect();
                let view = panel::Panel {
                    title: "History".into(),
                    tabs: &history::SCOPES,
                    tab: *tab,
                    search: Some(query.field(None)),
                    rows: &rows,
                    selected: *index,
                    empty: "No matching prompts.",
                    hint: "↑↓ navigate     enter insert     tab scope     esc close",
                };
                view.render(width, PANEL_ROWS)
            }
            Panel::Accounts { logout, query, index } => {
                let accounts = matching_accounts(self.client.accounts(), &query.text());
                let (rows, account_rows) = account_rows(&accounts, &self.plugins.account_labels());
                let view = panel::Panel {
                    title: if *logout { "Remove account" } else { "Accounts" }.into(),
                    tabs: &[],
                    tab: 0,
                    search: Some(query.field(None)),
                    rows: &rows,
                    selected: account_rows.get(*index).copied().unwrap_or(0),
                    empty: "No matching accounts.",
                    hint: if *logout { "↑↓ navigate     enter remove     esc close" } else { "↑↓ navigate     enter use     esc close" },
                };
                view.render(width, PANEL_ROWS)
            }
        }
    }

    /// Sorts only on open, so a run that finishes does not move under the cursor while the panel is up.
    fn open_subagents(&mut self) {
        self.subagents.lock().unwrap().running_first();
        self.panel = Some(Panel::Subagents { index: 0 });
    }

    fn session_title(&self, id: &str) -> String {
        let session = Session::list(&self.sessions_dir).into_iter().find(|s| s.id == id);
        session.map_or(id.to_string(), |s| s.name.unwrap_or(s.title))
    }

    fn copy_last_answer(&self) -> Result<(), String> {
        let answer = self.entries.iter().rev().find_map(|entry| match &entry.item {
            Item::Assistant { text, .. } if !text.trim().is_empty() => Some(text.clone()),
            _ => None,
        });
        copy_to_clipboard(&answer.ok_or("No agent message to copy yet.")?)
    }

    fn menu_matches(&self) -> Vec<(String, String)> {
        match self.file_query() {
            Some(query) => {
                // Redraws repeat every 80 ms while the agent works, so the listing runs once per typed query.
                let mut cached = self.path_matches.borrow_mut();
                if cached.0 != self.cwd.join(query) {
                    *cached = (self.cwd.join(query), matching_paths(&self.cwd, query));
                }
                cached.1.clone()
            }
            None => {
                // A closed menu forgets its listing, so typing `@` again sees files created since.
                self.path_matches.take();
                let mut commands = self.slash_commands.clone();
                commands.extend(self.enabled_skills().map(|skill| (format!("skill:{}", skill.name), skill.description.split_whitespace().collect::<Vec<_>>().join(" "))));
                commands.extend(self.templates.iter().map(|template| (format!("prompt:{}", template.name), template.description.clone())));
                commands.extend(self.plugins.commands());
                matching_commands(&commands, slash_query(&self.editor.text(), self.editor.before_cursor()))
            }
        }
    }

    /// The `@` token right before the cursor, without the `@`.
    fn file_query(&self) -> Option<&str> {
        self.editor.before_cursor().rsplit(char::is_whitespace).next()?.strip_prefix('@')
    }

    /// Swaps the typed query for the path; a directory keeps the menu open for its entries.
    fn accept_path(&mut self, path: &str) {
        let typed = self.file_query().map_or(0, |query| query.chars().count());
        for _ in 0..typed {
            self.editor.backspace();
        }
        self.editor.insert(path);
        if !path.ends_with('/') {
            self.editor.insert(" ");
        }
        self.menu_index = 0;
    }

    /// Fills the editor with the selected command; commands without arguments run right away on Enter.
    /// A `/` typed in front of existing text keeps that text as the command's arguments, or in the editor for a command without them.
    fn accept_slash(&mut self, run: bool) {
        let matches = self.menu_matches();
        let name = matches[self.menu_index.min(matches.len() - 1)].0.clone();
        let takes_args = matches!(name.as_str(), "thinking" | "rename-title" | "btw" | "goal" | "import") || name.starts_with("skill:") || name.starts_with("prompt:");
        let text = self.editor.text();
        let rest = text[slash_query(&text, self.editor.before_cursor()).len()..].trim().to_string();
        if takes_args {
            self.editor.set(&format!("/{name} {rest}"));
        } else if run || !rest.is_empty() {
            self.editor.set(&rest);
            self.submit(format!("/{name}"));
        } else {
            self.editor.set(&format!("/{name}"));
        }
    }

    fn menu(&self) -> Vec<Line> {
        let matches = self.menu_matches();
        if matches.is_empty() {
            return Vec::new();
        }
        let selected = self.menu_index.min(matches.len() - 1);
        let menu_rows = self.settings.menu_rows;
        let first = (selected + 1).saturating_sub(menu_rows);
        let mut rows = vec![Vec::new()];
        for (index, (name, description)) in matches.iter().enumerate().skip(first).take(menu_rows) {
            let label = format!("{name:<31} {description}");
            if index == selected {
                rows.push(vec![span("→", OK), span(format!(" {label}"), TOOL_NAME)]);
            } else {
                rows.push(vec![span(format!("  {label}"), TOOL_ARGS)]);
            }
        }
        if matches.len() > menu_rows {
            rows.extend([Vec::new(), vec![span(format!("  {}/{}", selected + 1, matches.len()), DIM)]]);
        }
        rows
    }

    fn start(&mut self, command: Command) {
        self.scroll = 0;
        if self.working.is_some() {
            self.deferred_commands.push_back(command);
            self.hint = Some(("Runs when the turn ends".into(), Instant::now()));
            return;
        }
        if self.commands.send(command).is_err() {
            self.push(Item::Error("The agent thread stopped.".into()));
            // Otherwise the event loop would retry FollowUps on every tick.
            self.steering.lock().unwrap().clear();
            self.follow_ups.lock().unwrap().clear();
            return;
        }
        self.working = Some(Instant::now());
    }

    fn agent_event(&mut self, event: Event) {
        match event {
            Event::Text(delta) => self.append_assistant(&delta, false),
            Event::Thinking(delta) => self.append_assistant(&delta, true),
            Event::Notice(text) => {
                let (source, text) = notice_source(&text);
                self.system(source, text);
            }
            Event::SubagentDone(run) => self.push(Item::Subagent(run)),
            Event::ToolStart { id, name, input } => self.push(Item::Tool { id, name, input, result: None }),
            Event::ToolEnd { id, output, is_error } => {
                let entry = self.entries.iter_mut().rev().find(|e| matches!(&e.item, Item::Tool { id: tool_id, .. } if *tool_id == id));
                if let Some(entry) = entry {
                    if let Item::Tool { result, .. } = &mut entry.item {
                        *result = Some((output, is_error));
                    }
                    entry.cache = None;
                }
            }
            Event::Steered(text) => self.push(Item::User(text)),
            // The output tail went to the model; /ps shows it in full.
            Event::FollowUp { source, text } => self.system(&source, text.split_once('\n').map_or(text.as_str(), |(head, _)| head)),
            Event::TurnEnd { usage, context_tokens } => {
                self.context_tokens = context_tokens;
                self.last_usage = usage;
                self.prompt_usage.add(usage);
                self.read_session_usage();
            }
            Event::Titled(title) => set_terminal_title(&title),
            Event::SessionStarted { id, path } => {
                self.session_id = id;
                self.session_path = path;
                self.spent = Spent::default();
                self.session_usage = Vec::new();
                self.system("session", "Started a fresh conversation");
            }
            Event::Permission { request, reply } => {
                self.notify(&format!("{} {}", request.question, request.detail));
                self.open_agent_panel(Panel::Permission { request, reply, index: 0, opened: Instant::now() });
            }
            Event::Ask { questions, reply } => {
                self.notify(&questions.first().map_or(String::new(), |question| question.question.clone()));
                self.open_agent_panel(Panel::Ask(ask::AskPanel::new(questions, reply)));
            }
            Event::Reviewed { allowed, detail, reason } => {
                let verdict = if allowed { "allowed" } else { "denied" };
                self.system("auto", format!("{verdict} {detail}: {reason}"));
            }
            Event::Compacted { summary } => {
                self.context_tokens = 0;
                let lines = summary.lines().count();
                self.system("compact", format!("Compacted context into a {lines}-line summary"));
            }
        }
    }

    /// The agent thread blocks on this panel's reply, so it replaces any open one; a waiting /login loses its tokens.
    fn open_agent_panel(&mut self, panel: Panel) {
        if matches!(self.panel, Some(Panel::Login(_))) {
            self.push(Item::Error("Login cancelled: the agent needs an answer first. Run /login again.".into()));
        }
        self.replace_panel(panel);
    }

    /// A reply opens its panel over any open one; a waiting /login it replaces frees its callback port.
    fn replace_panel(&mut self, panel: Panel) {
        if let Some(Panel::Login(login)) = &self.panel {
            login.cancel();
        }
        self.panel = Some(panel);
    }

    fn append_assistant(&mut self, delta: &str, is_thinking: bool) {
        if !matches!(self.entries.last().map(|e| &e.item), Some(Item::Assistant { .. })) {
            self.push(Item::Assistant { text: String::new(), thinking: String::new() });
        }
        let entry = self.entries.last_mut().unwrap();
        if let Item::Assistant { text, thinking } = &mut entry.item {
            if is_thinking { thinking } else { text }.push_str(delta);
        }
        entry.cache = None;
    }

    /// Desktop notification, sent only while the terminal is in the background.
    fn notify(&self, body: &str) {
        if !self.settings.notifications || self.focused == Some(true) {
            return;
        }
        print!("{}", notify::sequence("agent", body, |name| std::env::var(name).ok()));
        let _ = std::io::stdout().flush();
    }

    fn done(&mut self, result: Result<String, String>) {
        match &result {
            Ok(answer) if answer.trim().is_empty() => self.notify("Done"),
            Ok(answer) => self.notify(answer),
            Err(error) => self.notify(&format!("Failed: {error}")),
        }
        if let Err(error) = result {
            self.push(Item::Error(error));
        }
        if let Some(started) = self.working.take() {
            let usage = std::mem::take(&mut self.prompt_usage);
            // /clear, /resume and !bash finish without a model call, so there is nothing to report.
            if usage.input + usage.output > 0 {
                let took = started.elapsed();
                let spent = Spent { input: usage.input, output: usage.output, time: took };
                self.spent.add(spent);
                let _ = self.commands.send(Command::SaveSpent(spent));
                self.push(Item::Done { took, usage, total: self.spent.time });
            }
        }
        self.git = git_info(&self.cwd);
    }

    /// The whole screen: transcript, then status, open panel or slash menu, editor and footer pinned to the last rows.
    fn frame(&mut self, width: usize, height: usize) -> (Vec<Line>, (u16, u16)) {
        if matches!(self.panel, Some(Panel::Diff { .. })) {
            return (self.diff_view(width, height), (0, height.saturating_sub(1) as u16));
        }
        if matches!(self.panel, Some(Panel::Subagent { .. })) {
            return (self.subagent_view(width, height), (0, height.saturating_sub(1) as u16));
        }
        if matches!(self.panel, Some(Panel::Terminal { .. })) {
            return (self.terminal_view(width, height), (0, height.saturating_sub(1) as u16));
        }
        let mut bottom: Vec<Line> = Vec::new();
        if let Some(started) = self.working {
            let tick = (started.elapsed().as_millis() / 80) as usize;
            bottom.push(Vec::new());
            bottom.push(shimmer_line(WORKING, tick));
        }
        let queued = self.steering.lock().unwrap().clone();
        for text in &queued {
            bottom.push(vec![span(format!(" Steering: {}", text.replace('\n', " ")), DIM)]);
        }
        for (text, _) in &self.queued_commands {
            bottom.push(vec![span(format!(" Queued: {}", text.replace('\n', " ")), DIM)]);
        }
        if !queued.is_empty() || !self.queued_commands.is_empty() {
            bottom.push(vec![span(format!(" ↳ {ALT}+Up to edit all queued messages"), DIM)]);
        }
        if let Some(recording) = &self.recording {
            bottom.push(Vec::new());
            bottom.push(recording_line(recording.started.elapsed()));
        } else if let Some(started) = self.transcribing {
            bottom.push(Vec::new());
            bottom.push(shimmer_line(TRANSCRIBING, (started.elapsed().as_millis() / 80) as usize));
        }
        bottom.extend(todo_lines(&self.todos.lock().unwrap()));
        let running = self.terminals.lock().unwrap().running();
        if running > 0 {
            let noun = if running == 1 { "terminal" } else { "terminals" };
            bottom.push(vec![span(format!(" {running} background {noun} running • /ps to view"), DIM)]);
        }
        let running = self.subagents.lock().unwrap().running();
        if running > 0 {
            let noun = if running == 1 { "subagent" } else { "subagents" };
            bottom.push(vec![span(format!(" {running} {noun} running • ↓ to view"), DIM)]);
        }
        let mut panel_cursor = None;
        match &self.panel {
            Some(panel) => {
                let mut lines = self.render_panel(panel, width);
                panel_cursor = take_cursor(&mut lines).map(|(x, y)| (x, bottom.len() + y));
                bottom.extend(lines);
            }
            None => bottom.extend(self.menu()),
        }
        bottom.push(Vec::new());
        if self.editor_width != width {
            // The selection counts wrapped rows, which a new width renumbers; input_line would index past them.
            self.input_selection = Selection::default();
            self.editor_width = width;
        }
        let (mut editor_rows, (cursor_x, cursor_row)) = self.editor.render(width);
        let editor_top = bottom.len();
        let shown = editor_rows.len().min(EDITOR_ROWS);
        self.input_top = self.input_top.clamp((cursor_row + 1).saturating_sub(shown), cursor_row).min(editor_rows.len() - shown);
        let below = editor_rows.len() - self.input_top - shown;
        editor_rows.truncate(self.input_top + shown);
        editor_rows.drain(..self.input_top);
        for (row, line) in editor_rows.iter_mut().enumerate() {
            if let Some((from, to)) = self.input_selection.columns(below + shown - 1 - row) {
                *line = select(line, from, to);
            }
        }
        bottom.extend(editor_rows);
        let cursor = panel_cursor.unwrap_or((cursor_x, editor_top + cursor_row - self.input_top));
        bottom.push(Vec::new());
        bottom.extend(self.footer(width));

        let room = height.saturating_sub(bottom.len());
        // The last column is kept for the scrollbar so text does not rewrap when it appears.
        let content_width = width.saturating_sub(1);
        let (mut transcript, mut total, mut prompts) = self.transcript_tail(content_width, room + self.scroll);
        // Scroll counts lines up from the bottom, so a scrolled-up view or a selection moves up by what streamed in below it.
        let (last_total, last_width) = self.transcript_size;
        // A new width rewraps every line, so the selected lines no longer hold the selected text.
        if content_width != last_width {
            self.selection = Selection::default();
            self.edge_drag = None;
        }
        if content_width == last_width && total > last_total {
            self.selection.shift(total - last_total);
            if self.scroll > 0 || self.selection.range().is_some() {
                self.scroll += total - last_total;
                (transcript, total, prompts) = self.transcript_tail(content_width, room + self.scroll);
            }
        }
        self.transcript_size = (total, content_width);
        self.scroll = self.scroll.min(transcript.len().saturating_sub(room));
        let end = transcript.len() - self.scroll;
        let start = end.saturating_sub(room);
        let mut rows: Vec<Line> = transcript[start..end].to_vec();
        let visible = rows.len();
        self.transcript_view = (visible, content_width);
        // Sticky header: the prompt of the turn at the top of the view, once its first line scrolls out above.
        if let Some(&(_, index)) = prompts.iter().find(|(top, _)| *top >= self.scroll + visible) {
            // shortcut: clicks and selection on the pinned rows hit the transcript lines under them.
            let prompt = self.entries[index].lines(content_width, self.expanded);
            pin_prompt(&mut rows, prompt, content_width);
        }
        for (row, line) in rows.iter_mut().enumerate() {
            if let Some((from, to)) = self.selection.columns(self.scroll + visible - 1 - row) {
                *line = select(line, from, to);
            }
        }
        // Floats over the transcript top so a hint appearing or expiring does not shift the layout.
        if let (Some((hint, _)), Some(first)) = (&self.hint, rows.first_mut()) {
            *first = overlay_right(first, vec![span(format!(" {hint} "), Style { selected: true, ..PLAIN })], content_width);
        }
        if let (true, Some(last)) = (self.scroll > 0, rows.last_mut()) {
            *last = overlay_center(last, vec![span(format!(" ↓ Jump to latest message · {ALT}+Down "), BRIGHT.on(240))], content_width);
        }
        for (row, glyph) in scrollbar(total, visible, self.scroll).into_iter().enumerate() {
            let pad = content_width.saturating_sub(line_width(&rows[row]));
            rows[row].push(span(format!("{}{glyph}", " ".repeat(pad)), MUTED));
        }
        rows.resize(room, Vec::new());
        self.input_view = (rows.len() + editor_top, shown, below);
        let cursor_y = rows.len() + cursor.1;
        rows.extend(bottom);
        rows.truncate(height);
        let rows = rows.iter().map(|row| truncate(row, width)).collect();
        (rows, (cursor.0 as u16, cursor_y.min(height.saturating_sub(1)) as u16))
    }

    /// The open run's session filling the screen, bottom-anchored like the transcript, reread when its file grows.
    fn subagent_view(&mut self, width: usize, height: usize) -> Vec<Line> {
        let Some(Panel::Subagent { index, scroll, loaded, lines }) = self.panel.as_mut() else { unreachable!("frame checks the panel") };
        let run = self.subagents.lock().unwrap().list[*index].clone();
        let session = run.state.lock().unwrap().session.clone();
        let size = session.as_ref().and_then(|path| std::fs::metadata(path).ok()).map_or(0, |meta| meta.len());
        if *loaded != Some((size, width)) {
            *loaded = Some((size, width));
            *lines = match session {
                None => system_lines("⚙", "task", &run.task, SYSTEM, width),
                Some(path) => subagent_lines(Path::new(&path), width).unwrap_or_else(|error| system_lines("✗", "error", &error, ERROR, width)),
            };
        }
        let header = vec![span(format!(" {}", run.summary()), SYSTEM), span("  read-only", DIM)];
        full_screen(header, lines, scroll, width, height, " ↑↓ scroll     ←→ previous/next run     esc back")
    }

    fn diff_view(&mut self, width: usize, height: usize) -> Vec<Line> {
        let Some(Panel::Diff { files, index, scroll }) = self.panel.as_mut() else { unreachable!("frame checks the panel") };
        let lines: Vec<Line> = diff_lines(&files[*index]).iter().map(|line| truncate(line, width)).collect();
        let header = vec![span(format!(" {}", files[*index].path), TOOL_NAME), span(format!("  {}/{}", *index + 1, files.len()), DIM)];
        full_screen(header, &lines, scroll, width, height, " ↑↓ scroll     ←→ previous/next file     esc back")
    }

    fn diff_key(&mut self, code: KeyCode) {
        let Some(Panel::Diff { files, index, scroll }) = self.panel.as_mut() else { unreachable!("panel_key checks the panel") };
        match code {
            KeyCode::Up => *scroll = scroll.saturating_add(1),
            KeyCode::Down => *scroll = scroll.saturating_sub(1),
            KeyCode::PageUp => *scroll = scroll.saturating_add(10),
            KeyCode::PageDown => *scroll = scroll.saturating_sub(10),
            KeyCode::Left => (*index, *scroll) = (index.saturating_sub(1), DIFF_TOP),
            KeyCode::Right => (*index, *scroll) = ((*index + 1).min(files.len() - 1), DIFF_TOP),
            KeyCode::Esc => {
                let (files, index) = (std::mem::take(files), *index);
                self.panel = Some(Panel::Changes { files, index });
            }
            _ => {}
        }
    }

    fn subagent_key(&mut self, code: KeyCode) {
        let last = self.subagents.lock().unwrap().list.len() - 1;
        let Some(Panel::Subagent { index, scroll, loaded, .. }) = self.panel.as_mut() else { unreachable!("panel_key checks the panel") };
        match code {
            KeyCode::Up => *scroll += 1,
            KeyCode::Down => *scroll = scroll.saturating_sub(1),
            KeyCode::PageUp => *scroll += 10,
            KeyCode::PageDown => *scroll = scroll.saturating_sub(10),
            KeyCode::Left | KeyCode::Right => {
                let next = if code == KeyCode::Left { index.saturating_sub(1) } else { (*index + 1).min(last) };
                if next != *index {
                    (*index, *scroll, *loaded) = (next, 0, None);
                }
            }
            KeyCode::Esc => {
                let index = *index;
                self.panel = Some(Panel::Subagents { index });
            }
            _ => {}
        }
    }

    /// The open terminal's output filling the screen, reread when its logs grow.
    fn terminal_view(&mut self, width: usize, height: usize) -> Vec<Line> {
        let Some(Panel::Terminal { index, scroll, loaded, lines }) = self.panel.as_mut() else { unreachable!("frame checks the panel") };
        let terminal = self.terminals.lock().unwrap().list[*index].clone();
        let size = terminal.log_size();
        if *loaded != Some((size, width)) {
            *loaded = Some((size, width));
            *lines = log_lines(&terminal.log(), width);
        }
        let header = vec![span(format!(" {} {}", terminal.id, terminal.title), SYSTEM), span(format!("  {} {}", terminal.status().as_str(), terminal.elapsed()), DIM)];
        full_screen(header, lines, scroll, width, height, " ↑↓ scroll     ←→ previous/next terminal     k kill     esc back")
    }

    fn terminal_key(&mut self, code: KeyCode) {
        let terminals = self.terminals.lock().unwrap();
        let last = terminals.list.len() - 1;
        let Some(Panel::Terminal { index, scroll, loaded, .. }) = self.panel.as_mut() else { unreachable!("panel_key checks the panel") };
        match code {
            KeyCode::Up => *scroll += 1,
            KeyCode::Down => *scroll = scroll.saturating_sub(1),
            KeyCode::PageUp => *scroll += 10,
            KeyCode::PageDown => *scroll = scroll.saturating_sub(10),
            KeyCode::Char('k') => _ = terminals.list[*index].stop(),
            KeyCode::Left | KeyCode::Right => {
                let next = if code == KeyCode::Left { index.saturating_sub(1) } else { (*index + 1).min(last) };
                if next != *index {
                    (*index, *scroll, *loaded) = (next, 0, None);
                }
            }
            KeyCode::Esc => {
                let index = *index;
                drop(terminals);
                self.panel = Some(Panel::Terminals { index });
            }
            _ => {}
        }
    }

    /// The last `needed` transcript lines (or all of them if there are fewer), the transcript's total line count,
    /// and every user prompt as (its top line counted up from the last line, its entry index), nearest the bottom first.
    fn transcript_tail(&mut self, width: usize, needed: usize) -> (Vec<Line>, usize, Vec<(usize, usize)>) {
        let expanded = self.expanded;
        let mut chunks: Vec<Vec<Line>> = Vec::new();
        let mut count = 0;
        let mut prompts = Vec::new();
        // Run of the nearest rendered entry below; empty ones (collapsed thinking) must not split tool or log runs.
        let mut below_run = None;
        for index in (0..self.entries.len()).rev() {
            let run = transcript_run(&self.entries[index].item);
            let is_prompt = matches!(self.entries[index].item, Item::User(_));
            let lines = self.entries[index].lines(width, expanded);
            if lines.is_empty() {
                continue;
            }
            if below_run.is_some_and(|below| run.is_none() || below != run) {
                if count < needed {
                    chunks.push(vec![Vec::new()]);
                }
                count += 1;
            }
            if count < needed {
                chunks.push(lines.clone());
            }
            count += lines.len();
            if is_prompt {
                prompts.push((count - 1, index));
            }
            below_run = Some(run);
        }
        (chunks.into_iter().rev().flatten().collect(), count, prompts)
    }

    fn footer(&self, width: usize) -> Vec<Line> {
        let home = std::env::var("HOME").unwrap_or_default();
        let tilde = |path: &Path| {
            let path = path.display().to_string();
            match path.strip_prefix(&home) {
                Some(rest) if !home.is_empty() => format!("~{rest}"),
                _ => path,
            }
        };
        let mut left = vec![span(tilde(&self.cwd), PLAIN.bold())];
        if let Some(branch) = &self.git.branch {
            left.push(span(" ", PLAIN));
            left.push(span(branch, MUTED));
        }
        if self.git.changed > 0 {
            left.push(span(" ", PLAIN));
            left.push(span(format!("{} changed", self.git.changed), WARN));
        }
        if let Some(main) = &self.git.main {
            left.push(span(format!(" → {} {}", main.branch, tilde(&main.path)), WARN));
        }
        if self.git.others > 0 {
            left.push(span(format!(" +{} wt", self.git.others), MUTED));
        }
        if let Some(goal) = self.goal.lock().unwrap().as_ref() {
            left.push(span(format!(" goal {}", goal.status.as_str()), MUTED));
        }
        let percent = (self.context_tokens * 100).checked_div(self.context_window()).unwrap_or(0);
        let mut right = Line::default();
        for status in self.plugins.statuses() {
            for (text, tone) in status {
                let style = match tone.as_deref() {
                    Some("ok") => OK,
                    Some("warn") => WARN,
                    Some("error") => ERROR,
                    _ => MUTED,
                };
                right.push(span(text, style));
            }
            right.push(span("  ", MUTED));
        }
        right.push(span(self.model().map_or(LOADING_MODELS.into(), models::short_label), MUTED));
        right.push(span(" ", PLAIN));
        right.push(span(&self.settings.thinking, thinking_style(&self.settings.thinking)));
        if self.context_tokens > 0 {
            right.push(span(format!(" {} {percent}%", tokens(self.context_tokens)), MUTED));
        }
        if self.settings.footer_cost
            && let Some(cost) = usage::total_cost(&self.session_usage, self.client.catalog())
        {
            right.push(span(format!(" {}", usage::dollars(cost)), MUTED));
        }
        footer_rows(left, right, width)
    }
}

impl Entry {
    fn lines(&mut self, width: usize, expanded: bool) -> &Vec<Line> {
        let fresh = matches!(&self.cache, Some((w, e, _)) if *w == width && *e == expanded);
        if !fresh {
            self.cache = Some((width, expanded, render_item(&self.item, width, expanded)));
        }
        &self.cache.as_ref().unwrap().2
    }
}

/// Accounts whose name or provider matches the query, see `panel::search`.
fn matching_accounts(accounts: Vec<AccountView>, query: &str) -> Vec<AccountView> {
    panel::search(accounts, query, |a| format!("{} {}", a.name, a.provider))
}

fn matching_prompts<'a>(entries: &'a [history::Entry], query: &str, in_scope: impl Fn(&history::Entry) -> bool) -> Vec<&'a str> {
    panel::search(entries.iter().filter(|entry| in_scope(entry)), query, |entry| entry.prompt.clone()).into_iter().map(|entry| entry.prompt.as_str()).collect()
}

/// "+3 -1" for the /changes rows.
fn diff_stat(diff: &str) -> Line {
    let body = diff.lines().filter(|line| !line.starts_with("+++") && !line.starts_with("---"));
    let (mut added, mut removed) = (0, 0);
    for line in body {
        match line.chars().next() {
            Some('+') => added += 1,
            Some('-') => removed += 1,
            _ => {}
        }
    }
    vec![span(format!("+{added}"), OK), span(" ", PLAIN), span(format!("-{removed}"), ERROR)]
}

/// `full_screen` counts scroll up from the bottom and clamps it, so this opens a diff at its first line.
const DIFF_TOP: usize = usize::MAX;

/// A file's /changes diff without git's header lines, added lines green and removed red.
fn diff_lines(file: &FileChange) -> Vec<Line> {
    let mut lines = Vec::new();
    let hunks = file.diff.lines().skip_while(|line| !line.starts_with("@@"));
    for line in hunks {
        let style = match line.chars().next() {
            Some('+') => OK,
            Some('-') => ERROR,
            Some('@') => MUTED,
            _ => PLAIN,
        };
        lines.push(vec![span(format!("  {}", line.replace('\t', "    ")), style)]);
    }
    lines
}

const USAGE_TABS: [&str; 5] = ["Session", "24 hours", "7 days", "All", "Limits"];

/// A column header row, one row per model, and a total row.
fn usage_rows(totals: &[ModelTotal], catalog: &[Model]) -> Vec<panel::Row> {
    if totals.is_empty() {
        return Vec::new();
    }
    let row = |left: &str, cells: [String; 6], style| panel::Row {
        left: left.to_string(),
        right: vec![span(format!("{:>8}  {:>8}  {:>8}  {:>10}  {:>11}  {:>9}", cells[0], cells[1], cells[2], cells[3], cells[4], cells[5]), style)],
    };
    let counts = |requests: usize, u: &Usage, cost: Option<f64>| {
        [requests.to_string(), tokens(u.input), tokens(u.output), tokens(u.cache_read), tokens(u.cache_write), cost.map(usage::dollars).unwrap_or_default()]
    };
    let mut rows = vec![row("Model", ["Requests", "Input", "Output", "Cache read", "Cache write", "Cost"].map(String::from), MUTED)];
    let (mut requests, mut sum) = (0, Usage::default());
    for total in totals {
        let used = total.usage();
        rows.push(row(&total.model, counts(total.requests.len(), &used, usage::model_cost(total, catalog)), PLAIN));
        requests += total.requests.len();
        sum.add(used);
    }
    rows.push(row("Total", counts(requests, &sum, usage::total_cost(totals, catalog)), BRIGHT));
    rows
}

/// The `/account` table: subscriptions, then API keys, each under a heading row, with columns shared by both groups.
/// Also returns each account's row index, since headings and the gap between groups cannot be selected.
fn account_rows(accounts: &[AccountView], labels: &[(String, HashMap<String, String>)]) -> (Vec<panel::Row>, Vec<usize>) {
    let mut table: Vec<(String, Vec<String>)> = Vec::new();
    let mut account_rows = Vec::new();
    let mut group = None;
    for a in accounts {
        if group != Some(a.oauth) {
            if group.is_some() {
                table.push((String::new(), Vec::new()));
            }
            group = Some(a.oauth);
            let (title, five_hour, seven_day) = if a.oauth { ("Subscription", "5h", "7d") } else { ("API key", "", "") };
            let mut heading: Vec<String> = ["Provider", five_hour, seven_day, "Default"].map(String::from).into();
            if a.oauth {
                heading.extend(labels.iter().map(|(plugin, _)| plugin.clone()));
            }
            table.push((format!("  {title}"), heading));
        }
        let percent = |share: Option<f64>| match (a.oauth, share) {
            (true, Some(share)) => format!("{share:.0}%"),
            (true, None) => "?".to_string(),
            (false, _) => String::new(),
        };
        let mark = if a.current { "✓ " } else { "  " };
        let mut cells = vec![a.provider.clone(), percent(a.usage.as_ref().map(|u| u.five_hour)), percent(a.usage.as_ref().map(|u| u.seven_day)), if a.pinned { "pinned" } else { "" }.to_string()];
        for (_, by_account) in labels {
            cells.push(a.account_id.as_ref().and_then(|id| by_account.get(id)).cloned().unwrap_or_default());
        }
        account_rows.push(table.len());
        table.push((format!("{mark}{}", a.name), cells));
    }
    let columns = table.iter().map(|(_, cells)| cells.len()).max().unwrap_or(0);
    let widths: Vec<usize> = (0..columns).map(|c| table.iter().filter_map(|(_, cells)| cells.get(c)).map(|cell| width(cell)).max().unwrap_or(0)).collect();
    let rows = table
        .into_iter()
        .map(|(left, cells)| {
            let mut right = String::new();
            for (c, cell) in cells.iter().enumerate() {
                let pad = " ".repeat(widths[c] - width(cell));
                // The 5h and 7d percentages line up on the right.
                let padded = if c == 1 || c == 2 { format!("{pad}{cell}") } else { format!("{cell}{pad}") };
                right.push_str(&padded);
                right.push_str("  ");
            }
            panel::Row { left, right: vec![span(right.trim_end(), MUTED)] }
        })
        .collect();
    (rows, account_rows)
}

/// "500k", "1m" or "250000", within 100k to 1M.
fn parse_context_window(text: &str) -> Option<u64> {
    let text = text.to_lowercase();
    let (digits, unit) = if let Some(digits) = text.strip_suffix('k') {
        (digits, 1_000)
    } else if let Some(digits) = text.strip_suffix('m') {
        (digits, 1_000_000)
    } else {
        (text.as_str(), 1)
    };
    let window = digits.parse::<u64>().ok()?.checked_mul(unit)?;
    (100_000..=1_000_000).contains(&window).then_some(window)
}

/// The trimmed arguments when `text` is the slash command `name`, alone or followed by a space.
fn command_args<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let rest = text.trim().strip_prefix(name)?;
    (rest.is_empty() || rest.starts_with(' ')).then(|| rest.trim())
}

/// OSC 7 tells Ghostty the cwd, so new splits open in the worktree this session moved to.
fn report_cwd(cwd: &Path) {
    // Control characters too: an ESC or BEL in a directory name would end the sequence early.
    let mut url = String::new();
    for c in cwd.display().to_string().chars() {
        if c == '%' || c == ' ' || c.is_control() {
            url.push_str(&format!("%{:02X}", c as u32));
        } else {
            url.push(c);
        }
    }
    print!("\x1b]7;file://{url}\x1b\\");
    let _ = std::io::stdout().flush();
}

/// OSC 0 sets the terminal's window and tab title.
fn set_terminal_title(title: &str) {
    print!("\x1b]0;{}\x07", notify::printable(title));
    let _ = std::io::stdout().flush();
}

const TODO_ROWS: usize = 5;
const PINNED_PROMPT_ROWS: usize = 3;

/// Draws a prompt over the top transcript rows, cut to three rows with "…".
fn pin_prompt(rows: &mut [Line], prompt: &[Line], width: usize) {
    let pinned = PINNED_PROMPT_ROWS.min(rows.len()).min(prompt.len());
    rows[..pinned].clone_from_slice(&prompt[..pinned]);
    if prompt.len() > pinned && pinned > 0 {
        let last = &mut rows[pinned - 1];
        last.push(span("…", BRIGHT.bold()));
        *last = truncate(last, width);
    }
}

/// The widget above the editor: up to five tasks from the first open one, gone once nothing is open.
fn todo_lines(todos: &Todos) -> Vec<Line> {
    if !todos.has_open() {
        return Vec::new();
    }
    let tasks: Vec<_> = todos.visible().skip_while(|t| t.status == Status::Completed).collect();
    let mut lines = vec![Vec::new()];
    for task in tasks.iter().take(TODO_ROWS) {
        let (mark, text) = match (task.status, &task.active_form) {
            (Status::InProgress, Some(form)) => ("▸", form),
            (Status::InProgress, None) => ("▸", &task.subject),
            (Status::Completed, _) => ("✓", &task.subject),
            _ => ("○", &task.subject),
        };
        lines.push(vec![span(format!(" {mark} {text}"), todo_style(task.status))]);
    }
    if tasks.len() > TODO_ROWS {
        lines.push(vec![span(format!("   +{} more, /todos", tasks.len() - TODO_ROWS), DIM)]);
    }
    lines
}

fn todo_style(status: Status) -> Style {
    match status {
        Status::InProgress => BRIGHT,
        Status::Completed | Status::Deleted => DIM,
        Status::Pending => PLAIN,
    }
}

fn render_item(item: &Item, width: usize, expanded: bool) -> Vec<Line> {
    match item {
        Item::Lines(lines) => lines.clone(),
        Item::System { source, text, failed: false } => system_lines("⚙", source, text, SYSTEM, width),
        Item::System { source, text, failed: true } => system_lines("✗", source, text, ERROR, width),
        Item::Subagent(run) => subagent_done_lines(run, width),
        Item::Error(text) => system_lines("✗", "error", text, ERROR, width),
        Item::User(text) => {
            let indent = vec![span("┃", BRIGHT), span(" ", PLAIN)];
            wrap(&[span(text, BRIGHT.bold())], width.saturating_sub(1), &indent)
        }
        Item::Assistant { text, thinking } => {
            let mut lines = Vec::new();
            if expanded && !thinking.trim().is_empty() {
                lines.extend(wrap(&[span(thinking.trim(), MUTED.italic())], width, &vec![span(" ", PLAIN)]));
                if !text.trim().is_empty() {
                    lines.push(Vec::new());
                }
            }
            // One column of margin, matching the thinking text above.
            for line in markdown::render(text, width.saturating_sub(1)) {
                let mut indented = vec![span(" ", PLAIN)];
                indented.extend(line);
                lines.push(indented);
            }
            lines
        }
        Item::Tool { name, input, result, .. } => render_tool(name, input, result.as_ref(), width, expanded),
        Item::Done { took, usage, total } => {
            vec![vec![span(format!(" {} (↑{} ↓{}) total {}", duration(*took), tokens(usage.input), tokens(usage.output), duration(*total)), MUTED)]]
        }
    }
}

fn system_lines(icon: &str, source: &str, text: &str, style: Style, width: usize) -> Vec<Line> {
    let hang = vec![span("  ", PLAIN)];
    let mut lines = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let spans = if index == 0 {
            vec![span(icon, style), span(" ", PLAIN), span(source, style), span(" ", PLAIN), span(line, TOOL_ARGS)]
        } else if let Some((label, description)) = line.split_once('\t') {
            // A `label\tdescription` row wraps its description under the description column.
            let column = vec![span(" ".repeat(3 + render::width(label)), PLAIN)];
            let mut rows = wrap(&[span(description, TOOL_ARGS)], width, &column);
            rows[0][0] = span(format!("  {label} "), TOOL_ARGS);
            lines.extend(rows);
            continue;
        } else {
            vec![span(line, TOOL_ARGS)]
        };
        lines.extend(wrap(&spans, width, &hang));
    }
    // Wrapped rows hang under the source; the icon row starts at column 0 like a tool row.
    if let Some(first) = lines.first_mut() {
        first.remove(0);
    }
    lines
}

fn subagent_done_lines(run: &Run, width: usize) -> Vec<Line> {
    let elapsed = duration(run.elapsed());
    let state = run.state.lock().unwrap();
    let plural = if state.tool_calls == 1 { "" } else { "s" };
    let mut stats = format!(" · {} tool{plural} · {elapsed}", state.tool_calls);
    if state.usage != Usage::default() {
        stats += &format!(" · {}", usage::brief(state.usage));
    }
    let (icon, icon_style) = if matches!(state.result, Some(Err(_))) { ("✗", ERROR) } else { ("✓", OK) };
    let mut spans = vec![span(icon, icon_style), span(" ", PLAIN), span(&run.id, SYSTEM), span(" ", PLAIN), span(&run.label, TOOL_NAME), span(stats, TOOL_ARGS)];
    if let Some(Err(error)) = &state.result {
        spans.extend([span(" · ", TOOL_ARGS), span(error, ERROR)]);
    }
    let mut lines = wrap(&spans, width, &vec![span("  ", PLAIN)]);
    lines[0].remove(0);
    lines
}

fn render_tool(name: &str, input: &Value, result: Option<&(String, bool)>, width: usize, expanded: bool) -> Vec<Line> {
    let (icon, icon_style) = match result {
        None => ("•", MUTED),
        Some((_, false)) => ("✓", OK),
        Some((_, true)) => ("✗", ERROR),
    };
    let header = match skill_name(name, input) {
        Some(skill) => {
            let hint = if expanded { "" } else { " (ctrl+o to expand)" };
            vec![span(icon, icon_style), span(" [skill] ", TOOL_NAME), span(skill, TOOL_ARGS), span(hint, MUTED)]
        }
        None => {
            vec![span(icon, icon_style), span(" ", PLAIN), span(name, TOOL_NAME), span(" ", PLAIN), span(tool_args(name, input), TOOL_ARGS)]
        }
    };
    if !expanded {
        return vec![truncate(&header, width)];
    }
    let mut lines = vec![header];
    let Some((output, is_error)) = result else { return lines };
    let style = if *is_error { ERROR } else { TOOL_ARGS };
    let pretty = serde_json::from_str::<Value>(output).ok().filter(|v| v.is_object() || v.is_array()).and_then(|v| serde_json::to_string_pretty(&v).ok());
    let output = pretty.as_deref().unwrap_or(output);
    let shown = output.lines().take(TOOL_OUTPUT_LINES).map(|line| line.replace('\t', "    "));
    let is_diff = !is_error && matches!(name, "replace" | "replace_match" | "insert");
    for (index, line) in shown.enumerate() {
        let lead = if index == 0 { "└ " } else { "  " };
        let style = match line.chars().next() {
            Some('+') if is_diff => OK,
            Some('-') if is_diff => ERROR,
            _ => style,
        };
        lines.push(truncate(&vec![span(lead, MUTED), span(line, style)], width));
    }
    let hidden = output.lines().count().saturating_sub(TOOL_OUTPUT_LINES);
    if hidden > 0 {
        lines.push(vec![span(format!("  … {hidden} more lines"), MUTED)]);
    }
    lines
}

fn skill_name(name: &str, input: &Value) -> Option<String> {
    let path = Path::new(input["path"].as_str()?);
    if name != "read" || path.file_name()? != "SKILL.md" {
        return None;
    }
    Some(path.parent()?.file_name()?.to_string_lossy().into_owned())
}

fn tool_args(name: &str, input: &Value) -> String {
    let field = |key: &str| input[key].as_str().unwrap_or_default().to_string();
    match name {
        "bash" => {
            let command = field("command").lines().next().unwrap_or_default().to_string();
            match input["timeout"].as_f64() {
                Some(secs) if secs > 0.0 => format!("{command} (timeout {secs}s)"),
                _ => command,
            }
        }
        "read_image" => field("image"),
        "read" => {
            if let Some(paths) = input["paths"].as_array() {
                return paths.iter().filter_map(|entry| entry.as_str().or(entry["path"].as_str())).collect::<Vec<_>>().join(", ");
            }
            let mut text = field("path");
            if let Some(offset) = input["offset"].as_u64().map(|n| n.to_string()).or(input["offset"].as_str().map(String::from)) {
                text.push_str(&format!(" from {offset}"));
            }
            if let Some(limit) = input["limit"].as_u64() {
                text.push_str(&format!(" ({limit} lines)"));
            }
            text
        }
        "write" => field("path"),
        "replace" | "replace_match" | "insert" => {
            let anchors = match name {
                "insert" => format!("{} {}", field("direction"), field("anchor")),
                _ => {
                    let from = input["remove_from"].as_str().or(input["replace_from"].as_str()).unwrap_or_default();
                    let to = input["remove_to"].as_str().or(input["replace_to"].as_str()).unwrap_or_default();
                    if from == to { from.to_string() } else { format!("{from}..{to}") }
                }
            };
            match input["path"].as_str() {
                Some(path) if anchors.is_empty() => path.to_string(),
                Some(path) => format!("{path} {anchors}"),
                None => anchors,
            }
        }
        "rg" => match input["path"].as_str() {
            Some(path) => format!("{} in {path}", field("pattern")),
            None => field("pattern"),
        },
        _ => match input {
            Value::Object(map) => map.iter().map(|(k, v)| format!("{k}: {}", plain_json(v))).collect::<Vec<_>>().join(" · "),
            _ => plain_json(input),
        },
    }
}

/// JSON for a one-line header: strings unquoted, objects as `key: value`.
fn plain_json(value: &Value) -> String {
    match value {
        Value::String(s) => s.lines().next().unwrap_or_default().to_string(),
        Value::Array(items) => format!("[{}]", items.iter().map(plain_json).collect::<Vec<_>>().join(", ")),
        Value::Object(map) => format!("{{{}}}", map.iter().map(|(k, v)| format!("{k}: {}", plain_json(v))).collect::<Vec<_>>().join(", ")),
        other => other.to_string(),
    }
}

/// What the slash menu completes: the `/word` up to the cursor when the cursor sits in the text's first word, else the whole text.
fn slash_query<'a>(text: &'a str, before_cursor: &'a str) -> &'a str {
    if text.starts_with(before_cursor) && !before_cursor.contains(char::is_whitespace) { before_cursor } else { text }
}

/// Commands matching a typed `/name`: prefix matches first, then names containing it. Past the name, its known arguments.
fn matching_commands(commands: &[(String, String)], text: &str) -> Vec<(String, String)> {
    let Some(query) = text.strip_prefix('/') else { return Vec::new() };
    if let Some((name, arg)) = query.split_once(' ') {
        return matching_args(name, arg);
    }
    let query = query.to_lowercase();
    let (mut matches, mut inner, mut fuzzy) = (Vec::new(), Vec::new(), Vec::new());
    for command in commands {
        if command.0.starts_with(&query) {
            matches.push(command.clone());
            continue;
        }
        match panel::search_rank(&command.0, &query) {
            Some(0) => inner.push(command.clone()),
            Some(_) => fuzzy.push(command.clone()),
            None => {}
        }
    }
    matches.extend(inner);
    matches.extend(fuzzy);
    matches
}

/// Fixed argument values of `name` starting with `arg`, as whole `name value` entries so accepting one fills or runs the command.
fn matching_args(name: &str, arg: &str) -> Vec<(String, String)> {
    let values: Vec<(&str, &str)> = match name {
        "thinking" => THINKING_LEVELS.iter().map(|level| (*level, "")).collect(),
        "ponytail" => PONYTAIL_LEVELS.iter().map(|level| (*level, "")).collect(),
        "caveman" => CAVEMAN_LEVELS.iter().map(|level| (*level, "")).collect(),
        "permissions" => MODES.iter().map(|mode| (mode.label(), mode.description())).collect(),
        "goal" => vec![("pause", "Pause the goal"), ("resume", "Resume the goal"), ("stop", "Stop the goal"), ("done", "Mark the goal done")],
        "context-window" => vec![("auto", "Drop this model's override")],
        _ => return Vec::new(),
    };
    let arg = arg.to_lowercase();
    let mut matches = Vec::new();
    for (value, description) in values {
        if value.starts_with(&arg) {
            matches.push((format!("{name} {value}"), description.to_string()));
        }
    }
    matches
}

/// Entries of the query's directory whose names start with its last part, directories first with a trailing `/`.
/// Dotfiles show only once the last part starts with a dot.
/// A query without a `/` also lists deeper paths whose names start with it, after the top-level ones.
fn matching_paths(cwd: &Path, query: &str) -> Vec<(String, String)> {
    let (dir, prefix) = query.split_at(query.rfind('/').map_or(0, |slash| slash + 1));
    let Ok(entries) = std::fs::read_dir(cwd.join(dir)) else { return Vec::new() };
    let prefix = prefix.to_lowercase();
    let mut paths = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.to_lowercase().starts_with(&prefix) || (name.starts_with('.') && !prefix.starts_with('.')) {
            continue;
        }
        let is_dir = entry.path().is_dir();
        paths.push((!is_dir, format!("{dir}{name}{}", if is_dir { "/" } else { "" })));
    }
    paths.sort();
    if dir.is_empty() && !prefix.is_empty() {
        paths.extend(nested_paths(cwd, &prefix));
    }
    paths.into_iter().map(|(_, path)| (path, String::new())).collect()
}

/// Paths below the top level whose names start with `prefix`, from `fd` so ignored trees like target/ stay out.
/// fd is killed after 150 ms, so a huge tree (a home directory takes 7 s) lists nothing deeper instead of freezing typing.
fn nested_paths(cwd: &Path, prefix: &str) -> Vec<(bool, String)> {
    let mut fd = crate::process::command("fd");
    fd.args(["--color=never", "--ignore-case", "--glob", "--min-depth=2", "--max-results=50"]).current_dir(cwd);
    fd.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null());
    if prefix.starts_with('.') {
        fd.arg("--hidden");
    }
    let Ok(mut child) = fd.arg(format!("{prefix}*")).spawn() else { return Vec::new() };
    let started = Instant::now();
    while child.try_wait().is_ok_and(|status| status.is_none()) {
        if started.elapsed() > Duration::from_millis(150) {
            let _ = child.kill();
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let Ok(output) = child.wait_with_output() else { return Vec::new() };
    if !output.status.success() {
        return Vec::new();
    }
    let mut paths: Vec<(bool, String)> = String::from_utf8_lossy(&output.stdout).lines().map(|path| (!path.ends_with('/'), path.to_string())).collect();
    paths.sort();
    paths
}

/// pj's position-only scrollbar for `visible` rows of a `total`-line transcript scrolled `scroll` lines up from the end.
/// Empty when everything fits.
fn scrollbar(total: usize, visible: usize, scroll: usize) -> Vec<&'static str> {
    if visible == 0 || total <= visible {
        return Vec::new();
    }
    let thumb = (visible * visible).div_ceil(total).clamp(1, visible);
    let max_scroll = total - visible;
    let top = (max_scroll - scroll.min(max_scroll)) * (visible - thumb) / max_scroll;
    (0..visible)
        .map(|row| match row {
            _ if row < top || row >= top + thumb => " ",
            _ if thumb == 1 => "•",
            _ if row == top => "╷",
            _ if row + 1 == top + thumb => "╵",
            _ => "│",
        })
        .collect()
}

/// A project server's entry wrapped to `width` and cut to 6 rows; true when cut.
fn mcp_entry(entry: &str, width: usize) -> (Vec<Line>, bool) {
    const ROWS: usize = 6;
    let mut lines = wrap(&[span(entry, WARN)], width, &vec![span("  ", PLAIN)]);
    let cut = lines.len() > ROWS;
    if cut {
        lines.truncate(ROWS - 1);
        lines.push(vec![span("  … cut here, read the rest in .mcp.json or .agents/mcp.json", MUTED)]);
    }
    (lines, cut)
}

/// Pads between `left` and `right` so `right` ends at the last column.
fn spread(left: Line, right: Line, width: usize) -> Line {
    let gap = width.saturating_sub(line_width(&left) + line_width(&right)).max(1);
    [left, vec![span(" ".repeat(gap), PLAIN)], right].concat()
}

/// `left` word-wrapped to `width`, with `right` at the end of its last row, or on a row of its own when that one is full.
fn footer_rows(left: Line, right: Line, width: usize) -> Vec<Line> {
    let mut rows = wrap(&left, width, &Vec::new());
    let mut last = rows.pop().expect("wrap returns at least one row");
    if line_width(&last) + 1 + line_width(&right) > width {
        rows.push(std::mem::take(&mut last));
    }
    rows.push(spread(last, right, width));
    rows
}

fn context_lines(split: agent::context::PerPart, window: u64, session: Usage, last: Usage) -> Vec<Line> {
    let used: u64 = split.iter().sum();
    let percent = |part: u64, of: u64| format!("{}%", (part * 100).checked_div(of).unwrap_or(0));
    let mut lines = vec![vec![span("Context ", TOOL_NAME), span(format!("{} of {} ({})", tokens(used), tokens(window), percent(used, window)), PLAIN)]];
    for (name, part) in agent::context::PARTS.iter().zip(split) {
        lines.push(vec![span(format!("  {name:<16}"), MUTED), span(format!("{:>7} {:>5}", tokens(part), percent(part, used)), PLAIN)]);
    }
    lines.push(vec![span(format!("  {:<16}{:>7} {:>7} {:>5}", "Cache", "read", "write", "hit"), MUTED)]);
    for (name, usage) in [("Session", session), ("Last request", last)] {
        let hit = percent(usage.cache_read, usage.input + usage.cache_read + usage.cache_write);
        lines.push(vec![span(format!("  {name:<16}"), MUTED), span(format!("{:>7} {:>7} {:>5}", tokens(usage.cache_read), tokens(usage.cache_write), hit), PLAIN)]);
    }
    lines
}

fn hotkeys_lines() -> Vec<Line> {
    let key_width = HOTKEYS.iter().flat_map(|(_, keys)| keys.iter()).map(|(key, _)| key.len()).max().unwrap_or(0);
    let mut lines = vec![vec![span("⚙", SYSTEM), span(" ", PLAIN), span("hotkeys", SYSTEM)]];
    for (group, keys) in HOTKEYS {
        lines.push(vec![span(format!("  {group}"), TOOL_NAME.bold())]);
        for (key, action) in keys {
            lines.push(vec![span(format!("    {key:key_width$}   "), TOOL_NAME), span(*action, TOOL_ARGS)]);
        }
    }
    lines
}

fn banner() -> Vec<Line> {
    vec![vec![span("░▒▓", DIM), span("█▶ agent", BRIGHT.bold()), span(format!(" v{} · Run /help for commands", env!("CARGO_PKG_VERSION")), MUTED)]]
}

/// `#1 jpeg 38 KB` for an image block from `image::block`; base64 carries 3 bytes in 4 chars.
fn image_note(number: usize, block: &Value) -> String {
    let source = &block["source"];
    let format = source["media_type"].as_str().unwrap_or_default().trim_start_matches("image/");
    let bytes = source["data"].as_str().unwrap_or_default().len() * 3 / 4;
    format!("#{number} {format} {} KB", bytes.div_ceil(1024))
}

/// Undoes `take_images`: the kept images go back to the numbers their tags in `text` use, gaps hold Null.
fn image_slots(text: &str, images: Vec<Value>) -> Vec<Value> {
    let mut slots = Vec::new();
    let mut images = images.into_iter().peekable();
    while images.peek().is_some() && slots.len() < text.len() {
        let tagged = text.contains(&format!("[Image #{}]", slots.len() + 1));
        slots.push(if tagged { images.next().unwrap() } else { Value::Null });
    }
    slots
}

/// The entry of the prompt chosen in the rewind panel: match the points to user entries from the end.
fn rewind_entry(entries: &[Entry], points: &[RewindPoint], chosen: usize) -> Option<usize> {
    let mut next = points.len();
    for (position, entry) in entries.iter().enumerate().rev() {
        let Item::User(text) = &entry.item else { continue };
        if *text != points[next - 1].text {
            continue;
        }
        next -= 1;
        if next == chosen {
            return Some(position);
        }
    }
    None
}

/// "[lsp] Cannot read x" shows as source "lsp"; an unprefixed notice comes from "agent".
fn notice_source(text: &str) -> (&str, &str) {
    text.strip_prefix('[').and_then(|rest| rest.split_once("] ")).unwrap_or(("agent", text))
}

fn rewind_note(files: Option<RestoredFiles>) -> String {
    let Some(files) = files else { return "Rewound the conversation. Files are not tracked outside a git repo.".into() };
    let mut note = match files.restored.len() {
        0 => "Rewound the conversation. No files to restore.".to_string(),
        _ => format!("Rewound the conversation and restored {}.", files.restored.join(", ")),
    };
    if !files.kept.is_empty() {
        note += &format!(" Kept {}: changed after the agent wrote them.", files.kept.join(", "));
    }
    note
}

/// A full-screen read-only view: `header`, `lines` bottom-anchored like the transcript and scrolled up by `scroll`, a rule and `hint`.
fn full_screen(header: Line, lines: &[Line], scroll: &mut usize, width: usize, height: usize, hint: &str) -> Vec<Line> {
    let room = height.saturating_sub(3);
    *scroll = (*scroll).min(lines.len().saturating_sub(room));
    let end = lines.len() - *scroll;
    let start = end.saturating_sub(room);
    let mut rows = vec![header];
    rows.extend(std::iter::repeat_n(Vec::new(), room - (end - start)));
    rows.extend_from_slice(&lines[start..end]);
    rows.push(vec![span("─".repeat(width), Style::fg(240))]);
    rows.push(vec![span(hint, DIM)]);
    rows
}

/// A terminal log as plain wrapped rows: colors dropped, a line redrawn with `\r` (a progress bar) keeps its last state.
fn log_lines(text: &str, width: usize) -> Vec<Line> {
    let indent = vec![span(" ", PLAIN)];
    let mut lines = Vec::new();
    for line in strip_ansi(text).lines() {
        let last = line.rfind('\r').map_or(line, |at| &line[at + 1..]);
        lines.extend(wrap(&[span(last.replace('\t', "    "), PLAIN)], width, &indent));
    }
    lines
}

/// A subagent's session rendered like the main transcript: a blank line between items, except inside a tool or log run.
fn subagent_lines(path: &Path, width: usize) -> Result<Vec<Line>, String> {
    let history = Session::history(path)?;
    let mut lines = Vec::new();
    let mut previous_run = None;
    for item in history_items(&history.messages) {
        let item_lines = render_item(&item, width, false);
        if item_lines.is_empty() {
            continue;
        }
        let run = transcript_run(&item);
        if run.is_none() || run != previous_run {
            lines.push(Vec::new());
        }
        lines.extend(item_lines);
        previous_run = run;
    }
    Ok(lines)
}

/// Consecutive items of the same run render without a blank line between them.
fn transcript_run(item: &Item) -> Option<&'static str> {
    match item {
        Item::Tool { .. } => Some("tool"),
        Item::System { .. } | Item::Subagent(_) | Item::Error(_) => Some("log"),
        _ => None,
    }
}

/// Transcript items for a resumed session.
fn history_items(messages: &[Value]) -> Vec<Item> {
    let mut items = Vec::new();
    for message in messages {
        for block in message["content"].as_array().into_iter().flatten() {
            match (message["role"].as_str(), block["type"].as_str()) {
                (Some("user"), Some("text")) => items.push(Item::User(block["text"].as_str().unwrap_or_default().to_string())),
                (Some("assistant"), Some("text")) => items.push(Item::Assistant { text: block["text"].as_str().unwrap_or_default().to_string(), thinking: String::new() }),
                (Some("assistant"), Some("tool_use")) => items.push(Item::Tool {
                    id: block["id"].as_str().unwrap_or_default().to_string(),
                    name: block["name"].as_str().unwrap_or_default().to_string(),
                    input: block["input"].clone(),
                    result: None,
                }),
                (Some("user"), Some("tool_result")) => {
                    let id = block["tool_use_id"].as_str().unwrap_or_default();
                    let result = (block["content"].as_str().unwrap_or_default().to_string(), block["is_error"].as_bool().unwrap_or(false));
                    for item in items.iter_mut().rev() {
                        if let Item::Tool { id: tool_id, result: slot, .. } = item
                            && tool_id == id
                        {
                            *slot = Some(result);
                            break;
                        }
                    }
                }
                _ => {}
            }
        }
    }
    items
}

fn git_info(cwd: &Path) -> Git {
    let run = |args: &[&str]| crate::process::command("git").args(args).current_dir(cwd).stderr(std::process::Stdio::null()).output().ok().filter(|o| o.status.success());
    let branch = run(&["branch", "--show-current"]).map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).map(|b| if b.is_empty() { "detached".to_string() } else { b });
    let changed = run(&["status", "--porcelain", "--untracked-files=all"]).map_or(0, |o| o.stdout.split(|b| *b == b'\n').filter(|l| !l.is_empty()).count());
    let (worktrees, current) = worktree::list(cwd).unwrap_or_default();
    let others = worktrees.len().saturating_sub(1);
    let main = worktrees.into_iter().next().filter(|main| main.path != current);
    Git { branch, changed, main, others }
}

fn context_window_label(window: Option<u64>) -> String {
    window.map_or("auto".to_string(), tokens)
}

/// `SETTING_ORDER` rows whose name matches the query, see `panel::search`.
fn matching_settings(query: &str) -> Vec<usize> {
    panel::search(SETTING_ORDER, query, |&r| SETTING_NAMES[r].to_string())
}

/// Override keys for the context windows panel: providers on tab 0, models matching `query` on tab 1.
fn context_window_targets<'a>(catalog: &'a [Model], settings: &Settings, tab: usize, query: &str) -> Vec<(String, Option<&'a Model>)> {
    if tab == 0 {
        return settings.sort_providers(&models::PROVIDERS).into_iter().map(|provider| (provider.to_string(), None)).collect();
    }
    matching_models(catalog, settings, 0, query).into_iter().flatten().map(|model| (model_key(model), Some(model))).collect()
}

/// Where the window in effect for `model` comes from.
fn window_source(settings: &Settings, model: &Model) -> &'static str {
    if settings.context_windows.contains_key(&model_key(model)) {
        "model"
    } else if settings.context_windows.contains_key(&model.provider) {
        "provider"
    } else {
        "catalog"
    }
}

/// Grey for "off" brightening linearly to pure white for "max".
fn thinking_style(level: &str) -> Style {
    let index = THINKING_LEVELS.iter().position(|l| *l == level).unwrap_or(0) as u32;
    let (darkest, last) = (0x76, THINKING_LEVELS.len() as u32 - 1);
    let gray = darkest + index * (0xff - darkest) / last;
    Style::rgb(gray * 0x010101)
}

/// /subagents: per-status counts in the title, then one row per run with a status icon, a dim id and the label as the main text.
fn subagents_panel(runs: &[Arc<Run>], selected: usize, columns: usize) -> Vec<Line> {
    let mut total = Usage::default();
    let (mut running, mut done, mut failed) = (0, 0, 0);
    for run in runs {
        let state = run.state.lock().unwrap();
        total.add(state.usage);
        match state.result {
            None => running += 1,
            Some(Ok(_)) => done += 1,
            Some(Err(_)) => failed += 1,
        }
    }
    let tick = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() / 80;
    // Same ✦ pulse as the Working line: lit 5 of every 12 ticks of 80 ms.
    let spinner = star(((tick % 12) as usize).abs_diff(2));
    let mut title = vec![span(format!("Subagents {}", runs.len()), BRIGHT.bold())];
    if running > 0 {
        title.extend([span("  ", PLAIN), spinner.clone(), span(format!(" {running} running"), SYSTEM)]);
    }
    if done > 0 {
        title.push(span(format!("  ✓ {done}"), OK));
    }
    if failed > 0 {
        title.push(span(format!("  ✗ {failed}"), ERROR));
    }
    if total != Usage::default() {
        title.push(span(format!("   {}", usage::brief(total)), DIM));
    }
    let id_width = runs.iter().map(|run| width(&run.id)).max().unwrap_or(0);
    let label_width = runs.iter().map(|run| width(&run.label)).max().unwrap_or(0);
    let pad = |text: &str, to: usize| format!("{text}{}", " ".repeat(to - width(text)));
    panel::frame(vec![title], runs.len(), selected, "↑↓ navigate     enter view     k stop     esc close", columns, PANEL_ROWS, |index| {
        let run = &runs[index];
        let elapsed = duration(run.elapsed());
        let state = run.state.lock().unwrap();
        let (icon, label_style, time_style) = match state.result {
            None => (spinner.clone(), BRIGHT, SYSTEM),
            Some(Ok(_)) => (span("✓", OK), MUTED, MUTED),
            Some(Err(_)) => (span("✗", ERROR), MUTED, MUTED),
        };
        let label_style = if index == selected { BRIGHT.bold() } else { label_style };
        let tokens = if state.usage == Usage::default() { String::new() } else { usage::brief(state.usage) };
        vec![icon, span(format!(" {} ", pad(&run.id, id_width)), DIM), span(pad(&run.label, label_width), label_style), span(format!("  {elapsed:<8}"), time_style), span(tokens, DIM)]
    })
}

/// /ps: the /subagents layout, with the title as the main text.
fn terminals_panel(terminals: &[Arc<Terminal>], selected: usize, columns: usize) -> Vec<Line> {
    let statuses: Vec<TerminalStatus> = terminals.iter().map(|t| t.status()).collect();
    let count = |status| statuses.iter().filter(|s| **s == status).count();
    let (running, done, failed, killed) = (count(TerminalStatus::Running), count(TerminalStatus::Done), count(TerminalStatus::Failed), count(TerminalStatus::Killed));
    let tick = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() / 80;
    let spinner = star(((tick % 12) as usize).abs_diff(2));
    let mut title = vec![span(format!("Background terminals {}", terminals.len()), BRIGHT.bold())];
    if running > 0 {
        title.extend([span("  ", PLAIN), spinner.clone(), span(format!(" {running} running"), SYSTEM)]);
    }
    if done > 0 {
        title.push(span(format!("  ✓ {done}"), OK));
    }
    if failed > 0 {
        title.push(span(format!("  ✗ {failed}"), ERROR));
    }
    if killed > 0 {
        title.push(span(format!("  ■ {killed}"), MUTED));
    }
    let id_width = terminals.iter().map(|t| width(&t.id)).max().unwrap_or(0);
    let title_width = terminals.iter().map(|t| width(&t.title)).max().unwrap_or(0);
    let pad = |text: &str, to: usize| format!("{text}{}", " ".repeat(to - width(text)));
    panel::frame(vec![title], terminals.len(), selected, "↑↓ navigate     enter view     k kill     esc close", columns, PANEL_ROWS, |index| {
        let terminal = &terminals[index];
        let (icon, title_style, time_style) = match statuses[index] {
            TerminalStatus::Running => (spinner.clone(), BRIGHT, SYSTEM),
            TerminalStatus::Done => (span("✓", OK), MUTED, MUTED),
            TerminalStatus::Failed => (span("✗", ERROR), MUTED, MUTED),
            TerminalStatus::Killed => (span("■", MUTED), MUTED, MUTED),
        };
        let title_style = if index == selected { BRIGHT.bold() } else { title_style };
        vec![icon, span(format!(" {} ", pad(&terminal.id, id_width)), DIM), span(pad(&terminal.title, title_width), title_style), span(format!("  {}", terminal.elapsed()), time_style)]
    })
}

fn duration(d: Duration) -> String {
    let secs = d.as_secs();
    match (secs / 3600, secs / 60 % 60, secs % 60) {
        (0, 0, s) => format!("{s}s"),
        (0, m, s) => format!("{m}m {s}s"),
        (h, m, s) => format!("{h}h {m}m {s}s"),
    }
}

fn copy_to_clipboard(text: &str) -> Result<(), String> {
    let (program, args): (&str, &[&str]) = if cfg!(target_os = "macos") {
        ("pbcopy", &[])
    } else if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        ("wl-copy", &[])
    } else {
        ("xclip", &["-selection", "clipboard"])
    };
    let mut child = crate::process::command(program)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("Cannot run {program}: {e}"))?;
    // The stdin handle drops before `wait`, so the child sees EOF; it is reaped even when the write failed.
    let written = child.stdin.take().unwrap().write_all(text.as_bytes());
    child.wait().map_err(|e| e.to_string())?;
    written.map_err(|e| e.to_string())
}

fn open_in_browser(url: &str) -> Result<(), String> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(format!("Not a web link: {url}"));
    }
    if !cfg!(target_os = "macos") {
        // Reaped on a thread: xdg-open runs a browser without a desktop handler in the foreground.
        let mut child =
            crate::process::command("xdg-open").arg(url).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().map_err(|e| format!("Cannot run xdg-open: {e}"))?;
        std::thread::spawn(move || child.wait());
        return Ok(());
    }
    // output() and not status(): open's stderr would draw over the alternate screen.
    let output = crate::process::command("open").arg(url).output().map_err(|e| format!("Cannot run open: {e}"))?;
    if !output.status.success() {
        return Err(format!("open {url} failed: {}", String::from_utf8_lossy(&output.stderr).trim()));
    }
    Ok(())
}

/// fx's "Permission needed" prompt.
fn permission_lines(request: &Request, selected: usize, width: usize) -> Vec<Line> {
    let title = vec![span("  ", PLAIN), span("Permission needed · Choose one", PLAIN.bold())];
    let always = request.kind.always();
    let mut lines = vec![Vec::new(), spread(title, vec![span(request.kind.label(), MUTED)], width), Vec::new(), vec![span(format!("  {}", request.question), PLAIN.bold())], Vec::new()];
    let prefix = if request.kind == Kind::Command { "$ " } else { "" };
    for (index, line) in request.detail.lines().enumerate() {
        let lead = if index == 0 { prefix } else { "  " };
        lines.extend(wrap(&[span(format!("{lead}{line}"), BRIGHT.bold())], width, &vec![span("  ", PLAIN)]));
    }
    lines.push(Vec::new());
    for (index, option) in ["Yes", always, "No"].iter().enumerate() {
        let label = format!("{}. {option}", index + 1);
        lines.push(if index == selected { vec![span("  ", PLAIN), span("→ ", OK), span(label, BRIGHT.bold())] } else { vec![span(format!("    {label}"), PLAIN)] });
    }
    lines.push(vec![span("↑↓ move     enter choose     1-3 pick     esc deny", MUTED)]);
    lines
}

/// Picker rows for settings `row` whose provider or id matches `query` (see `panel::search`): pinned models first, each part grouped in provider order.
/// `None` is "auto", offered for the optional condense and fallback models.
fn matching_models<'a>(catalog: &'a [Model], settings: &Settings, row: usize, query: &str) -> Vec<Option<&'a Model>> {
    let auto = (row != 0 && panel::search_rank("auto", query).is_some()).then_some(None);
    let providers = settings.sort_providers(&models::PROVIDERS);
    let mut matches = panel::search(catalog, query, |m| format!("{} {}", model_owners(m), m.id));
    matches.sort_by_key(|m| (!settings.pinned_models.contains(&model_key(m)), providers.iter().position(|provider| *provider == m.provider).unwrap_or(usize::MAX)));
    auto.into_iter().chain(matches.into_iter().map(Some)).collect()
}

/// "openai-codex · pro, plus": the provider and the accounts whose model list has the model.
fn model_owners(model: &Model) -> String {
    if model.accounts.is_empty() {
        return model.provider.clone();
    }
    format!("{} · {}", model.provider, model.accounts.join(", "))
}

/// The model id settings `row` edits: 0 model, 8 condense, 12 fallback, 29 reviewer, 30 subagent.
fn model_setting(settings: &mut Settings, row: usize) -> &mut String {
    match row {
        0 => &mut settings.model,
        8 => &mut settings.condense_model,
        12 => &mut settings.fallback_model,
        29 => &mut settings.reviewer_model,
        _ => &mut settings.subagent_model,
    }
}

/// The option after (or before) `current`, wrapping around; the first option if `current` is not listed.
fn step<T: PartialEq + Clone>(options: &[T], current: &T, forward: bool) -> T {
    let position = options.iter().position(|o| o == current);
    let index = match (position, forward) {
        (None, _) => 0,
        (Some(i), true) => (i + 1) % options.len(),
        (Some(i), false) => (i + options.len() - 1) % options.len(),
    };
    options[index].clone()
}

fn switch_label(percent: u64) -> String {
    if percent == 0 { "never".into() } else { format!("{percent}%") }
}

/// fx's inline setting values: every option, the current one checked and bright. A current value missing from the options is appended.
fn choices(options: impl IntoIterator<Item = String>, current: &str) -> Line {
    let mut line = Vec::new();
    let mut listed = false;
    for option in options {
        if !line.is_empty() {
            line.push(span("  ", PLAIN));
        }
        listed |= option == current;
        line.push(if option == current { span(format!("✓ {option}"), BRIGHT.bold()) } else { span(option, MUTED) });
    }
    if !listed {
        line.push(span(format!("  ✓ {current}"), BRIGHT.bold()));
    }
    line
}

/// An on/off row for a tool the user installs separately; the setting does nothing until `binary` is on PATH.
fn on_off_choices(on: bool) -> Line {
    choices(["off", "on"].map(String::from), if on { "on" } else { "off" })
}

fn optional_tool(on: bool, binary: &str) -> Line {
    let mut line = on_off_choices(on);
    if !crate::process::installed(binary) {
        line.push(span(format!("  {binary} is not installed"), MUTED));
    }
    line
}

/// fx's session age: 45s, 3m, 5h, 2d.
fn age(time: SystemTime) -> String {
    let secs = time.elapsed().map_or(0, |d| d.as_secs());
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

fn shimmer_line(label: &str, tick: usize) -> Line {
    let head = (tick % (label.len() + 10)) as isize - 3;
    let level = |column: usize| match (column as isize - head).unsigned_abs() {
        distance @ 0..=2 => 255 - 4 * distance as u8,
        _ => 245,
    };
    let mut line = vec![span(" ", PLAIN), star(head.unsigned_abs()), span(" ", PLAIN)];
    for (index, letter) in label.chars().enumerate() {
        line.push(span(letter.to_string(), Style::fg(level(index + 2))));
    }
    line
}

/// Lit ✦ while a shimmer band `distance` columns away passes over it, dim ✧ otherwise.
fn star(distance: usize) -> Span {
    match distance {
        0..=2 => span("✦", Style::fg(255 - 4 * distance as u8)),
        _ => span("✧", DIM),
    }
}

fn recording_line(elapsed: Duration) -> Line {
    let seconds = elapsed.as_secs();
    vec![span(" ●", ERROR), span(format!(" {}:{:02}", seconds / 60, seconds % 60), BRIGHT), span("  Enter to transcribe · Esc to cancel", DIM)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn bracketed_notice_prefix_becomes_the_source() {
        assert_eq!(notice_source("[lsp] Cannot read a.rs"), ("lsp", "Cannot read a.rs"));
        assert_eq!(notice_source("Operation aborted"), ("agent", "Operation aborted"));
    }

    #[test]
    fn duration_rolls_minutes_into_hours() {
        assert_eq!(duration(Duration::from_secs(59)), "59s");
        assert_eq!(duration(Duration::from_secs(27811)), "7h 43m 31s");
    }

    #[test]
    fn step_wraps_and_starts_over_from_an_unlisted_value() {
        let models = ["a", "b", "c"];
        assert_eq!(step(&models, &"c", true), "a");
        assert_eq!(step(&models, &"x", true), "a");
    }

    #[test]
    fn a_finished_subagent_shows_its_status_counts_and_error() {
        let run = crate::agent::subagent::scan_run();
        *run.state.lock().unwrap() = crate::agent::subagent::RunState {
            tool_calls: 1,
            elapsed: Some(Duration::from_secs(319)),
            result: Some(Ok("found".into())),
            usage: Usage { input: 108, output: 33_000, ..Usage::default() },
            ..Default::default()
        };
        let rows: Vec<String> = subagent_done_lines(&run, 80).iter().map(text).collect();
        assert_eq!(rows, ["✓ sa-1 scan · 1 tool · 5m 19s · ↑108 ↓33k cache 0"]);
        *run.state.lock().unwrap() = crate::agent::subagent::RunState { tool_calls: 31, elapsed: Some(Duration::from_secs(8)), result: Some(Err("Operation aborted".into())), ..Default::default() };
        let rows: Vec<String> = subagent_done_lines(&run, 30).iter().map(text).collect();
        assert_eq!(rows, ["✗ sa-1 scan · 31 tools · 8s ", "  · Operation aborted"]);
    }

    #[test]
    fn unknown_tool_args_read_as_key_values() {
        let input = json!({"action": "create", "subject": "Manifest: a, b", "ids": [1, 2], "q": [{"header": "Panel"}]});
        assert_eq!(tool_args("todo", &input), "action: create · ids: [1, 2] · q: [{header: Panel}] · subject: Manifest: a, b");
    }

    fn text(line: &Line) -> String {
        line.iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn footer_wraps_by_word_when_the_row_is_full() {
        let rows = |width| footer_rows(vec![span("~/r/app-pr detached → main ~/r/app", PLAIN)], vec![span("opus 12%", MUTED)], width).iter().map(text).collect::<Vec<_>>();
        assert_eq!(rows(44), ["~/r/app-pr detached → main ~/r/app  opus 12%"]);
        assert_eq!(rows(30), ["~/r/app-pr detached → main ", "~/r/app               opus 12%"]);
        assert_eq!(rows(15), ["~/r/app-pr ", "detached → main", "~/r/app", "       opus 12%"]);
    }

    #[test]
    fn working_glyph_fills_only_while_the_band_passes_it() {
        let lit = shimmer_line(WORKING, 3);
        assert_eq!(text(&lit), " ✦ Working");
        assert_eq!(lit[1].style, Style::fg(255));

        let later = shimmer_line(WORKING, 10);
        assert_eq!(text(&later), " ✧ Working");
        assert_eq!(later[1].style, DIM);
        assert_eq!(later[3 + 5].style, Style::fg(255));
    }

    #[test]
    fn recording_line_shows_minutes_and_seconds() {
        let line = recording_line(Duration::from_secs(75));
        assert_eq!(text(&line), " ● 1:15  Enter to transcribe · Esc to cancel");
        assert_eq!(line[0].style, ERROR);
    }

    #[test]
    fn settings_list_every_row_once_and_filter_by_name() {
        let mut rows = matching_settings("");
        rows.sort();
        assert_eq!(rows, (0..SETTING_NAMES.len()).collect::<Vec<_>>());
        assert_eq!(matching_settings("CONTEXT WIN"), [CONTEXT_WINDOW_ROW]);
        assert!(matching_settings("no such setting").is_empty());
    }

    #[test]
    fn hotkeys_list_groups_with_keys_in_one_column() {
        let rows: Vec<String> = hotkeys_lines().iter().map(text).collect();
        assert_eq!(rows[0], "⚙ hotkeys");
        assert_eq!(rows[1], "  Send");
        assert!(rows[2].starts_with("    Enter  "), "{rows:?}");
        let column = |row: &String| row.trim_start().len() - row.trim_start().split("  ").skip(1).collect::<Vec<_>>().join("  ").trim_start().len();
        let starts: Vec<usize> = rows.iter().filter(|row| row.starts_with("    ")).map(|row| row.len() - row.trim_start().len() + column(row)).collect();
        assert!(starts.windows(2).all(|pair| pair[0] == pair[1]), "{rows:?}");
    }

    #[test]
    fn context_window_accepts_100k_to_1m() {
        assert_eq!(parse_context_window("100k"), Some(100_000));
        assert_eq!(parse_context_window("1M"), Some(1_000_000));
        assert_eq!(parse_context_window("250000"), Some(250_000));
        assert_eq!(parse_context_window("99k"), None);
        assert_eq!(parse_context_window("2m"), None);
        assert_eq!(parse_context_window("lots"), None);
    }

    #[test]
    fn collapsed_tool_call_is_one_line_even_for_long_bash_and_errors() {
        let ok = ("total 1\na.txt  6B\n".to_string(), false);
        let long = json!({"command": "echo aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"});
        let rows: Vec<String> = render_tool("bash", &long, Some(&ok), 20, false).iter().map(text).collect();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].chars().count() <= 20, "{rows:?}");
        let failed = ("no such file\n".to_string(), true);
        assert_eq!(render_tool("read", &json!({"path": "x"}), Some(&failed), 40, false).len(), 1);
    }

    #[test]
    fn expanded_tool_call_puts_the_output_under_a_hook() {
        let result = ("total 1\na.txt  6B\n".to_string(), false);
        let lines = render_tool("bash", &json!({"command": "ls", "timeout": 10}), Some(&result), 40, true);
        let rows: Vec<String> = lines.iter().map(text).collect();
        assert_eq!(rows, ["✓ bash ls (timeout 10s)", "└ total 1", "  a.txt  6B"]);
    }

    #[test]
    fn reading_a_skill_file_shows_the_skill_name() {
        let result = ("body".to_string(), false);
        let lines = render_tool("read", &json!({"path": "/x/skills/verify-web-app/SKILL.md"}), Some(&result), 80, false);
        assert_eq!(text(&lines[0]), "✓ [skill] verify-web-app (ctrl+o to expand)");
    }

    #[test]
    fn log_row_wraps_under_its_source() {
        let lines = system_lines("✗", "error", "aaa bbb ccc\nddd", ERROR, 13);
        let rows: Vec<String> = lines.iter().map(text).collect();
        assert_eq!(rows, ["✗ error aaa", "  bbb ccc", "  ddd"]);
    }

    #[test]
    fn a_tabbed_log_row_wraps_its_description_under_the_description_column() {
        let lines = system_lines("⚙", "help", "2 commands\n/goal\taaa bbb ccc\n/btw\t", SYSTEM, 16);
        let rows: Vec<String> = lines.iter().map(text).collect();
        assert_eq!(rows, ["⚙ help 2 ", "  commands", "  /goal aaa bbb ", "        ccc", "  /btw "]);
    }

    #[test]
    fn edit_diff_colors_added_green_removed_red_context_gray() {
        let result = (" AAAA│keep\n-BBBB│old\n+CCCC│new".to_string(), false);
        let lines = render_tool("replace", &json!({"path": "a.rs"}), Some(&result), 40, true);
        let colors: Vec<Style> = lines[1..].iter().map(|line| line[1].style).collect();
        assert_eq!(colors, [TOOL_ARGS, ERROR, OK]);
    }

    #[test]
    fn slash_menu_lists_prefix_matches_first_and_closes_after_a_space() {
        let commands: Vec<(String, String)> = ["compact", "skill:code-review", "copy"].iter().map(|n| (n.to_string(), String::new())).collect();
        let names = |text: &str| matching_commands(&commands, text).into_iter().map(|c| c.0).collect::<Vec<_>>();
        assert_eq!(names("/co"), ["compact", "copy", "skill:code-review"]);
        assert_eq!(names("/"), ["compact", "skill:code-review", "copy"]);
        assert!(names("/copy now").is_empty());
        assert!(names("copy").is_empty());
    }

    #[test]
    fn slash_menu_completes_a_slash_typed_in_front_of_existing_text() {
        assert_eq!(slash_query("/skfix the bug", "/sk"), "/sk");
        assert_eq!(slash_query("/thinking m", "/thinking m"), "/thinking m");
        assert_eq!(slash_query("fix the bug /sk", "fix the bug /sk"), "fix the bug /sk");
    }

    #[test]
    fn slash_menu_lists_known_arguments_after_the_name() {
        let names = |text: &str| matching_commands(&[], text).into_iter().map(|c| c.0).collect::<Vec<_>>();
        assert_eq!(names("/thinking m"), ["thinking minimal", "thinking medium", "thinking max"]);
        assert_eq!(names("/permissions "), ["permissions ask", "permissions auto", "permissions full access"]);
        assert!(names("/thinking medium ").is_empty());
    }

    #[test]
    fn at_menu_lists_a_directory_then_nested_names_with_folders_first_and_dotfiles_on_request() {
        let root = std::env::temp_dir().join(format!("agent-at-{}", std::process::id()));
        std::fs::create_dir_all(root.join("src/tui")).unwrap();
        for file in ["b.md", ".env", "src/main.rs"] {
            std::fs::write(root.join(file), "").unwrap();
        }
        let paths = |query: &str| matching_paths(&root, query).into_iter().map(|p| p.0).collect::<Vec<_>>();
        assert_eq!(paths(""), ["src/", "b.md"]);
        assert_eq!(paths("."), [".env"]);
        assert_eq!(paths("src/"), ["src/tui/", "src/main.rs"]);
        assert_eq!(paths("src/M"), ["src/main.rs"]);
        assert_eq!(paths("tu"), ["src/tui/"]);
        assert_eq!(paths("m"), ["src/main.rs"]);
        assert!(paths("nope/").is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn thinking_level_brightens_from_grey_to_pure_white() {
        let grays: Vec<Style> = THINKING_LEVELS.iter().map(|l| thinking_style(l)).collect();
        assert_eq!(grays[0], Style::rgb(0x767676));
        assert_eq!(grays[THINKING_LEVELS.len() - 1], Style::rgb(0xffffff));
        assert!(grays.windows(2).all(|pair| pair[0] != pair[1]));
    }

    #[test]
    fn token_counts_round_like_pi() {
        assert_eq!([tokens(950), tokens(14_400), tokens(999_600), tokens(1_250_000)], ["950", "14k", "1.0M", "1.2M"]);
    }

    #[test]
    fn model_search_matches_provider_account_or_id_ignoring_case() {
        let model = |provider: &str, id: &str, accounts: &[&str]| Model { provider: provider.into(), id: id.into(), accounts: accounts.iter().map(|a| a.to_string()).collect(), ..Model::default() };
        let catalog = [model("anthropic", "claude-haiku-5-5", &["work"]), model("openai-codex", "gpt-6-luna", &["pro", "plus"]), model("openai-codex", "gpt-6-sol", &["pro"])];
        let mut settings = settings::load(Path::new("/nonexistent")).unwrap();
        let ids = |settings: &Settings, row, query| matching_models(&catalog, settings, row, query).iter().map(|m| m.map_or("auto", |m| m.id.as_str()).to_string()).collect::<Vec<_>>();
        assert_eq!(ids(&settings, 0, "HAIKU"), ["claude-haiku-5-5"]);
        assert_eq!(ids(&settings, 0, "codex"), ["gpt-6-luna", "gpt-6-sol"]);
        assert_eq!(ids(&settings, 0, "plus"), ["gpt-6-luna"]);
        assert_eq!(model_owners(&catalog[1]), "openai-codex · pro, plus");
        assert_eq!(ids(&settings, 0, "").len(), catalog.len());
        assert_eq!(ids(&settings, 8, "").len(), catalog.len() + 1);
        assert_eq!(ids(&settings, 12, "aut"), ["auto", "gpt-6-luna"]);
        assert!(ids(&settings, 8, "xyz").is_empty());
        settings.provider_sort = "manual".into();
        settings.provider_order = vec!["openai-codex".into()];
        assert_eq!(ids(&settings, 8, ""), ["auto", "gpt-6-luna", "gpt-6-sol", "claude-haiku-5-5"]);
        settings.pinned_models = vec!["openai-codex/gpt-6-sol".into()];
        assert_eq!(ids(&settings, 8, ""), ["auto", "gpt-6-sol", "gpt-6-luna", "claude-haiku-5-5"]);
    }

    #[test]
    fn scrollbar_thumb_tracks_the_position_and_hides_when_everything_fits() {
        assert!(scrollbar(4, 4, 0).is_empty());
        assert_eq!(scrollbar(8, 4, 0), [" ", " ", "╷", "╵"]);
        assert_eq!(scrollbar(8, 4, 4), ["╷", "╵", " ", " "]);
        assert_eq!(scrollbar(100, 3, 48), [" ", "•", " "]);
    }

    #[test]
    fn pinned_prompt_keeps_three_rows_and_marks_the_cut() {
        let text = |rows: &[Line]| rows.iter().map(|row| row.iter().map(|s| s.text.as_str()).collect::<String>()).collect::<Vec<_>>();
        let line = |t: &str| vec![span(t, PLAIN)];
        let mut rows = vec![line("x"); 5];
        pin_prompt(&mut rows, &[line("a"), line("b")], 10);
        assert_eq!(text(&rows), ["a", "b", "x", "x", "x"]);
        pin_prompt(&mut rows, &[line("1"), line("2"), line("3"), line("4")], 10);
        assert_eq!(text(&rows), ["1", "2", "3…", "x", "x"]);
    }

    #[test]
    fn history_pairs_tool_results_with_their_calls() {
        let messages = [
            json!({"role": "assistant", "content": [{"type": "tool_use", "id": "t1", "name": "read", "input": {"path": "a"}}]}),
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "x", "is_error": true}]}),
        ];
        let items = history_items(&messages);
        assert!(matches!(&items[0], Item::Tool { result: Some((out, true)), .. } if out == "x"));
    }

    #[test]
    fn rewind_entry_finds_the_chosen_prompt_among_repeats_and_bash_output() {
        let entry = |item: Item| Entry { item, cache: None };
        let user = |text: &str| entry(Item::User(text.into()));
        // A resumed session shows shell output blocks as user items too; they are not rewind points.
        let entries = [user("go"), entry(Item::Lines(Vec::new())), user("Ran `ls`"), user("go"), user("done?")];
        let point = |at: usize, text: &str| RewindPoint { at, text: text.into(), images: Vec::new() };
        let points = [point(0, "go"), point(2, "go"), point(4, "done?")];
        assert_eq!(rewind_entry(&entries, &points, 0), Some(0));
        assert_eq!(rewind_entry(&entries, &points, 1), Some(3));
    }

    #[test]
    fn image_slots_put_kept_images_back_under_their_tags() {
        let (two, three) = (json!("png2"), json!("png3"));
        let slots = image_slots("see [Image #2] and [Image #3]", vec![two.clone(), three.clone()]);
        assert_eq!(slots, [Value::Null, two, three]);
    }

    #[test]
    fn image_note_names_the_tag_format_and_decoded_size() {
        let block = json!({"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "A".repeat(4096)}});
        assert_eq!(image_note(2, &block), "#2 jpeg 3 KB");
    }

    #[test]
    fn subagent_lines_render_its_session_file() {
        let dir = std::env::temp_dir().join(format!("agent-subagent-lines-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let session = dir.join("abc123.jsonl");
        let messages = [json!({"role": "user", "content": [{"type": "text", "text": "review auth"}]}), json!({"role": "assistant", "content": [{"type": "text", "text": "Token stays cached."}]})];
        let entries: Vec<String> = messages.iter().map(|message| json!({"type": "message", "message": message}).to_string()).collect();
        std::fs::write(&session, entries.join("\n")).unwrap();
        let lines = subagent_lines(&session, 60);
        std::fs::remove_dir_all(&dir).unwrap();
        let lines: Vec<String> = lines.unwrap().iter().map(text).collect();
        assert!(lines.iter().any(|line| line.contains("review auth")));
        assert!(lines.iter().any(|line| line.contains("Token stays cached.")));
    }

    #[test]
    fn subagent_lines_keep_consecutive_tool_calls_together() {
        let dir = std::env::temp_dir().join(format!("agent-subagent-tools-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let session = dir.join("abc123.jsonl");
        let tool = |id: &str, path: &str| json!({"type": "tool_use", "id": id, "name": "read", "input": {"path": path}});
        let result = |id: &str| json!({"type": "tool_result", "tool_use_id": id, "content": "ok"});
        let messages = [
            json!({"role": "assistant", "content": [tool("a", "one.rs"), tool("b", "two.rs")]}),
            json!({"role": "user", "content": [result("a"), result("b")]}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "Done."}]}),
        ];
        let entries: Vec<String> = messages.iter().map(|message| json!({"type": "message", "message": message}).to_string()).collect();
        std::fs::write(&session, entries.join("\n")).unwrap();
        let lines = subagent_lines(&session, 60);
        std::fs::remove_dir_all(&dir).unwrap();
        let lines: Vec<String> = lines.unwrap().iter().map(text).collect();
        let one = lines.iter().position(|line| line.contains("one.rs")).unwrap();
        let two = lines.iter().position(|line| line.contains("two.rs")).unwrap();
        let done = lines.iter().position(|line| line.contains("Done.")).unwrap();
        assert!(lines[one..two].iter().all(|line| !line.trim().is_empty()), "{lines:#?}");
        assert!(lines[two..done].iter().any(|line| line.trim().is_empty()), "{lines:#?}");
    }

    #[test]
    fn log_lines_drop_colors_and_keep_the_last_progress_redraw() {
        let lines: Vec<String> = log_lines("\x1b[32mok\x1b[0m\n10%\r55%\r100%\nabcdefghij", 6).iter().map(text).collect();
        assert_eq!(lines, [" ok", " 100%", " abcde", " fghij"]);
    }

    #[test]
    fn account_rows_group_subscriptions_and_api_keys_in_shared_columns() {
        let account = |name: &str, oauth: bool, id: Option<&str>, usage: Option<(f64, f64)>| AccountView {
            provider: "anthropic".into(),
            name: name.into(),
            oauth,
            account_id: id.map(String::from),
            usage: usage.map(|(five_hour, seven_day)| crate::api::accounts::Usage { five_hour, seven_day, resets_at: None }),
            current: name == "claudescom",
            pinned: name == "rokartur",
        };
        let accounts = [account("claudescom", true, Some("uuid-1"), Some((21.0, 73.0))), account("rokartur", true, Some("uuid-2"), None), account("claudescom-api", false, None, None)];
        let labels = [("usagefleet".to_string(), HashMap::from([("uuid-1".to_string(), "uf_tVdC…".to_string())]))];
        let (rows, account_rows) = account_rows(&accounts, &labels);
        let table: Vec<(&str, String)> = rows.iter().map(|row| (row.left.as_str(), text(&row.right))).collect();
        assert_eq!(
            table,
            [
                ("  Subscription", "Provider    5h   7d  Default  usagefleet".to_string()),
                ("✓ claudescom", "anthropic  21%  73%           uf_tVdC…".to_string()),
                ("  rokartur", "anthropic    ?    ?  pinned".to_string()),
                ("", String::new()),
                ("  API key", "Provider             Default".to_string()),
                ("  claudescom-api", "anthropic".to_string()),
            ]
        );
        assert_eq!(account_rows, [1, 2, 5]);
        let names = |query| matching_accounts(accounts.to_vec(), query).into_iter().map(|a| a.name).collect::<Vec<_>>();
        assert_eq!(names("API"), ["claudescom-api", "claudescom", "rokartur"]);
        assert_eq!(names("anthropic").len(), 3);
    }
}
