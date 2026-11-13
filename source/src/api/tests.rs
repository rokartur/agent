use super::thread::THREAD_IDLE;
use super::*;

#[test]
fn picking_an_account_reports_it_before_the_next_request() {
    let home = std::env::temp_dir().join(format!("agent-pick-{}", std::process::id()));
    let client = Client::new(&home).unwrap();
    let account = |name: &str| Account { provider: "anthropic".into(), name: name.into(), secret: Secret::ApiKey("sk".into()) };
    client.reload(vec![account("home"), account("work")]);
    client.state.lock().unwrap().current.insert("anthropic".into(), "home".into());

    let mut settings = crate::settings::load(&home).unwrap();
    settings.default_accounts.insert("anthropic".into(), "work".into());
    client.configure(&settings);

    assert_eq!(client.account("anthropic"), Some(("work".into(), None)));
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn a_remote_headroom_gets_the_requests_with_the_proxy_token() {
    let home = std::env::temp_dir().join(format!("agent-remote-headroom-{}", std::process::id()));
    let mut client = Client::new(&home).unwrap();
    client.anthropic_base = "http://127.0.0.1:9".into();
    let (headroom, requests) = mock_provider(&[(200, PONG)]);
    client.reload(vec![Account { provider: "anthropic".into(), name: "work".into(), secret: Secret::ApiKey("sk".into()) }]);
    *client.catalog.write().unwrap() = vec![Model { provider: "anthropic".into(), id: "claude".into(), ..Model::default() }].into();
    let mut settings = crate::settings::load(&home).unwrap();
    (settings.headroom, settings.headroom_url, settings.headroom_token) = (true, headroom, "t0ken".into());
    client.configure(&settings);

    let sent = client.send(&json!({"model": "claude", "max_tokens": 10}), &AtomicBool::new(false), &mut |_| {});
    std::fs::remove_dir_all(&home).ok();

    assert!(sent.is_ok(), "{:?}", sent.err());
    assert_eq!(requests.recv().unwrap()["x-headroom-proxy-token"], "t0ken");
}

#[test]
fn a_failed_token_refresh_moves_the_request_to_the_next_account() {
    let home = std::env::temp_dir().join(format!("agent-refresh-{}", std::process::id()));
    let client = Client::new(&home).unwrap();
    let expired = oauth::Tokens { access: "a".into(), refresh: "r".into(), expires: 0, account_id: None };
    let account = |name: &str| Account { provider: "anthropic".into(), name: name.into(), secret: Secret::OAuth(expired.clone()) };
    client.reload(vec![account("home"), account("work")]);
    *client.catalog.write().unwrap() = vec![Model { provider: "anthropic".into(), id: "claude".into(), ..Model::default() }].into();

    let mut notices = Vec::new();
    let error = client.send(&json!({"model": "claude"}), &AtomicBool::new(false), &mut |delta| {
        if let Delta::Notice(text) = delta {
            notices.push(text.to_string());
        }
    });
    std::fs::remove_dir_all(&home).ok();

    // Neither account is in the accounts file, so each refresh fails without a network call.
    assert!(error.is_err());
    assert!(notices.iter().any(|n| n.starts_with("home ")) && notices.iter().any(|n| n.starts_with("work ")), "{notices:?}");
}

#[test]
fn account_names_maps_saved_account_ids_to_names() {
    let home = std::env::temp_dir().join(format!("agent-names-{}", std::process::id()));
    let client = Client::new(&home).unwrap();
    let saved = |name: &str, id: &str| Account {
        provider: "anthropic".into(),
        name: name.into(),
        secret: Secret::OAuth(oauth::Tokens { access: "a".into(), refresh: "r".into(), expires: 5, account_id: Some(id.into()) }),
    };
    client.reload(vec![saved("home", "uuid-home"), saved("work", "uuid-work")]);

    let ids = |id: &str| HashMap::from([("anthropic".to_string(), id.to_string())]);
    assert_eq!(client.account_names(&ids("uuid-work")), HashMap::from([("anthropic".to_string(), "work".to_string())]));
    assert_eq!(client.account_names(&ids("uuid-gone")), HashMap::new());
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn moving_an_account_reorders_only_its_provider_and_is_saved() {
    let home = std::env::temp_dir().join(format!("agent-move-{}", std::process::id()));
    let client = Client::new(&home).unwrap();
    for (provider, name) in [("anthropic", "zeta"), ("openrouter", "or"), ("anthropic", "alpha"), ("anthropic", "mid")] {
        client.add(Account { provider: provider.into(), name: name.into(), secret: Secret::ApiKey("k".into()) }).unwrap();
    }
    let names = || client.accounts().into_iter().filter(|a| a.provider == "anthropic").map(|a| a.name).collect::<Vec<_>>();
    assert_eq!(names(), ["zeta", "alpha", "mid"]);

    client.move_account("anthropic", "mid", true).unwrap();
    client.move_account("anthropic", "zeta", true).unwrap();
    assert_eq!(names(), ["zeta", "mid", "alpha"]);
    client.move_account("anthropic", "zeta", false).unwrap();
    let saved: Vec<_> = accounts::load(&home).unwrap().into_iter().map(|a| a.name).collect();
    std::fs::remove_dir_all(&home).ok();
    assert_eq!(saved, ["mid", "zeta", "alpha", "or"]);
}

#[test]
fn stream_builds_text_thinking_and_tool_blocks() {
    let events = [
        r#"{"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":10,"cache_read_input_tokens":5,"output_tokens":1}}}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hm"}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig"}}"#,
        r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
        r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Hi"}}"#,
        r#"{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"t1","name":"bash","input":{}}}"#,
        r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"command\":"}}"#,
        r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"\"ls\"}"}}"#,
        r#"{"type":"content_block_stop","index":2}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":7}}"#,
        r#"{"type":"message_stop"}"#,
    ];
    let body: String = events.iter().map(|e| format!("event: x\ndata: {e}\n\n")).collect();
    let mut streamed = String::new();
    let on_delta = &mut |delta: Delta| match delta {
        Delta::Text(t) => streamed.push_str(t),
        Delta::Thinking(t) => streamed.push_str(&format!("[{t}]")),
        Delta::Notice(_) => {}
    };
    let reply = parse_stream(&mut body.as_bytes(), &AtomicBool::new(false), on_delta).ok().unwrap();
    assert_eq!(streamed, "[hm]Hi");
    assert_eq!(reply.stop_reason, "tool_use");
    assert_eq!((reply.usage.input, reply.usage.output, reply.usage.cache_read), (10, 7, 5));
    assert_eq!(reply.id, "msg_1");
    let content = &reply.message["content"];
    assert_eq!(content[0], json!({"type": "thinking", "thinking": "hm", "signature": "sig"}));
    assert_eq!(content[2]["input"], json!({"command": "ls"}));
}

