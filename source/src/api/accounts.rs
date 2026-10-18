use serde_json::{Value, json};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::oauth::Tokens;

#[derive(Clone, Debug, PartialEq)]
pub struct Account {
    pub provider: String,
    pub name: String,
    pub secret: Secret,
}

#[derive(Clone, PartialEq)]
pub enum Secret {
    ApiKey(String),
    OAuth(Tokens),
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Secret::ApiKey(_) => f.write_str("ApiKey(..)"),
            Secret::OAuth(tokens) => f.debug_tuple("OAuth").field(tokens).finish(),
        }
    }
}

impl Account {
    pub fn is_oauth(&self) -> bool {
        matches!(self.secret, Secret::OAuth(_))
    }

    /// The Claude account uuid or ChatGPT account id; API keys have none.
    pub fn account_id(&self) -> Option<&str> {
        match &self.secret {
            Secret::OAuth(tokens) => tokens.account_id.as_deref(),
            Secret::ApiKey(_) => None,
        }
    }
}

pub fn path(home: &Path) -> PathBuf {
    home.join(".agents/accounts.json")
}

/// Every saved account, in priority order per provider. A missing file is no accounts; a broken one is an error, never overwritten.
pub fn load(home: &Path) -> Result<Vec<Account>, String> {
    let path = path(home);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("Cannot read {}: {e}", path.display())),
    };
    let file: Value = serde_json::from_str(&text).map_err(|e| format!("{} is not valid JSON: {e}", path.display()))?;
    // update rewrites the whole file from what load returned, so anything skipped here would be deleted.
    let broken = |what: &str| format!("{} is broken: {what}. Fix or remove it by hand.", path.display());
    let file = file.as_object().ok_or_else(|| broken("not a JSON object"))?;
    let mut accounts = Vec::new();
    for (provider, entries) in file {
        let entries = entries.as_array().ok_or_else(|| broken(&format!("{provider} is not a list")))?;
        for entry in entries {
            let text = |key: &str| entry[key].as_str().map(String::from).ok_or_else(|| broken(&format!("a {provider} entry has no {key}")));
            let secret = match entry["type"].as_str() {
                Some("api_key") => Secret::ApiKey(text("key")?),
                Some("oauth") => Secret::OAuth(Tokens {
                    access: text("access")?,
                    refresh: text("refresh")?,
                    expires: entry["expires"].as_u64().ok_or_else(|| broken(&format!("a {provider} entry has no expires")))?,
                    account_id: entry["accountId"].as_str().map(String::from),
                }),
                _ => return Err(broken(&format!("a {provider} entry has an unknown type"))),
            };
            accounts.push(Account { provider: provider.clone(), name: text("name")?, secret });
        }
    }
    Ok(accounts)
}

fn save(home: &Path, accounts: &[Account]) -> Result<(), String> {
    let mut file = json!({});
    for account in accounts {
        let entry = match &account.secret {
            Secret::ApiKey(key) => json!({"name": account.name, "type": "api_key", "key": key}),
            Secret::OAuth(t) => json!({"name": account.name, "type": "oauth", "access": t.access, "refresh": t.refresh, "expires": t.expires, "accountId": t.account_id}),
        };
        match file[&account.provider].as_array_mut() {
            Some(list) => list.push(entry),
            None => file[&account.provider] = json!([entry]),
        }
    }
    write_private(&path(home), &format!("{file:#}\n"))
}

/// Writes `text` through a 0600 temp file and a rename, so a crash never leaves half a file.
pub(crate) fn write_private(path: &Path, text: &str) -> Result<(), String> {
    // Renaming over a symlink would replace a dotfiles link with a plain file.
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    // Per process: two agents sharing one temp name would rename each other's half-written file into place.
    let temp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let mut out = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&temp).map_err(|e| format!("Cannot write {}: {e}", temp.display()))?;
    // mode() applies only on create, so a leftover temp file is made private before the secrets go in.
    let written = out.set_permissions(std::fs::Permissions::from_mode(0o600)).and_then(|()| out.write_all(text.as_bytes())).and_then(|()| out.sync_all()).and_then(|()| std::fs::rename(&temp, &path));
    written.map_err(|e| {
        let _ = std::fs::remove_file(&temp);
        format!("Cannot write {}: {e}", path.display())
    })
}

