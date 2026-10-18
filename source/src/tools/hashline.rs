use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::tools::{MAX_BYTES, MAX_LINES, format_size};

// Two-letter pieces (sorted), then a row-major bitset of valid piece pairs, packed from pi's anchor table.
const TABLE: &[u8] = include_bytes!("anchors.bin");
const PIECES: usize = 1360;
const ROW_BYTES: usize = PIECES / 8;
const ANCHOR_COUNT: u32 = 1_353_139;
const PROBE_STRIDE: u32 = 836_286;

/// Session-wide anchor registry: every served line owns a unique 4-letter anchor.
pub struct Anchors {
    row_starts: Vec<u32>,
    probe: u32,
    owner: HashMap<String, PathBuf>,
    minted: u32,
    files: HashMap<PathBuf, Snapshot>,
    /// One level per file, like pi: the bytes before and after the last anchored edit.
    undo: HashMap<PathBuf, (Vec<u8>, Vec<u8>)>,
}

#[derive(Default)]
struct Snapshot {
    lines: Vec<String>,
    anchors: Vec<String>,
}

struct Change {
    start: usize,
    removed: Vec<(String, String)>,
    added: usize,
}

struct Text {
    lines: Vec<String>,
    bom: bool,
    crlf: bool,
    final_newline: bool,
}

impl Anchors {
    pub fn new(seed: &str) -> Anchors {
        let mut row_starts = Vec::with_capacity(PIECES);
        let mut total = 0;
        for row in 0..PIECES {
            row_starts.push(total);
            total += row_bits(row).iter().map(|b| b.count_ones()).sum::<u32>();
        }
        assert_eq!(total, ANCHOR_COUNT);
        // The clock keeps a resumed session from re-minting anchors its history still shows on other lines.
        let mut probe: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in seed.bytes().chain(crate::process::now_ms().to_le_bytes()) {
            probe = (probe ^ byte as u64).wrapping_mul(0x100_0000_01b3);
        }
        Anchors { row_starts, probe: (probe % ANCHOR_COUNT as u64) as u32, minted: 0, owner: HashMap::new(), files: HashMap::new(), undo: HashMap::new() }
    }

    fn anchor_at(&self, index: u32) -> String {
        let row = self.row_starts.partition_point(|&start| start <= index) - 1;
        let mut rank = index - self.row_starts[row];
        let bits = row_bits(row);
        for col in 0..PIECES {
            if bits[col / 8] >> (col % 8) & 1 == 0 {
                continue;
            }
            if rank == 0 {
                return format!("{}{}", piece(row), piece(col));
            }
            rank -= 1;
        }
        unreachable!("anchor index {index} is past its row")
    }

    /// The stride is coprime with ANCHOR_COUNT, so the probe visits every anchor once before repeating; `sync` checks that some are left.
    fn mint(&mut self, path: &Path) -> String {
        self.minted += 1;
        self.probe = (self.probe + PROBE_STRIDE) % ANCHOR_COUNT;
        let anchor = self.anchor_at(self.probe);
        self.owner.insert(anchor.clone(), path.to_path_buf());
        anchor
    }

    /// Records `lines` as the content of `path`. Lines in the unchanged prefix and suffix keep their anchors.
    // shortcut: prefix/suffix alignment re-mints anchors between two separate external edits, use a line diff if that hurts.
    fn sync(&mut self, path: &Path, lines: Vec<String>) -> Result<Change, String> {
        let old = self.files.remove(path).unwrap_or_default();
        let (prefix, suffix) = unchanged_ends(&old.lines, &lines);
        let old_end = old.lines.len() - suffix;
        let added = lines.len() - suffix - prefix;
        if let Err(e) = self.anchors_left(path, added) {
            self.files.insert(path.to_path_buf(), old);
            return Err(e);
        }
        let mut removed = Vec::new();
        for i in prefix..old_end {
            self.owner.remove(&old.anchors[i]);
            removed.push((old.anchors[i].clone(), old.lines[i].clone()));
        }
        let mut anchors = old.anchors[..prefix].to_vec();
        for _ in 0..added {
            anchors.push(self.mint(path));
        }
        anchors.extend_from_slice(&old.anchors[old_end..]);
        self.files.insert(path.to_path_buf(), Snapshot { lines, anchors });
        Ok(Change { start: prefix, removed, added })
    }

    fn anchors_left(&self, path: &Path, needed: usize) -> Result<(), String> {
        let left = ANCHOR_COUNT - self.minted;
        if needed as u64 > left as u64 {
            return Err(format!("[E_ANCHORS_EXHAUSTED] {} needs {needed} new anchors, but only {left} of {ANCHOR_COUNT} are left in this session. Start a new session.", path.display()));
        }
        Ok(())
    }

    pub fn minted(&self) -> u32 {
        self.minted
    }

