#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub use unix::*;
#[cfg(windows)]
pub use windows::*;

use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub const SIGHUP: i32 = 1;
pub const SIGKILL: i32 = 9;
pub const SIGTERM: i32 = 15;

/// Runs a `command(..)` with stdout and stderr captured. Past `timeout` its group is killed and the result is None.
pub fn output_within(command: &mut Command, timeout: Duration) -> std::io::Result<Option<Output>> {
    let mut child = command.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    // Read while it runs: a child writing more than a pipe buffer (64 KiB) would otherwise block until the timeout.
    let stdout = read_in_background(child.stdout.take());
    let stderr = read_in_background(child.stderr.take());
    let Some(status) = wait_within(&mut child, timeout)? else { return Ok(None) };
    // A background child (`sleep 5 &`) keeps the pipes open, so the readers would wait for it. Killing the group
    // closes them on Unix; Windows' taskkill finds no tree under an exited parent, so there a grace period ends the wait.
    #[cfg(unix)]
    signal_group(child.id(), SIGKILL);
    let drained_by = cfg!(windows).then(|| Instant::now() + Duration::from_secs(1));
    Ok(Some(Output { status, stdout: collect(&stdout, drained_by), stderr: collect(&stderr, drained_by) }))
}

/// Waits for a `command(..)` child. Past `timeout` its group is killed and the result is None.
pub fn wait_within(child: &mut Child, timeout: Duration) -> std::io::Result<Option<ExitStatus>> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if Instant::now() > deadline {
            signal_group(child.id(), SIGKILL);
            child.wait()?;
            return Ok(None);
        }
        // Short poll: callers such as the rtk rewrite sit on the bash tool's path.
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn read_in_background(pipe: Option<impl std::io::Read + Send + 'static>) -> mpsc::Receiver<Vec<u8>> {
    let mut pipe = pipe.expect("piped above");
    let (send, chunks) = mpsc::channel();
    std::thread::spawn(move || {
        let mut chunk = [0; 8192];
        // A read error ends the capture early; the caller still gets the exit status and what arrived.
        loop {
            let read = match pipe.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => read,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            };
            if send.send(chunk[..read].to_vec()).is_err() {
                break;
            }
        }
    });
    chunks
}

/// What a reader sent until its pipe closed or `deadline`, if any, passed.
fn collect(chunks: &mpsc::Receiver<Vec<u8>>, deadline: Option<Instant>) -> Vec<u8> {
    let Some(deadline) = deadline else { return chunks.iter().flatten().collect() };
    let mut bytes = Vec::new();
    while let Ok(chunk) = chunks.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        bytes.extend(chunk);
    }
    bytes
}

/// `std::fs::canonicalize` without the `\\?\` prefix Windows adds, which git, shells and path comparisons do not expect.
#[allow(clippy::disallowed_methods)]
pub fn canonicalize(path: impl AsRef<std::path::Path>) -> std::io::Result<std::path::PathBuf> {
    let path = std::fs::canonicalize(path)?;
    #[cfg(windows)]
    if let Some(std::path::Component::Prefix(prefix)) = path.components().next() {
        use std::path::Prefix;
        let mut root = std::ffi::OsString::new();
        match prefix.kind() {
            Prefix::VerbatimDisk(drive) => root.push(format!(r"{}:\", drive as char)),
            Prefix::VerbatimUNC(server, share) => {
                for part in [r"\\".as_ref(), server, r"\".as_ref(), share, r"\".as_ref()] {
                    root.push(part);
                }
            }
            _ => return Ok(path),
        }
        let mut local = std::path::PathBuf::from(root);
        local.extend(path.components().skip(1));
        return Ok(local);
    }
    Ok(path)
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).expect("clock is after 1970").as_millis() as u64
}

pub fn hash(text: &str) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, text.as_bytes());
    let hex: String = digest.as_ref().iter().map(|b| format!("{b:02x}")).collect();
    format!("sha256:{hex}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_child_gets_no_variable_meant_for_its_parent_alone() {
        let removed: Vec<_> = command("true").get_envs().filter(|(_, value)| value.is_none()).map(|(name, _)| name.to_owned()).collect();
        assert!(removed.contains(&crate::acp::SESSION_ENV.into()) && removed.contains(&crate::mcp::CLIENT_SERVERS_ENV.into()), "{removed:?}");
    }

    #[test]
    fn installed_finds_only_executables_on_path() {
        assert!(installed("git"));
        assert!(!installed("agent-no-such-binary"));
    }

    #[test]
    fn private_dir_refuses_a_file_in_its_place() {
        let path = std::env::temp_dir().join(format!("agent-private-file-{}", std::process::id()));
        std::fs::write(&path, "").unwrap();
        let refused = private_dir(&path);
        std::fs::remove_file(&path).unwrap();
        assert!(refused.unwrap_err().contains("not a private directory"));
    }

    #[test]
    #[cfg(unix)]
    fn private_dir_refuses_an_existing_shared_directory() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("agent-private-{}", std::process::id()));
        private_dir(&dir).unwrap();
        private_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let shared = private_dir(&dir);
        std::fs::remove_dir(&dir).unwrap();
        assert!(shared.unwrap_err().contains("not a private directory"));
    }

    #[test]
    fn output_within_returns_the_output_or_kills_a_late_command() {
        let sh = || command(shell("sh").unwrap());
        let output = output_within(sh().args(["-c", "echo hi"]), Duration::from_secs(5)).unwrap().unwrap();
        assert_eq!(output.stdout, b"hi\n");
        let started = Instant::now();
        assert!(output_within(sh().args(["-c", "sleep 5; echo late"]), Duration::from_millis(100)).unwrap().is_none());
        assert!(started.elapsed() < Duration::from_secs(2));
        let started = Instant::now();
        let output = output_within(sh().args(["-c", "echo hi; sleep 5 &"]), Duration::from_secs(5)).unwrap().unwrap();
        assert_eq!(output.stdout, b"hi\n");
        assert!(started.elapsed() < Duration::from_secs(3), "a leftover child holding the pipe was waited for");
        let output = output_within(sh().args(["-c", "head -c 200000 /dev/zero"]), Duration::from_secs(5)).unwrap().expect("more than a pipe buffer is read while it runs");
        assert_eq!(output.stdout.len(), 200_000);
    }

    #[test]
    #[cfg(windows)]
    fn canonicalize_drops_the_verbatim_prefix() {
        let path = canonicalize(std::env::temp_dir()).unwrap();
        assert!(!path.to_string_lossy().starts_with(r"\\?\"), "{}", path.display());
        assert!(path.is_dir());
    }

    #[test]
    fn wait_unreaped_reports_the_exit_and_leaves_the_child_to_reap() {
        let mut scripts = vec!["exit 3"];
        if cfg!(unix) {
            scripts.push("kill -9 $$");
        }
        for script in scripts {
            let mut child = command(shell("sh").unwrap()).args(["-c", script]).spawn().unwrap();
            let unreaped = wait_unreaped(child.id()).unwrap();
            assert_eq!(unreaped, child.wait().unwrap());
        }
    }
}
