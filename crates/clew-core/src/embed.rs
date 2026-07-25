//! Embeddings + semantic search over the codebase.
//!
//! We embed each function/file **explanation summary** (concise, semantic, and
//! already cached) with an OpenAI-compatible `/embeddings` endpoint, and keep a
//! small vector index under `.clew/cache/embeddings.json`. A natural-language
//! query is embedded the same way and ranked by cosine similarity, so you can
//! find code by what it *does* rather than by its text. This is also the
//! retrieval layer a future "Ask clew" will build on.
//!
//! DeepSeek has no embeddings API, so the embedding endpoint is configured
//! separately (defaulting to OpenAI, key falling back to `OPENAI_API_KEY`).

use std::io::Read;
use std::path::{Path, PathBuf};

use crate::explain::Node;
use crate::incremental::{Version, content_hash};

/// Reduced dimensionality — text-embedding-3-* supports `dimensions`; 512 keeps
/// quality high while cutting the index to a third of the full 1536.
const DIMS: usize = 512;
const DEFAULT_MODEL: &str = "text-embedding-3-small";
const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

/// Embedding endpoint configuration (separate from the chat provider).
#[derive(Debug, Clone)]
pub struct Config {
    pub api_key: String,
    pub model: String,
    pub base_url: String,
}

fn config_path() -> Option<PathBuf> {
    Some(crate::lsp::store::data_root()?.join("config.toml"))
}

impl Config {
    /// Load from the `[embedding]` section, falling back to `OPENAI_API_KEY` and
    /// the OpenAI defaults. `None` when no key is available.
    pub fn load() -> Option<Config> {
        let table: Option<toml::Value> = config_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| toml::from_str(&t).ok());
        let emb = table.as_ref().and_then(|t| t.get("embedding"));
        let field = |k: &str| {
            emb.and_then(|e| e.get(k))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        };

        let api_key = field("api_key")
            .filter(|k| !k.is_empty())
            .or_else(|| std::env::var("OPENAI_API_KEY").ok())
            .filter(|k| !k.is_empty())?;
        let model = field("model")
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| DEFAULT_MODEL.into());
        let base_url = field("base_url")
            .filter(|b| !b.is_empty())
            .unwrap_or_else(|| DEFAULT_BASE_URL.into())
            .trim_end_matches('/')
            .to_string();
        Some(Config {
            api_key,
            model,
            base_url,
        })
    }

    pub fn available() -> bool {
        Config::load().is_some()
    }

    /// Build a config from settings fields, filling blank model/base_url.
    pub fn from_parts(api_key: String, model: String, base_url: String) -> Config {
        let model = if model.trim().is_empty() {
            DEFAULT_MODEL.to_string()
        } else {
            model.trim().to_string()
        };
        let base_url = if base_url.trim().is_empty() {
            DEFAULT_BASE_URL.to_string()
        } else {
            base_url.trim().trim_end_matches('/').to_string()
        };
        Config {
            api_key: api_key.trim().to_string(),
            model,
            base_url,
        }
    }

    /// The stored embedding settings (defaults filled) — for the settings form.
    pub fn current_or_default() -> Config {
        Config::load().unwrap_or_else(|| Config {
            api_key: String::new(),
            model: DEFAULT_MODEL.to_string(),
            base_url: DEFAULT_BASE_URL.to_string(),
        })
    }

    /// Persist the `[embedding]` section, preserving other config sections.
    pub fn save(&self) -> Result<(), String> {
        let path = config_path().ok_or("no data directory")?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let mut root: toml::Table = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default();
        let mut emb = toml::Table::new();
        emb.insert("api_key".into(), self.api_key.clone().into());
        emb.insert("model".into(), self.model.clone().into());
        emb.insert("base_url".into(), self.base_url.clone().into());
        root.insert("embedding".into(), toml::Value::Table(emb));
        let s = toml::to_string(&root).map_err(|e| e.to_string())?;
        std::fs::write(&path, s).map_err(|e| e.to_string())
    }
}

