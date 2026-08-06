//! Multi-provider LLM client + global config for the explain feature.
//!
//! The credential lives in clew's **global** config (`<data_root>/config.toml`),
//! not in a project's `.clew/` — it's a cross-project credential, like the shared
//! LSP binaries. When no key is configured the explain feature routes to the
//! in-app settings, and the rest of clew is unaffected (and fully offline).
//!
//! ```toml
//! [llm]
//! provider = "anthropic"   # anthropic | openai | deepseek | custom
//! api_key  = "sk-..."
//! model    = "claude-haiku-4-5-20251001"   # optional; provider default otherwise
//! base_url = "https://api.anthropic.com"   # optional; provider default otherwise
//! ```
//!
//! Anthropic talks to the Messages API; OpenAI/DeepSeek/custom talk to the
//! OpenAI-compatible `/chat/completions` API (custom lets you point `base_url` at
//! any compatible endpoint, e.g. a local server). A provider-specific env var
//! (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `DEEPSEEK_API_KEY`) overrides the file.

use std::fmt;
use std::io::Read;
use std::path::PathBuf;

const API_VERSION: &str = "2023-06-01"; // Anthropic

/// A completion provider. OpenAI and DeepSeek differ only in `base_url`/`model`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Anthropic,
    OpenAI,
    DeepSeek,
    Custom,
}

impl Provider {
    /// Every provider, in display order (drives the settings picker).
    pub const ALL: [Provider; 4] = [
        Provider::Anthropic,
        Provider::OpenAI,
        Provider::DeepSeek,
        Provider::Custom,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Provider::Anthropic => "Anthropic",
            Provider::OpenAI => "OpenAI",
            Provider::DeepSeek => "DeepSeek",
            Provider::Custom => "Custom (OpenAI-compatible)",
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            Provider::Anthropic => "anthropic",
            Provider::OpenAI => "openai",
            Provider::DeepSeek => "deepseek",
            Provider::Custom => "custom",
        }
    }

    pub fn from_slug(s: &str) -> Provider {
        match s.trim().to_ascii_lowercase().as_str() {
            "openai" => Provider::OpenAI,
            "deepseek" => Provider::DeepSeek,
            "custom" => Provider::Custom,
            _ => Provider::Anthropic,
        }
    }

    /// A sensible, inexpensive default model — most explanation nodes are small.
    pub fn default_model(self) -> &'static str {
        match self {
            Provider::Anthropic => "claude-haiku-4-5-20251001",
            Provider::OpenAI => "gpt-4o-mini",
            // DeepSeek retired the `deepseek-chat` alias; the API now accepts
            // only `deepseek-v4-pro` / `deepseek-v4-flash`. Flash is the cheap,
            // fast tier that suits bulk per-symbol explanation.
            Provider::DeepSeek => "deepseek-v4-flash",
            Provider::Custom => "",
        }
    }

    pub fn default_base_url(self) -> &'static str {
        match self {
            Provider::Anthropic => "https://api.anthropic.com",
            Provider::OpenAI => "https://api.openai.com/v1",
            Provider::DeepSeek => "https://api.deepseek.com/v1",
            Provider::Custom => "",
        }
    }

    fn env_key(self) -> &'static str {
        match self {
            Provider::Anthropic => "ANTHROPIC_API_KEY",
            Provider::OpenAI => "OPENAI_API_KEY",
            Provider::DeepSeek => "DEEPSEEK_API_KEY",
            Provider::Custom => "",
        }
    }
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Resolved LLM configuration.
#[derive(Debug, Clone)]
pub struct Config {
    pub provider: Provider,
    pub api_key: String,
    pub model: String,
    pub base_url: String,
}

impl Config {
    /// Fill blank model/base_url with the provider defaults.
    pub fn from_parts(
        provider: Provider,
        api_key: String,
        model: String,
        base_url: String,
    ) -> Config {
        let model = if model.trim().is_empty() {
            provider.default_model().to_string()
        } else {
            model.trim().to_string()
        };
        let base_url = if base_url.trim().is_empty() {
            provider.default_base_url().to_string()
        } else {
            base_url.trim().trim_end_matches('/').to_string()
        };
        Config {
            provider,
            api_key: api_key.trim().to_string(),
            model,
            base_url,
        }
    }
}

fn config_path() -> Option<PathBuf> {
    Some(crate::lsp::store::data_root()?.join("config.toml"))
}

impl Config {
    /// The stored settings regardless of whether a key is present — used to
    /// pre-fill the settings form. Defaults to Anthropic with an empty key.
    pub fn current_or_default() -> Config {
        let table: Option<toml::Value> = config_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| toml::from_str(&t).ok());
        let llm = table.as_ref().and_then(|t| t.get("llm"));
        let str_field = |k: &str| {
            llm.and_then(|l| l.get(k))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        };

