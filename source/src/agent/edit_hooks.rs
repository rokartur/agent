use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use crate::tools::hashline::{self, Anchors};

const CONFIG: &str = ".agents/edit-hooks.json";
const MAX_FIX_ROUNDS: usize = 3;
const MAX_OUTPUT_CHARS: usize = 4000;

/// A command from `.agents/edit-hooks.json`, run in the directory that holds `.agents`.
#[derive(Debug)]
pub struct Hook {
    pub dir: PathBuf,
    pub command: String,
}

/// Files an edit tool call changes. Call it before the tool runs: a replace retires the anchors it names.
pub fn edited_files(tool: &str, input: &Value, cwd: &Path, anchors: &Anchors) -> Vec<PathBuf> {
    let anchor_keys: &[&str] = match tool {
        "write" | "replace_match" if input["path"].is_string() => return input["path"].as_str().map(|path| hashline::resolve(cwd, path)).into_iter().collect(),
        "replace" | "replace_match" => &["remove_from", "replace_from"],
        "insert" => &["anchor"],
        _ => return Vec::new(),
    };
    anchor_keys.iter().filter_map(|key| anchors.owner_of(input[*key].as_str()?).ok()).collect()
}

/// The onStop commands of the nearest config above each file; files without a config run nothing.
/// A broken config becomes an error for its dir only, so the other dirs still run their hooks.
/// Dirs in a project outside `trusted_projects` run none; the count says how many commands they skipped.
pub fn hooks(files: &[PathBuf], trusted_projects: &[String]) -> (Vec<Hook>, Vec<String>, usize) {
    let mut files_by_dir: BTreeMap<PathBuf, Vec<String>> = BTreeMap::new();
    for file in files {
        let Ok(file) = file.canonicalize() else { continue };
        let Some(dir) = config_dir(&file) else { continue };
        let relative = file.strip_prefix(&dir).expect("the config dir is an ancestor of the file").display().to_string();
        let group = files_by_dir.entry(dir).or_default();
        if !group.contains(&relative) {
            group.push(relative);
        }
    }
    let (mut hooks, mut errors, mut skipped) = (Vec::new(), Vec::new(), 0);
    for (dir, files) in files_by_dir {
        match dir_commands(&dir, &files) {
            Ok(commands) if !crate::agent::prompt::is_trusted(trusted_projects, &dir) => skipped += commands.len(),
            Ok(commands) => hooks.extend(commands.into_iter().map(|command| Hook { dir: dir.clone(), command })),
            Err(e) => errors.push(format!("[edit-hooks] {} is invalid: {e}", dir.join(CONFIG).display())),
        }
    }
    (hooks, errors, skipped)
}

fn dir_commands(dir: &Path, files: &[String]) -> Result<Vec<String>, String> {
    let text = std::fs::read_to_string(dir.join(CONFIG)).map_err(|e| e.to_string())?;
    let config: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    commands(&config, files)
}

/// Stops at the repository root, so a file never runs the hooks of a project around it.
/// Outside a repository there is no root, and the walk would reach configs in ~ or /tmp.
fn config_dir(file: &Path) -> Option<PathBuf> {
    let mut nearest = None;
    for dir in file.ancestors().skip(1) {
        if nearest.is_none() && dir.join(CONFIG).is_file() {
            nearest = Some(dir.to_path_buf());
        }
        if dir.join(".git").exists() {
            return nearest;
        }
    }
    None
}

/// Each file goes to the first glob it matches; a glob with a slash matches the path from the config dir.
// shortcut: serde_json sorts object keys, so "first" is key order, not file order; enable its preserve_order feature if a config needs file order.
fn commands(config: &Value, files: &[String]) -> Result<Vec<String>, String> {
    let on_stop = config["onStop"].as_object().ok_or("\"onStop\" must map globs to commands")?;
    let mut files_by_glob: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for file in files {
        let name = file.rsplit('/').next().unwrap_or(file);
        if let Some(glob) = on_stop.keys().find(|glob| matches(glob, if glob.contains('/') { file } else { name })) {
            files_by_glob.entry(glob).or_default().push(file);
        }
    }
    let mut commands = Vec::new();
    for (glob, value) in on_stop {
        let templates = match value {
            Value::String(command) => vec![command.as_str()],
            _ => value.as_array().and_then(|items| items.iter().map(Value::as_str).collect()).ok_or(format!("\"{glob}\" must be a command or a list of commands"))?,
        };
        let Some(matched) = files_by_glob.get(glob.as_str()) else { continue };
        for template in templates {
            if template.contains("{files}") && template.contains("{file}") {
                return Err(format!("\"{glob}\": a command takes {{files}} or {{file}}, not both"));
            }
            if template.contains("{files}") {
                let quoted: Vec<String> = matched.iter().map(|file| shell_quote(file)).collect();
                commands.push(template.replace("{files}", &quoted.join(" ")));
            } else if template.contains("{file}") {
                commands.extend(matched.iter().map(|file| template.replace("{file}", &shell_quote(file))));
            } else {
                commands.push(template.to_string());
            }
        }
    }
    Ok(commands)
}

