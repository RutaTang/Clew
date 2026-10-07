//! Embeddings + semantic search over the codebase.
//!
//! We embed each function/file **explanation summary** (concise, semantic, and
//! already cached) with an OpenAI-compatible `/embeddings` endpoint, and keep a
//! small vector index as `embeddings.json` in the project's derived store —
//! clew's own data directory, keyed by host and project root (see
//! [`crate::derived`]), never inside the repository. A natural-language query
//! is embedded the same way and ranked by cosine similarity, so you can find
//! code by what it *does* rather than by its text; "Ask clew" retrieves its
//! context through the same index.
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

/// Largest successful `/embeddings` response read. A 96-text batch of
/// 3072-dimensional vectors is ~6 MiB of JSON; anything past this is not an
/// answer to the request, and reading it unbounded let one endpoint grow the
/// process without limit.
const MAX_RESPONSE_BYTES: u64 = 64 * 1024 * 1024;

// The meter beneath the connection ([`agent`]) counts every byte it delivers,
// framing included: a whole answer of the size above must fit under it with
// as much again to spare, or a legitimate chunked answer fails at the meter.
const _: () = assert!(2 * MAX_RESPONSE_BYTES <= crate::llm::RAW_RESPONSE_CAP);

/// Whether `model` is documented to accept the `dimensions` request field:
/// OpenAI's text-embedding-3 family, also when a router prefixes it
/// (`openai/text-embedding-3-small`). Everything else gets no `dimensions`:
/// OpenAI rejects it for older models (`text-embedding-ada-002`), and
/// OpenAI-compatible servers variously reject, ignore or honor it — so its
/// meaning is only known where it is documented.
fn supports_dimensions(model: &str) -> bool {
    model
        .rsplit('/')
        .next()
        .unwrap_or(model)
        .to_ascii_lowercase()
        .starts_with("text-embedding-3")
}

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
/// (the reason the on-disk `Stored` form records both). Vectors from two
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
        let config = Self::current_or_default();
        (!config.api_key.is_empty()).then_some(config)
    }

    /// The stored embedding settings (defaults filled), even without a key.
    /// The settings form must snapshot these values so a later save does not
    /// mistake the stored model and endpoint for edits made by another window.
    pub fn current_or_default() -> Config {
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
            .filter(|k| !k.is_empty())
            .unwrap_or_default();
        Config {
            api_key,
            model,
            base_url,
        }
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
/// Without it the call could park a thread forever: with no read timeout (as
/// ureq's default agent has none), an endpoint that accepts the connection and
/// then says nothing — a wedged local model server is the common case — held
/// the caller indefinitely, and every caller is blocking. It bounds each
/// silence, and nothing more: a peer that is never silent this long is
/// bounded by [`BATCH_PACE`], and a caller that stops wanting the answer
/// gives the request up at once ([`embed_batch_cancellable`]) — every caller
/// does: the GUI's FIND and Ask retrieval (`src/app/semantic.rs`,
/// `src/app/ask.rs`), its index build, and the server's `semantic_find` and
/// `Embed`.
///
/// Generous on purpose: a cold local model can legitimately take tens of
/// seconds to answer the first embedding of a batch.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// The pace one embeddings batch's answer must keep (`crate::net::Pace`):
/// three minutes from the batch's first byte out to start, then 8 KiB a
/// second, held over windows of one idle limit ([`REQUEST_TIMEOUT`], a
/// minute) — each must bring 480 KiB, or the batch fails at its end.
///
/// [`REQUEST_TIMEOUT`] bounds each silence; this bounds the whole. A peer
/// that trickles its answer a byte at a time never trips an idle limit, and
/// held the caller — a thread and a connection — for as long as it went on.
/// A deadline of its own, though, has to fit the largest answer on the
/// slowest link, and a fixed one did not: a batch of 96 vectors of 3072 or
/// 4096 dimensions is 6 to 9 MB of JSON, which needs half a megabit a
/// second to come in within three minutes — below that every batch failed,
/// and the index build with it. Paced, an answer that keeps coming faster
/// than 8 KiB a second — by a read's worth a window, as the pace is looked
/// at once a read — completes whatever its size; one that falls behind
/// that for a window is cut off at its end — after the first three
/// minutes, three idle limits, room for the slowest cold local model to
/// start answering. A window is one idle limit: none falls within a silence
/// the idle limit lets pass, and a trickle is cut four minutes in. What a
/// window brings past its due buys none after it, so no burst buys a
/// trickle. The most a batch can take is still bounded: four minutes, and
/// what one answer may deliver at all (`crate::llm::RAW_RESPONSE_CAP`) at
/// that rate.
const BATCH_PACE: crate::net::Pace = crate::net::Pace {
    grace: std::time::Duration::from_secs(180),
    window: REQUEST_TIMEOUT,
    floor: 8 * 1024,
};

/// The agent an embeddings request to `url` goes through: the model-provider
/// transport policy of [`crate::llm::agent_for`], the one the chat calls use,
/// with [`REQUEST_TIMEOUT`] on every read and write and [`BATCH_PACE`] on
/// the whole — and the proxy it goes through, for what a failure there says.
/// Its connections are kept for the next batch, and each response on one is
/// metered beneath ureq (`crate::llm::Limits::direct`): a peer that answered
/// with an endless run of chunked framing cannot grow the process past the
/// meter, however many batches the connection carried before.
///
/// One policy, not a second copy of it. This used to be an agent of its own —
/// bundled roots only, no proxy — so an embeddings endpoint behind a corporate
/// proxy or a private CA failed where the same host's chat worked, and a
/// `NO_PROXY` local server was unreachable once a proxy was set. What the two
/// always shared still holds: no redirect is followed, which matters even
/// though this endpoint authenticates with `Authorization` (which ureq does
/// strip on a redirect): following would silently send the batch to a host
/// the user never configured and parse whatever came back as embeddings.
fn agent(url: &str) -> Result<(ureq::Agent, Option<crate::net::Via>), String> {
    build_agent(url, REQUEST_TIMEOUT, BATCH_PACE)
}

/// Split out from [`agent`] so a test can watch a short timeout, or a short
/// pace, actually fire; the real ones are too long to wait on.
fn build_agent(
    url: &str,
    timeout: std::time::Duration,
    pace: crate::net::Pace,
) -> Result<(ureq::Agent, Option<crate::net::Via>), String> {
    crate::llm::agent_for(url, crate::llm::Limits::direct(timeout, pace)).map_err(String::from)
}

/// Embed a batch of texts in one request (keep batches modest to stay under the
/// endpoint's token cap). Blocking — run off the UI thread — and bounded:
/// every silence by [`REQUEST_TIMEOUT`], the whole by [`BATCH_PACE`]. A
/// caller that must be able to stop it uses [`embed_batch_cancellable`].
pub fn embed_batch(cfg: &Config, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
    embed_batch_cancellable(cfg, texts, &|| false)
}

/// How often a caller waiting on a batch re-tests its `cancelled` predicate.
const CANCEL_POLL: std::time::Duration = std::time::Duration::from_millis(100);

/// [`embed_batch`], given up the moment `cancelled` says so — with the
/// request itself, not just the wait for it.
///
/// The request runs on a worker thread whose connection is on the worker's
/// line (`crate::llm::spawn_worker`), and this side only waits for its
/// answer. A stop drops the worker's abandon, which shuts the connection
/// down under whatever the request is blocked in — the connect, the wait for
/// the answer, the answer coming in — so the worker lets go at once, and an
/// endpoint working on a batch nobody wants sees the connection close.
/// Otherwise the connection is kept for the next batch: every batch's worker
/// is abandoned once its answer is in, and that abandon, of a request that is
/// over, cannot reach the batch the connection serves next
/// (`crate::net::Lease`).
pub fn embed_batch_cancellable(
    cfg: &Config,
    texts: &[String],
    cancelled: &dyn Fn() -> bool,
) -> Result<Vec<Vec<f32>>, String> {
    if texts.is_empty() {
        return Ok(Vec::new());
    }
    let url = endpoint(cfg);
    let (agent, via) = agent(&url)?;
    run_batch(agent, via, url, cfg, texts, cancelled)
}

/// [`embed_batch_cancellable`]'s request, through `agent` (a test's own in
/// tests), on a worker thread.
fn run_batch(
    agent: ureq::Agent,
    via: Option<crate::net::Via>,
    url: String,
    cfg: &Config,
    texts: &[String],
    cancelled: &dyn Fn() -> bool,
) -> Result<Vec<Vec<f32>>, String> {
    let (cfg, texts) = (cfg.clone(), texts.to_vec());
    let (answer, _abandon) = crate::llm::spawn_worker(1, move |_, tx| {
        let _ = tx.send(request_batch(&agent, via.as_ref(), &url, &cfg, &texts));
    })
    .map_err(String::from)?;
    loop {
        if cancelled() {
            return Err(crate::llm::CANCELLED.to_string());
        }
        match answer.recv_timeout(CANCEL_POLL) {
            Ok(result) => return result,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            // The worker died without answering (it panicked).
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err("the embeddings request stopped unexpectedly".into());
            }
        }
    }
}