        let provider = str_field("provider")
            .map(|s| Provider::from_slug(&s))
            .unwrap_or(Provider::Anthropic);
        // `api_key` (new) or `anthropic_api_key` (legacy) or the provider env var.
        let api_key = str_field("api_key")
            .or_else(|| str_field("anthropic_api_key"))
            .filter(|k| !k.is_empty())
            .or_else(|| {
                let env = provider.env_key();
                (!env.is_empty()).then(|| std::env::var(env).ok()).flatten()
            })
            .filter(|k| !k.is_empty())
            .unwrap_or_default();
        let model = str_field("model").unwrap_or_default();
        let base_url = str_field("base_url").unwrap_or_default();
        Config::from_parts(provider, api_key, model, base_url)
    }

    /// Load a usable config, or `None` when no key is configured (in which case
    /// the explain feature prompts the user to open settings).
    pub fn load() -> Option<Config> {
        let cfg = Config::current_or_default();
        (!cfg.api_key.is_empty()).then_some(cfg)
    }

    /// Whether a key is configured.
    pub fn available() -> bool {
        Config::load().is_some()
    }

    /// Persist this config to the global `config.toml`, preserving other sections.
    pub fn save(&self) -> Result<(), String> {
        let path = config_path().ok_or("no data directory")?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let mut root: toml::Table = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default();
        let mut llm = toml::Table::new();
        llm.insert("provider".into(), self.provider.slug().into());
        llm.insert("api_key".into(), self.api_key.clone().into());
        llm.insert("model".into(), self.model.clone().into());
        llm.insert("base_url".into(), self.base_url.clone().into());
        root.insert("llm".into(), toml::Value::Table(llm));
        let s = toml::to_string(&root).map_err(|e| e.to_string())?;
        std::fs::write(&path, s).map_err(|e| e.to_string())
    }
}

/// The path where the config lives, for a "not configured" hint.
pub fn config_hint() -> String {
    config_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "<clew data dir>/config.toml".into())
}

/// Who authored a chat message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

/// One message in a multi-turn conversation.
#[derive(Debug, Clone)]
pub struct ChatMsg {
    pub role: Role,
    pub content: String,
}

impl ChatMsg {
    pub fn user(content: impl Into<String>) -> ChatMsg {
        ChatMsg {
            role: Role::User,
            content: content.into(),
        }
    }
    pub fn assistant(content: impl Into<String>) -> ChatMsg {
        ChatMsg {
            role: Role::Assistant,
            content: content.into(),
        }
    }
    pub fn role_str(&self) -> &'static str {
        match self.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }
}

// -- tool calling (agent turns) ----------------------------------------------

/// A tool the model may call during an agent turn.
#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    /// JSON Schema for the arguments object.
    pub parameters: serde_json::Value,
}

/// One tool invocation the model requested.
#[derive(Debug, Clone)]
pub struct ToolCall {
    /// Provider-assigned id correlating the result back to this call.
    pub id: String,
    pub name: String,
    pub args: serde_json::Value,
}

/// A message in a tool-calling conversation. Distinct from [`ChatMsg`] because
/// assistant turns carry structured tool calls and their results must be
/// round-tripped in the provider's own shape.
#[derive(Debug, Clone)]
pub enum AgentMsg {
    User(String),
    /// An assistant turn: optional prose plus the tool calls it requested.
    Assistant {
        text: String,
        calls: Vec<ToolCall>,
        /// Raw `thinking` / `redacted_thinking` blocks from a reasoning model,
        /// verbatim. Anthropic requires them back unmodified (they carry a
        /// signature) at the start of the assistant turn when tools are in
        /// play; other providers never see them. Empty for non-reasoning
        /// models and replayed history.
        thinking: Vec<serde_json::Value>,
    },
    /// The result of one earlier tool call, fed back to the model.
    ToolResult {
        call: ToolCall,
        content: String,
    },
}

/// What one agent step produced: prose, and the tool calls to run next (empty
/// when the model is done exploring).
#[derive(Debug, Clone)]
pub struct StepOutput {
    pub text: String,
    pub calls: Vec<ToolCall>,
    /// Raw reasoning blocks to round-trip on the next step (see
    /// [`AgentMsg::Assistant::thinking`]).
    pub thinking: Vec<serde_json::Value>,
    /// The step hit `max_tokens` mid-response (Anthropic `stop_reason:
    /// "max_tokens"`, OpenAI `finish_reason: "length"`). Any tool calls in a
    /// truncated step may have half-written arguments and must not be executed;
    /// the caller should retry with a larger budget.
    pub truncated: bool,
}

/// One tool-calling completion step (blocking — call off the UI thread). The
/// model sees the tools and either requests calls or answers in prose.
pub fn complete_tools(
    cfg: &Config,
    system: &str,
    messages: &[AgentMsg],
    tools: &[ToolDef],
    max_tokens: u32,
) -> Result<StepOutput, String> {
    if cfg.provider == Provider::Anthropic {
        anthropic_tools(cfg, system, messages, tools, max_tokens)
    } else {
        openai_tools(cfg, system, messages, tools, max_tokens)
    }
}