pub fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// Glob over `/`-separated paths: `*` and `?` stay within a segment, `**` crosses them, `{a,b}` picks one.
fn matches(pattern: &str, path: &str) -> bool {
    if let Some((open, close, choices)) = brace_group(pattern) {
        let (head, tail) = (&pattern[..open], &pattern[close + 1..]);
        return choices.iter().any(|choice| matches(&format!("{head}{choice}{tail}"), path));
    }
    let pattern: Vec<char> = pattern.chars().collect();
    let path: Vec<char> = path.chars().collect();
    matches_chars(&pattern, &path, &mut HashSet::new())
}

/// The first `{...}` group: its byte range and its top-level choices, so `{a,{b,c}}` gives `a` and `{b,c}`.
fn brace_group(pattern: &str) -> Option<(usize, usize, Vec<&str>)> {
    let open = pattern.find('{')?;
    let (mut depth, mut start, mut choices) = (0, open + 1, Vec::new());
    for (index, c) in pattern.char_indices().skip_while(|(index, _)| *index < open) {
        match c {
            '{' => depth += 1,
            ',' if depth == 1 => {
                choices.push(&pattern[start..index]);
                start = index + 1;
            }
            '}' if depth == 1 => {
                choices.push(&pattern[start..index]);
                return Some((open, index, choices));
            }
            '}' => depth -= 1,
            _ => {}
        }
    }
    None
}

/// `failed` remembers (pattern, path) suffix lengths that did not match, so many stars cannot backtrack exponentially.
fn matches_chars(pattern: &[char], path: &[char], failed: &mut HashSet<(usize, usize)>) -> bool {
    if failed.contains(&(pattern.len(), path.len())) {
        return false;
    }
    let matched = match pattern {
        [] => path.is_empty(),
        ['*', '*', '/', rest @ ..] => matches_chars(rest, path, failed) || (0..path.len()).any(|i| path[i] == '/' && matches_chars(rest, &path[i + 1..], failed)),
        ['*', '*', rest @ ..] => (0..=path.len()).any(|i| matches_chars(rest, &path[i..], failed)),
        ['*', rest @ ..] => {
            let segment_end = path.iter().position(|c| *c == '/').unwrap_or(path.len());
            (0..=segment_end).any(|i| matches_chars(rest, &path[i..], failed))
        }
        ['?', rest @ ..] => path.first().is_some_and(|c| *c != '/') && matches_chars(rest, &path[1..], failed),
        [c, rest @ ..] => path.first() == Some(c) && matches_chars(rest, &path[1..], failed),
    };
    if !matched {
        failed.insert((pattern.len(), path.len()));
    }
    matched
}

pub fn failure(command: &str, output: &str) -> String {
    let kept: String = output.chars().take(MAX_OUTPUT_CHARS).collect();
    let cut = if kept.len() < output.len() { "\n[output cut]" } else { "" };
    format!("[edit-hooks] {command} failed:\n{kept}{cut}")
}