/// Where `cfg`'s embeddings requests go.
fn endpoint(cfg: &Config) -> String {
    format!("{}/embeddings", cfg.base_url.trim_end_matches('/'))
}

/// Largest error body read for the endpoint's own message.
const MAX_ERROR_BODY_BYTES: u64 = 64 * 1024;

/// [`embed_batch`]'s request, through `agent` (a test's own in tests), which
/// goes through the proxy `via`.
fn request_batch(
    agent: &ureq::Agent,
    via: Option<&crate::net::Via>,
    url: &str,
    cfg: &Config,
    texts: &[String],
) -> Result<Vec<Vec<f32>>, String> {
    let uri = crate::net::request_uri(url)?;
    let mut body = serde_json::json!({
        "model": cfg.model,
        "input": texts,
    });
    if supports_dimensions(&cfg.model) {
        body["dimensions"] = DIMS.into();
    }
    let body = body.to_string();
    // A panic inside ureq is an error of this request, not of the thread
    // that asked (`net::guarded`).
    let resp = crate::net::guarded(|| {
        agent
            .post(uri)
            .header("Authorization", format!("Bearer {}", cfg.api_key))
            .header("content-type", "application/json")
            .send(body.as_str())
    })
    .map_err(|e| format!("request failed: {}", crate::net::failure(&e, via)))?;
    let status = resp.status();
    // No redirect is followed (the agent follows none), so a 3xx is an
    // ordinary response whose body is a redirect page. Say so, rather than
    // failing later with "bad JSON".
    if status.is_redirection() {
        let to = resp
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("elsewhere");
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
    if status.as_u16() == 407
        && let Some(via) = via
    {
        // What the proxy's challenges offer decides what can be done: a
        // user name and password clew can send, or logins (NTLM, Negotiate)
        // it cannot make — as for a model call (`crate::llm`).
        return Err(format!(
            "request failed: {}",
            via.answered_407(resp.headers())
        ));
    }
    if status.is_client_error() || status.is_server_error() {
        let mut raw = Vec::new();
        let _ = resp
            .into_body()
            .into_reader()
            .take(MAX_ERROR_BODY_BYTES)
            .read_to_end(&mut raw);
        let raw = String::from_utf8_lossy(&raw);
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
        return Err(format!("embeddings API error {}: {msg}", status.as_u16()));
    }
    let text = read_body(resp.into_body().into_reader(), MAX_RESPONSE_BYTES)?;
    parse_embeddings(&text, texts.len())
}

/// A response body as text, refusing one longer than `cap` bytes (read through
/// the cap, never buffered whole first).
fn read_body(body: impl Read, cap: u64) -> Result<String, String> {
    let mut s = String::new();
    body.take(cap + 1)
        .read_to_string(&mut s)
        .map_err(|e| format!("read: {}", crate::net::describe_io(&e)))?;
    if s.len() as u64 > cap {
        return Err(format!(
            "embeddings response exceeded {} MiB; not an answer to this request",
            cap.div_ceil(1024 * 1024)
        ));
    }
    Ok(s)
}

/// The vectors in an `/embeddings` response, in input order — or an error
/// naming what is wrong with it.
///
/// Strict on purpose: every element must be a number, every input must get
/// exactly one vector, and all vectors must share one length. A vector with a
/// non-number silently dropped, or a batch of mixed lengths, used to reach
/// the index, where cosine against a different length is 0 — so the failure
/// surfaced much later as "no results", with nothing to say why.
fn parse_embeddings(text: &str, expected: usize) -> Result<Vec<Vec<f32>>, String> {
    let json: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("bad JSON: {e}"))?;
    let data = json
        .get("data")
        .and_then(|d| d.as_array())
        .ok_or("no data in response")?;
    if data.len() != expected {
        return Err(format!(
            "expected {expected} embeddings, got {}",
            data.len()
        ));
    }
    let mut rows: Vec<Option<Vec<f32>>> = vec![None; expected];
    for (pos, d) in data.iter().enumerate() {
        // `data` is index-ordered per the spec; honor an explicit index.
        let idx = match d.get("index") {
            Some(i) => i
                .as_u64()
                .map(|i| i as usize)
                .ok_or_else(|| format!("embedding {pos} has a non-integer index"))?,
            None => pos,
        };
        let slot = rows
            .get_mut(idx)
            .ok_or_else(|| format!("embedding index {idx} is out of range"))?;
        if slot.is_some() {
            return Err(format!("embedding index {idx} appears twice"));
        }
        let values = d
            .get("embedding")
            .and_then(|e| e.as_array())
            .ok_or_else(|| format!("embedding {idx} is not a list of numbers"))?;
        let vec = values
            .iter()
            .map(|x| x.as_f64().map(|f| f as f32))
            .collect::<Option<Vec<f32>>>()
            .ok_or_else(|| format!("embedding {idx} holds a non-number"))?;
        if vec.is_empty() {
            return Err(format!("embedding {idx} is empty"));
        }
        *slot = Some(vec);
    }
    let rows: Vec<Vec<f32>> = rows.into_iter().flatten().collect();
    if let Some(first) = rows.first()
        && let Some(other) = rows.iter().find(|v| v.len() != first.len())
    {
        return Err(format!(
            "the endpoint returned vectors of different lengths ({} and {})",
            first.len(),
            other.len()
        ));
    }
    Ok(rows)
}