/// Stream the closing step of a tool conversation: tools are still declared
/// (the transcript's tool blocks require them) but `tool_choice: none` pins
/// the model to prose, and each text token is forwarded through `on_delta` as
/// it arrives. Returns the full text. Blocking.
pub fn complete_tools_stream(
    cfg: &Config,
    system: &str,
    messages: &[AgentMsg],
    tools: &[ToolDef],
    max_tokens: u32,
    mut on_delta: impl FnMut(&str),
) -> Result<String, String> {
    if cfg.provider == Provider::Anthropic {
        let url = format!("{}/v1/messages", cfg.base_url.trim_end_matches('/'));
        let body = anthropic_tools_body(cfg, system, messages, tools, max_tokens, true);
        let reader = open_stream(
            ureq::post(&url)
                .set("x-api-key", &cfg.api_key)
                .set("anthropic-version", API_VERSION)
                .set("content-type", "application/json"),
            &body,
            "Anthropic",
        )?;
        read_sse(reader, &mut on_delta, |json| {
            (json.get("type").and_then(|t| t.as_str()) == Some("content_block_delta"))
                .then(|| {
                    json.pointer("/delta/text")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                })
                .flatten()
        })
    } else {
        if cfg.base_url.is_empty() {
            return Err("no base URL set for this provider".into());
        }
        let url = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));
        let body = openai_tools_body(cfg, system, messages, tools, max_tokens, true);
        let reader = open_stream(
            ureq::post(&url)
                .set("Authorization", &format!("Bearer {}", cfg.api_key))
                .set("content-type", "application/json"),
            &body,
            cfg.provider.label(),
        )?;
        read_sse(reader, &mut on_delta, |json| {
            json.pointer("/choices/0/delta/content")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
    }
}

/// Put an ephemeral `cache_control` breakpoint on the last content block of the
/// last message. A tool loop re-sends the whole growing conversation every
/// step; a breakpoint at the tail lets the next step read this step's prefix
/// from the provider's prompt cache instead of re-processing it, which cuts
/// both cost and latency roughly in proportion to the conversation length. The
/// system prompt carries its own breakpoint (covering the tools+system prefix).
/// String content is lifted into a block array, the shape per-block fields need.
fn mark_tail_cache(msgs: &mut [serde_json::Value]) {
    let Some(last) = msgs.last_mut() else { return };
    let content = &mut last["content"];
    if let Some(text) = content.as_str() {
        if text.is_empty() {
            return; // the API rejects empty text blocks; nothing worth caching
        }
        *content = serde_json::json!([{ "type": "text", "text": text }]);
    }
    if let Some(block) = content.as_array_mut().and_then(|b| b.last_mut()) {
        block["cache_control"] = serde_json::json!({ "type": "ephemeral" });
    }
}

/// Build the Anthropic Messages body for a tool conversation. `final_stream`
/// turns on SSE and pins `tool_choice: none` for the streamed closing answer.
fn anthropic_tools_body(
    cfg: &Config,
    system: &str,
    messages: &[AgentMsg],
    tools: &[ToolDef],
    max_tokens: u32,
    final_stream: bool,
) -> String {
    // Anthropic wants tool results as `tool_result` blocks in the user message
    // immediately following the assistant's `tool_use` — merge consecutive
    // results into one user message.
    let mut msgs: Vec<serde_json::Value> = Vec::new();
    for m in messages {
        match m {
            AgentMsg::User(text) => {
                msgs.push(serde_json::json!({ "role": "user", "content": text }));
            }
            AgentMsg::Assistant {
                text,
                calls,
                thinking,
            } => {
                // Reasoning blocks come first, verbatim — the API validates
                // their signatures and rejects reordered or altered blocks.
                let mut blocks: Vec<serde_json::Value> = thinking.clone();
                if !text.is_empty() {
                    blocks.push(serde_json::json!({ "type": "text", "text": text }));
                }
                for c in calls {
                    blocks.push(serde_json::json!({
                        "type": "tool_use", "id": c.id, "name": c.name, "input": c.args,
                    }));
                }
                msgs.push(serde_json::json!({ "role": "assistant", "content": blocks }));
            }
            AgentMsg::ToolResult { call, content } => {
                let block = serde_json::json!({
                    "type": "tool_result", "tool_use_id": call.id, "content": content,
                });
                match msgs.last_mut() {
                    Some(last)
                        if last["role"] == "user"
                            && last["content"].is_array()
                            && last["content"][0]["type"] == "tool_result" =>
                    {
                        last["content"].as_array_mut().unwrap().push(block);
                    }
                    _ => msgs.push(serde_json::json!({ "role": "user", "content": [block] })),
                }
            }
        }
    }
    let tool_defs: Vec<serde_json::Value> = tools
        .iter()
        .map(|t| {
            serde_json::json!({
                "name": t.name, "description": t.description, "input_schema": t.parameters,
            })
        })
        .collect();
    mark_tail_cache(&mut msgs);
    let mut body = serde_json::json!({
        "model": cfg.model,
        "max_tokens": max_tokens,
        // The system block carries a breakpoint too, so the (tools + system)
        // prefix — identical every step — is cached from the first step on.
        "system": [{
            "type": "text", "text": system,
            "cache_control": { "type": "ephemeral" },
        }],
        "messages": msgs,
        "tools": tool_defs,
    });
    if final_stream {
        body["stream"] = true.into();
        body["tool_choice"] = serde_json::json!({ "type": "none" });
    }
    body.to_string()
}

