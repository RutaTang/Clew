use super::*;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines};

/// The fake agent's ends of the pipes.
struct Fake {
    lines: Lines<BufReader<DuplexStream>>,
    out: DuplexStream,
}

impl Fake {
    async fn recv(&mut self) -> Value {
        let line = self.lines.next_line().await.unwrap().expect("a message");
        serde_json::from_str(&line).unwrap()
    }

    async fn send(&mut self, msg: Value) {
        self.out
            .write_all(format!("{msg}\n").as_bytes())
            .await
            .unwrap();
    }

    /// Answer `initialize`.
    async fn initialize(&mut self, auth: Value) {
        let init = self.recv().await;
        assert_eq!(init["method"], "initialize");
        assert_eq!(init["params"]["protocolVersion"], 1);
        assert_eq!(
            init["params"]["clientCapabilities"]["fs"]["readTextFile"],
            true
        );
        assert_eq!(init["params"]["clientCapabilities"]["terminal"], false);
        self.send(json!({"jsonrpc": "2.0", "id": init["id"], "result": {
            "protocolVersion": 1,
            "agentCapabilities": {"promptCapabilities": {"embeddedContext": true}, "mcpCapabilities": {"http": false}},
            "agentInfo": {"name": "fake-agent", "title": "Fake Agent", "version": "3"},
            "authMethods": auth
        }}))
        .await;
    }
}

fn start() -> (DuplexStream, DuplexStream, Fake) {
    let (client_out, agent_in) = tokio::io::duplex(1 << 16);
    let (agent_out, client_in) = tokio::io::duplex(1 << 16);
    (
        client_in,
        client_out,
        Fake {
            lines: BufReader::new(agent_in).lines(),
            out: agent_out,
        },
    )
}

fn project() -> crate::testutil::TempDir {
    let dir = crate::testutil::TempDir::new("acp");
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/a.rs"), "one\ntwo\nthree\n").unwrap();
    dir
}

