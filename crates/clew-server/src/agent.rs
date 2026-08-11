//! The Ask agent: a server-side tool loop answering one question about the
//! open project.
//!
//! The model explores with read-only tools (search / read / outline / files /
//! history / semantic find / cached explanations / language-server navigation)
//! until it can answer, then calls `answer` and the final answer is generated
//! by a **streaming** request whose tokens forward to the client as they
//! arrive. Every tool call streams back as an `AgentStep` notification (the
//! step chips in the Ask panel), the answer as `AgentDelta` chunks, and the
//! turn closes with `AgentDone`. The whole run is blocking — callers run it
//! inside `spawn_blocking`.
//!
//! The exploration steps are streamed too, even though nothing forwards their
//! tokens: an SSE read is the only point where the turn's stop flag can reach a
//! request that is already on the wire (see `llm::complete_tools_step`). Only an
//! endpoint that refuses to stream drops back to a blocking POST, and loses the
//! seam with it.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use clew_core::fs_scan::FileEntry;
use clew_core::{embed, explain, git, highlight, llm, outline, search};
use clew_protocol::{AgentRef, AiChatMsg, Event, ServerMessage};
use tokio::sync::mpsc::UnboundedSender;

use crate::agent_lsp::{LspPool, Semantic};

/// Exploration budget: at most this many model steps (a step may batch several
/// tool calls). Past it, the model is told to answer with what it has. Current
/// models sustain long tool loops reliably; the cap is a cost backstop, not a
/// capability guess, and cross-file questions routinely need 15+ steps.
const MAX_STEPS: usize = 30;
/// Per-tool-result cap, so one grep of a vendored file can't flood the context.
const MAX_RESULT_CHARS: usize = 6_000;
/// Lines `read` returns per call at most.
const MAX_READ_LINES: usize = 250;
/// Tokens per exploration step. Generous on purpose: a step may batch several
/// tool calls whose JSON arguments add up, and a cap hit mid-arguments yields a
/// truncated call. Models stop early when done, so the cap costs nothing extra.
const STEP_TOKENS: u32 = 4_000;
const ANSWER_TOKENS: u32 = 4_000;
/// Hard ceiling for the doubling retry when a step is truncated mid-response.
const STEP_TOKENS_CEIL: u32 = 16_000;
/// Largest file any tool will pull into memory. The agent reads whatever path
/// the model names inside the project, so every one of those reads needs the
/// same bound — a per-tool limit on what is RETURNED shapes the context and
/// nothing else, because by then the whole file is already resident.
const MAX_TOOL_READ_BYTES: u64 = 4 * 1024 * 1024;
/// How long the turn waits for the one embeddings request `semantic_find`
/// makes, and how often that wait re-reads the stop flag. `embed_batch` is a
/// blocking POST on ureq's default agent, which sets no read or write timeout:
/// an endpoint that completes the handshake and then goes silent (a local model
/// server still loading, a black-holing proxy, a route that died after the
/// request went out) parked the whole turn inside it — no `AgentDone`, the Ask
/// panel spinning, and Stop inert because a blocking POST has no seam the flag
/// can reach. See `embed_query`.
const EMBED_QUERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
const EMBED_STOP_POLL: std::time::Duration = std::time::Duration::from_millis(100);

/// Everything a tool needs to run, resolved once per turn.
struct Ctx<'a> {
    root: PathBuf,
    files: Arc<Vec<FileEntry>>,
    embed_cfg: Option<embed::Config>,
    /// Lazily-loaded project caches (only read if a tool needs them).
    explain_cache: std::cell::OnceCell<explain::Cache>,
    embed_index: std::cell::OnceCell<embed::Index>,
    /// Language servers for the semantic tools, shared across turns.
    lsp: Arc<LspPool>,
    /// Runtime handle to drive the (async) LSP calls from this blocking loop.
    /// `None` only in unit tests, where the semantic tools degrade to a note.
    rt: Option<tokio::runtime::Handle>,
    /// The turn's stop flag, honored inside long-running tool retries too.
    stop: &'a AtomicBool,
}

impl Ctx<'_> {
    /// This project's derived-artifact store. The server IS the host, so the
    /// key is unscoped — a remote project's caches live on the machine that
    /// computed them, which is this one.
    fn derived(&self) -> Option<PathBuf> {
        clew_core::derived::dir(None, &self.root)
    }

    /// The explanation cache, or an empty one when this project has none yet
    /// (the tools then report that rather than inventing summaries).
    fn load_explain_cache(&self) -> explain::Cache {
        match self.derived() {
            Some(store) => explain::load(&store, &self.root),
            None => explain::Cache::default(),
        }
    }

    /// The semantic index, kept only when it was built in the embedding space
    /// this turn queries in. The authority is the turn's own `embed_cfg`, which
    /// arrives from the client over `SetAiConfig`, never this machine's
    /// config.toml: on a remote host that file describes a different user's
    /// setup, and a mismatch there would rank a fresh query vector against
    /// vectors from another model — cosine still returns confident numbers, so
    /// the wrong answer would look right.
    fn load_embed_index(&self) -> embed::Index {
        match self.derived() {
            Some(store) => embed::load_for(&store, &self.root, self.embed_cfg.as_ref()),
            None => embed::Index::default(),
        }
    }
}

