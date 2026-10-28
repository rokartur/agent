//! `agent service`: runs `agent serve --tailnet` at login, as a launchd agent on macOS or a systemd user unit on
//! Linux, so a host accepts sessions with no terminal open on it.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::process;

const LABEL: &str = "com.rokartur.agent.tailnet";
const UNIT: &str = "agent-tailnet.service";
const LOG: &str = ".agents/tailnet.log";
const LOG_LIMIT: u64 = 1 << 20;
/// launchd sends SIGTERM and SIGKILL after its 20 s ExitTimeOut.
const UNLOAD_TIMEOUT: Duration = Duration::from_secs(25);
const USAGE: &str = "Usage: agent service install [--on <host>]|status|restart|uninstall.";
const INSTALLER: &str = "https://agent.rokartur.com/install.sh";

fn launchd() -> bool {
    cfg!(target_os = "macos")
}

pub fn command(args: &[String]) -> Result<(), String> {
    let home = crate::home()?;
    match args {
        [action] if action == "install" => install(&home),
        [action, on, host] if action == "install" && on == "--on" && !host.starts_with('-') => install_on(host),
        [action] if action == "status" => status(&home),
        [action] if action == "restart" => {
            if !file(&home).exists() {
                return Err("The service is not installed; run agent service install.".into());
            }
            restart()?;
            println!("Restarted the listener.");
            Ok(())
        }
        [action] if action == "uninstall" => uninstall(&home),
        _ => Err(USAGE.into()),
    }
}

/// After an update swapped the binary, so the listener runs the new release and clients of that release are let in.
pub fn restart_if_installed(home: &Path) -> Result<bool, String> {
    if !file(home).exists() {
        return Ok(false);
    }
    restart().map(|()| true)
}

fn file(home: &Path) -> PathBuf {
    if launchd() {
        return home.join(format!("Library/LaunchAgents/{LABEL}.plist"));
    }
    // systemd reads user units from $XDG_CONFIG_HOME instead of ~/.config when it is set.
    let config = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).filter(|dir| dir.is_absolute()).unwrap_or_else(|| home.join(".config"));
    config.join("systemd/user").join(UNIT)
}

fn log(home: &Path) -> String {
    if launchd() { home.join(LOG).display().to_string() } else { format!("journalctl --user -u {UNIT}") }
}

/// launchd never rotates the log, and while Tailscale is down every restart adds a line, one each 10 s.
/// launchd opens it with O_APPEND, so emptying it under the open descriptor is safe.
pub fn trim_log(home: &Path) {
    let log = home.join(LOG);
    if std::fs::metadata(&log).is_ok_and(|meta| meta.len() > LOG_LIMIT) {
        _ = std::fs::File::create(&log);
    }
}

/// Writes the startup file anew and reloads it, so running it again also repairs a moved binary or a changed PATH.
fn install(home: &Path) -> Result<(), String> {
    let settings = crate::settings::load(home)?;
    if !settings.serve_tailnet {
        // Installing the listener is the opt-in to a network port.
        let after = crate::settings::Settings { serve_tailnet: true, ..settings.clone() };
        crate::settings::save_change(home, &settings, &after)?;
        println!("Turned on \"serve_tailnet\" in ~/.agents/settings.json.");
    }
    let exe = std::env::current_exe().map_err(|e| format!("Cannot find the agent binary: {e}"))?;
    let exe = exe.to_str().ok_or("The agent binary's path is not UTF-8.")?;
    // Sessions get the listener's environment, so they find the same tools as the shell that installs it.
    let path = std::env::var("PATH").map_err(|_| "PATH is not set.")?;
    let file = file(home);
    let text = if launchd() { plist(exe, &path, &log(home)) } else { unit(exe, &path) };
    let dir = file.parent().expect("the startup file sits in a directory");
    std::fs::create_dir_all(dir).map_err(|e| format!("Cannot create {}: {e}", dir.display()))?;
    std::fs::create_dir_all(home.join(".agents")).map_err(|e| format!("Cannot create {}: {e}", home.join(".agents").display()))?;
    std::fs::write(&file, text).map_err(|e| format!("Cannot write {}: {e}", file.display()))?;
    if launchd() {
        unload()?;
        run("launchctl", &["bootstrap", &domain(), &file.to_string_lossy()])
            .map_err(|e| format!("{e} A launchd agent needs this user logged in on the screen; a headless Mac needs automatic login."))?;
    } else {
        run("systemctl", &["--user", "daemon-reload"])?;
        run("systemctl", &["--user", "enable", UNIT])?;
        run("systemctl", &["--user", "restart", UNIT])?;
        // Lingering starts the user's units at boot, before anyone logs in, and keeps them after the last logout.
        run("loginctl", &["enable-linger"])?;
    }
    println!("Installed {}. The listener runs now and after every login; its log: {}.", file.display(), log(home));
    Ok(())
}

