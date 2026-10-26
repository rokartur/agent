// settings::to_json's json! literal outgrows the default limit of 128.
#![recursion_limit = "256"]

mod acp;
mod agent;
mod api;
mod headroom;
mod image;
mod mcp;
mod plugins;
mod process;
mod settings;
mod tools;
mod tui;
mod update;
mod worktree;

use serde_json::Value;
use std::io::{BufRead, BufReader, IsTerminal, Read, Write};
use std::path::PathBuf;

use agent::permissions::{Decision, Mode};
use agent::{Agent, Event, History, Session, modes, prompt, snapshots, usage};
use api::Client;

const USAGE: &str = concat!(
    "agent v",
    env!("CARGO_PKG_VERSION"),
    r#"
Fast, native coding agent for the terminal.

agent starts an interactive session by default. Use `agent -p <message>` to run one noninteractive request.

Usage:
  agent [flags] [message...]
  agent <command> [...args]

Commands:
  plugin install <name|path>  Copy a bundled plugin or a plugin directory into ~/.agents/plugins
  plugin remove <name>        Remove an installed plugin
  plugin list                 List bundled and installed plugins
  update                      Check for a newer release now and install it
  doctor [--provider <p>]     Ping every saved account and show its limits; exits 1 if one fails
  acp                         Serve the Agent Client Protocol on stdin and stdout, for editors like Zed

Flags:
  --model <id>                Model id, e.g. claude-opus-5-5
  --thinking <level>          off, minimal, low, medium, high, xhigh, max
  -p, --print                 Run the message, print the answer and exit
  --json                      Like --print, then print one JSON summary
  --headless                  Run the message, print JSONL events (tool, session, usage, done, error)
  -c, --continue              Continue the latest session in this directory
  -r, --resume [id]           Open a session by id or id prefix, or pick one from the list
  --add-dir <path>            Also work in this directory; repeat for more, /workspace changes the list
  --no-session                Do not save the session
  --auto                      Let a reviewer model approve tool calls
  --full-access, --yolo       Run every tool call without asking
  --ask                       Ask before commands, file edits, URL fetches, subagents, MCP and plugin tools
  --parent <id>               Run as a subagent of that session: linked to it, no Agent, Task*, web, AskUserQuestion, plan or worktree tools;
                              with --headless, stdin takes the task and later messages as JSON string lines
  --explore                   With --parent: only read and rg, no modes prompt
  --web                       With --parent: keep WebSearch, WebFetch and GetSearchContent
  --trust                     Trust this project for this run: load its .agents files and run its edit hooks
  --tools <a,b,...>           With acp, -p, --json or --headless: offer only these tools
  --account <p>=<name>        Use that saved account first for provider p, like a pin in /account
  -h, --help                  Display this help and exit
  --version                   Print the agent version and exit

Examples:
  agent
      Start a fresh interactive session

  agent -p "Explain the changes in this repository"
      Run one request, print the answer and exit

  agent -c
      Continue the latest session in this directory

  agent plugin list
      Show bundled and installed plugins

Run `/help` inside an interactive session for slash commands.

Learn more about agent:  https://agent.rokartur.com
Report a problem:        https://github.com/rokartur/agent/issues"#
);

#[derive(Default)]
struct Options {
    model: Option<String>,
    thinking: Option<String>,
    permission_mode: Option<Mode>,
    print: bool,
    json: bool,
    headless: bool,
    parent: Option<String>,
    explore: bool,
    /// With --parent: keep the web tools, which Workflow agents get.
    web: bool,
    /// --trust: load this project's `.agents` files and run its edit hooks for this run only.
    trust: bool,
    /// Provider and account name the parent uses, pinned for this run only.
    account: Option<(String, String)>,
    /// `agent acp`: serve the Agent Client Protocol instead of one run.
    acp: bool,
    /// --tools: the only tools this run may call.
    tools: Option<Vec<String>>,
    save: bool,
    resume: Option<Resume>,
    prompt: Option<String>,
    /// --add-dir, repeatable: directories outside cwd the task also works in.
    add_dirs: Vec<String>,
}

enum Resume {
    Latest,
    Id(String),
    /// `--resume` without an id: a new session with the /resume list open.
    Pick,
}

fn home() -> Result<PathBuf, String> {
    std::env::var_os("HOME").map(PathBuf::from).ok_or_else(|| "HOME is not set.".into())
}

