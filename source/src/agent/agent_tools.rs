use super::*;

const BG_WAIT_SUBAGENTS_ONLY: &str = "WaitForAgents waits for subagents only. A background Bash task (bt-N) reports its exit as a message: keep working or end the turn, or peek with TaskOutput.";

impl Agent {
    pub(super) fn todo(&mut self, input: &Value) -> Result<String, String> {
        let (text, changed) = self.todos.lock().unwrap().apply(input)?;
        if changed {
            self.session.append(&self.todos.lock().unwrap().entry())?;
        }
        Ok(text)
    }

    /// TaskStop: `sa-N` stops a subagent, `wf-N` a workflow, anything else names a background terminal.
    pub(super) fn task_stop(&self, input: &Value) -> Result<String, String> {
        let id = input["task_id"].as_str().ok_or("TaskStop needs `task_id`.")?;
        if id.starts_with("wf-") {
            let run = self.workflows.lock().unwrap().iter().find(|run| run.id == id).cloned().ok_or_else(|| format!("No workflow {id}."))?;
            run.stop();
            return Ok(format!("Stopped {} ({}) and its agents. Relaunch with resumeFromRunId {} to reuse its finished agents.", run.id, run.name, run.run_id));
        }
        if id.starts_with("sa-") {
            let run = self.subagents.lock().unwrap().named(id).ok_or_else(|| format!("No subagent {id}."))?;
            run.stop();
            return Ok(format!("Stopped {} ({}). SendMessage resumes it once it has exited.", run.id, run.label));
        }
        // Stopping can take seconds, so the terminals lock is released before the kill.
        let terminal = self.terminals.lock().unwrap().find(id)?;
        Ok(bg::kill(&[terminal], &self.follow_ups, &self.abort))
    }

