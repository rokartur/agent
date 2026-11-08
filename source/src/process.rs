#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub use unix::*;
#[cfg(windows)]
pub use windows::*;

use std::process::{Command, Output, Stdio};
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
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() > deadline {
            signal_group(child.id(), SIGKILL);
            child.wait()?;
            return Ok(None);
        }
        // Short poll: callers such as the rtk rewrite sit on the bash tool's path.
        std::thread::sleep(Duration::from_millis(10));
    };
    // A background child (`sleep 5 &`) keeps the pipes open, so the readers would wait for it.
    signal_group(child.id(), SIGKILL);
    let joined = |reader: std::thread::JoinHandle<Vec<u8>>| reader.join().expect("pipe reader does not panic");
    Ok(Some(Output { status, stdout: joined(stdout), stderr: joined(stderr) }))
}

fn read_in_background(pipe: Option<impl std::io::Read + Send + 'static>) -> std::thread::JoinHandle<Vec<u8>> {
    let mut pipe = pipe.expect("piped above");
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        // A read error ends the capture early; the caller still gets the exit status and what arrived.
        _ = pipe.read_to_end(&mut bytes);
        bytes
    })
}

/// `std::fs::canonicalize` without the `\\?\` prefix Windows adds, which git, shells and path comparisons do not expect.
#[allow(clippy::disallowed_methods)]
pub fn canonicalize(path: impl AsRef<std::path::Path>) -> std::io::Result<std::path::PathBuf> {
    let path = std::fs::canonicalize(path)?;
    #[cfg(windows)]
    if let Some(local) = path.to_str().and_then(|path| path.strip_prefix(r"\\?\"))
        && !local.starts_with(r"UNC\")
    {
        return Ok(local.into());
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn a_child_gets_no_variable_meant_for_its_parent_alone() {
        let removed: Vec<_> = command("true").get_envs().filter(|(_, value)| value.is_none()).map(|(name, _)| name.to_owned()).collect();
        assert!(removed.contains(&crate::acp::SESSION_ENV.into()) && removed.contains(&crate::mcp::CLIENT_SERVERS_ENV.into()), "{removed:?}");
    }

    #[test]
    fn installed_finds_only_executables_on_path() {
        assert!(installed("sh"));
        assert!(!installed("agent-no-such-binary"));
    }

    #[test]
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
        let output = output_within(command("sh").args(["-c", "echo hi"]), Duration::from_secs(5)).unwrap().unwrap();
        assert_eq!(output.stdout, b"hi\n");
        let started = Instant::now();
        assert!(output_within(command("sh").args(["-c", "sleep 5; echo late"]), Duration::from_millis(100)).unwrap().is_none());
        assert!(started.elapsed() < Duration::from_secs(2));
        let started = Instant::now();
        let output = output_within(command("sh").args(["-c", "echo hi; sleep 5 &"]), Duration::from_secs(5)).unwrap().unwrap();
        assert_eq!(output.stdout, b"hi\n");
        assert!(started.elapsed() < Duration::from_secs(2), "a leftover child holding the pipe was waited for");
        let output = output_within(command("sh").args(["-c", "head -c 200000 /dev/zero"]), Duration::from_secs(2)).unwrap().expect("more than a pipe buffer is read while it runs");
        assert_eq!(output.stdout.len(), 200_000);
    }

    #[test]
    fn wait_unreaped_reports_the_exit_and_leaves_the_child_to_reap() {
        for script in ["exit 3", "kill -9 $$"] {
            let mut child = command("sh").args(["-c", script]).spawn().unwrap();
            let unreaped = wait_unreaped(child.id()).unwrap();
            assert_eq!(unreaped, child.wait().unwrap());
        }
    }
}
