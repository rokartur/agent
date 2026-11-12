use super::journal::read_journal;
use super::*;

#[test]
fn a_workflow_command_passes_its_input_as_args() {
    assert!(!command_prompt("deep-research", " ").contains("args"));
    assert!(command_prompt("deep-research", "rust async").ends_with("\n\nrust async"));
}

fn host() -> (Host, Arc<Mutex<Vec<FollowUp>>>) {
    let follow_ups = Arc::new(Mutex::new(Vec::new()));
    let host = Host { runs: Arc::default(), follow_ups: follow_ups.clone(), base: subagent::test_launch(), models: vec![("claude-opus-4".into(), None)] };
    (host, follow_ups)
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("agent-workflow-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A journal line the way `Ledger` writes it, for a call made with `options`.
fn done(prompt: &str, options: &str, result: Value) -> String {
    json!({"key": format!("{prompt}\n{options}"), "n": 0, "event": "done", "result": result}).to_string()
}

fn wait_for_result(workflow: &Workflow) -> Result<String, String> {
    let deadline = Instant::now() + Duration::from_secs(10);
    while workflow.is_running() {
        assert!(Instant::now() < deadline, "the workflow did not finish");
        std::thread::sleep(Duration::from_millis(20));
    }
    workflow.state.lock().unwrap().result.clone().unwrap()
}

#[test]
fn meta_is_read_without_running_the_script() {
    let script = "export const meta = {\n  name: 'audit', // a } in a comment\n  description: 'Find { braces }',\n  phases: ['Find', { title: 'Fix' }],\n}\nawait agent('x')";
    let meta = parse_meta(script).unwrap();
    assert_eq!(meta, Meta { name: "audit".into(), description: "Find { braces }".into(), phases: vec!["Find".into(), "Fix".into()] });
    assert!(parse_meta("const x = 1\nexport const meta = { name: 'a', description: 'b' }").is_err());
    assert!(parse_meta("export const meta = { name: 'a' }").unwrap_err().contains("description"));
}

#[test]
fn a_meta_block_that_loops_forever_is_cut_off() {
    let error = parse_meta("export const meta = { name: (() => { while (true); })(), description: 'b' }").unwrap_err();
    assert!(error.contains("interrupted"), "{error}");
}

#[test]
fn check_rejects_imports_and_syntax_errors() {
    let meta = "export const meta = { name: 'a', description: 'b' }\n";
    assert!(check(&format!("{meta}const fs = await import('fs')")).unwrap_err().contains("cannot load modules"));
    assert!(check(&format!("{meta}import fs from 'fs'")).unwrap_err().contains("cannot load modules"));
    assert!(check(&format!("{meta}const x = ;")).unwrap_err().contains("Syntax error"));
    assert!(check(&format!("{meta}return await agent('x')")).is_ok());
}

#[test]
fn the_bundled_workflow_passes_its_own_checks() {
    assert_eq!(check(DEEP_RESEARCH).unwrap().name, "deep-research");
}

#[test]
fn schemas_validate_and_contradictions_are_found_before_the_agent_starts() {
    let schema = json!({"type": "object", "required": ["files"], "properties": {"files": {"type": "array", "items": {"type": "string"}}}});
    assert!(validate(&json!({"files": ["a.ts"]}), &schema, "$").is_ok());
    assert_eq!(validate(&json!({"files": [1]}), &schema, "$").unwrap_err(), "$.files[0] must be string, got number");
    assert_eq!(validate(&json!({}), &schema, "$").unwrap_err(), "$ is missing the required key `files`");
    assert!(validate(&json!({"verdict": "maybe"}), &json!({"properties": {"verdict": {"enum": ["yes", "no"]}}}), "$").is_err());
    let closed = json!({"type": "object", "properties": {"a": {"type": "object", "additionalProperties": false, "required": ["b"]}}});
    assert!(contradiction(&closed).unwrap().contains("`b` is required"));
    assert!(contradiction(&schema).is_none());
}

#[test]
fn json_answers_are_found_in_fences_and_prose() {
    assert_eq!(parse_json_answer("```json\n{\"a\": 1}\n```").unwrap(), json!({"a": 1}));
    assert_eq!(parse_json_answer("Here it is: [1, 2] done").unwrap(), json!([1, 2]));
    assert!(parse_json_answer("no json here").is_err());
}

#[test]
fn replay_reruns_from_the_first_failed_or_changed_agent() {
    let journal =
        [done("a", "{}", json!("A")), json!({"key": "b\n{}", "n": 0, "event": "failed"}).to_string(), done("c", "{}", json!("C")), json!({"key": "d\n{}", "n": 0, "event": "start"}).to_string()];
    let call = |prompt: &str| Call { id: 0, prompt: prompt.into(), options: json!({}), phase: String::new() };
    let dir = temp_dir("replay");
    let mut ledger = Ledger::open(&dir.join("journal.jsonl"), read_journal(&journal.join("\n"))).unwrap();
    let mut replay = |prompt: &str| {
        let key = ledger.key(&call(prompt));
        ledger.replay(&key)
    };
    assert_eq!(replay("a"), Some(json!("A")));
    assert_eq!(replay("b"), None);
    assert_eq!(replay("c"), None, "every agent after a failed one runs again");

    let mut ledger = Ledger::open(&dir.join("journal.jsonl"), read_journal(&journal.join("\n"))).unwrap();
    let key = ledger.key(&call("d"));
    assert_eq!(ledger.replay(&key), None);
    let key = ledger.key(&call("a"));
    assert_eq!(ledger.replay(&key), Some(json!("A")), "an agent the stopped run never finished does not rerun the rest");
    let key = ledger.key(&call("edited"));
    assert_eq!(ledger.replay(&key), None);
    assert!(ledger.diverged);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_resumed_run_replays_saved_agents_through_the_script() {
    let runs_dir = temp_dir("resume");
    let options = r#"{"label":"find","schema":{"properties":{"files":{"type":"array"}},"type":"object"}}"#;
    let journal = [done("List files.", options, json!({"files": ["a.ts", "b.ts"]})), done("Audit a.ts", "{}", json!("a ok")), done("Audit b.ts", "{}", json!("b ok"))];
    std::fs::create_dir_all(runs_dir.join("earlier")).unwrap();
    std::fs::write(runs_dir.join("earlier/journal.jsonl"), journal.join("\n")).unwrap();
    let script = r#"export const meta = { name: 'audit', description: 'Audit files', phases: ['Find', 'Audit'] }
phase('Find')
const found = await agent('List files.', { label: 'find', schema: { type: 'object', properties: { files: { type: 'array' } } } })
phase('Audit')
const audits = await pipeline(found.files, (file) => agent(`Audit ${file}`))
let clock
try { Date.now() } catch (error) { clock = error.message }
log('audited', audits.length)
return { audits, prefix: args.prefix, clock }"#;
    let (host, follow_ups) = host();
    let workflows = Arc::default();
    let run = launch(&workflows, script.into(), json!({"prefix": "src/"}), Some("earlier"), &runs_dir, host).unwrap();
    let result: Value = serde_json::from_str(&wait_for_result(&run).unwrap()).unwrap();
    assert_eq!(result["audits"], json!(["a ok", "b ok"]));
    assert_eq!(result["prefix"], "src/");
    assert!(result["clock"].as_str().unwrap().contains("Date.now() is disabled"));
    let state = run.state.lock().unwrap();
    assert_eq!(state.phases.iter().map(|phase| (phase.title.as_str(), phase.steps.len())).collect::<Vec<_>>(), [("Find", 1), ("Audit", 2)]);
    assert!(state.phases.iter().flat_map(|phase| &phase.steps).all(|step| step.status == StepStatus::Cached));
    assert_eq!(state.logs, ["audited 2"]);
    let queued = follow_ups.lock().unwrap();
    assert_eq!(queued[0].source, run.id);
    assert!(queued[0].text.starts_with(&format!("Workflow {} (audit) finished.", run.id)));
    assert!(std::fs::read_to_string(run.script_path.with_file_name("journal.jsonl")).unwrap().contains("Audit b.ts"), "the new run journals its replayed agents for a later resume");
    std::fs::remove_dir_all(runs_dir).unwrap();
}

#[test]
fn a_script_that_awaits_nothing_running_fails_instead_of_hanging() {
    let runs_dir = temp_dir("stuck");
    let (host, follow_ups) = host();
    let script = "export const meta = { name: 'stuck', description: 'Waits forever' }\nawait new Promise(() => {})";
    let run = launch(&Arc::default(), script.into(), Value::Null, None, &runs_dir, host).unwrap();
    assert!(wait_for_result(&run).unwrap_err().contains("never settles"));
    assert!(follow_ups.lock().unwrap()[0].text.contains("resumeFromRunId"));
    std::fs::remove_dir_all(runs_dir).unwrap();
}

#[test]
fn run_ids_cannot_leave_the_runs_directory() {
    assert!(run_dir(Path::new("/runs"), "../../etc").is_err());
    assert_eq!(run_dir(Path::new("/runs"), "audit-1f").unwrap(), Path::new("/runs/audit-1f"));
}

#[test]
fn saving_refuses_names_that_leave_the_directory() {
    let personal = temp_dir("save-names");
    assert_eq!(save("x", "audit", &personal, false).unwrap(), personal.join("audit.js"));
    assert_eq!(std::fs::read_to_string(personal.join("audit.js")).unwrap(), "x");
    assert!(save("x", "../escape", &personal, false).is_err());
    assert!(save("x", ".hidden", &personal, false).is_err());
    std::fs::remove_dir_all(personal).unwrap();
}

#[test]
#[cfg(unix)]
fn saving_refuses_symlinked_project_dirs() {
    let root = temp_dir("save");
    let personal = root.join("personal");
    std::fs::create_dir_all(&personal).unwrap();
    let linked = root.join("project").join(".agents");
    std::fs::create_dir_all(&linked).unwrap();
    std::os::unix::fs::symlink(&personal, linked.join("workflows")).unwrap();
    assert!(save("x", "audit", &linked.join("workflows"), true).unwrap_err().contains("symlink"));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn resuming_an_unknown_run_says_there_is_nothing_to_resume() {
    let runs_dir = temp_dir("unknown");
    let (host, _) = host();
    let script = "export const meta = { name: 'a', description: 'b' }\nreturn 1";
    let error = launch(&Arc::default(), script.into(), Value::Null, Some("missing"), &runs_dir, host).err().unwrap();
    assert!(error.contains("nothing to resume"));
    std::fs::remove_dir_all(runs_dir).unwrap();
}