/// Run one agent turn. Blocking; emits notifications on `out` throughout.
#[allow(clippy::too_many_arguments)]
pub fn run(
    root: PathBuf,
    files: Arc<Vec<FileEntry>>,
    chat: llm::Config,
    embed_cfg: Option<embed::Config>,
    lsp: Arc<LspPool>,
    rt: tokio::runtime::Handle,
    stream: u64,
    question: String,
    history: Vec<AiChatMsg>,
    context: String,
    out: &UnboundedSender<ServerMessage>,
    stop: &AtomicBool,
) {
    let notify = |event: Event| {
        let _ = out.send(ServerMessage::Notification { sub: None, event });
    };
    let done = |error: Option<String>| {
        notify(Event::AgentDone { stream, error });
    };
    let stopped = || stop.load(Ordering::Relaxed);

    let ctx = Ctx {
        root: root.clone(),
        files,
        embed_cfg,
        explain_cache: std::cell::OnceCell::new(),
        embed_index: std::cell::OnceCell::new(),
        lsp,
        rt: Some(rt),
        stop,
    };
    let system = system_prompt(&ctx);
    let tools = tool_defs();

    // Replay recent turns, then the question (with any client-side grounding —
    // pinned selections / debugger state — appended verbatim).
    let mut msgs: Vec<llm::AgentMsg> = history
        .into_iter()
        .map(|m| {
            if m.role == "assistant" {
                llm::AgentMsg::Assistant {
                    text: m.content,
                    calls: Vec::new(),
                    thinking: Vec::new(),
                }
            } else {
                llm::AgentMsg::User(m.content)
            }
        })
        .collect();
    let user = if context.trim().is_empty() {
        question
    } else {
        format!("{question}\n\n{context}")
    };
    msgs.push(llm::AgentMsg::User(user));

    // Stream the final answer: tokens forward to the panel as they arrive.
    // `sent` records whether anything reached the client, deciding between
    // "fail the turn" and "quietly fall back" when the stream errors.
    let sent = std::cell::Cell::new(false);
    let stream_answer = |msgs: &[llm::AgentMsg]| {
        sent.set(false);
        llm::complete_tools_stream(
            &chat,
            &system,
            msgs,
            &tools,
            ANSWER_TOKENS,
            |delta| {
                // A stopped turn must not keep painting the panel.
                if stopped() {
                    return;
                }
                sent.set(true);
                notify(Event::AgentDelta {
                    stream,
                    text: delta.to_string(),
                });
            },
            // The turn's stop flag, reaching inside the stream: Stop pressed
            // mid-answer drops the connection instead of letting the provider
            // generate (and bill) to the end while the panel keeps spinning.
            // The error comes back as `llm::CANCELLED`, which the `stopped()`
            // arms below turn into the turn's closing
            // `AgentDone { error: "stopped" }`.
            &stopped,
        )
    };

    // The loop: let the model explore until it answers or the budget runs out.
    let mut seen: HashSet<String> = HashSet::new();
    let mut answer = String::new();
    let mut streamed = false;
    for step in 0..=MAX_STEPS {
        if stopped() {
            done(Some("stopped".into()));
            return;
        }
        // Budget exhausted: one last step, told to answer without tools.
        if step == MAX_STEPS {
            msgs.push(llm::AgentMsg::User(
                "Stop exploring now. Write your final answer from what you have gathered, \
                 citing code as path:line."
                    .into(),
            ));
            match stream_answer(&msgs) {
                Ok(text) => {
                    if stopped() {
                        done(Some("stopped".into()));
                        return;
                    }
                    answer = text;
                    streamed = true;
                    break;
                }
                // A stop during the stream reports as "stopped", not as
                // whatever error the abandoned stream happened to die with.
                Err(_) if stopped() => {
                    done(Some("stopped".into()));
                    return;
                }
                Err(e) if sent.get() => {
                    done(Some(e));
                    return;
                }
                // The stream never started (e.g. an endpoint without
                // `tool_choice: "none"`): fall through to a plain completion.
                Err(_) => {}
            }
        }
        // A step cut off by `max_tokens` may carry half-written tool calls
        // that must never run. Retry the step with a doubled budget up to the
        // ceiling; past it, discard the suspect calls and tell the model.
        let mut tokens = if step == MAX_STEPS {
            ANSWER_TOKENS
        } else {
            STEP_TOKENS
        };
        let step_out = loop {
            // Streamed (see `complete_tools_step`) purely so the stop flag can
            // reach a step already on the wire: sent as a blocking POST, a step
            // had no seam at all, and Stop pressed during one let it generate
            // (and bill) to the end with the panel still spinning.
            match llm::complete_tools_step(&chat, &system, &msgs, &tools, tokens, &stopped) {
                // A stop mid-step comes back as `llm::CANCELLED`; report it as
                // the turn stopping, not as a failed request.
                Err(_) if stopped() => {
                    done(Some("stopped".into()));
                    return;
                }
                Err(e) => {
                    done(Some(e));
                    return;
                }
                Ok(o) if o.truncated && tokens < STEP_TOKENS_CEIL => {
                    tokens = tokens.saturating_mul(2).min(STEP_TOKENS_CEIL);
                }
                Ok(o) => break o,
            }
            if stopped() {
                done(Some("stopped".into()));
                return;
            }
        };
        if step == MAX_STEPS {
            // A final answer truncated even at the ceiling is delivered as-is:
            // a partial answer beats an error.
            answer = step_out.text;
            // The model ignored the stop order and only called tools: refuse
            // every call with a budget notice and take its next prose reply,
            // instead of misreporting the turn as "empty answer".
            if answer.trim().is_empty() && !step_out.calls.is_empty() {
                msgs.push(llm::AgentMsg::Assistant {
                    text: String::new(),
                    calls: step_out.calls.clone(),
                    thinking: step_out.thinking,
                });
                for call in step_out.calls {
                    msgs.push(llm::AgentMsg::ToolResult {
                        call,
                        content: "Exploration budget exhausted — write your final answer now."
                            .into(),
                    });
                }
                match llm::complete_tools_step(
                    &chat,
                    &system,
                    &msgs,
                    &tools,
                    ANSWER_TOKENS,
                    &stopped,
                ) {
                    Ok(o) if !o.text.trim().is_empty() => answer = o.text,
                    // Still tool-calls-only: report what actually happened
                    // instead of the generic "empty answer".
                    Ok(_) => {
                        done(Some(
                            "the model kept requesting tools after the exploration \
                             budget was exhausted"
                                .into(),
                        ));
                        return;
                    }
                    Err(_) if stopped() => {
                        done(Some("stopped".into()));
                        return;
                    }
                    Err(e) => {
                        done(Some(e));
                        return;
                    }
                }
            }
            break;
        }
        if step_out.calls.is_empty() {
            answer = step_out.text;
            break;
        }
        if step_out.truncated {
            // Truncated at the ceiling with calls attached — skip them.
            if !step_out.text.is_empty() {
                msgs.push(llm::AgentMsg::Assistant {
                    text: step_out.text,
                    calls: Vec::new(),
                    thinking: step_out.thinking,
                });
            }
            msgs.push(llm::AgentMsg::User(
                "Your last response overflowed the token budget mid-tool-call, so its \
                 calls were discarded. Continue with fewer, smaller tool calls per step."
                    .into(),
            ));
            continue;
        }
        msgs.push(llm::AgentMsg::Assistant {
            text: step_out.text.clone(),
            calls: step_out.calls.clone(),
            thinking: step_out.thinking.clone(),
        });
        let mut finalize = false;
        for call in step_out.calls {
            if stopped() {
                done(Some("stopped".into()));
                return;
            }
            // `answer` ends exploration: acknowledge the call (every tool_use
            // needs a result), then stream the answer after this loop.
            if call.name == "answer" {
                finalize = true;
                msgs.push(llm::AgentMsg::ToolResult {
                    call,
                    content: "Write your final answer now from what you gathered, citing \
                              code as path:line."
                        .into(),
                });
                continue;
            }
            // An exact repeat of a settled call gains nothing — nudge the
            // model onward instead. Transient outcomes (LSP still indexing,
            // server errors) stay retryable: the tool result itself invites
            // the retry, so blocking it would cement a false negative.
            let key = format!("{}:{}", call.name, call.args);
            let (content, title, refs) = if seen.contains(&key) {
                (
                    "You already ran this exact call — its result is above. \
                     Try a different query or tool."
                        .to_string(),
                    format!("{} (repeat skipped)", call.name),
                    Vec::new(),
                )
            } else {
                let (content, title, refs, dedup) = exec_tool(&ctx, &call.name, &call.args);
                if dedup {
                    seen.insert(key);
                }
                (content, title, refs)
            };
            notify(Event::AgentStep {
                stream,
                tool: call.name.clone(),
                title,
                refs,
            });
            msgs.push(llm::AgentMsg::ToolResult {
                call,
                content: cap(&content, MAX_RESULT_CHARS),
            });
        }
        if finalize {
            match stream_answer(&msgs) {
                Ok(text) => {
                    if stopped() {
                        done(Some("stopped".into()));
                        return;
                    }
                    answer = text;
                    streamed = true;
                    break;
                }
                Err(_) if stopped() => {
                    done(Some("stopped".into()));
                    return;
                }
                Err(e) if sent.get() => {
                    done(Some(e));
                    return;
                }
                // The stream never started: keep looping — the model answers
                // in prose on a later step and takes the chunked path.
                Err(_) => {}
            }
        }
    }

    if stopped() {
        done(Some("stopped".into()));
        return;
    }
    if answer.trim().is_empty() {
        done(Some("the model returned an empty answer".into()));
        return;
    }
    // Fallback path only (the model answered in prose instead of calling
    // `answer`): the text arrived whole, so feed it to the panel in chunks.
    // The streamed path already delivered every token as a real delta.
    if !streamed {
        let mut buf = String::new();
        for ch in answer.chars() {
            buf.push(ch);
            if buf.len() >= 400 && ch == '\n' {
                notify(Event::AgentDelta {
                    stream,
                    text: std::mem::take(&mut buf),
                });
            }
        }
        if !buf.is_empty() {
            notify(Event::AgentDelta { stream, text: buf });
        }
    }
    done(None);
}

