use super::*;
use std::io::{BufRead, BufReader as StdBufReader, Read as _, Write as _};
use std::net::TcpListener;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

// ------------------------------------------------------------- pure parts

#[test]
fn the_probe_tells_eras_apart() {
    let modern = read_probe(Ok(
        json!({"supportedVersions": ["2026-07-28", "2025-11-25"]}),
    ));
    assert!(matches!(modern, Ok(Probe::Modern(v, Some(_))) if v == MODERN_VERSION));
    // A dual-era server speaking only older revisions.
    let older = read_probe(Ok(json!({"supportedVersions": ["2025-06-18"]})));
    assert!(matches!(older, Ok(Probe::Legacy(v)) if v == "2025-06-18"));
    // Modern, and says which versions it does speak.
    let unsupported = read_probe(Err(Some(RpcError {
        code: UNSUPPORTED_PROTOCOL_VERSION,
        message: "no".into(),
        data: Some(json!({"supported": ["2025-11-25"]})),
    })));
    assert!(matches!(unsupported, Ok(Probe::Legacy(v)) if v == "2025-11-25"));
    let none = read_probe(Err(Some(RpcError {
        code: UNSUPPORTED_PROTOCOL_VERSION,
        message: "no".into(),
        data: Some(json!({"supported": ["2099-01-01"]})),
    })));
    assert!(none.is_err());
    // Any other error, or none at all: legacy.
    assert!(matches!(
        read_probe(Err(Some(RpcError::method_not_found("server/discover")))),
        Ok(Probe::Legacy(_))
    ));
    assert!(matches!(read_probe(Err(None)), Ok(Probe::Legacy(_))));
}

#[test]
fn modern_requests_carry_their_meta() {
    let params = with_meta(
        json!({"name": "t", "_meta": {"progressToken": 1}}),
        MODERN_VERSION,
    );
    let meta = &params["_meta"];
    assert_eq!(
        meta["io.modelcontextprotocol/protocolVersion"],
        MODERN_VERSION
    );
    assert_eq!(meta["io.modelcontextprotocol/clientInfo"]["name"], "clew");
    assert_eq!(
        meta["io.modelcontextprotocol/clientCapabilities"],
        json!({})
    );
    assert_eq!(meta["progressToken"], 1, "the caller's own meta is kept");
    assert_eq!(params["name"], "t");
    assert!(with_meta(Value::Null, MODERN_VERSION)["_meta"].is_object());
}

#[test]
fn tools_parse_with_their_hints_and_safe_schemas() {
    let tool = parse_tool(
        &json!({"name": "rm", "description": "deletes", "annotations": {"title": "Remove"}}),
        false,
    )
    .unwrap();
    assert_eq!(tool.display_name(), "Remove");
    assert!(!tool.read_only && tool.destructive && tool.open_world);
    assert_eq!(
        tool.input_schema,
        json!({"type": "object", "properties": {}})
    );
    let ro = parse_tool(
        &json!({"name": "ls", "annotations": {"readOnlyHint": true, "destructiveHint": true}}),
        false,
    )
    .unwrap();
    assert!(
        ro.read_only && !ro.destructive,
        "destructive means nothing when read-only"
    );
    assert!(parse_tool(&json!({"description": "no name"}), false).is_err());
}

