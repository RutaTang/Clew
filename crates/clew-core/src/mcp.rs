//! The Model Context Protocol: clew as an MCP CLIENT of the servers the user
//! configured ([`config`]), whose tools the Ask agent may call, and the
//! protocol half of clew's own MCP SERVER ([`serve`], run by `clew-server
//! mcp`), which gives other agents clew's code-intelligence tools.
//!
//! ## Eras
//!
//! MCP changed shape in revision `2026-07-28`: the `initialize` handshake is
//! gone, and every request carries its protocol version, the client's
//! identity and its capabilities in `_meta` instead ("modern"). Servers of
//! the earlier revisions ("legacy": `2025-11-25` back to `2024-11-05`) are
//! still the common case, so this client speaks both, as the revision's
//! compatibility rules prescribe:
//!
//! - **stdio** probes with `server/discover`. A `DiscoverResult` (or a
//!   recognized modern error, such as an unsupported version naming the ones
//!   the server does speak) marks a modern server; any other error, or no
//!   answer in time, a legacy one, which gets the `initialize` handshake.
//! - **Streamable HTTP** sends the same probe; a 4xx whose body is not a
//!   recognized modern error falls back to `initialize`, and on to that era's
//!   session (`Mcp-Session-Id`) and server-to-client requests.
//!
//! The era is a property of the server for the life of the connection.
//!
//! ## What a server is trusted with
//!
//! The servers are the user's own configuration — the user chose to run
//! them — but their tools act on the world (open issues, send messages,
//! delete things), and the model that calls them reads repository data an
//! attacker may have written. So a tool call runs only once the user allowed
//! it ([`config::Approve`]); this module only carries calls out. A server's
//! annotations (`readOnlyHint`) are its own claims, and only steer approval
//! when the user said to trust them. What a server returns is data: it is
//! flattened to text for the model ([`flatten_result`]), size-capped, and
//! nothing in it is executed.
//!
//! The deprecated HTTP+SSE transport of `2024-11-05` is not spoken; such a
//! server is named as one ([`McpError::OldSse`]).

pub mod config;
mod http;
pub mod hub;
pub mod oauth;
pub mod serve;

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use crate::framing::CallError;
use crate::linerpc::{self, CancelStyle, PeerMsg, RpcError, Session};

pub use config::{Approve, ServerConfig, Transport};

/// The modern revision this client speaks (and prefers).
pub const MODERN_VERSION: &str = "2026-07-28";

/// Every modern revision this client speaks, newest first.
pub const MODERN_VERSIONS: &[&str] = &[MODERN_VERSION];

/// The handshake-era revisions this client speaks, newest first.
pub const LEGACY_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// The header mirrors of a modern request did not match its body.
pub const HEADER_MISMATCH: i64 = -32020;
/// The request needed a client capability that was not declared.
pub const MISSING_REQUIRED_CLIENT_CAPABILITY: i64 = -32021;
/// The requested protocol version is not one the server speaks; `data`
/// lists those it does (`supported`).
pub const UNSUPPORTED_PROTOCOL_VERSION: i64 = -32022;

/// An error code only a modern server sends: what tells "modern, but not
/// this request" from "legacy" during the era probe.
pub fn is_modern_error(code: i64) -> bool {
    matches!(
        code,
        HEADER_MISMATCH | MISSING_REQUIRED_CLIENT_CAPABILITY | UNSUPPORTED_PROTOCOL_VERSION
    )
}

/// How long the era probe (`server/discover`) may go unanswered before the
/// server is taken for a legacy one. Generous: a server started through
/// `npx` or `uvx` may first download itself.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// How long connecting (the probe, the handshake, the first tool listing)
/// may take in all.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a `tools/list` page may take.
pub const LIST_TIMEOUT: Duration = Duration::from_secs(30);

/// Most `tools/list` pages read; a server that keeps handing out cursors is
/// cut off here, its tools so far kept.
const MAX_LIST_PAGES: usize = 50;

/// Most tools one server may offer the model. Past this the model's tool
/// list itself becomes the problem.
pub const MAX_TOOLS: usize = 200;

/// Most text one tool result hands the model; the agent caps it again.
pub const MAX_RESULT_CHARS: usize = 100_000;

/// How long a legacy server's tool listing is reused without a
/// `list_changed` notice, and a modern one's at most (its `ttlMs` may say
/// less).
const TOOLS_FRESH_FOR: Duration = Duration::from_secs(300);

/// Most input-required rounds one request may go through (MRTR) before it is
/// given up: a server that keeps asking to be retried is not converging.
const MAX_RETRY_ROUNDS: usize = 3;

