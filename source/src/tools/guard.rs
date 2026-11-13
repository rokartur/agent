use serde_json::{Value, json};

// pi's audit of 53k bash calls: 10k were cat/sed -n/head file reads (a third of the "edit without read" failures),
// and blind `sleep N` burned 41h of wall time.
const MIN_BLOCKED_SLEEP_SECS: u64 = 15;
const HEAD_DEFAULT_LINES: u64 = 10;

/// Why bash should not run `command` (a blind sleep, a merge that ignores CI or an in-place edit of one file), or None when it may run.
/// File reads chained with other commands run as written: refusing them cost every benchmark run 1-3 turns,
/// and Edit by `file_path` needs no anchors.
pub fn bash(command: &str) -> Option<String> {
    if blind_sleep(command) {
        return Some("No blind sleep. Server: curl -sf --retry 30 --retry-all-errors --retry-delay 1 URL. CI: gh run watch --exit-status, gh pr checks --watch (rerun it when it says no checks reported yet). Long job: Bash run_in_background.".into());
    }
    if unchecked_merge(command) {
        return Some("gh pr merge would run whatever gh pr checks returned. Gate it on the exit status: gh pr checks N --watch --fail-fast && gh pr merge N ...".into());
    }
    if in_place_edit(command) {
        return Some("Edit one file with Edit (`edits` for several changes), ReplaceLines or InsertLines: they run the edit checks and show new language server errors. sed -i and perl -i stay for many files at once (a glob, xargs, find -exec) or on another machine.".into());
    }
    None
}

/// `sed -i` or `perl -i` on one named file. A glob, a variable, xargs, find -exec, ssh or offload passes:
/// Edit cannot reach those files in one call.
fn in_place_edit(command: &str) -> bool {
    if command.contains("ssh ") || command.contains("offload ") {
        return false;
    }
    shell_segments(command).iter().any(|words| {
        let start = words.iter().position(|word| !word.contains('=') && !matches!(word.as_str(), "do" | "then" | "else")).unwrap_or(words.len());
        let [tool, args @ ..] = &words[start..] else { return false };
        let in_place = match tool.as_str() {
            "sed" => args.iter().any(|arg| arg.starts_with("--in-place") || short_flags(arg, char::is_ascii_alphanumeric).contains('i')),
            // Perl's uppercase switches take a glued argument, as in -MList::Util.
            "perl" => args.iter().any(|arg| short_flags(arg, |ch| ch.is_ascii_lowercase() || ch.is_ascii_digit()).contains('i')),
            _ => false,
        };
        let file = args.iter().rev().find(|arg| !arg.contains(['>', '<']));
        in_place && file.is_some_and(|file| !file.contains(['*', '?', '[', '{', '$', '`']))
    })
}

/// The switch letters a `-xyz` word starts with, up to the first one `is_switch` rejects; empty for `--long` and non-flags.
fn short_flags(word: &str, is_switch: impl Fn(&char) -> bool) -> String {
    match word.strip_prefix('-') {
        Some(flags) if !flags.starts_with('-') => flags.chars().take_while(is_switch).collect(),
        _ => String::new(),
    }
}

/// The words of each command `;`, `&`, `|` or a newline separates, quotes kept, so a quoted script stays one word.
fn shell_segments(command: &str) -> Vec<Vec<String>> {
    let mut segments = vec![Vec::new()];
    let mut word = String::new();
    let mut quote = None;
    let mut chars = command.chars();
    while let Some(ch) = chars.next() {
        if quote.is_none() && (ch.is_whitespace() || ";&|".contains(ch)) {
            if !word.is_empty() {
                segments.last_mut().unwrap().push(std::mem::take(&mut word));
            }
            if ch != ' ' && ch != '\t' {
                segments.push(Vec::new());
            }
            continue;
        }
        word.push(ch);
        match quote {
            Some(open) if ch == open => quote = None,
            Some('\'') => {}
            _ if ch == '\\' => word.extend(chars.next()),
            None if ch == '\'' || ch == '"' => quote = Some(ch),
            _ => {}
        }
    }
    if !word.is_empty() {
        segments.last_mut().unwrap().push(word);
    }
    segments
}