#[tokio::test]
async fn a_turn_streams_its_updates_and_serves_the_agents_requests() {
    let dir = project();
    let root = dir.path().canonicalize().unwrap();
    let (reader, writer, mut fake) = start();
    let agent_root = root.clone();
    let agent_task = tokio::spawn(async move {
        fake.initialize(json!([{"id": "login", "name": "Log in"}]))
            .await;
        // The first session needs a sign-in, which the client asks for.
        let new = fake.recv().await;
        assert_eq!(new["method"], "session/new");
        assert_eq!(new["params"]["cwd"], agent_root.to_string_lossy().as_ref());
        let servers = new["params"]["mcpServers"].as_array().unwrap();
        assert_eq!(
            servers.len(),
            1,
            "the HTTP server is held back: {servers:?}"
        );
        assert_eq!(servers[0]["name"], "clew");
        assert_eq!(servers[0]["args"][0], "mcp");
        fake.send(json!({"jsonrpc": "2.0", "id": new["id"], "error": {"code": -32000, "message": "Authentication required"}})).await;
        let auth = fake.recv().await;
        assert_eq!(
            (auth["method"].as_str(), auth["params"]["methodId"].as_str()),
            (Some("authenticate"), Some("login"))
        );
        fake.send(json!({"jsonrpc": "2.0", "id": auth["id"], "result": {}}))
            .await;
        let new = fake.recv().await;
        fake.send(json!({"jsonrpc": "2.0", "id": new["id"], "result": {"sessionId": "s1"}}))
            .await;

        let prompt = fake.recv().await;
        assert_eq!(prompt["method"], "session/prompt");
        assert_eq!(prompt["params"]["prompt"][0]["text"], "why?");
        assert_eq!(prompt["params"]["prompt"][1]["type"], "resource");
        let update = |u: Value| json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s1", "update": u}});
        fake.send(update(json!({"sessionUpdate": "plan", "entries": [{"content": "look", "priority": "high", "status": "in_progress"}]}))).await;
        fake.send(update(json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "hmm"}}))).await;
        fake.send(update(json!({"sessionUpdate": "tool_call", "toolCallId": "t1", "title": "Read a.rs", "kind": "read", "status": "pending",
            "locations": [{"path": agent_root.join("src/a.rs").to_string_lossy(), "line": 2}]}))).await;
        // Reads through the client: inside the project, and outside it.
        let a = agent_root.join("src/a.rs").to_string_lossy().into_owned();
        fake.send(json!({"jsonrpc": "2.0", "id": 100, "method": "fs/read_text_file", "params": {"sessionId": "s1", "path": a, "line": 2, "limit": 1}})).await;
        let read = fake.recv().await;
        assert_eq!(read["result"]["content"], "two\n");
        fake.send(json!({"jsonrpc": "2.0", "id": 101, "method": "fs/read_text_file", "params": {"sessionId": "s1", "path": "/etc/hostname"}})).await;
        let refused = fake.recv().await;
        assert!(
            refused["error"]["message"]
                .as_str()
                .unwrap()
                .contains("outside the project"),
            "{refused}"
        );
        // Ask before writing.
        fake.send(json!({"jsonrpc": "2.0", "id": 102, "method": "session/request_permission", "params": {"sessionId": "s1",
            "toolCall": {"toolCallId": "t2", "title": "Write b.rs", "kind": "edit"},
            "options": [{"optionId": "yes", "name": "Allow", "kind": "allow_once"}, {"optionId": "no", "name": "Reject", "kind": "reject_once"}]}})).await;
        let answer = fake.recv().await;
        assert_eq!(answer["id"], 102);
        assert_eq!(
            answer["result"]["outcome"],
            json!({"outcome": "selected", "optionId": "yes"})
        );
        let b = agent_root
            .join("src/new/b.rs")
            .to_string_lossy()
            .into_owned();
        fake.send(json!({"jsonrpc": "2.0", "id": 103, "method": "fs/write_text_file", "params": {"sessionId": "s1", "path": b, "content": "fn b() {}\n"}})).await;
        assert_eq!(fake.recv().await["result"], json!({}));
        fake.send(update(
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed",
            "content": [{"type": "diff", "path": "src/a.rs", "oldText": "a", "newText": "b\nc"}]}),
        ))
        .await;
        fake.send(update(json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Because "}}))).await;
        fake.send(update(json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "reasons."}}))).await;
        fake.send(json!({"jsonrpc": "2.0", "id": 104, "method": "terminal/create", "params": {}}))
            .await;
        assert_eq!(
            fake.recv().await["error"]["code"],
            crate::linerpc::METHOD_NOT_FOUND
        );
        fake.send(
            json!({"jsonrpc": "2.0", "id": prompt["id"], "result": {"stopReason": "end_turn"}}),
        )
        .await;
    });

    let (agent, mut events) = Agent::over(reader, writer, &root).await.unwrap();
    assert_eq!(agent.info().display_name(), "Fake Agent");
    assert!(agent.info().embedded_context);
    let mcp = vec![
        McpServerSpec::Stdio {
            name: "clew".into(),
            command: "/bin/clew-server".into(),
            args: vec![
                "mcp".into(),
                "--root".into(),
                root.to_string_lossy().into_owned(),
            ],
            env: vec![],
        },
        McpServerSpec::Http {
            name: "web".into(),
            url: "https://x/mcp".into(),
            headers: vec![],
        },
    ];
    let session = agent.new_session(&mcp).await.unwrap();
    assert_eq!(session, "s1");
    let blocks = vec![
        text_block("why?"),
        file_block(agent.info(), &root.join("src/a.rs"), Some("one")),
    ];
    let agent = Arc::new(agent);
    let turn = {
        let agent = agent.clone();
        let session = session.clone();
        tokio::spawn(async move { agent.prompt(&session, blocks).await })
    };
    let mut text = String::new();
    let mut seen = Vec::new();
    loop {
        match events.recv().await.unwrap() {
            AgentEvent::Text { text: t, .. } => text.push_str(&t),
            AgentEvent::Permission(request) => {
                assert_eq!(request.tool.title.as_deref(), Some("Write b.rs"));
                assert_eq!(request.options.len(), 2);
                agent.answer_permission(request.id, Some("yes"));
                agent.answer_permission(request.id, Some("again"));
                seen.push("permission".to_string());
            }
            AgentEvent::FileWritten { path } => {
                assert_eq!(std::fs::read_to_string(&path).unwrap(), "fn b() {}\n");
                seen.push("written".to_string());
            }
            AgentEvent::ToolCall { call, .. } => {
                assert_eq!(call.kind.as_deref(), Some("read"));
                assert_eq!(call.locations.as_ref().unwrap()[0].1, Some(2));
                seen.push("call".to_string());
            }
            AgentEvent::ToolCallUpdate { call, .. } => {
                assert_eq!(call.status, Some(ToolStatus::Completed));
                assert_eq!(
                    call.summary.as_deref(),
                    Some("edited src/a.rs (−1 +2 lines)")
                );
                seen.push("update".to_string());
            }
            AgentEvent::Plan { entries, .. } => {
                assert_eq!(entries[0].content, "look");
                seen.push("plan".to_string());
            }
            AgentEvent::Thought { text, .. } => {
                assert_eq!(text, "hmm");
                seen.push("thought".to_string());
            }
            AgentEvent::Exited(why) => panic!("exited: {why}"),
            AgentEvent::Options { .. } => {}
        }
        if text == "Because reasons." {
            break;
        }
    }
    assert_eq!(turn.await.unwrap().unwrap(), StopReason::EndTurn);
    assert_eq!(
        seen,
        ["plan", "thought", "call", "permission", "written", "update"]
    );
    agent_task.await.unwrap();
}

