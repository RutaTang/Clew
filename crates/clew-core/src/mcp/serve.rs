//! The protocol half of an MCP server on stdio: what `clew-server mcp` runs
//! to give another agent (Claude Code, an editor's assistant, an ACP agent
//! clew started) clew's code-intelligence tools. The tools themselves come
//! from a [`Host`].
//!
//! Dual-era, as the `2026-07-28` revision describes a server that keeps
//! serving older clients: a request carrying the modern per-request `_meta`
//! is served statelessly (an unknown version is refused with the versions
//! this server speaks), `server/discover` says what it speaks, and an
//! `initialize` selects the legacy handshake for the rest of the process.
//!
//! Every tool is read-only — this server answers questions about code — and
//! says so in its annotations. A call runs on a blocking thread with a stop
//! flag the client's `notifications/cancelled` raises.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use super::{LEGACY_VERSIONS, MODERN_VERSIONS, ToolOutput, UNSUPPORTED_PROTOCOL_VERSION};
use crate::linerpc::{RpcError, response};

/// One tool the host offers.
#[derive(Debug, Clone, PartialEq)]
pub struct Tool {
    pub name: String,
    pub title: Option<String>,
    pub description: String,
    pub input_schema: Value,
}

/// What a server serves.
pub trait Host: Send + Sync + 'static {
    /// The server's name and version, and its guidance for the model.
    fn identity(&self) -> (String, String, Option<String>);
    /// The tools, in a fixed order.
    fn tools(&self) -> Vec<Tool>;
    /// Run tool `name` on `args`, blocking; `stop` is raised when the client
    /// gives the call up. `Err` is a call the protocol refuses (no such
    /// tool); a tool's own failure is an `Ok` output with `is_error` set.
    fn call(&self, name: &str, args: &Value, stop: &AtomicBool) -> Result<ToolOutput, String>;
}

/// How long a modern client may cache the tool list, in milliseconds.
const TOOLS_TTL_MS: u64 = 60 * 60 * 1000;

/// The requests being worked on, by the client's id (as JSON text), with
/// their stop flags.
type InFlight = Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>;

/// Serve `host` over `reader`/`writer` until the client closes its end.
pub async fn serve<R, W>(host: Arc<dyn Host>, reader: R, writer: W)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Value>();
    let writer_task = tokio::spawn(async move {
        let mut writer = writer;
        while let Some(msg) = out_rx.recv().await {
            let mut line = msg.to_string().into_bytes();
            line.push(b'\n');
            if writer.write_all(&line).await.is_err() || writer.flush().await.is_err() {
                return;
            }
        }
    });
    let in_flight: InFlight = Arc::default();
    let mut legacy: Option<String> = None;
    let mut reader = BufReader::new(reader);
    let mut calls = Vec::new();
    loop {
        let line = match crate::framing::read_json_line(&mut reader).await {
            Ok(Some(line)) => line,
            Ok(None) | Err(_) => break,
        };
        let msg = match serde_json::from_slice::<Value>(&line) {
            Ok(msg) => msg,
            Err(e) => {
                let _ = out_tx.send(json!({
                    "jsonrpc": "2.0", "id": null,
                    "error": RpcError::new(crate::linerpc::PARSE_ERROR, format!("not JSON: {e}")).to_json(),
                }));
                continue;
            }
        };
        let messages = match msg {
            Value::Array(items) => items,
            msg => vec![msg],
        };
        for msg in messages {
            if let Some(call) = dispatch(&host, msg, &mut legacy, &in_flight, &out_tx) {
                calls.push(call);
            }
        }
        calls.retain(|c: &tokio::task::JoinHandle<()>| !c.is_finished());
    }
    // The client is gone: stop what it asked for, and let it wind down.
    for flag in in_flight
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .values()
    {
        flag.store(true, Ordering::Relaxed);
    }
    for call in calls {
        let _ = call.await;
    }
    drop(out_tx);
    let _ = writer_task.await;
}

