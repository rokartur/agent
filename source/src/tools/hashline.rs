use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::tools::{MAX_BYTES, MAX_LINES, format_size};

// A single-path read without `limit` pages at this many lines; `paths` and written files still show up to MAX_LINES.
// The page bash output and search results get too, so a typical source file arrives in one call.
const READ_DEFAULT_LINES: usize = crate::tools::MAX_LINES;

// Two-letter pieces (sorted), then a row-major bitset of valid piece pairs, packed from pi's anchor table.
const TABLE: &[u8] = include_bytes!("anchors.bin");
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x100_0000_01b3;
const PIECES: usize = 1360;
const ROW_BYTES: usize = PIECES / 8;
const ANCHOR_COUNT: u32 = 1_353_139;
const PROBE_STRIDE: u32 = 836_286;

/// An editor's buffers, read and written in place of the disk, so a read sees unsaved changes (ACP's fs methods).
pub trait EditorFiles: Send + Sync {
    fn read(&self, path: &Path) -> Result<String, String>;
    fn write(&self, path: &Path, text: &str) -> Result<(), String>;
}

/// Session-wide anchor registry: every served line owns a unique 4-letter anchor.
pub struct Anchors {
    row_starts: Vec<u32>,
    probe: u32,
    owner: HashMap<Anchor, Arc<Path>>,
    minted: u32,
    files: HashMap<PathBuf, Snapshot>,
    /// Read, Write and the edit tools go through it when set; search and every other reader see the disk.
    pub editor: Option<Arc<dyn EditorFiles>>,
}

#[derive(Default)]
struct Snapshot {
    lines: Vec<String>,
    anchors: Vec<Anchor>,
}

/// Four ASCII letters kept inline: a long session holds one per line of every file it read.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Anchor([u8; 4]);

impl Anchor {
    fn as_str(&self) -> &str {
        std::str::from_utf8(&self.0).unwrap()
    }
}

// Hashes as its str so `owner` can be looked up by the &str the model sends.
impl std::hash::Hash for Anchor {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}

impl std::borrow::Borrow<str> for Anchor {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl std::fmt::Display for Anchor {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One run of changed lines: `removed` from the old snapshot, `added` new lines starting at `start` in the new one.
struct Hunk {
    start: usize,
    removed: Vec<(Anchor, String)>,
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
        let mut probe = FNV_OFFSET;
        for byte in seed.bytes().chain(crate::process::now_ms().to_le_bytes()) {
            probe = (probe ^ byte as u64).wrapping_mul(FNV_PRIME);
        }
        Anchors { row_starts, probe: (probe % ANCHOR_COUNT as u64) as u32, minted: 0, owner: HashMap::new(), files: HashMap::new(), editor: None }
    }

    fn anchor_at(&self, index: u32) -> Anchor {
        let row = self.row_starts.partition_point(|&start| start <= index) - 1;
        let mut rank = index - self.row_starts[row];
        let bits = row_bits(row);
        for col in 0..PIECES {
            if bits[col / 8] >> (col % 8) & 1 == 0 {
                continue;
            }
            if rank == 0 {
                let (high, low) = (piece(row).as_bytes(), piece(col).as_bytes());
                return Anchor([high[0], high[1], low[0], low[1]]);
            }
            rank -= 1;
        }
        unreachable!("anchor index {index} is past its row")
    }

    /// The stride is coprime with ANCHOR_COUNT, so the probe visits every anchor once before repeating; `sync` checks that some are left.
    fn mint(&mut self, path: &Arc<Path>) -> Anchor {
        self.minted += 1;
        self.probe = (self.probe + PROBE_STRIDE) % ANCHOR_COUNT;
        let anchor = self.anchor_at(self.probe);
        self.owner.insert(anchor, Arc::clone(path));
        anchor
    }