/// Embed many texts, chunked to stay under the endpoint's per-request cap.
/// Blocking — run off the UI thread — and given up the moment `cancelled`
/// says so, the batch in flight with it ([`embed_batch_cancellable`]): an
/// index build nobody waits for any more stops embedding, and billing, at
/// once, rather than after every batch it has left.
pub fn embed_all(
    cfg: &Config,
    texts: &[String],
    cancelled: &dyn Fn() -> bool,
) -> Result<Vec<Vec<f32>>, String> {
    const CHUNK: usize = 96;
    let mut out: Vec<Vec<f32>> = Vec::with_capacity(texts.len());
    for chunk in texts.chunks(CHUNK) {
        if cancelled() {
            return Err(crate::llm::CANCELLED.to_string());
        }
        let batch = embed_batch_cancellable(cfg, chunk, cancelled)?;
        // Each batch is uniform; so must the batches be with each other.
        if let (Some(first), Some(next)) = (out.first(), batch.first())
            && first.len() != next.len()
        {
            return Err(format!(
                "the endpoint changed its vector length mid-build ({} then {})",
                first.len(),
                next.len()
            ));
        }
        out.extend(batch);
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

/// The vector index in memory. On disk it is wrapped by `Stored`, which also
/// records the endpoint, so [`load`] can tell a foreign index from this one's.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Index {
    pub model: String,
    /// The endpoint these vectors were built against. Carried here and not
    /// only in `Stored` because a model name does not name a space: the same
    /// name served by another provider is a different one, and without this
    /// field [`Space::is_foreign`] could not see a repoint that left the model
    /// alone. That blind spot let one window's stale vectors be reused after
    /// another window changed only the endpoint, and then written back stamped
    /// with the NEW space — poisoning the shared file for good.
    pub base_url: String,
    pub entries: Vec<Entry>,
}

impl Index {
    /// The space these vectors live in.
    pub fn space(&self) -> Space {
        Space {
            model: self.model.clone(),
            base_url: self.base_url.clone(),
        }
    }

    /// The vector length of this index, or `None` when it is empty. Every
    /// index clew builds, loads or merges is uniform (see `uniform_dims`).
    pub fn dims(&self) -> Option<usize> {
        self.entries.first().map(|e| e.vec.len())
    }
}

/// Keep only the entries with the index's majority vector length, returning
/// how many were dropped. Mixed lengths come from a build whose endpoint
/// changed its output size without changing its name (or from a file written
/// before lengths were checked); cosine across two lengths is meaningless, so
/// the minority is dropped and simply re-embedded by the next build.
fn uniform_dims(entries: &mut Vec<Entry>) -> usize {
    let mut counts: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    for e in entries.iter() {
        *counts.entry(e.vec.len()).or_default() += 1;
    }
    // Ties go to the longer vector, deterministically.
    let Some((&keep, _)) = counts.iter().max_by_key(|&(len, n)| (*n, *len)) else {
        return 0;
    };
    let before = entries.len();
    entries.retain(|e| e.vec.len() == keep);
    before - entries.len()
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

/// [`load`], also saying why a stored index that EXISTS was not used: not a
/// plain file, over the state-file read cap, or unreadable. A caller that can
/// show it should: without it, an index that is silently refused is rebuilt —
/// and re-billed — at every project open with nothing but a stderr line to
/// say why. An index from another embedding space is NOT a problem; it is the
/// expected result of a config change, and reads as absent.
pub fn load_checked(store: &Path, root: &Path) -> (Index, Option<String>) {
    let cfg = Config::load();
    load_in(store, root, cfg.map(|c| c.space()).as_ref())
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
    let (index, problem) = load_in(store, root, cfg.map(Config::space).as_ref());
    // Absent is normal. Present but refused is not, and must not look like
    // "no index": say why, so a rebuild that keeps vanishing has an
    // explanation somewhere even for a caller that cannot show one.
    if let Some(problem) = problem {
        eprintln!("clew: {problem}");
    }
    index
}

/// [`load_for`] against an explicit space (`None`: nothing can match), with
/// the reason a stored index that exists was refused (see [`load_checked`]).
fn load_in(store: &Path, root: &Path, space: Option<&Space>) -> (Index, Option<String>) {
    let path = index_path(store);
    let Some(text) = crate::statefile::read(&path) else {
        return (Index::default(), stored_index_problem(store));
    };
    let stored = match serde_json::from_str::<Stored>(&text) {
        Ok(stored) => stored,
        Err(e) => {
            let problem = format!(
                "the semantic index {} is unreadable ({e}); it will be rebuilt",
                path.display()
            );
            return (Index::default(), Some(problem));
        }
    };
    if !space.is_some_and(|s| s.model == stored.model && s.base_url == stored.base_url) {
        return (Index::default(), None);
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
    uniform_dims(&mut index.entries);
    (index, None)
}

/// Why the stored index for `store` exists but cannot even be read, if it
/// does: over the state-file read cap, or not a plain file. `None` when it is
/// absent or readable. Cheap (one `lstat`), but blind to a file that reads
/// and does not parse — [`load_checked`] reports that case too.
pub fn stored_index_problem(store: &Path) -> Option<String> {
    let path = index_path(store);
    let meta = std::fs::symlink_metadata(&path).ok()?;
    if !meta.is_file() {
        return Some(format!(
            "the semantic index {} is not a plain file; ignored",
            path.display()
        ));
    }
    (meta.len() > crate::statefile::MAX_STATE_BYTES).then(|| {
        format!(
            "the semantic index {} is {} MiB, over the {} MiB read cap; ignored and rebuilt",
            path.display(),
            meta.len() / (1024 * 1024),
            crate::statefile::MAX_STATE_BYTES / (1024 * 1024)
        )
    })
}

/// Persist the index, stamped with the embedding space ITS VECTORS came from
/// (`index.model` / `index.base_url`).
///
/// Stamping the live config instead — what this used to do — let a config
/// change that landed while a build ran label old-space vectors with the new
/// space, after which every [`load_for`] trusted them for good. The index
/// knows its own space; the config at write time does not.
///
/// Correct only when the caller's index IS the whole truth. A window builds
/// its index from the nodes ITS explanation cache holds, so a save from a
/// second window covering fewer nodes must go through [`merge_built`].
pub fn save(store: &Path, index: &Index) -> std::io::Result<()> {
    save_bounded(store, index).map(|_| ())
}

/// [`save`], never writing more than [`load_for`] will read back: when the
/// serialized index exceeds the state-file read cap, entries are left out of
/// the FILE from the end of `index.entries` until it fits, and their count is
/// returned; `index` itself is untouched. Which entries those are is the
/// caller's order — [`merge_built`] makes it deterministic, and puts the
/// entries its build did not cover last. An index written past the cap used
/// to load as EMPTY on every start — silently — and was then rebuilt, and
/// re-billed, each session.
pub fn save_bounded(store: &Path, index: &Index) -> std::io::Result<usize> {
    save_capped(store, index, crate::statefile::MAX_STATE_BYTES)
}

fn save_capped(store: &Path, index: &Index, cap: u64) -> std::io::Result<usize> {
    let mut keep = index.entries.len();
    loop {
        let stored = StoredRef {
            model: &index.model,
            base_url: &index.base_url,
            entries: &index.entries[..keep],
        };
        let json = serde_json::to_vec(&stored).map_err(|e| std::io::Error::other(e.to_string()))?;
        if json.len() as u64 <= cap || keep == 0 {
            crate::statefile::write_atomic(&index_path(store), &json)?;
            let left_out = index.entries.len() - keep;
            if left_out > 0 {
                eprintln!(
                    "clew: the semantic index is over the {} MiB file cap; its last {left_out} entries were not saved \
                     (a later session re-embeds them)",
                    cap / (1024 * 1024)
                );
            }
            return Ok(left_out);
        }
        // Shrink in proportion to the excess (plus a margin), not one entry
        // per serialization.
        let per_entry = (json.len() / keep).max(1);
        let excess = json.len() - cap as usize;
        keep = keep.saturating_sub(excess / per_entry + 1 + keep / 100);
    }
}

/// Serializes the read-modify-write below across this process's windows.
static SAVE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Fold a freshly built index into the one ON DISK RIGHT NOW, returning the
/// merged index the caller must adopt — all of it, whether or not all of it
/// fits in the file.
///
/// A build covers the nodes the BUILDING window's explanation cache holds, and
/// the derived store is shared by every window and every clew process on the
/// project. Writing the build wholesale therefore replaced an index covering
/// the whole project with one covering the subset one window knew about, and
/// FIND then answered from that subset with no sign anything was missing.
///
/// Fresh entries win per node (they were embedded from the current summary).
/// The merge has ONE order, whatever order the build listed its entries in
/// (it comes out of a hash map): this build's entries first, then the entries
/// only the stored index has, each part in canonical node order (`Node`'s
/// `Ord`, the order `explain.json` is written in). So the same inputs always
/// write the same bytes, and when the file must be cut to the read cap (see
/// [`save_bounded`]) the same entries are always the ones left out: first
/// those this build did not cover — another window's nodes, or nodes that no
/// longer exist — then this build's last nodes in node order. Nothing records
/// when a vector was embedded, so "the oldest go" is not a promise this can
/// make; the old claim that it did was false.
///
/// Only the WRITTEN copy is cut. The caller builds its next index from the
/// one returned here, and when that was the cut copy every entry past the cap
/// went back to the endpoint on every build — to be cut from the file again.
/// Now the entries the file cannot hold are paid for at most once per
/// session: when a new session loads the capped file and builds.
///
/// The stored index is judged against the BUILD's space, not the live
/// config's: a stored index from any other space is discarded, so a merged
/// file always describes the one space its stamp names. Stored entries of a
/// different vector length than the build's are dropped for the same reason.
///
/// The read-merge-write runs under the store's file lock
/// ([`crate::statefile::lock`]), so a second clew process cannot slip a write
/// in between. When the lock cannot be taken the merge is still computed from
/// a fresh read and returned — the vectors were paid for — but NOTHING is
/// written, and the `Err` says why.
pub fn merge_built(store: &Path, root: &Path, built: &Index) -> (Index, std::io::Result<()>) {
    let (merged, saved) = merge_capped(store, root, built, crate::statefile::MAX_STATE_BYTES);
    (merged, saved.map(|_| ()))
}

/// [`merge_built`] under an explicit file cap, returning how many entries the
/// written copy left out.
fn merge_capped(
    store: &Path,
    root: &Path,
    built: &Index,
    cap: u64,
) -> (Index, std::io::Result<usize>) {
    // Poisoning only means an earlier caller panicked; the index is re-read
    // from disk here regardless, so there is no corrupt state to inherit.
    let _serialized = SAVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // A lock that cannot be taken leaves the file alone (see below): writing
    // unlocked is the lost update the lock prevents, and the reasons it fails
    // — the store cannot be created, the lock's name is squatted — forbid the
    // write as well.
    let locked = crate::statefile::lock(&index_path(store))
        .map_err(|e| std::io::Error::new(e.kind(), format!("could not lock embeddings.json: {e}")));
    let (stored, problem) = load_in(store, root, Some(&built.space()));
    if let Some(problem) = problem {
        eprintln!("clew: {problem}");
    }
    let dims = built.dims();
    let mut entries = in_node_order(built.entries.clone());
    let only_stored = {
        let covered: std::collections::HashSet<&Node> = entries.iter().map(|e| &e.node).collect();
        in_node_order(
            stored
                .entries
                .into_iter()
                .filter(|e| !covered.contains(&e.node) && dims.is_none_or(|d| e.vec.len() == d))
                .collect(),
        )
    };
    entries.extend(only_stored);
    // `retain`, so the order survives.
    uniform_dims(&mut entries);
    let merged = Index {
        model: built.model.clone(),
        base_url: built.base_url.clone(),
        entries,
    };
    // Held (when it was taken) until the write is done.
    let saved = locked.and_then(|_held| save_capped(store, &merged, cap));
    (merged, saved)
}

/// `entries` in canonical node order, each node once (the first listing wins;
/// the sort is stable).
fn in_node_order(mut entries: Vec<Entry>) -> Vec<Entry> {
    entries.sort_by(|a, b| a.node.cmp(&b.node));
    entries.dedup_by(|later, first| later.node == first.node);
    entries
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

/// Whether `query` can be ranked against `index` at all: an explicit error when
/// their vector lengths differ (the endpoint changed its output size, or the
/// index predates a change in which models get the `dimensions` field).
/// Cosine across two lengths is 0, so without this the search just came back
/// empty — "no results", with nothing to say the index needs a rebuild.
pub fn check_query(index: &Index, query: &[f32]) -> Result<(), String> {
    match index.dims() {
        Some(dims) if dims != query.len() => Err(format!(
            "the semantic index holds {dims}-dimensional vectors but the query embedding has {} — \
             the embedding endpoint changed its output; rebuild the index",
            query.len()
        )),
        _ => Ok(()),
    }
}

/// [`search`], refusing a query of the wrong vector length (see
/// [`check_query`]) instead of returning nothing.
pub fn search_checked<'a>(
    index: &'a Index,
    query: &[f32],
    k: usize,
) -> Result<Vec<(&'a Node, f32)>, String> {
    check_query(index, query)?;
    Ok(search(index, query, k))
}

/// Rank the index by cosine similarity to `query`, returning the top `k` nodes
/// with their scores (descending), above a small relevance floor. A query of
/// the wrong length ranks nothing; use [`search_checked`] to be told why.
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

    /// A batch's pace for a test: `grace` seconds, and the real window and
    /// floor.
    fn paced(grace: u64) -> crate::net::Pace {
        crate::net::Pace {
            grace: std::time::Duration::from_secs(grace),
            ..BATCH_PACE
        }
    }

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
        let dir = crate::testutil::DataDir::new("embed-config");
        let _key = crate::testutil::EnvVars::new().set("OPENAI_API_KEY", "sk-env");
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
    }

    #[test]
    fn settings_preserve_a_custom_embedding_space_without_a_key() {
        let _data = crate::testutil::DataDir::new("embed-keyless-settings");
        let _key = crate::testutil::EnvVars::new().set("OPENAI_API_KEY", "fake-provider-key");
        Config::from_parts(
            String::new(),
            "custom-before".into(),
            "https://before.invalid/v1".into(),
        )
        .save()
        .unwrap();

        assert!(
            Config::load().is_none(),
            "a foreign endpoint needs its own key"
        );
        let snapshot = Config::current_or_default();
        assert!(
            snapshot.api_key.is_empty(),
            "the provider key must stay on its endpoint"
        );
        assert_eq!(snapshot.model, "custom-before");
        assert_eq!(snapshot.base_url, "https://before.invalid/v1");

        let edited = Config::from_parts(
            "fake-typed-key".into(),
            "custom-after".into(),
            "https://after.invalid/v1".into(),
        );
        assert!(edited.save_from(&snapshot).unwrap().is_empty());
        let saved = Config::load().unwrap();
        assert_eq!(saved.model, edited.model);
        assert_eq!(saved.base_url, edited.base_url);
        assert_eq!(saved.api_key, edited.api_key);
    }

    /// An index built in another embedding space must read as absent: reusing
    /// it would rank the query by cosine against vectors it shares no geometry
    /// with, silently and with no way to fix it from the UI.
    ///
    /// The project is spelled as clew spells one — resolved, as a project
    /// root is when it opens — whatever the temp dir's spelling: under one
    /// with a `..` in it (`TMPDIR=/work/../tmp`) every entry failed the
    /// lexical check `load` keeps entries by, and the index read as empty
    /// in its own space.
    #[test]
    fn index_from_another_model_or_endpoint_is_discarded() {
        let dir = crate::testutil::DataDir::new("embed-index");
        let _key = crate::testutil::EnvVars::new().remove("OPENAI_API_KEY");
        let resolved = dir.canonicalize().unwrap();
        let store = resolved.join("store");
        let root = resolved.join("proj");
        std::fs::create_dir_all(&store).unwrap();
        std::fs::create_dir_all(&root).unwrap();
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
    }

    /// The space is the model and the endpoint, and NOTHING else. A key is not
    /// part of it: reading the space through `Config::load` would make a
    /// cleared key read as a move to another space (it returns `None`) and a
    /// rotated key read as no change only by accident — and the callers that
    /// compare spaces across a config write answer "throw the index away" to
    /// every difference they see.
    #[test]
    fn the_stored_space_is_the_model_and_endpoint_and_ignores_the_key() {
        let dir = crate::testutil::DataDir::new("embed-space");
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
    }

    /// An endpoint that accepts and then says nothing must not own the calling
    /// thread for the life of the process. An agent with no read timeout (as
    /// ureq's default has none) waited on it for good, and every caller here
    /// is blocking.
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

        let url = format!("http://{addr}/embeddings");
        let (agent, _) =
            build_agent(&url, std::time::Duration::from_millis(300), paced(60)).unwrap();
        let cfg = Config {
            api_key: "k".into(),
            model: "m".into(),
            base_url: format!("http://{addr}"),
        };
        // Timed from the request: building the agent the first time builds
        // the process's trust too, which reads the system's store and is
        // slow on a busy machine.
        let started = std::time::Instant::now();
        let err = request_batch(&agent, None, &url, &cfg, &["x".to_string()])
            .expect_err("a silent endpoint is a failure, not an answer");
        let waited = started.elapsed();
        assert!(
            waited < std::time::Duration::from_secs(3),
            "gave up after {waited:?}, which is the timeout not firing ({err})"
        );
    }

    /// An embeddings answer is metered beneath ureq, as a model call's is:
    /// an answer whose last chunk is followed by trailer lines that never end
    /// — none of them body, so the body cap never sees them — fails at the
    /// meter, and the client hangs up. The connection used to come from
    /// ureq's pool, where no meter could be attached, and the answer grew the
    /// process's memory for as long as the peer kept sending it (in ureq 2,
    /// as one chunk-size line, which the input buffer now stops on its own).
    #[test]
    fn endless_framing_fails_at_the_meter() {
        use std::io::Write;
        const METER: u64 = 256 * 1024;
        // The peer's bound, far past the meter: a client still reading here
        // was never cut off (and a test without the meter still ends).
        const ENDLESS: u64 = 16 * 1024 * 1024;
        let (sent_tx, sent) = std::sync::mpsc::channel();
        let script: crate::testutil::TlsScript = Box::new(move |tls| {
            let _ = tls.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n");
            let trailers = b"X-Trailer: aaaaaaaaaaaaaaaaaaa\r\n".repeat(2048);
            let mut total = 0u64;
            while total < ENDLESS && tls.write_all(&trailers).and_then(|()| tls.flush()).is_ok() {
                total += trailers.len() as u64;
            }
            let _ = sent_tx.send(total);
        });
        let cfg = Config {
            api_key: "k".into(),
            model: "m".into(),
            base_url: crate::testutil::tls_serve(vec![script]),
        };
        // The embeddings profile, as `agent` builds it, with the test's trust
        // and a small meter.
        let agent = crate::llm::test_agent(
            crate::llm::Limits::direct(std::time::Duration::from_secs(20), paced(60)),
            crate::testutil::trusting_the_test_cert(),
            METER,
        );
        let err = request_batch(&agent, None, &endpoint(&cfg), &cfg, &["hello".to_string()])
            .expect_err("endless trailers are not a set of embeddings");
        assert!(
            err.contains("byte limit"),
            "not stopped by the meter: {err}"
        );
        let sent = sent
            .recv_timeout(std::time::Duration::from_secs(20))
            .expect("the peer stops once the client hangs up");
        assert!(
            sent < ENDLESS,
            "the client took {sent} bytes of trailers without hanging up"
        );
    }

    /// A plain-http embeddings endpoint whose proxy answers with a 407
    /// offering only logins clew cannot make says so — however right the
    /// password the setting names — and what can make them, as a model call
    /// does. Read as a plain request for a user name and password, it told
    /// the user to correct a password that was right.
    #[test]
    fn a_proxy_that_takes_only_logins_clew_cannot_make_says_so() {
        let (proxy_url, _heads) = crate::testutil::http_proxy(
            "127.0.0.1:0",
            false,
            1,
            crate::testutil::ProxyAnswer::Login("NTLM"),
        )
        .expect("a fake proxy");
        let spec = proxy_url.replacen("://", "://me:right@", 1);
        let cfg = Config {
            api_key: "k".into(),
            model: "m".into(),
            base_url: "http://embeddings.lan.invalid:8080/v1".into(),
        };
        let via = crate::net::Via {
            spec: spec.clone(),
            system: None,
            for_https: false,
        };
        let agent = crate::llm::test_agent_routed(
            crate::llm::Limits::direct(std::time::Duration::from_secs(20), paced(60)),
            crate::net::route(Some(&spec)).expect("a usable proxy setting"),
            crate::net::tls_config(),
            crate::llm::RAW_RESPONSE_CAP,
        );
        let err = request_batch(
            &agent,
            Some(&via),
            &endpoint(&cfg),
            &cfg,
            &["hello".to_string()],
        )
        .expect_err("a proxy that refuses every login yields no embeddings");
        assert!(
            err.contains("it takes only NTLM logins, which clew cannot make")
                && err.contains("cntlm or px"),
            "{err}"
        );
    }

    /// fixR5 #6: embeddings keep their connection for the next batch, and
    /// its meter counts one answer at a time — so batch after batch on one
    /// kept connection goes through, however far past the meter their sum
    /// runs. Counted over the connection's life, the meter would fail a
    /// healthy kept connection in the end (which is why R4 kept none).
    ///
    /// fixR7 #6: each batch runs on a worker whose line is closed as it
    /// ends — every batch's abandon comes after its request is over — and
    /// the connection is kept all the same: the line held it for that
    /// request's term alone (`net::Lease`), not for the next batch's.
    #[test]
    fn a_kept_connection_carries_batch_after_batch() {
        use std::io::Write;
        const METER: u64 = 4 * 1024;
        const BATCHES: usize = 4;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let (served_tx, served) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let Ok((mut conn, _)) = listener.accept() else {
                return;
            };
            // An answer of some 3 KiB: under the meter alone, over it with
            // the one before.
            let vector = vec!["0.125"; 500].join(",");
            let body = format!(r#"{{"data":[{{"index":0,"embedding":[{vector}]}}]}}"#);
            let mut answered = 0;
            for _ in 0..BATCHES {
                if crate::testutil::read_http(&mut conn).head.is_empty() {
                    break;
                }
                let _ = write!(
                    conn,
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{body}",
                    body.len()
                );
                answered += 1;
            }
            let _ = served_tx.send(answered);
        });
        let cfg = Config {
            api_key: "k".into(),
            model: "m".into(),
            base_url,
        };
        let agent = crate::llm::test_agent(
            crate::llm::Limits::direct(std::time::Duration::from_secs(20), paced(60)),
            crate::testutil::trusting_the_test_cert(),
            METER,
        );
        for batch in 0..BATCHES {
            let texts = ["x".to_string()];
            let vectors = run_batch(agent.clone(), None, endpoint(&cfg), &cfg, &texts, &|| false)
                .unwrap_or_else(|e| panic!("batch {batch}: {e}"));
            assert_eq!(vectors[0].len(), 500);
        }
        assert_eq!(
            served.recv_timeout(std::time::Duration::from_secs(10)),
            Ok(BATCHES),
            "every batch came on the one kept connection"
        );
    }

    /// fixR7 #6, paced since: an embeddings batch is bounded as a whole, by
    /// the pace its answer must keep ([`BATCH_PACE`]). A peer that trickles
    /// its answer a byte at a time never trips the idle limit, and held the
    /// caller — a thread and a connection — for as long as it went on: here
    /// a minute, against a grace and windows of one second. An answer that
    /// comes in steadily, if slowly, takes the time it needs: here two
    /// seconds, past that grace, which a fixed deadline of one second failed
    /// — as three minutes failed every large batch on a link under half a
    /// megabit.
    #[test]
    fn a_slow_answer_completes_and_a_trickled_one_is_given_up() {
        use std::io::Write;
        use std::time::{Duration, Instant};
        const PACE: crate::net::Pace = crate::net::Pace {
            grace: Duration::from_secs(1),
            window: Duration::from_secs(1),
            floor: 4096,
        };
        /// An endpoint answering its one request with `head`, then `body` in
        /// `pieces` every `every`, or until the client hangs up.
        fn serving(head: String, body: Vec<u8>, pieces: usize, every: Duration) -> Config {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let base_url = format!("http://{}", listener.local_addr().unwrap());
            std::thread::spawn(move || {
                let Ok((mut conn, _)) = listener.accept() else {
                    return;
                };
                crate::testutil::read_http(&mut conn);
                let _ = conn.write_all(head.as_bytes());
                for piece in body.chunks(pieces) {
                    if conn.write_all(piece).is_err() {
                        return;
                    }
                    std::thread::sleep(every);
                }
            });
            Config {
                api_key: "k".into(),
                model: "m".into(),
                base_url,
            }
        }
        let batch = |cfg: &Config| {
            let url = endpoint(cfg);
            let (agent, _) = build_agent(&url, Duration::from_secs(10), PACE).unwrap();
            let began = Instant::now();
            let result = run_batch(agent, None, url, cfg, &["x".to_string()], &|| false);
            (result, began.elapsed())
        };

        // Steady: a 40 KB answer, 2 KB every 100 ms — twenty a second.
        let values = vec!["0.5"; 10_000].join(",");
        let body = format!(r#"{{"data":[{{"index":0,"embedding":[{values}]}}]}}"#);
        let head = format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n", body.len());
        let steady = serving(head, body.into_bytes(), 2048, Duration::from_millis(100));
        let (result, took) = batch(&steady);
        let vectors = result.expect("a steady answer was given up");
        assert_eq!(vectors[0].len(), 10_000);
        assert!(took > PACE.grace, "not past the grace: {took:?}");

        // Trickled: a byte every 50 ms, for a minute.
        let head = "HTTP/1.1 200 OK\r\ncontent-length: 100000\r\n\r\n".to_string();
        let trickled = serving(head, vec![b' '; 1200], 1, Duration::from_millis(50));
        let (result, took) = batch(&trickled);
        let err = result.expect_err("a trickled answer outlasted its pace");
        assert!(err.contains("timed out"), "{err}");
        assert!(err.contains("slower than 4 KiB/s"), "{err}");
        assert!(took < Duration::from_secs(5), "the pace held for {took:?}");
    }

    /// fixR7 #6: a batch can be stopped, and the stop reaches its connection
    /// — a kept one included, which is on the line of the batch using it for
    /// as long as that batch is. The endpoint sees the connection close at
    /// once, and the caller is let go at once. A kept connection used to be
    /// on no line: a stopped batch held it, and its thread, until the
    /// endpoint answered or the idle limit ran out.
    #[test]
    fn a_stopped_batch_closes_its_kept_connection() {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let (arrived_tx, arrived) = std::sync::mpsc::channel();
        let (closed_tx, closed) = std::sync::mpsc::channel();
        // One connection: the first batch answered, the second taken in and
        // never answered.
        std::thread::spawn(move || {
            let Ok((mut conn, _)) = listener.accept() else {
                return;
            };
            crate::testutil::read_http(&mut conn);
            let body = r#"{"data":[{"index":0,"embedding":[0.5]}]}"#;
            let _ = write!(
                conn,
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{body}",
                body.len()
            );
            if crate::testutil::read_http(&mut conn).head.is_empty() {
                return;
            }
            let _ = arrived_tx.send(());
            let _ = conn.set_read_timeout(Some(std::time::Duration::from_secs(30)));
            let _ = std::io::Read::read(&mut conn, &mut [0u8; 1]);
            let _ = closed_tx.send(std::time::Instant::now());
        });
        let cfg = Config {
            api_key: "k".into(),
            model: "m".into(),
            base_url,
        };
        let agent = crate::llm::test_agent(
            crate::llm::Limits::direct(std::time::Duration::from_secs(20), paced(60)),
            crate::testutil::trusting_the_test_cert(),
            crate::llm::RAW_RESPONSE_CAP,
        );
        let texts = ["x".to_string()];
        let first = run_batch(agent.clone(), None, endpoint(&cfg), &cfg, &texts, &|| false);
        assert_eq!(first, Ok(vec![vec![0.5]]));
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = stop.clone();
        std::thread::spawn(move || {
            if arrived
                .recv_timeout(std::time::Duration::from_secs(10))
                .is_ok()
            {
                flag.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        });
        let err = run_batch(agent, None, endpoint(&cfg), &cfg, &texts, &|| {
            stop.load(std::sync::atomic::Ordering::Relaxed)
        })
        .unwrap_err();
        let stopped = std::time::Instant::now();
        assert_eq!(err, crate::llm::CANCELLED);
        let at = closed
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the stop did not reach the kept connection");
        assert!(
            at.saturating_duration_since(stopped) < std::time::Duration::from_secs(2),
            "the connection closed {:?} after the stop",
            at.saturating_duration_since(stopped)
        );
    }

    /// The batch must go where it was addressed. Proven through `embed_batch`
    /// rather than through `agent()` directly, because the defect being kept
    /// out is `embed_batch` reaching for `ureq::post` and its implicit agent.
    ///
    /// Deterministic: the endpoint reads the WHOLE request before it answers
    /// and closes only after the client has. It used to answer after one
    /// `read` and drop the socket, which under load reset the connection while
    /// ureq 2 was still handling the answer — and panicked it about one run in
    /// ten (see `a_peer_that_resets_the_connection_is_an_error_not_a_panic`).
    #[test]
    fn a_redirect_is_refused_rather_than_followed() {
        use std::io::Write;
        // Where the redirect points. Answers with a perfectly valid embedding,
        // so a followed hop would look like success — and reports the visit.
        let sink = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let sink_addr = sink.local_addr().unwrap();
        let (visited_tx, visited) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            for conn in sink.incoming() {
                let Ok(mut conn) = conn else { break };
                let _ = visited_tx.send(());
                crate::testutil::read_http_request(&mut conn);
                let body = r#"{"data":[{"index":0,"embedding":[1.0]}]}"#;
                let _ = conn.write_all(
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
        let endpoint = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            crate::testutil::read_http_request(&mut conn);
            conn.write_all(
                format!(
                    "HTTP/1.1 302 Found\r\nlocation: http://{sink_addr}/embeddings\r\n\
                     connection: close\r\ncontent-length: 0\r\n\r\n"
                )
                .as_bytes(),
            )
            .unwrap();
            // Close only once the client has: its EOF first, then ours — a
            // plain FIN, never a reset under a response still being handled.
            conn.set_read_timeout(Some(std::time::Duration::from_secs(20)))
                .unwrap();
            let _ = std::io::Read::read(&mut conn, &mut [0u8; 64]);
        });

        let cfg = Config {
            api_key: "SECRET-KEY".into(),
            model: "m".into(),
            base_url: format!("http://{addr}"),
        };
        let err = embed_batch(&cfg, &["hello".to_string()])
            .expect_err("a 3xx is not a set of embeddings");
        assert!(err.contains("redirected"), "the refusal is reported: {err}");
        endpoint.join().unwrap();
        assert!(
            visited.try_recv().is_err(),
            "the batch never reached the redirect's target"
        );
    }

    /// A peer that resets the connection right after an empty answer used to
    /// PANIC the thread that asked, inside ureq 2: it handed such an answer's
    /// connection back by clearing the socket's timeouts under an `expect`,
    /// and macOS refuses that `setsockopt` once a reset has shut the
    /// connection down. Here the answer is one a kept connection could carry
    /// on after (no `connection: close`), so the connection is probed for
    /// reuse whichever side of the reset the probe lands on: the result is
    /// the redirect's refusal, an error — never a panic
    /// (`net::tests::a_reset_connection_is_an_error_and_not_kept` pins the
    /// probe itself).
    #[test]
    fn a_peer_that_resets_the_connection_is_an_error_not_a_panic() {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let peer = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            crate::testutil::read_http_request(&mut conn);
            conn.write_all(
                b"HTTP/1.1 302 Found\r\nlocation: http://127.0.0.1:9/\r\ncontent-length: 0\r\n\r\n",
            )
            .unwrap();
            crate::testutil::abort(conn);
        });
        let cfg = Config {
            api_key: "k".into(),
            model: "m".into(),
            base_url: format!("http://{addr}"),
        };
        let result = std::panic::catch_unwind(|| embed_batch(&cfg, &["hello".to_string()]));
        peer.join().unwrap();
        let err = result
            .expect("a reset connection panicked the thread that asked")
            .expect_err("a reset connection is not a set of embeddings");
        // The redirect's refusal — or, should the reset overtake the answer
        // on its way in, the reset itself.
        assert!(err.contains("redirected") || err.contains("reset"), "{err}");
    }

    /// A build covers the nodes the BUILDING window's explanation cache holds,
    /// and the derived store is shared by every window on the project — so
    /// writing a build wholesale shrank a whole-project index down to one
    /// window's subset, and FIND then answered from that subset silently.
    #[test]
    fn a_partial_build_tops_the_stored_index_up_instead_of_replacing_it() {
        // `save` stamps the index with the space its OWN vectors came from
        // (its `model` and `base_url`), and `load` keeps a stored index only
        // when the live config names that same space — so the config below
        // names the space the indexes are built in.
        let dir = crate::testutil::DataDir::new("embed-merge");
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
    }
}

