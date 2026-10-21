//! GitHub code URLs: a shallow clone the agent can explore, summarised as tree, listing, README or file.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const CLONE_REUSE: Duration = Duration::from_secs(3600);
const CLONE_TIMEOUT: Duration = Duration::from_secs(120);
const NOISE: [&str; 15] = ["node_modules", "vendor", ".next", "dist", "build", "__pycache__", ".venv", "venv", ".tox", ".mypy_cache", ".pytest_cache", "target", ".gradle", ".idea", ".vscode"];
const TREE_LIMIT: usize = 200;
const README_CHARS: usize = 8192;
const FILE_CHARS: usize = 100_000;
const FOOTER: &str = "Use the Read and Bash tools at the path above to explore further. The clone is a cache that forceClone replaces; do not edit it.";

#[derive(Debug, PartialEq)]
pub enum Kind {
    Root,
    Tree,
    Blob,
}

#[derive(Debug, PartialEq)]
pub struct Repo {
    pub owner: String,
    pub repo: String,
    pub kind: Kind,
    pub git_ref: Option<String>,
    pub path: String,
}

/// pi's parseGitHubUrl: None for non-GitHub URLs and for pages other than `tree` or `blob` (issues, pulls, ...).
pub fn parse(url: &str) -> Option<Repo> {
    if !matches!(super::host(url)?.as_str(), "github.com" | "www.github.com") {
        return None;
    }
    let path = url.split_once("://")?.1.split(['?', '#']).next()?;
    let segments: Vec<String> = path.split('/').skip(1).filter(|s| !s.is_empty()).map(percent_decode).collect::<Option<_>>()?;
    let [owner, repo, rest @ ..] = segments.as_slice() else { return None };
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    let owner_ok = owner.len() <= 39 && !owner.contains("--") && !owner.starts_with('-') && !owner.ends_with('-') && owner.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    let repo_ok = (1..=100).contains(&repo.len()) && repo != "." && repo != ".." && repo.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c));
    if !owner_ok || !repo_ok {
        return None;
    }
    let (owner, repo) = (owner.clone(), repo.to_string());
    let Some(kind) = rest.first() else { return Some(Repo { owner, repo, kind: Kind::Root, git_ref: None, path: String::new() }) };
    let kind = match kind.as_str() {
        "tree" => Kind::Tree,
        "blob" => Kind::Blob,
        _ => return None,
    };
    let git_ref = rest.get(1)?;
    // A leading `-` would reach `git fetch` as an option.
    if git_ref.len() > 1024 || git_ref.starts_with('-') || git_ref.chars().any(char::is_control) {
        return None;
    }
    let path = rest[2..].join("/");
    // The path is joined onto the clone dir: `..` or a leading `/` (also percent-encoded) would escape it.
    if path.starts_with(['/', '\\']) || path.split(['/', '\\']).any(|part| part == "..") {
        return None;
    }
    Some(Repo { owner, repo, kind, git_ref: Some(git_ref.clone()), path })
}

fn percent_decode(segment: &str) -> Option<String> {
    let bytes = segment.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            out.push(crate::api::oauth::hex_byte(&bytes[i + 1..])?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Clones (or reuses) the repository and describes the part the URL points at.
pub fn fetch(repo: &Repo, force_clone: bool) -> Result<String, String> {
    let dir = clone(repo, force_clone)?;
    Ok(describe(repo, &dir))
}

/// shortcut: plain git with the user's credential helpers and no repository size check, so private repos the user can reach are cloned.
fn clone(repo: &Repo, force_clone: bool) -> Result<PathBuf, String> {
    let git_ref = repo.git_ref.as_deref().unwrap_or("HEAD");
    let safe_ref: String = git_ref.chars().map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '_' }).collect();
    // Not the shared temp dir: on Linux another user could plant a clone there.
    let home = std::env::var_os("HOME").ok_or("HOME is not set.")?;
    // safe_ref maps `feature/x` and `feature_x` alike, so a hash of the real ref keeps them apart.
    let ref_id = &crate::process::hash(git_ref)["sha256:".len()..][..12];
    let dir = PathBuf::from(home).join(".cache/agent-github").join(&repo.owner).join(format!("{}@{safe_ref}-{ref_id}", repo.repo));
    // A branch moves, so a clone answers for an hour.
    let age = std::fs::metadata(dir.join(".git")).and_then(|meta| meta.modified()).ok().and_then(|modified| modified.elapsed().ok());
    if age.is_some_and(|age| age < CLONE_REUSE) && !force_clone {
        return Ok(dir);
    }
    // Each fetch checks out into its own dir, so a concurrent or interrupted one never leaves a half tree at `dir`.
    let partial = PathBuf::from(format!("{}.partial-{}-{:?}", dir.display(), std::process::id(), std::thread::current().id()));
    let url = format!("https://github.com/{}/{}.git", repo.owner, repo.repo);
    if let Err(error) = checkout(&url, git_ref, &partial) {
        _ = std::fs::remove_dir_all(&partial);
        return Err(error);
    }
    // Moved aside, not deleted in place: a parallel fetch of the same repo may still be reading `dir`.
    let old = PathBuf::from(format!("{}.old-{}-{:?}", dir.display(), std::process::id(), std::thread::current().id()));
    _ = std::fs::rename(&dir, &old);
    let moved = std::fs::rename(&partial, &dir);
    _ = std::fs::remove_dir_all(&old);
    if let Err(error) = moved {
        _ = std::fs::remove_dir_all(&partial);
        // A concurrent fetch may have won the rename; its tree serves this request.
        if !dir.join(".git").is_dir() {
            return Err(format!("Cannot move the clone to {}: {error}", dir.display()));
        }
    }
    Ok(dir)
}