#[test]
fn header_params_follow_the_transport_rules() {
    let ok = json!({"type": "object", "properties": {
        "region": {"type": "string", "x-mcp-header": "Region"},
        "nested": {"type": "object", "properties": {"n": {"type": "integer", "x-mcp-header": "N"}}},
        "q": {"type": "string"}
    }});
    let mut found = header_params(&ok).unwrap();
    found.sort();
    assert_eq!(
        found,
        vec![
            (vec!["nested".to_string(), "n".to_string()], "N".to_string()),
            (vec!["region".to_string()], "Region".to_string()),
        ]
    );
    for bad in [
        json!({"properties": {"x": {"type": "number", "x-mcp-header": "X"}}}),
        json!({"properties": {"x": {"type": "string", "x-mcp-header": "bad name"}}}),
        json!({"properties": {"a": {"type": "string", "x-mcp-header": "H"}, "b": {"type": "string", "x-mcp-header": "h"}}}),
        json!({"properties": {"a": {"type": "array", "items": {"type": "string", "x-mcp-header": "H"}}}}),
        json!({"oneOf": [{"properties": {"a": {"type": "string", "x-mcp-header": "H"}}}]}),
    ] {
        assert!(header_params(&bad).is_err(), "{bad}");
    }
    // Over HTTP a tool with a bad annotation is left out; over stdio it is ignored.
    let tool = json!({"name": "t", "inputSchema": {"type": "object", "properties": {"x": {"type": "number", "x-mcp-header": "X"}}}});
    assert!(parse_tool(&tool, true).is_err());
    assert!(parse_tool(&tool, false).is_ok());
}

#[test]
fn header_values_are_encoded_when_they_must_be() {
    assert_eq!(header_value("us-west1"), "us-west1");
    assert_eq!(
        header_value("Hello, 世界"),
        "=?base64?SGVsbG8sIOS4lueVjA==?="
    );
    assert_eq!(header_value(" padded "), "=?base64?IHBhZGRlZCA=?=");
    assert_eq!(header_value("line1\nline2"), "=?base64?bGluZTEKbGluZTI=?=");
    assert_eq!(
        header_value("=?base64?literal?="),
        "=?base64?PT9iYXNlNjQ/bGl0ZXJhbD89?="
    );
    let tool = parse_tool(
        &json!({"name": "q", "inputSchema": {"type": "object", "properties": {
            "region": {"type": "string", "x-mcp-header": "Region"},
            "dry": {"type": "boolean", "x-mcp-header": "Dry"}
        }}}),
        true,
    )
    .unwrap();
    let mut headers = param_headers(&tool, &json!({"region": "eu", "dry": true}));
    headers.sort();
    assert_eq!(
        headers,
        vec![
            ("Mcp-Param-Dry".to_string(), "true".to_string()),
            ("Mcp-Param-Region".to_string(), "eu".to_string()),
        ]
    );
    assert!(param_headers(&tool, &json!({"region": null})).is_empty());
}

#[test]
fn results_flatten_to_text_the_model_can_read() {
    let out = flatten_result(&json!({"content": [
        {"type": "text", "text": "one"},
        {"type": "image", "data": "AAAA", "mimeType": "image/png"},
        {"type": "resource_link", "uri": "file:///a", "name": "a", "description": "the a"},
        {"type": "resource", "resource": {"uri": "file:///b", "text": "bee"}},
        {"type": "resource", "resource": {"uri": "file:///c", "blob": "AA==", "mimeType": "x/y"}},
        {"type": "mystery"}
    ], "isError": true}));
    assert!(out.is_error);
    for want in [
        "one",
        "[an image (image/png",
        "[resource: a <file:///a>] the a",
        "--- file:///b ---\nbee",
        "binary resource file:///c (x/y)",
        "`mystery`",
    ] {
        assert!(out.text.contains(want), "{want} in {}", out.text);
    }
    let structured = flatten_result(&json!({"content": [], "structuredContent": {"t": 1}}));
    assert!(structured.text.contains("\"t\": 1"));
    assert!(!structured.is_error);
    assert_eq!(
        flatten_result(&json!({})).text,
        "(the tool returned nothing)"
    );
    let big = flatten_result(
        &json!({"content": [{"type": "text", "text": "x".repeat(MAX_RESULT_CHARS + 10)}]}),
    );
    assert!(big.text.ends_with("(the rest of the result was cut)"));
}