    /// Records `lines` as the content of `path`. Lines a line diff matches to the old content keep their anchors.
    fn sync(&mut self, path: &Path, lines: Vec<String>) -> Result<Vec<Hunk>, String> {
        let old = self.files.remove(path).unwrap_or_default();
        let mut pairs = matching_lines(&old.lines, &lines);
        let added = lines.len() - pairs.len();
        if let Err(e) = self.anchors_left(path, added) {
            self.files.insert(path.to_path_buf(), old);
            return Err(e);
        }
        pairs.push((old.lines.len(), lines.len()));
        let owner = Arc::from(path);
        let mut anchors = Vec::with_capacity(lines.len());
        let mut hunks = Vec::new();
        let (mut old_at, mut new_at) = (0, 0);
        for (old_match, new_match) in pairs {
            if old_match > old_at || new_match > new_at {
                let mut removed = Vec::new();
                for i in old_at..old_match {
                    self.owner.remove(&old.anchors[i]);
                    removed.push((old.anchors[i], old.lines[i].clone()));
                }
                hunks.push(Hunk { start: new_at, removed, added: new_match - new_at });
                for _ in new_at..new_match {
                    anchors.push(self.mint(&owner));
                }
            }
            if new_match < lines.len() {
                anchors.push(old.anchors[old_match]);
            }
            (old_at, new_at) = (old_match + 1, new_match + 1);
        }
        self.files.insert(path.to_path_buf(), Snapshot { lines, anchors });
        Ok(hunks)
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
        self.files.get(path)?.anchors.get(number.checked_sub(1)?).map(Anchor::as_str)
    }