#[cfg(test)]
mod integrity_tests {
    use super::*;
    use std::path::PathBuf;

    fn entry(name: &str, vec: Vec<f32>) -> Entry {
        Entry {
            node: Node::Function {
                file: PathBuf::from(format!("/p/{name}.rs")),
                name: name.into(),
                ordinal: 0,
            },
            hash: 0,
            vec,
        }
    }

    fn index(model: &str, base_url: &str, entries: Vec<Entry>) -> Index {
        Index {
            model: model.into(),
            base_url: base_url.into(),
            entries,
        }
    }

    /// A private data dir with `config.toml` naming `model`: `CLEW_DATA_DIR`
    /// (under the env lock) for as long as the guard lives.
    fn with_config(tag: &str, model: &str) -> crate::testutil::DataDir {
        let data = crate::testutil::DataDir::new(&format!("embed-integrity-{tag}"));
        std::fs::write(
            data.join("config.toml"),
            format!("[embedding]\napi_key = \"k\"\nmodel = \"{model}\"\n"),
        )
        .unwrap();
        data
    }

    /// The file is stamped with the space its VECTORS came from, whatever the
    /// live config says at write time — so a config change that lands while
    /// a build runs cannot relabel old-space vectors as new-space ones.
    #[test]
    fn save_stamps_the_index_own_space_not_the_live_config() {
        let dir = with_config("stamp", "m-live");
        let root = PathBuf::from("/p");
        let built = index(
            "m-built",
            DEFAULT_BASE_URL,
            vec![entry("f", vec![1.0, 0.0])],
        );
        save(&dir, &built).unwrap();
        // The live config names another space: the file is foreign to it.
        assert!(load(&dir, &root).entries.is_empty());
        // Loaded in its own space it is all there.
        let own = Config::from_parts("k".into(), "m-built".into(), DEFAULT_BASE_URL.into());
        assert_eq!(load_for(&dir, &root, Some(&own)).entries.len(), 1);
    }

