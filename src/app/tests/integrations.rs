//! MCP tool calls of an agent turn, and ACP agents in the Ask panel.

use super::*;
use crate::app::integrations::{AcpLink, ToolMap, ToolTarget};
use clew_core::{acp, mcp};
use serde_json::{Value, json};
use std::sync::atomic::AtomicBool;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

/// A tool host that upper-cases its argument.
struct Shout;

impl mcp::serve::Host for Shout {
    fn identity(&self) -> (String, String, Option<String>) {
        ("shout".into(), "1".into(), None)
    }
    fn tools(&self) -> Vec<mcp::serve::Tool> {
        vec![mcp::serve::Tool {
            name: "shout".into(),
            title: None,
            description: "Upper-cases s".into(),
            input_schema: json!({"type": "object", "properties": {"s": {"type": "string"}}}),
        }]
    }
    fn call(&self, _: &str, args: &Value, _: &AtomicBool) -> Result<mcp::ToolOutput, String> {
        Ok(mcp::ToolOutput {
            text: args["s"].as_str().unwrap_or_default().to_uppercase(),
            is_error: false,
        })
    }
}

/// A connected MCP client of [`Shout`], on `rt` (which must outlive it), and
/// a tool map naming its one tool `mcp__shout__shout` under `approve`.
fn shout_tools(rt: &tokio::runtime::Runtime, approve: mcp::Approve) -> ToolMap {
    let client = rt.block_on(async {
        let (client_out, server_in) = tokio::io::duplex(1 << 16);
        let (server_out, client_in) = tokio::io::duplex(1 << 16);
        tokio::spawn(mcp::serve::serve(Arc::new(Shout), server_in, server_out));
        mcp::Client::over("shout", client_in, client_out)
            .await
            .unwrap()
    });
    let tool = rt.block_on(client.tools()).unwrap()[0].clone();
    let server = mcp::ServerConfig {
        name: "shout".into(),
        transport: mcp::Transport::Stdio(clew_core::linerpc::Launch::default()),
        enabled: true,
        approve,
        allow: Vec::new(),
        timeout: std::time::Duration::from_secs(10),
    };
    let mut map = ToolMap::new();
    map.insert(
        "mcp__shout__shout".into(),
        ToolTarget {
            server,
            client: Arc::new(client),
            tool,
        },
    );
    map
}

fn external() -> Vec<clew_protocol::ExternalTool> {
    vec![clew_protocol::ExternalTool {
        name: "mcp__shout__shout".into(),
        description: "Upper-cases s".into(),
        parameters: json!({"type": "object"}),
    }]
}

/// The first `AgentToolResult` the app sent.
fn tool_result(
    rx: &mut tokio::sync::mpsc::Receiver<clew_protocol::ClientMessage>,
) -> (u64, u64, String, bool) {
    while let Ok(msg) = rx.try_recv() {
        if let clew_protocol::Request::AgentToolResult {
            stream,
            call,
            content,
            is_error,
        } = msg.request
        {
            return (stream, call, content, is_error);
        }
    }
    panic!("no AgentToolResult was sent");
}