#[test]
fn model_tool_names_are_short_safe_and_distinct() {
    assert_eq!(
        model_tool_name("git hub", "list.issues"),
        "mcp__git_hub__list_issues"
    );
    let a = model_tool_name("server", &"a".repeat(80));
    let b = model_tool_name("server", &format!("{}b", "a".repeat(79)));
    assert!(a.len() <= 64 && b.len() <= 64);
    assert_ne!(a, b);
    assert!(
        a.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    );
}

// ------------------------------------------------------------ fake servers

/// A legacy (2025-06-18) stdio server, in the shape the reference SDKs
/// answer: unknown methods before `initialize` are refused, tools come in two
/// pages, and the server pings the client and announces a list change.
async fn legacy_stdio_server(reader: tokio::io::DuplexStream, mut writer: tokio::io::DuplexStream) {
    let mut lines = BufReader::new(reader).lines();
    let mut pinged = false;
    while let Ok(Some(line)) = lines.next_line().await {
        let msg: Value = serde_json::from_str(&line).unwrap();
        let id = msg.get("id").cloned();
        let method = msg
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let reply = match (id, method.as_str()) {
            (Some(id), "initialize") => {
                assert_eq!(msg["params"]["protocolVersion"], LEGACY_VERSIONS[0]);
                json!({"jsonrpc": "2.0", "id": id, "result": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {"tools": {"listChanged": true}},
                    "serverInfo": {"name": "legacy-fake", "version": "9"},
                    "instructions": "use me"
                }})
            }
            (Some(id), "tools/list") => {
                assert!(
                    msg["params"].get("_meta").is_none(),
                    "no modern meta for a legacy server"
                );
                if msg["params"]["cursor"] == "p2" {
                    json!({"jsonrpc": "2.0", "id": id, "result": {"tools": [
                        {"name": "second", "annotations": {"readOnlyHint": true}}
                    ]}})
                } else {
                    json!({"jsonrpc": "2.0", "id": id, "result": {"tools": [
                        {"name": "first", "description": "one", "inputSchema": {"type": "object", "properties": {"x": {"type": "string"}}}}
                    ], "nextCursor": "p2"}})
                }
            }
            (Some(id), "tools/call") => {
                if !pinged {
                    pinged = true;
                    let ping = json!({"jsonrpc": "2.0", "id": "srv-1", "method": "ping"});
                    writer
                        .write_all(format!("{ping}\n").as_bytes())
                        .await
                        .unwrap();
                    let answer: Value =
                        serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
                    assert_eq!(
                        answer,
                        json!({"jsonrpc": "2.0", "id": "srv-1", "result": {}})
                    );
                    let changed =
                        json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"});
                    writer
                        .write_all(format!("{changed}\n").as_bytes())
                        .await
                        .unwrap();
                }
                let x = msg["params"]["arguments"]["x"]
                    .as_str()
                    .unwrap_or("?")
                    .to_string();
                json!({"jsonrpc": "2.0", "id": id, "result": {
                    "content": [{"type": "text", "text": format!("got {x}")}]
                }})
            }
            (Some(id), other) => {
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("Method not found: {other}")}})
            }
            (None, _) => continue,
        };
        writer
            .write_all(format!("{reply}\n").as_bytes())
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn a_legacy_stdio_server_is_initialized_listed_and_called() {
    let (client_out, server_in) = tokio::io::duplex(1 << 16);
    let (server_out, client_in) = tokio::io::duplex(1 << 16);
    tokio::spawn(legacy_stdio_server(server_in, server_out));
    let client = Client::over("fake", client_in, client_out).await.unwrap();
    assert_eq!(client.info().era, Era::Legacy("2025-06-18".into()));
    assert_eq!(client.info().name, "legacy-fake");
    assert_eq!(client.info().instructions.as_deref(), Some("use me"));
    let tools = client.tools().await.unwrap();
    let names: Vec<_> = tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["first", "second"], "both pages");
    assert!(tools[1].read_only);
    let out = client
        .call_tool(&tools[0], json!({"x": "it"}), Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(
        out,
        ToolOutput {
            text: "got it".into(),
            is_error: false
        }
    );
    // The list_changed notice makes the next listing a fresh one.
    for _ in 0..100 {
        if client.stale.load(Ordering::Relaxed) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(client.stale.load(Ordering::Relaxed));
    assert_eq!(client.tools().await.unwrap().len(), 2);
    assert!(!client.stale.load(Ordering::Relaxed));
}

/// clew's own server, as a modern one would be reached.
struct Tiny;

impl serve::Host for Tiny {
    fn identity(&self) -> (String, String, Option<String>) {
        ("tiny".into(), "2".into(), None)
    }
    fn tools(&self) -> Vec<serve::Tool> {
        vec![serve::Tool {
            name: "shout".into(),
            title: None,
            description: "uppercases".into(),
            input_schema: json!({"type": "object", "properties": {"s": {"type": "string"}}}),
        }]
    }
    fn call(&self, _: &str, args: &Value, _: &AtomicBool) -> Result<ToolOutput, String> {
        Ok(ToolOutput {
            text: args["s"].as_str().unwrap_or_default().to_uppercase(),
            is_error: false,
        })
    }
}

#[tokio::test]
async fn clews_own_server_is_reached_as_a_modern_one() {
    let (client_out, server_in) = tokio::io::duplex(1 << 16);
    let (server_out, client_in) = tokio::io::duplex(1 << 16);
    tokio::spawn(serve::serve(Arc::new(Tiny), server_in, server_out));
    let client = Client::over("tiny", client_in, client_out).await.unwrap();
    assert_eq!(client.info().era, Era::Modern(MODERN_VERSION.into()));
    assert_eq!(client.info().name, "tiny");
    let tools = client.tools().await.unwrap();
    assert_eq!(tools[0].name, "shout");
    assert!(tools[0].read_only, "clew's tools say they are read-only");
    let out = client
        .call_tool(&tools[0], json!({"s": "hey"}), Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(out.text, "HEY");
}

// ------------------------------------------------------------- HTTP

/// One HTTP request as the fake server read it.
#[derive(Debug, Clone)]
struct Seen {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// A tiny HTTP server on loopback: `answer` turns each request into a status,
/// extra headers and a body. Returns its URL and what it saw.
fn http_server(
    answer: impl Fn(&Seen) -> Reply + Send + Sync + 'static,
) -> (String, Arc<Mutex<Vec<Seen>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!(
        "http://127.0.0.1:{}/mcp",
        listener.local_addr().unwrap().port()
    );
    let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
    let log = seen.clone();
    let answer = Arc::new(answer);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            let (log, answer) = (log.clone(), answer.clone());
            // A thread per connection: the client keeps connections open
            // and may open another while one is still streaming.
            std::thread::spawn(move || serve_connection(stream, &log, &*answer));
        }
    });
    (url, seen)
}

