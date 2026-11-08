use std::path::Path;
use std::time::Duration;

use serde_json::Value;

use crate::process;

const RELEASES: &str = "https://github.com/rokartur/agent/releases";
const RELEASES_API: &str = "https://api.github.com/repos/rokartur/agent/releases?per_page=20";
const CURRENT: &str = env!("CARGO_PKG_VERSION");
/// `latest` follows stable releases only, `pre-release` also takes pre-releases, `off` never checks.
pub const CHANNELS: [&str; 3] = ["latest", "pre-release", "off"];
/// GitHub allows 60 unauthenticated API calls an hour per IP, and every TUI start would spend one.
const CHECK_EVERY: Duration = Duration::from_secs(24 * 60 * 60);

/// What an update is doing right now; `agent update` draws it, the start-up check passes `&|_| ()`.
pub enum Progress<'a> {
    Checking,
    Downloading { version: &'a str, received: u64, total: Option<u64> },
    Verifying,
    Installing,
}

/// Only CI release builds update themselves, so a local `cargo build` never gets replaced.
pub fn enabled() -> bool {
    option_env!("AGENT_RELEASE").is_some()
}

/// Replaces the running binary with the newest release on `channel`, once a day. Returns the installed version.
pub fn install_latest(home: &Path, channel: &str) -> Result<Option<String>, String> {
    let stamp = home.join(".agents/update-check");
    if channel == "off" || !due(&stamp, channel) {
        return Ok(None);
    }
    let latest = latest_version(channel == "pre-release");
    // Ok(None) is an offline start, which checks again next start instead of next day.
    if !matches!(latest, Ok(None)) {
        mark_checked(&stamp, channel)?;
    }
    let Some(latest) = latest? else { return Ok(None) };
    install_if_newer(latest, &|_| ())
}

/// `agent update`: checks now, whatever the channel and the last check, and reports an offline start as an error.
pub fn install_now(channel: &str, report: &dyn Fn(Progress)) -> Result<Option<String>, String> {
    report(Progress::Checking);
    let latest = latest_version(channel == "pre-release")?.ok_or("Cannot reach GitHub to check for updates.")?;
    install_if_newer(latest, report)
}

fn install_if_newer(latest: String, report: &dyn Fn(Progress)) -> Result<Option<String>, String> {
    if !is_newer(&latest, CURRENT) {
        return Ok(None);
    }
    let exe = std::env::current_exe().map_err(|e| format!("Cannot find the agent binary: {e}"))?;
    let staged = exe.with_file_name(format!(".agent-update-{}{}", std::process::id(), std::env::consts::EXE_SUFFIX));
    let result = download(&latest, &staged, report)
        .and_then(|()| {
            report(Progress::Verifying);
            verify(&staged, &latest)
        })
        .and_then(|()| {
            report(Progress::Installing);
            replace(&staged, &exe).map_err(|e| format!("Cannot replace {}: {e}", exe.display()))
        });
    if let Err(error) = result {
        let _ = std::fs::remove_file(&staged);
        return Err(error);
    }
    Ok(Some(latest))
}

#[cfg(unix)]
fn replace(staged: &Path, exe: &Path) -> std::io::Result<()> {
    std::fs::rename(staged, exe)
}

/// Windows cannot overwrite a running binary but can rename it, so the running one moves aside first. A binary moved
/// aside by an earlier update is removed once no agent runs it.
#[cfg(windows)]
fn replace(staged: &Path, exe: &Path) -> std::io::Result<()> {
    let dir = exe.parent().expect("a binary sits in a directory");
    for old in std::fs::read_dir(dir)?.map_while(Result::ok) {
        if old.file_name().to_string_lossy().starts_with(".agent-old-") {
            let _ = std::fs::remove_file(old.path());
        }
    }
    let aside = dir.join(format!(".agent-old-{}.exe", std::process::id()));
    std::fs::rename(exe, &aside)?;
    std::fs::rename(staged, exe).map_err(|e| match std::fs::rename(&aside, exe) {
        Ok(()) => e,
        Err(_) => std::io::Error::new(e.kind(), format!("{e}; the previous binary is at {}", aside.display())),
    })
}

/// The stamp holds the channel last checked, so switching channels checks at once.
fn due(stamp: &Path, channel: &str) -> bool {
    let Ok(checked) = std::fs::read_to_string(stamp) else { return true };
    let age = std::fs::metadata(stamp).and_then(|meta| meta.modified()).ok().and_then(|modified| modified.elapsed().ok());
    checked != channel || age.is_none_or(|age| age >= CHECK_EVERY)
}