#[test]
fn server_tool_iterations_bill_their_sum_but_the_context_is_one_request() {
    let events = [
        r#"{"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":4,"cache_creation_input_tokens":2769,"cache_read_input_tokens":0,"output_tokens":2}}}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"srvtoolu_1","name":"tool_search_tool_regex","input":{}}}"#,
        r#"{"type":"content_block_stop","index":0}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"input_tokens":641,"cache_creation_input_tokens":2769,"cache_read_input_tokens":2769,"output_tokens":211}}"#,
        r#"{"type":"message_stop"}"#,
    ];
    let body: String = events.iter().map(|e| format!("event: x\ndata: {e}\n\n")).collect();
    let reply = parse_stream(&mut body.as_bytes(), &AtomicBool::new(false), &mut |_| {}).ok().unwrap();
    assert_eq!((reply.usage.input, reply.usage.cache_read, reply.usage.cache_write), (641, 2769, 2769));
    assert_eq!(reply.context_tokens, 4 + 2769 + 211);
}

#[test]
fn the_1h_cache_write_split_survives_to_the_reply() {
    let start = |usage: &str| format!(r#"{{"type":"message_start","message":{{"id":"msg_1","usage":{usage}}}}}"#);
    let parse = |usage: &str| {
        let body = format!("data: {}\n\ndata: {{\"type\":\"message_stop\"}}\n\n", start(usage));
        parse_stream(&mut body.as_bytes(), &AtomicBool::new(false), &mut |_| {}).ok().unwrap().cache_write_1h
    };
    let split = r#"{"input_tokens":4,"cache_creation_input_tokens":300,"cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":300}}"#;
    assert_eq!(parse(split), Some(300));
    assert_eq!(parse(r#"{"input_tokens":4,"cache_creation_input_tokens":300}"#), None);
}

#[test]
fn tool_input_keeps_a_raw_tab_inside_a_string() {
    let events = [
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t1","name":"write","input":{}}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"text\": \"\\\"\tx\\n\"}"}}"#,
        r#"{"type":"content_block_stop","index":0}"#,
        r#"{"type":"message_stop"}"#,
    ];
    let body: String = events.iter().map(|e| format!("event: x\ndata: {e}\n\n")).collect();
    let reply = parse_stream(&mut body.as_bytes(), &AtomicBool::new(false), &mut |_| {}).ok().unwrap();
    assert_eq!(reply.message["content"][0]["input"], json!({"text": "\"\tx\n"}));
}

#[test]
fn invalid_tool_input_is_reported_not_fatal() {
    let events = [
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t1","name":"write","input":{}}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"text\": \"a\"b\"}"}}"#,
        r#"{"type":"content_block_stop","index":0}"#,
        r#"{"type":"message_stop"}"#,
    ];
    let body: String = events.iter().map(|e| format!("event: x\ndata: {e}\n\n")).collect();
    let reply = parse_stream(&mut body.as_bytes(), &AtomicBool::new(false), &mut |_| {}).ok().unwrap();
    assert_eq!(reply.message["content"][0]["input"], json!({}));
    let [bad] = reply.bad_tool_inputs.as_slice() else { panic!("expected one bad input") };
    assert_eq!((bad.id.as_str(), bad.raw.as_str()), ("t1", r#"{"text": "a"b"}"#));
    assert!(bad.error.starts_with("Bad tool input JSON: expected `,` or `}`"), "{}", bad.error);
}

#[test]
fn rare_tools_load_through_tool_search_and_the_breakpoint_moves_to_a_loaded_tool() {
    let tools = json!([{"name": "Read"}, {"name": "Edit"}, {"name": "Agent"}, {"name": "WebSearch"}]);
    let mut body = request("m", 1000, "high", "5m", "sys", "", &tools, &[]);
    defer_rare_tools(&mut body);
    let names: Vec<&str> = body["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["tool_search_tool_bm25", "Read", "Edit", "Agent", "WebSearch"]);
    assert_eq!(body["tools"][2]["cache_control"]["type"], "ephemeral");
    for deferred in &body["tools"].as_array().unwrap()[3..] {
        assert_eq!((deferred["defer_loading"].as_bool(), deferred.get("cache_control")), (Some(true), None));
    }
    let names_deferred = body["system"].as_array().unwrap().last().unwrap()["text"].as_str().unwrap();
    assert!(names_deferred.ends_with(": Agent, WebSearch."), "{names_deferred}");
    let core_only = json!([{"name": "Read"}, {"name": "Grep"}]);
    let mut body = request("m", 1000, "high", "5m", "sys", "", &core_only, &[]);
    let before = body.clone();
    defer_rare_tools(&mut body);
    assert_eq!(body, before);
    let mut summary = plain_request("m", "sys", "prompt", 100);
    defer_rare_tools(&mut summary);
    assert!(summary.get("tools").is_none());
}

#[test]
fn request_marks_cache_breakpoints() {
    let tools = json!([{"name": "a"}, {"name": "b"}]);
    let messages = [json!({"role": "user", "content": [{"type": "text", "text": "1"}, {"type": "text", "text": "2"}]})];
    let body = request("m", 1000, "high", "5m", "sys", "<cwd>/a</cwd>", &tools, &messages);
    assert!(body["tools"][0].get("cache_control").is_none());
    assert_eq!(body["tools"][1]["cache_control"]["type"], "ephemeral");
    assert!(body["messages"][0]["content"][0].get("cache_control").is_none());
    assert_eq!(body["messages"][0]["content"][1]["cache_control"]["type"], "ephemeral");
    assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
    assert_eq!(body["system"][1], json!({"type": "text", "text": "<cwd>/a</cwd>"}));
    assert_eq!(request("m", 1000, "high", "5m", "sys", "", &tools, &messages)["system"].as_array().unwrap().len(), 1);
    let hour = request("m", 1000, "high", "1h", "sys", "", &tools, &messages);
    for breakpoint in [&hour["tools"][1], &hour["messages"][0]["content"][1], &hour["system"][0]] {
        assert_eq!(breakpoint["cache_control"], json!({"type": "ephemeral", "ttl": "1h"}));
    }
}

#[test]
#[cfg(unix)]
fn a_stalled_stream_times_out_and_esc_ends_it_as_aborted() {
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    let (mut server, client) = UnixStream::pair().unwrap();
    server.write_all(b"data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{}}}\n").unwrap();
    let not_aborted = AtomicBool::new(false);
    let error = parse_stream(&mut watched_body(client.try_clone().unwrap(), &not_aborted, Duration::from_millis(200)), &not_aborted, &mut |_| {}).err().unwrap();
    assert!(error.retryable && error.message.contains("no data"), "{}", error.message);

    let abort = Arc::new(AtomicBool::new(false));
    let setter = Arc::clone(&abort);
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        setter.store(true, Ordering::Relaxed);
    });
    let started = Instant::now();
    let reply = parse_stream(&mut watched_body(client, &abort, Duration::from_secs(60)), &abort, &mut |_| {}).ok().unwrap();
    assert_eq!(reply.stop_reason, "aborted");
    assert!(started.elapsed() < Duration::from_secs(5));
    drop(server);
}

