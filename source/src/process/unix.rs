use std::ffi::OsStr;
use std::os::fd::IntoRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

use super::{SIGHUP, SIGKILL, SIGTERM, output_within};

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

const SIGINT: i32 = 2;

/// The shell `name` (`sh` or `bash`) that `-c` scripts run in.
pub fn shell(name: &str) -> Result<PathBuf, String> {
    Ok(PathBuf::from(name))
}

/// Every child runs in its own session with stdin on /dev/null unless the caller pipes it, so it cannot open `/dev/tty`
/// or read the TUI's keys: `zsh -i`, `script` or a password prompt would otherwise take the terminal's foreground group.
/// The session is also a process group, so `signal_group(pid, ..)` reaches every child.
#[allow(clippy::disallowed_methods)]
pub fn command(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    command.stdin(Stdio::null());
    command.env_remove(crate::mcp::CLIENT_SERVERS_ENV);
    // Set for a session's process only: an `agent acp` its Bash starts routes again.
    command.env_remove(crate::acp::SESSION_ENV);
    command.envs(super::NO_HISTORY);
    // SAFETY: the hook only calls setsid, which is async-signal-safe.
    unsafe { command.pre_exec(new_session) };
    command
}

fn new_session() -> std::io::Result<()> {
    // SAFETY: setsid takes no arguments; it fails only for a process group leader, which a freshly forked child is not.
    if unsafe { setsid() } == -1 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
}

/// Options that create a file readable by this user only.
pub fn private_file() -> std::fs::OpenOptions {
    let mut options = std::fs::OpenOptions::new();
    options.mode(0o600);
    options
}

/// Makes an existing file readable by this user only; the umask or an older file can leave it wider than `private_file`.
pub fn make_private(path: &Path) -> std::io::Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

pub fn make_executable(path: &Path) -> std::io::Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
}

/// Moves `temp` over `path` in one step.
pub fn replace_with(path: &Path, temp: &Path) -> std::io::Result<()> {
    std::fs::rename(temp, path)
}

/// Sends this process's stdout and stderr to `file` for the rest of its life.
pub fn redirect_output_to(file: std::fs::File) -> std::io::Result<()> {
    let fd = file.into_raw_fd();
    // SAFETY: dup2 only takes descriptors; `fd` is open and stays open for the life of the process.
    if unsafe { libc::dup2(fd, 1) == -1 || libc::dup2(fd, 2) == -1 } {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
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

/// The signal that ended a child, None when it exited.
pub fn signal_of(status: ExitStatus) -> Option<i32> {
    status.signal()
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

/// Sends `signal` to process `pid` alone.
pub fn signal_process(pid: u32, signal: i32) -> std::io::Result<()> {
    // A pid of 0 or past i32 from a broken card would become kill(0) or kill(-1): this group or every process.
    let Some(pid) = i32::try_from(pid).ok().filter(|&pid| pid > 0) else {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("its card names pid {pid}.")));
    };
    // SAFETY: kill only sends a signal.
    if unsafe { kill(pid, signal) } == -1 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
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

/// Whether `binary` is an executable file in some PATH directory; rtk and headroom are optional installs.
pub fn installed(binary: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else { return false };
    std::env::split_paths(&path).any(|dir| std::fs::metadata(dir.join(binary)).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
}