/// The system prompt: who the agent is, the project's shape, and the rules.
fn system_prompt(ctx: &Ctx) -> String {
    // First-level entries give the model a map without a tool call.
    let mut top: Vec<&str> = ctx
        .files
        .iter()
        .map(|f| f.rel.split('/').next().unwrap_or(&f.rel))
        .collect();
    top.sort_unstable();
    top.dedup();
    let top = top.join(", ");
    let name = ctx
        .root
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    format!(
        "You are clew's code agent, answering questions about the project \"{name}\" \
         ({} files; top-level entries: {top}).\n\
         \n\
         Ground every claim in real code by exploring with the tools first:\n\
         - `search` for exact text/regex, `semantic_find` for meaning, `files` to list paths\n\
         - `read` for code (always cite what you read), `outline` for a file's symbols\n\
         - `definition` / `references` / `hover` for language-server-precise navigation: \
           where a symbol is defined, every place it is used, its type and docs. \
           Prefer these over `search` when tracing call chains or same-named symbols.\n\
         - `history` for how a file evolved, `explanations` for cached AI summaries\n\
         Explore purposefully. The moment you can answer, call `answer` and then write \
         it. Do not guess at code you have not read.\n\
         \n\
         When you answer:\n\
         - Cite locations as `path:line` (e.g. `src/app/update.rs:62`) — they become links.\n\
         - Use concise markdown; lead with the direct answer, then the supporting detail.\n\
         - Answer in the language the question was asked in.",
        ctx.files.len()
    )
}

