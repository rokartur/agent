use std::cell::RefCell;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

const KEEP_DAYS: u64 = 30;

/// Trees of one git work tree, kept in a private repo under ~/.agents/snapshots so a rewind can put files back.
/// Ignored files are not stored, like in the project's own repo.
pub struct Snapshots {
    git_dir: PathBuf,
    work_tree: PathBuf,
    index: PathBuf,
    /// The project's `.git/info/exclude`; `git add` with our git dir would read our own instead.
    exclude: PathBuf,
    /// The ref this process wrote last.
    last_ref: RefCell<String>,
    /// ~/.agents relative to the work tree, when inside it (a dotfiles repo).
    agent_data: Option<PathBuf>,
}

impl Snapshots {
    /// None outside a git work tree: snapshotting any directory (say ~) could copy gigabytes.
    pub fn open(home: &Path, cwd: &Path) -> Option<Snapshots> {
        let paths = run(crate::process::command("git").arg("-C").arg(cwd).args(["rev-parse", "--show-toplevel", "--path-format=absolute", "--git-path", "info/exclude"])).ok()?;
        let (top, exclude) = paths.split_once('\n')?;
        let (work_tree, exclude) = (PathBuf::from(top), PathBuf::from(exclude));
        let git_dir = home.join(".agents/snapshots").join(work_tree.to_string_lossy().replace('/', "-"));
        // One index per process, so two agents in the same repo do not fight over index.lock; the private
        // spill dir keeps other local users from locking or planting it, and quitting removes it.
        let spills = crate::tools::spill_dir();
        crate::process::private_dir(&spills).ok()?;
        let index = spills.join("snapshot.index");
        let agent_data = home.join(".agents").canonicalize().ok().and_then(|dir| dir.strip_prefix(&work_tree).ok().map(Path::to_path_buf));
        Some(Snapshots { git_dir, work_tree, index, exclude, last_ref: RefCell::default(), agent_data })
    }

    /// Stores the work tree as it is now and returns the tree id.
    /// The tree stays reachable from a ref named after today, so gc packs it and `prune` keeps it for KEEP_DAYS.
    pub fn take(&self) -> Result<String, String> {
        if !self.git_dir.exists() {
            // Snapshots hold every file of the project, .env included, so only this user may read them.
            let parent = self.git_dir.parent().expect("git_dir is under ~/.agents/snapshots");
            std::fs::create_dir_all(parent).map_err(|e| format!("Cannot create {}: {e}", parent.display()))?;
            crate::process::private_dir(&self.git_dir)?;
            run(crate::process::command("git").args(["init", "-q", "--bare", "--shared=0600"]).arg(&self.git_dir))?;
        }
        self.copy_exclude()?;
        self.git(&["add", "-A"])?;
        let tree = self.git(&["write-tree"])?;
        let tree_ref = format!("refs/snapshots/{}/{tree}", today());
        // Most tool calls change nothing: the ref already exists and no new objects need a gc check.
        if *self.last_ref.borrow() == tree_ref {
            return Ok(tree);
        }
        self.git(&["update-ref", &tree_ref, &tree])?;
        // add and write-tree never start auto gc; it checks a count, and packs in the background.
        self.git(&["gc", "--auto", "--quiet"])?;
        *self.last_ref.borrow_mut() = tree_ref;
        Ok(tree)
    }

    /// Files hidden only by the project's info/exclude (often `.env`) must stay out of the snapshots too, and so must the agent's own data.
    fn copy_exclude(&self) -> Result<(), String> {
        let mut rules = match std::fs::read(&self.exclude) {
            Ok(rules) => rules,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(format!("Cannot read {}: {e}", self.exclude.display())),
        };
        // Each take would store these again, the snapshot stores themselves included.
        // shortcut: glob characters in `agent_data` are not escaped; such a path keeps them in the snapshots.
        if let Some(dir) = &self.agent_data {
            for data in ["sessions", "snapshots", "models"] {
                rules.extend_from_slice(format!("\n/{}/", dir.join(data).display()).as_bytes());
            }
        }
        let info = self.git_dir.join("info");
        std::fs::create_dir_all(&info).map_err(|e| format!("Cannot create {}: {e}", info.display()))?;
        let ours = info.join("exclude");
        std::fs::write(&ours, rules).map_err(|e| format!("Cannot write {}: {e}", ours.display()))
    }

    /// Paths that differ between two trees.
    pub fn changed(&self, from: &str, to: &str) -> Result<Vec<String>, String> {
        let names = self.git(&["diff", "--name-only", "--no-renames", "-z", from, to])?;
        Ok(names.split('\0').filter(|name| !name.is_empty()).map(String::from).collect())
    }

    /// The unified diff of `path` between two trees, empty when it is the same in both.
    pub fn diff(&self, from: &str, to: &str, path: &str) -> Result<String, String> {
        self.git(&["diff", "--no-color", "--no-ext-diff", "--no-renames", from, to, "--", path])
    }