    /// Syncs a canonical `path` from disk so search rows can carry its anchors.
    /// Files over `max_lines` stay untracked: anchors are never freed, and a few hits in large generated files would use them all.
    pub fn track(&mut self, path: &Path, max_lines: usize) -> Result<(), String> {
        let text = Text::load(path)?;
        if text.lines.len() > max_lines {
            return Err(format!("{} has more than {max_lines} lines.", path.display()));
        }
        self.sync(path, text.lines)?;
        Ok(())
    }

    /// The anchor of 1-based line `number` in a tracked file.
    pub fn line_anchor(&self, path: &Path, number: usize) -> Option<&str> {
        self.files.get(path)?.anchors.get(number.checked_sub(1)?).map(String::as_str)
    }

    pub fn read(&mut self, cwd: &Path, input: &Value) -> Result<String, String> {
        if !input["paths"].is_null() {
            let Some(paths) = input["paths"].as_array().filter(|paths| !paths.is_empty()) else {
                return Err("[E_BAD_SHAPE] \"paths\" must be a non-empty array of strings.".into());
            };
            if !input["offset"].is_null() || !input["limit"].is_null() {
                return Err("[E_BAD_SHAPE] \"paths\" reads whole files; use \"path\" with \"offset\" and \"limit\" to page one file.".into());
            }
            return self.read_many(cwd, paths);
        }
        let path = existing_file(cwd, str_arg(input, &["path"])?)?;
        let text = Text::load(&path)?;
        let empty = text.is_empty();
        self.sync(&path, text.lines)?;
        let snapshot = &self.files[&path];
        if empty {
            return Ok(format!("{}│\n[File is empty. Use replace to insert content.]", snapshot.anchors[0]));
        }
        let total = snapshot.lines.len();
        let start = match &input["offset"] {
            Value::Null => 0,
            Value::Number(n) => n.as_u64().filter(|&n| n >= 1).ok_or("[E_BAD_SHAPE] Read request field \"offset\" must be a positive integer.")? as usize - 1,
            Value::String(s) => match s.parse::<usize>() {
                Ok(0) => return Err("[E_BAD_SHAPE] Read request field \"offset\" must be a positive integer or an anchor.".into()),
                Ok(n) => n - 1,
                Err(_) => self.index_of(&path, s)? + 1,
            },
            _ => return Err("[E_BAD_SHAPE] offset must be a line number or an anchor.".into()),
        };
        let limit = match &input["limit"] {
            Value::Null => None,
            v => Some(v.as_u64().filter(|&n| n >= 1).ok_or("[E_BAD_SHAPE] Read request field \"limit\" must be a positive integer.")? as usize),
        };
        if start >= total {
            if let Some(anchor) = input["offset"].as_str().filter(|s| s.parse::<usize>().is_err()) {
                return Ok(format!("[No lines after {anchor}: it is the last line of {}.]", path.display()));
            }
            return Ok(format!("Offset {} is beyond end of file ({total} lines total). Use offset=1 to read from the start, or offset={total} to read the last line.", start + 1));
        }
        Ok(self.rows(&path, start, limit))
    }

    // A failed file reports its error under its header so the other files still arrive.
    fn read_many(&mut self, cwd: &Path, paths: &[Value]) -> Result<String, String> {
        let mut out = Vec::new();
        for path in paths {
            let path = path.as_str().ok_or("[E_BAD_SHAPE] Every entry in \"paths\" must be a string.")?;
            let body = self.read(cwd, &json!({"path": path})).unwrap_or_else(|e| e);
            out.push(format!("== {path}\n{body}"));
        }
        Ok(crate::tools::truncate_head(out.join("\n\n"), "read"))
    }

    /// Serves `path` from `start`, cut at `limit`, MAX_LINES or MAX_BYTES, with a hint to continue.
    fn rows(&self, path: &Path, start: usize, limit: Option<usize>) -> String {
        let snapshot = &self.files[path];
        let total = snapshot.lines.len();
        let end = limit.map_or(total, |limit| start.saturating_add(limit).min(total));
        let mut out = String::new();
        let mut shown = start;
        let mut byte_limited = false;
        while shown < end {
            let row = row_text(&snapshot.anchors[shown], &snapshot.lines[shown], shown + 1, path);
            if shown - start == MAX_LINES || out.len() + usize::from(shown > start) + row.len() > MAX_BYTES {
                byte_limited = shown - start < MAX_LINES;
                break;
            }
            if shown > start {
                out.push('\n');
            }
            out.push_str(&row);
            shown += 1;
        }
        if shown < total {
            let size = if byte_limited { format!(" ({} limit)", format_size(MAX_BYTES)) } else { String::new() };
            let last = &snapshot.anchors[shown - 1];
            out.push_str(&format!("\n\n[Showing lines {}-{shown} of {total}{size}. Use offset={last} to continue.]", start + 1));
        }
        out
    }

    pub fn written(&mut self, path: &Path) -> Result<String, String> {
        self.track(path, usize::MAX)?;
        Ok(self.rows(path, 0, None))
    }