    /// A merge reads the stored index in the BUILD's space. A stored index
    /// from another space is dropped, never mixed in, and the file names the
    /// build's space.
    #[test]
    fn merge_judges_the_stored_index_by_the_build_space() {
        let dir = with_config("merge-space", "m-a");
        let root = PathBuf::from("/p");
        save(
            &dir,
            &index("m-a", DEFAULT_BASE_URL, vec![entry("old", vec![1.0])]),
        )
        .unwrap();
        let built = index("m-b", DEFAULT_BASE_URL, vec![entry("new", vec![2.0])]);
        let (merged, saved) = merge_built(&dir, &root, &built);
        saved.unwrap();
        assert_eq!(
            merged.entries.len(),
            1,
            "the m-a vector was not mixed into m-b"
        );
        assert_eq!(merged.model, "m-b");
        let b = Config::from_parts("k".into(), "m-b".into(), DEFAULT_BASE_URL.into());
        assert_eq!(load_for(&dir, &root, Some(&b)).entries.len(), 1);
    }

    /// The file never grows past what `load` reads back: entries are left out
    /// of it from the END of the list, and the list itself is not touched.
    #[test]
    fn an_over_cap_index_file_is_cut_from_the_end() {
        let dir = with_config("cap", "m");
        let root = PathBuf::from("/p");
        let many: Vec<Entry> = (0..200)
            .map(|i| entry(&format!("e{i}"), vec![0.5; 64]))
            .collect();
        let full = index("m", DEFAULT_BASE_URL, many);
        let one = serde_json::to_vec(&full.entries[0]).unwrap().len() as u64;
        let cap = one * 50;
        let left_out = save_capped(&dir, &full, cap).unwrap();
        assert!(
            left_out > 100,
            "only {left_out} left out under a 50-entry cap"
        );
        let bytes = std::fs::metadata(index_path(&dir)).unwrap().len();
        assert!(bytes <= cap, "{bytes} bytes written under a {cap}-byte cap");
        let back = load(&dir, &root);
        assert_eq!(back.entries.len(), 200 - left_out);
        // Left out from the END: the file holds a prefix of the list.
        for (written, listed) in back.entries.iter().zip(&full.entries) {
            assert_eq!(written.node, listed.node);
        }
    }

