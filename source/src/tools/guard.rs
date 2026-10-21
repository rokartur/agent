use serde_json::{Value, json};

// pi's audit of 53k bash calls: 10k were cat/sed -n/head file reads (a third of the "edit without read" failures),
// and blind `sleep N` burned 41h of wall time.
const MIN_BLOCKED_SLEEP_SECS: u64 = 15;
const HEAD_DEFAULT_LINES: u64 = 10;

/// Why bash should not run `command` (a blind sleep), or None when it may run.
pub fn bash(command: &str) -> Option<String> {
    blind_sleep(command).then(|| "No blind sleep. Server: curl -sf --retry 30 --retry-all-errors --retry-delay 1 URL. CI: gh run watch --exit-status, gh pr checks --watch. Long job: Bash run_in_background.".into())
}

/// The `read` input for a command that only prints files: `cat FILE`, `head [-N] FILE` or `sed -n 'N,Mp' FILE`
/// chained with `;`, `&&` or newlines, maybe after `cd DIR` and between `echo` separators. Running it as `read`
/// returns anchors in the same turn, where a refusal cost the model a second call.
pub fn file_read(command: &str) -> Option<Value> {
    let command = command.replace("&&", ";").replace('\n', ";");
    let mut dir = None;
    let mut paths: Vec<Value> = Vec::new();
    for (index, segment) in command.split(';').enumerate() {
        let words: Vec<&str> = segment.split_whitespace().collect();
        match words.as_slice() {
            [] => {}
            ["cd", cd_dir] if index == 0 => dir = Some(*cd_dir),
            ["echo", rest @ ..] if !rest.iter().any(|word| word.contains('>')) => {}
            words => {
                let range = printed_range(words, dir)?;
                if !paths.contains(&range) {
                    paths.push(range);
                }
            }
        }
    }
    (!paths.is_empty()).then(|| json!({"paths": paths}))
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
    let path = dir.map_or(file.to_string(), |dir| std::path::Path::new(dir).join(file).display().to_string());
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
    fn blocks_blind_sleeps_only() {
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
        ];
        for (command, advice) in blocked {
            let reason = bash(command).unwrap_or_else(|| panic!("{command} should be blocked"));
            assert!(reason.contains(advice), "{command}: {reason}");
        }
        let allowed = [
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
            "cat a.txt; rm a.txt",
            "echo hi > a.txt; cat a.txt",
            "echo ---",
        ];
        for command in allowed {
            assert_eq!(bash(command), None, "{command}");
            assert_eq!(file_read(command), None, "{command}");
        }
    }
}