/// Reads, changes and writes the file under a lock, so accounts another agent process saves meanwhile survive.
pub fn update(home: &Path, change: impl FnOnce(&mut Vec<Account>) -> Result<(), String>) -> Result<Vec<Account>, String> {
    locked(&path(home), || {
        let mut accounts = load(home)?;
        change(&mut accounts)?;
        save(home, &accounts)?;
        Ok(accounts)
    })
}

/// Runs `change` holding the `<path>.lock` file lock, which every agent process takes before it writes `path`.
pub(crate) fn locked<T>(path: &Path, change: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    let lock_path = path.with_extension("json.lock");
    std::fs::create_dir_all(lock_path.parent().expect("stores sit in ~/.agents")).map_err(|e| format!("Cannot create {}: {e}", lock_path.display()))?;
    let lock = std::fs::OpenOptions::new().write(true).create(true).truncate(false).mode(0o600).open(&lock_path).map_err(|e| format!("Cannot open {}: {e}", lock_path.display()))?;
    lock.lock().map_err(|e| format!("Cannot lock {}: {e}", lock_path.display()))?;
    change()
}

/// Subscription windows in percent (0-100) and when the fuller one resets.
#[derive(Clone, Debug, PartialEq)]
pub struct Usage {
    pub five_hour: f64,
    pub seven_day: f64,
    /// Unix milliseconds.
    pub resets_at: Option<u64>,
}

impl Usage {
    pub fn peak(&self) -> f64 {
        self.five_hour.max(self.seven_day)
    }
}

/// `GET /api/oauth/usage`: `five_hour` and `seven_day` with `utilization` in percent and ISO `resets_at`.
fn parse_claude_usage(data: &Value) -> Option<Usage> {
    let window = |key: &str| (data[key]["utilization"].as_f64(), data[key]["resets_at"].as_str().and_then(iso_ms));
    let ((five_hour, five_reset), (seven_day, seven_reset)) = (window("five_hour"), window("seven_day"));
    if five_hour.is_none() && seven_day.is_none() {
        return None;
    }
    let (five_hour, seven_day) = (five_hour.unwrap_or(0.0), seven_day.unwrap_or(0.0));
    Some(Usage { five_hour, seven_day, resets_at: if five_hour >= seven_day { five_reset } else { seven_reset } })
}

/// `GET /wham/usage`: `rate_limit.primary_window` (5h) and `secondary_window` (7d) with `used_percent` and unix `reset_at`.
fn parse_codex_usage(data: &Value) -> Option<Usage> {
    let window = |key: &str| {
        let window = &data["rate_limit"][key];
        (window["used_percent"].as_f64(), window["reset_at"].as_f64().map(|secs| secs as u64 * 1000))
    };
    let ((five_hour, five_reset), (seven_day, seven_reset)) = (window("primary_window"), window("secondary_window"));
    let five_hour = five_hour?;
    let seven_day = seven_day.unwrap_or(0.0);
    Some(Usage { five_hour, seven_day, resets_at: if five_hour >= seven_day { five_reset } else { seven_reset } })
}

/// Fetches an OAuth account's windows; `base` is the provider's API root.
pub fn fetch_usage(account: &Account, base: &str) -> Result<Usage, String> {
    let Secret::OAuth(tokens) = &account.secret else { return Err("API keys have no usage windows.".into()) };
    let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).timeout_global(Some(Duration::from_secs(10))).build().into();
    let request = match account.provider.as_str() {
        "anthropic" => agent.get(format!("{base}/api/oauth/usage")).header("anthropic-beta", "oauth-2025-04-20,claude-code-20250219").header("user-agent", crate::api::CLAUDE_CODE_USER_AGENT),
        _ => agent.get(format!("{base}/wham/usage")).header("chatgpt-account-id", tokens.account_id.as_deref().unwrap_or_default()),
    };
    let response = request.header("authorization", format!("Bearer {}", tokens.access)).header("accept", "application/json").call().map_err(|e| format!("Usage request failed: {e}."))?;
    let status = response.status().as_u16();
    let mut text = String::new();
    response.into_body().into_reader().read_to_string(&mut text).map_err(|e| e.to_string())?;
    if status != 200 {
        return Err(format!("Usage request failed ({status})."));
    }
    let data: Value = serde_json::from_str(&text).map_err(|e| format!("Usage response is not JSON: {e}."))?;
    let usage = if account.provider == "anthropic" { parse_claude_usage(&data) } else { parse_codex_usage(&data) };
    usage.ok_or("The usage response has no windows.".into())
}