/// A fresh install that never logged in has no `~/.agents` yet.
fn mark_checked(stamp: &Path, channel: &str) -> Result<(), String> {
    let dir = stamp.parent().expect("the stamp sits in ~/.agents");
    std::fs::create_dir_all(dir).map_err(|e| format!("Cannot create {}: {e}", dir.display()))?;
    std::fs::write(stamp, channel).map_err(|e| format!("Cannot write {}: {e}", stamp.display()))
}

/// Offline start is normal, so a network failure means no update; an HTTP error such as a 403 rate limit is reported.
fn latest_version(include_prereleases: bool) -> Result<Option<String>, String> {
    let http: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(10))).build().into();
    let mut request = http.get(RELEASES_API);
    if let Some(token) = github_token() {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    let mut response = match request.call() {
        Ok(response) => response,
        Err(ureq::Error::StatusCode(status)) => return Err(format!("Cannot check for updates: GitHub answered {status}.")),
        Err(_) => return Ok(None),
    };
    let Ok(body) = response.body_mut().read_to_string() else { return Ok(None) };
    let Ok(releases) = serde_json::from_str::<Value>(&body) else { return Ok(None) };
    // A channel without releases yet counts as up to date.
    Ok(Some(newest(&releases, include_prereleases).unwrap_or(CURRENT).to_owned()))
}

/// GitHub allows a token 5000 API calls an hour instead of 60 per IP. Same order as `gh`: GH_TOKEN, GITHUB_TOKEN, its login.
fn github_token() -> Option<String> {
    let from_env = ["GH_TOKEN", "GITHUB_TOKEN"].into_iter().find_map(|name| std::env::var(name).ok().filter(|token| !token.trim().is_empty()));
    from_env.or_else(|| {
        let output = process::output_within(process::command("gh").args(["auth", "token"]), Duration::from_secs(5)).ok()??;
        let token = String::from_utf8(output.stdout).ok()?.trim().to_owned();
        (output.status.success() && !token.is_empty()).then_some(token)
    })
}

/// A token with access to the repo also lists drafts, whose assets cannot be downloaded.
fn newest(releases: &Value, include_prereleases: bool) -> Option<&str> {
    releases
        .as_array()?
        .iter()
        .filter(|release| release["draft"] != true)
        .filter(|release| include_prereleases || release["prerelease"] == false)
        .filter_map(|release| release["tag_name"].as_str()?.strip_prefix('v'))
        .max_by_key(|version| precedence(version))
}

fn download(version: &str, path: &Path, report: &dyn Fn(Progress)) -> Result<(), String> {
    let asset = format!("agent-{}-{}{}", std::env::consts::OS, std::env::consts::ARCH, std::env::consts::EXE_SUFFIX);
    let url = format!("{RELEASES}/download/v{version}/{asset}");
    let http: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(300))).build().into();
    // Created first, so an unwritable binary directory fails before a transfer of up to 100 MiB.
    let mut file = std::fs::File::create(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::PermissionDenied => {
            let dir = path.parent().unwrap_or(path).display();
            format!("No write access to {dir}, where agent is installed. Reinstall it to a folder you own, or set update_channel to off.")
        }
        _ => format!("Cannot write {}: {e}", path.display()),
    })?;
    let mut response = http.get(&url).call().map_err(|e| format!("Cannot download {url}: {e}"))?;
    let total = response.body().content_length();
    let mut body = response.body_mut().with_config().limit(100 << 20).reader();
    let mut received = 0;
    let mut chunk = [0u8; 64 << 10];
    report(Progress::Downloading { version, received, total });
    loop {
        let read = std::io::Read::read(&mut body, &mut chunk).map_err(|e| format!("Cannot download {url}: {e}"))?;
        if read == 0 {
            break;
        }
        std::io::Write::write_all(&mut file, &chunk[..read]).map_err(|e| format!("Cannot write {}: {e}", path.display()))?;
        received += read as u64;
        report(Progress::Downloading { version, received, total });
    }
    process::make_executable(path).map_err(|e| format!("Cannot make {} executable: {e}", path.display()))
}

