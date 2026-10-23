use std::ffi::OsStr;
use std::os::fd::IntoRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Command, ExitStatus, Output, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

unsafe extern "C" {
    fn setsid() -> i32;
    fn signal(signum: i32, handler: extern "C" fn(i32)) -> usize;
    fn write(fd: i32, buf: *const std::ffi::c_void, count: usize) -> isize;
    fn kill(pid: i32, sig: i32) -> i32;
    fn geteuid() -> u32;
    fn waitid(idtype: u32, id: u32, info: *mut SigInfo, options: i32) -> i32;
    #[cfg_attr(target_os = "macos", link_name = "__error")]
    #[cfg_attr(target_os = "linux", link_name = "__errno_location")]
    fn errno_location() -> *mut i32;
}

/// `siginfo_t`, read only for `si_code` and `si_status`; 128 bytes covers Linux and macOS.
#[repr(C, align(8))]
struct SigInfo([u8; 128]);

const P_PID: u32 = 1;
const WEXITED: i32 = 4;
const CLD_EXITED: i32 = 1;
const CLD_DUMPED: i32 = 3;
const SI_CODE_OFFSET: usize = 8;
#[cfg(target_os = "linux")]
const WNOWAIT: i32 = 0x0100_0000;
#[cfg(target_os = "macos")]
const WNOWAIT: i32 = 0x20;
#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
const SI_STATUS_OFFSET: usize = 24;
#[cfg(any(target_os = "macos", all(target_os = "linux", target_pointer_width = "32")))]
const SI_STATUS_OFFSET: usize = 20;

pub const SIGHUP: i32 = 1;
const SIGINT: i32 = 2;
pub const SIGKILL: i32 = 9;
pub const SIGTERM: i32 = 15;

/// Every child runs in its own session with stdin on /dev/null unless the caller pipes it, so it cannot open `/dev/tty`
/// or read the TUI's keys: `zsh -i`, `script` or a password prompt would otherwise take the terminal's foreground group.
/// The session is also a process group, so `signal_group(pid, ..)` reaches every child.
#[allow(clippy::disallowed_methods)]
pub fn command(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    command.stdin(Stdio::null());
    // SAFETY: the hook only calls setsid, which is async-signal-safe.
    unsafe { command.pre_exec(new_session) };
    command
}

fn new_session() -> std::io::Result<()> {
    // SAFETY: setsid takes no arguments; it fails only for a process group leader, which a freshly forked child is not.
    if unsafe { setsid() } == -1 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
}

/// Creates `dir` readable by this user only. On Linux `/tmp` is shared and the per-pid names are predictable, so an
/// existing one is kept only when this user made it private; another user's directory could hold planted symlinks.
pub fn private_dir(dir: &Path) -> Result<(), String> {
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        // The umask can strip owner bits from `mode`, and the next call would then refuse this directory.
        Ok(()) => return std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(|e| format!("Cannot restrict {}: {e}", dir.display())),
        Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => return Err(format!("Cannot create {}: {e}", dir.display())),
        Err(_) => {}
    }
    let metadata = std::fs::symlink_metadata(dir).map_err(|e| format!("Cannot read {}: {e}", dir.display()))?;
    // SAFETY: geteuid takes no arguments and cannot fail.
    let private = metadata.is_dir() && metadata.uid() == unsafe { geteuid() } && metadata.mode() & 0o777 == 0o700;
    if !private {
        return Err(format!("Cannot use {}: not a private directory of this user", dir.display()));
    }
    Ok(())
}

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

/// Signal 0 checks the process exists; EPERM means it does, under another user.
pub fn alive(pid: u32) -> bool {
    // SAFETY: kill with signal 0 sends nothing.
    let sent = unsafe { kill(pid as i32, 0) == 0 };
    sent || std::io::Error::last_os_error().raw_os_error() == Some(1)
}

/// Sends `signal` to every process in group `group`; a group that is already gone needs no handling.
pub fn signal_group(group: u32, signal: i32) {
    let Ok(group) = i32::try_from(group) else { return };
    // SAFETY: kill takes two integers and touches no memory of ours.
    unsafe { kill(-group, signal) };
}

