//! `mcpServers` from `~/.agents/mcp.json`, then `<cwd>/.mcp.json`, then `<cwd>/.agents/mcp.json`.
//! A later file overrides an earlier one per server: `{"enabled": false}` turns a global server off,
//! and a new `command` or `url` replaces the whole entry, so global headers never follow a project URL.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::{Map, Value};

use super::client::Launch;

#[derive(Clone, Debug, PartialEq)]
pub struct Server {
    pub name: String,
    pub launch: Launch,
    pub enabled: bool,
    /// Set when a project file chose what runs or where it connects; the server stays off until
    /// this key is in `settings.approved_mcp`. Any change to the entry makes a new key.
    pub approval: Option<String>,
    /// The merged entry as JSON, before `${VAR}` expansion: what the user approves.
    pub raw: String,
}

/// The servers in name order, and the first error of a file or an entry that was skipped.
pub fn load(home: &Path, cwd: &Path) -> (Vec<Server>, Option<String>) {
    // Run from ~, the "project" files are the user's own.
    let project = cwd != home;
    let files = [(home.join(".agents/mcp.json"), false, home), (cwd.join(".mcp.json"), project, cwd), (cwd.join(".agents/mcp.json"), project, cwd)];
    let mut merged: BTreeMap<String, (Map<String, Value>, bool)> = BTreeMap::new();
    let mut first_error = None;
    for (file, from_project, base) in files {
        let text = match std::fs::read_to_string(&file) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                first_error.get_or_insert(format!("{}: {e}", file.display()));
                continue;
            }
        };
        let config: Value = match serde_json::from_str(&text) {
            Ok(config) => config,
            Err(e) => {
                first_error.get_or_insert(format!("{}: {e}", file.display()));
                continue;
            }
        };
        for (name, entry) in config["mcpServers"].as_object().into_iter().flatten() {
            let Some(entry) = entry.as_object() else { continue };
            let (slot, project_set) = merged.entry(name.clone()).or_default();
            if entry.contains_key("command") || entry.contains_key("url") {
                slot.retain(|key, _| key == "enabled");
                *project_set = false;
            }
            for (key, value) in entry {
                // A relative `cwd` belongs to the file that sets it: home for the global file.
                let value = match (key.as_str(), value) {
                    ("cwd", Value::String(dir)) => Value::String(base.join(dir).to_string_lossy().into_owned()),
                    _ => value.clone(),
                };
                // Turning a server on from the repo still needs approval; turning it off does not.
                *project_set |= from_project && (key != "enabled" || value == true);
                slot.insert(key.clone(), value);
            }
        }
    }
    let mut servers = Vec::new();
    for (name, (mut entry, from_project)) in merged {
        let enabled = match entry.remove("enabled") {
            None => true,
            Some(Value::Bool(enabled)) => enabled,
            Some(_) => {
                first_error.get_or_insert(format!("MCP server {name}: enabled must be true or false."));
                continue;
            }
        };
        let raw = launch_first(&entry);
        let entry = Value::Object(entry);
        // A digest, so header and env secrets from the project file stay out of settings.json.
        let approval = from_project.then(|| crate::process::hash(&format!("{} {name} {raw}", cwd.display())));
        match server(&name, &entry, cwd, &|key| std::env::var(key).ok()) {
            Ok(launch) => servers.push(Server { name, launch, enabled, approval, raw }),
            Err(error) => _ = first_error.get_or_insert(error),
        }
    }
    (servers, first_error)
}

/// The `mcpServers` of an ACP session/new: stdio `{name, command, args, env: [{name, value}]}`, run in `cwd`,
/// or `{"type": "http", name, url, headers: [{name, value}]}`. The editor chose them, so they need no approval.
pub fn client_servers(list: &Value, cwd: &Path) -> Result<Vec<Server>, String> {
    let pairs = |list: &Value| -> Vec<(String, String)> {
        let pair = |item: &Value| Some((item["name"].as_str()?.to_string(), item["value"].as_str()?.to_string()));
        list.as_array().into_iter().flatten().filter_map(pair).collect()
    };
    let mut servers = Vec::new();
    for entry in list.as_array().into_iter().flatten() {
        let name = entry["name"].as_str().ok_or("Every MCP server needs a name.")?;
        let launch = match (entry["type"].as_str(), entry["command"].as_str(), entry["url"].as_str()) {
            (None | Some("stdio"), Some(command), _) => Launch::Stdio {
                command: command.to_string(),
                args: entry["args"].as_array().into_iter().flatten().filter_map(|arg| arg.as_str().map(String::from)).collect(),
                env: pairs(&entry["env"]),
                cwd: cwd.to_path_buf(),
            },
            (Some("http"), _, Some(url)) => Launch::Http { url: url.to_string(), headers: pairs(&entry["headers"]) },
            _ => return Err(format!("MCP server {name}: only stdio servers and http servers with a url are supported.")),
        };
        servers.push(Server { name: name.to_string(), launch, enabled: true, approval: None, raw: entry.to_string() });
    }
    Ok(servers)
}

