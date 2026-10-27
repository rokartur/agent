//! `agent service`: runs `agent serve --tailnet` at login, as a launchd agent on macOS or a systemd user unit on
//! Linux, so a host accepts sessions with no terminal open on it.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::process;

const LABEL: &str = "com.rokartur.agent.tailnet";
const UNIT: &str = "agent-tailnet.service";
/// launchd sends SIGTERM and SIGKILL after its 20 s ExitTimeOut.
const UNLOAD_TIMEOUT: Duration = Duration::from_secs(25);
const USAGE: &str = "Usage: agent service install|status|restart|uninstall.";

fn launchd() -> bool {
    cfg!(target_os = "macos")
}

pub fn command(args: &[String]) -> Result<(), String> {
    let home = crate::home()?;
    match args {
        [action] if action == "install" => install(&home),
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
    if launchd() { home.join(".agents/tailnet.log").display().to_string() } else { format!("journalctl --user -u {UNIT}") }
}

/// Writes the startup file anew and reloads it, so running it again also repairs a moved binary or a changed PATH.
fn install(home: &Path) -> Result<(), String> {
    if !crate::settings::load(home)?.serve_tailnet {
        return Err("Turn the listener on first: set \"serve_tailnet\": true in ~/.agents/settings.json.".into());
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
}