#[test]
fn an_mcp_call_waits_for_approval_then_its_result_goes_to_the_turn() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut app = scanned_app("mcp-approve");
    let mut rx = attach_server(&mut app);
    let _pump = app.start_agent_turn(
        "why?".into(),
        external(),
        shout_tools(&rt, mcp::Approve::Ask),
    );
    let stream = app.proj.link.agent_stream.expect("an agent turn is open");
    let asked = std::iter::from_fn(|| rx.try_recv().ok())
        .find_map(|m| match m.request {
            clew_protocol::Request::AgentAsk { tools, .. } => Some(tools),
            _ => None,
        })
        .expect("the turn was asked");
    assert_eq!(
        asked[0].name, "mcp__shout__shout",
        "the turn offers the tool"
    );

    // The server's call reaches the turn through its piece channel.
    let _ = app.handle_server_event(clew_protocol::Event::AgentToolCall {
        stream,
        call: 1,
        tool: "mcp__shout__shout".into(),
        args: json!({"s": "hi"}),
    });
    let _ = app.update(Message::Ask(AskMsg::AgentToolCalled {
        stamp: app.stamp(),
        stream,
        call: 1,
        tool: "mcp__shout__shout".into(),
        args: json!({"s": "hi"}),
    }));
    let turn = app.proj.ask_turns.last().unwrap();
    assert_eq!(turn.extras.approvals.len(), 1, "it asks first");
    assert!(turn.extras.approvals[0].detail.contains("\"hi\""));
    assert_eq!(turn.steps.last().unwrap().state, StepState::Waiting);
    assert!(rx.try_recv().is_err(), "nothing runs before the answer");

    let run = app.update(Message::Ask(AskMsg::Approve {
        stream,
        key: ApprovalKey::Mcp { call: 1 },
        option: "once".into(),
    }));
    assert!(
        app.proj
            .ask_turns
            .last()
            .unwrap()
            .extras
            .approvals
            .is_empty()
    );
    drive(&mut app, run, |m| {
        matches!(m, Message::Ask(AskMsg::McpToolDone { .. }))
    });
    assert_eq!(tool_result(&mut rx), (stream, 1, "HI".into(), false));
    assert_eq!(
        app.proj
            .ask_turns
            .last()
            .unwrap()
            .steps
            .last()
            .unwrap()
            .state,
        StepState::Done
    );
}

#[test]
fn a_declined_or_unknown_mcp_call_is_answered_as_an_error() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut app = scanned_app("mcp-deny");
    let mut rx = attach_server(&mut app);
    let _pump = app.start_agent_turn(
        "why?".into(),
        external(),
        shout_tools(&rt, mcp::Approve::Ask),
    );
    let stream = app.proj.link.agent_stream.unwrap();
    while rx.try_recv().is_ok() {}
    let call = |app: &mut App, call: u64, tool: &str| {
        let stamp = app.stamp();
        app.update(Message::Ask(AskMsg::AgentToolCalled {
            stamp,
            stream,
            call,
            tool: tool.into(),
            args: json!({}),
        }))
    };
    let _ = call(&mut app, 1, "mcp__shout__shout");
    let _ = app.update(Message::Ask(AskMsg::Approve {
        stream,
        key: ApprovalKey::Mcp { call: 1 },
        option: "deny".into(),
    }));
    let (_, _, content, is_error) = tool_result(&mut rx);
    assert!(is_error && content.contains("declined"), "{content}");
    let _ = call(&mut app, 2, "mcp__nobody__nothing");
    let (_, call_no, content, is_error) = tool_result(&mut rx);
    assert_eq!(call_no, 2);
    assert!(is_error && content.contains("not a tool"), "{content}");
    // Allowed only after the turn stopped waiting for it: not run.
    let _ = call(&mut app, 3, "mcp__shout__shout");
    app.proj
        .link
        .mcp_pending
        .get_mut(&(stream, 3))
        .unwrap()
        .deadline = std::time::Instant::now() - std::time::Duration::from_secs(1);
    let _ = app.update(Message::Ask(AskMsg::Approve {
        stream,
        key: ApprovalKey::Mcp { call: 3 },
        option: "once".into(),
    }));
    assert!(app.proj.inflight.mcp_calls.is_empty(), "not run");
    assert!(app.status.contains("waited too long"), "{}", app.status);
    let turn = app.proj.ask_turns.last().unwrap();
    assert_eq!(turn.steps.last().unwrap().state, StepState::Failed);
}