fn anthropic_tools(
    cfg: &Config,
    system: &str,
    messages: &[AgentMsg],
    tools: &[ToolDef],
    max_tokens: u32,
) -> Result<StepOutput, String> {
    let url = format!("{}/v1/messages", cfg.base_url.trim_end_matches('/'));
    let body = anthropic_tools_body(cfg, system, messages, tools, max_tokens, false);
    let text = send(
        ureq::post(&url)
            .set("x-api-key", &cfg.api_key)
            .set("anthropic-version", API_VERSION)
            .set("content-type", "application/json"),
        &body,
        "Anthropic",
    )?;
    let json: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("bad JSON response: {e}"))?;
    let mut out = StepOutput {
        text: String::new(),
        calls: Vec::new(),
        thinking: Vec::new(),
        truncated: json.get("stop_reason").and_then(|v| v.as_str()) == Some("max_tokens"),
    };
    for block in json
        .get("content")
        .and_then(|c| c.as_array())
        .into_iter()
        .flatten()
    {
        match block.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                    out.text.push_str(t);
                }
            }
            // Reasoning models interleave thinking with tool use; keep the
            // blocks whole so the next step can hand them back untouched.
            Some("thinking") | Some("redacted_thinking") => out.thinking.push(block.clone()),
            Some("tool_use") => out.calls.push(ToolCall {
                id: block
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                name: block
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                args: block.get("input").cloned().unwrap_or(serde_json::json!({})),
            }),
            _ => {}
        }
    }
    out.text = out.text.trim().to_string();
    Ok(out)
}

/// Build the `/chat/completions` body for a tool conversation. `final_stream`
/// turns on SSE and pins `tool_choice: "none"` for the streamed closing answer.
fn openai_tools_body(
    cfg: &Config,
    system: &str,
    messages: &[AgentMsg],
    tools: &[ToolDef],
    max_tokens: u32,
    final_stream: bool,
) -> String {
    let mut msgs = vec![serde_json::json!({ "role": "system", "content": system })];
    for m in messages {
        match m {
            AgentMsg::User(text) => {
                msgs.push(serde_json::json!({ "role": "user", "content": text }));
            }
            // OpenAI-compatible reasoners keep their reasoning out of the
            // round-trip (DeepSeek even rejects `reasoning_content` echoed
            // back), so `thinking` is intentionally dropped here.
            AgentMsg::Assistant { text, calls, .. } => {
                let mut msg = serde_json::json!({ "role": "assistant" });
                msg["content"] = if text.is_empty() {
                    serde_json::Value::Null
                } else {
                    text.clone().into()
                };
                if !calls.is_empty() {
                    let tc: Vec<serde_json::Value> = calls
                        .iter()
                        .map(|c| {
                            serde_json::json!({
                                "id": c.id,
                                "type": "function",
                                "function": {
                                    "name": c.name,
                                    "arguments": c.args.to_string(),
                                },
                            })
                        })
                        .collect();
                    msg["tool_calls"] = tc.into();
                }
                msgs.push(msg);
            }
            AgentMsg::ToolResult { call, content } => {
                msgs.push(serde_json::json!({
                    "role": "tool", "tool_call_id": call.id, "content": content,
                }));
            }
        }
    }
    let tool_defs: Vec<serde_json::Value> = tools
        .iter()
        .map(|t| {
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                },
            })
        })
        .collect();
    let mut body = serde_json::json!({ "model": cfg.model, "messages": msgs, "tools": tool_defs });
    let m = cfg.model.to_ascii_lowercase();
    let newer = ["gpt-5", "o1", "o3", "o4"].iter().any(|p| m.starts_with(p));
    body[if newer {
        "max_completion_tokens"
    } else {
        "max_tokens"
    }] = max_tokens.into();
    if final_stream {
        body["stream"] = true.into();
        body["tool_choice"] = "none".into();
    }
    body.to_string()
}

fn openai_tools(
    cfg: &Config,
    system: &str,
    messages: &[AgentMsg],
    tools: &[ToolDef],
    max_tokens: u32,
) -> Result<StepOutput, String> {
    if cfg.base_url.is_empty() {
        return Err("no base URL set for this provider".into());
    }
    let url = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));
    let body = openai_tools_body(cfg, system, messages, tools, max_tokens, false);
    let text = send(
        ureq::post(&url)
            .set("Authorization", &format!("Bearer {}", cfg.api_key))
            .set("content-type", "application/json"),
        &body,
        cfg.provider.label(),
    )?;
    let json: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("bad JSON response: {e}"))?;
    let msg = json
        .pointer("/choices/0/message")
        .ok_or("no message in response")?;
    let mut out = StepOutput {
        text: msg
            .get("content")
            .and_then(|c| c.as_str())
            .unwrap_or_default()
            .trim()
            .to_string(),
        calls: Vec::new(),
        thinking: Vec::new(),
        truncated: json
            .pointer("/choices/0/finish_reason")
            .and_then(|v| v.as_str())
            == Some("length"),
    };
    for tc in msg
        .get("tool_calls")
        .and_then(|t| t.as_array())
        .into_iter()
        .flatten()
    {
        let name = tc
            .pointer("/function/name")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        // Arguments arrive as a JSON string; tolerate malformed ones as empty.
        let args = tc
            .pointer("/function/arguments")
            .and_then(|v| v.as_str())
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or(serde_json::json!({}));
        out.calls.push(ToolCall {
            id: tc
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            name,
            args,
        });
    }
    Ok(out)
}