    pub fn read(&mut self, cwd: &Path, input: &Value) -> Result<String, String> {
        if !input["paths"].is_null() {
            let Some(paths) = input["paths"].as_array().filter(|paths| !paths.is_empty()) else {
                return Err("[E_BAD_SHAPE] \"paths\" must be a non-empty array of paths or {path, offset, limit} ranges.".into());
            };
            if !input["offset"].is_null() || !input["limit"].is_null() {
                return Err("[E_BAD_SHAPE] With \"paths\", put \"offset\" and \"limit\" inside each entry: {path, offset, limit}.".into());
            }
            return self.read_many(cwd, paths);
        }
        let path = existing_file(cwd, str_arg(input, "file_path")?)?;
        let text = self.load(&path)?;
        let empty = text.is_empty();
        self.sync(&path, text.lines)?;
        let snapshot = &self.files[&path];
        if empty {
            return Ok(format!("{}│\n[File is empty. Use replace to insert content.]", snapshot.anchors[0]));
        }
        let total = snapshot.lines.len();
        let start = match &input["offset"] {
            Value::Null => 0,
            Value::Number(n) => n.as_u64().filter(|&n| n >= 1).ok_or("[E_BAD_SHAPE] Read request field \"offset\" must be a positive line number or an anchor.")? as usize - 1,
            Value::String(s) => match s.parse::<usize>() {
                Ok(0) => return Err("[E_BAD_SHAPE] Read request field \"offset\" must be a positive line number or an anchor.".into()),
                Ok(n) => n - 1,
                Err(_) => self.index_of(&path, s)? + 1,
            },
            _ => return Err("[E_BAD_SHAPE] Read request field \"offset\" must be a positive line number or an anchor.".into()),
        };
        let limit = match &input["limit"] {
            Value::Null => Some(READ_DEFAULT_LINES),
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
        for entry in paths {
            let (path, offset, limit) = match entry {
                Value::String(path) => (path.as_str(), &Value::Null, &Value::Null),
                Value::Object(range) => match range.get("file_path").and_then(Value::as_str) {
                    Some(path) => (path, &entry["offset"], &entry["limit"]),
                    None => return Err("[E_BAD_SHAPE] A \"paths\" range needs a string \"path\".".into()),
                },
                _ => return Err("[E_BAD_SHAPE] Every entry in \"paths\" must be a path or {path, offset, limit}.".into()),
            };
            let limit = if limit.is_null() { &json!(MAX_LINES) } else { limit };
            let body = self.read(cwd, &json!({"file_path": path, "offset": offset, "limit": limit})).unwrap_or_else(|e| e);
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
            let row = row_text(snapshot.anchors[shown].as_str(), &snapshot.lines[shown], shown + 1, path);
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

    pub fn replace(&mut self, input: &Value) -> Result<String, String> {
        let from = str_arg(input, "remove_from")?;
        let to = str_arg(input, "remove_to")?;
        let text = str_arg(input, "text")?;
        let (path, mut file, first, last) = self.resolve_range(from, to)?;
        let at_end = last + 1 == file.lines.len();
        file.lines.splice(first..=last, split_text(text));
        file.final_newline |= at_end && text.ends_with('\n');
        self.commit(&path, file)
    }

    pub fn replace_match(&mut self, cwd: &Path, input: &Value) -> Result<String, String> {
        let (path, mut file, first, last) = match input["file_path"].as_str() {
            Some(path) => self.resolve_file(cwd, path)?,
            None => self.resolve_range(str_arg(input, "replace_from")?, str_arg(input, "replace_to")?)?,
        };
        let Some(edits) = input["edits"].as_array() else {
            replace_once(&mut file, first, last, input, &path)?;
            return self.commit(&path, file);
        };
        if input["file_path"].is_null() {
            return Err("[E_BAD_SHAPE] edits need `path`, not anchors.".into());
        }
        for (i, edit) in edits.iter().enumerate() {
            let last = file.lines.len() - 1;
            replace_once(&mut file, 0, last, edit, &path).map_err(|error| format!("{error} (edits[{i}]; no edit was written)"))?;
        }
        self.commit(&path, file)
    }

    pub fn insert(&mut self, input: &Value) -> Result<String, String> {
        let anchor = str_arg(input, "anchor")?;
        let text = str_arg(input, "text")?;
        let after = direction_is_after(input)?;
        let (path, mut file, index, _) = self.resolve_range(anchor, anchor)?;
        let lines = if text.is_empty() { vec![String::new()] } else { split_text(text) };
        let at = if after { index + 1 } else { index };
        let at_end = at == file.lines.len();
        file.lines.splice(at..at, lines);
        file.final_newline |= at_end && text.ends_with('\n');
        self.commit(&path, file)
    }

    /// Finds the file owning both anchors, re-reads it, and returns the line range they address.
    fn resolve_range(&mut self, from: &str, to: &str) -> Result<(PathBuf, Text, usize, usize), String> {
        let path = self.owner_of(from)?;
        if self.owner_of(to)? != path {
            return Err(format!("[E_BAD_SHAPE] \"{from}\" and \"{to}\" belong to different files."));
        }
        let file = self.load(&path)?;
        self.sync(&path, file.lines.clone())?;
        let (a, b) = (self.index_of(&path, from)?, self.index_of(&path, to)?);
        Ok((path, file, a.min(b), a.max(b)))
    }

    pub fn owner_of(&self, anchor: &str) -> Result<PathBuf, String> {
        self.owner.get(anchor).map(|owner| owner.to_path_buf()).ok_or_else(|| format!("[E_STALE_ANCHOR] \"{anchor}\" is not owned in this session. Call read() on the target file first."))
    }

    fn index_of(&self, path: &Path, anchor: &str) -> Result<usize, String> {
        if let Some(owner) = self.owner.get(anchor).filter(|owner| owner.as_ref() != path) {
            return Err(format!("[E_STALE_ANCHOR] \"{anchor}\" is owned by {}. Read {0} to use that anchor.", owner.display()));
        }
        self.files[path].anchors.iter().position(|a| a.as_str() == anchor).ok_or_else(|| {
            format!("[E_STALE_ANCHOR] \"{anchor}\" is not a live anchor in {0}: the file changed since read, or the anchor is mistyped (send only the 4 letters before │). Call read() on {0} for fresh anchors.", path.display())
        })
    }

    fn load(&self, path: &Path) -> Result<Text, String> {
        match &self.editor {
            Some(editor) => Ok(Text::parse(&editor.read(path)?)),
            None => Text::load(path),
        }
    }

    /// Loads `path` whole for an edit by content: no read first, so anchors are minted here.
    fn resolve_file(&mut self, cwd: &Path, path: &str) -> Result<(PathBuf, Text, usize, usize), String> {
        let path = existing_file(cwd, path)?;
        let file = self.load(&path)?;
        self.sync(&path, file.lines.clone())?;
        let last = file.lines.len() - 1;
        Ok((path, file, 0, last))
    }

    fn commit(&mut self, path: &Path, mut file: Text) -> Result<String, String> {
        if file.lines.is_empty() {
            file.lines.push(String::new());
            file.final_newline = false;
        }
        let before = match &self.editor {
            Some(editor) => editor.read(path)?.into_bytes(),
            None => std::fs::read(path).map_err(|e| format!("Cannot read {}: {e}", path.display()))?,
        };
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
        match &self.editor {
            Some(editor) => editor.write(path, &after)?,
            None => replace_file(path, after.as_bytes())?,
        }
        let empty = written.is_empty();
        let hunks = self.sync(path, written.lines)?;
        let snapshot = &self.files[path];
        if empty {
            return Ok(format!("File is empty. Use replace on {}│ to insert content.", snapshot.anchors[0]));
        }
        Ok(crate::tools::truncate_head(render_hunks(snapshot, &hunks), "diff"))
    }
}

/// Each hunk with one line of context on both sides, ` ...` where unchanged lines are left out.
fn render_hunks(snapshot: &Snapshot, hunks: &[Hunk]) -> String {
    let row = |mark: char, i: usize| format!("{mark}{}│{}", snapshot.anchors[i], snapshot.lines[i]);
    let mut out = Vec::new();
    let mut shown_until = 0;
    for hunk in hunks {
        if hunk.start >= 1 {
            let before = hunk.start - 1;
            if before > shown_until {
                out.push(" ...".to_string());
            }
            if before >= shown_until {
                out.push(row(' ', before));
            }
        }
        for (anchor, line) in &hunk.removed {
            out.push(format!("-{anchor}│{line}"));
        }
        for i in hunk.start..hunk.start + hunk.added {
            out.push(row('+', i));
        }
        let after = hunk.start + hunk.added;
        if after < snapshot.lines.len() {
            out.push(row(' ', after));
        }
        shown_until = after + 1;
    }
    if shown_until < snapshot.lines.len() {
        out.push(" ...".to_string());
    }
    out.join("\n")
}

const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
const BINARY_SNIFF_BYTES: usize = 8192;

impl Text {
    fn load(path: &Path) -> Result<Text, String> {
        let size = std::fs::metadata(path).map_err(|e| format!("Cannot read {}: {e}", path.display()))?.len();
        if size > MAX_FILE_BYTES {
            return Err(format!("{} is {}, over the {} limit for anchored files. Use bash with sed -n and head.", path.display(), format_size(size as usize), format_size(MAX_FILE_BYTES as usize)));
        }
        let bytes = std::fs::read(path).map_err(|e| format!("Cannot read {}: {e}", path.display()))?;
        if bytes[..bytes.len().min(BINARY_SNIFF_BYTES)].contains(&0) {
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

/// Index pairs of lines `old` and `new` share, in order: the unchanged ends, then a longest common subsequence of the middle.
fn matching_lines(old: &[String], new: &[String]) -> Vec<(usize, usize)> {
    let (prefix, suffix) = unchanged_ends(old, new);
    let (a, b) = (&old[prefix..old.len() - suffix], &new[prefix..new.len() - suffix]);
    let mut pairs: Vec<(usize, usize)> = (0..prefix).map(|i| (i, i)).collect();
    // shortcut: an LCS table past 4M cells costs too much memory, so a change that large keeps no anchors in its middle.
    if a.len() * b.len() <= 4_000_000 {
        // lcs[i][j]: the common subsequence length of a[i..] and b[j..].
        let width = b.len() + 1;
        let mut lcs = vec![0u32; (a.len() + 1) * width];
        for i in (0..a.len()).rev() {
            for j in (0..b.len()).rev() {
                lcs[i * width + j] = if a[i] == b[j] { lcs[(i + 1) * width + j + 1] + 1 } else { lcs[(i + 1) * width + j].max(lcs[i * width + j + 1]) };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < a.len() && j < b.len() {
            if a[i] == b[j] {
                pairs.push((prefix + i, prefix + j));
                (i, j) = (i + 1, j + 1);
            } else if lcs[(i + 1) * width + j] >= lcs[i * width + j + 1] {
                i += 1;
            } else {
                j += 1;
            }
        }
    }
    pairs.extend((0..suffix).map(|k| (old.len() - suffix + k, new.len() - suffix + k)));
    pairs
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
    let quoted = crate::agent::edit_hooks::shell_quote(&path.display().to_string());
    format!("{anchor}│[Line {number} is {}, exceeds {}; content not shown. Use bash: sed -n '{number}p' {quoted} | head -c {MAX_BYTES}]", format_size(line.len()), format_size(MAX_BYTES))
}

fn replace_once(file: &mut Text, first: usize, last: usize, edit: &Value, path: &Path) -> Result<(), String> {
    let old = str_arg(edit, "old_string")?;
    let new = str_arg(edit, "new_string")?;
    if old.is_empty() {
        return Err("[E_BAD_SHAPE] old_string must not be empty.".into());
    }
    let region = file.lines[first..=last].join("\n");
    let matches = region.matches(old).count();
    if matches == 0 {
        return Err(format!("[E_NO_MATCH] old_string was not found in lines {}-{} of {}. Copy the fragment exactly from the file.", first + 1, last + 1, path.display()));
    }
    if matches > 1 {
        return Err(format!("[E_AMBIGUOUS] old_string occurs {matches} times in lines {}-{} of {}. Add surrounding text or narrow the range so it occurs once.", first + 1, last + 1, path.display()));
    }
    let new = new.replace("\r\n", "\n");
    let mut lines: Vec<String> = region.replace(old, &new).split('\n').map(String::from).collect();
    // Like replace: a trailing break ends the last line written instead of adding a blank one.
    let at = region.find(old).expect("counted above");
    if new.ends_with('\n') && at + old.len() == region.len() {
        lines.pop();
        file.final_newline |= last + 1 == file.lines.len();
    }
    file.lines.splice(first..=last, lines);
    Ok(())
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
    match str_arg(input, "direction")? {
        "after" => Ok(true),
        "before" => Ok(false),
        other => Err(format!("[E_BAD_SHAPE] direction must be \"after\" or \"before\", got \"{other}\".")),
    }
}

pub fn str_arg<'a>(input: &'a Value, name: &str) -> Result<&'a str, String> {
    input[name].as_str().ok_or_else(|| format!("[E_BAD_SHAPE] Missing string field \"{name}\"."))
}

/// Replaces an existing `path` through a temp file and rename, so a crash or full disk never truncates it; a new file is written in place.
pub(super) fn replace_file(path: &Path, new: &[u8]) -> Result<(), String> {
    // rename would replace a symlink itself, so the temp file goes next to the file it points to.
    let Ok(path) = path.canonicalize() else { return std::fs::write(path, new).map_err(|e| format!("Cannot write {}: {e}", path.display())) };
    let name = path.file_name().expect("a canonical file path has a name").to_string_lossy();
    let temp = path.with_file_name(format!(".{name}.{}.tmp", crate::api::oauth::random_hex(4)));
    let write = || -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temp)?;
        file.set_permissions(std::fs::metadata(&path)?.permissions())?;
        file.write_all(new)?;
        file.sync_all()?;
        std::fs::rename(&temp, &path)
    };
    write().map_err(|e| {
        let _ = std::fs::remove_file(&temp);
        format!("Cannot write {}: {e}. The file was left as it was.", path.display())
    })
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
        return Err(format!("{} is a directory. List it with fd or ls in bash.", full.display()));
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
    fn replacing_a_file_keeps_its_mode_and_writes_through_a_symlink() {
        use std::os::unix::fs::PermissionsExt;
        let target = temp_file("replace-file", "old");
        let dir = target.parent().unwrap().to_path_buf();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o751)).unwrap();
        let link = dir.join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        replace_file(&link, b"new").unwrap();
        let state = (std::fs::read_to_string(&target).unwrap(), link.is_symlink(), std::fs::metadata(&target).unwrap().permissions().mode() & 0o777, std::fs::read_dir(&dir).unwrap().count());
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(state, ("new".to_string(), true, 0o751, 2));
    }

    #[test]
    fn anchor_table_matches_pi() {
        let anchors = Anchors::new("");
        let picks: Vec<String> = [0, 1, 500_000, 836_286, 1_353_138].iter().map(|&i| anchors.anchor_at(i).to_string()).collect();
        assert_eq!(picks, ["AAAA", "AAAB", "PuOb", "ekJR", "zzzz"]);
    }

    #[test]
    fn read_serves_unique_anchors_and_pages_by_anchor() {
        let path = temp_file("read", "a\nb\nc\n");
        let mut anchors = Anchors::new("s");
        let first = anchors.read(Path::new("/"), &json!({"file_path": path, "limit": 2})).unwrap();
        let served = anchors_of(&first);
        assert_eq!(served.len(), 2);
        assert!(first.ends_with(&format!("[Showing lines 1-2 of 3. Use offset={} to continue.]", served[1])));
        let rest = anchors.read(Path::new("/"), &json!({"file_path": path, "offset": served[1]})).unwrap();
        assert_eq!(rest.split_once('│').unwrap().1, "c");
        let last = anchors_of(&rest)[0].clone();
        let after_last = anchors.read(Path::new("/"), &json!({"file_path": path, "offset": last})).unwrap();
        assert_eq!(after_last, format!("[No lines after {last}: it is the last line of {}.]", path.display()));
    }

    #[test]
    fn read_without_limit_pages_at_the_default() {
        let path = temp_file("default", &"x\n".repeat(READ_DEFAULT_LINES + 1));
        let page = Anchors::new("s").read(Path::new("/"), &json!({"file_path": path})).unwrap();
        assert!(page.contains(&format!("[Showing lines 1-{READ_DEFAULT_LINES} of {}.", READ_DEFAULT_LINES + 1)), "{}", &page[page.len() - 80..]);
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
    fn read_serves_ranges_of_several_files_in_one_call() {
        let path = temp_file("read_ranges", "one\ntwo\nthree\nfour\n");
        let mut anchors = Anchors::new("s");
        let ranges = json!({"paths": [{"file_path": path, "offset": 2, "limit": 1}, {"file_path": path, "offset": 4}]});
        let out = anchors.read(Path::new("/"), &ranges).unwrap();
        let rows: Vec<&str> = out.lines().filter(|line| line.contains('│')).map(|line| line.split('│').nth(1).unwrap()).collect();
        assert_eq!(rows, ["two", "four"], "{out}");
        let bad = anchors.read(Path::new("/"), &json!({"paths": [{"offset": 2}]})).unwrap_err();
        assert!(bad.starts_with("[E_BAD_SHAPE]"), "{bad}");
    }

    #[test]
    fn replace_keeps_untouched_anchors_and_reports_diff() {
        let path = temp_file("replace", "one\ntwo\nthree\nfour\n");
        let mut anchors = Anchors::new("s");
        let a = anchors_of(&anchors.read(Path::new("/"), &json!({"file_path": path})).unwrap());
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
        let a = anchors_of(&anchors.read(Path::new("/"), &json!({"file_path": path})).unwrap());
        std::fs::write(&path, "one\n2\nthree\n").unwrap();
        assert!(anchors.replace(&json!({"remove_from": a[1], "remove_to": a[1], "text": "x"})).unwrap_err().contains("changed since read"));
        anchors.replace_match(Path::new("/"), &json!({"replace_from": a[2], "replace_to": a[2], "old_string": "ee", "new_string": "EE"})).unwrap();
        anchors.insert(&json!({"anchor": a[0], "direction": "before", "text": "zero"})).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "zero\none\n2\nthrEE\n");
    }

    #[test]
    fn replace_match_refuses_a_fragment_that_occurs_twice() {
        let path = temp_file("ambiguous", "foo(foo)\n");
        let mut anchors = Anchors::new("s");
        let a = anchors_of(&anchors.read(Path::new("/"), &json!({"file_path": path})).unwrap());
        let error = anchors.replace_match(Path::new("/"), &json!({"replace_from": a[0], "replace_to": a[0], "old_string": "foo", "new_string": "bar"})).unwrap_err();
        assert!(error.starts_with("[E_AMBIGUOUS]"), "{error}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "foo(foo)\n");
    }

    #[test]
    fn replace_match_with_a_trailing_break_keeps_text_after_the_match() {
        let path = temp_file("overlap", "aaa\n");
        let mut anchors = Anchors::new("s");
        let a = anchors_of(&anchors.read(Path::new("/"), &json!({"file_path": path})).unwrap());
        anchors.replace_match(Path::new("/"), &json!({"replace_from": a[0], "replace_to": a[0], "old_string": "aa", "new_string": "x\n"})).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "x\na\n");
    }

    #[test]
    fn replace_match_by_path_edits_an_unread_file_and_serves_anchors() {
        let path = temp_file("by_path", "one\ntwo\nthree\n");
        let mut anchors = Anchors::new("s");
        let diff = anchors.replace_match(Path::new("/"), &json!({"file_path": path, "old_string": "two", "new_string": "TWO"})).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\nTWO\nthree\n");
        let added = &diff.lines().find(|line| line.starts_with('+')).unwrap()[1..5];
        anchors.replace(&json!({"remove_from": added, "remove_to": added, "text": "2"})).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\n2\nthree\n");
        let error = anchors.replace_match(Path::new("/"), &json!({"file_path": path, "old_string": "e", "new_string": "E"})).unwrap_err();
        assert!(error.starts_with("[E_AMBIGUOUS]"), "{error}");
    }

    #[test]
    fn replace_match_applies_all_edits_or_none() {
        let path = temp_file("edits", "one\ntwo\nthree\n");
        let mut anchors = Anchors::new("s");
        let edits = json!([{"old_string": "one", "new_string": "1"}, {"old_string": "three", "new_string": "3\n"}]);
        anchors.replace_match(Path::new("/"), &json!({"file_path": path, "edits": edits})).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "1\ntwo\n3\n");
        let edits = json!([{"old_string": "two", "new_string": "2"}, {"old_string": "missing", "new_string": "x"}]);
        let error = anchors.replace_match(Path::new("/"), &json!({"file_path": path, "edits": edits})).unwrap_err();
        assert!(error.starts_with("[E_NO_MATCH]") && error.contains("edits[1]"), "{error}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "1\ntwo\n3\n");
    }

    #[test]
    fn edits_far_apart_show_two_hunks_and_keep_the_anchors_between() {
        let lines: Vec<String> = (0..50).map(|i| format!("line {i}")).collect();
        let path = temp_file("hunks", &(lines.join("\n") + "\n"));
        let mut anchors = Anchors::new("s");
        let before = anchors_of(&anchors.read(Path::new("/"), &json!({"file_path": path})).unwrap());
        let edits = json!([{"old_string": "line 5\n", "new_string": "five\n"}, {"old_string": "line 40\n", "new_string": "forty\nforty-one\n"}]);
        let out = anchors.replace_match(Path::new("/"), &json!({"file_path": path, "edits": edits})).unwrap();
        let changed: Vec<&str> = out.lines().filter(|row| row.starts_with(['-', '+'])).collect();
        assert_eq!(changed.len(), 5, "{out}");
        assert_eq!(out.matches(" ...").count(), 3, "{out}");
        let after = anchors_of(&anchors.read(Path::new("/"), &json!({"file_path": path})).unwrap());
        assert_eq!(after[6..40], before[6..40]);
        assert_eq!(after[42..], before[41..]);
    }

    #[test]
    fn crlf_bom_and_missing_final_newline_survive_edits() {
        let path = temp_file("crlf", "\u{feff}a\r\nb");
        let mut anchors = Anchors::new("s");
        let a = anchors_of(&anchors.read(Path::new("/"), &json!({"file_path": path})).unwrap());
        anchors.insert(&json!({"anchor": a[0], "direction": "after", "text": "mid"})).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "\u{feff}a\r\nmid\r\nb");
    }

    #[test]
    fn deleting_every_line_leaves_an_empty_line_anchor() {
        let path = temp_file("empty", "a\nb\n");
        let mut anchors = Anchors::new("s");
        let a = anchors_of(&anchors.read(Path::new("/"), &json!({"file_path": path})).unwrap());
        let out = anchors.replace(&json!({"remove_from": a[0], "remove_to": a[1], "text": ""})).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
        let empty = out.split("replace on ").nth(1).unwrap().split('│').next().unwrap();
        anchors.replace(&json!({"remove_from": empty, "remove_to": empty, "text": "x\n"})).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "x\n");
    }

