use std::path::{Path, PathBuf};

/// One checkout from `git worktree list --porcelain`.
#[derive(Clone, Debug, PartialEq)]
pub struct Worktree {
    pub path: PathBuf,
    pub branch: String,
}

/// Every checkout, main one first, and the one `cwd` is in.
pub fn list(cwd: &Path) -> Result<(Vec<Worktree>, PathBuf), String> {
    // -z: a path holding a newline would split a block in the plain porcelain.
    let porcelain = git(cwd, &["worktree", "list", "--porcelain", "-z"])?;
    // Both come from git as real paths, so they compare even when cwd went through a symlink.
    let current = PathBuf::from(git(cwd, &["rev-parse", "--show-toplevel"])?.trim());
    Ok((parse(&porcelain), current))
}

/// The worktree `name` names (branch or directory), created as `<main>-<name>` when none does.
pub fn open(cwd: &Path, worktrees: &[Worktree], name: &str) -> Result<PathBuf, String> {
    // "detached" is the label `parse` gives a checkout without a branch, not a branch name.
    let existing = worktrees.iter().find(|w| (w.branch == name && name != "detached") || w.path.file_name().is_some_and(|n| n == name));
    if let Some(worktree) = existing {
        return Ok(worktree.path.clone());
    }
    // `list` puts the main checkout first, as `git worktree list` does.
    let main = worktrees.first().ok_or("No worktrees listed.")?;
    let path = format!("{}-{}", main.path.display(), name.replace('/', "-"));
    let branch_exists = git(cwd, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{name}")]).is_ok();
    let args: Vec<&str> = if branch_exists { vec!["worktree", "add", &path, name] } else { vec!["worktree", "add", "-b", name, &path] };
    git(cwd, &args)?;
    Ok(PathBuf::from(path))
}

/// git refuses a checkout with changes, staged or not, or untracked files; ignored files such as `.env` go with it.
pub fn remove(cwd: &Path, path: &Path) -> Result<(), String> {
    git(cwd, &["worktree", "remove", &path.display().to_string()]).map(|_| ())
}

/// A worktree EnterWorktree made: its own branch, started at `base`.
#[derive(Clone, Debug)]
pub struct Created {
    pub path: PathBuf,
    pub branch: String,
    pub base: String,
}

/// Claude Code's EnterWorktree: `.agents/worktrees/<name>` in the main checkout on a new branch `worktree-<name>`,
/// from origin's default branch, or from HEAD in a repo without one.
pub fn create(cwd: &Path, name: &str) -> Result<Created, String> {
    let (worktrees, _) = list(cwd)?;
    let main = &worktrees.first().ok_or("No worktrees listed.")?.path;
    let path = main.join(".agents/worktrees").join(name);
    let branch = format!("worktree-{name}");
    let start = if git(cwd, &["rev-parse", "--verify", "--quiet", "origin/HEAD"]).is_ok() { "origin/HEAD" } else { "HEAD" };
    let base = git(cwd, &["rev-parse", start])?.trim().to_string();
    hide_worktrees_dir(main)?;
    git(cwd, &["worktree", "add", "-b", &branch, &path.display().to_string(), &base])?;
    Ok(Created { path, branch, base })
}

/// Keeps `.agents/worktrees/` out of the main checkout's `git status` without touching the project's `.gitignore`.
fn hide_worktrees_dir(main: &Path) -> Result<(), String> {
    const PATTERN: &str = "/.agents/worktrees/";
    let exclude = main.join(git(main, &["rev-parse", "--git-path", "info/exclude"])?.trim());
    let current = match std::fs::read(&exclude) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(format!("Cannot read {}: {e}", exclude.display())),
    };
    if current.split(|byte| *byte == b'\n').any(|line| line == PATTERN.as_bytes()) {
        return Ok(());
    }
    let separator = if current.is_empty() || current.ends_with(b"\n") { "" } else { "\n" };
    std::fs::create_dir_all(exclude.parent().ok_or("info/exclude has no parent")?).map_err(|e| e.to_string())?;
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(&exclude).map_err(|e| format!("Cannot write {}: {e}", exclude.display()))?;
    std::io::Write::write_all(&mut file, format!("{separator}{PATTERN}\n").as_bytes()).map_err(|e| format!("Cannot write {}: {e}", exclude.display()))
}

/// Uncommitted files and commits made since `created.base`, as git lists them; empty when there are none.
pub fn changes(created: &Created) -> Result<String, String> {
    let status = git(&created.path, &["status", "--porcelain"])?;
    let commits = git(&created.path, &["log", "--oneline", &format!("{}..HEAD", created.base)])?;
    Ok(format!("{status}{commits}").trim_end().to_string())
}

