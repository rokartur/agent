use super::*;

const SNAPSHOT_FAILED: &str = "File snapshot failed, a rewind will not restore files from this tool call";

/// A user prompt the conversation can go back to.
#[derive(Debug, PartialEq)]
pub struct RewindPoint {
    /// Message index of the prompt.
    pub at: usize,
    pub text: String,
    pub images: Vec<Value>,
}

impl Agent {
    /// Files this session changed, with diffs; None when the work tree is not snapshotted.
    pub fn changes(&self) -> Result<Option<Vec<FileChange>>, String> {
        let Some(snapshots) = &self.snapshots else { return Ok(None) };
        session_changes(snapshots, &self.history.checkpoints).map(Some)
    }

    /// Drops the message at `index` and everything after it, and puts back the files those prompts changed.
    /// None: the work tree is not snapshotted, so files stay as they are.
    pub fn rewind(&mut self, index: usize) -> Result<Option<RestoredFiles>, String> {
        let files = self.snapshots.as_ref().map(|snapshots| restore_files(snapshots, &self.history.checkpoints, &self.history.file_checkpoints, index)).transpose()?;
        self.forget_from(index)?;
        Ok(files)
    }

    /// Drops message `index` and every later one from the context, and leaves the files as they are.
    pub fn forget_from(&mut self, index: usize) -> Result<(), String> {
        // Saved first: a rewind only in memory would come back on resume.
        self.record(json!({"type": "rewind", "index": index}))?;
        self.estimate_context_tokens();
        Ok(())
    }

    /// Snapshots around a tool that can change files, so a rewind touches only what this session changed.
    /// A file another session edits while this call runs still counts as this session's.
    pub(super) fn run_tool_with_checkpoint(&mut self, id: &str, name: &str, input: &Value, targets: &[PathBuf], emit: &mut dyn FnMut(Event)) -> Result<(String, bool), String> {
        // Before the snapshot: the move swaps the snapshot repo, so before and after would come from different checkouts.
        if name == "Bash"
            && let Some(dir) = tools::cd_target(input["command"].as_str().unwrap_or_default())
        {
            // The auto reviewer trusts calls inside the working directory, so moving it is a call to review.
            if let Some(reason) = self.check_permission(Some(id), name, input, targets, emit) {
                return Ok((reason, true));
            }
            return Ok(match self.cd(dir, emit) {
                Ok(text) => (text, false),
                Err(text) => (text, true),
            });
        }
        let Some(snapshots) = self.snapshots.as_ref().filter(|_| tools::facts(name).is_some_and(|facts| facts.changes_files)) else {
            return Ok(self.run_tool(id, name, input, targets, emit));
        };
        let files_before = match save_files_left_out(snapshots, targets) {
            Ok(files) => files,
            Err(error) => {
                emit(Event::Notice(format!("{SNAPSHOT_FAILED}: {error}")));
                Vec::new()
            }
        };
        let before = match snapshots.take() {
            Ok(before) => before,
            Err(error) => {
                emit(Event::Notice(format!("{SNAPSHOT_FAILED}: {error}")));
                return Ok(self.run_tool(id, name, input, targets, emit));
            }
        };
        let result = self.run_tool(id, name, input, targets, emit);
        let snapshots = self.snapshots.as_ref().expect("checked at the top of run_tool_with_checkpoint");
        let at = self.history.messages.len();
        let mut entries = Vec::new();
        for (path, before) in files_before {
            match snapshots.save_file(&path) {
                Ok(after) if after == before => {}
                Ok(after) => entries.push(json!({"type": "file_checkpoint", "index": at, "path": path, "before": before, "after": after})),
                Err(error) => emit(Event::Notice(format!("{SNAPSHOT_FAILED}: {error}"))),
            }
        }
        match snapshots.take() {
            Ok(after) if after == before => {}
            Ok(after) => entries.push(json!({"type": "tool_checkpoint", "index": at, "before": before, "after": after})),
            Err(error) => emit(Event::Notice(format!("{SNAPSHOT_FAILED}: {error}"))),
        }
        for entry in entries {
            self.record(entry)?;
        }
        Ok(result)
    }
}

/// User prompts that can be rewound to, oldest first.
pub fn rewind_points(messages: &[Value]) -> Vec<RewindPoint> {
    let mut points = Vec::new();
    for (at, message) in messages.iter().enumerate() {
        let Some(blocks) = message["content"].as_array().filter(|_| message["role"] == "user") else { continue };
        if blocks.iter().any(|block| block["type"] == "tool_result") {
            continue;
        }
        // The typed text is the last block; shell output and todo notes come before it.
        let Some(text) = blocks.iter().rev().find_map(|block| block["text"].as_str()) else { continue };
        // A turn started by finished background work has no typed prompt.
        if harness_source(text).is_some() {
            continue;
        }
        let images = blocks.iter().filter(|block| block["type"] == "image").cloned().collect();
        points.push(RewindPoint { at, text: text.to_string(), images });
    }
    points
}

