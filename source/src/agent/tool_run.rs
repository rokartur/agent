use super::*;

impl Agent {
    /// Runs the edit hooks for the files this prompt changed and returns their failures.
    /// The commands come from a repo file, so each one passes the same permission check as bash.
    pub(super) fn run_edit_hooks(&mut self, files: Vec<PathBuf>, emit: &mut dyn FnMut(Event)) -> Vec<String> {
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
            if let Some(refusal) = self.check_permission(None, "Bash", &approved, &[], emit) {
                emit(Event::Notice(format!("[edit-hooks] {} skipped: {refusal}", hook.label)));
                continue;
            }
            emit(Event::Notice(format!("[edit-hooks] {}", hook.label)));
            let (output, is_error) = tools::run("Bash", &input, &hook.dir, &mut self.anchors, &self.rtk, &self.abort);
            if is_error {
                failures.push(edit_hooks::failure(&hook.label, &output));
            }
        }
        failures
    }

    /// The refusal of the first before_tool hook that matches and fails. Fail closed: a hook that crashes or times out refuses.
    fn before_tool_refusal(&mut self, tool: &str, input: &Value, subjects: &[String]) -> Option<String> {
        static CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let cwd = crate::process::canonicalize(&self.cwd).unwrap_or_else(|_| self.cwd.clone());
        for hook in self.before_tool.clone() {
            if !permissions::rule_matches(&hook.matches, tool, subjects, &cwd, false) {
                continue;
            }
            // In the agent's folder, not the temp dir: there another user could plant a link under a name this predictable.
            let call = self.home.join(format!(".agents/before-tool-{}-{}.json", std::process::id(), CALLS.fetch_add(1, Ordering::Relaxed)));
            let refused = |reason: &str| Some(format!("Blocked by the before_tool hook \"{}\":\n{}", hook.command, reason.chars().take(4000).collect::<String>()));
            if hook.command.contains("{call}")
                && let Err(e) = crate::api::accounts::write_private(&call, &json!({"tool": tool, "input": input, "cwd": self.cwd}).to_string())
            {
                return refused(&e);
            }
            let command = hook.command.replace("{call}", &edit_hooks::shell_quote(&call.display().to_string()));
            // Straight to the Bash tool: through check_permission the hook would run itself.
            let (output, is_error) = tools::run("Bash", &json!({"command": command, "timeout": 10_000}), &self.cwd, &mut self.anchors, &self.rtk, &self.abort);
            let _ = std::fs::remove_file(&call);
            if is_error {
                return refused(&output);
            }
        }
        None
    }

    /// One tool call, from the model's reply or from a Code script: its events, the run, the LSP report after an edit.
    /// Returns the result's content, the text in it, and whether the call failed. `bad_input` is the error of an input that was not valid JSON.
    pub(super) fn run_call(&mut self, id: &str, name: &str, input: &Value, bad_input: Option<&str>, emit: &mut dyn FnMut(Event)) -> Result<(Value, String, bool), String> {
        let targets = edit_hooks::edited_files(name, input, &self.cwd, &self.anchors);
        let mut shown = input.clone();
        // Anchors mean nothing to the user, show the file they resolve to.
        if let Some(file) = targets.first() {
            shown["file_path"] = file.strip_prefix(&self.cwd).unwrap_or(file).display().to_string().into();
        }
        emit(Event::ToolStart { id: id.into(), name: name.into(), input: shown });
        self.plugins.event(json!({"type": "tool_call", "name": name, "input": input, "context": self.plugin_context()}));
        let started = Instant::now();
        let (mut output, is_error, extra_blocks) = if let Some(error) = bad_input {
            (format!("{error} The call did not run; send it again with valid JSON."), true, Vec::new())
        } else if let Some(error) = tools::undeclared_params(&self.tools, name, input) {
            (format!("{error} The call did not run; send it again with only the parameters its schema lists."), true, Vec::new())
        } else if let Some(path) = self.requested_image(name, input) {
            match crate::image::block(&path) {
                Ok(block) => (format!("[{} is attached as an image.]", path.display()), false, vec![block]),
                Err(error) => (error, true, Vec::new()),
            }
        } else {
            let (output, is_error) = self.run_tool_with_checkpoint(id, name, input, &targets, emit)?;
            (output, is_error, std::mem::take(&mut self.result_blocks))
        };
        if !is_error {
            for file in &targets {
                match self.lsp.after_edit(file, &self.cwd, &self.abort) {
                    Ok(Some(report)) => output = format!("{output}\n\n{report}"),
                    Ok(None) => {}
                    Err(notice) => emit(Event::Notice(notice)),
                }
            }
            self.edited.extend(targets);
        }
        self.tool_calls += 1;
        // Includes the LSP check after an edit: the whole wait before the next tool.
        self.session.append(&json!({"type": "tool_time", "id": id, "name": name, "ms": started.elapsed().as_millis() as u64}))?;
        let content = if extra_blocks.is_empty() {
            json!(output)
        } else {
            let mut blocks = vec![json!({"type": "text", "text": output})];
            blocks.extend(extra_blocks);
            json!(blocks)
        };
        // Video puts its transcript in text blocks between the frames; the UI, plugins and condense read it too.
        let output = crate::acp::result_text(&content);
        self.plugins.event(json!({"type": "tool_result", "name": name, "output": output, "isError": is_error, "context": self.plugin_context()}));
        emit(Event::ToolEnd { id: id.into(), output: output.clone(), is_error });
        Ok((content, output, is_error))
    }

    /// Runs one tool call after the permission check; agent-level tools are handled in `agent_tools`, file and shell tools in `tools`.
    pub(super) fn run_tool(&mut self, id: &str, name: &str, input: &Value, targets: &[PathBuf], emit: &mut dyn FnMut(Event)) -> (String, bool) {
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
            let (output, is_error) = self.run_tool(id, "Read", &read, &[], emit);
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
        if let Some(reason) = self.check_permission(Some(id), name, input, targets, emit) {
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
            "ContextTreeQuery" => condense::query(&self.history.messages, &self.history.condensed, input),
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
            "Code" => self.code(id, input, emit),
            "WaitForAgents" => self.bg_wait(input),
            "CronCreate" if self.interactive => tools::cron::create(&self.crons, input, &self.follow_ups),
            "ScheduleWakeup" if self.interactive => tools::cron::wakeup(&self.crons, input, &self.follow_ups),
            "CronCreate" | "ScheduleWakeup" => Err("Scheduling needs the interactive UI; this process ends with its prompt.".into()),
            "CronDelete" => self.crons.lock().unwrap().delete(input),
            "CronList" => Ok(self.crons.lock().unwrap().list()),
            "PushNotification" => self.push_notification(input, emit),
            "WebSearch" | "WebFetch" | "GetSearchContent" => self.web.run(name, input),
            "AskUserQuestion" => self.ask_user(input, emit),
            "Video" => self.video(id, input, emit),
            "ImageGenerate" => self.generate_image(input),
            "Page" => self.page(id, input, emit),
            _ => return self.plugins.tool(name, input, &self.plugin_context(), &self.abort).or_else(|| self.mcp.tool(name, input, &self.abort)).unwrap_or_else(|| self.run_file_tool(id, name, input)),
        };
        match result {
            Ok(text) => (text, false),
            Err(text) => (text, true),
        }
    }

    fn run_file_tool(&mut self, id: &str, name: &str, input: &Value) -> (String, bool) {
        let (mut output, is_error) = match &self.editor_terminal {
            Some(terminal) if name == "Bash" => tools::bash_in_terminal(terminal.as_ref(), id, input, &self.cwd, &self.rtk, &self.abort),
            _ => tools::run(name, input, &self.cwd, &mut self.anchors, &self.rtk, &self.abort),
        };
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
                let path = tools::hashline::resolve(&self.cwd, path);
                self.record_skill_read(&path);
                let nested = prompt::nested_context(&self.cwd, &self.extra_dirs, &path, &mut self.nested_instructions);
                if !nested.is_empty() {
                    output = format!("{output}\n\n{nested}");
                }
            }
        }
        (output, is_error)
    }

    /// The image file a single-path `Read` names; it goes to the model as an image block, not as anchored text.
    pub(super) fn requested_image(&self, name: &str, input: &Value) -> Option<PathBuf> {
        if name != "Read" {
            return None;
        }
        let path = tools::hashline::resolve(&self.cwd, input["file_path"].as_str()?);
        crate::image::media_type(&path).map(|_| path)
    }

    /// A read SKILL.md counts as the agent using that skill.
    pub(super) fn record_skill_read(&self, path: &Path) {
        if path.file_name().is_none_or(|name| name != "SKILL.md") {
            return;
        }
        let Some(skill) = prompt::parse_skill(path) else { return };
        // Usage stats must not fail the read that triggered them.
        let _ = skill_usage::record(&self.agent_dir, &skill.name, skill_usage::By::Agent, &self.cwd);
    }

    /// None when the call may run; otherwise the tool result that explains the refusal.
    /// A user's "No" also stops the turn, like Esc.
    pub(super) fn check_permission(&mut self, call: Option<&str>, tool: &str, input: &Value, edited_files: &[PathBuf], emit: &mut dyn FnMut(Event)) -> Option<String> {
        // A script file is checked as the text it holds now, like an inline script: a path alone would show no code and cover every later content.
        let resolved;
        let input = match input["scriptPath"].as_str() {
            Some(path) if tool == "Workflow" => {
                let path = tools::hashline::resolve(&self.cwd, path);
                let script = match std::fs::read_to_string(&path) {
                    Ok(script) => script,
                    Err(e) => return Some(format!("Cannot read {}: {e}", path.display())),
                };
                let mut with_script = input.clone();
                with_script.as_object_mut()?.remove("scriptPath");
                with_script["script"] = json!(script);
                resolved = with_script;
                &resolved
            }
            _ => input,
        };
        // A saved workflow is granted by its file, so a project workflow that later shadows the same name asks again.
        let saved_file: Vec<PathBuf> = match (tool, input["name"].as_str()) {
            ("Workflow", Some(name)) => self.saved_workflow(name.trim_start_matches('/')).and_then(|saved| saved.path).into_iter().collect(),
            _ => Vec::new(),
        };
        let files = if tool == "Workflow" { &saved_file } else { edited_files };
        // Above request_for: a deny rule also covers Read, which never asks.
        let subjects = permissions::subjects(tool, input, files, &self.cwd);
        let verdict = permissions::verdict(&self.permission_rules, tool, &subjects, &self.cwd);
        // A deny rule refuses the call without running the user's hooks.
        let hook_refusal = if matches!(verdict, Some(Verdict::Deny(_))) { None } else { self.before_tool_refusal(tool, input, &subjects) };
        let decided = permissions::decide(permissions::Call {
            tool,
            input,
            files,
            verdict,
            hook_refusal,
            // Read after the hooks run; acp.rs relies on that order.
            mode: *self.permission_mode.lock().unwrap(),
            grants: &self.grants,
            cwd: &self.cwd,
            extra_dirs: &self.extra_dirs,
            user_requests: &self.user_requests,
            follow_up: self.follow_up.as_deref(),
        });
        let request = match decided {
            Step::Allow => return None,
            Step::Deny(reason) => return Some(reason),
            Step::Review(request, prompt) => return self.review(&request, prompt, emit),
            Step::AskUser(request) => request,
        };
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
    pub(super) fn review(&mut self, request: &Request, prompt: Option<String>, emit: &mut dyn FnMut(Event)) -> Option<String> {
        let (allowed, reason) = match prompt {
            Some(prompt) => self.ask_reviewer(&prompt),
            None => (false, format!("the call is over {} characters, too long to review", permissions::REVIEW_DETAIL_CHARS)),
        };
        emit(Event::Reviewed { allowed, detail: request.detail.clone(), reason: reason.clone() });
        let entry = json!({"type": "review", "kind": request.kind.label(), "detail": request.detail, "allowed": allowed, "reason": reason});
        if let Err(error) = self.session.append(&entry) {
            return Some(format!("Denied: the review could not be saved: {error}."));
        }
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
}