/// Embed a batch of texts in one request (keep batches modest to stay under the
/// endpoint's token cap). Blocking — run off the UI thread.
pub fn embed_batch(cfg: &Config, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
    if texts.is_empty() {
        return Ok(Vec::new());
    }
    let url = format!("{}/embeddings", cfg.base_url.trim_end_matches('/'));
    let body = serde_json::json!({
        "model": cfg.model,
        "input": texts,
        "dimensions": DIMS,
    })
    .to_string();
    let resp = ureq::post(&url)
        .set("Authorization", &format!("Bearer {}", cfg.api_key))
        .set("content-type", "application/json")
        .send_string(&body);
    let text = match resp {
        Ok(r) => {
            let mut s = String::new();
            r.into_reader()
                .read_to_string(&mut s)
                .map_err(|e| format!("read: {e}"))?;
            s
        }
        Err(ureq::Error::Status(code, r)) => {
            let raw = r.into_string().unwrap_or_default();
            // Prefer the API's `error.message`; fall back to the first line.
            let msg = serde_json::from_str::<serde_json::Value>(&raw)
                .ok()
                .and_then(|j| {
                    j.pointer("/error/message")
                        .and_then(|m| m.as_str())
                        .map(str::to_string)
                })
                .unwrap_or_else(|| raw.lines().next().unwrap_or("").to_string());
            let msg: String = msg.chars().take(200).collect();
            return Err(format!("embeddings API error {code}: {msg}"));
        }
        Err(e) => return Err(format!("request failed: {e}")),
    };
    let json: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("bad JSON: {e}"))?;
    let data = json
        .get("data")
        .and_then(|d| d.as_array())
        .ok_or("no data in response")?;
    // `data` is index-ordered per the spec, but sort defensively.
    let mut rows: Vec<(usize, Vec<f32>)> = data
        .iter()
        .filter_map(|d| {
            let idx = d.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
            let v = d
                .get("embedding")?
                .as_array()?
                .iter()
                .filter_map(|x| x.as_f64().map(|f| f as f32))
                .collect::<Vec<f32>>();
            Some((idx, v))
        })
        .collect();
    rows.sort_by_key(|(i, _)| *i);
    if rows.len() != texts.len() {
        return Err(format!(
            "expected {} embeddings, got {}",
            texts.len(),
            rows.len()
        ));
    }
    Ok(rows.into_iter().map(|(_, v)| v).collect())
}

/// Embed many texts, chunked to stay under the endpoint's per-request cap.
/// Blocking — run off the UI thread.
pub fn embed_all(cfg: &Config, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
    const CHUNK: usize = 96;
    let mut out = Vec::with_capacity(texts.len());
    for chunk in texts.chunks(CHUNK) {
        out.extend(embed_batch(cfg, chunk)?);
    }
    Ok(out)
}

/// One indexed unit: the node, the hash of the text embedded (to detect a stale
/// summary), and its vector.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Entry {
    pub node: Node,
    pub hash: Version,
    pub vec: Vec<f32>,
}

/// The on-disk vector index.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Index {
    pub model: String,
    pub entries: Vec<Entry>,
}

fn index_path(root: &Path) -> PathBuf {
    root.join(".clew").join("cache").join("embeddings.json")
}

pub fn load(root: &Path) -> Index {
    std::fs::read_to_string(index_path(root))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save(root: &Path, index: &Index) -> std::io::Result<()> {
    let path = index_path(root);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_string(index).map_err(|e| std::io::Error::other(e.to_string()))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, &path)
}

/// Hash of the text a node embeds (so an unchanged summary is never re-embedded).
pub fn text_hash(text: &str) -> Version {
    content_hash(text.as_bytes())
}

/// Cosine similarity of two equal-length vectors.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    let denom = na.sqrt() * nb.sqrt();
    if denom == 0.0 { 0.0 } else { dot / denom }
}

/// Rank the index by cosine similarity to `query`, returning the top `k` nodes
/// with their scores (descending), above a small relevance floor.
pub fn search<'a>(index: &'a Index, query: &[f32], k: usize) -> Vec<(&'a Node, f32)> {
    let mut scored: Vec<(&Node, f32)> = index
        .entries
        .iter()
        .map(|e| (&e.node, cosine(query, &e.vec)))
        .filter(|(_, s)| *s > 0.15)
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1));
    scored.truncate(k);
    scored
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn cosine_and_search_rank_by_similarity() {
        let f = |file: &str, name: &str| Node::Function {
            file: PathBuf::from(file),
            name: name.into(),
        };
        let index = Index {
            model: "m".into(),
            entries: vec![
                Entry {
                    node: f("a.rs", "near"),
                    hash: 0,
                    vec: vec![1.0, 0.0, 0.0],
                },
                Entry {
                    node: f("a.rs", "far"),
                    hash: 0,
                    vec: vec![0.0, 1.0, 0.0],
                },
                Entry {
                    node: f("a.rs", "mid"),
                    hash: 0,
                    vec: vec![0.7, 0.7, 0.0],
                },
            ],
        };
        let q = [1.0, 0.0, 0.0];
        let hits = search(&index, &q, 2);
        assert_eq!(hits.len(), 2);
        assert!(matches!(hits[0].0, Node::Function { name, .. } if name == "near"));
        assert!(hits[0].1 > hits[1].1, "ranked by similarity");
        // The orthogonal vector is below the floor and excluded.
        assert!(
            !hits
                .iter()
                .any(|(n, _)| matches!(n, Node::Function { name, .. } if name == "far"))
        );
    }
}