/// Waits for child `pid` to exit but leaves it a zombie, so neither its pid nor the process group it leads can be
/// reused while `signal_group` may still target it.
pub fn wait_unreaped(pid: u32) -> std::io::Result<ExitStatus> {
    let mut info = SigInfo([0; 128]);
    // SAFETY: info is writable and larger than siginfo_t.
    while unsafe { waitid(P_PID, pid, &mut info, WEXITED | WNOWAIT) } == -1 {
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
    let field = |offset: usize| i32::from_ne_bytes([info.0[offset], info.0[offset + 1], info.0[offset + 2], info.0[offset + 3]]);
    let status = field(SI_STATUS_OFFSET);
    // The wait(2) encoding ExitStatus decodes: exit code in bits 8-15, terminating signal in bits 0-6, core dump in bit 7.
    Ok(ExitStatus::from_raw(match field(SI_CODE_OFFSET) {
        CLD_EXITED => (status & 0xff) << 8,
        CLD_DUMPED => status & 0x7f | 0x80,
        _ => status & 0x7f,
    }))
}

static SIGNAL_PIPE: AtomicI32 = AtomicI32::new(-1);

/// Ends the agent and its children on SIGHUP, SIGINT or SIGTERM, then runs `cleanup`. Children from `command` sit in
/// their own sessions, so neither the kernel's SIGHUP nor a signal to the agent reaches them.
pub fn exit_on_signal(cleanup: fn()) {
    extern "C" fn wake(signum: i32) {
        // SAFETY: errno is thread-local; the handler puts back what the interrupted code may still read.
        let errno = unsafe { *errno_location() };
        // SAFETY: write is async-signal-safe and the pipe is never closed.
        unsafe { write(SIGNAL_PIPE.load(Ordering::Relaxed), [signum as u8].as_ptr().cast(), 1) };
        // SAFETY: as above.
        unsafe { *errno_location() = errno };
    }
    let (mut receiver, sender) = std::io::pipe().expect("pipe for signals");
    SIGNAL_PIPE.store(sender.into_raw_fd(), Ordering::Relaxed);
    for signum in [SIGHUP, SIGINT, SIGTERM] {
        // SAFETY: the handler only calls write. exec resets a caught signal to its default, so children still die on it.
        unsafe { signal(signum, wake) };
    }
    std::thread::spawn(move || {
        let mut signum = [0];
        if std::io::Read::read(&mut receiver, &mut signum).is_ok_and(|n| n == 1) {
            hang_up_children();
            cleanup();
            std::process::exit(128 + i32::from(signum[0]));
        }
    });
}

/// Sends SIGHUP, then SIGKILL for those that ignore it (`nohup`), to the group of every direct child. Zombies count:
/// a bg terminal whose `sh` exited (`wait_unreaped`) may still have members; macOS `pgrep -P` skips zombies, `ps` does not.
pub fn hang_up_children() {
    // Bounded: a hung `ps` on a loaded host must not keep the agent from killing its children and exiting.
    let Ok(Some(output)) = output_within(command("ps").args(["-A", "-o", "pid=,ppid=,comm="]), Duration::from_secs(2)) else { return };
    let agent = std::process::id().to_string();
    let mut groups: Vec<u32> = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut fields = line.split_whitespace();
        // `ps` lists itself as our child; it is reaped by now, so its pid may already belong to someone else.
        if let (Some(pid), Some(parent), Some(program)) = (fields.next(), fields.next(), fields.next())
            && parent == agent
            && program != "ps"
            && !program.ends_with("/ps")
            && let Ok(pid) = pid.parse()
        {
            groups.push(pid);
        }
    }
    for &group in &groups {
        signal_group(group, SIGHUP);
    }
    // Subagents run --headless and get the shorter grace, so they SIGKILL their own nohup children before the root's
    // SIGKILL ends them.
    let headless = std::env::args().any(|arg| arg == "--headless");
    std::thread::sleep(Duration::from_millis(if headless { 200 } else { 600 }));
    for group in groups {
        signal_group(group, SIGKILL);
    }
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).expect("clock is after 1970").as_millis() as u64
}

pub fn hash(text: &str) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, text.as_bytes());
    let hex: String = digest.as_ref().iter().map(|b| format!("{b:02x}")).collect();
    format!("sha256:{hex}")
}

/// Whether `binary` is an executable file in some PATH directory; rtk and headroom are optional installs.
pub fn installed(binary: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else { return false };
    std::env::split_paths(&path).any(|dir| std::fs::metadata(dir.join(binary)).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installed_finds_only_executables_on_path() {
        assert!(installed("sh"));
        assert!(!installed("agent-no-such-binary"));
    }

    #[test]
    fn private_dir_refuses_an_existing_shared_directory() {
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
