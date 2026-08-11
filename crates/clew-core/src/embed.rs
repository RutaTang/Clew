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
//! separately (defaulting to OpenAI, key falling back to `OPENAI_API_KEY` only
//! while the endpoint is still OpenAI's).

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

/// The model and endpoint named by the stored `[embedding]` section, defaults
/// filled and the endpoint trimmed exactly as [`Config::load`] does.
///
/// Split out of `load` because the embedding SPACE is exactly these two values
/// and nothing else. Reading it must not depend on a key being present, or
/// rotating one — or clearing it, which makes `load` return `None` — would read
/// as a move to another space and throw away a usable index (see [`Space`]).
fn model_and_base_url(emb: Option<&toml::Table>) -> (String, String) {
    let field = |k: &str| {
        emb.and_then(|e| e.get(k))
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    let base_url = field("base_url")
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| DEFAULT_BASE_URL.into())
        .trim_end_matches('/')
        .to_string();
    let model = field("model")
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| DEFAULT_MODEL.into());
    (model, base_url)
}

/// The identity of a vector space: the model AND the endpoint serving it.
///
/// Cosine only means anything *within* one space, and the model name alone does
/// not name one — the same name served by another provider is a different space
/// (the reason the on-disk [`Stored`] form records both). Vectors from two
/// spaces still produce confident-looking similarities, so every place that
/// keeps vectors across a possible config change has to be able to say which
/// space they came from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Space {
    pub model: String,
    pub base_url: String,
}

impl Space {
    /// True when `index` demonstrably belongs to some OTHER space, judged from
    /// what an in-memory index carries.
    ///
    /// Both halves are compared, because either alone is insufficient: the
    /// same model name served by another provider is a different space, and
    /// the settings handler that drops the index across a save only covers the
    /// window that performed it — every other window keeps its own `App` and
    /// its own copy, and re-reads the config file on the next call. So this is
    /// the check that has to catch a change made by another window or by hand-
    /// editing `config.toml`, whichever field moved. An empty index is never
    /// foreign: there is nothing to mis-rank.
    ///
    /// An index loaded before this field existed deserializes with an empty
    /// `base_url`, which no real config matches, so it reads as foreign and is
    /// rebuilt — the safe direction.
    pub fn is_foreign(&self, index: &Index) -> bool {
        !index.entries.is_empty() && (index.model != self.model || index.base_url != self.base_url)
    }
}

/// The embedding space named by the stored config right now, key or no key.
pub fn stored_space() -> Space {
    let (model, base_url) = model_and_base_url(crate::globalconfig::section("embedding").as_ref());
    Space { model, base_url }
}

impl Config {
    /// The space this config's vectors live in.
    pub fn space(&self) -> Space {
        Space {
            model: self.model.clone(),
            base_url: self.base_url.clone(),
        }
    }

    /// Load from the `[embedding]` section, falling back to `OPENAI_API_KEY` and
    /// the OpenAI defaults. `None` when no key is available.
    pub fn load() -> Option<Config> {
        let emb = crate::globalconfig::section("embedding");
        let field = |k: &str| {
            emb.as_ref()
                .and_then(|e| e.get(k))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        };

        // Resolved before the key, because whether the environment may supply
        // the key depends on where the request would go.
        let (model, base_url) = model_and_base_url(emb.as_ref());
        let api_key = field("api_key")
            .filter(|k| !k.is_empty())
            .or_else(|| {
                // Only OpenAI's own endpoint inherits `OPENAI_API_KEY`. With a
                // third-party or self-hosted base_url and no key of its own,
                // the fallback would hand the user's OpenAI secret to that
                // host as a Bearer token. Shared with the chat path rather
                // than restated: the rule was fixed here first and the chat
                // side stayed open, which is what a second copy of a predicate
                // buys.
                crate::llm::env_key_for_endpoint("OPENAI_API_KEY", &base_url, DEFAULT_BASE_URL)
            })
            .filter(|k| !k.is_empty())?;
        Some(Config {
            api_key,
            model,
            base_url,
        })
    }