/// `agent service install --on <host>`: over one `ssh <host>`, installs agent there when it is missing, then runs
/// `agent service install` there. The host's output shows here, and its exit status is this command's.
fn install_on(host: &str) -> Result<(), String> {
    println!("Connecting to {host} over ssh...");
    let status = process::command("ssh").args([host, &remote_install()]).stdin(std::process::Stdio::inherit()).status().map_err(|e| format!("Cannot run ssh: {e}"))?;
    match status.code() {
        Some(0) => {
            println!("Done on {host}.");
            Ok(())
        }
        // ssh exits 255 for its own failures, which it has printed.
        Some(255) => Err(format!("ssh {host} failed. agent service install --on needs `ssh {host}` to work from this computer.")),
        _ => Err(format!("agent service install failed on {host}: {status}.")),
    }
}

/// The script `ssh` runs on the host. Through `sh -c`, so a fish or other login shell runs it the same way; the final
/// install runs in the login shell again, since the PATH it records is the one sessions there get. A release older
/// than `agent service` would take `service install` as a message and run an agent turn, so it is refused first.
fn remote_install() -> String {
    let script = format!(
        r#"bin=$(command -v agent || echo "$HOME/.local/bin/agent"); if [ ! -x "$bin" ]; then echo "Installing agent..."; curl -fsSL {INSTALLER} | sh || exit 1; bin="$HOME/.local/bin/agent"; fi; if ! "$bin" --help | grep -q "service install"; then echo "$("$bin" --version) here has no agent service; run agent update here, then try again." >&2; exit 1; fi; echo "Found $("$bin" --version) at $bin; installing the listener..."; exec "${{SHELL:-/bin/sh}}" -lc "'$bin' service install""#
    );
    format!("sh -c '{}'", script.replace('\'', r"'\''"))
}

fn status(home: &Path) -> Result<(), String> {
    let file = file(home);
    if !file.exists() {
        println!("Not installed. agent service install starts the listener at login.");
        return Ok(());
    }
    let answer = if launchd() { run("launchctl", &["print", &target()]) } else { run("systemctl", &["--user", "show", UNIT, "--property=MainPID"]) };
    match answer {
        Ok(text) => match pid(&text) {
            Some(pid) => println!("Running, pid {pid}."),
            None => println!("Installed, not running."),
        },
        Err(_) => println!("Installed in {}, not loaded; agent service install loads it.", file.display()),
    }
    println!("Log: {}", log(home));
    Ok(())
}

fn restart() -> Result<(), String> {
    if launchd() { run("launchctl", &["kickstart", "-k", &target()]) } else { run("systemctl", &["--user", "restart", UNIT]) }.map(drop)
}

fn uninstall(home: &Path) -> Result<(), String> {
    let file = file(home);
    if !file.exists() {
        println!("Not installed.");
        return Ok(());
    }
    if launchd() {
        unload()?;
    } else {
        run("systemctl", &["--user", "disable", "--now", UNIT])?;
    }
    std::fs::remove_file(&file).map_err(|e| format!("Cannot remove {}: {e}", file.display()))?;
    if !launchd() {
        run("systemctl", &["--user", "daemon-reload"])?;
    }
    println!("Uninstalled. The listener stopped and no longer starts at login; running sessions keep running.");
    Ok(())
}