/// `accounts.when_all_over_limit`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Policy {
    LeastUsed,
    Block,
    OtherProvider,
}

impl Policy {
    pub const ALL: [Policy; 3] = [Policy::LeastUsed, Policy::Block, Policy::OtherProvider];

    pub fn parse(text: &str) -> Option<Policy> {
        Policy::ALL.into_iter().find(|p| p.label() == text)
    }

    pub fn label(self) -> &'static str {
        match self {
            Policy::LeastUsed => "least-used",
            Policy::Block => "block",
            Policy::OtherProvider => "other-provider",
        }
    }
}

/// What the picker knows about one account: its windows (None for API keys or a failed fetch) and a recent 429.
pub struct Candidate {
    pub usage: Option<Usage>,
    pub limited: bool,
}

#[derive(Debug, PartialEq)]
pub enum Pick {
    Account(usize),
    /// Every account is over the threshold or rate-limited; this one has the most room left.
    OverLimit(usize),
    Blocked {
        resets_at: Option<u64>,
    },
    OtherProvider,
}

/// The pinned account while it is under `threshold` and not rate-limited, else the first such account, else what `policy` says.
/// A `threshold` of 0 turns switching off: always the pinned (or first) account, even when limited.
pub fn pick(candidates: &[Candidate], threshold: f64, policy: Policy, pinned: Option<usize>) -> Pick {
    if threshold <= 0.0 {
        return Pick::Account(pinned.unwrap_or(0));
    }
    let eligible = |c: &Candidate| !c.limited && c.usage.as_ref().is_none_or(|u| u.peak() < threshold);
    if let Some(index) = pinned.filter(|i| candidates.get(*i).is_some_and(eligible)) {
        return Pick::Account(index);
    }
    if let Some(index) = candidates.iter().position(eligible) {
        return Pick::Account(index);
    }
    let peak = |c: &Candidate| c.usage.as_ref().map_or(0.0, Usage::peak);
    match policy {
        Policy::LeastUsed => {
            let pool: Vec<usize> = (0..candidates.len()).filter(|i| !candidates[*i].limited).collect();
            let pool = if pool.is_empty() { (0..candidates.len()).collect() } else { pool };
            let least = pool.into_iter().min_by(|a, b| peak(&candidates[*a]).total_cmp(&peak(&candidates[*b])));
            Pick::OverLimit(least.expect("pick gets at least one candidate"))
        }
        Policy::Block => Pick::Blocked { resets_at: candidates.iter().filter_map(|c| c.usage.as_ref()?.resets_at).min() },
        Policy::OtherProvider => Pick::OtherProvider,
    }
}

/// "in 2h 13m" for a unix-millisecond time.
pub fn resets_in(resets_at: u64) -> String {
    let minutes = resets_at.saturating_sub(crate::process::now_ms()) / 60_000;
    if minutes >= 60 { format!("in {}h {}m", minutes / 60, minutes % 60) } else { format!("in {minutes}m") }
}