    #[test]
    fn a_resumed_session_never_serves_the_anchors_of_its_previous_run() {
        let path = temp_file("resume", "a\nb\n");
        let first = anchors_of(&Anchors::new("s").read(Path::new("/"), &json!({"file_path": path})).unwrap());
        std::thread::sleep(std::time::Duration::from_millis(2));
        let resumed = anchors_of(&Anchors::new("s").read(Path::new("/"), &json!({"file_path": path})).unwrap());
        assert!(!resumed.iter().any(|anchor| first.contains(anchor)), "{first:?} {resumed:?}");
    }

    #[test]
    fn running_out_of_anchors_is_an_error_not_a_crash() {
        let path = temp_file("exhaust", "a\n");
        let mut anchors = Anchors::new("s");
        anchors.minted = ANCHOR_COUNT;
        assert!(anchors.read(Path::new("/"), &json!({"file_path": path})).unwrap_err().starts_with("[E_ANCHORS_EXHAUSTED]"));
    }

    #[test]
    fn an_edit_needs_anchors_only_for_the_lines_it_adds() {
        let path = temp_file("exhaust-edit", "a\nb\nc\n");
        let mut anchors = Anchors::new("s");
        let a = anchors_of(&anchors.read(Path::new("/"), &json!({"file_path": path})).unwrap());
        anchors.minted = ANCHOR_COUNT - 1;
        anchors.replace(&json!({"remove_from": a[1], "remove_to": a[1], "text": "B\n"})).unwrap();
    }