fn doctor_command(args: &[String]) -> Result<(), String> {
    let provider = match args {
        [] => None,
        [flag, provider] if flag == "--provider" && api::models::PROVIDERS.contains(&provider.as_str()) => Some(provider.as_str()),
        _ => return Err(format!("Usage: agent doctor [--provider {}].", api::models::PROVIDERS.join("|"))),
    };
    let lines = Client::new(&home()?)?.doctor(provider);
    if lines.is_empty() {
        return Err("No saved account. Use /login.".into());
    }
    for line in &lines {
        println!("{}", line.as_ref().unwrap_or_else(|failed| failed));
    }
    let failed = lines.iter().filter(|line| line.is_err()).count();
    if failed > 0 { Err(format!("{failed} of {} accounts failed.", lines.len())) } else { Ok(()) }
}

fn update_command(args: &[String]) -> Result<(), String> {
    if !args.is_empty() {
        return Err("Usage: agent update.".into());
    }
    if !update::enabled() {
        return Err("Only release builds update themselves. Rebuild from source instead.".into());
    }
    // `off` only stops the automatic check, so an explicit update with it follows stable releases.
    match update::install_now(&settings::load(&home()?)?.update_channel)? {
        Some(version) => println!("Updated to v{version}."),
        None => println!("agent v{} is up to date.", env!("CARGO_PKG_VERSION")),
    }
    Ok(())
}

fn plugin_command(args: &[String]) -> Result<(), String> {
    let home = home()?;
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
        _ => return Err(format!("Unknown plugin command.\n{}", help(std::io::stderr().is_terminal()))),
    }
    Ok(())
}

fn main() {
    process::exit_on_signal(clean_up_on_signal);
    // An earlier agent with this pid that crashed left files that the spill and bg log `create_new` would refuse.
    tools::remove_spills();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = match args.first().map(String::as_str) {
        Some("plugin") => Some(plugin_command(&args[1..])),
        Some("doctor") => Some(doctor_command(&args[1..])),
        Some("update") => Some(update_command(&args[1..])),
        _ => None,
    };
    if let Some(result) = command {
        if let Err(message) = result {
            eprintln!("error: {message}");
            std::process::exit(1);
        }
        return;
    }
    let options = match parse_args(args) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("{message}\n{}", help(std::io::stderr().is_terminal()));
            std::process::exit(2);
        }
    };
    if let Err(message) = run(options) {
        eprintln!("error: {message}");
        std::process::exit(1);
    }
}