/// What the fake server answers: a status, extra headers, a body.
type Reply = (u16, Vec<(String, String)>, String);
type Answer = dyn Fn(&Seen) -> Reply + Send + Sync;

fn serve_connection(mut stream: std::net::TcpStream, log: &Mutex<Vec<Seen>>, answer: &Answer) {
    let mut reader = StdBufReader::new(stream.try_clone().unwrap());
    loop {
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
            return;
        }
        let method = request_line
            .split(' ')
            .next()
            .unwrap_or_default()
            .to_string();
        let path = request_line
            .split(' ')
            .nth(1)
            .unwrap_or_default()
            .to_string();
        let mut headers = Vec::new();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            let (k, v) = line.split_once(':').unwrap();
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
        let len: usize = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
            .map(|(_, v)| v.parse().unwrap())
            .unwrap_or(0);
        let mut body = vec![0; len];
        if reader.read_exact(&mut body).is_err() {
            return;
        }
        let seen = Seen {
            method,
            path,
            headers,
            body: String::from_utf8(body).unwrap(),
        };
        log.lock().unwrap().push(seen.clone());
        let (status, extra, body) = answer(&seen);
        let mut response = format!("HTTP/1.1 {status} X\r\nContent-Length: {}\r\n", body.len());
        for (k, v) in extra {
            response.push_str(&format!("{k}: {v}\r\n"));
        }
        response.push_str("\r\n");
        response.push_str(&body);
        if stream.write_all(response.as_bytes()).is_err() {
            return;
        }
    }
}

