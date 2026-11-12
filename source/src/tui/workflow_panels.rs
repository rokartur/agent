use super::*;

impl App {
    /// A single run opens its progress view directly, as in Claude Code.
    pub(super) fn open_workflows(&mut self) {
        let runs = self.workflows.lock().unwrap().clone();
        match runs.as_slice() {
            [] => self.system("workflows", "No workflows yet. Ask for one, or run a saved one such as /deep-research."),
            [run] => self.panel = Some(Panel::WorkflowRun { id: run.id.clone(), phase: None, index: 0 }),
            _ => self.panel = Some(Panel::Workflows { index: runs.len() - 1 }),
        }
    }

    pub(super) fn workflow_key(&mut self, code: KeyCode) {
        let runs = self.workflows.lock().unwrap().clone();
        match self.panel.as_mut() {
            Some(Panel::Workflows { index }) => match code {
                KeyCode::Up => *index = index.saturating_sub(1),
                KeyCode::Down => *index = (*index + 1).min(runs.len() - 1),
                KeyCode::Char('x') => runs[*index].stop(),
                KeyCode::Enter | KeyCode::Right => self.panel = Some(Panel::WorkflowRun { id: runs[*index].id.clone(), phase: None, index: 0 }),
                KeyCode::Esc | KeyCode::Left => self.panel = None,
                _ => {}
            },
            Some(Panel::WorkflowRun { id, phase, index }) => {
                let Some(position) = runs.iter().position(|run| run.id == *id) else { return self.panel = None };
                let run = &runs[position];
                let (len, agent) = {
                    let state = run.state.lock().unwrap();
                    match *phase {
                        None => (state.phases.len(), None),
                        Some(shown) => (state.phases[shown].steps.len(), state.phases[shown].steps.get(*index).and_then(|step| step.run.clone())),
                    }
                };
                match code {
                    KeyCode::Up => *index = index.saturating_sub(1),
                    KeyCode::Down => *index = (*index + 1).min(len.saturating_sub(1)),
                    KeyCode::Enter | KeyCode::Right if phase.is_none() && len > 0 => (*phase, *index) = (Some(*index), 0),
                    KeyCode::Enter | KeyCode::Right if agent.is_some() => self.view_run(agent),
                    KeyCode::Esc | KeyCode::Left => match *phase {
                        Some(shown) => (*phase, *index) = (None, shown),
                        None if runs.len() > 1 => self.panel = Some(Panel::Workflows { index: position }),
                        None => self.panel = None,
                    },
                    KeyCode::Char('p') => _ = run.paused.fetch_xor(true, std::sync::atomic::Ordering::Relaxed),
                    KeyCode::Char('x') => match (phase, agent) {
                        (Some(_), Some(agent)) => agent.stop(),
                        (Some(_), None) => {}
                        (None, _) => run.stop(),
                    },
                    KeyCode::Char('s') => self.panel = Some(Panel::WorkflowSave { id: id.clone(), personal: false }),
                    _ => {}
                }
            }
            Some(Panel::WorkflowSave { id, personal }) => match code {
                KeyCode::Tab | KeyCode::Up | KeyCode::Down => *personal = !*personal,
                KeyCode::Esc => self.panel = Some(Panel::WorkflowRun { id: id.clone(), phase: None, index: 0 }),
                KeyCode::Enter => {
                    let Some(run) = runs.iter().find(|run| run.id == *id) else { return self.panel = None };
                    let personal = *personal;
                    let dir = if personal { self.home.join(".agents").join("workflows") } else { workflow::project_save_dir(&self.cwd) };
                    match workflow::save(&run.script, &run.name, &dir, !personal) {
                        Ok(path) => {
                            self.panel = None;
                            let runs_as = if personal || self.settings.trusts(&self.cwd) {
                                format!("Run it as /{}.", run.name)
                            } else {
                                format!("It runs as /{} once /trust loads this project's files.", run.name)
                            };
                            self.system("workflows", format!("Saved {}. {runs_as}", path.display()));
                            self.load_skills_and_prompts();
                        }
                        Err(error) => self.push(Item::Error(error)),
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }

    pub(super) fn workflow_panel(&self, columns: usize) -> Vec<Line> {
        let runs = self.workflows.lock().unwrap().clone();
        match &self.panel {
            Some(Panel::Workflows { index }) => {
                let rows: Vec<list::Row> = runs.iter().map(|run| list::Row { left: format!("{} {}", run.id, run.name), right: vec![span(run.summary(), DIM)] }).collect();
                let view = list::List {
                    title: format!("Workflows {}", runs.len()),
                    tabs: &[],
                    tab: 0,
                    search: None,
                    rows: &rows,
                    selected: *index,
                    empty: "",
                    hint: "↑↓ select     enter open     x stop     esc close",
                };
                view.render(columns, PANEL_ROWS)
            }
            Some(Panel::WorkflowRun { id, phase, index }) => {
                let Some(run) = runs.iter().find(|run| run.id == *id) else { return Vec::new() };
                let mut head = vec![vec![span(&run.name, BRIGHT.bold()), span(format!("  {}", run.summary()), MUTED)], vec![span(&run.description, DIM)]];
                let state = run.state.lock().unwrap();
                head.extend(state.logs.iter().rev().take(3).rev().map(|log| vec![span(format!("log: {log}"), DIM)]));
                let (rows, hint): (Vec<(String, String)>, _) = match *phase {
                    None => {
                        let rows = state.phases.iter().map(|shown| (if shown.title.is_empty() { "Agents".to_string() } else { shown.title.clone() }, step_counts(&shown.steps))).collect();
                        (rows, "↑↓ select · Enter agents · p pause/resume · x stop run · s save as command · Esc back")
                    }
                    Some(shown) => {
                        let rows = state.phases[shown].steps.iter().map(step_row).collect();
                        (rows, "↑↓ select · Enter transcript · x stop agent · Esc back")
                    }
                };
                let left_width = rows.iter().map(|(left, _)| width(left)).max().unwrap_or(0);
                list::frame(head, rows.len(), *index, hint, columns, PANEL_ROWS, |row| {
                    let (left, right) = &rows[row];
                    let style = if row == *index { BRIGHT.bold() } else { PLAIN };
                    vec![span(format!("{left}{}    ", " ".repeat(left_width - width(left))), style), span(right, DIM)]
                })
            }
            Some(Panel::WorkflowSave { id, personal }) => {
                let Some(run) = runs.iter().find(|run| run.id == *id) else { return Vec::new() };
                let places = [("Project", workflow::project_save_dir(&self.cwd)), ("Personal", self.home.join(".agents").join("workflows"))];
                let rows: Vec<list::Row> =
                    places.iter().map(|(place, dir)| list::Row { left: place.to_string(), right: vec![span(dir.join(format!("{}.js", run.name)).display().to_string(), DIM)] }).collect();
                let view = list::List {
                    title: format!("Save as /{}", run.name),
                    tabs: &[],
                    tab: 0,
                    search: None,
                    rows: &rows,
                    selected: usize::from(*personal),
                    empty: "",
                    hint: "tab switch location     enter save     esc back",
                };
                view.render(columns, PANEL_ROWS)
            }
            _ => Vec::new(),
        }
    }
}