    pub fn available() -> bool {
        Config::load().is_some()
    }

    /// The key as it is written in `config.toml`, with NO `OPENAI_API_KEY`
    /// fallback — see [`crate::llm::Config::stored_key`] for why a settings
    /// form must pre-fill from this one.
    pub fn stored_key() -> String {
        crate::globalconfig::section("embedding")
            .as_ref()
            .and_then(|e| e.get("api_key"))
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_default()
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

    /// Persist the `[embedding]` section, preserving other config sections
    /// (see [`crate::globalconfig::update`]).
    pub fn save(&self) -> Result<(), String> {
        let mut emb = toml::Table::new();
        emb.insert("api_key".into(), self.api_key.clone().into());
        emb.insert("model".into(), self.model.clone().into());
        emb.insert("base_url".into(), self.base_url.clone().into());
        crate::globalconfig::update("embedding", emb)
    }

    /// Persist this config as an edit OF `previous`, keeping any field another
    /// writer changed since that snapshot was taken. Returns the kept fields.
    /// See [`crate::llm::Config::save_from`] for why the settings form must
    /// use this and not [`Config::save`].
    pub fn save_from(&self, previous: &Config) -> Result<Vec<String>, String> {
        crate::globalconfig::update_fields(
            "embedding",
            &[
                ("api_key", previous.api_key.clone(), self.api_key.clone()),
                ("model", previous.model.clone(), self.model.clone()),
                ("base_url", previous.base_url.clone(), self.base_url.clone()),
            ],
        )
    }
}

/// How long an embeddings socket may go silent before the request fails.
///
/// Without it the call could park a thread forever: `ureq`'s default agent sets
/// no read timeout, so an endpoint that accepts the connection and then says
/// nothing — a wedged local model server is the common case — held the caller
/// indefinitely. Every caller is blocking, so that thread was gone for the life
/// of the process: the GUI's FIND and RAG paths (`handlers_features`) and the
/// server's `semantic_find`, which polls its stop flag against a 60s deadline
/// and then abandons the thread this bounds.
///
/// Generous on purpose: a cold local model can legitimately take tens of
/// seconds to answer the first embedding of a batch.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// The agent every embeddings request goes through.
///
/// Two properties, both of which the implicit `ureq::post` agent lacks:
/// [`REQUEST_TIMEOUT`], and `redirects(0)`. The latter matters even though this
/// endpoint authenticates with `Authorization` (which ureq does strip on a
/// redirect): following would silently send the batch to a host the user never
/// configured and parse whatever came back as embeddings. Refusing keeps the
/// request where it was addressed — the same rule [`crate::llm`] applies, where
/// `x-api-key` is not stripped and the stakes are the key itself.
fn agent() -> ureq::Agent {
    static AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    AGENT.get_or_init(|| build_agent(REQUEST_TIMEOUT)).clone()
}

/// Split out from [`agent`] so a test can watch a short timeout actually fire;
/// the real one is too long to wait on.
fn build_agent(timeout: std::time::Duration) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .redirects(0)
        .timeout_read(timeout)
        .timeout_write(timeout)
        .build()
}