/// The read-only tool set the model may call.
fn tool_defs() -> Vec<llm::ToolDef> {
    let t = |name: &str, description: &str, parameters: serde_json::Value| llm::ToolDef {
        name: name.into(),
        description: description.into(),
        parameters,
    };
    vec![
        t(
            "search",
            "Search all project files for exact text or a regex. Returns file:line matches with a preview.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Text or regex to find" },
                    "regex": { "type": "boolean", "description": "Treat query as a regex (default false)" }
                },
                "required": ["query"]
            }),
        ),
        t(
            "read",
            "Read a file's source with line numbers. Use start_line/end_line to window into big files (at most 250 lines per call).",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "file": { "type": "string", "description": "Project-relative path" },
                    "start_line": { "type": "integer", "description": "1-based first line (default 1)" },
                    "end_line": { "type": "integer", "description": "1-based last line (default start+249)" }
                },
                "required": ["file"]
            }),
        ),
        t(
            "outline",
            "List a file's symbols (functions, types, methods) with their line spans — the fast way to map a file before reading it.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "file": { "type": "string", "description": "Project-relative path" }
                },
                "required": ["file"]
            }),
        ),
        t(
            "files",
            "List project file paths, optionally only those under a prefix (e.g. \"src/app/\").",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "prefix": { "type": "string", "description": "Path prefix filter (optional)" }
                }
            }),
        ),
        t(
            "definition",
            "Jump to a symbol's definition, resolved by the language server (semantic — exact even for same-named symbols). Anchor on an occurrence: the file and 1-based line where the symbol appears (as shown by read/search/outline) plus the symbol text.",
            lsp_tool_params(),
        ),
        t(
            "references",
            "Every place a symbol is used, project-wide, resolved by the language server. Anchor on one occurrence (file, 1-based line, symbol text); returns file:line rows with previews.",
            lsp_tool_params(),
        ),
        t(
            "hover",
            "The language server's type signature and docs for the symbol at a location — quick type/API info without reading the whole file.",
            lsp_tool_params(),
        ),
        t(
            "history",
            "Recent git commits that touched a file (subject, author, age) — how and why it changed.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "file": { "type": "string", "description": "Project-relative path" }
                },
                "required": ["file"]
            }),
        ),
        t(
            "semantic_find",
            "Meaning-based search over pre-generated explanations of this project's files and functions. Good for \"where is X handled?\" questions; needs the project's semantic index (falls back with a note when absent).",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "What you are looking for, in natural language" }
                },
                "required": ["query"]
            }),
        ),
        t(
            "explanations",
            "Cached AI explanations for a file and its functions, if the project has them — a shortcut past reading everything.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "file": { "type": "string", "description": "Project-relative path" }
                },
                "required": ["file"]
            }),
        ),
        t(
            "answer",
            "Call this (alone, with no other tools) the moment you have gathered enough to answer. It ends exploration; you then write the final answer, which streams to the user as you write it.",
            serde_json::json!({ "type": "object", "properties": {} }),
        ),
    ]
}

/// The shared parameter schema of the language-server tools.
fn lsp_tool_params() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "file": { "type": "string", "description": "Project-relative path" },
            "line": { "type": "integer", "description": "1-based line the symbol appears on" },
            "symbol": { "type": "string", "description": "The symbol text on that line (an identifier, not an expression)" }
        },
        "required": ["file", "line", "symbol"]
    })
}

/// Run one tool call. Returns `(result_for_model, chip_title, chip_refs,
/// dedup)` — tools never fail the turn; problems come back as text the model
/// can react to. `dedup: false` marks a transient outcome (an LSP error, or a
/// result produced while the server was still indexing) that an identical
/// later call may legitimately improve on, so it must not be repeat-blocked.
fn exec_tool(
    ctx: &Ctx,
    name: &str,
    args: &serde_json::Value,
) -> (String, String, Vec<AgentRef>, bool) {
    guard_tool(name, || dispatch_tool(ctx, name, args))
}

/// Turn a panic inside a tool into a tool RESULT the model can see.
///
/// Every argument a tool reads is model-written and unvalidated, and an
/// injected model picks them adversarially, so a panicking tool is a reachable
/// state rather than a theoretical one. It used to end the turn silently: the
/// caller runs `agent::run` inside a `spawn_blocking` whose `JoinHandle` is
/// dropped (`lib.rs`), so the unwind vanished there — `AgentDone` never went
/// out, the client's Ask panel spun with no error, Stop was inert (it only sets
/// a flag nothing reads any more) and the `agents` entry leaked. Reporting the
/// failure lets the model react and lets the turn close normally.
///
/// This is a backstop, not a licence to panic: `AssertUnwindSafe` is sound here
/// only because `Ctx`'s interior mutability is two `OnceCell` caches, which an
/// init closure that panics leaves empty rather than half-filled, and because
/// nothing else in a tool outlives the call. The panic message still reaches
/// stderr through the default hook.
fn guard_tool(
    name: &str,
    run: impl FnOnce() -> (String, String, Vec<AgentRef>, bool),
) -> (String, String, Vec<AgentRef>, bool) {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)) {
        Ok(result) => result,
        // Dedup-blocked like any other deterministic result: the same
        // arguments panic again, so the model has to vary them to make
        // progress.
        Err(_) => (
            format!(
                "the `{name}` tool failed on these arguments — try different ones, or another tool"
            ),
            format!("{name} (failed)"),
            Vec::new(),
            true,
        ),
    }
}

/// The tool table itself. Never called directly — every entry runs under
/// `exec_tool`'s panic guard.
fn dispatch_tool(
    ctx: &Ctx,
    name: &str,
    args: &serde_json::Value,
) -> (String, String, Vec<AgentRef>, bool) {
    if matches!(name, "definition" | "references" | "hover") {
        let str_arg = |k: &str| args.get(k).and_then(|v| v.as_str()).unwrap_or("").trim();
        let rel = str_arg("file");
        let symbol = str_arg("symbol").to_string();
        let line = args.get("line").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        let title = format!("{name} {symbol} ({rel}:{line})");
        let Some(abs) = confine(&ctx.root, rel) else {
            return (refused(rel), title, Vec::new(), true);
        };
        if line == 0 {
            return (
                "missing or invalid `line` (1-based)".into(),
                title,
                Vec::new(),
                true,
            );
        }
        let kind = match name {
            "definition" => Semantic::Definition,
            "references" => Semantic::References,
            _ => Semantic::Hover,
        };
        let Some(rt) = &ctx.rt else {
            return (
                "semantic tools unavailable here".into(),
                title,
                Vec::new(),
                true,
            );
        };
        return match rt.block_on(ctx.lsp.query(kind, rel, &abs, line, &symbol, ctx.stop)) {
            Ok(res) => {
                let refs = res
                    .targets
                    .iter()
                    .map(|(rel, line)| AgentRef {
                        rel: rel.clone(),
                        line: Some(*line),
                    })
                    .collect();
                (res.content, title, refs, !res.transient)
            }
            // Errors (server starting, timed out, not installed yet) are
            // transient by nature — never repeat-block their retry.
            Err(e) => (e, title, Vec::new(), false),
        };
    }
    let (content, title, refs) = exec_tool_basic(ctx, name, args);
    (content, title, refs, true)
}

