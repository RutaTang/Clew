//! Anthropic Messages API client + global LLM config for the explain feature.
//!
//! The API key lives in clew's **global** config (`<data_root>/config.toml`), not
//! in a project's `.clew/` — it's a cross-project credential, like the shared LSP
//! binaries. When no key is configured, the explain feature stays hidden and the
//! rest of clew is unaffected (and fully offline).
//!
//! ```toml
//! [llm]
//! anthropic_api_key = "sk-ant-..."
//! model = "claude-haiku-4-5-20251001"   # optional; default is fast + cheap
//! ```
//!
//! `ANTHROPIC_API_KEY` in the environment overrides the config file's key.

// Consumed by the explain orchestrator + UI (next); allow is removed there.
#![allow(dead_code)]

use std::io::Read;
use std::path::PathBuf;

/// Fast, inexpensive default — most explanation nodes are small leaf summaries.
const DEFAULT_MODEL: &str = "claude-haiku-4-5-20251001";
const API_URL: &str = "https://api.anthropic.com/v1/messages";
const API_VERSION: &str = "2023-06-01";

/// Resolved LLM configuration.
#[derive(Debug, Clone)]
pub struct Config {
    pub api_key: String,
    pub model: String,
}

fn config_path() -> Option<PathBuf> {
    Some(crate::lsp::store::data_root()?.join("config.toml"))
}

impl Config {
    /// Load the config from the global config file (key overridable by the
    /// `ANTHROPIC_API_KEY` env var). Returns `None` when no key is available, in
    /// which case the explain feature is unavailable.
    pub fn load() -> Option<Config> {
        let table: Option<toml::Value> = config_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| toml::from_str(&t).ok());
        let llm = table.as_ref().and_then(|t| t.get("llm"));

        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .ok()
            .filter(|k| !k.is_empty())
            .or_else(|| {
                llm.and_then(|l| l.get("anthropic_api_key"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            })
            .filter(|k| !k.is_empty())?;

        let model = llm
            .and_then(|l| l.get("model"))
            .and_then(|v| v.as_str())
            .filter(|m| !m.is_empty())
            .unwrap_or(DEFAULT_MODEL)
            .to_string();

        Some(Config { api_key, model })
    }

    /// Whether a key is configured (drives whether the explain UI is shown).
    pub fn available() -> bool {
        Config::load().is_some()
    }
}

/// The path where a user should put their key, for a "not configured" hint.
pub fn config_hint() -> String {
    config_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "<clew data dir>/config.toml".into())
}

/// One synchronous completion (blocking — call off the UI thread). Returns the
/// assistant's text, or an error string.
pub fn complete(cfg: &Config, system: &str, prompt: &str, max_tokens: u32) -> Result<String, String> {
    let body = serde_json::json!({
        "model": cfg.model,
        "max_tokens": max_tokens,
        "system": system,
        "messages": [{ "role": "user", "content": prompt }],
    })
    .to_string();

    let resp = ureq::post(API_URL)
        .set("x-api-key", &cfg.api_key)
        .set("anthropic-version", API_VERSION)
        .set("content-type", "application/json")
        .send_string(&body);

    let text = match resp {
        Ok(r) => {
            let mut s = String::new();
            r.into_reader()
                .read_to_string(&mut s)
                .map_err(|e| format!("read response: {e}"))?;
            s
        }
        // A non-2xx status carries the error body, which is the useful message.
        Err(ureq::Error::Status(code, r)) => {
            let msg = r.into_string().unwrap_or_default();
            return Err(format!("Anthropic API error {code}: {}", first_line(&msg)));
        }
        Err(e) => return Err(format!("request failed: {e}")),
    };

    let json: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("bad JSON response: {e}"))?;
    json.get("content")
        .and_then(|c| c.as_array())
        .and_then(|blocks| blocks.iter().find_map(|b| b.get("text").and_then(|t| t.as_str())))
        .map(|s| s.trim().to_string())
        .ok_or_else(|| "no text in Anthropic response".to_string())
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").chars().take(200).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_parses_key_and_model_with_default() {
        // Point the data dir at a temp location and write a config.
        let dir = std::env::temp_dir().join("clew-llm-config-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.toml"),
            "[llm]\nanthropic_api_key = \"sk-test\"\n",
        )
        .unwrap();
        // SAFETY: single-threaded test; sets env for this process only.
        unsafe {
            std::env::set_var("CLEW_DATA_DIR", &dir);
            std::env::remove_var("ANTHROPIC_API_KEY");
        }

        let cfg = Config::load().expect("config with a key loads");
        assert_eq!(cfg.api_key, "sk-test");
        assert_eq!(cfg.model, DEFAULT_MODEL, "model defaults when unset");

        // No key → unavailable.
        std::fs::write(dir.join("config.toml"), "[llm]\nmodel = \"x\"\n").unwrap();
        assert!(Config::load().is_none(), "no key means unavailable");

        unsafe {
            std::env::remove_var("CLEW_DATA_DIR");
        }
    }
}
