//! The Ask agent: a server-side tool loop answering one question about the
//! open project.
//!
//! The model explores with read-only tools (search / read / outline / files /
//! history / semantic find / cached explanations / language-server navigation)
//! until it can answer, then calls `answer` and the final answer is generated
//! by a **streaming** request whose tokens forward to the client as they
//! arrive. Every tool call streams back as an `AgentStep` notification (the
//! step chips in the Ask panel), the answer as `AgentDelta` chunks, and the
//! turn closes with exactly one `AgentDone`. The whole run is blocking —
//! callers run it inside `spawn_blocking`.
//!
//! The exploration steps are streamed too, even though nothing forwards their
//! tokens: the stream is what lets the turn's stop flag reach a request that is
//! already on the wire (see `llm::complete_tools_step`). Only an endpoint that
//! REFUSES to stream drops back to a blocking POST; a request that failed after
//! reaching the provider is never sent again (it may already be generating, and
//! a resend is billed twice).
//!
//! Every request re-sends the whole conversation, so the turn keeps it inside
//! a budget: at most [`MAX_CALLS_PER_STEP`] tool calls run per step, and past
//! [`CONTEXT_SOFT_BYTES`] the oldest tool results are elided (see
//! `Turn::fit_context`).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use clew_core::fs_scan::FileEntry;
use clew_core::{embed, explain, git, highlight, llm, outline, search};
use clew_protocol::{AgentRef, AiChatMsg, Event, ServerMessage, StreamOutcome};
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
/// The longest the one embeddings request `semantic_find` makes may take
/// before the turn gives it up and searches without it — the request, not
/// only the wait: `embed_query` ends it then, as it does on a Stop. The
/// request's own limits are the embeddings module's: a silence of
/// `embed::REQUEST_TIMEOUT`, and the pace its answer must keep
/// (`embed::BATCH_PACE`), which is what bounds a peer that never falls
/// silent that long (one that trickles its answer). Both are longer than a
/// turn should wait on one query — an endpoint that accepts the request and
/// then goes quiet (a local model server still loading, a black-holing
/// proxy, a route that died after the request went out) held the turn, the
/// Ask panel spinning, for as long as they let it.
const EMBED_QUERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
/// Most tool calls one step may run. A step that fans out into dozens of
/// calls would pour dozens of results into the context at once; past the cap
/// the calls are answered ("request it again") instead of run.
const MAX_CALLS_PER_STEP: usize = 8;
/// Tries at the closing answer (empty replies, tool calls where prose was
/// asked for) before the turn gives up.
const ANSWER_ATTEMPTS: usize = 3;
/// Context budget, in bytes of request text (about four per token). Past the
/// soft limit the oldest tool results are elided down to the target; past the
/// hard limit — reachable only when the question, context or history alone is
/// that large — the turn ends before sending instead of after a rejected,
/// billed request. Sized under the smallest context window of the default
/// models (~128k tokens), so it is a guard, not a fit.
const CONTEXT_SOFT_BYTES: usize = 200_000;
const CONTEXT_TARGET_BYTES: usize = 150_000;
const CONTEXT_HARD_BYTES: usize = 400_000;
/// Tool results never elided: the newest step's worth, which the model is
/// working from right now.
const KEEP_RECENT_RESULTS: usize = MAX_CALLS_PER_STEP;
/// Most first-level entries the system prompt lists (see `top_level_listing`).
const MAX_TOP_ENTRIES: usize = 60;

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

/// Run one agent turn. Blocking; emits notifications on `out` throughout, and
/// always closes with exactly one `AgentDone`.
///
/// A notification the transport can no longer carry means the client is gone;
/// the turn then stops itself (it sets `stop`) instead of exploring and
/// billing for nobody. The answer's text is charged against `budget`, the
/// transport's backpressure: a client that stopped reading holds the turn up
/// rather than its queue growing.
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
    budget: &crate::OutputBudget,
    stop: &AtomicBool,
) {
    // Held by the budget, but never past a Stop (see `send_bulk_unless`).
    let notify = |event: Event| {
        let stopped = || stop.load(Ordering::Relaxed);
        let msg = ServerMessage::Notification { event };
        if !crate::transport::send_bulk_unless(out, budget, msg, &stopped) {
            stop.store(true, Ordering::Relaxed);
        }
    };
    let ctx = Ctx {
        root,
        files,
        embed_cfg,
        explain_cache: std::cell::OnceCell::new(),
        embed_index: std::cell::OnceCell::new(),
        lsp,
        rt: Some(rt),
        stop,
    };
    // Replay recent turns, then the question (with any client-side grounding —
    // pinned selections / debugger state — appended verbatim).
    let mut msgs = replay_history(history);
    msgs.push(llm::AgentMsg::User(if context.trim().is_empty() {
        question
    } else {
        format!("{question}\n\n{context}")
    }));
    let mut turn = Turn {
        system: system_prompt(&ctx),
        tools: tool_defs(),
        ctx: &ctx,
        chat: &chat,
        msgs,
        seen: HashMap::new(),
        stream,
        notify: &notify,
    };
    let outcome = match turn.drive() {
        _ if stop.load(Ordering::Relaxed) => StreamOutcome::Stopped,
        Ok(()) => StreamOutcome::Done,
        Err(Halt::Stopped) => StreamOutcome::Stopped,
        Err(Halt::Failed(message)) => StreamOutcome::Failed(message),
    };
    notify(Event::AgentDone { stream, outcome });
}

/// Why a turn ended without an answer.
enum Halt {
    /// The stop flag fired (Stop, Cancel, a project switch, a lost client).
    Stopped,
    /// The turn failed; the message is for the user.
    Failed(String),
}

impl From<llm::LlmError> for Halt {
    fn from(e: llm::LlmError) -> Halt {
        match e {
            llm::LlmError::Cancelled => Halt::Stopped,
            e => Halt::Failed(e.to_string()),
        }
    }
}

/// The phase a turn is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Tool-capable steps: the model reads and searches.
    Explore,
    /// The model said it can answer (or the budget ran out): the closing
    /// answer, streamed with tools pinned off.
    Answer,
}

/// One turn's conversation and bookkeeping.
struct Turn<'a> {
    system: String,
    tools: Vec<llm::ToolDef>,
    ctx: &'a Ctx<'a>,
    chat: &'a llm::Config,
    msgs: Vec<llm::AgentMsg>,
    /// Settled calls (`name:args`) → index in `msgs` of the result the model
    /// already has. An exact repeat is answered with a pointer to it instead
    /// of running again; eliding that result (see [`Turn::fit_context`])
    /// forgets the entry, so the model may fetch it again.
    seen: HashMap<String, usize>,
    stream: u64,
    notify: &'a dyn Fn(Event),
}