    /// The blob id of `path` in `tree`, None when the tree lacks it.
    pub fn blob(&self, tree: &str, path: &str) -> Option<String> {
        self.git(&["rev-parse", "--verify", "-q", &format!("{tree}:{path}")]).ok()
    }

    /// Whether the work tree's ignore rules, as they are on disk now, ignore `path`.
    pub fn ignored(&self, path: &str) -> Result<bool, String> {
        let output = self.command(&["check-ignore", "-q", "--no-index", "--", path]).output().map_err(|e| format!("Cannot run git: {e}"))?;
        match output.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(String::from_utf8_lossy(&output.stderr).trim().to_string()),
        }
    }

    /// Writes `path` as it is in `tree`, or deletes it when the tree lacks it.
    pub fn restore(&self, tree: &str, path: &str) -> Result<(), String> {
        if self.blob(tree, path).is_some() {
            return self.git(&["--literal-pathspecs", "checkout", tree, "--", path]).map(|_| ());
        }
        let file = self.work_tree.join(path);
        match std::fs::remove_file(&file) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(format!("Cannot delete {}: {e}", file.display())),
            _ => Ok(()),
        }
    }

    fn git(&self, args: &[&str]) -> Result<String, String> {
        run(&mut self.command(args))
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = store_git(&self.git_dir);
        command.arg("--work-tree").arg(&self.work_tree);
        command.args(["-c", "core.autocrlf=false"]).args(args).env("GIT_INDEX_FILE", &self.index);
        command.current_dir(&self.work_tree);
        command
    }
}

/// Deletes snapshot days older than KEEP_DAYS from every project's store, and a whole store once none is left.
/// Takes seconds of git gc per store, so run it off the main thread.
pub fn prune(home: &Path) {
    let Ok(stores) = std::fs::read_dir(home.join(".agents/snapshots")) else { return };
    for store in stores.filter_map(|e| e.ok()).map(|e| e.path()) {
        let Some(name) = store.file_name().and_then(|name| name.to_str()).map(String::from) else { continue };
        // Store names start with '-' (the absolute path), so "trash-" is only ever a store an interrupted prune was deleting.
        if name.starts_with("trash-") {
            let _ = std::fs::remove_dir_all(&store);
            continue;
        }
        // Each failure below only costs disk space until the next prune.
        match expire(&store) {
            // Nothing taken here for KEEP_DAYS, say a removed worktree. Renamed first, so a quit halfway leaves a trash dir.
            Ok((expired, 0)) if expired > 0 => {
                let trash = store.with_file_name(format!("trash{name}"));
                if std::fs::rename(&store, &trash).is_ok() {
                    let _ = std::fs::remove_dir_all(&trash);
                }
            }
            // Auto gc only packs loose objects; the expired ones in old packs go only with a full gc.
            Ok((expired, _)) if expired > 0 => {
                let _ = store_git(&store).args(["gc", "--quiet"]).output();
            }
            _ => {}
        }
    }
}

/// Deletes the refs of days older than KEEP_DAYS; returns how many it deleted and how many are left.
fn expire(git_dir: &Path) -> Result<(usize, usize), String> {
    let refs = run(store_git(git_dir).args(["for-each-ref", "--format=%(refname)", "refs/snapshots/"]))?;
    let oldest_kept = today().saturating_sub(KEEP_DAYS);
    let (expired, kept): (Vec<&str>, Vec<&str>) = refs.lines().partition(|name| name.split('/').nth(2).and_then(|day| day.parse::<u64>().ok()).is_some_and(|day| day < oldest_kept));
    if !expired.is_empty() {
        let deletes: String = expired.iter().map(|name| format!("delete {name}\n")).collect();
        let mut child = store_git(git_dir).args(["update-ref", "--stdin"]).stdin(Stdio::piped()).stdout(Stdio::null()).spawn().map_err(|e| format!("Cannot run git: {e}"))?;
        child.stdin.take().expect("stdin is piped").write_all(deletes.as_bytes()).map_err(|e| format!("Cannot write to git update-ref: {e}"))?;
        if !child.wait().map_err(|e| format!("Cannot run git: {e}"))?.success() {
            return Err("git update-ref could not delete expired snapshots".into());
        }
    }
    Ok((expired.len(), kept.len()))
}

/// git on one snapshot store. The gc settings go on the command line: stores made before 0.1.0 saved gc.auto=0 and never packed.
fn store_git(git_dir: &Path) -> Command {
    let mut command = crate::process::command("git");
    command.arg("--git-dir").arg(git_dir);
    // Git's default waits for 6700 loose objects; packing sooner stores each edit as a small delta.
    command.args(["-c", "gc.auto=500"]);
    // Expired snapshots leave many unreachable objects; with the default two-week grace gc warns
    // into gc.log and skips auto gc for a day. An hour still covers a take between write-tree and update-ref.
    command.args(["-c", "gc.pruneExpire=1.hour.ago"]);
    command
}

