use super::*;

pub(super) fn spawn_worker(mut agent: Agent, mut settings: Settings, model_lists: JoinHandle<Vec<String>>, commands: Receiver<Command>, events: Sender<Msg>) {
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
                Command::Handoff(goal) => agent.handoff_prompt(goal.as_deref()).and_then(|prompt| {
                    let _ = events.send(Msg::HandedOff(prompt.clone()));
                    start_fresh(&mut agent, &events, &prompt)
                }),
                Command::Fresh(text) => start_fresh(&mut agent, &events, &text),
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
                        // The TUI's model, effort and mode follow settings.json.
                        Ok(Opened { messages, accounts, .. }) => {
                            let path = agent.session.display_path();
                            let _ = events.send(Msg::Resumed { id: agent.session.id.clone(), path, messages, spent: agent.spent, notice: agent.session.fork_notice(), accounts });
                            let _ = events.send(Msg::Workspace(agent.extra_dirs.clone()));
                            continue;
                        }
                        Err(error) => Err(error),
                    }
                }
                Command::Context => {
                    let (split, instructions) = agent.context_split();
                    let _ = events.send(Msg::Context { split, instructions, session: agent.usage });
                    continue;
                }
                Command::Changes => {
                    let _ = events.send(Msg::Changes(agent.changes()));
                    continue;
                }
                Command::RewindPoints => {
                    let _ = events.send(Msg::RewindPoints(agent::rewind_points(&agent.history.messages)));
                    continue;
                }
                Command::Retract => {
                    let unanswered = agent::rewind_points(&agent.history.messages).pop().filter(|point| point.at + 1 == agent.history.messages.len());
                    if let Some(point) = unanswered {
                        match agent.rewind(point.at) {
                            Ok(_) => _ = events.send(Msg::Retracted(point)),
                            Err(error) => _ = events.send(Msg::Agent(Event::Notice(error))),
                        }
                    }
                    continue;
                }
                Command::Rewind { at, entry } => match agent.rewind(at) {
                    Ok(files) => {
                        let _ = events.send(Msg::Rewound { messages: agent.history.messages.clone(), files, entry });
                        continue;
                    }
                    Err(error) => Err(error),
                },
                Command::Worktree(cwd) => agent
                    .move_to(cwd, &mut |event| {
                        let _ = events.send(Msg::Agent(event));
                    })
                    .map(|()| String::new()),
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
                    let _ = events.send(Msg::Workspace(agent.extra_dirs.clone()));
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
                Command::SavePlan => {
                    if let Err(error) = agent.save_plan() {
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

pub(super) fn start_fresh(agent: &mut Agent, events: &Sender<Msg>, prompt: &str) -> Result<String, String> {
    agent.new_session()?;
    let path = agent.session.display_path();
    let _ = events.send(Msg::Agent(Event::SessionStarted { id: agent.session.id.clone(), path }));
    agent.pursue(prompt, &mut |event| {
        let _ = events.send(Msg::Agent(event));
    })
}

/// The notice /workspace shows: the result of add or remove, then the list.
pub(super) fn workspace(agent: &mut Agent, args: &str) -> String {
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

pub(super) fn settle_model(agent: &mut Agent, settings: &mut Settings, failures: Vec<String>) -> Msg {
    let model = agent.settle_model(settings).map(|swapped| (settings.model.clone(), swapped));
    Msg::ModelSettled { model, failures }
}

pub(super) fn spawn_update(home: PathBuf, channel: String, events: Sender<Msg>) {
    std::thread::spawn(move || {
        let notice = match crate::update::install_latest(&home, &channel) {
            Ok(Some(version)) => match crate::service::restart_if_installed(&home) {
                Ok(_) => format!("Updated to v{version}. Restart agent to use it."),
                Err(error) => format!("Updated to v{version}. Restart agent to use it. The listener still runs the old release: {error}"),
            },
            Ok(None) => return,
            Err(error) => format!("Update failed: {error}"),
        };
        let _ = events.send(Msg::Agent(Event::Notice(notice)));
    });
}

/// One thread per command, so a long-running entry such as a dev server does not hold back the rest.
pub(super) fn spawn_on_start(commands: Vec<String>, events: Sender<Msg>) {
    for command in commands {
        let events = events.clone();
        std::thread::spawn(move || {
            let output = crate::process::shell("sh").and_then(|sh| crate::process::command(sh).args(["-c", &command]).output().map_err(|e| e.to_string()));
            let failure = match output {
                Ok(output) if output.status.success() => return,
                Ok(output) => tail(&String::from_utf8_lossy(&output.stderr)),
                Err(e) => e,
            };
            let _ = events.send(Msg::Agent(Event::Notice(format!("on_start \"{command}\" failed: {failure}"))));
        });
    }
}

/// `install apply` restarts the proxy, so a reinstall at start can fail the first request or two.
pub(super) fn spawn_headroom_sync(home: PathBuf, env: std::collections::BTreeMap<String, String>, events: Sender<Msg>) {
    std::thread::spawn(move || {
        if let Err(error) = crate::headroom::sync(&home, &env) {
            let _ = events.send(Msg::Agent(Event::Notice(format!("headroom sync failed: {}", tail(&error)))));
        }
    });
}

pub(super) fn tail(text: &str) -> String {
    let lines: Vec<&str> = text.trim_end().lines().collect();
    lines[lines.len().saturating_sub(5)..].join("\n")
}