/// The deterministic tools: same args always give the same result within a
/// turn, so their repeats are always dedup-blocked.
fn exec_tool_basic(
    ctx: &Ctx,
    name: &str,
    args: &serde_json::Value,
) -> (String, String, Vec<AgentRef>) {
    let str_arg = |k: &str| args.get(k).and_then(|v| v.as_str()).unwrap_or("").trim();
    match name {
        "search" => {
            let query = str_arg("query");
            let regex = args.get("regex").and_then(|v| v.as_bool()).unwrap_or(false);
            let result = search::search(
                ctx.files.clone(),
                search::SearchOptions {
                    query: query.into(),
                    regex,
                    case_sensitive: false,
                    whole_word: false,
                    include: String::new(),
                    exclude: String::new(),
                    root: Some(ctx.root.clone()),
                },
            );
            if let Some(e) = result.error {
                return (
                    format!("search error: {e}"),
                    format!("search \"{query}\""),
                    Vec::new(),
                );
            }
            let total = result.hits.len();
            let lines: Vec<String> = result
                .hits
                .iter()
                .take(40)
                .map(|h| format!("{}:{}: {}", h.rel, h.line, h.preview.trim()))
                .collect();
            let mut content = lines.join("\n");
            if total > 40 {
                content.push_str(&format!("\n… {} more hits (narrow the query)", total - 40));
            }
            if total == 0 {
                content = "no matches".into();
            }
            let refs = result
                .hits
                .iter()
                .take(8)
                .map(|h| AgentRef {
                    rel: h.rel.clone(),
                    line: Some(h.line),
                })
                .collect();
            (content, format!("search \"{query}\" → {total}"), refs)
        }
        "read" => {
            let rel = str_arg("file");
            let Some(abs) = confine(&ctx.root, rel) else {
                return (refused(rel), format!("read {rel}"), Vec::new());
            };
            // Notebooks read as their script projection (cells as `# %%`
            // blocks) — the raw JSON is noise, and projection lines are the
            // notebook's canonical line space.
            // Bounded and plain-file-only at the READ, not after it (see
            // `MAX_TOOL_READ_BYTES`).
            let source = clew_core::statefile::read_capped(&abs, MAX_TOOL_READ_BYTES).map(|s| {
                if clew_core::notebook::is_notebook(&abs) {
                    clew_core::notebook::parse(&s)
                        .map(|nb| nb.projection)
                        .unwrap_or(s)
                } else {
                    s
                }
            });
            let Some(source) = source else {
                return (
                    format!("cannot read {rel} (missing, too large, or not text)"),
                    format!("read {rel}"),
                    Vec::new(),
                );
            };
            let total = source.lines().count();
            let start = args
                .get("start_line")
                .and_then(|v| v.as_u64())
                .map(|v| v as usize)
                .unwrap_or(1)
                .max(1);
            // Say so instead of computing a window around a line that does not
            // exist. `start` is model-written with no upper bound, and the old
            // code carried it into `start + MAX_READ_LINES - 1`, which panicked
            // for the top values of u64 (debug) or wrapped (release) and
            // otherwise emitted a nonsense header like `lines 999999-100 of
            // 100` over an empty body. Same wording as the LSP tools'
            // out-of-range reply, so the model reads one shape of error.
            if start > total {
                return (
                    format!("{rel} has only {total} lines (asked to start at {start})"),
                    format!("read {rel}:{start}"),
                    Vec::new(),
                );
            }
            // Saturating even though `start <= total` now bounds it: the cap is
            // the invariant the slice below depends on, not a consequence of
            // the check above.
            let last = start.saturating_add(MAX_READ_LINES - 1).min(total);
            // `clamp` cannot panic here — `start <= last` holds by construction
            // — and it also pulls an `end_line` BELOW `start` back up, which
            // used to print a backwards range over a one-line body.
            let end = args
                .get("end_line")
                .and_then(|v| v.as_u64())
                .map(|v| v as usize)
                .unwrap_or(last)
                .clamp(start, last);
            let body: Vec<String> = source
                .lines()
                .enumerate()
                .skip(start.saturating_sub(1))
                .take(end.saturating_sub(start) + 1)
                .map(|(i, l)| format!("{:>5}| {l}", i + 1))
                .collect();
            let mut content = format!("{rel} lines {start}-{end} of {total}:\n");
            content.push_str(&body.join("\n"));
            (
                content,
                format!("read {rel}:{start}-{end}"),
                vec![AgentRef {
                    rel: rel.to_string(),
                    line: Some(start),
                }],
            )
        }
        "outline" => {
            let rel = str_arg("file");
            let Some(abs) = confine(&ctx.root, rel) else {
                return (refused(rel), format!("outline {rel}"), Vec::new());
            };
            // Notebooks outline as their cells. Bounded and plain-file-only
            // at the read, like every other tool (see `MAX_TOOL_READ_BYTES`).
            if clew_core::notebook::is_notebook(&abs) {
                let Some(nb) = clew_core::statefile::read_capped(&abs, MAX_TOOL_READ_BYTES)
                    .and_then(|s| clew_core::notebook::parse(&s))
                else {
                    return (
                        format!("cannot read {rel}"),
                        format!("outline {rel}"),
                        Vec::new(),
                    );
                };
                let cells = nb.outline();
                let n = cells.len();
                let content = if cells.is_empty() {
                    "empty notebook".to_string()
                } else {
                    cells
                        .iter()
                        .map(|(name, kind, line, end)| format!("{line:>5}-{end:<5} {kind} {name}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                return (
                    content,
                    format!("outline {rel} ({n} cells)"),
                    vec![AgentRef {
                        rel: rel.to_string(),
                        line: None,
                    }],
                );
            }
            let (Some(source), Some(key)) = (
                clew_core::statefile::read_capped(&abs, MAX_TOOL_READ_BYTES),
                highlight::detect(&abs),
            ) else {
                return (
                    format!("no outline for {rel} (unsupported language or unreadable)"),
                    format!("outline {rel}"),
                    Vec::new(),
                );
            };
            let symbols = outline::extract(&source, key);
            let n = symbols.len();
            let content = if symbols.is_empty() {
                "no symbols found".to_string()
            } else {
                symbols
                    .iter()
                    .map(|s| format!("{:>5}-{:<5} {} {}", s.line, s.end_line, s.kind, s.name))
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            (
                content,
                format!("outline {rel} ({n} symbols)"),
                vec![AgentRef {
                    rel: rel.to_string(),
                    line: None,
                }],
            )
        }
        "files" => {
            let prefix = str_arg("prefix");
            let matching: Vec<&str> = ctx
                .files
                .iter()
                .map(|f| f.rel.as_str())
                .filter(|r| prefix.is_empty() || r.starts_with(prefix))
                .collect();
            let total = matching.len();
            let mut content = matching
                .iter()
                .take(200)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n");
            if total > 200 {
                content.push_str(&format!("\n… {} more (narrow the prefix)", total - 200));
            }
            if total == 0 {
                content = "no files under that prefix".into();
            }
            let label = if prefix.is_empty() { "*" } else { prefix };
            (content, format!("files {label} → {total}"), Vec::new())
        }
        "history" => {
            let rel = str_arg("file");
            if confine(&ctx.root, rel).is_none() {
                return (refused(rel), format!("history {rel}"), Vec::new());
            }
            let commits = git::file_history(&ctx.root, rel, 15);
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let n = commits.len();
            let content = if commits.is_empty() {
                "no git history (untracked file or not a repository)".to_string()
            } else {
                commits
                    .iter()
                    .map(|c| {
                        format!(
                            "{} {} — {} ({})",
                            &c.sha[..c.sha.len().min(8)],
                            git::relative_time(c.time, now),
                            c.subject,
                            c.author
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            (
                content,
                format!("history {rel} ({n} commits)"),
                vec![AgentRef {
                    rel: rel.to_string(),
                    line: None,
                }],
            )
        }
        "semantic_find" => {
            let query = str_arg("query");
            let Some(ecfg) = &ctx.embed_cfg else {
                return (
                    "semantic index unavailable (no embedding provider configured) — use `search` instead"
                        .into(),
                    format!("find \"{query}\" (no index)"),
                    Vec::new(),
                );
            };
            let index = ctx.embed_index.get_or_init(|| ctx.load_embed_index());
            if index.entries.is_empty() {
                return (
                    "semantic index not built for this project — use `search` instead".into(),
                    format!("find \"{query}\" (no index)"),
                    Vec::new(),
                );
            }
            let qvec = match embed_query(ecfg, query, ctx.stop) {
                Ok(v) => v,
                Err(e) => {
                    return (
                        format!("embedding the query failed ({e}) — use `search` instead"),
                        format!("find \"{query}\" (failed)"),
                        Vec::new(),
                    );
                }
            };
            let cache = ctx.explain_cache.get_or_init(|| ctx.load_explain_cache());
            let hits = embed::search(index, &qvec, 10);
            let n = hits.len();
            let mut refs = Vec::new();
            let content = hits
                .iter()
                .map(|(node, score)| {
                    let rel = node
                        .path()
                        .strip_prefix(&ctx.root)
                        .unwrap_or(node.path())
                        .to_string_lossy()
                        .into_owned();
                    let label = match node {
                        explain::Node::Function { name, .. } => format!("{rel} :: {name}"),
                        _ => rel.clone(),
                    };
                    if refs.len() < 8 {
                        refs.push(AgentRef { rel, line: None });
                    }
                    let summary = cache
                        .get(node)
                        .map(|c| first_sentence(&c.summary))
                        .unwrap_or_default();
                    format!("({score:.2}) {label} — {summary}")
                })
                .collect::<Vec<_>>()
                .join("\n");
            (content, format!("find \"{query}\" → {n}"), refs)
        }
        "explanations" => {
            let rel = str_arg("file");
            let cache = ctx.explain_cache.get_or_init(|| ctx.load_explain_cache());
            let mut lines: Vec<String> = Vec::new();
            for (node, cached) in cache.iter() {
                let node_rel = node
                    .path()
                    .strip_prefix(&ctx.root)
                    .unwrap_or(node.path())
                    .to_string_lossy()
                    .into_owned();
                if node_rel != rel {
                    continue;
                }
                match node {
                    explain::Node::Function { name, .. } => {
                        lines.push(format!("fn {name}: {}", first_sentence(&cached.summary)));
                    }
                    _ => lines.insert(0, format!("file: {}", cached.summary.clone())),
                }
            }
            let n = lines.len();
            let content = if lines.is_empty() {
                "no cached explanations for this file (project not explained yet)".to_string()
            } else {
                lines.join("\n")
            };
            (
                content,
                format!("explanations {rel} → {n}"),
                vec![AgentRef {
                    rel: rel.to_string(),
                    line: None,
                }],
            )
        }
        other => (
            format!("unknown tool: {other}"),
            format!("{other}?"),
            Vec::new(),
        ),
    }
}

/// Embed one query for `semantic_find`, bounded in time and reachable by the
/// turn's stop flag.
///
/// The request runs on its own thread so that the waiting turn keeps a seam:
/// `embed_batch` is a blocking POST with no cancellation point of its own
/// (contrast the streamed model steps, which poll the flag between SSE events),
/// so a slow endpoint used to park the turn there with nothing able to reach
/// the flag.
///
/// What is bounded HERE is how long the TURN waits, not the request. On timeout
/// or Stop the worker thread is abandoned and its result discarded; it is
/// reclaimed when the POST ends on its own, which `embed::REQUEST_TIMEOUT`
/// guarantees it eventually does. So a stalled endpoint costs one parked thread
/// for up to that long, and no longer for the life of the process. The request
/// still cannot be *cancelled* — nothing here reaches the socket.
fn embed_query(cfg: &embed::Config, query: &str, stop: &AtomicBool) -> Result<Vec<f32>, String> {
    let (tx, rx) = std::sync::mpsc::channel();
    let cfg = cfg.clone();
    let text = query.to_string();
    std::thread::spawn(move || {
        let _ = tx.send(embed::embed_batch(&cfg, std::slice::from_ref(&text)));
    });
    let deadline = std::time::Instant::now() + EMBED_QUERY_TIMEOUT;
    loop {
        match rx.recv_timeout(EMBED_STOP_POLL) {
            Ok(Ok(mut vecs)) if !vecs.is_empty() => return Ok(vecs.remove(0)),
            Ok(Ok(_)) => return Err("the endpoint returned no vector".into()),
            Ok(Err(e)) => return Err(e),
            // The worker dropped its sender without answering (it panicked):
            // report it rather than waiting out the deadline for nothing.
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err("the embedding request died".into());
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if stop.load(Ordering::Relaxed) {
                    return Err("stopped".into());
                }
                if std::time::Instant::now() >= deadline {
                    return Err(format!("no answer in {}s", EMBED_QUERY_TIMEOUT.as_secs()));
                }
            }
        }
    }
}

fn refused(rel: &str) -> String {
    format!("refused: path escapes the project: {rel}")
}

/// First sentence of a summary, for one-line tool output.
fn first_sentence(s: &str) -> String {
    let s = s.trim();
    match s.find(". ") {
        Some(i) => s[..i + 1].to_string(),
        None => s.chars().take(200).collect(),
    }
}

/// Truncate to `max` characters on a char boundary, noting the cut.
fn cap(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut cut = max;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…\n[truncated]", &s[..cut])
}

/// Resolve `rel` against `root`, refusing anything that escapes the project —
/// the same confinement `ReadFile` applies (agent tool args are model-written
/// and treated as untrusted). Returns the **canonical** path, so downstream
/// consumers (the language-server tools especially) agree with what external
/// processes report for the same file even under a symlinked root.
fn confine(root: &std::path::Path, rel: &str) -> Option<PathBuf> {
    use std::path::Component;
    let rel_path = std::path::Path::new(rel);
    if rel_path.is_absolute() {
        return None;
    }
    if rel_path
        .components()
        .any(|c| matches!(c, Component::ParentDir))
    {
        return None;
    }
    let canon = root.join(rel_path).canonicalize().ok()?;
    let root_canon = root.canonicalize().ok()?;
    canon.starts_with(&root_canon).then_some(canon)
}

#[cfg(test)]
mod tests {
    use super::*;

    static TEST_STOP: AtomicBool = AtomicBool::new(false);

    fn ctx_for(dir: &std::path::Path, rels: &[&str]) -> Ctx<'static> {
        let files = rels
            .iter()
            .map(|r| FileEntry {
                abs: dir.join(r),
                rel: r.to_string(),
            })
            .collect();
        Ctx {
            root: dir.to_path_buf(),
            files: Arc::new(files),
            embed_cfg: None,
            explain_cache: std::cell::OnceCell::new(),
            embed_index: std::cell::OnceCell::new(),
            lsp: Arc::new(LspPool::new(dir.to_path_buf(), Default::default())),
            rt: None,
            stop: &TEST_STOP,
        }
    }

    /// A Stop pressed while an exploration step is on the wire has to reach
    /// that step, and it only can because the step goes out STREAMED: a
    /// blocking POST has no point between "sent" and "answered" where the flag
    /// could be read, so the step generated (and billed) to its end while the
    /// panel kept spinning, and the turn closed a whole step late. The provider
    /// here flips the flag before it answers, standing in for a Stop that lands
    /// while the request is outstanding.
    #[test]
    fn a_stop_reaches_an_exploration_step_already_on_the_wire() {
        let stop = Arc::new(AtomicBool::new(false));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (req_tx, req_rx) = std::sync::mpsc::channel();
        let flag = stop.clone();
        std::thread::spawn(move || {
            let Ok((mut conn, _)) = listener.accept() else {
                return;
            };
            // Read the WHOLE request before answering: a step's body is several
            // KB (system prompt + tool schemas) and arrives in many segments,
            // and answering mid-write resets the connection instead of
            // delivering the response.
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
                    body_len = String::from_utf8_lossy(&req[..pos])
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
            let start = body_start.unwrap_or(req.len());
            let _ = req_tx.send(String::from_utf8_lossy(&req[start..]).into_owned());
            // "Stop" pressed while the request is outstanding.
            flag.store(true, Ordering::Relaxed);
            let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
                       data: [DONE]\n\n";
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                 connection: close\r\ncontent-length: {}\r\n\r\n{sse}",
                sse.len()
            );
            let _ = std::io::Write::write_all(&mut conn, resp.as_bytes());
            // The listener drops with this thread, so a step that ignored the
            // Stop and asked for another is refused rather than left hanging.
        });

        let dir = std::env::temp_dir().join("clew-agent-stop-test");
        std::fs::create_dir_all(&dir).unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (out, mut rx) = tokio::sync::mpsc::unbounded_channel();
        run(
            dir.clone(),
            Arc::new(Vec::new()),
            llm::Config::from_parts(llm::Provider::Custom, "k".into(), "m".into(), base),
            None,
            Arc::new(LspPool::new(dir, Default::default())),
            rt.handle().clone(),
            7,
            "why?".into(),
            Vec::new(),
            String::new(),
            &out,
            &stop,
        );

        let body = req_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the step was sent");
        assert!(
            body.contains("\"stream\":true"),
            "an exploration step must go out streamed, or nothing can cancel it: {body}"
        );
        let mut done = None;
        while let Ok(ServerMessage::Notification { event, .. }) = rx.try_recv() {
            if let Event::AgentDone { error, .. } = event {
                done = Some(error);
            }
        }
        assert_eq!(
            done,
            Some(Some("stopped".into())),
            "the turn closes as stopped, not as a failed request"
        );
    }

    #[test]
    fn read_tool_windows_and_numbers_lines() {
        let dir = std::env::temp_dir().join("clew-agent-read-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let body: String = (1..=30).map(|i| format!("line {i}\n")).collect();
        std::fs::write(dir.join("a.txt"), body).unwrap();
        let ctx = ctx_for(&dir, &["a.txt"]);

        let (content, title, refs, dedup) = exec_tool(
            &ctx,
            "read",
            &serde_json::json!({ "file": "a.txt", "start_line": 10, "end_line": 12 }),
        );
        assert!(dedup, "deterministic tools dedup their repeats");
        assert!(content.contains("   10| line 10"));
        assert!(content.contains("   12| line 12"));
        assert!(!content.contains("line 13"));
        assert_eq!(title, "read a.txt:10-12");
        assert_eq!(refs[0].line, Some(10));
    }

    /// `start_line` is model-written and unvalidated, so the window arithmetic
    /// has to hold at the numeric edge: `start + MAX_READ_LINES - 1` overflowed
    /// for the top values of u64 (a panic under debug's overflow checks, a
    /// wrapped nonsense window in release), and that panic vanished into the
    /// detached `spawn_blocking` running the turn, so the Ask panel spun with
    /// no error. Out-of-range now reads as an error the model can act on.
    #[test]
    fn read_tool_survives_a_start_line_at_the_numeric_edge() {
        let dir = std::env::temp_dir().join("clew-agent-read-edge-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let body: String = (1..=30).map(|i| format!("line {i}\n")).collect();
        std::fs::write(dir.join("a.txt"), body).unwrap();
        let ctx = ctx_for(&dir, &["a.txt"]);

        let (content, _, refs, _) = exec_tool(
            &ctx,
            "read",
            &serde_json::json!({ "file": "a.txt", "start_line": u64::MAX }),
        );
        assert_eq!(
            content,
            "a.txt has only 30 lines (asked to start at 18446744073709551615)"
        );
        assert!(refs.is_empty(), "a refused window points at nothing");

        // Same shape for a merely out-of-range start, which used to emit
        // `lines 999999-30 of 30:` over an empty body.
        let (content, _, _, _) = exec_tool(
            &ctx,
            "read",
            &serde_json::json!({ "file": "a.txt", "start_line": 999_999 }),
        );
        assert!(content.starts_with("a.txt has only 30 lines"));

        // An `end_line` before `start_line` reads as a one-line window at
        // `start`, not as a backwards range.
        let (content, title, _, _) = exec_tool(
            &ctx,
            "read",
            &serde_json::json!({ "file": "a.txt", "start_line": 10, "end_line": 2 }),
        );
        assert_eq!(title, "read a.txt:10-10");
        assert!(content.contains("   10| line 10") && !content.contains("line 11"));
    }

    /// A panicking tool must reach the model as a result, not take the turn
    /// down: `agent::run` runs inside a `spawn_blocking` whose `JoinHandle` is
    /// dropped, so an unwind out of a tool sent no `AgentDone` at all and left
    /// the Ask panel spinning with Stop inert. (The panic below still prints
    /// through the default hook — that output is expected.)
    #[test]
    fn a_panicking_tool_answers_the_model_instead_of_ending_the_turn() {
        let (content, title, refs, dedup) = guard_tool("read", || panic!("arithmetic went wrong"));
        assert!(
            content.contains("the `read` tool failed"),
            "the model has to see the failure: {content}"
        );
        assert_eq!(title, "read (failed)");
        assert!(refs.is_empty());
        assert!(
            dedup,
            "the same arguments panic again — block the exact repeat"
        );
    }

    /// An embeddings endpoint that accepts the connection and then says nothing
    /// used to park the turn inside `embed_batch` forever (ureq's default agent
    /// has no read timeout), so `AgentDone` never went out and Stop — which
    /// only sets this flag — could not reach it. The wait now polls the flag.
    #[test]
    fn a_stalled_embeddings_endpoint_yields_to_stop() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        // Accept and hold the socket open without answering: an established
        // but silent connection is what blocks ureq indefinitely. Held well
        // past the assertion so the wait cannot end by the peer hanging up.
        std::thread::spawn(move || {
            let conn = listener.accept();
            std::thread::sleep(std::time::Duration::from_secs(30));
            drop(conn);
        });

        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            flag.store(true, Ordering::Relaxed);
        });

        let cfg = embed::Config {
            api_key: "k".into(),
            model: "m".into(),
            base_url: base,
        };
        let began = std::time::Instant::now();
        let err = embed_query(&cfg, "why?", &stop).unwrap_err();
        assert_eq!(err, "stopped");
        assert!(
            began.elapsed() < std::time::Duration::from_secs(5),
            "the Stop has to land while the request is still outstanding, not after it dies: {:?}",
            began.elapsed()
        );
    }

    #[test]
    fn read_tool_refuses_escapes() {
        let dir = std::env::temp_dir().join("clew-agent-confine-test");
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = ctx_for(&dir, &[]);
        let (content, _, _, _) = exec_tool(
            &ctx,
            "read",
            &serde_json::json!({ "file": "../secret.txt" }),
        );
        assert!(content.starts_with("refused"));
        let (content, _, _, _) =
            exec_tool(&ctx, "read", &serde_json::json!({ "file": "/etc/passwd" }));
        assert!(content.starts_with("refused"));
    }

    #[test]
    fn files_tool_filters_by_prefix() {
        let dir = std::env::temp_dir().join("clew-agent-files-test");
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = ctx_for(&dir, &["src/a.rs", "src/b.rs", "docs/c.md"]);
        let (content, title, _, _) =
            exec_tool(&ctx, "files", &serde_json::json!({ "prefix": "src/" }));
        assert!(content.contains("src/a.rs") && content.contains("src/b.rs"));
        assert!(!content.contains("docs/c.md"));
        assert_eq!(title, "files src/ → 2");
    }

    #[test]
    fn duplicate_and_unknown_tools_answer_in_text() {
        let dir = std::env::temp_dir().join("clew-agent-unknown-test");
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = ctx_for(&dir, &[]);
        let (content, _, _, _) = exec_tool(&ctx, "nope", &serde_json::json!({}));
        assert!(content.contains("unknown tool"));
    }

    #[test]
    fn cap_truncates_on_char_boundary() {
        let s = "汉字".repeat(10_000);
        let capped = cap(&s, MAX_RESULT_CHARS);
        assert!(capped.len() <= MAX_RESULT_CHARS + 20);
        assert!(capped.ends_with("[truncated]"));
    }
}