/// This client's name and version as servers are told them.
pub fn client_info() -> Value {
    json!({
        "name": "clew",
        "title": "Clew",
        "version": client_version(),
    })
}

static CLIENT_VERSION: OnceLock<&'static str> = OnceLock::new();

/// Set the version servers are told this client is (the app's own, rather
/// than this library's). The first call wins.
pub fn set_client_version(version: &'static str) {
    let _ = CLIENT_VERSION.set(version);
}

fn client_version() -> &'static str {
    CLIENT_VERSION
        .get()
        .copied()
        .unwrap_or(env!("CARGO_PKG_VERSION"))
}

/// Which revision a connection speaks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Era {
    /// Per-request metadata, no handshake.
    Modern(String),
    /// Negotiated with `initialize`.
    Legacy(String),
}

impl Era {
    pub fn version(&self) -> &str {
        match self {
            Era::Modern(v) | Era::Legacy(v) => v,
        }
    }

    pub fn is_modern(&self) -> bool {
        matches!(self, Era::Modern(_))
    }
}

/// What a server said about itself. Self-reported: for display only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
    /// Guidance for the model on using the server, if it gave any.
    pub instructions: Option<String>,
    pub era: Era,
}

/// One tool a server offers.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolInfo {
    pub name: String,
    /// A display name, if the server gave one (`title`, else the
    /// annotations' `title`).
    pub title: Option<String>,
    pub description: String,
    /// The arguments' JSON Schema, an object schema.
    pub input_schema: Value,
    /// The server's own claims about the tool ([`ToolInfo::read_only`] is
    /// what the "trust read-only tools" approval policy reads).
    pub read_only: bool,
    pub destructive: bool,
    pub open_world: bool,
    /// Arguments mirrored into `Mcp-Param-*` headers on Streamable HTTP:
    /// each property path and its header name.
    pub header_params: Vec<(Vec<String>, String)>,
}

impl ToolInfo {
    /// What to call the tool in front of a person.
    pub fn display_name(&self) -> &str {
        self.title.as_deref().unwrap_or(&self.name)
    }
}

/// A tool call's result, as the model is shown it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    pub text: String,
    /// The tool reported a failure (`isError`): the model sees the text as
    /// the error it is.
    pub is_error: bool,
}

/// Why talking to a server failed.
#[derive(Debug, Clone, PartialEq)]
pub enum McpError {
    /// The configuration cannot be used as it is.
    Config(String),
    /// The server could not be started or reached, or the connection broke.
    Transport(String),
    /// The server did not answer in time.
    TimedOut,
    /// The server answered with a JSON-RPC error.
    Rpc {
        code: i64,
        message: String,
        data: Option<Value>,
    },
    /// The server answered with an HTTP error status and no JSON-RPC error.
    Http { status: u16, body: String },
    /// The server wants credentials (HTTP 401/403). `sign_in`: it says how
    /// to get them (an OAuth challenge), so the user can sign in to it
    /// ([`oauth::sign_in`]) rather than configure a header.
    Unauthorized {
        status: u16,
        detail: String,
        sign_in: bool,
    },
    /// The server's answer is not what the protocol says it sends.
    Protocol(String),
    /// The server only speaks the deprecated HTTP+SSE transport.
    OldSse,
    /// The request was given up on (its caller went away).
    Cancelled,
}

impl std::fmt::Display for McpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            McpError::Config(why) => write!(f, "{why}"),
            McpError::Transport(why) => write!(f, "{why}"),
            McpError::TimedOut => write!(f, "the server did not answer in time"),
            McpError::Rpc { code, message, .. } => write!(f, "{message} (error {code})"),
            McpError::Http { status, body } if body.is_empty() => {
                write!(f, "the server answered HTTP {status}")
            }
            McpError::Http { status, body } => {
                write!(f, "the server answered HTTP {status}: {body}")
            }
            McpError::Unauthorized {
                status,
                sign_in: true,
                ..
            } => write!(
                f,
                "the server needs you to sign in (HTTP {status}) — Settings → MCP servers → Sign in"
            ),
            McpError::Unauthorized { status, detail, .. } => write!(
                f,
                "the server refused the request (HTTP {status}) — it needs credentials: \
                 add them as a header (e.g. Authorization) in the server's settings{}",
                if detail.is_empty() {
                    String::new()
                } else {
                    format!(" ({detail})")
                }
            ),
            McpError::Protocol(why) => write!(f, "the server's answer is not valid MCP: {why}"),
            McpError::OldSse => write!(
                f,
                "this server speaks only the old HTTP+SSE transport (MCP 2024-11-05), which clew \
                 does not — use its Streamable HTTP endpoint, or run it over stdio"
            ),
            McpError::Cancelled => write!(f, "cancelled"),
        }
    }
}