/// Embed a batch of texts in one request (keep batches modest to stay under the
/// endpoint's token cap). Blocking — run off the UI thread, and note it is
/// bounded by [`REQUEST_TIMEOUT`] but NOT cancellable: nothing about dropping
/// the caller reaches the socket.
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
    let resp = agent()
        .post(&url)
        .set("Authorization", &format!("Bearer {}", cfg.api_key))
        .set("content-type", "application/json")
        .send_string(&body);
    let text = match resp {
        // `redirects(0)` turns a 3xx into an ordinary response whose body is a
        // redirect page. Say so, rather than failing later with "bad JSON".
        Ok(r) if (300..400).contains(&r.status()) => {
            let to = r.header("location").unwrap_or("elsewhere").to_string();
            return Err(format!(
                "embeddings endpoint redirected to {}; not following — point base_url at the real endpoint",
                to.lines()
                    .next()
                    .unwrap_or("")
                    .chars()
                    .take(200)
                    .collect::<String>()
            ));
        }
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

/// The vector index in memory. On disk it is wrapped by [`Stored`], which also
/// records the endpoint, so [`load`] can tell a foreign index from this one's.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Index {
    pub model: String,
    /// The endpoint these vectors were built against. Carried here and not
    /// only in [`Stored`] because a model name does not name a space: the same
    /// name served by another provider is a different one, and without this
    /// field [`Space::is_foreign`] could not see a repoint that left the model
    /// alone. That blind spot let one window's stale vectors be reused after
    /// another window changed only the endpoint, and then written back stamped
    /// with the NEW space — poisoning the shared file for good.
    pub base_url: String,
    pub entries: Vec<Entry>,
}

/// The on-disk shape: the entries plus the identity of the embedding space
/// they live in. Cosine only means anything *within* one space, and the name of
/// the model alone does not name a space — the same model name served by
/// another provider is a different one — so the file records the endpoint too.
#[derive(serde::Deserialize)]
struct Stored {
    #[serde(default)]
    model: String,
    /// `#[serde(default)]` keeps indexes written before this field readable.
    /// They read back as `""`, which matches no real endpoint, so they are
    /// treated as foreign and rebuilt rather than trusted.
    #[serde(default)]
    base_url: String,
    #[serde(default)]
    entries: Vec<Entry>,
}

/// Borrowed twin of [`Stored`] for writing, so saving does not clone every
/// vector in the index.
#[derive(serde::Serialize)]
struct StoredRef<'a> {
    model: &'a str,
    base_url: &'a str,
    entries: &'a [Entry],
}

fn index_path(store: &Path) -> PathBuf {
    store.join("embeddings.json")
}

/// Load the index for `store`, judged against this machine's current embedding
/// config (see [`load_for`]).
pub fn load(store: &Path, root: &Path) -> Index {
    load_for(store, root, Config::load().as_ref())
}

/// Load the index, keeping it only when `cfg` names the exact model AND
/// endpoint its vectors were built with.
///
/// Nothing else invalidates the file: the builder's reuse gate keys on the
/// summary hash, which does not move when the embedding config does, so after
/// a model or provider change every old vector would be reused and the freshly
/// embedded query would be ranked against a different vector space. Cosine
/// still returns confident-looking numbers there, so the failure is silent —
/// discarding the index here is what forces the rebuild.
///
/// This judges the file on disk, and only when it is READ. A copy already in
/// memory is not covered: the GUI loads once at project open and keeps it for
/// the session, so the same rule has to be applied again when the config
/// changes under it (see [`Space`] and [`stored_space`]).
pub fn load_for(store: &Path, root: &Path, cfg: Option<&Config>) -> Index {
    let Some(stored) = crate::statefile::read(&index_path(store))
        .and_then(|s| serde_json::from_str::<Stored>(&s).ok())
    else {
        return Index::default();
    };
    if !cfg.is_some_and(|c| c.model == stored.model && c.base_url == stored.base_url) {
        return Index::default();
    }
    let mut index = Index {
        model: stored.model,
        base_url: stored.base_url,
        entries: stored.entries,
    };
    // Nodes store absolute paths: an entry left over from a moved or
    // renamed project must not become a clickable search result that
    // opens a file outside this one.
    index
        .entries
        .retain(|e| crate::statefile::safe_abs_under(root, e.node.path()));
    index
}