/// Handle one message; a tool call is started on a task of its own, which
/// is returned.
fn dispatch(
    host: &Arc<dyn Host>,
    msg: Value,
    legacy: &mut Option<String>,
    in_flight: &InFlight,
    out: &mpsc::UnboundedSender<Value>,
) -> Option<tokio::task::JoinHandle<()>> {
    let method = msg.get("method").and_then(Value::as_str)?.to_string();
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let Some(id) = msg.get("id").filter(|v| !v.is_null()).cloned() else {
        if method == "notifications/cancelled"
            && let Some(request) = params.get("requestId")
            && let Some(flag) = in_flight
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(&request.to_string())
        {
            flag.store(true, Ordering::Relaxed);
        }
        return None;
    };
    let reply = |result: Result<Value, RpcError>| {
        let _ = out.send(response(&id, result));
    };
    // The modern per-request version, when the request carries one.
    let requested = params
        .get("_meta")
        .and_then(|m| m.get("io.modelcontextprotocol/protocolVersion"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let modern = requested.is_some();
    if let Some(version) = &requested
        && method != "server/discover"
        && !MODERN_VERSIONS.contains(&version.as_str())
        && !LEGACY_VERSIONS.contains(&version.as_str())
    {
        reply(Err(unsupported(version)));
        return None;
    }
    let (name, version, instructions) = host.identity();
    let complete = |mut result: Value| {
        if modern && let Some(map) = result.as_object_mut() {
            map.insert("resultType".into(), "complete".into());
            map.insert(
                "_meta".into(),
                json!({ "io.modelcontextprotocol/serverInfo": { "name": name, "version": version } }),
            );
        }
        result
    };
    match method.as_str() {
        "initialize" => {
            let asked = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let chosen = if LEGACY_VERSIONS.contains(&asked) {
                asked.to_string()
            } else {
                LEGACY_VERSIONS[0].to_string()
            };
            *legacy = Some(chosen.clone());
            let mut result = json!({
                "protocolVersion": chosen,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": name, "version": version },
            });
            if let Some(text) = &instructions {
                result["instructions"] = text.clone().into();
            }
            reply(Ok(result));
        }
        "ping" => reply(Ok(complete(json!({})))),
        "server/discover" => {
            let supported: Vec<&str> = MODERN_VERSIONS
                .iter()
                .chain(LEGACY_VERSIONS)
                .copied()
                .collect();
            let mut result = json!({
                "resultType": "complete",
                "supportedVersions": supported,
                "capabilities": { "tools": {} },
                "_meta": { "io.modelcontextprotocol/serverInfo": { "name": name, "version": version } },
                "ttlMs": TOOLS_TTL_MS,
                "cacheScope": "private",
            });
            if let Some(text) = &instructions {
                result["instructions"] = text.clone().into();
            }
            reply(Ok(result));
        }
        "tools/list" => {
            let tools: Vec<Value> = host.tools().iter().map(tool_json).collect();
            let mut result = json!({ "tools": tools });
            if modern {
                result["ttlMs"] = TOOLS_TTL_MS.into();
                result["cacheScope"] = "private".into();
            }
            reply(Ok(complete(result)));
        }
        "tools/call" => {
            let Some(tool) = params
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_owned)
            else {
                reply(Err(RpcError::invalid_params("tools/call needs a `name`")));
                return None;
            };
            if !host.tools().iter().any(|t| t.name == tool) {
                reply(Err(RpcError::invalid_params(format!(
                    "unknown tool: {tool}"
                ))));
                return None;
            }
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let stop = Arc::new(AtomicBool::new(false));
            let key = id.to_string();
            in_flight
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(key.clone(), stop.clone());
            let host = host.clone();
            let out = out.clone();
            let in_flight = in_flight.clone();
            return Some(tokio::spawn(async move {
                let flag = stop.clone();
                let ran = tokio::task::spawn_blocking(move || host.call(&tool, &args, &flag)).await;
                in_flight
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&key);
                // A cancelled request is owed no answer.
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                let result = match ran {
                    Ok(Ok(output)) => {
                        let mut result = json!({
                            "content": [{ "type": "text", "text": output.text }],
                            "isError": output.is_error,
                        });
                        if modern {
                            result["resultType"] = "complete".into();
                        }
                        Ok(result)
                    }
                    Ok(Err(why)) => Err(RpcError::invalid_params(why)),
                    Err(_) => Err(RpcError::internal("the tool failed unexpectedly")),
                };
                let _ = out.send(response(&id, result));
            }));
        }
        // Lists this server has nothing in.
        "resources/list" => reply(Ok(complete(json!({ "resources": [] })))),
        "resources/templates/list" => reply(Ok(complete(json!({ "resourceTemplates": [] })))),
        "prompts/list" => reply(Ok(complete(json!({ "prompts": [] })))),
        // Accepted and ignored: this server does not log to the client.
        "logging/setLevel" => reply(Ok(json!({}))),
        other => reply(Err(RpcError::method_not_found(other))),
    }
    None
}

/// The error for a request in a version this server does not speak.
fn unsupported(requested: &str) -> RpcError {
    let supported: Vec<&str> = MODERN_VERSIONS
        .iter()
        .chain(LEGACY_VERSIONS)
        .copied()
        .collect();
    RpcError {
        code: UNSUPPORTED_PROTOCOL_VERSION,
        message: "Unsupported protocol version".into(),
        data: Some(json!({ "supported": supported, "requested": requested })),
    }
}