#[tokio::test]
async fn cancelling_a_turn_answers_its_open_permissions() {
    let dir = project();
    let root = dir.path().canonicalize().unwrap();
    let (reader, writer, mut fake) = start();
    let agent_task = tokio::spawn(async move {
        fake.initialize(json!([])).await;
        let new = fake.recv().await;
        fake.send(json!({"jsonrpc": "2.0", "id": new["id"], "result": {"sessionId": "s9"}}))
            .await;
        let prompt = fake.recv().await;
        fake.send(json!({"jsonrpc": "2.0", "id": "perm", "method": "session/request_permission", "params": {"sessionId": "s9",
            "toolCall": {"toolCallId": "t"}, "options": [{"optionId": "ok", "name": "OK", "kind": "allow_once"}]}})).await;
        let cancel = fake.recv().await;
        assert_eq!(cancel["method"], "session/cancel");
        assert!(cancel.get("id").is_none());
        let answer = fake.recv().await;
        assert_eq!(answer["id"], "perm");
        assert_eq!(answer["result"]["outcome"]["outcome"], "cancelled");
        fake.send(
            json!({"jsonrpc": "2.0", "id": prompt["id"], "result": {"stopReason": "cancelled"}}),
        )
        .await;
    });
    let (agent, mut events) = Agent::over(reader, writer, &root).await.unwrap();
    let agent = Arc::new(agent);
    let session = agent.new_session(&[]).await.unwrap();
    let turn = {
        let agent = agent.clone();
        tokio::spawn(async move { agent.prompt("s9", vec![text_block("go")]).await })
    };
    let AgentEvent::Permission(_) = events.recv().await.unwrap() else {
        panic!("expected a permission request");
    };
    agent.cancel(&session);
    assert_eq!(turn.await.unwrap().unwrap(), StopReason::Cancelled);
    agent_task.await.unwrap();
}

