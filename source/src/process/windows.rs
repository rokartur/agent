use std::ffi::{OsStr, c_void};
use std::os::windows::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::OnceLock;

type Handle = *mut c_void;

unsafe extern "system" {
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> Handle;
    fn GetExitCodeProcess(process: Handle, code: *mut u32) -> i32;
    fn TerminateProcess(process: Handle, code: u32) -> i32;
    fn WaitForSingleObject(handle: Handle, milliseconds: u32) -> u32;
    fn CloseHandle(handle: Handle) -> i32;
    fn GetCurrentProcess() -> Handle;
    fn SetConsoleCtrlHandler(handler: Option<unsafe extern "system" fn(u32) -> i32>, add: i32) -> i32;
    fn CreateJobObjectW(attributes: *const c_void, name: *const u16) -> Handle;
    fn SetInformationJobObject(job: Handle, class: i32, info: *const c_void, length: u32) -> i32;
    fn AssignProcessToJobObject(job: Handle, process: Handle) -> i32;
    fn SetStdHandle(which: u32, handle: Handle) -> i32;
    fn ReplaceFileW(replaced: *const u16, replacement: *const u16, backup: *const u16, flags: u32, exclude: *const c_void, reserved: *const c_void) -> i32;
}

const PROCESS_TERMINATE: u32 = 0x0001;
const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
const SYNCHRONIZE: u32 = 0x0010_0000;
const ERROR_ACCESS_DENIED: i32 = 5;
const INFINITE: u32 = u32::MAX;
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
const CHILD_FLAGS: u32 = CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW;
const WAIT_TIMEOUT: u32 = 0x102;
const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
const STD_ERROR_HANDLE: u32 = -12i32 as u32;
const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: i32 = 9;
const JOB_OBJECT_LIMIT_BREAKAWAY_OK: u32 = 0x0800;
const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x2000;
const REPLACEFILE_IGNORE_MERGE_ERRORS: u32 = 0x2;

#[repr(C)]
#[derive(Default)]
struct BasicLimits {
    per_process_user_time: i64,
    per_job_user_time: i64,
    flags: u32,
    min_working_set: usize,
    max_working_set: usize,
    active_processes: u32,
    affinity: usize,
    priority_class: u32,
    scheduling_class: u32,
}

#[repr(C)]
#[derive(Default)]
struct ExtendedLimits {
    basic: BasicLimits,
    io_counters: [u64; 6],
    process_memory: usize,
    job_memory: usize,
    peak_process_memory: usize,
    peak_job_memory: usize,
}

/// The shell `name` (`sh` or `bash`) that `-c` scripts run in, from Git for Windows. A bare `bash` would find
/// `System32\bash.exe` first, which runs the command inside WSL.
pub fn shell(name: &str) -> Result<PathBuf, String> {
    static GIT_BIN: OnceLock<Option<PathBuf>> = OnceLock::new();
    let bin = GIT_BIN.get_or_init(find_git_bin).as_ref().ok_or("Cannot find Git Bash. Commands run in Git for Windows' bash.exe: install it from https://git-scm.com/download/win.")?;
    Ok(bin.join(format!("{name}.exe")))
}

/// Git's `bin` directory, which holds `bash.exe` and `sh.exe`.
fn find_git_bin() -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    // A relative entry such as `.` would resolve against the project, which could then ship its own shell.
    let path: Vec<PathBuf> = std::env::split_paths(&path).filter(|dir| dir.is_absolute()).collect();
    let mut candidates = Vec::new();
    // Git's installer puts `Git\cmd` on PATH; the shells sit in `Git\bin`.
    for dir in &path {
        if dir.join("git.exe").is_file()
            && let Some(root) = dir.parent()
        {
            candidates.push(root.join("bin"));
        }
    }
    if let Some(programs) = std::env::var_os("ProgramFiles") {
        candidates.push(PathBuf::from(programs).join("Git").join("bin"));
    }
    // Lowercase: PATH often has `C:\WINDOWS\system32` where SystemRoot says `C:\Windows`.
    let system = std::env::var_os("SystemRoot").map(|system| system.to_string_lossy().to_lowercase());
    candidates.extend(path.into_iter().filter(|dir| system.as_ref().is_none_or(|system| !dir.to_string_lossy().to_lowercase().starts_with(system.as_str()))));
    candidates.into_iter().find(|dir| dir.join("bash.exe").is_file() && dir.join("sh.exe").is_file())
}

