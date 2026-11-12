use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub const GREEN: &str = "\x1b[32m";
pub const RED: &str = "\x1b[31m";
pub const CYAN: &str = "\x1b[36m";
pub const BOLD: &str = "\x1b[1m";
pub const DIM: &str = "\x1b[2m";
pub const RESET: &str = "\x1b[0m";

/// One live line on stderr for a command that runs in steps: a spinner frame, the step and its detail, redrawn
/// every 80 ms; a step that ends stays behind as `✓ step` with its note and time. Off a terminal each step prints
/// once, as a plain line.
pub struct Spinner {
    line: Arc<Mutex<Line>>,
    stop: Arc<AtomicBool>,
    drawing: Option<JoinHandle<()>>,
}

struct Line {
    step: String,
    detail: String,
    note: String,
    started: Instant,
}

impl Spinner {
    pub fn start() -> Spinner {
        let line = Arc::new(Mutex::new(Line { step: String::new(), detail: String::new(), note: String::new(), started: Instant::now() }));
        let stop = Arc::new(AtomicBool::new(false));
        let drawing = std::io::stderr().is_terminal().then(|| {
            let (line, stop) = (Arc::clone(&line), Arc::clone(&stop));
            std::thread::spawn(move || {
                for frame in FRAMES.iter().cycle() {
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    let current = line.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                    if !current.step.is_empty() {
                        let mut stderr = std::io::stderr().lock();
                        let _ = write!(stderr, "\r\x1b[2K{CYAN}{frame}{RESET} {}{}", current.step, current.detail);
                        let _ = stderr.flush();
                    }
                    drop(current);
                    std::thread::sleep(Duration::from_millis(80));
                }
            })
        });
        Spinner { line, stop, drawing }
    }

    /// A new step closes the previous one; the same step again only refreshes its detail and its note, which the
    /// closed line shows next to the time.
    pub fn set(&self, step: &str, detail: String, note: String) {
        let mut line = self.line.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let started = line.step != step;
        let done = advance(&mut line, step, detail, note, Instant::now());
        match (&self.drawing, done, started) {
            (Some(_), Some(done), _) => {
                let _ = writeln!(std::io::stderr(), "\r\x1b[2K{}", closed(&done, true));
            }
            (None, _, true) => {
                let _ = writeln!(std::io::stderr(), "{step}");
            }
            _ => {}
        }
    }

    /// Closes the current step as done, or as failed when `ok` is false; the caller prints the outcome.
    pub fn finish(mut self, ok: bool) {
        self.stop.store(true, Ordering::Relaxed);
        let Some(drawing) = self.drawing.take() else { return };
        let _ = drawing.join();
        let line = self.line.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut stderr = std::io::stderr().lock();
        let _ = write!(stderr, "\r\x1b[2K");
        if !line.step.is_empty() {
            let _ = writeln!(stderr, "{}", closed(&line, ok));
        }
        let _ = stderr.flush();
    }
}

/// Moves the line to `step`; returns the step it left, if any.
fn advance(line: &mut Line, step: &str, detail: String, note: String, now: Instant) -> Option<Line> {
    let previous = std::mem::replace(line, Line { step: step.to_string(), detail, note, started: now });
    if previous.step == step {
        line.started = previous.started;
        return None;
    }
    (!previous.step.is_empty()).then_some(previous)
}

/// `✓ Download v1.2.3              45.6 MiB, 5.6s`, with a red `✗` for a failed step.
fn closed(line: &Line, ok: bool) -> String {
    let mark = if ok { format!("{GREEN}✓{RESET}") } else { format!("{RED}✗{RESET}") };
    let elapsed = format!("{:.1}s", line.started.elapsed().as_secs_f64());
    let note = if line.note.is_empty() { elapsed } else { format!("{}, {elapsed}", line.note) };
    format!("{mark} {:<28} {DIM}{note}{RESET}", line.step)
}

pub fn mib(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / f64::from(1 << 20))
}

/// `  ━━━━━━━━━━━━━━━━━━━━  12.3 / 45.6 MiB  8.1 MiB/s`, the bar cyan up to the share received; the bytes and the
/// speed alone when the size is unknown.
pub fn transfer(received: u64, total: Option<u64>, elapsed: Duration) -> String {
    let speed = format!("{DIM}{}/s{RESET}", mib((received as f64 / elapsed.as_secs_f64().max(0.001)) as u64));
    let Some(total) = total.filter(|total| *total > 0) else { return format!("  {}  {speed}", mib(received)) };
    let filled = (received * 20 / total).min(20) as usize;
    let size = format!("{:.1} / {}", received as f64 / f64::from(1 << 20), mib(total));
    format!("  {CYAN}{}{RESET}{DIM}{}{RESET}  {size}  {speed}", "━".repeat(filled), "━".repeat(20 - filled))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_step_returns_the_finished_one_and_the_same_step_only_updates_the_detail() {
        let start = Instant::now();
        let mut line = Line { step: String::new(), detail: String::new(), note: String::new(), started: start };
        assert!(advance(&mut line, "Check for updates", String::new(), String::new(), start).is_none());
        let later = start + Duration::from_secs(1);
        let done = advance(&mut line, "Download v1.2.3", "  0.0 MiB".into(), String::new(), later).unwrap();
        assert_eq!(done.step, "Check for updates");
        assert!(advance(&mut line, "Download v1.2.3", "  1.0 MiB".into(), "1.0 MiB".into(), later + Duration::from_secs(1)).is_none());
        assert_eq!((line.step.as_str(), line.detail.as_str(), line.note.as_str(), line.started), ("Download v1.2.3", "  1.0 MiB", "1.0 MiB", later));
    }

    #[test]
    fn a_closed_step_shows_its_mark_its_note_and_its_time() {
        let line = Line { step: "Download v1.2.3".into(), detail: String::new(), note: "45.6 MiB".into(), started: Instant::now() };
        assert_eq!(closed(&line, true), format!("{GREEN}✓{RESET} Download v1.2.3              {DIM}45.6 MiB, 0.0s{RESET}"));
        let line = Line { note: String::new(), ..line };
        assert_eq!(closed(&line, false), format!("{RED}✗{RESET} Download v1.2.3              {DIM}0.0s{RESET}"));
    }

    #[test]
    fn transfer_shows_a_bar_with_a_known_size_and_bytes_alone_without_one() {
        let second = Duration::from_secs(1);
        assert_eq!(transfer(12_897_484, Some(47_815_065), second), format!("  {CYAN}━━━━━{RESET}{DIM}━━━━━━━━━━━━━━━{RESET}  12.3 / 45.6 MiB  {DIM}12.3 MiB/s{RESET}"));
        assert_eq!(transfer(47_815_065, Some(47_815_065), second * 2), format!("  {CYAN}{}{RESET}{DIM}{RESET}  45.6 / 45.6 MiB  {DIM}22.8 MiB/s{RESET}", "━".repeat(20)));
        assert_eq!(transfer(1_048_576, None, second), format!("  1.0 MiB  {DIM}1.0 MiB/s{RESET}"));
        assert_eq!(transfer(0, Some(0), second), format!("  0.0 MiB  {DIM}0.0 MiB/s{RESET}"));
    }
}