    /// Serialized size of one [`entry`]-shaped entry with a `dims`-long vector,
    /// for sizing a cap in entries.
    fn entry_bytes(dims: usize) -> u64 {
        serde_json::to_vec(&entry("e0000", vec![0.25; dims]))
            .unwrap()
            .len() as u64
    }

    /// What the app's next build would have to send to the endpoint: the
    /// nodes it covers that the index it adopted has no vector for (the reuse
    /// rule of the builder, which keys on node and summary hash).
    fn to_embed(adopted: &Index, next_build: &[Node]) -> usize {
        let have: std::collections::HashSet<&Node> =
            adopted.entries.iter().map(|e| &e.node).collect();
        next_build.iter().filter(|n| !have.contains(n)).count()
    }

    /// Past the file cap, the index the caller adopts is still the WHOLE
    /// merge. It used to be cut to what the file held, so the next build found
    /// every entry past the cap missing, re-embedded them all (about a hundred
    /// requests per auto-refresh on a big project), and the save cut them
    /// again — on every build, forever.
    #[test]
    fn an_over_cap_merge_keeps_every_entry_and_re_embeds_nothing() {
        let dir = with_config("cap-merge", "m");
        let root = PathBuf::from("/p");
        const N: usize = 120;
        let built = index(
            "m",
            DEFAULT_BASE_URL,
            (0..N)
                .map(|i| entry(&format!("e{i:04}"), vec![0.25; 16]))
                .collect(),
        );
        let nodes: Vec<Node> = built.entries.iter().map(|e| e.node.clone()).collect();
        let cap = entry_bytes(16) * 40;

        let (adopted, saved) = merge_capped(&dir, &root, &built, cap);
        let left_out = saved.unwrap();
        assert!(
            left_out > 0,
            "the cap must bite for this test to mean anything"
        );
        assert_eq!(adopted.entries.len(), N, "the adopted index was cut");
        assert_eq!(
            load(&dir, &root).entries.len(),
            N - left_out,
            "the file holds what fits"
        );
        // The next build, from the adopted index, embeds nothing again.
        assert_eq!(to_embed(&adopted, &nodes), 0);

        // And merging that next build changes nothing: same index, same file.
        let first_file = std::fs::read(index_path(&dir)).unwrap();
        let (again, saved) = merge_capped(&dir, &root, &adopted, cap);
        assert_eq!(saved.unwrap(), left_out);
        assert_eq!(again.entries.len(), N);
        assert_eq!(std::fs::read(index_path(&dir)).unwrap(), first_file);

        // A new session loads the capped file: only what the file could not
        // hold is embedded again, once.
        let reloaded = load(&dir, &root);
        assert_eq!(to_embed(&reloaded, &nodes), left_out);
    }

