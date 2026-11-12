# Plan 004: Read returns the text of a PDF

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in "STOP conditions" occurs, stop and report, do not
> improvise. When done, update this plan's row in `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat 52b32af..HEAD -- source/src/tools/mod.rs source/src/tools/hashline.rs source/src/tools/tools.json source/src/tools/video.rs`
> If any of these changed, compare the "Current state" excerpts with the live
> code before you proceed. A mismatch is a STOP condition.

## Status

- **Priority**: P3
- **Effort**: S
- **Risk**: LOW
- **Depends on**: none
- **Category**: direction
- **Planned at**: commit `52b32af`, 2026-11-12

## Why this matters

`Read` on a PDF answers "is a binary file. Use bash with file, xxd or strings
to inspect it", so the model burns turns on `strings` or gives up. Specs,
datasheets and papers arrive as PDFs. After this plan `Read` returns a PDF's
text, a page range at a time, on every provider.

## Current state

All paths are under `source/`.

- `src/tools/mod.rs:92`: the dispatch.

```rust
pub fn run(name: &str, input: &Value, cwd: &Path, anchors: &mut Anchors, rtk: &Rtk, abort: &AtomicBool) -> (String, bool) {
    let result = match name {
        "Read" => anchors.read(cwd, input),
        "Bash" => bash(input, cwd, rtk, abort, None),
```

- `src/tools/mod.rs:11-23`: `pub mod ask;` ... `pub mod video;` `pub mod web;`.
- `src/tools/hashline.rs:644`: the refusal, used at lines 358 and 450.

```rust
fn binary_file(path: &Path) -> String {
    format!("{} is a binary file. Use bash with file, xxd or strings to inspect it.", path.display())
}
```

- `src/tools/hashline.rs`: `resolve(cwd, path)` turns a tool path into an
  absolute one; `Read` pages text at 2000 lines or 50KB.
- `src/tools/video.rs:241`: the pattern for an optional external program,
  started with `crate::process::command("ffprobe")`.
  `crate::process::installed(binary)` (`src/process/unix.rs:229`,
  `src/process/windows.rs:269`) says whether a program is on `PATH`.
- `src/image.rs:180` `sniffed_media_type`: file types are decided by the
  bytes, not the extension. Do the same.
- `src/tools/tools.json:3`: the `Read` schema. Its description ends with
  "Images come back as images."
- `src/agent/mod.rs:911` `requested_image`: images take their own path before
  the tool runs. A PDF is not an image, so it reaches `tools::run`.
- `Cargo.toml` has eight dependencies and no PDF crate. `CONTRIBUTING.md`:
  "Do not add a dependency for something a few lines of code can do."

## The design (decided, do not redesign)

`Read` with `file_path` naming a file that starts with the bytes `%PDF-`
runs `pdftotext` (poppler) and returns plain text. No anchors: a PDF is not
edited by line.

- New input `pages`: `"3"` or `"2-9"`, 1-based, inclusive. Default: the first
  20 pages.
- Command: `pdftotext -layout -f <first> -l <last> <absolute path> -`.
  pdftotext ends each page with a form feed (`\x0c`): split on it.
- Result:

```
[PDF /abs/file.pdf, pages 1-20]
--- page 1 ---
...
--- page 20 ---
...
[More pages may follow: pages "21-40".]
```

  The last line appears only when the number of pages returned equals the
  number asked for.
- Over 50KB of text: cut at a char boundary and end with
  `[Cut at 50KB: ask for fewer pages.]`.
- No text on any page (a scan): return, not as an error,
  `[No text on pages A-B of <path>: a scanned PDF. Render the pages with: pdftoppm -png -r 100 -f A -l B <path> /tmp/page, then Read the PNG files.]`
  Images already reach the model through `Read`, so scans need no new code.
- pdftotext missing: error
  `Reading a PDF needs pdftotext. Install poppler: brew install poppler, apt-get install poppler-utils, or choco install poppler.`
- pdftotext exits non-zero (encrypted or damaged file): error with its stderr.
- A PDF inside `paths`: the message of `binary_file` becomes, for a PDF,
  `<path> is a PDF. Read it alone with file_path, and pages for a range.`

Not in the design: the Anthropic `document` block. It gives the model the
page images too, but needs a branch in four provider adapters
(`api/mod.rs`, `openai_chat.rs`, `codex.rs`, `google.rs`). Text works on all
of them today.

## Commands you will need

| Purpose | Command (in `source/`) | Expected |
|---|---|---|
| Tool present | `command -v pdftotext` | prints a path. If not: install poppler first, see STOP conditions |
| Tests | `cargo test --locked pdf` | all pass |
| All tests | `cargo test --locked` | all pass |
| Lint | `cargo clippy --all-targets --locked -- -D warnings` | exit 0 |
| Format | `cargo fmt --check` | exit 0 |

On Artur's MacBook every cargo command goes through `offload '<command>'`;
pdftotext must then be installed on the Mac Studio.

## Scope

**In scope**:
- `source/src/tools/pdf.rs` (create)
- `source/src/tools/mod.rs` (the `mod` line and the `"Read"` arm)
- `source/src/tools/hashline.rs` (only `binary_file`)
- `source/src/tools/tools.json` (the `Read` entry)
- `source/docs/tools.md`, `README.md` (one sentence)

**Out of scope**:
- `source/Cargo.toml`: no new crate.
- `.github/workflows/ci.yml`: do not install poppler in CI (the dependency setup there is cached and tuned). See "Test plan".
- `src/image.rs`, `src/api/*`: no document blocks.
- OCR, tables, and reading PDFs from URLs (`WebFetch` is another tool).

