mod ask;
mod attached;
mod btw;
mod client;
mod cmux;
mod commands;
mod complete;
mod editor;
mod events;
mod extension_panels;
mod footer;
mod history;
mod input;
mod list;
mod login;
mod markdown;
mod notify;
mod panels;
mod render;
mod screens;
mod selection;
mod server;
mod session_panels;
mod settings_panels;
mod skills;
mod transcript;
mod view;
mod voice;
mod wire;
mod worker;
mod workflow_panels;

pub use client::{Ending, Fullscreen, attach, confirm, ctrl_c_within, notice};
pub use wire::{PROTOCOL, Place, SILENCE};

use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste, EnableFocusChange, EnableMouseCapture, Event as Input, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
    MouseButton, MouseEvent, MouseEventKind,
};
#[cfg(unix)]
use crossterm::event::{KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags};
use crossterm::execute;
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Stdout;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use crate::agent::FollowUp;
use crate::agent::modes::{self, CAVEMAN_LEVELS, OUTPUT_STYLES, PONYTAIL_LEVELS};
use crate::agent::peers::Peer;
use crate::agent::permissions::{Decision, Kind, MODES, Mode, Request};
use crate::agent::prompt::{Skill, Template};
use crate::agent::skill_usage;
use crate::agent::subagent::{Run, Runs};
use crate::agent::usage::{self, ModelTotal, tokens};
use crate::agent::workflow::{self, StepStatus, Workflow};
use crate::agent::{self, Agent, Event, FileChange, Opened, RestoredFiles, RewindPoint, Session, SessionInfo, Spent};
use crate::api::accounts::Policy;
use crate::api::models::{self, Model};
use crate::api::oauth::Tokens;
use crate::api::{AccountStatus, AccountView, Client, Usage};
use crate::settings::THINKING_LEVELS;
use crate::settings::{self, CACHE_TTLS, CONFLICT_PRIORITIES, MID_TURN, PROVIDER_SORTS, PRUNE_SESSIONS, SPEEDS, Settings, WORKING_ANIMATIONS, model_key};
use crate::tools::bg::{Status as TerminalStatus, Terminal, Terminals};
use crate::tools::goal::{Goal, Status as GoalStatus};
use crate::tools::plan::Plan;
use crate::tools::rtk::strip_ansi;
use crate::tools::todo::{Status, Task, Todos};
use crate::worktree::{self, Worktree};
use commands::*;
use complete::*;
use editor::Editor;
use footer::*;
use panels::*;
use render::{BRIGHT, DIM, Line, MUTED, PLAIN, Span, Style, line_width, link_at, overlay_center, overlay_right, plain_text, select, span, take_cursor, text_in, truncate, width, wrap};
use screens::*;
use selection::{Point, Selection};
use session_panels::*;
use settings_panels::*;
use transcript::*;
use view::*;
use worker::*;

const WORKING: &str = "Working";
const THINKING: &str = "Thinking";
const TRANSCRIBING: &str = "Transcribing";
/// Key a dropped image's block carries its file path under until `take_images` shows and strips it.
const DROPPED_PATH: &str = "dropped_path";
const LOADING_MODELS: &str = "loading models";
// Matched whole: an error hint that mentions Ctrl+C must not arm quitting.
const EXIT_HINT: &str = "Press Ctrl+C again to exit";
const REWIND_HINT: &str = "Press Esc again to rewind";
const MODELS_STILL_LOADING: &str = "The model lists are still loading.";
const TOOL_OUTPUT_LINES: usize = 20;
/// One line per tick while a selection drag rests on the transcript edge.
const ANIMATION_TICK: Duration = Duration::from_millis(80);
const DONE_FLASH: Duration = Duration::from_millis(400);
const HINT_LIFETIME: Duration = Duration::from_millis(1500);
/// Other sessions and terminals add worktrees, switch branches and edit files, so the footer polls git.
const GIT_REFRESH: Duration = Duration::from_secs(3);
const RUN_VIEW_REFRESH: Duration = Duration::from_millis(250);
const EDGE_SCROLL: Duration = Duration::from_millis(40);
const PERMISSION_SETTLE: Duration = Duration::from_millis(400);