#[test]
fn a_trusted_server_runs_at_once_and_stop_drops_its_calls() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut app = scanned_app("mcp-trusted");
    let mut rx = attach_server(&mut app);
    let _pump = app.start_agent_turn(
        "why?".into(),
        external(),
        shout_tools(&rt, mcp::Approve::Always),
    );
    let stream = app.proj.link.agent_stream.unwrap();
    while rx.try_recv().is_ok() {}
    let stamp = app.stamp();
    let run = app.update(Message::Ask(AskMsg::AgentToolCalled {
        stamp,
        stream,
        call: 1,
        tool: "mcp__shout__shout".into(),
        args: json!({"s": "go"}),
    }));
    assert!(
        app.proj
            .ask_turns
            .last()
            .unwrap()
            .extras
            .approvals
            .is_empty()
    );
    assert_eq!(app.proj.inflight.mcp_calls.len(), 1, "running");
    drive(&mut app, run, |m| {
        matches!(m, Message::Ask(AskMsg::McpToolDone { .. }))
    });
    assert_eq!(tool_result(&mut rx).2, "GO");

    // A call still running when the user presses Stop is dropped.
    let stamp = app.stamp();
    let _ = app.update(Message::Ask(AskMsg::AgentToolCalled {
        stamp,
        stream,
        call: 2,
        tool: "mcp__shout__shout".into(),
        args: json!({"s": "late"}),
    }));
    let _ = app.update(Message::Ask(AskMsg::Stop));
    assert!(app.proj.inflight.mcp_calls.is_empty());
    assert!(app.proj.link.agent_tools.is_empty() || app.proj.link.mcp_pending.is_empty());
    let late = app.update(Message::Ask(AskMsg::McpToolDone {
        stamp: app.stamp(),
        stream,
        call: 2,
        result: Ok(mcp::ToolOutput {
            text: "LATE".into(),
            is_error: false,
        }),
    }));
    drop(late);
    assert!(
        std::iter::from_fn(|| rx.try_recv().ok())
            .all(|m| !matches!(m.request, clew_protocol::Request::AgentToolResult { .. })),
        "a stopped call is owed nothing"
    );
    // A call that crossed the Cancel on its way is not run.
    let stamp = app.stamp();
    let _ = app.update(Message::Ask(AskMsg::AgentToolCalled {
        stamp,
        stream,
        call: 3,
        tool: "mcp__shout__shout".into(),
        args: json!({"s": "after"}),
    }));
    assert!(app.proj.inflight.mcp_calls.is_empty(), "not run");
    assert!(
        app.proj
            .ask_turns
            .last()
            .unwrap()
            .extras
            .approvals
            .is_empty(),
        "nor asked about"
    );
    let (_, call, _, is_error) = tool_result(&mut rx);
    assert!(call == 3 && is_error);
}

// ------------------------------------------------------------------- ACP

/// The fake agent's side of an [`acp::Agent::over`] pipe pair.
struct FakeAgent {
    lines: tokio::io::Lines<tokio::io::BufReader<tokio::io::DuplexStream>>,
    out: tokio::io::DuplexStream,
}

impl FakeAgent {
    async fn recv(&mut self) -> Value {
        let line = self.lines.next_line().await.unwrap().unwrap();
        serde_json::from_str(&line).unwrap()
    }
    async fn send(&mut self, msg: Value) {
        self.out
            .write_all(format!("{msg}\n").as_bytes())
            .await
            .unwrap();
    }
}

/// An agent on `rt`, its session `s1` open, and the fake's end.
fn fake_agent(rt: &tokio::runtime::Runtime, root: &Path) -> (Arc<acp::Agent>, FakeAgent) {
    rt.block_on(async {
        let (client_out, agent_in) = tokio::io::duplex(1 << 16);
        let (agent_out, client_in) = tokio::io::duplex(1 << 16);
        let mut fake = FakeAgent {
            lines: tokio::io::BufReader::new(agent_in).lines(),
            out: agent_out,
        };
        let opening = tokio::spawn(async move {
            let init = fake.recv().await;
            fake.send(json!({"jsonrpc": "2.0", "id": init["id"], "result": {
                "protocolVersion": 1, "agentInfo": {"name": "fake", "title": "Fake Agent", "version": "1"}
            }}))
            .await;
            fake
        });
        let (agent, _events) = acp::Agent::over(client_in, client_out, root).await.unwrap();
        (Arc::new(agent), opening.await.unwrap())
    })
}

/// Put `agent` in the app as the project's agent, a turn on `stream` running.
fn running_turn(app: &mut App, agent: Arc<acp::Agent>) -> u64 {
    let stream = app.mint_request_id();
    app.proj.ask_turns.push(AskTurn {
        stream,
        question: "why?".into(),
        streaming: true,
        extras: TurnExtras {
            agent: Some("Fake Agent".into()),
            ..TurnExtras::default()
        },
        ..AskTurn::default()
    });
    let mut link = AcpLink::new("fake".into(), "Fake Agent".into());
    link.agent = Some(agent);
    link.session = Some("s1".into());
    link.turn = Some(stream);
    app.proj.acp = Some(link);
    stream
}