/// bootout returns while the listener still stops, and a bootstrap before it is gone fails with "5: Input/output error".
fn unload() -> Result<(), String> {
    let loaded = || run("launchctl", &["print", &target()]).is_ok();
    if !loaded() {
        return Ok(());
    }
    // A job that finishes stopping between the two calls makes bootout fail with "3: No such process", so the
    // loop below, not its answer, decides.
    let _ = run("launchctl", &["bootout", &target()]);
    let deadline = Instant::now() + UNLOAD_TIMEOUT;
    while loaded() {
        if Instant::now() > deadline {
            return Err(format!("launchd still runs {LABEL} {} s after bootout.", UNLOAD_TIMEOUT.as_secs()));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

fn domain() -> String {
    // SAFETY: getuid takes no arguments and cannot fail.
    format!("gui/{}", unsafe { libc::getuid() })
}

fn target() -> String {
    format!("{}/{LABEL}", domain())
}

fn run(program: &str, args: &[&str]) -> Result<String, String> {
    let output = process::command(program).args(args).output().map_err(|e| format!("Cannot run {program}: {e}"))?;
    if !output.status.success() {
        let said = String::from_utf8_lossy(&output.stderr);
        return Err(format!("{program} {} failed: {}.", args.join(" "), said.trim().trim_end_matches('.')));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The listener's pid in `launchctl print` (`pid = 42`) or `systemctl show` (`MainPID=42`, 0 when stopped).
fn pid(text: &str) -> Option<u32> {
    text.lines().find_map(|line| line.trim().strip_prefix("pid = ").or_else(|| line.strip_prefix("MainPID="))?.parse().ok()).filter(|&pid| pid > 0)
}

/// `KeepAlive` starts it again when it exits, as it does while Tailscale is not up yet after a login.
fn plist(exe: &str, path: &str, log: &str) -> String {
    let [label, exe, path, log] = [LABEL, exe, path, log].map(xml);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key>
  <array><string>{exe}</string><string>serve</string><string>--tailnet</string></array>
  <key>EnvironmentVariables</key>
  <dict><key>PATH</key><string>{path}</string></dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#
    )
}

/// `KillMode=process`: a restart ends only the listener, not the session servers it started in its cgroup.
fn unit(exe: &str, path: &str) -> String {
    let [exe, path] = [exe, &format!("PATH={path}")].map(quoted);
    format!(
        "[Unit]
Description=agent sessions over Tailscale
After=network-online.target tailscaled.service

[Service]
ExecStart={exe} serve --tailnet
Environment={path}
Restart=always
RestartSec=5
KillMode=process

[Install]
WantedBy=default.target
"
    )
}

fn xml(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// A double-quoted systemd word; `%` starts a specifier there, so it doubles.
fn quoted(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\"").replace('%', "%%"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_plist_runs_the_listener_at_login_and_keeps_it_alive() {
        let text = plist("/Users/a&b/bin/agent", "/opt/homebrew/bin:/usr/bin", "/Users/a&b/.agents/tailnet.log");
        assert!(text.contains("<array><string>/Users/a&amp;b/bin/agent</string><string>serve</string><string>--tailnet</string></array>"));
        assert!(text.contains("<key>PATH</key><string>/opt/homebrew/bin:/usr/bin</string>"));
        assert!(text.contains("<key>RunAtLoad</key><true/>") && text.contains("<key>KeepAlive</key><true/>"));
        assert!(text.contains("<key>StandardErrorPath</key><string>/Users/a&amp;b/.agents/tailnet.log</string>"));
    }

    #[test]
    fn the_unit_quotes_its_paths_and_spares_the_sessions_on_restart() {
        let text = unit("/home/a b/100%/agent", "/home/a b/bin:/usr/bin");
        assert!(text.contains("ExecStart=\"/home/a b/100%%/agent\" serve --tailnet\n"));
        assert!(text.contains("Environment=\"PATH=/home/a b/bin:/usr/bin\"\n"));
        assert!(text.contains("Restart=always\n") && text.contains("KillMode=process\n") && text.contains("WantedBy=default.target\n"));
    }

    #[test]
    fn the_pid_comes_from_launchctl_or_systemctl() {
        assert_eq!(pid("gui/501/com.rokartur.agent.tailnet = {\n\tactive count = 1\n\tstate = running\n\tpid = 4242\n}"), Some(4242));
        assert_eq!(pid("gui/501/com.rokartur.agent.tailnet = {\n\tstate = not running\n\tlast exit code = 1\n}"), None);
        assert_eq!(pid("MainPID=4242\n"), Some(4242));
        assert_eq!(pid("MainPID=0\n"), None);
    }

    #[test]
    fn a_log_past_its_limit_is_emptied_under_the_appending_writer() {
        use std::io::Write;
        let home = std::env::temp_dir().join(format!("agent-service-log-{}", std::process::id()));
        std::fs::create_dir_all(home.join(".agents")).unwrap();
        let log = home.join(LOG);
        let mut writer = std::fs::OpenOptions::new().create(true).append(true).open(&log).unwrap();
        writer.write_all(&vec![b'x'; LOG_LIMIT as usize]).unwrap();
        trim_log(&home);
        assert_eq!(std::fs::metadata(&log).unwrap().len(), LOG_LIMIT, "a log at its limit stays");
        writer.write_all(b"x").unwrap();
        trim_log(&home);
        writer.write_all(b"after\n").unwrap();
        let text = std::fs::read_to_string(&log).unwrap();
        std::fs::remove_dir_all(&home).unwrap();
        assert_eq!(text, "after\n");
    }

    #[test]
    fn the_remote_install_runs_the_hosts_agent_in_its_login_shell_and_refuses_an_old_release() {
        let home = std::env::temp_dir().join(format!("agent-service-remote-{}", std::process::id()));
        let bin = home.join(".local/bin");
        std::fs::create_dir_all(&bin).unwrap();
        // The command line ssh hands the host's login shell, run here by sh with no agent on PATH.
        let run_with = |help: &str| {
            std::fs::write(bin.join("agent"), format!("#!/bin/sh\ncase \"$1\" in --help) echo '{help}';; --version) echo 'agent 0.0.1';; *) echo \"ran: $*\";; esac\n")).unwrap();
            std::fs::set_permissions(bin.join("agent"), std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
            process::command("/bin/sh").args(["-c", &remote_install()]).env("HOME", &home).env("PATH", "/usr/bin:/bin").env("SHELL", "/bin/sh").output().unwrap()
        };
        let current = run_with("  service install   Run serve --tailnet");
        let old = run_with("  attach [id]");
        std::fs::remove_dir_all(&home).unwrap();
        let found = format!("Found agent 0.0.1 at {}; installing the listener...\nran: service install\n", bin.join("agent").display());
        assert_eq!(String::from_utf8_lossy(&current.stdout), found, "{}", String::from_utf8_lossy(&current.stderr));
        assert!(current.status.success());
        assert_eq!(String::from_utf8_lossy(&old.stdout), "");
        assert_eq!(String::from_utf8_lossy(&old.stderr), "agent 0.0.1 here has no agent service; run agent update here, then try again.\n");
        assert!(!old.status.success());
    }
}