/// One synchronous single-turn completion (blocking — call off the UI thread).
/// Returns the assistant's text, or an error string.
pub fn complete(
    cfg: &Config,
    system: &str,
    prompt: &str,
    max_tokens: u32,
) -> Result<String, String> {
    complete_chat(cfg, system, &[ChatMsg::user(prompt)], max_tokens)
}

/// A multi-turn completion: the whole conversation is sent so the model can
/// resolve follow-ups ("it", "that function") against earlier turns. Blocking.
pub fn complete_chat(
    cfg: &Config,
    system: &str,
    messages: &[ChatMsg],
    max_tokens: u32,
) -> Result<String, String> {
    if cfg.provider == Provider::Anthropic {
        anthropic(cfg, system, messages, max_tokens)
    } else {
        openai_compatible(cfg, system, messages, max_tokens)
    }
}

/// A streaming multi-turn completion: `on_delta` is called with each token as it
/// arrives (Server-Sent Events), and the full text is returned at the end.
/// Blocking; run off the async runtime.
pub fn complete_chat_stream(
    cfg: &Config,
    system: &str,
    messages: &[ChatMsg],
    max_tokens: u32,
    mut on_delta: impl FnMut(&str),
) -> Result<String, String> {
    if cfg.provider == Provider::Anthropic {
        anthropic_stream(cfg, system, messages, max_tokens, &mut on_delta)
    } else {
        openai_stream(cfg, system, messages, max_tokens, &mut on_delta)
    }
}

/// Open an SSE response, mapping HTTP errors to their body like [`send`].
fn open_stream(
    req: ureq::Request,
    body: &str,
    who: &str,
) -> Result<Box<dyn std::io::Read + Send + Sync + 'static>, String> {
    send_with_retry(&req, body, who).map(|r| r.into_reader())
}

/// Read `data: …` SSE lines, extracting each token via `pick` (which returns the
/// delta text for a parsed event, or `None` to skip). Stops at `[DONE]`.
///
/// Providers report post-200 failures as **in-stream events** (Anthropic sends
/// `{"type":"error"}` on overload, OpenAI-compatible servers an `error`
/// object). Those must surface as `Err` — swallowing them would return a
/// silently truncated text as if the stream had completed.
fn read_sse(
    reader: Box<dyn std::io::Read + Send + Sync + 'static>,
    mut on_delta: impl FnMut(&str),
    pick: impl Fn(&serde_json::Value) -> Option<String>,
) -> Result<String, String> {
    use std::io::BufRead;
    let mut full = String::new();
    // A healthy stream always announces its end (OpenAI `[DONE]`, Anthropic
    // `message_stop`). EOF without it means the connection dropped mid-answer
    // — that must not pass as a completed text.
    let mut terminated = false;
    for line in std::io::BufReader::new(reader).lines() {
        let line = line.map_err(|e| format!("stream read: {e}"))?;
        let Some(data) = line.strip_prefix("data:").map(str::trim) else {
            continue;
        };
        if data == "[DONE]" {
            terminated = true;
            break;
        }
        let Ok(json) = serde_json::from_str::<serde_json::Value>(data) else {
            continue;
        };
        // `error` must be present AND non-null: some OpenAI-compatible
        // proxies attach `"error": null` to every healthy chunk.
        if json.get("type").and_then(|t| t.as_str()) == Some("error")
            || json.get("error").is_some_and(|e| !e.is_null())
        {
            let msg = json
                .pointer("/error/message")
                .and_then(|v| v.as_str())
                .unwrap_or("the provider reported a stream error");
            return Err(format!("stream error: {msg}"));
        }
        if json.get("type").and_then(|t| t.as_str()) == Some("message_stop") {
            terminated = true;
            continue;
        }
        if let Some(delta) = pick(&json) {
            on_delta(&delta);
            full.push_str(&delta);
        }
    }
    if !terminated {
        return Err("the stream ended before completion (connection dropped?)".into());
    }
    Ok(full.trim().to_string())
}

fn openai_stream(
    cfg: &Config,
    system: &str,
    messages: &[ChatMsg],
    max_tokens: u32,
    on_delta: &mut dyn FnMut(&str),
) -> Result<String, String> {
    if cfg.base_url.is_empty() {
        return Err("no base URL set for this provider".into());
    }
    let url = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));
    let mut msgs = vec![serde_json::json!({ "role": "system", "content": system })];
    msgs.extend(json_messages(messages));
    let mut body = serde_json::json!({ "model": cfg.model, "messages": msgs, "stream": true });
    let m = cfg.model.to_ascii_lowercase();
    let newer = ["gpt-5", "o1", "o3", "o4"].iter().any(|p| m.starts_with(p));
    body[if newer {
        "max_completion_tokens"
    } else {
        "max_tokens"
    }] = max_tokens.into();
    let reader = open_stream(
        ureq::post(&url)
            .set("Authorization", &format!("Bearer {}", cfg.api_key))
            .set("content-type", "application/json"),
        &body.to_string(),
        cfg.provider.label(),
    )?;
    read_sse(reader, on_delta, |json| {
        json.pointer("/choices/0/delta/content")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    })
}