impl Turn<'_> {
    fn stopped(&self) -> bool {
        self.ctx.stop.load(Ordering::Relaxed)
    }

    fn check_stop(&self) -> Result<(), Halt> {
        if self.stopped() {
            Err(Halt::Stopped)
        } else {
            Ok(())
        }
    }

    /// Explore until the model can answer (or the budget runs out), then
    /// deliver the answer. Every model request is accounted for: at most
    /// [`MAX_STEPS`] exploration steps and [`ANSWER_ATTEMPTS`] tries at the
    /// closing answer, so no path loops.
    fn drive(&mut self) -> Result<(), Halt> {
        let mut phase = Phase::Explore;
        let mut steps = 0usize;
        let mut answer_attempts = 0usize;
        let mut exhausted = false;
        // The endpoint refused `tool_choice: "none"` (a 400/422 before any
        // token): answers then come from ordinary steps, as prose.
        let mut unpinned = false;
        // One retry for a reply with nothing in it, before calling it a failure.
        let mut empty_retried = false;
        loop {
            self.check_stop()?;
            if phase == Phase::Explore && steps >= MAX_STEPS {
                // Budget exhausted: one last try, told to answer from what it has.
                exhausted = true;
                phase = Phase::Answer;
                self.msgs.push(llm::AgentMsg::User(
                    "Stop exploring now. Write your final answer from what you have gathered, \
                     citing code as path:line."
                        .into(),
                ));
            }
            self.fit_context()?;
            match phase {
                Phase::Explore => {
                    let out = self.step()?;
                    steps += 1;
                    if out.calls.is_empty() {
                        // Prose instead of `answer`: the text IS the answer.
                        if !out.text.is_empty() {
                            self.deliver_whole(out.text, out.truncated);
                            return Ok(());
                        }
                        self.retry_empty(&mut empty_retried, &out)?;
                        continue;
                    }
                    if out.truncated {
                        // Truncated even at the ceiling, with calls attached:
                        // their arguments may be half-written — never run them.
                        self.discard_truncated_calls(out.text);
                        continue;
                    }
                    if self.run_calls(out.text, out.calls)? {
                        phase = Phase::Answer;
                    }
                }
                Phase::Answer => {
                    answer_attempts += 1;
                    if answer_attempts > ANSWER_ATTEMPTS {
                        return Err(Halt::Failed(
                            "the model did not produce an answer (it kept requesting tools \
                             instead)"
                                .into(),
                        ));
                    }
                    let out = if unpinned {
                        self.step()?
                    } else {
                        match self.stream_answer() {
                            Ok(out) => out,
                            // An endpoint without `tool_choice: "none"` (or
                            // without SSE) refuses the pinned stream outright,
                            // before anything is generated — the one case in
                            // which asking again, differently, is safe. Every
                            // other failure is final: after a transport error
                            // the provider may already be generating, and a
                            // resend would be billed twice.
                            Err(e) if e.stream_refused() => {
                                unpinned = true;
                                answer_attempts -= 1;
                                continue;
                            }
                            Err(e) => return Err(e.into()),
                        }
                    };
                    if !out.text.is_empty() {
                        if unpinned {
                            self.deliver_whole(out.text, out.truncated);
                        } else if out.truncated {
                            // The tokens already went out as they arrived;
                            // only the note is left to say.
                            self.delta(llm::TRUNCATED_NOTE.to_string());
                        }
                        return Ok(());
                    }
                    if out.calls.is_empty() {
                        self.retry_empty(&mut empty_retried, &out)?;
                        continue;
                    }
                    // Tool calls where prose was asked for: the endpoint
                    // ignored `tool_choice: none` (or, unpinned, the model
                    // simply went on exploring). Reading only the text turned
                    // this into "the model returned an empty answer".
                    if exhausted || steps >= MAX_STEPS {
                        self.refuse_calls(out.calls);
                    } else {
                        // It wants to keep exploring, and budget remains: that
                        // is an exploration step, and counts as one.
                        steps += 1;
                        answer_attempts -= 1;
                        if !self.run_calls(out.text, out.calls)? {
                            phase = Phase::Explore;
                        }
                    }
                }
            }
        }
    }

    /// One tool-capable step. A step cut off by `max_tokens` may carry
    /// half-written tool calls; it is re-asked with a doubled budget up to the
    /// ceiling. That is a resend of a request that COMPLETED, so it cannot
    /// duplicate a generation still running — unlike a resend after a
    /// transport error, which nothing here does.
    fn step(&self) -> Result<llm::StepOutput, Halt> {
        let mut tokens = STEP_TOKENS;
        loop {
            // Streamed (see `complete_tools_step`) so the stop flag reaches a
            // step already on the wire.
            let out = llm::complete_tools_step(
                self.chat,
                &self.system,
                &self.msgs,
                &self.tools,
                tokens,
                &|| self.stopped(),
            )?;
            if out.truncated && tokens < STEP_TOKENS_CEIL {
                self.check_stop()?;
                tokens = tokens.saturating_mul(2).min(STEP_TOKENS_CEIL);
                continue;
            }
            return Ok(out);
        }
    }

    /// The closing answer, streamed: tokens forward to the panel as they
    /// arrive, and Stop drops the connection mid-answer.
    fn stream_answer(&self) -> Result<llm::StepOutput, llm::LlmError> {
        llm::complete_tools_stream(
            self.chat,
            &self.system,
            &self.msgs,
            &self.tools,
            ANSWER_TOKENS,
            |delta| {
                // A stopped turn must not keep painting the panel.
                if !self.stopped() {
                    self.delta(delta.to_string());
                }
            },
            &|| self.stopped(),
        )
    }

    fn delta(&self, text: String) {
        (self.notify)(Event::AgentDelta {
            stream: self.stream,
            text,
        });
    }

    /// Deliver an answer that arrived whole (a prose step, not the stream):
    /// fed to the panel in chunks, with the truncation note when it was cut.
    fn deliver_whole(&self, mut text: String, truncated: bool) {
        if truncated {
            text.push_str(llm::TRUNCATED_NOTE);
        }
        let mut buf = String::new();
        for ch in text.chars() {
            buf.push(ch);
            if buf.len() >= 400 && ch == '\n' {
                self.delta(std::mem::take(&mut buf));
            }
        }
        if !buf.is_empty() {
            self.delta(buf);
        }
    }

    /// A reply with neither text nor calls: ask once more with an explicit
    /// nudge, then give up with what the provider said about why.
    fn retry_empty(&mut self, retried: &mut bool, out: &llm::StepOutput) -> Result<(), Halt> {
        if *retried {
            return Err(Halt::Failed(match &out.stop_reason {
                Some(reason) => {
                    format!("the model returned an empty answer (stop reason: {reason})")
                }
                None => "the model returned an empty answer".into(),
            }));
        }
        *retried = true;
        self.msgs.push(llm::AgentMsg::User(
            "Your last reply was empty. Write your final answer now, in prose, citing code \
             as path:line."
                .into(),
        ));
        Ok(())
    }

    fn discard_truncated_calls(&mut self, text: String) {
        if !text.is_empty() {
            self.msgs.push(llm::AgentMsg::Assistant {
                text,
                calls: Vec::new(),
            });
        }
        self.msgs.push(llm::AgentMsg::User(
            "Your last response overflowed the token budget mid-tool-call, so its calls were \
             discarded. Continue with fewer, smaller tool calls per step."
                .into(),
        ));
    }

    /// Answer every call with a budget notice (each `tool_use` needs its
    /// result), so the next request can only be prose.
    fn refuse_calls(&mut self, calls: Vec<llm::ToolCall>) {
        self.msgs.push(llm::AgentMsg::Assistant {
            text: String::new(),
            calls: calls.clone(),
        });
        for call in calls {
            self.msgs.push(llm::AgentMsg::ToolResult {
                call,
                content: "Exploration budget exhausted — write your final answer now.".into(),
            });
        }
    }

    /// Record the assistant turn and run its calls, at most
    /// [`MAX_CALLS_PER_STEP`] of them. Returns whether the model called
    /// `answer`.
    fn run_calls(&mut self, text: String, calls: Vec<llm::ToolCall>) -> Result<bool, Halt> {
        self.msgs.push(llm::AgentMsg::Assistant {
            text,
            calls: calls.clone(),
        });
        let mut finalize = false;
        let mut ran = 0usize;
        for call in calls {
            self.check_stop()?;
            // `answer` ends exploration: acknowledge the call (every tool_use
            // needs a result), then stream the answer.
            if call.name == "answer" {
                finalize = true;
                self.msgs.push(llm::AgentMsg::ToolResult {
                    call,
                    content: "Write your final answer now from what you gathered, citing code \
                              as path:line."
                        .into(),
                });
                continue;
            }
            // A step that fans out into dozens of calls would pour dozens of
            // results into the context at once; past the cap they are
            // answered, not run.
            if ran >= MAX_CALLS_PER_STEP {
                self.msgs.push(llm::AgentMsg::ToolResult {
                    call,
                    content: format!(
                        "Not run: at most {MAX_CALLS_PER_STEP} tool calls run per step. Request \
                         it again in your next step if you still need it."
                    ),
                });
                continue;
            }
            ran += 1;
            // An exact repeat of a settled call gains nothing — nudge the
            // model onward instead. Transient outcomes (LSP still indexing,
            // server errors) stay retryable: the tool result itself invites
            // the retry, so blocking it would cement a false negative.
            let key = format!("{}:{}", call.name, call.args);
            let (content, title, refs) = if self.seen.contains_key(&key) {
                (
                    "You already ran this exact call — its result is above. Try a different \
                     query or tool."
                        .to_string(),
                    format!("{} (repeat skipped)", call.name),
                    Vec::new(),
                )
            } else {
                let (content, title, refs, dedup) = exec_tool(self.ctx, &call.name, &call.args);
                if dedup {
                    self.seen.insert(key, self.msgs.len());
                }
                (content, title, refs)
            };
            (self.notify)(Event::AgentStep {
                stream: self.stream,
                tool: call.name.clone(),
                title,
                refs,
            });
            self.msgs.push(llm::AgentMsg::ToolResult {
                call,
                content: cap(&content, MAX_RESULT_CHARS),
            });
        }
        Ok(finalize)
    }

    /// Keep the conversation inside the context budget before a request.
    ///
    /// Every step re-sends the whole conversation, so without accounting a
    /// long exploration grew until the provider rejected it — after paying for
    /// every step that led there. Past [`CONTEXT_SOFT_BYTES`] the oldest tool
    /// results are replaced by a marker (the newest [`KEEP_RECENT_RESULTS`]
    /// always stay) until the estimate is back under [`CONTEXT_TARGET_BYTES`];
    /// if it is still over [`CONTEXT_HARD_BYTES`] — the question, context or
    /// history alone is that large — the turn ends before sending, with a
    /// reason, instead of after a rejected request.
    fn fit_context(&mut self) -> Result<(), Halt> {
        let mut size = conversation_bytes(&self.system, &self.tools, &self.msgs);
        if size > CONTEXT_SOFT_BYTES {
            let results: Vec<usize> = self
                .msgs
                .iter()
                .enumerate()
                .filter(|(_, m)| matches!(m, llm::AgentMsg::ToolResult { .. }))
                .map(|(i, _)| i)
                .collect();
            let elidable = results.len().saturating_sub(KEEP_RECENT_RESULTS);
            for &index in &results[..elidable] {
                if size <= CONTEXT_TARGET_BYTES {
                    break;
                }
                if let llm::AgentMsg::ToolResult { content, .. } = &mut self.msgs[index]
                    && content.len() > ELIDED_RESULT.len()
                {
                    size -= content.len() - ELIDED_RESULT.len();
                    *content = ELIDED_RESULT.to_string();
                    // "Its result is above" is no longer true.
                    self.seen.retain(|_, at| *at != index);
                }
            }
        }
        if size > CONTEXT_HARD_BYTES {
            return Err(Halt::Failed(format!(
                "the conversation is too large to send ({} KB) — ask a narrower question or \
                 clear the chat",
                size / 1024
            )));
        }
        Ok(())
    }
}