    pub fn replace(&mut self, input: &Value) -> Result<String, String> {
        let from = str_arg(input, &["remove_from"])?;
        let to = str_arg(input, &["remove_to"])?;
        let text = str_arg(input, &["text"])?;
        let (path, mut file, first, last) = self.resolve_range(from, to)?;
        let at_end = last + 1 == file.lines.len();
        file.lines.splice(first..=last, split_text(text));
        file.final_newline |= at_end && text.ends_with('\n');
        self.commit(&path, file)
    }

    pub fn replace_match(&mut self, input: &Value) -> Result<String, String> {
        let from = str_arg(input, &["replace_from"])?;
        let to = str_arg(input, &["replace_to"])?;
        let old = str_arg(input, &["old_string"])?;
        let new = str_arg(input, &["new_string"])?;
        if old.is_empty() {
            return Err("[E_BAD_SHAPE] old_string must not be empty.".into());
        }
        let (path, mut file, first, last) = self.resolve_range(from, to)?;
        let region = file.lines[first..=last].join("\n");
        if !region.contains(old) {
            return Err(format!("[E_NO_MATCH] old_string was not found in lines {}-{} of {}. Copy the fragment exactly from the served rows.", first + 1, last + 1, path.display()));
        }
        let matches = region.matches(old).count();
        if matches > 1 {
            return Err(format!(
                "[E_AMBIGUOUS] old_string occurs {matches} times in lines {}-{} of {}. Add surrounding text or narrow the range so it occurs once.",
                first + 1,
                last + 1,
                path.display()
            ));
        }
        let new = new.replace("\r\n", "\n");
        let mut lines: Vec<String> = region.replace(old, &new).split('\n').map(String::from).collect();
        // Like replace: a trailing break ends the last line written instead of adding a blank one.
        if new.ends_with('\n') && region.ends_with(old) {
            lines.pop();
            file.final_newline |= last + 1 == file.lines.len();
        }
        file.lines.splice(first..=last, lines);
        self.commit(&path, file)
    }

    pub fn insert(&mut self, input: &Value) -> Result<String, String> {
        let anchor = str_arg(input, &["anchor"])?;
        let text = str_arg(input, &["text"])?;
        let after = direction_is_after(input)?;
        let (path, mut file, index, _) = self.resolve_range(anchor, anchor)?;
        let lines = if text.is_empty() { vec![String::new()] } else { split_text(text) };
        let at = if after { index + 1 } else { index };
        let at_end = at == file.lines.len();
        file.lines.splice(at..at, lines);
        file.final_newline |= at_end && text.ends_with('\n');
        self.commit(&path, file)
    }

    /// Duplicates the source range next to `anchor`, in the same or another file; the copies get fresh anchors.
    /// shortcut: copied lines take the destination's line endings, not their own; track per-line endings if mixed files matter.
    pub fn copy(&mut self, input: &Value) -> Result<String, String> {
        let (source, source_file, first, last) = self.resolve_range(str_arg(input, &["source_from"])?, str_arg(input, &["source_to"])?)?;
        let (path, mut file, at) = self.destination(input)?;
        if path == source {
            refuse_inside(at, first, last)?;
        }
        file.lines.splice(at..at, source_file.lines[first..=last].iter().cloned());
        self.commit(&path, file)
    }

    /// Moves the source range next to `anchor`; across files the destination is written first, then the source.
    pub fn move_lines(&mut self, input: &Value) -> Result<String, String> {
        let (source, mut source_file, first, last) = self.resolve_range(str_arg(input, &["source_from"])?, str_arg(input, &["source_to"])?)?;
        let (path, mut file, at) = self.destination(input)?;
        let block: Vec<String> = source_file.lines.drain(first..=last).collect();
        if path == source {
            refuse_inside(at, first, last)?;
            let at = if at > last { at - block.len() } else { at };
            let count = block.len();
            source_file.lines.splice(at..at, block);
            return Ok(format!("Successfully moved {count} line(s) within {}.\n\n{}", path.display(), self.commit(&path, source_file)?));
        }
        let count = block.len();
        file.lines.splice(at..at, block);
        let destination_diff = self.commit(&path, file)?;
        let source_diff = self.commit(&source, source_file).map_err(|e| format!("{e}. The block was already added to {}; call undo_last_change on {0} to remove it.", path.display()))?;
        Ok(format!(
            "Successfully moved {count} line(s) from {} to {}. Undoing the move takes undo_last_change on both files.\n\n{0}:\n{source_diff}\n\n{1}:\n{destination_diff}",
            source.display(),
            path.display()
        ))
    }

