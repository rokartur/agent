use std::cell::RefCell;
use std::fmt::Write as _;
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
        let snapshots = Snapshots { git_dir, work_tree, index, exclude, last_ref: RefCell::default() };
        if snapshots.git_dir.exists() {
            // Only costs disk space when it fails, so it never turns snapshots off.
            let _ = snapshots.expire();
        }
        Some(snapshots)
    }

    /// Stores the work tree as it is now and returns the tree id.
    /// The tree stays reachable from a ref named after today, so gc packs it and keeps it for KEEP_DAYS.
    pub fn take(&self) -> Result<String, String> {
        if !self.git_dir.exists() {
            // Snapshots hold every file of the project, .env included, so only this user may read them.
            let parent = self.git_dir.parent().expect("git_dir is under ~/.agents/snapshots");
            std::fs::create_dir_all(parent).map_err(|e| format!("Cannot create {}: {e}", parent.display()))?;
            crate::process::private_dir(&self.git_dir)?;
            run(crate::process::command("git").args(["init", "-q", "--bare", "--shared=0600"]).arg(&self.git_dir))?;
            // Git's default waits for 6700 loose objects; packing sooner stores each edit as a small delta.
            self.git(&["config", "gc.auto", "500"])?;
            // Expired snapshots leave many unreachable objects; with the default two-week grace gc warns
            // into gc.log and skips auto gc for a day. An hour still covers a take between write-tree and update-ref.
            self.git(&["config", "gc.pruneExpire", "1.hour.ago"])?;
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

    /// Files hidden only by the project's info/exclude (often `.env`) must stay out of the snapshots too.
    fn copy_exclude(&self) -> Result<(), String> {
        let rules = match std::fs::read(&self.exclude) {
            Ok(rules) => rules,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(format!("Cannot read {}: {e}", self.exclude.display())),
        };
        let info = self.git_dir.join("info");
        std::fs::create_dir_all(&info).map_err(|e| format!("Cannot create {}: {e}", info.display()))?;
        let ours = info.join("exclude");
        std::fs::write(&ours, rules).map_err(|e| format!("Cannot write {}: {e}", ours.display()))
    }

    /// Deletes the refs of days older than KEEP_DAYS; the next auto gc prunes their objects.
    fn expire(&self) -> Result<(), String> {
        let refs = self.git(&["for-each-ref", "--format=%(refname)", "refs/snapshots/"])?;
        let oldest_kept = today().saturating_sub(KEEP_DAYS);
        let mut deletes = String::new();
        for name in refs.lines() {
            let day = name.split('/').nth(2).and_then(|day| day.parse::<u64>().ok());
            if day.is_some_and(|day| day < oldest_kept) {
                writeln!(deletes, "delete {name}").expect("writing to a String cannot fail");
            }
        }
        if !deletes.is_empty() {
            let mut command = self.command(&["update-ref", "--stdin"]);
            let mut child = command.stdin(Stdio::piped()).stdout(Stdio::null()).spawn().map_err(|e| format!("Cannot run git: {e}"))?;
            child.stdin.take().expect("stdin is piped").write_all(deletes.as_bytes()).map_err(|e| format!("Cannot write to git update-ref: {e}"))?;
            if !child.wait().map_err(|e| format!("Cannot run git: {e}"))?.success() {
                return Err("git update-ref could not delete expired snapshots".into());
            }
        }
        Ok(())
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
        let mut command = crate::process::command("git");
        command.arg("--git-dir").arg(&self.git_dir).arg("--work-tree").arg(&self.work_tree);
        command.args(["-c", "core.autocrlf=false"]).args(args).env("GIT_INDEX_FILE", &self.index);
        command.current_dir(&self.work_tree);
        command
    }
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
    fn opening_snapshots_drops_trees_older_than_the_kept_days() {
        let _index = SNAPSHOT_INDEX.lock().unwrap();
        let (root, work) = git_work("agent-expire");
        std::fs::write(work.join("a.txt"), "v1").unwrap();
        let recent = Snapshots::open(&root, &work).unwrap().take().unwrap();
        let git_dir = std::fs::read_dir(root.join(".agents/snapshots")).unwrap().next().unwrap().unwrap().path();
        let refs = || {
            let output = crate::process::command("git").arg("--git-dir").arg(&git_dir).args(["for-each-ref", "--format=%(refname)"]).output().unwrap();
            String::from_utf8(output.stdout).unwrap().lines().map(String::from).collect::<Vec<_>>()
        };
        let recent_ref = refs().pop().unwrap();
        let old_ref = format!("refs/snapshots/1/{recent}");
        assert!(crate::process::command("git").arg("--git-dir").arg(&git_dir).args(["update-ref", &old_ref, &recent]).status().unwrap().success());

        Snapshots::open(&root, &work).unwrap();
        let kept = refs();
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(kept, [recent_ref]);
    }
}