#[test]
fn an_acp_turn_shows_its_steps_plan_and_answer_and_asks_for_permission() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut app = scanned_app("acp-turn");
    let root = app.proj.project.as_ref().unwrap().root.clone();
    let (agent, mut fake) = fake_agent(&rt, &root);
    let stream = running_turn(&mut app, agent);
    assert!(app.ask_stream_active(), "the turn holds the gate");
    let event = |app: &mut App, event: acp::AgentEvent| {
        let stamp = app.stamp();
        let _ = app.update(Message::Ask(AskMsg::AcpEvent { stamp, event }));
    };
    let session = || "s1".to_string();
    event(
        &mut app,
        acp::AgentEvent::Plan {
            session: session(),
            entries: vec![acp::PlanEntry {
                content: "read lib.rs".into(),
                status: "in_progress".into(),
            }],
        },
    );
    event(
        &mut app,
        acp::AgentEvent::ToolCall {
            session: session(),
            call: acp::ToolCallInfo {
                id: "t1".into(),
                title: Some("Read lib.rs".into()),
                kind: Some("read".into()),
                status: Some(acp::ToolStatus::InProgress),
                locations: Some(vec![(
                    root.canonicalize().unwrap().join("src/lib.rs"),
                    Some(3),
                )]),
                summary: None,
            },
        },
    );
    event(
        &mut app,
        acp::AgentEvent::ToolCallUpdate {
            session: session(),
            call: acp::ToolCallInfo {
                id: "t1".into(),
                status: Some(acp::ToolStatus::Completed),
                summary: Some("3 lines".into()),
                ..acp::ToolCallInfo::default()
            },
        },
    );
    event(
        &mut app,
        acp::AgentEvent::Thought {
            session: session(),
            text: "hmm".into(),
        },
    );
    event(
        &mut app,
        acp::AgentEvent::Permission(acp::PermissionRequest {
            id: 7,
            session: session(),
            tool: acp::ToolCallInfo {
                id: "t2".into(),
                title: Some("edit lib.rs".into()),
                ..acp::ToolCallInfo::default()
            },
            options: vec![
                acp::PermissionOption {
                    id: "ok".into(),
                    name: "Allow".into(),
                    kind: "allow_once".into(),
                },
                acp::PermissionOption {
                    id: "no".into(),
                    name: "Reject".into(),
                    kind: "reject_once".into(),
                },
            ],
        }),
    );
    event(
        &mut app,
        acp::AgentEvent::Text {
            session: session(),
            text: "Because.".into(),
        },
    );
    let turn = app.proj.ask_turns.last().unwrap();
    assert_eq!(turn.extras.plan[0].content, "read lib.rs");
    let step = &turn.steps[0];
    assert_eq!(step.tool, "read");
    assert_eq!(step.state, StepState::Done);
    assert_eq!(step.title, "Read lib.rs — 3 lines");
    assert_eq!(step.refs, [("src/lib.rs".to_string(), Some(3))]);
    assert_eq!(turn.extras.thought, "hmm");
    assert_eq!(turn.answer_md, "Because.");
    let approval = &turn.extras.approvals[0];
    assert_eq!(approval.key, ApprovalKey::Acp { id: 7 });
    assert_eq!(approval.options.iter().filter(|o| o.allows).count(), 1);

    // The user's answer reaches the agent: the request it answers is the
    // agent's own, open in the client.
    rt.block_on(fake.send(json!({"jsonrpc": "2.0", "id": "perm-1", "method": "session/request_permission",
        "params": {"sessionId": "s1", "toolCall": {"toolCallId": "t2"}, "options": [{"optionId": "ok", "name": "Allow", "kind": "allow_once"}]}})));
    std::thread::sleep(std::time::Duration::from_millis(100));
    let _ = app.update(Message::Ask(AskMsg::Approve {
        stream,
        key: ApprovalKey::Acp { id: 1 },
        option: "ok".into(),
    }));
    let answer = rt.block_on(fake.recv());
    assert_eq!(answer["id"], "perm-1");
    assert_eq!(answer["result"]["outcome"]["optionId"], "ok");

    // The turn ends.
    let _ = app.update(Message::Ask(AskMsg::AcpTurnEnded {
        stamp: app.stamp(),
        stream,
        result: Ok(acp::StopReason::EndTurn),
    }));
    let turn = app.proj.ask_turns.last().unwrap();
    assert!(!turn.streaming);
    assert!(turn.extras.approvals.is_empty() && turn.extras.thought.is_empty());
    assert!(!app.ask_stream_active());
}