/// Days since the Unix epoch, in UTC.
fn today() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).expect("clock is after 1970").as_secs() / 86_400
}

fn run(command: &mut Command) -> Result<String, String> {
    let output = command.output().map_err(|e| format!("Cannot run git: {e}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim_end_matches('\n').to_string())
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// Snapshots keeps one git index per process, so tests that take snapshots run one at a time.
    pub static SNAPSHOT_INDEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// A fresh git work tree at `<tmp>/<name>-<pid>/work`; returns the root and the work tree.
    pub fn git_work(name: &str) -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("{name}-{}", std::process::id()));
        let work = root.join("work");
        std::fs::create_dir_all(&work).unwrap();
        assert!(crate::process::command("git").arg("-C").arg(&work).args(["init", "-q"]).status().unwrap().success());
        (root, work)
    }

    #[test]
    fn restoring_a_path_with_glob_characters_touches_only_that_file() {
        let _index = SNAPSHOT_INDEX.lock().unwrap();
        let (root, work) = git_work("agent-restore-glob");
        std::fs::write(work.join("a*.txt"), "v1").unwrap();
        std::fs::write(work.join("ab.txt"), "v1").unwrap();
        let snapshots = Snapshots::open(&root, &work).unwrap();
        let tree = snapshots.take().unwrap();
        std::fs::write(work.join("a*.txt"), "v2").unwrap();
        std::fs::write(work.join("ab.txt"), "user").unwrap();
        snapshots.restore(&tree, "a*.txt").unwrap();
        let state = [std::fs::read_to_string(work.join("a*.txt")).unwrap(), std::fs::read_to_string(work.join("ab.txt")).unwrap()];
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(state, ["v1", "user"]);
    }

    #[test]
    fn snapshots_skip_files_the_project_excludes_locally() {
        let _index = SNAPSHOT_INDEX.lock().unwrap();
        let (root, work) = git_work("agent-snapshot-exclude");
        std::fs::write(work.join(".git/info/exclude"), ".env\n").unwrap();
        std::fs::write(work.join(".env"), "TOKEN=x").unwrap();
        std::fs::write(work.join("a.txt"), "a").unwrap();
        let snapshots = Snapshots::open(&root, &work).unwrap();
        let tree = snapshots.take().unwrap();
        let git_dir = std::fs::read_dir(root.join(".agents/snapshots")).unwrap().next().unwrap().unwrap().path();
        let listing = crate::process::command("git").arg("--git-dir").arg(&git_dir).args(["ls-tree", "--name-only", &tree]).output().unwrap();
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(String::from_utf8(listing.stdout).unwrap(), "a.txt\n");
    }

    #[test]
    fn snapshots_skip_the_agent_data_inside_the_work_tree() {
        let _index = SNAPSHOT_INDEX.lock().unwrap();
        // The home is the work tree, as with ~/.agents in a dotfiles repo; the store lands inside it too.
        let (root, home) = git_work("agent-snapshot-home");
        for path in [".agents/sessions/a.jsonl", ".agents/models/voice.bin", ".agents/settings.json"] {
            std::fs::create_dir_all(home.join(path).parent().unwrap()).unwrap();
            std::fs::write(home.join(path), "x").unwrap();
        }
        let snapshots = Snapshots::open(&home, &home).unwrap();
        snapshots.take().unwrap();
        let tree = snapshots.take().unwrap();
        let listing = snapshots.git(&["ls-tree", "-r", "--name-only", &tree]).unwrap();
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(listing, ".agents/settings.json");
    }

    #[test]
    fn prune_drops_expired_days_and_stores_with_none_left() {
        let _index = SNAPSHOT_INDEX.lock().unwrap();
        let (root, work) = git_work("agent-expire");
        std::fs::write(work.join("a.txt"), "v1").unwrap();
        let snapshots = Snapshots::open(&root, &work).unwrap();
        let recent = snapshots.take().unwrap();
        let git = |git_dir: &Path, args: &[&str]| run(crate::process::command("git").arg("--git-dir").arg(git_dir).args(args)).unwrap();
        let refs = |git_dir: &Path| git(git_dir, &["for-each-ref", "--format=%(refname)"]);
        let recent_ref = refs(&snapshots.git_dir);
        git(&snapshots.git_dir, &["update-ref", &format!("refs/snapshots/1/{recent}"), &recent]);
        let stores = root.join(".agents/snapshots");
        let idle = stores.join("-idle");
        run(crate::process::command("git").args(["init", "-q", "--bare"]).arg(&idle)).unwrap();
        let empty_tree = git(&idle, &["mktree"]);
        git(&idle, &["update-ref", &format!("refs/snapshots/1/{empty_tree}"), &empty_tree]);
        std::fs::create_dir(stores.join("trash-interrupted")).unwrap();

        prune(&root);
        let kept = refs(&snapshots.git_dir);
        let left = std::fs::read_dir(&stores).unwrap().count();
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!((kept, left), (recent_ref, 1));
    }
}