const WARN: Style = Style::rgb(0xffb340);
const FOOTER_SEPARATOR: &str = " · ";
const SYSTEM: Style = Style::rgb(0x58a6ff);
const OK: Style = Style::rgb(0x3fdc6a);
const ERROR: Style = Style::rgb(0xff5c57);
const TOOL_NAME: Style = Style::rgb(0xffffff);
const TOOL_ARGS: Style = Style::rgb(0x6c6c6c);
const NOTICE_TEXT: Style = Style::rgb(0xbbbbbb);
const PROMPT_BG: u8 = 237;
const PROMPT_LEAD: Style = TOOL_NAME.bold();

enum Msg {
    Attached(server::Client),
    /// A terminal event from the client with this id.
    Input(u64, Input),
    /// An image from the client's computer: its clipboard's after Ctrl+V, or a dropped file with the `paste` that named it.
    Image {
        id: u64,
        paste: Option<String>,
        image: Result<Value, String>,
    },
    /// The client's computer copied what the session asked it to.
    TerminalCopied,
    /// The client's computer could not copy or open what the session asked it to.
    TerminalFailed(String),
    /// Where the client with this id sees the session after /env removed one of its saved environments.
    Place(u64, wire::Place),
    Detached(u64),
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
    /// The agent's extra directories after /workspace or a resume.
    Workspace(Vec<PathBuf>),
    BtwDelta(String),
    BtwDone(Result<(), String>),
    Cloned {
        id: String,
        path: Option<String>,
    },
    /// /handoff wrote the first prompt of the fresh session it starts.
    HandedOff(String),
    LoginFinished {
        provider: &'static str,
        /// The exchange's cancel flag, which tells it apart from a newer /login.
        cancel: Arc<AtomicBool>,
        result: Result<Tokens, String>,
    },
    /// GitHub answered with a Copilot device code; `cancel` belongs to the /login that asked.
    CopilotDevice {
        cancel: Arc<AtomicBool>,
        device: crate::api::copilot::Device,
    },
    /// Account usage arrived for /account.
    UsageFetched,
    WorktreeRemoved(PathBuf, Result<(), String>),
    /// The footer's git state, read off the UI thread in this cwd, and how long the read took.
    Git(PathBuf, Git, Duration),
    /// The rules of a newly picked ponytail or caveman level finished downloading.
    ModesFetched(Result<(), String>),
    /// A plugin set its status or exited: redraw the footer and /plugins.
    PluginsChanged,
    /// An MCP server connected, failed or exited: redraw /mcp.
    McpChanged,
    Transcribed(Result<String, String>),
    RewindPoints(Vec<RewindPoint>),
    /// The unanswered prompt Esc took back, now gone from the session.
    Retracted(RewindPoint),
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

enum Command {
    /// The prompt text and the image blocks pasted into it.
    Prompt(String, Vec<Value>),
    /// `!cmd` typed by the user; `keep` adds its output to the next prompt.
    Bash {
        command: String,
        keep: bool,
    },
    Compact(Option<String>),
    /// /handoff and the next task it names.
    Handoff(Option<String>),
    /// New settings; the worker settles their model and answers with Msg::ModelSettled.
    Apply(Box<Settings>),
    Clear,
    /// Starts a fresh conversation with this prompt.
    Fresh(String),
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
    /// Writes the plan mode the UI changed to the session.
    SavePlan,
    SaveSpent(Spent),
    /// Starts a turn from follow-ups that arrived while idle.
    FollowUps,
    RewindPoints,
    /// Drops the last prompt if the model has not answered it.
    Retract,
    /// Drops the prompt at message `at` and everything after it; `entry` passes through to `Msg::Rewound`.
    Rewind {
        at: usize,
        entry: Option<usize>,
    },
    Changes,
    Context,
}

const MENU_ROWS: [usize; 5] = [4, 6, 8, 10, 12];
const PANEL_ROWS: usize = 8;
const TASK_NAME_WIDTH: usize = 48;
const PLAN_OPTIONS: [&str; 3] = ["Start fresh and implement", "Export to a _PLAN.md file", "Keep planning"];
const PLAN_FRESH_ROWS: [&str; 3] = ["Model", "Effort", "Start"];
const EDITOR_ROWS: usize = 10;
const SCROLL_LINES: [usize; 5] = [1, 2, 3, 5, 10];
const ALT: &str = if cfg!(target_os = "macos") { "Option" } else { "Alt" };

const HOTKEYS: [(&str, &[(&str, &str)]); 4] = [
    (
        "Send",
        &[
            ("Enter", "send; while working, steers the turn or waits for it to end, per Messages while working"),
            (if cfg!(target_os = "macos") { "Option+Enter" } else { "Alt+Enter" }, "while working, send the other way; otherwise a new line"),
            ("Shift+Enter", "new line (also Ctrl+J)"),
            ("Tab", "complete a /command or /skill, or an @path"),
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
            ("Ctrl+P", "cycle the models pinned with Tab in /model"),
            ("PageUp, PageDown", "scroll the transcript"),
            (if cfg!(target_os = "macos") { "Option+Down" } else { "Alt+Down" }, "jump to the latest message"),
            ("Down", "walk the shells and agents pills (Left/Right between them) and workflows, Enter opens one (empty editor)"),
        ],
    ),
    (
        "Stop",
        &[
            ("Esc", "abort the running turn; before any answer, take the prompt back into the editor"),
            ("Esc Esc", "rewind to an earlier prompt and its files (idle)"),
            ("Ctrl+C Ctrl+C", "quit"),
            ("Ctrl+D", "quit on an empty editor"),
        ],
    ),
];

enum Item {
    Lines(Vec<Line>),
    /// fx's system line: `* source: text`, for agent, command and extension messages.
    System {
        source: String,
        text: String,
        tone: Tone,
    },
    /// A foreground subagent's end: status, label, tool calls, time, tokens.
    Subagent(Arc<Run>),
    Error(String),
    User(String),
    /// The `[Image #n]` tags of the prompt above, drawn right under it without a blank line.
    Images(Vec<String>),
    Assistant {
        text: String,
        thinking: String,
    },
    Tool {
        id: String,
        name: String,
        input: Value,
        result: Option<(String, bool)>,
        /// The running call's last ToolProgress, drawn under its row.
        progress: Option<String>,
        /// When the call began, for the seconds on its running row; None for a call replayed from history.
        started: Option<Instant>,
    },
    /// Claude Code's `Update Todos`: the whole list after a Todo call.
    Todos(Vec<Task>),
    /// fx's line after each answer: elapsed time, the prompt's uncached input and output tokens, and the session's total time.
    Done {
        took: Duration,
        usage: Usage,
        total: Duration,
    },
}

/// How loud a system line is: Info stays dim, Warn and Error take a mark and a color.
#[derive(Clone, Copy)]
enum Tone {
    Info,
    Warn,
    Error,
}

/// A transcript item with its rendered lines cached for one (width, expanded) pair.
struct Entry {
    item: Item,
    cache: Option<(usize, bool, Vec<Line>)>,
}

#[derive(Default)]
struct Git {
    branch: Option<String>,
    changed: usize,
    /// Commits to push and to pull against the upstream, as of the last fetch.
    ahead: usize,
    behind: usize,
    /// The main checkout, set only when the session runs in a linked worktree.
    main: Option<Worktree>,
    /// Worktrees of this repository besides the one the session runs in.
    others: usize,
}

/// How a copy confirms itself: a passing hint, or a transcript line.
enum Copied {
    Hint(String),
    Line(String),
}

struct App {
    entries: Vec<Entry>,
    editor: Editor,
    commands: Sender<Command>,
    working: Option<Instant>,
    /// Esc came before any answer, so the turn's end puts the prompt back into the editor.
    retract: bool,
    recording: Option<voice::Recording>,
    transcribing: Option<Instant>,
    /// When the last turn ended, for the flash on its done row.
    turn_ended: Option<Instant>,
    /// The attached terminals, each drawn the frame sized to the smallest of them.
    clients: Vec<server::Client>,
    /// The client the last input came from, which /detach lets go.
    typing_client: Option<u64>,
    /// A page published while no terminal was attached, for the next one that attaches to open.
    pending_page: Option<String>,
    /// The frame size of the last draw, for mouse events and layout checks outside `frame`.
    size: (usize, usize),
    /// The OSC 0 sequence of the current title, sent again to each terminal that attaches.
    title: String,
    /// Some inside a cmux terminal, whose sidebar shows the turn's state.
    cmux: Option<cmux::Cmux>,
    /// How the last turn ended, reported to Rex once nothing is running or waiting.
    outcome: notify::Status,
    /// The status Rex last got, so each frame sends only a change.
    status_sent: Option<notify::Status>,
    prompt_usage: Usage,
    /// Answer and thinking chars streamed since the last request's usage arrived, for the live token estimate.
    streamed_chars: usize,
    /// The token count the working line shows, counting up to the estimate a step per frame.
    shown_output: u64,
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
    /// The file `session_usage` was read from and how far, so a turn reads only what it appended.
    session_usage_read: (PathBuf, u64),
    cwd: PathBuf,
    /// The path menu's last listing and the cwd and query it answers.
    path_matches: std::cell::RefCell<(String, Vec<(String, String)>)>,
    git: Git,
    /// When the next background git read starts; None while one runs.
    git_due: Option<Instant>,
    plugins: crate::plugins::Plugins,
    mcp: crate::mcp::Mcp,
    steering: crate::agent::Steering,
    /// Slash commands, `!bash` and prompts with pasted images typed while the agent works, with those images; they run one by one once it is idle.
    queued_commands: VecDeque<(String, Vec<Value>)>,
    /// Worker commands picked mid-turn, like a worktree or session from a panel; the worker is busy until the turn ends.
    deferred_commands: VecDeque<Command>,
    abort: Arc<AtomicBool>,
    permission_mode: Arc<Mutex<Mode>>,
    hint: Option<(String, Instant)>,
    /// Shown once the terminal confirms the copy, so a clipboard that failed never reads as copied.
    pending_copy: Option<Copied>,
    /// Image blocks pasted into the editor; `[Image #n]` in the text refers to `images[n - 1]`.
    images: Vec<Value>,
    quit: bool,
    skills: Vec<Skill>,
    templates: Vec<Template>,
    /// Saved and bundled workflows, each run as `/<name>`.
    saved_workflows: Vec<workflow::Saved>,
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
    layout: Layout,
    input_selection: Selection,
    /// The last left press landed on the editor, so drags and the release belong to `input_selection`.
    selecting_input: bool,
    todos: Arc<Mutex<Todos>>,
    goal: Arc<Mutex<Option<Goal>>>,
    plan: Arc<Mutex<Plan>>,
    extra_dirs: Vec<PathBuf>,
    follow_ups: Arc<Mutex<Vec<FollowUp>>>,
    terminals: Arc<Mutex<Terminals>>,
    crons: Arc<Mutex<crate::tools::cron::Crons>>,
    subagents: Arc<Mutex<Runs>>,
    workflows: Arc<Mutex<Vec<Arc<Workflow>>>>,
    viewed: Option<RunView>,
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
    /// The directory a remote start asked for, which is not on this computer, so the session started in home.
    pub missing_cwd: Option<PathBuf>,
    /// Why `--on auto` started the session on this computer.
    pub placed: Option<String>,
}

/// Runs the session for the terminals that attach on `socket`, until one quits or it idles unattached past
/// `unload_idle_sessions_after_minutes`. `model_lists` is the startup load of the account model lists; the worker
/// settles the model on it before its first command.
pub fn run(agent: Agent, setup: Setup, model_lists: JoinHandle<Vec<String>>, socket: &Path) -> Result<(), String> {
    let prompt = setup.prompt.clone();
    let pick_session = setup.pick_session;
    let (events, inbox) = mpsc::channel();
    let (commands, command_inbox) = mpsc::channel();
    let mut app = App::new(&agent, setup, commands, events.clone());
    app.read_session_usage();
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
    if app.settings.headroom && app.settings.headroom_url.is_empty() && crate::process::installed("headroom") {
        spawn_headroom_sync(app.home.clone(), app.settings.headroom_env.clone(), events.clone());
    }
    server::listen(socket, events)?;
    app.set_title(&app.saved_title(&app.session_id).unwrap_or_else(|| unnamed_title(&app.cwd)));
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // `panic = "abort"` skips Drop, so ffmpeg and bg terminals in their own sessions would outlive the agent.
        crate::process::hang_up_children();
        previous_hook(info);
    }));
    if let Some(prompt) = prompt {
        app.submit(prompt, false);
    }
    if pick_session {
        app.open_resume();
    }
    app.offer_trust();
    let result = event_loop(&mut app, &inbox);
    app.show_progress(false);
    if let Some(cmux) = app.cmux.take() {
        cmux.finish();
    }
    app.terminals.lock().unwrap().kill_all();
    app.subagents.lock().unwrap().stop_all();
    crate::tools::remove_spills();
    if let Some(recording) = app.recording.take() {
        recording.cancel();
    }
    let summary = match &app.session_path {
        Some(path) if std::fs::read_to_string(path).is_ok_and(|text| text.contains(r#""type":"message""#)) => exit_summary(&app, Path::new(path)),
        _ => String::new(),
    };
    for client in std::mem::take(&mut app.clients) {
        client.finish(&summary);
    }
    let _ = std::fs::remove_file(socket);
    result
}

/// Printed to the normal screen after the TUI leaves, so it stays in the scrollback; mirrors `banner`.
fn exit_summary(app: &App, session_path: &Path) -> String {
    const DIM: &str = "\x1b[38;5;240m";
    const MUTED: &str = "\x1b[38;5;245m";
    const RESET: &str = "\x1b[0m";
    let mut text = format!("\n  {DIM}░▒▓{RESET}\x1b[1m█▶ agent{RESET}   {MUTED}session saved{RESET}\n\n");
    if let Some(title) = app.saved_title(&app.session_id) {
        let title: String = title.chars().filter(|c| !c.is_control()).collect();
        text += &format!("  {MUTED}title {RESET}  {title}\n");
    }
    let totals = usage::session_totals(session_path);
    if let Some(cost) = usage::total_cost(&totals, &app.client.catalog()) {
        let mut sum = Usage::default();
        for total in &totals {
            sum.add(total.usage());
        }
        text += &format!("  {MUTED}cost  {RESET}  {} {MUTED}· {}{RESET}\n", usage::dollars(cost), usage::brief(sum));
    }
    text += &format!("  {MUTED}resume{RESET}  \x1b[38;5;75magent --resume {}{RESET}\n\n", app.session_id);
    text
}

fn enter(out: &mut Stdout) -> std::io::Result<()> {
    terminal::enable_raw_mode()?;
    // Keys typed before raw mode sit in the tty cooked, Enter as the LF raw mode reads as Ctrl+J, so they go.
    // Fails harmlessly with ENOTTY when stdin is a pipe.
    #[cfg(unix)]
    unsafe {
        libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH)
    };
    execute!(out, EnterAlternateScreen, EnableBracketedPaste, EnableMouseCapture, EnableFocusChange)?;
    // crossterm 0.29 answers Unsupported on Windows, whose console API reports keys without escape codes anyway.
    #[cfg(unix)]
    execute!(out, PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES))?;
    Ok(())
}