fn parse_args(args: Vec<String>) -> Result<Options, String> {
    let acp = args.first().is_some_and(|command| command == "acp");
    let mut options = Options { save: true, acp, ..Options::default() };
    let mut words = Vec::new();
    let mut args = args.into_iter().skip(usize::from(acp)).peekable();
    while let Some(arg) = args.next() {
        // `--model --print hi` must not set the model to `--print`.
        let mut value = |name: &str| args.next().filter(|value| !value.starts_with("--")).ok_or(format!("{name} needs a value."));
        if matches!(arg.as_str(), "--resume" | "-c" | "--continue") && options.resume.is_some() {
            return Err("Pass one of -c and --resume.".into());
        }
        match arg.as_str() {
            "--model" => options.model = Some(value("--model")?),
            "--tools" => options.tools = Some(value("--tools")?.split(',').map(|name| name.trim().to_string()).filter(|name| !name.is_empty()).collect()),
            "--thinking" => {
                let level = value("--thinking")?;
                if !settings::THINKING_LEVELS.contains(&level.as_str()) {
                    return Err(format!("--thinking takes one of: {}.", settings::THINKING_LEVELS.join(", ")));
                }
                options.thinking = Some(level);
            }
            "-r" | "--resume" => {
                if options.resume.is_some() {
                    return Err("Use only one of -c and -r.".into());
                }
                options.resume = Some(match args.next_if(|next| !next.starts_with('-')) {
                    Some(id) => Resume::Id(id),
                    None => Resume::Pick,
                });
            }
            "-c" | "--continue" => {
                if options.resume.is_some() {
                    return Err("Use only one of -c and -r.".into());
                }
                options.resume = Some(Resume::Latest);
            }
            "-p" | "--print" => options.print = true,
            "--json" => options.json = true,
            "--headless" => options.headless = true,
            "--parent" => options.parent = Some(value("--parent")?),
            "--add-dir" => options.add_dirs.push(value("--add-dir")?),
            "--explore" => options.explore = true,
            "--web" => options.web = true,
            "--trust" => options.trust = true,
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
                println!("{}", help(std::io::stdout().is_terminal()));
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
    if !options.save && options.resume.is_some() {
        return Err("--no-session cannot be combined with --resume or -c.".into());
    }
    if options.headless && options.print {
        return Err("--headless cannot be combined with --print.".into());
    }
    if options.tools.is_some() && !(options.print || options.json || options.headless || options.acp) {
        return Err("--tools needs acp, -p, --json or --headless; /settings would reset the tool list.".into());
    }
    let picks_session = matches!(options.resume, Some(Resume::Pick));
    if options.acp && (options.print || options.json || options.headless || options.prompt.is_some() || picks_session) {
        return Err("acp reads its messages from the editor, so it takes no message, -p, --json, --headless or -r without an id.".into());
    }
    if picks_session && (options.print || options.json || options.headless || options.parent.is_some()) {
        return Err("--resume without an id opens the session list, so it needs the interactive mode.".into());
    }
    if picks_session && options.prompt.is_some() {
        return Err("--resume without an id opens the session list and takes no message.".into());
    }
    Ok(options)
}

fn help(styled: bool) -> String {
    if styled { styled_help(USAGE) } else { USAGE.to_string() }
}

/// Styles USAGE by the shape of each line: headings end with ':', entries split name and text on two spaces.
fn styled_help(usage: &str) -> String {
    const BOLD: &str = "\x1b[1m";
    const DIM: &str = "\x1b[2m";
    const UNDERLINE: &str = "\x1b[4m";
    const RESET: &str = "\x1b[0m";
    let mut lines = Vec::new();
    for (index, line) in usage.lines().enumerate() {
        let styled = if index == 0 {
            let (name, version) = line.split_once(' ').unwrap_or((line, ""));
            format!("{BOLD}{name}{RESET} {DIM}{version}{RESET}")
        } else if line.is_empty() {
            String::new()
        } else if let Some(text) = line.strip_prefix("      ") {
            format!("      {DIM}{text}{RESET}")
        } else if let Some(entry) = line.strip_prefix("  ") {
            match entry.split_once("  ") {
                Some((name, rest)) => {
                    let text = rest.trim_start();
                    let gap = &entry[name.len()..entry.len() - text.len()];
                    format!("  {name}{gap}{DIM}{text}{RESET}")
                }
                None => line.to_string(),
            }
        } else if line.ends_with(':') {
            format!("{BOLD}{line}{RESET}")
        } else if let Some((label, rest)) = line.split_once(":  ") {
            let url = rest.trim_start();
            let gap = &line[label.len() + 1..line.len() - url.len()];
            format!("{BOLD}{label}:{RESET}{gap}{UNDERLINE}{url}{RESET}")
        } else {
            format!("{DIM}{line}{RESET}")
        };
        lines.push(styled);
    }
    lines.join("\n")
}

fn run(options: Options) -> Result<(), String> {
    // Piped stdin would become a message, and without a terminal on stdout print mode runs instead of the TUI.
    if matches!(options.resume, Some(Resume::Pick)) && !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
        return Err("--resume without an id opens the session list, so it needs a terminal.".into());
    }
    let home = home()?;
    // The flags below apply to this run only; /settings saves only the keys it changes.
    let mut settings = settings::load(&home)?;
    settings.reset_to_defaults();
    if let Some(model) = options.model {
        settings.model = model;
    }
    if let Some(thinking) = options.thinking {
        settings.thinking = thinking;
    }
    if let Some(mode) = options.permission_mode {
        settings.permission_mode = mode;
    }
    let cwd = std::env::current_dir().map_err(|e| format!("Cannot read the working directory: {e}"))?;
    if options.trust {
        settings.trusted_projects.push(prompt::project_root(&cwd).display().to_string());
    }
    let sessions_dir = home.join(".agents/sessions");
    let (session, history) = match &options.resume {
        Some(Resume::Id(id)) => Session::resume(&sessions_dir, id)?,
        Some(Resume::Latest) => Session::resume(&sessions_dir, &Session::latest(&sessions_dir, &cwd)?)?,
        Some(Resume::Pick) | None => (Session::start(&sessions_dir, options.save, &cwd, &settings.model)?, History::default()),
    };
    let client = Client::new(&home)?;
    settings.default_accounts.extend(client.account_names(&history.accounts));
    if let Some((provider, name)) = options.account {
        settings.default_accounts.insert(provider, name);
    }
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
    if let Some(max_age) = settings.prune_sessions_after() {
        Session::prune(&sessions_dir, max_age);
    }
    if let Some(parent) = options.parent.as_ref().filter(|_| options.resume.is_none()) {
        session.append(&serde_json::json!({"type": "parent", "session": parent}))?;
    }
    // Before agent.plugins.sync starts the processes, so they run the updated code.
    let plugin_updates = plugins::update_bundled(&home);
    let mut agent = Agent::new(client, &settings, home.clone(), cwd, session, sessions_dir, options.parent.clone());
    agent.plugins.sync(&settings.disabled_plugins, &settings.plugin_settings);
    agent.mcp.sync(&settings.disabled_mcp, &settings.approved_mcp);
    agent.load(history);
    for dir in &options.add_dirs {
        agent.add_dir(dir)?;
    }
    let mut prompt = options.prompt;
    if options.headless && options.parent.is_some() {
        prompt = Some(read_subagent_stdin(agent.steering.clone())?);
    // In acp mode stdin carries the protocol.
    } else if !options.acp && !std::io::stdin().is_terminal() {
        let mut piped = String::new();
        // An inherited pipe that never closes (IDE task, ssh) would otherwise hang here silently.
        if std::io::stderr().is_terminal() {
            eprintln!("Reading the message from stdin...");
        }
        std::io::stdin().read_to_string(&mut piped).map_err(|e| format!("Cannot read stdin: {e}"))?;
        if !piped.trim().is_empty() {
            prompt = Some(match prompt {
                Some(words) => format!("{words}\n\n{}", piped.trim_end()),
                None => piped.trim_end().to_string(),
            });
        }
    }
    if options.acp || options.headless || options.print || options.json || !std::io::stdout().is_terminal() {
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
        // After settle_model, which resets the tools. Only the root spawns, runs terminals and searches the web, and a child has no UI to ask in.
        if options.parent.is_some() {
            const ROOT_ONLY: [&str; 18] = [
                "Agent",
                "Workflow",
                "ListAgents",
                "WaitForAgents",
                "TaskOutput",
                "TaskStop",
                "WebSearch",
                "WebFetch",
                "GetSearchContent",
                "AskUserQuestion",
                "CronCreate",
                "CronDelete",
                "CronList",
                "ScheduleWakeup",
                "PushNotification",
                "EnterPlanMode",
                "EnterWorktree",
                "ExitWorktree",
            ];
            let web = |name: &str| options.web && matches!(name, "WebSearch" | "WebFetch" | "GetSearchContent");
            let keep = |name: &str| if options.explore { matches!(name, "Read" | "Grep") } else { !ROOT_ONLY.contains(&name) || web(name) };
            if let Some(tools) = agent.tools.as_array_mut() {
                tools.retain(|tool| keep(tool["name"].as_str().unwrap_or_default()));
                for tool in tools.iter_mut().filter(|tool| tool["name"] == "Bash") {
                    if let Some(properties) = tool["input_schema"]["properties"].as_object_mut() {
                        properties.remove("run_in_background");
                    }
                }
            }
            // An explore child only reads and reports, so the ponytail and caveman style rules would be dead weight.
            if options.explore {
                agent.modes.clear();
            }
        }
        if let Some(allowed) = &options.tools {
            agent.limit_tools(allowed)?;
        }
        let result = if options.acp {
            acp::serve(&mut agent)
        } else if options.headless {
            headless_mode(&mut agent, prompt.as_deref().unwrap_or_default())
        } else {
            print_mode(&mut agent, &prompt.ok_or("A message is needed with --print, --json or a piped stdout.")?, options.json)
        };
        agent.terminals.lock().unwrap().kill_all();
        agent.subagents.lock().unwrap().stop_all();
        tools::remove_spills();
        return result;
    }
    agent.interactive = true;
    // Without an inbox the session still works; it only cannot be messaged.
    match agent::peers::open(&home, &agent.follow_ups) {
        Ok(inbox) => {
            agent.inbox = Some(inbox);
            agent.describe_to_peers(false);
        }
        Err(error) => eprintln!("Cross-session messages are off: {error}"),
    }
    agent.snapshots = snapshots::Snapshots::open(&home, &agent.cwd);
    tui::run(agent, tui::Setup { home, settings, prompt, pick_session: matches!(options.resume, Some(Resume::Pick)), modes_error, plugin_updates }, model_lists)
}

/// The panic hook's terminal restore does not run when a signal ends the process, so a `kill` would leave the shell in raw mode.
fn clean_up_on_signal() {
    tools::remove_spills();
    let _ = std::fs::remove_dir_all(tools::web::store_dir());
    if std::io::stdout().is_terminal() {
        tui::leave(&mut std::io::stdout());
    }
}

const PRINT_BACKGROUND_CEILING: std::time::Duration = std::time::Duration::from_secs(600);

/// Non-interactive run: streams text to stdout (or nothing with --json) and progress to stderr.
fn print_mode(agent: &mut Agent, prompt: &str, json: bool) -> Result<(), String> {
    let mut on_event = |event| match event {
        Event::Text(text) if !json => {
            print!("{}", terminal_safe(&text));
            let _ = std::io::stdout().flush();
        }
        Event::ToolStart { name, input, .. } if !json => eprintln!("· {} {}", terminal_safe(&name), terminal_safe(&preview(&input))),
        Event::Notice(text) => eprintln!("{}", terminal_safe(&text)),
        Event::SubagentDone(run) => eprintln!("{}", terminal_safe(&run.summary())),
        Event::Permission { request, reply } => {
            eprintln!("Denied {}: {} (print mode cannot ask; use --auto or --full-access)", request.kind.label(), terminal_safe(&request.detail));
            let _ = reply.send(Decision::Deny);
        }
        Event::Reviewed { allowed, detail, reason } => eprintln!("auto: {} {}: {}", if allowed { "allowed" } else { "denied" }, terminal_safe(&detail), terminal_safe(&reason)),
        Event::TurnEnd { .. } if !json => println!(),
        _ => {}
    };
    let mut answer = agent.prompt(prompt, &mut on_event);
    // Background subagents and workflows report back as follow-ups; each handled one restarts the idle ceiling.
    let mut idle_since = std::time::Instant::now();
    while answer.is_ok() && agent.has_background_work() && idle_since.elapsed() < PRINT_BACKGROUND_CEILING {
        std::thread::sleep(std::time::Duration::from_millis(200));
        if agent.follow_ups.lock().unwrap().is_empty() {
            continue;
        }
        match agent.run_follow_ups(&mut on_event) {
            Ok(text) if text.is_empty() => {}
            result => answer = result,
        }
        idle_since = std::time::Instant::now();
    }
    if json {
        // A script reading stdout gets a JSON line on failure too, not an empty string.
        match &answer {
            Ok(answer) => println!("{}", agent.summary(answer)),
            Err(message) => println!("{}", serde_json::json!({"error": message})),
        }
    }
    answer.map(|_| ())
}

/// Child-process mode: one JSON object per line on stdout, which the parent's subagent tool reads.
fn headless_mode(agent: &mut Agent, prompt: &str) -> Result<(), String> {
    let line = |event: Value| {
        println!("{event}");
        let _ = std::io::stdout().flush();
    };
    if prompt.trim().is_empty() {
        let message = "Headless mode needs a message.";
        line(serde_json::json!({"type": "error", "message": message}));
        return Err(message.into());
    }
    if let Some(path) = &agent.session.path {
        line(serde_json::json!({"type": "session", "path": path}));
    }
    let result = agent.prompt(prompt, &mut |event| match event {
        Event::ToolStart { name, .. } => line(serde_json::json!({"type": "tool", "name": name})),
        Event::Outbox { to, text } => line(serde_json::json!({"type": "message", "to": to, "text": text})),
        Event::Permission { reply, .. } => _ = reply.send(Decision::Deny),
        _ => {}
    });
    line(usage::entry(&agent.model, agent.usage, process::now_ms()));
    match &result {
        Ok(answer) => line(serde_json::json!({"type": "done", "answer": answer})),
        Err(message) => line(serde_json::json!({"type": "error", "message": message})),
    }
    result.map(|_| ())
}

/// A subagent's stdin, one JSON string per line: its task, then messages the user sends while it runs, which steer it.
fn read_subagent_stdin(steering: agent::Steering) -> Result<String, String> {
    let mut lines = BufReader::new(std::io::stdin()).lines();
    let task = lines.next().ok_or("No task on stdin.")?.map_err(|e| format!("Cannot read stdin: {e}"))?;
    let task = serde_json::from_str(&task).map_err(|e| format!("The task is not a JSON string: {e}"))?;
    std::thread::spawn(move || {
        for line in lines.map_while(Result::ok) {
            if let Ok(text) = serde_json::from_str(&line) {
                steering.lock().unwrap().push((text, Vec::new()));
            }
        }
    });
    Ok(task)
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
    fn tools_flag_lists_names_and_needs_a_run_without_the_ui() {
        let args = |list: &[&str]| list.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
        let options = super::parse_args(args(&["--tools", "Read, Grep,", "-p", "hi"])).unwrap();
        assert_eq!(options.tools, Some(vec!["Read".to_string(), "Grep".to_string()]));
        assert!(super::parse_args(args(&["--tools", "Read", "hi"])).is_err());
    }

    #[test]
    fn acp_command_takes_flags_but_no_message() {
        let args = |list: &[&str]| list.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
        let options = super::parse_args(args(&["acp", "--auto", "--tools", "Read"])).unwrap();
        assert!(options.acp && options.prompt.is_none());
        assert!(super::parse_args(args(&["acp", "hi"])).is_err());
        assert!(super::parse_args(args(&["acp", "-p"])).is_err());
        assert!(!super::parse_args(args(&["-p", "acp"])).unwrap().acp);
    }

    #[test]
    fn thinking_flag_takes_only_known_levels() {
        assert_eq!(super::parse_args(vec!["--thinking".into(), "low".into()]).unwrap().thinking.as_deref(), Some("low"));
        assert!(super::parse_args(vec!["--thinking".into(), "huge".into()]).is_err());
    }

    #[test]
    fn no_session_refuses_to_reopen_a_saved_session() {
        assert!(super::parse_args(vec!["--no-session".into(), "-c".into()]).is_err());
        assert!(super::parse_args(vec!["--resume".into(), "abc".into(), "--no-session".into()]).is_err());
        assert!(super::parse_args(vec!["--no-session".into()]).is_ok());
    }

    #[test]
    fn conflicting_or_missing_flag_values_are_refused() {
        assert!(super::parse_args(vec!["-c".into(), "--resume".into(), "abc".into()]).is_err());
        assert!(super::parse_args(vec!["--model".into(), "--print".into(), "hi".into()]).is_err());
        assert!(super::parse_args(vec!["--headless".into(), "--print".into(), "hi".into()]).is_err());
    }

    #[test]
    fn resume_flag_without_an_id_picks_a_session_only_in_the_interactive_mode() {
        let parse = |args: &[&str]| super::parse_args(args.iter().map(|arg| arg.to_string()).collect());
        assert!(matches!(parse(&["-r", "ab12"]).unwrap().resume, Some(super::Resume::Id(id)) if id == "ab12"));
        assert!(matches!(parse(&["-r"]).unwrap().resume, Some(super::Resume::Pick)));
        assert!(matches!(parse(&["--resume", "--auto"]).unwrap().resume, Some(super::Resume::Pick)));
        for refused in [&["-r", "-p"][..], &["--headless", "-r"], &["-r", "--parent", "ab12"], &["hi", "-r"], &["-r", "--", "hi"], &["-c", "-r"], &["-r", "ab12", "-c"]] {
            assert!(parse(refused).is_err(), "{refused:?}");
        }
    }

    #[test]
    fn terminal_safe_drops_escape_sequences() {
        assert_eq!(super::terminal_safe("a\x1b]52;c;eA==\x07b\n\tc\u{9b}"), "a]52;c;eA==b\n\tc");
    }

    #[test]
    fn help_dims_descriptions_and_bolds_headings() {
        let styled = super::styled_help("agent v1\nFast.\n\nFlags:\n  --ask  Ask first\n  agent -c\n      Continue\nDocs:  https://x.dev");
        let lines: Vec<&str> = styled.lines().collect();
        assert_eq!(lines[0], "\x1b[1magent\x1b[0m \x1b[2mv1\x1b[0m");
        assert_eq!(lines[1], "\x1b[2mFast.\x1b[0m");
        assert_eq!(lines[3], "\x1b[1mFlags:\x1b[0m");
        assert_eq!(lines[4], "  --ask  \x1b[2mAsk first\x1b[0m");
        assert_eq!(lines[5], "  agent -c");
        assert_eq!(lines[6], "      \x1b[2mContinue\x1b[0m");
        assert_eq!(lines[7], "\x1b[1mDocs:\x1b[0m  \x1b[4mhttps://x.dev\x1b[0m");
    }
}