/// Serves a models list that both Anthropic and OpenRouter parse; messages get `replies` in order, the last one from then on,
/// and their request bodies go to the returned receiver with the proxy token and beta headers in them.
fn mock_provider(replies: &[(u16, &str)]) -> (String, std::sync::mpsc::Receiver<Value>) {
    use std::io::{BufRead, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (sent, received) = std::sync::mpsc::channel();
    let mut replies: Vec<(u16, String)> = replies.iter().map(|(status, body)| (*status, body.to_string())).collect();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request_line = String::new();
            reader.read_line(&mut request_line).unwrap();
            // Read the whole request, so closing the socket does not reset it under the client.
            let mut length = 0;
            let mut proxy_token = None;
            let mut beta = None;
            let mut header = String::new();
            while reader.read_line(&mut header).unwrap() > 2 {
                if let Some(value) = header.to_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap();
                }
                if let Some(value) = header.to_lowercase().strip_prefix("x-headroom-proxy-token:") {
                    proxy_token = Some(value.trim().to_string());
                }
                if let Some(value) = header.to_lowercase().strip_prefix("anthropic-beta:") {
                    beta = Some(value.trim().to_string());
                }
                header.clear();
            }
            let mut request = vec![0; length];
            std::io::Read::read_exact(&mut reader, &mut request).unwrap();
            let (status, body) = if request_line.contains("/v1/models") {
                (
                        200,
                        r#"{"data":[{"id":"claude-haiku-9","max_input_tokens":200000,"max_tokens":8192},{"id":"a/one:free","context_length":1000,"supported_parameters":["tools"]},{"id":"b/two:free","context_length":1000,"supported_parameters":["tools"]}]}"#.to_string(),
                    )
            } else if request_line.starts_with("GET") {
                // Codex and Claude usage windows; unknown usage does not block a request.
                (404, "{}".to_string())
            } else {
                let mut request: Value = serde_json::from_slice(&request).unwrap();
                if let Some(token) = proxy_token {
                    request["x-headroom-proxy-token"] = json!(token);
                }
                if let Some(beta) = beta {
                    request["anthropic-beta"] = json!(beta);
                }
                // A test that only reads the doctor lines has dropped the receiver.
                let _ = sent.send(request);
                if replies.len() > 1 { replies.remove(0) } else { replies[0].clone() }
            };
            write!(stream, "HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).unwrap();
        }
    });
    (base, received)
}

const PONG: &str = concat!("data: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"usage\":{}}}\n\n", "data: {\"type\":\"message_stop\"}\n\n");