/// The marker an elided tool result is replaced with.
const ELIDED_RESULT: &str =
    "[result elided to save context — run the tool again if you still need it]";

/// Estimated size of a request, in bytes of text — the currency the context
/// budget is kept in (roughly four bytes per token for code and English).
fn conversation_bytes(system: &str, tools: &[llm::ToolDef], msgs: &[llm::AgentMsg]) -> usize {
    let tools: usize = tools
        .iter()
        .map(|t| t.name.len() + t.description.len() + t.parameters.to_string().len())
        .sum();
    let msgs: usize = msgs
        .iter()
        .map(|m| match m {
            llm::AgentMsg::User(text) => text.len(),
            llm::AgentMsg::Assistant { text, calls } => {
                text.len()
                    + calls
                        .iter()
                        .map(|c| c.id.len() + c.name.len() + c.args.to_string().len())
                        .sum::<usize>()
            }
            llm::AgentMsg::ToolResult { call, content } => call.id.len() + content.len(),
        })
        .sum();
    system.len() + tools + msgs
}

/// The client's recent turns as a conversation the providers accept.
///
/// Only answered questions are replayed. An assistant turn with no text — a
/// turn that failed, or was stopped before its first token — is rejected by
/// the providers outright (an empty assistant message is a 400), and the
/// question it leaves unanswered adds nothing a follow-up could resolve
/// against. What remains strictly alternates user/assistant starting with the
/// user, which the Messages API requires; the new question follows it.
fn replay_history(history: Vec<AiChatMsg>) -> Vec<llm::AgentMsg> {
    let mut out = Vec::new();
    let mut question: Option<String> = None;
    for m in history {
        if m.role == "assistant" {
            match question.take() {
                Some(q) if !m.content.trim().is_empty() => {
                    out.push(llm::AgentMsg::User(q));
                    out.push(llm::AgentMsg::Assistant {
                        text: m.content,
                        calls: Vec::new(),
                    });
                }
                // An empty answer drops its question; an answer with no
                // question before it (a truncated history) is dropped too.
                _ => {}
            }
        } else if !m.content.trim().is_empty() {
            // A newer question supersedes one that never got an answer.
            question = Some(m.content);
        }
    }
    out
}

/// The system prompt: who the agent is, the project's shape, and the rules.
fn system_prompt(ctx: &Ctx) -> String {
    let name = ctx
        .root
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    format!(
        "You are clew's code agent, answering questions about the project \"{name}\" \
         ({} files; top-level entries: {}).\n\
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
        ctx.files.len(),
        top_level_listing(&ctx.files),
    )
}