fn json_ct() -> Vec<(String, String)> {
    vec![("Content-Type".into(), "application/json".into())]
}

fn sse_ct() -> Vec<(String, String)> {
    vec![("Content-Type".into(), "text/event-stream".into())]
}

fn http_config(url: &str) -> ServerConfig {
    ServerConfig {
        name: "web".into(),
        transport: Transport::Http {
            url: url.into(),
            headers: vec![("Authorization".into(), "Bearer sekrit".into())],
        },
        enabled: true,
        approve: Approve::Ask,
        allow: Vec::new(),
        timeout: Duration::from_secs(5),
    }
}

#[tokio::test]
async fn a_modern_http_server_gets_mirrored_headers() {
    let (url, seen) = http_server(|req| {
        let msg: Value = serde_json::from_str(&req.body).unwrap();
        let id = msg["id"].clone();
        let result = match msg["method"].as_str().unwrap() {
            "server/discover" => {
                json!({"resultType": "complete", "supportedVersions": ["2026-07-28"],
                "capabilities": {"tools": {}}, "_meta": {"io.modelcontextprotocol/serverInfo": {"name": "modern-web", "version": "1"}}})
            }
            "tools/list" => {
                json!({"resultType": "complete", "ttlMs": 60000, "cacheScope": "private", "tools": [
                    {"name": "q", "inputSchema": {"type": "object", "properties": {"region": {"type": "string", "x-mcp-header": "Region"}}}},
                    {"name": "bad", "inputSchema": {"type": "object", "properties": {"n": {"type": "number", "x-mcp-header": "N"}}}}
                ]})
            }
            "tools/call" => {
                json!({"resultType": "complete", "content": [{"type": "text", "text": "done"}]})
            }
            other => panic!("unexpected {other}"),
        };
        // The call answers as a stream: a progress notification, then the response.
        if msg["method"] == "tools/call" {
            let note = json!({"jsonrpc": "2.0", "method": "notifications/progress", "params": {"progress": 1}});
            let resp = json!({"jsonrpc": "2.0", "id": id, "result": result});
            return (
                200,
                sse_ct(),
                format!(": hi\n\ndata: {note}\n\nevent: message\ndata: {resp}\n\n"),
            );
        }
        (
            200,
            json_ct(),
            json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string(),
        )
    });
    let client = Client::connect(&http_config(&url)).await.unwrap();
    assert_eq!(client.info().era, Era::Modern(MODERN_VERSION.into()));
    assert_eq!(client.info().name, "modern-web");
    let tools = client.tools().await.unwrap();
    assert_eq!(
        tools.len(),
        1,
        "the tool with a bad x-mcp-header is left out"
    );
    assert_eq!(client.skipped_tools().len(), 1);
    let out = client
        .call_tool(&tools[0], json!({"region": "eu-1"}), Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(out.text, "done");
    let seen = seen.lock().unwrap().clone();
    let call = seen.iter().find(|s| s.body.contains("tools/call")).unwrap();
    assert_eq!(call.header("MCP-Protocol-Version"), Some(MODERN_VERSION));
    assert_eq!(call.header("Mcp-Method"), Some("tools/call"));
    assert_eq!(call.header("Mcp-Name"), Some("q"));
    assert_eq!(call.header("Mcp-Param-Region"), Some("eu-1"));
    assert_eq!(call.header("Authorization"), Some("Bearer sekrit"));
    assert!(call.header("Mcp-Session-Id").is_none());
    let accept = call.header("Accept").unwrap();
    assert!(accept.contains("application/json") && accept.contains("text/event-stream"));
}

#[tokio::test]
async fn a_legacy_http_server_gets_its_session_and_its_requests_answered() {
    let sessions = Arc::new(Mutex::new(0u32));
    let counter = sessions.clone();
    let (url, seen) = http_server(move |req| {
        if req.method == "DELETE" {
            return (200, vec![], String::new());
        }
        let msg: Value = serde_json::from_str(&req.body).unwrap();
        let method = msg
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let session = req.header("Mcp-Session-Id").map(str::to_owned);
        if msg.get("method").is_none() {
            // Our answer to the server's ping.
            assert_eq!(msg["result"], json!({}));
            return (202, vec![], String::new());
        }
        match method {
            "initialize" => {
                let mut n = counter.lock().unwrap();
                *n += 1;
                let mut headers = json_ct();
                headers.push(("Mcp-Session-Id".into(), format!("s{n}")));
                let body = json!({"jsonrpc": "2.0", "id": msg["id"], "result": {
                    "protocolVersion": "2025-11-25", "capabilities": {}, "serverInfo": {"name": "legacy-web", "version": "1"}}});
                (200, headers, body.to_string())
            }
            "notifications/initialized" => (202, vec![], String::new()),
            _ if session.is_none() => (
                400,
                json_ct(),
                json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32000, "message": "Bad Request: No valid session ID provided"}}).to_string(),
            ),
            // The first session is forgotten after the listing.
            "tools/call" if session.as_deref() == Some("s1") => (404, vec![], String::new()),
            "tools/list" => (
                200,
                json_ct(),
                json!({"jsonrpc": "2.0", "id": msg["id"], "result": {"tools": [{"name": "t"}]}}).to_string(),
            ),
            "tools/call" => {
                let ping = json!({"jsonrpc": "2.0", "id": 77, "method": "ping"});
                let resp = json!({"jsonrpc": "2.0", "id": msg["id"], "result": {"content": [{"type": "text", "text": "ok"}]}});
                (200, sse_ct(), format!("data: {ping}\n\ndata: {resp}\n\n"))
            }
            other => panic!("unexpected {other}"),
        }
    });
    let client = Client::connect(&http_config(&url)).await.unwrap();
    assert_eq!(client.info().era, Era::Legacy("2025-11-25".into()));
    let tools = client.tools().await.unwrap();
    let out = client
        .call_tool(&tools[0], json!({}), Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(out.text, "ok");
    assert_eq!(
        *sessions.lock().unwrap(),
        2,
        "the forgotten session was opened again"
    );
    client.close().await;
    let seen = seen.lock().unwrap().clone();
    let list = seen.iter().find(|s| s.body.contains("tools/list")).unwrap();
    assert_eq!(list.header("Mcp-Session-Id"), Some("s1"));
    assert_eq!(list.header("MCP-Protocol-Version"), Some("2025-11-25"));
    assert!(
        list.header("Mcp-Method").is_none(),
        "no modern mirrors for a legacy server"
    );
    assert!(
        seen.iter().any(|s| s.body.contains("\"id\":77")),
        "the server's ping was answered"
    );
    let delete = seen.iter().find(|s| s.method == "DELETE").unwrap();
    assert_eq!(delete.header("Mcp-Session-Id"), Some("s2"));
}