    /// One order, whatever order the build listed its entries in (it comes
    /// out of a hash map): the same file bytes, and the same entries left out
    /// of it. The build's own entries come first, so an over-cap cut drops
    /// the entries only the stored index had before any of the build's.
    #[test]
    fn a_merge_is_deterministic_and_leaves_out_uncovered_entries_first() {
        let dir = with_config("cap-order", "m");
        let root = PathBuf::from("/p");
        let stored_only: Vec<Entry> = (0..30)
            .map(|i| entry(&format!("s{i:04}"), vec![0.5; 16]))
            .collect();
        let fresh: Vec<Entry> = (0..30)
            .map(|i| entry(&format!("f{i:04}"), vec![0.5; 16]))
            .collect();
        let cap = entry_bytes(16) * 40;
        let mut files = Vec::new();
        let mut orders = Vec::new();
        for reversed in [false, true] {
            let mut seed = stored_only.clone();
            let mut build = fresh.clone();
            if reversed {
                seed.reverse();
                build.reverse();
            }
            save(&dir, &index("m", DEFAULT_BASE_URL, seed)).unwrap();
            let (merged, saved) =
                merge_capped(&dir, &root, &index("m", DEFAULT_BASE_URL, build), cap);
            let left_out = saved.unwrap();
            assert!(left_out > 0 && left_out < 30, "{left_out}");
            files.push(std::fs::read(index_path(&dir)).unwrap());
            orders.push(
                merged
                    .entries
                    .iter()
                    .map(|e| e.node.clone())
                    .collect::<Vec<_>>(),
            );
            // Every fresh entry made it to the file; only stored-only ones
            // were left out.
            let on_disk: std::collections::HashSet<Node> = load(&dir, &root)
                .entries
                .into_iter()
                .map(|e| e.node)
                .collect();
            assert!(fresh.iter().all(|e| on_disk.contains(&e.node)));
        }
        assert_eq!(files[0], files[1], "the file depends on input order");
        assert_eq!(orders[0], orders[1], "the index depends on input order");
        // Fresh first, each part in node order.
        let mut expected: Vec<Node> = fresh.iter().map(|e| e.node.clone()).collect();
        expected.sort();
        let mut rest: Vec<Node> = stored_only.iter().map(|e| e.node.clone()).collect();
        rest.sort();
        expected.extend(rest);
        assert_eq!(orders[0], expected);
    }

    /// A stored index that exists but is refused says why; one that is merely
    /// absent, or from another embedding space, is not a problem.
    #[test]
    fn a_refused_stored_index_is_reported_by_load_checked() {
        let dir = with_config("checked", "m");
        let root = PathBuf::from("/p");
        assert_eq!(load_checked(&dir, &root).1, None, "absent");

        std::fs::write(index_path(&dir), b"{ not json").unwrap();
        let (loaded, problem) = load_checked(&dir, &root);
        assert!(loaded.entries.is_empty());
        let problem = problem.expect("an unparseable file is a problem");
        assert!(problem.contains("unreadable"), "{problem}");

        save(
            &dir,
            &index("other-model", DEFAULT_BASE_URL, vec![entry("f", vec![1.0])]),
        )
        .unwrap();
        let (loaded, problem) = load_checked(&dir, &root);
        assert!(loaded.entries.is_empty(), "foreign vectors are not loaded");
        assert_eq!(problem, None, "a foreign space is not a problem");

        save(
            &dir,
            &index("m", DEFAULT_BASE_URL, vec![entry("f", vec![1.0])]),
        )
        .unwrap();
        let (loaded, problem) = load_checked(&dir, &root);
        assert_eq!(problem, None);
        assert_eq!(
            loaded.entries.len(),
            1,
            "its own space loads as `load` does"
        );
        assert_eq!(loaded.entries[0].node, load(&dir, &root).entries[0].node);
    }