fn anthropic_stream(
    cfg: &Config,
    system: &str,
    messages: &[ChatMsg],
    max_tokens: u32,
    on_delta: &mut dyn FnMut(&str),
) -> Result<String, String> {
    let url = format!("{}/v1/messages", cfg.base_url.trim_end_matches('/'));
    let body = serde_json::json!({
        "model": cfg.model,
        "max_tokens": max_tokens,
        "system": system,
        "messages": json_messages(messages),
        "stream": true,
    })
    .to_string();
    let reader = open_stream(
        ureq::post(&url)
            .set("x-api-key", &cfg.api_key)
            .set("anthropic-version", API_VERSION)
            .set("content-type", "application/json"),
        &body,
        "Anthropic",
    )?;
    // Anthropic emits typed events; `content_block_delta` carries the token text.
    read_sse(reader, on_delta, |json| {
        (json.get("type").and_then(|t| t.as_str()) == Some("content_block_delta"))
            .then(|| {
                json.pointer("/delta/text")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            })
            .flatten()
    })
}

/// Render the conversation as provider-agnostic `{role, content}` JSON objects.
fn json_messages(messages: &[ChatMsg]) -> Vec<serde_json::Value> {
    messages
        .iter()
        .map(|m| serde_json::json!({ "role": m.role_str(), "content": m.content }))
        .collect()
}

fn anthropic(
    cfg: &Config,
    system: &str,
    messages: &[ChatMsg],
    max_tokens: u32,
) -> Result<String, String> {
    let url = format!("{}/v1/messages", cfg.base_url.trim_end_matches('/'));
    let body = serde_json::json!({
        "model": cfg.model,
        "max_tokens": max_tokens,
        "system": system,
        "messages": json_messages(messages),
    })
    .to_string();
    let text = send(
        ureq::post(&url)
            .set("x-api-key", &cfg.api_key)
            .set("anthropic-version", API_VERSION)
            .set("content-type", "application/json"),
        &body,
        "Anthropic",
    )?;
    let json: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("bad JSON response: {e}"))?;
    json.get("content")
        .and_then(|c| c.as_array())
        .and_then(|blocks| {
            blocks
                .iter()
                .find_map(|b| b.get("text").and_then(|t| t.as_str()))
        })
        .map(|s| s.trim().to_string())
        .ok_or_else(|| "no text in Anthropic response".to_string())
}

fn openai_compatible(
    cfg: &Config,
    system: &str,
    messages: &[ChatMsg],
    max_tokens: u32,
) -> Result<String, String> {
    if cfg.base_url.is_empty() {
        return Err("no base URL set for this provider".into());
    }
    let url = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));
    // Prepend the system prompt, then the conversation turns.
    let mut msgs = vec![serde_json::json!({ "role": "system", "content": system })];
    msgs.extend(json_messages(messages));
    let mut body = serde_json::json!({
        "model": cfg.model,
        "messages": msgs,
    });
    // Newer OpenAI models (gpt-5*, o-series) reject the classic `max_tokens` and
    // require `max_completion_tokens`; classic chat models still take `max_tokens`.
    let m = cfg.model.to_ascii_lowercase();
    let newer = ["gpt-5", "o1", "o3", "o4"].iter().any(|p| m.starts_with(p));
    let field = if newer {
        "max_completion_tokens"
    } else {
        "max_tokens"
    };
    body[field] = max_tokens.into();
    let body = body.to_string();
    let text = send(
        ureq::post(&url)
            .set("Authorization", &format!("Bearer {}", cfg.api_key))
            .set("content-type", "application/json"),
        &body,
        cfg.provider.label(),
    )?;
    let json: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("bad JSON response: {e}"))?;
    json.pointer("/choices/0/message/content")
        .and_then(|c| c.as_str())
        .map(|s| s.trim().to_string())
        .ok_or_else(|| "no text in response".to_string())
}

/// Send a JSON body and read the response text, mapping HTTP errors to their body.
fn send(req: ureq::Request, body: &str, who: &str) -> Result<String, String> {
    let r = send_with_retry(&req, body, who)?;
    let mut s = String::new();
    r.into_reader()
        .read_to_string(&mut s)
        .map_err(|e| format!("read response: {e}"))?;
    Ok(s)
}

/// How many times a transient failure is retried before giving up.
const SEND_RETRIES: u32 = 3;

/// Statuses worth retrying: rate limits (429), overload (529 on Anthropic),
/// timeouts and transient server errors. 4xx besides these are caller bugs or
/// auth problems and fail immediately.
fn transient_status(code: u16) -> bool {
    matches!(code, 408 | 429 | 500 | 502 | 503 | 504 | 529)
}