## Git workflow

- Branch `feat/read-pdf`.
- Commit: `feat(tools): read the text of a PDF`.
- Do not push or open a PR unless told to.

## Steps

### Step 1: `src/tools/pdf.rs`

```rust
const DEFAULT_PAGES: usize = 20;
const MAX_BYTES: usize = 50_000;

/// By its first bytes, as image.rs does: an extension can lie.
pub fn is_pdf(path: &Path) -> bool
/// `"3"` or `"2-9"`, 1-based; None gives the first DEFAULT_PAGES.
fn page_range(pages: Option<&str>) -> Result<(usize, usize), String>
pub fn read(path: &Path, input: &Value) -> Result<String, String>
```

`page_range` errors on `0`, on `last < first`, and on anything that is not
one number or two joined by `-`:
`[E_BAD_SHAPE] "pages" must be a page or a range, e.g. "3" or "2-9".`
(`[E_BAD_SHAPE]` is the prefix the other `Read` input errors use.)
Start pdftotext with `crate::process::command("pdftotext")` and read its
stdout and stderr with `.output()`. Add `pub mod pdf;` to `tools/mod.rs`.

**Verify**: `cargo test --locked pdf` passes with the unit tests from "Test plan".

### Step 2: dispatch

In `tools::run`:

```rust
"Read" => match input["file_path"].as_str().map(|path| hashline::resolve(cwd, path)).filter(|path| pdf::is_pdf(path)) {
    Some(path) => pdf::read(&path, input),
    None => anchors.read(cwd, input),
},
```

In `hashline.rs` make `binary_file` return the PDF sentence when
`crate::tools::pdf::is_pdf(path)`.

**Verify**: `cargo test --locked` passes. Then by hand, with any PDF:
build (`cargo build`), start the TUI on a throwaway home
(`tui run t "env HOME=/tmp/agent-home $PWD/target/debug/agent"`), ask it to
read the PDF, and see page text in the tool result.

### Step 3: schema and docs

- `tools.json`, `Read`: append to the description
  "A PDF comes back as text, 20 pages per call; `pages` picks the range."
  and add the property
  `"pages": { "type": "string", "description": "PDF only: a page or range, e.g. \"3\" or \"21-40\"" }`.
  Keep it this short: the description is sent with every request.
- `source/docs/tools.md`: after "Line anchors" add a three-line section "PDF".
- `README.md`: in the paragraph that lists what `Read`-like features need
  ("It watches videos" names `ffmpeg`), add one bullet or sentence: PDFs are
  read as text and need `pdftotext` from poppler.

**Verify**: `jq '.[] | select(.name == "Read") | .input_schema.properties.pages' src/tools/tools.json` prints the property (adjust the path if the file's top level is not an array). clippy and `cargo fmt --check` exit 0.

## Test plan

In `pdf.rs`, `#[cfg(test)] mod tests`:

Always run:
- `page_range`: `None` gives `(1, 20)`; `"3"` gives `(3, 3)`; `"2-9"` gives `(2, 9)`; `"0"`, `"9-2"`, `"a"`, `"1-2-3"` are errors.
- `is_pdf`: true for a temp file starting with `%PDF-1.4`, false for a text file named `x.pdf`, false for a missing file.
- the 50KB cut and the page headers, tested on a function that takes pdftotext's output as a string (so split `read` into "run" and "format").

Run only where pdftotext is installed (start each with
`if !crate::process::installed("pdftotext") { return; }`):
- a one-page PDF with the text `Hello PDF` comes back under `--- page 1 ---`. Write the fixture as a `const` string in the test: a minimal PDF with one page, a Helvetica font and the content stream `BT /F1 24 Tf 72 720 Td (Hello PDF) Tj ET`. Confirm with `pdftotext` on the command line that your fixture prints `Hello PDF` before you rely on it.
- a scanned PDF gives the "No text" message: make it with ImageMagick, which the tests already require (`magick -size 200x100 xc:white <tmp>/scan.pdf`).
- `pages: "5-6"` on the one-page PDF gives no page text and no error.

CI has no pdftotext, so the second group does not run there. Say so in your
report: whether CI should install poppler is the maintainer's decision.

## Done criteria

- [ ] `cargo test --locked` exits 0 on a machine where `command -v pdftotext` prints a path, and the installed-only tests ran (temporarily add a `panic!` after the guard of one, see it fail, remove it)
- [ ] clippy and `cargo fmt --check` exit 0
- [ ] `git diff --stat -- source/Cargo.toml source/Cargo.lock` is empty
- [ ] `rg -n 'is a PDF' source/src/tools/hashline.rs` has one match
- [ ] `git status --porcelain` lists only in-scope files
- [ ] `plans/README.md` row updated

## STOP conditions

- pdftotext cannot be installed where the tests run. Do not mark the plan done on unit tests alone.
- The hand-written fixture does not yield `Hello PDF` after two attempts: report, do not commit a binary PDF.
- `binary_file` is also used for a write or edit path where the PDF sentence would mislead: report the call site.
- The ACP path (`anchors.editor`, `hashline.rs:38`) must serve the PDF from an editor buffer for your change to work.
- A step's verification fails twice.

## Maintenance notes

- shortcut to record in a code comment at the pdftotext call: `// shortcut: no timeout or abort; 20 pages take well under a second, add both if page ranges grow.`
- Next step if text is not enough (figures, scans on Claude): send the file as an Anthropic `document` block and translate it in each adapter, as `image::block` is translated today.
- Reviewers: the path passed to pdftotext is absolute, so a file name starting with `-` cannot become a flag.