    /// Fresh entries come first in a merge, so a cut drops stored-only ones.
    #[test]
    fn a_merge_puts_fresh_entries_first() {
        let dir = with_config("order", "m");
        let root = PathBuf::from("/p");
        save(
            &dir,
            &index("m", DEFAULT_BASE_URL, vec![entry("stored", vec![1.0])]),
        )
        .unwrap();
        let built = index("m", DEFAULT_BASE_URL, vec![entry("fresh", vec![2.0])]);
        let (merged, saved) = merge_built(&dir, &root, &built);
        saved.unwrap();
        let names: Vec<&str> = merged
            .entries
            .iter()
            .map(|e| match &e.node {
                Node::Function { name, .. } => name.as_str(),
                _ => "",
            })
            .collect();
        assert_eq!(names, vec!["fresh", "stored"]);
    }

    /// Without the file lock nothing is written: the merge used to carry on
    /// unlocked, which is the lost update the lock exists to prevent. The
    /// merged index is still computed from a fresh read and handed back — its
    /// vectors were paid for — and the `Err` says why it was not saved.
    #[test]
    fn a_merge_writes_nothing_when_the_lock_cannot_be_taken() {
        let data = crate::testutil::DataDir::new("embed-lock");
        std::fs::write(
            data.join("config.toml"),
            "[embedding]\napi_key = \"k\"\nmodel = \"m\"\n",
        )
        .unwrap();
        let root = PathBuf::from("/p");
        save(
            &data,
            &index("m", DEFAULT_BASE_URL, vec![entry("stored", vec![1.0])]),
        )
        .unwrap();
        let before = std::fs::read(index_path(&data)).unwrap();
        // Something that is not a plain file squats on the lock's name.
        std::fs::create_dir(data.join(".embeddings.json.lock")).unwrap();

        let built = index("m", DEFAULT_BASE_URL, vec![entry("fresh", vec![2.0])]);
        let (merged, saved) = merge_built(&data, &root, &built);

        let err = saved.expect_err("no write without the lock");
        assert!(
            err.to_string().contains("could not lock embeddings.json"),
            "{err}"
        );
        assert_eq!(
            merged.entries.len(),
            2,
            "the fresh read merged with the build is handed back"
        );
        assert_eq!(
            std::fs::read(index_path(&data)).unwrap(),
            before,
            "the file is left as it was"
        );
    }

    /// An index file too big to read is reported, not silently treated as no
    /// index at all.
    #[test]
    fn an_unreadable_stored_index_is_reported() {
        let dir = with_config("oversize", "m");
        let f = std::fs::File::create(index_path(&dir)).unwrap();
        // Sparse: no bytes are written.
        f.set_len(crate::statefile::MAX_STATE_BYTES + 1).unwrap();
        let problem = stored_index_problem(&dir).expect("an over-cap file is a problem");
        assert!(problem.contains("read cap"), "{problem}");
        assert!(load(&dir, Path::new("/p")).entries.is_empty());
        std::fs::remove_file(index_path(&dir)).unwrap();
        assert_eq!(stored_index_problem(&dir), None, "absent is not a problem");
    }

    /// Mixed vector lengths never survive a load or a merge.
    #[test]
    fn indexes_are_kept_to_one_vector_length() {
        let dir = with_config("dims", "m");
        let root = PathBuf::from("/p");
        let mixed = index(
            "m",
            DEFAULT_BASE_URL,
            vec![
                entry("a", vec![1.0; 3]),
                entry("b", vec![1.0; 3]),
                entry("c", vec![1.0; 5]),
            ],
        );
        save(&dir, &mixed).unwrap();
        let back = load(&dir, &root);
        assert_eq!(back.entries.len(), 2, "the minority length is dropped");
        assert_eq!(back.dims(), Some(3));

        // A build at a new length replaces the old vectors instead of mixing.
        let built = index("m", DEFAULT_BASE_URL, vec![entry("z", vec![1.0; 7])]);
        let (merged, saved) = merge_built(&dir, &root, &built);
        saved.unwrap();
        assert_eq!(merged.entries.len(), 1);
        assert_eq!(merged.dims(), Some(7));
    }

    #[test]
    fn a_query_of_the_wrong_length_is_an_error_not_an_empty_result() {
        let idx = index("m", DEFAULT_BASE_URL, vec![entry("a", vec![1.0, 0.0, 0.0])]);
        let err = search_checked(&idx, &[1.0, 0.0], 5).expect_err("2 vs 3 dimensions");
        assert!(
            err.contains("3-dimensional") && err.contains("rebuild"),
            "{err}"
        );
        assert_eq!(search_checked(&idx, &[1.0, 0.0, 0.0], 5).unwrap().len(), 1);
        assert!(
            search_checked(&Index::default(), &[1.0], 5)
                .unwrap()
                .is_empty()
        );
    }

    /// A response is accepted only when it is exactly one vector of numbers
    /// per input, all of one length.
    #[test]
    fn embeddings_responses_are_validated() {
        let ok = r#"{"data":[{"index":1,"embedding":[3,4]},{"index":0,"embedding":[1,2]}]}"#;
        assert_eq!(
            parse_embeddings(ok, 2).unwrap(),
            vec![vec![1.0, 2.0], vec![3.0, 4.0]]
        );
        for (bad, expected, why) in [
            (
                r#"{"data":[{"index":0,"embedding":[1,"x"]}]}"#,
                1,
                "non-number",
            ),
            (
                r#"{"data":[{"index":0,"embedding":[1,2]},{"index":1,"embedding":[1]}]}"#,
                2,
                "different lengths",
            ),
            (
                r#"{"data":[{"index":0,"embedding":[1]},{"index":0,"embedding":[2]}]}"#,
                2,
                "twice",
            ),
            (
                r#"{"data":[{"index":5,"embedding":[1]}]}"#,
                1,
                "out of range",
            ),
            (r#"{"data":[{"index":0,"embedding":[]}]}"#, 1, "empty"),
            (r#"{"data":[]}"#, 1, "expected 1"),
        ] {
            let err = parse_embeddings(bad, expected).expect_err(why);
            assert!(err.contains(why), "{why}: {err}");
        }
        // The body cap is enforced while reading.
        let err = read_body(std::io::Cursor::new(vec![b'x'; 2048]), 1024).expect_err("over cap");
        assert!(err.contains("exceeded"), "{err}");
        assert_eq!(
            read_body(std::io::Cursor::new(b"{}".to_vec()), 1024).unwrap(),
            "{}"
        );
    }

    /// `dimensions` goes only to models documented to take it. Checked on
    /// the wire, against a local endpoint that records each request body.
    #[test]
    fn dimensions_is_sent_only_to_models_that_support_it() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        std::thread::spawn(move || {
            for conn in listener.incoming().take(2) {
                let Ok(mut conn) = conn else { break };
                // The whole request, by its length and bounded (see
                // `testutil`): its body is what the test checks.
                let _ = tx.send(crate::testutil::read_http_request(&mut conn));
                let body = r#"{"data":[{"index":0,"embedding":[1.0,0.0]}]}"#;
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
        let cfg = |model: &str| Config {
            api_key: "k".into(),
            model: model.into(),
            base_url: format!("http://{addr}"),
        };
        embed_batch(&cfg("text-embedding-3-small"), &["a".to_string()]).unwrap();
        let v3 = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert!(v3.contains("\"dimensions\":512"), "{v3}");
        embed_batch(&cfg("nomic-embed-text"), &["a".to_string()]).unwrap();
        let other = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert!(!other.contains("dimensions"), "{other}");
        assert!(supports_dimensions("openai/text-embedding-3-large"));
        assert!(!supports_dimensions("text-embedding-ada-002"));
    }
}
