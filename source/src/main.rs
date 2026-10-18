mod agent;
mod api;
mod mcp;
mod plugins;
mod process;
mod settings;
mod tools;
mod tui;
mod update;
mod worktree;

use serde_json::Value;
use std::collections::HashSet;
use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use agent::permissions::{Decision, Grants, Mode};
use agent::{Agent, Event, History, Session, Spent, condense, modes, prompt, snapshots, usage};
use api::{Client, Usage};
use tools::hashline::Anchors;
use tools::rtk;

const USAGE: &str = "Usage: agent [options] [message...]
       agent plugin install <name|path>          Copy a bundled plugin or a plugin directory into ~/.agents/plugins
       agent plugin remove <name>
       agent plugin list

Options:
  --model <id>          Model id, e.g. claude-opus-5-5
  --thinking <level>    off, minimal, low, medium, high, xhigh, max
  -p, --print           Run the message, print the answer, exit
  --json                Like --print, then print one JSON summary
  -c, --continue        Continue the latest session in this directory
  --resume <id>         Open a session by id or id prefix
  --no-session          Do not save the session
  --auto                Let a reviewer model approve tool calls
  --full-access, --yolo Run every tool call without asking
  --ask                 Ask before commands, file edits, subagents, MCP and plugin tools
  --headless            Run the message, print JSONL events (tool, session, done, error)
  --parent <id>         Run as a subagent of that session: linked to it, no load_tools or ask_user_question
  --account <p>=<name>  Use that saved account first for provider p, like a pin in /account
  --version             Print the version
  -h, --help            Print this help";

struct Options {
    model: Option<String>,
    thinking: Option<String>,
    permission_mode: Option<Mode>,
    print: bool,
    json: bool,
    headless: bool,
    parent: Option<String>,
    /// Provider and account name the parent uses, pinned for this run only.
    account: Option<(String, String)>,
    save: bool,
    resume: Option<Resume>,
    prompt: Option<String>,
}

enum Resume {
    Latest,
    Id(String),
}

fn plugin_command(args: &[String]) -> Result<(), String> {
    let home = PathBuf::from(std::env::var("HOME").map_err(|_| "HOME is not set.")?);
    match args {
        [action, source] if action == "install" => println!("Installed {}.", plugins::install(&home, source)?),
        [action, name] if action == "remove" => {
            plugins::remove(&home, name)?;
            println!("Removed {name}.");
        }
        [action] if action == "list" => {
            let disabled = settings::load(&home)?.disabled_plugins;
            for view in plugins::Plugins::new(&home).list() {
                let state = match (view.installed, disabled.contains(&view.name)) {
                    (false, _) => "available",
                    (true, true) => "disabled",
                    (true, false) => "enabled",
                };
                println!("{:<16} {:<10} {:<10} {}", view.name, view.version, state, view.description);
            }
        }
        _ => return Err(format!("Unknown plugin command.\n{USAGE}")),
    }
    Ok(())
}

fn main() {
    process::exit_on_signal(clean_up_on_signal);
    // An earlier agent with this pid that crashed left files that the spill and bg log `create_new` would refuse.
    tools::remove_spills();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|command| command == "plugin") {
        if let Err(message) = plugin_command(&args[1..]) {
            eprintln!("error: {message}");
            std::process::exit(1);
        }
        return;
    }
    let options = match parse_args(args) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("{message}\n{USAGE}");
            std::process::exit(2);
        }
    };
    if let Err(message) = run(options) {
        eprintln!("error: {message}");
        std::process::exit(1);
    }
}

