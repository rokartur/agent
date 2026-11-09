use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::Instant;

pub struct Recording {
    ffmpeg: Child,
    wav: PathBuf,
    model: PathBuf,
    pub started: Instant,
}

pub fn start(home: &Path) -> Result<Recording, String> {
    // ffmpeg's dshow input has no default microphone; it takes a device name.
    if cfg!(windows) {
        return Err("/voice does not work on Windows yet.".into());
    }
    let model = crate::tools::video::whisper_model(&home.join(".agents"))?;
    // Unique per recording, and created here so ffmpeg -y overwrites our own file, never a planted link.
    let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).expect("clock is after 1970").as_nanos();
    let wav = std::env::temp_dir().join(format!("agent-voice-{}-{nonce}.wav", std::process::id()));
    crate::process::private_file().write(true).create_new(true).open(&wav).map_err(|e| format!("Cannot create {}: {e}", wav.display()))?;
    let ffmpeg = crate::process::command("ffmpeg")
        .args(["-loglevel", "error", "-y"])
        .args(if cfg!(target_os = "macos") { ["-f", "avfoundation", "-i", ":default"] } else { ["-f", "pulse", "-i", "default"] })
        .args(["-ar", "16000", "-ac", "1"])
        .arg(&wav)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| {
            let _ = std::fs::remove_file(&wav);
            format!("Cannot run ffmpeg: {e}")
        })?;
    Ok(Recording { ffmpeg, wav, model, started: Instant::now() })
}

impl Recording {
    pub fn cancel(mut self) {
        let _ = self.ffmpeg.kill();
        let _ = self.ffmpeg.wait();
        let _ = std::fs::remove_file(&self.wav);
    }

    /// Blocks until whisper-cli finishes, so call it off the UI thread.
    pub fn transcribe(mut self) -> Result<String, String> {
        // ffmpeg writes the WAV header sizes only when it quits on "q", not on kill.
        let quit = self.ffmpeg.stdin.take().expect("stdin is piped").write_all(b"q");
        let status = self.ffmpeg.wait().map_err(|e| format!("ffmpeg: {e}"))?;
        if quit.is_err() || !status.success() {
            let _ = std::fs::remove_file(&self.wav);
            return Err("Recording failed. Check that the terminal may use the microphone.".into());
        }
        let output = crate::process::command("whisper-cli").args(["-l", "auto", "-nt", "-np", "-m"]).arg(&self.model).arg("-f").arg(&self.wav).output();
        let _ = std::fs::remove_file(&self.wav);
        let output = output.map_err(|e| format!("Cannot run whisper-cli: {e}. Install it: brew install whisper-cpp"))?;
        if !output.status.success() {
            return Err(format!("whisper-cli failed: {}", String::from_utf8_lossy(&output.stderr).trim()));
        }
        spoken_text(&String::from_utf8_lossy(&output.stdout))
    }
}

fn spoken_text(stdout: &str) -> Result<String, String> {
    let text = stdout.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() || text == "[BLANK_AUDIO]" {
        return Err("Heard nothing.".into());
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_segments_and_rejects_silence() {
        assert_eq!(spoken_text(" Cześć,\n sprawdź testy.\n").unwrap(), "Cześć, sprawdź testy.");
        assert!(spoken_text("\n [BLANK_AUDIO]\n").is_err());
        assert!(spoken_text("").is_err());
    }

    #[test]
    #[cfg(windows)]
    fn voice_is_refused_on_windows() {
        assert_eq!(start(&std::env::temp_dir()).err().unwrap(), "/voice does not work on Windows yet.");
    }
}