/// Unix milliseconds of an ISO 8601 UTC time like `2026-09-27T05:30:00.000Z` or `+00:00`.
fn iso_ms(text: &str) -> Option<u64> {
    let (date, time) = text.split_once('T')?;
    let mut date = date.split('-').map(|p| p.parse::<i64>().ok());
    let (year, month, day) = (date.next()??, date.next()??, date.next()??);
    let time = time.trim_end_matches('Z');
    let time = time.split(['+']).next()?;
    let mut clock = time.split(':');
    let (hour, minute) = (clock.next()?.parse::<i64>().ok()?, clock.next()?.parse::<i64>().ok()?);
    let second = clock.next().map_or(Some(0.0), |s| s.parse::<f64>().ok())? as i64;
    // Howard Hinnant's days_from_civil.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    u64::try_from(((days * 24 + hour) * 60 + minute) * 60 + second).ok().map(|secs| secs * 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(five_hour: f64, seven_day: f64) -> Option<Usage> {
        Some(Usage { five_hour, seven_day, resets_at: Some(five_hour as u64) })
    }

    #[test]
    fn pick_follows_threshold_pin_and_policies() {
        let fresh = [Candidate { usage: usage(95.0, 10.0), limited: false }, Candidate { usage: usage(20.0, 30.0), limited: false }, Candidate { usage: None, limited: false }];
        assert_eq!(pick(&fresh, 90.0, Policy::Block, None), Pick::Account(1));
        assert_eq!(pick(&fresh, 90.0, Policy::Block, Some(2)), Pick::Account(2));
        assert_eq!(pick(&fresh, 90.0, Policy::Block, Some(0)), Pick::Account(1));
        let full = [Candidate { usage: usage(95.0, 10.0), limited: false }, Candidate { usage: usage(91.0, 50.0), limited: false }, Candidate { usage: None, limited: true }];
        assert_eq!(pick(&full, 90.0, Policy::LeastUsed, None), Pick::OverLimit(1));
        assert_eq!(pick(&full, 90.0, Policy::Block, None), Pick::Blocked { resets_at: Some(91) });
        assert_eq!(pick(&full, 90.0, Policy::OtherProvider, None), Pick::OtherProvider);
        let all_limited = [Candidate { usage: None, limited: true }];
        assert_eq!(pick(&all_limited, 90.0, Policy::LeastUsed, None), Pick::OverLimit(0));
        assert_eq!(pick(&all_limited, 0.0, Policy::Block, Some(0)), Pick::Account(0));
        assert_eq!(pick(&full, 0.0, Policy::Block, Some(2)), Pick::Account(2));
    }

    #[test]
    fn parses_both_usage_endpoints() {
        let claude = json!({"five_hour": {"utilization": 26.0, "resets_at": "2026-09-27T05:30:00.000Z"}, "seven_day": {"utilization": 40.5, "resets_at": "2026-10-01T00:00:00+00:00"}});
        assert_eq!(parse_claude_usage(&claude), Some(Usage { five_hour: 26.0, seven_day: 40.5, resets_at: Some(1_790_812_800_000) }));
        assert_eq!(iso_ms("1970-01-02T00:00:01Z"), Some(86_401_000));
        let codex = json!({"rate_limit": {"allowed": true, "primary_window": {"used_percent": 93, "reset_at": 1790000000, "limit_window_seconds": 18000}, "secondary_window": null}});
        assert_eq!(parse_codex_usage(&codex), Some(Usage { five_hour: 93.0, seven_day: 0.0, resets_at: Some(1_790_000_000_000) }));
        assert_eq!(parse_codex_usage(&json!({"plan_type": "free"})), None);
    }

    #[test]
    fn file_round_trips_with_owner_only_access() {
        let home = std::env::temp_dir().join(format!("agent-accounts-{}", std::process::id()));
        let tokens = Tokens { access: "a".into(), refresh: "r".into(), expires: 5, account_id: Some("acc".into()) };
        let accounts = [
            Account { provider: "openai-codex".into(), name: "work".into(), secret: Secret::OAuth(tokens) },
            Account { provider: "anthropic".into(), name: "key".into(), secret: Secret::ApiKey("sk".into()) },
        ];
        update(&home, |list| {
            list.extend(accounts.iter().cloned());
            Ok(())
        })
        .unwrap();
        let mut loaded = load(&home).unwrap();
        loaded.sort_by(|a, b| a.provider.cmp(&b.provider));
        assert_eq!(loaded, [accounts[1].clone(), accounts[0].clone()]);
        assert_eq!(std::fs::metadata(path(&home)).unwrap().permissions().mode() & 0o777, 0o600);
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn concurrent_updates_keep_every_account() {
        let home = std::env::temp_dir().join(format!("agent-accounts-race-{}", std::process::id()));
        let threads: Vec<_> = (0..8)
            .map(|n| {
                let home = home.clone();
                std::thread::spawn(move || {
                    update(&home, |accounts| {
                        accounts.push(Account { provider: "anthropic".into(), name: n.to_string(), secret: Secret::ApiKey("k".into()) });
                        Ok(())
                    })
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap().unwrap();
        }
        let saved = load(&home).unwrap().len();
        std::fs::remove_dir_all(&home).unwrap();
        assert_eq!(saved, 8);
    }

    #[test]
    fn an_entry_load_cannot_read_is_an_error_not_dropped() {
        let home = std::env::temp_dir().join(format!("agent-accounts-shape-{}", std::process::id()));
        std::fs::create_dir_all(home.join(".agents")).unwrap();
        std::fs::write(path(&home), r#"{"anthropic": [{"name": "a", "type": "OAuth"}]}"#).unwrap();
        let loaded = load(&home);
        std::fs::remove_dir_all(&home).unwrap();
        assert!(loaded.unwrap_err().contains("unknown type"));
    }
}