/// The message for the model after `round` failed hook runs, and whether the turn goes on so it can fix them.
pub fn report(failures: &[String], round: usize) -> (String, bool) {
    let failures = failures.join("\n\n");
    if round > MAX_FIX_ROUNDS {
        return (format!("Edit hooks still fail after {MAX_FIX_ROUNDS} fix rounds, stopped retrying.\n\n{failures}"), false);
    }
    (failures, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs_match_names_and_paths() {
        let cases = [
            ("*.ts", "a.ts", true),
            ("*.ts", "a.tsx", false),
            ("*.ts", "src/a.ts", false),
            ("*.{ts,tsx}", "a.tsx", true),
            ("*.{ts,tsx}", "a.js", false),
            ("?.rs", "a.rs", true),
            ("?.rs", "ab.rs", false),
            ("src/**/*.rs", "src/a.rs", true),
            ("src/**/*.rs", "src/x/y/a.rs", true),
            ("src/**/*.rs", "lib/a.rs", false),
            ("src/*", "src/x/a.rs", false),
            ("**", "a/b/c", true),
            ("*.{rs,{ts,tsx}}", "a.tsx", true),
            ("*.{rs,{ts,tsx}}", "a.{ts", false),
            (&"*a".repeat(30), &"a".repeat(60), true),
            (&format!("{}b", "*a".repeat(30)), &"a".repeat(60), false),
        ];
        for (pattern, path, expected) in cases {
            assert_eq!(matches(pattern, path), expected, "{pattern} vs {path}");
        }
    }

    #[test]
    fn files_run_the_nearest_config_once_per_first_matching_glob() {
        let root = std::env::temp_dir().join(format!("agent-edit-hooks-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let write = |path: &str, text: &str| {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        std::fs::create_dir_all(root.join(".git")).unwrap();
        write(".agents/edit-hooks.json", r#"{"onStop": {"*.ts": ["fmt {files}", "check {file}", "tsc"], "*.md": "fmt {files}", "src/**": "never"}}"#);
        write("pkg/.agents/edit-hooks.json", r#"{"onStop": {"*": "lint {files}"}}"#);
        write("broken/.agents/edit-hooks.json", "{");
        for file in ["a.ts", "it's.ts", "README.md", "pkg/b.rs", "notes.txt", "broken/c.rs"] {
            write(file, "");
        }
        let files: Vec<PathBuf> = ["a.ts", "it's.ts", "README.md", "pkg/b.rs", "notes.txt", "a.ts", "deleted.ts", "broken/c.rs"].iter().map(|f| root.join(f)).collect();
        let (hooks, errors, _) = hooks(&files, &[root.display().to_string()]);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("broken/.agents/edit-hooks.json is invalid"), "{errors:?}");
        let commands: Vec<(&Path, &str)> = hooks.iter().map(|hook| (hook.dir.as_path(), hook.command.as_str())).collect();
        let pkg = root.join("pkg");
        assert_eq!(
            commands,
            [
                (root.as_path(), "fmt 'README.md'"),
                (root.as_path(), r"fmt 'a.ts' 'it'\''s.ts'"),
                (root.as_path(), "check 'a.ts'"),
                (root.as_path(), r"check 'it'\''s.ts'"),
                (root.as_path(), "tsc"),
                (pkg.as_path(), "lint 'b.rs'"),
            ]
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_repository_outside_trusted_projects_runs_no_hooks_and_counts_them() {
        let root = std::env::temp_dir().join(format!("agent-edit-hooks-untrusted-{}", std::process::id()));
        let vendored = root.join("vendor/lib");
        for dir in [root.join(".agents"), root.join(".git"), vendored.join(".agents"), vendored.join(".git")] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let root = root.canonicalize().unwrap();
        std::fs::write(root.join(CONFIG), r#"{"onStop": {"*": "fmt {file}"}}"#).unwrap();
        std::fs::write(vendored.join(CONFIG), r#"{"onStop": {"*": ["touch pwned", "fmt {file}"]}}"#).unwrap();
        let files = [root.join("a.txt"), vendored.join("b.txt")];
        for file in &files {
            std::fs::write(file, "").unwrap();
        }
        let (hooks, _, skipped) = hooks(&files, &[root.display().to_string()]);
        std::fs::remove_dir_all(&root).unwrap();
        let commands: Vec<&str> = hooks.iter().map(|hook| hook.command.as_str()).collect();
        assert_eq!((commands, skipped), (vec!["fmt 'a.txt'"], 2));
    }

    #[test]
    fn files_outside_a_repository_run_no_hooks() {
        let root = std::env::temp_dir().join(format!("agent-edit-hooks-norepo-{}", std::process::id()));
        std::fs::create_dir_all(root.join(".agents")).unwrap();
        std::fs::write(root.join(CONFIG), r#"{"onStop": {"*": "touch pwned"}}"#).unwrap();
        std::fs::write(root.join("a.txt"), "").unwrap();
        let (hooks, _, _) = hooks(&[root.join("a.txt")], &[root.display().to_string()]);
        std::fs::remove_dir_all(&root).unwrap();
        assert!(hooks.is_empty());
    }

    #[test]
    fn failures_go_back_to_the_model_for_three_rounds() {
        let failures = [failure("tsc", "error TS2322")];
        assert_eq!(report(&failures, 1), ("[edit-hooks] tsc failed:\nerror TS2322".to_string(), true));
        assert!(report(&failures, 3).1);
        let (message, retry) = report(&failures, 4);
        assert!(!retry);
        assert!(message.starts_with("Edit hooks still fail after 3 fix rounds"), "{message}");
    }
}