    pub(super) fn send_message(&mut self, input: &Value, emit: &mut dyn FnMut(Event)) -> Result<String, String> {
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

    pub(super) fn list_agents(&self) -> Result<String, String> {
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

    pub(super) fn bg_wait(&self, input: &Value) -> Result<String, String> {
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
    pub(super) fn subagent(&mut self, input: &Value, emit: &mut dyn FnMut(Event)) -> Result<String, String> {
        let tasks = subagent::tasks(input)?;
        // A child with an unknown model dies before its first JSONL line, so reject it here.
        if let Some(model) = tasks.iter().filter_map(|t| t.model.as_deref()).find(|m| !self.client.catalog().iter().any(|c| c.id == *m)) {
            return Err(format!("Unknown model {model}."));
        }
        let cwd = match input["cwd"].as_str() {
            Some(dir) => tools::hashline::resolve(&self.cwd, dir),
            None => self.cwd.clone(),
        };
        let on = match input["on"].as_str().filter(|on| *on != tailnet::LOCAL) {
            None => None,
            Some(on) => {
                let settings = crate::settings::load(&self.home)?;
                tailnet::check(&settings, on)?;
                let on = if on == tailnet::AUTO { tailnet::place(&settings, &cwd).0 } else { on.to_string() };
                Some((on, settings)).filter(|(on, _)| on != tailnet::LOCAL)
            }
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
                    client_mcp: self.mcp.client_list(),
                    on: on.clone(),
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
    pub(super) fn workflow(&mut self, input: &Value) -> Result<String, String> {
        let script = match (input["scriptPath"].as_str(), input["script"].as_str(), input["name"].as_str()) {
            (Some(path), _, _) => {
                let path = tools::hashline::resolve(&self.cwd, path);
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
            client_mcp: self.mcp.client_list(),
            on: None,
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

    pub(super) fn saved_workflow(&self, name: &str) -> Option<workflow::Saved> {
        self.saved_workflows().into_iter().find(|saved| saved.meta.name == name)
    }

    /// `sessions/<id>/` beside the session file, for its pages and workflow runs; prune removes it with the file.
    fn session_folder(&self) -> Option<PathBuf> {
        self.session.path.as_ref().map(|path| path.with_extension(""))
    }

    /// One directory per run next to the session file, so a resumed session can still relaunch its runs.
    fn workflow_runs_dir(&self) -> PathBuf {
        match self.session_folder() {
            Some(folder) => folder.join("workflows"),
            None => std::env::temp_dir().join("agent-workflows").join(&self.session.id),
        }
    }

    pub(super) fn complete_plan(&mut self, input: &Value) -> Result<String, String> {
        let plan = input["plan"].as_str().map(str::trim).unwrap_or_default();
        if plan.is_empty() {
            return Err("Error: plan required".into());
        }
        *self.plan.lock().unwrap() = Plan::Ready(plan.to_string());
        self.save_plan()?;
        Ok("Plan submitted. End your turn now; the user picks what happens next.".into())
    }

    /// Claude Code's EnterPlanMode: the user consents before plan mode starts.
    pub(super) fn enter_plan(&mut self, emit: &mut dyn FnMut(Event)) -> Result<String, String> {
        if *self.plan.lock().unwrap() != Plan::Off {
            return Err("Already in plan mode.".into());
        }
        if !self.asks {
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
        match answers.recv().unwrap_or_default().0.first() {
            Some(Some(ask::Answer::Picked(0))) => {}
            Some(Some(ask::Answer::Typed(text))) => return Err(format!("User declined to enter plan mode and said: {text}")),
            _ => return Err("User declined to enter plan mode. Continue with the task without planning.".into()),
        }
        *self.plan.lock().unwrap() = Plan::Drafting;
        self.save_plan()?;
        Ok("Entered plan mode. You should now focus on exploring the codebase and designing an implementation approach.\n\nIn plan mode, you should:\n1. Thoroughly explore the codebase to understand existing patterns\n2. Identify similar features and architectural approaches\n3. Consider multiple approaches and their trade-offs\n4. Use AskUserQuestion if you need to clarify the approach\n5. Design a concrete implementation strategy\n6. When ready, use ExitPlanMode to present your plan for approval\n\nRemember: DO NOT write or edit any files yet. This is a read-only exploration and planning phase.".into())
    }

    /// Looks the skill up the way the system prompt lists it, so a name outside that list never reads a file.
    pub(super) fn skill(&self, input: &Value) -> Result<String, String> {
        let name = input["skill"].as_str().unwrap_or_default().trim().trim_start_matches('/');
        let args = input["args"].as_str().unwrap_or_default();
        let settings = crate::settings::load(&self.home)?;
        let mut skills = prompt::load_skills(&self.agent_dir, &self.cwd, settings.trusts(&self.cwd), |skill| settings.skill_enabled(skill), settings.prefer_global());
        skills.retain(|skill| skill.model_invocable);
        let skill = skills.iter().find(|skill| skill.name == name).ok_or_else(|| format!("Unknown skill: {name}"))?;
        self.record_skill_read(&skill.path);
        prompt::expand_skill(&format!("/skill:{name} {args}"), &skills)
    }

    pub(super) fn enter_worktree(&mut self, input: &Value, emit: &mut dyn FnMut(Event)) -> Result<String, String> {
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
        let typed = tools::hashline::resolve(&self.cwd, path);
        let path = crate::process::canonicalize(&typed).map_err(|e| format!("Cannot open {}: {e}", typed.display()))?;
        let (worktrees, current) = crate::worktree::list(&self.cwd)?;
        if !worktrees.iter().any(|worktree| crate::process::canonicalize(&worktree.path).is_ok_and(|known| known == path)) {
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

    pub(super) fn exit_worktree(&mut self, input: &Value, emit: &mut dyn FnMut(Event)) -> Result<String, String> {
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

    pub(super) fn goal_tool(&mut self, input: &Value) -> Result<String, String> {
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
        self.abort.store(false, Ordering::Relaxed);
        let mut answer = self.prompt(text, emit)?;
        loop {
            // A finished run queues a follow-up, and the idle TUI turns it into a turn that pursues the goal again.
            let background_running = self.has_background_work() || self.terminals.lock().unwrap().running() > 0;
            let next = match self.goal.lock().unwrap().as_mut() {
                Some(goal) => goal.after_turn(self.abort.load(Ordering::Relaxed), background_running),
                None => AfterTurn::Idle,
            };
            match next {
                AfterTurn::Idle => return Ok(answer),
                AfterTurn::Paused(note) => {
                    self.save_goal()?;
                    emit(Event::Notice(note));
                    return Ok(answer);
                }
                AfterTurn::Waiting(note) => {
                    emit(Event::Notice(note));
                    return Ok(answer);
                }
                AfterTurn::Continue(prompt) => {
                    self.save_goal()?;
                    emit(Event::Steered(prompt.clone()));
                    answer = self.turn(&prompt, false, emit)?;
                }
            }
        }
    }

    pub(super) fn push_notification(&self, input: &Value, emit: &mut dyn FnMut(Event)) -> Result<String, String> {
        if !self.interactive {
            return Err(ask::NO_UI.into());
        }
        let message = input["message"].as_str().map(str::trim).filter(|m| !m.is_empty()).ok_or("message required.")?;
        emit(Event::Push(message.to_string()));
        Ok("Sent. It shows only while the terminal is in the background and notifications are on in /settings.".into())
    }

    /// Serves pages on this computer's tailnet address, so a terminal that attaches later from another device can open
    /// them too, or on 127.0.0.1 without Tailscale or when that address cannot be bound.
    pub(super) fn page(&mut self, id: &str, input: &Value, emit: &mut dyn FnMut(Event)) -> Result<String, String> {
        if !self.interactive {
            return Err(ask::NO_UI.into());
        }
        let html = tools::page::render(input)?;
        if self.pages.is_none() {
            let tailnet = tailnet::self_address().and_then(|(ip, owner)| tools::page::Pages::start(ip, Some(owner)));
            self.pages = Some(match tailnet {
                Ok(pages) => pages,
                Err(_) => tools::page::Pages::start(std::net::Ipv4Addr::LOCALHOST.into(), None)?,
            });
        }
        let dir = match self.session_folder() {
            Some(folder) => folder.join("pages"),
            None => {
                crate::process::private_dir(&tools::spill_dir())?;
                tools::spill_dir().join("pages")
            }
        };
        let pages = self.pages.as_ref().expect("started above");
        let wait = input["wait"] == true;
        let published = pages.publish(&dir, &html, wait)?;
        emit(Event::OpenPage(published.url.clone()));
        let shown = format!("Sent {} to the user's browser, saved as {}.", published.url, published.path.display());
        if !wait {
            return Ok(shown);
        }
        emit(Event::ToolProgress { id: id.into(), text: format!("waiting for {}, Esc cancels", published.url) });
        let submitted = pages.wait(published.number, &self.abort).ok_or(ABORTED)?;
        Ok(format!("{shown}\n{}", tools::truncate_head(tools::page::answer(input, &submitted), "page")))
    }

    pub(super) fn ask_user(&mut self, input: &Value, emit: &mut dyn FnMut(Event)) -> Result<String, String> {
        if !self.asks {
            return Err(ask::NO_UI.into());
        }
        let questions = ask::parse(input)?;
        let (reply, answers) = mpsc::channel();
        let asked = std::time::Instant::now();
        emit(Event::Ask { questions: questions.clone(), reply });
        let (answers, images) = answers.recv().unwrap_or_default();
        self.waited_on_user += asked.elapsed();
        self.result_blocks = images;
        Ok(ask::response(&questions, &answers))
    }

    pub(super) fn video(&mut self, id: &str, input: &Value, emit: &mut dyn FnMut(Event)) -> Result<String, String> {
        let mut progress = |text| emit(Event::ToolProgress { id: id.into(), text });
        let spent = Arc::new(Mutex::new(None));
        let (client, model, abort, slot) = (self.client.clone(), self.cheap_model.clone(), self.abort.clone(), spent.clone());
        let summarize = move |description: &str| {
            let request = api::plain_request(&model, tools::video::SUMMARY_SYSTEM, description, tools::video::SUMMARY_MAX_TOKENS);
            let reply = client.send_quiet(&request, &abort)?;
            let summary = text_of(&reply.message);
            let stop_reason = reply.stop_reason.clone();
            *slot.lock().unwrap() = Some(reply);
            if stop_reason != "end_turn" || summary.trim().is_empty() {
                return Err(format!("{model} stopped with {stop_reason}"));
            }
            Ok(summary)
        };
        let watched = tools::video::watch(input, &self.cwd, &self.agent_dir, &self.abort, &mut progress, summarize);
        if let Some(reply) = spent.lock().unwrap().take() {
            self.usage.add(reply.usage);
            self.session.append(&usage::reply_entry(&reply, now_ms()))?;
        }
        let (text, blocks) = watched?;
        self.result_blocks = blocks;
        Ok(text)
    }

    /// Writes the generated image to `file_path` and attaches it, read back like a Read of an image.
    pub(super) fn generate_image(&mut self, input: &Value) -> Result<String, String> {
        let prompt = tools::hashline::str_arg(input, "prompt")?;
        let path = tools::hashline::resolve(&self.cwd, tools::hashline::str_arg(input, "file_path")?);
        let images: Option<Vec<String>> = serde_json::from_value(input["images"].clone()).map_err(|_| "\"images\" must be an array of paths.")?;
        let mut inputs = Vec::new();
        for image in images.unwrap_or_default() {
            inputs.push(crate::image::block(&tools::hashline::resolve(&self.cwd, &image))?);
        }
        // Before the request: an image drawn for a target that cannot take it is paid for and lost.
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("Cannot create {}: {e}", parent.display()))?;
        }
        tools::check_target(&path)?;
        let bytes = self.client.generate_image(&self.image_model, prompt, &inputs, &self.abort)?;
        let media_type = crate::image::sniffed_media_type(&bytes).ok_or("The image model answered with something that is not a PNG, JPEG, GIF or WebP image.")?;
        tools::hashline::replace_file(&path, &bytes)?;
        let mut text = format!("Wrote {} ({media_type}) to {}", tools::format_size(bytes.len()), path.display());
        match crate::image::block(&path) {
            Ok(block) => {
                self.result_blocks = vec![block];
                text.push_str("; it is attached as an image.");
            }
            // The file is written; an error here would have the model pay for the same image again.
            Err(error) => text.push_str(&format!("; it could not be attached: {error}")),
        }
        // A Gemini image model answers in the format it likes, which need not be the one the path's extension names.
        if let Some(named) = crate::image::media_type(&path).filter(|named| *named != media_type) {
            text.push_str(&format!(" The extension says {named}: rename the file if that matters."));
        }
        Ok(text)
    }
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
