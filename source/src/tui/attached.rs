use super::*;

impl App {
    pub(super) fn add_client(&mut self, mut client: server::Client) {
        client.queue(&cwd_sequence(&self.cwd));
        client.queue(&self.title);
        if self.working.is_some()
            && self.settings.progress_bar
            && let Some(sequence) = notify::progress(true, |name| client.env(name))
        {
            client.queue(&sequence);
        }
        if let Some(status) = &self.status_sent
            && let Some(sequence) = notify::program_status(status, |name| client.env(name))
        {
            client.queue(&sequence);
        }
        if let Some(url) = self.pending_page.take() {
            client.send(json!({"open": url}));
        }
        self.clients.push(client);
        self.invalidate_screens();
    }

    pub(super) fn input(&mut self, id: u64, input: Input) {
        self.typing_client = Some(id);
        match input {
            Input::Key(key) if key.kind != KeyEventKind::Release => {
                // Typing proves focus, also in terminals that never report it.
                self.set_focus(id, true);
                // A drag released outside the window sends no button-up, so a key stops its edge scroll.
                self.edge_drag = None;
                self.key(key);
            }
            Input::Paste(text) => self.paste(&text),
            Input::Resize(width, height) => {
                if let Some(client) = self.clients.iter_mut().find(|client| client.id == id) {
                    client.size = (width as usize, height as usize);
                }
                self.invalidate_screens();
            }
            // A terminal larger than the shared frame reports clicks outside it.
            Input::Mouse(mouse) if usize::from(mouse.column) < self.size.0 && usize::from(mouse.row) < self.size.1 => self.mouse(mouse),
            Input::FocusGained => self.set_focus(id, true),
            Input::FocusLost => self.set_focus(id, false),
            _ => {}
        }
    }

    /// Copying, opening links and reading the clipboard happen on the computer of the terminal that typed, which in a
    /// remote session is not this one. A failure comes back as `Msg::TerminalFailed`.
    pub(super) fn ask_terminal(&self, request: Value) {
        if let Some(client) = self.clients.iter().find(|client| Some(client.id) == self.typing_client) {
            client.send(request);
        }
    }

    fn set_focus(&mut self, id: u64, focused: bool) {
        if let Some(client) = self.clients.iter_mut().find(|client| client.id == id) {
            client.focused = Some(focused);
        }
    }

    /// /detach: lets go of the terminal that typed it; the session and its other terminals go on.
    pub(super) fn detach(&mut self) {
        let Some(index) = self.clients.iter().position(|client| Some(client.id) == self.typing_client) else { return };
        let client = self.clients.remove(index);
        client.finish(&format!("Detached; the session keeps running. Return to it with: agent attach {}\n", self.session_id));
        self.invalidate_screens();
    }

    /// /env: without a name opens the environments of the terminal that typed it, with one moves that terminal to a
    /// new session there. A session that never got a message and has no other terminal ends instead of idling.
    pub(super) fn switch_environment(&mut self, name: &str) {
        let Some(index) = self.clients.iter().position(|client| Some(client.id) == self.typing_client) else { return };
        let place = &self.clients[index].place;
        if name.is_empty() {
            self.panel = Some(Panel::Environments { place: place.clone(), index: 0, naming: None, confirm: false });
            return;
        }
        if !place.environments.iter().any(|environment| environment == name) {
            let error = format!("No environment {name}. It must be one of your computers, online on the tailnet and running `agent serve --tailnet` (`agent service install`).");
            self.push(Item::Error(error));
            return;
        }
        self.release_typing_client(|client| client.switch(name));
    }

    /// Hands the terminal that typed the command to `goodbye`. A session that never got a message and has no other
    /// terminal ends instead of idling.
    pub(super) fn release_typing_client(&mut self, goodbye: impl FnOnce(server::Client)) {
        let Some(index) = self.clients.iter().position(|client| Some(client.id) == self.typing_client) else { return };
        goodbye(self.clients.remove(index));
        self.invalidate_screens();
        let talked = self.session_path.as_ref().is_some_and(|path| std::fs::read_to_string(path).is_ok_and(|text| text.contains(r#""type":"message""#)));
        if !talked && self.clients.is_empty() && self.working.is_none() {
            self.quit = true;
        }
    }

    /// One frame at the smallest attached size, sent to every client; a session without clients draws nothing.
    pub(super) fn draw(&mut self) {
        let Some(width) = self.clients.iter().map(|client| client.size.0).min() else { return };
        let height = self.clients.iter().map(|client| client.size.1).min().expect("a width means a client");
        self.size = (width, height);
        let (rows, cursor) = self.frame(width, height);
        for client in &mut self.clients {
            client.draw(rows.clone(), cursor);
        }
    }

    pub(super) fn invalidate_screens(&mut self) {
        for client in &mut self.clients {
            client.invalidate();
        }
    }

    /// OSC 0 sets the terminal's window and tab title.
    pub(super) fn set_title(&mut self, title: &str) {
        self.title = format!("\x1b]0;{}\x07", notify::printable(title));
        for client in &mut self.clients {
            client.queue(&self.title);
        }
    }

    pub(super) fn show_progress(&mut self, busy: bool) {
        for client in &mut self.clients {
            if let Some(sequence) = notify::progress(busy, |name| client.env(name)) {
                client.queue(&sequence);
            }
        }
    }

    pub(super) fn report_to_cmux(&mut self) {
        let status = if matches!(self.panel, Some(Panel::Permission { .. } | Panel::Ask(_))) {
            cmux::Status::NeedsInput
        } else if self.working.is_some() {
            cmux::Status::Running
        } else {
            cmux::Status::Idle
        };
        let resume = self.session_path.as_ref().map(|_| (self.session_id.clone(), self.cwd.clone()));
        if let Some(cmux) = &mut self.cmux {
            cmux.report(status, resume);
        }
    }

    pub(super) fn report_to_rex(&mut self) {
        let status = match &self.panel {
            Some(Panel::Permission { request, .. }) => notify::Status { state: "blocked", kind: Some("permission"), msg: format!("{} {}", request.question, request.detail) },
            Some(Panel::Ask(ask)) => notify::Status { state: "blocked", kind: Some("question"), msg: ask.first_question().to_string() },
            _ if self.working.is_some() => notify::Status { state: "working", kind: None, msg: String::new() },
            _ => self.outcome.clone(),
        };
        if self.status_sent.as_ref() == Some(&status) {
            return;
        }
        for client in &mut self.clients {
            if let Some(sequence) = notify::program_status(&status, |name| client.env(name)) {
                client.queue(&sequence);
            }
        }
        self.status_sent = Some(status);
    }

    /// Desktop notification, sent only while the terminal is in the background; cmux decides that itself and also lists it in its sidebar.
    /// Rex notifies from the program status record instead, so it gets only `Push`, which has no subtitle.
    pub(super) fn notify(&mut self, subtitle: &str, body: &str) {
        if !self.settings.notifications {
            return;
        }
        if let Some(cmux) = &self.cmux {
            cmux.notify(subtitle, &notify::printable(body));
            return;
        }
        let has_status_record = |client: &server::Client| client.env("TERM_PROGRAM").as_deref() == Some("rex") && !subtitle.is_empty();
        for client in self.clients.iter_mut().filter(|client| client.focused != Some(true) && !has_status_record(client)) {
            let sequence = notify::sequence("agent", body, |name| client.env(name));
            client.queue(&sequence);
        }
    }
}
