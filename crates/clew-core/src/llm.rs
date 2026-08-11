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
//! (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `DEEPSEEK_API_KEY`) fills in a missing
//! `api_key`, but only while `base_url` is still that provider's own endpoint
//! (see [`env_key_for_endpoint`]).

use std::fmt;
use std::io::Read;

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

    /// The environment variable that may supply this provider's key.
    ///
    /// Public only so a settings form can NAME the variable it is deferring to.
    /// Reading it is not enough on its own: a key read from here may only be
    /// sent to the provider's own endpoint, which is what
    /// [`env_key_for_endpoint`] decides. Nothing should call
    /// `std::env::var(p.env_key())` directly.
    pub fn env_key(self) -> &'static str {
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

/// A provider's environment credential, but only when the request would go to
/// that provider's own endpoint.
///
/// `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` / `DEEPSEEK_API_KEY` are secrets *for
/// one host*. Inheriting one for a `base_url` the user pointed elsewhere — a
/// corporate gateway, an OpenRouter/LiteLLM proxy, a local server — would send
/// their real provider key to that host as `x-api-key` / `Bearer`, silently, in
/// exactly the case where they deliberately left the key field blank because
/// that endpoint needs no key of its own.
///
/// [`crate::embed`] resolves its own key through this same function on purpose:
/// two copies of this predicate are two chances for one of them to drift open.
/// The match is exact after trimming a trailing `/`, so a hand-written
/// `https://api.openai.com/v1/` still counts as OpenAI's, while anything else
/// (a different case, a different path, a lookalike host) fails closed and is
/// simply treated as "no key configured".
pub(crate) fn env_key_for_endpoint(
    env_var: &str,
    base_url: &str,
    provider_base_url: &str,
) -> Option<String> {
    fn normalize(u: &str) -> &str {
        u.trim().trim_end_matches('/')
    }
    // An empty provider endpoint is `Provider::Custom`, which has no env var of
    // its own and must never borrow another provider's.
    if env_var.is_empty() || provider_base_url.trim().is_empty() {
        return None;
    }
    if normalize(base_url) != normalize(provider_base_url) {
        return None;
    }
    std::env::var(env_var).ok().filter(|k| !k.is_empty())
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

impl Config {
    /// The stored settings regardless of whether a key is present — used to
    /// pre-fill the settings form. Defaults to Anthropic with an empty key.
    pub fn current_or_default() -> Config {
        let llm = crate::globalconfig::section("llm");
        let str_field = |k: &str| {
            llm.as_ref()
                .and_then(|l| l.get(k))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        };

        let provider = str_field("provider")
            .map(|s| Provider::from_slug(&s))
            .unwrap_or(Provider::Anthropic);
        let model = str_field("model").unwrap_or_default();
        // Resolved before the key, because whether the environment may supply
        // the key depends on where the request would go. `from_parts` fills a
        // blank `base_url` with the provider default, so the endpoint the
        // fallback is judged against is the one the request will actually use.
        let base_url = str_field("base_url").unwrap_or_default();
        let endpoint = if base_url.trim().is_empty() {
            provider.default_base_url()
        } else {
            base_url.trim()
        };
        // `api_key` (new) or `anthropic_api_key` (legacy) or the provider env var.
        let api_key = str_field("api_key")
            .or_else(|| str_field("anthropic_api_key"))
            .filter(|k| !k.is_empty())
            .or_else(|| {
                env_key_for_endpoint(provider.env_key(), endpoint, provider.default_base_url())
            })
            .filter(|k| !k.is_empty())
            .unwrap_or_default();
        Config::from_parts(provider, api_key, model, base_url)
    }

    /// The key as it is written in `config.toml`, with NO environment fallback.
    ///
    /// A settings form must pre-fill from this and not from
    /// [`Config::current_or_default`]. The resolved key is only allowed to
    /// travel to the provider's own endpoint (see [`env_key_for_endpoint`]),
    /// but a form that pre-fills with it turns it into a stored key the moment
    /// the user saves — and a stored key goes wherever `base_url` points. So
    /// typing a gateway URL into a pre-filled form wrote the user's real
    /// provider secret into the file next to that gateway, by a different route
    /// than the one the endpoint check closes.
    pub fn stored_key() -> String {
        let llm = crate::globalconfig::section("llm");
        let str_field = |k: &str| {
            llm.as_ref()
                .and_then(|l| l.get(k))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        };
        str_field("api_key")
            .or_else(|| str_field("anthropic_api_key"))
            .unwrap_or_default()
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

    /// Persist this config to the global `config.toml`, preserving other
    /// sections (see [`crate::globalconfig::update`] for why that is not a
    /// plain read-modify-write).
    pub fn save(&self) -> Result<(), String> {
        let mut llm = toml::Table::new();
        llm.insert("provider".into(), self.provider.slug().into());
        llm.insert("api_key".into(), self.api_key.clone().into());
        llm.insert("model".into(), self.model.clone().into());
        llm.insert("base_url".into(), self.base_url.clone().into());
        crate::globalconfig::update("llm", llm)
    }

    /// Persist this config as an edit OF `previous` — the values the writer
    /// read when it took its snapshot — keeping any field another writer has
    /// changed since. Returns the field names that were kept.
    ///
    /// The settings form is filled when the modal OPENS and written back on
    /// Save, so [`Config::save`] wrote a whole `[llm]` section built from
    /// values that may be minutes old: a second window that stored an API key
    /// in between had it replaced by the blank this form still held, with no
    /// warning and no undo. Use this from any writer that did not read the
    /// file immediately before writing it.
    pub fn save_from(&self, previous: &Config) -> Result<Vec<String>, String> {
        crate::globalconfig::update_fields(
            "llm",
            &[
                (
                    "provider",
                    previous.provider.slug().to_string(),
                    self.provider.slug().to_string(),
                ),
                ("api_key", previous.api_key.clone(), self.api_key.clone()),
                ("model", previous.model.clone(), self.model.clone()),
                ("base_url", previous.base_url.clone(), self.base_url.clone()),
            ],
        )
    }
}

/// The path where the config lives, for a "not configured" hint.
pub fn config_hint() -> String {
    crate::globalconfig::path()
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

/// How a tool-conversation request goes on the wire. The body is otherwise
/// identical, so this is the only thing that decides whether the call has a
/// cancellation seam.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wire {
    /// One blocking POST. No seam: once it is sent, only the provider
    /// finishing can end it.
    Blocking,
    /// SSE, tools still callable — an exploration step.
    Stream,
    /// SSE with `tool_choice: none` — the closing answer, pinned to prose.
    StreamAnswer,
}

/// One tool-calling completion step as a single blocking POST (call off the UI
/// thread). The model sees the tools and either requests calls or answers in
/// prose.
///
/// A blocking POST cannot be cancelled: nothing reaches the socket between
/// sending and the provider's answer, so a caller that gives up still pays for
/// the whole generation. Callers that can be stopped want
/// [`complete_tools_step`] instead; this stays as its fallback for endpoints
/// that refuse `stream: true`.
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

/// One exploration step of a tool conversation, issued as a STREAM so that it
/// can be stopped. The step's value is still the whole [`StepOutput`] — nothing
/// is forwarded token by token — the stream is here purely for the seam:
/// `cancelled` is polled between SSE events and dropping the reader closes the
/// connection, which is what actually ends (and stops billing) a generation the
/// user abandoned. Cancelling reports [`CANCELLED`]. Blocking.
///
/// This is why the agent's tool loop can honor Stop mid-step at all. Sent as
/// [`complete_tools`] the same step had no seam, so a Stop pressed while it was
/// on the wire could not reach it: it generated and billed to the end while the
/// panel kept spinning.
///
/// Falls back to [`complete_tools`] when the stream never OPENS — some
/// OpenAI-compatible endpoints refuse `stream: true`, and losing the seam beats
/// losing the turn. The price is that a failure neither form can get past (bad
/// key, rate limit) is retried once per form before the turn gives up, which
/// only lengthens a path that was going to end in an error. A failure AFTER the
/// first byte is never retried: the provider is already generating against that
/// request, and these carry no idempotency key, so a resend is billed twice.
pub fn complete_tools_step(
    cfg: &Config,
    system: &str,
    messages: &[AgentMsg],
    tools: &[ToolDef],
    max_tokens: u32,
    cancelled: &dyn Fn() -> bool,
) -> Result<StepOutput, String> {
    let anthropic = cfg.provider == Provider::Anthropic;
    if !anthropic && cfg.base_url.is_empty() {
        return Err("no base URL set for this provider".into());
    }
    let base = cfg.base_url.trim_end_matches('/');
    let (req, body, who) = if anthropic {
        (
            stream_agent()
                .post(&format!("{base}/v1/messages"))
                .set("x-api-key", &cfg.api_key)
                .set("anthropic-version", API_VERSION)
                .set("content-type", "application/json"),
            anthropic_tools_body(cfg, system, messages, tools, max_tokens, Wire::Stream),
            "Anthropic",
        )
    } else {
        (
            stream_agent()
                .post(&format!("{base}/chat/completions"))
                .set("Authorization", &format!("Bearer {}", cfg.api_key))
                .set("content-type", "application/json"),
            openai_tools_body(cfg, system, messages, tools, max_tokens, Wire::Stream),
            cfg.provider.label(),
        )
    };
    let reader = match open_stream(req, &body, who) {
        Ok(r) => r,
        // A stopped turn must not spend a second billed request on the
        // fallback — the whole point of the seam is not paying for work the
        // user walked away from.
        Err(_) if cancelled() => return Err(CANCELLED.into()),
        Err(_) => return complete_tools(cfg, system, messages, tools, max_tokens),
    };
    if anthropic {
        anthropic_step(reader, cancelled)
    } else {
        openai_step(reader, cancelled)
    }
}

/// One in-progress content block of a streamed step, keyed by the stream's
/// block index. A streamed block arrives as a shape (`content_block_start` /
/// the first `tool_calls` entry) followed by payload fragments, so nothing is
/// usable until the fragments are joined.
#[derive(Default)]
struct StreamBlock {
    kind: String,
    id: String,
    name: String,
    /// Joined payload: prose text, tool-argument JSON, or reasoning text.
    body: String,
    /// A reasoning block's signature. The API validates it on the way back, so
    /// it has to survive the round trip verbatim or the next step is rejected.
    signature: String,
    /// A `redacted_thinking` block's opaque payload, which arrives whole.
    data: String,
}

/// Reassemble an Anthropic streamed step. Blocks are collected by index (the
/// order the model emitted them, which is the order they must be replayed in).
fn anthropic_step(
    reader: Box<dyn std::io::Read + Send + Sync + 'static>,
    cancelled: &dyn Fn() -> bool,
) -> Result<StepOutput, String> {
    let mut blocks: std::collections::BTreeMap<u64, StreamBlock> =
        std::collections::BTreeMap::new();
    let mut truncated = false;
    let str_at = |json: &serde_json::Value, ptr: &str| {
        json.pointer(ptr)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    };
    read_sse_events(
        reader,
        |json| {
            let idx = json.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
            match json.get("type").and_then(|t| t.as_str()) {
                Some("content_block_start") => {
                    let block = blocks.entry(idx).or_default();
                    block.kind = str_at(json, "/content_block/type");
                    block.id = str_at(json, "/content_block/id");
                    block.name = str_at(json, "/content_block/name");
                    block.data = str_at(json, "/content_block/data");
                }
                Some("content_block_delta") => {
                    let block = blocks.entry(idx).or_default();
                    match json.pointer("/delta/type").and_then(|v| v.as_str()) {
                        // `input_json_delta` fragments are pieces of one JSON
                        // document: only the concatenation parses.
                        Some("text_delta") => block.body.push_str(&str_at(json, "/delta/text")),
                        Some("input_json_delta") => {
                            block.body.push_str(&str_at(json, "/delta/partial_json"));
                        }
                        Some("thinking_delta") => {
                            block.body.push_str(&str_at(json, "/delta/thinking"));
                        }
                        Some("signature_delta") => {
                            block.signature.push_str(&str_at(json, "/delta/signature"));
                        }
                        _ => {}
                    }
                }
                // The step hit `max_tokens`: its tool calls may be half-written
                // and the caller must retry rather than run them.
                Some("message_delta") => {
                    truncated |= json.pointer("/delta/stop_reason").and_then(|v| v.as_str())
                        == Some("max_tokens");
                }
                _ => {}
            }
        },
        cancelled,
    )?;
    let mut out = StepOutput {
        text: String::new(),
        calls: Vec::new(),
        thinking: Vec::new(),
        truncated,
    };
    for block in blocks.into_values() {
        match block.kind.as_str() {
            "text" => out.text.push_str(&block.body),
            "thinking" => out.thinking.push(serde_json::json!({
                "type": "thinking", "thinking": block.body, "signature": block.signature,
            })),
            "redacted_thinking" => out.thinking.push(serde_json::json!({
                "type": "redacted_thinking", "data": block.data,
            })),
            // A call with no arguments streams no `input_json_delta` at all,
            // and a truncated one leaves half a document — both mean `{}`.
            "tool_use" => out.calls.push(ToolCall {
                id: block.id,
                name: block.name,
                args: serde_json::from_str(&block.body).unwrap_or(serde_json::json!({})),
            }),
            _ => {}
        }
    }
    out.text = out.text.trim().to_string();
    Ok(out)
}

/// Reassemble an OpenAI-compatible streamed step. Reasoning is deliberately
/// dropped, exactly as in [`openai_tools`] — these endpoints reject their own
/// reasoning echoed back.
fn openai_step(
    reader: Box<dyn std::io::Read + Send + Sync + 'static>,
    cancelled: &dyn Fn() -> bool,
) -> Result<StepOutput, String> {
    let mut text = String::new();
    let mut calls: std::collections::BTreeMap<u64, StreamBlock> = std::collections::BTreeMap::new();
    let mut truncated = false;
    read_sse_events(
        reader,
        |json| {
            let Some(choice) = json.pointer("/choices/0") else {
                return;
            };
            truncated |= choice.get("finish_reason").and_then(|v| v.as_str()) == Some("length");
            if let Some(t) = choice.pointer("/delta/content").and_then(|v| v.as_str()) {
                text.push_str(t);
            }
            for call in choice
                .pointer("/delta/tool_calls")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
            {
                // `index` is what ties a call's fragments together. Only the
                // arguments are ever split: `id` and `name` arrive whole on the
                // first fragment, and some compatible servers repeat them on
                // every one, so these take the first non-empty value instead of
                // joining (joining yields `call_1call_1`, an id the provider
                // then rejects on the tool result).
                let idx = call.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
                let block = calls.entry(idx).or_default();
                if block.id.is_empty()
                    && let Some(id) = call.get("id").and_then(|v| v.as_str())
                {
                    block.id.push_str(id);
                }
                if block.name.is_empty()
                    && let Some(name) = call.pointer("/function/name").and_then(|v| v.as_str())
                {
                    block.name.push_str(name);
                }
                if let Some(args) = call.pointer("/function/arguments").and_then(|v| v.as_str()) {
                    block.body.push_str(args);
                }
            }
        },
        cancelled,
    )?;
    Ok(StepOutput {
        text: text.trim().to_string(),
        calls: calls
            .into_values()
            .map(|block| ToolCall {
                id: block.id,
                name: block.name,
                // Same tolerance as the blocking path: malformed or absent
                // arguments read as empty rather than failing the step.
                args: serde_json::from_str(&block.body).unwrap_or(serde_json::json!({})),
            })
            .collect(),
        thinking: Vec::new(),
        truncated,
    })
}

/// Stream the closing step of a tool conversation: tools are still declared
/// (the transcript's tool blocks require them) but `tool_choice: none` pins
/// the model to prose, and each text token is forwarded through `on_delta` as
/// it arrives. Returns the full text. Blocking.
///
/// `cancelled` is polled between events, exactly as in [`complete_chat_stream`]
/// — the agent's Stop button flips it, and cancelling reports [`CANCELLED`].
/// This covers only the turn's CLOSING answer; its exploration steps get the
/// same seam from [`complete_tools_step`]. The request also goes out on
/// [`stream_agent`], whose read timeout is what wakes this loop up to notice
/// the flag on a silent stream.
pub fn complete_tools_stream(
    cfg: &Config,
    system: &str,
    messages: &[AgentMsg],
    tools: &[ToolDef],
    max_tokens: u32,
    mut on_delta: impl FnMut(&str),
    cancelled: &dyn Fn() -> bool,
) -> Result<String, String> {
    if cfg.provider == Provider::Anthropic {
        let url = format!("{}/v1/messages", cfg.base_url.trim_end_matches('/'));
        let body =
            anthropic_tools_body(cfg, system, messages, tools, max_tokens, Wire::StreamAnswer);
        let reader = open_stream(
            stream_agent()
                .post(&url)
                .set("x-api-key", &cfg.api_key)
                .set("anthropic-version", API_VERSION)
                .set("content-type", "application/json"),
            &body,
            "Anthropic",
        )?;
        read_sse(
            reader,
            &mut on_delta,
            |json| {
                (json.get("type").and_then(|t| t.as_str()) == Some("content_block_delta"))
                    .then(|| {
                        json.pointer("/delta/text")
                            .and_then(|v| v.as_str())
                            .map(str::to_string)
                    })
                    .flatten()
            },
            cancelled,
        )
    } else {
        if cfg.base_url.is_empty() {
            return Err("no base URL set for this provider".into());
        }
        let url = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));
        let body = openai_tools_body(cfg, system, messages, tools, max_tokens, Wire::StreamAnswer);
        let reader = open_stream(
            stream_agent()
                .post(&url)
                .set("Authorization", &format!("Bearer {}", cfg.api_key))
                .set("content-type", "application/json"),
            &body,
            cfg.provider.label(),
        )?;
        read_sse(
            reader,
            &mut on_delta,
            |json| {
                json.pointer("/choices/0/delta/content")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
            },
            cancelled,
        )
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

/// Build the Anthropic Messages body for a tool conversation. `wire` turns on
/// SSE, and pins `tool_choice: none` for the streamed closing answer only — an
/// exploration step streams with its tools still callable.
fn anthropic_tools_body(
    cfg: &Config,
    system: &str,
    messages: &[AgentMsg],
    tools: &[ToolDef],
    max_tokens: u32,
    wire: Wire,
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
    if wire != Wire::Blocking {
        body["stream"] = true.into();
    }
    if wire == Wire::StreamAnswer {
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
    let body = anthropic_tools_body(cfg, system, messages, tools, max_tokens, Wire::Blocking);
    let text = send(
        blocking_agent()
            .post(&url)
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

/// Build the `/chat/completions` body for a tool conversation. `wire` turns on
/// SSE, and pins `tool_choice: "none"` for the streamed closing answer only —
/// an exploration step streams with its tools still callable.
fn openai_tools_body(
    cfg: &Config,
    system: &str,
    messages: &[AgentMsg],
    tools: &[ToolDef],
    max_tokens: u32,
    wire: Wire,
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
    if wire != Wire::Blocking {
        body["stream"] = true.into();
    }
    if wire == Wire::StreamAnswer {
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
    let body = openai_tools_body(cfg, system, messages, tools, max_tokens, Wire::Blocking);
    let text = send(
        blocking_agent()
            .post(&url)
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

/// The error a stream returns when `cancelled` asked it to stop. Callers use
/// it to tell "the user abandoned this answer" from a real failure — the
/// partial text is deliberately discarded rather than returned as if the
/// answer had completed.
pub const CANCELLED: &str = "cancelled";

/// A streaming multi-turn completion: `on_delta` is called with each token as
/// it arrives (Server-Sent Events), and the full text is returned at the end.
///
/// `cancelled` is polled between events so an abandoned answer stops costing
/// money: without it the provider call ran to completion no matter what the
/// caller did, because nothing about dropping the caller reaches this loop.
/// Blocking; run off the async runtime.
pub fn complete_chat_stream(
    cfg: &Config,
    system: &str,
    messages: &[ChatMsg],
    max_tokens: u32,
    mut on_delta: impl FnMut(&str),
    cancelled: &dyn Fn() -> bool,
) -> Result<String, String> {
    if cfg.provider == Provider::Anthropic {
        anthropic_stream(cfg, system, messages, max_tokens, &mut on_delta, cancelled)
    } else {
        openai_stream(cfg, system, messages, max_tokens, &mut on_delta, cancelled)
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

/// How long a streamed response may go silent before the reading thread wakes
/// up to re-test cancellation.
///
/// Cancellation is cooperative — [`read_sse_events`] polls the flag between
/// lines — so a provider that answers 200 and then sends nothing used to park that
/// thread forever. Nothing could reach the flag, and on the server the
/// blocked thread still owned a clone of the output channel, so shutdown
/// waited on a stream that would never end.
///
/// Generous on purpose: it must clear the gap before the first token on a
/// queued request (and the prompt-eval pause of a local model), which is why
/// it is a wakeup interval and not a failure. Providers that keep-alive
/// (Anthropic's `ping`) reset it long before it fires.
///
/// Note it is a SOCKET-level read timeout, so it also bounds the wait for the
/// response headers — before which the request is already on the wire. That is
/// why `retryable_transport` refuses to resend on it.
const STREAM_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Whether a read error is the idle timeout above rather than a real failure.
fn is_idle_timeout(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
    )
}

/// Why every agent here is built with `redirects(0)`.
///
/// ureq follows 3xx by default, and on the hop it strips only `authorization`
/// and `cookie` (`redirect_auth_headers` defaults to `Never`). Every other
/// header is copied verbatim to the new URL with no host check — including
/// Anthropic's `x-api-key`, which is not an `Authorization` header. A 301/302/303
/// also rewrites the POST to a GET and follows it. So a configured host that
/// answers `302 Location: https://elsewhere/` was handed the user's real
/// provider key, and the code here could not tell: it only ever saw the final
/// 200. Refusing to follow is the only place that decision can be made, because
/// by the time a response comes back the key has already left.
///
/// This costs nothing real: provider API endpoints do not redirect. A 3xx that
/// does arrive is surfaced by [`send_with_retry`] as an error rather than parsed
/// as a body.
fn no_redirects(b: ureq::AgentBuilder) -> ureq::AgentBuilder {
    b.redirects(0)
}

/// The agent used for STREAMED requests, carrying [`STREAM_IDLE_TIMEOUT`].
/// Non-streaming calls use [`blocking_agent`] instead: they legitimately hold a
/// silent socket for the whole generation, with no line boundaries to wake up
/// on. That is also why they cannot be cancelled, and why every call an agent
/// turn can be stopped mid-flight goes out streamed.
fn stream_agent() -> ureq::Agent {
    static AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    AGENT
        .get_or_init(|| {
            no_redirects(ureq::AgentBuilder::new())
                .timeout_read(STREAM_IDLE_TIMEOUT)
                .build()
        })
        .clone()
}

/// The agent used for NON-streamed requests. It exists only so those calls stop
/// using `ureq::post`, whose implicit agent follows redirects — see
/// [`no_redirects`]. It carries no read timeout on purpose: a blocking
/// completion holds a silent socket for the whole generation.
fn blocking_agent() -> ureq::Agent {
    static AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    AGENT
        .get_or_init(|| no_redirects(ureq::AgentBuilder::new()).build())
        .clone()
}

/// Read `data: …` SSE lines, handing every parsed event to `on_event`. Stops at
/// `[DONE]`. The two shapes a streamed request can want — a running text and a
/// reassembled tool step — differ only in that callback, and everything that
/// makes the loop safe (the cancellation poll, the idle wakeup, in-stream error
/// events, the terminator check) has to stay in ONE place or one of the two
/// silently loses it.
///
/// Providers report post-200 failures as **in-stream events** (Anthropic sends
/// `{"type":"error"}` on overload, OpenAI-compatible servers an `error`
/// object). Those must surface as `Err` — swallowing them would return a
/// silently truncated result as if the stream had completed.
fn read_sse_events(
    reader: Box<dyn std::io::Read + Send + Sync + 'static>,
    mut on_event: impl FnMut(&serde_json::Value),
    cancelled: &dyn Fn() -> bool,
) -> Result<(), String> {
    use std::io::BufRead;
    // A healthy stream always announces its end (OpenAI `[DONE]`, Anthropic
    // `message_stop`). EOF without it means the connection dropped mid-answer
    // — that must not pass as a completed text.
    let mut terminated = false;
    let mut reader = std::io::BufReader::new(reader);
    // Kept ACROSS iterations, unlike `lines()`. An idle-timeout wakeup can
    // land in the middle of a line, and a fresh buffer each pass would drop
    // the bytes already read and corrupt the stream.
    let mut buf: Vec<u8> = Vec::new();
    loop {
        // Checked before every read and after every wakeup: dropping the
        // reader here closes the HTTP connection, which is what actually
        // stops the generation.
        if cancelled() {
            return Err(CANCELLED.into());
        }
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) => break, // EOF
            Ok(_) => {}
            // The socket went quiet for `STREAM_IDLE_TIMEOUT` (see
            // `stream_agent`). Not an error: loop back and re-test
            // cancellation. This is the only thing that can interrupt a
            // provider which answers 200 and then sends nothing.
            Err(e) if is_idle_timeout(&e) => continue,
            Err(e) => return Err(format!("stream read: {e}")),
        }
        let line = String::from_utf8_lossy(&buf).into_owned();
        buf.clear();
        let Some(data) = line.trim_end().strip_prefix("data:").map(str::trim) else {
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
        on_event(&json);
    }
    if !terminated {
        return Err("the stream ended before completion (connection dropped?)".into());
    }
    Ok(())
}

/// Collect a streamed text answer: `pick` returns the delta text for a parsed
/// event (or `None` to skip it), each delta is forwarded to `on_delta` as it
/// arrives, and the joined text comes back at the end.
fn read_sse(
    reader: Box<dyn std::io::Read + Send + Sync + 'static>,
    mut on_delta: impl FnMut(&str),
    pick: impl Fn(&serde_json::Value) -> Option<String>,
    cancelled: &dyn Fn() -> bool,
) -> Result<String, String> {
    let mut full = String::new();
    read_sse_events(
        reader,
        |json| {
            if let Some(delta) = pick(json) {
                on_delta(&delta);
                full.push_str(&delta);
            }
        },
        cancelled,
    )?;
    Ok(full.trim().to_string())
}

fn openai_stream(
    cfg: &Config,
    system: &str,
    messages: &[ChatMsg],
    max_tokens: u32,
    on_delta: &mut dyn FnMut(&str),
    cancelled: &dyn Fn() -> bool,
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
        stream_agent()
            .post(&url)
            .set("Authorization", &format!("Bearer {}", cfg.api_key))
            .set("content-type", "application/json"),
        &body.to_string(),
        cfg.provider.label(),
    )?;
    read_sse(
        reader,
        on_delta,
        |json| {
            json.pointer("/choices/0/delta/content")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        },
        cancelled,
    )
}

fn anthropic_stream(
    cfg: &Config,
    system: &str,
    messages: &[ChatMsg],
    max_tokens: u32,
    on_delta: &mut dyn FnMut(&str),
    cancelled: &dyn Fn() -> bool,
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
        stream_agent()
            .post(&url)
            .set("x-api-key", &cfg.api_key)
            .set("anthropic-version", API_VERSION)
            .set("content-type", "application/json"),
        &body,
        "Anthropic",
    )?;
    // Anthropic emits typed events; `content_block_delta` carries the token text.
    read_sse(
        reader,
        on_delta,
        |json| {
            (json.get("type").and_then(|t| t.as_str()) == Some("content_block_delta"))
                .then(|| {
                    json.pointer("/delta/text")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                })
                .flatten()
        },
        cancelled,
    )
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
        blocking_agent()
            .post(&url)
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
        blocking_agent()
            .post(&url)
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
///
/// A status is always safe to retry: receiving one proves the provider
/// REFUSED the request rather than starting on it.
fn transient_status(code: u16) -> bool {
    matches!(code, 408 | 429 | 500 | 502 | 503 | 504 | 529)
}

/// Whether a transport error happened before the provider could have accepted
/// the request, making a resend safe.
///
/// This is not a detail. These requests are billed per token and carry no
/// idempotency key, so a resend of one the provider already started on is
/// charged twice and generates twice. The read timeout is the trap: streamed
/// requests carry [`STREAM_IDLE_TIMEOUT`] as a socket-level `timeout_read`,
/// which also covers the wait for the response HEADERS — so a provider that
/// simply took longer than that to produce its first byte surfaced here as an
/// ordinary transport failure, and the same POST went out up to four times.
///
/// Only failures to establish the connection qualify. Anything that fails once
/// bytes are on the wire is reported to the caller instead.
fn retryable_transport(e: &ureq::Error) -> bool {
    matches!(
        e.kind(),
        ureq::ErrorKind::Dns
            | ureq::ErrorKind::ConnectionFailed
            | ureq::ErrorKind::ProxyConnect
            | ureq::ErrorKind::InvalidProxyUrl
    )
}

/// Send with exponential backoff on transient failures (HTTP status above, or
/// a transport error like a dropped connection). Honors a numeric
/// `retry-after` header when the provider sends one. Blocking sleeps — every
/// caller already runs off the UI thread / async runtime.
fn send_with_retry(req: &ureq::Request, body: &str, who: &str) -> Result<ureq::Response, String> {
    let mut delay = std::time::Duration::from_secs(1);
    for attempt in 0..=SEND_RETRIES {
        let wait = match req.clone().send_string(body) {
            // With `redirects(0)` a 3xx comes back as a normal `Ok` response
            // whose body is the redirect page, not the provider's JSON. Report
            // it as what it is. Parsing it would surface as "bad JSON response"
            // and send the user hunting for a provider outage, when the real
            // answer is that their `base_url` points at something that wants to
            // move them somewhere else — the exact case in which following
            // along would have handed that somewhere else their API key.
            Ok(r) if (300..400).contains(&r.status()) => {
                let to = r.header("location").unwrap_or("elsewhere").to_string();
                return Err(format!(
                    "{who} endpoint redirected to {}; not following, because the \
                     API key would travel with it — point base_url at the real endpoint",
                    first_line(&to)
                ));
            }
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
                if attempt == SEND_RETRIES || !retryable_transport(&e) {
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

    /// These requests are billed per token and carry no idempotency key, so a
    /// resend is only safe while the provider cannot have seen the request.
    /// Everything failing after the bytes are on the wire — including the read
    /// timeout that also covers the wait for response headers — has to be
    /// reported, not retried. Treating them alike sent one POST four times.
    #[test]
    fn only_pre_connection_failures_are_resent() {
        // Nothing listens on port 1: the connection never opens.
        let e = ureq::get("http://127.0.0.1:1/").call().unwrap_err();
        assert!(
            retryable_transport(&e),
            "a connect failure is safe to retry, got {:?}",
            e.kind()
        );

        // Accepts the connection, then says nothing: the request IS on the
        // wire, and the provider may well be generating against it.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let held = listener.accept();
            std::thread::sleep(std::time::Duration::from_secs(3));
            drop(held);
        });
        let agent = ureq::AgentBuilder::new()
            .timeout_read(std::time::Duration::from_millis(200))
            .build();
        let e = agent
            .post(&format!("http://{addr}/"))
            .send_string("{}")
            .unwrap_err();
        assert!(
            !retryable_transport(&e),
            "a read timeout must not resend a billed POST, got {:?}",
            e.kind()
        );
    }

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
        let full = read_sse(sse_reader(body), |d| seen.push_str(d), pick_text, &|| false).unwrap();
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
        let err = read_sse(sse_reader(body), |_| {}, pick_text, &|| false).unwrap_err();
        assert!(
            err.contains("Overloaded"),
            "error carries the message: {err}"
        );

        // OpenAI-compatible style: a bare `error` object.
        let body = "data: {\"error\":{\"message\":\"quota exceeded\"}}\n";
        let err = read_sse(sse_reader(body), |_| {}, pick_text, &|| false).unwrap_err();
        assert!(err.contains("quota exceeded"));

        // `"error": null` on a healthy chunk (some proxies do this) is NOT an
        // error.
        let body = "data: {\"delta\":{\"text\":\"ok\"},\"error\":null}\n\ndata: [DONE]\n";
        let full = read_sse(sse_reader(body), |_| {}, pick_text, &|| false).unwrap();
        assert_eq!(full, "ok");
    }

    #[test]
    fn read_sse_rejects_eof_without_terminator() {
        // Connection dropped mid-answer: no [DONE], no message_stop — the
        // partial text must not be returned as a completed answer.
        let body = "data: {\"delta\":{\"text\":\"half an ans\"}}\n";
        let err = read_sse(sse_reader(body), |_| {}, pick_text, &|| false).unwrap_err();
        assert!(err.contains("ended before completion"), "{err}");

        // The Anthropic terminator counts too.
        let body = "data: {\"delta\":{\"text\":\"whole\"}}\n\n\
                    data: {\"type\":\"message_stop\"}\n";
        let full = read_sse(sse_reader(body), |_| {}, pick_text, &|| false).unwrap();
        assert_eq!(full, "whole");
    }

    /// The predicate is re-tested before every line, so a flag flipped
    /// mid-answer stops the loop where it stands: the remaining tokens are
    /// never consumed, the reader is dropped (closing the connection, which is
    /// what actually stops the generation), and the caller gets [`CANCELLED`]
    /// rather than a text that looks complete.
    #[test]
    fn read_sse_stops_consuming_once_cancelled() {
        let body = "data: {\"delta\":{\"text\":\"one\"}}\n\n\
                    data: {\"delta\":{\"text\":\"two\"}}\n\n\
                    data: [DONE]\n";
        let stop = std::sync::atomic::AtomicBool::new(false);
        let mut seen = String::new();
        let err = read_sse(
            sse_reader(body),
            |d| {
                seen.push_str(d);
                stop.store(true, std::sync::atomic::Ordering::Relaxed); // "Stop" pressed
            },
            pick_text,
            &|| stop.load(std::sync::atomic::Ordering::Relaxed),
        )
        .unwrap_err();
        assert_eq!(err, CANCELLED);
        assert_eq!(seen, "one", "no token after the stop reached the caller");
    }

    /// The agent's closing answer streams through here, and its turn is stopped
    /// with a flag. Before this predicate existed the call hardcoded "never
    /// cancelled", so pressing Stop left the in-flight request generating (and
    /// billing) to the end while the panel spun.
    #[test]
    fn tools_stream_honors_the_turns_stop_flag() {
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"one\"}}]}\n\n\
                   data: {\"choices\":[{\"delta\":{\"content\":\"two\"}}]}\n\n\
                   data: [DONE]\n\n";
        let resp = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
             connection: close\r\ncontent-length: {}\r\n\r\n{sse}",
            sse.len()
        );
        let (addr, handle) = mock_http(vec![resp]);
        let cfg = Config::from_parts(Provider::Custom, "k".into(), "m".into(), addr);

        let stop = std::sync::atomic::AtomicBool::new(false);
        let mut seen = String::new();
        let err = complete_tools_stream(
            &cfg,
            "system",
            &[AgentMsg::User("q".into())],
            &[],
            64,
            |d| {
                seen.push_str(d);
                stop.store(true, std::sync::atomic::Ordering::Relaxed);
            },
            &|| stop.load(std::sync::atomic::Ordering::Relaxed),
        )
        .unwrap_err();
        assert_eq!(err, CANCELLED, "the stop must abort the stream");
        assert_eq!(seen, "one", "the rest of the answer is never consumed");
        assert_eq!(handle.join().unwrap(), 1);
    }

    /// An exploration step is streamed only so it can be stopped, so nothing
    /// about the step may be lost in the reassembly: tool arguments arrive as
    /// fragments of one JSON document and only the concatenation parses, and a
    /// step that hit `max_tokens` has to keep saying so or the caller runs
    /// half-written calls instead of retrying.
    #[test]
    fn openai_step_rejoins_fragmented_tool_calls() {
        let sse = concat!(
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","#,
            r#""function":{"name":"search","arguments":"{\"q\": "}}]}}]}"#,
            "\n\n",
            // This one repeats `id`/`name`, as some compatible servers do.
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","#,
            r#""function":{"name":"search","arguments":"\"needle\"}"}}]}}]}"#,
            "\n\n",
            r#"data: {"choices":[{"delta":{"content":"looking"}}]}"#,
            "\n\n",
            r#"data: {"choices":[{"delta":{},"finish_reason":"length"}]}"#,
            "\n\ndata: [DONE]\n\n",
        );
        let out = openai_step(sse_reader(sse), &|| false).expect("a complete stream");
        assert_eq!(out.text, "looking");
        assert_eq!(out.calls.len(), 1, "the fragments are ONE call");
        assert_eq!(out.calls[0].id, "call_1");
        assert_eq!(out.calls[0].name, "search");
        assert_eq!(out.calls[0].args["q"], "needle");
        assert!(out.truncated, "`length` still reports as truncated");
    }

    /// Same for Anthropic, where the round trip is stricter: the API validates
    /// a reasoning block's signature on the next step, so `thinking` and
    /// `signature` deltas have to rebuild the block the blocking path would
    /// have returned whole, and blocks must come back in emission order.
    #[test]
    fn anthropic_step_rebuilds_blocks_with_their_signatures() {
        let sse = concat!(
            r#"data: {"type":"content_block_start","index":0,"#,
            r#""content_block":{"type":"thinking","thinking":""}}"#,
            "\n\n",
            r#"data: {"type":"content_block_delta","index":0,"#,
            r#""delta":{"type":"thinking_delta","thinking":"weighing"}}"#,
            "\n\n",
            r#"data: {"type":"content_block_delta","index":0,"#,
            r#""delta":{"type":"signature_delta","signature":"sig123"}}"#,
            "\n\n",
            r#"data: {"type":"content_block_start","index":1,"#,
            r#""content_block":{"type":"text","text":""}}"#,
            "\n\n",
            r#"data: {"type":"content_block_delta","index":1,"#,
            r#""delta":{"type":"text_delta","text":"reading"}}"#,
            "\n\n",
            r#"data: {"type":"content_block_start","index":2,"#,
            r#""content_block":{"type":"tool_use","id":"toolu_1","name":"read","input":{}}}"#,
            "\n\n",
            r#"data: {"type":"content_block_delta","index":2,"#,
            r#""delta":{"type":"input_json_delta","partial_json":"{\"path\": "}}"#,
            "\n\n",
            r#"data: {"type":"content_block_delta","index":2,"#,
            r#""delta":{"type":"input_json_delta","partial_json":"\"src/lib.rs\"}"}}"#,
            "\n\n",
            r#"data: {"type":"message_stop"}"#,
            "\n\n",
        );
        let out = anthropic_step(sse_reader(sse), &|| false).expect("a complete stream");
        assert_eq!(out.text, "reading");
        assert_eq!(out.thinking.len(), 1);
        assert_eq!(out.thinking[0]["type"], "thinking");
        assert_eq!(out.thinking[0]["thinking"], "weighing");
        assert_eq!(out.thinking[0]["signature"], "sig123");
        assert_eq!(out.calls.len(), 1);
        assert_eq!(out.calls[0].id, "toolu_1");
        assert_eq!(out.calls[0].name, "read");
        assert_eq!(out.calls[0].args["path"], "src/lib.rs");
        assert!(!out.truncated);
    }

    /// The reason an exploration step streams at all. Sent as `complete_tools`
    /// the step was one blocking POST with no seam, so a Stop pressed while it
    /// was on the wire could not reach it: the step generated (and billed) to
    /// the end, and the turn only closed at the next test BETWEEN steps.
    #[test]
    fn tools_step_stops_a_request_already_on_the_wire() {
        let sse = concat!(
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","#,
            r#""function":{"name":"search","arguments":"{}"}}]}}]}"#,
            "\n\n",
            r#"data: {"choices":[{"delta":{"content":"more"}}]}"#,
            "\n\ndata: [DONE]\n\n",
        );
        let resp = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
             connection: close\r\ncontent-length: {}\r\n\r\n{sse}",
            sse.len()
        );
        let (addr, handle) = mock_http(vec![resp]);
        let cfg = Config::from_parts(Provider::Custom, "k".into(), "m".into(), addr);

        // "Stop" pressed once the step is already streaming.
        let polls = std::cell::Cell::new(0u32);
        let err = complete_tools_step(
            &cfg,
            "system",
            &[AgentMsg::User("q".into())],
            &[],
            64,
            &|| {
                polls.set(polls.get() + 1);
                polls.get() > 2
            },
        )
        .unwrap_err();
        assert_eq!(err, CANCELLED, "the stop must abort the step");
        assert_eq!(
            handle.join().unwrap(),
            1,
            "a stopped step must not spend a second billed request"
        );
    }

    /// Some OpenAI-compatible endpoints refuse `stream: true`. Losing the
    /// cancellation seam beats losing the turn, so the step retries as the
    /// blocking POST — but only because the stream never OPENED. A stopped turn
    /// skips the retry instead of paying for a step nobody will read.
    #[test]
    fn tools_step_falls_back_when_the_stream_never_opens() {
        let refused = "HTTP/1.1 400 Bad Request\r\nconnection: close\r\n\
                       content-length: 21\r\n\r\nstreaming unsupported"
            .to_string();
        let body = r#"{"choices":[{"message":{"content":"hi"},"finish_reason":"stop"}]}"#;
        let ok = format!(
            "HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        );
        let (addr, handle) = mock_http(vec![refused.clone(), ok]);
        let cfg = Config::from_parts(Provider::Custom, "k".into(), "m".into(), addr);
        let out = complete_tools_step(&cfg, "s", &[AgentMsg::User("q".into())], &[], 64, &|| false)
            .expect("the blocking form answers");
        assert_eq!(out.text, "hi");
        assert_eq!(handle.join().unwrap(), 2, "stream refused, then blocking");

        let (addr, handle) = mock_http(vec![refused]);
        let cfg = Config::from_parts(Provider::Custom, "k".into(), "m".into(), addr);
        let err = complete_tools_step(&cfg, "s", &[AgentMsg::User("q".into())], &[], 64, &|| true)
            .unwrap_err();
        assert_eq!(err, CANCELLED);
        assert_eq!(handle.join().unwrap(), 1, "no fallback for a stopped turn");
    }

    /// An exploration step must stay tool-capable: only the closing answer is
    /// pinned to prose. Streaming a step with `tool_choice: none` would make the
    /// model answer from nothing instead of exploring.
    #[test]
    fn only_the_closing_answer_pins_tool_choice_none() {
        let cfg = Config::from_parts(Provider::Custom, "k".into(), "m".into(), "http://x".into());
        let msgs = [AgentMsg::User("q".into())];
        let parse = |wire| -> serde_json::Value {
            serde_json::from_str(&openai_tools_body(&cfg, "s", &msgs, &[], 64, wire)).unwrap()
        };
        let blocking = parse(Wire::Blocking);
        assert!(blocking.get("stream").is_none());
        assert!(blocking.get("tool_choice").is_none());

        let step = parse(Wire::Stream);
        assert_eq!(step["stream"], true);
        assert!(
            step.get("tool_choice").is_none(),
            "a streamed step still calls tools"
        );

        let answer = parse(Wire::StreamAnswer);
        assert_eq!(answer["stream"], true);
        assert_eq!(answer["tool_choice"], "none");

        let anthropic = Config::from_parts(
            Provider::Anthropic,
            "k".into(),
            "m".into(),
            "http://x".into(),
        );
        let step: serde_json::Value = serde_json::from_str(&anthropic_tools_body(
            &anthropic,
            "s",
            &msgs,
            &[],
            64,
            Wire::Stream,
        ))
        .unwrap();
        assert_eq!(step["stream"], true);
        assert!(step.get("tool_choice").is_none());
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

    /// A configured host must not be able to hand the user's provider key to
    /// another one. ureq follows 3xx by default and copies every header except
    /// `authorization`/`cookie` to the new URL with no host check, so
    /// Anthropic's `x-api-key` rode along; a 302 also rewrites the POST to a
    /// GET, so nothing about the request looked unusual afterwards.
    #[test]
    fn a_redirect_never_carries_the_api_key_to_another_host() {
        // The host the redirect points at. It records anything it is ever sent,
        // so a followed redirect is caught by evidence and not by absence.
        let sink = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let sink_addr = sink.local_addr().unwrap();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let recorder = seen.clone();
        std::thread::spawn(move || {
            for conn in sink.incoming() {
                let Ok(mut conn) = conn else { break };
                let mut buf = [0u8; 4096];
                let n = std::io::Read::read(&mut conn, &mut buf).unwrap_or(0);
                recorder
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buf[..n]).into_owned());
                let _ = std::io::Write::write_all(
                    &mut conn,
                    b"HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-length: 2\r\n\r\nok",
                );
            }
        });

        let moved = format!(
            "HTTP/1.1 302 Found\r\nlocation: http://{sink_addr}/v1/messages\r\n\
             connection: close\r\ncontent-length: 0\r\n\r\n"
        );
        let (addr, handle) = mock_http(vec![moved]);
        let err = send(
            blocking_agent().post(&addr).set("x-api-key", "SECRET-KEY"),
            "{}",
            "Anthropic",
        )
        .expect_err("a 3xx is not a completion");

        assert!(err.contains("redirected"), "the refusal is reported: {err}");
        assert_eq!(handle.join().unwrap(), 1, "the redirect is not followed");
        // Give a wrongly-followed redirect time to land before concluding it
        // did not happen: the hop would be on the client's thread, which has
        // already returned, but the accept on the sink is on another one.
        std::thread::sleep(std::time::Duration::from_millis(200));
        let seen = seen.lock().unwrap();
        assert!(
            seen.is_empty(),
            "the redirect target was contacted at all: {seen:?}"
        );
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

    /// A provider's env var is a secret for that provider's host. Pointed at a
    /// gateway, proxy or local server the user deliberately left keyless, clew
    /// must read as unconfigured rather than send the real key there as
    /// `x-api-key` / `Bearer`.
    #[test]
    fn env_key_is_inherited_only_by_the_providers_own_endpoint() {
        let _env = crate::env_lock();
        let dir = std::env::temp_dir().join("clew-llm-env-key-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // SAFETY: env mutation serialized by env_lock.
        unsafe {
            std::env::set_var("CLEW_DATA_DIR", &dir);
            std::env::set_var("ANTHROPIC_API_KEY", "sk-ant-env");
            std::env::set_var("OPENAI_API_KEY", "sk-oai-env");
            std::env::set_var("DEEPSEEK_API_KEY", "sk-ds-env");
        }
        let path = dir.join("config.toml");
        let write = |toml: String| std::fs::write(&path, toml).unwrap();
        let foreign = "http://localhost:11434/v1"; // an Ollama-style relay

        // Every provider that has an env fallback, each of its branches.
        for (slug, env_key, own_url) in [
            ("anthropic", "sk-ant-env", "https://api.anthropic.com"),
            ("openai", "sk-oai-env", "https://api.openai.com/v1"),
            ("deepseek", "sk-ds-env", "https://api.deepseek.com/v1"),
        ] {
            // Its own endpoint, left implicit, inherits the env key.
            write(format!("[llm]\nprovider = \"{slug}\"\n"));
            let c = Config::load().expect("the provider's own endpoint is configured");
            assert_eq!(c.api_key, env_key, "{slug}: implicit default endpoint");
            assert_eq!(c.base_url, own_url, "{slug}");

            // Spelled out, trailing slash and all — compared after the trim.
            write(format!(
                "[llm]\nprovider = \"{slug}\"\nbase_url = \"{own_url}/\"\n"
            ));
            let c = Config::load().expect("still the provider's own host");
            assert_eq!(c.api_key, env_key, "{slug}: trailing slash");

            // A foreign endpoint with no key of its own is simply unconfigured.
            write(format!(
                "[llm]\nprovider = \"{slug}\"\nbase_url = \"{foreign}\"\n"
            ));
            assert!(
                Config::load().is_none(),
                "{slug}: the env key must not travel to a foreign endpoint"
            );

            // The same endpoint with its own key still works, and uses it.
            write(format!(
                "[llm]\nprovider = \"{slug}\"\napi_key = \"sk-local\"\nbase_url = \"{foreign}\"\n"
            ));
            let c = Config::load().expect("an explicit key configures the endpoint");
            assert_eq!(c.api_key, "sk-local", "{slug}: explicit key wins");
        }

        // Custom has no env var of its own and must not borrow another
        // provider's, even when aimed straight at that provider's host.
        write("[llm]\nprovider = \"custom\"\nbase_url = \"https://api.anthropic.com\"\n".into());
        assert!(
            Config::load().is_none(),
            "Custom must not inherit ANTHROPIC_API_KEY"
        );

        // An unknown slug resolves to Anthropic, so it must be judged as
        // Anthropic — a foreign base_url still declines.
        write(format!(
            "[llm]\nprovider = \"acme\"\nbase_url = \"{foreign}\"\n"
        ));
        assert!(
            Config::load().is_none(),
            "an unknown provider falls back to Anthropic and must decline too"
        );

        // The legacy file field is unaffected: it is stored, not inherited.
        write(format!(
            "[llm]\nanthropic_api_key = \"sk-old\"\nbase_url = \"{foreign}\"\n"
        ));
        assert_eq!(Config::load().expect("legacy key").api_key, "sk-old");

        // SAFETY: env mutation serialized by env_lock.
        unsafe {
            std::env::remove_var("CLEW_DATA_DIR");
            std::env::remove_var("ANTHROPIC_API_KEY");
            std::env::remove_var("OPENAI_API_KEY");
            std::env::remove_var("DEEPSEEK_API_KEY");
        }
    }
}
