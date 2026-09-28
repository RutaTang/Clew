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
//! (see `env_key_for_endpoint`).
//!
//! ## Transport
//!
//! Every request is built by one function per concern — `prepare` (endpoint,
//! headers), `send_with_retry` (the one retry policy), `agent_for` (the
//! HTTP agent) — instead of per entry point:
//!
//! - redirects are never followed (see `send_with_retry`);
//! - the transport is clew-core's one (`crate::net::agent`): TLS trusts the
//!   bundled Mozilla roots AND the operating system's store, so a private or
//!   corporate CA works without breaking hosts that have no store; the
//!   standard proxy variables are honoured — and when they name none, the
//!   system's proxy settings — with `NO_PROXY`, the system's exceptions and a
//!   loopback bypass; every connection is metered, plain http as much as
//!   https;
//! - connects are bounded, streams have an idle limit, and blocking calls an
//!   overall deadline (`Limits`);
//! - every request runs on a worker thread, and the caller only waits on a
//!   channel while polling its `cancelled` predicate, so a stop is honoured
//!   within `CANCEL_POLL` whatever the socket is doing — and the abandoned
//!   worker's connection is shut down then too, which is what makes the
//!   provider stop generating (`crate::net::Line`);
//! - the embeddings client (`crate::embed`) goes through the same agents
//!   (`agent_for` with `Limits::direct`), so one policy covers every call
//!   to a model provider.
//!
//! ## Extended thinking
//!
//! No request enables Anthropic's extended thinking (`thinking` is never set),
//! so no response carries `thinking` / `redacted_thinking` blocks and nothing
//! here round-trips them. Enabling it would need that round trip back: in a
//! tool loop the API requires each step's thinking blocks to be returned
//! verbatim, signature included.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::io::{BufRead, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde_json::{Value, json};

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
    /// `env_key_for_endpoint` decides. Nothing should call
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
    /// travel to the provider's own endpoint (see `env_key_for_endpoint`),
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
    pub parameters: Value,
}

/// One tool invocation the model requested.
#[derive(Debug, Clone)]
pub struct ToolCall {
    /// Provider-assigned id correlating the result back to this call.
    pub id: String,
    pub name: String,
    pub args: Value,
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
    },
    /// The result of one earlier tool call, fed back to the model.
    ToolResult {
        call: ToolCall,
        content: String,
    },
}

/// What one agent step produced: prose, and the tool calls to run next (empty
/// when the model is done exploring).
#[derive(Debug, Clone, Default)]
pub struct StepOutput {
    pub text: String,
    pub calls: Vec<ToolCall>,
    /// The step hit its output budget mid-response (Anthropic `stop_reason:
    /// "max_tokens"`, OpenAI `finish_reason: "length"`). Any tool calls in a
    /// truncated step may have half-written arguments and must not be executed;
    /// the caller should retry with a larger budget.
    pub truncated: bool,
    /// Why the provider stopped (`end_turn`, `tool_use`, `stop`, `length`,
    /// `refusal`, `content_filter`, …), when it said.
    pub stop_reason: Option<String>,
}

/// A finished text completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    pub text: String,
    /// The answer was cut off by the output budget (see [`StepOutput::truncated`]).
    pub truncated: bool,
    pub stop_reason: Option<String>,
}

/// Appended to an answer the provider cut off at its output limit, so a reader
/// can tell a truncated answer from a complete one. Plain text on purpose: it
/// is shown both as markdown (Ask) and verbatim (explanations).
pub const TRUNCATED_NOTE: &str = "\n\n(truncated: the model reached its output limit)";

impl Completion {
    /// The text, with [`TRUNCATED_NOTE`] appended when it was cut off — for
    /// callers whose only channel back is the text itself.
    pub fn into_text_with_note(self) -> String {
        if self.truncated {
            format!("{}{TRUNCATED_NOTE}", self.text)
        } else {
            self.text
        }
    }
}

/// Whether a provider's stop reason means the output budget cut the answer.
/// The values do not collide across dialects, so one predicate serves both.
fn is_truncation(reason: &str) -> bool {
    matches!(
        reason,
        "max_tokens" | "length" | "model_context_window_exceeded"
    )
}

// -- errors ------------------------------------------------------------------

/// Why a model request failed.
///
/// Typed because what the caller may safely do next depends on it. A refusal
/// the provider answered before generating anything (a status) may be retried
/// in another form; a failure after the request reached the provider must not
/// be — these requests carry no idempotency key, so a resend is billed twice
/// and generates twice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LlmError {
    /// The caller's `cancelled` predicate fired. Displays as [`CANCELLED`].
    Cancelled,
    /// Refused before any I/O: this configuration cannot make a request.
    Config(String),
    /// The request never reached the provider (DNS, connect, proxy), even
    /// after the retries such failures get.
    Connect(String),
    /// The provider answered with an HTTP error status.
    Status {
        who: String,
        code: u16,
        /// The provider's own error code or type (`invalid_api_key`,
        /// `authentication_error`, `rate_limit_error`, …), when it gave one.
        kind: Option<String>,
        message: String,
    },
    /// The endpoint answered with a redirect, which is never followed (see
    /// `send_with_retry`).
    Redirected { who: String, location: String },
    /// The request was on the wire and the transport failed afterwards —
    /// while waiting for the response or mid-body. The provider may be
    /// generating against it, so this is never resent.
    Transport(String),
    /// The provider reported a failure inside a 200 stream (overload, quota…).
    Stream(String),
    /// The response arrived but could not be understood.
    Protocol(String),
}

impl fmt::Display for LlmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LlmError::Cancelled => f.write_str(CANCELLED),
            LlmError::Config(m) | LlmError::Protocol(m) => f.write_str(m),
            LlmError::Connect(m) | LlmError::Transport(m) => write!(f, "request failed: {m}"),
            // The status code and the provider's error kind both stay in the
            // text: callers that only see a string (the explain pass's
            // auth-failure check) match on them.
            LlmError::Status {
                who,
                code,
                kind: Some(kind),
                message,
            } => write!(f, "{who} API error {code} ({kind}): {message}"),
            LlmError::Status {
                who,
                code,
                kind: None,
                message,
            } => write!(f, "{who} API error {code}: {message}"),
            LlmError::Redirected { who, location } => write!(
                f,
                "{who} endpoint redirected to {location}; not following, because the \
                 API key would travel with it — point base_url at the real endpoint"
            ),
            LlmError::Stream(m) => write!(f, "stream error: {m}"),
        }
    }
}

impl std::error::Error for LlmError {}

impl From<LlmError> for String {
    fn from(e: LlmError) -> String {
        e.to_string()
    }
}

impl LlmError {
    /// Whether a failure to OPEN a stream says the endpoint refuses streaming
    /// itself, so the same request may be sent again unstreamed.
    ///
    /// Only a 400/422 status qualifies — how OpenAI-compatible servers without
    /// SSE (or without `tool_choice: "none"`) reject the body. A status proves
    /// the provider refused the request instead of starting on it. A transport
    /// error proves nothing of the kind: the stream's read timeout also covers
    /// the wait for the response headers, so a provider that was slow to its
    /// first byte used to look exactly like one that refused, and got the same
    /// request a second time, billed twice.
    pub fn stream_refused(&self) -> bool {
        matches!(
            self,
            LlmError::Status {
                code: 400 | 422,
                ..
            }
        )
    }
}

// -- public entry points -----------------------------------------------------

/// How a tool-conversation request goes on the wire. The body is otherwise
/// identical.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wire {
    /// One blocking POST.
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
/// The POST runs on a worker thread and `cancelled` is polled while it is out,
/// so the CALLER is released within `CANCEL_POLL` of a stop, and the
/// worker's connection is shut down then too (`crate::net::Line`) — which is
/// what stops the provider generating (and billing). Callers that can be
/// stopped still want [`complete_tools_step`], which only falls back to this
/// for endpoints that refuse to stream.
pub fn complete_tools(
    cfg: &Config,
    system: &str,
    messages: &[AgentMsg],
    tools: &[ToolDef],
    max_tokens: u32,
    cancelled: &dyn Fn() -> bool,
) -> Result<StepOutput, LlmError> {
    let body = tools_body(cfg, system, messages, tools, max_tokens, Wire::Blocking);
    let prepared = prepare(cfg, body, BLOCKING_LIMITS)?;
    let text = run_blocking(prepared, cancelled)?;
    parse_response(cfg.provider.dialect(), &text)
}

/// One exploration step of a tool conversation, issued as a STREAM so that it
/// can be stopped. The step's value is still the whole [`StepOutput`] — nothing
/// is forwarded token by token — the stream is here for the seam: when
/// `cancelled` fires, the caller returns at once and the connection is shut
/// down (`crate::net::Line`), which is what actually ends (and stops billing)
/// a generation the user abandoned.
/// Blocking.
///
/// Falls back to [`complete_tools`] only when the endpoint REFUSED the stream
/// with a status ([`LlmError::stream_refused`]) — some OpenAI-compatible
/// servers reject `stream: true`, and losing the seam beats losing the turn.
/// Never after a transport failure: the request may already be generating, and
/// a resend is billed twice.
pub fn complete_tools_step(
    cfg: &Config,
    system: &str,
    messages: &[AgentMsg],
    tools: &[ToolDef],
    max_tokens: u32,
    cancelled: &dyn Fn() -> bool,
) -> Result<StepOutput, LlmError> {
    let body = tools_body(cfg, system, messages, tools, max_tokens, Wire::Stream);
    let prepared = prepare(cfg, body, STREAM_LIMITS)?;
    match stream_request(prepared, cfg.provider.dialect(), &mut |_| {}, cancelled) {
        // A stopped turn must not spend a second request on the fallback.
        Err(e) if e.stream_refused() && !cancelled() => {
            complete_tools(cfg, system, messages, tools, max_tokens, cancelled)
        }
        other => other,
    }
}

/// Stream the closing step of a tool conversation: tools are still declared
/// (the transcript's tool blocks require them) but `tool_choice: none` pins the
/// model to prose, and each text token is forwarded through `on_delta` as it
/// arrives. Blocking; `cancelled` behaves as in [`complete_tools_step`].
///
/// The whole [`StepOutput`] comes back, tool calls included: some
/// OpenAI-compatible endpoints ignore `tool_choice: none` and stream a tool
/// call instead of prose. Reading only the text turned that into an empty
/// "answer"; the caller now sees the calls and can let the model keep
/// exploring. No fallback here — a 400/422 (an endpoint that rejects
/// `tool_choice: "none"`) is the caller's to route.
pub fn complete_tools_stream(
    cfg: &Config,
    system: &str,
    messages: &[AgentMsg],
    tools: &[ToolDef],
    max_tokens: u32,
    mut on_delta: impl FnMut(&str),
    cancelled: &dyn Fn() -> bool,
) -> Result<StepOutput, LlmError> {
    let body = tools_body(cfg, system, messages, tools, max_tokens, Wire::StreamAnswer);
    let prepared = prepare(cfg, body, STREAM_LIMITS)?;
    stream_request(prepared, cfg.provider.dialect(), &mut on_delta, cancelled)
}

/// A multi-turn completion: the whole conversation is sent so the model can
/// resolve follow-ups ("it", "that function") against earlier turns. Blocking.
///
/// A truncated answer comes back with [`TRUNCATED_NOTE`] appended — the text
/// is this API's only channel. Not cancellable, so test-only: every caller
/// can be abandoned (an answer the user may stop, a task that may be aborted)
/// and calls [`complete_chat_full`] with a live `cancelled`, which also
/// reports truncation as a flag.
#[cfg(test)]
pub fn complete_chat(
    cfg: &Config,
    system: &str,
    messages: &[ChatMsg],
    max_tokens: u32,
) -> Result<String, String> {
    complete_chat_full(cfg, system, messages, max_tokens, &|| false)
        .map(Completion::into_text_with_note)
        .map_err(String::from)
}

/// A multi-turn completion (the whole conversation is sent, so the model can
/// resolve follow-ups against earlier turns), with a typed result and a
/// cancellation seam: the POST
/// runs on a worker thread and `cancelled` is polled while it is out (see
/// [`complete_tools`] for what that can and cannot stop).
pub fn complete_chat_full(
    cfg: &Config,
    system: &str,
    messages: &[ChatMsg],
    max_tokens: u32,
    cancelled: &dyn Fn() -> bool,
) -> Result<Completion, LlmError> {
    let body = chat_body(cfg, system, messages, max_tokens, false);
    let prepared = prepare(cfg, body, BLOCKING_LIMITS)?;
    let text = run_blocking(prepared, cancelled)?;
    let out = parse_response(cfg.provider.dialect(), &text)?;
    if out.text.is_empty() && !out.truncated {
        return Err(LlmError::Protocol(match out.stop_reason {
            Some(reason) => format!("no text in response (stop reason: {reason})"),
            None => "no text in response".into(),
        }));
    }
    Ok(Completion {
        text: out.text,
        truncated: out.truncated,
        stop_reason: out.stop_reason,
    })
}

/// The error a stream returns when `cancelled` asked it to stop. Callers use
/// it to tell "the user abandoned this answer" from a real failure — the
/// partial text is deliberately discarded rather than returned as if the
/// answer had completed.
pub const CANCELLED: &str = "cancelled";

/// A streaming multi-turn completion: `on_delta` is called with each token as
/// it arrives (Server-Sent Events), and the full text is returned at the end.
///
/// `cancelled` is polled while waiting, so an abandoned answer stops costing
/// money: the caller is released within `CANCEL_POLL`, and the connection is
/// shut down (`crate::net::Line`). A truncated answer ends with
/// [`TRUNCATED_NOTE`], delivered through `on_delta` too so a reader sees it.
/// Blocking; run off the async runtime.
pub fn complete_chat_stream(
    cfg: &Config,
    system: &str,
    messages: &[ChatMsg],
    max_tokens: u32,
    mut on_delta: impl FnMut(&str),
    cancelled: &dyn Fn() -> bool,
) -> Result<String, String> {
    let done =
        complete_chat_stream_full(cfg, system, messages, max_tokens, &mut on_delta, cancelled)
            .map_err(String::from)?;
    if done.truncated {
        on_delta(TRUNCATED_NOTE);
    }
    Ok(done.into_text_with_note())
}

/// [`complete_chat_stream`] with a typed result; it reports truncation as a
/// flag and leaves any note to the caller.
pub fn complete_chat_stream_full(
    cfg: &Config,
    system: &str,
    messages: &[ChatMsg],
    max_tokens: u32,
    on_delta: &mut dyn FnMut(&str),
    cancelled: &dyn Fn() -> bool,
) -> Result<Completion, LlmError> {
    let body = chat_body(cfg, system, messages, max_tokens, true);
    let prepared = prepare(cfg, body, STREAM_LIMITS)?;
    let out = stream_request(prepared, cfg.provider.dialect(), on_delta, cancelled)?;
    Ok(Completion {
        text: out.text,
        truncated: out.truncated,
        stop_reason: out.stop_reason,
    })
}

// -- provider dialects -------------------------------------------------------

/// The two wire dialects every provider speaks one of: Anthropic's Messages
/// API, or the OpenAI-compatible `/chat/completions` (OpenAI, DeepSeek and any
/// Custom endpoint).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dialect {
    Anthropic,
    OpenAi,
}

impl Provider {
    fn dialect(self) -> Dialect {
        match self {
            Provider::Anthropic => Dialect::Anthropic,
            Provider::OpenAI | Provider::DeepSeek | Provider::Custom => Dialect::OpenAi,
        }
    }
}

/// A request ready to send: the agent and endpoint, its headers, the JSON
/// body, the provider's name for messages, and the proxy it goes through.
struct Prepared {
    agent: ureq::Agent,
    /// The endpoint, as ureq is handed it (`crate::net::request_uri`).
    uri: ureq::http::Uri,
    headers: Vec<(&'static str, String)>,
    body: String,
    who: String,
    via: Option<crate::net::Via>,
}

impl Prepared {
    /// POST the request once — each attempt of [`send_with_retry`] builds it
    /// afresh — with a panic inside ureq turned into an error
    /// (`crate::net::guarded`).
    fn send(&self) -> Result<ureq::http::Response<ureq::Body>, ureq::Error> {
        crate::net::guarded(|| {
            let mut request = self.agent.post(self.uri.clone());
            for (name, value) in &self.headers {
                request = request.header(*name, value.as_str());
            }
            request.send(self.body.as_str())
        })
    }
}

/// Address and dress one request for `cfg`'s provider. The ONE place that
/// knows each dialect's path and headers — they used to be spelled out at every
/// entry point.
fn prepare(cfg: &Config, body: String, limits: Limits) -> Result<Prepared, LlmError> {
    let base = cfg.base_url.trim().trim_end_matches('/');
    if base.is_empty() {
        return Err(LlmError::Config("no base URL set for this provider".into()));
    }
    let dialect = cfg.provider.dialect();
    let url = match dialect {
        Dialect::Anthropic => format!("{base}/v1/messages"),
        Dialect::OpenAi => format!("{base}/chat/completions"),
    };
    let (agent, via) = agent_for(&url, limits)?;
    let uri = crate::net::request_uri(&url).map_err(LlmError::Config)?;
    let mut headers = vec![("content-type", "application/json".to_string())];
    let who = match dialect {
        Dialect::Anthropic => {
            headers.push(("x-api-key", cfg.api_key.clone()));
            headers.push(("anthropic-version", API_VERSION.to_string()));
            "Anthropic".to_string()
        }
        Dialect::OpenAi => {
            headers.push(("Authorization", format!("Bearer {}", cfg.api_key)));
            cfg.provider.label().to_string()
        }
    };
    Ok(Prepared {
        agent,
        uri,
        headers,
        body,
        who,
        via,
    })
}

/// Which field carries the output budget. Newer OpenAI models (gpt-5*,
/// o-series) reject the classic `max_tokens` and require
/// `max_completion_tokens`; classic chat models and most compatible servers
/// still take `max_tokens`. The one copy of that heuristic.
fn max_tokens_field(model: &str) -> &'static str {
    let m = model.to_ascii_lowercase();
    if ["gpt-5", "o1", "o3", "o4"].iter().any(|p| m.starts_with(p)) {
        "max_completion_tokens"
    } else {
        "max_tokens"
    }
}

