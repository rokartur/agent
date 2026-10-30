use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

pub const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// One live line on stderr for a command that runs in steps: a spinner frame, the step and its detail, redrawn
/// every 80 ms; a step that ends stays behind as `✓ step`. Off a terminal each step prints once, as a plain line.
pub struct Spinner {
    line: Arc<Mutex<Line>>,
    stop: Arc<AtomicBool>,
    drawing: Option<JoinHandle<()>>,
}

#[derive(Default)]
struct Line {
    step: String,
    detail: String,
}

impl Spinner {
    pub fn start() -> Spinner {
        let line = Arc::new(Mutex::new(Line::default()));
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
                        let _ = write!(stderr, "\r\x1b[2K{frame} {}{}", current.step, current.detail);
                        let _ = stderr.flush();
                    }
                    drop(current);
                    std::thread::sleep(Duration::from_millis(80));
                }
            })
        });
        Spinner { line, stop, drawing }
    }

    /// A new step closes the previous one; the same step again only refreshes its detail.
    pub fn set(&self, step: &str, detail: String) {
        let mut line = self.line.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let started = line.step != step;
        let done = advance(&mut line, step, detail);
        match (&self.drawing, done, started) {
            (Some(_), Some(done), _) => {
                let _ = writeln!(std::io::stderr(), "\r\x1b[2K✓ {done}");
            }
            (None, _, true) => {
                let _ = writeln!(std::io::stderr(), "{step}");
            }
            _ => {}
        }
    }

    /// Clears the live line; the caller prints the outcome.
    pub fn finish(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(drawing) = self.drawing.take() {
            let _ = drawing.join();
            let _ = write!(std::io::stderr(), "\r\x1b[2K");
            let _ = std::io::stderr().flush();
        }
    }
}

/// Moves the line to `step`; returns the step it left, if any.
fn advance(line: &mut Line, step: &str, detail: String) -> Option<String> {
    let done = (line.step != step && !line.step.is_empty()).then(|| std::mem::take(&mut line.step));
    line.step = step.to_string();
    line.detail = detail;
    done
}

/// `12.3 / 45.6 MiB [#####...............] 26%`, or the bytes alone when the size is unknown.
pub fn transfer(received: u64, total: Option<u64>) -> String {
    let mib = |bytes: u64| bytes as f64 / f64::from(1 << 20);
    let Some(total) = total.filter(|total| *total > 0) else { return format!("  {:.1} MiB", mib(received)) };
    let filled = (received * 20 / total).min(20) as usize;
    format!("  {:.1} / {:.1} MiB [{}{}] {}%", mib(received), mib(total), "#".repeat(filled), ".".repeat(20 - filled), received * 100 / total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_step_returns_the_finished_one_and_the_same_step_only_updates_the_detail() {
        let mut line = Line::default();
        assert_eq!(advance(&mut line, "Check for updates", String::new()), None);
        assert_eq!(advance(&mut line, "Download v1.2.3", "  0.0 MiB".into()), Some("Check for updates".into()));
        assert_eq!(advance(&mut line, "Download v1.2.3", "  1.0 MiB".into()), None);
        assert_eq!((line.step.as_str(), line.detail.as_str()), ("Download v1.2.3", "  1.0 MiB"));
    }

    #[test]
    fn transfer_shows_a_bar_with_a_known_size_and_bytes_alone_without_one() {
        assert_eq!(transfer(12_897_484, Some(47_815_065)), "  12.3 / 45.6 MiB [#####...............] 26%");
        assert_eq!(transfer(47_815_065, Some(47_815_065)), "  45.6 / 45.6 MiB [####################] 100%");
        assert_eq!(transfer(1_048_576, None), "  1.0 MiB");
        assert_eq!(transfer(0, Some(0)), "  0.0 MiB");
    }
}