/// The entry as JSON with what runs and where it connects first, so a cut-off approval panel still shows them.
fn launch_first(entry: &Map<String, Value>) -> String {
    const LAUNCH: [&str; 4] = ["command", "args", "url", "cwd"];
    let rest = entry.keys().map(String::as_str).filter(|key| !LAUNCH.contains(key));
    let fields: Vec<String> = LAUNCH.into_iter().chain(rest).filter_map(|key| Some(format!("{}:{}", Value::from(key), entry.get(key)?))).collect();
    format!("{{{}}}", fields.join(","))
}

fn server(name: &str, entry: &Value, cwd: &Path, env: &dyn Fn(&str) -> Option<String>) -> Result<Launch, String> {
    // `"X-Port": 8080` means "8080", the way other `.mcp.json` clients read it.
    let text = |value: &Value| expand(&value.as_str().map_or_else(|| value.to_string(), String::from), env);
    let pairs = |key: &str| -> Vec<(String, String)> { entry[key].as_object().into_iter().flatten().map(|(key, value)| (key.clone(), text(value))).collect() };
    if entry["type"] == "sse" {
        return Err(format!("MCP server {name} uses the old SSE transport; only streamable HTTP is supported."));
    }
    match (entry["command"].as_str(), entry["url"].as_str()) {
        (Some(command), None) => Ok(Launch::Stdio {
            command: expand(command, env),
            args: entry["args"].as_array().into_iter().flatten().map(text).collect(),
            env: pairs("env"),
            cwd: cwd.join(entry["cwd"].as_str().unwrap_or_default()),
        }),
        (None, Some(url)) => Ok(Launch::Http { url: expand(url, env), headers: pairs("headers") }),
        (Some(_), Some(_)) => Err(format!("MCP server {name} has both a command and a url.")),
        (None, None) => Err(format!("MCP server {name} needs a command or a url.")),
    }
}

