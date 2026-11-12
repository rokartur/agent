use super::*;

/// Where a run records its agents, and the earlier run it replays.
/// Keys are the call text and how many identical calls came before it, so a reordered fan-out still finds its results.
pub(super) struct Ledger {
    prior: HashMap<(String, usize), Recorded>,
    seen: HashMap<String, usize>,
    /// Set at the first call that failed or changed since the earlier run; every call after it runs live.
    pub(super) diverged: bool,
    file: std::fs::File,
}

#[derive(Clone, PartialEq, Debug)]
pub(super) enum Recorded {
    /// Started but never finished, as when the run was stopped: it runs again without rerunning the agents after it.
    Started,
    Done(Value),
    Failed,
}

impl Ledger {
    pub(super) fn open(path: &Path, prior: HashMap<(String, usize), Recorded>) -> Result<Ledger, String> {
        let file = OpenOptions::new().create(true).append(true).open(path).map_err(|e| format!("Cannot open {}: {e}", path.display()))?;
        Ok(Ledger { prior, seen: HashMap::new(), diverged: false, file })
    }

    pub(super) fn key(&mut self, call: &Call) -> (String, usize) {
        let text = format!("{}\n{}", call.prompt, call.options);
        let n = self.seen.entry(text.clone()).or_insert(0);
        *n += 1;
        (text, *n - 1)
    }

    /// The earlier run's result for this call, when it finished and nothing before it diverged.
    pub(super) fn replay(&mut self, key: &(String, usize)) -> Option<Value> {
        if self.prior.is_empty() || self.diverged {
            return None;
        }
        match self.prior.get(key) {
            Some(Recorded::Done(result)) => Some(result.clone()),
            Some(Recorded::Started) => None,
            Some(Recorded::Failed) | None => {
                self.diverged = true;
                None
            }
        }
    }

    pub(super) fn write(&mut self, key: &(String, usize), event: &str, result: Option<&Value>) {
        let mut entry = json!({"key": key.0, "n": key.1, "event": event});
        if let Some(result) = result {
            entry["result"] = result.clone();
        }
        // A lost line only costs a rerun on resume.
        let _ = writeln!(self.file, "{entry}");
    }

    /// Stopping the whole run counts nobody as failed, so its stopped agents run again on resume without rerunning the rest.
    pub(super) fn finished(&mut self, key: &(String, usize), outcome: &Result<Value, String>, run_stopped: bool) {
        match outcome {
            Ok(Value::Null) | Err(_) if run_stopped => {}
            Ok(Value::Null) | Err(_) => self.write(key, "failed", None),
            Ok(value) => self.write(key, "done", Some(value)),
        }
    }
}

pub(super) fn prior_journal(workflows: &Arc<Mutex<Vec<Arc<Workflow>>>>, runs_dir: &Path, run_id: &str) -> Result<HashMap<(String, usize), Recorded>, String> {
    let earlier = workflows.lock().unwrap().iter().find(|workflow| workflow.run_id == run_id).cloned();
    if let Some(earlier) = earlier {
        if earlier.is_running() {
            return Err(format!("{} ({run_id}) is still running. Stop it with TaskStop first.", earlier.id));
        }
        let live = earlier.live_agents();
        if live > 0 {
            return Err(format!("{live} agents of {run_id} have not exited yet. Relaunch once they have."));
        }
    }
    let path = run_dir(runs_dir, run_id)?.join("journal.jsonl");
    let text = std::fs::read_to_string(&path).map_err(|_| format!("Run {run_id}: nothing to resume. Start the workflow over as a new run."))?;
    Ok(read_journal(&text))
}

pub(super) fn read_journal(text: &str) -> HashMap<(String, usize), Recorded> {
    let mut journal = HashMap::new();
    for entry in text.lines().filter_map(|line| serde_json::from_str::<Value>(line).ok()) {
        let key = (entry["key"].as_str().unwrap_or_default().to_string(), entry["n"].as_u64().unwrap_or_default() as usize);
        let recorded = match entry["event"].as_str() {
            Some("done") => Recorded::Done(entry["result"].clone()),
            Some("failed") => Recorded::Failed,
            _ => Recorded::Started,
        };
        journal.insert(key, recorded);
    }
    journal
}