#[tokio::test]
async fn an_http_server_that_wants_credentials_says_so() {
    let (url, _) = http_server(|_| {
        (
            401,
            vec![("WWW-Authenticate".into(), "Bearer realm=\"mcp\"".into())],
            String::new(),
        )
    });
    let err = Client::connect(&http_config(&url)).await.unwrap_err();
    assert!(
        matches!(err, McpError::Unauthorized { status: 401, .. }),
        "{err:?}"
    );
    assert!(err.to_string().contains("needs credentials"));
}

#[tokio::test]
async fn an_old_http_sse_server_is_named_as_one() {
    let (url, _) = http_server(|req| {
        if req.method == "GET" {
            return (
                200,
                sse_ct(),
                "event: endpoint\ndata: /messages?session=1\n\n".into(),
            );
        }
        (405, vec![], String::new())
    });
    let err = Client::connect(&http_config(&url)).await.unwrap_err();
    assert_eq!(err, McpError::OldSse);
}

// ------------------------------------------------------------- OAuth

/// The fake world of a sign-in: an MCP server that wants a token, its
/// protected resource metadata, and an authorization server, on one
/// loopback port. `iss` is what the authorization server's redirect says
/// it is.
fn oauth_world() -> (String, Arc<Mutex<Option<String>>>) {
    let base: Arc<std::sync::OnceLock<String>> = Arc::default();
    let challenge: Arc<Mutex<Option<String>>> = Arc::default();
    let (b, ch) = (base.clone(), challenge.clone());
    let (url, _) = http_server(move |req| {
        let base = b.get().unwrap().clone();
        let json = |v: Value| (200, json_ct(), v.to_string());
        match (req.method.as_str(), req.path.as_str()) {
            ("POST", "/mcp") => {
                let auth = req.header("Authorization").unwrap_or_default();
                if auth != "Bearer at-1" && auth != "Bearer at-2" {
                    let www = format!(
                        "Bearer resource_metadata=\"{base}/.well-known/oauth-protected-resource/mcp\", scope=\"read\""
                    );
                    return (401, vec![("WWW-Authenticate".into(), www)], String::new());
                }
                let msg: Value = serde_json::from_str(&req.body).unwrap();
                let result = match msg["method"].as_str().unwrap() {
                    "server/discover" => {
                        json!({"resultType": "complete", "supportedVersions": ["2026-07-28"], "capabilities": {}})
                    }
                    _ => {
                        json!({"resultType": "complete", "tools": [], "ttlMs": 0, "cacheScope": "private"})
                    }
                };
                json(json!({"jsonrpc": "2.0", "id": msg["id"], "result": result}))
            }
            ("GET", "/.well-known/oauth-protected-resource/mcp") => json(json!({
                "resource": format!("{base}/mcp"),
                "authorization_servers": [format!("{base}/auth")],
            })),
            ("GET", "/.well-known/oauth-authorization-server/auth") => json(json!({
                "issuer": format!("{base}/auth"),
                "authorization_endpoint": format!("{base}/auth/authorize"),
                "token_endpoint": format!("{base}/auth/token"),
                "registration_endpoint": format!("{base}/auth/register"),
                "code_challenge_methods_supported": ["S256"],
                "authorization_response_iss_parameter_supported": true,
            })),
            ("POST", "/auth/register") => {
                let reg: Value = serde_json::from_str(&req.body).unwrap();
                assert_eq!(reg["application_type"], "native");
                assert_eq!(reg["token_endpoint_auth_method"], "none");
                assert!(
                    reg["redirect_uris"][0]
                        .as_str()
                        .unwrap()
                        .starts_with("http://127.0.0.1:")
                );
                json(json!({"client_id": "cid"}))
            }
            ("POST", "/auth/token") => {
                let form: std::collections::HashMap<String, String> =
                    url::form_urlencoded::parse(req.body.as_bytes())
                        .into_owned()
                        .collect();
                assert_eq!(form["client_id"], "cid");
                assert_eq!(form["resource"], format!("{base}/mcp"));
                match form["grant_type"].as_str() {
                    "authorization_code" => {
                        assert_eq!(form["code"], "the-code");
                        let expected = ch.lock().unwrap().clone().unwrap();
                        assert_eq!(oauth::pkce_challenge(&form["code_verifier"]), expected);
                        json(
                            json!({"access_token": "at-1", "token_type": "Bearer", "expires_in": 3600, "refresh_token": "rt-1"}),
                        )
                    }
                    "refresh_token" => {
                        assert_eq!(form["refresh_token"], "rt-1");
                        json(
                            json!({"access_token": "at-2", "token_type": "bearer", "expires_in": 3600}),
                        )
                    }
                    other => panic!("grant {other}"),
                }
            }
            (m, p) => (404, vec![], format!("no {m} {p}")),
        }
    });
    let root = url.trim_end_matches("/mcp").to_string();
    base.set(root.clone()).unwrap();
    (root, challenge)
}