#[test]
fn stopping_an_acp_turn_cancels_it_on_the_agent() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut app = scanned_app("acp-stop");
    let root = app.proj.project.as_ref().unwrap().root.clone();
    let (agent, mut fake) = fake_agent(&rt, &root);
    let stream = running_turn(&mut app, agent);
    let _ = app.update(Message::Ask(AskMsg::Stop));
    let cancel = rt.block_on(fake.recv());
    assert_eq!(cancel["method"], "session/cancel");
    assert_eq!(cancel["params"]["sessionId"], "s1");
    // The turn closes at once; the next question waits for the agent to end
    // the stopped one.
    assert_eq!(app.status, "Answer stopped");
    assert!(!app.proj.ask_turns.last().unwrap().streaming);
    assert!(!app.ask_stream_active());
    app.acp_agents = vec![acp::AgentConfig {
        name: "fake".into(),
        launch: clew_core::linerpc::Launch::default(),
        share_mcp: false,
    }];
    let _ = app.start_acp_ask("fake".into(), "again?".into());
    let link = app.proj.acp.as_ref().unwrap();
    assert!(link.pending.is_some() && link.turn.is_none(), "it waits");
    let next = app.update(Message::Ask(AskMsg::AcpTurnEnded {
        stamp: app.stamp(),
        stream,
        result: Ok(acp::StopReason::Cancelled),
    }));
    drop(next);
    let link = app.proj.acp.as_ref().unwrap();
    assert!(
        link.pending.is_none() && link.turn.is_some(),
        "and then goes"
    );
}

#[test]
fn ending_an_agent_mid_turn_closes_the_turn_and_old_cards_answer_nothing() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut app = scanned_app("acp-end");
    let root = app.proj.project.as_ref().unwrap().root.clone();
    let (agent, mut fake) = fake_agent(&rt, &root);
    let stream = running_turn(&mut app, agent);
    // The agent asks; its card shows.
    rt.block_on(fake.send(json!({"jsonrpc": "2.0", "id": "perm-1", "method": "session/request_permission",
        "params": {"sessionId": "s1", "toolCall": {"toolCallId": "t1"}, "options": [{"optionId": "ok", "name": "Allow", "kind": "allow_once"}]}})));
    std::thread::sleep(std::time::Duration::from_millis(100));
    let stamp = app.stamp();
    let _ = app.update(Message::Ask(AskMsg::AcpEvent {
        stamp,
        event: acp::AgentEvent::Permission(acp::PermissionRequest {
            id: 1,
            session: "s1".into(),
            tool: acp::ToolCallInfo {
                id: "t1".into(),
                ..acp::ToolCallInfo::default()
            },
            options: vec![acp::PermissionOption {
                id: "ok".into(),
                name: "Allow".into(),
                kind: "allow_once".into(),
            }],
        }),
    }));
    // A card of a turn that is not the agent's running one answers nothing.
    app.proj.acp.as_mut().unwrap().turn = Some(stream + 1000);
    let _ = app.update(Message::Ask(AskMsg::Approve {
        stream,
        key: ApprovalKey::Acp { id: 1 },
        option: "ok".into(),
    }));
    let quiet = rt.block_on(async {
        tokio::time::timeout(std::time::Duration::from_millis(200), fake.recv()).await
    });
    assert!(quiet.is_err(), "nothing was answered: {quiet:?}");
    // Ending the agent (removed, or restarted for its MCP servers) closes its
    // turn and its cards, as Stop does.
    app.proj.acp.as_mut().unwrap().turn = Some(stream);
    app.proj
        .ask_turns
        .last_mut()
        .unwrap()
        .extras
        .approvals
        .push(crate::app::model::Approval {
            key: ApprovalKey::Acp { id: 1 },
            title: "edit".into(),
            detail: String::new(),
            options: Vec::new(),
        });
    let _ = app.end_acp_agent();
    let cancel = rt.block_on(fake.recv());
    assert_eq!(cancel["method"], "session/cancel");
    assert!(app.proj.acp.is_none());
    let turn = app.proj.ask_turns.last().unwrap();
    assert!(!turn.streaming && turn.extras.approvals.is_empty());
    assert!(!app.ask_stream_active());
}