/// Fetches `git_ref` (branch, tag, commit or HEAD) at depth 1 into `dir`.
fn checkout(remote: &str, git_ref: &str, dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("Cannot create {}: {e}", dir.display()))?;
    let deadline = Instant::now() + CLONE_TIMEOUT;
    for args in [&["init", "-q"][..], &["fetch", "-q", "--depth", "1", remote, git_ref], &["checkout", "-q", "FETCH_HEAD"]] {
        git(dir, args, deadline)?;
    }
    Ok(())
}

fn git(dir: &Path, args: &[&str], deadline: Instant) -> Result<(), String> {
    let mut git = crate::process::command("git");
    // A link to /etc/passwd or /proc/self/environ must not be followed when the clone is read.
    git.arg("-C").arg(dir).args(["-c", "core.symlinks=false"]).args(args).env("GIT_TERMINAL_PROMPT", "0");
    // Set inside a git hook, these would point init, fetch and checkout at the user's own repository.
    git.env_remove("GIT_DIR").env_remove("GIT_WORK_TREE").env_remove("GIT_INDEX_FILE");
    let output = crate::process::output_within(&mut git, deadline.saturating_duration_since(Instant::now()))
        .map_err(|e| format!("git: {e}"))?
        .ok_or_else(|| format!("git {} timed out after {} s.", args[0], CLONE_TIMEOUT.as_secs()))?;
    if !output.status.success() {
        return Err(format!("git {} failed: {}", args[0], String::from_utf8_lossy(&output.stderr).trim()));
    }
    Ok(())
}

fn describe(repo: &Repo, dir: &Path) -> String {
    let mut lines = vec![format!("Repository cloned to: {}", dir.display()), String::new()];
    let target = dir.join(&repo.path);
    let not_found = format!("Path `{}` not found in clone. Showing repository root instead.", repo.path);
    match repo.kind {
        Kind::Root => {
            lines.extend(["## Structure".to_string(), tree(dir), String::new()]);
            if let Some((name, readme)) = readme(dir) {
                lines.extend([format!("## {name}"), readme, String::new()]);
            }
        }
        Kind::Tree if !target.exists() => lines.extend([not_found, String::new(), "## Structure".into(), tree(dir), String::new()]),
        Kind::Tree => {
            let shown = if repo.path.is_empty() { "/" } else { &repo.path };
            lines.extend([format!("## {shown}"), listing(&target), String::new()]);
        }
        Kind::Blob if !target.exists() => lines.extend([not_found, String::new()]),
        Kind::Blob if target.is_dir() => lines.extend([format!("## {}", repo.path), listing(&target), String::new()]),
        Kind::Blob => match std::fs::read(&target).ok().filter(|b| !b.iter().take(512).any(|&byte| byte == 0)).and_then(|b| String::from_utf8(b).ok()) {
            None => lines.extend([format!("Could not read `{}` as UTF-8 text.", repo.path), String::new()]),
            Some(text) if text.chars().count() > FILE_CHARS => lines.extend([
                format!("## {}", repo.path),
                super::char_slice(&text, 0, FILE_CHARS).to_string(),
                String::new(),
                format!("[File truncated at {FILE_CHARS} chars. Full file: {}]", target.display()),
                String::new(),
            ]),
            Some(text) => lines.extend([format!("## {}", repo.path), text, String::new()]),
        },
    }
    lines.push(FOOTER.into());
    lines.join("\n")
}

/// Depth-first sorted tree without `.git`; noise directories are named but not entered.
fn tree(root: &Path) -> String {
    let mut lines = Vec::new();
    walk(root, root, &mut lines);
    if lines.len() > TREE_LIMIT {
        lines.truncate(TREE_LIMIT);
        lines.push(format!("... (truncated at {TREE_LIMIT} entries)"));
    }
    lines.join("\n")
}