/// Render the conversation as provider-agnostic `{role, content}` JSON objects.
fn json_messages(messages: &[ChatMsg]) -> Vec<Value> {
    messages
        .iter()
        .map(|m| json!({ "role": m.role_str(), "content": m.content }))
        .collect()
}

/// The body of a plain (tool-less) chat completion in `cfg`'s dialect.
fn chat_body(
    cfg: &Config,
    system: &str,
    messages: &[ChatMsg],
    max_tokens: u32,
    stream: bool,
) -> String {
    let mut body = match cfg.provider.dialect() {
        Dialect::Anthropic => json!({
            "model": cfg.model,
            "max_tokens": max_tokens,
            "system": system,
            "messages": json_messages(messages),
        }),
        Dialect::OpenAi => {
            let mut msgs = vec![json!({ "role": "system", "content": system })];
            msgs.extend(json_messages(messages));
            let mut body = json!({ "model": cfg.model, "messages": msgs });
            body[max_tokens_field(&cfg.model)] = max_tokens.into();
            body
        }
    };
    if stream {
        body["stream"] = true.into();
    }
    body.to_string()
}

/// The body of a tool-conversation request in `cfg`'s dialect.
fn tools_body(
    cfg: &Config,
    system: &str,
    messages: &[AgentMsg],
    tools: &[ToolDef],
    max_tokens: u32,
    wire: Wire,
) -> String {
    match cfg.provider.dialect() {
        Dialect::Anthropic => anthropic_tools_body(cfg, system, messages, tools, max_tokens, wire),
        Dialect::OpenAi => openai_tools_body(cfg, system, messages, tools, max_tokens, wire),
    }
}

/// Put an ephemeral `cache_control` breakpoint on the last content block of the
/// last message. A tool loop re-sends the whole growing conversation every
/// step; a breakpoint at the tail lets the next step read this step's prefix
/// from the provider's prompt cache instead of re-processing it, which cuts
/// both cost and latency roughly in proportion to the conversation length. The
/// system prompt carries its own breakpoint (covering the tools+system prefix).
/// String content is lifted into a block array, the shape per-block fields need.
fn mark_tail_cache(msgs: &mut [Value]) {
    let Some(last) = msgs.last_mut() else { return };
    let content = &mut last["content"];
    if let Some(text) = content.as_str() {
        if text.is_empty() {
            return; // the API rejects empty text blocks; nothing worth caching
        }
        *content = json!([{ "type": "text", "text": text }]);
    }
    if let Some(block) = content.as_array_mut().and_then(|b| b.last_mut()) {
        block["cache_control"] = json!({ "type": "ephemeral" });
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
    let mut msgs: Vec<Value> = Vec::new();
    for m in messages {
        match m {
            AgentMsg::User(text) => {
                msgs.push(json!({ "role": "user", "content": text }));
            }
            AgentMsg::Assistant { text, calls } => {
                let mut blocks: Vec<Value> = Vec::new();
                if !text.is_empty() {
                    blocks.push(json!({ "type": "text", "text": text }));
                }
                for c in calls {
                    blocks.push(json!({
                        "type": "tool_use", "id": c.id, "name": c.name, "input": c.args,
                    }));
                }
                msgs.push(json!({ "role": "assistant", "content": blocks }));
            }
            AgentMsg::ToolResult { call, content } => {
                let block = json!({
                    "type": "tool_result", "tool_use_id": call.id, "content": content,
                });
                match msgs.last_mut() {
                    Some(last)
                        if last["role"] == "user"
                            && last["content"][0]["type"] == "tool_result" =>
                    {
                        if let Some(blocks) = last["content"].as_array_mut() {
                            blocks.push(block);
                        }
                    }
                    _ => msgs.push(json!({ "role": "user", "content": [block] })),
                }
            }
        }
    }
    let tool_defs: Vec<Value> = tools
        .iter()
        .map(|t| json!({ "name": t.name, "description": t.description, "input_schema": t.parameters }))
        .collect();
    mark_tail_cache(&mut msgs);
    let mut body = json!({
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
        body["tool_choice"] = json!({ "type": "none" });
    }
    body.to_string()
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
    let mut msgs = vec![json!({ "role": "system", "content": system })];
    for m in messages {
        match m {
            AgentMsg::User(text) => {
                msgs.push(json!({ "role": "user", "content": text }));
            }
            AgentMsg::Assistant { text, calls } => {
                let mut msg = json!({ "role": "assistant" });
                msg["content"] = if text.is_empty() {
                    Value::Null
                } else {
                    text.clone().into()
                };
                if !calls.is_empty() {
                    let tc: Vec<Value> = calls
                        .iter()
                        .map(|c| {
                            json!({
                                "id": c.id,
                                "type": "function",
                                "function": { "name": c.name, "arguments": c.args.to_string() },
                            })
                        })
                        .collect();
                    msg["tool_calls"] = tc.into();
                }
                msgs.push(msg);
            }
            AgentMsg::ToolResult { call, content } => {
                msgs.push(json!({ "role": "tool", "tool_call_id": call.id, "content": content }));
            }
        }
    }
    let tool_defs: Vec<Value> = tools
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "function": { "name": t.name, "description": t.description, "parameters": t.parameters },
            })
        })
        .collect();
    let mut body = json!({ "model": cfg.model, "messages": msgs, "tools": tool_defs });
    body[max_tokens_field(&cfg.model)] = max_tokens.into();
    if wire != Wire::Blocking {
        body["stream"] = true.into();
    }
    if wire == Wire::StreamAnswer {
        body["tool_choice"] = "none".into();
    }
    body.to_string()
}