pub(crate) fn leave(out: &mut Stdout) {
    #[cfg(unix)]
    let _ = execute!(out, PopKeyboardEnhancementFlags);
    let _ = execute!(out, DisableMouseCapture, DisableFocusChange, DisableBracketedPaste, LeaveAlternateScreen);
    let _ = terminal::disable_raw_mode();
}

fn event_loop(app: &mut App, inbox: &Receiver<Msg>) -> Result<(), String> {
    let mut progress_sent: Option<Instant> = None;
    let mut unattended_since: Option<Instant> = None;
    loop {
        let busy = app.working.is_some() && app.settings.progress_bar;
        if let Some(show) = progress_due(busy, progress_sent, Instant::now()) {
            progress_sent = show.then(Instant::now);
            app.show_progress(show);
        }
        app.report_to_cmux();
        app.report_to_rex();
        app.draw();
        let mut wait = if app.edge_drag.is_some() {
            EDGE_SCROLL
        } else if app.working.is_some()
            || app.hint.is_some()
            || app.transcribing.is_some()
            || app.turn_ended.is_some_and(|ended| ended.elapsed() < DONE_FLASH)
            || (matches!(app.panel, Some(Panel::Tasks { .. } | Panel::Terminal { .. })) && (app.subagents.lock().unwrap().running() > 0 || app.terminals.lock().unwrap().running() > 0))
        {
            ANIMATION_TICK
        } else {
            // A recording, a terminal, subagent or workflow that ends while idle, and a message from another session
            // queue work with nothing else to wake this loop.
            Duration::from_secs(1)
        };
        let unattended = app.clients.is_empty() && app.working.is_none() && !app.background_running();
        unattended_since = if unattended { unattended_since.or(Some(Instant::now())) } else { None };
        let unload_after = Duration::from_secs(app.settings.unload_idle_sessions_after_minutes.saturating_mul(60));
        if let Some(since) = unattended_since
            && !unload_after.is_zero()
        {
            let left = unload_after.saturating_sub(since.elapsed());
            if left.is_zero() {
                return Ok(());
            }
            wait = wait.min(left);
        }
        match inbox.recv_timeout(wait) {
            Ok(msg) => {
                app.handle(msg);
                while let Ok(msg) = inbox.try_recv() {
                    app.handle(msg);
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
        if !app.clients.is_empty() && app.git_due.is_some_and(|due| Instant::now() >= due) {
            app.git_due = None;
            let (cwd, events) = (app.cwd.clone(), app.events.clone());
            std::thread::spawn(move || {
                let started = Instant::now();
                let git = git_info(&cwd);
                let _ = events.send(Msg::Git(cwd, git, started.elapsed()));
            });
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
            app.submit(text, false);
            app.images = pasted;
        }
        if app.quit {
            return Ok(());
        }
    }
}

impl App {
    fn new(agent: &Agent, setup: Setup, commands: Sender<Command>, events: Sender<Msg>) -> App {
        let Setup { home, settings, modes_error, plugin_updates, missing_cwd, placed, .. } = setup;
        let mut recent_prompts: Vec<String> = history::load(&home.join(".agents")).into_iter().map(|entry| entry.prompt).collect();
        recent_prompts.drain(..recent_prompts.len().saturating_sub(100));
        let slash_commands: Vec<(String, String)> = BUILTIN_COMMANDS.iter().map(|(name, _, description)| (name.to_string(), description.to_string())).collect();
        let mut app = App {
            entries: Vec::new(),
            editor: Editor::with_history(recent_prompts),
            commands,
            working: None,
            retract: false,
            recording: None,
            transcribing: None,
            turn_ended: None,
            clients: Vec::new(),
            typing_client: None,
            pending_page: None,
            size: (0, 0),
            title: String::new(),
            cmux: cmux::Cmux::detect(),
            outcome: notify::Status { state: "idle", kind: None, msg: String::new() },
            status_sent: None,
            prompt_usage: Usage::default(),
            streamed_chars: 0,
            shown_output: 0,
            spent: agent.spent,
            expanded: settings.expand_tools,
            scroll: 0,
            home,
            settings,
            context_tokens: 0,
            last_usage: Usage::default(),
            session_usage: Vec::new(),
            session_usage_read: Default::default(),
            git: Git::default(),
            git_due: Some(Instant::now()),
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
            pending_copy: None,
            images: Vec::new(),
            quit: false,
            skills: Vec::new(),
            templates: Vec::new(),
            saved_workflows: Vec::new(),
            slash_commands,
            menu_index: 0,
            header_len: 0,
            session_id: agent.session.id.clone(),
            session_path: agent.session.display_path(),
            sessions_dir: agent.sessions_dir.clone(),
            panel: None,
            selection: Selection::default(),
            edge_drag: None,
            layout: Layout::default(),
            input_selection: Selection::default(),
            selecting_input: false,
            todos: agent.todos.clone(),
            goal: agent.goal.clone(),
            plan: agent.plan.clone(),
            extra_dirs: agent.extra_dirs.clone(),
            follow_ups: agent.follow_ups.clone(),
            terminals: agent.terminals.clone(),
            crons: agent.crons.clone(),
            subagents: agent.subagents.clone(),
            workflows: agent.workflows.clone(),
            viewed: None,
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
        if let Some(missing) = missing_cwd {
            let notice =
                format!("{} is not on this computer, so this session started in {}. Move it with !cd <dir>, then /worktree to pick a branch or worktree.", missing.display(), app.cwd.display());
            app.system("session", notice);
        }
        if let Some(placed) = placed {
            app.system("session", placed);
        }
        app.header_len = app.entries.len();
        app.push_history(&agent.history.messages);
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

    /// A Todo call becomes the list after it; a run of such calls keeps only the last list.
    fn show_todos(&mut self, index: usize) {
        let tasks = self.todos.lock().unwrap().visible().cloned().collect();
        self.entries[index] = Entry { item: Item::Todos(tasks), cache: None };
        if index > self.header_len && matches!(self.entries[index - 1].item, Item::Todos(_)) {
            self.entries.remove(index - 1);
        }
    }

    fn system(&mut self, source: &str, text: impl Into<String>) {
        self.push(Item::System { source: source.to_string(), text: text.into(), tone: Tone::Info });
    }

    fn warn(&mut self, source: &str, text: impl Into<String>) {
        self.push(Item::System { source: source.to_string(), text: text.into(), tone: Tone::Warn });
    }

    fn background_running(&self) -> bool {
        self.terminals.lock().unwrap().running() > 0 || self.subagents.lock().unwrap().running() > 0 || self.workflows.lock().unwrap().iter().any(|run| run.is_running())
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
        // Startup already refreshed the rules; a new level downloads only its missing file, behind the UI: offline that takes 10 s per file.
        if self.settings.ponytail != old.ponytail || self.settings.caveman != old.caveman {
            let (home, ponytail, caveman, events) = (self.home.clone(), self.settings.ponytail.clone(), self.settings.caveman.clone(), self.events.clone());
            std::thread::spawn(move || {
                let _ = events.send(Msg::ModesFetched(modes::fetch(&home, &ponytail, &caveman, false)));
            });
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
    fn model(&self) -> Option<Model> {
        self.client.catalog().iter().find(|m| m.id == self.settings.model).cloned()
    }

    /// Reads what the session file gained since the last read; an unsaved session has none, so its footer shows no cost.
    fn read_session_usage(&mut self) {
        let path = self.sessions_dir.join(format!("{}.jsonl", self.session_id));
        if self.session_usage_read.0 != path {
            self.session_usage = Vec::new();
            self.session_usage_read = (path, 0);
        }
        let (path, read) = &mut self.session_usage_read;
        let (new, end) = usage::session_totals_from(path, *read);
        *read = end;
        for total in new {
            match self.session_usage.iter_mut().find(|known| known.model == total.model) {
                Some(known) => known.requests.extend(total.requests),
                None => self.session_usage.push(total),
            }
        }
    }

    /// The settings key of the current model; says so when the model lists are still loading.
    fn loaded_model_key(&mut self) -> Option<String> {
        let key = self.model().as_ref().map(model_key);
        if key.is_none() {
            self.system("models", MODELS_STILL_LOADING);
        }
        key
    }

    fn context_window(&self) -> u64 {
        self.model().map_or(0, |model| self.settings.context_window(&model))
    }

    fn rtk_gain(&mut self) {
        match crate::process::command("rtk").arg("gain").output() {
            Ok(output) if output.status.success() => self.system("rtk", String::from_utf8_lossy(&output.stdout).trim_end()),
            Ok(output) => self.push(Item::Error(format!("rtk gain failed: {}", String::from_utf8_lossy(&output.stderr).trim_end()))),
            Err(error) => self.push(Item::Error(format!("Cannot run rtk: {error}"))),
        }
    }

    fn refresh_usage_in_background(&self) {
        let (client, events) = (self.client.clone(), self.events.clone());
        std::thread::spawn(move || {
            client.refresh_all_usage();
            let _ = events.send(Msg::UsageFetched);
        });
    }

    fn login_action(&mut self, action: login::Action) {
        match action {
            login::Action::None => {}
            login::Action::Close => self.panel = None,
            login::Action::Copy(url) => {
                self.copy(&url, Copied::Hint("Copied login link".into()));
            }
            login::Action::Exchange { provider, cancel, exchange } => {
                let events = self.events.clone();
                std::thread::spawn(move || {
                    let result = exchange(&cancel);
                    let _ = events.send(Msg::LoginFinished { provider, cancel, result });
                });
            }
            login::Action::StartDevice(cancel) => {
                let events = self.events.clone();
                std::thread::spawn(move || {
                    let _ = match crate::api::copilot::start() {
                        Ok(device) => events.send(Msg::CopilotDevice { cancel, device }),
                        Err(error) => events.send(Msg::LoginFinished { provider: crate::api::copilot::PROVIDER, cancel, result: Err(error) }),
                    };
                });
            }
            login::Action::Save(account) => {
                self.panel = None;
                let name = account.name.clone();
                let previous_id = self.client.accounts().into_iter().find(|saved| saved.provider == account.provider && saved.name == name).and_then(|saved| saved.account_id);
                let account_id = account.account_id().map(String::from);
                match self.client.add(account) {
                    Ok(()) => {
                        self.system("login", format!("Saved {name}."));
                        self.accounts_changed();
                    }
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

    /// Lists the models again after /login or /logout changed the accounts, so /model shows them without a restart,
    /// and has the agent apply the settings again, so ImageGenerate follows the accounts that can draw.
    fn accounts_changed(&self) {
        let (client, events) = (self.client.clone(), self.events.clone());
        std::thread::spawn(move || {
            for failure in client.load_catalog() {
                let _ = events.send(Msg::Agent(Event::Notice(format!("Model list failed: {failure}"))));
            }
        });
        let _ = self.commands.send(Command::Apply(Box::new(self.settings.clone())));
    }

    /// `None` for a session not saved yet, which has no prompt to name it by.
    fn saved_title(&self, id: &str) -> Option<String> {
        let session = Session::info(&self.sessions_dir.join(format!("{id}.jsonl")))?;
        Some(session.name.unwrap_or(session.title))
    }

    fn last_answer(&self) -> Option<String> {
        self.entries.iter().rev().find_map(|entry| match &entry.item {
            Item::Assistant { text, .. } if !text.trim().is_empty() => Some(text.clone()),
            _ => None,
        })
    }
}

impl Entry {
    fn lines(&mut self, width: usize, expanded: bool, cwd: &Path) -> &Vec<Line> {
        let fresh = matches!(&self.cache, Some((w, e, _)) if *w == width && *e == expanded);
        if !fresh {
            self.cache = Some((width, expanded, render_item(&self.item, width, expanded, cwd)));
        }
        &self.cache.as_ref().unwrap().2
    }
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
fn cwd_sequence(cwd: &Path) -> String {
    // Control characters too: an ESC or BEL in a directory name would end the sequence early.
    let mut url = String::new();
    for c in cwd.display().to_string().chars() {
        if c == '%' || c == ' ' || c.is_control() {
            url.push_str(&format!("%{:02X}", c as u32));
        } else {
            url.push(c);
        }
    }
    format!("\x1b]7;file://{url}\x1b\\")
}

/// Ghostty drops a progress bar that sees no update for 15 s, so a long turn resends it.
const PROGRESS_REFRESH: Duration = Duration::from_secs(5);

/// The progress state to send now, if any: `sent` is when "busy" last went out, None while the bar is cleared.
fn progress_due(busy: bool, sent: Option<Instant>, now: Instant) -> Option<bool> {
    match sent {
        None => busy.then_some(true),
        Some(_) if !busy => Some(false),
        Some(at) => (now.duration_since(at) >= PROGRESS_REFRESH).then_some(true),
    }
}

fn unnamed_title(cwd: &Path) -> String {
    let project = cwd.file_name().map(|name| name.to_string_lossy()).unwrap_or_default();
    format!("{project} | agent")
}

const ULTRACODE_NOTE: &str = "The user typed the keyword `ultracode`, a direct request for a workflow: write a Workflow script for this task and run it with the Workflow tool.";

/// Only a prompt the user typed counts; the keyword in a file or tool output starts nothing.
fn mentions_ultracode(text: &str) -> bool {
    text.split(|c: char| !c.is_alphanumeric()).any(|word| word.eq_ignore_ascii_case("ultracode"))
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
/// Whether anything but notices came after the last prompt on screen.
fn answered(entries: &[Entry]) -> bool {
    let mut since_prompt = entries.iter().rev().take_while(|entry| !matches!(entry.item, Item::User(_)));
    !since_prompt.all(|entry| matches!(entry.item, Item::System { .. } | Item::Images(_)))
}

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

fn git_info(cwd: &Path) -> Git {
    let run = |args: &[&str]| crate::process::command("git").args(args).current_dir(cwd).stderr(std::process::Stdio::null()).output().ok().filter(|o| o.status.success());
    let branch = run(&["branch", "--show-current"]).map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).map(|b| if b.is_empty() { "detached".to_string() } else { b });
    let status = run(&["status", "--porcelain=v2", "--branch", "--untracked-files=all"]).map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    let (changed, ahead, behind) = status_counts(&status);
    let (worktrees, current) = worktree::list(cwd).unwrap_or_default();
    let others = worktrees.len().saturating_sub(1);
    let main = worktrees.into_iter().next().filter(|main| main.path != current);
    Git { branch, changed, ahead, behind, main, others }
}

/// (changed files, commits ahead, commits behind) from `git status --porcelain=v2 --branch`.
fn status_counts(status: &str) -> (usize, usize, usize) {
    let (mut changed, mut ahead, mut behind) = (0, 0, 0);
    for line in status.lines() {
        if let Some(counts) = line.strip_prefix("# branch.ab ") {
            for count in counts.split(' ') {
                if let Some(n) = count.strip_prefix('+') {
                    ahead = n.parse().unwrap_or(0);
                } else if let Some(n) = count.strip_prefix('-') {
                    behind = n.parse().unwrap_or(0);
                }
            }
        } else if !line.starts_with('#') && !line.is_empty() {
            changed += 1;
        }
    }
    (changed, ahead, behind)
}

#[cfg(test)]
mod tests;