/// Send with exponential backoff on transient failures (HTTP status above, or
/// a transport error like a dropped connection). Honors a numeric
/// `retry-after` header when the provider sends one. Blocking sleeps — every
/// caller already runs off the UI thread / async runtime.
fn send_with_retry(req: &ureq::Request, body: &str, who: &str) -> Result<ureq::Response, String> {
    let mut delay = std::time::Duration::from_secs(1);
    for attempt in 0..=SEND_RETRIES {
        let wait = match req.clone().send_string(body) {
            Ok(r) => return Ok(r),
            Err(ureq::Error::Status(code, r)) => {
                if attempt == SEND_RETRIES || !transient_status(code) {
                    let msg = r.into_string().unwrap_or_default();
                    return Err(format!("{who} API error {code}: {}", first_line(&msg)));
                }
                r.header("retry-after")
                    .and_then(|v| v.parse::<u64>().ok())
                    .map(std::time::Duration::from_secs)
                    .unwrap_or(delay)
                    .min(std::time::Duration::from_secs(30))
            }
            Err(e) => {
                if attempt == SEND_RETRIES {
                    return Err(format!("request failed: {e}"));
                }
                delay
            }
        };
        std::thread::sleep(wait);
        delay = (delay * 2).min(std::time::Duration::from_secs(8));
    }
    unreachable!("loop returns on the last attempt")
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").chars().take(200).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_cache_marks_last_block_only() {
        // String content is lifted into a marked block array.
        let mut msgs = vec![
            serde_json::json!({ "role": "user", "content": "first" }),
            serde_json::json!({ "role": "user", "content": "question" }),
        ];
        mark_tail_cache(&mut msgs);
        assert!(msgs[0]["content"].is_string(), "earlier messages untouched");
        assert_eq!(msgs[1]["content"][0]["text"], "question");
        assert_eq!(msgs[1]["content"][0]["cache_control"]["type"], "ephemeral");

        // Block-array content: only the last block is marked.
        let mut msgs = vec![serde_json::json!({
            "role": "user",
            "content": [
                { "type": "tool_result", "tool_use_id": "a", "content": "x" },
                { "type": "tool_result", "tool_use_id": "b", "content": "y" },
            ]
        })];
        mark_tail_cache(&mut msgs);
        assert!(msgs[0]["content"][0].get("cache_control").is_none());
        assert_eq!(msgs[0]["content"][1]["cache_control"]["type"], "ephemeral");

        // Empty string content stays untouched (empty text blocks are invalid).
        let mut msgs = vec![serde_json::json!({ "role": "user", "content": "" })];
        mark_tail_cache(&mut msgs);
        assert_eq!(msgs[0]["content"], "");
    }

    /// Serve canned HTTP responses on a local socket, one per connection.
    fn mock_http(responses: Vec<String>) -> (String, std::thread::JoinHandle<usize>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = format!("http://{}/", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let mut served = 0;
            for resp in responses {
                let Ok((mut conn, _)) = listener.accept() else {
                    break;
                };
                // Read the WHOLE request (headers + declared body) before
                // responding. Under load the request arrives in several TCP
                // segments; answering after a partial read closes the socket
                // while the client is still writing, and the resulting reset
                // corrupts the client's view of the response (flaky tests).
                let mut req: Vec<u8> = Vec::new();
                let mut buf = [0u8; 8192];
                let mut body_start: Option<usize> = None;
                let mut body_len = 0usize;
                loop {
                    match std::io::Read::read(&mut conn, &mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                    if body_start.is_none()
                        && let Some(pos) = req.windows(4).position(|w| w == b"\r\n\r\n")
                    {
                        body_start = Some(pos + 4);
                        let headers = String::from_utf8_lossy(&req[..pos]);
                        body_len = headers
                            .lines()
                            .find_map(|l| {
                                let (name, value) = l.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().ok())?
                            })
                            .unwrap_or(0);
                    }
                    if let Some(start) = body_start
                        && req.len() >= start + body_len
                    {
                        break;
                    }
                }
                std::io::Write::write_all(&mut conn, resp.as_bytes()).unwrap();
                let _ = std::io::Write::flush(&mut conn);
                served += 1;
            }
            served
        });
        (addr, handle)
    }

    fn sse_reader(body: &str) -> Box<dyn std::io::Read + Send + Sync + 'static> {
        Box::new(std::io::Cursor::new(body.as_bytes().to_vec()))
    }

    fn pick_text(json: &serde_json::Value) -> Option<String> {
        json.pointer("/delta/text")
            .and_then(|v| v.as_str())
            .map(str::to_string)
    }

    #[test]
    fn read_sse_collects_deltas_and_stops_at_done() {
        let body = "data: {\"delta\":{\"text\":\"hel\"}}\n\n\
                    data: {\"delta\":{\"text\":\"lo\"}}\n\n\
                    data: [DONE]\n\n\
                    data: {\"delta\":{\"text\":\"ignored\"}}\n";
        let mut seen = String::new();
        let full = read_sse(sse_reader(body), |d| seen.push_str(d), pick_text).unwrap();
        assert_eq!(full, "hello");
        assert_eq!(seen, "hello");
    }

    #[test]
    fn read_sse_surfaces_in_stream_error_events() {
        // Anthropic-style mid-stream failure: the text so far must not be
        // returned as a completed answer.
        let body = "data: {\"delta\":{\"text\":\"partial\"}}\n\n\
                    data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\
                    \"message\":\"Overloaded\"}}\n";
        let err = read_sse(sse_reader(body), |_| {}, pick_text).unwrap_err();
        assert!(
            err.contains("Overloaded"),
            "error carries the message: {err}"
        );

        // OpenAI-compatible style: a bare `error` object.
        let body = "data: {\"error\":{\"message\":\"quota exceeded\"}}\n";
        let err = read_sse(sse_reader(body), |_| {}, pick_text).unwrap_err();
        assert!(err.contains("quota exceeded"));

        // `"error": null` on a healthy chunk (some proxies do this) is NOT an
        // error.
        let body = "data: {\"delta\":{\"text\":\"ok\"},\"error\":null}\n\ndata: [DONE]\n";
        let full = read_sse(sse_reader(body), |_| {}, pick_text).unwrap();
        assert_eq!(full, "ok");
    }

    #[test]
    fn read_sse_rejects_eof_without_terminator() {
        // Connection dropped mid-answer: no [DONE], no message_stop — the
        // partial text must not be returned as a completed answer.
        let body = "data: {\"delta\":{\"text\":\"half an ans\"}}\n";
        let err = read_sse(sse_reader(body), |_| {}, pick_text).unwrap_err();
        assert!(err.contains("ended before completion"), "{err}");

        // The Anthropic terminator counts too.
        let body = "data: {\"delta\":{\"text\":\"whole\"}}\n\n\
                    data: {\"type\":\"message_stop\"}\n";
        let full = read_sse(sse_reader(body), |_| {}, pick_text).unwrap();
        assert_eq!(full, "whole");
    }

    #[test]
    fn send_retries_transient_and_honors_retry_after() {
        let overloaded = "HTTP/1.1 529 Overloaded\r\nretry-after: 0\r\n\
                          connection: close\r\ncontent-length: 0\r\n\r\n"
            .to_string();
        let ok = "HTTP/1.1 200 OK\r\nconnection: close\r\n\
                  content-length: 2\r\n\r\nok"
            .to_string();
        let (addr, handle) = mock_http(vec![overloaded, ok]);
        let out = send(ureq::post(&addr), "{}", "test").expect("retried to success");
        assert_eq!(out, "ok");
        assert_eq!(handle.join().unwrap(), 2);
    }

    #[test]
    fn send_fails_fast_on_non_transient_status() {
        let bad = "HTTP/1.1 400 Bad Request\r\nconnection: close\r\n\
                   content-length: 4\r\n\r\nnope"
            .to_string();
        let (addr, handle) = mock_http(vec![bad]);
        let err = send(ureq::post(&addr), "{}", "test").unwrap_err();
        assert!(err.contains("400"), "error names the status: {err}");
        assert_eq!(handle.join().unwrap(), 1, "no retry on 400");
    }

    #[test]
    fn config_resolves_provider_key_model_and_base_url() {
        let _env = crate::env_lock();
        let dir = std::env::temp_dir().join("clew-llm-config-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // SAFETY: env mutation serialized by env_lock.
        unsafe {
            std::env::set_var("CLEW_DATA_DIR", &dir);
            std::env::remove_var("ANTHROPIC_API_KEY");
            std::env::remove_var("OPENAI_API_KEY");
            std::env::remove_var("DEEPSEEK_API_KEY");
        }

        // DeepSeek with an explicit key; model/base_url fall back to defaults.
        std::fs::write(
            dir.join("config.toml"),
            "[llm]\nprovider = \"deepseek\"\napi_key = \"sk-ds\"\n",
        )
        .unwrap();
        let cfg = Config::load().expect("configured");
        assert_eq!(cfg.provider, Provider::DeepSeek);
        assert_eq!(cfg.api_key, "sk-ds");
        assert_eq!(cfg.model, "deepseek-v4-flash");
        assert_eq!(cfg.base_url, "https://api.deepseek.com/v1");

        // Legacy `anthropic_api_key` still loads as Anthropic.
        std::fs::write(
            dir.join("config.toml"),
            "[llm]\nanthropic_api_key = \"sk-old\"\n",
        )
        .unwrap();
        let cfg = Config::load().expect("legacy key");
        assert_eq!(cfg.provider, Provider::Anthropic);
        assert_eq!(cfg.api_key, "sk-old");

        // No key → unavailable, but current_or_default still yields defaults.
        std::fs::write(dir.join("config.toml"), "[llm]\nprovider = \"openai\"\n").unwrap();
        assert!(Config::load().is_none());
        let d = Config::current_or_default();
        assert_eq!(d.provider, Provider::OpenAI);
        assert_eq!(d.model, "gpt-4o-mini");

        // Round-trip save → load.
        let saved = Config::from_parts(
            Provider::Custom,
            "k".into(),
            "my-model".into(),
            "http://localhost:1234/v1".into(),
        );
        saved.save().unwrap();
        let back = Config::load().expect("saved config loads");
        assert_eq!(back.provider, Provider::Custom);
        assert_eq!(back.model, "my-model");
        assert_eq!(back.base_url, "http://localhost:1234/v1");

        unsafe {
            std::env::remove_var("CLEW_DATA_DIR");
        }
    }
}