/// Persist the index, stamping the embedding space it belongs to.
///
/// The endpoint comes from the live config rather than from `index`, which only
/// carries the model: the process that just built these vectors is the one
/// holding the config they came from. With no config left to read we stamp
/// nothing, so the file reads back as foreign — better than claiming a space we
/// cannot name.
/// That makes ONE demand of the caller, and it is the whole reason a stale
/// index can become permanent: an index built under a config the live one has
/// since moved away from must never be handed here, or the file would claim the
/// new space for old-space vectors and every later [`load_for`] would trust it.
/// The only builder is the GUI, which discards a build whose embedding space
/// changed while it ran (`on_build_embeddings`).
/// Correct only when the caller's index IS the whole truth. A window builds
/// its index from the nodes ITS explanation cache holds, so a save from a
/// second window covering fewer nodes must go through [`merge_built`].
pub fn save(store: &Path, index: &Index) -> std::io::Result<()> {
    let base_url = Config::load().map(|c| c.base_url).unwrap_or_default();
    let stored = StoredRef {
        model: &index.model,
        base_url: &base_url,
        entries: &index.entries,
    };
    let json = serde_json::to_string(&stored).map_err(|e| std::io::Error::other(e.to_string()))?;
    crate::statefile::write_atomic(&index_path(store), json.as_bytes())
}