/// `gh pr merge` reached from `gh pr checks` through `;`, a newline or a pipe, which drop the checks' exit status.
/// A script that reads `$?` gates the merge itself.
fn unchecked_merge(command: &str) -> bool {
    let Some(merge) = command.find("gh pr merge") else { return false };
    let Some(checks) = command[..merge].rfind("gh pr checks") else { return false };
    let between = &command[checks..merge];
    between.contains([';', '\n', '|']) && !between.contains("$?")
}

/// The `Read` input for a command that only prints files: `cat FILE`, `head [-N] FILE` or `sed -n 'N,Mp' FILE`
/// chained with `;`, `&&` or newlines, maybe after `cd DIR` and between `echo` separators. Running it as `Read`
/// returns anchors in the same turn, where a refusal cost the model a second call.
pub fn file_read(command: &str) -> Option<Value> {
    let (paths, runs_other) = split_reads(command);
    (!paths.is_empty() && !runs_other).then(|| json!({"paths": paths}))
}

/// The ranges the plain `cat`, `head` and `sed -n` segments of `command` print, and whether any other segment runs.
fn split_reads(command: &str) -> (Vec<Value>, bool) {
    let command = command.replace("&&", ";").replace('\n', ";");
    let mut dir = None;
    let mut paths: Vec<Value> = Vec::new();
    let mut runs_other = false;
    for (index, segment) in command.split(';').enumerate() {
        let words: Vec<&str> = segment.split_whitespace().collect();
        match words.as_slice() {
            [] => {}
            ["cd", cd_dir] if index == 0 => dir = Some(*cd_dir),
            ["echo", rest @ ..] if !rest.iter().any(|word| word.contains('>')) => {}
            words => match printed_range(words, dir) {
                Some(range) if !paths.contains(&range) => paths.push(range),
                Some(_) => {}
                None => runs_other = true,
            },
        }
    }
    (paths, runs_other)
}

/// The `{path, offset, limit}` range a lone `cat FILE`, `head [-N] FILE` or `sed -n 'N,Mp' FILE` prints, with `path` joined onto `dir`.
fn printed_range(words: &[&str], dir: Option<&str>) -> Option<Value> {
    let (file, offset, limit) = match words {
        ["cat", file] | ["cat", "-n", file] => (file, None, None),
        ["head", file] => (file, None, Some(HEAD_DEFAULT_LINES)),
        ["head", count, file] => (file, None, Some(count.strip_prefix("-n").or(count.strip_prefix('-')).and_then(number)?)),
        ["head", "-n", count, file] => (file, None, Some(number(count)?)),
        ["sed", "-n", range, file] => {
            let (first, last) = print_range(range)?;
            (file, Some(first), Some(last.saturating_sub(first) + 1))
        }
        _ => return None,
    };
    let file = file.trim_matches(['\'', '"']);
    // Joined as text: these are bash paths, and Path::join would put a backslash in them on Windows.
    let path = match dir {
        Some(dir) if !file.starts_with('/') => format!("{}/{file}", dir.trim_end_matches('/')),
        _ => file.to_string(),
    };
    if file.starts_with('-') || path.contains(['|', '<', '>', ';', '&', '$', '`', '*', '?', '[', '{', '(']) {
        return None;
    }
    let mut range = json!({"file_path": path});
    if let Some(offset) = offset {
        range["offset"] = offset.into();
    }
    if let Some(limit) = limit {
        range["limit"] = limit.into();
    }
    Some(range)
}