#[tokio::test]
async fn an_agent_of_another_protocol_version_is_refused() {
    let dir = project();
    let (reader, writer, mut fake) = start();
    tokio::spawn(async move {
        let init = fake.recv().await;
        fake.send(json!({"jsonrpc": "2.0", "id": init["id"], "result": {"protocolVersion": 2}}))
            .await;
    });
    let err = Agent::over(reader, writer, dir.path()).await.unwrap_err();
    assert!(err.to_string().contains("version 2"), "{err}");
}

#[tokio::test]
async fn a_terminal_only_login_is_explained() {
    let dir = project();
    let (reader, writer, mut fake) = start();
    tokio::spawn(async move {
        fake.initialize(
            json!([{"id": "t", "name": "Terminal login", "type": "terminal", "args": ["--login"]}]),
        )
        .await;
        let new = fake.recv().await;
        fake.send(json!({"jsonrpc": "2.0", "id": new["id"], "error": {"code": -32000, "message": "auth"}})).await;
        // Keep the pipe open until the client is done.
        let _ = fake.lines.next_line().await;
    });
    let (agent, _events) = Agent::over(reader, writer, dir.path()).await.unwrap();
    let err = agent.new_session(&[]).await.unwrap_err();
    assert!(
        matches!(&err, AcpError::NeedsLogin(how) if how.contains("--login")),
        "{err:?}"
    );
}

#[test]
fn writes_stay_in_the_project() {
    let dir = project();
    let root = dir.path().canonicalize().unwrap();
    let outside = root.parent().unwrap().join("clew-acp-escape.txt");
    let params = |path: &Path| json!({"path": path.to_string_lossy(), "content": "x"});
    assert!(write_file(&root, &params(&outside)).is_err());
    assert!(write_file(&root, &params(&root.join("../escape.txt"))).is_err());
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(root.parent().unwrap(), root.join("up")).unwrap();
        assert!(write_file(&root, &params(&root.join("up/escape.txt"))).is_err());
        std::os::unix::fs::symlink(&outside, root.join("link.txt")).unwrap();
        assert!(write_file(&root, &params(&root.join("link.txt"))).is_err());
        // Folders past a link that leads out are not made either.
        let away = crate::testutil::TempDir::new("acp-away");
        std::os::unix::fs::symlink(away.path(), root.join("away")).unwrap();
        assert!(write_file(&root, &params(&root.join("away/new/deep/x.txt"))).is_err());
        assert!(!away.path().join("new").exists(), "nothing made outside");
        // A link that stays inside is followed.
        std::fs::create_dir(root.join("real")).unwrap();
        std::os::unix::fs::symlink(root.join("real"), root.join("alias")).unwrap();
        let made = write_file(&root, &params(&root.join("alias/sub/y.txt"))).unwrap();
        assert_eq!(made, root.join("real/sub/y.txt"));
    }
    assert!(!outside.exists());
    assert!(write_file(&root, &params(&root.join("ok.txt"))).is_ok());
    let deep = write_file(&root, &params(&root.join("a/b/c.txt"))).unwrap();
    assert_eq!(std::fs::read_to_string(deep).unwrap(), "x");
}