fn parse_args(args: Vec<String>) -> Result<Options, String> {
    let mut options = Options { model: None, thinking: None, permission_mode: None, print: false, json: false, headless: false, parent: None, account: None, save: true, resume: None, prompt: None };
    let mut words = Vec::new();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or(format!("{name} needs a value."));
        match arg.as_str() {
            "--model" => options.model = Some(value("--model")?),
            "--thinking" => {
                let level = value("--thinking")?;
                if !tui::THINKING_LEVELS.contains(&level.as_str()) {
                    return Err(format!("--thinking takes one of: {}.", tui::THINKING_LEVELS.join(", ")));
                }
                options.thinking = Some(level);
            }
            "--resume" => options.resume = Some(Resume::Id(value("--resume")?)),
            "-c" | "--continue" => options.resume = Some(Resume::Latest),
            "-p" | "--print" => options.print = true,
            "--json" => options.json = true,
            "--headless" => options.headless = true,
            "--parent" => options.parent = Some(value("--parent")?),
            "--account" => {
                let account = value("--account")?;
                let (provider, name) = account.split_once('=').ok_or("--account needs provider=name.")?;
                options.account = Some((provider.into(), name.into()));
            }
            "--no-session" => options.save = false,
            "--auto" => options.permission_mode = Some(Mode::Auto),
            "--full-access" | "--yolo" => options.permission_mode = Some(Mode::FullAccess),
            "--ask" => options.permission_mode = Some(Mode::Ask),
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            "--version" => {
                println!("agent {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "--" => words.extend(args.by_ref()),
            flag if flag.starts_with('-') => return Err(format!("Unknown option {flag}.")),
            _ => words.push(arg),
        }
    }
    if !words.is_empty() {
        options.prompt = Some(words.join(" "));
    }
    Ok(options)
}

fn run(options: Options) -> Result<(), String> {
    let home = PathBuf::from(std::env::var("HOME").map_err(|_| "HOME is not set.")?);
    let agent_dir = home.join(".agents");
    // The flags below apply to this run only; /settings saves only the keys it changes.
    let mut settings = settings::load(&home)?;
    if let Some(model) = options.model {
        settings.model = model;
    }
    if let Some(thinking) = options.thinking {
        settings.thinking = thinking;
    }
    if let Some(mode) = options.permission_mode {
        settings.permission_mode = mode;
    }
    if let Some((provider, name)) = options.account {
        settings.default_accounts.insert(provider, name);
    }
    let client = Client::new(&home)?;
    client.configure(&settings);
    // About 0.5 s of network; the TUI opens meanwhile and settle_model waits for it.
    let model_lists = std::thread::spawn({
        let client = client.clone();
        move || client.load_catalog()
    });
    // Downloads only a missing file; the refresh of cached ones runs behind it, so startup never waits on it.
    let modes_error = modes::fetch(&home, &settings.ponytail, &settings.caveman, false).err();
    // Subagents use the copy their parent refreshed seconds earlier.
    if options.parent.is_none() {
        std::thread::spawn({
            let (home, ponytail, caveman) = (home.clone(), settings.ponytail.clone(), settings.caveman.clone());
            move || modes::fetch(&home, &ponytail, &caveman, true)
        });
    }
    let cwd = std::env::current_dir().map_err(|e| format!("Cannot read the working directory: {e}"))?;
    let sessions_dir = home.join(".agents/sessions");
    let (session, history) = match &options.resume {
        Some(Resume::Id(id)) => Session::resume(&sessions_dir, id)?,
        Some(Resume::Latest) => Session::resume(&sessions_dir, &Session::latest(&sessions_dir, &cwd)?)?,
        None => (Session::start(&sessions_dir, options.save, &cwd, &settings.model)?, History::default()),
    };
    if let Some(max_age) = settings.prune_sessions_after() {
        Session::prune(&sessions_dir, max_age);
    }
    if let Some(parent) = &options.parent {
        session.append(&serde_json::json!({"type": "parent", "session": parent}))?;
    }
    // Before agent.plugins.sync starts the processes, so they run the updated code.
    let plugin_updates = plugins::update_bundled(&home);
    let mut agent = Agent {
        client,
        system: prompt::system_prompt(&agent_dir, &cwd, &settings.disabled_skills),
        modes: modes::prompt(&home, &settings.ponytail, &settings.caveman),
        // agent.load below fills it, with the swarm prompt for the session's loaded groups.
        tools: Value::Null,
        anchors: Anchors::new(&session.id),
        rtk: if settings.rtk { rtk::Rtk::ON } else { rtk::Rtk::OFF },
        nested_instructions: HashSet::new(),
        shell_context: Vec::new(),
        images: Vec::new(),
        model: settings.model.clone(),
        effort: settings.thinking.clone(),
        // settle_model fills these three and condense once the model lists arrive.
        context_window: 0,
        max_tokens: 0,
        cheap_model: String::new(),
        auto_compact: settings.auto_compact,
        reserve_tokens: settings.reserve_tokens,
        keep_recent_tokens: settings.keep_recent_tokens,
        last_context_tokens: 0,
        condense: condense::Config::new(&settings, ""),
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
        terminals: Arc::default(),
        web: tools::web::Web::new(&home, std::env::temp_dir().join(format!("agent-web-{}", std::process::id()))),
        subagents: Arc::default(),
        subagent_concurrency: settings.subagent_concurrency,
        subagent_effort: settings.subagent_effort.clone(),
        loaded_groups: Vec::new(),
        extra_dirs: Vec::new(),
        spent: Spent::default(),
        interactive: false,
        agent_dir: agent_dir.clone(),
        home: home.clone(),
        plugins: plugins::Plugins::new(&home),
        mcp: mcp::Mcp::new(&home, &cwd),
        snapshots: None,
        lsp: agent::lsp::Lsp::new(settings.lsp.clone()),
        checkpoints: Vec::new(),
        cwd,
        messages: Vec::new(),
        session,
        sessions_dir,
    };
    agent.plugins.sync(&settings.disabled_plugins, &settings.plugin_settings);
    agent.mcp.sync(&settings.disabled_mcp, &settings.approved_mcp);
    agent.load(history);
    let mut prompt = options.prompt;
    if !std::io::stdin().is_terminal() {
        let mut piped = String::new();
        std::io::stdin().read_to_string(&mut piped).map_err(|e| format!("Cannot read stdin: {e}"))?;
        if !piped.trim().is_empty() {
            prompt = Some(match prompt {
                Some(words) => format!("{words}\n\n{}", piped.trim_end()),
                None => piped.trim_end().to_string(),
            });
        }
    }
    if options.headless || options.print || options.json || !std::io::stdout().is_terminal() {
        if let Some(error) = &modes_error {
            eprintln!("{error}");
        }
        agent.mcp.settled();
        let (servers, mcp_error) = agent.mcp.list();
        if let Some(error) = mcp_error {
            eprintln!("{error}");
        }
        for server in servers {
            if let mcp::Status::Failed(error) = server.status {
                eprintln!("MCP server {}: {error}", server.name);
            }
        }
        if let Some(notice) = agent.session.fork_notice() {
            eprintln!("{notice}");
        }
        for notice in &plugin_updates {
            eprintln!("{notice}");
        }
        for notice in model_lists.join().expect("the model lists thread") {
            eprintln!("{notice}");
        }
        if let Some(notice) = agent.settle_model(&mut settings)? {
            eprintln!("{notice}");
        }
        agent.announce_session();
        // After settle_model, which resets the tools to the session's loaded groups. Only the root spawns, and a child has no UI to ask in.
        if options.parent.is_some() {
            agent.tools = Value::Array(agent.tools.as_array().into_iter().flatten().filter(|tool| !matches!(tool["name"].as_str(), Some("load_tools" | "ask_user_question"))).cloned().collect());
        }
        let result =
            if options.headless { headless_mode(&mut agent, prompt.as_deref().unwrap_or_default()) } else { print_mode(&mut agent, &prompt.ok_or("Print mode needs a message.")?, options.json) };
        agent.terminals.lock().unwrap().kill_all();
        agent.subagents.lock().unwrap().stop_all();
        tools::remove_spills();
        return result;
    }
    // All skills: /skills lists the disabled ones too, the TUI filters by settings.disabled_skills.
    let skills = prompt::load_skills(&agent_dir, &agent.cwd, &[]);
    let templates = prompt::load_templates(&agent_dir, &agent.cwd);
    agent.interactive = true;
    agent.snapshots = snapshots::Snapshots::open(&home, &agent.cwd);
    tui::run(agent, tui::Setup { home, settings, skills, templates, prompt, modes_error, plugin_updates }, model_lists)
}

/// The panic hook's terminal restore does not run when a signal ends the process, so a `kill` would leave the shell in raw mode.
fn clean_up_on_signal() {
    tools::remove_spills();
    if std::io::stdout().is_terminal() {
        tui::leave(&mut std::io::stdout());
    }
}

/// Non-interactive run: streams text to stdout (or nothing with --json) and progress to stderr.
fn print_mode(agent: &mut Agent, prompt: &str, json: bool) -> Result<(), String> {
    let answer = agent.prompt(prompt, &mut |event| match event {
        Event::Text(text) if !json => {
            print!("{}", terminal_safe(&text));
            let _ = std::io::stdout().flush();
        }
        Event::ToolStart { name, input, .. } if !json => eprintln!("· {name} {}", preview(&input)),
        Event::Notice(text) => eprintln!("{}", terminal_safe(&text)),
        Event::Permission { request, reply } => {
            eprintln!("Denied {}: {} (print mode cannot ask; use --auto or --full-access)", request.kind.label(), terminal_safe(&request.detail));
            let _ = reply.send(Decision::Deny);
        }
        Event::Reviewed { allowed, detail, reason } => eprintln!("auto: {} {}: {}", if allowed { "allowed" } else { "denied" }, terminal_safe(&detail), terminal_safe(&reason)),
        Event::TurnEnd { .. } if !json => println!(),
        _ => {}
    })?;
    if json {
        println!("{}", agent.summary(&answer));
    }
    Ok(())
}

/// Child-process mode: one JSON object per line on stdout, which the parent's subagent tool reads.
fn headless_mode(agent: &mut Agent, prompt: &str) -> Result<(), String> {
    let line = |event: Value| {
        println!("{event}");
        let _ = std::io::stdout().flush();
    };
    if prompt.trim().is_empty() {
        line(serde_json::json!({"type": "error", "message": "Headless mode needs a message."}));
        return Err("Headless mode needs a message.".into());
    }
    if let Some(path) = &agent.session.path {
        line(serde_json::json!({"type": "session", "path": path}));
    }
    let result = agent.prompt(prompt, &mut |event| match event {
        Event::ToolStart { name, .. } => line(serde_json::json!({"type": "tool", "name": name})),
        Event::Permission { reply, .. } => _ = reply.send(Decision::Deny),
        _ => {}
    });
    line(usage::entry(&agent.model, agent.usage, process::now_ms()));
    match result {
        Ok(answer) => line(serde_json::json!({"type": "done", "answer": answer})),
        Err(message) => line(serde_json::json!({"type": "error", "message": message})),
    }
    Ok(())
}

fn preview(input: &Value) -> String {
    let text = input.to_string();
    match text.char_indices().nth(160) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text,
    }
}

/// Model and tool text can carry escape sequences (OSC 52 writes the clipboard); the TUI drops them in `to_ansi`.
fn terminal_safe(text: &str) -> String {
    text.chars().filter(|c| matches!(c, '\n' | '\t') || !c.is_control()).collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn account_flag_names_a_provider_and_an_account() {
        let options = super::parse_args(vec!["--account".into(), "anthropic=work".into()]).unwrap();
        assert_eq!(options.account, Some(("anthropic".into(), "work".into())));
        assert!(super::parse_args(vec!["--account".into(), "work".into()]).is_err());
    }

    #[test]
    fn thinking_flag_takes_only_known_levels() {
        assert_eq!(super::parse_args(vec!["--thinking".into(), "low".into()]).unwrap().thinking.as_deref(), Some("low"));
        assert!(super::parse_args(vec!["--thinking".into(), "huge".into()]).is_err());
    }

    #[test]
    fn terminal_safe_drops_escape_sequences() {
        assert_eq!(super::terminal_safe("a\x1b]52;c;eA==\x07b\n\tc\u{9b}"), "a]52;c;eA==b\n\tc");
    }
}