/// First and last line of `Np` or `N,Mp`, bare or quoted.
fn print_range(range: &str) -> Option<(u64, u64)> {
    let lines = range.trim_matches(['\'', '"']).strip_suffix('p')?;
    match lines.split_once(',') {
        Some((first, last)) => Some((number(first)?, number(last)?)),
        None => number(lines).map(|line| (line, line)),
    }
}

/// A `sleep` of MIN_BLOCKED_SLEEP_SECS or more that starts a command before the first loop body.
fn blind_sleep(command: &str) -> bool {
    let command = command.replace('\n', " ; ").replace('(', "( ");
    let words: Vec<&str> = command.split_whitespace().collect();
    let words = &words[..words.iter().position(|word| *word == "do").unwrap_or(words.len())];
    words.iter().enumerate().any(|(index, word)| {
        let starts_command = index == 0 || words[index - 1].ends_with([';', '&', '|', '(', '{']) || matches!(words[index - 1], "then" | "else");
        *word == "sleep" && starts_command && words.get(index + 1).map_or(0, |arg| sleep_seconds(arg)) >= MIN_BLOCKED_SLEEP_SECS
    })
}

/// Seconds of a `sleep` argument such as `30`, `5m` or `2h`; a count too large for u64 saturates.
fn sleep_seconds(arg: &str) -> u64 {
    let digits = arg.find(|ch: char| !ch.is_ascii_digit()).unwrap_or(arg.len());
    let scale = match &arg[digits..] {
        "m" => 60,
        "h" => 3_600,
        "d" => 86_400,
        _ => 1,
    };
    match arg[..digits].parse::<u64>() {
        Ok(count) => count.saturating_mul(scale),
        Err(_) if digits > 0 => u64::MAX,
        Err(_) => 0,
    }
}