/// Serializes the read-modify-write below across this process's windows.
static SAVE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Fold a freshly built index into the one ON DISK RIGHT NOW, returning the
/// merged index the caller must adopt.
///
/// A build covers the nodes the BUILDING window's explanation cache holds, and
/// the derived store is shared by every window and every clew process on the
/// project. Writing the build wholesale therefore replaced an index covering
/// the whole project with one covering the subset one window knew about, and
/// FIND then answered from that subset with no sign anything was missing.
///
/// Fresh entries win per node (they were embedded from the current summary);
/// entries only the stored index has are kept. `load` discards a stored index
/// built in a different embedding space, so nothing foreign is merged in —
/// a merged file always describes one space.
pub fn merge_built(store: &Path, root: &Path, built: &Index) -> (Index, std::io::Result<()>) {
    // Poisoning only means an earlier caller panicked; the index is re-read
    // from disk here regardless, so there is no corrupt state to inherit.
    let _serialized = SAVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _exclusive = crate::statefile::lock_exclusive(&index_path(store));
    let mut merged = load(store, root);
    // The build names the space the merged file is stamped with; `load` has
    // already discarded a stored index belonging to any other one.
    merged.model = built.model.clone();
    merged.base_url = built.base_url.clone();
    for fresh in &built.entries {
        match merged.entries.iter().position(|e| e.node == fresh.node) {
            Some(i) => merged.entries[i] = fresh.clone(),
            None => merged.entries.push(fresh.clone()),
        }
    }
    let saved = save(store, &merged);
    (merged, saved)
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
            ordinal: 0,
        };
        let index = Index {
            model: "m".into(),
            base_url: DEFAULT_BASE_URL.into(),
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

    /// `OPENAI_API_KEY` is OpenAI's key: a base_url pointing anywhere else must
    /// not inherit it, or a blank key field silently Bearer-tokens the user's
    /// OpenAI secret to a third-party host.
    #[test]
    fn env_key_is_inherited_only_by_the_openai_endpoint() {
        let _env = crate::env_lock();
        let dir = std::env::temp_dir().join("clew-embed-config-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // SAFETY: env mutation serialized by env_lock.
        unsafe {
            std::env::set_var("CLEW_DATA_DIR", &dir);
            std::env::set_var("OPENAI_API_KEY", "sk-env");
        }
        let cfg = dir.join("config.toml");

        // A foreign endpoint with no key of its own is simply unconfigured.
        std::fs::write(
            &cfg,
            "[embedding]\nbase_url = \"http://localhost:1234/v1\"\n",
        )
        .unwrap();
        assert!(
            Config::load().is_none(),
            "the env key must not travel to a non-OpenAI endpoint"
        );

        // The same endpoint with its own key still works, and uses that key.
        std::fs::write(
            &cfg,
            "[embedding]\napi_key = \"sk-local\"\nbase_url = \"http://localhost:1234/v1\"\n",
        )
        .unwrap();
        let c = Config::load().expect("explicit key configures the endpoint");
        assert_eq!(c.api_key, "sk-local");

        // OpenAI's own endpoint (implicit default) does inherit it.
        std::fs::write(&cfg, "[embedding]\nmodel = \"m\"\n").unwrap();
        let c = Config::load().expect("OpenAI default inherits the env key");
        assert_eq!(c.api_key, "sk-env");
        assert_eq!(c.base_url, DEFAULT_BASE_URL);

        // Written out explicitly, trailing slash and all — the comparison runs
        // after the trim, so this is still OpenAI.
        std::fs::write(
            &cfg,
            "[embedding]\nbase_url = \"https://api.openai.com/v1/\"\n",
        )
        .unwrap();
        assert_eq!(Config::load().expect("still OpenAI").api_key, "sk-env");

        // SAFETY: env mutation serialized by env_lock.
        unsafe {
            std::env::remove_var("CLEW_DATA_DIR");
            std::env::remove_var("OPENAI_API_KEY");
        }
    }

    /// An index built in another embedding space must read as absent: reusing
    /// it would rank the query by cosine against vectors it shares no geometry
    /// with, silently and with no way to fix it from the UI.
    #[test]
    fn index_from_another_model_or_endpoint_is_discarded() {
        let _env = crate::env_lock();
        let dir = std::env::temp_dir().join("clew-embed-index-test");
        let _ = std::fs::remove_dir_all(&dir);
        let store = dir.join("store");
        let root = dir.join("proj");
        std::fs::create_dir_all(&store).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        // SAFETY: env mutation serialized by env_lock.
        unsafe {
            std::env::set_var("CLEW_DATA_DIR", &dir);
            std::env::remove_var("OPENAI_API_KEY");
        }
        std::fs::write(
            dir.join("config.toml"),
            "[embedding]\napi_key = \"sk\"\nmodel = \"m-a\"\n",
        )
        .unwrap();

        let index = Index {
            model: "m-a".into(),
            base_url: DEFAULT_BASE_URL.into(),
            entries: vec![Entry {
                node: Node::Function {
                    file: root.join("a.rs"),
                    name: "f".into(),
                    ordinal: 0,
                },
                hash: 0,
                vec: vec![1.0, 0.0],
            }],
        };
        save(&store, &index).unwrap();

        // The config it was saved under still owns it.
        assert_eq!(load(&store, &root).entries.len(), 1, "same space is kept");

        // Another model, same endpoint.
        let other = Config::from_parts("sk".into(), "m-b".into(), DEFAULT_BASE_URL.into());
        assert!(
            load_for(&store, &root, Some(&other)).entries.is_empty(),
            "a model change must invalidate the index"
        );

        // Same model name, another provider serving it.
        let other =
            Config::from_parts("sk".into(), "m-a".into(), "http://localhost:1234/v1".into());
        assert!(
            load_for(&store, &root, Some(&other)).entries.is_empty(),
            "an endpoint change must invalidate the index"
        );

        // An index written before the identity fields existed names no space.
        // Serializing an `Index` no longer produces that shape — it now
        // carries both — so the file is written field by field, omitting them.
        std::fs::write(
            index_path(&store),
            serde_json::to_string(&serde_json::json!({ "entries": index.entries }))
                .unwrap()
                .as_bytes(),
        )
        .unwrap();
        assert!(
            load(&store, &root).entries.is_empty(),
            "a pre-identity index is foreign, not matching"
        );

        // SAFETY: env mutation serialized by env_lock.
        unsafe {
            std::env::remove_var("CLEW_DATA_DIR");
        }
    }

    /// The space is the model and the endpoint, and NOTHING else. A key is not
    /// part of it: reading the space through `Config::load` would make a
    /// cleared key read as a move to another space (it returns `None`) and a
    /// rotated key read as no change only by accident — and the callers that
    /// compare spaces across a config write answer "throw the index away" to
    /// every difference they see.
    #[test]
    fn the_stored_space_is_the_model_and_endpoint_and_ignores_the_key() {
        let _env = crate::env_lock();
        let dir = std::env::temp_dir().join("clew-embed-space-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // SAFETY: env mutation serialized by env_lock.
        unsafe { std::env::set_var("CLEW_DATA_DIR", &dir) };
        let cfg = dir.join("config.toml");
        let write = |body: &str| std::fs::write(&cfg, body).unwrap();

        write("[embedding]\napi_key = \"sk-one\"\nmodel = \"m\"\n");
        let base = stored_space();
        assert_eq!(base.model, "m");
        assert_eq!(base.base_url, DEFAULT_BASE_URL);

        // Rotating the key, and clearing it entirely, leave the space alone.
        write("[embedding]\napi_key = \"sk-two\"\nmodel = \"m\"\n");
        assert_eq!(stored_space(), base, "a rotated key is not a new space");
        write("[embedding]\nmodel = \"m\"\n");
        assert_eq!(stored_space(), base, "a cleared key is not a new space");

        // Either half of the identity moving IS a new space.
        write("[embedding]\napi_key = \"sk-one\"\nmodel = \"m2\"\n");
        assert_ne!(stored_space(), base, "a model change is a new space");
        write(
            "[embedding]\napi_key = \"sk-one\"\nmodel = \"m\"\nbase_url = \"http://localhost:1234/v1\"\n",
        );
        assert_ne!(stored_space(), base, "an endpoint change is a new space");

        // An in-memory index carries BOTH halves, so either one moving
        // disowns it. An empty index is nobody's — nothing to mis-rank.
        let index = Index {
            model: "m".into(),
            base_url: DEFAULT_BASE_URL.into(),
            entries: vec![Entry {
                node: Node::Function {
                    file: PathBuf::from("/p/a.rs"),
                    name: "f".into(),
                    ordinal: 0,
                },
                hash: 0,
                vec: vec![1.0],
            }],
        };
        assert!(!base.is_foreign(&index));
        assert!(!base.is_foreign(&Index::default()));
        write("[embedding]\napi_key = \"sk-one\"\nmodel = \"m2\"\n");
        assert!(
            stored_space().is_foreign(&index),
            "a model change must disown the vectors"
        );
        // The endpoint-only repoint, which this could NOT see while `Index`
        // carried the model alone. It has to be seen here: the settings
        // handler only drops the index of the window that saved, so another
        // window kept these vectors and would have had them reused and then
        // written back stamped with the new space.
        let repointed = Space {
            model: "m".into(),
            base_url: "http://localhost:1234/v1".into(),
        };
        assert!(
            repointed.is_foreign(&index),
            "the same model at another provider is another space"
        );
        // An index written before `base_url` existed reads as foreign, which
        // rebuilds it — the safe direction.
        let legacy = Index {
            model: "m".into(),
            base_url: String::new(),
            entries: index.entries.clone(),
        };
        assert!(base.is_foreign(&legacy));

        // SAFETY: env mutation serialized by env_lock.
        unsafe { std::env::remove_var("CLEW_DATA_DIR") };
    }

    /// An endpoint that accepts and then says nothing must not own the calling
    /// thread for the life of the process. `ureq`'s default agent has no read
    /// timeout, and every caller here is blocking.
    #[test]
    fn a_silent_endpoint_gives_the_thread_back() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        // Hold the accepted socket open and never answer.
        std::thread::spawn(move || {
            let held = listener.accept();
            std::thread::sleep(std::time::Duration::from_secs(10));
            drop(held);
        });

        let started = std::time::Instant::now();
        let err = build_agent(std::time::Duration::from_millis(300))
            .post(&format!("http://{addr}/embeddings"))
            .send_string("{}")
            .expect_err("a silent endpoint is a failure, not an answer");
        let waited = started.elapsed();
        assert!(
            waited < std::time::Duration::from_secs(3),
            "gave up after {waited:?}, which is the timeout not firing ({err})"
        );
    }

    /// The batch must go where it was addressed. Proven through `embed_batch`
    /// rather than through `agent()` directly, because the defect being kept
    /// out is `embed_batch` reaching for `ureq::post` and its implicit agent.
    #[test]
    fn a_redirect_is_refused_rather_than_followed() {
        // Where the redirect points. Answers with a perfectly valid embedding,
        // so a followed hop would look like success.
        let sink = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let sink_addr = sink.local_addr().unwrap();
        std::thread::spawn(move || {
            for conn in sink.incoming() {
                let Ok(mut conn) = conn else { break };
                let mut buf = [0u8; 4096];
                let _ = std::io::Read::read(&mut conn, &mut buf);
                let body = r#"{"data":[{"index":0,"embedding":[1.0]}]}"#;
                let _ = std::io::Write::write_all(
                    &mut conn,
                    format!(
                        "HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
            }
        });

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let Ok((mut conn, _)) = listener.accept() else {
                return;
            };
            let mut buf = [0u8; 8192];
            let _ = std::io::Read::read(&mut conn, &mut buf);
            let _ = std::io::Write::write_all(
                &mut conn,
                format!(
                    "HTTP/1.1 302 Found\r\nlocation: http://{sink_addr}/embeddings\r\n\
                     connection: close\r\ncontent-length: 0\r\n\r\n"
                )
                .as_bytes(),
            );
        });

        let cfg = Config {
            api_key: "SECRET-KEY".into(),
            model: "m".into(),
            base_url: format!("http://{addr}"),
        };
        let err = embed_batch(&cfg, &["hello".to_string()])
            .expect_err("a 3xx is not a set of embeddings");
        assert!(err.contains("redirected"), "the refusal is reported: {err}");
    }

    /// A build covers the nodes the BUILDING window's explanation cache holds,
    /// and the derived store is shared by every window on the project — so
    /// writing a build wholesale shrank a whole-project index down to one
    /// window's subset, and FIND then answered from that subset silently.
    #[test]
    fn a_partial_build_tops_the_stored_index_up_instead_of_replacing_it() {
        let _env = crate::env_lock();
        let dir = std::env::temp_dir().join("clew-embed-merge-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // SAFETY: env mutation serialized by env_lock. `save` stamps the
        // endpoint from the live config, and `load` refuses an index from any
        // other one — so both halves need the same configured endpoint.
        unsafe { std::env::set_var("CLEW_DATA_DIR", &dir) };
        std::fs::write(
            dir.join("config.toml"),
            "[embedding]\napi_key = \"k\"\nmodel = \"m\"\n",
        )
        .unwrap();
        let root = PathBuf::from("/p");
        let entry = |name: &str, v: f32| Entry {
            node: Node::Function {
                file: PathBuf::from("/p/a.rs"),
                name: name.into(),
                ordinal: 0,
            },
            hash: 0,
            vec: vec![v],
        };

        // Window A indexes the whole project.
        let whole = Index {
            model: "m".into(),
            base_url: DEFAULT_BASE_URL.into(),
            entries: vec![entry("f1", 1.0), entry("f2", 2.0)],
        };
        save(&dir, &whole).unwrap();

        // Window B, whose explanation cache only holds f2, rebuilds. What the
        // wholesale write did:
        let partial = Index {
            model: "m".into(),
            base_url: DEFAULT_BASE_URL.into(),
            entries: vec![entry("f2", 9.0)],
        };
        save(&dir, &partial).unwrap();
        assert_eq!(load(&dir, &root).entries.len(), 1, "this is the loss");

        // The same build merged: f1 survives, and f2 takes the fresh vector.
        save(&dir, &whole).unwrap();
        let (merged, saved) = merge_built(&dir, &root, &partial);
        saved.unwrap();
        assert_eq!(merged.entries.len(), 2);
        let f2 = merged
            .entries
            .iter()
            .find(|e| matches!(&e.node, Node::Function { name, .. } if name == "f2"))
            .unwrap();
        assert_eq!(f2.vec, vec![9.0], "the freshly embedded vector wins");
        assert_eq!(load(&dir, &root).entries.len(), 2, "and it is on disk");
        unsafe { std::env::remove_var("CLEW_DATA_DIR") };
    }
}