#[tokio::test]
async fn session_settings_are_offered_and_chosen() {
    let dir = project();
    let (reader, writer, mut fake) = start();
    let agent_task = tokio::spawn(async move {
        fake.initialize(json!([])).await;
        let new = fake.recv().await;
        fake.send(json!({"jsonrpc": "2.0", "id": new["id"], "result": {"sessionId": "s1",
            "modes": {"currentModeId": "ask", "availableModes": [{"id": "ask", "name": "Ask"}, {"id": "code", "name": "Code"}]},
            "configOptions": [
                {"id": "model", "name": "Model", "category": "model", "type": "select", "currentValue": "m1",
                 "options": [{"group": "fast", "name": "Fast", "options": [{"value": "m1", "name": "Model 1"}]},
                             {"group": "big", "name": "Big", "options": [{"value": "m2", "name": "Model 2"}]}]},
                {"id": "verbose", "name": "Verbose", "type": "boolean", "currentValue": true}
            ]}})).await;
        let set = fake.recv().await;
        assert_eq!(set["method"], "session/set_config_option");
        assert_eq!(
            (
                set["params"]["configId"].as_str(),
                set["params"]["value"].as_str()
            ),
            (Some("model"), Some("m2"))
        );
        fake.send(json!({"jsonrpc": "2.0", "id": set["id"], "result": {}}))
            .await;
        fake.send(json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s1",
            "update": {"sessionUpdate": "config_option_update", "configOptions": [
                {"id": "model", "name": "Model", "type": "select", "currentValue": "m1", "options": [{"value": "m1", "name": "Model 1"}]}
            ]}}})).await;
        let _ = fake.lines.next_line().await;
    });
    let (agent, mut events) = Agent::over(reader, writer, dir.path()).await.unwrap();
    let session = agent.new_session(&[]).await.unwrap();
    let options = agent.options(&session);
    assert_eq!(
        options.len(),
        1,
        "config options win over modes; booleans are not offered"
    );
    assert_eq!(
        options[0].choices,
        [
            ("m1".to_string(), "Model 1".to_string()),
            ("m2".to_string(), "Model 2".to_string())
        ]
    );
    let after = agent.set_option(&session, "model", "m2").await.unwrap();
    assert_eq!(after[0].current, "m2");
    let AgentEvent::Options { options, .. } = events.recv().await.unwrap() else {
        panic!("expected the settings to change");
    };
    assert_eq!(options[0].choices.len(), 1);
    drop(agent);
    let _ = agent_task.await;
}

#[test]
fn legacy_modes_read_as_a_setting() {
    let options = read_session_options(&json!({
        "modes": {"currentModeId": "plan", "availableModes": [{"id": "plan", "name": "Plan"}, {"id": "edit", "name": "Edit"}]}
    }));
    assert_eq!(options.len(), 1);
    assert_eq!(options[0].id, MODE_OPTION);
    assert_eq!(options[0].current, "plan");
}

#[test]
fn settings_updates_are_kept_only_for_sessions_and_kinds_they_concern() {
    let store: SessionOptions = Default::default();
    let update = |session: &str, update: Value| {
        options_update(&json!({"sessionId": session, "update": update}), &store)
    };
    // A chunk is no settings update, and makes no entry.
    assert!(update("s1", json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "hi"}})).is_none());
    // A mode change for a session clew holds no settings of is nothing.
    assert!(
        update(
            "s1",
            json!({"sessionUpdate": "current_mode_update", "currentModeId": "code"})
        )
        .is_none()
    );
    assert!(store.lock().unwrap().is_empty());
    // So many sessions' settings are kept, and no more.
    let list = json!({"sessionUpdate": "config_option_update", "configOptions": [
        {"id": "model", "name": "Model", "type": "select", "currentValue": "m1", "options": [{"value": "m1", "name": "M1"}]}
    ]});
    for i in 0..MAX_SESSIONS {
        assert!(update(&format!("s{i}"), list.clone()).is_some());
    }
    assert!(update("one-too-many", list.clone()).is_none());
    assert!(
        update("s0", list).is_some(),
        "a known session is still followed"
    );
    assert_eq!(store.lock().unwrap().len(), MAX_SESSIONS);
}

#[test]
fn an_agent_option_cannot_pass_for_clews_modes() {
    let options = read_session_options(&json!({"configOptions": [
        {"id": MODE_OPTION, "name": "Sneaky", "type": "select", "currentValue": "a", "options": [{"value": "a", "name": "A"}]},
        {"id": "model", "name": "Model", "type": "select", "currentValue": "m1", "options": [{"value": "m1", "name": "M1"}]}
    ]}));
    assert_eq!(options.len(), 1);
    assert_eq!(options[0].id, "model");
}