#[test]
fn an_acp_sessions_settings_follow_the_agent() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut app = scanned_app("acp-options");
    let root = app.proj.project.as_ref().unwrap().root.clone();
    let (agent, _fake) = fake_agent(&rt, &root);
    running_turn(&mut app, agent);
    let mode = |current: &str| acp::ConfigOption {
        id: acp::MODE_OPTION.into(),
        name: "Mode".into(),
        category: Some("mode".into()),
        current: current.into(),
        choices: vec![("ask".into(), "Ask".into()), ("code".into(), "Code".into())],
    };
    // The agent says the session's mode changed.
    let stamp = app.stamp();
    let _ = app.update(Message::Ask(AskMsg::AcpEvent {
        stamp,
        event: acp::AgentEvent::Options {
            session: "s1".into(),
            options: vec![mode("code")],
        },
    }));
    assert_eq!(app.proj.acp.as_ref().unwrap().options, [mode("code")]);
    // Another session's settings are not this one's.
    let stamp = app.stamp();
    let _ = app.update(Message::Ask(AskMsg::AcpEvent {
        stamp,
        event: acp::AgentEvent::Options {
            session: "s2".into(),
            options: vec![mode("ask")],
        },
    }));
    assert_eq!(app.proj.acp.as_ref().unwrap().options, [mode("code")]);
    // A choice the agent took replaces them; one it refused says why.
    let stamp = app.stamp();
    let _ = app.update(Message::Ask(AskMsg::AcpOptionSet {
        stamp,
        result: Ok(vec![mode("ask")]),
    }));
    assert_eq!(app.proj.acp.as_ref().unwrap().options, [mode("ask")]);
    let stamp = app.stamp();
    let _ = app.update(Message::Ask(AskMsg::AcpOptionSet {
        stamp,
        result: Err("no such mode".into()),
    }));
    assert!(app.status.contains("no such mode"), "{}", app.status);
    assert_eq!(app.proj.acp.as_ref().unwrap().options, [mode("ask")]);
}

#[test]
fn an_agent_that_ignores_a_cancel_is_started_afresh_for_the_next_question() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut app = scanned_app("acp-overdue");
    let root = app.proj.project.as_ref().unwrap().root.clone();
    let (agent, mut fake) = fake_agent(&rt, &root);
    let stream = running_turn(&mut app, agent);
    let _ = app.update(Message::Ask(AskMsg::Stop));
    assert_eq!(rt.block_on(fake.recv())["method"], "session/cancel");
    app.acp_agents = vec![acp::AgentConfig {
        name: "fake".into(),
        launch: clew_core::linerpc::Launch::default(),
        share_mcp: false,
    }];
    let _ = app.start_acp_ask("fake".into(), "again?".into());
    let next = app.proj.acp.as_ref().unwrap().pending.as_ref().unwrap().0;
    // The agent never ends the stopped turn.
    let stamp = app.stamp();
    let _ = app.update(Message::Ask(AskMsg::AcpCancelOverdue { stamp, stream }));
    let link = app.proj.acp.as_ref().unwrap();
    assert!(
        link.agent.is_none() && link.cancelling.is_none(),
        "a new agent"
    );
    assert_eq!(
        link.pending.as_ref().map(|p| p.0),
        Some(next),
        "for the question"
    );
    assert!(app.status.contains("starting it again"), "{}", app.status);
    // A late notice of a turn already dealt with changes nothing.
    let stamp = app.stamp();
    let _ = app.update(Message::Ask(AskMsg::AcpCancelOverdue { stamp, stream }));
    assert_eq!(
        app.proj.acp.as_ref().unwrap().pending.as_ref().map(|p| p.0),
        Some(next)
    );
}