fn number(text: &str) -> Option<u64> {
    text.bytes().all(|byte| byte.is_ascii_digit()).then(|| text.parse().ok()).flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_file_reads_become_read_ranges() {
        let reads = [
            ("cat package.json", json!([{"file_path": "package.json"}])),
            ("cd /repo && sed -n '10,40p' src/a.ts", json!([{"file_path": "/repo/src/a.ts", "offset": 10, "limit": 31}])),
            ("sed -n 5p notes.md", json!([{"file_path": "notes.md", "offset": 5, "limit": 1}])),
            ("head -50 README.md", json!([{"file_path": "README.md", "limit": 50}])),
            ("head -n 20 Cargo.toml", json!([{"file_path": "Cargo.toml", "limit": 20}])),
            ("head src/main.rs", json!([{"file_path": "src/main.rs", "limit": 10}])),
            ("cat -n src/a.rs", json!([{"file_path": "src/a.rs"}])),
            ("cat \"notes.md\"", json!([{"file_path": "notes.md"}])),
            ("head -n20 a.md", json!([{"file_path": "a.md", "limit": 20}])),
            ("sed -n '1,40p' a.rs; echo ---; sed -n 5,9p b.rs", json!([{"file_path": "a.rs", "offset": 1, "limit": 40}, {"file_path": "b.rs", "offset": 5, "limit": 5}])),
            (
                "cd /r && sed -n 1,5p a.rs && sed -n 9p a.rs\nhead b.rs",
                json!([{"file_path": "/r/a.rs", "offset": 1, "limit": 5}, {"file_path": "/r/a.rs", "offset": 9, "limit": 1}, {"file_path": "/r/b.rs", "limit": 10}]),
            ),
            ("cat a.rs; cat a.rs", json!([{"file_path": "a.rs"}])),
        ];
        for (command, paths) in reads {
            assert_eq!(file_read(command), Some(json!({"paths": paths})), "{command}");
        }
    }

    #[test]
    fn blocks_blind_sleeps_unchecked_merges_and_one_file_in_place_edits_only() {
        let blocked = [
            ("sleep 150; gh run list --limit 3", "gh run watch"),
            ("(bun run dev &) ; sleep 30 && curl localhost:3000", "curl -sf --retry"),
            ("sleep 15", "gh run watch"),
            ("sleep 5m && gh run list", "gh run watch"),
            ("sleep 99999999999999999999", "gh run watch"),
            ("gh run rerun 1\nsleep 120\ngh run view 1", "gh run watch"),
            ("(sleep 30) && curl localhost:3000", "curl -sf --retry"),
            ("if true; then sleep 60; fi", "gh run watch"),
            ("sleep 60 && echo waiting for deploy", "gh run watch"),
            ("sleep 60; for f in *.log; do wc -l $f; done", "gh run watch"),
            ("gh pr checks 26 --watch 2>&1 | tail -5; gh pr merge 26 --squash", "&& gh pr merge"),
            ("gh pr checks 12 --watch; gh pr merge 12 --squash", "&& gh pr merge"),
            ("gh pr checks 12 --watch\ngh pr merge 12", "&& gh pr merge"),
            ("gh pr checks 12 --watch || gh pr merge 12", "&& gh pr merge"),
            ("sed -i '' 's/a/b/' src/main.rs", "Edit one file"),
            ("cd source && sed -i 's/foo; bar/baz/' src/a.rs && cargo build", "Edit one file"),
            ("perl -pi -e 's/x/y/' README.md", "Edit one file"),
            ("sed -Ei.bak 's/a b/c/' 'my notes.md'", "Edit one file"),
            ("LC_ALL=C sed --in-place 's/a/b/' a.md 2>/dev/null", "Edit one file"),
            ("if true; then sed -i '' \"s/a/\\\"b/\" a.md; fi", "Edit one file"),
        ];
        for (command, advice) in blocked {
            let reason = bash(command).unwrap_or_else(|| panic!("{command} should be blocked"));
            assert!(reason.contains(advice), "{command}: {reason}");
        }
        let allowed = [
            "git status --short && cat a.rs",
            "cat TASK.md; ls -R | head -50",
            "echo hi > a.txt; cat a.txt",
            "cat a.log | rg error",
            "cat > /tmp/x.py <<'EOF'\nprint(1)\nEOF",
            "cat src/*.ts",
            "cat a.txt b.txt",
            "tail -50 dev.log",
            "sleep 2 && curl localhost:3000",
            "for i in $(seq 10); do curl -sf localhost && break; sleep 20; done",
            "echo sleep 60",
            "sleep 14",
            "while ! curl -sf localhost; do sleep 20; done",
            "cat --help",
            "cat src/{a,b}.ts",
            "cd $(foo) && cat x",
            "sed -n 1,400p a.php | rg -n Column",
            "cat > /tmp/x.sh <<'EOF'\nhead a.rs\nEOF\nbash /tmp/x.sh",
            "rg -n \"a; cat b.rs\" src",
            "echo ---",
            "gh pr checks 5 --watch --fail-fast && gh pr merge 5 --squash --delete-branch",
            "gh pr checks 13 --watch >/dev/null 2>&1; s=$?; [ $s -eq 0 ] && gh pr merge 13 --squash",
            "gh pr merge 7 --squash; gh pr checks 8",
            "sed -i '' 's/a/b/' src/*.ts",
            "sed -i 's/a/b/; s/c/d/' src/{a,b}.rs",
            "rg -l foo | xargs sed -i '' 's/foo/bar/'",
            "for f in a b; do sed -i '' 's/x/y/' \"$f\"; done",
            "find . -name '*.md' -exec sed -i '' 's/a/b/' {} +",
            "ssh studio 'cd /tmp/x && sed -i \"\" \"s/a/b/\" src/a.rs'",
            "offload 'sed -i \"\" \"\\$d\" src/a.rs'",
            "perl -MList::Util -ne 'print' a.txt",
            "sed -n 's/a/b/p' a.md",
        ];
        for command in allowed {
            assert_eq!(bash(command), None, "{command}");
            assert_eq!(file_read(command), None, "{command}");
        }
    }
}