impl std::error::Error for McpError {}

impl From<CallError> for McpError {
    fn from(e: CallError) -> Self {
        match e {
            CallError::TimedOut => McpError::TimedOut,
            CallError::NotRunning | CallError::Closed => {
                McpError::Transport("the server is not running".into())
            }
            CallError::Failed(f) => match f.code {
                Some(code) => McpError::Rpc {
                    code,
                    message: f.message,
                    data: f.data,
                },
                None => McpError::Transport(format!("the server {}", f.message)),
            },
        }
    }
}

/// `params` with the modern per-request metadata merged into its `_meta`.
pub fn with_meta(params: Value, version: &str) -> Value {
    let mut params = match params {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    let meta = params
        .entry("_meta")
        .or_insert_with(|| Value::Object(Map::new()));
    if !meta.is_object() {
        *meta = Value::Object(Map::new());
    }
    let meta = meta.as_object_mut().expect("an object");
    meta.insert(
        "io.modelcontextprotocol/protocolVersion".into(),
        version.into(),
    );
    meta.insert("io.modelcontextprotocol/clientInfo".into(), client_info());
    meta.insert(
        "io.modelcontextprotocol/clientCapabilities".into(),
        json!({}),
    );
    Value::Object(params)
}

/// The modern version to speak with a server that supports `supported`, if
/// any.
fn pick_modern(supported: &[String]) -> Option<String> {
    MODERN_VERSIONS
        .iter()
        .find(|v| supported.iter().any(|s| s == *v))
        .map(|v| v.to_string())
}

/// The legacy version to offer a server that supports `supported` (empty:
/// unknown, so the newest).
fn pick_legacy(supported: &[String]) -> Option<String> {
    if supported.is_empty() {
        return Some(LEGACY_VERSIONS[0].to_string());
    }
    LEGACY_VERSIONS
        .iter()
        .find(|v| supported.iter().any(|s| s == *v))
        .map(|v| v.to_string())
}

/// The versions an unsupported-version error says the server speaks.
fn supported_in(data: Option<&Value>) -> Vec<String> {
    data.and_then(|d| d.get("supported"))
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// What the era probe found.
pub(crate) enum Probe {
    /// A modern server: the version to speak, and its discover result if it
    /// gave one.
    Modern(String, Option<Value>),
    /// A legacy server: the version to offer in `initialize`.
    Legacy(String),
}

/// Read the era probe's outcome: `Ok` is the server's answer to
/// `server/discover`, `Err` its error (code, message, data) or `None` when it
/// gave no recognizable answer at all (an HTTP error without a JSON-RPC body,
/// silence).
pub(crate) fn read_probe(outcome: Result<Value, Option<RpcError>>) -> Result<Probe, McpError> {
    match outcome {
        Ok(result) => {
            let supported: Vec<String> = result
                .get("supportedVersions")
                .and_then(Value::as_array)
                .map(|l| {
                    l.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            if let Some(version) = pick_modern(&supported) {
                return Ok(Probe::Modern(version, Some(result)));
            }
            // A dual-era server that speaks only older revisions with us.
            match pick_legacy(&supported).filter(|_| !supported.is_empty()) {
                Some(version) => Ok(Probe::Legacy(version)),
                None => Err(McpError::Protocol(format!(
                    "the server speaks MCP {}, and clew speaks {} and {}",
                    supported.join(", "),
                    MODERN_VERSIONS.join(", "),
                    LEGACY_VERSIONS.join(", ")
                ))),
            }
        }
        Err(Some(err)) if err.code == UNSUPPORTED_PROTOCOL_VERSION => {
            let supported = supported_in(err.data.as_ref());
            if let Some(version) = pick_modern(&supported) {
                return Ok(Probe::Modern(version, None));
            }
            match pick_legacy(&supported).filter(|_| !supported.is_empty()) {
                Some(version) => Ok(Probe::Legacy(version)),
                None => Err(McpError::Protocol(format!(
                    "the server speaks MCP {}, which clew does not",
                    if supported.is_empty() {
                        "versions it did not name".to_string()
                    } else {
                        supported.join(", ")
                    }
                ))),
            }
        }
        Err(Some(err)) if is_modern_error(err.code) => {
            Ok(Probe::Modern(MODERN_VERSION.to_string(), None))
        }
        // Any other error, or none: a legacy server.
        Err(_) => Ok(Probe::Legacy(LEGACY_VERSIONS[0].to_string())),
    }
}

/// The `initialize` request of the legacy handshake.
pub(crate) fn initialize_params(version: &str) -> Value {
    json!({
        "protocolVersion": version,
        "capabilities": {},
        "clientInfo": client_info(),
    })
}

/// Read a legacy server's `initialize` answer.
pub(crate) fn read_initialize(result: &Value) -> Result<ServerInfo, McpError> {
    let version = result
        .get("protocolVersion")
        .and_then(Value::as_str)
        .ok_or_else(|| McpError::Protocol("initialize named no protocolVersion".into()))?;
    if !LEGACY_VERSIONS.contains(&version) {
        return Err(McpError::Protocol(format!(
            "the server chose MCP {version}, which clew does not speak"
        )));
    }
    let info = result.get("serverInfo");
    Ok(ServerInfo {
        name: str_at(info, "name").unwrap_or_default(),
        version: str_at(info, "version").unwrap_or_default(),
        instructions: str_at(Some(result), "instructions"),
        era: Era::Legacy(version.to_string()),
    })
}

/// Read a modern server's identity from its discover result (when the probe
/// got one).
fn modern_info(version: String, discover: Option<&Value>) -> ServerInfo {
    let info = discover
        .and_then(|d| d.get("_meta"))
        .and_then(|m| m.get("io.modelcontextprotocol/serverInfo"));
    ServerInfo {
        name: str_at(info, "name").unwrap_or_default(),
        version: str_at(info, "version").unwrap_or_default(),
        instructions: str_at(discover, "instructions"),
        era: Era::Modern(version),
    }
}

fn str_at(v: Option<&Value>, key: &str) -> Option<String> {
    v?.get(key)?
        .as_str()
        .map(str::to_owned)
        .filter(|s| !s.is_empty())
}

/// The tools of one `tools/list` page, each parsed ([`parse_tool`]); a tool
/// that does not parse is left out and named in `skipped`.
fn parse_tools(page: &Value, http: bool, skipped: &mut Vec<String>) -> Vec<ToolInfo> {
    let Some(tools) = page.get("tools").and_then(Value::as_array) else {
        return Vec::new();
    };
    tools
        .iter()
        .filter_map(|tool| match parse_tool(tool, http) {
            Ok(tool) => Some(tool),
            Err(why) => {
                skipped.push(why);
                None
            }
        })
        .collect()
}

/// One tool definition. `http` applies the Streamable HTTP rule that a tool
/// whose `x-mcp-header` annotations break the rules is left out.
pub fn parse_tool(tool: &Value, http: bool) -> Result<ToolInfo, String> {
    let name = tool
        .get("name")
        .and_then(Value::as_str)
        .filter(|n| !n.is_empty())
        .ok_or("a tool without a name")?
        .to_string();
    let annotations = tool.get("annotations");
    let hint = |key: &str| {
        annotations
            .and_then(|a| a.get(key))
            .and_then(Value::as_bool)
    };
    let mut input_schema = tool
        .get("inputSchema")
        .filter(|s| s.is_object())
        .cloned()
        .unwrap_or_else(|| json!({ "type": "object" }));
    if let Some(map) = input_schema.as_object_mut() {
        map.entry("type").or_insert_with(|| "object".into());
        // Several model APIs refuse an object schema without `properties`.
        map.entry("properties")
            .or_insert_with(|| Value::Object(Map::new()));
    }
    let header_params = if http {
        header_params(&input_schema).map_err(|why| format!("{name}: {why}"))?
    } else {
        Vec::new()
    };
    let read_only = hint("readOnlyHint").unwrap_or(false);
    Ok(ToolInfo {
        title: str_at(Some(tool), "title")
            .or_else(|| str_at(annotations, "title"))
            .filter(|t| *t != name),
        description: tool
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        input_schema,
        read_only,
        destructive: !read_only && hint("destructiveHint").unwrap_or(true),
        open_world: hint("openWorldHint").unwrap_or(true),
        header_params,
        name,
    })
}

/// The `x-mcp-header` annotations of an input schema, validated as the
/// Streamable HTTP transport requires: reached from the root through
/// `properties` alone, on primitive (string, integer, boolean) properties,
/// with token names unique regardless of case.
pub fn header_params(schema: &Value) -> Result<Vec<(Vec<String>, String)>, String> {
    fn walk(
        schema: &Value,
        path: &mut Vec<String>,
        out: &mut Vec<(Vec<String>, String)>,
    ) -> Result<(), String> {
        let Some(props) = schema.get("properties").and_then(Value::as_object) else {
            return Ok(());
        };
        for (key, prop) in props {
            path.push(key.clone());
            if let Some(header) = prop.get("x-mcp-header") {
                let header = header
                    .as_str()
                    .filter(|h| !h.is_empty() && h.bytes().all(is_tchar))
                    .ok_or_else(|| {
                        format!("x-mcp-header of `{}` is not a header name", path.join("."))
                    })?;
                let primitive = matches!(
                    prop.get("type").and_then(Value::as_str),
                    Some("string" | "integer" | "boolean")
                );
                if !primitive {
                    return Err(format!(
                        "x-mcp-header on `{}`, which is not a string, integer or boolean",
                        path.join(".")
                    ));
                }
                out.push((path.clone(), header.to_string()));
            }
            walk(prop, path, out)?;
            path.pop();
        }
        Ok(())
    }
    fn count(v: &Value) -> usize {
        match v {
            Value::Object(map) => {
                map.contains_key("x-mcp-header") as usize + map.values().map(count).sum::<usize>()
            }
            Value::Array(items) => items.iter().map(count).sum(),
            _ => 0,
        }
    }
    let mut out = Vec::new();
    walk(schema, &mut Vec::new(), &mut out)?;
    if count(schema) != out.len() {
        return Err("an x-mcp-header outside the schema's plain properties".into());
    }
    let mut seen = HashSet::new();
    for (_, header) in &out {
        if !seen.insert(header.to_ascii_lowercase()) {
            return Err(format!("x-mcp-header `{header}` appears twice"));
        }
    }
    Ok(out)
}

/// An RFC 9110 `tchar`.
fn is_tchar(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

/// A header value as the Streamable HTTP transport encodes one: as it is
/// when it is plain visible ASCII without edge whitespace, else Base64 in the
/// `=?base64?…?=` sentinel (as is any value that looks like the sentinel).
pub fn header_value(value: &str) -> String {
    let plain = value
        .bytes()
        .all(|b| (0x20..=0x7e).contains(&b) || b == b'\t')
        && value.trim() == value
        && !(value.starts_with("=?base64?") && value.ends_with("?="));
    if plain {
        value.to_string()
    } else {
        use base64::Engine;
        format!(
            "=?base64?{}?=",
            base64::engine::general_purpose::STANDARD.encode(value.as_bytes())
        )
    }
}

/// The `Mcp-Param-*` headers a call's arguments carry for `tool`.
pub fn param_headers(tool: &ToolInfo, args: &Value) -> Vec<(String, String)> {
    tool.header_params
        .iter()
        .filter_map(|(path, header)| {
            let mut v = args;
            for key in path {
                v = v.get(key)?;
            }
            let text = match v {
                Value::String(s) => s.clone(),
                Value::Bool(b) => b.to_string(),
                Value::Number(n) if n.is_i64() || n.is_u64() => n.to_string(),
                _ => return None,
            };
            Some((format!("Mcp-Param-{header}"), header_value(&text)))
        })
        .collect()
}

/// A tool result as text for the model: its text blocks, and a line for each
/// block the model cannot see (images, audio, binary resources); the
/// structured result when there is no text; capped at [`MAX_RESULT_CHARS`].
pub fn flatten_result(result: &Value) -> ToolOutput {
    let mut parts: Vec<String> = Vec::new();
    if let Some(blocks) = result.get("content").and_then(Value::as_array) {
        for block in blocks {
            if let Some(part) = flatten_block(block) {
                parts.push(part);
            }
        }
    }
    let has_text = parts.iter().any(|p| !p.trim().is_empty());
    if !has_text && let Some(structured) = result.get("structuredContent") {
        parts.push(serde_json::to_string_pretty(structured).unwrap_or_default());
    }
    let mut text = parts.join("\n");
    if text.trim().is_empty() {
        text = "(the tool returned nothing)".into();
    }
    if text.chars().count() > MAX_RESULT_CHARS {
        text = text.chars().take(MAX_RESULT_CHARS).collect::<String>()
            + "\n… (the rest of the result was cut)";
    }
    ToolOutput {
        text,
        is_error: result.get("isError").and_then(Value::as_bool) == Some(true),
    }
}

/// One content block as text.
fn flatten_block(block: &Value) -> Option<String> {
    let kind = block.get("type").and_then(Value::as_str)?;
    let mime = || {
        block
            .get("mimeType")
            .and_then(Value::as_str)
            .unwrap_or("unknown type")
            .to_string()
    };
    let size = |field: &str| {
        block
            .get(field)
            .and_then(Value::as_str)
            .map_or(0, |d| d.len() * 3 / 4)
    };
    Some(match kind {
        "text" => block.get("text").and_then(Value::as_str)?.to_string(),
        "image" => format!(
            "[an image ({}, about {} bytes) — not shown]",
            mime(),
            size("data")
        ),
        "audio" => format!(
            "[audio ({}, about {} bytes) — not shown]",
            mime(),
            size("data")
        ),
        "resource_link" => {
            let uri = block.get("uri").and_then(Value::as_str).unwrap_or_default();
            let name = block.get("name").and_then(Value::as_str).unwrap_or(uri);
            let mut line = format!("[resource: {name} <{uri}>]");
            if let Some(desc) = block.get("description").and_then(Value::as_str) {
                line.push_str(&format!(" {desc}"));
            }
            line
        }
        "resource" => {
            let res = block.get("resource")?;
            let uri = res.get("uri").and_then(Value::as_str).unwrap_or_default();
            match res.get("text").and_then(Value::as_str) {
                Some(text) => format!("--- {uri} ---\n{text}"),
                None => format!(
                    "[binary resource {uri} ({}) — not shown]",
                    res.get("mimeType")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown type")
                ),
            }
        }
        other => format!("[a `{other}` block — not shown]"),
    })
}

/// The name a server's tool goes by in front of the model:
/// `mcp__<server>__<tool>`, in the characters every model API takes, at most
/// 64 of them — a longer one is cut and given a hash of the whole, so two
/// tools never share one.
pub fn model_tool_name(server: &str, tool: &str) -> String {
    let clean = |s: &str| {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect::<String>()
    };
    let full = format!("mcp__{}__{}", clean(server), clean(tool));
    if full.len() <= 64 {
        return full;
    }
    use sha2::Digest;
    let digest = sha2::Sha256::digest(format!("{server}\u{0}{tool}").as_bytes());
    let tag: String = digest[..4].iter().map(|b| format!("{b:02x}")).collect();
    format!("{}_{tag}", &full[..64 - 9])
}

/// The link under a client: a process, or an HTTP endpoint.
enum Link {
    Stdio(Session),
    Http(Box<http::HttpLink>),
}

/// A connection to one MCP server.
pub struct Client {
    name: String,
    link: Link,
    info: ServerInfo,
    /// Raised by a `notifications/tools/list_changed` from the server.
    stale: Arc<AtomicBool>,
    /// The last tool listing and how long it is fresh for.
    tools: Mutex<Option<(Instant, Arc<Vec<ToolInfo>>)>>,
    /// Tools the last listing left out, and why.
    skipped: Mutex<Vec<String>>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("name", &self.name)
            .field("era", &self.info.era)
            .finish()
    }
}

impl Client {
    /// Start (or reach) the server `config` names and agree on a protocol
    /// revision with it. Must run inside a tokio runtime.
    pub async fn connect(config: &ServerConfig) -> Result<Client, McpError> {
        let stale = Arc::new(AtomicBool::new(false));
        let connect = async {
            match &config.transport {
                Transport::Stdio(launch) => connect_stdio(launch, stale.clone()).await,
                Transport::Http { url, headers } => {
                    let link = http::HttpLink::new(url, headers, stale.clone())?;
                    let info = link.open().await?;
                    Ok((Link::Http(Box::new(link)), info))
                }
            }
        };
        let (link, info) = tokio::time::timeout(CONNECT_TIMEOUT, connect)
            .await
            .map_err(|_| McpError::TimedOut)??;
        Ok(Client {
            name: config.name.clone(),
            link,
            info,
            stale,
            tools: Mutex::new(None),
            skipped: Mutex::new(Vec::new()),
        })
    }

    /// A client of the server whose output is `reader` and whose input is
    /// `writer` — a server this program did not start as a process (tests,
    /// an in-process server).
    pub async fn over<R, W>(name: &str, reader: R, writer: W) -> Result<Client, McpError>
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
        W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let stale = Arc::new(AtomicBool::new(false));
        let session = Session::over(reader, writer, stdio_options());
        let (link, info) =
            tokio::time::timeout(CONNECT_TIMEOUT, open_session(session, stale.clone()))
                .await
                .map_err(|_| McpError::TimedOut)??;
        Ok(Client {
            name: name.to_string(),
            link,
            info,
            stale,
            tools: Mutex::new(None),
            skipped: Mutex::new(Vec::new()),
        })
    }

    /// The configured name of the server.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// What the server said about itself.
    pub fn info(&self) -> &ServerInfo {
        &self.info
    }

    /// Whether the connection still works (a process that exited does not).
    pub fn alive(&self) -> bool {
        match &self.link {
            Link::Stdio(session) => session.alive(),
            Link::Http(_) => true,
        }
    }

    /// What the server process last wrote to stderr (stdio only).
    pub fn stderr_tail(&self) -> String {
        match &self.link {
            Link::Stdio(session) => session.stderr_tail(),
            Link::Http(_) => String::new(),
        }
    }

    /// Tools the last listing left out, and why.
    pub fn skipped_tools(&self) -> Vec<String> {
        self.skipped
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// One request, era-dressed, with the multi-round-trip pattern followed
    /// for the rounds a server may ask for that need nothing from us.
    async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
        tool: Option<&ToolInfo>,
    ) -> Result<Value, McpError> {
        let mut params = params;
        for _ in 0..=MAX_RETRY_ROUNDS {
            let dressed = match &self.info.era {
                Era::Modern(version) => with_meta(params.clone(), version),
                Era::Legacy(_) => params.clone(),
            };
            let result = match &self.link {
                Link::Stdio(session) => session.call(method, dressed, timeout).await?,
                Link::Http(link) => link.request(method, dressed, timeout, tool).await?,
            };
            match result.get("resultType").and_then(Value::as_str) {
                // Earlier revisions send no `resultType`: complete.
                None | Some("complete") => return Ok(result),
                Some("input_required") => {
                    let asked: Vec<String> = result
                        .get("inputRequests")
                        .and_then(Value::as_object)
                        .map(|reqs| {
                            reqs.values()
                                .filter_map(|r| r.get("method").and_then(Value::as_str))
                                .map(str::to_owned)
                                .collect()
                        })
                        .unwrap_or_default();
                    if !asked.is_empty() {
                        // Nothing was declared that would let a server ask
                        // (no roots, sampling or elicitation capability).
                        return Err(McpError::Protocol(format!(
                            "the server asked for input clew does not give ({})",
                            asked.join(", ")
                        )));
                    }
                    let Some(state) = result.get("requestState").cloned() else {
                        return Err(McpError::Protocol(
                            "an input-required result asking for nothing".into(),
                        ));
                    };
                    if let Some(map) = params.as_object_mut() {
                        map.insert("requestState".into(), state);
                    }
                }
                Some(other) => {
                    return Err(McpError::Protocol(format!("a `{other}` result")));
                }
            }
        }
        Err(McpError::Protocol(
            "the server kept asking for the request to be retried".into(),
        ))
    }

    /// The server's tools: the last listing while it is fresh, else a new
    /// one (every page of it).
    pub async fn tools(&self) -> Result<Arc<Vec<ToolInfo>>, McpError> {
        if !self.stale.swap(false, Ordering::Relaxed)
            && let Some((fresh_until, tools)) = self
                .tools
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .as_ref()
            && Instant::now() < *fresh_until
        {
            return Ok(tools.clone());
        }
        let http = matches!(self.link, Link::Http(_));
        let mut tools: Vec<ToolInfo> = Vec::new();
        let mut skipped = Vec::new();
        let mut cursor: Option<String> = None;
        let mut fresh_for = TOOLS_FRESH_FOR;
        for _ in 0..MAX_LIST_PAGES {
            let params = match &cursor {
                Some(c) => json!({ "cursor": c }),
                None => json!({}),
            };
            let page = self
                .request("tools/list", params, LIST_TIMEOUT, None)
                .await?;
            if let Some(ttl) = page.get("ttlMs").and_then(Value::as_u64) {
                fresh_for = fresh_for.min(Duration::from_millis(ttl));
            }
            tools.extend(parse_tools(&page, http, &mut skipped));
            cursor = page
                .get("nextCursor")
                .and_then(Value::as_str)
                .map(str::to_owned);
            if cursor.is_none() || tools.len() >= MAX_TOOLS {
                break;
            }
        }
        let mut seen = HashSet::new();
        tools.retain(|t| seen.insert(t.name.clone()));
        if tools.len() > MAX_TOOLS {
            skipped.push(format!(
                "{} more tools past the first {MAX_TOOLS}",
                tools.len() - MAX_TOOLS
            ));
            tools.truncate(MAX_TOOLS);
        }
        let tools = Arc::new(tools);
        *self.tools.lock().unwrap_or_else(PoisonError::into_inner) =
            Some((Instant::now() + fresh_for, tools.clone()));
        *self.skipped.lock().unwrap_or_else(PoisonError::into_inner) = skipped;
        Ok(tools)
    }

    /// Call `tool` with `args` (a JSON object), for at most `timeout`.
    /// Dropping the future cancels the call on the server.
    pub async fn call_tool(
        &self,
        tool: &ToolInfo,
        args: Value,
        timeout: Duration,
    ) -> Result<ToolOutput, McpError> {
        let args = if args.is_object() { args } else { json!({}) };
        let params = json!({ "name": tool.name, "arguments": args });
        let result = self
            .request("tools/call", params, timeout, Some(tool))
            .await?;
        Ok(flatten_result(&result))
    }

    /// End the connection: the process's input closed (then the process
    /// killed, after a grace), or the HTTP session deleted.
    pub async fn close(&self) {
        match &self.link {
            Link::Stdio(session) => session.stop().await,
            Link::Http(link) => link.close().await,
        }
    }
}

/// How a stdio session treats the server: `ping` answered on the spot, an
/// abandoned call cancelled with `notifications/cancelled`.
fn stdio_options() -> linerpc::Options {
    let quick: linerpc::Quick = Box::new(|method, _| match method {
        "ping" => Some(Ok(json!({}))),
        _ => None,
    });
    linerpc::Options {
        quick,
        cancel: Some(CancelStyle::Mcp),
    }
}

/// Start a stdio server, probe its era, and complete the legacy handshake if
/// it needs one.
async fn connect_stdio(
    launch: &linerpc::Launch,
    stale: Arc<AtomicBool>,
) -> Result<(Link, ServerInfo), McpError> {
    let session = Session::spawn(launch, stdio_options()).map_err(McpError::Transport)?;
    open_session(session, stale).await
}

/// Probe the era of the server on `session`, and complete the legacy
/// handshake if it needs one.
async fn open_session(
    session: Session,
    stale: Arc<AtomicBool>,
) -> Result<(Link, ServerInfo), McpError> {
    let peer = session.take_peer().expect("a fresh session");
    let died = |session: &Session, e: McpError| match session.stderr_tail() {
        tail if tail.is_empty() || session.alive() => e,
        tail => McpError::Transport(format!("{e}; it said:\n{tail}")),
    };
    let probe = session
        .call(
            "server/discover",
            with_meta(json!({}), MODERN_VERSION),
            PROBE_TIMEOUT,
        )
        .await;
    let probe = match probe {
        Ok(result) => Ok(result),
        Err(CallError::Failed(f)) => Err(f.code.map(|code| RpcError {
            code,
            message: f.message,
            data: f.data,
        })),
        Err(CallError::TimedOut) => Err(None),
        Err(e) => return Err(died(&session, McpError::from(e))),
    };
    let info = match read_probe(probe)? {
        Probe::Modern(version, discover) => modern_info(version, discover.as_ref()),
        Probe::Legacy(version) => {
            let result = session
                .call("initialize", initialize_params(&version), PROBE_TIMEOUT)
                .await
                .map_err(|e| died(&session, McpError::from(e)))?;
            let info = read_initialize(&result)?;
            session.notify("notifications/initialized", Value::Null);
            info
        }
    };
    tokio::spawn(serve_peer(peer, stale, session_responder(&session)));
    Ok((Link::Stdio(session), info))
}

/// How the peer task answers the server's own requests: by writing to the
/// session (a send to an ended session is dropped).
fn session_responder(
    session: &Session,
) -> impl Fn(&Value, Result<Value, RpcError>) + Send + 'static {
    let handle = session.handle();
    move |id, result| handle.send_raw(linerpc::response(id, result))
}

/// The server's own messages on a stdio connection: a tool list that
/// changed marks the listing stale; every request (other than the `ping`
/// answered inside the session) is refused — this client declared no
/// capability a server could ask of it.
async fn serve_peer(
    mut peer: tokio::sync::mpsc::UnboundedReceiver<PeerMsg>,
    stale: Arc<AtomicBool>,
    respond: impl Fn(&Value, Result<Value, RpcError>),
) {
    while let Some(msg) = peer.recv().await {
        match msg {
            PeerMsg::Notification { method, .. } => {
                if method == "notifications/tools/list_changed" {
                    stale.store(true, Ordering::Relaxed);
                }
            }
            PeerMsg::Request { id, method, .. } => {
                respond(&id, Err(RpcError::method_not_found(&method)));
            }
        }
    }
}

#[cfg(test)]
mod tests;