/// A browser that approves at once: it checks the authorization request and
/// follows its redirect back, saying it came from `iss`.
fn approving_browser(
    challenge: Arc<Mutex<Option<String>>>,
    iss: String,
) -> impl FnOnce(&str) -> Result<(), String> {
    move |auth_url: &str| {
        let parsed = url::Url::parse(auth_url).unwrap();
        let q: std::collections::HashMap<String, String> =
            parsed.query_pairs().into_owned().collect();
        assert_eq!(q["response_type"], "code");
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["scope"], "read", "the challenge's scope");
        assert!(q["resource"].ends_with("/mcp"));
        *challenge.lock().unwrap() = Some(q["code_challenge"].clone());
        let redirect = url::Url::parse(&q["redirect_uri"]).unwrap();
        let target = format!(
            "{}?code=the-code&state={}&iss={}",
            redirect.path(),
            q["state"],
            url::form_urlencoded::byte_serialize(iss.as_bytes()).collect::<String>()
        );
        let port = redirect.port().unwrap();
        std::thread::spawn(move || {
            let mut conn = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
            write!(conn, "GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").unwrap();
            let mut answer = String::new();
            let _ = conn.read_to_string(&mut answer);
            assert!(answer.contains("Signed in"), "{answer}");
        });
        Ok(())
    }
}