    /// Restores the bytes from before the last anchored edit of `path`, unless the file changed since.
    pub fn undo(&mut self, cwd: &Path, input: &Value) -> Result<String, String> {
        let raw = str_arg(input, &["path"])?;
        let path = canonical_even_if_deleted(resolve(cwd, raw));
        let Some((before, after)) = self.undo.get(&path) else {
            return Err(format!("[E_UNDO_NONE] No undo history for {raw}."));
        };
        let current = match std::fs::read(&path) {
            Ok(bytes) => Some(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(format!("Cannot read {}: {e}", path.display())),
        };
        if current.as_ref().is_some_and(|bytes| bytes != after) {
            return Err(format!(
                "[E_UNDO_STALE] Cannot undo last change on {raw}: the file was modified after the edit, so nothing was reverted and the file was left untouched. The undo record is kept. Do not edit the file to force the undo. Call read() to inspect the current state."
            ));
        }
        write_or_restore(&path, before, after)?;
        self.undo.remove(&path);
        let change = self.sync(&path, Text::load(&path)?.lines)?;
        let mut parts = vec![format!("Undone last change on {raw}.")];
        if current.is_none() {
            parts.push("The file was deleted; restored it from undo history.".into());
        }
        if change.added > 0 || !change.removed.is_empty() {
            parts.push(format!("Removed {} line(s), restored {} line(s).", change.removed.len(), change.added));
        }
        parts.push("Call read for fresh anchors.".into());
        Ok(parts.join("\n"))
    }

    /// The file `anchor` belongs to and the line index a block lands on, from `direction`.
    fn destination(&mut self, input: &Value) -> Result<(PathBuf, Text, usize), String> {
        let anchor = str_arg(input, &["anchor"])?;
        let after = direction_is_after(input)?;
        let (path, file, index, _) = self.resolve_range(anchor, anchor)?;
        Ok((path, file, if after { index + 1 } else { index }))
    }

    /// Finds the file owning both anchors, re-reads it, and returns the line range they address.
    fn resolve_range(&mut self, from: &str, to: &str) -> Result<(PathBuf, Text, usize, usize), String> {
        let path = self.owner_of(from)?;
        if self.owner_of(to)? != path {
            return Err(format!("[E_BAD_SHAPE] \"{from}\" and \"{to}\" belong to different files."));
        }
        let file = Text::load(&path)?;
        self.sync(&path, file.lines.clone())?;
        let (a, b) = (self.index_of(&path, from)?, self.index_of(&path, to)?);
        Ok((path, file, a.min(b), a.max(b)))
    }

    pub fn owner_of(&self, anchor: &str) -> Result<PathBuf, String> {
        self.owner.get(anchor).cloned().ok_or_else(|| format!("[E_STALE_ANCHOR] \"{anchor}\" is not owned in this session. Call read() on the target file first."))
    }

    fn index_of(&self, path: &Path, anchor: &str) -> Result<usize, String> {
        if let Some(owner) = self.owner.get(anchor).filter(|owner| *owner != path) {
            return Err(format!("[E_STALE_ANCHOR] \"{anchor}\" is owned by {}. Read {0} to use that anchor.", owner.display()));
        }
        self.files[path].anchors.iter().position(|a| a == anchor).ok_or_else(|| {
            format!("[E_STALE_ANCHOR] \"{anchor}\" is not a live anchor in {0}: the file changed since read, or the anchor is mistyped (send only the 4 letters before │). Call read() on {0} for fresh anchors.", path.display())
        })
    }

    fn commit(&mut self, path: &Path, mut file: Text) -> Result<String, String> {
        if file.lines.is_empty() {
            file.lines.push(String::new());
            file.final_newline = false;
        }
        let before = std::fs::read(path).map_err(|e| format!("Cannot read {}: {e}", path.display()))?;
        let after = file.encode();
        if after.as_bytes() == before {
            return Ok(format!("No changes made to {}: the edit produced identical content.", path.display()));
        }
        // Reparsed, as an empty last line becomes the final newline on disk.
        let written = Text::parse(&after);
        let old = self.files.get(path).map(|snapshot| snapshot.lines.as_slice()).unwrap_or_default();
        let (prefix, suffix) = unchanged_ends(old, &written.lines);
        // Checked before writing, so the edit is not left on disk without anchors.
        self.anchors_left(path, written.lines.len() - prefix - suffix)?;
        write_or_restore(path, after.as_bytes(), &before)?;
        self.undo.insert(path.to_path_buf(), (before, after.into_bytes()));
        let empty = written.is_empty();
        let change = self.sync(path, written.lines)?;
        let snapshot = &self.files[path];
        if empty {
            return Ok(format!("File is empty. Use replace on {}│ to insert content.", snapshot.anchors[0]));
        }
        let mut out = Vec::new();
        let start = change.start;
        if start >= 2 {
            out.push(" ...".to_string());
        }
        if start >= 1 {
            out.push(format!(" {}│{}", snapshot.anchors[start - 1], snapshot.lines[start - 1]));
        }
        for (anchor, line) in &change.removed {
            out.push(format!("-{anchor}│{line}"));
        }
        for i in start..start + change.added {
            out.push(format!("+{}│{}", snapshot.anchors[i], snapshot.lines[i]));
        }
        let after = start + change.added;
        if after < snapshot.lines.len() {
            out.push(format!(" {}│{}", snapshot.anchors[after], snapshot.lines[after]));
        }
        if after + 1 < snapshot.lines.len() {
            out.push(" ...".to_string());
        }
        Ok(crate::tools::truncate_head(out.join("\n"), "diff"))
    }
}

const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;

impl Text {
    fn load(path: &Path) -> Result<Text, String> {
        let size = std::fs::metadata(path).map_err(|e| format!("Cannot read {}: {e}", path.display()))?.len();
        if size > MAX_FILE_BYTES {
            return Err(format!("{} is {}, over the {} limit for anchored files. Use bash with sed -n and head.", path.display(), format_size(size as usize), format_size(MAX_FILE_BYTES as usize)));
        }
        let bytes = std::fs::read(path).map_err(|e| format!("Cannot read {}: {e}", path.display()))?;
        if bytes[..bytes.len().min(8192)].contains(&0) {
            return Err(format!("{} is a binary file. Use bash with file, xxd or strings to inspect it.", path.display()));
        }
        let raw = String::from_utf8(bytes).map_err(|_| format!("{} is not UTF-8 text. Use bash with iconv to convert it, or edit it with bash.", path.display()))?;
        Ok(Text::parse(&raw))
    }

