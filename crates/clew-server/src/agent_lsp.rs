//! Language-server access for the Ask agent's semantic tools.
//!
//! The agent runs on the server, so it owns its language-server instances,
//! resolved exactly like `SpawnLsp` resolves them (project `.clew/lsp.toml`
//! over the built-in registry). A server starts lazily on the first semantic
//! tool call for its language and is reused across turns. Nothing is
//! auto-installed here: provisioning stays a user-consented action in the
//! client, and an uninstalled server surfaces as a tool result the model can
//! react to (fall back to `search`, tell the user).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clew_core::highlight;
use clew_core::incremental::content_hash;
use clew_core::lsp::client::{LspClient, PositionEncoding, Target};
use clew_core::lsp::{config, store};
use tokio::sync::Mutex;

/// How long to give a freshly-started server's initial indexing before
/// answering queries anyway. Results while indexing carry a retry note.
const INDEX_WAIT: Duration = Duration::from_secs(20);
/// Time for indexing progress to first appear after the handshake.
const INDEX_GRACE: Duration = Duration::from_millis(1500);
/// Per-request timeout — a hung server must not hang the whole agent turn.
const CALL_TIMEOUT: Duration = Duration::from_secs(15);
/// Most targets a result lists, mirroring the `search` tool's cap.
const MAX_TARGETS: usize = 40;
/// Cooldown before a failed server start is attempted again. Failures like
/// "not installed" heal (the user consents to the install in the client), so
/// they must not be cached for the pool's lifetime.
const FAIL_RETRY: Duration = Duration::from_secs(60);

/// Lazily-started language servers for one project, keyed by language.
pub struct LspPool {
    /// Root as the rest of the server knows it — identity for pool reuse.
    root: PathBuf,
    /// Canonical root: external processes (cargo, the language servers)
    /// report paths in canonical form, so URIs we send and prefixes we strip
    /// must use it or a symlinked root (e.g. `/tmp` on macOS) breaks both.
    canon: PathBuf,
    /// One slot per language, each with its own lock, so a slow first start
    /// of one language never blocks queries on another.
    slots: Mutex<HashMap<String, Arc<LangSlot>>>,
}

struct LangSlot {
    state: Mutex<LangState>,
}

enum LangState {
    Unstarted,
    Ready(Entry),
    /// Startup failed; cached with a cooldown (see [`FAIL_RETRY`]).
    Failed {
        error: String,
        at: Instant,
    },
}

struct Entry {
    client: LspClient,
    /// When the server was started. Readiness cannot rely on progress
    /// reporting alone (there are busy-but-silent phases, e.g. while `cargo
    /// metadata` blocks on a lock), so empty results from a freshly-started
    /// server are retried for a while after this instant.
    started: Instant,
    /// `didOpen`'d documents: content hash + version, so an on-disk edit
    /// re-syncs via `didChange` instead of leaving a stale overlay the
    /// server would silently answer against.
    docs: HashMap<PathBuf, DocState>,
}

struct DocState {
    hash: u64,
    version: i64,
}

/// Which semantic query to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Semantic {
    Definition,
    References,
    Hover,
}

/// A formatted query result: the text for the model, plus project-relative
/// `(rel, 1-based line)` targets for the client's step chips.
pub struct SemanticResult {
    pub content: String,
    pub targets: Vec<(String, usize)>,
    /// The result may legitimately change on a later retry (the server was
    /// still indexing). Callers must not dedup-block an identical retry.
    pub transient: bool,
}