#[test]
fn doctor_reports_ok_for_a_working_account_and_the_provider_error_for_a_rejected_one() {
    let home = std::env::temp_dir().join(format!("agent-doctor-{}", std::process::id()));
    let check = |messages_status, messages_body| {
        let mut client = Client::new(&home).unwrap();
        client.anthropic_base = mock_provider(&[(messages_status, messages_body)]).0;
        client.reload(vec![Account { provider: "anthropic".into(), name: "work".into(), secret: Secret::ApiKey("sk".into()) }]);
        client.doctor(Some("anthropic"))
    };

    let ok = check(200, PONG);
    let rejected = check(401, r#"{"error":{"message":"invalid x-api-key"}}"#);
    std::fs::remove_dir_all(&home).ok();

    assert!(matches!(&ok[..], [Ok(line)] if line.contains("anthropic/work")), "{ok:?}");
    assert!(matches!(&rejected[..], [Err(line)] if line.contains("anthropic/work") && line.contains("401") && line.contains("invalid x-api-key")), "{rejected:?}");
}

#[test]
fn account_status_puts_a_running_check_over_a_rate_limit_over_the_last_result() {
    let account = Account { provider: "anthropic".into(), name: "work".into(), secret: Secret::ApiKey("sk".into()) };
    let now = Instant::now();
    let mut state = State::default();
    assert_eq!(status(&state, &account, now), AccountStatus::Unchecked);
    state.usage_errors.insert(key(&account), "Usage request failed (401).".into());
    assert_eq!(status(&state, &account, now), AccountStatus::Failed("Usage request failed (401).".into()));
    state.checks.insert(key(&account), AccountStatus::Working);
    assert_eq!(status(&state, &account, now), AccountStatus::Working);
    state.limited.insert(key(&account), now + LIMITED_FOR);
    assert_eq!(status(&state, &account, now), AccountStatus::Limited);
    state.checks.insert(key(&account), AccountStatus::Checking);
    assert_eq!(status(&state, &account, now), AccountStatus::Checking);
}

#[test]
fn doctor_pings_openrouter_on_free_models_without_a_thinking_field() {
    let home = std::env::temp_dir().join(format!("agent-doctor-or-{}", std::process::id()));
    let mut client = Client::new(&home).unwrap();
    let (base, requests) = mock_provider(&[(200, PONG)]);
    client.openrouter_base = base;
    client.reload(vec![Account { provider: "openrouter".into(), name: "key".into(), secret: Secret::ApiKey("sk".into()) }]);

    let lines = client.doctor(Some("openrouter"));
    std::fs::remove_dir_all(&home).ok();

    assert!(matches!(&lines[..], [Ok(_)]), "{lines:?}");
    let ping = requests.recv().unwrap();
    assert!(ping.get("thinking").is_none(), "{ping}");
    assert_eq!(ping["model"], "a/one:free");
    // A free model hits its rate limit often, so OpenRouter moves the ping to the next one.
    assert_eq!(ping["fallbacks"], json!([{"model": "b/two:free"}]));
}

#[test]
fn each_provider_gets_its_endpoint_and_parser() {
    let home = std::env::temp_dir().join(format!("agent-outgoing-{}", std::process::id()));
    let client = Client::new(&home).unwrap();
    let custom = |name: &str, api| CustomProvider { name: name.into(), base_url: "http://x".into(), api_key_env: String::new(), api, models: Vec::new() };
    client.state.lock().unwrap().custom_providers = vec![custom("own-anthropic", CustomApi::Anthropic), custom("own-openai", CustomApi::OpenAi)];
    let request = json!({"model": "m", "max_tokens": 10, "messages": [{"role": "user", "content": "hi"}]});
    let tokens = oauth::Tokens { access: "a".into(), refresh: "r".into(), expires: u64::MAX, account_id: Some("p".into()) };
    let google = Parser::Google { owner: google::signature_owner(google::ANTIGRAVITY, google::wire_model(&request)) };
    let rows = [
        ("own-anthropic", Secret::ApiKey("k".into()), Parser::Messages, "max_tokens"),
        ("own-openai", Secret::ApiKey("k".into()), Parser::OpenAiChat, "stream_options"),
        ("anthropic", Secret::OAuth(tokens.clone()), Parser::Messages, "system"),
        ("openrouter", Secret::ApiKey("k".into()), Parser::Messages, "max_tokens"),
        (copilot::PROVIDER, Secret::OAuth(tokens.clone()), Parser::OpenAiChat, "stream_options"),
        (google::ANTIGRAVITY, Secret::OAuth(tokens.clone()), google, "project"),
        ("openai-codex", Secret::OAuth(tokens), Parser::Codex, "input"),
    ];
    for (provider, secret, parser, marker) in rows {
        let outgoing = client.outgoing(&Account { provider: provider.into(), name: "n".into(), secret }, &request).map_err(|e| e.message).unwrap();
        assert_eq!((provider, &outgoing.parser, outgoing.body.get(marker).is_some()), (provider, &parser, true), "{}", outgoing.body);
    }
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_custom_provider_model_replaces_the_openrouter_model_with_the_same_id() {
    let home = std::env::temp_dir().join(format!("agent-custom-clash-{}", std::process::id()));
    let mut client = Client::new(&home).unwrap();
    let (base, _) = mock_provider(&[(200, PONG)]);
    client.openrouter_base = base;
    let model = crate::settings::CustomModel { id: "one:free".into(), context_window: 1, max_tokens: 1 };
    client.state.lock().unwrap().custom_providers = vec![CustomProvider { name: "a".into(), base_url: "http://x".into(), api_key_env: String::new(), api: CustomApi::OpenAi, models: vec![model] }];

    let errors = client.load_catalog();
    std::fs::remove_dir_all(&home).ok();

    assert!(errors.is_empty(), "{errors:?}");
    let catalog = client.catalog();
    let same_id: Vec<&str> = catalog.iter().filter(|m| m.id == "a/one:free").map(|m| m.provider.as_str()).collect();
    assert_eq!(same_id, ["a"]);
}

#[test]
fn loading_the_catalog_again_lists_the_models_of_an_account_added_since() {
    let home = std::env::temp_dir().join(format!("agent-catalog-reload-{}", std::process::id()));
    let mut client = Client::new(&home).unwrap();
    let (base, _) = mock_provider(&[(200, PONG)]);
    (client.anthropic_base, client.openrouter_base) = (base.clone(), base);
    client.reload(Vec::new());
    client.load_catalog();
    let has_claude = |client: &Client| client.catalog().iter().any(|m| m.provider == "anthropic" && m.id == "claude-haiku-9");
    assert!(!has_claude(&client));

    client.reload(vec![Account { provider: "anthropic".into(), name: "work".into(), secret: Secret::ApiKey("sk".into()) }]);
    let errors = client.load_catalog();
    std::fs::remove_dir_all(&home).ok();

    assert!(errors.is_empty(), "{errors:?}");
    assert!(has_claude(&client));
}

/// An Anthropic account answering `status` on every attempt, with an OpenRouter account and `a/one:free` as `fallback_model`.
fn send_with_failover(status: u16, body: &'static str) -> (Result<Reply, String>, Vec<String>, std::sync::mpsc::Receiver<Value>) {
    let home = std::env::temp_dir().join(format!("agent-failover-{status}-{}", std::process::id()));
    let mut client = Client::new(&home).unwrap();
    client.anthropic_base = mock_provider(&[(status, body)]).0;
    let (openrouter_base, fallback_requests) = mock_provider(&[(200, PONG)]);
    client.openrouter_base = openrouter_base;
    client.reload(vec![
        Account { provider: "anthropic".into(), name: "work".into(), secret: Secret::ApiKey("sk".into()) },
        Account { provider: "openrouter".into(), name: "key".into(), secret: Secret::ApiKey("sk".into()) },
    ]);
    let models = vec![Model { provider: "anthropic".into(), id: "claude".into(), ..Model::default() }, Model { provider: "openrouter".into(), id: "a/one:free".into(), ..Model::default() }];
    *client.catalog.write().unwrap() = models.into();
    client.state.lock().unwrap().fallback_model = "a/one:free".into();

    let mut notices = Vec::new();
    let sent = client.send(&json!({"model": "claude", "max_tokens": 10}), &AtomicBool::new(false), &mut |delta| {
        if let Delta::Notice(text) = delta {
            notices.push(text.to_string());
        }
    });
    std::fs::remove_dir_all(&home).ok();
    (sent, notices, fallback_requests)
}

#[test]
fn an_overloaded_provider_fails_over_to_the_fallback_model_after_its_retries() {
    let (sent, notices, fallback_requests) = send_with_failover(529, r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#);

    assert_eq!(sent.unwrap().model, "a/one:free");
    assert_eq!(fallback_requests.recv().unwrap()["model"], "a/one:free");
    assert!(notices.last().is_some_and(|n| n.contains("Switched to a/one:free for this request")), "{notices:?}");
}

#[test]
fn an_account_a_plugin_blocks_until_a_reset_moves_the_session_to_the_next() {
    let home = std::env::temp_dir().join(format!("agent-plugin-block-{}", std::process::id()));
    let client = Client::new(&home).unwrap();
    std::fs::remove_dir_all(&home).ok();
    let account = |name: &str| Account { provider: "anthropic".into(), name: name.into(), secret: Secret::ApiKey("sk".into()) };
    client.reload(vec![account("first"), account("second")]);
    *client.catalog.write().unwrap() = vec![Model { provider: "anthropic".into(), id: "claude".into(), ..Model::default() }].into();
    let in_a_week = crate::process::now_ms() + 7 * 24 * 3_600_000;

    client.state.lock().unwrap().threshold = 0.0;
    assert!(!client.switch_account_until("claude", in_a_week), "switching off keeps the account");
    client.state.lock().unwrap().threshold = 90.0;

    assert!(client.switch_account_until("claude", in_a_week));
    assert_eq!(client.account("anthropic").unwrap().0, "second");
    // A pick mid-turn must not return to the blocked account once LIMITED_FOR runs out.
    assert!(client.state.lock().unwrap().limited["anthropic/first"] > Instant::now() + LIMITED_FOR);
}

#[test]
fn a_rejected_request_does_not_fail_over() {
    let (sent, notices, fallback_requests) = send_with_failover(401, r#"{"error":{"message":"invalid x-api-key"}}"#);

    assert!(sent.is_err_and(|error| error.contains("401")));
    assert!(fallback_requests.try_recv().is_err());
    assert!(notices.iter().all(|n| !n.contains("Switched")), "{notices:?}");
}

#[test]
fn a_custom_provider_model_goes_to_its_chat_completions_endpoint_without_the_name_prefix() {
    let home = std::env::temp_dir().join(format!("agent-custom-{}", std::process::id()));
    let client = Client::new(&home).unwrap();
    let (base, requests) = mock_provider(&[(200, "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"pong\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n")]);
    let local = CustomProvider { name: "ollama".into(), base_url: format!("{base}/v1"), api_key_env: String::new(), api: CustomApi::OpenAi, models: Vec::new() };
    *client.catalog.write().unwrap() = vec![Model { provider: "ollama".into(), id: "ollama/qwen3:8b".into(), ..Model::default() }].into();
    client.state.lock().unwrap().custom_providers = vec![local];

    let reply = client.send(&plain_request("ollama/qwen3:8b", "Be brief.", "ping", 10), &AtomicBool::new(false), &mut |_| {});
    std::fs::remove_dir_all(&home).ok();

    let reply = reply.unwrap();
    assert_eq!(reply.message["content"], json!([{"type": "text", "text": "pong"}]));
    assert_eq!((reply.provider.as_str(), reply.model.as_str()), ("ollama", "ollama/qwen3:8b"));
    assert_eq!(requests.recv().unwrap()["model"], "qwen3:8b");
}

#[test]
fn an_anthropic_compatible_provider_gets_a_messages_request_without_anthropic_only_fields() {
    let home = std::env::temp_dir().join(format!("agent-custom-anthropic-{}", std::process::id()));
    let client = Client::new(&home).unwrap();
    let (base, requests) = mock_provider(&[(200, PONG)]);
    let minimax = CustomProvider { name: "minimax".into(), base_url: base, api_key_env: String::new(), api: CustomApi::Anthropic, models: Vec::new() };
    *client.catalog.write().unwrap() = vec![Model { provider: "minimax".into(), id: "minimax/MiniMax-M2".into(), ..Model::default() }].into();
    client.state.lock().unwrap().custom_providers = vec![minimax];
    let mut request = plain_request("minimax/MiniMax-M2", "Be brief.", "ping", 10);
    request["thinking"] = json!({"type": "adaptive"});
    request["speed"] = json!("fast");

    let reply = client.send(&request, &AtomicBool::new(false), &mut |_| {});
    std::fs::remove_dir_all(&home).ok();

    assert_eq!(reply.unwrap().provider, "minimax");
    let sent = requests.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(sent["model"], "MiniMax-M2");
    assert!(sent.get("thinking").is_none() && sent.get("speed").is_none(), "{sent}");
}

#[test]
fn a_saved_gemini_cli_account_lists_no_models_and_never_reaches_codex() {
    let home = std::env::temp_dir().join(format!("agent-gemini-cli-{}", std::process::id()));
    let mut client = Client::new(&home).unwrap();
    let (base, requests) = mock_provider(&[(200, r#"{"models":[]}"#)]);
    client.chatgpt_base = base;
    let tokens = oauth::Tokens { access: "a".into(), refresh: "r".into(), expires: u64::MAX, account_id: None };
    let listed = client.list_models(&Account { provider: "google-gemini-cli".into(), name: "gemini".into(), secret: Secret::OAuth(tokens) });
    std::fs::remove_dir_all(&home).ok();

    assert_eq!(listed.err().unwrap(), "google-gemini-cli accounts are no longer supported. Remove this account with /logout.");
    assert!(requests.recv_timeout(std::time::Duration::from_millis(200)).is_err());
}

#[test]
fn a_rejected_codex_tier_retries_once_at_standard_speed() {
    let home = std::env::temp_dir().join(format!("agent-tier-{}", std::process::id()));
    let mut client = Client::new(&home).unwrap();
    let (base, requests) = mock_provider(&[(400, r#"{"error":{"message":"Unsupported service_tier: ultrafast"}}"#)]);
    client.chatgpt_base = base;
    let tokens = oauth::Tokens { access: "a".into(), refresh: "r".into(), expires: u64::MAX, account_id: None };
    client.reload(vec![Account { provider: "openai-codex".into(), name: "pro".into(), secret: Secret::OAuth(tokens) }]);
    *client.catalog.write().unwrap() = vec![Model { provider: "openai-codex".into(), id: "gpt".into(), ..Model::default() }].into();
    let mut request = plain_request("gpt", "Be brief.", "ping", 10);
    request["speed"] = json!("ultrafast");

    let mut notices = Vec::new();
    let sent = client.send(&request, &AtomicBool::new(false), &mut |delta| {
        if let Delta::Notice(text) = delta {
            notices.push(text.to_string());
        }
    });
    std::fs::remove_dir_all(&home).ok();

    assert!(sent.is_err_and(|error| error.contains("service_tier")));
    assert_eq!(requests.recv().unwrap()["service_tier"], "ultrafast");
    assert_eq!(requests.recv_timeout(std::time::Duration::from_secs(5)).unwrap().get("service_tier"), None);
    assert!(requests.try_recv().is_err());
    assert!(notices.iter().any(|n| n.contains("Retrying at standard speed")), "{notices:?}");
}

/// An SSE reply `id` that ends its turn, which a thread keeps.
fn ended_turn(id: &str) -> String {
    let events =
        [json!({"type": "message_start", "message": {"id": id, "usage": {}}}), json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {}}), json!({"type": "message_stop"})];
    events.iter().map(|event| format!("data: {event}\n\n")).collect()
}

const THREAD_NOT_FOUND: &str = r#"{"type":"error","error":{"type":"not_found_error","message":"Thread not found.","details":{"error_code":"thread_not_found"}}}"#;

/// A client that sends Anthropic turns in message threads, as it does on api.anthropic.com, to a server answering `replies`.
fn threaded_client(home: &Path, replies: &[(u16, &str)]) -> (Client, std::sync::mpsc::Receiver<Value>) {
    let mut client = Client::new(home).unwrap();
    let (base, requests) = mock_provider(replies);
    client.anthropic_base = base;
    client.reload(vec![Account { provider: "anthropic".into(), name: "work".into(), secret: Secret::ApiKey("sk".into()) }]);
    *client.catalog.write().unwrap() = vec![Model { provider: "anthropic".into(), id: "claude".into(), ..Model::default() }].into();
    client.state.lock().unwrap().message_threads = true;
    (client, requests)
}

/// The request of a turn after `history`, built the way `run_turns` builds it.
fn turn(history: &[Value]) -> Value {
    request("claude", 10, "low", "5m", "You code.", "", &json!([{"name": "Read", "input_schema": {"type": "object"}}]), history)
}

fn user(text: &str) -> Value {
    json!({"role": "user", "content": [{"type": "text", "text": text}]})
}

#[test]
fn a_reply_cut_off_after_its_text_streamed_is_retried_when_that_text_can_be_taken_back() {
    let home = std::env::temp_dir().join(format!("agent-cut-retry-{}", std::process::id()));
    let started = [
        json!({"type": "message_start", "message": {"id": "msg_1", "usage": {}}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Hel"}}),
    ];
    let cut: String = started.iter().map(|event| format!("data: {event}\n\n")).collect();
    let (client, requests) = threaded_client(&home, &[(200, &cut), (200, &ended_turn("msg_2"))]);

    let sent = client.send_in_thread(&turn(&[user("hi")]), &mut MessageThread::default(), &AtomicBool::new(false), false, &mut |_| {});
    std::fs::remove_dir_all(&home).ok();

    assert_eq!(sent.unwrap().id, "msg_2");
    assert_eq!(requests.try_iter().count(), 2);
}

#[test]
fn a_turn_in_a_thread_sends_only_the_messages_after_the_last_reply() {
    let home = std::env::temp_dir().join(format!("agent-thread-continue-{}", std::process::id()));
    let (client, requests) = threaded_client(&home, &[(200, &ended_turn("msg_1")), (200, &ended_turn("msg_2"))]);
    let mut thread = MessageThread::default();

    let mut history = vec![user("ls")];
    let first = client.send_in_thread(&turn(&history), &mut thread, &AtomicBool::new(false), true, &mut |_| {}).unwrap();
    history.extend([first.message, user("and pwd")]);
    let second = client.send_in_thread(&turn(&history), &mut thread, &AtomicBool::new(false), true, &mut |_| {});
    std::fs::remove_dir_all(&home).ok();

    assert!(second.is_ok(), "{:?}", second.err());
    let create = requests.recv().unwrap();
    assert_eq!(create["thread"], json!({"type": "create"}));
    assert_eq!(create["anthropic-beta"], format!("{THINKING_BETA},{MESSAGE_THREADS_BETA}"));
    assert!(create.get("system").is_some() && create.get("tools").is_some(), "{create}");
    let next = requests.recv().unwrap();
    assert_eq!(next["thread"], json!({"type": "continue", "previous_message_id": "msg_1"}));
    assert_eq!(next["anthropic-beta"], create["anthropic-beta"]);
    assert!(next.get("system").is_none() && next.get("tools").is_none(), "{next}");
    // Only the new message, with the breakpoint that caches the turn.
    assert_eq!(next["messages"], json!([turn(&history)["messages"][2]]));
}

#[test]
fn an_oauth_thread_names_claude_code_only_in_its_create() {
    let home = std::env::temp_dir().join(format!("agent-thread-oauth-{}", std::process::id()));
    let (client, requests) = threaded_client(&home, &[(200, &ended_turn("msg_1")), (200, &ended_turn("msg_2"))]);
    let tokens = oauth::Tokens { access: "a".into(), refresh: "r".into(), expires: u64::MAX, account_id: None };
    client.reload(vec![Account { provider: "anthropic".into(), name: "pro".into(), secret: Secret::OAuth(tokens) }]);
    let mut thread = MessageThread::default();

    let mut history = vec![user("ls")];
    let first = client.send_in_thread(&turn(&history), &mut thread, &AtomicBool::new(false), true, &mut |_| {}).unwrap();
    history.extend([first.message, user("and pwd")]);
    let second = client.send_in_thread(&turn(&history), &mut thread, &AtomicBool::new(false), true, &mut |_| {});
    std::fs::remove_dir_all(&home).ok();

    assert!(second.is_ok(), "{:?}", second.err());
    let create = requests.recv().unwrap();
    assert_eq!(create["system"][0]["text"], CLAUDE_CODE_IDENTITY);
    let next = requests.recv().unwrap();
    assert_eq!(next["thread"]["type"], "continue");
    assert!(next.get("system").is_none(), "{next}");
    assert_eq!(next["anthropic-beta"], create["anthropic-beta"]);
}

#[test]
fn a_thread_continues_only_an_unchanged_conversation_after_a_reply_the_api_keeps() {
    let reply =
        |stop_reason: &str| Reply { message: json!({"role": "assistant", "content": [{"type": "text", "text": "ok"}]}), stop_reason: stop_reason.into(), id: "msg_1".into(), ..Reply::default() };
    let history = vec![user("ls")];
    let next = [history.clone(), vec![reply("end_turn").message, user("and pwd")]].concat();
    let mut thread = MessageThread::default();
    thread.record("anthropic/work".into(), &turn(&history), &reply("end_turn"));
    assert!(thread.continue_request("anthropic/work", &turn(&next)).is_some());

    let mut edited = next.clone();
    edited[0] = user("pwd");
    assert!(thread.continue_request("anthropic/work", &turn(&edited)).is_none());
    let answered_elsewhere = [next.clone(), vec![reply("end_turn").message, user("and date")]].concat();
    assert!(thread.continue_request("anthropic/work", &turn(&answered_elsewhere)).is_none());
    let mut other_effort = turn(&next);
    other_effort["output_config"]["effort"] = json!("high");
    assert!(thread.continue_request("anthropic/work", &other_effort).is_none());
    assert!(thread.continue_request("anthropic/other", &turn(&next)).is_none());
    thread.last = Some(("msg_1".into(), Instant::now() - THREAD_IDLE));
    assert!(thread.continue_request("anthropic/work", &turn(&next)).is_none());

    let unparsed = BadToolInput { id: "t1".into(), error: "Bad tool input JSON.".into(), raw: "{".into() };
    for kept_back in [reply("max_tokens"), reply("refusal"), reply("pause_turn"), reply("aborted"), Reply { bad_tool_inputs: vec![unparsed], ..reply("tool_use") }] {
        thread.record("anthropic/work".into(), &turn(&history), &reply("end_turn"));
        thread.record("anthropic/work".into(), &turn(&history), &kept_back);
        assert!(thread.continue_request("anthropic/work", &turn(&next)).is_none(), "{}", kept_back.stop_reason);
    }
}

#[test]
fn a_lost_thread_is_replayed_and_turns_go_without_one_once_a_replay_is_lost() {
    let home = std::env::temp_dir().join(format!("agent-thread-lost-{}", std::process::id()));
    let (msg_1, msg_2, msg_3) = (ended_turn("msg_1"), ended_turn("msg_2"), ended_turn("msg_3"));
    let (client, requests) = threaded_client(&home, &[(200, &msg_1), (404, THREAD_NOT_FOUND), (200, &msg_2), (404, THREAD_NOT_FOUND), (200, &msg_3)]);
    let mut thread = MessageThread::default();

    let mut history = vec![user("ls")];
    for text in ["and pwd", "and date", "and whoami"] {
        let reply = client.send_in_thread(&turn(&history), &mut thread, &AtomicBool::new(false), true, &mut |_| {}).unwrap();
        history.extend([reply.message, user(text)]);
    }
    let last = client.send_in_thread(&turn(&history), &mut thread, &AtomicBool::new(false), true, &mut |_| {});
    std::fs::remove_dir_all(&home).ok();

    assert!(last.is_ok(), "{:?}", last.err());
    let sent: Vec<Value> = requests.try_iter().collect();
    let threads: Vec<Value> = sent.iter().map(|request| request["thread"].clone()).collect();
    let create = json!({"type": "create"});
    let continue_from = |id: &str| json!({"type": "continue", "previous_message_id": id});
    assert_eq!(threads, [create.clone(), continue_from("msg_1"), create, continue_from("msg_2"), Value::Null, Value::Null]);
    // The replay carries the whole conversation; a request outside a thread names only the thinking beta.
    assert_eq!(sent[2]["messages"].as_array().unwrap().len(), 3);
    assert_eq!(sent[4]["anthropic-beta"], THINKING_BETA);
}

#[test]
fn a_turn_the_api_cannot_keep_in_a_thread_goes_without_one() {
    let home = std::env::temp_dir().join(format!("agent-thread-unsupported-{}", std::process::id()));
    let refused = r#"{"type":"error","error":{"type":"invalid_request_error","message":"Unsupported.","details":{"error_code":"thread_unsupported_request","unsupported":["hipaa"]}}}"#;
    let (client, requests) = threaded_client(&home, &[(400, refused), (200, &ended_turn("msg_1"))]);
    let mut thread = MessageThread::default();

    let mut history = vec![user("ls")];
    let first = client.send_in_thread(&turn(&history), &mut thread, &AtomicBool::new(false), true, &mut |_| {}).unwrap();
    history.extend([first.message, user("and pwd")]);
    let second = client.send_in_thread(&turn(&history), &mut thread, &AtomicBool::new(false), true, &mut |_| {});
    std::fs::remove_dir_all(&home).ok();

    assert!(second.is_ok(), "{:?}", second.err());
    let threads: Vec<Value> = requests.try_iter().map(|request| request["thread"].clone()).collect();
    assert_eq!(threads, [json!({"type": "create"}), Value::Null, Value::Null]);
}

const INVALID_REQUEST: &str = r#"{"type":"error","error":{"type":"invalid_request_error","message":"Invalid."}}"#;

#[test]
fn a_continue_refused_with_an_unknown_400_is_replayed() {
    let home = std::env::temp_dir().join(format!("agent-thread-continue-400-{}", std::process::id()));
    let (client, requests) = threaded_client(&home, &[(200, &ended_turn("msg_1")), (400, INVALID_REQUEST), (200, &ended_turn("msg_2"))]);
    let mut thread = MessageThread::default();

    let mut history = vec![user("ls")];
    let first = client.send_in_thread(&turn(&history), &mut thread, &AtomicBool::new(false), true, &mut |_| {}).unwrap();
    history.extend([first.message, user("and pwd")]);
    let second = client.send_in_thread(&turn(&history), &mut thread, &AtomicBool::new(false), true, &mut |_| {});
    std::fs::remove_dir_all(&home).ok();

    assert!(second.is_ok(), "{:?}", second.err());
    let threads: Vec<Value> = requests.try_iter().map(|request| request["thread"].clone()).collect();
    assert_eq!(threads, [json!({"type": "create"}), json!({"type": "continue", "previous_message_id": "msg_1"}), json!({"type": "create"})]);
}

#[test]
fn a_create_refused_with_an_unknown_400_goes_without_a_thread_on_that_model_once_the_plain_turn_passes() {
    let home = std::env::temp_dir().join(format!("agent-thread-create-400-{}", std::process::id()));
    let (client, requests) = threaded_client(&home, &[(400, INVALID_REQUEST), (200, &ended_turn("msg_1"))]);
    let models = ["claude", "other"].map(|id| Model { provider: "anthropic".into(), id: id.into(), ..Model::default() });
    *client.catalog.write().unwrap() = models.to_vec().into();
    let mut thread = MessageThread::default();

    let mut history = vec![user("ls")];
    let first = client.send_in_thread(&turn(&history), &mut thread, &AtomicBool::new(false), true, &mut |_| {}).unwrap();
    history.extend([first.message, user("and pwd")]);
    client.send_in_thread(&turn(&history), &mut thread, &AtomicBool::new(false), true, &mut |_| {}).unwrap();
    let mut other_model = turn(&history);
    other_model["model"] = json!("other");
    client.send_in_thread(&other_model, &mut thread, &AtomicBool::new(false), true, &mut |_| {}).unwrap();
    std::fs::remove_dir_all(&home).ok();

    let threads: Vec<Value> = requests.try_iter().map(|request| request["thread"].clone()).collect();
    assert_eq!(threads, [json!({"type": "create"}), Value::Null, Value::Null, json!({"type": "create"})]);
}

#[test]
fn a_create_refused_with_a_400_the_plain_turn_gets_too_keeps_threads_on() {
    let home = std::env::temp_dir().join(format!("agent-thread-create-real-400-{}", std::process::id()));
    let (client, requests) = threaded_client(&home, &[(400, INVALID_REQUEST), (400, INVALID_REQUEST), (200, &ended_turn("msg_1"))]);
    let mut thread = MessageThread::default();

    let history = vec![user("ls")];
    let refused = client.send_in_thread(&turn(&history), &mut thread, &AtomicBool::new(false), true, &mut |_| {});
    let retried = client.send_in_thread(&turn(&history), &mut thread, &AtomicBool::new(false), true, &mut |_| {});
    std::fs::remove_dir_all(&home).ok();

    assert!(refused.is_err() && retried.is_ok());
    let threads: Vec<Value> = requests.try_iter().map(|request| request["thread"].clone()).collect();
    assert_eq!(threads, [json!({"type": "create"}), Value::Null, json!({"type": "create"})]);
}

#[test]
fn threads_stay_off_behind_headroom_or_another_anthropic_base_url() {
    let home = std::env::temp_dir().join(format!("agent-thread-proxy-{}", std::process::id()));
    let mut client = Client::new(&home).unwrap();
    let mut settings = crate::settings::load(&home).unwrap();
    client.anthropic_base = ANTHROPIC_API.into();
    client.configure(&settings);
    assert!(client.message_threads());

    (settings.headroom, settings.headroom_url) = (true, "http://127.0.0.1:9".into());
    client.configure(&settings);
    assert!(!client.message_threads());
    settings.headroom = false;
    client.anthropic_base = "http://127.0.0.1:9".into();
    client.configure(&settings);
    std::fs::remove_dir_all(&home).ok();
    assert!(!client.message_threads());
}