/// A tool as `tools/list` lists it.
fn tool_json(tool: &Tool) -> Value {
    let mut t = json!({
        "name": tool.name,
        "description": tool.description,
        "inputSchema": tool.input_schema,
        "annotations": {
            "readOnlyHint": true,
            "destructiveHint": false,
            "idempotentHint": true,
            "openWorldHint": false,
        },
    });
    if let Some(title) = &tool.title {
        t["title"] = title.clone().into();
        t["annotations"]["title"] = title.clone().into();
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, DuplexStream};

    struct Echo;

    impl Host for Echo {
        fn identity(&self) -> (String, String, Option<String>) {
            ("echo".into(), "1.0".into(), Some("Say things back.".into()))
        }
        fn tools(&self) -> Vec<Tool> {
            vec![Tool {
                name: "echo".into(),
                title: Some("Echo".into()),
                description: "Says it back".into(),
                input_schema: json!({"type": "object", "properties": {"text": {"type": "string"}}}),
            }]
        }
        fn call(&self, name: &str, args: &Value, stop: &AtomicBool) -> Result<ToolOutput, String> {
            assert_eq!(name, "echo");
            if args["text"] == "wait" {
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
            Ok(ToolOutput {
                text: args["text"].as_str().unwrap_or_default().to_string(),
                is_error: args["text"] == "fail",
            })
        }
    }

    fn start() -> (DuplexStream, BufReader<DuplexStream>) {
        let (client_out, server_in) = tokio::io::duplex(1 << 16);
        let (server_out, client_in) = tokio::io::duplex(1 << 16);
        tokio::spawn(serve(Arc::new(Echo), server_in, server_out));
        (client_out, BufReader::new(client_in))
    }

    async fn ask(tx: &mut DuplexStream, rx: &mut BufReader<DuplexStream>, msg: Value) -> Value {
        tx.write_all(format!("{msg}\n").as_bytes()).await.unwrap();
        let mut line = String::new();
        rx.read_line(&mut line).await.unwrap();
        serde_json::from_str(&line).unwrap()
    }

    #[tokio::test]
    async fn a_legacy_client_initializes_lists_and_calls() {
        let (mut tx, mut rx) = start();
        let init = ask(
            &mut tx,
            &mut rx,
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                   "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}}),
        )
        .await;
        assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(init["result"]["serverInfo"]["name"], "echo");
        assert_eq!(init["result"]["instructions"], "Say things back.");
        tx.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
            .await
            .unwrap();
        let list = ask(
            &mut tx,
            &mut rx,
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
        )
        .await;
        assert_eq!(list["result"]["tools"][0]["name"], "echo");
        assert_eq!(
            list["result"]["tools"][0]["annotations"]["readOnlyHint"],
            true
        );
        assert!(list["result"].get("resultType").is_none());
        let call = ask(
            &mut tx,
            &mut rx,
            json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "echo", "arguments": {"text": "hi"}}}),
        )
        .await;
        assert_eq!(call["result"]["content"][0]["text"], "hi");
        assert_eq!(call["result"]["isError"], false);
        let unknown = ask(
            &mut tx,
            &mut rx,
            json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "nope"}}),
        )
        .await;
        assert_eq!(unknown["error"]["code"], crate::linerpc::INVALID_PARAMS);
        let missing = ask(
            &mut tx,
            &mut rx,
            json!({"jsonrpc": "2.0", "id": 5, "method": "sampling/x"}),
        )
        .await;
        assert_eq!(missing["error"]["code"], crate::linerpc::METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn a_modern_client_discovers_and_is_held_to_known_versions() {
        let (mut tx, mut rx) = start();
        let meta = |v: &str| json!({"io.modelcontextprotocol/protocolVersion": v, "io.modelcontextprotocol/clientCapabilities": {}});
        let disc = ask(
            &mut tx,
            &mut rx,
            json!({"jsonrpc": "2.0", "id": "d", "method": "server/discover", "params": {"_meta": meta("2026-07-28")}}),
        )
        .await;
        assert_eq!(disc["result"]["supportedVersions"][0], "2026-07-28");
        assert_eq!(disc["result"]["resultType"], "complete");
        let list = ask(
            &mut tx,
            &mut rx,
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {"_meta": meta("2026-07-28")}}),
        )
        .await;
        assert_eq!(list["result"]["resultType"], "complete");
        assert_eq!(list["result"]["cacheScope"], "private");
        let wrong = ask(
            &mut tx,
            &mut rx,
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {"_meta": meta("1900-01-01")}}),
        )
        .await;
        assert_eq!(wrong["error"]["code"], UNSUPPORTED_PROTOCOL_VERSION);
        assert_eq!(wrong["error"]["data"]["requested"], "1900-01-01");
    }

    #[tokio::test]
    async fn a_cancelled_call_is_stopped_and_not_answered() {
        let (mut tx, mut rx) = start();
        tx.write_all(
            format!(
                "{}\n{}\n",
                json!({"jsonrpc": "2.0", "id": 9, "method": "tools/call", "params": {"name": "echo", "arguments": {"text": "wait"}}}),
                json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 9}}),
            )
            .as_bytes(),
        )
        .await
        .unwrap();
        let next = ask(
            &mut tx,
            &mut rx,
            json!({"jsonrpc": "2.0", "id": 10, "method": "ping"}),
        )
        .await;
        assert_eq!(next["id"], 10, "the cancelled call got no answer: {next}");
    }
}