/// Removes the worktree even with changes, then its branch.
pub fn discard(main: &Path, created: &Created) -> Result<(), String> {
    git(main, &["worktree", "remove", "--force", &created.path.display().to_string()])?;
    git(main, &["branch", "-D", &created.branch]).map(|_| ())
}

fn parse(porcelain: &str) -> Vec<Worktree> {
    porcelain
        .trim_end_matches('\0')
        .split("\0\0")
        .map(|block| {
            let path = block.split('\0').next().unwrap_or_default().trim_start_matches("worktree ");
            let branch = block.split('\0').find_map(|l| l.strip_prefix("branch refs/heads/")).unwrap_or("detached");
            Worktree { path: PathBuf::from(path), branch: branch.to_string() }
        })
        .collect()
}

fn git(cwd: &Path, args: &[&str]) -> Result<String, String> {
    // Runs on the UI thread; a checkout with a slow post-checkout hook must not freeze it for good.
    let output = crate::process::output_within(crate::process::command("git").args(args).current_dir(cwd), std::time::Duration::from_secs(120))
        .map_err(|e| format!("Cannot run git: {e}"))?
        .ok_or("git timed out after 120 s.")?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_branches_and_detached_checkouts() {
        let porcelain = "worktree /r/app\0HEAD abc\0branch refs/heads/main\0\0worktree /r/a\npp-feat\0HEAD def\0branch refs/heads/feat/x\0\0worktree /r/app-old\0HEAD 123\0detached\0\0";
        let worktree = |path: &str, branch: &str| Worktree { path: PathBuf::from(path), branch: branch.into() };
        assert_eq!(parse(porcelain), [worktree("/r/app", "main"), worktree("/r/a\npp-feat", "feat/x"), worktree("/r/app-old", "detached")]);
    }

    #[test]
    fn remove_refuses_changes_but_not_ignored_files() {
        let main = std::env::temp_dir().join(format!("agent-worktree-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&main);
        std::fs::create_dir_all(&main).unwrap();
        for args in [&["init", "-q", "-b", "main"][..], &["-c", "user.email=a@b.c", "-c", "user.name=a", "commit", "-q", "--allow-empty", "-m", "a"]] {
            git(&main, args).unwrap();
        }
        let checkout = open(&main, &list(&main).unwrap().0, "side").unwrap();
        // info/exclude, not .gitignore: an untracked .gitignore would make git refuse the removal by itself.
        std::fs::write(main.join(".git/info/exclude"), ".env\n").unwrap();
        std::fs::write(checkout.join(".env"), "KEY=1").unwrap();
        std::fs::write(checkout.join("notes.md"), "draft").unwrap();
        assert!(remove(&main, &checkout).is_err());
        std::fs::remove_file(checkout.join("notes.md")).unwrap();
        remove(&main, &checkout).unwrap();
        assert!(!checkout.exists());
        std::fs::remove_dir_all(&main).unwrap();
    }

    #[test]
    fn creating_a_worktree_keeps_exclude_rules_that_are_not_utf8() {
        let main = std::env::temp_dir().join(format!("agent-worktree-exclude-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&main);
        std::fs::create_dir_all(&main).unwrap();
        for args in [&["init", "-q", "-b", "main"][..], &["-c", "user.email=a@b.c", "-c", "user.name=a", "commit", "-q", "--allow-empty", "-m", "a"]] {
            git(&main, args).unwrap();
        }
        std::fs::write(main.join(".git/info/exclude"), b".env\n\xff\xfe\n").unwrap();
        create(&main, "one").unwrap();
        create(&main, "two").unwrap();
        assert_eq!(std::fs::read(main.join(".git/info/exclude")).unwrap(), b".env\n\xff\xfe\n/.agents/worktrees/\n");
        std::fs::remove_dir_all(&main).unwrap();
    }

    #[test]
    fn created_worktrees_stay_out_of_the_main_status() {
        let main = std::env::temp_dir().join(format!("agent-worktree-create-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&main);
        std::fs::create_dir_all(&main).unwrap();
        for args in [&["init", "-q", "-b", "main"][..], &["-c", "user.email=a@b.c", "-c", "user.name=a", "commit", "-q", "--allow-empty", "-m", "a"]] {
            git(&main, args).unwrap();
        }
        std::fs::write(main.join(".git/info/exclude"), "# kept").unwrap();
        let created = create(&main, "one").unwrap();
        create(&main, "two").unwrap();
        assert_eq!(git(&main, &["status", "--porcelain"]).unwrap(), "");
        assert_eq!(std::fs::read_to_string(main.join(".git/info/exclude")).unwrap(), "# kept\n/.agents/worktrees/\n");
        discard(&main, &created).unwrap();
        std::fs::remove_dir_all(&main).unwrap();
    }
}