    /// CRLF only when every line break is one; a mixed file keeps its `\r`s in the line text.
    fn parse(raw: &str) -> Text {
        let bom = raw.starts_with('\u{feff}');
        let body = raw.strip_prefix('\u{feff}').unwrap_or(raw);
        let crlf = body.contains("\r\n") && !body.replace("\r\n", "").contains('\n');
        let body = if crlf { body.replace("\r\n", "\n") } else { body.to_string() };
        let final_newline = body.ends_with('\n');
        let mut lines: Vec<String> = body.split('\n').map(String::from).collect();
        if final_newline {
            lines.pop();
        }
        Text { lines, bom, crlf, final_newline }
    }

    fn encode(&self) -> String {
        let newline = if self.crlf { "\r\n" } else { "\n" };
        let mut out = if self.bom { "\u{feff}".to_string() } else { String::new() };
        out.push_str(&self.lines.join(newline));
        if self.final_newline {
            out.push_str(newline);
        }
        out
    }

    fn is_empty(&self) -> bool {
        self.lines.len() == 1 && self.lines[0].is_empty() && !self.final_newline
    }
}

/// The line counts `old` and `new` share at the start and, not overlapping it, at the end.
fn unchanged_ends(old: &[String], new: &[String]) -> (usize, usize) {
    let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let max_suffix = old.len().min(new.len()) - prefix;
    let suffix = old.iter().rev().zip(new.iter().rev()).take(max_suffix).take_while(|(a, b)| a == b).count();
    (prefix, suffix)
}

fn row_bits(row: usize) -> &'static [u8] {
    &TABLE[PIECES * 2 + row * ROW_BYTES..][..ROW_BYTES]
}

fn piece(index: usize) -> &'static str {
    std::str::from_utf8(&TABLE[index * 2..index * 2 + 2]).unwrap()
}

fn row_text(anchor: &str, line: &str, number: usize, path: &Path) -> String {
    let row = format!("{anchor}│{line}");
    if row.len() <= MAX_BYTES {
        return row;
    }
    let quoted = format!("'{}'", path.display().to_string().replace('\'', r"'\''"));
    format!("{anchor}│[Line {number} is {}, exceeds {}; content not shown. Use bash: sed -n '{number}p' {quoted} | head -c {MAX_BYTES}]", format_size(line.len()), format_size(MAX_BYTES))
}

/// Replacement text to lines: one trailing line break ends the last line instead of adding a blank one.
fn split_text(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let text = text.strip_suffix('\n').unwrap_or(text);
    text.split('\n').map(|line| line.strip_suffix('\r').unwrap_or(line).to_string()).collect()
}

fn direction_is_after(input: &Value) -> Result<bool, String> {
    match str_arg(input, &["direction"])? {
        "after" => Ok(true),
        "before" => Ok(false),
        other => Err(format!("[E_BAD_SHAPE] direction must be \"after\" or \"before\", got \"{other}\".")),
    }
}

/// A block landing at index `at` of its own file must not split its source lines `first..=last`.
fn refuse_inside(at: usize, first: usize, last: usize) -> Result<(), String> {
    if at > first && at <= last {
        return Err(format!("[E_BAD_SHAPE] \"anchor\" places the block inside the source range (lines {}-{}). Use a line outside the source range.", first + 1, last + 1));
    }
    Ok(())
}

pub fn str_arg<'a>(input: &'a Value, names: &[&str]) -> Result<&'a str, String> {
    names.iter().find_map(|name| input[name].as_str()).ok_or_else(|| format!("[E_BAD_SHAPE] Missing string field \"{}\".", names[0]))
}

/// Writes `new` in place; when that fails, puts `old` back so a half-written file is not left behind.
pub(super) fn write_or_restore(path: &Path, new: &[u8], old: &[u8]) -> Result<(), String> {
    let Err(e) = std::fs::write(path, new) else { return Ok(()) };
    let outcome = match std::fs::write(path, old) {
        Ok(()) => "The file was left as it was.".to_string(),
        Err(restore) => format!("Restoring its previous content failed too ({restore}); it may be truncated."),
    };
    Err(format!("Cannot write {}: {e}. {outcome}", path.display()))
}

