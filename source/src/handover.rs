//! /reload execs the binary on disk on the same pid (`reload_server` in main.rs). Children stay ours across exec, so the
//! old server lists what follows them in a file and the new one takes them over: background terminals, subagents with
//! their pipes, scheduled tasks, queued follow-ups, and the id the sessions it started report to.

use serde_json::{Value, json};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::agent::subagent::{self, Runs};
use crate::agent::{Agent, FollowUp};
use crate::settings::Settings;
use crate::tools::bg::{Status, Terminals};
use crate::tools::cron::{self, Crons};

/// The path of the file the server before /reload wrote.
pub const ENV: &str = "AGENT_HANDOVER";

/// Ends every child but the running terminals and subagents, then writes the file `command` finds through `ENV`.
/// Runs last before exec: a subagent event read after it is lost.
pub fn prepare(mut command: Command, terminals: &Mutex<Terminals>, subagents: &Mutex<Runs>, crons: &Mutex<Crons>, follow_ups: &Mutex<Vec<FollowUp>>, first_session: &str) -> Command {
    let mut keep: Vec<u32> = terminals.lock().unwrap().list.iter().filter(|t| t.status() == Status::Running).map(|t| t.pid).collect();
    keep.extend(subagent::pids_here(&subagents.lock().unwrap()));
    // MCP servers, plugins and language servers run in sessions of their own, so closed pipes alone may not end them.
    let ended = crate::process::hang_up_children_except(&keep);
    // The new server knows none of them, so a child left unreaped would stay a zombie for the rest of the session.
    let deadline = Instant::now() + Duration::from_millis(500);
    let mut left = ended;
    while !left.is_empty() && Instant::now() < deadline {
        // SAFETY: waitpid with a null status pointer only reaps; it returns the pid once that child is gone.
        left.retain(|&pid| unsafe { libc::waitpid(pid as i32, std::ptr::null_mut(), libc::WNOHANG) } == 0);
        std::thread::sleep(Duration::from_millis(10));
    }
    let handed = json!({
        "terminals": terminals.lock().unwrap().hand_over(),
        "subagents": subagent::hand_over(&subagents.lock().unwrap()),
        "crons": crons.lock().unwrap().hand_over(),
        "follow_ups": follow_ups.lock().unwrap().iter().map(|f| json!({"source": f.source, "text": f.text})).collect::<Vec<_>>(),
        "first_session": first_session,
    });
    let path = crate::tools::spill_dir().join("handover.json");
    let written = crate::process::private_dir(&crate::tools::spill_dir())
        .and_then(|()| crate::process::private_file().write(true).create(true).truncate(true).open(&path).map_err(|e| e.to_string()))
        .and_then(|mut file| std::io::Write::write_all(&mut file, handed.to_string().as_bytes()).map_err(|e| e.to_string()));
    match written {
        Ok(()) => _ = command.env(ENV, &path),
        // The new server would not know them, and nothing would stop them.
        Err(_) => {
            terminals.lock().unwrap().kill_all();
            subagents.lock().unwrap().stop_all();
        }
    }
    command
}

/// Takes over what `prepare` listed; a server not started by /reload has no `ENV`.
pub fn adopt(agent: &mut Agent, settings: &Settings) -> Result<(), String> {
    let Some(path) = std::env::var_os(ENV) else { return Ok(()) };
    // A second /reload reuses the path, so a missing file means this process already took it over.
    let Ok(text) = std::fs::read_to_string(&path) else { return Ok(()) };
    let _ = std::fs::remove_file(&path);
    let handed: Value = serde_json::from_str(&text).map_err(|e| format!("Cannot read what /reload handed over: {e}"))?;
    agent.terminals.lock().unwrap().adopt(&handed["terminals"], &agent.follow_ups);
    subagent::adopt(&agent.subagents, &handed["subagents"], settings, &agent.follow_ups);
    cron::adopt(&agent.crons, &handed["crons"], &agent.follow_ups);
    let queued = handed["follow_ups"].as_array().into_iter().flatten();
    agent.follow_ups.lock().unwrap().extend(queued.map(|f| FollowUp { source: f["source"].as_str().unwrap_or_default().into(), text: f["text"].as_str().unwrap_or_default().into() }));
    if let Some(first_session) = handed["first_session"].as_str() {
        agent.first_session = first_session.into();
    }
    Ok(())
}