/// Every child runs in its own process group with stdin on NUL unless the caller pipes it, and on a hidden console of
/// its own, so it cannot read the TUI's keys and Ctrl+C in the terminal does not reach it.
#[allow(clippy::disallowed_methods)]
pub fn command(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    command.stdin(Stdio::null());
    command.env_remove(crate::mcp::CLIENT_SERVERS_ENV);
    // Set for a session's process only: an `agent acp` its Bash starts routes again.
    command.env_remove(crate::acp::SESSION_ENV);
    command.envs(super::NO_HISTORY);
    command.creation_flags(CHILD_FLAGS);
    command
}

/// Spawns `command` outside the job from `exit_on_signal`, so it outlives the agent. An outer job that forbids
/// breakaway (some CI runners and IDE terminals) keeps it inside; it then ends with the agent.
pub fn spawn_detached(command: &mut Command) -> std::io::Result<std::process::Child> {
    command.creation_flags(CHILD_FLAGS | CREATE_BREAKAWAY_FROM_JOB);
    match command.spawn() {
        Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED) => command.creation_flags(CHILD_FLAGS).spawn(),
        spawned => spawned,
    }
}

/// Sends this process's stdout and stderr to `file` for the rest of its life; std looks the handles up on each write.
pub fn redirect_output_to(file: std::fs::File) -> std::io::Result<()> {
    use std::os::windows::io::IntoRawHandle;
    let handle = file.into_raw_handle();
    // SAFETY: `handle` is an open file handle that stays open for the life of the process.
    if unsafe { SetStdHandle(STD_OUTPUT_HANDLE, handle) == 0 || SetStdHandle(STD_ERROR_HANDLE, handle) == 0 } {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// The user's profile and temp directories are private to the user through their inherited ACLs, so the files created
/// there need no mode of their own.
pub fn private_file() -> std::fs::OpenOptions {
    std::fs::OpenOptions::new()
}

pub fn make_private(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

pub fn make_executable(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Moves `temp` over `path`. ReplaceFileW, not rename: rename fails while another process (an editor, an
/// indexer) has `path` open, and drops the ACL and attributes `path` had.
pub fn replace_with(path: &Path, temp: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    let wide = |path: &Path| path.as_os_str().encode_wide().chain([0]).collect::<Vec<u16>>();
    let (path, temp) = (wide(path), wide(temp));
    let replaced = unsafe { ReplaceFileW(path.as_ptr(), temp.as_ptr(), std::ptr::null(), REPLACEFILE_IGNORE_MERGE_ERRORS, std::ptr::null(), std::ptr::null()) };
    if replaced == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Creates `dir`; it inherits the ACL of the user's profile or temp directory it sits in. An existing one must be a
/// plain directory: a junction or link there could send the files somewhere shared.
pub fn private_dir(dir: &Path) -> Result<(), String> {
    match std::fs::create_dir(dir) {
        Ok(()) => return Ok(()),
        Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => return Err(format!("Cannot create {}: {e}", dir.display())),
        Err(_) => {}
    }
    let metadata = std::fs::symlink_metadata(dir).map_err(|e| format!("Cannot read {}: {e}", dir.display()))?;
    // symlink_metadata reports a junction or link as a link, never as a directory.
    if !metadata.is_dir() {
        return Err(format!("Cannot use {}: not a private directory of this user", dir.display()));
    }
    Ok(())
}

/// Windows ends a process with an exit code, never a signal.
pub fn signal_of(_status: ExitStatus) -> Option<i32> {
    None
}

/// An `OpenProcess` refused with access denied means the process exists under another user. Not the exit code:
/// STILL_ACTIVE (259) is also what a process that called exit(259) leaves.
pub fn alive(pid: u32) -> bool {
    // SAFETY: OpenProcess takes plain integers; a non-null handle is closed below.
    let process = unsafe { OpenProcess(SYNCHRONIZE, 0, pid) };
    if process.is_null() {
        return std::io::Error::last_os_error().raw_os_error() == Some(ERROR_ACCESS_DENIED);
    }
    // SAFETY: `process` is a valid handle with SYNCHRONIZE access.
    let running = unsafe { WaitForSingleObject(process, 0) } == WAIT_TIMEOUT;
    // SAFETY: the handle came from OpenProcess above.
    unsafe { CloseHandle(process) };
    running
}

/// Ends `group` and the processes it started; Windows has no signals, so every `signal` kills.
// shortcut: `taskkill /T` follows parent pids, so a child whose parent already exited (`server &` after its bash) is
// missed until the agent exits and its job ends it; give each command a job object if that matters.
pub fn signal_group(group: u32, _signal: i32) {
    let _ = command("taskkill").args(["/T", "/F", "/PID", &group.to_string()]).stdout(Stdio::null()).stderr(Stdio::null()).status();
}

/// Ends process `pid`; its job then ends the processes it started.
pub fn signal_process(pid: u32, _signal: i32) -> std::io::Result<()> {
    // SAFETY: OpenProcess takes plain integers; a non-null handle is closed below.
    let process = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
    if process.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `process` is a valid handle opened for termination.
    let ended = if unsafe { TerminateProcess(process, 1) } != 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) };
    // SAFETY: the handle came from OpenProcess above.
    unsafe { CloseHandle(process) };
    ended
}

/// Waits for process `pid` to exit. Its handle stays open for the life of the agent, so Windows cannot give the pid
/// to another process while `signal_group` may still target it.
pub fn wait_unreaped(pid: u32) -> std::io::Result<ExitStatus> {
    // SAFETY: OpenProcess takes plain integers.
    let process = unsafe { OpenProcess(SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    let mut code = 0;
    // SAFETY: `process` is a valid handle with SYNCHRONIZE and query access, and `code` is writable.
    if unsafe { WaitForSingleObject(process, INFINITE) } != 0 || unsafe { GetExitCodeProcess(process, &mut code) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(ExitStatus::from_raw(code))
}

static CLEANUP: OnceLock<fn()> = OnceLock::new();

/// Puts the agent in a job that ends every process it starts once the agent exits, however it exits, and runs
/// `cleanup` on Ctrl+C, Ctrl+Break, a closed console window, logoff or shutdown.
pub fn exit_on_signal(cleanup: fn()) {
    unsafe extern "system" fn handle(_event: u32) -> i32 {
        if let Some(cleanup) = CLEANUP.get() {
            cleanup();
        }
        // 130, as a shell reports a process Ctrl+C ended.
        std::process::exit(130);
    }
    let _ = CLEANUP.set(cleanup);
    // SAFETY: `handle` stays valid for the life of the process.
    unsafe { SetConsoleCtrlHandler(Some(handle), 1) };
    end_children_with_the_agent();
}

fn end_children_with_the_agent() {
    let limits = ExtendedLimits { basic: BasicLimits { flags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK, ..Default::default() }, ..Default::default() };
    // SAFETY: null attributes and name create an anonymous job; its handle stays open until the process exits, which
    // is what ends the job's processes.
    let joined = unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        !job.is_null()
            && SetInformationJobObject(job, JOB_OBJECT_EXTENDED_LIMIT_INFORMATION, (&raw const limits).cast(), size_of::<ExtendedLimits>() as u32) != 0
            && AssignProcessToJobObject(job, GetCurrentProcess()) != 0
    };
    // An outer job that forbids nesting only loses this cleanup.
    if !joined {
        eprintln!("warning: processes the agent starts may outlive it: cannot join a job: {}", std::io::Error::last_os_error());
    }
}

/// The job from `exit_on_signal` ends the children once the agent exits, which a panic does right after this.
pub fn hang_up_children() {}

/// Whether `binary` is an executable in some PATH directory; rtk and headroom are optional installs.
pub fn installed(binary: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else { return false };
    std::env::split_paths(&path).any(|dir| dir.join(format!("{binary}.exe")).is_file())
}