/// Replaces `${NAME}` with the environment variable and `${NAME:-default}` with the default when
/// it is unset; an unset variable without a default becomes nothing.
fn expand(text: &str, env: &dyn Fn(&str) -> Option<String>) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        let Some(end) = rest[start..].find('}') else { break };
        out.push_str(&rest[..start]);
        let (name, default) = rest[start + 2..start + end].split_once(":-").unwrap_or((&rest[start + 2..start + end], ""));
        out.push_str(&env(name).unwrap_or_else(|| default.to_string()));
        rest = &rest[start + end + 1..];
    }
    out + rest
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn write(path: PathBuf, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn project_files_override_global_servers_by_name() {
        let root = std::env::temp_dir().join(format!("agent-mcp-config-{}", std::process::id()));
        let (home, cwd) = (root.join("home"), root.join("project"));
        write(home.join(".agents/mcp.json"), r#"{"mcpServers": {"fs": {"command": "fs-server", "args": ["--ro"]}, "web": {"url": "https://a.example/mcp"}}}"#);
        write(cwd.join(".mcp.json"), r#"{"mcpServers": {"web": {"url": "https://b.example/mcp"}, "local": {"command": "x", "cwd": "tools"}}}"#);
        write(cwd.join(".agents/mcp.json"), r#"{"mcpServers": {"fs": {"enabled": false}}}"#);
        let (servers, _) = load(&home, &cwd);
        std::fs::remove_dir_all(&root).unwrap();

        let names: Vec<_> = servers.iter().map(|server| server.name.as_str()).collect();
        assert_eq!(names, ["fs", "local", "web"]);
        assert!(!servers[0].enabled);
        assert_eq!(servers[0].launch, Launch::Stdio { command: "fs-server".into(), args: vec!["--ro".into()], env: vec![], cwd: cwd.clone() });
        assert_eq!(servers[1].launch, Launch::Stdio { command: "x".into(), args: vec![], env: vec![], cwd: cwd.join("tools") });
        assert_eq!(servers[2].launch, Launch::Http { url: "https://b.example/mcp".into(), headers: vec![] });
    }

    #[test]
    fn values_expand_environment_variables() {
        let env = |key: &str| (key == "TOKEN").then(|| "s3cret".to_string());
        let entry = serde_json::json!({"url": "https://x/mcp", "headers": {"Authorization": "Bearer ${TOKEN}", "X-Missing": "${NOPE}!", "X-Port": 8080}});
        let parsed = server("x", &entry, Path::new("/"), &env).unwrap();
        let headers = vec![("Authorization".into(), "Bearer s3cret".into()), ("X-Missing".into(), "!".into()), ("X-Port".into(), "8080".into())];
        assert_eq!(parsed, Launch::Http { url: "https://x/mcp".into(), headers });
        let entry = serde_json::json!({"command": "${NOPE:-npx}", "args": ["--token=${TOKEN}", 3]});
        let parsed = server("x", &entry, Path::new("/"), &env).unwrap();
        assert_eq!(parsed, Launch::Stdio { command: "npx".into(), args: vec!["--token=s3cret".into(), "3".into()], env: vec![], cwd: "/".into() });
    }

    #[test]
    fn an_entry_with_no_launch_two_launches_or_the_old_sse_transport_is_rejected() {
        let env = |_: &str| None;
        assert!(server("bad", &serde_json::json!({}), Path::new("/"), &env).is_err());
        assert!(server("bad", &serde_json::json!({"command": "x", "url": "https://x/mcp"}), Path::new("/"), &env).is_err());
        assert!(server("bad", &serde_json::json!({"type": "sse", "url": "https://x/sse"}), Path::new("/"), &env).is_err());
    }

    #[test]
    fn an_acp_client_passes_stdio_and_http_servers_but_no_sse() {
        let list = serde_json::json!([
            {"name": "fs", "command": "fs-server", "args": ["--ro"], "env": [{"name": "TOKEN", "value": "${NOT_EXPANDED}"}]},
            {"type": "http", "name": "web", "url": "https://x/mcp", "headers": [{"name": "Authorization", "value": "Bearer t"}]},
        ]);
        let servers = client_servers(&list, Path::new("/repo")).unwrap();
        let env = vec![("TOKEN".into(), "${NOT_EXPANDED}".into())];
        assert_eq!(servers[0].launch, Launch::Stdio { command: "fs-server".into(), args: vec!["--ro".into()], env, cwd: "/repo".into() });
        assert_eq!(servers[1].launch, Launch::Http { url: "https://x/mcp".into(), headers: vec![("Authorization".into(), "Bearer t".into())] });
        assert!(servers.iter().all(|server| server.enabled && server.approval.is_none()));
        assert!(client_servers(&serde_json::json!([{"type": "sse", "name": "old", "url": "https://x/sse", "headers": []}]), Path::new("/")).is_err());
        assert!(client_servers(&serde_json::json!([{"command": "x"}]), Path::new("/")).is_err());
    }

    #[test]
    fn a_global_server_cwd_is_relative_to_home_and_enabled_must_be_a_bool() {
        let root = std::env::temp_dir().join(format!("agent-mcp-cwd-{}", std::process::id()));
        let (home, cwd) = (root.join("home"), root.join("project"));
        write(home.join(".agents/mcp.json"), r#"{"mcpServers": {"tool": {"command": "t", "cwd": "tools"}, "odd": {"command": "o", "enabled": "false"}}}"#);
        let (servers, error) = load(&home, &cwd);
        std::fs::remove_dir_all(&root).unwrap();

        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].launch, Launch::Stdio { command: "t".into(), args: vec![], env: vec![], cwd: home.join("tools") });
        assert!(error.unwrap().contains("odd"));
    }

    #[test]
    fn a_server_whose_launch_a_project_file_sets_needs_approval() {
        let root = std::env::temp_dir().join(format!("agent-mcp-approval-{}", std::process::id()));
        let (home, cwd) = (root.join("home"), root.join("project"));
        write(
            home.join(".agents/mcp.json"),
            r#"{"mcpServers": {"global": {"command": "g"}, "web": {"url": "https://a.example/mcp", "headers": {"Authorization": "Bearer ${T}"}}, "off": {"command": "o"}, "zoff": {"command": "z", "enabled": false}}}"#,
        );
        write(
            cwd.join(".mcp.json"),
            r#"{"mcpServers": {"web": {"url": "https://evil.example/mcp"}, "off": {"enabled": false}, "repo": {"_": "x", "command": "./run", "args": ["a"]}, "zoff": {"enabled": true}}}"#,
        );
        let (servers, error) = load(&home, &cwd);
        std::fs::remove_dir_all(&root).unwrap();

        assert_eq!(error, None);
        let approvals: Vec<_> = servers[..4].iter().map(|server| server.approval.clone()).collect();
        let key = |name: &str, entry: &str| Some(crate::process::hash(&format!("{} {name} {entry}", cwd.display())));
        assert_eq!(approvals, [None, None, key("repo", r#"{"command":"./run","args":["a"],"_":"x"}"#), key("web", r#"{"url":"https://evil.example/mcp"}"#)]);
        assert!(servers[4].approval.is_some(), "turning on a server from the repo needs approval");
        assert_eq!(servers[3].launch, Launch::Http { url: "https://evil.example/mcp".into(), headers: vec![] });
    }

    #[test]
    fn a_broken_entry_or_file_keeps_the_other_servers() {
        let root = std::env::temp_dir().join(format!("agent-mcp-broken-{}", std::process::id()));
        let (home, cwd) = (root.join("home"), root.join("project"));
        write(home.join(".agents/mcp.json"), r#"{"mcpServers": {"fs": {"command": "fs-server"}, "half": {"enabled": false}}}"#);
        std::fs::create_dir_all(cwd.join(".mcp.json")).unwrap();
        let (servers, error) = load(&home, &cwd);
        std::fs::remove_dir_all(&root).unwrap();

        assert_eq!(servers.iter().map(|server| server.name.as_str()).collect::<Vec<_>>(), ["fs"]);
        assert!(error.unwrap().contains(".mcp.json"));
    }
}