/// A truncated or wrong download must never replace a working binary.
fn verify(path: &Path, version: &str) -> Result<(), String> {
    let output = process::output_within(process::command(path).arg("--version"), Duration::from_secs(10))
        .map_err(|e| format!("Cannot run the downloaded agent: {e}"))?
        .ok_or("The downloaded agent did not answer --version within 10 s.")?;
    let reported = String::from_utf8_lossy(&output.stdout);
    if reported.trim() != format!("agent {version}") {
        return Err(format!("The downloaded agent reports {:?}, expected {version}.", reported.trim()));
    }
    Ok(())
}

pub fn is_newer(latest: &str, current: &str) -> bool {
    match (precedence(latest), precedence(current)) {
        (Some(latest), Some(current)) => latest > current,
        _ => false,
    }
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum Identifier {
    Number(u64),
    Text(String),
}

/// Semver order: 0.3.0-beta.2 < 0.3.0-beta.10 < 0.3.0-rc.1 < 0.3.0.
fn precedence(version: &str) -> Option<(Vec<u64>, bool, Vec<Identifier>)> {
    // Build metadata (`+1`) does not take part in semver precedence.
    let version = version.split_once('+').map_or(version, |(version, _)| version);
    let (core, pre) = match version.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (version, None),
    };
    let core = core.split('.').map(|n| n.parse().ok()).collect::<Option<Vec<u64>>>()?;
    let identifiers = pre.into_iter().flat_map(|pre| pre.split('.')).map(|id| id.parse().map_or_else(|_| Identifier::Text(id.to_string()), Identifier::Number)).collect();
    Some((core, pre.is_none(), identifiers))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replace_puts_the_new_binary_in_place_and_clears_older_leftovers() {
        let dir = std::env::temp_dir().join(format!("agent-replace-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (exe, staged, leftover) = (dir.join("agent.exe"), dir.join(".agent-update-1.exe"), dir.join(".agent-old-1.exe"));
        std::fs::write(&exe, "old").unwrap();
        std::fs::write(&staged, "new").unwrap();
        std::fs::write(&leftover, "older").unwrap();

        replace(&staged, &exe).unwrap();

        let installed = std::fs::read_to_string(&exe).unwrap();
        let mut left: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|entry| entry.unwrap().file_name().into_string().unwrap()).collect();
        left.sort();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(installed, "new");
        let aside = format!(".agent-old-{}.exe", std::process::id());
        let expected = if cfg!(windows) { vec![aside.as_str(), "agent.exe"] } else { vec![".agent-old-1.exe", "agent.exe"] };
        assert_eq!(left, expected);
    }

    #[test]
    fn only_a_higher_release_counts_as_newer() {
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(is_newer("0.10.0", "0.9.0"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        assert!(!is_newer("0.1.x", "0.1.0"));
    }

    #[test]
    fn pre_releases_sort_below_their_release() {
        assert!(is_newer("0.3.0-beta.1", "0.2.0"));
        assert!(is_newer("0.3.0-beta.10", "0.3.0-beta.2"));
        assert!(is_newer("0.3.0-rc.1", "0.3.0-beta.10"));
        assert!(is_newer("0.3.1+7", "0.3.0+9") && !is_newer("0.3.0+9", "0.3.0"));
        assert!(is_newer("0.3.0", "0.3.0-rc.1"));
        assert!(!is_newer("0.3.0-rc.1", "0.3.0"));
    }

    #[test]
    fn a_start_checks_at_most_once_a_day_per_channel() {
        let home = std::env::temp_dir().join(format!("agent-update-check-{}", std::process::id()));
        let stamp = home.join(".agents/update-check");
        assert!(due(&stamp, "latest"));
        mark_checked(&stamp, "latest").unwrap();
        assert!(!due(&stamp, "latest"));
        assert!(due(&stamp, "pre-release"));
        let file = std::fs::File::options().write(true).open(&stamp).unwrap();
        file.set_modified(std::time::SystemTime::now() - CHECK_EVERY).unwrap();
        assert!(due(&stamp, "latest"));
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn channel_picks_the_newest_release_it_follows() {
        let releases = serde_json::json!([
            {"tag_name": "v0.4.0", "prerelease": false, "draft": true},
            {"tag_name": "v0.3.0-beta.1", "prerelease": true},
            {"tag_name": "v0.2.1", "prerelease": false},
            {"tag_name": "v0.2.0", "prerelease": false},
        ]);
        assert_eq!(newest(&releases, false), Some("0.2.1"));
        assert_eq!(newest(&releases, true), Some("0.3.0-beta.1"));
    }
}