impl LspPool {
    pub fn new(root: PathBuf) -> Self {
        let canon = root.canonicalize().unwrap_or_else(|_| root.clone());
        LspPool {
            root,
            canon,
            slots: Mutex::new(HashMap::new()),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Run one semantic query against the file's language server. `line1` is
    /// 1-based (the numbering every other agent tool uses); `symbol` is the
    /// identifier text to anchor on within that line.
    pub async fn query(
        &self,
        kind: Semantic,
        rel: &str,
        abs: &Path,
        line1: usize,
        symbol: &str,
        stop: &std::sync::atomic::AtomicBool,
    ) -> Result<SemanticResult, String> {
        // Canonical from here on: the URI we open must be the path the
        // server's own project model (e.g. cargo metadata) uses.
        let abs = abs.canonicalize().unwrap_or_else(|_| abs.to_path_buf());
        let Some(language) = highlight::detect(&abs) else {
            return Err(format!("no language server support for {rel}"));
        };
        let source = std::fs::read_to_string(&abs).map_err(|_| format!("cannot read {rel}"))?;
        let Some(line_text) = source.lines().nth(line1.saturating_sub(1)) else {
            return Err(format!(
                "{rel} has only {} lines (asked for line {line1})",
                source.lines().count()
            ));
        };
        let Some(byte_col) = symbol_byte_col(line_text, symbol) else {
            return Err(format!(
                "`{symbol}` does not appear on line {line1} of {rel} — \
                 pass the line the symbol is on, as shown by read/search/outline"
            ));
        };

        let (client, started) = self.client_for(language, &abs, &source).await?;
        let character = match client.encoding {
            PositionEncoding::Utf8 => byte_col,
            PositionEncoding::Utf16 => line_text[..byte_col].encode_utf16().count(),
        };
        let line0 = line1 - 1;

        // An empty result — or an outright request error — from a busy server
        // is usually transient: the symbol isn't in its index yet, or the
        // server rejects requests while loading the workspace. "Busy" is
        // reported progress OR a freshly-started server (there are
        // busy-but-silent phases, e.g. `cargo metadata` blocked on a lock).
        // Re-query until the result lands, the server settles, or the bounded
        // window closes.
        let waited = Instant::now();
        let outcome = loop {
            let result = run_query(&client, kind, &abs, line0, character).await;
            let stopped = stop.load(std::sync::atomic::Ordering::Relaxed);
            let busy = client.progress().is_some() || started.elapsed() < INDEX_WAIT;
            let retryable = busy && waited.elapsed() < INDEX_WAIT && !stopped;
            match result {
                Ok(out) => {
                    let empty = match &out {
                        HoverOrTargets::Hover(text) => text.is_none(),
                        HoverOrTargets::Targets(t) => t.is_empty(),
                    };
                    if !empty || !retryable {
                        break out;
                    }
                }
                Err(e) if !retryable => return Err(e),
                Err(_) => {}
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        };

        let mut result = match outcome {
            HoverOrTargets::Hover(Some(text)) => SemanticResult {
                content: text,
                targets: Vec::new(),
                transient: false,
            },
            HoverOrTargets::Hover(None) => SemanticResult {
                content: format!("no hover info for `{symbol}`"),
                targets: Vec::new(),
                transient: false,
            },
            HoverOrTargets::Targets(targets) => self.format_targets(&targets, symbol, kind),
        };
        if client.progress().is_some() {
            result.content.push_str(
                "\n(note: the language server is still indexing — an empty result may \
                 be a false negative; retry this call in a later step)",
            );
            result.transient = true;
        }
        Ok(result)
    }

    /// Get or start the server for `language`, and sync `abs` onto it with
    /// `source` (the text the query positions were computed against).
    async fn client_for(
        &self,
        language: &str,
        abs: &Path,
        source: &str,
    ) -> Result<(LspClient, Instant), String> {
        // The pool lock is held only to fetch the per-language slot; slot
        // work (startup included) proceeds under the slot's own lock.
        let slot = {
            let mut slots = self.slots.lock().await;
            slots
                .entry(language.to_string())
                .or_insert_with(|| {
                    Arc::new(LangSlot {
                        state: Mutex::new(LangState::Unstarted),
                    })
                })
                .clone()
        };
        let mut state = slot.state.lock().await;
        if let LangState::Failed { error, at } = &*state {
            if at.elapsed() < FAIL_RETRY {
                return Err(error.clone());
            }
            *state = LangState::Unstarted;
        }
        // A server that died after startup (crash, OOM-kill) leaves a client
        // whose every request fails: discard it and start fresh. The docs map
        // goes with it — the new server needs its own didOpens.
        if matches!(&*state, LangState::Ready(e) if !e.client.alive()) {
            *state = LangState::Unstarted;
        }
        if matches!(*state, LangState::Unstarted) {
            match start(&self.canon, language).await {
                Ok(client) => {
                    *state = LangState::Ready(Entry {
                        client,
                        started: Instant::now(),
                        docs: HashMap::new(),
                    });
                }
                Err(error) => {
                    *state = LangState::Failed {
                        error: error.clone(),
                        at: Instant::now(),
                    };
                    return Err(error);
                }
            }
        }
        let LangState::Ready(entry) = &mut *state else {
            unreachable!("state is Ready after the arms above")
        };
        let hash = content_hash(source.as_bytes());
        match entry.docs.get_mut(abs) {
            None => {
                entry.client.did_open(abs, language, 1, source);
                entry
                    .docs
                    .insert(abs.to_path_buf(), DocState { hash, version: 1 });
            }
            Some(doc) if doc.hash != hash => {
                doc.version += 1;
                entry.client.did_change(abs, doc.version, source);
                doc.hash = hash;
            }
            Some(_) => {}
        }
        Ok((entry.client.clone(), entry.started))
    }

    /// Render targets as `path:line: <line text>` rows, project paths first.
    fn format_targets(&self, targets: &[Target], symbol: &str, kind: Semantic) -> SemanticResult {
        if targets.is_empty() {
            let what = match kind {
                Semantic::Definition => "definition",
                _ => "references",
            };
            return SemanticResult {
                content: format!("no {what} found for `{symbol}`"),
                targets: Vec::new(),
                transient: false,
            };
        }
        let mut line_cache: HashMap<PathBuf, Vec<String>> = HashMap::new();
        let mut rows = Vec::new();
        let mut refs = Vec::new();
        for t in targets.iter().take(MAX_TARGETS) {
            let line1 = t.line + 1;
            // Servers answer in canonical paths; fall back to the raw root
            // for callers that pass non-canonical targets (tests).
            let rel = t
                .path
                .strip_prefix(&self.canon)
                .or_else(|_| t.path.strip_prefix(&self.root))
                .ok();
            let label = match rel {
                Some(rel) => {
                    let rel = rel.to_string_lossy().into_owned();
                    if refs.len() < 8 {
                        refs.push((rel.clone(), line1));
                    }
                    rel
                }
                // Outside the project (stdlib, dependencies): show where, but
                // it is not navigable in clew, so no chip.
                None => t.path.to_string_lossy().into_owned(),
            };
            let preview = line_cache
                .entry(t.path.clone())
                .or_insert_with(|| {
                    std::fs::read_to_string(&t.path)
                        .map(|s| s.lines().map(str::to_string).collect())
                        .unwrap_or_default()
                })
                .get(t.line)
                .map(|l| l.trim().to_string())
                .unwrap_or_default();
            rows.push(format!("{label}:{line1}: {preview}"));
        }
        let mut content = rows.join("\n");
        if targets.len() > MAX_TARGETS {
            content.push_str(&format!("\n… {} more", targets.len() - MAX_TARGETS));
        }
        SemanticResult {
            content,
            targets: refs,
            transient: false,
        }
    }
}

enum HoverOrTargets {
    Hover(Option<String>),
    Targets(Vec<Target>),
}

/// One raw LSP request for `kind`, time-boxed so a hung server cannot hang
/// the agent turn.
async fn run_query(
    client: &LspClient,
    kind: Semantic,
    abs: &Path,
    line0: usize,
    character: usize,
) -> Result<HoverOrTargets, String> {
    let fut = async {
        match kind {
            Semantic::Definition => client.definition(abs, line0, character).await,
            Semantic::References => {
                client
                    .navigate("textDocument/references", abs, line0, character)
                    .await
            }
            Semantic::Hover => {
                let text = client.hover(abs, line0, character).await?;
                return Ok(HoverOrTargets::Hover(text));
            }
        }
        .map(HoverOrTargets::Targets)
    };
    tokio::time::timeout(CALL_TIMEOUT, fut)
        .await
        .map_err(|_| "the language server timed out".to_string())?
}

/// Resolve and launch the server for `language`, then wait out its initial
/// indexing (bounded). Mirrors the `SpawnLsp` resolution, minus installs.
async fn start(root: &Path, language: &str) -> Result<LspClient, String> {
    let config = config::ProjectLspConfig::load(root).unwrap_or_default();
    let Some(server) = config.resolve(language) else {
        return Err(format!(
            "no language server is configured for {language} — use `search` instead"
        ));
    };
    let exe = match server.command.clone() {
        Some(cmd) => cmd,
        None => match store::locate(&server) {
            store::Located::Ready(exe) => exe,
            store::Located::NeedsDownload { .. } | store::Located::NeedsInstall { .. } => {
                return Err(format!(
                    "the {language} language server is not installed — the user can \
                     open a {language} file in clew to install it; use `search` for now"
                ));
            }
            store::Located::Unsupported(msg) => return Err(msg),
        },
    };
    let client = LspClient::start(&exe, &server.args, root, server.init_options.clone()).await?;
    // Progress can lag the handshake; give it a moment to appear, then wait
    // (bounded) for the initial index so first queries aren't false negatives.
    tokio::time::sleep(INDEX_GRACE).await;
    let started = Instant::now();
    while client.progress().is_some() && started.elapsed() < INDEX_WAIT {
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    Ok(client)
}

/// Byte offset of `symbol` in `line`, preferring a match that stands alone as
/// an identifier (not embedded in a longer word) so `id` anchors on `id`, not
/// the middle of `identifier`.
fn symbol_byte_col(line: &str, symbol: &str) -> Option<usize> {
    if symbol.is_empty() {
        return None;
    }
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    let mut fallback = None;
    let mut from = 0;
    while let Some(i) = line[from..].find(symbol) {
        let at = from + i;
        let before_ok = line[..at].chars().next_back().is_none_or(|c| !is_ident(c));
        let after_ok = line[at + symbol.len()..]
            .chars()
            .next()
            .is_none_or(|c| !is_ident(c));
        if before_ok && after_ok {
            return Some(at);
        }
        fallback.get_or_insert(at);
        from = at + symbol.len();
    }
    fallback
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbol_position_prefers_whole_identifiers() {
        // `id` embedded in `identifier` is skipped in favor of the standalone one.
        assert_eq!(symbol_byte_col("let identifier = id;", "id"), Some(17));
        // No standalone occurrence: fall back to the first embedded match.
        assert_eq!(symbol_byte_col("let identifier = 1;", "id"), Some(4));
        assert_eq!(symbol_byte_col("nothing here", "id"), None);
        assert_eq!(symbol_byte_col("x", ""), None);
    }

    #[test]
    fn symbol_position_counts_bytes_for_unicode_lines() {
        // "变量" is 6 bytes; the byte column must reflect that.
        let line = "变量 = call()";
        assert_eq!(symbol_byte_col(line, "call"), Some(9));
        // And the utf-16 conversion the caller applies would count code units.
        let byte_col = symbol_byte_col(line, "call").unwrap();
        assert_eq!(line[..byte_col].encode_utf16().count(), 5);
    }

    #[test]
    fn format_targets_relativizes_and_previews() {
        let dir = std::env::temp_dir().join("clew-agent-lsp-fmt-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), "fn one() {}\nfn two() {}\n").unwrap();
        let pool = LspPool::new(dir.clone());
        let targets = vec![
            // Canonical form, as a real server reports it.
            Target {
                path: dir.canonicalize().unwrap().join("a.rs"),
                line: 1,
                character: 3,
            },
            // Raw (non-canonical) form still relativizes via the fallback.
            Target {
                path: dir.join("a.rs"),
                line: 0,
                character: 0,
            },
            Target {
                path: PathBuf::from("/outside/lib.rs"),
                line: 0,
                character: 0,
            },
        ];
        let out = pool.format_targets(&targets, "two", Semantic::Definition);
        assert!(out.content.contains("a.rs:2: fn two() {}"));
        assert!(out.content.contains("a.rs:1: fn one() {}"));
        assert!(out.content.contains("/outside/lib.rs:1"));
        // Only the in-project targets become chips.
        assert_eq!(
            out.targets,
            vec![("a.rs".to_string(), 2), ("a.rs".to_string(), 1)]
        );

        let empty = pool.format_targets(&[], "two", Semantic::References);
        assert!(empty.content.contains("no references"));
    }
}