/// A file this session's tool calls changed, with its diff from before the first of them to now.
#[derive(Debug)]
pub struct FileChange {
    pub path: String,
    pub diff: String,
}

/// Files this session's tool calls changed that still differ from how they were before the first change.
pub(super) fn session_changes(snapshots: &Snapshots, checkpoints: &[Checkpoint]) -> Result<Vec<FileChange>, String> {
    let changed = changed_paths(snapshots, checkpoints)?;
    if changed.is_empty() {
        return Ok(Vec::new());
    }
    let now = snapshots.take()?;
    let mut changes = Vec::new();
    for (path, (before, _)) in changed {
        let diff = snapshots.diff(before, &now, &path)?;
        if !diff.is_empty() {
            changes.push(FileChange { path, diff });
        }
    }
    Ok(changes)
}

/// Files a rewind put back, and files it left alone because they changed after the agent last wrote them.
#[derive(Debug, Default, PartialEq)]
pub struct RestoredFiles {
    pub restored: Vec<String>,
    pub kept: Vec<String>,
}

/// Puts each file this session's tool calls from message `index` on changed back as it was before the first of them.
pub(super) fn restore_files(snapshots: &Snapshots, checkpoints: &[Checkpoint], file_checkpoints: &[FileCheckpoint], index: usize) -> Result<RestoredFiles, String> {
    let mut files = restore_files_left_out(snapshots, file_checkpoints, index)?;
    let changed = changed_paths(snapshots, checkpoints.iter().filter(|checkpoint| checkpoint.at >= index))?;
    if changed.is_empty() {
        return Ok(files);
    }
    let now = snapshots.take()?;
    let mut deletions = Vec::new();
    for (path, (before, after)) in changed {
        let current = snapshots.blob(&now, &path);
        let before_blob = snapshots.blob(before, &path);
        if current != snapshots.blob(after, &path) {
            files.kept.push(path);
        } else if current != before_blob {
            if before_blob.is_none() {
                deletions.push((path, before));
            } else {
                snapshots.restore(before, &path)?;
                files.restored.push(path);
            }
        }
    }
    // Snapshots skip ignored files, so a path missing from `before` may have been ignored then (an unignored `.env`).
    // Asked after the restores above, so the ignore files are as they were before.
    for (path, before) in deletions {
        if snapshots.ignored(&path)? {
            files.kept.push(path);
        } else {
            snapshots.restore(before, &path)?;
            files.restored.push(path);
        }
    }
    Ok(files)
}

/// Saves each target the work tree snapshots leave out, with its blob id from before the tool call.
fn save_files_left_out(snapshots: &Snapshots, targets: &[PathBuf]) -> Result<Vec<(PathBuf, Option<String>)>, String> {
    let mut saved = Vec::new();
    for path in targets {
        if !snapshots.stores(path)? {
            saved.push((path.clone(), snapshots.save_file(path)?));
        }
    }
    Ok(saved)
}

/// Like the work tree part of `restore_files`, for the files `save_files_left_out` saved.
fn restore_files_left_out(snapshots: &Snapshots, file_checkpoints: &[FileCheckpoint], index: usize) -> Result<RestoredFiles, String> {
    let mut changed: BTreeMap<&Path, (Option<&str>, Option<&str>)> = BTreeMap::new();
    for checkpoint in file_checkpoints.iter().filter(|checkpoint| checkpoint.at >= index) {
        let (_, last_after) = changed.entry(&checkpoint.path).or_insert((checkpoint.before.as_deref(), checkpoint.after.as_deref()));
        *last_after = checkpoint.after.as_deref();
    }
    let mut files = RestoredFiles::default();
    for (path, (before, after)) in changed {
        let current = snapshots.file_id(path)?;
        if current.as_deref() != after {
            files.kept.push(path.display().to_string());
        } else if current.as_deref() != before {
            snapshots.restore_file(path, before)?;
            files.restored.push(path.display().to_string());
        }
    }
    Ok(files)
}

/// Each path `checkpoints` changed, with the snapshot from before the first change and after the last.
fn changed_paths<'a>(snapshots: &Snapshots, checkpoints: impl IntoIterator<Item = &'a Checkpoint>) -> Result<BTreeMap<String, (&'a str, &'a str)>, String> {
    let mut changed = BTreeMap::new();
    for checkpoint in checkpoints {
        for path in snapshots.changed(&checkpoint.before, &checkpoint.after)? {
            let (_, last_after) = changed.entry(path).or_insert((checkpoint.before.as_str(), checkpoint.after.as_str()));
            *last_after = checkpoint.after.as_str();
        }
    }
    Ok(changed)
}