/// The project's first-level entries for the system prompt — a map without a
/// tool call. Directories first (marked `/`), then files, each sorted, and at
/// most [`MAX_TOP_ENTRIES`] of them: a flat folder of thousands of files used
/// to put every name into a prompt that is re-sent with every step.
fn top_level_listing(files: &[FileEntry]) -> String {
    let mut dirs: Vec<&str> = Vec::new();
    let mut plain: Vec<&str> = Vec::new();
    for f in files {
        match f.rel.split_once('/') {
            Some((dir, _)) => dirs.push(dir),
            None => plain.push(&f.rel),
        }
    }
    for list in [&mut dirs, &mut plain] {
        // Case-insensitive, ties broken by the exact name so that equal names
        // are adjacent for `dedup` whatever their case neighbours.
        list.sort_by_cached_key(|s| (s.to_lowercase(), s.to_string()));
        list.dedup();
    }
    let total = dirs.len() + plain.len();
    let mut listing = dirs
        .iter()
        .map(|d| format!("{d}/"))
        .chain(plain.iter().map(|f| f.to_string()))
        .take(MAX_TOP_ENTRIES)
        .collect::<Vec<_>>()
        .join(", ");
    if total > MAX_TOP_ENTRIES {
        listing.push_str(&format!(", … {} more", total - MAX_TOP_ENTRIES));
    }
    listing
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
/// state rather than a theoretical one. Unguarded, the unwind ends
/// `agent::run`; the request loop awaits the turn's `spawn_blocking` and can
/// only close it as failed ("the agent turn failed unexpectedly", `lib.rs`) —
/// every step the model took, and the answer it was working toward, lost to
/// one bad argument. (Before that await existed the unwind vanished with a
/// dropped `JoinHandle`, and no `AgentDone` went out at all.) Reporting the
/// failure as the tool's result lets the model react and the turn go on.
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
        let line = uint_arg(args, "line").map_or(0, |n| usize::try_from(n).unwrap_or(usize::MAX));
        let title = format!("{name} {symbol} ({rel}:{line})");
        let abs = match resolve(ctx, rel) {
            Ok(abs) => abs,
            Err(refusal) => return (refusal, title, Vec::new(), true),
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
            let regex = bool_arg(args, "regex").unwrap_or(false);
            // Every read confined to the root (a required argument here), and
            // the files that could not be searched are named to the model: a
            // search that quietly covers less than it claims reads as "the
            // code does not do that".
            let result = search::search_report(
                &ctx.root,
                ctx.files.clone(),
                search::SearchOptions {
                    query: query.into(),
                    regex,
                    ..Default::default()
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
            if !result.skipped.is_empty() {
                let named: Vec<String> = result
                    .skipped
                    .iter()
                    .take(5)
                    .map(|f| format!("{} ({})", f.rel, f.reason))
                    .collect();
                content.push_str(&format!(
                    "\n(not searched: {}{})",
                    named.join(", "),
                    if result.skipped.len() > 5 {
                        format!(", and {} more", result.skipped.len() - 5)
                    } else {
                        String::new()
                    }
                ));
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
            let abs = match resolve(ctx, rel) {
                Ok(abs) => abs,
                Err(refusal) => return (refusal, format!("read {rel}"), Vec::new()),
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
            let start = uint_arg(args, "start_line")
                .map_or(1, |v| usize::try_from(v).unwrap_or(usize::MAX))
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
            let end = uint_arg(args, "end_line")
                .map_or(last, |v| usize::try_from(v).unwrap_or(usize::MAX))
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
            let abs = match resolve(ctx, rel) {
                Ok(abs) => abs,
                Err(refusal) => return (refusal, format!("outline {rel}"), Vec::new()),
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
            if let Err(refusal) = resolve(ctx, rel) {
                return (refusal, format!("history {rel}"), Vec::new());
            }
            let commits = match git::file_history(&ctx.root, rel, 15) {
                Ok(commits) => commits,
                // Said as it is: "no history" would be a false answer.
                Err(e) => {
                    return (
                        format!("git history unavailable: {e}"),
                        format!("history {rel}"),
                        Vec::new(),
                    );
                }
            };
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let n = commits.len();
            let content = if commits.is_empty() {
                "no git history (the file has no commits)".to_string()
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
            // Checked: a query vector of another length than the index's
            // means another embedding model — ranking it anyway returns
            // confident nonsense, so the model is told to search instead.
            let hits = match embed::search_checked(index, &qvec, 10) {
                Ok(hits) => hits,
                Err(e) => {
                    return (
                        format!("semantic index unusable ({e}) — use `search` instead"),
                        format!("find \"{query}\" (index mismatch)"),
                        Vec::new(),
                    );
                }
            };
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

/// Embed one query for `semantic_find`, given up on a Stop of the turn or at
/// [`EMBED_QUERY_TIMEOUT`], whichever comes first.
///
/// Given up is the request itself, not only the turn's wait for it:
/// `embed_batch_cancellable` runs it on a worker whose connection a stop
/// shuts down, wherever the request is — the connect, the wait for the
/// answer, the answer coming in — so the turn and the worker let go at once,
/// and the endpoint sees the connection close. The wait used to be the
/// turn's alone: its own thread ran a POST nothing could reach, and a Stop,
/// or the deadline, left that thread and the socket held until the endpoint
/// answered or the request's own limits ran out.
fn embed_query(cfg: &embed::Config, query: &str, stop: &AtomicBool) -> Result<Vec<f32>, String> {
    embed_query_within(cfg, query, stop, EMBED_QUERY_TIMEOUT)
}

/// [`embed_query`], given up after `limit` — [`EMBED_QUERY_TIMEOUT`], or a
/// test's short one.
fn embed_query_within(
    cfg: &embed::Config,
    query: &str,
    stop: &AtomicBool,
    limit: std::time::Duration,
) -> Result<Vec<f32>, String> {
    let deadline = std::time::Instant::now() + limit;
    let stopped = || stop.load(Ordering::Relaxed);
    let result = embed::embed_batch_cancellable(cfg, &[query.to_string()], &|| {
        stopped() || std::time::Instant::now() >= deadline
    });
    match result {
        Ok(mut vecs) if !vecs.is_empty() => Ok(vecs.remove(0)),
        Ok(_) => Err("the endpoint returned no vector".into()),
        // Given up here: say which of the two ended it.
        Err(e) if e == llm::CANCELLED => Err(if stopped() {
            "stopped".into()
        } else {
            format!("no answer in {limit:?}")
        }),
        Err(e) => Err(e),
    }
}

/// Resolve a model-named path inside the project, or the tool result saying
/// why not. Tool arguments are model-written and treated as untrusted, so this
/// is the same confinement the server applies to client paths — literally the
/// same one ([`clew_core::confine`]); the agent used to carry its own copy,
/// which had drifted from the server's. The path is CANONICAL, so downstream
/// consumers (the language-server tools especially) agree with what external
/// processes report for the same file even under a symlinked root.
fn resolve(ctx: &Ctx, rel: &str) -> Result<PathBuf, String> {
    clew_core::confine::confine(&ctx.root, rel).map_err(|e| {
        if e.is_escape() {
            format!("refused: path escapes the project: {rel}")
        } else {
            format!("cannot open {rel}: {e}")
        }
    })
}

/// A model-written non-negative integer argument, read leniently: models send
/// `12`, `12.0` and `"12"` for the same line number, and a strict `as_u64`
/// read the last two as "missing". Negative or non-numeric values are `None`
/// (the caller's default or refusal applies); fractions round down.
fn uint_arg(args: &serde_json::Value, key: &str) -> Option<u64> {
    let from_f64 = |f: f64| (f.is_finite() && f >= 0.0).then(|| f.floor() as u64);
    match args.get(key)? {
        serde_json::Value::Number(n) => n.as_u64().or_else(|| n.as_f64().and_then(from_f64)),
        serde_json::Value::String(s) => {
            let s = s.trim();
            s.parse::<u64>()
                .ok()
                .or_else(|| s.parse::<f64>().ok().and_then(from_f64))
        }
        _ => None,
    }
}

/// A model-written boolean argument, read leniently (`true`, `"true"`, `1`).
fn bool_arg(args: &serde_json::Value, key: &str) -> Option<bool> {
    match args.get(key)? {
        serde_json::Value::Bool(b) => Some(*b),
        serde_json::Value::Number(n) => n.as_u64().map(|n| n != 0),
        serde_json::Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" | "1" => Some(true),
            "false" | "no" | "0" => Some(false),
            _ => None,
        },
        _ => None,
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::Scratch;

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
    /// panel kept spinning, and the turn closed a whole step late.
    ///
    /// The provider here starts answering, flips the flag — a Stop landing
    /// while the step streams — and then keeps the step open for ten seconds.
    /// "Closed as stopped" alone would prove nothing (`run` reports Stopped
    /// whenever the flag is set at the end); what shows the Stop REACHED the
    /// step is that the turn returns long before the step would have ended,
    /// that the step's connection is dropped under the provider, and that no
    /// further step is asked for.
    #[test]
    fn a_stop_reaches_an_exploration_step_already_on_the_wire() {
        use std::time::{Duration, Instant};
        let stop = Arc::new(AtomicBool::new(false));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (req_tx, req_rx) = std::sync::mpsc::channel();
        let (cut_tx, cut_rx) = std::sync::mpsc::channel();
        let flag = stop.clone();
        let provider = std::thread::spawn(move || {
            let Ok((mut conn, _)) = listener.accept() else {
                return false;
            };
            // Read the WHOLE request before answering: a step's body is several
            // KB (system prompt + tool schemas) and arrives in many segments,
            // and answering mid-write resets the connection instead of
            // delivering the response.
            let _ = req_tx.send(clew_core::testutil::read_http_request(&mut conn));
            // Start answering; the body runs until the connection closes.
            let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                        connection: close\r\n\r\n\
                        data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n";
            let _ = std::io::Write::write_all(&mut conn, head.as_bytes());
            // "Stop" pressed while the step is still streaming.
            flag.store(true, Ordering::Relaxed);
            // Keep the step open, a ping every 100 ms, for ten seconds —
            // unless the stopped step drops the connection first.
            let began = Instant::now();
            let mut cut = false;
            while began.elapsed() < Duration::from_secs(10) {
                std::thread::sleep(Duration::from_millis(100));
                if std::io::Write::write_all(&mut conn, b": ping\n\n").is_err() {
                    cut = true;
                    break;
                }
            }
            let _ = cut_tx.send(cut);
            // A step asked for after the Stop would be waiting to be accepted.
            listener.set_nonblocking(true).unwrap();
            listener.accept().is_ok()
        });

        let dir = Scratch::new("agent-stop");
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (out, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let began = Instant::now();
        run(
            dir.to_path_buf(),
            Arc::new(Vec::new()),
            llm::Config::from_parts(llm::Provider::Custom, "k".into(), "m".into(), base),
            None,
            Arc::new(LspPool::new(dir.to_path_buf(), Default::default())),
            rt.handle().clone(),
            7,
            "why?".into(),
            Vec::new(),
            String::new(),
            &out,
            &crate::OutputBudget::new(),
            &stop,
        );
        let took = began.elapsed();

        let body = req_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the step was sent");
        assert!(
            body.contains("\"stream\":true"),
            "an exploration step must go out streamed, or nothing can cancel it: {body}"
        );
        assert!(
            took < Duration::from_secs(5),
            "the turn waited for the stopped step to finish: {took:?}"
        );
        let cut = cut_rx
            .recv_timeout(Duration::from_secs(15))
            .expect("the provider reports");
        assert!(cut, "the stopped step's connection stayed open");
        assert!(
            !provider.join().unwrap(),
            "the turn asked for another step after the Stop"
        );
        let mut done = None;
        while let Ok(ServerMessage::Notification { event, .. }) = rx.try_recv() {
            if let Event::AgentDone { outcome, .. } = event {
                done = Some(outcome);
            }
        }
        assert_eq!(
            done,
            Some(StreamOutcome::Stopped),
            "the turn closes as stopped, not as a failed request"
        );
    }

    /// The turn's streamed text is held by the output budget — with a full
    /// queue ahead none of it goes out — and a Stop still ends the turn while
    /// a delta waits there, instead of the step generating (and billing) on
    /// until the link drained.
    #[test]
    fn a_stop_reaches_a_turn_parked_on_the_output_budget() {
        use std::time::{Duration, Instant};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (answered_tx, answered) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let Ok((mut conn, _)) = listener.accept() else {
                return;
            };
            clew_core::testutil::read_http_request(&mut conn);
            // A step that answers in prose: the turn's answer, delivered to
            // the panel as deltas.
            let answer = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                          connection: close\r\n\r\n\
                          data: {\"choices\":[{\"delta\":{\"content\":\"the answer\"}}]}\n\n\
                          data: [DONE]\n\n";
            let _ = std::io::Write::write_all(&mut conn, answer.as_bytes());
            let _ = answered_tx.send(());
        });
        let budget = crate::OutputBudget::new();
        assert!(budget.charge_blocking_unless(crate::OutputBudget::CAP + 1, &|| false));
        let stop = Arc::new(AtomicBool::new(false));
        let (setter, freer) = (stop.clone(), budget.clone());
        std::thread::spawn(move || {
            // Stop once the answer is in and its first delta waits on the
            // full queue.
            if answered.recv_timeout(Duration::from_secs(20)).is_ok() {
                std::thread::sleep(Duration::from_millis(300));
            }
            setter.store(true, Ordering::Relaxed);
            // A turn that ignored the stop is let go later, so the test ends.
            std::thread::sleep(Duration::from_secs(8));
            freer.release(crate::OutputBudget::CAP + 1);
        });
        let dir = Scratch::new("agent-stop-parked");
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (out, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let began = Instant::now();
        run(
            dir.to_path_buf(),
            Arc::new(Vec::new()),
            llm::Config::from_parts(llm::Provider::Custom, "k".into(), "m".into(), base),
            None,
            Arc::new(LspPool::new(dir.to_path_buf(), Default::default())),
            rt.handle().clone(),
            8,
            "why?".into(),
            Vec::new(),
            String::new(),
            &out,
            &budget,
            &stop,
        );
        let took = began.elapsed();
        assert!(
            took < Duration::from_secs(6),
            "the stopped turn waited for the queue: {took:?}"
        );
        let mut done = None;
        while let Ok(ServerMessage::Notification { event, .. }) = rx.try_recv() {
            match event {
                Event::AgentDelta { .. } => panic!("a delta went out past a full queue"),
                Event::AgentDone { outcome, .. } => done = Some(outcome),
                _ => {}
            }
        }
        assert_eq!(done, Some(StreamOutcome::Stopped));
    }

    #[test]
    fn read_tool_windows_and_numbers_lines() {
        let dir = Scratch::new("agent-read");
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
        let dir = Scratch::new("agent-read-edge");
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
    /// down: an unwind out of a tool ends `agent::run`, which the request loop
    /// can only report as a failed turn (`lib.rs` awaits it and sends
    /// `AgentDone` with the failure), losing every step taken. (The panic below
    /// still prints through the default hook — that output is expected.)
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

    /// An embeddings endpoint that accepts the request and then says nothing
    /// used to park the turn inside `embed_batch` forever (its agent had no
    /// read timeout then), so `AgentDone` never went out and Stop — which
    /// only sets this flag — could not reach it. Then the turn's wait polled
    /// the flag, and let go; the request itself, on a thread of its own that
    /// nothing reached, held that thread and the connection until the
    /// endpoint answered or the request's own limits ran out — after a Stop,
    /// and after the turn's deadline alike. Either now gives the request up:
    /// the turn lets go at once, and the endpoint sees the connection close.
    #[test]
    fn a_stalled_embeddings_endpoint_yields_to_stop() {
        use std::time::{Duration, Instant};
        let cfg = |base_url: String| embed::Config {
            api_key: "k".into(),
            model: "m".into(),
            base_url,
        };
        // A Stop once the request is out, waiting on the endpoint.
        let (base, arrived, closed) = clew_core::testutil::silent_http_endpoint();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let (stopped_tx, stopped_at) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            if arrived.recv_timeout(Duration::from_secs(10)).is_ok() {
                flag.store(true, Ordering::Relaxed);
                let _ = stopped_tx.send(Instant::now());
            }
        });
        let err = embed_query(&cfg(base), "why?", &stop).unwrap_err();
        let returned_at = Instant::now();
        assert_eq!(err, "stopped");
        // Timed from the Stop: the first request of a process also loads the
        // system's trust store, slow on a busy machine, before it goes out.
        let stop_at = stopped_at.recv().expect("the request reached the endpoint");
        assert!(
            returned_at.saturating_duration_since(stop_at) < Duration::from_secs(2),
            "the Stop has to land while the request is still outstanding, not after it dies: {:?}",
            returned_at.saturating_duration_since(stop_at)
        );
        let (gone, at) = closed
            .recv_timeout(Duration::from_secs(5))
            .expect("the Stop never reached the request's connection");
        assert!(gone);
        assert!(
            at.saturating_duration_since(stop_at) < Duration::from_secs(2),
            "the connection closed {:?} after the Stop",
            at.saturating_duration_since(stop_at)
        );

        // The turn's deadline passing: the same.
        let (base, arrived, closed) = clew_core::testutil::silent_http_endpoint();
        let limit = Duration::from_secs(2);
        let never = AtomicBool::new(false);
        let err = embed_query_within(&cfg(base), "why?", &never, limit).unwrap_err();
        let late_at = Instant::now();
        assert_eq!(err, "no answer in 2s");
        assert!(arrived.try_recv().is_ok(), "the request never went out");
        let (gone, at) = closed
            .recv_timeout(Duration::from_secs(5))
            .expect("the deadline never reached the request's connection");
        assert!(gone);
        assert!(
            at.saturating_duration_since(late_at) < Duration::from_secs(2),
            "the connection closed {:?} after the deadline",
            at.saturating_duration_since(late_at)
        );
    }

    #[test]
    fn read_tool_refuses_escapes() {
        let dir = Scratch::new("agent-confine");
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
        let dir = Scratch::new("agent-files");
        let ctx = ctx_for(&dir, &["src/a.rs", "src/b.rs", "docs/c.md"]);
        let (content, title, _, _) =
            exec_tool(&ctx, "files", &serde_json::json!({ "prefix": "src/" }));
        assert!(content.contains("src/a.rs") && content.contains("src/b.rs"));
        assert!(!content.contains("docs/c.md"));
        assert_eq!(title, "files src/ → 2");
    }

    #[test]
    fn duplicate_and_unknown_tools_answer_in_text() {
        let dir = Scratch::new("agent-unknown");
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

    // -- a scripted provider ---------------------------------------------------

    /// A scripted model provider on 127.0.0.1: one canned HTTP response per
    /// request, in order, and a plain close for any request past the script.
    /// It serves until [`Provider::requests`] is called; a turn is
    /// synchronous, so by then every request it made has been accepted and is
    /// counted — including one that should never have been made.
    struct Provider {
        base: String,
        stop: Arc<AtomicBool>,
        handle: std::thread::JoinHandle<Vec<String>>,
    }

    impl Provider {
        fn new(responses: Vec<String>) -> Provider {
            Provider::reading_within(responses, clew_core::testutil::HTTP_READ_BOUND)
        }

        /// [`Provider::new`], waiting at most `bound` for each read of a
        /// request: for a test about one that stops short.
        fn reading_within(responses: Vec<String>, bound: std::time::Duration) -> Provider {
            use std::io::Write;
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let stop = Arc::new(AtomicBool::new(false));
            let stopped = stop.clone();
            let handle = std::thread::spawn(move || {
                let mut bodies = Vec::new();
                let mut responses = responses.into_iter();
                // A safety net only: a test that never asks still ends.
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
                loop {
                    // Read before the accept: a request whose connection was
                    // made before `requests` was called is then already
                    // queued, and taken, however late this thread comes to
                    // it. Read after, it could see the stop past a queue
                    // that had been empty a moment before, and leave that
                    // request out.
                    let asked = stopped.load(Ordering::SeqCst);
                    let mut conn = match listener.accept() {
                        Ok((conn, _)) => conn,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            if asked || std::time::Instant::now() >= deadline {
                                break;
                            }
                            std::thread::sleep(std::time::Duration::from_millis(5));
                            continue;
                        }
                        Err(_) => break,
                    };
                    conn.set_nonblocking(false).unwrap();
                    // The whole request before answering: a step's body is
                    // several KB, and answering mid-write resets it. Bounded,
                    // so a request that stops short of its length ends the
                    // read instead of hanging `requests`, which joins this
                    // thread.
                    let request = clew_core::testutil::read_http_within(&mut conn, bound);
                    bodies.push(request.body);
                    if let Some(resp) = responses.next() {
                        let _ = conn.write_all(resp.as_bytes());
                        let _ = conn.flush();
                    }
                }
                bodies
            });
            Provider { base, stop, handle }
        }

        /// The request bodies it was sent. Call once the turn is over.
        fn requests(self) -> Vec<String> {
            self.stop.store(true, Ordering::SeqCst);
            self.handle.join().unwrap()
        }
    }

    /// A request that stops short of its `Content-Length` on a connection
    /// its client keeps open — a turn that failed mid-send — ends the
    /// provider's read at its bound, so the test fails on what arrived. The
    /// provider used to read with no bound at all, and `requests`, which
    /// joins its thread, hung for good.
    ///
    /// The bound is seconds, not milliseconds: it is what a read waits for
    /// the next bytes, and a busy machine that took longer than a bound of
    /// 200 ms to deliver the part that did come ended the read before it —
    /// the test then failed on an empty request. Seconds still end the read
    /// well inside the wait below; an unbounded one never does.
    #[test]
    fn a_short_request_fails_the_provider_fast() {
        use std::io::Write;
        let provider = Provider::reading_within(Vec::new(), std::time::Duration::from_secs(2));
        let addr = provider.base.trim_start_matches("http://").to_string();
        let mut client = std::net::TcpStream::connect(addr).unwrap();
        client
            .write_all(
                b"POST /v1/chat/completions HTTP/1.1\r\ncontent-length: 64\r\n\r\n{\"model\"",
            )
            .unwrap();
        let (done_tx, done) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = done_tx.send(provider.requests());
        });
        let bodies = done
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("a request that stopped short hung the provider");
        assert_eq!(bodies, ["{\"model\""]);
        drop(client);
    }

    /// A complete plain HTTP response.
    fn http(status: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\nconnection: close\r\n\
             content-length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    /// A complete SSE response (OpenAI-compatible) carrying `events`.
    fn sse(events: &[serde_json::Value]) -> String {
        let mut body: String = events.iter().map(|e| format!("data: {e}\n\n")).collect();
        body.push_str("data: [DONE]\n\n");
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\
             content-length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    /// A stream the connection drops in the middle of: headers and `events`,
    /// then EOF with no terminator — a transport failure after the provider
    /// accepted (and may be generating against) the request.
    fn sse_cut(events: &[serde_json::Value]) -> String {
        let body: String = events.iter().map(|e| format!("data: {e}\n\n")).collect();
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{body}"
        )
    }

    fn text_event(text: &str, finish: Option<&str>) -> serde_json::Value {
        serde_json::json!({ "choices": [{ "delta": { "content": text }, "finish_reason": finish }] })
    }

    fn call_event(index: u64, name: &str, args: serde_json::Value) -> serde_json::Value {
        serde_json::json!({ "choices": [{ "delta": { "tool_calls": [{
            "index": index, "id": format!("call_{index}_{name}"),
            "function": { "name": name, "arguments": args.to_string() },
        }] } }] })
    }

    /// A step whose reply is prose.
    fn prose(text: &str) -> String {
        sse(&[text_event(text, Some("stop"))])
    }

    /// A step whose reply is tool calls.
    fn calls(list: &[(&str, serde_json::Value)]) -> String {
        let mut events: Vec<serde_json::Value> = list
            .iter()
            .enumerate()
            .map(|(i, (name, args))| call_event(i as u64, name, args.clone()))
            .collect();
        events.push(
            serde_json::json!({ "choices": [{ "delta": {}, "finish_reason": "tool_calls" }] }),
        );
        sse(&events)
    }

    /// What a turn told the client.
    #[derive(Debug, Default)]
    struct Outcome {
        steps: Vec<String>,
        answer: String,
        /// How the one `AgentDone` said the turn ended.
        done: Option<StreamOutcome>,
    }

    impl Outcome {
        /// The failure the turn closed with; panics on any other ending.
        fn failure(&self) -> &str {
            match &self.done {
                Some(StreamOutcome::Failed(message)) => message,
                other => panic!("expected the turn to fail, it ended {other:?}"),
            }
        }
    }

    /// A small project to explore, removed when the test is done with it.
    fn project(tag: &str) -> Scratch {
        let dir = Scratch::new(&format!("agent-{tag}"));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("a.rs"), "pub fn answer() -> u32 {\n    42\n}\n").unwrap();
        dir
    }

    /// Run one turn against `provider`, returning what reached the client.
    fn turn(dir: &std::path::Path, provider: &Provider, history: Vec<AiChatMsg>) -> Outcome {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (out, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let stop = AtomicBool::new(false);
        run(
            dir.to_path_buf(),
            Arc::new(vec![FileEntry {
                abs: dir.join("a.rs"),
                rel: "a.rs".into(),
            }]),
            llm::Config::from_parts(
                llm::Provider::Custom,
                "k".into(),
                "m".into(),
                provider.base.clone(),
            ),
            None,
            Arc::new(LspPool::new(dir.to_path_buf(), Default::default())),
            rt.handle().clone(),
            3,
            "what does answer return?".into(),
            history,
            String::new(),
            &out,
            &crate::OutputBudget::new(),
            &stop,
        );
        let mut outcome = Outcome::default();
        while let Ok(ServerMessage::Notification { event, .. }) = rx.try_recv() {
            match event {
                Event::AgentStep { title, .. } => outcome.steps.push(title),
                Event::AgentDelta { text, .. } => outcome.answer.push_str(&text),
                Event::AgentDone { outcome: done, .. } => {
                    assert!(outcome.done.is_none(), "exactly one AgentDone");
                    outcome.done = Some(done);
                }
                _ => {}
            }
        }
        outcome
    }

    // -- C1: no resend after the request reached the provider ------------------

    /// The closing answer's stream dropped before its first token. The old
    /// loop took "nothing was sent yet" to mean "the stream never started"
    /// and asked again — but the provider had accepted that request and may
    /// have been generating against it, so the resend was billed twice. Only
    /// a status that REFUSED the stream licenses another request; a transport
    /// failure ends the turn with its reason.
    #[test]
    fn a_transport_failure_in_the_answer_is_reported_not_resent() {
        let dir = project("answer-cut");
        let provider = Provider::new(vec![
            calls(&[("answer", serde_json::json!({}))]),
            sse_cut(&[]),
        ]);
        let outcome = turn(&dir, &provider, Vec::new());
        let error = outcome.failure();
        assert!(error.contains("ended before completion"), "{error}");
        assert_eq!(provider.requests().len(), 2, "no third request");
    }

    /// Same for an exploration step that dies mid-stream.
    #[test]
    fn a_transport_failure_mid_step_ends_the_turn() {
        let dir = project("step-cut");
        let provider = Provider::new(vec![sse_cut(&[text_event("Let me look", None)])]);
        let outcome = turn(&dir, &provider, Vec::new());
        outcome.failure();
        assert_eq!(provider.requests().len(), 1, "the step is not sent again");
    }

    /// An endpoint that refuses `tool_choice: "none"` with a 400 refused the
    /// request before generating anything, so asking again differently is
    /// safe: the answer then comes from an ordinary (unpinned) step.
    #[test]
    fn a_refused_pinned_answer_falls_back_to_an_ordinary_step() {
        let dir = project("answer-refused");
        let provider = Provider::new(vec![
            calls(&[("answer", serde_json::json!({}))]),
            http(
                "400 Bad Request",
                r#"{"error":{"message":"tool_choice none is not supported"}}"#,
            ),
            prose("It returns 42 (a.rs:2)."),
        ]);
        let outcome = turn(&dir, &provider, Vec::new());
        assert_eq!(outcome.done, Some(StreamOutcome::Done), "{outcome:?}");
        assert_eq!(outcome.answer, "It returns 42 (a.rs:2).");
        let requests = provider.requests();
        assert_eq!(requests.len(), 3);
        assert!(requests[1].contains("\"tool_choice\":\"none\""));
        assert!(
            !requests[2].contains("tool_choice"),
            "the fallback is unpinned"
        );
    }

    // -- C5: tool calls where prose was asked for ------------------------------

    /// Some OpenAI-compatible endpoints ignore `tool_choice: none` and stream a
    /// tool call as the closing answer. That used to read as an empty answer
    /// ("the model returned an empty answer"); the call is now run and the
    /// model keeps exploring while budget remains.
    #[test]
    fn a_closing_answer_that_calls_a_tool_keeps_exploring() {
        let dir = project("answer-calls");
        let provider = Provider::new(vec![
            calls(&[("answer", serde_json::json!({}))]),
            calls(&[("read", serde_json::json!({ "file": "a.rs" }))]),
            prose("It returns 42."),
        ]);
        let outcome = turn(&dir, &provider, Vec::new());
        assert_eq!(outcome.done, Some(StreamOutcome::Done), "{outcome:?}");
        assert_eq!(outcome.answer, "It returns 42.");
        assert!(
            outcome.steps.iter().any(|s| s.starts_with("read a.rs")),
            "the call was run: {:?}",
            outcome.steps
        );
        assert_eq!(provider.requests().len(), 3);
    }

    /// An empty reply is asked about once more before it is reported — and
    /// the report says why the provider stopped.
    #[test]
    fn an_empty_reply_is_retried_once_then_reported() {
        let dir = project("empty");
        let provider = Provider::new(vec![prose(""), prose("It returns 42.")]);
        let outcome = turn(&dir, &provider, Vec::new());
        assert_eq!(outcome.done, Some(StreamOutcome::Done));
        assert_eq!(outcome.answer, "It returns 42.");
        assert_eq!(provider.requests().len(), 2);

        let provider = Provider::new(vec![prose(""), prose("")]);
        let outcome = turn(&dir, &provider, Vec::new());
        let error = outcome.failure();
        assert!(
            error.contains("empty answer") && error.contains("stop"),
            "{error}"
        );
        assert_eq!(provider.requests().len(), 2, "one retry, not a loop");
    }

    /// A prose answer cut at the output budget is re-asked with a doubled
    /// budget up to the ceiling; truncated even there, it is delivered —
    /// marked as truncated, never passed off as complete.
    #[test]
    fn a_truncated_prose_step_is_delivered_with_a_note() {
        let dir = project("truncated-ceiling");
        let cut = || sse(&[text_event("It returns", Some("length"))]);
        // STEP_TOKENS doubles up to STEP_TOKENS_CEIL: 4000 → 8000 → 16000.
        let provider = Provider::new(vec![cut(), cut(), cut()]);
        let outcome = turn(&dir, &provider, Vec::new());
        assert_eq!(outcome.done, Some(StreamOutcome::Done), "{outcome:?}");
        assert!(outcome.answer.starts_with("It returns"));
        assert!(
            outcome.answer.ends_with(llm::TRUNCATED_NOTE),
            "{}",
            outcome.answer
        );
        let requests = provider.requests();
        assert_eq!(requests.len(), 3);
        assert!(
            requests[2].contains("16000"),
            "the last try used the ceiling"
        );
    }

    // -- C16: a client that is gone stops the turn -----------------------------

    /// When the transport can no longer carry the turn's notifications the
    /// client is gone, and the turn must stop instead of exploring (and
    /// paying for model calls) for nobody.
    #[test]
    fn a_turn_whose_client_is_gone_stops() {
        let dir = project("gone");
        let provider = Provider::new(vec![
            calls(&[("files", serde_json::json!({}))]),
            calls(&[("files", serde_json::json!({ "prefix": "src/" }))]),
            prose("done"),
        ]);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (out, rx) = tokio::sync::mpsc::unbounded_channel();
        drop(rx);
        let stop = AtomicBool::new(false);
        run(
            dir.to_path_buf(),
            Arc::new(Vec::new()),
            llm::Config::from_parts(
                llm::Provider::Custom,
                "k".into(),
                "m".into(),
                provider.base.clone(),
            ),
            None,
            Arc::new(LspPool::new(dir.to_path_buf(), Default::default())),
            rt.handle().clone(),
            4,
            "q".into(),
            Vec::new(),
            String::new(),
            &out,
            &crate::OutputBudget::new(),
            &stop,
        );
        assert!(stop.load(Ordering::Relaxed), "the lost client set the stop");
        assert_eq!(
            provider.requests().len(),
            1,
            "no step after the client left"
        );
    }

    // -- C2: context accounting ------------------------------------------------

    fn test_turn<'a>(
        ctx: &'a Ctx<'a>,
        chat: &'a llm::Config,
        notify: &'a dyn Fn(Event),
    ) -> Turn<'a> {
        Turn {
            system: "system".into(),
            tools: tool_defs(),
            ctx,
            chat,
            msgs: vec![llm::AgentMsg::User("question".into())],
            seen: HashMap::new(),
            stream: 1,
            notify,
        }
    }

    fn call(i: usize) -> llm::ToolCall {
        llm::ToolCall {
            id: format!("c{i}"),
            name: "read".into(),
            args: serde_json::json!({ "file": format!("f{i}.rs") }),
        }
    }

    /// Every step re-sends the whole conversation, so a long exploration grew
    /// until the provider rejected it — after paying for every step before.
    /// Past the soft budget the OLDEST tool results are elided (the newest
    /// step's worth never), and the repeat guard forgets them so the model may
    /// fetch one again.
    #[test]
    fn old_tool_results_are_elided_past_the_budget() {
        let dir = project("context");
        let ctx = ctx_for(&dir, &[]);
        let chat = llm::Config::from_parts(
            llm::Provider::Custom,
            "k".into(),
            "m".into(),
            "http://x".into(),
        );
        let notify = |_: Event| {};
        let mut turn = test_turn(&ctx, &chat, &notify);
        let big = "x".repeat(15_000);
        for i in 0..20 {
            turn.msgs.push(llm::AgentMsg::Assistant {
                text: String::new(),
                calls: vec![call(i)],
            });
            turn.seen.insert(format!("read:{i}"), turn.msgs.len());
            turn.msgs.push(llm::AgentMsg::ToolResult {
                call: call(i),
                content: big.clone(),
            });
        }
        let before = conversation_bytes(&turn.system, &turn.tools, &turn.msgs);
        assert!(before > CONTEXT_SOFT_BYTES, "the fixture is over budget");

        assert!(turn.fit_context().is_ok());
        let after = conversation_bytes(&turn.system, &turn.tools, &turn.msgs);
        assert!(after <= CONTEXT_TARGET_BYTES, "{after}");
        let results: Vec<&str> = turn
            .msgs
            .iter()
            .filter_map(|m| match m {
                llm::AgentMsg::ToolResult { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(results[0], ELIDED_RESULT, "the oldest go first");
        assert!(
            results[results.len() - KEEP_RECENT_RESULTS..]
                .iter()
                .all(|r| r.len() == big.len()),
            "the newest step's worth is never elided"
        );
        let elided = results.iter().filter(|r| **r == ELIDED_RESULT).count();
        assert_eq!(
            turn.seen.len(),
            20 - elided,
            "elided results are no longer 'above'"
        );
        // Within budget now: another pass elides nothing more.
        assert!(turn.fit_context().is_ok());
        let again = turn
            .msgs
            .iter()
            .filter(|m| matches!(m, llm::AgentMsg::ToolResult { content, .. } if content == ELIDED_RESULT))
            .count();
        assert_eq!(again, elided);
    }

    /// A conversation that cannot fit even with every result elided — a huge
    /// pasted context — ends the turn BEFORE the request, with a reason,
    /// instead of after a rejected (and billed) one.
    #[test]
    fn a_conversation_too_large_to_send_is_refused_before_sending() {
        let dir = project("context-hard");
        let ctx = ctx_for(&dir, &[]);
        let chat = llm::Config::from_parts(
            llm::Provider::Custom,
            "k".into(),
            "m".into(),
            "http://x".into(),
        );
        let notify = |_: Event| {};
        let mut turn = test_turn(&ctx, &chat, &notify);
        turn.msgs = vec![llm::AgentMsg::User("y".repeat(CONTEXT_HARD_BYTES + 1))];
        match turn.fit_context() {
            Err(Halt::Failed(message)) => assert!(message.contains("too large"), "{message}"),
            _ => panic!("must refuse"),
        }
    }

    /// A step that fans out into dozens of calls runs at most
    /// MAX_CALLS_PER_STEP of them; the rest are answered (every call needs a
    /// result) and not run.
    #[test]
    fn a_step_runs_at_most_the_per_step_cap() {
        let dir = project("fanout");
        let ctx = ctx_for(&dir, &["a.rs"]);
        let chat = llm::Config::from_parts(
            llm::Provider::Custom,
            "k".into(),
            "m".into(),
            "http://x".into(),
        );
        let steps = std::cell::Cell::new(0usize);
        let notify = |event: Event| {
            if matches!(event, Event::AgentStep { .. }) {
                steps.set(steps.get() + 1);
            }
        };
        let mut turn = test_turn(&ctx, &chat, &notify);
        let fanout: Vec<llm::ToolCall> = (0..MAX_CALLS_PER_STEP + 3)
            .map(|i| llm::ToolCall {
                id: format!("c{i}"),
                name: "files".into(),
                args: serde_json::json!({ "prefix": format!("p{i}/") }),
            })
            .collect();
        assert!(matches!(turn.run_calls(String::new(), fanout), Ok(false)));
        assert_eq!(steps.get(), MAX_CALLS_PER_STEP, "only the cap ran");
        let refused = turn
            .msgs
            .iter()
            .filter(|m| matches!(m, llm::AgentMsg::ToolResult { content, .. } if content.starts_with("Not run")))
            .count();
        assert_eq!(refused, 3, "the rest were answered, not run");
    }

    // -- C3: the system prompt stays small -------------------------------------

    /// A flat folder of thousands of files used to put every name into a
    /// prompt that is re-sent with every step.
    #[test]
    fn the_root_listing_is_capped_with_directories_first() {
        let mut files: Vec<FileEntry> = (0..100)
            .map(|i| FileEntry {
                abs: PathBuf::from(format!("/p/f{i:03}.txt")),
                rel: format!("f{i:03}.txt"),
            })
            .collect();
        for dir in ["src", "Docs", "assets"] {
            files.push(FileEntry {
                abs: PathBuf::from(format!("/p/{dir}/x")),
                rel: format!("{dir}/x"),
            });
            files.push(FileEntry {
                abs: PathBuf::from(format!("/p/{dir}/y")),
                rel: format!("{dir}/y"),
            });
        }
        let listing = top_level_listing(&files);
        assert!(
            listing.starts_with("assets/, Docs/, src/, f000.txt"),
            "{listing}"
        );
        assert!(listing.ends_with(", … 43 more"), "{listing}");
        let shown = listing.split(", ").filter(|e| !e.starts_with('…')).count();
        assert_eq!(shown, MAX_TOP_ENTRIES);
        // Small projects are listed whole.
        assert_eq!(top_level_listing(&files[..2]), "f000.txt, f001.txt");
    }

    // -- C9: history replay ----------------------------------------------------

    /// Only answered questions are replayed: an assistant turn with no text (a
    /// failed or stopped turn) is a 400 from the providers, and its question
    /// is dropped with it; an unanswered question superseded by a newer one is
    /// dropped too, so the replay alternates user/assistant.
    #[test]
    fn history_replays_only_answered_questions() {
        let msg = |role: &str, content: &str| AiChatMsg {
            role: role.into(),
            content: content.into(),
        };
        let replay = replay_history(vec![
            msg("user", "q1"),
            msg("assistant", ""),
            msg("user", "q2"),
            msg("assistant", "a2"),
            msg("assistant", "orphan"),
            msg("user", "q3"),
            msg("user", "q4"),
            msg("assistant", "a4"),
            msg("user", "unanswered"),
        ]);
        let shape: Vec<String> = replay
            .iter()
            .map(|m| match m {
                llm::AgentMsg::User(t) => format!("u:{t}"),
                llm::AgentMsg::Assistant { text, .. } => format!("a:{text}"),
                llm::AgentMsg::ToolResult { .. } => "tool".into(),
            })
            .collect();
        assert_eq!(shape, ["u:q2", "a:a2", "u:q4", "a:a4"]);
    }

    // -- C10: lenient tool arguments -------------------------------------------

    #[test]
    fn numeric_and_boolean_arguments_are_read_leniently() {
        let args = serde_json::json!({
            "int": 12, "float": 12.0, "str": "12", "padded": " 12 ", "frac": "12.9",
            "neg": -1, "word": "twelve", "bool": true, "t": "true", "one": 1, "no": "no",
        });
        for key in ["int", "float", "str", "padded", "frac"] {
            assert_eq!(uint_arg(&args, key), Some(12), "{key}");
        }
        for key in ["neg", "word", "bool", "missing"] {
            assert_eq!(uint_arg(&args, key), None, "{key}");
        }
        assert_eq!(bool_arg(&args, "bool"), Some(true));
        assert_eq!(bool_arg(&args, "t"), Some(true));
        assert_eq!(bool_arg(&args, "one"), Some(true));
        assert_eq!(bool_arg(&args, "no"), Some(false));
        assert_eq!(bool_arg(&args, "word"), None);

        // End to end: a window written with strings and floats is honoured
        // instead of silently becoming "from line 1".
        let dir = project("lenient");
        let body: String = (1..=30).map(|i| format!("line {i}\n")).collect();
        std::fs::write(dir.join("b.txt"), body).unwrap();
        let ctx = ctx_for(&dir, &["b.txt"]);
        let (content, title, _, _) = exec_tool(
            &ctx,
            "read",
            &serde_json::json!({ "file": "b.txt", "start_line": "10", "end_line": 12.0 }),
        );
        assert_eq!(title, "read b.txt:10-12", "{content}");
    }

    // -- C13: the shared confinement -------------------------------------------

    /// The tools go through the one confinement predicate the server uses:
    /// a symlink leading out of the project is refused, and a file that is
    /// simply missing says so rather than claiming an escape.
    #[cfg(unix)]
    #[test]
    fn tools_refuse_a_symlink_out_of_the_project() {
        let dir = project("confine-link");
        let outside = Scratch::new("agent-outside");
        std::fs::write(outside.join("secret.txt"), "secret").unwrap();
        std::os::unix::fs::symlink(outside.join("secret.txt"), dir.join("leak.txt")).unwrap();
        let ctx = ctx_for(&dir, &[]);
        for tool in ["read", "outline", "history"] {
            let (content, _, _, _) =
                exec_tool(&ctx, tool, &serde_json::json!({ "file": "leak.txt" }));
            assert!(content.starts_with("refused"), "{tool}: {content}");
            assert!(!content.contains("secret\n"), "{tool} leaked the target");
        }
        let (content, _, _, _) = exec_tool(&ctx, "read", &serde_json::json!({ "file": "nope.rs" }));
        assert!(content.starts_with("cannot open nope.rs"), "{content}");
    }
}