/// A deleted file has no canonical path, but its directory still does.
fn canonical_even_if_deleted(path: PathBuf) -> PathBuf {
    if let Ok(canonical) = path.canonicalize() {
        return canonical;
    }
    match (path.parent().and_then(|dir| dir.canonicalize().ok()), path.file_name()) {
        (Some(dir), Some(name)) => dir.join(name),
        _ => path,
    }
}

pub fn resolve(cwd: &Path, path: &str) -> PathBuf {
    let home = || PathBuf::from(std::env::var("HOME").expect("main exits when HOME is not set"));
    if path == "~" {
        return home();
    }
    match path.strip_prefix("~/") {
        Some(rest) => home().join(rest),
        None => cwd.join(path),
    }
}

fn existing_file(cwd: &Path, path: &str) -> Result<PathBuf, String> {
    let full = resolve(cwd, path).canonicalize().map_err(|e| format!("Cannot read {path}: {e}"))?;
    if full.is_dir() {
        return Err(format!("{} is a directory. Use fd or bash ls to list it.", full.display()));
    }
    if !full.is_file() {
        return Err(format!("{} is not a regular file. Use bash ls -l to inspect it.", full.display()));
    }
    Ok(full)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_file(name: &str, content: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("agent-hashline-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("f.txt");
        std::fs::write(&path, content).unwrap();
        path.canonicalize().unwrap()
    }

    fn anchors_of(output: &str) -> Vec<String> {
        output.lines().filter_map(|row| row.split_once('│')).map(|(a, _)| a.trim_start_matches(['+', '-', ' ']).to_string()).collect()
    }

    #[test]
    fn anchor_table_matches_pi() {
        let anchors = Anchors::new("");
        let picks: Vec<String> = [0, 1, 500_000, 836_286, 1_353_138].iter().map(|&i| anchors.anchor_at(i)).collect();
        assert_eq!(picks, ["AAAA", "AAAB", "PuOb", "ekJR", "zzzz"]);
    }

    #[test]
    fn read_serves_unique_anchors_and_pages_by_anchor() {
        let path = temp_file("read", "a\nb\nc\n");
        let mut anchors = Anchors::new("s");
        let first = anchors.read(Path::new("/"), &json!({"path": path, "limit": 2})).unwrap();
        let served = anchors_of(&first);
        assert_eq!(served.len(), 2);
        assert!(first.ends_with(&format!("[Showing lines 1-2 of 3. Use offset={} to continue.]", served[1])));
        let rest = anchors.read(Path::new("/"), &json!({"path": path, "offset": served[1]})).unwrap();
        assert_eq!(rest.split_once('│').unwrap().1, "c");
        let last = anchors_of(&rest)[0].clone();
        let after_last = anchors.read(Path::new("/"), &json!({"path": path, "offset": last})).unwrap();
        assert_eq!(after_last, format!("[No lines after {last}: it is the last line of {}.]", path.display()));
    }

    #[test]
    fn read_serves_several_files_in_one_call_with_editable_anchors() {
        let first = temp_file("read_many_a", "one\ntwo\n");
        let second = temp_file("read_many_b", "three\n");
        let mut anchors = Anchors::new("s");
        let out = anchors.read(Path::new("/"), &json!({"paths": [first, second]})).unwrap();
        assert!(out.starts_with(&format!("== {}\n", first.display())));
        assert!(out.contains(&format!("\n== {}\n", second.display())));
        let served = anchors_of(&out);
        assert_eq!(served.len(), 3);
        anchors.replace(&json!({"remove_from": served[2], "remove_to": served[2], "text": "THREE\n"})).unwrap();
        assert_eq!(std::fs::read_to_string(&second).unwrap(), "THREE\n");
    }

    #[test]
    fn replace_keeps_untouched_anchors_and_reports_diff() {
        let path = temp_file("replace", "one\ntwo\nthree\nfour\n");
        let mut anchors = Anchors::new("s");
        let a = anchors_of(&anchors.read(Path::new("/"), &json!({"path": path})).unwrap());
        let diff = anchors.replace(&json!({"remove_from": a[1], "remove_to": a[2], "text": "TWO\n"})).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\nTWO\nfour\n");
        let lines: Vec<&str> = diff.lines().collect();
        assert_eq!(lines[0], format!(" {}│one", a[0]));
        assert_eq!(lines[1], format!("-{}│two", a[1]));
        assert_eq!(lines[2], format!("-{}│three", a[2]));
        assert!(lines[3].starts_with('+') && lines[3].ends_with("│TWO"));
        assert_eq!(lines[4], format!(" {}│four", a[3]));
        let stale = anchors.replace(&json!({"remove_from": a[1], "remove_to": a[1], "text": "x"})).unwrap_err();
        assert!(stale.starts_with("[E_STALE_ANCHOR]"));
    }

    #[test]
    fn external_change_keeps_unchanged_anchors_and_refuses_stale_ones() {
        let path = temp_file("external", "one\ntwo\nthree\n");
        let mut anchors = Anchors::new("s");
        let a = anchors_of(&anchors.read(Path::new("/"), &json!({"path": path})).unwrap());
        std::fs::write(&path, "one\n2\nthree\n").unwrap();
        assert!(anchors.replace(&json!({"remove_from": a[1], "remove_to": a[1], "text": "x"})).unwrap_err().contains("changed since read"));
        anchors.replace_match(&json!({"replace_from": a[2], "replace_to": a[2], "old_string": "ee", "new_string": "EE"})).unwrap();
        anchors.insert(&json!({"anchor": a[0], "direction": "before", "text": "zero"})).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "zero\none\n2\nthrEE\n");
    }

    #[test]
    fn replace_match_refuses_a_fragment_that_occurs_twice() {
        let path = temp_file("ambiguous", "foo(foo)\n");
        let mut anchors = Anchors::new("s");
        let a = anchors_of(&anchors.read(Path::new("/"), &json!({"path": path})).unwrap());
        let error = anchors.replace_match(&json!({"replace_from": a[0], "replace_to": a[0], "old_string": "foo", "new_string": "bar"})).unwrap_err();
        assert!(error.starts_with("[E_AMBIGUOUS]"), "{error}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "foo(foo)\n");
    }

    #[test]
    fn crlf_bom_and_missing_final_newline_survive_edits() {
        let path = temp_file("crlf", "\u{feff}a\r\nb");
        let mut anchors = Anchors::new("s");
        let a = anchors_of(&anchors.read(Path::new("/"), &json!({"path": path})).unwrap());
        anchors.insert(&json!({"anchor": a[0], "direction": "after", "text": "mid"})).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "\u{feff}a\r\nmid\r\nb");
    }

    #[test]
    fn deleting_every_line_leaves_an_empty_line_anchor() {
        let path = temp_file("empty", "a\nb\n");
        let mut anchors = Anchors::new("s");
        let a = anchors_of(&anchors.read(Path::new("/"), &json!({"path": path})).unwrap());
        let out = anchors.replace(&json!({"remove_from": a[0], "remove_to": a[1], "text": ""})).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
        let empty = out.split("replace on ").nth(1).unwrap().split('│').next().unwrap();
        anchors.replace(&json!({"remove_from": empty, "remove_to": empty, "text": "x\n"})).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "x\n");
    }

    #[test]
    fn copy_gives_fresh_anchors_and_move_relocates_within_and_across_files() {
        let path = temp_file("copy", "a\nb\nc\n");
        let other = path.with_file_name("g.txt");
        std::fs::write(&other, "x\n").unwrap();
        let mut anchors = Anchors::new("s");
        let a = anchors_of(&anchors.read(Path::new("/"), &json!({"path": path})).unwrap());
        let diff = anchors.copy(&json!({"source_from": a[0], "source_to": a[1], "anchor": a[2], "direction": "after"})).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\nb\nc\na\nb\n");
        let copied: Vec<String> = diff.lines().filter(|l| l.starts_with('+')).map(|l| anchors_of(l)[0].clone()).collect();
        assert!(copied.len() == 2 && !copied.iter().any(|anchor| a.contains(anchor)));
        let inside = anchors.move_lines(&json!({"source_from": a[0], "source_to": a[2], "anchor": a[1], "direction": "after"})).unwrap_err();
        assert!(inside.contains("inside the source range"));
        anchors.move_lines(&json!({"source_from": a[0], "source_to": a[0], "anchor": a[2], "direction": "after"})).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "b\nc\na\na\nb\n");
        let a = anchors_of(&anchors.read(Path::new("/"), &json!({"path": path})).unwrap());
        let x = anchors_of(&anchors.read(Path::new("/"), &json!({"path": other})).unwrap());
        anchors.move_lines(&json!({"source_from": a[0], "source_to": a[0], "anchor": x[0], "direction": "before"})).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "c\na\na\nb\n");
        assert_eq!(std::fs::read_to_string(&other).unwrap(), "b\nx\n");
    }

    #[test]
    fn undo_restores_bom_and_crlf_and_a_deleted_file_but_refuses_a_changed_one() {
        let path = temp_file("undo", "\u{feff}a\r\nb\r\n");
        let mut anchors = Anchors::new("s");
        let undo = json!({"path": path});
        let a = anchors_of(&anchors.read(Path::new("/"), &undo).unwrap());
        anchors.replace(&json!({"remove_from": a[0], "remove_to": a[0], "text": "A\n"})).unwrap();
        std::fs::write(&path, "changed").unwrap();
        assert!(anchors.undo(Path::new("/"), &undo).unwrap_err().starts_with("[E_UNDO_STALE]"));
        std::fs::remove_file(&path).unwrap();
        let out = anchors.undo(Path::new("/"), &undo).unwrap();
        assert!(out.contains("The file was deleted; restored it from undo history."));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "\u{feff}a\r\nb\r\n");
        assert!(anchors.undo(Path::new("/"), &undo).unwrap_err().starts_with("[E_UNDO_NONE]"));
    }

    #[test]
    fn a_resumed_session_never_serves_the_anchors_of_its_previous_run() {
        let path = temp_file("resume", "a\nb\n");
        let first = anchors_of(&Anchors::new("s").read(Path::new("/"), &json!({"path": path})).unwrap());
        std::thread::sleep(std::time::Duration::from_millis(2));
        let resumed = anchors_of(&Anchors::new("s").read(Path::new("/"), &json!({"path": path})).unwrap());
        assert!(!resumed.iter().any(|anchor| first.contains(anchor)), "{first:?} {resumed:?}");
    }

    #[test]
    fn running_out_of_anchors_is_an_error_not_a_crash() {
        let path = temp_file("exhaust", "a\n");
        let mut anchors = Anchors::new("s");
        anchors.minted = ANCHOR_COUNT;
        assert!(anchors.read(Path::new("/"), &json!({"path": path})).unwrap_err().starts_with("[E_ANCHORS_EXHAUSTED]"));
    }

    #[test]
    fn an_edit_needs_anchors_only_for_the_lines_it_adds() {
        let path = temp_file("exhaust-edit", "a\nb\nc\n");
        let mut anchors = Anchors::new("s");
        let a = anchors_of(&anchors.read(Path::new("/"), &json!({"path": path})).unwrap());
        anchors.minted = ANCHOR_COUNT - 1;
        anchors.replace(&json!({"remove_from": a[1], "remove_to": a[1], "text": "B\n"})).unwrap();
    }

    #[test]
    fn a_mixed_line_ending_file_keeps_each_lines_ending() {
        let path = temp_file("mixed", "a\nb\r\nc\n");
        let mut anchors = Anchors::new("s");
        let a = anchors_of(&anchors.read(Path::new("/"), &json!({"path": path})).unwrap());
        anchors.replace(&json!({"remove_from": a[2], "remove_to": a[2], "text": "C\n"})).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\nb\r\nC\n");
    }

    #[test]
    fn replace_match_ends_the_line_on_a_trailing_break_in_the_files_line_ending() {
        let path = temp_file("match", "foo\r\nx\r\n");
        let mut anchors = Anchors::new("s");
        let a = anchors_of(&anchors.read(Path::new("/"), &json!({"path": path})).unwrap());
        anchors.replace_match(&json!({"replace_from": a[0], "replace_to": a[0], "old_string": "foo", "new_string": "a\r\nb\n"})).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\r\nb\r\nx\r\n");
    }

    #[test]
    fn a_row_of_exactly_max_bytes_is_served() {
        let line = "x".repeat(MAX_BYTES - "ABCD│".len());
        let path = temp_file("wide", &format!("{line}\nb\n"));
        let out = Anchors::new("s").read(Path::new("/"), &json!({"path": path})).unwrap();
        assert!(out.lines().next().unwrap().ends_with(&line), "{}", &out[out.len() - 200..]);
    }

    #[test]
    fn the_diff_serves_only_anchors_of_lines_on_disk() {
        let path = temp_file("addnewline", "a");
        let mut anchors = Anchors::new("s");
        let a = anchors_of(&anchors.read(Path::new("/"), &json!({"path": path})).unwrap());
        let out = anchors.insert(&json!({"anchor": a[0], "direction": "after", "text": ""})).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\n");
        assert!(!out.lines().any(|row| row.starts_with('+')), "{out}");
    }

    #[test]
    fn undo_restores_a_file_deleted_after_an_edit_through_a_symlinked_dir() {
        let path = temp_file("undolink", "a\n");
        let link = path.parent().unwrap().with_extension("link");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(path.parent().unwrap(), &link).unwrap();
        let through = json!({"path": link.join("f.txt")});
        let mut anchors = Anchors::new("s");
        let a = anchors_of(&anchors.read(Path::new("/"), &through).unwrap());
        anchors.replace(&json!({"remove_from": a[0], "remove_to": a[0], "text": "b\n"})).unwrap();
        std::fs::remove_file(&path).unwrap();
        anchors.undo(Path::new("/"), &through).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\n");
    }

    #[test]
    fn read_refuses_devices() {
        let err = Anchors::new("s").read(Path::new("/"), &json!({"path": "/dev/null"})).unwrap_err();
        assert!(err.contains("is not a regular file"), "{err}");
    }

    #[test]
    fn the_long_line_hint_quotes_the_path_and_measures_the_line() {
        let row = row_text("ABCD", &"x".repeat(60_000), 1, Path::new("/r/it's x;y"));
        assert!(row.contains("Line 1 is 58.6KB") && row.contains(r"sed -n '1p' '/r/it'\''s x;y' |"), "{row}");
    }
}