#[test]
fn signing_in_gets_a_token_the_server_takes_and_renews_it() {
    let _data = crate::testutil::DataDir::new("mcp-oauth");
    let (base, challenge) = oauth_world();
    let url = format!("{base}/mcp");
    let config = ServerConfig {
        transport: Transport::Http {
            url: url.clone(),
            headers: Vec::new(),
        },
        ..http_config(&url)
    };
    let rt = tokio::runtime::Runtime::new().unwrap();
    // Not signed in: the server's 401 says a sign-in is the way.
    let err = rt.block_on(Client::connect(&config)).unwrap_err();
    assert!(
        matches!(err, McpError::Unauthorized { sign_in: true, .. }),
        "{err:?}"
    );
    assert!(err.to_string().contains("Sign in"));

    oauth::sign_in(
        &url,
        None,
        approving_browser(challenge, format!("{base}/auth")),
    )
    .unwrap();
    assert!(oauth::signed_in(&url));
    let client = rt.block_on(Client::connect(&config)).unwrap();
    assert!(client.info().era.is_modern());

    // An expired token is renewed with the refresh token, which is kept.
    oauth::edit_store_for_tests(&url, |s| s.expires_at = Some(1)).unwrap();
    assert_eq!(oauth::bearer(&url).as_deref(), Some("at-2"));
    assert_eq!(
        oauth::session(&url).unwrap().refresh_token.as_deref(),
        Some("rt-1")
    );
    rt.block_on(Client::connect(&config)).unwrap();
    oauth::sign_out(&url).unwrap();
    assert!(!oauth::signed_in(&url));
}

#[test]
fn a_sign_in_answered_by_another_issuer_is_refused() {
    let _data = crate::testutil::DataDir::new("mcp-oauth-iss");
    let (base, challenge) = oauth_world();
    let url = format!("{base}/mcp");
    let err = oauth::sign_in(
        &url,
        None,
        approving_browser(challenge, "https://evil.example".into()),
    )
    .unwrap_err();
    assert!(err.contains("another authorization server"), "{err}");
    assert!(!oauth::signed_in(&url));
}