fn walk(root: &Path, dir: &Path, lines: &mut Vec<String>) {
    for path in sorted(dir) {
        if lines.len() > TREE_LIMIT {
            return;
        }
        let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        if name == ".git" {
            continue;
        }
        let rel = path.strip_prefix(root).unwrap_or(&path).display().to_string();
        if !path.is_dir() {
            lines.push(rel);
        } else if NOISE.contains(&name.as_str()) {
            lines.push(format!("{rel}/  [skipped]"));
        } else {
            lines.push(format!("{rel}/"));
            walk(root, &path, lines);
        }
    }
}

fn listing(dir: &Path) -> String {
    let lines: Vec<String> = sorted(dir)
        .into_iter()
        .map(|path| {
            let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
            match std::fs::metadata(&path) {
                Ok(meta) if meta.is_file() => format!("  {name}  ({})", crate::tools::format_size(meta.len() as usize)),
                _ => format!("  {name}/"),
            }
        })
        .collect();
    lines.join("\n")
}

fn sorted(dir: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir).into_iter().flatten().flatten().map(|entry| entry.path()).collect();
    paths.sort();
    paths
}

fn readme(dir: &Path) -> Option<(&'static str, String)> {
    ["README.md", "readme.md", "README", "README.txt", "README.rst"].into_iter().find_map(|name| {
        let text = std::fs::read_to_string(dir.join(name)).ok()?;
        if text.chars().count() <= README_CHARS {
            return Some((name, text));
        }
        Some((name, format!("{}\n\n[README truncated at {README_CHARS} chars]", super::char_slice(&text, 0, README_CHARS))))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_code_urls_and_refuses_the_rest() {
        let blob = parse("https://github.com/rust-lang/rust/blob/main/src/lib%20x.rs?plain=1").unwrap();
        assert_eq!(blob, Repo { owner: "rust-lang".into(), repo: "rust".into(), kind: Kind::Blob, git_ref: Some("main".into()), path: "src/lib x.rs".into() });
        assert_eq!(parse("https://www.github.com/a/b.git").unwrap().kind, Kind::Root);
        assert!(parse("https://github.com/a/b/issues/1").is_none());
        assert!(parse("https://github.com/a--b/c").is_none());
        assert!(parse("https://github.com/a/..").is_none());
        assert!(parse("https://github.com/a/b/blob/main/%2e%2e/%2e%2e/etc/passwd").is_none());
        assert!(parse("https://github.com/a/b/tree/--upload-pack=x/src").is_none());
        assert!(parse("https://github.com/a/b/tree/%2D%2Dupload-pack=x").is_none());
        assert!(parse("https://github.com/a/b/blob/main/x%+1").is_none());
        assert!(parse("https://github.com/a").is_none());
        assert!(parse("https://gitlab.com/a/b").is_none());
    }

    #[test]
    fn describe_shows_root_tree_with_noise_skipped_and_readme() {
        let dir = std::env::temp_dir().join(format!("agent-github-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("node_modules/x")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.join("README.md"), "# Hi").unwrap();
        let repo = Repo { owner: "o".into(), repo: "r".into(), kind: Kind::Root, git_ref: None, path: String::new() };
        let text = describe(&repo, &dir);
        let expected = format!("Repository cloned to: {}\n\n## Structure\nREADME.md\nnode_modules/  [skipped]\nsrc/\nsrc/main.rs\n\n## README.md\n# Hi\n\n{FOOTER}", dir.display());
        assert_eq!(text, expected);
        let tree = Repo { kind: Kind::Tree, path: "src".into(), git_ref: Some("main".into()), ..repo };
        assert!(describe(&tree, &dir).contains("## src\n  main.rs  (12B)\n"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn checkout_fetches_a_branch_or_a_commit_and_keeps_symlinks_as_plain_files() {
        let root = std::env::temp_dir().join(format!("agent-github-checkout-{}", std::process::id()));
        let remote = root.join("remote");
        std::fs::create_dir_all(&remote).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", remote.join("README.md")).unwrap();
        let run = |args: &[&str]| {
            let output = crate::process::command("git").arg("-C").arg(&remote).args(args).output().unwrap();
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
            String::from_utf8(output.stdout).unwrap()
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["add", "."]);
        run(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "x"]);
        let sha = run(&["rev-parse", "HEAD"]);

        for git_ref in ["main", sha.trim()] {
            let dir = root.join(git_ref);
            checkout(remote.to_str().unwrap(), git_ref, &dir).unwrap();
            assert!(std::fs::symlink_metadata(dir.join("README.md")).unwrap().is_file());
            assert_eq!(std::fs::read_to_string(dir.join("README.md")).unwrap(), "/etc/passwd");
        }
        assert!(checkout(remote.to_str().unwrap(), "missing", &root.join("missing")).unwrap_err().starts_with("git fetch failed:"));
        std::fs::remove_dir_all(&root).unwrap();
    }
}
