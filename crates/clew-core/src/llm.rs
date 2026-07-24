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
    pub const ALL: [Provider; 4] =
        [Provider::Anthropic, Provider::OpenAI, Provider::DeepSeek, Provider::Custom];

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
    pub fn from_parts(provider: Provider, api_key: String, model: String, base_url: String) -> Config {
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
        Config { provider, api_key: api_key.trim().to_string(), model, base_url }
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
            llm.and_then(|l| l.get(k)).and_then(|v| v.as_str()).map(str::to_string)
        };

        let provider = str_field("provider").map(|s| Provider::from_slug(&s)).unwrap_or(Provider::Anthropic);
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
        ChatMsg { role: Role::User, content: content.into() }
    }
    pub fn assistant(content: impl Into<String>) -> ChatMsg {
        ChatMsg { role: Role::Assistant, content: content.into() }
    }
    pub fn role_str(&self) -> &'static str {
        match self.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }
}

/// One synchronous single-turn completion (blocking — call off the UI thread).
/// Returns the assistant's text, or an error string.
pub fn complete(cfg: &Config, system: &str, prompt: &str, max_tokens: u32) -> Result<String, String> {
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
    match req.send_string(body) {
        Ok(r) => Ok(r.into_reader()),
        Err(ureq::Error::Status(code, r)) => {
            let msg = r.into_string().unwrap_or_default();
            Err(format!("{who} API error {code}: {}", first_line(&msg)))
        }
        Err(e) => Err(format!("request failed: {e}")),
    }
}

/// Read `data: …` SSE lines, extracting each token via `pick` (which returns the
/// delta text for a parsed event, or `None` to skip). Stops at `[DONE]`.
fn read_sse(
    reader: Box<dyn std::io::Read + Send + Sync + 'static>,
    mut on_delta: impl FnMut(&str),
    pick: impl Fn(&serde_json::Value) -> Option<String>,
) -> Result<String, String> {
    use std::io::BufRead;
    let mut full = String::new();
    for line in std::io::BufReader::new(reader).lines() {
        let line = line.map_err(|e| format!("stream read: {e}"))?;
        let Some(data) = line.strip_prefix("data:").map(str::trim) else {
            continue;
        };
        if data == "[DONE]" {
            break;
        }
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(data)
            && let Some(delta) = pick(&json)
        {
            on_delta(&delta);
            full.push_str(&delta);
        }
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
    body[if newer { "max_completion_tokens" } else { "max_tokens" }] = max_tokens.into();
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
            .then(|| json.pointer("/delta/text").and_then(|v| v.as_str()).map(str::to_string))
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

fn anthropic(cfg: &Config, system: &str, messages: &[ChatMsg], max_tokens: u32) -> Result<String, String> {
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
        .and_then(|blocks| blocks.iter().find_map(|b| b.get("text").and_then(|t| t.as_str())))
        .map(|s| s.trim().to_string())
        .ok_or_else(|| "no text in Anthropic response".to_string())
}

fn openai_compatible(cfg: &Config, system: &str, messages: &[ChatMsg], max_tokens: u32) -> Result<String, String> {
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
    let field = if newer { "max_completion_tokens" } else { "max_tokens" };
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
    match req.send_string(body) {
        Ok(r) => {
            let mut s = String::new();
            r.into_reader().read_to_string(&mut s).map_err(|e| format!("read response: {e}"))?;
            Ok(s)
        }
        Err(ureq::Error::Status(code, r)) => {
            let msg = r.into_string().unwrap_or_default();
            Err(format!("{who} API error {code}: {}", first_line(&msg)))
        }
        Err(e) => Err(format!("request failed: {e}")),
    }
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").chars().take(200).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_resolves_provider_key_model_and_base_url() {
        let dir = std::env::temp_dir().join("clew-llm-config-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // SAFETY: single-threaded test; sets env for this process only.
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
        std::fs::write(dir.join("config.toml"), "[llm]\nanthropic_api_key = \"sk-old\"\n").unwrap();
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