/// Parse a non-streamed response (text, tool calls, stop reason) in either
/// dialect.
fn parse_response(dialect: Dialect, text: &str) -> Result<StepOutput, LlmError> {
    let json: Value = serde_json::from_str(text)
        .map_err(|e| LlmError::Protocol(format!("bad JSON response: {e}")))?;
    let str_of = |v: &Value, key: &str| {
        v.get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let mut out = StepOutput::default();
    match dialect {
        Dialect::Anthropic => {
            // Every text block, in order: a response can carry several, and
            // taking the first alone silently dropped the rest of an answer.
            for block in json
                .get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => out.text.push_str(&str_of(block, "text")),
                    Some("tool_use") => out.calls.push(ToolCall {
                        id: str_of(block, "id"),
                        name: str_of(block, "name"),
                        args: block.get("input").cloned().unwrap_or_else(|| json!({})),
                    }),
                    // `thinking` blocks cannot occur: extended thinking is
                    // never requested (see the module docs).
                    _ => {}
                }
            }
            out.stop_reason = json
                .get("stop_reason")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        Dialect::OpenAi => {
            let msg = json
                .pointer("/choices/0/message")
                .ok_or_else(|| LlmError::Protocol("no message in response".into()))?;
            out.text = str_of(msg, "content");
            for tc in msg
                .get("tool_calls")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                // Arguments arrive as a JSON string; malformed ones read as
                // empty rather than failing the step.
                let args = tc
                    .pointer("/function/arguments")
                    .and_then(Value::as_str)
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or_else(|| json!({}));
                out.calls.push(ToolCall {
                    id: str_of(tc, "id"),
                    name: tc
                        .pointer("/function/name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    args,
                });
            }
            out.stop_reason = json
                .pointer("/choices/0/finish_reason")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
    }
    out.truncated = out.stop_reason.as_deref().is_some_and(is_truncation);
    out.text = out.text.trim().to_string();
    Ok(out)
}

// -- streamed responses ------------------------------------------------------

/// A tool call being reassembled from a stream's fragments.
#[derive(Default)]
struct CallFragment {
    id: String,
    name: String,
    /// Joined argument JSON: only the concatenation parses.
    args: String,
}

/// Largest response one stream may reassemble — its text and every tool
/// call's id, name and arguments together: the bound a blocking body has
/// ([`MAX_BODY_BYTES`]). [`MAX_SSE_LINE`] bounds one line only, so a stream of
/// small events used to grow the answer, and every tool call's arguments,
/// without limit.
const MAX_STREAM_BYTES: usize = MAX_BODY_BYTES as usize;

/// Most tool calls one streamed step may open. Each is a map entry keyed by
/// the index the stream names, and an entry costs memory whether or not
/// anything is appended to it, so the byte budget alone cannot bound them. A
/// real step makes a handful.
const MAX_STREAM_CALLS: usize = 256;

/// Incremental reassembly of one streamed response, for either dialect.
struct StreamState {
    text: String,
    /// Tool calls by the stream's block (Anthropic) or call (OpenAI) index —
    /// the order they must be replayed in.
    calls: BTreeMap<u64, CallFragment>,
    stop_reason: Option<String>,
    /// Bytes reassembled so far, against `max_bytes`.
    bytes: usize,
    max_bytes: usize,
    max_calls: usize,
}

impl Default for StreamState {
    fn default() -> Self {
        StreamState::within(MAX_STREAM_BYTES, MAX_STREAM_CALLS)
    }
}

impl StreamState {
    fn within(max_bytes: usize, max_calls: usize) -> Self {
        StreamState {
            text: String::new(),
            calls: BTreeMap::new(),
            stop_reason: None,
            bytes: 0,
            max_bytes,
            max_calls,
        }
    }

    /// Apply one parsed event, forwarding any prose to `on_text`. An error
    /// ends the stream: it outgrew what one response may be.
    fn apply(
        &mut self,
        dialect: Dialect,
        json: &Value,
        on_text: &mut dyn FnMut(&str),
    ) -> Result<(), LlmError> {
        match dialect {
            Dialect::Anthropic => self.apply_anthropic(json, on_text),
            Dialect::OpenAi => self.apply_openai(json, on_text),
        }
    }

    /// Account for `n` more bytes of response, BEFORE they are kept (or
    /// forwarded).
    fn charge(&mut self, n: usize) -> Result<(), LlmError> {
        self.bytes = self.bytes.saturating_add(n);
        if self.bytes > self.max_bytes {
            return Err(LlmError::Protocol(format!(
                "the streamed response exceeded {} MB",
                (self.max_bytes >> 20).max(1)
            )));
        }
        Ok(())
    }

    /// The call at stream index `idx`, opened if it is new — unless that
    /// would be one call too many.
    fn call(&mut self, idx: u64) -> Result<&mut CallFragment, LlmError> {
        if !self.calls.contains_key(&idx) && self.calls.len() >= self.max_calls {
            return Err(LlmError::Protocol(format!(
                "the stream opened more than {} tool calls",
                self.max_calls
            )));
        }
        Ok(self.calls.entry(idx).or_default())
    }

    fn apply_anthropic(
        &mut self,
        json: &Value,
        on_text: &mut dyn FnMut(&str),
    ) -> Result<(), LlmError> {
        let idx = json.get("index").and_then(Value::as_u64).unwrap_or(0);
        let str_at = |ptr: &str| {
            json.pointer(ptr)
                .and_then(Value::as_str)
                .unwrap_or_default()
        };
        match json.get("type").and_then(Value::as_str) {
            Some("content_block_start") => match str_at("/content_block/type") {
                "tool_use" => {
                    let (id, name) = (str_at("/content_block/id"), str_at("/content_block/name"));
                    self.charge(id.len() + name.len())?;
                    let call = self.call(idx)?;
                    call.id = id.to_string();
                    call.name = name.to_string();
                }
                "text" => self.push_text(str_at("/content_block/text"), on_text)?,
                _ => {}
            },
            Some("content_block_delta") => match str_at("/delta/type") {
                "text_delta" => self.push_text(str_at("/delta/text"), on_text)?,
                // Pieces of one JSON document: only the concatenation parses.
                // A call with no arguments streams none at all (`{}` below).
                "input_json_delta" => {
                    let part = str_at("/delta/partial_json");
                    if self.calls.contains_key(&idx) {
                        self.charge(part.len())?;
                        if let Some(call) = self.calls.get_mut(&idx) {
                            call.args.push_str(part);
                        }
                    }
                }
                // `thinking_delta` / `signature_delta` cannot occur: extended
                // thinking is never requested (see the module docs).
                _ => {}
            },
            Some("message_delta") => {
                if let Some(reason) = json.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.stop_reason = Some(reason.to_string());
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn apply_openai(
        &mut self,
        json: &Value,
        on_text: &mut dyn FnMut(&str),
    ) -> Result<(), LlmError> {
        let Some(choice) = json.pointer("/choices/0") else {
            return Ok(());
        };
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.stop_reason = Some(reason.to_string());
        }
        if let Some(t) = choice.pointer("/delta/content").and_then(Value::as_str) {
            self.push_text(t, on_text)?;
        }
        for call in choice
            .pointer("/delta/tool_calls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            // `index` is what ties a call's fragments together. Only the
            // arguments are ever split: `id` and `name` arrive whole on the
            // first fragment, and some compatible servers repeat them on every
            // one, so these take the first non-empty value instead of joining
            // (joining yields `call_1call_1`, an id the provider then rejects
            // on the tool result).
            let idx = call.get("index").and_then(Value::as_u64).unwrap_or(0);
            let (has_id, has_name) = self
                .calls
                .get(&idx)
                .map_or((false, false), |c| (!c.id.is_empty(), !c.name.is_empty()));
            let id = call.get("id").and_then(Value::as_str).filter(|_| !has_id);
            let name = call
                .pointer("/function/name")
                .and_then(Value::as_str)
                .filter(|_| !has_name);
            let args = call.pointer("/function/arguments").and_then(Value::as_str);
            self.charge([id, name, args].iter().flatten().map(|s| s.len()).sum())?;
            let entry = self.call(idx)?;
            if let Some(id) = id {
                entry.id.push_str(id);
            }
            if let Some(name) = name {
                entry.name.push_str(name);
            }
            if let Some(args) = args {
                entry.args.push_str(args);
            }
        }
        Ok(())
    }

    fn push_text(&mut self, text: &str, on_text: &mut dyn FnMut(&str)) -> Result<(), LlmError> {
        if !text.is_empty() {
            self.charge(text.len())?;
            self.text.push_str(text);
            on_text(text);
        }
        Ok(())
    }

    fn finish(self) -> StepOutput {
        StepOutput {
            text: self.text.trim().to_string(),
            calls: self
                .calls
                .into_values()
                .map(|call| ToolCall {
                    id: call.id,
                    name: call.name,
                    // Malformed or absent arguments read as empty rather than
                    // failing the step, exactly as in the blocking parser; a
                    // truncated step's calls are never run anyway.
                    args: serde_json::from_str(&call.args).unwrap_or_else(|_| json!({})),
                })
                .collect(),
            truncated: self.stop_reason.as_deref().is_some_and(is_truncation),
            stop_reason: self.stop_reason,
        }
    }
}

/// Send `prepared` as a stream and reassemble the response. Prose is forwarded
/// to `on_text` as it arrives.
fn stream_request(
    prepared: Prepared,
    dialect: Dialect,
    on_text: &mut dyn FnMut(&str),
    cancelled: &dyn Fn() -> bool,
) -> Result<StepOutput, LlmError> {
    let stream = spawn_sse(move |abandoned| {
        send_with_retry(&prepared, abandoned)
            .map(|resp| -> Box<dyn Read + Send> { Box::new(resp.into_body().into_reader()) })
    })?;
    collect_stream(stream, dialect, on_text, cancelled)
}

/// Reassemble a stream that is already being read.
fn collect_stream(
    stream: SseStream,
    dialect: Dialect,
    on_text: &mut dyn FnMut(&str),
    cancelled: &dyn Fn() -> bool,
) -> Result<StepOutput, LlmError> {
    collect_into(StreamState::default(), stream, dialect, on_text, cancelled)
}

/// [`collect_stream`] into `state` (whose limits a test chooses).
fn collect_into(
    mut state: StreamState,
    stream: SseStream,
    dialect: Dialect,
    on_text: &mut dyn FnMut(&str),
    cancelled: &dyn Fn() -> bool,
) -> Result<StepOutput, LlmError> {
    let announced = read_sse_events(
        stream,
        |json| state.apply(dialect, json, on_text),
        cancelled,
    )?;
    // A healthy stream announces its end (`[DONE]`, `message_stop`) or at
    // least says why it stopped. EOF with neither means the connection dropped
    // mid-answer, which must not pass as a completed text.
    if !announced && state.stop_reason.is_none() {
        return Err(LlmError::Transport(
            "the stream ended before completion (connection dropped?)".into(),
        ));
    }
    Ok(state.finish())
}

/// One line's worth of an SSE stream, as the reader thread hands it over.
enum SsePiece {
    /// The payload of one `data:` line.
    Data(String),
    /// The body ended (EOF).
    End,
    /// Opening or reading failed; nothing follows.
    Failed(LlmError),
}

/// A stream being read on its own thread (see [`spawn_sse`]).
struct SseStream {
    rx: Receiver<SsePiece>,
    /// Dropping the stream tells the reader nobody is listening any more.
    _abandon: Abandon,
}

/// Start a stream on a dedicated reader thread: `open` sends the request (and
/// may retry — it is handed the abandon flag to stop between attempts), then
/// the thread reads `data:` lines into a bounded channel.
///
/// The thread is what makes cancellation reliable. The caller waits on the
/// channel with a timeout, so its `cancelled` predicate is polled every
/// [`CANCEL_POLL`] whatever the socket is doing — including while the request
/// waits for response headers, which a read on the caller's own thread could
/// never be interrupted in. Nothing here resumes a read after an I/O error: a
/// socket timeout ends the stream (see [`pump_sse`]).
fn spawn_sse(
    open: impl FnOnce(&AtomicBool) -> Result<Box<dyn Read + Send>, LlmError> + Send + 'static,
) -> Result<SseStream, LlmError> {
    let (rx, abandon) = spawn_worker(SSE_QUEUE, move |abandoned, tx| {
        let reader = match open(abandoned) {
            Ok(reader) => reader,
            Err(e) => {
                let _ = tx.send(SsePiece::Failed(e));
                return;
            }
        };
        pump_sse(reader, abandoned, tx);
    })?;
    Ok(SseStream {
        rx,
        _abandon: abandon,
    })
}

/// SSE lines buffered between the reader thread and its consumer. Small on
/// purpose: a slow consumer should push back on the socket, not grow memory.
const SSE_QUEUE: usize = 64;

/// Longest single SSE line accepted. Real events are a few hundred bytes (a
/// whole tool-argument document at most); an endless line is a broken or
/// hostile endpoint, and without a cap it would grow memory without bound.
const MAX_SSE_LINE: usize = 4 * 1024 * 1024;

/// Read `data:` lines from `reader` into `tx` until the body ends, a read
/// fails, or nobody is listening.
///
/// Every I/O error is terminal — the stream's idle timeout included. The old
/// loop resumed after a timeout to re-test cancellation, which ureq 2's
/// chunked decoder did not survive: a timeout in the middle of a chunk-size
/// line lost the bytes already read, and the rest of the body parsed as
/// garbage. Cancellation no longer needs the socket to wake up (the consumer
/// polls the channel instead), so the timeout is left to mean what it says.
///
/// Returning drops `reader`, which closes the connection. An abandon does not
/// wait for that: it shuts the socket down under the blocked read
/// ([`crate::net::Line`]), which fails the read at once, and this loop returns
/// on the error.
fn pump_sse(reader: Box<dyn Read + Send>, abandoned: &AtomicBool, tx: &SyncSender<SsePiece>) {
    let mut reader = std::io::BufReader::new(reader);
    let mut buf: Vec<u8> = Vec::new();
    loop {
        if abandoned.load(Ordering::Relaxed) {
            return;
        }
        buf.clear();
        let piece = match (&mut reader)
            .take(MAX_SSE_LINE as u64 + 1)
            .read_until(b'\n', &mut buf)
        {
            Ok(0) => SsePiece::End,
            Ok(_) if buf.len() > MAX_SSE_LINE => SsePiece::Failed(LlmError::Protocol(format!(
                "the stream sent a line over {} MB",
                MAX_SSE_LINE >> 20
            ))),
            Ok(_) => match sse_data(&buf) {
                Some(data) => SsePiece::Data(data),
                // Comments (`: ping`), `event:` names and blank separators.
                None => continue,
            },
            Err(e) => SsePiece::Failed(LlmError::Transport(format!("stream read: {e}"))),
        };
        let last = !matches!(piece, SsePiece::Data(_));
        if tx.send(piece).is_err() || last {
            return;
        }
    }
}

/// The payload of a `data:` line, or `None` for any other line.
fn sse_data(line: &[u8]) -> Option<String> {
    let line = String::from_utf8_lossy(line);
    let data = line.trim_end_matches(['\r', '\n']).strip_prefix("data:")?;
    Some(data.trim().to_string())
}

/// Drain an SSE stream, handing every parsed JSON event to `on_event`. Returns
/// whether the stream announced its end (`[DONE]` / `message_stop`). An error
/// from `on_event` ends it with that error (and the stream is dropped, which
/// closes its connection).
///
/// `cancelled` is polled before every event and at least every
/// [`CANCEL_POLL`] while the stream is silent; a stop returns [`CANCELLED`]
/// immediately, and dropping the stream shuts its connection down
/// ([`crate::net::Line`]).
///
/// Providers report post-200 failures as **in-stream events** (Anthropic sends
/// `{"type":"error"}` on overload, OpenAI-compatible servers an `error`
/// object). Those surface as [`LlmError::Stream`] — swallowing them would
/// return a silently truncated result as if the stream had completed.
fn read_sse_events(
    stream: SseStream,
    mut on_event: impl FnMut(&Value) -> Result<(), LlmError>,
    cancelled: &dyn Fn() -> bool,
) -> Result<bool, LlmError> {
    loop {
        if cancelled() {
            return Err(LlmError::Cancelled);
        }
        let data = match stream.rx.recv_timeout(CANCEL_POLL) {
            Ok(SsePiece::Data(data)) => data,
            Ok(SsePiece::End) => return Ok(false),
            Ok(SsePiece::Failed(e)) => return Err(e),
            Err(RecvTimeoutError::Timeout) => continue,
            // The reader died without saying why (it panicked).
            Err(RecvTimeoutError::Disconnected) => {
                return Err(LlmError::Transport(
                    "the stream reader stopped unexpectedly".into(),
                ));
            }
        };
        if data == "[DONE]" {
            return Ok(true);
        }
        let Ok(json) = serde_json::from_str::<Value>(&data) else {
            continue;
        };
        // `error` must be present AND non-null: some OpenAI-compatible proxies
        // attach `"error": null` to every healthy chunk.
        if json.get("type").and_then(Value::as_str) == Some("error")
            || json.get("error").is_some_and(|e| !e.is_null())
        {
            let message = json
                .pointer("/error/message")
                .and_then(Value::as_str)
                .or_else(|| json.get("error").and_then(Value::as_str))
                .unwrap_or("the provider reported a stream error");
            return Err(LlmError::Stream(
                match json.pointer("/error/type").and_then(Value::as_str) {
                    Some(kind) => format!("{} ({kind})", first_line(message)),
                    None => first_line(message),
                },
            ));
        }
        if json.get("type").and_then(Value::as_str) == Some("message_stop") {
            return Ok(true);
        }
        on_event(&json)?;
    }
}

// -- workers -----------------------------------------------------------------

/// How often a waiting caller re-tests its `cancelled` predicate.
const CANCEL_POLL: Duration = Duration::from_millis(100);

/// Tells a worker thread that nobody is waiting for it any more: set when the
/// waiting side drops it — which is also how cancellation reaches the thread,
/// and, through its [`crate::net::Line`], its connection.
pub(crate) struct Abandon {
    flag: Arc<AtomicBool>,
    line: Arc<crate::net::Line>,
}

impl Drop for Abandon {
    fn drop(&mut self) {
        self.flag.store(true, Ordering::Relaxed);
        self.line.close();
    }
}

/// Run `work` on its own thread, which reports through a bounded channel.
/// The thread's requests go on the returned [`Abandon`]'s line
/// ([`crate::net::Line::tether`]), so dropping it shuts down whatever
/// connection they are on, wherever the request is: its connect, a proxy's
/// or a TLS handshake, the wait for the response, a body gone quiet.
///
/// Without that an abandoned request held its connection until the provider
/// spoke again: the worker sat in a socket read nothing could interrupt. A
/// streamed answer kept generating (and billing) until its next event came
/// in to fail the send — a provider gone quiet let go only at the 180 s idle
/// timeout — and a blocking call, whose socket is silent for the whole
/// generation, ran to its end or to the 600 s deadline. The closed
/// connection is what tells the provider to stop. It reaches a plain-http
/// endpoint (a model server on this machine or the LAN) as it does an https
/// one: clew-core makes every connection itself (`crate::net::agent`).
/// Embeddings batches run the same way ([`crate::embed`]).
pub(crate) fn spawn_worker<T: Send + 'static>(
    capacity: usize,
    work: impl FnOnce(&AtomicBool, &SyncSender<T>) + Send + 'static,
) -> Result<(Receiver<T>, Abandon), LlmError> {
    let (tx, rx) = std::sync::mpsc::sync_channel(capacity);
    let flag = Arc::new(AtomicBool::new(false));
    let line = Arc::new(crate::net::Line::default());
    let (seen, worker_line) = (flag.clone(), line.clone());
    std::thread::Builder::new()
        .name("clew-llm".into())
        .spawn(move || {
            let _tether = crate::net::Line::tether(worker_line);
            work(&seen, &tx)
        })
        // Nothing was sent: this is as safe to report as a failed connect.
        .map_err(|e| LlmError::Connect(format!("could not start a request thread: {e}")))?;
    Ok((rx, Abandon { flag, line }))
}

/// The most one response to a model call may deliver on its connection,
/// all it sends counted (`crate::net::Profile::response_cap`): four times
/// the body cap. Embeddings share it (`crate::embed` checks its own body cap
/// fits).
///
/// A blocking call's JSON body is about its own size on the wire, so there
/// [`MAX_BODY_BYTES`] is what binds. A stream is the other way round: its
/// server-sent-event framing wraps every token in a whole JSON event, and an
/// OpenAI-style chunk is some fifty times the text it carries (Anthropic's,
/// about thirty). So for a stream this cap binds first — at roughly 2.5 MiB of
/// OpenAI-style text, long before the 32 MiB of [`MAX_STREAM_BYTES`], which
/// binds only a stream of few, large events. Neither can cut a real answer
/// short: the largest budget any call asks for is an agent step's retry
/// ceiling of 16,000 tokens, some 64 KB of text and 4 MB of framing.
pub(crate) const RAW_RESPONSE_CAP: u64 = 4 * MAX_BODY_BYTES;

/// Send `prepared` as one blocking POST on a worker thread and return its
/// body, polling `cancelled` while it is out. On a stop the worker is
/// abandoned: it makes no further attempt, and its connection is shut down
/// ([`crate::net::Line`]), which ends it at once.
fn run_blocking(prepared: Prepared, cancelled: &dyn Fn() -> bool) -> Result<String, LlmError> {
    let (rx, _abandon) = spawn_worker(1, move |abandoned, tx| {
        let result = send_with_retry(&prepared, abandoned).and_then(read_body);
        let _ = tx.send(result);
    })?;
    loop {
        if cancelled() {
            return Err(LlmError::Cancelled);
        }
        match rx.recv_timeout(CANCEL_POLL) {
            Ok(result) => return result,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => {
                return Err(LlmError::Transport(
                    "the request thread stopped unexpectedly".into(),
                ));
            }
        }
    }
}

/// Largest non-streamed response body read. A completion is kilobytes; this
/// only stops a broken endpoint from filling memory.
const MAX_BODY_BYTES: u64 = 32 * 1024 * 1024;

fn read_body(resp: ureq::http::Response<ureq::Body>) -> Result<String, LlmError> {
    let mut bytes = Vec::new();
    resp.into_body()
        .into_reader()
        .take(MAX_BODY_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| {
            LlmError::Transport(format!("read response: {}", crate::net::describe_io(&e)))
        })?;
    if bytes.len() as u64 > MAX_BODY_BYTES {
        return Err(LlmError::Protocol(format!(
            "the response exceeded {} MB",
            MAX_BODY_BYTES >> 20
        )));
    }
    String::from_utf8(bytes).map_err(|_| LlmError::Protocol("the response is not UTF-8".into()))
}

// -- transport ---------------------------------------------------------------

/// The transport profile one kind of request is built with: its timeouts,
/// and whether its connections are kept for the next request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct Limits {
    /// Establishing the connection: the name lookup, and then TCP, a proxy's
    /// handshake and TLS, each within this.
    connect: Duration,
    /// The longest one write of the request may wait for the peer.
    write: Duration,
    /// The longest silence tolerated on a read — for a stream, before the
    /// response headers as well as between events, and terminal there (the
    /// stream ends, see [`pump_sse`]).
    idle: Option<Duration>,
    /// Blocking calls: the whole request, body included.
    total: Option<Duration>,
    /// Embeddings batches: the pace the answer must keep, once a grace is
    /// spent (`crate::net::Pace`).
    pace: Option<crate::net::Pace>,
    /// Whether a connection is kept for the next request to its endpoint.
    pooled: bool,
}

impl Limits {
    /// A request whose connection is kept for the next one — an embeddings
    /// batch: `timeout` bounds every read and write, and `pace` the whole
    /// request, so that a peer trickling its answer a byte at a time — which
    /// never trips the idle limit — cannot hold it for long, while a large
    /// answer on a slow link still has the time it needs.
    ///
    /// The meter bounds each response a kept connection carries, not the
    /// connection's whole life (`crate::net::Profile`), and the connection
    /// is on the line of each request that takes it up, for that request's
    /// time on it alone (`crate::net::Lease`), so the abandon of one batch
    /// cannot reach the next. Model calls keep none.
    pub(crate) const fn direct(timeout: Duration, pace: crate::net::Pace) -> Limits {
        Limits {
            connect: CONNECT_TIMEOUT,
            write: timeout,
            idle: Some(timeout),
            total: None,
            pace: Some(pace),
            pooled: true,
        }
    }

    /// The transport of these limits, each response held to `response_cap`.
    fn profile(self, response_cap: u64) -> crate::net::Profile {
        crate::net::Profile {
            connect: self.connect,
            read_idle: self.idle,
            write_idle: Some(self.write),
            total: self.total,
            pace: self.pace,
            response_cap,
            pooled: self.pooled,
            https_only: false,
        }
    }
}

/// A stalled connect is a dead route; there is nothing to wait for.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const WRITE_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a stream may stay silent before it is declared dead.
///
/// It no longer doubles as a cancellation wakeup (the consumer polls a channel
/// instead), so it can be what it claims to be: a failure. Generous on
/// purpose, because it also covers the wait before the first token — a queued
/// request, a local model evaluating a long prompt. Providers that keep a
/// stream alive (Anthropic's `ping`) reset it long before it fires.
///
/// It is a SOCKET-level read timeout, so it bounds the wait for the response
/// headers too — before which the request is already on the wire. That is why
/// [`retryable_transport`] refuses to resend on it.
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(180);

/// Deadline for a whole non-streamed call. A blocking completion holds a
/// silent socket for the entire generation, so no read timeout can tell a slow
/// answer from a dead one; an overall deadline at least guarantees the worker
/// ends. Long enough for a slow local model writing a few thousand tokens.
const BLOCKING_TIMEOUT: Duration = Duration::from_secs(600);

const STREAM_LIMITS: Limits = Limits {
    connect: CONNECT_TIMEOUT,
    write: WRITE_TIMEOUT,
    idle: Some(STREAM_IDLE_TIMEOUT),
    total: None,
    pace: None,
    pooled: false,
};

const BLOCKING_LIMITS: Limits = Limits {
    connect: CONNECT_TIMEOUT,
    write: WRITE_TIMEOUT,
    idle: None,
    total: Some(BLOCKING_TIMEOUT),
    pace: None,
    pooled: false,
};

/// Build an agent with `limits`, its connections taking `route`.
fn build_agent(limits: Limits, route: crate::net::Route) -> ureq::Agent {
    build_agent_trusting(limits, route, crate::net::tls_config())
}

/// [`build_agent`] with the TLS trust given — a test's own root.
fn build_agent_trusting(
    limits: Limits,
    route: crate::net::Route,
    tls: Arc<rustls::ClientConfig>,
) -> ureq::Agent {
    build_agent_metered(limits, route, tls, RAW_RESPONSE_CAP)
}

/// [`build_agent_trusting`] with the cap on what one response may deliver
/// given — a test's small one. The agent is clew-core's
/// (`crate::net::agent`): it follows no redirect, every connection under it
/// is metered, and a model call's connections are on the call's line.
fn build_agent_metered(
    limits: Limits,
    route: crate::net::Route,
    tls: Arc<rustls::ClientConfig>,
    raw_cap: u64,
) -> ureq::Agent {
    crate::net::agent(route, tls, limits.profile(raw_cap))
}

/// The agent [`agent_for`] builds for `limits`, with a test's own trust and
/// meter cap, and no proxy — for another module's tests (`crate::embed`).
#[cfg(test)]
pub(crate) fn test_agent(
    limits: Limits,
    tls: Arc<rustls::ClientConfig>,
    raw_cap: u64,
) -> ureq::Agent {
    build_agent_metered(limits, crate::net::Route::Direct, tls, raw_cap)
}

/// [`test_agent`] through `route` — a test's own proxy — for another
/// module's tests of what a proxy's answer makes of a request
/// (`crate::embed`).
#[cfg(test)]
pub(crate) fn test_agent_routed(
    limits: Limits,
    route: crate::net::Route,
    tls: Arc<rustls::ClientConfig>,
    raw_cap: u64,
) -> ureq::Agent {
    build_agent_metered(limits, route, tls, raw_cap)
}

/// The agent for a request to `url` with `limits` — the policy every call to
/// a model provider goes through, chat and embeddings alike: no redirects,
/// the trust and the proxy every request of clew's shares
/// ([`crate::net::tls_config`], [`crate::net::via`]), and `limits` —
/// and the proxy it goes through, for what a failure there says. Agents are
/// cached per (limits, proxy); a pooled profile's keep their connections
/// for the next request.
///
/// A proxy the user configured but that cannot be used is an error, not a
/// reason to connect directly ([`crate::net::route`]).
pub(crate) fn agent_for(
    url: &str,
    limits: Limits,
) -> Result<(ureq::Agent, Option<crate::net::Via>), LlmError> {
    agent_with_proxies(url, limits, &crate::net::ProxySources::LIVE)
}

/// [`agent_for`] with what it reads proxies from injected, so the routing
/// can be tested without touching the process environment or the machine's
/// settings.
fn agent_with_proxies(
    url: &str,
    limits: Limits,
    proxies: &crate::net::ProxySources<'_>,
) -> Result<(ureq::Agent, Option<crate::net::Via>), LlmError> {
    type Key = (Limits, Option<String>);
    static AGENTS: OnceLock<Mutex<HashMap<Key, ureq::Agent>>> = OnceLock::new();
    let via = crate::net::via(url, proxies);
    let spec = via.as_ref().map(|via| via.spec.clone());
    let route = crate::net::route(spec.as_deref()).map_err(LlmError::Config)?;
    let mut agents = AGENTS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let key = (limits, spec);
    if let Some(agent) = agents.get(&key) {
        return Ok((agent.clone(), via));
    }
    let agent = build_agent(limits, route);
    agents.insert(key, agent.clone());
    Ok((agent, via))
}

// -- retries -----------------------------------------------------------------

/// How many times a transient failure is retried before giving up.
const SEND_RETRIES: u32 = 3;

/// Longest single wait between attempts, whatever `retry-after` asks for.
const MAX_RETRY_WAIT: Duration = Duration::from_secs(30);

/// Statuses worth retrying: the request timing out before the provider had it
/// (408), rate limits (429), the provider's own internal error (500),
/// unavailable (503) and overload (529 on Anthropic). 4xx besides these are
/// caller bugs or auth problems and fail immediately.
///
/// Each of these is the PROVIDER answering that it did not take the request,
/// which is what makes a resend safe. A gateway's 502/504 is not in that
/// class: it is a proxy in front of the provider reporting that its upstream
/// failed or went quiet — the gateway's own read timeout — which says nothing
/// about whether the model is already generating behind it. That is exactly
/// the case [`retryable_transport`] refuses to resend on, so these are not
/// retried either: the user may retry, knowingly, rather than be billed twice.
fn transient_status(code: u16) -> bool {
    matches!(code, 408 | 429 | 500 | 503 | 529)
}

/// Whether a transport error happened before the provider could have accepted
/// the request, making a resend safe.
///
/// This is not a detail. These requests are billed per token and carry no
/// idempotency key, so a resend of one the provider already started on is
/// charged twice and generates twice. The read timeout is the trap: streamed
/// requests carry [`STREAM_IDLE_TIMEOUT`] as a socket-level read timeout,
/// which also covers the wait for the response HEADERS — so a provider that
/// simply took longer than that to produce its first byte surfaced here as an
/// ordinary transport failure, and the same POST went out up to four times.
///
/// Only failures to establish the connection qualify: its name lookup, TCP,
/// a proxy's handshake — an http proxy's `CONNECT` or a SOCKS one's — and TLS
/// ([`crate::net::is_unreached`]). Anything that fails once bytes are on the
/// wire is reported to the caller instead.
///
/// And of those, only the ones another attempt may get past. A proxy that
/// REFUSED the request — the tunnel, a user name and password clew does not
/// have, the ones its setting names (a hang-up on them included) — answers
/// the same again ([`crate::net::proxy_refused`]): resending only makes the
/// user wait for it, and a proxy that checks logins against a directory
/// counts every refused one against the account, which a few attempts per
/// call soon lock. One that answered that it could not reach the target, or
/// not now — or that held its answer past the connect timeout, as a slow
/// target makes it — is resent, as a direct connect that could not reach it
/// is.
fn retryable_transport(e: &ureq::Error) -> bool {
    crate::net::is_unreached(e) && !crate::net::proxy_refused(e)
}

/// Send with exponential backoff on transient failures (a transient status, or
/// a failure to connect at all). Honors `retry-after` / `retry-after-ms` when
/// the provider sends one. The ONE retry policy every request goes through.
///
/// Runs on a worker thread, so its sleeps block nobody; `abandoned` ends it
/// between attempts once the caller has stopped waiting.
fn send_with_retry(
    prepared: &Prepared,
    abandoned: &AtomicBool,
) -> Result<ureq::http::Response<ureq::Body>, LlmError> {
    let who = prepared.who.as_str();
    let mut delay = Duration::from_secs(1);
    let mut attempt = 0;
    loop {
        if abandoned.load(Ordering::Relaxed) {
            return Err(LlmError::Cancelled);
        }
        let last = attempt >= SEND_RETRIES;
        // A panic inside ureq is an error of this request (`net::guarded`),
        // on the wire as far as a retry is concerned: never resent below.
        let wait = match prepared.send() {
            // No redirect is followed — the agent follows none (the default
            // copies every header but `authorization` and `cookie` to the new
            // host, Anthropic's `x-api-key` included, and turns the POST into
            // a GET) — so a 3xx comes back as a response whose body is the
            // redirect page, not the provider's JSON. Report it as what it
            // is: parsing it would surface as "bad JSON response" and send the
            // user hunting for a provider outage, when the real answer is
            // that their `base_url` points at something that wants to move
            // them somewhere else — the exact case in which following along
            // would have handed that somewhere else their API key.
            Ok(r) if r.status().is_redirection() => {
                let location = r
                    .headers()
                    .get("location")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("elsewhere");
                return Err(LlmError::Redirected {
                    who: who.to_string(),
                    location: first_line(location),
                });
            }
            // Only a proxy asks for proxy credentials: nothing reached the
            // provider.
            Ok(r) if r.status().as_u16() == 407 && prepared.via.is_some() => {
                let via = prepared
                    .via
                    .as_ref()
                    .map(|via| via.answered_407(r.headers()));
                return Err(LlmError::Connect(via.unwrap_or_default()));
            }
            Ok(r) if r.status().is_client_error() || r.status().is_server_error() => {
                let code = r.status().as_u16();
                if last || !transient_status(code) {
                    return Err(status_error(who, code, r));
                }
                retry_after(&r).unwrap_or(delay).min(MAX_RETRY_WAIT)
            }
            Ok(r) => return Ok(r),
            // Nothing reached the provider: a failure to connect, resent
            // only while another attempt may get further.
            Err(e) if crate::net::is_unreached(&e) => {
                if last || !retryable_transport(&e) {
                    return Err(LlmError::Connect(crate::net::failure(
                        &e,
                        prepared.via.as_ref(),
                    )));
                }
                delay
            }
            Err(e) => {
                return Err(LlmError::Transport(crate::net::failure(
                    &e,
                    prepared.via.as_ref(),
                )));
            }
        };
        sleep_unless_abandoned(wait, abandoned);
        delay = (delay * 2).min(Duration::from_secs(8));
        attempt += 1;
    }
}

/// The wait a `429`/`503` asked for: `retry-after` (seconds) or OpenAI's
/// `retry-after-ms`. HTTP-date forms are ignored (the backoff applies).
fn retry_after(r: &ureq::http::Response<ureq::Body>) -> Option<Duration> {
    let header = |name: &str| {
        r.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
    };
    if let Some(ms) = header("retry-after-ms") {
        return Some(Duration::from_millis(ms));
    }
    header("retry-after").map(Duration::from_secs)
}

fn sleep_unless_abandoned(total: Duration, abandoned: &AtomicBool) {
    let deadline = std::time::Instant::now() + total;
    while !abandoned.load(Ordering::Relaxed) {
        let now = std::time::Instant::now();
        if now >= deadline {
            return;
        }
        std::thread::sleep((deadline - now).min(CANCEL_POLL));
    }
}

/// Largest error body read when extracting the provider's message.
const MAX_ERROR_BODY_BYTES: u64 = 64 * 1024;

/// An HTTP error status as a typed error carrying the provider's own words.
fn status_error(who: &str, code: u16, resp: ureq::http::Response<ureq::Body>) -> LlmError {
    let mut body = Vec::new();
    let _ = resp
        .into_body()
        .into_reader()
        .take(MAX_ERROR_BODY_BYTES)
        .read_to_end(&mut body);
    let (kind, message) = provider_error(&String::from_utf8_lossy(&body));
    LlmError::Status {
        who: who.to_string(),
        code,
        kind,
        message,
    }
}

/// `(kind, message)` from a provider's error body.
///
/// The raw JSON used to go straight into the user-facing message
/// (`{"type":"error","error":{"type":"authentication_error",…`). The message is
/// `/error/message` in both dialects; the kind is OpenAI's specific `code`
/// (`invalid_api_key`) or else the `type` (Anthropic's `authentication_error`),
/// kept because callers holding only the string match on it. Bodies that are
/// not JSON, or not shaped like this, fall back to their first line.
fn provider_error(body: &str) -> (Option<String>, String) {
    let fallback = || {
        let line = first_line(body.trim());
        if line.is_empty() {
            "(no details)".to_string()
        } else {
            line
        }
    };
    let Ok(json) = serde_json::from_str::<Value>(body) else {
        return (None, fallback());
    };
    let str_at = |ptr: &str| {
        json.pointer(ptr)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let message = str_at("/error/message")
        .or_else(|| str_at("/error"))
        .or_else(|| str_at("/message"))
        .or_else(|| str_at("/detail"))
        .map(|m| first_line(&m));
    let kind = str_at("/error/code").or_else(|| str_at("/error/type"));
    (kind, message.unwrap_or_else(fallback))
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").chars().take(300).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    // Read the WHOLE request before answering (see there for why).
    use crate::testutil::read_http_request as read_request;
    use std::io::Write;
    use std::time::Instant;

    // -- helpers -------------------------------------------------------------

    /// One scripted connection of the mock provider.
    enum Script {
        /// Answer with these bytes, then close.
        Respond(String),
        /// Send `head`, keep the connection open and silent for `stall`, then
        /// send `tail` and report whether the client has closed its end.
        Stall {
            head: String,
            stall: Duration,
            tail: String,
        },
    }

    /// What the mock provider saw.
    #[derive(Default, Debug)]
    struct MockLog {
        /// Requests read (one per connection).
        served: usize,
        /// Their bodies, in order.
        bodies: Vec<String>,
        /// For `Stall` scripts: the client had closed the connection by the
        /// time the tail was sent.
        closed_after_stall: Vec<bool>,
    }

    /// How long the mock keeps listening after its last script, so that a
    /// request that should never be made — a resend, a fallback — is caught
    /// and counted instead of hitting a closed port unseen.
    const LINGER: Duration = Duration::from_millis(500);

    /// Serve scripted HTTP responses on a local socket, one per connection.
    /// Connections beyond the scripts are counted in `served` and closed.
    fn mock(scripts: Vec<Script>) -> (String, std::thread::JoinHandle<MockLog>) {
        mock_hooked(scripts, || {})
    }

    /// [`mock`], calling `on_request` once each request has been read — the
    /// moment it is provably on the wire. Tests that stop a request in flight
    /// flip their flag from here: a timer instead would race the worker thread
    /// on a loaded machine, and could stop the request before it was sent.
    fn mock_hooked(
        scripts: Vec<Script>,
        on_request: impl Fn() + Send + 'static,
    ) -> (String, std::thread::JoinHandle<MockLog>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let mut log = MockLog::default();
            let mut scripts = scripts.into_iter();
            // Generous while scripts are pending (a slow CI host), short once
            // they are all served.
            let mut deadline = Instant::now() + Duration::from_secs(20);
            loop {
                let mut conn = match listener.accept() {
                    Ok((conn, _)) => conn,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        if Instant::now() >= deadline {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(_) => break,
                };
                conn.set_nonblocking(false).unwrap();
                log.bodies.push(read_request(&mut conn));
                log.served += 1;
                on_request();
                match scripts.next() {
                    Some(Script::Respond(resp)) => {
                        let _ = conn.write_all(resp.as_bytes());
                        let _ = conn.flush();
                    }
                    Some(Script::Stall { head, stall, tail }) => {
                        let _ = conn.write_all(head.as_bytes());
                        let _ = conn.flush();
                        std::thread::sleep(stall);
                        let _ = conn.write_all(tail.as_bytes());
                        let _ = conn.flush();
                        log.closed_after_stall.push(peer_closed(&mut conn));
                    }
                    // Unscripted: counted above, answered with nothing.
                    None => {}
                }
                if scripts.as_slice().is_empty() {
                    deadline = Instant::now() + LINGER;
                }
            }
            log
        });
        (addr, handle)
    }

    /// A stop flag the mock flips when the request arrives, and when it did.
    struct StopOnRequest {
        flag: Arc<AtomicBool>,
        at: Arc<Mutex<Option<Instant>>>,
    }

    impl StopOnRequest {
        fn new() -> StopOnRequest {
            StopOnRequest {
                flag: Arc::new(AtomicBool::new(false)),
                at: Arc::new(Mutex::new(None)),
            }
        }

        /// The hook for [`mock_hooked`].
        fn hook(&self) -> impl Fn() + Send + 'static {
            let (flag, at) = (self.flag.clone(), self.at.clone());
            move || {
                *at.lock().unwrap() = Some(Instant::now());
                flag.store(true, Ordering::Relaxed);
            }
        }

        fn stopped(&self) -> bool {
            self.flag.load(Ordering::Relaxed)
        }

        /// How long ago the stop fired.
        fn since(&self) -> Duration {
            self.at.lock().unwrap().expect("the stop fired").elapsed()
        }
    }

    /// Whether the client has closed its end: EOF or a reset on the next read.
    /// A client still listening leaves the read blocked until the timeout.
    fn peer_closed(conn: &mut std::net::TcpStream) -> bool {
        // macOS refuses a socket option on a connection its peer has reset —
        // as a client that closed does, when bytes arrive for it after.
        if conn.set_read_timeout(Some(Duration::from_secs(5))).is_err() {
            return true;
        }
        let mut probe = [0u8; 1];
        match std::io::Read::read(conn, &mut probe) {
            Ok(0) => true,
            Ok(_) => false,
            Err(e) => !matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ),
        }
    }

    /// A complete HTTP response with `body`.
    fn http(status: &str, content_type: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\nconnection: close\r\n\
             content-length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    /// Response headers for a stream whose body is sent separately (no
    /// length: the body ends when the connection closes).
    fn sse_head() -> String {
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n".into()
    }

    fn custom(addr: String) -> Config {
        Config::from_parts(Provider::Custom, "k".into(), "m".into(), addr)
    }

    fn anthropic_at(addr: String) -> Config {
        Config::from_parts(Provider::Anthropic, "k".into(), "m".into(), addr)
    }

    /// An SSE stream over in-memory bytes, read by the same reader thread a
    /// network stream gets.
    fn sse_bytes(body: &str) -> SseStream {
        let bytes = body.as_bytes().to_vec();
        spawn_sse(move |_| Ok(Box::new(std::io::Cursor::new(bytes)) as Box<dyn Read + Send>))
            .unwrap()
    }

    fn collect_text(body: &str, dialect: Dialect) -> Result<(String, StepOutput), LlmError> {
        let mut seen = String::new();
        let out = collect_stream(sse_bytes(body), dialect, &mut |d| seen.push_str(d), &|| {
            false
        })?;
        Ok((seen, out))
    }

    fn never() -> impl Fn() -> bool {
        || false
    }

    /// A deadline no test here comes near.
    const MINUTE: Duration = Duration::from_secs(60);

    /// An embeddings batch's pace that no test here comes near.
    const PACED: crate::net::Pace = crate::net::Pace {
        grace: MINUTE,
        window: MINUTE,
        floor: 1024,
    };

    /// A POST of `{}` to `url` through `agent`, for `send_with_retry`.
    fn raw(agent: &ureq::Agent, url: &str) -> Prepared {
        Prepared {
            agent: agent.clone(),
            uri: crate::net::request_uri(url).unwrap(),
            headers: Vec::new(),
            body: "{}".into(),
            who: "test".into(),
            via: None,
        }
    }

    /// [`raw`], through a blocking call's agent with no proxy.
    fn plain(url: &str) -> Prepared {
        raw(
            &build_agent(BLOCKING_LIMITS, crate::net::Route::Direct),
            url,
        )
    }

    /// A stream is held to the size a blocking body is: only single lines
    /// were capped, so a stream of small events grew the answer — or one
    /// tool call's arguments — without limit. Past the cap the stream ends
    /// with an error and nothing past it is forwarded; a run of ever-new
    /// tool-call indexes is refused the same way, content or not.
    #[test]
    fn a_stream_is_capped_in_total_not_just_per_line() {
        let chunk = |t: &str| {
            format!(
                "data: {}\n\n",
                json!({"choices": [{"delta": {"content": t}}]})
            )
        };
        let body = (0..10).map(|_| chunk("0123456789")).collect::<String>() + "data: [DONE]\n\n";
        let collect = |state: StreamState, body: &str, dialect: Dialect| {
            let mut seen = String::new();
            let out = collect_into(
                state,
                sse_bytes(body),
                dialect,
                &mut |d| seen.push_str(d),
                &never(),
            );
            (out, seen)
        };
        // Room for all of it.
        let (out, _) = collect(StreamState::within(100, 4), &body, Dialect::OpenAi);
        assert_eq!(out.unwrap().text.len(), 100);
        // One byte less: refused, and what was forwarded stayed under it.
        let (out, seen) = collect(StreamState::within(99, 4), &body, Dialect::OpenAi);
        let err = out.unwrap_err();
        assert!(
            matches!(err, LlmError::Protocol(ref m) if m.contains("exceeded")),
            "{err}"
        );
        assert_eq!(seen.len(), 90);

        // Tool-call arguments count too (Anthropic's `input_json_delta`).
        let start = format!(
            "data: {}\n\n",
            json!({"type": "content_block_start", "index": 1,
                   "content_block": {"type": "tool_use", "id": "t", "name": "read"}})
        );
        let part = format!(
            "data: {}\n\n",
            json!({"type": "content_block_delta", "index": 1,
                   "delta": {"type": "input_json_delta", "partial_json": "{\"path\": \"aaaaaaaaaa\"}"}})
        );
        let (out, _) = collect(
            StreamState::within(200, 4),
            &(start + &part.repeat(20)),
            Dialect::Anthropic,
        );
        assert!(matches!(out, Err(LlmError::Protocol(_))), "{out:?}");

        // So does the number of calls opened (OpenAI's `index`).
        let open = |i: u32| {
            format!(
                "data: {}\n\n",
                json!({"choices": [{"delta": {"tool_calls": [{"index": i}]}}]})
            )
        };
        let (out, _) = collect(
            StreamState::within(1 << 20, 4),
            &(0..5).map(open).collect::<String>(),
            Dialect::OpenAi,
        );
        assert!(
            matches!(out, Err(LlmError::Protocol(ref m)) if m.contains("tool calls")),
            "{out:?}"
        );
        // The real bound is the blocking body's.
        assert_eq!(MAX_STREAM_BYTES as u64, MAX_BODY_BYTES);
    }

    // -- retry policy (C1) ---------------------------------------------------

    /// These requests are billed per token and carry no idempotency key, so a
    /// resend is only safe while the provider cannot have seen the request.
    /// Everything failing after the bytes are on the wire — including the read
    /// timeout that also covers the wait for response headers — has to be
    /// reported, not retried. Treating them alike sent one POST four times.
    #[test]
    fn only_pre_connection_failures_are_resent() {
        let brief = Limits {
            idle: Some(Duration::from_millis(200)),
            ..STREAM_LIMITS
        };
        let send = |agent: &ureq::Agent, url: &str| raw(agent, url).send().unwrap_err();
        // Nothing listens on port 1: the connection never opens.
        let direct = build_agent(brief, crate::net::Route::Direct);
        let e = send(&direct, "http://127.0.0.1:1/");
        assert!(
            retryable_transport(&e),
            "a connect failure is safe to retry: {e}"
        );

        // So is one to a proxy that cannot be reached: as passing as a
        // connect that fails without one.
        let route = crate::net::route(Some("http://127.0.0.1:1")).unwrap();
        let e = send(&build_agent(brief, route), "https://api.example.invalid/v1");
        assert!(
            retryable_transport(&e),
            "a proxy that cannot be reached is safe to retry: {e}"
        );

        // fixR5 #10: nor does a proxy that refuses the tunnel let anything
        // of the request through — a SOCKS proxy's refusal as much as an
        // http proxy's (whose `CONNECT` refusal ureq 2 retried while it
        // took a SOCKS one for a failure on the wire). fixR7 #1: but such a
        // refusal is the proxy's answer, which a resend only gets again.
        // fixR9 #1: not so an answer that it could not reach the target, or
        // not now — as passing as a direct connect's failure to reach it,
        // which is resent.
        let proxied = |spec: &str| {
            let route = crate::net::route(Some(spec)).unwrap();
            send(&build_agent(brief, route), "https://api.example.invalid/v1")
        };
        for (code, retried) in [(2, false), (5, true), (4, true), (1, true)] {
            let (socks, _) =
                crate::testutil::socks5_proxy(None, crate::testutil::SocksAnswer::Refuse(code));
            let e = proxied(&format!("socks5h://{socks}"));
            assert!(
                crate::net::is_unreached(&e),
                "a SOCKS refusal lets nothing through: {e}"
            );
            assert_eq!(retryable_transport(&e), retried, "SOCKS reply {code}: {e}");
        }
        for (status, retried) in [(403, false), (503, true), (502, true), (501, false)] {
            let (http, _) = crate::testutil::http_proxy(
                "127.0.0.1:0",
                false,
                1,
                crate::testutil::ProxyAnswer::Status(status),
            )
            .unwrap();
            let e = proxied(&http);
            assert!(
                crate::net::is_unreached(&e),
                "a CONNECT refusal lets nothing through: {e}"
            );
            assert_eq!(retryable_transport(&e), retried, "CONNECT {status}: {e}");
        }

        // fixR9 #5: a proxy that hangs up on a CONNECT is resent as one that
        // cannot be reached — unless the CONNECT carried a user name and
        // password, which the hang-up may have refused.
        for (login, retried) in [("", true), ("me:pw@", false)] {
            let (http, _) = crate::testutil::http_proxy(
                "127.0.0.1:0",
                false,
                1,
                crate::testutil::ProxyAnswer::HangUp,
            )
            .unwrap();
            let e = proxied(&http.replacen("://", &format!("://{login}"), 1));
            assert!(crate::net::is_unreached(&e), "{login}: {e}");
            assert_eq!(retryable_transport(&e), retried, "{login}: {e}");
        }
        // fixR10 #1: but one that holds the CONNECT past the connect timeout
        // — its connect to a slow target has not ended — refused nothing, a
        // user name and password in it or not: resent.
        let hasty = Limits {
            connect: Duration::from_secs(1),
            ..brief
        };
        for login in ["", "me:pw@"] {
            let (http, _) = crate::testutil::http_proxy(
                "127.0.0.1:0",
                false,
                1,
                crate::testutil::ProxyAnswer::Late(504, Duration::from_secs(3)),
            )
            .unwrap();
            let spec = http.replacen("://", &format!("://{login}"), 1);
            let route = crate::net::route(Some(&spec)).unwrap();
            let e = send(&build_agent(hasty, route), "https://api.example.invalid/v1");
            assert!(crate::net::is_unreached(&e), "{login}: {e}");
            assert!(retryable_transport(&e), "{login}: {e}");
        }

        // Accepts the connection, then says nothing: the request IS on the
        // wire, and the provider may well be generating against it.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let held = listener.accept();
            std::thread::sleep(Duration::from_secs(3));
            drop(held);
        });
        let e = send(&direct, &format!("http://{addr}/"));
        assert!(
            !retryable_transport(&e),
            "a read timeout must not resend a billed POST: {e}"
        );
    }

    /// fixR7 #1: a proxy that asks for a user name and password, or refuses
    /// the ones its setting names, is asked once per call — not three times
    /// more, at the retries a failure to connect gets: asked again, it
    /// answers the same, and a proxy that checks logins against a directory
    /// counts every refused one against the account — four a call lock it
    /// soon. The call fails at once, as a failure to connect (nothing reached
    /// the provider), saying what to do.
    #[test]
    fn a_proxy_that_refuses_credentials_is_asked_once_per_call() {
        let url = "https://api.example.invalid/v1/chat/completions";
        let call_to = |url: &str, spec: &str| {
            let route = crate::net::route(Some(spec)).unwrap();
            let via = crate::net::Via {
                spec: spec.to_string(),
                system: None,
                for_https: url.starts_with("https:"),
            };
            // Trusting the test's root, which an `https://` proxy here has.
            let trust = crate::testutil::trusting_the_test_cert();
            let prepared = Prepared {
                via: Some(via),
                ..raw(&build_agent_trusting(BLOCKING_LIMITS, route, trust), url)
            };
            let began = Instant::now();
            let result = send_with_retry(&prepared, &AtomicBool::new(false));
            (result.map(|_| ()), began.elapsed())
        };
        let call = |spec: &str| call_to(url, spec);
        // Whatever the fake proxy reports, counted until it goes quiet.
        fn count<T>(reports: &std::sync::mpsc::Receiver<T>) -> usize {
            let mut n = 0;
            while reports.recv_timeout(Duration::from_millis(300)).is_ok() {
                n += 1;
            }
            n
        }

        // An http proxy answering every CONNECT with a 407.
        let (proxy_url, heads) = crate::testutil::http_proxy(
            "127.0.0.1:0",
            false,
            SEND_RETRIES as usize + 1,
            crate::testutil::ProxyAnswer::Status(407),
        )
        .unwrap();
        let spec = proxy_url.replacen("://", "://me:wrong@", 1);
        let (result, took) = call(&spec);
        let err = result.unwrap_err();
        assert!(matches!(err, LlmError::Connect(_)), "{err:?}");
        assert!(
            err.to_string()
                .contains("refused the user name and password"),
            "{err}"
        );
        assert_eq!(count(&heads), 1, "the proxy was asked again");
        // At once, not after the retries' waits (a second, two, four).
        assert!(took < Duration::from_secs(5), "waited {took:?} to fail");

        // fixR9 #5: one that hangs up on a CONNECT carrying the login, which
        // may be how it refuses it.
        let (proxy_url, heads) = crate::testutil::http_proxy(
            "127.0.0.1:0",
            false,
            SEND_RETRIES as usize + 1,
            crate::testutil::ProxyAnswer::HangUp,
        )
        .unwrap();
        let (result, took) = call(&proxy_url.replacen("://", "://me:pw@", 1));
        let err = result.unwrap_err();
        assert!(matches!(err, LlmError::Connect(_)), "{err:?}");
        // fixR10 #1: said to be a refusal only perhaps.
        let text = err.to_string();
        assert!(
            text.contains("it may have refused them")
                && text.contains("if they are right, try again later"),
            "{text}"
        );
        assert_eq!(count(&heads), 1, "the proxy was asked again");
        assert!(took < Duration::from_secs(5), "waited {took:?} to fail");

        // fixR11 #1: and one spoken to over TLS (`https://`), whose hang-up
        // is its TLS session just ending, with no close_notify. That end was
        // taken for the connection's failure, and the login went to the
        // proxy three times more.
        let (proxy_url, heads) = crate::testutil::http_proxy(
            "127.0.0.1:0",
            true,
            SEND_RETRIES as usize + 1,
            crate::testutil::ProxyAnswer::HangUp,
        )
        .unwrap();
        let (result, took) = call(&proxy_url.replacen("://", "://me:pw@", 1));
        let heads: Vec<String> =
            std::iter::from_fn(|| heads.recv_timeout(Duration::from_millis(300)).ok()).collect();
        assert_eq!(heads.len(), 1, "the proxy was asked again: {heads:?}");
        assert!(heads[0].starts_with("CONNECT "), "{heads:?}");
        assert!(took < Duration::from_secs(5), "waited {took:?} to fail");
        let err = result.unwrap_err();
        assert!(matches!(err, LlmError::Connect(_)), "{err:?}");
        let text = err.to_string();
        assert!(
            text.contains("gave no answer to the CONNECT")
                && text.contains("it may have refused them"),
            "{text}"
        );

        // fixR10 #4: a CONNECT answered with a 407 that offers only logins
        // clew cannot make: asked again, it answers the same.
        let (proxy_url, heads) = crate::testutil::http_proxy(
            "127.0.0.1:0",
            false,
            SEND_RETRIES as usize + 1,
            crate::testutil::ProxyAnswer::Login("NTLM"),
        )
        .unwrap();
        let (result, took) = call(&proxy_url.replacen("://", "://me:right@", 1));
        let err = result.unwrap_err();
        assert!(matches!(err, LlmError::Connect(_)), "{err:?}");
        let text = err.to_string();
        assert!(
            text.contains("CONNECT") && text.contains("it takes only NTLM logins"),
            "{text}"
        );
        assert_eq!(count(&heads), 1, "the proxy was asked again");
        assert!(took < Duration::from_secs(5), "waited {took:?} to fail");

        // A SOCKS5 proxy refusing every login; one asking for a login the
        // setting does not name; and (fixR9 #5) one that hangs up on the
        // login rather than answer it.
        for (setting, answer, what) in [
            (
                "me:wrong@",
                crate::testutil::SocksAnswer::Relay,
                "refused the user name and password",
            ),
            (
                "",
                crate::testutil::SocksAnswer::Relay,
                "asks for a user name and password",
            ),
            (
                "me:pw@",
                crate::testutil::SocksAnswer::HangUp,
                "gave no answer to the user name and password",
            ),
        ] {
            let (socks, seen) = crate::testutil::socks5_proxy_for(
                SEND_RETRIES as usize + 1,
                Some(("me", "pw")),
                answer,
            );
            let (result, took) = call(&format!("socks5h://{setting}{socks}"));
            let err = result.unwrap_err();
            assert!(matches!(err, LlmError::Connect(_)), "{err:?}");
            assert!(err.to_string().contains(what), "{err}");
            assert_eq!(count(&seen), 1, "{what}: the proxy was asked again");
            assert!(took < Duration::from_secs(5), "{what}: waited {took:?}");
        }

        // fixR9 #3: a plain-http call its proxy answers with a 407 offering
        // only logins clew cannot make says so — however right the password
        // the setting names — and what can make them.
        let (proxy_url, heads) = crate::testutil::http_proxy(
            "127.0.0.1:0",
            false,
            SEND_RETRIES as usize + 1,
            crate::testutil::ProxyAnswer::Login("NTLM"),
        )
        .unwrap();
        let (result, _) = call_to(
            "http://models.lan.invalid:11434/v1/chat/completions",
            &proxy_url.replacen("://", "://me:right@", 1),
        );
        let err = result.unwrap_err();
        assert!(matches!(err, LlmError::Connect(_)), "{err:?}");
        let text = err.to_string();
        assert!(
            text.contains("it takes only NTLM logins, which clew cannot make")
                && text.contains("cntlm or px"),
            "{text}"
        );
        assert_eq!(count(&heads), 1, "the proxy was asked again");
    }

    /// fixR7 #3: a model's answer over TLS whose body ends with its
    /// connection (no length, not chunked) is whole only when the provider
    /// ended TLS with its close_notify. One whose connection just ended was
    /// taken for all of the answer (ureq takes rustls's early end for the
    /// peer closing), however much of it had come.
    #[test]
    fn a_model_answer_cut_short_over_tls_is_an_error() {
        const ANSWER: &[u8] = b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
            connection: close\r\n\r\n{\"choices\":[{\"message\":{\"content\":\"hi\"}}]}";
        let cut: crate::testutil::TlsScript = Box::new(|tls: &mut crate::testutil::TlsConn| {
            let _ = tls.write_all(ANSWER);
            let _ = tls.flush();
            let _ = tls.sock.shutdown(std::net::Shutdown::Write);
            let _ = tls.sock.set_read_timeout(Some(Duration::from_secs(20)));
            let _ = std::io::Read::read(&mut tls.sock, &mut [0u8; 1]);
        });
        let base = crate::testutil::tls_serve(vec![cut, crate::testutil::tls_answer(ANSWER)]);
        let agent = build_agent_trusting(
            BLOCKING_LIMITS,
            crate::net::Route::Direct,
            trusting_the_mock(),
        );
        let url = format!("{base}/v1/chat/completions");
        let read = || {
            raw(&agent, &url)
                .send()
                .map_err(|e| e.to_string())
                .and_then(|r| read_body(r).map_err(|e| e.to_string()))
        };
        let err = read().expect_err("an answer cut short passed for a whole one");
        assert!(err.contains("close_notify"), "{err}");
        let whole = read().expect("an answer TLS ended properly");
        assert!(whole.ends_with(r#""hi"}}]}"#), "{whole}");
    }

    /// Only a status that REFUSED the stream may be answered by sending the
    /// same request again unstreamed. Every transport failure is excluded,
    /// because it can happen after the provider accepted the request.
    #[test]
    fn only_a_refusing_status_licenses_the_unstreamed_retry() {
        let status = |code| LlmError::Status {
            who: "x".into(),
            code,
            kind: None,
            message: String::new(),
        };
        assert!(status(400).stream_refused());
        assert!(status(422).stream_refused());
        for other in [
            status(401),
            status(429),
            status(500),
            LlmError::Transport("timed out reading response".into()),
            LlmError::Connect("refused".into()),
            LlmError::Stream("overloaded".into()),
            LlmError::Protocol("bad JSON".into()),
            LlmError::Cancelled,
        ] {
            assert!(!other.stream_refused(), "{other:?} must not be retried");
        }
    }

    /// A step whose stream dies after the provider started answering must be
    /// reported, never resent in the blocking form: the provider was already
    /// generating (and billing) against the first request.
    #[test]
    fn a_step_that_fails_mid_stream_is_not_sent_again() {
        let partial = format!(
            "{}data: {{\"choices\":[{{\"delta\":{{\"content\":\"half\"}}}}]}}\n\n",
            sse_head()
        );
        // The mock lingers after its script, so a resend would be counted.
        let (addr, handle) = mock(vec![Script::Respond(partial)]);
        let err = complete_tools_step(
            &custom(addr),
            "s",
            &[AgentMsg::User("q".into())],
            &[],
            64,
            &never(),
        )
        .unwrap_err();
        assert!(
            matches!(err, LlmError::Transport(ref m) if m.contains("ended before completion")),
            "{err:?}"
        );
        assert_eq!(handle.join().unwrap().served, 1, "exactly one request");
    }

    /// Same for a status that is not a refusal of streaming: an auth failure
    /// is final in either form, so it is reported after one request.
    #[test]
    fn a_non_refusing_status_is_not_retried_unstreamed() {
        let denied = http(
            "401 Unauthorized",
            "application/json",
            r#"{"error":{"message":"Incorrect API key provided","type":"invalid_request_error","code":"invalid_api_key"}}"#,
        );
        let (addr, handle) = mock(vec![Script::Respond(denied)]);
        let err = complete_tools_step(
            &custom(addr),
            "s",
            &[AgentMsg::User("q".into())],
            &[],
            64,
            &never(),
        )
        .unwrap_err();
        assert!(matches!(err, LlmError::Status { code: 401, .. }), "{err:?}");
        assert_eq!(handle.join().unwrap().served, 1);
    }

    /// Some OpenAI-compatible endpoints refuse `stream: true`. Losing the
    /// cancellation seam beats losing the turn, so the step retries as the
    /// blocking POST — but only because the stream was REFUSED. A stopped turn
    /// skips the retry instead of paying for a step nobody will read.
    #[test]
    fn tools_step_falls_back_when_the_stream_is_refused() {
        let refused = http("400 Bad Request", "text/plain", "streaming unsupported");
        let ok = http(
            "200 OK",
            "application/json",
            r#"{"choices":[{"message":{"content":"hi"},"finish_reason":"stop"}]}"#,
        );
        let (addr, handle) = mock(vec![Script::Respond(refused.clone()), Script::Respond(ok)]);
        let out = complete_tools_step(
            &custom(addr),
            "s",
            &[AgentMsg::User("q".into())],
            &[],
            64,
            &never(),
        )
        .expect("the blocking form answers");
        assert_eq!(out.text, "hi");
        let log = handle.join().unwrap();
        assert_eq!(log.served, 2, "stream refused, then blocking");
        assert!(log.bodies[0].contains("\"stream\":true"));
        assert!(
            !log.bodies[1].contains("\"stream\""),
            "the fallback is unstreamed"
        );

        // Stopped while the refused request is still out: the refusal lands
        // after the stop, and must not be answered with the fallback.
        let stop = StopOnRequest::new();
        let (addr, handle) = mock_hooked(
            vec![Script::Stall {
                head: String::new(),
                stall: Duration::from_millis(400),
                tail: refused,
            }],
            stop.hook(),
        );
        let err = complete_tools_step(
            &custom(addr),
            "s",
            &[AgentMsg::User("q".into())],
            &[],
            64,
            &|| stop.stopped(),
        )
        .unwrap_err();
        assert_eq!(err, LlmError::Cancelled);
        assert_eq!(err.to_string(), CANCELLED);
        assert_eq!(
            handle.join().unwrap().served,
            1,
            "no fallback for a stopped turn"
        );
    }

    #[test]
    fn send_retries_transient_and_honors_retry_after() {
        let overloaded = "HTTP/1.1 529 Overloaded\r\nretry-after: 0\r\n\
                          connection: close\r\ncontent-length: 0\r\n\r\n"
            .to_string();
        let (addr, handle) = mock(vec![
            Script::Respond(overloaded),
            Script::Respond(http("200 OK", "text/plain", "ok")),
        ]);
        let resp =
            send_with_retry(&plain(&addr), &AtomicBool::new(false)).expect("retried to success");
        assert_eq!(read_body(resp).unwrap(), "ok");
        assert_eq!(handle.join().unwrap().served, 2);
    }

    #[test]
    fn send_fails_fast_on_non_transient_status() {
        let (addr, handle) = mock(vec![Script::Respond(http(
            "400 Bad Request",
            "text/plain",
            "nope",
        ))]);
        let err = send_with_retry(&plain(&addr), &AtomicBool::new(false)).unwrap_err();
        assert!(
            err.to_string().contains("400"),
            "error names the status: {err}"
        );
        assert_eq!(handle.join().unwrap().served, 1, "no retry on 400");
    }

    /// A gateway's 502/504 reports that its upstream failed or went quiet — the
    /// model may be generating behind it — so it is reported, not resent: a
    /// resend could be billed twice, the same reason a transport failure after
    /// the request left is never resent. (`retry-after: 0` makes any resend
    /// immediate, so the mock would count it.) The provider's own refusals
    /// still are retried.
    #[test]
    fn a_gateway_error_is_reported_not_resent() {
        for status in ["502 Bad Gateway", "504 Gateway Timeout"] {
            let resp = format!(
                "HTTP/1.1 {status}\r\nretry-after: 0\r\nconnection: close\r\n\
                 content-length: 0\r\n\r\n"
            );
            let (addr, handle) = mock(vec![Script::Respond(resp)]);
            let err = send_with_retry(&plain(&addr), &AtomicBool::new(false)).unwrap_err();
            assert!(err.to_string().contains(&status[..3]), "{err}");
            assert_eq!(
                handle.join().unwrap().served,
                1,
                "{status} must not be resent"
            );
        }
        for code in [408, 429, 500, 503, 529] {
            assert!(
                transient_status(code),
                "{code} is the provider's own refusal"
            );
        }
    }

    /// A caller that stopped waiting must not keep a worker retrying behind it.
    #[test]
    fn an_abandoned_send_makes_no_further_attempt() {
        let err =
            send_with_retry(&plain("http://127.0.0.1:1/"), &AtomicBool::new(true)).unwrap_err();
        assert_eq!(err, LlmError::Cancelled);
    }

    // -- error text (C7) -----------------------------------------------------

    /// The provider's own message, not its raw JSON, reaches the user — while
    /// the status code and the provider's error kind stay in the text, because
    /// callers holding only a string (the explain pass's auth-failure check)
    /// match on `401` / `invalid_api_key` / `authentication`.
    #[test]
    fn provider_errors_read_as_their_message_and_keep_code_and_kind() {
        let anthropic = r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#;
        assert_eq!(
            provider_error(anthropic),
            (
                Some("authentication_error".into()),
                "invalid x-api-key".into()
            )
        );
        let openai = r#"{"error":{"message":"Incorrect API key provided: sk-...","type":"invalid_request_error","param":null,"code":"invalid_api_key"}}"#;
        assert_eq!(
            provider_error(openai),
            (
                Some("invalid_api_key".into()),
                "Incorrect API key provided: sk-...".into()
            )
        );
        // Other shapes compatible servers use.
        assert_eq!(
            provider_error(r#"{"error":"model not found"}"#).1,
            "model not found"
        );
        assert_eq!(
            provider_error(r#"{"detail":"Not authenticated"}"#).1,
            "Not authenticated"
        );
        assert_eq!(
            provider_error("upstream timed out\n<html>").1,
            "upstream timed out"
        );
        assert_eq!(provider_error("").1, "(no details)");

        let (addr, handle) = mock(vec![Script::Respond(http(
            "401 Unauthorized",
            "application/json",
            anthropic,
        ))]);
        let err = complete_chat(&anthropic_at(addr), "s", &[ChatMsg::user("q")], 16).unwrap_err();
        handle.join().unwrap();
        assert_eq!(
            err,
            "Anthropic API error 401 (authentication_error): invalid x-api-key"
        );
        assert!(!err.contains('{'), "no raw JSON in the message: {err}");
    }

    // -- redirects -----------------------------------------------------------

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
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let recorder = seen.clone();
        std::thread::spawn(move || {
            for conn in sink.incoming() {
                let Ok(mut conn) = conn else { break };
                // The whole request, bounded (a single read could stop
                // before the header that carries the key).
                let request = crate::testutil::read_http(&mut conn);
                recorder
                    .lock()
                    .unwrap()
                    .push(format!("{}\r\n\r\n{}", request.head, request.body));
                let _ = conn.write_all(
                    b"HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-length: 2\r\n\r\nok",
                );
            }
        });

        let moved = format!(
            "HTTP/1.1 302 Found\r\nlocation: http://{sink_addr}/v1/messages\r\n\
             connection: close\r\ncontent-length: 0\r\n\r\n"
        );
        let (addr, handle) = mock(vec![Script::Respond(moved)]);
        let cfg = Config::from_parts(Provider::Anthropic, "SECRET-KEY".into(), "m".into(), addr);
        let err = complete_chat(&cfg, "s", &[ChatMsg::user("q")], 16)
            .expect_err("a 3xx is not a completion");

        assert!(err.contains("redirected"), "the refusal is reported: {err}");
        assert_eq!(
            handle.join().unwrap().served,
            1,
            "the redirect is not followed"
        );
        // Give a wrongly-followed redirect time to land before concluding it
        // did not happen: the hop would be on the client's thread, which has
        // already returned, but the accept on the sink is on another one.
        std::thread::sleep(Duration::from_millis(200));
        let seen = seen.lock().unwrap();
        assert!(
            seen.is_empty(),
            "the redirect target was contacted at all: {seen:?}"
        );
    }

    // -- stream reading (C6) -------------------------------------------------

    #[test]
    fn read_sse_collects_deltas_and_stops_at_done() {
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"hel\"}}]}\n\n\
                    data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n\
                    data: [DONE]\n\n\
                    data: {\"choices\":[{\"delta\":{\"content\":\"ignored\"}}]}\n";
        let (seen, out) = collect_text(body, Dialect::OpenAi).unwrap();
        assert_eq!(out.text, "hello");
        assert_eq!(seen, "hello");
    }

    #[test]
    fn read_sse_surfaces_in_stream_error_events() {
        // Anthropic-style mid-stream failure: the text so far must not be
        // returned as a completed answer.
        let body = "data: {\"type\":\"content_block_delta\",\"index\":0,\
                    \"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n\n\
                    data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\
                    \"message\":\"Overloaded\"}}\n";
        let err = collect_text(body, Dialect::Anthropic).unwrap_err();
        assert_eq!(
            err,
            LlmError::Stream("Overloaded (overloaded_error)".into())
        );

        // OpenAI-compatible style: a bare `error` object.
        let body = "data: {\"error\":{\"message\":\"quota exceeded\"}}\n";
        let err = collect_text(body, Dialect::OpenAi).unwrap_err();
        assert!(err.to_string().contains("quota exceeded"), "{err}");

        // `"error": null` on a healthy chunk (some proxies do this) is NOT an
        // error.
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}],\"error\":null}\n\n\
                    data: [DONE]\n";
        assert_eq!(collect_text(body, Dialect::OpenAi).unwrap().1.text, "ok");
    }

    #[test]
    fn read_sse_rejects_eof_without_an_end() {
        // Connection dropped mid-answer: no [DONE], no message_stop, no stop
        // reason — the partial text must not be returned as an answer.
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"half an ans\"}}]}\n";
        let err = collect_text(body, Dialect::OpenAi).unwrap_err();
        assert!(err.to_string().contains("ended before completion"), "{err}");

        // The Anthropic terminator counts.
        let body = "data: {\"type\":\"content_block_delta\",\"index\":0,\
                    \"delta\":{\"type\":\"text_delta\",\"text\":\"whole\"}}\n\n\
                    data: {\"type\":\"message_stop\"}\n";
        assert_eq!(
            collect_text(body, Dialect::Anthropic).unwrap().1.text,
            "whole"
        );

        // So does a stated stop reason, for servers that close without
        // `[DONE]`: an answer whose end was announced is complete.
        let body =
            "data: {\"choices\":[{\"delta\":{\"content\":\"done\"},\"finish_reason\":\"stop\"}]}\n";
        assert_eq!(collect_text(body, Dialect::OpenAi).unwrap().1.text, "done");
    }

    /// The predicate is re-tested before every event, so a flag flipped
    /// mid-answer stops the loop where it stands: the remaining tokens are
    /// never delivered and the caller gets [`CANCELLED`] rather than a text
    /// that looks complete.
    #[test]
    fn read_sse_stops_consuming_once_cancelled() {
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"one\"}}]}\n\n\
                    data: {\"choices\":[{\"delta\":{\"content\":\"two\"}}]}\n\n\
                    data: [DONE]\n";
        let stop = AtomicBool::new(false);
        let mut seen = String::new();
        let err = collect_stream(
            sse_bytes(body),
            Dialect::OpenAi,
            &mut |d| {
                seen.push_str(d);
                stop.store(true, Ordering::Relaxed); // "Stop" pressed
            },
            &|| stop.load(Ordering::Relaxed),
        )
        .unwrap_err();
        assert_eq!(err, LlmError::Cancelled);
        assert_eq!(seen, "one", "no token after the stop reached the caller");
    }

    /// A socket read timeout ENDS the stream. The old loop resumed after it to
    /// re-test cancellation, but ureq's chunked decoder is not safe to resume
    /// mid-chunk: bytes already consumed from a chunk-size line are lost and
    /// the rest of the body misparses. Nothing may be read past the error.
    #[test]
    fn a_read_timeout_ends_the_stream_instead_of_resuming() {
        struct TimesOutOnce(u8);
        impl Read for TimesOutOnce {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                self.0 += 1;
                let chunk: &[u8] = match self.0 {
                    1 => b"data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n",
                    2 => return Err(std::io::ErrorKind::TimedOut.into()),
                    3 => b"data: {\"choices\":[{\"delta\":{\"content\":\"b\"}}]}\ndata: [DONE]\n",
                    _ => b"",
                };
                buf[..chunk.len()].copy_from_slice(chunk);
                Ok(chunk.len())
            }
        }
        let stream = spawn_sse(|_| Ok(Box::new(TimesOutOnce(0)) as Box<dyn Read + Send>)).unwrap();
        let mut seen = String::new();
        let err = collect_stream(stream, Dialect::OpenAi, &mut |d| seen.push_str(d), &|| {
            false
        })
        .unwrap_err();
        assert!(
            matches!(err, LlmError::Transport(ref m) if m.contains("stream read")),
            "{err:?}"
        );
        assert_eq!(seen, "a", "nothing after the timeout was read");
    }

    /// The reason the stream is read on its own thread. A provider that goes
    /// silent mid-answer used to hold the caller inside a socket read until the
    /// read timeout, so Stop landed a whole timeout late. Now the caller is
    /// released at once, and the connection goes with it
    /// (`an_abandoned_plain_http_stream_closes_its_connection_at_once` pins
    /// when).
    #[test]
    fn a_stop_releases_the_caller_while_the_provider_is_silent() {
        let head = format!(
            "{}data: {{\"choices\":[{{\"delta\":{{\"content\":\"first\"}}}}]}}\n\n",
            sse_head()
        );
        let tail = "data: {\"choices\":[{\"delta\":{\"content\":\"more\"}}]}\n\n".repeat(64);
        let (addr, handle) = mock(vec![Script::Stall {
            head,
            stall: Duration::from_millis(1500),
            tail,
        }]);
        let stop = AtomicBool::new(false);
        let stopped_at = std::cell::Cell::new(None);
        let err = complete_chat_stream_full(
            &custom(addr),
            "s",
            &[ChatMsg::user("q")],
            64,
            &mut |_| {
                stopped_at.set(Some(Instant::now()));
                stop.store(true, Ordering::Relaxed);
            },
            &|| stop.load(Ordering::Relaxed),
        )
        .unwrap_err();
        assert_eq!(err, LlmError::Cancelled);
        let waited = stopped_at.get().expect("the first token arrived").elapsed();
        assert!(
            waited < Duration::from_millis(1000),
            "the caller must not wait out the provider's silence: {waited:?}"
        );
        let log = handle.join().unwrap();
        assert_eq!(
            log.closed_after_stall,
            vec![true],
            "the abandoned stream must close its connection"
        );
    }

    /// Stop also has to reach a request still waiting for its response
    /// headers — the window a caller-thread read could never be interrupted in.
    #[test]
    fn a_stop_releases_the_caller_before_the_headers_arrive() {
        let stop = StopOnRequest::new();
        let (addr, handle) = mock_hooked(
            vec![Script::Stall {
                head: String::new(),
                stall: Duration::from_millis(1500),
                tail: http("200 OK", "application/json", "{}"),
            }],
            stop.hook(),
        );
        let err = complete_tools_step(
            &custom(addr),
            "s",
            &[AgentMsg::User("q".into())],
            &[],
            64,
            &|| stop.stopped(),
        )
        .unwrap_err();
        assert_eq!(err, LlmError::Cancelled);
        assert!(
            stop.since() < Duration::from_millis(1000),
            "{:?}",
            stop.since()
        );
        assert_eq!(
            handle.join().unwrap().served,
            1,
            "no fallback for a stopped step"
        );
    }

    // -- cancellable connections (C6) ----------------------------------------

    /// Client trust for [`silent_tls_provider`]: the test certificate,
    /// nothing else.
    fn trusting_the_mock() -> Arc<rustls::ClientConfig> {
        crate::testutil::trusting_the_test_cert()
    }

    /// A provider on 127.0.0.1 speaking TLS that reads one request, sends
    /// `head` (possibly nothing), and then never sends another byte — the
    /// provider that is generating a long answer, or has gone quiet. Reports
    /// when the request is in, and when the client's end of the connection
    /// closed.
    fn silent_tls_provider(
        head: &'static str,
    ) -> (
        String,
        std::sync::mpsc::Receiver<()>,
        std::sync::mpsc::Receiver<Instant>,
    ) {
        let config = crate::testutil::test_tls_server_config();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "https://{}/v1/chat/completions",
            listener.local_addr().unwrap()
        );
        let (arrived_tx, arrived) = std::sync::mpsc::channel();
        let (closed_tx, closed) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let Ok((tcp, _)) = listener.accept() else {
                return;
            };
            // Bounded, so a client that never lets go fails the test instead
            // of hanging it.
            tcp.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
            let mut tls =
                rustls::StreamOwned::new(rustls::ServerConnection::new(config).unwrap(), tcp);
            read_request(&mut tls);
            let _ = arrived_tx.send(());
            let _ = tls.write_all(head.as_bytes());
            let _ = tls.flush();
            // The client is never answered further; this read ends when its
            // side of the connection goes away (or at the bound above).
            let mut probe = [0u8; 1];
            let _ = std::io::Read::read(&mut tls, &mut probe);
            let _ = closed_tx.send(Instant::now());
        });
        (url, arrived, closed)
    }

    /// How long after `since` the provider saw the connection close — a
    /// failure when it did not within a few seconds.
    fn closed_within(closed: &std::sync::mpsc::Receiver<Instant>, since: Instant) -> Duration {
        let at = closed
            .recv_timeout(Duration::from_secs(10))
            .expect("the provider must see the connection close");
        at.saturating_duration_since(since)
    }

    /// An abandoned stream closes its connection AT ONCE. It used to stay open
    /// until the provider's next event failed the reader's send: a provider
    /// that kept generating kept billing until then, and one that had gone
    /// quiet held the socket (and the reader thread) for the 180 s idle limit.
    /// The provider here never sends a byte after the headers, so only the
    /// shutdown the abandon performs can end the read.
    #[test]
    fn an_abandoned_stream_closes_its_connection_at_once() {
        let (url, arrived, closed) = silent_tls_provider(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
        );
        let agent = build_agent_trusting(
            STREAM_LIMITS,
            crate::net::Route::Direct,
            trusting_the_mock(),
        );
        let prepared = raw(&agent, &url);
        let stream = spawn_sse(move |abandoned| {
            send_with_retry(&prepared, abandoned)
                .map(|resp| -> Box<dyn Read + Send> { Box::new(resp.into_body().into_reader()) })
        })
        .unwrap();
        arrived
            .recv_timeout(Duration::from_secs(10))
            .expect("the request reached the provider");
        // Let the headers land, so the reader is parked in the body.
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            closed.try_recv().is_err(),
            "the connection must still be open while the stream is being read"
        );
        let abandoned = Instant::now();
        drop(stream);
        let waited = closed_within(&closed, abandoned);
        assert!(
            waited < Duration::from_secs(2),
            "the abandon must close the connection, not wait for the provider: {waited:?}"
        );
    }

    /// fixR5 #2: the same over plain http — a model server on this machine
    /// or the LAN. Its connection used to be out of the abandon's reach (ureq
    /// 2 handed out only a TLS connection's socket): the provider kept
    /// generating until its next line, or the 180 s idle limit, and the
    /// reader thread with it. Now the connection closes at the abandon, and
    /// the reader thread ends with it.
    #[test]
    fn an_abandoned_plain_http_stream_closes_its_connection_at_once() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://{}/v1/chat/completions",
            listener.local_addr().unwrap()
        );
        let (arrived_tx, arrived) = std::sync::mpsc::channel();
        let (closed_tx, closed) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let Ok((mut conn, _)) = listener.accept() else {
                return;
            };
            read_request(&mut conn);
            let _ = arrived_tx.send(());
            let _ = conn.write_all(sse_head().as_bytes());
            // Silent from here; the read ends when the client's side goes
            // (or at the bound, which fails the test).
            conn.set_read_timeout(Some(Duration::from_secs(30)))
                .unwrap();
            let _ = std::io::Read::read(&mut conn, &mut [0u8; 1]);
            let _ = closed_tx.send(Instant::now());
        });
        let agent = build_agent(STREAM_LIMITS, crate::net::Route::Direct);
        let prepared = raw(&agent, &url);
        let stream = spawn_sse(move |abandoned| {
            send_with_retry(&prepared, abandoned)
                .map(|resp| -> Box<dyn Read + Send> { Box::new(resp.into_body().into_reader()) })
        })
        .unwrap();
        arrived
            .recv_timeout(Duration::from_secs(10))
            .expect("the request reached the provider");
        // Let the headers land, so the reader is parked in the body.
        std::thread::sleep(Duration::from_millis(300));
        let worker = Arc::downgrade(&stream._abandon.line);
        let abandoned = Instant::now();
        drop(stream);
        let waited = closed_within(&closed, abandoned);
        assert!(
            waited < Duration::from_secs(2),
            "the abandon must close the connection, not wait for the provider: {waited:?}"
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        while worker.strong_count() > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            worker.strong_count(),
            0,
            "the reader thread outlived the abandon"
        );
    }

    /// fixR5 #9: a model server on the LAN, over plain http, behind a SOCKS
    /// proxy from the system's settings — which covers plain http too when no
    /// http proxy is set — is reached through it, by name; R4 refused plain
    /// http through SOCKS, so such a server could not be reached at all. A
    /// system proxy that asks for the password it keeps in the keychain says
    /// so, and what to do, instead of passing for the provider's refusal.
    #[test]
    fn a_lan_model_server_behind_a_system_socks_proxy_is_reached() {
        let answer = http(
            "200 OK",
            "application/json",
            r#"{"choices":[{"message":{"content":"hi"},"finish_reason":"stop"}]}"#,
        );
        let (addr, handle) = mock(vec![Script::Respond(answer)]);
        let server: std::net::SocketAddr = addr.trim_start_matches("http://").parse().unwrap();
        let (socks, seen) =
            crate::testutil::socks5_proxy(None, crate::testutil::SocksAnswer::RelayTo(server));
        let (host, port) = socks.split_once(':').unwrap();
        let system = |host: &str, port: &str| {
            let (host, port) = (host.to_string(), port.parse::<i64>().unwrap());
            move || {
                use crate::net::SystemValue::{Number, Text};
                let dict = [
                    ("SOCKSEnable", Number(1)),
                    ("SOCKSProxy", Text(host.clone())),
                    ("SOCKSPort", Number(port)),
                ];
                crate::net::ProxyConfig::from_system(&|key| {
                    dict.iter().find(|(k, _)| *k == key).map(|(_, v)| v.clone())
                })
            }
        };
        let socks_settings = system(host, port);
        let proxies = crate::net::ProxySources {
            env: &|_| None,
            system: &socks_settings,
        };
        let url = "http://lan-model.invalid:11434/v1/chat/completions";
        let (agent, via) = agent_with_proxies(url, BLOCKING_LIMITS, &proxies).unwrap();
        assert!(via.as_ref().is_some_and(|via| via.system.is_some()));
        let prepared = Prepared {
            via,
            ..raw(&agent, url)
        };
        let text = run_blocking(prepared, &|| false).expect("the server answered");
        assert!(text.contains("hi"), "{text}");
        let seen = seen.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(seen.target, Some((3, "lan-model.invalid".into(), 11434)));
        assert_eq!(handle.join().unwrap().served, 1);

        // An http proxy from the system's settings that wants a password.
        let (proxy_url, _heads) = crate::testutil::http_proxy(
            "127.0.0.1:0",
            false,
            1,
            crate::testutil::ProxyAnswer::Status(407),
        )
        .unwrap();
        let proxy_addr = proxy_url.trim_start_matches("http://").to_string();
        let (host, port) = proxy_addr.split_once(':').unwrap();
        let (host, port) = (host.to_string(), port.parse::<i64>().unwrap());
        let keychain = move || {
            use crate::net::SystemValue::{Number, Text};
            let dict = [
                ("HTTPEnable", Number(1)),
                ("HTTPProxy", Text(host.clone())),
                ("HTTPPort", Number(port)),
            ];
            crate::net::ProxyConfig::from_system(&|key| {
                dict.iter().find(|(k, _)| *k == key).map(|(_, v)| v.clone())
            })
        };
        let proxies = crate::net::ProxySources {
            env: &|_| None,
            system: &keychain,
        };
        let (agent, via) = agent_with_proxies(url, BLOCKING_LIMITS, &proxies).unwrap();
        let prepared = Prepared {
            via,
            ..raw(&agent, url)
        };
        let err = run_blocking(prepared, &|| false).unwrap_err();
        let text = err.to_string();
        assert!(matches!(err, LlmError::Connect(_)), "{err:?}");
        assert!(text.contains("407") && text.contains("keychain"), "{text}");
        assert!(
            text.contains("Bypass proxy settings for these hosts"),
            "{text}"
        );
    }

    /// Same for a blocking call, whose socket is silent for the WHOLE
    /// generation: a stopped call used to leave its worker holding the
    /// connection until the provider finished (billing all of it) or the 600 s
    /// deadline. Here the provider never answers at all.
    #[test]
    fn a_stopped_blocking_call_closes_its_connection_at_once() {
        let (url, arrived, closed) = silent_tls_provider("");
        let agent = build_agent_trusting(
            BLOCKING_LIMITS,
            crate::net::Route::Direct,
            trusting_the_mock(),
        );
        let prepared = raw(&agent, &url);
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        std::thread::spawn(move || {
            if arrived.recv_timeout(Duration::from_secs(10)).is_ok() {
                flag.store(true, Ordering::Relaxed);
            }
        });
        let err = run_blocking(prepared, &|| stop.load(Ordering::Relaxed)).unwrap_err();
        assert_eq!(err, LlmError::Cancelled);
        let waited = closed_within(&closed, Instant::now());
        assert!(
            waited < Duration::from_secs(2),
            "the stop must close the connection, not leave it to the provider: {waited:?}"
        );
    }

    /// A stopped call lets a silent SOCKS proxy go too: the tunnel is opened
    /// on the call's own connection, which its line holds, so the stop shuts
    /// the handshake down. ureq's own SOCKS ran it on a helper thread and
    /// socket that nothing could reach, and left both behind.
    #[test]
    fn a_stopped_call_lets_a_silent_socks_proxy_go() {
        let (spec, greeted, closed) = crate::testutil::silent_socks_proxy();
        let url = "https://api.example.invalid/v1/chat/completions";
        let route = crate::net::route(Some(&spec)).unwrap();
        let agent = build_agent_trusting(BLOCKING_LIMITS, route, trusting_the_mock());
        let prepared = raw(&agent, url);
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        std::thread::spawn(move || {
            if greeted.recv_timeout(Duration::from_secs(10)).is_ok() {
                flag.store(true, Ordering::Relaxed);
            }
        });
        let err = run_blocking(prepared, &|| stop.load(Ordering::Relaxed)).unwrap_err();
        let stopped = Instant::now();
        assert_eq!(err, LlmError::Cancelled);
        let (gone, at) = closed.recv_timeout(Duration::from_secs(60)).unwrap();
        assert!(
            gone && at.saturating_duration_since(stopped) < Duration::from_secs(2),
            "the connection to the proxy outlived the stop"
        );
    }

    /// A line closed before its connection exists closes that connection as
    /// it is taken onto the line — the abandon that lands mid-connect or
    /// mid-handshake.
    #[test]
    fn a_connection_made_after_the_abandon_is_closed_on_arrival() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut server_side, _) = listener.accept().unwrap();
        server_side
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let line = Arc::new(crate::net::Line::default());
        line.close();
        crate::net::Lease::new(client.try_clone().unwrap()).take_up(Some(line));
        let mut probe = [0u8; 1];
        assert_eq!(
            std::io::Read::read(&mut server_side, &mut probe).unwrap(),
            0,
            "the peer must see the connection end"
        );
    }

    // -- blocking calls (C4) -------------------------------------------------

    /// A blocking completion has no socket-level seam, but the CALLER must not
    /// be held hostage by it: the POST runs on a worker and the stop flag is
    /// polled while it is out. (`Request::Chat` relies on this for `Cancel`.)
    #[test]
    fn a_blocking_call_yields_to_a_stop() {
        let stop = StopOnRequest::new();
        let (addr, handle) = mock_hooked(
            vec![Script::Stall {
                head: String::new(),
                stall: Duration::from_millis(1500),
                tail: http(
                    "200 OK",
                    "application/json",
                    r#"{"choices":[{"message":{"content":"late"},"finish_reason":"stop"}]}"#,
                ),
            }],
            stop.hook(),
        );
        let err = complete_chat_full(&custom(addr), "s", &[ChatMsg::user("q")], 64, &|| {
            stop.stopped()
        })
        .unwrap_err();
        assert_eq!(err, LlmError::Cancelled);
        assert!(
            stop.since() < Duration::from_millis(1000),
            "{:?}",
            stop.since()
        );
        assert_eq!(handle.join().unwrap().served, 1);
    }

    /// The blocking profile carries an overall deadline, not just a read
    /// timeout: a server trickling its body a byte at a time never trips a
    /// read timeout, and used to hold the worker (and, on the server, the
    /// process) forever.
    ///
    /// What tells the deadline apart from any other ending is the error
    /// itself — a timeout, while the body is still arriving — and the time:
    /// the server trickles for a minute (stopping only once the client has
    /// hung up), far past the 400 ms deadline and the 20 s the check allows.
    #[test]
    fn the_blocking_profile_has_an_overall_deadline() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = format!("http://{}/", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            let Ok((mut conn, _)) = listener.accept() else {
                return;
            };
            let _ = read_request(&mut conn);
            let _ = conn.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 100000\r\n\r\n");
            for _ in 0..1200 {
                if conn.write_all(b"x").is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });
        let limits = Limits {
            total: Some(Duration::from_millis(400)),
            ..BLOCKING_LIMITS
        };
        let began = Instant::now();
        let result = raw(&build_agent(limits, crate::net::Route::Direct), &addr)
            .send()
            .map_err(|e| crate::net::describe(&e))
            .and_then(|r| read_body(r).map_err(|e| e.to_string()));
        let err = result.expect_err("a trickling body must hit the deadline");
        assert!(err.contains("timed out"), "not the deadline: {err}");
        assert!(
            began.elapsed() < Duration::from_secs(20),
            "{:?}",
            began.elapsed()
        );
    }

    // -- tool steps ----------------------------------------------------------

    /// The agent's closing answer streams through here, and its turn is stopped
    /// with a flag.
    #[test]
    fn tools_stream_honors_the_turns_stop_flag() {
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"one\"}}]}\n\n\
                   data: {\"choices\":[{\"delta\":{\"content\":\"two\"}}]}\n\n\
                   data: [DONE]\n\n";
        let (addr, handle) = mock(vec![Script::Respond(http(
            "200 OK",
            "text/event-stream",
            sse,
        ))]);
        let stop = AtomicBool::new(false);
        let mut seen = String::new();
        let err = complete_tools_stream(
            &custom(addr),
            "system",
            &[AgentMsg::User("q".into())],
            &[],
            64,
            |d| {
                seen.push_str(d);
                stop.store(true, Ordering::Relaxed);
            },
            &|| stop.load(Ordering::Relaxed),
        )
        .unwrap_err();
        assert_eq!(err, LlmError::Cancelled, "the stop must abort the stream");
        assert_eq!(seen, "one", "the rest of the answer is never consumed");
        assert_eq!(handle.join().unwrap().served, 1);
    }

    /// Some OpenAI-compatible endpoints ignore `tool_choice: none` and stream a
    /// tool call as the "closing answer". Reading only the text turned that
    /// into an empty answer; the call has to come back to the caller.
    #[test]
    fn a_closing_answer_that_streams_a_tool_call_reports_it() {
        let sse = concat!(
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_9","#,
            r#""function":{"name":"read","arguments":"{\"file\":\"a.rs\"}"}}]}}]}"#,
            "\n\n",
            r#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            "\n\ndata: [DONE]\n\n",
        );
        let (addr, handle) = mock(vec![Script::Respond(http(
            "200 OK",
            "text/event-stream",
            sse,
        ))]);
        let mut seen = String::new();
        let out = complete_tools_stream(
            &custom(addr),
            "s",
            &[AgentMsg::User("q".into())],
            &[],
            64,
            |d| seen.push_str(d),
            &|| false,
        )
        .expect("a complete stream");
        handle.join().unwrap();
        assert!(out.text.is_empty() && seen.is_empty());
        assert_eq!(out.calls.len(), 1);
        assert_eq!(out.calls[0].name, "read");
        assert_eq!(out.calls[0].args["file"], "a.rs");
        assert_eq!(out.stop_reason.as_deref(), Some("tool_calls"));
    }

    /// An exploration step is streamed only so it can be stopped, so nothing
    /// about the step may be lost in the reassembly: tool arguments arrive as
    /// fragments of one JSON document and only the concatenation parses, and a
    /// step that hit `max_tokens` has to keep saying so or the caller runs
    /// half-written calls instead of retrying.
    #[test]
    fn openai_stream_rejoins_fragmented_tool_calls() {
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
        let (_, out) = collect_text(sse, Dialect::OpenAi).expect("a complete stream");
        assert_eq!(out.text, "looking");
        assert_eq!(out.calls.len(), 1, "the fragments are ONE call");
        assert_eq!(out.calls[0].id, "call_1");
        assert_eq!(out.calls[0].name, "search");
        assert_eq!(out.calls[0].args["q"], "needle");
        assert!(out.truncated, "`length` still reports as truncated");
    }

    /// Anthropic's stream: text and tool-use blocks come back in emission
    /// order, and reasoning deltas — which cannot occur, since extended
    /// thinking is never requested — are ignored rather than mistaken for text.
    #[test]
    fn anthropic_stream_rebuilds_text_and_tool_calls() {
        let sse = concat!(
            r#"data: {"type":"content_block_start","index":0,"#,
            r#""content_block":{"type":"thinking","thinking":""}}"#,
            "\n\n",
            r#"data: {"type":"content_block_delta","index":0,"#,
            r#""delta":{"type":"thinking_delta","thinking":"weighing"}}"#,
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
            r#""delta":{"type":"input_json_delta","partial_json":"{\"file\": "}}"#,
            "\n\n",
            r#"data: {"type":"content_block_delta","index":2,"#,
            r#""delta":{"type":"input_json_delta","partial_json":"\"src/lib.rs\"}"}}"#,
            "\n\n",
            r#"data: {"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
            "\n\n",
            r#"data: {"type":"message_stop"}"#,
            "\n\n",
        );
        let (seen, out) = collect_text(sse, Dialect::Anthropic).expect("a complete stream");
        assert_eq!(out.text, "reading");
        assert_eq!(seen, "reading", "reasoning never reaches the reader");
        assert_eq!(out.calls.len(), 1);
        assert_eq!(out.calls[0].id, "toolu_1");
        assert_eq!(out.calls[0].name, "read");
        assert_eq!(out.calls[0].args["file"], "src/lib.rs");
        assert!(!out.truncated);
        assert_eq!(out.stop_reason.as_deref(), Some("tool_use"));
    }

    /// The reason an exploration step streams at all: a Stop pressed while the
    /// step is on the wire has to reach it.
    #[test]
    fn tools_step_stops_a_request_already_on_the_wire() {
        let sse = concat!(
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","#,
            r#""function":{"name":"search","arguments":"{}"}}]}}]}"#,
            "\n\n",
            r#"data: {"choices":[{"delta":{"content":"more"}}]}"#,
            "\n\ndata: [DONE]\n\n",
        );
        let (addr, handle) = mock(vec![Script::Respond(http(
            "200 OK",
            "text/event-stream",
            sse,
        ))]);
        // "Stop" pressed once the step is already streaming.
        let polls = std::cell::Cell::new(0u32);
        let err = complete_tools_step(
            &custom(addr),
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
        assert_eq!(err, LlmError::Cancelled, "the stop must abort the step");
        assert_eq!(
            handle.join().unwrap().served,
            1,
            "a stopped step must not spend a second billed request"
        );
    }

    /// An exploration step must stay tool-capable: only the closing answer is
    /// pinned to prose.
    #[test]
    fn only_the_closing_answer_pins_tool_choice_none() {
        let cfg = custom("http://x".into());
        let msgs = [AgentMsg::User("q".into())];
        let parse = |wire| -> Value {
            serde_json::from_str(&tools_body(&cfg, "s", &msgs, &[], 64, wire)).unwrap()
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

        let anthropic = anthropic_at("http://x".into());
        let step: Value =
            serde_json::from_str(&tools_body(&anthropic, "s", &msgs, &[], 64, Wire::Stream))
                .unwrap();
        assert_eq!(step["stream"], true);
        assert!(step.get("tool_choice").is_none());
    }

    /// One place decides the budget field and the headers for every entry
    /// point (C23): the same heuristic for chat and tool bodies.
    #[test]
    fn every_body_uses_the_one_max_tokens_heuristic() {
        for (model, field) in [
            ("gpt-5-mini", "max_completion_tokens"),
            ("o3", "max_completion_tokens"),
            ("gpt-4o-mini", "max_tokens"),
            ("deepseek-v4-flash", "max_tokens"),
        ] {
            let cfg = Config::from_parts(Provider::OpenAI, "k".into(), model.into(), String::new());
            let chat: Value =
                serde_json::from_str(&chat_body(&cfg, "s", &[ChatMsg::user("q")], 7, false))
                    .unwrap();
            let tools: Value = serde_json::from_str(&tools_body(
                &cfg,
                "s",
                &[AgentMsg::User("q".into())],
                &[],
                7,
                Wire::Blocking,
            ))
            .unwrap();
            assert_eq!(chat[field], 7, "{model}");
            assert_eq!(tools[field], 7, "{model}");
        }
        let cfg = Config::from_parts(Provider::Custom, "k".into(), "m".into(), String::new());
        assert!(matches!(
            complete_chat_full(&cfg, "s", &[ChatMsg::user("q")], 7, &|| false),
            Err(LlmError::Config(_))
        ));
    }

    #[test]
    fn tail_cache_marks_last_block_only() {
        // String content is lifted into a marked block array.
        let mut msgs = vec![
            json!({ "role": "user", "content": "first" }),
            json!({ "role": "user", "content": "question" }),
        ];
        mark_tail_cache(&mut msgs);
        assert!(msgs[0]["content"].is_string(), "earlier messages untouched");
        assert_eq!(msgs[1]["content"][0]["text"], "question");
        assert_eq!(msgs[1]["content"][0]["cache_control"]["type"], "ephemeral");

        // Block-array content: only the last block is marked.
        let mut msgs = vec![json!({
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
        let mut msgs = vec![json!({ "role": "user", "content": "" })];
        mark_tail_cache(&mut msgs);
        assert_eq!(msgs[0]["content"], "");
    }

    // -- truncation (C8) -----------------------------------------------------

    /// A cut-off answer must read as cut off. The blocking Anthropic path used
    /// to return the FIRST text block and ignore `stop_reason`, so a truncated
    /// answer (or the second half of a split one) passed as complete.
    #[test]
    fn a_truncated_answer_says_so_in_every_path() {
        let body = r#"{"content":[{"type":"text","text":"first half, "},{"type":"text","text":"second half"}],"stop_reason":"max_tokens"}"#;
        let (addr, handle) = mock(vec![Script::Respond(http(
            "200 OK",
            "application/json",
            body,
        ))]);
        let done = complete_chat_full(&anthropic_at(addr), "s", &[ChatMsg::user("q")], 8, &|| {
            false
        })
        .expect("a completion");
        handle.join().unwrap();
        assert_eq!(
            done.text, "first half, second half",
            "every text block, in order"
        );
        assert!(done.truncated);
        assert_eq!(done.stop_reason.as_deref(), Some("max_tokens"));

        // The string API carries the note in the text itself.
        let (addr, handle) = mock(vec![Script::Respond(http(
            "200 OK",
            "application/json",
            body,
        ))]);
        let text = complete_chat(&anthropic_at(addr), "s", &[ChatMsg::user("q")], 8).unwrap();
        handle.join().unwrap();
        assert!(text.ends_with(TRUNCATED_NOTE), "{text}");

        // OpenAI dialect, blocking.
        let body = r#"{"choices":[{"message":{"content":"partial"},"finish_reason":"length"}]}"#;
        let (addr, handle) = mock(vec![Script::Respond(http(
            "200 OK",
            "application/json",
            body,
        ))]);
        let done =
            complete_chat_full(&custom(addr), "s", &[ChatMsg::user("q")], 8, &|| false).unwrap();
        handle.join().unwrap();
        assert!(done.truncated);

        // Streamed: the note is also delivered as a delta, so a reader that
        // only sees deltas (the Ask panel) sees it.
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n\
                   data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n\n\
                   data: [DONE]\n\n";
        let (addr, handle) = mock(vec![Script::Respond(http(
            "200 OK",
            "text/event-stream",
            sse,
        ))]);
        let mut seen = String::new();
        let text = complete_chat_stream(
            &custom(addr),
            "s",
            &[ChatMsg::user("q")],
            8,
            |d| seen.push_str(d),
            &|| false,
        )
        .unwrap();
        handle.join().unwrap();
        assert_eq!(seen, format!("partial{TRUNCATED_NOTE}"));
        assert_eq!(text, seen);

        // A complete answer carries no note.
        let body = r#"{"choices":[{"message":{"content":"whole"},"finish_reason":"stop"}]}"#;
        let (addr, handle) = mock(vec![Script::Respond(http(
            "200 OK",
            "application/json",
            body,
        ))]);
        assert_eq!(
            complete_chat(&custom(addr), "s", &[ChatMsg::user("q")], 8).unwrap(),
            "whole"
        );
        handle.join().unwrap();
    }

    // -- transport configuration (C12) ---------------------------------------

    /// Embeddings take the same transport policy as the chat calls — here,
    /// the proxy the environment names for the host. (Their agent used to be
    /// a separate one that ignored the proxy variables entirely.)
    #[test]
    fn the_embeddings_profile_goes_through_the_proxy_policy() {
        let proxy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy_url = format!("http://{}", proxy.local_addr().unwrap());
        let (seen_tx, seen) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            if let Ok((mut conn, _)) = proxy.accept() {
                // Bounded: a client that stops short fails the test, not
                // hangs it.
                let head = crate::testutil::read_http(&mut conn).head;
                let line = head.lines().next().unwrap_or_default().to_string();
                let _ = seen_tx.send(line);
                let _ = conn.write_all(
                    b"HTTP/1.1 502 Bad Gateway\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                );
            }
        });
        // A host that resolves nowhere: only the proxy can take this request.
        let url = "http://embeddings.invalid/v1/embeddings";
        let env = |name: &str| (name == "http_proxy").then(|| proxy_url.clone());
        let proxies = crate::net::ProxySources {
            env: &env,
            system: &|| None,
        };
        let (agent, via) =
            agent_with_proxies(url, Limits::direct(Duration::from_secs(5), PACED), &proxies)
                .unwrap();
        assert_eq!(via.map(|via| via.spec), Some(proxy_url.clone()));
        let _ = raw(&agent, url).send();
        let line = seen
            .recv_timeout(Duration::from_secs(5))
            .expect("the request must go to the proxy");
        // In absolute form, as a proxy is asked for a plain-http resource.
        assert!(line.starts_with(&format!("POST {url} ")), "{line}");
    }

    /// A model call's connection is metered beneath ureq: a response in
    /// chunked framing with an endless chunk-size line — which ureq decodes
    /// out of the body cap's sight, growing its buffer — fails at the meter
    /// instead of growing memory until the deadline.
    #[test]
    fn a_model_calls_chunked_framing_cannot_outgrow_its_meter() {
        let base =
            crate::testutil::tls_serve(vec![Box::new(|tls: &mut crate::testutil::TlsConn| {
                let _ = tls.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n");
                // 16 MiB of one chunk-size line, then silence.
                let digits = [b'f'; 64 * 1024];
                for _ in 0..256 {
                    if tls.write_all(&digits).and_then(|()| tls.flush()).is_err() {
                        return;
                    }
                }
                let _ = tls.sock.set_read_timeout(Some(Duration::from_secs(20)));
                let _ = std::io::Read::read(tls, &mut [0u8; 1]);
            })]);
        let limits = Limits {
            total: Some(Duration::from_secs(10)),
            ..BLOCKING_LIMITS
        };
        let agent = build_agent_metered(
            limits,
            crate::net::Route::Direct,
            trusting_the_mock(),
            64 * 1024,
        );
        let started = Instant::now();
        let body = raw(&agent, &format!("{base}/v1/chat/completions"))
            .send()
            .map_err(|e| LlmError::Transport(e.to_string()))
            .and_then(read_body);
        let err = body.expect_err("an endless chunk-size line was accepted");
        assert!(err.to_string().contains("byte limit"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "{:?}",
            started.elapsed()
        );
    }

    /// A model agent verifies its peer against the trust it is built with,
    /// under every profile (a model call's, and an embeddings batch's):
    /// production's (`net::tls_config`, the only trust `build_agent` passes)
    /// refuses a certificate none of its roots signed, and the same builder
    /// handed the root that did sign it talks to the peer.
    #[test]
    fn a_model_agent_verifies_against_the_trust_it_is_built_with() {
        let ok = b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok";
        for limits in [
            STREAM_LIMITS,
            Limits::direct(Duration::from_secs(10), PACED),
        ] {
            let base = crate::testutil::tls_serve(vec![
                crate::testutil::tls_answer(ok),
                crate::testutil::tls_answer(ok),
            ]);
            let url = format!("{base}/v1/chat/completions");
            let err = raw(&build_agent(limits, crate::net::Route::Direct), &url)
                .send()
                .expect_err("production trust accepted an unknown issuer");
            assert!(
                err.to_string().contains("UnknownIssuer"),
                "{limits:?}: {err}"
            );
            let trusting =
                build_agent_trusting(limits, crate::net::Route::Direct, trusting_the_mock());
            let body = raw(&trusting, &url)
                .send()
                .map_err(|e| e.to_string())
                .and_then(|r| read_body(r).map_err(|e| e.to_string()));
            assert_eq!(body.as_deref(), Ok("ok"), "{limits:?}");
        }
    }

    /// The trust a production agent is built with is `net::tls_config`'s:
    /// the bundled roots AND the operating system's store, where a corporate
    /// or private CA lives. The test above cannot tell that from any other
    /// real root store — ureq's bundled roots alone, which drop the OS store,
    /// refuse the test peer just the same. So the test certificate goes into
    /// the OS store as `tls_config` reads it: `SSL_CERT_FILE`, which
    /// `rustls-native-certs` loads in place of the platform's store. In a
    /// child process, because `tls_config` is built once per process and the
    /// variable would reach every test running beside this one. There, on
    /// both kinds of connection, a production agent must reach the peer; one
    /// with a root store of its own does not.
    #[test]
    fn a_model_agent_trusts_what_the_system_store_holds() {
        let dir = crate::testutil::TempDir::new("llm-os-trust");
        let roots = dir.join("roots.pem");
        std::fs::write(&roots, crate::testutil::TEST_CERT).unwrap();
        run_in_child(
            "llm::tests::child_a_model_agent_trusts_what_the_system_store_holds",
            &[
                ("SSL_CERT_FILE", Some(roots.as_os_str())),
                ("SSL_CERT_DIR", None),
            ],
        );
    }

    #[test]
    #[ignore = "runs in a child process, from a_model_agent_trusts_what_the_system_store_holds"]
    fn child_a_model_agent_trusts_what_the_system_store_holds() {
        if !in_child() {
            return;
        }
        let ok = b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok";
        for limits in [
            STREAM_LIMITS,
            Limits::direct(Duration::from_secs(10), PACED),
        ] {
            let base = crate::testutil::tls_serve(vec![crate::testutil::tls_answer(ok)]);
            let agent = build_agent(limits, crate::net::Route::Direct);
            let body = raw(&agent, &format!("{base}/v1/chat/completions"))
                .send()
                .map_err(|e| e.to_string())
                .and_then(|r| read_body(r).map_err(|e| e.to_string()));
            assert_eq!(
                body.as_deref(),
                Ok("ok"),
                "{limits:?}: the OS store's root was not trusted"
            );
        }
    }

    // -- configuration -------------------------------------------------------

    use crate::testutil::{in_child, run_in_child};

    /// A proxy variable that is not UTF-8 is still a proxy setting, for every
    /// kind of request clew-core makes. Read as unset — `env::var(..).ok()`,
    /// which model calls and embeddings used — the request connected directly
    /// around the proxy the user configured, API key included. Driven through
    /// each production call site, in a child that has the variable from birth
    /// (set here, it would race every request the tests beside it make).
    #[test]
    fn a_proxy_variable_that_is_not_utf8_is_not_ignored() {
        use std::os::unix::ffi::OsStrExt;
        let odd = std::ffi::OsStr::from_bytes(b"http://pr\xffoxy:3128");
        let mut env = vec![("https_proxy", Some(odd))];
        for unset in [
            "HTTPS_PROXY",
            "http_proxy",
            "HTTP_PROXY",
            "all_proxy",
            "ALL_PROXY",
            "no_proxy",
            "NO_PROXY",
        ] {
            env.push((unset, None));
        }
        run_in_child(
            "llm::tests::child_a_proxy_variable_that_is_not_utf8_is_not_ignored",
            &env,
        );
    }

    #[test]
    #[ignore = "runs in a child process, from a_proxy_variable_that_is_not_utf8_is_not_ignored"]
    fn child_a_proxy_variable_that_is_not_utf8_is_not_ignored() {
        if !in_child() {
            return;
        }
        let refused = |what: &str, err: String| {
            assert!(
                err.contains("unusable proxy setting"),
                "{what} went around the proxy: {err}"
            );
        };
        // Model calls.
        let err = agent_for("https://api.example.invalid/v1", BLOCKING_LIMITS)
            .expect_err("a model call's agent was built without the proxy");
        refused("a model call", err.to_string());
        // Embeddings.
        let cfg = crate::embed::Config::from_parts(
            "k".into(),
            "m".into(),
            "https://embeddings.invalid/v1".into(),
        );
        let err = crate::embed::embed_batch(&cfg, &["x".to_string()]).unwrap_err();
        refused("an embeddings batch", err);
        // Downloads.
        let limits = crate::net::Limits {
            max_bytes: 1024,
            deadline: Duration::from_secs(20),
        };
        let err = crate::net::get("https://releases.invalid/x", &[], limits).unwrap_err();
        refused("a download", err);
    }

    #[test]
    fn config_resolves_provider_key_model_and_base_url() {
        let dir = crate::testutil::TempDir::new("llm-config");
        run_in_child(
            "llm::tests::child_config_resolves_provider_key_model_and_base_url",
            &[
                ("CLEW_DATA_DIR", Some(dir.path().as_os_str())),
                ("ANTHROPIC_API_KEY", None),
                ("OPENAI_API_KEY", None),
                ("DEEPSEEK_API_KEY", None),
            ],
        );
    }

    #[test]
    #[ignore = "runs in a child process, from config_resolves_provider_key_model_and_base_url"]
    fn child_config_resolves_provider_key_model_and_base_url() {
        if !in_child() {
            return;
        }
        let dir = std::path::PathBuf::from(std::env::var_os("CLEW_DATA_DIR").unwrap());

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
    }

    /// A provider's env var is a secret for that provider's host. Pointed at a
    /// gateway, proxy or local server the user deliberately left keyless, clew
    /// must read as unconfigured rather than send the real key there as
    /// `x-api-key` / `Bearer`.
    #[test]
    fn env_key_is_inherited_only_by_the_providers_own_endpoint() {
        let dir = crate::testutil::TempDir::new("llm-env-key");
        let key = std::ffi::OsStr::new;
        run_in_child(
            "llm::tests::child_env_key_is_inherited_only_by_the_providers_own_endpoint",
            &[
                ("CLEW_DATA_DIR", Some(dir.path().as_os_str())),
                ("ANTHROPIC_API_KEY", Some(key("sk-ant-env"))),
                ("OPENAI_API_KEY", Some(key("sk-oai-env"))),
                ("DEEPSEEK_API_KEY", Some(key("sk-ds-env"))),
            ],
        );
    }

    #[test]
    #[ignore = "runs in a child process, from env_key_is_inherited_only_by_the_providers_own_endpoint"]
    fn child_env_key_is_inherited_only_by_the_providers_own_endpoint() {
        if !in_child() {
            return;
        }
        let dir = std::path::PathBuf::from(std::env::var_os("CLEW_DATA_DIR").unwrap());
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
    }
}