    #[test]
    fn a_mixed_line_ending_file_keeps_each_lines_ending() {
        let path = temp_file("mixed", "a\nb\r\nc\n");
        let mut anchors = Anchors::new("s");
        let a = anchors_of(&anchors.read(Path::new("/"), &json!({"file_path": path})).unwrap());
        anchors.replace(&json!({"remove_from": a[2], "remove_to": a[2], "text": "C\n"})).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\nb\r\nC\n");
    }

    #[test]
    fn replace_match_ends_the_line_on_a_trailing_break_in_the_files_line_ending() {
        let path = temp_file("match", "foo\r\nx\r\n");
        let mut anchors = Anchors::new("s");
        let a = anchors_of(&anchors.read(Path::new("/"), &json!({"file_path": path})).unwrap());
        anchors.replace_match(Path::new("/"), &json!({"replace_from": a[0], "replace_to": a[0], "old_string": "foo", "new_string": "a\r\nb\n"})).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\r\nb\r\nx\r\n");
    }

    #[test]
    fn a_row_of_exactly_max_bytes_is_served() {
        let line = "x".repeat(MAX_BYTES - "ABCD│".len());
        let path = temp_file("wide", &format!("{line}\nb\n"));
        let out = Anchors::new("s").read(Path::new("/"), &json!({"file_path": path})).unwrap();
        assert!(out.lines().next().unwrap().ends_with(&line), "{}", &out[out.len() - 200..]);
    }

    #[test]
    fn the_diff_serves_only_anchors_of_lines_on_disk() {
        let path = temp_file("addnewline", "a");
        let mut anchors = Anchors::new("s");
        let a = anchors_of(&anchors.read(Path::new("/"), &json!({"file_path": path})).unwrap());
        let out = anchors.insert(&json!({"anchor": a[0], "direction": "after", "text": ""})).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\n");
        assert!(!out.lines().any(|row| row.starts_with('+')), "{out}");
    }

    #[test]
    fn read_refuses_devices() {
        let err = Anchors::new("s").read(Path::new("/"), &json!({"file_path": "/dev/null"})).unwrap_err();
        assert!(err.contains("is not a regular file"), "{err}");
    }

    #[test]
    fn the_long_line_hint_quotes_the_path_and_measures_the_line() {
        let row = row_text("ABCD", &"x".repeat(60_000), 1, Path::new("/r/it's x;y"));
        assert!(row.contains("Line 1 is 58.6KB") && row.contains(r"sed -n '1p' '/r/it'\''s x;y' |"), "{row}");
    }
}