#[test]
fn an_acp_agent_is_asked_only_about_a_local_project_it_knows() {
    let mut app = scanned_app("acp-missing");
    app.ask_agent = Some("nobody".into());
    app.proj.ask_input = "why?".into();
    let _ = app.update(Message::Ask(AskMsg::Submit));
    assert!(
        app.status.contains("No ACP agent `nobody`"),
        "{}",
        app.status
    );
    assert_eq!(app.ask_agent, None, "the stale choice is dropped");
    assert!(app.proj.ask_turns.is_empty());
}

#[test]
fn the_settings_add_and_remove_servers_and_agents() {
    let data = test_dir("integrations-settings");
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();
    let _hold = data_dir_override(&data);
    let mut app = blank_app();
    let _ = app.update(Message::Settings(SettingsMsg::Open));
    let send = |app: &mut App, msg: IntegrationsMsg| {
        let _ = app.update(Message::Integrations(msg));
    };
    send(&mut app, IntegrationsMsg::McpName("gh".into()));
    send(
        &mut app,
        IntegrationsMsg::McpTarget("sh -c 'exit 0' \"two words\"".into()),
    );
    send(&mut app, IntegrationsMsg::McpExtra("TOKEN=abc".into()));
    send(&mut app, IntegrationsMsg::McpAdd);
    let draft = &app.settings.integrations;
    assert_eq!(draft.mcp.len(), 1, "{:?}", draft.note);
    let mcp::Transport::Stdio(launch) = &draft.mcp[0].transport else {
        panic!("stdio")
    };
    assert_eq!(launch.args, ["-c", "exit 0", "two words"]);
    assert_eq!(launch.env, [("TOKEN".to_string(), "abc".to_string())]);
    assert!(draft.new_mcp_target.is_empty(), "the form is cleared");

    send(
        &mut app,
        IntegrationsMsg::McpTarget(
            r#"{"mcpServers": {"web": {"url": "https://example.com/mcp"}}}"#.into(),
        ),
    );
    send(&mut app, IntegrationsMsg::McpAdd);
    assert_eq!(app.settings.integrations.mcp.len(), 2);
    send(
        &mut app,
        IntegrationsMsg::McpApprove("web".into(), mcp::Approve::ReadOnly),
    );
    send(&mut app, IntegrationsMsg::McpEnable("gh".into(), false));
    let (servers, _) = mcp::config::load();
    assert!(!servers.iter().find(|s| s.name == "gh").unwrap().enabled);
    assert_eq!(
        servers.iter().find(|s| s.name == "web").unwrap().approve,
        mcp::Approve::ReadOnly
    );
    send(&mut app, IntegrationsMsg::McpRemove("gh".into()));
    assert_eq!(app.settings.integrations.mcp.len(), 1);

    // Agents: a preset, then a custom one; the Ask panel's picker follows.
    send(&mut app, IntegrationsMsg::AcpPreset(0));
    send(&mut app, IntegrationsMsg::AcpName("mine".into()));
    send(&mut app, IntegrationsMsg::AcpCommand("sh --acp".into()));
    send(&mut app, IntegrationsMsg::AcpAdd);
    assert_eq!(
        app.acp_agents.len(),
        2,
        "{:?}",
        app.settings.integrations.note
    );
    let _ = app.update(Message::Ask(AskMsg::PickAgent(Some("mine".into()))));
    assert_eq!(
        crate::app::integrations::stored_ask_agent().as_deref(),
        Some("mine")
    );
    send(&mut app, IntegrationsMsg::AcpRemove("mine".into()));
    assert_eq!(app.acp_agents.len(), 1);
    assert_eq!(app.ask_agent, None, "a removed agent is no longer picked");

    // A form that cannot be saved says why.
    send(&mut app, IntegrationsMsg::McpName("bad name".into()));
    send(&mut app, IntegrationsMsg::McpTarget("sh".into()));
    send(&mut app, IntegrationsMsg::McpAdd);
    assert!(
        app.settings
            .integrations
            .note
            .as_deref()
            .is_some_and(|n| n.starts_with("Not saved")),
        "{:?}",
        app.settings.integrations.note
    );
}
