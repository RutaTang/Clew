//! clew — a code reader.
//!
//! v2: gitignore-aware file tree, virtualized tree-sitter code viewer,
//! fuzzy file finder (Cmd+P), project symbol search (Cmd+T), full-text
//! search (Cmd+Shift+F), navigation history, outline, split view (Cmd+\),
//! line selection + copy (Cmd+C), bookmarks (Cmd+D), go-to-line (:N).

mod analyze;
mod bookmarks;
mod cache;
mod callgraph;
mod codeview;
mod connect;
mod dap;
pub use clew_core::docs;
pub use clew_core::explain;
mod find;
mod finder;
// Moved into the shared `clew-core` crate (used by both the GUI and the headless
// server); re-exported so existing `crate::fs_scan` / `crate::search` paths hold.
pub use clew_core::fs_scan;
pub use clew_core::git;
mod glyph;
mod graphlayout;
mod highlight;
mod icons;
mod imports;
pub use clew_core::inactive;
pub use clew_core::incremental;
mod history;
mod index;
mod keymap;
mod langenv;
pub use clew_core::llm;
pub use clew_core::lsp;
#[cfg(target_os = "macos")]
mod macos;
mod notes;
pub use clew_core::outline;
mod projectcalls;
mod reading;
mod render;
mod resize;
pub use clew_core::search;
mod server;
mod stats;
mod structure;
mod watch;
mod theme;
mod ui;
mod viewer;
mod walkthrough;
pub use clew_core::embed;
mod overview;
mod richmd;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use iced::widget::operation;
use iced::widget::scrollable::{self, AbsoluteOffset};
use iced::{Element, Size, Subscription, Task, keyboard};

use crate::bookmarks::Bookmark;
use crate::finder::{Finder, FinderMode};
use crate::fs_scan::{DirNode, FileEntry, ScanResult};
use crate::highlight::HlLine;
use crate::history::{History, Loc};
use crate::index::SymbolEntry;
use crate::outline::Symbol;
use crate::search::SearchHit;
use crate::viewer::{MAX_FILE_BYTES, Viewer};

pub fn main() -> iced::Result {
    // Frameless: no OS title bar at all — clew draws its own window controls
    // (the red/amber/green buttons) in its toolbar, so dragging from a toolbar
    // button never moves the window (unlike a native full-size-content title
    // bar, which the OS drags from everywhere).
    let window = iced::window::Settings {
        size: Size::new(1280.0, 860.0),
        position: iced::window::Position::Centered,
        decorations: false,
        // A borderless window has square corners; we round them natively (see
        // `macos::round_corners`), which needs the window surface to carry an
        // alpha channel so the clipped-away corners composite over the desktop.
        transparent: true,
        ..iced::window::Settings::default()
    };
    iced::application(App::new, App::update, App::view)
        .title(App::title)
        .theme(App::theme)
        .subscription(App::subscription)
        // Embed the icon font (Nerd Font symbols) for file-type icons.
        .font(icons::FONT_BYTES)
        .window(window)
        .run()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidebarTab {
    Files,
    Search,
    /// Semantic search over the embedding index.
    Semantic,
    Marks,
    /// The navigation history tree (reading trail).
    Trail,
    /// Call hierarchy for the symbol `gc` was invoked on.
    Calls,
    /// Import graph rooted at the active file.
    Imports,
    /// The guided walkthrough: an ordered, code-anchored tour.
    Walk,
    /// Reading notes and per-file "understood" progress.
    Notes,
    /// The project's API documentation surface.
    Docs,
}

/// What the WALK tab's top input does: search the saved library, or generate a
/// new tour from a scope prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalkMode {
    Search,
    Walk,
}

/// The two views that share the collapsible bottom panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BottomTab {
    /// "Ask clew" Q&A.
    Ask,
    /// The debugger (call stack, variables, output).
    Debug,
}

/// A full-screen modal showing a project-wide graph overview.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overlay {
    /// The whole-project call graph (tree-sitter, name-resolved).
    ProjectCalls,
    /// The whole-project import graph.
    ProjectImports,
}

/// A file's name for a compact graph-node label (`client.rs`).
fn file_label(p: &std::path::Path) -> String {
    p.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string()
}

/// Column of `byte` within `line` in the server's position encoding.
fn encode_col(line: &str, byte: usize, utf16: bool) -> usize {
    let byte = byte.min(line.len());
    let prefix = &line[..byte];
    if utf16 {
        prefix.encode_utf16().count()
    } else {
        prefix.len()
    }
}

/// Background LSP call-hierarchy pass, driving a stream of progress messages and
/// a final precise call graph.
///
/// For each function in `query_defs` it prepares a call hierarchy at the name's
/// position and reads its incoming calls (and, for an incremental run, outgoing
/// too), producing symbol-keyed `(caller, callee)` edges kept to the project.
/// The result starts from `base` — for an incremental run, edges touching a
/// `changed` file are dropped first, then re-derived — so only the changed
/// functions are re-queried. Bounded-concurrent, per-request timeout so a wedged
/// server can't hang the pass.
#[allow(clippy::too_many_arguments)]
async fn refine_stream(
    mut output: iced::futures::channel::mpsc::Sender<Message>,
    all_defs: Vec<projectcalls::Def>,
    query_defs: Vec<projectcalls::Def>,
    base: projectcalls::SymEdges,
    changed: Option<HashSet<PathBuf>>,
    clients: HashMap<String, lsp::client::LspClient>,
    root: PathBuf,
    generation: u64,
) {
    use iced::futures::{SinkExt, StreamExt};
    use std::time::Duration;

    // File lines, read once, for locating each function name's column.
    let mut file_lines: HashMap<PathBuf, Vec<String>> = HashMap::new();
    for d in &query_defs {
        file_lines.entry(d.file.clone()).or_insert_with(|| {
            std::fs::read_to_string(&d.file)
                .map(|s| s.lines().map(str::to_string).collect())
                .unwrap_or_default()
        });
    }

    // One query per function; `key` is its symbol identity for edge endpoints.
    let both_dirs = changed.is_some();
    struct Query {
        key: projectcalls::SymKey,
        client: lsp::client::LspClient,
        file: PathBuf,
        line0: usize,
        character: usize,
    }
    let mut queries = Vec::new();
    for d in &query_defs {
        let Some(lang) = highlight::detect(&d.file) else {
            continue;
        };
        let Some(client) = clients.get(lang) else {
            continue;
        };
        let utf16 = client.encoding == lsp::client::PositionEncoding::Utf16;
        let line0 = d.line.saturating_sub(1);
        let character = file_lines
            .get(&d.file)
            .and_then(|lines| lines.get(line0))
            .and_then(|text| text.find(&d.name).map(|b| encode_col(text, b, utf16)))
            .unwrap_or(0);
        queries.push(Query {
            key: (d.file.clone(), d.name.clone()),
            client: client.clone(),
            file: d.file.clone(),
            line0,
            character,
        });
    }

    let total = queries.len();
    let mut stream = iced::futures::stream::iter(queries.into_iter().map(|q| async move {
        // A wedged server must not hang the whole pass.
        let work = async {
            let items = q
                .client
                .prepare_call_hierarchy(&q.file, q.line0, q.character)
                .await;
            let mut incoming = Vec::new();
            let mut outgoing = Vec::new();
            for it in items {
                incoming.extend(q.client.incoming_calls(it.raw.clone()).await);
                if both_dirs {
                    outgoing.extend(q.client.outgoing_calls(it.raw).await);
                }
            }
            (incoming, outgoing)
        };
        let (incoming, outgoing) = tokio::time::timeout(Duration::from_secs(15), work)
            .await
            .unwrap_or_default();
        (q.key, incoming, outgoing)
    }))
    .buffer_unordered(12);

    // Start from the base edge set, dropping edges that touch a changed file
    // (they'll be re-derived from this pass's queries).
    let mut edges: projectcalls::SymEdges = base;
    if let Some(changed) = &changed {
        edges.retain(|((cf, _), (ef, _))| !changed.contains(cf) && !changed.contains(ef));
    }
    let in_project = |p: &Path| p.starts_with(&root);

    let mut done = 0usize;
    while let Some((key, incoming, outgoing)) = stream.next().await {
        for caller in incoming {
            if in_project(&caller.path) {
                edges.insert(((caller.path, caller.name), key.clone()));
            }
        }
        for callee in outgoing {
            if in_project(&callee.path) {
                edges.insert((key.clone(), (callee.path, callee.name)));
            }
        }
        done += 1;
        if done.is_multiple_of(16) || done == total {
            let _ = output
                .send(Message::RefineProgress { generation, done, total })
                .await;
        }
    }

    let graph = projectcalls::ProjectCallGraph::graph_from_sym_edges(all_defs, &edges);
    let _ = output
        .send(Message::ProjectCallsRefined { root, generation, edges, graph })
        .await;
}

/// System prompt for the explain pass.
const EXPLAIN_SYSTEM: &str = "You are an expert code explainer. You are given a \
function, file, or folder, plus concise summaries of what it depends on. Reply \
with a plain-prose explanation of what it does and why it exists — 2 to 4 \
sentences, no preamble, no bullet points, no restating the code.";

/// System prompt for the on-demand per-block walkthrough (the `Explain blocks`
/// drill-down). Unlike [`EXPLAIN_SYSTEM`] this asks for structured Markdown.
const EXPLAIN_BLOCKS_SYSTEM: &str = "You are an expert code explainer. Walk \
through the given function block by block, in the order the code executes. For \
each logical block write a short bold Markdown heading naming what it does, then \
one or two sentences on how and why, quoting key lines with inline code. Be \
precise and concise; do not restate every line. Output GitHub-flavored Markdown.";

/// System prompt for "Ask clew": answers grounded in the retrieved code context.
const ASK_SYSTEM: &str = "You are answering a developer's questions about THIS \
codebase in an ongoing conversation. Earlier turns are included; a follow-up may \
refer to them (\"it\", \"that function\", \"why?\"). Use ONLY the provided code \
context — the most semantically relevant functions and files, each with a summary \
and (for functions) its source — together with the conversation so far. Whenever \
you name a file or function from the context, cite it as a Markdown link so the \
reader can click straight to it, using the path and line from that item's header: \
[name](path#Lline) — e.g. a header `### main — src/main.rs (L68)` becomes \
[main](src/main.rs#L68). Link on first mention rather than using bare backticks. \
If a \"Runtime state\" block is present, the program \
is PAUSED in the debugger — use the live call stack and variable values to \
answer questions about what is happening at that point (e.g. why a variable \
holds its value, or which branch was taken). If the context doesn't contain the \
answer, say so briefly instead of guessing. CRITICAL: do not infer or invent \
control flow, triggers, timing, or mechanisms that are not explicitly shown in \
the provided context — never write things like \"polls periodically\", \"runs on \
a background timer\", or \"the watcher marks it dirty\" unless that exact code is \
in the context. If the context shows WHAT happens but not HOW or WHEN it is \
triggered (or the relevant subsystem clearly isn't among the retrieved files), \
say that the triggering/handling code isn't in the retrieved context rather than \
describing a plausible-sounding mechanism. Be concise and concrete. Output \
GitHub-flavored Markdown.";

/// System prompt for "Why is this here?": explain a line/selection's reason for
/// existing from the commit(s) that introduced it.
const WHY_SYSTEM: &str = "You explain WHY a specific piece of code exists, using \
the commit(s) that introduced or last changed it. You are given the code and, \
for each relevant commit, its message and the change it made to this file. \
Answer the developer's implicit question — why is this here? what problem does \
it solve, or what does it guard against? — concretely and grounded in the commit \
intent. 2 to 4 sentences of GitHub-flavored Markdown. Do not just restate what \
the code obviously does; focus on the WHY. If the commit messages are \
uninformative, say what can be inferred from the change and note the history is \
terse.";

/// System prompt for a time-travel step's "what & why": summarize one commit.
const TIME_WHY_SYSTEM: &str = "You explain a single git commit's change to a \
developer scrubbing through a file's history. Given the commit message and its \
diff for one file, write 1-2 plain-English sentences: WHAT changed and WHY (the \
intent) — grounded ONLY in the diff and message, never invented. Do not restate \
the diff line by line. If the message already gives the reason, use it. If the \
history is terse, say what can be inferred. Plain text, no Markdown headers.";

/// System prompt for "the story of this code block": a narrative of its
/// evolution (a function, struct, enum, class, trait, …).
const TIME_STORY_SYSTEM: &str = "You are telling the story of how ONE code block \
— a function, struct, enum, class, trait, or similar — evolved, for a developer \
trying to understand why it is the way it is. You are given the block's kind and \
name and a reverse-chronological list of the commits that changed it (each with \
its message and diff). Write a short GitHub-flavored Markdown narrative of 3 to 6 \
steps IN CHRONOLOGICAL ORDER (oldest first): what each meaningful change did and \
why, and how it reached its current shape. Be concrete and specific to these \
diffs; skip trivial/formatting commits. Ground every claim in the provided diffs \
— do not invent motivations. Finish with a one-line **Today:** summary of what \
it now is.";

/// Auto-refresh runs at most this often. When watched source files change, the
/// understanding (explanations → semantic index → overview) is refreshed, but a
/// burst of edits coalesces into one pass no sooner than this after the last.
/// A manual (user-initiated) refresh ignores the cooldown.
pub(crate) const AUTO_REFRESH_MIN_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(30);

/// Read the project and assemble the explain engine's inputs: every function's
/// body + signature + call-graph callees, each file's functions + structure, and
/// the folder tree. Blocking; run off the UI thread.
fn gather_explain_inputs(files: Vec<PathBuf>, root: PathBuf) -> explain::Inputs {
    use std::collections::BTreeSet;

    // Read + parse supported files once.
    let mut contents: HashMap<PathBuf, (String, &'static str)> = HashMap::new();
    let mut all_defs: Vec<projectcalls::Def> = Vec::new();
    for f in &files {
        let Some(lang) = highlight::detect(f) else { continue };
        let ok_size = std::fs::metadata(f)
            .map(|m| m.len() <= index::MAX_INDEX_FILE_BYTES)
            .unwrap_or(false);
        if !ok_size {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(f) else { continue };
        for s in outline::extract(&content, lang) {
            all_defs.push(projectcalls::Def {
                name: s.name,
                kind: s.kind,
                file: f.clone(),
                line: s.line,
            });
        }
        contents.insert(f.clone(), (content, lang));
    }

    // Call graph for callee edges (tree-sitter; same-file + unique-name scope).
    let sources: Vec<(PathBuf, String)> =
        contents.iter().map(|(k, (v, _))| (k.clone(), v.clone())).collect();
    let callable = projectcalls::ProjectCallGraph::callable(&all_defs);
    let calls = projectcalls::ProjectCallGraph::build(callable, &sources, &HashMap::new());
    let callee_map = calls.callee_keys();

    let mut functions = Vec::new();
    let mut file_inputs = Vec::new();
    let mut folder_files: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
    let mut folder_subs: HashMap<PathBuf, BTreeSet<PathBuf>> = HashMap::new();
    let mut folders_seen: BTreeSet<PathBuf> = BTreeSet::new();

    for (f, (content, lang)) in &contents {
        let lines: Vec<&str> = content.lines().collect();
        let mut fn_keys = Vec::new();
        let mut types = Vec::new();
        for s in outline::extract(content, lang) {
            if matches!(s.kind.as_str(), "function" | "method") {
                let start = s.line.saturating_sub(1);
                let tag_end = s.end_line.clamp(s.line, lines.len());
                // The outline may tag only the signature (Dart tags
                // `function_signature`, which even for a multi-line signature
                // stops before the body); extend to the body's matching brace so
                // the explanation sees the real body, not a lone header. See
                // fn_body_end (paren-aware, so Dart named-parameter `{ }` inside
                // the parens doesn't cut the body short).
                let end = fn_body_end(&lines, start).map_or(tag_end, |e| e.max(tag_end));
                let body = lines.get(start..end).unwrap_or(&[]).join("\n");
                let signature = lines.get(start).map(|l| l.trim().to_string()).unwrap_or_default();
                let key = (f.clone(), s.name.clone());
                let callees = callee_map.get(&key).cloned().unwrap_or_default();
                functions.push(explain::FnInput {
                    file: f.clone(),
                    name: s.name,
                    signature,
                    body,
                    callees,
                });
                fn_keys.push(key);
            } else {
                types.push(format!("{} {}", s.kind, s.name));
            }
        }
        let imports: Vec<String> =
            index::file_imports(content, lang).into_iter().map(|r| r.module).collect();
        let mut structure = String::new();
        if !types.is_empty() {
            structure.push_str(&format!("Types: {}\n", types.join(", ")));
        }
        if !imports.is_empty() {
            structure.push_str(&format!("Imports: {}", imports.join(", ")));
        }
        file_inputs.push(explain::FileInput { path: f.clone(), functions: fn_keys, structure });

        // Folder tree: register the file's ancestor dirs up to the project root.
        if let Some(parent) = f.parent().filter(|p| p.starts_with(&root)) {
            folder_files.entry(parent.to_path_buf()).or_default().push(f.clone());
        }
        let mut dir = f.parent();
        while let Some(d) = dir {
            if !d.starts_with(&root) {
                break;
            }
            folders_seen.insert(d.to_path_buf());
            if d == root {
                break;
            }
            if let Some(up) = d.parent() {
                folder_subs.entry(up.to_path_buf()).or_default().insert(d.to_path_buf());
            }
            dir = d.parent();
        }
    }

    let folders = folders_seen
        .into_iter()
        .map(|d| explain::FolderInput {
            files: folder_files.get(&d).cloned().unwrap_or_default(),
            subfolders: folder_subs.get(&d).map(|s| s.iter().cloned().collect()).unwrap_or_default(),
            path: d,
        })
        .collect();

    explain::Inputs { functions, files: file_inputs, folders }
}

/// One function's block-detail inputs: its signature, full body, and
/// `(callee_name, summary)` context for the functions it calls.
type FnDetailInput = (String, String, Vec<(String, String)>);

/// Re-read one file and assemble the block-detail inputs for a single function
/// (see [`FnDetailInput`]). Callees are resolved against `summaries`, a
/// unique-name → summary map (ambiguous names are skipped). Runs fresh from disk
/// so it works even before a full Explain pass this session. Blocking; run off
/// the UI thread.
/// The 1-based line just past the `}` that closes the block opened on or after
/// `start` (0-based), or `None` when there's no `{` (e.g. an expression-bodied
/// function). Naive brace counting — best-effort, for extracting a body to show.
/// The exclusive end line of a function *body* whose header begins at `start`.
/// A single char-by-char pass: while still in the parameter list it ignores
/// braces (Dart's named parameters use `{ }` *within* the parens, e.g.
/// `fn(a, { b }) { … }`), so the body opens at the first `{` seen at paren-depth
/// zero; from there it brace-matches to the body's close. Doing it in one pass
/// (rather than finding the open line then rescanning it) avoids miscounting a
/// stray named-parameter `}` that shares the line with the body `{` (`}) async
/// {`). Returns `None` for a bodyless declaration (abstract method / trait
/// signature ending in `;`).
fn fn_body_end(lines: &[&str], start: usize) -> Option<usize> {
    let mut paren = 0i32;
    let mut brace = 0i32;
    let mut in_body = false;
    for (i, line) in lines.iter().enumerate().skip(start) {
        for ch in line.chars() {
            match ch {
                '(' if !in_body => paren += 1,
                ')' if !in_body => paren = (paren - 1).max(0),
                // A '{' inside the parameter list (paren > 0) is a Dart
                // named-parameter group — ignore it until the params close.
                '{' if !in_body && paren == 0 => {
                    in_body = true;
                    brace = 1;
                }
                '{' if in_body => brace += 1,
                '}' if in_body => {
                    brace -= 1;
                    if brace == 0 {
                        return Some(i + 1); // exclusive end for lines[start..end]
                    }
                }
                _ => {}
            }
        }
    }
    None
}

fn gather_fn_detail_input(
    file: PathBuf,
    name: &str,
    summaries: &HashMap<String, Option<String>>,
) -> Option<FnDetailInput> {
    let lang = highlight::detect(&file)?;
    let content = std::fs::read_to_string(&file).ok()?;
    let lines: Vec<&str> = content.lines().collect();

    // Locate the function's span → signature + full body.
    let sym = outline::extract(&content, lang)
        .into_iter()
        .find(|s| s.name == name && matches!(s.kind.as_str(), "function" | "method"))?;
    let start = sym.line.saturating_sub(1);
    let tag_end = sym.end_line.clamp(sym.line, lines.len());
    // The outline may tag only the function *signature*, not the body: Dart tags
    // `function_signature`, which for a wrapped (multi-line) signature spans
    // several lines yet still stops before the `{ … }` body. Always extend to the
    // body's matching brace and take whichever end is larger, so we never send
    // the model just the signature (it then reports "the body is missing").
    let end = fn_body_end(&lines, start).map_or(tag_end, |e| e.max(tag_end));
    let body = lines.get(start..end).unwrap_or(&[]).join("\n");
    let signature = lines.get(start).map(|l| l.trim().to_string()).unwrap_or_default();

    // Callees this function names, with their summaries for context.
    let mut seen: HashSet<String> = HashSet::new();
    let mut callees = Vec::new();
    for cs in projectcalls::calls_of(&content, lang) {
        if cs.caller.as_deref() != Some(name) || !seen.insert(cs.callee.clone()) {
            continue;
        }
        if let Some(Some(sum)) = summaries.get(&cs.callee) {
            callees.push((cs.callee.clone(), sum.clone()));
        }
    }
    Some((signature, body, callees))
}

/// A math/mermaid diagram rendered to an SVG, ready to place in the modal at a
/// fixed logical size.
#[derive(Debug, Clone)]
pub struct ExplainSvg {
    pub handle: iced::widget::svg::Handle,
    pub width: f32,
    pub height: f32,
}

/// One explanation segment prepared for display: markdown is pre-parsed once (not
/// per frame), math/mermaid carry the cache key of their rendered [`ExplainSvg`].
pub enum PreparedSeg {
    Markdown(Vec<iced::widget::markdown::Item>),
    DisplayMath(u64),
    InlineLine(Vec<PreparedInline>),
    /// A mermaid diagram: its render key, and the raw source kept as a fallback
    /// to show when the SVG isn't available (still rendering, or it failed).
    Mermaid(u64, String),
}

/// An inline piece of a text line that mixes prose and inline math.
pub enum PreparedInline {
    Text(String),
    Math(u64),
}

/// One turn in the "Ask clew" conversation.
pub struct AskTurn {
    pub question: String,
    /// Raw markdown answer, replayed to the LLM as history so follow-ups have
    /// the prior exchange in context. Accumulates token-by-token while streaming.
    pub answer_md: String,
    /// The answer rendered as ordered display segments (filled when the stream
    /// finishes; empty while streaming, when `answer_md` is shown as plain text).
    pub answer: Vec<PreparedSeg>,
    /// The retrieved nodes (with similarity scores) that grounded this answer,
    /// shown beneath it as clickable source chips.
    pub sources: Vec<(explain::Node, f32)>,
    /// True while the answer is still streaming in.
    pub streaming: bool,
}

/// One piece of a streaming chat answer, routed from the server's `ChatDelta` /
/// `ChatStreamDone` notifications (or a local stream) to the Ask flow.
pub enum ChatStreamPiece {
    Delta(String),
    /// Stream finished; `Some` carries the error when it failed.
    Done(Option<String>),
}

/// A code selection pinned as extra context for the conversation (added by the
/// code view's "Add to Ask"; shown as a removable, clickable chip above the
/// input). Pins persist across turns until removed, and several can be attached
/// at once so distinct snippets can be asked about together.
#[derive(Debug, Clone)]
pub struct AskPin {
    pub rel: String,
    pub file: PathBuf,
    pub line: usize,
    pub code: String,
}

/// Where a debug session is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebugStatus {
    /// Adapter starting / launching the program.
    Launching,
    /// The debuggee is running (not paused).
    Running,
    /// Paused at a breakpoint / step / exception.
    Stopped,
    /// The debuggee exited or the session ended.
    Terminated,
}

/// One scope of the stopped frame, with its variables loaded.
#[derive(Debug, Clone)]
pub struct DebugScope {
    pub name: String,
    pub vars: Vec<dap::Variable>,
}

/// A breakpoint on a line: unconditional, or stopping only when `condition`
/// (an expression the adapter evaluates in scope) is true.
#[derive(Debug, Clone, Default)]
pub struct Bp {
    pub condition: Option<String>,
}

/// A file's breakpoints as `(line, optional condition)` pairs — the shape the
/// DAP adapter's `setBreakpoints` takes.
type BpList = Vec<(usize, Option<String>)>;

/// Which stepping action to send the adapter.
#[derive(Debug, Clone, Copy)]
pub enum DebugCmd {
    Continue,
    StepOver,
    StepIn,
    StepOut,
}

/// A live debug session: the adapter handle plus the state clew shows (stack,
/// scopes, output, the current stopped line).
pub struct DebugSession {
    /// The adapter handle (None between StartDebug and the adapter being ready).
    pub client: Option<dap::DapClient>,
    pub status: DebugStatus,
    pub thread_id: Option<i64>,
    /// The call stack at the current stop (top frame first).
    pub frames: Vec<dap::StackFrame>,
    pub scopes: Vec<DebugScope>,
    /// Watch expressions re-evaluated on each stop: (expression, value).
    pub watches: Vec<(String, String)>,
    /// Program/adapter output, as (category, text) chunks.
    pub output: Vec<(String, String)>,
    /// The current stopped location (absolute file, 1-based line).
    pub current: Option<(PathBuf, usize)>,
    /// Resolved launch config for this session.
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    /// TCP port of the adapter (js-debug/dlv) — used to open child sessions.
    pub port: Option<u16>,
}

/// A short, readable name for a debug stack frame — the last path segment, with
/// a trailing mangling hash (`::h1a2b3…`) dropped. "main::factorial::hd89…" →
/// "factorial".
pub fn short_frame_name(name: &str) -> String {
    let parts: Vec<&str> = name.split("::").collect();
    let drop_hash = parts.last().is_some_and(|s| {
        s.len() > 3 && s.starts_with('h') && s[1..].chars().all(|c| c.is_ascii_hexdigit())
    });
    let end = if drop_hash { parts.len() - 1 } else { parts.len() };
    parts[..end].last().copied().unwrap_or(name).to_string()
}

/// Resolve a possibly-relative path from the launch config against the root.
fn resolve_rel(root: &Path, p: &str) -> PathBuf {
    let pb = PathBuf::from(p);
    if pb.is_absolute() { pb } else { root.join(pb) }
}

/// A parsed `.clew/launch.json`: what to run and (optionally) which adapter.
struct LaunchConfig {
    program: PathBuf,
    args: Vec<String>,
    cwd: PathBuf,
    /// Optional `"type"` hint (rust/python/go/dart/node) — else inferred.
    type_hint: Option<String>,
}

/// Read `.clew/launch.json`, resolving relative paths against the project root.
/// A missing/invalid file yields a helpful message.
fn read_launch_config(root: &Path) -> Result<LaunchConfig, String> {
    let path = root.join(".clew").join("launch.json");
    let text = std::fs::read_to_string(&path).map_err(|_| {
        format!("Create {} with {{\"program\": \"path\", \"type\": \"python\"}}", path.display())
    })?;
    let v: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("launch.json: {e}"))?;
    let program = v
        .get("program")
        .and_then(|p| p.as_str())
        .ok_or("launch.json needs a \"program\" field")?;
    let args = v
        .get("args")
        .and_then(|a| a.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let cwd = v
        .get("cwd")
        .and_then(|c| c.as_str())
        .map(|c| resolve_rel(root, c))
        .unwrap_or_else(|| root.to_path_buf());
    let type_hint = v.get("type").and_then(|t| t.as_str()).map(str::to_string);
    Ok(LaunchConfig { program: resolve_rel(root, program), args, cwd, type_hint })
}


/// Generate the missing math/mermaid SVGs off-thread, rendering each in-process
/// — RaTeX for math, `mermaid-rs-renderer` for diagrams — with no webview and no
/// helper binary. Each result is recolored/sized and its raw SVG cached on disk.
/// Blocking. (The module map is drawn on a native canvas, not mermaid, so the
/// diagrams reaching here are the smaller ones LLM explanations emit.)
fn generate_svgs(
    missing: Vec<richmd::Renderable>,
    root: PathBuf,
) -> HashMap<u64, richmd::PreparedSvg> {
    let mut out = HashMap::new();
    for r in missing {
        let is_math = r.kind == "math";
        let Some(svg) = (if is_math {
            render::math_svg(&r.src)
        } else {
            render::mermaid_svg(&r.src)
        }) else {
            continue; // unparseable source — skip rather than block the batch
        };
        richmd::store_raw(&root, r.key, &svg);
        out.insert(r.key, richmd::prepare_svg(&svg, is_math));
    }
    out
}

/// (Re)build the embedding index: reuse a node's vector when its summary hash is
/// unchanged, embed the rest. Blocking — run off the UI thread.
async fn build_embeddings(
    ai: &AiClient,
    cfg: &embed::Config,
    nodes: Vec<(explain::Node, String, incremental::Version)>,
    existing: embed::Index,
) -> Result<embed::Index, String> {
    let mut have: HashMap<explain::Node, embed::Entry> =
        existing.entries.into_iter().map(|e| (e.node.clone(), e)).collect();
    let mut entries: Vec<embed::Entry> = Vec::new();
    let mut pending: Vec<(explain::Node, incremental::Version)> = Vec::new();
    let mut texts: Vec<String> = Vec::new();
    for (node, text, hash) in nodes {
        match have.remove(&node) {
            Some(e) if e.hash == hash => entries.push(e),
            _ => {
                pending.push((node, hash));
                texts.push(text);
            }
        }
    }
    let vecs = ai.embed(cfg.clone(), texts).await?;
    for ((node, hash), vec) in pending.into_iter().zip(vecs) {
        entries.push(embed::Entry { node, hash, vec });
    }
    Ok(embed::Index { model: cfg.model.clone(), entries })
}

/// Background explain pass: schedule bottom-up, run each dependency level
/// concurrently (reusing `prev` where the prompt is unchanged, else calling the
/// LLM), streaming progress and the finished cache.
/// True when an explain failure looks like the LLM rejecting the request itself
/// (bad/expired key, no quota) rather than a transient hiccup — every subsequent
/// call would fail identically, so the pass should stop and say so.
fn is_auth_error(err: &str) -> bool {
    let e = err.to_ascii_lowercase();
    e.contains("401")
        || e.contains("403")
        || e.contains("unauthorized")
        || e.contains("authentication")
        || e.contains("invalid api key")
        || e.contains("invalid_api_key")
}

async fn explain_stream(
    mut output: iced::futures::channel::mpsc::Sender<Message>,
    inputs: explain::Inputs,
    prev: explain::Cache,
    cfg: llm::Config,
    ai: AiClient,
    root: PathBuf,
    generation: u64,
) {
    use iced::futures::{SinkExt, StreamExt};

    let groups = explain::schedule(&inputs);
    let levels = explain::levels(&groups);
    let total = groups.len();
    let mut cache = explain::Cache::new();
    let mut done = 0usize;
    let mut failed = 0usize;
    // Set when the LLM rejects the request (bad/expired key, no quota). Once seen
    // we stop the pass rather than firing thousands of calls that will all fail.
    let mut auth_error: Option<String> = None;

    // Show a determinate 0/total at once so the bar appears immediately, then
    // update as items land (see the throttle below).
    let _ = output
        .send(Message::ExplainProgress { generation, done, total, failed })
        .await;
    // Coalesce progress emits to ~10/s. Each emit drives a full UI re-render;
    // firing one after every item on a big repo (thousands of functions) floods
    // the iced event loop and starves it, so interactive LSP requests (hover,
    // go-to-def) queue behind the re-renders and lag to several seconds. When
    // items land slower than the interval every one still emits, so small/medium
    // passes keep their per-item smoothness.
    let mut last_emit = std::time::Instant::now();
    const PROGRESS_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

    'levels: for level in levels {
        // Build each group's prompt from already-finished summaries.
        let jobs: Vec<(usize, String, incremental::Version, Option<String>)> = level
            .iter()
            .map(|&gi| {
                let prompt = explain::prompt_for(&groups[gi], &inputs, &cache);
                let hash = incremental::content_hash(prompt.as_bytes());
                // Reuse a prior summary only if the prompt is unchanged AND it
                // was a real explanation — never carry a failure placeholder
                // forward, so a transient LLM outage retries instead of sticking.
                let reuse = prev
                    .get(groups[gi].key())
                    .filter(|c| c.prompt_hash == hash && !explain::is_error_summary(&c.summary))
                    .map(|c| c.summary.clone());
                (gi, prompt, hash, reuse)
            })
            .collect();

        // Run the level's LLM calls concurrently, folding each result in as it
        // lands so progress advances smoothly. `ok` is false when the call failed,
        // so the failure placeholder is never written to the cache.
        let mut stream = iced::futures::stream::iter(jobs.into_iter().map(
            |(gi, prompt, hash, reuse)| {
                let cfg = cfg.clone();
                let ai = ai.clone();
                async move {
                    let (summary, ok) = match reuse {
                        Some(s) => (s, true),
                        None => {
                            // Retry transient failures (rate limits, network blips)
                            // with exponential backoff so a busy provider doesn't
                            // leave gaps. A rejected key is not transient — surface
                            // it immediately so the pass can stop.
                            let mut outcome = (String::new(), false);
                            for attempt in 0..3u32 {
                                match ai
                                    .complete(cfg.clone(), EXPLAIN_SYSTEM, prompt.clone(), 400)
                                    .await
                                {
                                    Ok(s) => {
                                        outcome = (s, true);
                                        break;
                                    }
                                    Err(e) => {
                                        let auth = is_auth_error(&e);
                                        outcome = (format!("(explanation unavailable: {e})"), false);
                                        if auth || attempt == 2 {
                                            break;
                                        }
                                        tokio::time::sleep(std::time::Duration::from_millis(
                                            500 * (1 << attempt),
                                        ))
                                        .await;
                                    }
                                }
                            }
                            outcome
                        }
                    };
                    (gi, summary, ok, hash)
                }
            },
        ))
        // More in-flight LLM calls to speed the pass; the per-call retry with
        // backoff absorbs the extra rate-limit pressure, and failures are
        // surfaced honestly rather than silently dropped.
        .buffer_unordered(12);

        while let Some((gi, summary, ok, hash)) = stream.next().await {
            // Skip caching failures: leave the node unexplained so the next pass
            // retries it rather than poisoning the cache with an error string.
            if ok {
                for n in &groups[gi].nodes {
                    cache.insert(
                        n.clone(),
                        explain::Cached { summary: summary.clone(), prompt_hash: hash, detail: None },
                    );
                }
            } else {
                failed += 1;
                if auth_error.is_none() && is_auth_error(&summary) {
                    auth_error = Some(summary.clone());
                }
            }
            done += 1;
            // Throttled: emit only once the interval has passed since the last
            // update (or on the very last item), so the UI thread stays free to
            // service interactive LSP requests during a long pass.
            if last_emit.elapsed() >= PROGRESS_INTERVAL || done == total {
                last_emit = std::time::Instant::now();
                let _ = output
                    .send(Message::ExplainProgress { generation, done, total, failed })
                    .await;
            }
        }
        // A rejected key fails every call the same way — abort instead of burning
        // the whole quota, and surface why.
        if auth_error.is_some() {
            break 'levels;
        }
    }

    let _ = output
        .send(Message::ExplainDone { root, generation, cache, failed, auth_error })
        .await;
}

/// The kind of LSP navigation request.
#[derive(Debug, Clone, Copy)]
pub enum GotoKind {
    Definition,
    References,
    Implementation,
    TypeDefinition,
}

/// An active hover tooltip.
#[derive(Clone)]
pub struct HoverState {
    pub line: usize,
    pub col: usize,
    pub x: f32,
    pub y: f32,
    pub text: Option<String>,
    /// The cached one-line LLM summary of the hovered symbol, if it's an
    /// explained function/method. Set synchronously; shown above the LSP text.
    pub summary: Option<String>,
    /// The LSP diagnostic (error/warning) under the cursor, if any — so hovering
    /// a red-underlined symbol shows *what* the problem is, not just the squiggle.
    pub diagnostic: Option<String>,
}

/// The "Why is this here?" popup: an LLM explanation of why a line/selection
/// exists, grounded in the commit(s) that last touched it.
pub struct BlameWhy {
    /// e.g. "Why line 42 exists" / "Why lines 40–48 exist".
    pub title: String,
    /// The cited commits `(short sha, subject)`.
    pub commits: Vec<(String, String)>,
    /// True while the LLM answer is being generated.
    pub loading: bool,
    /// The rendered answer (empty while loading).
    pub prepared: Vec<PreparedSeg>,
}

/// A time-travel session: scrub the active file (or one function's line range)
/// through its git history, viewing each past revision read-only, with the lines
/// that revision changed highlighted in the gutter.
pub struct TimeTravel {
    pub abs: PathBuf,
    pub rel: String,
    pub lang: Option<&'static str>,
    /// Whole file, or scoped to one function's line range (`git log -L`).
    pub scope: TimeScope,
    /// Commits that touched the scope, newest first.
    pub commits: Vec<git::HistCommit>,
    /// Current position in `commits` (0 = newest / most recent).
    pub idx: usize,
    /// The historical content for `commits[idx]`, built read-only.
    pub viewer: Option<viewer::Viewer>,
    /// Scroll offset of the historical view (drives its sticky headers).
    pub scroll_y: f32,
    /// Caret position, carried in from the live file and kept across revisions
    /// (and clicks) so the reader's place doesn't vanish on entry.
    pub caret: Option<(usize, usize)>,
    /// The line to bring into view (symbol scope: the function's line).
    pub focus_line: Option<usize>,
    pub loading: bool,
    /// Bumped on every enter/step so stale async results are dropped.
    pub generation: u64,
    /// LLM "what & why" summary per commit sha (cached across steps).
    pub why: HashMap<String, String>,
    pub why_loading: bool,
    /// The "story of this function" narrative (symbol scope), prepared markdown.
    pub story: Option<Vec<PreparedSeg>>,
    pub story_loading: bool,
}

/// Whether a time-travel session follows the whole file or one code block —
/// any outline symbol with a line range (function, struct, enum, class, trait,
/// interface, impl, …).
#[derive(Debug, Clone)]
pub enum TimeScope {
    File,
    Symbol { name: String, kind: String, start: usize, end: usize },
}

impl TimeScope {
    pub fn symbol_name(&self) -> Option<&str> {
        match self {
            TimeScope::Symbol { name, .. } => Some(name),
            TimeScope::File => None,
        }
    }
}

/// Async-built content for one revision of a time-travel session.
#[derive(Debug, Clone)]
pub struct TimeStep {
    pub lines: Vec<highlight::HlLine>,
    pub content: String,
    pub symbols: Vec<outline::Symbol>,
    pub added: HashSet<usize>,
    pub focus_line: Option<usize>,
}

/// An open right-click navigation menu.
#[derive(Clone, Copy)]
pub struct ContextMenu {
    pub pane: usize,
    pub line: usize,
    pub col: usize,
    pub x: f32,
    pub y: f32,
}

impl GotoKind {
    /// Menu label for this navigation action.
    pub fn label(self) -> &'static str {
        match self {
            GotoKind::Definition => "Go to Definition",
            GotoKind::References => "Find References",
            GotoKind::Implementation => "Go to Implementation",
            GotoKind::TypeDefinition => "Go to Type Definition",
        }
    }

    fn method(self) -> &'static str {
        match self {
            GotoKind::Definition => "textDocument/definition",
            GotoKind::References => "textDocument/references",
            GotoKind::Implementation => "textDocument/implementation",
            GotoKind::TypeDefinition => "textDocument/typeDefinition",
        }
    }
    fn verb(self) -> &'static str {
        match self {
            GotoKind::Definition => "Looking up definition",
            GotoKind::References => "Finding references",
            GotoKind::Implementation => "Finding implementations",
            GotoKind::TypeDefinition => "Looking up type definition",
        }
    }
}

pub struct Project {
    pub root: PathBuf,
    pub tree: DirNode,
    pub files: Arc<Vec<FileEntry>>,
    pub truncated: bool,
}

#[derive(Default)]
pub struct SearchState {
    pub query: String,
    pub running: bool,
    pub ran: bool,
    pub hits: Vec<SearchHit>,
    /// Last search's error (bad regex/glob), shown under the input.
    pub error: Option<String>,
    /// Match options (regex/case/whole-word) and include/exclude globs.
    pub regex: bool,
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub include: String,
    pub exclude: String,
}

/// A toggleable match option in the search sidebar.
#[derive(Debug, Clone, Copy)]
pub enum SearchOpt {
    Regex,
    Case,
    WholeWord,
}

/// The active pane's diff-vs-HEAD, shown in place of the code when set.
pub struct DiffState {
    pub abs: PathBuf,
    pub rel: String,
    pub lines: Vec<git::DiffLine>,
}

/// State of the language server for one language.
pub enum LspSlot {
    Starting,
    Ready(lsp::client::LspClient),
    Failed(String),
    Unsupported(String),
    /// Awaiting the user's consent to download the server (see LspConsent).
    AwaitingConsent,
}

impl LspSlot {
    pub fn label(&self) -> String {
        match self {
            LspSlot::Starting => "starting…".into(),
            // A ready server that surfaced an error (e.g. rust-analyzer couldn't
            // load the workspace) shows it — otherwise it reads "ready" while
            // every go-to-def silently returns nothing. Else: live progress
            // (indexing) when active, else "ready".
            LspSlot::Ready(client) => match client.error() {
                Some(e) => format!("⚠ {}", lsp_error_summary(&e)),
                None => client.progress().unwrap_or_else(|| "ready".into()),
            },
            LspSlot::Failed(e) => format!("error: {e}"),
            LspSlot::Unsupported(e) => e.clone(),
            LspSlot::AwaitingConsent => "download needed".into(),
        }
    }
}

/// A compact, single-line form of a server error for the status bar: the first
/// line, trimmed of any trailing detail after a colon, capped in length.
fn lsp_error_summary(e: &str) -> String {
    let first = e.lines().next().unwrap_or(e).trim();
    // rust-analyzer's message reads "…failed to load workspace: <long detail>";
    // keep the human part before the first colon so the chip stays short.
    let head = first.split_once(':').map(|(h, _)| h).unwrap_or(first).trim();
    let head = if head.is_empty() { first } else { head };
    if head.chars().count() > 64 {
        format!("{}…", head.chars().take(64).collect::<String>())
    } else {
        head.to_string()
    }
}

/// How a pending, consent-gated provisioning will obtain the server.
#[derive(Clone)]
pub enum LspProvision {
    Download(lsp::registry::Download),
    Install(lsp::registry::Install),
}

/// A pending language-server provisioning the user must approve.
#[derive(Clone)]
pub struct LspConsent {
    pub language: String,
    pub server_name: String,
    pub version: String,
    pub provision: LspProvision,
    pub dest_dir: PathBuf,
}

impl LspConsent {
    /// One line describing what running the provisioning will do.
    pub fn describe(&self) -> String {
        match &self.provision {
            LspProvision::Download(d) => d
                .url
                .rsplit('/')
                .next()
                .map(|f| format!("download {f}"))
                .unwrap_or_else(|| "download a binary".into()),
            LspProvision::Install(i) => format!("{} (requires {} on PATH)", i.describe, i.tool),
        }
    }
}

/// The LLM settings modal: whether it's open, plus its draft chat / embedding
/// endpoint fields. Defaults to a closed modal with the Anthropic provider.
pub struct SettingsDraft {
    pub open: bool,
    pub provider: llm::Provider,
    pub key: String,
    pub model: String,
    pub base_url: String,
    pub embed_key: String,
    pub embed_model: String,
    pub embed_base_url: String,
}

impl Default for SettingsDraft {
    fn default() -> Self {
        Self {
            open: false,
            provider: llm::Provider::Anthropic,
            key: String::new(),
            model: String::new(),
            base_url: String::new(),
            embed_key: String::new(),
            embed_model: String::new(),
            embed_base_url: String::new(),
        }
    }
}

/// State for the DOCS tab — the project's API documentation view.
#[derive(Default)]
pub struct DocsState {
    /// The project's API documentation, per file (from the server's `BuildDocs`).
    pub files: Vec<clew_protocol::DocFile>,
    /// A `BuildDocs` is in flight.
    pub loading: bool,
    /// Which files are expanded in the DOCS tree (keys are file rels).
    pub expanded: HashSet<String>,
    /// Filter text for the DOCS tree (matches item names).
    pub filter: String,
    /// Show all symbols vs. only the public API surface (default: public only).
    pub show_all: bool,
    /// Group the Docs tree by module/package instead of by file (default: file).
    pub by_module: bool,
    /// The doc page rendered in the main pane, with the selected item's doc
    /// markdown pre-parsed (the markdown widget borrows it). `None` = no page.
    pub page: Option<DocPage>,
    /// A symbol name whose doc page to open once the index finishes building
    /// (set by "View docs" when the docs aren't built yet).
    pub pending_view: Option<String>,
}

pub struct App {
    pub project: Option<Project>,
    /// File to open automatically once the initial scan completes
    /// (set when the CLI argument is a file path).
    pub pending_open: Option<PathBuf>,
    /// Project root awaiting the user's consent to create `.clew`.
    /// While `Some`, the consent modal is shown.
    pub pending_consent: Option<PathBuf>,
    pub scanning: bool,
    pub sidebar: SidebarTab,
    /// The call hierarchy shown in the Calls sidebar tab, if any.
    pub call_graph: Option<callgraph::CallTree>,
    /// Whole-project file→file import graph, derived from tree-sitter and kept
    /// incrementally fresh; the Imports sidebar tab is a view onto it.
    pub import_graph: imports::ImportGraph,
    /// The import tree currently shown, rooted at the active file.
    pub import_tree: Option<imports::ImportTree>,
    /// Persisted Imports/Importers direction preference across focus changes.
    pub import_dir: imports::Dir,
    /// Import cycles in the project, recomputed when the graph changes (cached so
    /// the sidebar banner doesn't re-run cycle detection every frame).
    pub import_cycles: Vec<Vec<PathBuf>>,
    /// Whole-project symbol call graph (tree-sitter, name-resolved), built lazily
    /// when its overlay opens; drives the project call-graph overlay.
    pub project_calls: projectcalls::ProjectCallGraph,
    /// Registry revision the project call graph was last built at (to rebuild it
    /// only when files actually changed since).
    pub project_calls_rev: u64,
    /// True while the project call graph is being (re)built off-thread.
    pub building_calls: bool,
    /// True when `project_calls` is the exact LSP-resolved graph rather than the
    /// tree-sitter name-based approximation.
    pub project_calls_precise: bool,
    /// Generation counter for LSP-refine runs, so a late result from a superseded
    /// run (new project, re-refine, or a rebuild) is dropped.
    pub calls_gen: u64,
    /// LSP-refine progress `(done, total)` while a refine is running.
    pub refine_progress: Option<(usize, usize)>,
    /// The precise edge set, symbol-keyed, kept while `project_calls_precise` so
    /// a file change can patch only the affected functions.
    pub precise_edges: projectcalls::SymEdges,
    /// Source files changed since the last precise update, awaiting an
    /// incremental refine (coalesced when one is already running).
    pub precise_pending: HashSet<PathBuf>,
    /// The active project-graph modal overlay, if any.
    pub overlay: Option<Overlay>,
    /// LLM explanations keyed by function/file/folder, kept fresh incrementally.
    pub explanations: explain::Cache,
    /// True while the explain pass is running.
    pub explaining: bool,
    /// Explain progress `(done, total)` while a pass runs.
    pub explain_progress: Option<(usize, usize)>,
    /// How many attempts in the current pass have errored (surfaced in the UI so
    /// a failing pass doesn't masquerade as success).
    pub explain_failed: usize,
    /// Generation for explain passes, so a superseded result is dropped.
    pub explain_gen: u64,
    /// Abort handle for the running explain pass, so a long project pass can be
    /// cancelled (the bottom-up pass over a big repo is thousands of LLM calls).
    pub explain_abort: Option<iced::task::Handle>,
    /// The file/folder whose explanation overlay is open (Cmd+click a tree node).
    pub explain_view: Option<explain::Node>,
    /// The open explanation's content, prepared as ordered segments (markdown
    /// pre-parsed; math/mermaid keyed to rendered SVGs) — either the node's
    /// summary or a function's block detail (see [`App::explain_showing_detail`]).
    pub explain_prepared: Vec<PreparedSeg>,
    /// Rendered math/mermaid SVGs, keyed by content hash — a session cache shared
    /// across every explanation, backed by `.clew/cache/svg/` on disk.
    pub explain_svgs: HashMap<u64, ExplainSvg>,
    /// Generation for async SVG passes, so a superseded batch is dropped.
    pub explain_svg_gen: u64,
    /// True when the overlay is showing a function's per-block detail rather than
    /// its summary (toggled by the `Explain blocks` / `Summary` button).
    pub explain_showing_detail: bool,
    /// The generated architecture overview — RAW LLM markdown, no module map.
    /// The module diagram is injected fresh at prepare time from the current
    /// import graph (never baked into the cache), so it can't go stale.
    pub overview: Option<String>,
    /// The module map, drawn natively on a canvas in the overview home (like the
    /// Import Graph overlay) — laid out from the current import graph, not baked
    /// into the prose or a mermaid diagram.
    pub overview_map: Option<graphlayout::Layout>,
    /// The overview prepared for display (markdown + math/mermaid SVG segments).
    pub overview_prepared: Vec<PreparedSeg>,
    /// True while the overview is being generated.
    pub generating_overview: bool,
    /// The per-project library of saved walkthroughs (persisted with the project).
    pub walkthroughs: Vec<walkthrough::Walkthrough>,
    /// Index into `walkthroughs` of the tour being read, or `None` while browsing
    /// the library list.
    pub walkthrough_open: Option<usize>,
    pub walkthrough_step: usize,
    /// The scope currently being (re)generated, or `None` when idle. Lets the UI
    /// mark just that one row as busy while the rest of the library stays usable.
    pub generating_walkthrough: Option<String>,
    /// True while a walkthrough generation is on its one automatic retry (the LLM
    /// occasionally emits malformed JSON); prevents an endless retry loop.
    pub walkthrough_retried: bool,
    /// The shared top input: a search query in `Search` mode, a scope prompt in
    /// `Walk` mode.
    pub walkthrough_input: String,
    /// Whether the top input searches the library or generates a new tour.
    pub walkthrough_mode: WalkMode,
    /// The current step's narration, prepared for rich display (markdown, plus
    /// mermaid diagrams and math rendered as inline SVGs — same pipeline as the
    /// overview and explanations).
    pub walkthrough_prepared: Vec<PreparedSeg>,
    /// Height of the narration block in the WALK tab; the steps list above it
    /// takes the rest. The divider between them is draggable.
    pub walkthrough_narration_height: f32,
    /// True when the main area shows the overview "home" (vs. code / empty).
    pub show_overview: bool,
    /// Code statistics (lines by language) shown in the Stats full-pane view.
    pub stats: Option<stats::StatsReport>,
    /// True when the main area shows the Stats "home" (vs. code / overview).
    pub show_stats: bool,
    /// True while a stats computation is running (single-flight guard).
    pub building_stats: bool,
    /// Registry revision the stats were last computed at; a newer revision
    /// (a created / deleted / edited file) marks them stale. `u64::MAX` on
    /// project load forces one background refresh over the warm disk cache.
    pub stats_rev: u64,
    /// Request channel to the in-process clew-server, once it has connected.
    /// The client sends `clew-protocol` requests here and receives events back
    /// as `Message::ServerEvent` (see `server`). The client/server split is
    /// grown one flow at a time onto this seam.
    pub server_tx: Option<tokio::sync::mpsc::UnboundedSender<clew_protocol::ClientMessage>>,
    /// Where code is read from — local, or a remote host over SSH. This keys the
    /// server subscription: changing it restarts the transport against the new
    /// target, which is how an in-app Connect switches between local and remote.
    pub connection: connect::ConnTarget,
    /// Remembered SSH hosts, shown in the Connect modal (from `connections.toml`).
    pub saved_connections: Vec<connect::SavedConnection>,
    /// The Connect modal's state (closed, editing a host, browsing a remote's
    /// folders). `None` when the modal is closed.
    pub connect: Option<ConnectUi>,
    // -- Docs (API documentation view) --------------------------------------
    /// The API documentation view's state (see [`DocsState`]).
    pub docs: DocsState,
    /// In-flight streaming chat answers: stream id -> the channel feeding the Ask
    /// flow. `ChatDelta` / `ChatStreamDone` notifications are routed here.
    #[allow(clippy::type_complexity)]
    pub chat_streams: std::sync::Arc<
        std::sync::Mutex<
            std::collections::HashMap<u64, tokio::sync::mpsc::UnboundedSender<ChatStreamPiece>>,
        >,
    >,
    /// Next request id for server calls that need a correlated reply.
    pub next_req_id: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// In-flight AI RPCs: request id -> the caller awaiting its reply. Shared so
    /// background AI tasks can register/await while `update` resolves them.
    #[allow(clippy::type_complexity)]
    pub ai_pending: std::sync::Arc<
        std::sync::Mutex<
            std::collections::HashMap<u64, tokio::sync::oneshot::Sender<Result<clew_protocol::Event, String>>>,
        >,
    >,
    /// In-flight `ReadFile` requests: id -> why it was asked, so the reply is
    /// applied correctly (open-and-jump vs reload-in-place).
    pub pending_reads: std::collections::HashMap<u64, ReadKind>,
    /// Root of an in-flight server `OpenProject`, so its `Tree` reply can build
    /// the project (abs paths resolve against it).
    pub pending_scan_root: Option<PathBuf>,
    /// Next handle for a server-spawned process (language server / debug adapter).
    pub next_proc_id: u64,
    /// proc handle -> the channel that feeds `ProcessOutput` bytes into the
    /// matching LspClient's stdout bridge.
    pub proc_feeds: std::collections::HashMap<u64, tokio::sync::mpsc::UnboundedSender<Vec<u8>>>,
    /// language -> its live server-spawned proc handle, so a restart can kill the
    /// old process before starting a new one.
    pub lsp_procs: std::collections::HashMap<String, u64>,
    /// Semantic search: the embedding index over explanation summaries.
    pub embed_index: embed::Index,
    /// Whether an embedding endpoint is configured.
    pub embed_available: bool,
    /// True while the embedding index is being (re)built.
    pub building_embeddings: bool,
    /// The Semantic-tab query box and its ranked results.
    pub semantic_query: String,
    pub semantic_results: Vec<(explain::Node, f32)>,
    /// True while a semantic query is being embedded/searched.
    pub searching_semantic: bool,
    /// The collapsible bottom panel: whether it's shown, and which of its two
    /// tabs (Ask / Debug) is active.
    pub show_bottom: bool,
    pub bottom_tab: BottomTab,
    /// "Ask clew" Q&A: input, conversation, state.
    pub ask_input: String,
    pub ask_turns: Vec<AskTurn>,
    pub asking: bool,
    /// Code selections pinned as context, shown as chips above the input. They
    /// persist across turns until removed.
    pub ask_pins: Vec<AskPin>,
    /// The active debug session (DAP), if any.
    pub debug: Option<DebugSession>,
    /// Watch expressions (persist across stops/sessions) + the add-watch input.
    pub debug_watches: Vec<String>,
    pub debug_watch_input: String,
    /// Editing a breakpoint condition: (file, 1-based line, draft expression).
    pub bp_cond_edit: Option<(PathBuf, usize, String)>,
    /// Editing a bookmark note: (rel path, 1-based line, draft note text).
    pub note_edit: Option<(String, usize, String)>,
    /// Per-project reading notes / progress, anchored by (rel, symbol name).
    pub notes: Vec<notes::Note>,
    /// Editing a reading note: (rel path, symbol name, draft note text).
    pub reading_note_edit: Option<(String, String, String)>,
    /// The last function the debugger stopped in — so entering a NEW function
    /// records one reading-trail entry (not one per line step).
    pub debug_last_fn: Option<String>,
    /// Breakpoints per file (absolute path → 1-based line → breakpoint),
    /// independent of a running session so they can be set before and persist
    /// across runs.
    pub breakpoints: HashMap<PathBuf, std::collections::BTreeMap<usize, Bp>>,
    /// Auto-refresh throttle: when the last refresh pass began (`None` until the
    /// first). A watched-file change starts a pass only once the cooldown has
    /// lifted; a manual refresh ignores it. Runtime-only (not persisted).
    pub last_auto_refresh: Option<std::time::Instant>,
    /// A source file changed during the cooldown — refresh when the window lifts
    /// (picked up by `Tick`), so no change is dropped.
    pub refresh_pending: bool,
    /// Prompt hash of the cached overview, so a re-explain regenerates it only
    /// when its inputs actually changed (avoids a needless overview LLM call).
    pub overview_prompt_hash: Option<incremental::Version>,
    /// Whether an LLM key is configured (gates the explain UI). Checked at
    /// startup / project open, not per frame.
    pub llm_available: bool,
    /// Whether the toolbar's "More" overflow menu is open.
    pub show_tools_menu: bool,
    /// The status-bar `#[cfg]` target dropdown is open.
    pub show_target_menu: bool,
    /// The customizable command keymap (loaded from the global config).
    pub keymap: keymap::Keymap,
    /// Whether the "Keyboard Shortcuts" modal is open.
    pub show_shortcuts: bool,
    /// The action currently awaiting a new chord (capture mode), if any.
    pub rebinding: Option<keymap::Action>,
    /// Transient message in the shortcuts modal (conflict / invalid key).
    pub keymap_notice: Option<String>,
    /// Show each function's one-line summary inline past its signature.
    pub show_inline_summaries: bool,
    /// Show a one-line "what is this file" banner at the top of the code view.
    pub show_file_banner: bool,
    /// Show LSP inlay hints (inferred types, parameter names) inline.
    pub show_inlay_hints: bool,
    /// Target the `#[cfg]` dimming is evaluated against (host, or one the reader
    /// picks to study another platform's branches). Persisted per project.
    pub reading_target: inactive::Target,
    /// Show the code minimap on the right edge of the editor.
    pub show_minimap: bool,
    /// The LLM settings modal: whether it's open and its draft fields (see
    /// [`SettingsDraft`]).
    pub settings: SettingsDraft,
    /// Overlay view: `true` shows the node-link map, `false` the list.
    pub graph_mode: bool,
    /// Precomputed force-directed layout for the current overlay's map.
    pub graph_layout: Option<graphlayout::Layout>,
    pub expanded: HashSet<String>,
    /// Code panes; pane 1 exists only in split view.
    pub panes: [Option<Viewer>; 2],
    pub split: bool,
    pub active: usize,
    /// Whether the left sidebar (files / search / marks / calls / imports) is shown.
    pub show_left_sidebar: bool,
    /// Whether the right sidebar (Outline / Explain tabs) is shown.
    pub show_right_panel: bool,
    /// When set, the active pane shows this file's diff against `HEAD`.
    pub diff: Option<DiffState>,
    pub finder: Finder,
    pub search: SearchState,
    pub history: History,
    /// Trail nodes whose subtree is collapsed in the TRAIL view.
    pub trail_collapsed: std::collections::HashSet<usize>,
    pub bookmarks: Vec<Bookmark>,
    pub symbol_index: Arc<Vec<SymbolEntry>>,
    pub indexing: bool,
    /// Per-project LSP config from `.clew/lsp.toml`.
    pub lsp_config: lsp::config::ProjectLspConfig,
    /// One server slot per language.
    pub lsp: std::collections::HashMap<String, LspSlot>,
    /// Documents already sent to a server via didOpen (cleared per project).
    pub lsp_opened: HashSet<PathBuf>,
    /// Whole-project content-hash oracle: the authority on what changed, so the
    /// watcher's noisy events collapse to real byte changes.
    pub registry: incremental::Registry,
    /// Symbol index kept per file so a single file can be re-indexed in place;
    /// `symbol_index` is the flattened view the finder consumes.
    pub symbol_index_by_file: HashMap<PathBuf, Vec<SymbolEntry>>,
    /// Project-wide Rust type relations (traits implemented / implementors),
    /// built off-thread after indexing; feeds the hover structure peek.
    pub structure: structure::StructureIndex,
    /// Monotonic LSP document version, bumped on every `didChange`.
    pub lsp_doc_rev: i64,
    /// Last diagnostics version seen per language, to gate refresh ticks.
    pub seen_diag_version: std::collections::HashMap<String, u64>,
    /// Last inlay-hint refresh epoch seen per language, so a server-requested
    /// refresh re-fetches hints exactly once.
    pub seen_inlay_epoch: std::collections::HashMap<String, u64>,
    /// A language server download awaiting the user's consent.
    pub pending_lsp_consent: Option<LspConsent>,
    /// In-file find (Cmd+F), applied to the active pane.
    pub find: find::FindState,
    /// Active hover tooltip (Cmd-hover): position + content.
    pub hover: Option<HoverState>,
    /// Bumped on every hover-token change; a dwell task only shows the peek if its
    /// captured `gen` still matches (i.e. the cursor hasn't moved on).
    pub hover_gen: u64,
    /// The cursor is inside the hover tooltip — keep it open (so you can move into
    /// it and scroll it) and ignore code-view hover events until it leaves.
    pub hover_pinned: bool,
    /// The "Why is this here?" popup, when open.
    pub blame_why: Option<BlameWhy>,
    /// The active git time-travel session, if any.
    pub time_travel: Option<TimeTravel>,
    /// Generation counter for time-travel async results (drops stale loads).
    pub time_gen: u64,
    /// An open right-click navigation menu: (pane, line, col, window x, y).
    pub context_menu: Option<ContextMenu>,
    /// Whether the "Language Servers" management panel is open.
    pub server_panel: bool,
    /// Installed servers listed in the management panel (name, version, bytes).
    pub installed_servers: Vec<lsp::store::InstalledServer>,
    /// Languages actually present in the project that clew can serve.
    pub project_languages: Vec<String>,
    /// True while a mouse drag-selection is in progress.
    pub selecting: bool,
    /// True when the code view (not a text input/finder) has keyboard focus,
    /// so Vim-style motion keys move the cursor instead of typing.
    pub code_focused: bool,
    /// Pending `g` prefix for two-key motions (gg / gd / gr / gi / gy).
    pub pending_g: bool,
    /// Pending `z` prefix for fold commands (za / zR / zM).
    pub pending_z: bool,
    pub modifiers: keyboard::Modifiers,
    pub status: String,
    /// Logical window size (from resize events), drives responsive layout and
    /// clamps the draggable panel sizes below.
    pub window_width: f32,
    pub window_height: f32,
    /// Whether the window is in fullscreen (toggled by the green control).
    pub fullscreen: bool,
    /// Whether the window has keyboard focus. The custom traffic-light controls
    /// grey out when it doesn't, like native macOS.
    pub window_focused: bool,
    /// Whether the pointer is over the traffic-light cluster, so the icons show
    /// on all three (native behaviour), not just the hovered one.
    pub controls_hovered: bool,
    /// User-draggable panel sizes (px): left sidebar width, right context-panel
    /// width, and the bottom debug/ask panel height. See [`resize::Divider`].
    pub sidebar_width: f32,
    pub right_width: f32,
    pub bottom_height: f32,
    pub font_size: f32,
}

pub const DEFAULT_FONT_SIZE: f32 = 13.0;

/// Routes AI calls to clew-server (endpoint = Server) or runs them locally
/// (endpoint = Client). Cheap to clone (handles only), so each background AI
/// task takes one.
#[derive(Clone)]
pub struct AiClient {
    endpoint: clew_protocol::AiEndpoint,
    server_tx: Option<tokio::sync::mpsc::UnboundedSender<clew_protocol::ClientMessage>>,
    next_id: std::sync::Arc<std::sync::atomic::AtomicU64>,
    #[allow(clippy::type_complexity)]
    pending: std::sync::Arc<
        std::sync::Mutex<
            std::collections::HashMap<u64, tokio::sync::oneshot::Sender<Result<clew_protocol::Event, String>>>,
        >,
    >,
}

impl AiClient {
    /// The request channel to use when the AI endpoint is the server.
    fn server(&self) -> Option<&tokio::sync::mpsc::UnboundedSender<clew_protocol::ClientMessage>> {
        if self.endpoint == clew_protocol::AiEndpoint::Server {
            self.server_tx.as_ref()
        } else {
            None
        }
    }

    /// Send a request and await its correlated reply (resolved in `update`).
    async fn rpc(
        &self,
        tx: &tokio::sync::mpsc::UnboundedSender<clew_protocol::ClientMessage>,
        request: clew_protocol::Request,
    ) -> Result<clew_protocol::Event, String> {
        let id = self.next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (otx, orx) = tokio::sync::oneshot::channel();
        self.pending.lock().unwrap().insert(id, otx);
        tx.send(clew_protocol::ClientMessage { id, request })
            .map_err(|_| "server gone".to_string())?;
        orx.await.map_err(|_| "server dropped the request".to_string())?
    }

    /// A single-prompt completion.
    pub async fn complete(
        &self,
        cfg: llm::Config,
        system: &str,
        prompt: String,
        max_tokens: u32,
    ) -> Result<String, String> {
        self.complete_chat(cfg, system, vec![llm::ChatMsg::user(prompt)], max_tokens)
            .await
    }

    /// A multi-turn completion.
    pub async fn complete_chat(
        &self,
        cfg: llm::Config,
        system: &str,
        messages: Vec<llm::ChatMsg>,
        max_tokens: u32,
    ) -> Result<String, String> {
        if let Some(tx) = self.server() {
            let req = clew_protocol::Request::Chat {
                system: system.to_string(),
                messages: messages
                    .iter()
                    .map(|m| clew_protocol::AiChatMsg {
                        role: m.role_str().to_string(),
                        content: m.content.clone(),
                    })
                    .collect(),
                max_tokens,
            };
            return match self.rpc(tx, req).await? {
                clew_protocol::Event::ChatResult { text } => Ok(text),
                clew_protocol::Event::Error { message } => Err(message),
                _ => Err("unexpected reply to Chat".into()),
            };
        }
        // Client endpoint: call the provider directly (blocking HTTP off-thread).
        let system = system.to_string();
        tokio::task::spawn_blocking(move || llm::complete_chat(&cfg, &system, &messages, max_tokens))
            .await
            .unwrap_or_else(|_| Err("task join failed".into()))
    }

    /// Embed texts.
    pub async fn embed(
        &self,
        cfg: embed::Config,
        texts: Vec<String>,
    ) -> Result<Vec<Vec<f32>>, String> {
        if let Some(tx) = self.server() {
            return match self.rpc(tx, clew_protocol::Request::Embed { texts }).await? {
                clew_protocol::Event::Embeddings { vecs } => Ok(vecs),
                clew_protocol::Event::Error { message } => Err(message),
                _ => Err("unexpected reply to Embed".into()),
            };
        }
        tokio::task::spawn_blocking(move || embed::embed_all(&cfg, &texts))
            .await
            .unwrap_or_else(|_| Err("task join failed".into()))
    }
}

/// Why a `ReadFile` was requested, so its `FileContent` reply is applied right.
pub enum ReadKind {
    /// Opening the file: jump to `target` (1-based) in `pane`.
    Open { pane: usize, target: Option<usize> },
    /// Live refresh after an on-disk change: reload every pane showing the file
    /// in place, preserving scroll / caret / folds.
    Refresh,
}

/// One rendered entry on a doc page: a symbol with its signature and its doc
/// comment parsed to markdown items (which the markdown widget borrows).
pub struct DocEntryView {
    pub name: String,
    pub kind: String,
    pub signature: String,
    pub line: usize,
    /// Nesting depth for indenting members under their type (0 = the top item).
    pub depth: usize,
    pub doc_items: Vec<iced::widget::markdown::Item>,
}

/// The doc page shown in the main pane: the selected item followed by its
/// public members (like a rustdoc type page), all in one file.
pub struct DocPage {
    pub rel: String,
    pub entries: Vec<DocEntryView>,
}

/// Find the first documented item named `name` anywhere in the index, returning
/// its (file rel, definition line). Used by "View docs".
fn find_doc_by_name(files: &[clew_protocol::DocFile], name: &str) -> Option<(String, usize)> {
    fn search(items: &[clew_protocol::DocItem], name: &str) -> Option<usize> {
        for it in items {
            if it.name == name {
                return Some(it.line);
            }
            if let Some(line) = search(&it.children, name) {
                return Some(line);
            }
        }
        None
    }
    for f in files {
        if let Some(line) = search(&f.items, name) {
            return Some((f.rel.clone(), line));
        }
    }
    None
}

/// Find the doc item defined at `line`, searching nested members.
fn find_doc_item(items: &[clew_protocol::DocItem], line: usize) -> Option<&clew_protocol::DocItem> {
    for it in items {
        if it.line == line {
            return Some(it);
        }
        if let Some(found) = find_doc_item(&it.children, line) {
            return Some(found);
        }
    }
    None
}

/// Flatten an item and its members into page entries (depth-tagged for
/// indentation), parsing each doc comment to markdown. Members are included
/// only when public, unless `show_all`.
fn flatten_doc(item: &clew_protocol::DocItem, depth: usize, show_all: bool, out: &mut Vec<DocEntryView>) {
    out.push(DocEntryView {
        name: item.name.clone(),
        kind: item.kind.clone(),
        signature: item.signature.clone(),
        line: item.line,
        depth,
        doc_items: iced::widget::markdown::parse(&item.doc).collect(),
    });
    for c in &item.children {
        if show_all || c.public {
            flatten_doc(c, depth + 1, show_all, out);
        }
    }
}

/// The Connect modal's editable form + where it is in the flow. One modal walks
/// from picking a host, to waiting on the transport, to browsing the remote's
/// folders for the one to open.
pub struct ConnectUi {
    // New-connection form fields (strings so the text inputs bind directly;
    // `port` is parsed on submit).
    pub name: String,
    pub host: String,
    pub user: String,
    pub port: String,
    pub identity: String,
    pub stage: ConnectStage,
}

impl Default for ConnectUi {
    fn default() -> Self {
        ConnectUi {
            name: String::new(),
            host: String::new(),
            user: String::new(),
            port: "22".to_string(),
            identity: String::new(),
            stage: ConnectStage::Picking,
        }
    }
}

/// Which field of the new-connection form an edit targets.
#[derive(Debug, Clone, Copy)]
pub enum ConnectField {
    Name,
    Host,
    User,
    Port,
    Identity,
}

/// Where the Connect modal is in its flow.
pub enum ConnectStage {
    /// Choosing a saved host or filling in a new one.
    Picking,
    /// The SSH transport is coming up (bootstrapping the remote server).
    Connecting { label: String },
    /// Connected: browse the remote filesystem to pick a folder to open.
    Browsing(RemoteBrowser),
    /// The connection failed; show why, with the form still available.
    Error(String),
}

/// The remote folder picker's state: the directory in view and its children.
pub struct RemoteBrowser {
    /// Absolute path of the directory being shown (as the server resolved it).
    pub cwd: String,
    /// Parent directory for the "up" control, `None` at the filesystem root.
    pub parent: Option<String>,
    pub entries: Vec<clew_protocol::DirEntry>,
    /// True while a `ListDir` is in flight, so the view can show it is loading.
    pub loading: bool,
}

#[derive(Debug, Clone)]
pub enum Message {
    /// The clew-server started and handed us its request channel.
    ServerConnected(tokio::sync::mpsc::UnboundedSender<clew_protocol::ClientMessage>),
    /// The clew-server binary could not be spawned; fall back to local work.
    ServerUnavailable,
    // -- Connect (remote over SSH) ------------------------------------------
    /// Open the Connect modal (from the empty state, menu, or status bar).
    OpenConnect,
    /// Close the Connect modal without changing the connection.
    CloseConnect,
    /// Edit a field of the new-connection form.
    ConnectField(ConnectField, String),
    /// Pick a private-key file for the form via the native file dialog.
    ConnectPickIdentity,
    ConnectIdentityPicked(Option<PathBuf>),
    /// Connect to the host currently in the form (saving it for next time).
    ConnectSubmit,
    /// Connect to a saved host by index.
    ConnectToSaved(usize),
    /// Forget a saved host by index.
    ConnectRemoveSaved(usize),
    /// Switch back to reading local code (tears down the SSH transport).
    ConnectDisconnect,
    /// In the remote folder picker: enter a child directory / go up / list a path.
    RemoteBrowseTo(String),
    RemoteBrowseUp,
    /// Open the directory currently in view as the project.
    RemoteOpenHere,
    // -- Docs ---------------------------------------------------------------
    /// (Re)build the project's API docs from the server.
    DocsRefresh,
    /// Expand / collapse a file group in the DOCS tree.
    DocsToggleFile(String),
    /// Filter the DOCS tree by item name.
    DocsFilterChanged(String),
    /// Toggle showing all symbols vs. only the public API.
    DocsToggleShowAll,
    /// Toggle grouping the Docs tree by module/package vs. by file.
    DocsToggleGrouping,
    /// Open the doc page for the item at (file rel, definition line).
    DocsSelect { rel: String, line: usize },
    /// Open the doc page for the symbol under the cursor (from the code view's
    /// right-click menu).
    ViewDocsFromMenu,
    /// A proxied process (spawned from a background stream, e.g. the debug
    /// adapter) registers where its `ProcessOutput` should be routed.
    RegisterProcFeed {
        proc: u64,
        feed: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    },
    /// An event (reply or notification) from the clew-server.
    ServerEvent(clew_protocol::ServerMessage),
    OpenFolderPressed,
    FolderPicked(Option<PathBuf>),
    ConsentAllowed,
    ConsentDenied,
    ScanDone(ScanResult),
    /// A structural change (file created/deleted/renamed) rebuilt the tree; swap
    /// it in without the full project-open reset.
    TreeUpdated(ScanResult),
    SymbolIndexDone {
        root: PathBuf,
        indexed: index::Indexed,
    },
    ToggleDir(String),
    OpenRel {
        rel: String,
        line: Option<usize>,
    },
    OpenAbs {
        abs: PathBuf,
        line: Option<usize>,
        push: bool,
    },
    FileLoaded {
        pane: usize,
        abs: PathBuf,
        target: Option<usize>,
        result: Result<String, String>,
    },
    Highlighted {
        abs: PathBuf,
        lines: Vec<HlLine>,
        symbols: Vec<Symbol>,
        /// Signature line (1-based) -> doc comment, extracted alongside symbols.
        docs: HashMap<usize, String>,
        /// 0-based lines gated off by an inactive `#[cfg]` (dimmed).
        inactive: HashSet<usize>,
    },
    /// The project-wide Rust structure index finished building.
    StructureBuilt(structure::StructureIndex),
    /// Inlay hints came back from the language server for `abs`.
    InlayHintsLoaded {
        abs: PathBuf,
        hints: Vec<lsp::client::InlayHint>,
    },
    /// The watcher reports paths that may have changed on disk (unfiltered).
    FilesChanged(Vec<PathBuf>),
    /// Off-thread re-hash classified content-tracked paths (modified / deleted),
    /// and an existence probe of the other changed paths reported whether any is
    /// a create/delete of a (non-source) tree entry that needs a rescan.
    FilesRehashed {
        events: Vec<watch::FileEvent>,
        fs_structural: bool,
    },
    /// Per-line git blame + change status finished loading for `abs`.
    GitInfoLoaded {
        abs: PathBuf,
        info: Option<Arc<git::GitInfo>>,
    },
    CodeScrolled(usize, scrollable::Viewport),
    PaneFocused(usize),
    ToggleSplit,
    SelectStart {
        pane: usize,
        line: usize,
        col: usize,
    },
    SelectDrag {
        pane: usize,
        line: usize,
        col: usize,
    },
    SelectEnd,
    CopySelection,
    /// Toggle the fold headed by `line` in `pane` (gutter arrow click).
    FoldToggle {
        pane: usize,
        line: usize,
    },
    /// Scroll `pane` to a fraction `[0,1]` of the file (minimap click/drag).
    MinimapScrolled {
        pane: usize,
        fraction: f32,
    },
    SidebarTabPicked(SidebarTab),
    SearchQueryChanged(String),
    SearchSubmitted,
    SearchDone {
        result: search::SearchResult,
    },
    /// Toggle a match option (regex / case-sensitive / whole-word).
    SearchToggle(SearchOpt),
    SearchIncludeChanged(String),
    SearchExcludeChanged(String),
    FinderOpened(FinderMode),
    FinderClosed,
    FinderQueryChanged(String),
    FinderPick(usize),
    FinderConfirm,
    GotoLineRequested,
    BookmarkToggled,
    BookmarkRemoved(usize),
    /// Open the note editor for the bookmark at (rel, 1-based line).
    BookmarkNoteEdit(String, usize),
    /// The bookmark-note draft text changed.
    BookmarkNoteInput(String),
    /// Save the bookmark note draft.
    BookmarkNoteSave,
    /// Cancel editing the bookmark note.
    BookmarkNoteCancel,
    /// Toggle the "understood" flag on a symbol (from the outline / notes list).
    NoteToggleUnderstood { rel: String, symbol: String },
    /// Open the reading-note editor for a symbol (from the outline / notes list).
    NoteEditStart { rel: String, symbol: String },
    /// The reading-note draft text changed.
    NoteEditInput(String),
    /// Save the reading-note draft.
    NoteEditSave,
    /// Cancel editing the reading note.
    NoteEditCancel,
    /// Remove a reading note entirely (from the notes list).
    NoteRemove { rel: String, symbol: String },
    /// Jump to a noted symbol (resolving its live line; opens the file top if the
    /// symbol is orphaned).
    NoteJump { rel: String, symbol: String },
    GoBack,
    GoForward,
    /// Jump to a node in the history tree view.
    HistoryJump(usize),
    /// Collapse / expand a node's subtree in the TRAIL view.
    TrailToggleCollapse(usize),
    /// Clear the whole navigation history tree.
    HistoryClear,
    /// Show / hide the left sidebar.
    ToggleLeftSidebar,
    /// Show / hide the right sidebar (Outline / Explain tabs).
    ToggleRightPanel,
    /// Toggle the diff-vs-HEAD view for the active file.
    ToggleDiff,
    /// The diff for `abs` finished computing.
    DiffLoaded {
        abs: PathBuf,
        rel: String,
        lines: Vec<git::DiffLine>,
    },
    OutlineJump(usize),
    FontSizeDelta(f32),
    FontSizeReset,
    KeyPressed(keyboard::Key, keyboard::Modifiers),
    ModifiersChanged(keyboard::Modifiers),
    WindowResized(Size),
    /// The window has been realized; apply one-time native tweaks (rounded
    /// corners on macOS, since the frameless window has square corners).
    WindowOpened,
    /// Start dragging the whole window (from the custom title-bar region).
    TitleBarDragged,
    /// The window gained or lost focus (greys out the custom controls).
    WindowFocusChanged(bool),
    /// The pointer entered or left the traffic-light cluster (shows the icons).
    ControlsHover(bool),
    /// Custom window controls (frameless window has no OS buttons).
    CloseWindow,
    MinimizeWindow,
    ToggleFullscreen,
    /// Drag a panel divider: the payload is the cursor's absolute x (sidebar /
    /// right panel) or y (bottom panel). See [`resize::Divider`].
    ResizeSidebar(f32),
    ResizeRight(f32),
    ResizeBottom(f32),
    // --- LSP ---
    LspStartResult {
        language: String,
        result: Result<lsp::client::LspClient, String>,
    },
    LspConsentAllowed,
    LspConsentDismissed,
    LspDownloadResult {
        language: String,
        result: Result<PathBuf, String>,
    },
    GotoDefinition {
        pane: usize,
        line: usize,
        col: usize,
    },
    ContextMenuOpened {
        pane: usize,
        line: usize,
        col: usize,
        x: f32,
        y: f32,
    },
    ContextMenuClosed,
    ContextGoto(GotoKind),
    FindOpened,
    FindQueryChanged(String),
    FindStep(i32),
    FindClosed,
    HoverRequested {
        pane: usize,
        line: usize,
        col: usize,
        x: f32,
        y: f32,
    },
    /// The hover dwell elapsed for a token — show the peek if the cursor is still
    /// on it (`gen` matches the latest hover). Debounces flicker while moving.
    HoverDwell {
        epoch: u64,
        pane: usize,
        line: usize,
        col: usize,
        x: f32,
        y: f32,
    },
    /// The cursor left the code — clear any open peek.
    HoverCleared,
    /// The cursor entered (true) or left (false) the hover tooltip itself.
    HoverPin(bool),
    HoverResult {
        line: usize,
        col: usize,
        text: Option<String>,
    },
    DefinitionResult {
        result: Result<Vec<lsp::client::Target>, String>,
    },
    ReferencesResult {
        result: Result<Vec<lsp::client::Target>, String>,
    },
    /// Open the call hierarchy for the symbol under the cursor (`gc`).
    CallHierarchyRequested,
    /// Open the call hierarchy from the right-click context menu.
    CallHierarchyFromMenu,
    /// Explain the function at the right-click context menu.
    ExplainFromMenu,
    /// `prepareCallHierarchy` resolved the anchor item(s).
    CallHierarchyPrepared {
        direction: callgraph::Direction,
        lang: &'static str,
        items: Vec<lsp::client::CallItem>,
    },
    /// Expand (fetch children of, or toggle) a node in the call tree.
    CallHierarchyExpand(usize),
    /// A node's callers/callees arrived.
    CallHierarchyChildren {
        id: usize,
        items: Vec<lsp::client::CallItem>,
    },
    /// Flip between callers and callees.
    CallHierarchyDirection,
    /// Recursively expand the whole tree (to the project boundary).
    CallHierarchyExpandAll,
    /// Expand/collapse an import-tree node.
    ImportExpand(usize),
    /// Flip the import tree between Imports and Importers.
    ImportDirection,
    /// Recursively expand the whole import tree (to the project boundary).
    ImportExpandAll,
    /// Open a project-wide graph overlay (or switch which one).
    OpenOverlay(Overlay),
    /// Close the project-graph overlay.
    CloseOverlay,
    /// From an overlay: open a file, focus the Imports tab, and close the overlay.
    OverlayOpenImports(PathBuf),
    /// From an overlay: open a file at a line and close the overlay.
    OverlayOpenAt { abs: PathBuf, line: usize },
    /// The project call graph finished (re)building off-thread.
    ProjectCallsBuilt {
        root: PathBuf,
        graph: projectcalls::ProjectCallGraph,
    },
    /// Flip the current overlay between the list and the node-link map.
    OverlayViewToggle,
    /// Kick a background LSP pass that rebuilds the call graph with exact edges.
    RefineProjectCalls,
    /// Progress of the running LSP refine.
    RefineProgress {
        generation: u64,
        done: usize,
        total: usize,
    },
    /// The LSP-precise call graph finished (re)building. Carries the symbol-keyed
    /// edge set (kept for incremental patching) and the ready display graph.
    ProjectCallsRefined {
        root: PathBuf,
        generation: u64,
        edges: projectcalls::SymEdges,
        graph: projectcalls::ProjectCallGraph,
    },
    /// Explain the whole project (bottom-up LLM pass).
    ExplainProject,
    /// Cancel the running explain pass (abort remaining LLM calls).
    CancelExplain,
    /// Force an immediate refresh of the whole understanding (explanations →
    /// index → overview), bypassing the auto-refresh cooldown. User-initiated.
    RefreshAll,
    /// Explain-pass progress. `done` counts attempts (successes + failures);
    /// `failed` is how many of those attempts errored, so the UI can report
    /// honestly instead of implying every counted item succeeded.
    ExplainProgress {
        generation: u64,
        done: usize,
        total: usize,
        failed: usize,
    },
    /// The explain pass finished with the fresh cache. `failed` is the number of
    /// nodes whose explanation errored; `auth_error` is set when the pass was cut
    /// short because the LLM rejected the request (e.g. a bad API key).
    ExplainDone {
        root: PathBuf,
        generation: u64,
        cache: explain::Cache,
        failed: usize,
        auth_error: Option<String>,
    },
    /// Show a file's / folder's explanation (Cmd+click in the tree).
    ShowExplanation(explain::Node),
    /// Re-explain the node in the open explanation modal (invalidate + rerun).
    ReexplainNode,
    /// Show (or generate on demand) a function's block-by-block walkthrough.
    ExplainBlocks(explain::Node),
    /// The block walkthrough for `node` finished (or failed).
    BlocksExplained {
        node: explain::Node,
        detail: Result<String, String>,
    },
    /// A background pass finished rendering math/mermaid blocks to SVG.
    SvgsGenerated {
        generation: u64,
        map: HashMap<u64, richmd::PreparedSvg>,
    },
    /// Show the architecture overview "home" in the main area.
    ShowOverview,
    /// Generate (or regenerate) the architecture overview.
    GenerateOverview,
    /// Show the code-statistics "home" in the main area (computes if stale).
    ShowStats,
    /// Recompute the code statistics regardless of freshness (the Refresh button).
    RefreshStats,
    /// A stats computation finished for `root`.
    StatsDone {
        root: PathBuf,
        rev: u64,
        report: stats::StatsReport,
    },
    /// Generate a new walkthrough for `scope` (empty = the whole codebase). The
    /// result is upserted into the library by scope, then opened.
    GenerateWalkthrough(String),
    /// Generate a narrated walkthrough of the current branch/PR diff (or the last
    /// commit when there's no base branch). Upserted into the library like a tour.
    GenerateDiffWalkthrough,
    /// Regenerate the library tour at this index (reusing its saved scope).
    WalkthroughRegenerate(usize),
    /// Delete the library tour at this index and persist the smaller library.
    WalkthroughDelete(usize),
    /// A walkthrough finished generating; `scope` keys the upsert into the library.
    WalkthroughDone {
        scope: String,
        result: Result<walkthrough::Walkthrough, String>,
    },
    /// Open the library tour at this index for reading.
    WalkthroughOpen(usize),
    /// Return from a tour to the library list.
    WalkthroughBack,
    /// Flip the top input between searching the library and generating a tour.
    WalkthroughToggleMode,
    /// Jump to an absolute step index in the open walkthrough.
    WalkthroughGoto(usize),
    /// Move by a relative offset (Next / Prev).
    WalkthroughStep(i32),
    /// The top input (search query / scope prompt) changed.
    WalkthroughInputChanged(String),
    /// Drag the divider between the WALK steps list and the narration.
    ResizeWalkNarration(f32),
    /// The overview finished generating.
    OverviewDone {
        root: PathBuf,
        prompt_hash: incremental::Version,
        result: Result<String, String>,
    },
    /// Build / refresh the semantic embedding index.
    BuildEmbeddings,
    /// The embedding index finished building.
    EmbeddingsBuilt {
        root: PathBuf,
        result: Result<embed::Index, String>,
    },
    /// The Semantic-tab query text changed.
    SemanticQueryChanged(String),
    /// Run the semantic search for the current query.
    SemanticSearch,
    /// The query's embedding vector (or an error); the handler ranks the index.
    SemanticResults {
        query: String,
        result: Result<Vec<f32>, String>,
    },
    /// Open a semantic result: jump to the function/file in the code.
    OpenNode(explain::Node),
    /// Jump to the exact line where `caller` calls `callee` (from CALLED BY),
    /// resolved live from the caller file; falls back to the caller's definition.
    JumpToCall { caller_file: PathBuf, caller: String, callee: String },
    /// Toolbar "Ask": open the bottom panel on the Ask tab, or collapse it.
    ToggleAsk,
    /// Switch the bottom panel's tab (Ask / Debug), opening it if collapsed.
    BottomTabPicked(BottomTab),
    /// Collapse the bottom panel.
    CollapseBottom,
    /// Show / hide the toolbar's "More" overflow menu.
    ToggleToolsMenu,
    ToggleTargetMenu,
    /// Open the "Keyboard Shortcuts" modal (from the More menu).
    OpenShortcuts,
    /// Close the "Keyboard Shortcuts" modal.
    CloseShortcuts,
    /// Begin capturing a new chord for an action (click a binding).
    RebindStart(keymap::Action),
    /// Reset one action's binding to its default.
    RebindReset(keymap::Action),
    /// Reset every binding to its default.
    RebindResetAll,
    /// Toggle inline function summaries in the code view.
    ToggleInlineSummaries,
    /// Toggle the file-top summary banner in the code view.
    ToggleFileBanner,
    /// Toggle the code minimap.
    ToggleMinimap,
    /// Toggle LSP inlay hints (inferred types, parameter names).
    ToggleInlayHints,
    /// Pick the target the `#[cfg]` dimming is evaluated against.
    TargetSelected(inactive::Target),
    /// Toggle "skim" for the active file: fold function/method bodies to
    /// signatures + summaries, or expand them again.
    SkimFile,
    /// The Ask input box text changed.
    AskInputChanged(String),
    /// Submit the current question.
    AskSubmit,
    /// Ask a suggested (context-aware) question: fill the input and submit it.
    AskSuggested(String),
    /// The question's embedding vector came back (retrieval step).
    AskRetrieved {
        question: String,
        qvec: Result<Vec<f32>, String>,
    },
    /// A streamed token for the current answer — append to the open turn.
    AskDelta(String),
    /// The streamed answer finished (`Some` = the error it failed with).
    AskStreamEnded(Option<String>),
    /// Clear the whole Ask conversation.
    AskClear,
    /// Remove the pinned code-selection context at this index.
    AskUnpin(usize),
    /// Jump to the pinned selection at this index (open its file at its line).
    AskPinGoto(usize),
    /// Add the current code selection as a context chip and open the Ask panel.
    AskAboutSelection,
    /// Explain why the right-clicked line (or selection) exists, from git blame.
    WhyIsThisHere,
    /// The "why is this here?" answer finished generating.
    BlameWhyDone {
        title: String,
        commits: Vec<(String, String)>,
        result: Result<String, String>,
    },
    /// Close the "why is this here?" popup.
    BlameWhyClose,
    /// Enter git time travel for the active file; `symbol` scopes it to the
    /// function under the cursor.
    TimeTravelStart {
        symbol: bool,
    },
    /// Commits for a time-travel session finished loading.
    TimeTravelReady {
        generation: u64,
        abs: PathBuf,
        rel: String,
        lang: Option<&'static str>,
        scope: TimeScope,
        commits: Vec<git::HistCommit>,
    },
    /// Scrub to commit index `idx` (0 = newest).
    TimeTravelGoto(usize),
    /// The historical content for a step finished loading.
    TimeTravelStep {
        generation: u64,
        idx: usize,
        step: Box<TimeStep>,
    },
    /// The historical view scrolled (tracked for its sticky headers).
    TimeTravelScrolled(scrollable::Viewport),
    /// Place the caret / begin a selection in the historical (read-only) view.
    TimeTravelSelectStart {
        line: usize,
        col: usize,
    },
    /// Extend the historical view's selection while dragging.
    TimeTravelSelectDrag {
        line: usize,
        col: usize,
    },
    /// Switch a session between whole-file and the current function's scope.
    TimeTravelToggleScope,
    /// Generate the LLM "what & why" summary of the current commit's diff.
    TimeTravelWhy,
    TimeTravelWhyDone {
        generation: u64,
        sha: String,
        result: Result<String, String>,
    },
    /// Generate the "story of this function" narrative (symbol scope).
    TimeTravelStory,
    TimeTravelStoryDone {
        generation: u64,
        result: Result<String, String>,
    },
    /// Leave time travel, back to the live file.
    TimeTravelExit,
    /// A no-op sink for fire-and-forget async debug commands.
    Noop,
    /// Start (or restart) a debug session from the project's launch config.
    StartDebug,
    /// The adapter is ready; carry its handle + TCP port (for child sessions).
    DapStarted {
        client: dap::DapClient,
        port: Option<u16>,
    },
    /// A child session (js-debug) is ready; it becomes the active client.
    DapChildStarted(dap::DapClient),
    /// An event pushed from the debug adapter.
    DapEvent(dap::DapEvent),
    /// The stopped frame's stack + scopes/variables finished loading.
    DapStopInspected {
        frames: Vec<dap::StackFrame>,
        scopes: Vec<DebugScope>,
    },
    /// Stepping / continue control.
    DebugControl(DebugCmd),
    /// End the debug session.
    DebugStop,
    /// Toggle a breakpoint at (file, 1-based line) — from the code context menu.
    BreakpointToggle {
        path: PathBuf,
        line: usize,
    },
    /// Toggle a breakpoint at the right-clicked line (code context menu).
    ToggleBreakpointFromMenu,
    /// Open the condition editor for the right-clicked line (context menu).
    ConditionalBreakpointFromMenu,
    /// The breakpoint-condition draft changed.
    BpConditionInput(String),
    /// Apply the drafted condition (set a conditional breakpoint).
    BpConditionSet,
    /// Close the condition editor.
    BpConditionCancel,
    /// The add-watch input changed.
    DebugWatchInput(String),
    /// Add the current input as a watch expression.
    DebugWatchAdd,
    /// Remove watch expression at index.
    DebugWatchRemove(usize),
    /// Watch expressions finished evaluating: (expression, value) pairs.
    DebugWatchesEvaluated(Vec<(String, String)>),
    /// Starting the debugger failed.
    DebugFailed(String),
    /// Embedding settings draft edits.
    SettingsEmbedKeyChanged(String),
    SettingsEmbedModelChanged(String),
    SettingsEmbedBaseUrlChanged(String),
    /// Close the explanation overlay.
    CloseExplanation,
    /// A markdown link in an explanation was clicked.
    OpenLink(String),
    /// Toggle a markdown file between its rendered view and its raw source.
    ToggleMarkdownSource(usize),
    /// Open / close the LLM settings modal.
    OpenSettings,
    CloseSettings,
    /// LLM settings draft edits.
    SettingsProviderPicked(llm::Provider),
    SettingsKeyChanged(String),
    SettingsModelChanged(String),
    SettingsBaseUrlChanged(String),
    /// Save the LLM settings to the global config.
    SettingsSaved,
    Tick,
    ToggleServerPanel,
    LspRestart(String),
    LspRemove {
        name: String,
        version: String,
    },
    LspDownloadFor(String),
}

impl App {
    fn new() -> (Self, Task<Message>) {
        let mut app = App::blank();
        // On a remote connection the path is on that host, so it can't be
        // validated locally — hand it straight to the server.
        if app.connection.is_remote() {
            let task = match std::env::args().nth(1) {
                Some(arg) => app.start_scan(PathBuf::from(arg)),
                None => Task::none(),
            };
            return (app, task);
        }
        let task = match std::env::args().nth(1) {
            Some(arg) => {
                let path = PathBuf::from(&arg);
                let path = path.canonicalize().unwrap_or(path);
                if path.is_dir() {
                    app.request_open(path)
                } else if path.is_file() {
                    // Open the parent directory as the project, then the file.
                    let root = path
                        .parent()
                        .map(Path::to_path_buf)
                        .unwrap_or_else(|| path.clone());
                    app.pending_open = Some(path);
                    app.request_open(root)
                } else {
                    app.status = format!("No such path: {arg}");
                    Task::none()
                }
            }
            None => Task::none(),
        };
        (app, task)
    }

    fn blank() -> Self {
        App {
            project: None,
            pending_open: None,
            pending_consent: None,
            scanning: false,
            sidebar: SidebarTab::Files,
            call_graph: None,
            import_graph: imports::ImportGraph::default(),
            import_tree: None,
            import_dir: imports::Dir::Imports,
            import_cycles: Vec::new(),
            project_calls: projectcalls::ProjectCallGraph::default(),
            project_calls_rev: 0,
            building_calls: false,
            project_calls_precise: false,
            calls_gen: 0,
            refine_progress: None,
            precise_edges: projectcalls::SymEdges::default(),
            precise_pending: HashSet::new(),
            overlay: None,
            explanations: explain::Cache::new(),
            explaining: false,
            explain_progress: None,
            explain_failed: 0,
            explain_gen: 0,
            explain_abort: None,
            explain_view: None,
            explain_prepared: Vec::new(),
            explain_svgs: HashMap::new(),
            explain_svg_gen: 0,
            explain_showing_detail: false,
            overview: None,
            overview_map: None,
            overview_prepared: Vec::new(),
            generating_overview: false,
            walkthroughs: Vec::new(),
            walkthrough_open: None,
            walkthrough_step: 0,
            generating_walkthrough: None,
            walkthrough_retried: false,
            walkthrough_input: String::new(),
            walkthrough_mode: WalkMode::Search,
            walkthrough_prepared: Vec::new(),
            walkthrough_narration_height: 240.0,
            show_overview: false,
            stats: None,
            show_stats: false,
            building_stats: false,
            stats_rev: u64::MAX,
            server_tx: None,
            connection: connect::ConnTarget::from_env(),
            saved_connections: connect::load(),
            connect: None,
            docs: DocsState::default(),
            chat_streams: std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            next_req_id: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1)),
            ai_pending: std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            pending_reads: std::collections::HashMap::new(),
            pending_scan_root: None,
            next_proc_id: 1,
            proc_feeds: std::collections::HashMap::new(),
            lsp_procs: std::collections::HashMap::new(),
            embed_index: embed::Index::default(),
            embed_available: embed::Config::available(),
            building_embeddings: false,
            semantic_query: String::new(),
            semantic_results: Vec::new(),
            searching_semantic: false,
            show_bottom: false,
            bottom_tab: BottomTab::Ask,
            ask_input: String::new(),
            ask_turns: Vec::new(),
            asking: false,
            ask_pins: Vec::new(),
            debug: None,
            debug_watches: Vec::new(),
            debug_watch_input: String::new(),
            bp_cond_edit: None,
            note_edit: None,
            notes: Vec::new(),
            reading_note_edit: None,
            debug_last_fn: None,
            breakpoints: HashMap::new(),
            last_auto_refresh: None,
            refresh_pending: false,
            overview_prompt_hash: None,
            llm_available: llm::Config::available(),
            show_tools_menu: false,
            show_target_menu: false,
            keymap: keymap::Keymap::load(),
            show_shortcuts: false,
            rebinding: None,
            keymap_notice: None,
            show_inline_summaries: true,
            show_file_banner: true,
            show_inlay_hints: true,
            reading_target: inactive::Target::host(),
            show_minimap: true,
            settings: SettingsDraft::default(),
            graph_mode: true,
            graph_layout: None,
            expanded: HashSet::new(),
            panes: [None, None],
            split: false,
            active: 0,
            show_left_sidebar: true,
            show_right_panel: true,
            diff: None,
            finder: Finder::default(),
            search: SearchState::default(),
            history: History::default(),
            trail_collapsed: std::collections::HashSet::new(),
            bookmarks: Vec::new(),
            symbol_index: Arc::new(Vec::new()),
            indexing: false,
            lsp_config: lsp::config::ProjectLspConfig::default(),
            lsp: std::collections::HashMap::new(),
            lsp_opened: HashSet::new(),
            registry: incremental::Registry::default(),
            symbol_index_by_file: HashMap::new(),
            structure: structure::StructureIndex::default(),
            lsp_doc_rev: 1,
            seen_diag_version: std::collections::HashMap::new(),
            seen_inlay_epoch: std::collections::HashMap::new(),
            pending_lsp_consent: None,
            find: find::FindState::default(),
            hover: None,
            hover_gen: 0,
            hover_pinned: false,
            blame_why: None,
            time_travel: None,
            time_gen: 0,
            context_menu: None,
            server_panel: false,
            installed_servers: Vec::new(),
            project_languages: Vec::new(),
            selecting: false,
            code_focused: true,
            pending_g: false,
            pending_z: false,
            modifiers: keyboard::Modifiers::default(),
            status: "Open a folder to start reading".to_string(),
            window_width: 1280.0,
            window_height: 800.0,
            fullscreen: false,
            window_focused: true,
            controls_hovered: false,
            sidebar_width: 280.0,
            right_width: 400.0,
            bottom_height: 340.0,
            font_size: DEFAULT_FONT_SIZE,
        }
    }

    /// Keep the draggable panel sizes within sensible bounds for the current
    /// window, so a panel can never be dragged to nothing or over the code.
    fn clamp_panel_sizes(&mut self) {
        let w = self.window_width.max(400.0);
        let h = self.window_height.max(300.0);
        self.sidebar_width = self.sidebar_width.clamp(160.0, (w * 0.5).max(200.0));
        self.right_width = self.right_width.clamp(240.0, (w * 0.6).max(280.0));
        self.bottom_height = self.bottom_height.clamp(100.0, (h * 0.75).max(160.0));
    }

    pub fn line_height(&self) -> f32 {
        self.font_size + 7.0
    }

    pub fn active_viewer(&self) -> Option<&Viewer> {
        self.panes[self.active].as_ref()
    }

    fn active_viewer_mut(&mut self) -> Option<&mut Viewer> {
        self.panes[self.active].as_mut()
    }

    fn title(&self) -> String {
        match &self.project {
            Some(p) => format!(
                "Clew — {}",
                p.root.file_name().unwrap_or_default().to_string_lossy()
            ),
            None => "Clew".to_string(),
        }
    }

    fn view(&self) -> Element<'_, Message> {
        ui::view(self)
    }

    fn theme(&self) -> iced::Theme {
        theme::app_theme()
    }

    fn subscription(&self) -> Subscription<Message> {
        // listen_with (rather than keyboard::listen) also sees events already
        // captured by focused widgets, so shortcuts like Esc work while a
        // text input has focus.
        let events = iced::event::listen_with(|event, _status, _window| match event {
            iced::Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. }) => {
                Some(Message::KeyPressed(key, modifiers))
            }
            iced::Event::Keyboard(keyboard::Event::ModifiersChanged(m)) => {
                Some(Message::ModifiersChanged(m))
            }
            iced::Event::Mouse(iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left)) => {
                Some(Message::SelectEnd)
            }
            iced::Event::Window(iced::window::Event::Resized(size)) => {
                Some(Message::WindowResized(size))
            }
            iced::Event::Window(iced::window::Event::Opened { .. }) => {
                Some(Message::WindowOpened)
            }
            iced::Event::Window(iced::window::Event::Focused) => {
                Some(Message::WindowFocusChanged(true))
            }
            iced::Event::Window(iced::window::Event::Unfocused) => {
                Some(Message::WindowFocusChanged(false))
            }
            _ => None,
        });

        // On-disk changes are watched by clew-server, which streams FilesChanged
        // / Tree notifications (see `handle_server_event`); the client no longer
        // runs its own watcher.
        let mut subs = vec![events, server::subscription(self.connection.clone())];
        // Poll for live refresh only while something is changing (a server is
        // starting, indexing, the management panel is open, or an auto-refresh is
        // queued waiting out its cooldown) — idle stays quiet.
        if self.lsp_needs_refresh() || self.refresh_pending {
            subs.push(iced::time::every(std::time::Duration::from_millis(400)).map(|_| Message::Tick));
        }
        Subscription::batch(subs)
    }

    fn lsp_needs_refresh(&self) -> bool {
        self.server_panel
            || self.lsp.iter().any(|(lang, s)| match s {
                LspSlot::Starting => true,
                LspSlot::Ready(c) => {
                    c.progress().is_some()
                        || self.seen_diag_version.get(lang).copied() != Some(c.diag_version())
                        || self.seen_inlay_epoch.get(lang).copied() != Some(c.inlay_epoch())
                }
                _ => false,
            })
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        // Any action picked from the toolbar "More" menu dismisses it.
        if self.show_tools_menu
            && matches!(
                message,
                Message::OpenFolderPressed
                    | Message::ToggleServerPanel
                    | Message::ToggleDiff
                    | Message::ExplainProject
                    | Message::OpenSettings
            )
        {
            self.show_tools_menu = false;
        }
        match message {
            Message::OpenFolderPressed => Task::perform(pick_folder(), Message::FolderPicked),
            Message::FolderPicked(None) => Task::none(),
            Message::FolderPicked(Some(root)) => self.request_open(root),
            Message::ConsentDenied => {
                self.pending_consent = None;
                self.pending_open = None;
                self.status = "Project not opened: creating .clew was not allowed".to_string();
                Task::none()
            }
            Message::ConsentAllowed => self.on_consent_allowed(),
            Message::ScanDone(result) => self.on_scan_done(result),
            Message::TreeUpdated(result) => self.on_tree_updated(result),
            Message::SymbolIndexDone { root, indexed } => self.on_symbol_index_done(root, indexed),
            Message::StructureBuilt(index) => {
                self.structure = index;
                Task::none()
            }
            Message::InlayHintsLoaded { abs, hints } => self.on_inlay_hints_loaded(abs, hints),
            Message::ToggleDir(rel) => self.on_toggle_dir(rel),
            Message::OpenRel { rel, line } => self.on_open_rel(rel, line),
            Message::OpenAbs { abs, line, push } => self.open_file(abs, line, push),
            Message::FileLoaded {
                pane,
                abs,
                target,
                result,
            } => self.on_file_loaded(pane, abs, target, result),
            Message::Highlighted { abs, lines, symbols, docs, inactive } => self.on_highlighted(abs, lines, symbols, docs, inactive),
            Message::GitInfoLoaded { abs, info } => self.on_git_info_loaded(abs, info),
            Message::FilesChanged(paths) => self.on_files_changed(paths),
            Message::FilesRehashed { events, fs_structural } => self.on_files_rehashed(events, fs_structural),
            Message::CodeScrolled(pane, viewport) => {
                if let Some(v) = self.panes.get_mut(pane).and_then(Option::as_mut) {
                    v.scroll_y = viewport.absolute_offset().y;
                    v.viewport_h = viewport.bounds().height;
                }
                Task::none()
            }
            Message::PaneFocused(pane) => {
                if pane == 0 || self.split {
                    self.active = pane;
                    // The Imports tab follows the focused pane's file.
                    self.refresh_import_tree();
                }
                Task::none()
            }
            Message::ToggleSplit => self.on_toggle_split(),
            Message::SelectStart { pane, line, col } => self.on_select_start(pane, line, col),
            Message::SelectDrag { pane, line, col } => self.on_select_drag(pane, line, col),
            Message::SelectEnd => {
                self.selecting = false;
                Task::none()
            }
            Message::FoldToggle { pane, line } => {
                if let Some(v) = self.panes.get_mut(pane).and_then(Option::as_mut) {
                    v.toggle_fold(line);
                }
                Task::none()
            }
            Message::MinimapScrolled { pane, fraction } => self.on_minimap_scrolled(pane, fraction),
            Message::CopySelection => self.on_copy_selection(),
            Message::SidebarTabPicked(tab) => self.on_sidebar_tab_picked(tab),
            Message::SearchQueryChanged(query) => {
                self.search.query = query;
                Task::none()
            }
            Message::SearchToggle(opt) => {
                match opt {
                    SearchOpt::Regex => self.search.regex = !self.search.regex,
                    SearchOpt::Case => self.search.case_sensitive = !self.search.case_sensitive,
                    SearchOpt::WholeWord => self.search.whole_word = !self.search.whole_word,
                }
                // Re-run live so the effect of the toggle is immediate.
                self.run_search()
            }
            Message::SearchIncludeChanged(s) => {
                self.search.include = s;
                Task::none()
            }
            Message::SearchExcludeChanged(s) => {
                self.search.exclude = s;
                Task::none()
            }
            Message::SearchSubmitted => self.run_search(),
            Message::SearchDone { result } => {
                self.apply_search_result(result);
                Task::none()
            }
            Message::FinderOpened(mode) => self.on_finder_opened(mode),
            Message::FinderClosed => {
                self.finder.open = false;
                self.code_focused = true; // back to reading
                Task::none()
            }
            Message::FinderQueryChanged(query) => {
                self.finder.query = query;
                self.refresh_finder();
                Task::none()
            }
            Message::FinderPick(idx) => self.finder_open_index(idx),
            Message::FinderConfirm => self.on_finder_confirm(),
            Message::GotoLineRequested => self.on_goto_line_requested(),
            Message::BookmarkToggled => self.on_bookmark_toggled(),
            Message::BookmarkRemoved(idx) => self.on_bookmark_removed(idx),
            Message::BookmarkNoteEdit(rel, line) => self.on_bookmark_note_edit(rel, line),
            Message::BookmarkNoteInput(s) => {
                if let Some((_, _, draft)) = &mut self.note_edit {
                    *draft = s;
                }
                Task::none()
            }
            Message::BookmarkNoteSave => self.on_bookmark_note_save(),
            Message::BookmarkNoteCancel => {
                self.note_edit = None;
                Task::none()
            }
            Message::NoteToggleUnderstood { rel, symbol } => {
                notes::toggle_understood(&mut self.notes, &rel, &symbol);
                self.save_notes();
                Task::none()
            }
            Message::NoteEditStart { rel, symbol } => {
                let existing =
                    notes::find(&self.notes, &rel, &symbol).map(|n| n.text.clone()).unwrap_or_default();
                self.reading_note_edit = Some((rel, symbol, existing));
                operation::focus(ui::note_input_id())
            }
            Message::NoteEditInput(s) => {
                if let Some((_, _, draft)) = &mut self.reading_note_edit {
                    *draft = s;
                }
                Task::none()
            }
            Message::NoteEditSave => {
                if let Some((rel, symbol, draft)) = self.reading_note_edit.take() {
                    notes::set_text(&mut self.notes, &rel, &symbol, &draft);
                    self.save_notes();
                }
                Task::none()
            }
            Message::NoteEditCancel => {
                self.reading_note_edit = None;
                Task::none()
            }
            Message::NoteRemove { rel, symbol } => {
                notes::remove(&mut self.notes, &rel, &symbol);
                self.save_notes();
                Task::none()
            }
            Message::NoteJump { rel, symbol } => {
                let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
                    return Task::none();
                };
                let line = self.note_symbol_line(&rel, &symbol);
                self.open_file(root.join(&rel), line, true)
            }
            Message::GoBack => match self.history.back() {
                Some(loc) => {
                    self.save_history();
                    self.open_file(loc.path, loc.line, false)
                }
                None => Task::none(),
            },
            Message::GoForward => match self.history.forward() {
                Some(loc) => {
                    self.save_history();
                    self.open_file(loc.path, loc.line, false)
                }
                None => Task::none(),
            },
            Message::HistoryJump(id) => match self.history.goto(id) {
                Some(loc) => {
                    self.save_history();
                    self.open_file(loc.path, loc.line, false)
                }
                None => Task::none(),
            },
            Message::TrailToggleCollapse(id) => {
                if !self.trail_collapsed.remove(&id) {
                    self.trail_collapsed.insert(id);
                }
                Task::none()
            }
            Message::HistoryClear => {
                self.history.clear();
                self.save_history();
                Task::none()
            }
            Message::ToggleLeftSidebar => {
                self.show_left_sidebar = !self.show_left_sidebar;
                Task::none()
            }
            Message::ToggleRightPanel => {
                self.show_right_panel = !self.show_right_panel;
                Task::none()
            }
            Message::ToggleDiff => self.on_toggle_diff(),
            Message::DiffLoaded { abs, rel, lines } => {
                self.diff = Some(DiffState { abs, rel, lines });
                Task::none()
            }
            Message::OutlineJump(line) => self.on_outline_jump(line),
            Message::FontSizeDelta(delta) => {
                let old = self.line_height();
                self.font_size = (self.font_size + delta).clamp(9.0, 22.0);
                self.rescale_scroll(old)
            }
            Message::FontSizeReset => {
                let old = self.line_height();
                self.font_size = DEFAULT_FONT_SIZE;
                self.rescale_scroll(old)
            }
            Message::KeyPressed(key, modifiers) => self.handle_key(key, modifiers),
            Message::ModifiersChanged(modifiers) => {
                self.modifiers = modifiers;
                if !modifiers.command() {
                    self.hover = None; // hover is a Cmd-hover affordance
                }
                Task::none()
            }
            Message::TitleBarDragged => iced::window::latest().and_then(iced::window::drag),
            Message::WindowFocusChanged(focused) => {
                self.window_focused = focused;
                Task::none()
            }
            Message::ControlsHover(over) => {
                self.controls_hovered = over;
                Task::none()
            }
            Message::CloseWindow => iced::window::latest().and_then(iced::window::close),
            Message::MinimizeWindow => {
                iced::window::latest().and_then(|id| iced::window::minimize(id, true))
            }
            Message::ToggleFullscreen => {
                self.fullscreen = !self.fullscreen;
                let mode = if self.fullscreen {
                    iced::window::Mode::Fullscreen
                } else {
                    iced::window::Mode::Windowed
                };
                iced::window::latest().and_then(move |id| iced::window::set_mode(id, mode))
            }
            Message::WindowOpened => {
                // Frameless windows lose the OS rounded corners; restore them.
                #[cfg(target_os = "macos")]
                macos::round_corners(10.0);
                Task::none()
            }
            Message::WindowResized(size) => self.on_window_resized(size),
            Message::ResizeSidebar(x) => {
                self.sidebar_width = x;
                self.clamp_panel_sizes();
                Task::none()
            }
            Message::ResizeRight(x) => {
                self.right_width = self.window_width - x;
                self.clamp_panel_sizes();
                Task::none()
            }
            Message::ResizeBottom(y) => {
                self.bottom_height = self.window_height - y;
                self.clamp_panel_sizes();
                Task::none()
            }
            Message::LspStartResult { language, result } => match result {
                Ok(client) => {
                    // Open every already-loaded document of this language.
                    let open_task = self.open_docs_for_language(&language, &client);
                    self.lsp.insert(language, LspSlot::Ready(client));
                    open_task
                }
                Err(e) => {
                    self.status = format!("{language} server failed: {e}");
                    self.lsp.insert(language, LspSlot::Failed(e));
                    Task::none()
                }
            },
            Message::LspConsentDismissed => {
                if let Some(c) = self.pending_lsp_consent.take() {
                    self.lsp.insert(
                        c.language,
                        LspSlot::Unsupported("server download declined".into()),
                    );
                }
                Task::none()
            }
            Message::LspConsentAllowed => self.on_lsp_consent_allowed(),
            Message::LspDownloadResult { language, result } => match result {
                Ok(exe) => {
                    self.status = format!("{language} server installed");
                    self.start_lsp_with(&language, exe)
                }
                Err(e) => {
                    self.status = format!("{language} server download failed: {e}");
                    self.lsp.insert(language, LspSlot::Failed(e));
                    Task::none()
                }
            },
            Message::GotoDefinition { pane, line, col } => self.goto_definition(pane, line, col),
            Message::ContextMenuOpened { pane, line, col, x, y } => self.on_context_menu_opened(pane, line, col, x, y),
            Message::ContextMenuClosed => {
                self.context_menu = None;
                Task::none()
            }
            Message::ContextGoto(kind) => {
                let Some(menu) = self.context_menu.take() else {
                    return Task::none();
                };
                self.goto_request(menu.pane, menu.line, menu.col, kind)
            }
            Message::FindOpened => self.on_find_opened(),
            Message::FindQueryChanged(q) => {
                self.find.query = q;
                if let Some(v) = self.active_viewer() {
                    let lines = v.lines.clone();
                    self.find.recompute(&lines);
                }
                self.jump_to_find_match()
            }
            Message::FindStep(delta) => {
                self.find.step(delta);
                self.jump_to_find_match()
            }
            Message::FindClosed => {
                self.find.open = false;
                self.code_focused = true;
                Task::none()
            }
            Message::HoverRequested { pane, line, col, x, y } => self.on_hover_requested(pane, line, col, x, y),
            Message::HoverDwell { epoch, pane, line, col, x, y } => self.on_hover_dwell(epoch, pane, line, col, x, y),
            Message::HoverResult { line, col, text } => {
                if let Some(h) = &mut self.hover
                    && h.line == line
                    && h.col == col
                {
                    h.text = text;
                }
                Task::none()
            }
            Message::HoverCleared => self.on_hover_cleared(),
            Message::HoverPin(inside) => {
                self.hover_pinned = inside;
                if !inside {
                    // Left the tooltip: dismiss it and cancel any pending dwell.
                    self.hover = None;
                    self.hover_gen = self.hover_gen.wrapping_add(1);
                }
                Task::none()
            }
            Message::DefinitionResult { result } => self.on_definition_result(result),
            Message::ReferencesResult { result } => self.on_references_result(result),
            Message::CallHierarchyRequested => {
                let pane = self.active;
                let Some((line, col)) = self.active_viewer().and_then(|v| v.caret) else {
                    return Task::none();
                };
                self.call_hierarchy_at(pane, line, col)
            }
            Message::CallHierarchyFromMenu => {
                let Some(menu) = self.context_menu.take() else {
                    return Task::none();
                };
                self.call_hierarchy_at(menu.pane, menu.line, menu.col)
            }
            Message::ExplainFromMenu => self.on_explain_from_menu(),
            Message::CallHierarchyPrepared { direction, lang, items } => self.on_call_hierarchy_prepared(direction, lang, items),
            Message::CallHierarchyExpand(id) => self.on_call_hierarchy_expand(id),
            Message::CallHierarchyChildren { id, items } => self.on_call_hierarchy_children(id, items),
            Message::CallHierarchyDirection => self.on_call_hierarchy_direction(),
            Message::CallHierarchyExpandAll => self.on_call_hierarchy_expand_all(),
            Message::ImportExpand(id) => self.on_import_expand(id),
            Message::ImportDirection => {
                self.import_dir = self.import_dir.toggled();
                // A direction flip resets the (now meaningless) expand-all state.
                if let Some(t) = &mut self.import_tree {
                    t.full = false;
                }
                self.refresh_import_tree();
                Task::none()
            }
            Message::ImportExpandAll => {
                if let (Some(mut tree), Some(root)) =
                    (self.import_tree.take(), self.project.as_ref().map(|p| p.root.clone()))
                {
                    tree.expand_all(&self.import_graph, &root);
                    self.import_tree = Some(tree);
                }
                Task::none()
            }
            Message::OpenOverlay(which) => self.on_open_overlay(which),
            Message::OverlayViewToggle => {
                self.graph_mode = !self.graph_mode;
                if self.graph_mode {
                    self.refresh_graph_layout();
                }
                Task::none()
            }
            Message::CloseOverlay => {
                self.overlay = None;
                Task::none()
            }
            Message::OverlayOpenImports(path) => {
                self.overlay = None;
                self.sidebar = SidebarTab::Imports;
                self.open_file(path, None, true)
            }
            Message::OverlayOpenAt { abs, line } => {
                self.overlay = None;
                self.open_file(abs, Some(line), true)
            }
            Message::ProjectCallsBuilt { root, graph } => self.on_project_calls_built(root, graph),
            Message::RefineProjectCalls => self.refine_project_calls(),
            Message::RefineProgress { generation, done, total } => {
                if generation == self.calls_gen {
                    self.refine_progress = Some((done, total));
                }
                Task::none()
            }
            Message::ProjectCallsRefined { root, generation, edges, graph } => self.on_project_calls_refined(root, generation, edges, graph),
            Message::ExplainProject => self.on_explain_project(),
            Message::CancelExplain => self.on_cancel_explain(),
            Message::ExplainProgress { generation, done, total, failed } => {
                if generation == self.explain_gen {
                    self.explain_progress = Some((done, total));
                    self.explain_failed = failed;
                }
                Task::none()
            }
            Message::ExplainDone { root, generation, cache, failed, auth_error } => {
                self.on_explain_done(root, generation, cache, failed, auth_error)
            }
            Message::RefreshAll => self.on_refresh_all(),
            Message::ShowExplanation(node) => {
                self.show_right_panel = true;
                self.show_explanation(node)
            }
            Message::ReexplainNode => self.on_reexplain_node(),
            Message::ExplainBlocks(node) => self.on_explain_blocks(node),
            Message::BlocksExplained { node, detail } => self.on_blocks_explained(node, detail),
            Message::SvgsGenerated { generation, map } => self.on_svgs_generated(generation, map),
            Message::ShowOverview => {
                self.show_overview = true;
                self.show_stats = false;
                self.docs.page = None;
                Task::none()
            }
            Message::ServerConnected(tx) => self.on_server_connected(tx),
            Message::ServerUnavailable => self.on_server_unavailable(),
            Message::OpenConnect => {
                self.connect = Some(ConnectUi::default());
                // Already on a live remote? Skip the form and browse its folders.
                if self.connection.is_remote() && self.server_tx.is_some() {
                    self.enter_remote_browser(None);
                }
                Task::none()
            }
            Message::CloseConnect => {
                self.connect = None;
                Task::none()
            }
            Message::ConnectField(field, value) => self.on_connect_field(field, value),
            Message::ConnectPickIdentity => {
                Task::perform(pick_file(), Message::ConnectIdentityPicked)
            }
            Message::ConnectIdentityPicked(path) => {
                if let (Some(ui), Some(path)) = (&mut self.connect, path) {
                    ui.identity = path.to_string_lossy().into_owned();
                }
                Task::none()
            }
            Message::ConnectSubmit => self.on_connect_submit(),
            Message::ConnectToSaved(idx) => {
                if let Some(conn) = self.saved_connections.get(idx).cloned() {
                    self.connect_to(conn.target());
                }
                Task::none()
            }
            Message::ConnectRemoveSaved(idx) => {
                if idx < self.saved_connections.len() {
                    self.saved_connections.remove(idx);
                    if let Err(e) = connect::save(&self.saved_connections) {
                        self.status = format!("Cannot save connections: {e}");
                    }
                }
                Task::none()
            }
            Message::ConnectDisconnect => {
                self.connect = None;
                if self.connection.is_remote() {
                    self.connect_to(connect::ConnTarget::Local);
                }
                Task::none()
            }
            Message::RemoteBrowseTo(path) => {
                self.enter_remote_browser(Some(path));
                Task::none()
            }
            Message::RemoteBrowseUp => {
                if let Some(ConnectStage::Browsing(b)) =
                    self.connect.as_ref().map(|u| &u.stage)
                    && let Some(parent) = b.parent.clone()
                {
                    self.enter_remote_browser(Some(parent));
                }
                Task::none()
            }
            Message::RemoteOpenHere => self.on_remote_open_here(),
            Message::DocsRefresh => {
                self.request_docs();
                Task::none()
            }
            Message::DocsToggleFile(rel) => {
                if !self.docs.expanded.remove(&rel) {
                    self.docs.expanded.insert(rel);
                }
                Task::none()
            }
            Message::DocsFilterChanged(s) => {
                self.docs.filter = s;
                Task::none()
            }
            Message::DocsToggleShowAll => {
                self.docs.show_all = !self.docs.show_all;
                Task::none()
            }
            Message::DocsToggleGrouping => {
                self.docs.by_module = !self.docs.by_module;
                Task::none()
            }
            Message::DocsSelect { rel, line } => {
                self.open_doc_page(&rel, line);
                Task::none()
            }
            Message::ViewDocsFromMenu => self.on_view_docs_from_menu(),
            Message::RegisterProcFeed { proc, feed } => {
                self.proc_feeds.insert(proc, feed);
                Task::none()
            }
            Message::ServerEvent(msg) => match msg {
                clew_protocol::ServerMessage::Reply { id, event, .. } => {
                    self.handle_server_reply(id, event)
                }
                clew_protocol::ServerMessage::Notification { event, .. } => {
                    self.handle_server_event(event);
                    Task::none()
                }
            },
            Message::ShowStats => {
                self.show_stats = true;
                self.show_overview = false;
                self.docs.page = None;
                // Compute on entry when there's nothing to show or the file set
                // changed since the last run; otherwise the cached report stays.
                self.start_stats(false)
            }
            Message::RefreshStats => self.start_stats(true),
            Message::StatsDone { root, rev, report } => self.on_stats_done(root, rev, report),
            Message::GenerateOverview => self.on_generate_overview(),
            Message::GenerateWalkthrough(scope) => self.on_generate_walkthrough(scope),
            Message::GenerateDiffWalkthrough => self.on_generate_diff_walkthrough(),
            Message::WalkthroughRegenerate(i) => self.on_walkthrough_regenerate(i),
            Message::WalkthroughDelete(i) => self.on_walkthrough_delete(i),
            Message::WalkthroughDone { scope, result } => self.on_walkthrough_done(scope, result),
            Message::WalkthroughOpen(i) => {
                if i >= self.walkthroughs.len() {
                    return Task::none();
                }
                self.walkthrough_open = Some(i);
                self.walkthrough_step = 0;
                self.walkthrough_goto(0)
            }
            Message::WalkthroughBack => {
                self.walkthrough_open = None;
                self.walkthrough_prepared = Vec::new();
                Task::none()
            }
            Message::WalkthroughToggleMode => {
                self.walkthrough_mode = match self.walkthrough_mode {
                    WalkMode::Search => WalkMode::Walk,
                    WalkMode::Walk => WalkMode::Search,
                };
                Task::none()
            }
            Message::WalkthroughGoto(i) => self.walkthrough_goto(i),
            Message::WalkthroughStep(delta) => self.on_walkthrough_step(delta),
            Message::WalkthroughInputChanged(s) => {
                self.walkthrough_input = s;
                Task::none()
            }
            Message::ResizeWalkNarration(y) => {
                // Narration height = distance from the drag point to the window
                // bottom, clamped so neither block collapses.
                let max = (self.window_height - 160.0).max(120.0);
                self.walkthrough_narration_height = (self.window_height - y).clamp(90.0, max);
                Task::none()
            }
            Message::OverviewDone { root, prompt_hash, result } => self.on_overview_done(root, prompt_hash, result),
            Message::BuildEmbeddings => self.on_build_embeddings(),
            Message::EmbeddingsBuilt { root, result } => self.on_embeddings_built(root, result),
            Message::SemanticQueryChanged(q) => {
                self.semantic_query = q;
                Task::none()
            }
            Message::SemanticSearch => self.on_semantic_search(),
            Message::SemanticResults { query, result } => self.on_semantic_results(query, result),
            Message::OpenNode(node) => match node {
                explain::Node::Function { file, name } => {
                    let line = self
                        .symbol_index_by_file
                        .get(&file)
                        .and_then(|syms| syms.iter().find(|s| s.name == name).map(|s| s.line));
                    self.open_file(file, line, true)
                }
                explain::Node::File(p) => self.open_file(p, None, true),
                explain::Node::Folder(p) => self.show_explanation(explain::Node::Folder(p)),
            },
            Message::JumpToCall { caller_file, caller, callee } => {
                let line = self.call_site_line(&caller_file, &caller, &callee).or_else(|| {
                    self.symbol_index_by_file
                        .get(&caller_file)
                        .and_then(|syms| syms.iter().find(|s| s.name == caller).map(|s| s.line))
                });
                self.open_file(caller_file, line, true)
            }
            Message::ToggleAsk => self.on_toggle_ask(),
            Message::BottomTabPicked(tab) => {
                self.show_bottom = true;
                self.bottom_tab = tab;
                Task::none()
            }
            Message::CollapseBottom => {
                self.show_bottom = false;
                Task::none()
            }
            Message::ToggleToolsMenu => {
                self.show_tools_menu = !self.show_tools_menu;
                self.show_target_menu = false;
                Task::none()
            }
            Message::ToggleTargetMenu => {
                self.show_target_menu = !self.show_target_menu;
                self.show_tools_menu = false;
                Task::none()
            }
            Message::OpenShortcuts => {
                self.show_tools_menu = false;
                self.show_shortcuts = true;
                self.rebinding = None;
                self.keymap_notice = None;
                Task::none()
            }
            Message::CloseShortcuts => {
                self.show_shortcuts = false;
                self.rebinding = None;
                self.keymap_notice = None;
                Task::none()
            }
            Message::RebindStart(action) => {
                self.rebinding = Some(action);
                self.keymap_notice = None;
                Task::none()
            }
            Message::RebindReset(action) => {
                self.keymap.reset(action);
                self.rebinding = None;
                self.keymap_notice = None;
                if let Err(e) = self.keymap.save() {
                    self.status = format!("Could not save shortcuts: {e}");
                }
                Task::none()
            }
            Message::RebindResetAll => {
                self.keymap.reset_all();
                self.rebinding = None;
                self.keymap_notice = None;
                if let Err(e) = self.keymap.save() {
                    self.status = format!("Could not save shortcuts: {e}");
                }
                Task::none()
            }
            Message::ToggleInlineSummaries => {
                self.show_inline_summaries = !self.show_inline_summaries;
                self.show_tools_menu = false;
                Task::none()
            }
            Message::ToggleFileBanner => {
                self.show_file_banner = !self.show_file_banner;
                self.show_tools_menu = false;
                Task::none()
            }
            Message::ToggleMinimap => {
                self.show_minimap = !self.show_minimap;
                self.show_tools_menu = false;
                Task::none()
            }
            Message::TargetSelected(target) => self.on_target_selected(target),
            Message::ToggleInlayHints => self.on_toggle_inlay_hints(),
            Message::SkimFile => {
                self.skim_active_file();
                self.show_tools_menu = false;
                Task::none()
            }
            Message::AskInputChanged(s) => {
                self.ask_input = s;
                Task::none()
            }
            Message::AskSuggested(q) => {
                self.ask_input = q;
                Task::done(Message::AskSubmit)
            }
            Message::AskSubmit => self.on_ask_submit(),
            Message::AskRetrieved { question, qvec } => self.on_ask_retrieved(question, qvec),
            Message::AskDelta(text) => self.on_ask_delta(text),
            Message::AskStreamEnded(error) => self.on_ask_stream_ended(error),
            Message::AskClear => {
                self.ask_turns.clear();
                self.ask_pins.clear();
                Task::none()
            }
            Message::AskUnpin(i) => {
                if i < self.ask_pins.len() {
                    self.ask_pins.remove(i);
                }
                Task::none()
            }
            Message::AskPinGoto(i) => {
                match self.ask_pins.get(i) {
                    Some(pin) => self.open_file(pin.file.clone(), Some(pin.line), true),
                    None => Task::none(),
                }
            }
            Message::AskAboutSelection => self.on_ask_about_selection(),
            Message::WhyIsThisHere => self.on_why_is_this_here(),
            Message::BlameWhyDone { title, commits, result } => self.on_blame_why_done(title, commits, result),
            Message::BlameWhyClose => {
                self.blame_why = None;
                Task::none()
            }
            Message::TimeTravelStart { symbol } => self.on_time_travel_start(symbol),
            Message::TimeTravelReady { generation, abs, rel, lang, scope, commits } => self.on_time_travel_ready(generation, abs, rel, lang, scope, commits),
            Message::TimeTravelGoto(idx) => self.on_time_travel_goto(idx),
            Message::TimeTravelStep { generation, idx, step } => self.on_time_travel_step(generation, idx, step),
            Message::TimeTravelScrolled(viewport) => self.on_time_travel_scrolled(viewport),
            Message::TimeTravelSelectStart { line, col } => self.on_time_travel_select_start(line, col),
            Message::TimeTravelSelectDrag { line, col } => self.on_time_travel_select_drag(line, col),
            Message::TimeTravelToggleScope => {
                let Some(tt) = self.time_travel.as_ref() else {
                    return Task::none();
                };
                // File -> the function at the current focus/caret; Symbol -> File.
                let symbol = matches!(tt.scope, TimeScope::File);
                Task::done(Message::TimeTravelStart { symbol })
            }
            Message::TimeTravelExit => {
                self.time_travel = None;
                self.time_gen += 1; // invalidate any in-flight loads
                // Restore the live pane to where the reader was before entering
                // (its scrollable remounts at the top otherwise).
                let y = self.active_viewer().map(|v| v.scroll_y).unwrap_or(0.0);
                operation::scroll_to(ui::code_scroll_id(self.active), AbsoluteOffset { x: 0.0, y })
            }
            Message::TimeTravelWhy => self.on_time_travel_why(),
            Message::TimeTravelWhyDone { generation: _, sha, result } => self.on_time_travel_why_done(sha, result),
            Message::TimeTravelStory => self.on_time_travel_story(),
            Message::TimeTravelStoryDone { generation: _, result } => self.on_time_travel_story_done(result),
            Message::Noop => Task::none(),
            Message::StartDebug => self.start_debug(),
            Message::DapStarted { client, port } => {
                if let Some(session) = self.debug.as_mut() {
                    session.client = Some(client);
                    session.port = port;
                    session.status = DebugStatus::Running;
                    self.status = "Debugger running…".into();
                }
                Task::none()
            }
            Message::DapChildStarted(client) => {
                // js-debug's child session owns the real target: make it active.
                if let Some(session) = self.debug.as_mut() {
                    session.client = Some(client);
                }
                Task::none()
            }
            Message::DapEvent(ev) => self.on_dap_event(ev),
            Message::DapStopInspected { frames, scopes } => self.on_dap_stop_inspected(frames, scopes),
            Message::DebugControl(cmd) => self.debug_control(cmd),
            Message::DebugStop => self.on_debug_stop(),
            Message::BreakpointToggle { path, line } => self.on_breakpoint_toggle(path, line),
            Message::DebugFailed(e) => {
                self.debug = None;
                self.status = format!("Debug failed: {e}");
                Task::none()
            }
            Message::ToggleBreakpointFromMenu => self.on_toggle_breakpoint_from_menu(),
            Message::ConditionalBreakpointFromMenu => self.on_conditional_breakpoint_from_menu(),
            Message::BpConditionInput(s) => {
                if let Some((_, _, draft)) = &mut self.bp_cond_edit {
                    *draft = s;
                }
                Task::none()
            }
            Message::BpConditionSet => self.on_bp_condition_set(),
            Message::BpConditionCancel => {
                self.bp_cond_edit = None;
                Task::none()
            }
            Message::DebugWatchInput(s) => {
                self.debug_watch_input = s;
                Task::none()
            }
            Message::DebugWatchAdd => {
                let expr = self.debug_watch_input.trim().to_string();
                if expr.is_empty() {
                    return Task::none();
                }
                self.debug_watches.push(expr);
                self.debug_watch_input.clear();
                self.eval_watches()
            }
            Message::DebugWatchRemove(i) => self.on_debug_watch_remove(i),
            Message::DebugWatchesEvaluated(vals) => {
                if let Some(s) = self.debug.as_mut() {
                    s.watches = vals;
                }
                Task::none()
            }
            Message::SettingsEmbedKeyChanged(s) => {
                self.settings.embed_key = s;
                Task::none()
            }
            Message::SettingsEmbedModelChanged(s) => {
                self.settings.embed_model = s;
                Task::none()
            }
            Message::SettingsEmbedBaseUrlChanged(s) => {
                self.settings.embed_base_url = s;
                Task::none()
            }
            Message::CloseExplanation => {
                self.explain_view = None;
                self.explain_prepared = Vec::new();
                self.explain_showing_detail = false;
                Task::none()
            }
            Message::ToggleMarkdownSource(pane) => {
                if let Some(v) = self.panes.get_mut(pane).and_then(Option::as_mut) {
                    v.show_source = !v.show_source;
                }
                Task::none()
            }
            Message::OpenLink(url) => self.on_open_link(url),
            Message::OpenSettings => self.on_open_settings(),
            Message::CloseSettings => {
                self.settings.open = false;
                Task::none()
            }
            Message::SettingsProviderPicked(p) => {
                // Switching provider resets model/base_url to that provider's
                // defaults (the user can still edit them).
                self.settings.provider = p;
                self.settings.model = p.default_model().to_string();
                self.settings.base_url = p.default_base_url().to_string();
                Task::none()
            }
            Message::SettingsKeyChanged(s) => {
                self.settings.key = s;
                Task::none()
            }
            Message::SettingsModelChanged(s) => {
                self.settings.model = s;
                Task::none()
            }
            Message::SettingsBaseUrlChanged(s) => {
                self.settings.base_url = s;
                Task::none()
            }
            Message::SettingsSaved => self.on_settings_saved(),
            Message::Tick => self.on_tick(),
            Message::ToggleServerPanel => {
                self.server_panel = !self.server_panel;
                if self.server_panel {
                    self.installed_servers = lsp::store::installed_servers();
                }
                Task::none()
            }
            Message::LspRestart(language) => {
                // Drop the running server (kills its child), then re-provision.
                self.lsp.remove(&language);
                self.lsp_opened.retain(|p| {
                    // Re-open docs of this language on restart.
                    highlight::detect(p) != Some(language.as_str())
                });
                self.ensure_lsp(&language)
            }
            Message::LspRemove { name, version } => self.on_lsp_remove(name, version),
            Message::LspDownloadFor(language) => {
                // Force a fresh provisioning attempt for this language.
                self.lsp.remove(&language);
                self.ensure_lsp(&language)
            }
        }
    }

    // ---- Explain-domain handlers (extracted from `update`) --------------------

    /// Explain the whole project (bottom-up LLM pass), abortable from the UI.
    fn on_explain_project(&mut self) -> Task<Message> {
        let Some(cfg) = llm::Config::load() else {
            self.status = format!("Set your Anthropic key in {}", llm::config_hint());
            return Task::none();
        };
        let Some(project) = &self.project else {
            return Task::none();
        };
        let root = project.root.clone();
        let files: Vec<PathBuf> = project.files.iter().map(|f| f.abs.clone()).collect();
        let prev = self.explanations.clone();
        let ai = self.ai_client();
        self.explain_gen += 1;
        let generation = self.explain_gen;
        self.explaining = true;
        self.explain_progress = Some((0, 0));
        self.explain_failed = 0;
        self.status = "Explaining project…".into();
        let stream = iced::stream::channel(256, move |output| {
            let gather_root = root.clone();
            async move {
                let inputs = tokio::task::spawn_blocking(move || {
                    gather_explain_inputs(files, gather_root)
                })
                .await
                .unwrap_or_default();
                explain_stream(output, inputs, prev, cfg, ai, root, generation).await;
            }
        });
        // Abortable so a long project pass (thousands of LLM calls on a big repo)
        // can be cancelled from the UI; the handle is dropped when the pass
        // finishes (ExplainDone) or is cancelled.
        let (task, handle) = Task::run(stream, |m| m).abortable();
        self.explain_abort = Some(handle);
        task
    }

    /// Cancel the running explain pass (abort remaining calls, keep + save work).
    fn on_cancel_explain(&mut self) -> Task<Message> {
        // Stop the in-flight pass: abort the task (halts further LLM calls) and
        // bump the generation so any already-queued progress messages are
        // ignored. Cached explanations so far are kept.
        if let Some(handle) = self.explain_abort.take() {
            handle.abort();
        }
        self.explain_gen += 1;
        self.explaining = false;
        self.explain_progress = None;
        if let Some(root) = self.project.as_ref().map(|p| p.root.clone()) {
            let _ = explain::save(&root, &self.explanations);
        }
        self.status = "Explain cancelled".into();
        Task::none()
    }

    /// Fold a finished project explain pass into state and fan out the downstream
    /// refresh (index / overview / open panel), reporting the outcome honestly.
    fn on_explain_done(
        &mut self,
        root: PathBuf,
        generation: u64,
        cache: explain::Cache,
        failed: usize,
        auth_error: Option<String>,
    ) -> Task<Message> {
        if generation != self.explain_gen
            || self.project.as_ref().map(|p| &p.root) != Some(&root)
        {
            return Task::none();
        }
        self.explanations = cache;
        self.explaining = false;
        self.explain_progress = None;
        self.explain_abort = None;
        self.explain_failed = failed;
        let _ = explain::save(&root, &self.explanations);
        // Report honestly: a rejected key stops the pass and says why; a partial
        // run names how many failed; only a clean pass claims unqualified success.
        let n = self.explanations.len();
        self.status = if let Some(err) = auth_error {
            let reason: String = err.lines().next().unwrap_or(&err).chars().take(160).collect();
            format!("Explain stopped — the LLM rejected the request ({reason}). Check your API key in Settings.")
        } else if failed > 0 {
            format!("Explained {n} · {failed} failed — check your LLM connection and retry")
        } else {
            format!("Explained {n} functions/files/folders")
        };

        // Propagate the refreshed summaries to the downstream artifacts already in
        // use, each guarded so an unchanged input stays cheap. These run in the
        // background — they never switch the user's view.
        let mut tasks = Vec::new();
        if self.embed_available && !self.explanations.is_empty() {
            tasks.push(Task::done(Message::BuildEmbeddings));
        }
        if self.overview.is_some() && self.overview_inputs_changed() {
            tasks.push(Task::done(Message::GenerateOverview));
        }
        if self.refresh_pending {
            tasks.push(self.request_auto_refresh());
        }
        if let Some(node) = self.explain_view.clone() {
            let fresh_detail = self.explain_showing_detail
                .then(|| self.explanations.get(&node).and_then(|c| c.detail.clone()))
                .flatten();
            tasks.push(match fresh_detail {
                Some(detail) => self.show_detail(node, detail),
                None => self.show_explanation(node),
            });
        }
        Task::batch(tasks)
    }

    /// Re-explain the node in the open panel. Runs the cache-aware project pass
    /// (which regenerates this node and anything that embedded its summary), but
    /// only when the node is already cached — otherwise a single click would
    /// explain the whole project, so point the user at the explicit Explain-All.
    fn on_reexplain_node(&mut self) -> Task<Message> {
        let Some(node) = self.explain_view.clone() else {
            return Task::none();
        };
        if !self.llm_available {
            self.status = format!("Add an API key in Settings ({})", llm::config_hint());
            return Task::none();
        }
        if !self.explanations.contains_key(&node) {
            self.status =
                "Nothing to re-explain yet — run Explain in the toolbar to explain the project first.".into();
            return Task::none();
        }
        self.explanations.remove(&node);
        self.status = "Re-explaining…".into();
        Task::done(Message::ExplainProject)
    }

    /// Generate (or show the cached) block-by-block walkthrough for a function.
    fn on_explain_blocks(&mut self, node: explain::Node) -> Task<Message> {
        let explain::Node::Function { file, name } = node.clone() else {
            return Task::none(); // block detail only applies to functions
        };
        // Already generated? Show the cached walkthrough immediately.
        if let Some(detail) = self.explanations.get(&node).and_then(|c| c.detail.clone()) {
            return self.show_detail(node, detail);
        }
        let Some(cfg) = llm::Config::load() else {
            self.status = format!("Set your Anthropic key in {}", llm::config_hint());
            return Task::none();
        };
        // Unique-name → summary map so the off-thread gather can attach callee
        // context (ambiguous names resolve to None and are skipped).
        let mut summaries: HashMap<String, Option<String>> = HashMap::new();
        for (n, c) in &self.explanations {
            if let explain::Node::Function { name: fname, .. } = n {
                summaries
                    .entry(fname.clone())
                    .and_modify(|e| *e = None)
                    .or_insert_with(|| Some(c.summary.clone()));
            }
        }
        self.status = "Explaining blocks…".into();
        let ai = self.ai_client();
        Task::perform(
            async move {
                let prompt = tokio::task::spawn_blocking(move || {
                    let Some((sig, body, callees)) =
                        gather_fn_detail_input(file, &name, &summaries)
                    else {
                        return Err::<String, String>("function body not found".to_string());
                    };
                    Ok(explain::detail_prompt(&name, &sig, &body, &callees))
                })
                .await
                .unwrap_or_else(|_| Err("task join failed".into()));
                match prompt {
                    Ok(p) => ai.complete(cfg, EXPLAIN_BLOCKS_SYSTEM, p, 1024).await,
                    Err(e) => Err(e),
                }
            },
            move |detail| Message::BlocksExplained { node: node.clone(), detail },
        )
    }

    /// Persist a generated block walkthrough and show it if still on that node.
    fn on_blocks_explained(
        &mut self,
        node: explain::Node,
        detail: Result<String, String>,
    ) -> Task<Message> {
        match detail {
            Ok(md) => {
                // Persist the walkthrough alongside the summary (dropped
                // automatically when the entry is regenerated).
                if let Some(c) = self.explanations.get_mut(&node) {
                    c.detail = Some(md.clone());
                    if let Some(root) = self.project.as_ref().map(|p| p.root.clone()) {
                        let _ = explain::save(&root, &self.explanations);
                    }
                }
                self.status = "Explained blocks".into();
                // Only swap the view if the user is still on this node.
                if self.explain_view.as_ref() == Some(&node) {
                    return self.show_detail(node, md);
                }
            }
            Err(e) => self.status = format!("Block explanation failed: {e}"),
        }
        Task::none()
    }

    // ---- Walkthrough-domain handlers (extracted from `update`) ----------------

    /// Generate a scoped AI walkthrough (guided reading tour) of the project.
    fn on_generate_walkthrough(&mut self, scope: String) -> Task<Message> {
        let Some(cfg) = llm::Config::load() else {
            self.status = format!("Add an API key in Settings ({})", llm::config_hint());
            return Task::done(Message::OpenSettings);
        };
        if self.project.is_none() {
            return Task::none();
        }
        let project_name = self
            .project
            .as_ref()
            .and_then(|p| p.root.file_name())
            .and_then(|s| s.to_str())
            .unwrap_or("project")
            .to_string();
        let context = self.gather_walkthrough_context();
        let overview = self.overview.clone();
        let scope = scope.trim().to_string();
        let scope_opt = (!scope.is_empty()).then(|| scope.clone());
        let prompt = walkthrough::prompt(
            &project_name,
            overview.as_deref(),
            &context,
            scope_opt.as_deref(),
        );
        self.generating_walkthrough = Some(scope.clone());
        self.status = "Generating walkthrough…".into();
        let ai = self.ai_client();
        Task::perform(
            async move {
                let resp = ai.complete(cfg, walkthrough::SYSTEM, prompt, 4096).await;
                resp.and_then(|r| walkthrough::parse(&r))
            },
            move |result| Message::WalkthroughDone { scope: scope.clone(), result },
        )
    }

    /// Generate a "review my changes" walkthrough from the diff vs the review base.
    fn on_generate_diff_walkthrough(&mut self) -> Task<Message> {
        let Some(cfg) = llm::Config::load() else {
            self.status = format!("Add an API key in Settings ({})", llm::config_hint());
            return Task::done(Message::OpenSettings);
        };
        let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
            return Task::none();
        };
        let Some((base, label)) = git::review_base(&root) else {
            self.status =
                "Nothing to review (need a branch vs main/master, or a prior commit)".into();
            return Task::none();
        };
        let project_name = root
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("project")
            .to_string();
        // Collect the change: intent, changed files + their symbols, patch.
        let commits = git::commit_subjects(&root, &base);
        let mut changed_text = String::new();
        for (rel, ch) in git::changed_files(&root, &base) {
            changed_text.push_str(&format!("{ch} {rel}\n"));
            if let Some(syms) = self.symbol_index_by_file.get(&root.join(&rel)) {
                for s in syms.iter().filter(|s| {
                    matches!(
                        s.kind.as_str(),
                        "function" | "method" | "struct" | "class" | "enum" | "trait"
                    )
                }) {
                    changed_text.push_str(&format!("    {} {} @ L{}\n", s.kind, s.name, s.line));
                }
            }
        }
        let patch = git::range_patch(&root, &base, 12000);
        let prompt = walkthrough::diff_prompt(&project_name, &label, &commits, &changed_text, &patch);
        // A sentinel scope so the library shows it as a change review and
        // Regenerate re-runs the diff (not a normal scoped tour).
        let scope = format!("@diff {label}");
        self.generating_walkthrough = Some(scope.clone());
        self.status = "Reviewing changes…".into();
        let ai = self.ai_client();
        Task::perform(
            async move {
                let resp = ai.complete(cfg, walkthrough::DIFF_SYSTEM, prompt, 4096).await;
                resp.and_then(|r| walkthrough::parse(&r))
            },
            move |result| Message::WalkthroughDone { scope: scope.clone(), result },
        )
    }

    /// Fold a finished walkthrough into the library and open it (retry once on a
    /// malformed-JSON parse error).
    fn on_walkthrough_done(
        &mut self,
        scope: String,
        result: Result<walkthrough::Walkthrough, String>,
    ) -> Task<Message> {
        self.generating_walkthrough = None;
        match result {
            Ok(mut wt) => {
                // Drop steps that don't resolve to a real project file.
                wt.steps.retain(|s| self.resolve_walk_file(&s.file).is_some());
                if wt.steps.is_empty() {
                    self.status = "Walkthrough had no valid steps".into();
                    return Task::none();
                }
                wt.scope = scope.clone();
                // Upsert by scope: regenerating a tour replaces it in place, a
                // fresh scope is appended.
                let idx = match self.walkthroughs.iter().position(|w| w.scope == scope) {
                    Some(i) => {
                        self.walkthroughs[i] = wt;
                        i
                    }
                    None => {
                        self.walkthroughs.push(wt);
                        self.walkthroughs.len() - 1
                    }
                };
                self.walkthrough_open = Some(idx);
                self.walkthrough_step = 0;
                self.sidebar = SidebarTab::Walk;
                self.show_left_sidebar = true;
                self.walkthrough_retried = false;
                if let Some(root) = self.project.as_ref().map(|p| p.root.clone())
                    && let Err(e) = walkthrough::save_library(&root, &self.walkthroughs)
                {
                    self.status = format!("Could not save walkthrough: {e}");
                }
                self.walkthrough_goto(0)
            }
            Err(e) => {
                // The model occasionally returns malformed JSON — retry the
                // generation once before surfacing the failure.
                if e.starts_with("parse") && !self.walkthrough_retried {
                    self.walkthrough_retried = true;
                    self.status = "Retrying walkthrough…".into();
                    return if scope.starts_with("@diff") {
                        Task::done(Message::GenerateDiffWalkthrough)
                    } else {
                        Task::done(Message::GenerateWalkthrough(scope))
                    };
                }
                self.walkthrough_retried = false;
                self.status = format!("Walkthrough failed: {e}");
                Task::none()
            }
        }
    }

    /// A watched source file changed, so the understanding may be stale. Start a
    /// refresh now if the cooldown has lifted and nothing is running; otherwise
    /// mark it pending for the next `Tick` past the window, so no change is
    /// dropped. Only refreshes what already exists — the first build of each
    /// artifact stays an explicit user action.
    fn on_files_changed(&mut self, paths: Vec<PathBuf>) -> Task<Message> {
        let open: HashSet<PathBuf> =
            self.panes.iter().flatten().map(|v| v.abs.clone()).collect();
        // Every file the tree currently lists. The registry only tracks
        // source files, so it can't tell a new/removed non-source file
        // from an edit to one — the tree's own file list can.
        let known: HashSet<&PathBuf> = self
            .project
            .as_ref()
            .map(|p| p.files.iter().map(|f| &f.abs).collect())
            .unwrap_or_default();
        let mut seen = HashSet::new();
        // Split the changed paths in two. Content-tracked files (open,
        // already tracked, or a source file we index) are read + hashed
        // for a real content refresh. Everything else that changed (a
        // .txt, a .json) can't change content we display, but it can be
        // the *creation* or *deletion* of a tree entry — so it gets a
        // cheap existence probe (stat, no read) instead. The probe pairs
        // each path with whether the tree currently lists it; a mismatch
        // with on-disk existence is a create/delete that needs a rescan.
        let mut candidates: Vec<(PathBuf, incremental::Version)> = Vec::new();
        let mut probes: Vec<(PathBuf, bool)> = Vec::new();
        for p in paths {
            if !seen.insert(p.clone()) {
                continue;
            }
            if open.contains(&p)
                || self.registry.is_tracked(&p)
                || highlight::detect(&p).is_some()
            {
                let v = self.registry.version(&p).unwrap_or(0);
                candidates.push((p, v));
            } else {
                let in_tree = known.contains(&p);
                probes.push((p, in_tree));
            }
        }
        if candidates.is_empty() && probes.is_empty() {
            return Task::none();
        }
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    let events = watch::rehash(candidates);
                    let fs_structural = watch::structural_changes(&probes);
                    (events, fs_structural)
                })
                .await
                .unwrap_or_default()
            },
            |(events, fs_structural)| Message::FilesRehashed { events, fs_structural },
        )
    }

    fn on_files_rehashed(&mut self, events: Vec<watch::FileEvent>, fs_structural: bool) -> Task<Message> {
        let mut tasks = Vec::new();
        let mut index_dirty = false;
        // Non-source creations/deletions are already decided by the
        // existence probe in `FilesChanged`; source ones are folded in
        // per event below.
        let mut structural = fs_structural;
        let mut graph_dirty = false;
        let mut refreshed = 0usize;
        let mut touched: Vec<PathBuf> = Vec::new();
        // One resolver for the whole batch, over the current file set. A
        // structural change re-resolves the whole graph later (once the
        // rescan lands the new file set); here we only refresh out-edges.
        let resolver = self.import_resolver();
        for event in events {
            match event {
                watch::FileEvent::Modified(c) => {
                    touched.push(c.path.clone());
                    let lang_key = highlight::detect(&c.path);
                    // An untracked *source* file appearing is its creation,
                    // so the tree must gain it. Non-source create/delete is
                    // handled by the existence probe, which keeps an open
                    // non-source file merely being edited from looking
                    // structural here.
                    structural |= lang_key.is_some() && !self.registry.is_tracked(&c.path);
                    self.registry.set(c.path.clone(), c.hash);

                    // Re-index this one file in place (open or not).
                    if let Some(lang) = lang_key {
                        let rel = self.rel_of(&c.path);
                        let syms = index::file_symbols(&c.path, &rel, &c.content, lang);
                        if syms.is_empty() {
                            index_dirty |= self.symbol_index_by_file.remove(&c.path).is_some();
                        } else {
                            self.symbol_index_by_file.insert(c.path.clone(), syms);
                            index_dirty = true;
                        }
                        // Re-extract this file's imports and refresh its
                        // out-edges in the graph.
                        if let Some(res) = &resolver {
                            let raw = index::file_imports(&c.content, lang);
                            graph_dirty |= self.import_graph.set_file(
                                c.path.clone(),
                                raw,
                                res,
                                highlight::detect,
                            );
                        }
                    }

                    // Refresh every pane showing this file, keeping the
                    // reader's scroll/caret/folds so nothing jumps.
                    let mut on_screen = false;
                    for slot in &mut self.panes {
                        if let Some(v) = slot.as_mut().filter(|v| v.abs == c.path) {
                            let lines = highlight::plain_lines(&c.content);
                            v.reload(c.content.clone(), lines);
                            on_screen = true;
                        }
                    }
                    if on_screen {
                        refreshed += 1;
                        tasks.push(self.content_tasks(
                            c.path.clone(),
                            c.content.clone(),
                            lang_key,
                        ));
                        if let Some(lang) = lang_key
                            && let Some(LspSlot::Ready(client)) = self.lsp.get(lang)
                        {
                            self.lsp_doc_rev += 1;
                            client.did_change(&c.path, self.lsp_doc_rev, &c.content);
                        }
                    }
                }
                watch::FileEvent::Deleted(path) => {
                    touched.push(path.clone());
                    structural = true;
                    self.registry.remove(&path);
                    index_dirty |= self.symbol_index_by_file.remove(&path).is_some();
                    self.import_graph.remove_file(&path);
                    graph_dirty = true;
                    if self.panes.iter().flatten().any(|v| v.abs == path) {
                        self.status = format!("{} was deleted on disk", self.rel_of(&path));
                    }
                }
            }
        }
        if index_dirty {
            self.rebuild_symbol_index();
        }
        // Keep the reading trail anchored across edits: re-point each
        // changed file's history entries to their symbol's new line. A
        // deleted file has no symbols left, so its entries keep their line
        // (clicking one just reports the file is gone).
        let mut trail_moved = false;
        for path in &touched {
            let symbols: Vec<(String, usize)> = self
                .symbol_index_by_file
                .get(path)
                .map(|syms| {
                    syms.iter()
                        .filter(|s| matches!(s.kind.as_str(), "function" | "method"))
                        .map(|s| (s.name.clone(), s.line))
                        .collect()
                })
                .unwrap_or_default();
            trail_moved |= self.history.reanchor(path, &symbols);
        }
        if trail_moved {
            self.save_history();
        }
        // A pure content change only refreshes out-edges, so update the
        // tree now. A structural change re-resolves the whole graph once
        // the rescan lands the new file set (see `TreeUpdated`).
        if graph_dirty && !structural {
            self.import_cycles = self.import_graph.cycles();
            self.refresh_import_tree();
        }
        // If a file the open call hierarchy references changed, the tree
        // may now be out of date — flag it (re-run `gc` to refresh).
        if let Some(t) = &mut self.call_graph
            && !t.stale
            && touched.iter().any(|p| t.depends_on(p))
        {
            t.stale = true;
        }
        // Keep the open LSP-precise call graph fresh: re-query just the
        // changed files' functions, coalescing while a refine is running.
        if self.project_calls_precise && self.overlay == Some(Overlay::ProjectCalls) {
            let changed: HashSet<PathBuf> = touched
                .iter()
                .filter(|p| highlight::detect(p).is_some())
                .cloned()
                .collect();
            if !changed.is_empty() {
                self.precise_pending.extend(changed);
                if self.refine_progress.is_none() {
                    let pending = std::mem::take(&mut self.precise_pending);
                    tasks.push(self.refine_incremental(pending));
                }
            }
        }
        // A created/deleted/renamed file changes the tree and Cmd+P list;
        // rebuild them off-thread (the watcher already debounced the burst).
        if structural
            && let Some(root) = self.project.as_ref().map(|p| p.root.clone())
        {
            tasks.push(self.rescan_tree(root));
        }
        if refreshed == 1 {
            self.status = "Refreshed a file changed on disk".to_string();
        } else if refreshed > 1 {
            self.status = format!("Refreshed {refreshed} files changed on disk");
        }
        // A source file changed → the understanding (explanations →
        // index → overview) may be stale. Auto-refresh it, throttled to
        // AUTO_REFRESH_MIN_INTERVAL so an edit burst coalesces into one
        // pass (see `request_auto_refresh`).
        if touched.iter().any(|p| highlight::detect(p).is_some()) {
            tasks.push(self.request_auto_refresh());
        }
        Task::batch(tasks)
    }

    fn on_build_embeddings(&mut self) -> Task<Message> {
        let Some(cfg) = embed::Config::load() else {
            self.status = "Configure an embedding endpoint in Settings".into();
            return Task::none();
        };
        if self.explanations.is_empty() {
            self.status = "Run Explain All first — the index embeds the summaries".into();
            return Task::none();
        }
        let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
            return Task::none();
        };
        let nodes = self.gather_embed_nodes();
        let existing = std::mem::take(&mut self.embed_index);
        self.building_embeddings = true;
        self.status = "Building semantic index…".into();
        let ai = self.ai_client();
        Task::perform(
            async move { build_embeddings(&ai, &cfg, nodes, existing).await },
            move |result| Message::EmbeddingsBuilt { root: root.clone(), result },
        )
    }

    fn on_semantic_search(&mut self) -> Task<Message> {
        let query = self.semantic_query.trim().to_string();
        if query.is_empty() {
            return Task::none();
        }
        let Some(cfg) = embed::Config::load() else {
            self.status = "Configure an embedding endpoint in Settings".into();
            return Task::none();
        };
        if self.embed_index.entries.is_empty() {
            self.status = "Build the semantic index first (Semantic tab → Build index)".into();
            return Task::none();
        }
        self.searching_semantic = true;
        let label = query.clone();
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    embed::embed_batch(&cfg, std::slice::from_ref(&query))
                        .map(|mut v| v.pop().unwrap_or_default())
                })
                .await
                .unwrap_or_else(|_| Err("task join failed".into()))
            },
            move |result| Message::SemanticResults { query: label.clone(), result },
        )
    }

    fn on_ask_submit(&mut self) -> Task<Message> {
        let question = self.ask_input.trim().to_string();
        if question.is_empty() {
            return Task::none();
        }
        let Some(_lcfg) = llm::Config::load() else {
            self.status = "Configure an LLM provider in Settings to ask".into();
            return Task::none();
        };
        // Semantic retrieval needs an embedding index. But when the
        // debugger is paused or a selection is pinned, that live context
        // is the grounding — allow asking without an index.
        let ecfg = embed::Config::load();
        let has_index = !self.embed_index.entries.is_empty() && ecfg.is_some();
        let grounded = self.debug_context().is_some() || !self.ask_pins.is_empty();
        if !has_index && !grounded {
            // Be specific when a pass is already building the index, so a
            // question asked mid-"Explain All" doesn't read as a silent no-op.
            self.status = if self.explaining || self.building_embeddings {
                "Ask needs the semantic index — it's building now (finish Explain All), then re-ask".into()
            } else {
                "Build the semantic index first (FIND tab → Build index)".into()
            };
            return Task::none();
        }
        self.ask_input.clear();
        self.show_bottom = true;
        self.bottom_tab = BottomTab::Ask;
        self.asking = true;
        match ecfg.filter(|_| has_index) {
            Some(ecfg) => {
                let q = question.clone();
                Task::perform(
                    async move {
                        tokio::task::spawn_blocking(move || {
                            embed::embed_batch(&ecfg, std::slice::from_ref(&q))
                                .map(|mut v| v.pop().unwrap_or_default())
                        })
                        .await
                        .unwrap_or_else(|_| Err("task join failed".into()))
                    },
                    move |qvec| Message::AskRetrieved { question: question.clone(), qvec },
                )
            }
            // No index: skip retrieval, answer from the live grounding.
            None => Task::done(Message::AskRetrieved { question, qvec: Ok(Vec::new()) }),
        }
    }

    fn on_ask_retrieved(&mut self, question: String, qvec: Result<Vec<f32>, String>) -> Task<Message> {
        let qvec = match qvec {
            Ok(v) => v,
            Err(e) => {
                self.asking = false;
                self.status = format!("Ask failed: {e}");
                return Task::none();
            }
        };
        let Some(lcfg) = llm::Config::load() else {
            self.asking = false;
            return Task::none();
        };

        // Build the context node set: the freshly retrieved top-K, plus
        // the function under the cursor and the previous turn's sources —
        // so a follow-up ("why does it…") still has that code in view.
        // Dedup, keep the highest-scoring, cap the total.
        const MAX_CTX: usize = 18;
        let mut sources: Vec<(explain::Node, f32)> = embed::search(&self.embed_index, &qvec, 16)
            .into_iter()
            .map(|(n, s)| (n.clone(), s))
            .collect();
        let mut carried: Vec<explain::Node> = Vec::new();
        if let Some(t) = self.cursor_target() {
            carried.push(t);
        }
        if let Some(prev) = self.ask_turns.last() {
            carried.extend(prev.sources.iter().map(|(n, _)| n.clone()));
        }
        for n in carried {
            if !sources.iter().any(|(c, _)| *c == n) {
                let s = self.node_score(&n, &qvec);
                sources.push((n, s));
            }
        }
        // Broaden recall for cross-cutting questions: pull in the
        // import-graph neighbours of the top few non-hub files, so a
        // subsystem that feeds or uses the retrieved code (e.g. the file
        // watcher behind the indexer) can enter the context. Neighbours
        // still compete on relevance via `node_score`, with a small
        // connectivity nudge, and are capped so they can't crowd out
        // direct hits. Hub files (huge fan) are skipped — expanding them
        // would flood the context with loosely-related neighbours.
        {
            let node_file = |n: &explain::Node| match n {
                explain::Node::Function { file, .. } => file.clone(),
                explain::Node::File(p) | explain::Node::Folder(p) => p.clone(),
            };
            let mut have: HashSet<PathBuf> = sources.iter().map(|(n, _)| node_file(n)).collect();
            let seeds: Vec<PathBuf> = sources
                .iter()
                .take(4)
                .map(|(n, _)| node_file(n))
                .filter(|f| self.import_graph.fan_in(f) + self.import_graph.fan_out(f) <= 20)
                .collect();
            let mut added = 0usize;
            for f in seeds {
                if added >= 4 {
                    break;
                }
                let mut neigh: Vec<PathBuf> = self
                    .import_graph
                    .imports(&f)
                    .iter()
                    .filter_map(|e| match &e.target {
                        imports::Target::Internal(t) => Some(t.clone()),
                        _ => None,
                    })
                    .collect();
                neigh.extend(self.import_graph.importers(&f));
                neigh.sort();
                neigh.dedup();
                for nf in neigh {
                    if added >= 4 {
                        break;
                    }
                    if have.contains(&nf) {
                        continue;
                    }
                    let node = explain::Node::File(nf.clone());
                    if !self.explanations.contains_key(&node) {
                        continue;
                    }
                    let s = self.node_score(&node, &qvec) + 0.05;
                    sources.push((node, s));
                    have.insert(nf);
                    added += 1;
                }
            }
        }
        sources.sort_by(|a, b| b.1.total_cmp(&a.1));
        sources.truncate(MAX_CTX);

        // Assemble the context: the pinned selection first (if any), then
        // the ranked node context.
        let nodes: Vec<explain::Node> = sources.iter().map(|(n, _)| n.clone()).collect();
        let mut context = String::new();
        // If the debugger is paused, ground the answer in the live state.
        if let Some(state) = self.debug_context() {
            context.push_str(&state);
        }
        for pin in &self.ask_pins {
            context.push_str(&format!(
                "### Selected code — {} (L{})\n```\n{}\n```\n\n",
                pin.rel, pin.line, pin.code
            ));
        }
        context.push_str(&self.gather_ask_context(&nodes));

        // Replay recent turns as chat history so follow-ups resolve.
        const HIST_TURNS: usize = 6;
        let mut messages: Vec<llm::ChatMsg> = Vec::new();
        let start = self.ask_turns.len().saturating_sub(HIST_TURNS);
        for turn in &self.ask_turns[start..] {
            messages.push(llm::ChatMsg::user(turn.question.clone()));
            messages.push(llm::ChatMsg::assistant(turn.answer_md.clone()));
        }
        messages.push(llm::ChatMsg::user(format!(
            "Question: {question}\n\nCode context:\n{context}"
        )));

        self.start_ask_stream(question, sources, lcfg, ASK_SYSTEM.to_string(), messages)
    }

    fn on_ask_stream_ended(&mut self, error: Option<String>) -> Task<Message> {
        self.asking = false;
        if let Some(e) = &error {
            self.status = format!("Ask failed: {e}");
        }
        // Finalize the open turn: on error with no text, show why; then
        // render the accumulated markdown as rich segments.
        let md = match self.ask_turns.last_mut() {
            Some(turn) => {
                turn.streaming = false;
                if let Some(e) = &error
                    && turn.answer_md.trim().is_empty()
                {
                    turn.answer_md = format!("*Couldn't answer: {e}*");
                }
                turn.answer_md.clone()
            }
            None => return Task::none(),
        };
        let (prepared, task) = self.prepare_segments(&md);
        if let Some(turn) = self.ask_turns.last_mut() {
            turn.answer = prepared;
        }
        let to_bottom = operation::scroll_to(
            ui::ask_scroll_id(),
            AbsoluteOffset { x: 0.0, y: f32::MAX },
        );
        Task::batch([task, to_bottom])
    }

    fn on_ask_about_selection(&mut self) -> Task<Message> {
        // Add the right-clicked pane's selection (or the active pane's) as a
        // context chip, open the panel, and focus the input.
        let pane = self.context_menu.take().map(|m| m.pane).unwrap_or(self.active);
        match self.selection_pin(pane) {
            Some(pin) => {
                // Skip an exact duplicate (same file, line and code).
                let dup = self.ask_pins.iter().any(|p| {
                    p.file == pin.file && p.line == pin.line && p.code == pin.code
                });
                if !dup {
                    self.ask_pins.push(pin);
                }
                self.show_bottom = true;
                self.bottom_tab = BottomTab::Ask;
                self.code_focused = false; // the Ask input takes focus
                self.status = "Added selection to Ask — ask your question".into();
                operation::focus(ui::ask_input_id())
            }
            None => {
                self.status = "Select some code first, then Add to Ask".into();
                Task::none()
            }
        }
    }

    fn on_why_is_this_here(&mut self) -> Task<Message> {
        let menu = self.context_menu.take();
        let pane = menu.map(|m| m.pane).unwrap_or(self.active);
        let menu_line = menu.map(|m| m.line);
        let Some(cfg) = llm::Config::load() else {
            self.status = format!("Add an API key in Settings ({})", llm::config_hint());
            return Task::done(Message::OpenSettings);
        };
        let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
            return Task::none();
        };
        let Some(v) = self.panes.get(pane).and_then(Option::as_ref) else {
            return Task::none();
        };
        let Some(git) = v.git.clone() else {
            self.status = "No git history for this file".into();
            return Task::none();
        };
        // Target line range (0-based inclusive): the selection, else the
        // clicked/caret line.
        let (l0, l1) = match v.selection_ordered() {
            Some(((a, _), (b, _))) => (a, b),
            None => match menu_line.or(v.caret.map(|(l, _)| l)) {
                Some(l) => (l, l),
                None => return Task::none(),
            },
        };
        // Distinct committed commits touching the range (a few at most).
        let mut seen = HashSet::new();
        let mut commits: Vec<(String, String)> = Vec::new();
        for line in l0..=l1 {
            if let Some(b) = git.blame_for(line)
                && !b.uncommitted
                && !b.commit.is_empty()
                && seen.insert(b.commit.clone())
            {
                commits.push((b.commit.clone(), b.summary.clone()));
                if commits.len() >= 4 {
                    break;
                }
            }
        }
        if commits.is_empty() {
            self.status = "This code isn't committed yet — no history to explain".into();
            return Task::none();
        }
        let last = l1.min(l0 + 40); // cap the snippet
        let code: String =
            (l0..=last).filter_map(|l| v.source_line(l)).collect::<Vec<_>>().join("\n");
        let rel = v.rel.clone();
        let title = if l0 == l1 {
            format!("Why line {} exists", l0 + 1)
        } else {
            format!("Why lines {}–{} exist", l0 + 1, l1 + 1)
        };
        self.blame_why = Some(BlameWhy {
            title: title.clone(),
            commits: commits.clone(),
            loading: true,
            prepared: Vec::new(),
        });
        self.status = "Explaining why…".into();
        let commits_ctx = commits.clone();
        let ai = self.ai_client();
        Task::perform(
            async move {
                // Build the prompt off-thread (git diffs), then complete.
                let prompt = tokio::task::spawn_blocking(move || {
                    let mut ctx = format!(
                        "Code ({rel}, lines {}-{}):\n```\n{code}\n```\n\n",
                        l0 + 1,
                        last + 1
                    );
                    for (sha, _) in &commits_ctx {
                        let msg = git::commit_message(&root, sha).unwrap_or_default();
                        let diff = git::commit_file_diff(&root, sha, &rel, 3000);
                        ctx.push_str(&format!(
                            "### Commit {sha}\nMessage:\n{msg}\n\nWhat it changed here:\n```\n{diff}\n```\n\n"
                        ));
                    }
                    format!("Why does this code exist?\n\n{ctx}")
                })
                .await
                .unwrap_or_default();
                ai.complete(cfg, WHY_SYSTEM, prompt, 512).await
            },
            move |result| Message::BlameWhyDone { title, commits, result },
        )
    }

    fn on_time_travel_start(&mut self, symbol: bool) -> Task<Message> {
        self.show_tools_menu = false;
        let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
            self.status = "Time travel needs a git repository".into();
            return Task::none();
        };
        let Some(v) = self.active_viewer() else {
            return Task::none();
        };
        let (abs, rel, lang) = (v.abs.clone(), v.rel.clone(), v.lang_key);
        // Scope: the innermost code block (any kind — function, struct,
        // enum, class, trait, …) whose span contains the caret, else the
        // whole file. When re-scoping mid-session the caret comes from the
        // historical view; either way the block's NAME is resolved to its
        // HEAD line range, since `git log -L` interprets ranges vs HEAD.
        let scope = if symbol {
            let name = {
                let (line1, syms): (usize, &[outline::Symbol]) = match self
                    .time_travel
                    .as_ref()
                    .and_then(|t| t.viewer.as_ref().map(|hv| (t.caret, hv)))
                {
                    Some((c, hv)) => (c.map(|(l, _)| l + 1).unwrap_or(1), &hv.symbols),
                    None => (v.caret.map(|(l, _)| l + 1).unwrap_or(1), &v.symbols),
                };
                syms.iter()
                    .filter(|s| s.line <= line1 && line1 <= s.end_line && s.end_line >= s.line)
                    .min_by_key(|s| s.end_line.saturating_sub(s.line))
                    .map(|s| s.name.clone())
            };
            name.and_then(|n| {
                v.symbols.iter().find(|s| s.name == n).map(|s| TimeScope::Symbol {
                    name: s.name.clone(),
                    kind: s.kind.clone(),
                    start: s.line,
                    end: s.end_line,
                })
            })
            .unwrap_or(TimeScope::File)
        } else {
            TimeScope::File
        };
        self.time_gen += 1;
        let generation = self.time_gen;
        self.status = "Loading history…".into();
        let (scope_task, rel_task) = (scope.clone(), rel.clone());
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || match &scope_task {
                    TimeScope::File => git::file_history(&root, &rel_task, 200),
                    TimeScope::Symbol { start, end, .. } => {
                        git::symbol_history(&root, &rel_task, *start, *end, 200)
                    }
                })
                .await
                .unwrap_or_default()
            },
            move |commits| Message::TimeTravelReady {
                generation,
                abs: abs.clone(),
                rel: rel.clone(),
                lang,
                scope: scope.clone(),
                commits,
            },
        )
    }

    fn on_time_travel_ready(&mut self, generation: u64, abs: PathBuf, rel: String, lang: Option<&'static str>, scope: TimeScope, commits: Vec<git::HistCommit>) -> Task<Message> {
        if generation != self.time_gen {
            return Task::none();
        }
        if commits.is_empty() {
            self.status = match &scope {
                TimeScope::Symbol { name, .. } => format!("No git history for `{name}`"),
                TimeScope::File => "No git history for this file".into(),
            };
            return Task::none();
        }
        self.status.clear();
        // Start where the reader was: keep the existing session's scroll
        // and caret when re-scoping (so a scope toggle doesn't snap back),
        // else take them from the live file on first entry.
        let (scroll_y, caret) = self
            .time_travel
            .as_ref()
            .map(|t| (t.scroll_y, t.caret))
            .or_else(|| self.active_viewer().map(|v| (v.scroll_y, v.caret)))
            .unwrap_or((0.0, None));
        self.time_travel = Some(TimeTravel {
            abs,
            rel,
            lang,
            scope,
            commits,
            idx: 0,
            viewer: None,
            scroll_y,
            caret,
            focus_line: None,
            loading: true,
            generation,
            why: HashMap::new(),
            why_loading: false,
            story: None,
            story_loading: false,
        });
        Task::done(Message::TimeTravelGoto(0))
    }

    fn on_time_travel_goto(&mut self, idx: usize) -> Task<Message> {
        let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
            return Task::none();
        };
        let (commit, lang, focus_name) = {
            let Some(tt) = self.time_travel.as_ref() else {
                return Task::none();
            };
            let Some(commit) = tt.commits.get(idx) else {
                return Task::none();
            };
            (commit.clone(), tt.lang, tt.scope.symbol_name().map(str::to_string))
        };
        self.time_gen += 1;
        let generation = self.time_gen;
        if let Some(tt) = self.time_travel.as_mut() {
            tt.idx = idx;
            tt.loading = true;
            tt.generation = generation;
        }
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    let content =
                        git::file_at(&root, &commit.sha, &commit.path).unwrap_or_default();
                    let lines = highlight::highlight_lines(&content, lang);
                    let symbols =
                        lang.map(|l| outline::extract(&content, l)).unwrap_or_default();
                    let added = git::commit_added_lines(&root, &commit.sha, &commit.path);
                    let focus_line = focus_name
                        .and_then(|n| symbols.iter().find(|s| s.name == n).map(|s| s.line));
                    Box::new(TimeStep { lines, content, symbols, added, focus_line })
                })
                .await
                .ok()
            },
            move |step| match step {
                Some(step) => Message::TimeTravelStep { generation, idx, step },
                None => Message::TimeTravelExit,
            },
        )
    }

    fn on_time_travel_step(&mut self, generation: u64, idx: usize, step: Box<TimeStep>) -> Task<Message> {
        if generation != self.time_gen {
            return Task::none();
        }
        let line_height = self.line_height();
        let Some(tt) = self.time_travel.as_mut() else {
            return Task::none();
        };
        tt.loading = false;
        tt.idx = idx;
        tt.focus_line = step.focus_line;
        let n = step.lines.len();
        let status: Vec<Option<git::ChangeKind>> = (0..n)
            .map(|i| step.added.contains(&(i + 1)).then_some(git::ChangeKind::Added))
            .collect();
        let source = std::sync::Arc::new(step.content);
        let mut v =
            viewer::Viewer::new(tt.abs.clone(), tt.rel.clone(), tt.lang, source, step.lines);
        v.symbols = step.symbols;
        v.highlighted = true;
        v.git = Some(std::sync::Arc::new(git::GitInfo {
            blame: Vec::new(),
            status,
            deleted_at: HashSet::new(),
        }));
        let last_line = v.lines.len().saturating_sub(1);
        // Block scope: bring the block into view. File scope: keep the
        // reader's caret and scroll position (carried from entry).
        if let Some(fl) = step.focus_line {
            let head = (fl.saturating_sub(1), 0);
            v.caret = Some(head);
            tt.caret = Some(head);
            let y = v.scroll_offset_for(Some(fl), line_height);
            v.scroll_y = y;
            tt.scroll_y = y;
        } else {
            // Clamp the carried caret to this revision's bounds (older
            // revisions are shorter, and lines may be shorter too).
            v.caret = tt.caret.map(|(l, c)| {
                let l = l.min(last_line);
                let cols = v
                    .lines
                    .get(l)
                    .map(|ln| ln.spans.iter().map(|(t, _)| t.chars().count()).sum::<usize>())
                    .unwrap_or(0);
                (l, c.min(cols))
            });
            v.scroll_y = tt.scroll_y;
        }
        tt.viewer = Some(v);
        // Explicitly scroll the (freshly mounted) historical scrollable to
        // the carried offset — iced doesn't preserve scroll across the swap.
        let y = self.time_travel.as_ref().map(|t| t.scroll_y).unwrap_or(0.0);
        operation::scroll_to(ui::code_scroll_id(self.active), AbsoluteOffset { x: 0.0, y })
    }

    fn on_time_travel_select_start(&mut self, line: usize, col: usize) -> Task<Message> {
        let extend = self.modifiers.shift();
        let mut started = false;
        if let Some(tt) = self.time_travel.as_mut() {
            let head = (line, col);
            tt.caret = Some(head); // persist across scrubs
            if let Some(v) = tt.viewer.as_mut() {
                match (extend, v.selection) {
                    (true, Some((anchor, _))) => v.selection = Some((anchor, head)),
                    _ => v.selection = Some((head, head)),
                }
                v.caret = Some(head);
                started = true;
            }
        }
        if started {
            self.selecting = true;
        }
        Task::none()
    }

    fn on_time_travel_why(&mut self) -> Task<Message> {
        let (root, sha, path, subject) = {
            let Some(tt) = self.time_travel.as_ref() else {
                return Task::none();
            };
            let Some(c) = tt.commits.get(tt.idx) else {
                return Task::none();
            };
            if tt.why.contains_key(&c.sha) {
                return Task::none(); // already have it
            }
            let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
                return Task::none();
            };
            (root, c.sha.clone(), c.path.clone(), c.subject.clone())
        };
        let Some(cfg) = llm::Config::load() else {
            self.status = format!("Add an API key in Settings ({})", llm::config_hint());
            return Task::done(Message::OpenSettings);
        };
        if let Some(tt) = self.time_travel.as_mut() {
            tt.why_loading = true;
        }
        let generation = self.time_gen;
        let sha2 = sha.clone();
        let ai = self.ai_client();
        Task::perform(
            async move {
                let prompt = tokio::task::spawn_blocking(move || {
                    let msg = git::commit_message(&root, &sha2).unwrap_or(subject);
                    let diff = git::commit_file_diff(&root, &sha2, &path, 8000);
                    format!("Commit message:\n{msg}\n\nDiff of {path}:\n{diff}")
                })
                .await
                .unwrap_or_default();
                ai.complete(cfg, TIME_WHY_SYSTEM, prompt, 220).await
            },
            move |result| Message::TimeTravelWhyDone { generation, sha, result },
        )
    }

    fn on_time_travel_story(&mut self) -> Task<Message> {
        // Toggle: if a story is already showing, hide it.
        if self.time_travel.as_ref().is_some_and(|t| t.story.is_some()) {
            if let Some(tt) = self.time_travel.as_mut() {
                tt.story = None;
            }
            return Task::none();
        }
        let (root, name, commits) = {
            let Some(tt) = self.time_travel.as_ref() else {
                return Task::none();
            };
            let TimeScope::Symbol { name, kind, .. } = &tt.scope else {
                return Task::none();
            };
            let name = format!("{kind} {name}");
            let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
                return Task::none();
            };
            let commits: Vec<(String, String, String)> = tt
                .commits
                .iter()
                .take(12)
                .map(|c| (c.sha.clone(), c.subject.clone(), c.path.clone()))
                .collect();
            (root, name, commits)
        };
        let Some(cfg) = llm::Config::load() else {
            self.status = format!("Add an API key in Settings ({})", llm::config_hint());
            return Task::done(Message::OpenSettings);
        };
        if let Some(tt) = self.time_travel.as_mut() {
            tt.story_loading = true;
        }
        let generation = self.time_gen;
        let ai = self.ai_client();
        Task::perform(
            async move {
                let prompt = tokio::task::spawn_blocking(move || {
                    let mut ctx = String::new();
                    for (sha, subject, path) in &commits {
                        let short = &sha[..sha.len().min(8)];
                        let diff = git::commit_file_diff(&root, sha, path, 2500);
                        ctx.push_str(&format!(
                            "### {short} — {subject}\n```diff\n{diff}\n```\n\n"
                        ));
                    }
                    format!("Code block: {name}\n\nCommits (newest first):\n{ctx}")
                })
                .await
                .unwrap_or_default();
                ai.complete(cfg, TIME_STORY_SYSTEM, prompt, 900).await
            },
            move |result| Message::TimeTravelStoryDone { generation, result },
        )
    }

    fn on_hover_dwell(&mut self, epoch: u64, pane: usize, line: usize, col: usize, x: f32, y: f32) -> Task<Message> {
        if epoch != self.hover_gen || self.hover_pinned {
            return Task::none(); // cursor moved on, or is inside the tooltip
        }
        self.hover = Some(HoverState {
            line,
            col,
            x,
            y,
            text: None,
            // The Explain one-liner is cached, so attach it synchronously;
            // any LSP text arrives later and renders below it.
            summary: self.hover_summary(pane, line, col),
            // The diagnostic under the cursor (if the symbol is
            // underlined), so the hover explains the error.
            diagnostic: self.diagnostic_at(pane, line, col),
        });
        // Debug: while paused, hovering an identifier shows its live
        // value (evaluated in the current frame) instead of LSP info.
        if let Some(session) = self.debug.as_ref().filter(|s| s.status == DebugStatus::Stopped)
            && let (Some(client), Some(frame)) =
                (session.client.clone(), session.frames.first())
        {
            let frame_id = frame.id;
            if let Some(word) = self
                .panes
                .get(pane)
                .and_then(Option::as_ref)
                .and_then(|v| analyze::word_at(&v.lines, line, col))
            {
                let w = word.clone();
                return Task::perform(
                    async move { client.evaluate(&word, frame_id).await },
                    move |res| Message::HoverResult {
                        line,
                        col,
                        text: res.ok().filter(|v| !v.is_empty()).map(|v| format!("{w} = {v}")),
                    },
                );
            }
        }
        // Local peek (tree-sitter only): the same-file symbol's doc
        // comment and/or the Rust type's structure. Instant, no LSP
        // round-trip, and works with no server configured at all.
        if let Some(text) = self.local_peek(pane, line, col) {
            if let Some(h) = &mut self.hover {
                h.text = Some(text);
            }
            return Task::none();
        }
        // Pull the request context before mutating self further.
        let Some((lang, path, source_line)) =
            self.panes.get(pane).and_then(Option::as_ref).and_then(|v| {
                v.lang_key.map(|l| {
                    (l, v.abs.clone(), v.source_line(line).unwrap_or("").to_string())
                })
            })
        else {
            return Task::none();
        };
        let client = match self.lsp.get(lang) {
            Some(LspSlot::Ready(c)) => c.clone(),
            _ => return Task::none(),
        };
        let utf16 = client.encoding == lsp::client::PositionEncoding::Utf16;
        let character = viewer::character_offset(&source_line, col, utf16);
        Task::perform(
            async move { client.hover(&path, line, character).await },
            move |result| Message::HoverResult {
                line,
                col,
                text: result.ok().flatten(),
            },
        )
    }

    fn on_hover_requested(&mut self, pane: usize, line: usize, col: usize, x: f32, y: f32) -> Task<Message> {
        // The cursor is inside the tooltip — leave it be so it can be read
        // and scrolled.
        if self.hover_pinned {
            return Task::none();
        }
        // The code view reports the cursor in the scrollable's *content*
        // space (offset by the scroll); the tooltip overlay lives in
        // window space, so remove the pane's scroll to anchor it at the
        // cursor rather than that far below it.
        let y = y - self.panes.get(pane).and_then(Option::as_ref).map_or(0.0, |v| v.scroll_y);
        // Same token already shown: just reposition.
        if let Some(h) = &mut self.hover
            && h.line == line
            && h.col == col
        {
            h.x = x;
            h.y = y;
            return Task::none();
        }
        // New token: start a dwell so moving across code doesn't flash
        // tooltips — it shows only if the cursor rests here for a moment.
        // The current peek stays visible until the new one is ready, so the
        // cursor can travel down into it without it vanishing first.
        self.hover_gen = self.hover_gen.wrapping_add(1);
        let epoch = self.hover_gen;
        Task::perform(
            async move {
                tokio::time::sleep(std::time::Duration::from_millis(300)).await
            },
            move |_| Message::HoverDwell { epoch, pane, line, col, x, y },
        )
    }

    fn on_generate_overview(&mut self) -> Task<Message> {
        let Some(cfg) = llm::Config::load() else {
            self.status = format!("Add an API key in Settings ({})", llm::config_hint());
            return Task::none();
        };
        if self.explanations.is_empty() {
            self.status =
                "Run Explain All first — the overview is built from the explanations".into();
            return Task::none();
        }
        let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
            return Task::none();
        };
        let inputs = self.gather_overview_inputs();
        let prompt = overview::prompt(&inputs);
        let prompt_hash = incremental::content_hash(prompt.as_bytes());
        self.generating_overview = true;
        // Don't force the overview into view: a chained/background
        // regeneration must not interrupt someone reading code. The
        // manual entry points are already on the overview page.
        self.status = "Generating architecture overview…".into();
        let ai = self.ai_client();
        Task::perform(
            // Raw LLM prose only; the module map is folded in fresh at
            // prepare time so it always reflects the live imports.
            async move { ai.complete(cfg, overview::SYSTEM, prompt, 2048).await },
            move |result| Message::OverviewDone { root: root.clone(), prompt_hash, result },
        )
    }

    fn on_overview_done(&mut self, root: PathBuf, prompt_hash: incremental::Version, result: Result<String, String>) -> Task<Message> {
        if self.project.as_ref().map(|p| &p.root) != Some(&root) {
            return Task::none();
        }
        self.generating_overview = false;
        match result {
            Ok(markdown) => {
                // Persist the raw prose; fold the live module map in only
                // for display so the cache never carries a stale diagram.
                let _ = overview::save(
                    &root,
                    &overview::Cached { markdown: markdown.clone(), prompt_hash },
                );
                let display = self.overview_display(&markdown);
                let (prepared, task) = self.prepare_segments(&display);
                self.overview_prepared = prepared;
                self.overview = Some(markdown);
                self.overview_map = self.compute_overview_map();
                self.overview_prompt_hash = Some(prompt_hash);
                self.status = "Architecture overview ready".into();
                task
            }
            Err(e) => {
                self.status = format!("Overview failed: {e}");
                Task::none()
            }
        }
    }

    fn on_symbol_index_done(&mut self, root: PathBuf, indexed: index::Indexed) -> Task<Message> {
        // Ignore a late result from a project the user already switched
        // away from (it would seed the new project's registry with the
        // old project's files).
        if self.project.as_ref().map(|p| &p.root) != Some(&root) {
            return Task::none();
        }
        self.indexing = false;
        // Seed the change-detection registry from the same tree read.
        self.registry.seed(indexed.hashes);
        self.symbol_index_by_file = indexed.by_file;
        let changed_while_closed = indexed.changed.len();
        self.rebuild_symbol_index();
        // Build the import graph from the same single tree read.
        self.rebuild_import_graph(indexed.imports_by_file);
        if changed_while_closed > 0 {
            self.status = format!(
                "{changed_while_closed} file{} changed since last session",
                if changed_while_closed == 1 { "" } else { "s" }
            );
        } else if let Some(p) = &self.project {
            self.status = format!(
                "{} files · {} symbols",
                p.files.len(),
                self.symbol_index.len()
            );
        }
        // The import graph is now resolved, so refresh the overview's
        // module map if it was prepared before the imports were ready.
        let map_task = self.refresh_overview_map();
        // Build the Rust type-structure index off-thread (for the hover
        // "implements / implementors" peek).
        let structure_task = match self.project.as_ref().map(|p| p.files.clone()) {
            Some(files) => Task::perform(
                async move {
                    tokio::task::spawn_blocking(move || structure::build(&files))
                        .await
                        .unwrap_or_default()
                },
                Message::StructureBuilt,
            ),
            None => Task::none(),
        };
        Task::batch([map_task, structure_task])
    }

    fn on_inlay_hints_loaded(&mut self, abs: PathBuf, hints: Vec<lsp::client::InlayHint>) -> Task<Message> {
        // Encoding for mapping the server's character offsets to display
        // columns (tabs already expanded to 4).
        let utf16 = self
            .panes
            .iter()
            .flatten()
            .find(|v| v.abs == abs)
            .and_then(|v| v.lang_key)
            .and_then(|l| match self.lsp.get(l) {
                Some(LspSlot::Ready(c)) => {
                    Some(c.encoding == lsp::client::PositionEncoding::Utf16)
                }
                _ => None,
            })
            .unwrap_or(true);
        for slot in &mut self.panes {
            let Some(v) = slot.as_mut().filter(|v| v.abs == abs) else {
                continue;
            };
            let source = v.source.clone();
            let src_lines: Vec<&str> = source.lines().collect();
            let mut map: HashMap<usize, Vec<(usize, String)>> = HashMap::new();
            for h in &hints {
                let Some(line) = src_lines.get(h.line) else {
                    continue;
                };
                let col = viewer::display_col_from_char(line, h.character, utf16);
                let mut text = h.label.clone();
                if h.padding_left {
                    text.insert(0, ' ');
                }
                if h.padding_right {
                    text.push(' ');
                }
                map.entry(h.line).or_default().push((col, text));
            }
            for chips in map.values_mut() {
                chips.sort_by_key(|(c, _)| *c);
            }
            v.inlay_hints = map;
        }
        Task::none()
    }

    fn on_lsp_consent_allowed(&mut self) -> Task<Message> {
        let Some(c) = self.pending_lsp_consent.take() else {
            return Task::none();
        };
        self.lsp.insert(c.language.clone(), LspSlot::Starting);
        let (dest, language, version) = (c.dest_dir, c.language, c.version);
        match c.provision {
            LspProvision::Download(download) => {
                self.status = format!("Downloading {}…", c.server_name);
                Task::perform(
                    async move {
                        tokio::task::spawn_blocking(move || {
                            lsp::store::download_and_install(&download, &dest)
                        })
                        .await
                        .unwrap_or_else(|e| Err(e.to_string()))
                    },
                    move |result| Message::LspDownloadResult {
                        language: language.clone(),
                        result,
                    },
                )
            }
            LspProvision::Install(install) => {
                self.status = format!("Installing {}…", c.server_name);
                Task::perform(
                    async move {
                        tokio::task::spawn_blocking(move || {
                            lsp::store::toolchain_install(&install, &version, &dest)
                        })
                        .await
                        .unwrap_or_else(|e| Err(e.to_string()))
                    },
                    move |result| Message::LspDownloadResult {
                        language: language.clone(),
                        result,
                    },
                )
            }
        }
    }

    fn on_call_hierarchy_children(&mut self, id: usize, items: Vec<lsp::client::CallItem>) -> Task<Message> {
        // Keep only project-internal callers/callees — don't descend into
        // external libraries / std.
        let root = self.project.as_ref().map(|p| p.root.clone());
        let items: Vec<_> = match &root {
            Some(r) => items.into_iter().filter(|i| i.path.starts_with(r)).collect(),
            None => items,
        };
        let new_ids = match &mut self.call_graph {
            Some(t) => t.set_children(id, items),
            None => return Task::none(),
        };
        // In "expand all" mode, recurse into the new project-internal
        // children until the frontier is empty or the node cap is hit.
        let recurse = self
            .call_graph
            .as_ref()
            .is_some_and(|t| t.full && t.node_count() < callgraph::MAX_NODES);
        if recurse {
            let to_fetch: Vec<usize> = new_ids
                .into_iter()
                .filter(|&cid| {
                    self.call_graph.as_ref().is_some_and(|t| t.needs_fetch(cid))
                })
                .collect();
            Task::batch(
                to_fetch
                    .into_iter()
                    .map(|cid| self.fetch_children(cid))
                    .collect::<Vec<_>>(),
            )
        } else {
            Task::none()
        }
    }

    fn on_dap_stop_inspected(&mut self, frames: Vec<dap::StackFrame>, scopes: Vec<DebugScope>) -> Task<Message> {
        // Jump to the innermost frame that has source, and highlight it.
        let (target, fname) = {
            let Some(session) = self.debug.as_mut() else {
                return Task::none();
            };
            session.frames = frames;
            session.scopes = scopes;
            let t = session.frames.iter().find_map(|f| f.path.clone().map(|p| (p, f.line)));
            if let Some((path, line)) = &t {
                session.current = Some((path.clone(), *line));
            }
            let fname = session.frames.first().map(|f| short_frame_name(&f.name));
            (t, fname)
        };
        // Fuse into the reading trail: when execution enters a NEW
        // function, record one entry (labelled with the function name) so
        // the debug run becomes a navigable path in the TRAIL tab.
        if let (Some(fname), Some((path, line))) = (&fname, &target)
            && self.debug_last_fn.as_ref() != Some(fname)
        {
            self.debug_last_fn = Some(fname.clone());
            self.history.push(
                Loc { path: path.clone(), line: Some(*line) },
                Some(fname.clone()),
            );
            self.save_history();
        }
        self.show_bottom = true;
        self.bottom_tab = BottomTab::Debug;
        match target {
            Some((path, line)) => Task::batch([
                self.open_file(path, Some(line), false),
                self.eval_watches(),
            ]),
            None => self.eval_watches(),
        }
    }

    fn on_conditional_breakpoint_from_menu(&mut self) -> Task<Message> {
        let Some(menu) = self.context_menu.take() else {
            return Task::none();
        };
        let Some(abs) =
            self.panes.get(menu.pane).and_then(Option::as_ref).map(|v| v.abs.clone())
        else {
            return Task::none();
        };
        let line = menu.line + 1;
        // Pre-fill with any existing condition on this line.
        let existing = self
            .breakpoints
            .get(&abs)
            .and_then(|m| m.get(&line))
            .and_then(|bp| bp.condition.clone())
            .unwrap_or_default();
        self.bp_cond_edit = Some((abs, line, existing));
        operation::focus(ui::bp_condition_input_id())
    }

    fn on_server_connected(&mut self, tx: tokio::sync::mpsc::UnboundedSender<clew_protocol::ClientMessage>) -> Task<Message> {
        // The in-process clew-server is up; keep its request channel and
        // greet it. Backend flows migrate onto this seam one at a time.
        let hello = clew_protocol::ClientMessage {
            id: 0,
            request: clew_protocol::Request::Hello {
                protocol: clew_protocol::PROTOCOL_VERSION,
                ai: clew_protocol::AiEndpoint::Server,
            },
        };
        let _ = tx.send(hello);
        self.server_tx = Some(tx);
        // Resume a scan that was waiting for the server (its Tree reply
        // opens the project); otherwise, if a project is already open
        // (local-fallback path), tell the server about it for search.
        if let Some(root) = self.pending_scan_root.clone() {
            self.request_open_project(root);
        } else {
            self.sync_project_to_server();
        }
        // Give the server the AI config so server-endpoint calls work.
        self.send_ai_config();
        // If the Connect modal was waiting on this transport, move it into
        // the remote folder picker and list the home directory.
        if let Some(ui) = &self.connect
            && matches!(ui.stage, ConnectStage::Connecting { .. })
        {
            self.enter_remote_browser(None);
        }
        Task::none()
    }

    fn on_server_unavailable(&mut self) -> Task<Message> {
        // A remote bootstrap failure surfaces in the Connect modal rather
        // than falling back to a (meaningless) local scan of a remote path.
        if let Some(ui) = &mut self.connect
            && matches!(ui.stage, ConnectStage::Connecting { .. })
        {
            ui.stage = ConnectStage::Error(
                "Could not reach the host. Check the address, port, and key.".into(),
            );
            self.pending_scan_root = None;
            return Task::none();
        }
        // The server binary didn't spawn. Fall back to a local scan for
        // any project that was deferred waiting on it.
        if let Some(root) = self.pending_scan_root.take() {
            return self.local_scan(root);
        }
        Task::none()
    }

    fn on_connect_submit(&mut self) -> Task<Message> {
        let Some(ui) = &self.connect else {
            return Task::none();
        };
        let host = ui.host.trim().to_string();
        let user = ui.user.trim().to_string();
        if host.is_empty() || user.is_empty() {
            if let Some(ui) = &mut self.connect {
                ui.stage =
                    ConnectStage::Error("Host and user are required.".into());
            }
            return Task::none();
        }
        let conn = connect::SavedConnection {
            name: ui.name.trim().to_string(),
            host,
            user,
            port: ui.port.parse().unwrap_or(22),
            identity: ui.identity.trim().to_string(),
        };
        self.remember_connection(conn.clone());
        self.connect_to(conn.target());
        Task::none()
    }

    fn on_target_selected(&mut self, target: inactive::Target) -> Task<Message> {
        self.reading_target = target;
        self.show_tools_menu = false;
        self.show_target_menu = false;
        // Re-evaluate the cfg dimming for every open file.
        let t = self.reading_target.clone();
        for v in self.panes.iter_mut().flatten() {
            if let Some(lang) = v.lang_key {
                let src = v.source.clone();
                v.inactive_lines = inactive::inactive_lines(&src, lang, &t);
            }
        }
        if let Some(root) = self.project.as_ref().map(|p| p.root.clone()) {
            if let Err(e) = reading::save_target(&root, &self.reading_target) {
                self.status = format!("Could not save target: {e}");
            }
        }
        Task::none()
    }

    fn on_project_calls_built(&mut self, root: PathBuf, graph: projectcalls::ProjectCallGraph) -> Task<Message> {
        // Drop a late result from a previous project.
        if self.project.as_ref().map(|p| &p.root) != Some(&root) {
            return Task::none();
        }
        self.building_calls = false;
        self.project_calls = graph;
        // This is the name-based approximation; a superseding refine is
        // no longer valid, and no precise result is in effect.
        self.project_calls_precise = false;
        self.refine_progress = None;
        self.calls_gen += 1;
        self.precise_edges = projectcalls::SymEdges::default();
        self.precise_pending.clear();
        // The map depends on the freshly built graph.
        if self.overlay == Some(Overlay::ProjectCalls) {
            self.refresh_graph_layout();
        }
        // If files changed while this build was running, its data is
        // already stale — rebuild once so the open overlay self-heals.
        if self.overlay == Some(Overlay::ProjectCalls)
            && self.project_calls_rev != self.registry.revision()
        {
            return self.build_project_calls();
        }
        Task::none()
    }

    fn on_project_calls_refined(&mut self, root: PathBuf, generation: u64, edges: projectcalls::SymEdges, graph: projectcalls::ProjectCallGraph) -> Task<Message> {
        // Accept only the latest refine for the current project.
        if generation != self.calls_gen
            || self.project.as_ref().map(|p| &p.root) != Some(&root)
        {
            return Task::none();
        }
        self.project_calls = graph;
        self.precise_edges = edges;
        self.project_calls_precise = true;
        self.refine_progress = None;
        self.status = "Call graph refined with LSP".into();
        if self.overlay == Some(Overlay::ProjectCalls) {
            self.refresh_graph_layout();
        }
        // Files changed while this refine ran → fold them in now.
        if self.overlay == Some(Overlay::ProjectCalls) && !self.precise_pending.is_empty() {
            let changed = std::mem::take(&mut self.precise_pending);
            return self.refine_incremental(changed);
        }
        Task::none()
    }

    fn on_tick(&mut self) -> Task<Message> {
        // Snapshot each ready server's diagnostics + inlay-refresh epoch.
        let versions: Vec<(String, u64, u64)> = self
            .lsp
            .iter()
            .filter_map(|(lang, slot)| match slot {
                LspSlot::Ready(c) => Some((lang.clone(), c.diag_version(), c.inlay_epoch())),
                _ => None,
            })
            .collect();
        // Languages where the server just did work (re-analyzed, or asked
        // us to refresh inlay hints): (re)fetch hints for their shown
        // files. This is what makes hints appear after a cold-start
        // server finishes indexing and pushes inlayHint/refresh.
        let changed: Vec<String> = versions
            .iter()
            .filter(|(lang, diag, epoch)| {
                self.seen_diag_version.get(lang).copied() != Some(*diag)
                    || self.seen_inlay_epoch.get(lang).copied() != Some(*epoch)
            })
            .map(|(lang, _, _)| lang.clone())
            .collect();
        for (lang, diag, epoch) in &versions {
            self.seen_diag_version.insert(lang.clone(), *diag);
            self.seen_inlay_epoch.insert(lang.clone(), *epoch);
        }
        let mut inlay_tasks = Vec::new();
        for lang in &changed {
            let files: Vec<PathBuf> = self
                .panes
                .iter()
                .flatten()
                .filter(|v| v.lang_key == Some(lang.as_str()))
                .map(|v| v.abs.clone())
                .collect();
            for abs in files {
                inlay_tasks.push(self.inlay_request_lookup(&abs));
            }
        }
        // A change queued during the auto-refresh cooldown: fire it once
        // the window has lifted and nothing is running.
        let refresh = if self.refresh_pending
            && !self.explaining
            && !self.generating_overview
            && !self.building_embeddings
            && self
                .last_auto_refresh
                .map(|t| t.elapsed() >= AUTO_REFRESH_MIN_INTERVAL)
                .unwrap_or(true)
        {
            self.begin_refresh()
        } else {
            Task::none()
        };
        Task::batch([Task::batch(inlay_tasks), refresh])
    }

    fn on_sidebar_tab_picked(&mut self, tab: SidebarTab) -> Task<Message> {
        self.sidebar = tab;
        self.show_left_sidebar = true; // reveal it for external triggers
        self.show_tools_menu = false; // close the More menu if it opened this
        match tab {
            SidebarTab::Search => {
                // The search input takes keyboard focus.
                self.code_focused = false;
                operation::focus(ui::search_input_id())
            }
            SidebarTab::Imports => {
                // Sync the tree with the current file when the tab opens.
                self.refresh_import_tree();
                Task::none()
            }
            SidebarTab::Walk => {
                // Prepare the open tour's current step (markdown/mermaid)
                // if we haven't yet (e.g. a cached tour was just loaded).
                match self
                    .walkthrough_open
                    .and_then(|o| self.walkthroughs.get(o))
                    .and_then(|w| w.steps.get(self.walkthrough_step))
                {
                    Some(step) if self.walkthrough_prepared.is_empty() => {
                        let (prepared, task) = self.prepare_segments(&step.narration.clone());
                        self.walkthrough_prepared = prepared;
                        task
                    }
                    _ => Task::none(),
                }
            }
            SidebarTab::Docs => {
                // Build the API docs the first time the tab is opened.
                if self.docs.files.is_empty() && !self.docs.loading {
                    self.request_docs();
                }
                Task::none()
            }
            _ => Task::none(),
        }
    }

    fn on_toggle_diff(&mut self) -> Task<Message> {
        // Toggle off if already showing this file's diff.
        let active_abs = self.active_viewer().map(|v| v.abs.clone());
        if let (Some(d), Some(abs)) = (&self.diff, &active_abs)
            && d.abs == *abs
        {
            self.diff = None;
            return Task::none();
        }
        let Some(abs) = active_abs else {
            return Task::none();
        };
        let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
            return Task::none();
        };
        let rel = self.rel_of(&abs);
        Task::perform(
            async move {
                let file = abs.clone();
                let lines = tokio::task::spawn_blocking(move || {
                    git::diff_lines(&root, &file).unwrap_or_default()
                })
                .await
                .unwrap_or_default();
                (abs, rel, lines)
            },
            |(abs, rel, lines)| Message::DiffLoaded { abs, rel, lines },
        )
    }

    fn on_open_link(&mut self, url: String) -> Task<Message> {
        // http(s): hand a validated plain URL to the OS opener — never
        // file://, javascript:, a leading '-' (flag injection), etc.
        if url.starts_with("http://") || url.starts_with("https://") {
            let safe = !url.contains(['\n', '\r', '\0']) && url.len() < 2048;
            if safe {
                let opener = if cfg!(target_os = "macos") {
                    "open"
                } else if cfg!(target_os = "windows") {
                    "explorer"
                } else {
                    "xdg-open"
                };
                let _ = std::process::Command::new(opener).arg(&url).spawn();
            } else {
                self.status = format!("Refused to open link: {url}");
            }
            return Task::none();
        }
        // Otherwise treat it as a project-file reference (the overview's
        // links), e.g. `src/find.rs` or `find.rs#L20` — jump to it.
        if let Some((abs, line)) = self.resolve_project_link(&url) {
            self.show_overview = false;
            self.show_stats = false;
            return self.open_file(abs, line, true);
        }
        self.status = format!("Couldn't resolve link: {url}");
        Task::none()
    }

    fn on_settings_saved(&mut self) -> Task<Message> {
        let cfg = llm::Config::from_parts(
            self.settings.provider,
            self.settings.key.clone(),
            self.settings.model.clone(),
            self.settings.base_url.clone(),
        );
        let emb = embed::Config::from_parts(
            self.settings.embed_key.clone(),
            self.settings.embed_model.clone(),
            self.settings.embed_base_url.clone(),
        );
        let saved = cfg.save().and_then(|()| emb.save());
        match saved {
            Ok(()) => {
                self.llm_available = llm::Config::available();
                self.embed_available = embed::Config::available();
                self.settings.open = false;
                self.status = if self.llm_available {
                    format!("Settings saved ({})", cfg.provider.label())
                } else {
                    "Saved — add an API key to enable Explain".into()
                };
            }
            Err(e) => self.status = format!("Save failed: {e}"),
        }
        Task::none()
    }

    fn on_select_start(&mut self, pane: usize, line: usize, col: usize) -> Task<Message> {
        if pane == 0 || self.split {
            self.active = pane;
        }
        // Clicking the code gives it keyboard focus for cursor motion.
        self.code_focused = true;
        // Cmd/Ctrl-click is go-to-definition, not selection.
        if self.modifiers.command() && !self.modifiers.shift() {
            return self.goto_definition(pane, line, col);
        }
        let extend = self.modifiers.shift();
        if let Some(v) = self.panes.get_mut(pane).and_then(Option::as_mut) {
            let head = (line, col);
            match (extend, v.selection) {
                // Shift-click keeps the existing anchor and moves the head.
                (true, Some((anchor, _))) => v.selection = Some((anchor, head)),
                _ => v.selection = Some((head, head)),
            }
            v.caret = Some(head);
            self.selecting = true;
        }
        let follow = self.follow_caret(Task::none());
        Task::batch([follow, self.sync_reading_context()])
    }

    fn on_view_docs_from_menu(&mut self) -> Task<Message> {
        let Some(menu) = self.context_menu.take() else {
            return Task::none();
        };
        let Some(word) = self
            .panes
            .get(menu.pane)
            .and_then(Option::as_ref)
            .and_then(|v| analyze::word_at(&v.lines, menu.line, menu.col))
        else {
            return Task::none();
        };
        // Docs are built but hold no entry for this symbol (e.g. an
        // undocumented private item): rather than silently doing nothing,
        // fall back to its definition so "View docs" always lands the
        // reader somewhere useful.
        if !self.docs.files.is_empty() && find_doc_by_name(&self.docs.files, &word).is_none() {
            self.status = format!("No doc entry for “{word}” — showing its definition");
            return self.goto_definition(menu.pane, menu.line, menu.col);
        }
        self.view_docs_for(&word);
        Task::none()
    }

    fn on_highlighted(&mut self, abs: PathBuf, lines: Vec<HlLine>, symbols: Vec<Symbol>, docs: HashMap<usize, String>, inactive: HashSet<usize>) -> Task<Message> {
        let lines = Arc::new(lines);
        for slot in &mut self.panes {
            if let Some(v) = slot
                && v.abs == abs
                && v.lines.len() == lines.len()
            {
                v.set_lines(lines.clone());
                v.symbols = symbols.clone();
                v.docs = docs.clone();
                v.inactive_lines = inactive.clone();
                v.highlighted = true;
            }
        }
        // Symbols just landed — resolve the function under the caret.
        self.follow_caret(Task::none())
    }

    fn on_walkthrough_delete(&mut self, i: usize) -> Task<Message> {
        if i >= self.walkthroughs.len() {
            return Task::none();
        }
        self.walkthroughs.remove(i);
        // Keep the open index pointing at the same tour (or clear it when
        // the open one was removed).
        match self.walkthrough_open {
            Some(o) if o == i => {
                self.walkthrough_open = None;
                self.walkthrough_prepared = Vec::new();
            }
            Some(o) if o > i => self.walkthrough_open = Some(o - 1),
            _ => {}
        }
        if let Some(root) = self.project.as_ref().map(|p| p.root.clone())
            && let Err(e) = walkthrough::save_library(&root, &self.walkthroughs)
        {
            self.status = format!("Could not save walkthrough: {e}");
        }
        Task::none()
    }

    fn on_context_menu_opened(&mut self, pane: usize, line: usize, col: usize, x: f32, y: f32) -> Task<Message> {
        if pane == 0 || self.split {
            self.active = pane;
        }
        // Content space → window space (see HoverRequested): drop the
        // pane's scroll so the menu opens at the click, not below it.
        let y = y - self.panes.get(pane).and_then(Option::as_ref).map_or(0.0, |v| v.scroll_y);
        self.context_menu = Some(ContextMenu {
            pane,
            line,
            col,
            x,
            y,
        });
        Task::none()
    }

    fn on_bookmark_toggled(&mut self) -> Task<Message> {
        let line_height = self.line_height();
        let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
            return Task::none();
        };
        let Some(v) = self.active_viewer() else {
            return Task::none();
        };
        let line = v.current_line(line_height);
        let mut preview = v.line_text(line).trim().to_string();
        if preview.chars().count() > 80 {
            preview = preview.chars().take(80).collect();
        }
        let rel = v.rel.clone();
        let added = bookmarks::toggle(&mut self.bookmarks, &rel, line, preview);
        self.status = match bookmarks::save(&root, &self.bookmarks) {
            Ok(()) if added => format!("Bookmarked {rel}:{line}"),
            Ok(()) => format!("Removed bookmark {rel}:{line}"),
            Err(e) => format!("Cannot write .clew/bookmarks.json: {e}"),
        };
        Task::none()
    }

    fn on_tree_updated(&mut self, result: ScanResult) -> Task<Message> {
        // Only apply to the current project (a stale rescan from a
        // previous root is ignored).
        let current = self.project.as_ref().map(|p| p.root.clone());
        if current.as_deref() == Some(result.root.as_path()) {
            if let Some(p) = &mut self.project {
                p.tree = result.tree;
                p.files = Arc::new(result.files);
                p.truncated = result.truncated;
            }
            self.refresh_finder();
            // The file set changed, so imports that were unresolved (or
            // resolved to a since-moved file) may now resolve differently.
            self.reresolve_import_graph();
            if let Some(p) = &self.project {
                self.status =
                    format!("{} files · {} symbols", p.files.len(), self.symbol_index.len());
            }
        }
        Task::none()
    }

    fn on_find_opened(&mut self) -> Task<Message> {
        if self.active_viewer().is_none() {
            return Task::none();
        }
        self.find.open = true;
        self.code_focused = false; // the find input takes focus
        // Reveal everything so matches inside collapsed folds are shown.
        if let Some(v) = self.active_viewer_mut() {
            v.expand_all();
        }
        if let Some(v) = self.active_viewer() {
            let lines = v.lines.clone();
            self.find.recompute(&lines);
        }
        Task::batch([
            operation::focus(ui::find_input_id()),
            operation::select_all(ui::find_input_id()),
        ])
    }

    fn on_toggle_inlay_hints(&mut self) -> Task<Message> {
        self.show_inlay_hints = !self.show_inlay_hints;
        self.show_tools_menu = false;
        if self.show_inlay_hints {
            // Re-fetch for every shown file.
            let files: Vec<PathBuf> =
                self.panes.iter().flatten().map(|v| v.abs.clone()).collect();
            let tasks: Vec<Task<Message>> =
                files.iter().map(|abs| self.inlay_request_lookup(abs)).collect();
            Task::batch(tasks)
        } else {
            // Clear so the hints disappear immediately.
            for v in self.panes.iter_mut().flatten() {
                v.inlay_hints.clear();
            }
            Task::none()
        }
    }

    fn on_call_hierarchy_direction(&mut self) -> Task<Message> {
        let Some(tree) = &self.call_graph else {
            return Task::none();
        };
        let toggled = tree.direction.toggled();
        let lang = tree.lang;
        let was_full = tree.full;
        let root_items = tree
            .roots()
            .iter()
            .map(|&r| tree.node(r).item.clone())
            .collect();
        let mut rebuilt = callgraph::CallTree::new(toggled, lang, root_items);
        rebuilt.full = was_full; // keep "expand all" across a direction flip
        self.call_graph = Some(rebuilt);
        let roots = self.call_graph.as_ref().unwrap().roots().to_vec();
        Task::batch(roots.into_iter().map(|r| self.fetch_children(r)).collect::<Vec<_>>())
    }

    fn on_semantic_results(&mut self, query: String, result: Result<Vec<f32>, String>) -> Task<Message> {
        self.searching_semantic = false;
        if query != self.semantic_query.trim() {
            return Task::none(); // superseded by a newer query
        }
        match result {
            Ok(qvec) => {
                self.semantic_results = embed::search(&self.embed_index, &qvec, 20)
                    .into_iter()
                    .map(|(n, s)| (n.clone(), s))
                    .collect();
                self.status = format!("{} semantic matches", self.semantic_results.len());
            }
            Err(e) => self.status = format!("Search failed: {e}"),
        }
        Task::none()
    }

    fn on_open_overlay(&mut self, which: Overlay) -> Task<Message> {
        // The server panel and an overlay are mutually exclusive modals.
        self.server_panel = false;
        self.overlay = Some(which);
        // The call graph is built on demand; (re)build it if the project
        // changed since the last build — but never launch a second build
        // while one is already in flight (single-flight).
        if which == Overlay::ProjectCalls
            && !self.building_calls
            && (self.project_calls.is_empty()
                || self.project_calls_rev != self.registry.revision())
        {
            return self.build_project_calls();
        }
        self.refresh_graph_layout();
        Task::none()
    }

    fn on_window_resized(&mut self, size: Size) -> Task<Message> {
        self.window_width = size.width;
        self.window_height = size.height;
        // Keep panel sizes sane against the new window bounds.
        self.clamp_panel_sizes();
        // Keep the materialized window generous enough for the new
        // height until the next scroll event refines it.
        for v in self.panes.iter_mut().flatten() {
            v.viewport_h = v.viewport_h.max(size.height);
        }
        // The content layer is re-laid-out on resize; re-assert the
        // corner clip so it survives (idempotent, cheap).
        #[cfg(target_os = "macos")]
        macos::round_corners(10.0);
        Task::none()
    }

    fn on_blame_why_done(&mut self, title: String, commits: Vec<(String, String)>, result: Result<String, String>) -> Task<Message> {
        // Ignore a late answer if the user already closed the popup.
        if self.blame_why.is_none() {
            return Task::none();
        }
        let md = match result {
            Ok(m) => m,
            Err(e) => {
                self.status = format!("Couldn't explain: {e}");
                format!("*Couldn't explain why: {e}*")
            }
        };
        let (prepared, task) = self.prepare_segments(&md);
        self.blame_why = Some(BlameWhy { title, commits, loading: false, prepared });
        task
    }

    fn on_toggle_dir(&mut self, rel: String) -> Task<Message> {
        // Cmd+click a folder shows its architectural explanation instead
        // of expanding it.
        if self.modifiers.command()
            && let Some(project) = &self.project
        {
            let node = explain::Node::Folder(project.root.join(&rel));
            self.show_right_panel = true;
            return self.show_explanation(node);
        }
        if !self.expanded.remove(&rel) {
            self.expanded.insert(rel);
        }
        Task::none()
    }

    fn on_time_travel_story_done(&mut self, result: Result<String, String>) -> Task<Message> {
        let md = match result {
            Ok(md) => md,
            Err(e) => {
                self.status = format!("Story failed: {e}");
                return Task::none();
            }
        };
        let (prepared, task) = self.prepare_segments(&md);
        if let Some(tt) = self.time_travel.as_mut() {
            tt.story_loading = false;
            tt.story = Some(prepared);
        }
        task
    }

    fn on_minimap_scrolled(&mut self, pane: usize, fraction: f32) -> Task<Message> {
        let lh = self.line_height();
        if let Some(v) = self.panes.get_mut(pane).and_then(Option::as_mut) {
            let total = v.content_rows() as f32 * lh;
            let max_y = (total - v.viewport_h).max(0.0);
            // Center the clicked fraction in the viewport.
            let y = (fraction * total - v.viewport_h / 2.0).clamp(0.0, max_y);
            v.scroll_y = y;
            return operation::scroll_to(
                ui::code_scroll_id(pane),
                AbsoluteOffset { x: 0.0, y },
            );
        }
        Task::none()
    }

    fn on_lsp_remove(&mut self, name: String, version: String) -> Task<Message> {
        // Stop any running instance of this server first.
        let langs: Vec<String> = lsp::registry::by_name(&name)
            .map(|s| s.languages.iter().map(|l| l.to_string()).collect())
            .unwrap_or_default();
        for lang in langs {
            self.lsp.remove(&lang);
        }
        match lsp::store::remove(&name, &version) {
            Ok(_) => self.status = format!("Removed {name} {version}"),
            Err(e) => self.status = format!("Remove failed: {e}"),
        }
        self.installed_servers = lsp::store::installed_servers();
        Task::none()
    }

    fn on_embeddings_built(&mut self, root: PathBuf, result: Result<embed::Index, String>) -> Task<Message> {
        if self.project.as_ref().map(|p| &p.root) != Some(&root) {
            return Task::none();
        }
        self.building_embeddings = false;
        match result {
            Ok(index) => {
                let _ = embed::save(&root, &index);
                self.status = format!("Semantic index ready ({} items)", index.entries.len());
                self.embed_index = index;
            }
            Err(e) => self.status = format!("Index build failed: {e}"),
        }
        Task::none()
    }

    fn on_connect_field(&mut self, field: ConnectField, value: String) -> Task<Message> {
        if let Some(ui) = &mut self.connect {
            match field {
                ConnectField::Name => ui.name = value,
                ConnectField::Host => ui.host = value,
                ConnectField::User => ui.user = value,
                // Keep only digits so the port stays parseable.
                ConnectField::Port => {
                    ui.port = value.chars().filter(char::is_ascii_digit).collect()
                }
                ConnectField::Identity => ui.identity = value,
            }
        }
        Task::none()
    }

    fn on_copy_selection(&mut self) -> Task<Message> {
        // In time travel, copy the historical selection, not the live one.
        let viewer = self
            .time_travel
            .as_ref()
            .and_then(|t| t.viewer.as_ref())
            .or_else(|| self.active_viewer());
        let Some(text) = viewer.and_then(Viewer::selected_text) else {
            return Task::none();
        };
        let n = text.lines().count();
        self.status = format!("Copied {n} line{}", if n == 1 { "" } else { "s" });
        iced::clipboard::write(text)
    }

    fn on_consent_allowed(&mut self) -> Task<Message> {
        let Some(root) = self.pending_consent.take() else {
            return Task::none();
        };
        // Consent is recorded by the .clew directory itself.
        match std::fs::create_dir_all(root.join(".clew")) {
            Ok(()) => self.start_scan(root),
            Err(e) => {
                self.pending_open = None;
                self.status = format!("Cannot open project: .clew is not writable ({e})");
                Task::none()
            }
        }
    }

    fn on_call_hierarchy_expand(&mut self, id: usize) -> Task<Message> {
        let needs = self
            .call_graph
            .as_ref()
            .is_some_and(|t| t.needs_fetch(id));
        if needs {
            self.fetch_children(id)
        } else {
            if let Some(t) = &mut self.call_graph {
                t.toggle(id);
            }
            Task::none()
        }
    }

    fn on_toggle_split(&mut self) -> Task<Message> {
        if self.split {
            self.split = false;
            self.panes[1] = None;
            self.active = 0;
        } else {
            self.split = true;
            // Duplicate the current file for side-by-side reading.
            self.panes[1] = self.panes[0].clone();
            self.active = 1;
        }
        Task::none()
    }

    fn on_svgs_generated(&mut self, generation: u64, map: HashMap<u64, richmd::PreparedSvg>) -> Task<Message> {
        // SVGs are keyed by content hash (and disk-cached), so inserting
        // is idempotent — accept them even from a superseded generation,
        // otherwise a concurrent `prepare_segments` bumping the counter
        // can strand a diagram as a perpetual placeholder.
        for (key, prepared) in map {
            self.insert_svg(key, prepared);
        }
        if generation == self.explain_svg_gen {
            self.status = "Rendered math & diagrams".into();
        }
        Task::none()
    }

    fn on_refresh_all(&mut self) -> Task<Message> {
        if !self.llm_available {
            self.status = format!("Add an API key in Settings ({})", llm::config_hint());
            return Task::done(Message::OpenSettings);
        }
        // Already refreshing — let it finish (the chip is disabled too).
        if self.explaining || self.generating_overview || self.building_embeddings {
            return Task::none();
        }
        // Manual: bypass the 30s cooldown entirely.
        self.status = "Refreshing…".into();
        self.begin_refresh()
    }

    fn on_definition_result(
        &mut self,
        result: Result<Vec<lsp::client::Target>, String>,
    ) -> Task<Message> {
        match result {
            Ok(targets) if !targets.is_empty() => {
                let t = &targets[0];
                let abs = t.path.clone();
                let target_line = t.line + 1;
                // Clear the "Looking up definition…" progress; the jump itself
                // is the feedback (otherwise the status stays stuck on it).
                self.status.clear();
                self.open_file(abs, Some(target_line), true)
            }
            Ok(_) => {
                self.status = "No definition found".into();
                Task::none()
            }
            Err(e) => {
                self.status = format!("Definition failed: {e}");
                Task::none()
            }
        }
    }

    fn on_references_result(
        &mut self,
        result: Result<Vec<lsp::client::Target>, String>,
    ) -> Task<Message> {
        match result {
            Ok(refs) if !refs.is_empty() => {
                self.status = format!("{} reference(s) — showing them in Search", refs.len());
                self.show_references(refs)
            }
            Ok(_) => {
                self.status = "No references".into();
                Task::none()
            }
            Err(e) => {
                self.status = format!("References failed: {e}");
                Task::none()
            }
        }
    }

    fn on_open_settings(&mut self) -> Task<Message> {
        let c = llm::Config::current_or_default();
        self.settings.provider = c.provider;
        self.settings.key = c.api_key;
        self.settings.model = c.model;
        self.settings.base_url = c.base_url;
        let e = embed::Config::current_or_default();
        self.settings.embed_key = e.api_key;
        self.settings.embed_model = e.model;
        self.settings.embed_base_url = e.base_url;
        self.settings.open = true;
        Task::none()
    }

    fn on_goto_line_requested(&mut self) -> Task<Message> {
        if self.project.is_none() {
            return Task::none();
        }
        self.finder.open = true;
        self.finder.mode = FinderMode::Files;
        self.finder.query = ":".to_string();
        self.refresh_finder();
        Task::batch([
            operation::focus(ui::finder_input_id()),
            operation::move_cursor_to_end(ui::finder_input_id()),
        ])
    }

    fn on_finder_confirm(&mut self) -> Task<Message> {
        if let Some(line) = self.finder.goto_line() {
            self.finder.open = false;
            if let Some(abs) = self.active_viewer().map(|v| v.abs.clone()) {
                return self.open_file(abs, Some(line), true);
            }
            return Task::none();
        }
        match self.finder.results.get(self.finder.selected).copied() {
            Some(idx) => self.finder_open_index(idx),
            None => Task::none(),
        }
    }

    fn on_call_hierarchy_prepared(&mut self, direction: callgraph::Direction, lang: &'static str, items: Vec<lsp::client::CallItem>) -> Task<Message> {
        if items.is_empty() {
            self.status = "No call hierarchy for the symbol under the cursor".into();
            return Task::none();
        }
        self.call_graph = Some(callgraph::CallTree::new(direction, lang, items));
        self.sidebar = SidebarTab::Calls;
        // The tree is now the feedback; clear the transient "Building…"
        // status so it doesn't linger after results appear.
        self.status.clear();
        let roots = self.call_graph.as_ref().unwrap().roots().to_vec();
        Task::batch(roots.into_iter().map(|r| self.fetch_children(r)).collect::<Vec<_>>())
    }

    fn on_walkthrough_step(&mut self, delta: i32) -> Task<Message> {
        let n = self
            .walkthrough_open
            .and_then(|o| self.walkthroughs.get(o))
            .map(|w| w.steps.len())
            .unwrap_or(0);
        if n == 0 {
            return Task::none();
        }
        let i = (self.walkthrough_step as i32 + delta).clamp(0, n as i32 - 1) as usize;
        self.walkthrough_goto(i)
    }

    fn on_toggle_breakpoint_from_menu(&mut self) -> Task<Message> {
        let Some(menu) = self.context_menu.take() else {
            return Task::none();
        };
        let Some(abs) =
            self.panes.get(menu.pane).and_then(Option::as_ref).map(|v| v.abs.clone())
        else {
            return Task::none();
        };
        // menu.line is 0-based; breakpoints are 1-based.
        self.update(Message::BreakpointToggle { path: abs, line: menu.line + 1 })
    }

    fn on_time_travel_why_done(&mut self, sha: String, result: Result<String, String>) -> Task<Message> {
        if let Some(tt) = self.time_travel.as_mut() {
            tt.why_loading = false;
            match result {
                Ok(text) => {
                    tt.why.insert(sha, text.trim().to_string());
                }
                Err(e) => self.status = format!("Couldn't summarize: {e}"),
            }
        }
        Task::none()
    }

    fn on_stats_done(&mut self, root: PathBuf, rev: u64, report: stats::StatsReport) -> Task<Message> {
        // Drop a result from a project the user already switched away from.
        if self.project.as_ref().map(|p| &p.root) != Some(&root) {
            return Task::none();
        }
        self.building_stats = false;
        self.stats_rev = rev;
        let _ = stats::save(&root, &stats::Cached { report: report.clone(), rev });
        self.stats = Some(report);
        self.status = "Code statistics ready".into();
        Task::none()
    }

    fn on_select_drag(&mut self, pane: usize, line: usize, col: usize) -> Task<Message> {
        if self.selecting
            && pane == self.active
            && let Some(v) = self.panes.get_mut(pane).and_then(Option::as_mut)
            && let Some((anchor, _)) = v.selection
        {
            let head = (line, col);
            v.selection = Some((anchor, head));
            v.caret = Some(head);
        }
        Task::none()
    }

    fn on_open_rel(&mut self, rel: String, line: Option<usize>) -> Task<Message> {
        let Some(project) = &self.project else {
            return Task::none();
        };
        let abs = project.root.join(&rel);
        // Cmd+click a file shows its explanation instead of opening it.
        if self.modifiers.command() {
            self.show_right_panel = true;
            return self.show_explanation(explain::Node::File(abs));
        }
        self.open_file(abs, line, true)
    }

    fn on_import_expand(&mut self, id: usize) -> Task<Message> {
        if let (Some(mut tree), Some(root)) =
            (self.import_tree.take(), self.project.as_ref().map(|p| p.root.clone()))
        {
            // Guard against a stale id from a since-rebuilt (smaller) tree.
            if id < tree.node_count() {
                tree.toggle(id, &self.import_graph, &root);
            }
            self.import_tree = Some(tree);
        }
        Task::none()
    }

    fn on_debug_stop(&mut self) -> Task<Message> {
        self.status = "Debugger stopped".into();
        match self.debug.take().and_then(|s| s.client) {
            Some(client) => Task::perform(
                async move {
                    let _ = client.disconnect().await;
                },
                |()| Message::Noop,
            ),
            None => Task::none(),
        }
    }

    fn on_bp_condition_set(&mut self) -> Task<Message> {
        let Some((path, line, draft)) = self.bp_cond_edit.take() else {
            return Task::none();
        };
        let cond = draft.trim();
        let bp = Bp {
            condition: (!cond.is_empty()).then(|| cond.to_string()),
        };
        self.breakpoints.entry(path.clone()).or_default().insert(line, bp);
        self.status = "Conditional breakpoint set".into();
        self.push_breakpoints(&path)
    }

    fn on_toggle_ask(&mut self) -> Task<Message> {
        // Toolbar "Ask": open the bottom panel on the Ask tab, or collapse
        // it if Ask is already the shown tab.
        if self.show_bottom && self.bottom_tab == BottomTab::Ask {
            self.show_bottom = false;
        } else {
            self.show_bottom = true;
            self.bottom_tab = BottomTab::Ask;
        }
        Task::none()
    }

    fn on_time_travel_select_drag(&mut self, line: usize, col: usize) -> Task<Message> {
        if self.selecting
            && let Some(v) = self.time_travel.as_mut().and_then(|t| t.viewer.as_mut())
            && let Some((anchor, _)) = v.selection
        {
            let head = (line, col);
            v.selection = Some((anchor, head));
            v.caret = Some(head);
        }
        Task::none()
    }

    fn on_time_travel_scrolled(&mut self, viewport: scrollable::Viewport) -> Task<Message> {
        // Only track real scrolls once the revision is loaded; the loading
        // fallback view mounts at offset 0 and would otherwise clobber the
        // carried entry scroll before the step applies it.
        if let Some(tt) = self.time_travel.as_mut()
            && tt.viewer.is_some()
        {
            tt.scroll_y = viewport.absolute_offset().y;
        }
        Task::none()
    }

    fn on_remote_open_here(&mut self) -> Task<Message> {
        let cwd = match self.connect.as_ref().map(|u| &u.stage) {
            Some(ConnectStage::Browsing(b)) => Some(b.cwd.clone()),
            _ => None,
        };
        if let Some(cwd) = cwd {
            self.connect = None;
            return self.start_scan(PathBuf::from(cwd));
        }
        Task::none()
    }

    fn on_outline_jump(&mut self, line: usize) -> Task<Message> {
        let Some(abs) = self.active_viewer().map(|v| v.abs.clone()) else {
            return Task::none();
        };
        // Cmd+click an outline symbol explains it; a plain click jumps.
        if self.modifiers.command() {
            self.explain_symbol_at(abs, line)
        } else {
            self.open_file(abs, Some(line), true)
        }
    }

    fn on_explain_from_menu(&mut self) -> Task<Message> {
        let Some(menu) = self.context_menu.take() else {
            return Task::none();
        };
        let file = self.panes.get(menu.pane).and_then(Option::as_ref).map(|v| v.abs.clone());
        match file {
            // menu.line is 0-based; explain_symbol_at wants 1-based.
            Some(file) => self.explain_symbol_at(file, menu.line + 1),
            None => Task::none(),
        }
    }

    fn on_debug_watch_remove(&mut self, i: usize) -> Task<Message> {
        if i < self.debug_watches.len() {
            self.debug_watches.remove(i);
        }
        if let Some(s) = self.debug.as_mut()
            && i < s.watches.len()
        {
            s.watches.remove(i);
        }
        Task::none()
    }

    fn on_bookmark_removed(&mut self, idx: usize) -> Task<Message> {
        if idx < self.bookmarks.len() {
            self.bookmarks.remove(idx);
            if let Some(p) = &self.project
                && let Err(e) = bookmarks::save(&p.root, &self.bookmarks)
            {
                self.status = format!("Cannot write .clew/bookmarks.json: {e}");
            }
        }
        Task::none()
    }

    fn on_bookmark_note_save(&mut self) -> Task<Message> {
        if let Some((rel, line, draft)) = self.note_edit.take() {
            bookmarks::set_note(&mut self.bookmarks, &rel, line, Some(draft));
            if let Some(p) = &self.project
                && let Err(e) = bookmarks::save(&p.root, &self.bookmarks)
            {
                self.status = format!("Cannot write .clew/bookmarks.json: {e}");
            }
        }
        Task::none()
    }

    fn on_ask_delta(&mut self, text: String) -> Task<Message> {
        // First token(s): the answer is streaming, not "thinking".
        self.asking = false;
        if let Some(turn) = self.ask_turns.last_mut()
            && turn.streaming
        {
            turn.answer_md.push_str(&text);
        }
        // Follow the growing answer.
        operation::scroll_to(ui::ask_scroll_id(), AbsoluteOffset { x: 0.0, y: f32::MAX })
    }

    fn on_finder_opened(&mut self, mode: FinderMode) -> Task<Message> {
        if self.project.is_none() {
            return Task::none();
        }
        self.finder.open = true;
        self.finder.mode = mode;
        self.finder.query.clear();
        self.code_focused = false; // the finder input takes focus
        self.refresh_finder();
        operation::focus(ui::finder_input_id())
    }

    fn on_walkthrough_regenerate(&mut self, i: usize) -> Task<Message> {
        let Some(scope) = self.walkthroughs.get(i).map(|w| w.scope.clone()) else {
            return Task::none();
        };
        // A change-review tour re-runs the diff; a normal tour re-generates.
        if scope.starts_with("@diff") {
            return Task::done(Message::GenerateDiffWalkthrough);
        }
        Task::done(Message::GenerateWalkthrough(scope))
    }

    fn on_hover_cleared(&mut self) -> Task<Message> {
        // Cursor left the code area — drop the peek and cancel any pending
        // dwell (a stale HoverDwell will see the bumped gen and no-op).
        // But not if it's inside the tooltip (which overlaps the code).
        if !self.hover_pinned {
            self.hover = None;
            self.hover_gen = self.hover_gen.wrapping_add(1);
        }
        Task::none()
    }

    fn on_git_info_loaded(&mut self, abs: PathBuf, info: Option<Arc<git::GitInfo>>) -> Task<Message> {
        for slot in &mut self.panes {
            if let Some(v) = slot
                && v.abs == abs
            {
                v.git = info.clone();
            }
        }
        Task::none()
    }

    fn on_call_hierarchy_expand_all(&mut self) -> Task<Message> {
        let frontier = match &mut self.call_graph {
            Some(t) => {
                t.full = true;
                t.unfetched_frontier()
            }
            None => return Task::none(),
        };
        Task::batch(frontier.into_iter().map(|id| self.fetch_children(id)).collect::<Vec<_>>())
    }

    fn on_breakpoint_toggle(&mut self, path: PathBuf, line: usize) -> Task<Message> {
        let map = self.breakpoints.entry(path.clone()).or_default();
        if map.remove(&line).is_none() {
            map.insert(line, Bp::default());
        }
        if map.is_empty() {
            self.breakpoints.remove(&path);
        }
        self.push_breakpoints(&path)
    }

    fn on_bookmark_note_edit(&mut self, rel: String, line: usize) -> Task<Message> {
        let existing = self
            .bookmarks
            .iter()
            .find(|b| b.rel == rel && b.line == line)
            .and_then(|b| b.note.clone())
            .unwrap_or_default();
        self.note_edit = Some((rel, line, existing));
        operation::focus(ui::note_input_id())
    }

    fn request_auto_refresh(&mut self) -> Task<Message> {
        if !self.llm_available || self.explanations.is_empty() {
            return Task::none();
        }
        // Let any running pass finish, then re-check on completion / next tick.
        if self.explaining || self.generating_overview || self.building_embeddings {
            self.refresh_pending = true;
            return Task::none();
        }
        let cooled = self
            .last_auto_refresh
            .map(|t| t.elapsed() >= AUTO_REFRESH_MIN_INTERVAL)
            .unwrap_or(true);
        if cooled {
            self.begin_refresh()
        } else {
            self.refresh_pending = true;
            Task::none()
        }
    }

    /// Begin a refresh pass now, resetting the cooldown. Shared by the auto path
    /// and the manual force-refresh. The explain pass is cache-aware (only changed
    /// nodes hit the LLM); on completion it chains the semantic index and overview
    /// when those already exist (see `ExplainDone`).
    fn begin_refresh(&mut self) -> Task<Message> {
        self.last_auto_refresh = Some(std::time::Instant::now());
        self.refresh_pending = false;
        Task::done(Message::ExplainProject)
    }

    /// Whether the overview's inputs changed since it was generated, so a chained
    /// refresh regenerates it only when the result would actually differ (an
    /// overview pass is a full LLM call, unlike the incremental explain/index).
    fn overview_inputs_changed(&self) -> bool {
        let hash =
            incremental::content_hash(overview::prompt(&self.gather_overview_inputs()).as_bytes());
        self.overview_prompt_hash != Some(hash)
    }

    /// Fold a freshly-computed module diagram into the raw overview markdown for
    /// display. Returns the assembled markdown and the diagram used (so callers
    /// can tell whether a re-prepare is worthwhile).
    /// The overview prose for display: strip any legacy mermaid "Module map"
    /// section a cached overview may still carry (the map is drawn natively now).
    fn overview_display(&self, raw: &str) -> String {
        overview::strip_module_map(raw)
    }

    /// Lay out the module map from the current import graph, or None when there's
    /// too little structure to show.
    fn compute_overview_map(&self) -> Option<graphlayout::Layout> {
        let (nodes, edges) = overview::module_layout_inputs(&self.import_graph.scope_map())?;
        Some(graphlayout::layout(nodes, edges))
    }

    /// Recompute the native module-map layout, e.g. once the import graph finishes
    /// resolving. Cheap and synchronous — the map is a canvas, not a prose segment.
    fn refresh_overview_map(&mut self) -> Task<Message> {
        if self.overview.is_some() {
            self.overview_map = self.compute_overview_map();
        }
        Task::none()
    }

    fn on_scan_done(&mut self, result: ScanResult) -> Task<Message> {
        self.scanning = false;
        self.status = format!(
            "{} files{}",
            result.files.len(),
            if result.truncated { " (truncated)" } else { "" }
        );
        self.expanded.clear();
        self.panes = [None, None];
        self.split = false;
        self.active = 0;
        // Warm-start the navigation tree from this project's persisted history.
        self.history = history::load(&result.root);
        self.finder = Finder::default();
        self.search = SearchState::default();
        self.bookmarks = bookmarks::load(&result.root);
        self.notes = notes::load(&result.root);
        self.symbol_index = Arc::new(Vec::new());
        self.symbol_index_by_file.clear();
        // A new project: drop the old API docs (they belong to the old root).
        self.docs.files = Vec::new();
        self.docs.loading = false;
        self.docs.expanded.clear();
        self.docs.page = None;
        self.docs.filter.clear();
        self.docs.pending_view = None;
        self.registry.clear();
        self.call_graph = None;
        self.import_graph = imports::ImportGraph::default();
        self.import_tree = None;
        self.import_cycles = Vec::new();
        self.project_calls = projectcalls::ProjectCallGraph::default();
        self.project_calls_rev = 0;
        self.building_calls = false;
        self.project_calls_precise = false;
        self.calls_gen += 1;
        self.refine_progress = None;
        self.precise_edges = projectcalls::SymEdges::default();
        self.precise_pending = HashSet::new();
        self.overlay = None;
        self.graph_layout = None;
        // Warm-start explanations from this project's persisted cache.
        self.explanations = explain::load(&result.root);
        self.explaining = false;
        self.explain_progress = None;
        self.explain_gen += 1;
        self.explain_view = None;
        self.explain_prepared = Vec::new();
        self.explain_svgs.clear();
        self.explain_showing_detail = false;
        // Land on the architecture-overview home (warm-started from cache below).
        let cached_overview = overview::load(&result.root);
        self.overview_prompt_hash = cached_overview.as_ref().map(|c| c.prompt_hash);
        self.overview = cached_overview.map(|c| c.markdown);
        self.overview_prepared = Vec::new();
        self.generating_overview = false;
        self.show_overview = true;
        // Warm-start stats from disk so the Stats view paints instantly; the
        // `u64::MAX` sentinel forces one background refresh on first entry (the
        // registry revision — the freshness key — isn't stable across restarts).
        self.stats = stats::load(&result.root).map(|c| c.report);
        self.stats_rev = u64::MAX;
        self.building_stats = false;
        self.show_stats = false;
        // A fresh project starts with a clean auto-refresh cooldown.
        self.last_auto_refresh = None;
        self.refresh_pending = false;
        // Warm-start the semantic index and reset the search state.
        self.embed_index = embed::load(&result.root);
        self.embed_available = embed::Config::available();
        self.building_embeddings = false;
        self.semantic_query = String::new();
        self.semantic_results = Vec::new();
        self.searching_semantic = false;
        // Drop any servers from the previous project (kills their children).
        self.lsp.clear();
        self.lsp_opened.clear();
        self.pending_lsp_consent = None;
        self.lsp_config = lsp::config::ProjectLspConfig::load(&result.root).unwrap_or_default();
        self.reading_target = reading::load_target(&result.root).unwrap_or_else(inactive::Target::host);
        self.walkthroughs = walkthrough::load_library(&result.root);
        self.walkthrough_open = None;
        self.walkthrough_step = 0;
        self.walkthrough_prepared = Vec::new(); // prepared lazily when a tour opens
        // Languages actually present in the project that clew ships a server
        // for — drives which rows the server panel shows.
        let mut langs: Vec<String> = result
            .files
            .iter()
            .filter_map(|f| highlight::detect(&f.abs))
            .filter(|l| lsp::registry::default_for_language(l).is_some())
            .map(|l| l.to_string())
            .collect();
        langs.sort();
        langs.dedup();
        self.project_languages = langs;
        let files = Arc::new(result.files);
        self.project = Some(Project {
            root: result.root,
            tree: result.tree,
            files: files.clone(),
            truncated: result.truncated,
        });
        // The server already knows this project: either it produced this tree
        // (server-scan path in `start_scan`), or — if this came from the local
        // fallback — the `ServerConnected` handler syncs it when the server is up.

        // Build the project-wide symbol index in the background, warm-starting
        // from the persistent cache (only files changed while clew was closed
        // are re-read/re-parsed), and persist the refreshed cache.
        self.indexing = true;
        let index_root = self.project.as_ref().unwrap().root.clone();
        let tag_root = index_root.clone();
        let index_task = Task::perform(
            async move {
                tokio::task::spawn_blocking(move || index::build_indexed_warm(&index_root, files))
                    .await
                    .unwrap_or_default()
            },
            move |indexed| Message::SymbolIndexDone {
                root: tag_root.clone(),
                indexed,
            },
        );

        let open_task = match self.pending_open.take() {
            Some(file) => self.open_file(file, None, true), // clears show_overview
            None => Task::none(),
        };
        // Prepare the cached overview so the home screen renders it immediately.
        // The module map lays out from the import graph; if imports aren't
        // resolved yet, it fills in when indexing completes (refresh_overview_map).
        let overview_task = match self.overview.clone() {
            Some(md) => {
                let display = self.overview_display(&md);
                let (prepared, task) = self.prepare_segments(&display);
                self.overview_prepared = prepared;
                self.overview_map = self.compute_overview_map();
                task
            }
            None => Task::none(),
        };
        // No auto-explain on startup: warm-start from the persisted cache and
        // show what's there. Explanations (re)generate only on an explicit
        // request (whole project / one function) or when a file's hash changes.
        Task::batch([index_task, open_task, overview_task])
    }

    /// Status text and the action button for a language row in the server
    /// panel. Distinguishes running / installed-but-idle / not-downloaded so an
    /// installed server never shows a misleading "Download".
    pub fn lsp_row(&self, language: &str) -> (String, Option<(&'static str, Message)>) {
        let restart = || {
            Some((
                "Restart",
                Message::LspRestart(language.to_string()),
            ))
        };
        let provision = |label: &'static str| {
            Some((label, Message::LspDownloadFor(language.to_string())))
        };
        match self.lsp.get(language) {
            Some(LspSlot::Ready(c)) => (c.progress().unwrap_or_else(|| "ready".into()), restart()),
            Some(LspSlot::Starting) => ("starting…".into(), None),
            Some(LspSlot::Failed(e)) => (format!("error: {e}"), provision("Retry")),
            Some(LspSlot::Unsupported(e)) => (e.clone(), None),
            Some(LspSlot::AwaitingConsent) => ("download pending".into(), provision("Download")),
            None => match self.lsp_config.resolve(language) {
                Some(server) => match lsp::store::locate(&server) {
                    lsp::store::Located::Ready(_) => {
                        ("installed · starts on open".into(), provision("Start"))
                    }
                    lsp::store::Located::NeedsDownload { .. } => {
                        ("not downloaded".into(), provision("Download"))
                    }
                    lsp::store::Located::NeedsInstall { .. } => {
                        ("not installed".into(), provision("Install"))
                    }
                    lsp::store::Located::Unsupported(m) => (m, None),
                },
                None => ("no server".into(), None),
            },
        }
    }

    /// Languages to show in the server panel: those present in the project,
    /// plus any installed or running server (so they can be managed anywhere).
    pub fn managed_languages(&self) -> Vec<String> {
        let mut langs = self.project_languages.clone();
        for srv in &self.installed_servers {
            if let Some(spec) = lsp::registry::by_name(&srv.name) {
                langs.extend(spec.languages.iter().map(|l| l.to_string()));
            }
        }
        langs.extend(self.lsp.keys().cloned());
        langs.sort();
        langs.dedup();
        langs
    }

    /// Ensure a language server is provisioned/started for `language`, and open
    /// any already-loaded documents once it is ready. Idempotent.
    fn ensure_lsp(&mut self, language: &str) -> Task<Message> {
        if self.project.is_none() {
            return Task::none();
        }
        match self.lsp.get(language) {
            Some(LspSlot::Ready(client)) => {
                let client = client.clone();
                return self.open_docs_for_language(language, &client);
            }
            // Starting / failed / unsupported / awaiting consent: nothing to do.
            Some(_) => return Task::none(),
            None => {}
        }

        let Some(server) = self.lsp_config.resolve(language) else {
            self.lsp.insert(
                language.to_string(),
                LspSlot::Unsupported("no server for this language".into()),
            );
            return Task::none();
        };
        let (provision, dest_dir) = match lsp::store::locate(&server) {
            lsp::store::Located::Ready(exe) => return self.start_lsp_with(language, exe),
            lsp::store::Located::NeedsDownload { download, dest_dir } => {
                (LspProvision::Download(download), dest_dir)
            }
            lsp::store::Located::NeedsInstall {
                install, dest_dir, ..
            } => (LspProvision::Install(install), dest_dir),
            lsp::store::Located::Unsupported(msg) => {
                self.lsp.insert(language.to_string(), LspSlot::Unsupported(msg));
                return Task::none();
            }
        };
        self.lsp
            .insert(language.to_string(), LspSlot::AwaitingConsent);
        self.pending_lsp_consent = Some(LspConsent {
            language: language.to_string(),
            server_name: server.server_name,
            version: server.version,
            provision,
            dest_dir,
        });
        Task::none()
    }

    /// Launch the server executable and run the handshake in the background.
    fn start_lsp_with(&mut self, language: &str, exe: PathBuf) -> Task<Message> {
        let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
            return Task::none();
        };
        let Some(server) = self.lsp_config.resolve(language) else {
            return Task::none();
        };
        self.lsp.insert(language.to_string(), LspSlot::Starting);
        let lang = language.to_string();
        let args = server.args.clone();
        // Merge the auto-detected language environment (e.g. a project venv for
        // Python) under any explicit lsp.toml init_options (explicit wins).
        let init = langenv::merge(language, &server.server_name, &root, server.init_options.clone());

        // Preferred: spawn the language server on clew-server and proxy its
        // stdio, so it runs where the code lives (local today, remote later).
        if let Some(tx) = self.server_tx.clone() {
            // Kill a previous instance for this language (a restart).
            if let Some(old) = self.lsp_procs.remove(&lang) {
                self.proc_feeds.remove(&old);
                let _ = tx.send(clew_protocol::ClientMessage {
                    id: 0,
                    request: clew_protocol::Request::ProcessKill { proc: old },
                });
            }
            let proc = self.next_proc_id;
            self.next_proc_id += 1;
            self.lsp_procs.insert(lang.clone(), proc);

            // Remote: the server resolves and runs its OWN language server, so we
            // never ship a binary path. Local: send the client-resolved binary.
            let spawn = if self.connection.is_remote() {
                clew_protocol::Request::SpawnLsp {
                    proc,
                    language: lang.clone(),
                }
            } else {
                clew_protocol::Request::SpawnProcess {
                    proc,
                    cmd: exe.to_string_lossy().into_owned(),
                    args: args.clone(),
                    cwd: Some(root.to_string_lossy().into_owned()),
                }
            };
            let (client_stdin, client_stdout, feed) = proxy_transport(&tx, proc, spawn);
            self.proc_feeds.insert(proc, feed);

            let lang_done = lang.clone();
            return Task::perform(
                async move {
                    lsp::client::LspClient::connect(client_stdin, client_stdout, &root, init).await
                },
                move |result| Message::LspStartResult {
                    language: lang_done.clone(),
                    result,
                },
            );
        }

        // Fallback: spawn the language server locally.
        Task::perform(
            async move { lsp::client::LspClient::start(&exe, &args, &root, init).await },
            move |result| Message::LspStartResult {
                language: lang.clone(),
                result,
            },
        )
    }

    /// Send `didOpen` for every loaded document of `language` not yet opened.
    fn open_docs_for_language(
        &mut self,
        language: &str,
        client: &lsp::client::LspClient,
    ) -> Task<Message> {
        let docs: Vec<(PathBuf, Arc<String>)> = self
            .panes
            .iter()
            .flatten()
            .filter(|v| v.lang_key == Some(language))
            .map(|v| (v.abs.clone(), v.source.clone()))
            .collect();
        let mut tasks = Vec::new();
        for (path, source) in docs {
            if self.lsp_opened.insert(path.clone()) {
                client.did_open(&path, language, 1, &source);
            }
            tasks.push(self.inlay_request(&path, client));
        }
        Task::batch(tasks)
    }

    /// Request whole-file inlay hints for `abs` from `client` (no-op unless the
    /// server advertised the capability). Whole-file, not per-viewport: simpler,
    /// and the server caches.
    fn inlay_request(&self, abs: &Path, client: &lsp::client::LspClient) -> Task<Message> {
        if !client.inlay_hint || !self.show_inlay_hints {
            return Task::none();
        }
        let Some(lines) = self.panes.iter().flatten().find(|v| v.abs == *abs).map(|v| v.lines.len())
        else {
            return Task::none();
        };
        let client = client.clone();
        let path = abs.to_path_buf();
        let tag = path.clone();
        Task::perform(
            async move { client.inlay_hints(&path, 0, lines).await },
            move |hints| Message::InlayHintsLoaded { abs: tag.clone(), hints },
        )
    }

    /// Request inlay hints for `abs`, looking its language's server up in the
    /// registry (for callers that don't already hold the client).
    fn inlay_request_lookup(&self, abs: &Path) -> Task<Message> {
        let Some(lang) =
            self.panes.iter().flatten().find(|v| v.abs == *abs).and_then(|v| v.lang_key)
        else {
            return Task::none();
        };
        match self.lsp.get(lang) {
            Some(LspSlot::Ready(client)) => self.inlay_request(abs, client),
            _ => Task::none(),
        }
    }

    /// Resolve the definition at a clicked (line, display col) in `pane`.
    fn goto_definition(&mut self, pane: usize, line: usize, col: usize) -> Task<Message> {
        self.goto_request(pane, line, col, GotoKind::Definition)
    }

    /// Kick off a project search from the current query and options.
    fn run_search(&mut self) -> Task<Message> {
        let Some(project) = &self.project else {
            return Task::none();
        };
        if self.search.query.trim().is_empty() {
            self.search.hits.clear();
            self.search.error = None;
            self.search.ran = false;
            return Task::none();
        }
        self.search.running = true;
        self.search.ran = true;
        self.search.hits.clear();
        let files = project.files.clone();
        let opts = search::SearchOptions {
            query: self.search.query.trim().to_string(),
            regex: self.search.regex,
            case_sensitive: self.search.case_sensitive,
            whole_word: self.search.whole_word,
            include: self.search.include.clone(),
            exclude: self.search.exclude.clone(),
        };

        // Preferred path: run the search on the clew-server over the protocol.
        // Results come back as `Event::SearchResults` (see `handle_server_event`).
        if let Some(tx) = &self.server_tx {
            let request = clew_protocol::Request::Search {
                query: opts.query.clone(),
                regex: opts.regex,
                case_sensitive: opts.case_sensitive,
                whole_word: opts.whole_word,
                include: opts.include.clone(),
                exclude: opts.exclude.clone(),
            };
            if tx
                .send(clew_protocol::ClientMessage { id: 0, request })
                .is_ok()
            {
                return Task::none();
            }
        }

        // Fallback: server not connected yet (or its channel closed) — run the
        // same search in-process so search never depends on handshake timing.
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || search::search(files, opts))
                    .await
                    .unwrap_or_default()
            },
            |result| Message::SearchDone { result },
        )
    }

    /// Apply a completed search to the UI — shared by the server path and the
    /// in-process fallback so both render results identically.
    fn apply_search_result(&mut self, result: search::SearchResult) {
        self.search.running = false;
        self.search.error = result.error.clone();
        self.status = match &result.error {
            Some(e) => e.clone(),
            None if result.hits.len() >= search::MAX_HITS => {
                format!("{}+ matches (capped)", result.hits.len())
            }
            None => format!("{} matches", result.hits.len()),
        };
        self.search.hits = result.hits;
    }

    /// Tell the clew-server which project to scan, so its file-backed flows
    /// (search today) have a file list. Sent on project open and on (re)connect,
    /// whichever happens second; a no-op until both a project and the server are
    /// present.
    fn sync_project_to_server(&self) {
        if let (Some(tx), Some(project)) = (&self.server_tx, &self.project) {
            let request = clew_protocol::Request::OpenProject {
                root: project.root.to_string_lossy().into_owned(),
            };
            let _ = tx.send(clew_protocol::ClientMessage { id: 0, request });
        }
    }

    /// Show LSP references in the Search sidebar (reusing its result list).
    fn show_references(&mut self, refs: Vec<lsp::client::Target>) -> Task<Message> {
        let hits: Vec<SearchHit> = refs
            .into_iter()
            .take(search::MAX_HITS)
            .map(|t| {
                let rel = self.rel_of(&t.path);
                let preview = std::fs::read_to_string(&t.path)
                    .ok()
                    .and_then(|s| s.lines().nth(t.line).map(|l| l.trim().to_string()))
                    .unwrap_or_default();
                SearchHit {
                    abs: t.path,
                    rel,
                    line: t.line + 1,
                    preview,
                }
            })
            .collect();
        self.search.query = "(references)".to_string();
        self.search.ran = true;
        self.search.running = false;
        self.search.hits = hits;
        self.sidebar = SidebarTab::Search;
        self.code_focused = false;
        Task::none()
    }

    /// Run a navigation request from the active pane's cursor.
    fn goto_at_cursor(&mut self, kind: GotoKind) -> Task<Message> {
        let pane = self.active;
        let Some((line, col)) = self.active_viewer().and_then(|v| v.caret) else {
            return Task::none();
        };
        self.goto_request(pane, line, col, kind)
    }

    /// Dispatch an LSP navigation request (definition / references / …) at a
    /// clicked or cursor position.
    fn goto_request(
        &mut self,
        pane: usize,
        line: usize,
        col: usize,
        kind: GotoKind,
    ) -> Task<Message> {
        // Pull everything we need from the viewer before mutating self.
        let Some((lang, path, source_line)) =
            self.panes.get(pane).and_then(Option::as_ref).and_then(|v| {
                v.lang_key
                    .map(|l| (l, v.abs.clone(), v.source_line(line).unwrap_or("").to_string()))
            })
        else {
            return Task::none();
        };

        let client = match self.lsp.get(lang) {
            Some(LspSlot::Ready(c)) => c.clone(),
            _ => {
                self.status = format!("No {lang} server ready (⌘T to search symbols)");
                return Task::none();
            }
        };
        let utf16 = client.encoding == lsp::client::PositionEncoding::Utf16;
        let character = viewer::character_offset(&source_line, col, utf16);
        self.status = format!("{}…", kind.verb());
        let is_references = matches!(kind, GotoKind::References);
        Task::perform(
            async move { client.navigate(kind.method(), &path, line, character).await },
            move |result| {
                if is_references {
                    Message::ReferencesResult { result }
                } else {
                    Message::DefinitionResult { result }
                }
            },
        )
    }

    /// Prepare a call hierarchy at a (display line, col) in `pane`, gated on the
    /// server actually supporting it. Shared by `gc` and the context menu.
    fn call_hierarchy_at(&mut self, pane: usize, line: usize, col: usize) -> Task<Message> {
        let Some((lang, path, source_line)) =
            self.panes.get(pane).and_then(Option::as_ref).and_then(|v| {
                v.lang_key
                    .map(|l| (l, v.abs.clone(), v.source_line(line).unwrap_or("").to_string()))
            })
        else {
            return Task::none();
        };
        let client = match self.lsp.get(lang) {
            Some(LspSlot::Ready(c)) => c.clone(),
            _ => {
                self.status = format!("No {lang} server ready");
                return Task::none();
            }
        };
        if !client.call_hierarchy {
            self.status = format!("Call hierarchy isn't supported for {lang}");
            return Task::none();
        }
        let utf16 = client.encoding == lsp::client::PositionEncoding::Utf16;
        let character = viewer::character_offset(&source_line, col, utf16);
        self.status = "Building call hierarchy…".into();
        let direction = callgraph::Direction::Incoming;
        Task::perform(
            async move { client.prepare_call_hierarchy(&path, line, character).await },
            move |items| Message::CallHierarchyPrepared { direction, lang, items },
        )
    }

    /// Mark a node loading, then kick its fetch — the panel shows a spinner in
    /// the gap before the children arrive.
    fn fetch_children(&mut self, id: usize) -> Task<Message> {
        if let Some(t) = &mut self.call_graph {
            t.set_loading(id);
        }
        self.call_fetch_task(id)
    }

    /// Off-thread fetch of a call-tree node's callers/callees (direction from
    /// the tree), delivered as `CallHierarchyChildren`.
    fn call_fetch_task(&self, id: usize) -> Task<Message> {
        let Some(tree) = &self.call_graph else {
            return Task::none();
        };
        let client = match self.lsp.get(tree.lang) {
            Some(LspSlot::Ready(c)) => c.clone(),
            _ => return Task::none(),
        };
        let raw = tree.raw_of(id);
        let direction = tree.direction;
        Task::perform(
            async move {
                match direction {
                    callgraph::Direction::Incoming => client.incoming_calls(raw).await,
                    callgraph::Direction::Outgoing => client.outgoing_calls(raw).await,
                }
            },
            move |items| Message::CallHierarchyChildren { id, items },
        )
    }

    /// Re-flatten the per-file symbol map into `symbol_index` and refresh the
    /// finder when it is showing symbols.
    fn rebuild_symbol_index(&mut self) {
        self.symbol_index = Arc::new(index::flatten(&self.symbol_index_by_file));
        if self.finder.open && self.finder.mode == FinderMode::Symbols {
            self.finder.refresh_symbols(&self.symbol_index);
        }
    }

    /// Kick an off-thread (re)build of the project call graph from the current
    /// symbol index + file contents. Delivered as `ProjectCallsBuilt`.
    /// Apply an event from the clew-server. Backend flows are handled here as
    /// they migrate onto the protocol; for now it's just the handshake.
    fn handle_server_event(&mut self, event: clew_protocol::Event) {
        use clew_protocol::Event;
        match event {
            Event::Ready { .. } => {
                // The protocol handshake is internal — don't surface version
                // jargon in the status bar. Stay quiet until there's something
                // to say (a scan, a file count).
                self.status.clear();
            }
            Event::Error { message } => {
                // A failed folder listing stops the picker's spinner in place.
                if let Some(ConnectStage::Browsing(b)) =
                    self.connect.as_mut().map(|u| &mut u.stage)
                {
                    b.loading = false;
                }
                self.status = message;
            }
            Event::ChatDelta { stream, text } => {
                if let Some(tx) = self.chat_streams.lock().unwrap().get(&stream) {
                    let _ = tx.send(ChatStreamPiece::Delta(text));
                }
            }
            Event::ChatStreamDone { stream, error } => {
                if let Some(tx) = self.chat_streams.lock().unwrap().remove(&stream) {
                    let _ = tx.send(ChatStreamPiece::Done(error));
                }
            }
            Event::Docs { files } => {
                self.docs.files = files;
                self.docs.loading = false;
                // Resolve a "View docs" that was waiting on the index.
                if let Some(name) = self.docs.pending_view.take() {
                    match find_doc_by_name(&self.docs.files, &name) {
                        Some((rel, line)) => self.open_doc_page(&rel, line),
                        None => self.status = format!("No docs for “{name}”"),
                    }
                }
            }
            Event::DirListing {
                path,
                parent,
                entries,
            } => {
                // Fill the remote folder picker with this directory's contents.
                if let Some(ConnectStage::Browsing(b)) =
                    self.connect.as_mut().map(|u| &mut u.stage)
                {
                    b.cwd = path;
                    b.parent = parent;
                    b.entries = entries;
                    b.loading = false;
                }
            }
            Event::SearchResults { hits, error } => {
                // Rebuild absolute paths from the project root; the wire carries
                // only root-relative paths (meaningful across a remote server).
                let root = self.project.as_ref().map(|p| p.root.clone());
                let Some(root) = root else { return };
                let hits = hits
                    .into_iter()
                    .map(|h| search::SearchHit {
                        abs: root.join(&h.rel),
                        rel: h.rel,
                        line: h.line,
                        preview: h.preview,
                    })
                    .collect();
                self.apply_search_result(search::SearchResult { hits, error });
            }
            Event::GitInfo { rel, info } => {
                let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
                    return;
                };
                let abs = root.join(&rel);
                let info = info.map(Arc::new);
                for slot in &mut self.panes {
                    if let Some(v) = slot
                        && v.abs == abs
                    {
                        v.git = info.clone();
                    }
                }
            }
            Event::FilesChanged { rels } => {
                // The server's watcher reports on-disk changes.
                let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
                    return;
                };
                let open: HashSet<PathBuf> =
                    self.panes.iter().flatten().map(|v| v.abs.clone()).collect();
                let spec = self.target_spec();
                let mut index_dirty = false;
                for rel in &rels {
                    let abs = root.join(rel);
                    // Re-request an open file so its view reloads in place.
                    if open.contains(&abs)
                        && let Some(tx) = self.server_tx.clone()
                    {
                        let id = self.next_req_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let request = clew_protocol::Request::ReadFile {
                            rel: rel.clone(),
                            target: spec.clone(),
                        };
                        if tx
                            .send(clew_protocol::ClientMessage { id, request })
                            .is_ok()
                        {
                            self.pending_reads.insert(id, ReadKind::Refresh);
                        }
                    }
                    // Keep the project symbol index (Cmd+T) fresh. The index is
                    // still client-side, so it re-reads locally — as it already
                    // does when built on open; this moves to the server with the
                    // index flow.
                    if let Some(lang) = highlight::detect(&abs) {
                        match std::fs::read_to_string(&abs) {
                            Ok(content) => {
                                let syms = index::file_symbols(&abs, rel, &content, lang);
                                self.symbol_index_by_file.insert(abs, syms);
                                index_dirty = true;
                            }
                            Err(_) => {
                                index_dirty |= self.symbol_index_by_file.remove(&abs).is_some();
                            }
                        }
                    }
                }
                if index_dirty {
                    self.rebuild_symbol_index();
                }
                // Keep the API docs fresh while their tab is open.
                if self.sidebar == SidebarTab::Docs && !self.docs.loading {
                    self.request_docs();
                }
            }
            Event::Tree { tree, files, .. } => {
                // A structural change (create/delete) from the watcher: swap the
                // tree in place, keeping panes / scroll / everything else.
                if let Some(project) = &mut self.project {
                    let root = project.root.clone();
                    project.tree = tree;
                    project.files = Arc::new(
                        files
                            .into_iter()
                            .map(|rel| fs_scan::FileEntry {
                                abs: root.join(&rel),
                                rel,
                            })
                            .collect(),
                    );
                }
            }
            Event::ProcessOutput { proc, data } => {
                // Feed a proxied process's stdout into its LspClient bridge.
                if let Some(feed) = self.proc_feeds.get(&proc) {
                    let _ = feed.send(data);
                }
            }
            Event::ProcessExited { proc, .. } => {
                // Dropping the feed closes the bridge, so the LspClient sees EOF.
                self.proc_feeds.remove(&proc);
                self.lsp_procs.retain(|_, p| *p != proc);
            }
            // Other flows (Outline, …) handled here as they migrate.
            _ => {}
        }
    }

    /// Route a correlated server reply. `FileContent` needs the request id to
    /// find which pane asked for it; everything else is id-agnostic.
    /// An AI router for background tasks. Endpoint is Server (matching the Hello
    /// handshake); with no server channel it transparently runs calls locally.
    fn ai_client(&self) -> AiClient {
        AiClient {
            endpoint: clew_protocol::AiEndpoint::Server,
            server_tx: self.server_tx.clone(),
            next_id: self.next_req_id.clone(),
            pending: self.ai_pending.clone(),
        }
    }

    /// Hand the server the current AI provider config so it can make calls.
    fn send_ai_config(&self) {
        let Some(tx) = &self.server_tx else { return };
        let chat = llm::Config::load().map(|c| clew_protocol::AiChatConfig {
            provider: c.provider.slug().to_string(),
            api_key: c.api_key,
            model: c.model,
            base_url: c.base_url,
        });
        let embed = embed::Config::load().map(|c| clew_protocol::AiEmbedConfig {
            api_key: c.api_key,
            model: c.model,
            base_url: c.base_url,
        });
        if chat.is_some() || embed.is_some() {
            let _ = tx.send(clew_protocol::ClientMessage {
                id: 0,
                request: clew_protocol::Request::SetAiConfig { chat, embed },
            });
        }
    }

    fn handle_server_reply(&mut self, id: u64, event: clew_protocol::Event) -> Task<Message> {
        // An AI RPC reply: hand the event to the task awaiting it.
        if let Some(otx) = self.ai_pending.lock().unwrap().remove(&id) {
            let result = match event {
                clew_protocol::Event::Error { message } => Err(message),
                other => Ok(other),
            };
            let _ = otx.send(result);
            return Task::none();
        }
        match event {
            clew_protocol::Event::FileContent {
                rel,
                source,
                lines,
                symbols,
                docs,
                inactive,
            } => match self.pending_reads.remove(&id) {
                Some(ReadKind::Open { pane, target }) => {
                    self.apply_file_content(pane, target, rel, source, lines, symbols, docs, inactive)
                }
                Some(ReadKind::Refresh) => {
                    self.apply_file_refresh(rel, source, lines, symbols, docs, inactive)
                }
                None => Task::none(),
            },
            clew_protocol::Event::Tree {
                tree,
                files,
                truncated,
            } => {
                // Only build the project while we're opening one; a Tree that
                // arrives otherwise is a catch-up OpenProject reply (after a
                // local-fallback open) and must not re-open the project.
                if !self.scanning {
                    return Task::none();
                }
                let Some(root) = self.pending_scan_root.take() else {
                    return Task::none();
                };
                let files = files
                    .into_iter()
                    .map(|rel| fs_scan::FileEntry {
                        abs: root.join(&rel),
                        rel,
                    })
                    .collect();
                self.on_scan_done(ScanResult {
                    root,
                    tree,
                    files,
                    truncated,
                })
            }
            other => {
                self.handle_server_event(other);
                Task::none()
            }
        }
    }

    /// Build the viewer from a clew-server `FileContent` reply — the server-side
    /// equivalent of `on_file_loaded` + `Highlighted` in one step (content
    /// arrives already highlighted, so there is no plain phase or flash).
    #[allow(clippy::too_many_arguments)]
    fn apply_file_content(
        &mut self,
        pane: usize,
        target: Option<usize>,
        rel: String,
        source: String,
        lines: Vec<HlLine>,
        symbols: Vec<Symbol>,
        docs: Vec<(usize, String)>,
        inactive: Vec<usize>,
    ) -> Task<Message> {
        let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
            return Task::none();
        };
        // Opening a file leaves the doc page (and the overview/stats homes).
        self.docs.page = None;
        let abs = root.join(&rel);
        let git_rel = rel.clone();
        let lang_key = highlight::detect(&abs);
        let source = Arc::new(source);
        let line_height = self.line_height();
        let old_viewport = self
            .panes
            .get(pane)
            .and_then(|s| s.as_ref())
            .map(|v| v.viewport_h);

        let mut v = Viewer::new(abs.clone(), rel, lang_key, source.clone(), lines);
        v.symbols = symbols;
        v.docs = docs.into_iter().collect();
        v.inactive_lines = inactive.into_iter().collect();
        v.highlighted = true;
        if let Some(h) = old_viewport {
            v.viewport_h = h;
        }
        v.target_line = target;
        v.caret = Some((target.map(|t| t.saturating_sub(1)).unwrap_or(0), 0));
        let y = v.scroll_offset_for(target, line_height);
        v.scroll_y = y;
        self.status = v.rel.clone();
        self.panes[pane] = Some(v);
        // Seed the content hash so the watcher can tell real edits from noise.
        self.registry
            .set(abs.clone(), incremental::content_hash(source.as_bytes()));
        if pane == self.active {
            self.refresh_import_tree();
        }

        let scroll = operation::scroll_to(ui::code_scroll_id(pane), AbsoluteOffset { x: 0.0, y });
        let lsp_task = match lang_key {
            Some(lang) => self.ensure_lsp(lang),
            None => Task::none(),
        };
        // Ask the server for git blame; it fills in asynchronously via
        // Event::GitInfo, routed back to this file by rel.
        if let Some(tx) = self.server_tx.clone() {
            let id = self.next_req_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let request = clew_protocol::Request::GitInfo { rel: git_rel };
            let _ = tx.send(clew_protocol::ClientMessage { id, request });
        }
        self.follow_caret(Task::batch([scroll, lsp_task]))
    }

    /// The current reading target in its protocol wire form.
    fn target_spec(&self) -> clew_protocol::TargetSpec {
        clew_protocol::TargetSpec {
            label: self.reading_target.label.clone(),
            os: self.reading_target.os.clone(),
            arch: self.reading_target.arch.clone(),
            family: self.reading_target.family.clone(),
        }
    }

    /// Reload every pane showing `rel` in place after an on-disk change, keeping
    /// scroll / caret / folds (unlike opening, which jumps to a target line).
    #[allow(clippy::too_many_arguments)]
    fn apply_file_refresh(
        &mut self,
        rel: String,
        source: String,
        lines: Vec<HlLine>,
        symbols: Vec<Symbol>,
        docs: Vec<(usize, String)>,
        inactive: Vec<usize>,
    ) -> Task<Message> {
        let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
            return Task::none();
        };
        let abs = root.join(&rel);
        let source = Arc::new(source);
        let docs: HashMap<usize, String> = docs.into_iter().collect();
        let inactive: HashSet<usize> = inactive.into_iter().collect();
        for slot in &mut self.panes {
            if let Some(v) = slot
                && v.abs == abs
            {
                // Keeps scroll / caret / collapsed folds; then restore the
                // highlighting bundle the reload cleared.
                v.reload(source.clone(), lines.clone());
                v.symbols = symbols.clone();
                v.docs = docs.clone();
                v.inactive_lines = inactive.clone();
                v.highlighted = true;
            }
        }
        // Track the new bytes so the next change is detected against them.
        self.registry
            .set(abs, incremental::content_hash(source.as_bytes()));
        self.follow_caret(Task::none())
    }

    /// Kick off a stats computation off the UI thread when it's stale (or
    /// `force`d). Single-flight: never launches a second run while one is in
    /// flight. Stamps `stats_rev` with the registry revision so a later file
    /// change (which bumps the revision) marks the result stale.
    fn start_stats(&mut self, force: bool) -> Task<Message> {
        let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
            return Task::none();
        };
        let rev = self.registry.revision();
        let fresh = self.stats.is_some() && self.stats_rev == rev;
        if self.building_stats || (!force && fresh) {
            return Task::none();
        }
        self.building_stats = true;
        self.stats_rev = rev;
        if self.stats.is_none() {
            self.status = "Computing code statistics…".into();
        }
        let compute_root = root.clone();
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || stats::compute(&compute_root))
                    .await
                    .unwrap_or_default()
            },
            move |report| Message::StatsDone { root: root.clone(), rev, report },
        )
    }

    fn build_project_calls(&mut self) -> Task<Message> {
        let Some(project) = &self.project else {
            return Task::none();
        };
        // Callable definitions to link against, from the symbol index.
        let defs: Vec<projectcalls::Def> = self
            .symbol_index_by_file
            .values()
            .flatten()
            .map(|s| projectcalls::Def {
                name: s.name.clone(),
                kind: s.kind.clone(),
                file: s.abs.clone(),
                line: s.line,
            })
            .collect();
        let files: Vec<PathBuf> = project.files.iter().map(|f| f.abs.clone()).collect();
        let tag_root = project.root.clone();
        // Import scope: each file → the internal files it imports, so a called
        // name resolves to the definition actually in scope.
        let scope = self.import_graph.scope_map();
        self.project_calls_rev = self.registry.revision();
        self.building_calls = true;
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    // Read the current source of each supported, reasonably sized
                    // file (a big file's calls aren't worth the parse cost).
                    let sources: Vec<(PathBuf, String)> = files
                        .into_iter()
                        .filter(|f| highlight::detect(f).is_some())
                        .filter(|f| {
                            std::fs::metadata(f)
                                .map(|m| m.len() <= index::MAX_INDEX_FILE_BYTES)
                                .unwrap_or(false)
                        })
                        .filter_map(|f| std::fs::read_to_string(&f).ok().map(|c| (f, c)))
                        .collect();
                    projectcalls::ProjectCallGraph::build(defs, &sources, &scope)
                })
                .await
                .unwrap_or_default()
            },
            move |graph| Message::ProjectCallsBuilt {
                root: tag_root.clone(),
                graph,
            },
        )
    }

    /// Ensure the project call graph is available for the Explain panel's
    /// call-flow strip, building it in the background if it's empty or stale
    /// (single-flight). No-op when one is already in flight or the graph is
    /// current — so it's cheap to call as the cursor moves between functions.
    fn ensure_call_graph(&mut self) -> Task<Message> {
        if self.building_calls
            || (!self.project_calls.is_empty()
                && self.project_calls_rev == self.registry.revision())
        {
            return Task::none();
        }
        self.build_project_calls()
    }

    /// Ready, call-hierarchy-capable servers keyed by language.
    fn call_hierarchy_clients(&self) -> HashMap<String, lsp::client::LspClient> {
        let mut clients = HashMap::new();
        for (lang, slot) in &self.lsp {
            if let LspSlot::Ready(c) = slot
                && c.call_hierarchy
            {
                clients.insert(lang.clone(), c.clone());
            }
        }
        clients
    }

    /// Every callable function whose language has a ready call-hierarchy server.
    fn refinable_defs(&self, clients: &HashMap<String, lsp::client::LspClient>) -> Vec<projectcalls::Def> {
        let all: Vec<projectcalls::Def> = self
            .symbol_index_by_file
            .values()
            .flatten()
            .map(|s| projectcalls::Def {
                name: s.name.clone(),
                kind: s.kind.clone(),
                file: s.abs.clone(),
                line: s.line,
            })
            .collect();
        projectcalls::ProjectCallGraph::callable(&all)
            .into_iter()
            .filter(|d| highlight::detect(&d.file).is_some_and(|l| clients.contains_key(l)))
            .collect()
    }

    /// Full LSP refine (the "Refine with LSP" button): query every project
    /// function and rebuild the precise graph from scratch.
    fn refine_project_calls(&mut self) -> Task<Message> {
        let clients = self.call_hierarchy_clients();
        if clients.is_empty() {
            self.status =
                "No language server ready — open a file to start one, then retry".into();
            return Task::none();
        }
        let all = self.refinable_defs(&clients);
        if all.is_empty() {
            self.status = "No functions to refine for the ready server(s)".into();
            return Task::none();
        }
        self.spawn_refine(clients, all.clone(), all, projectcalls::SymEdges::default(), None)
    }

    /// Incrementally refresh the precise graph after files changed: re-query only
    /// the changed files' functions and patch the edge set.
    fn refine_incremental(&mut self, changed: HashSet<PathBuf>) -> Task<Message> {
        let clients = self.call_hierarchy_clients();
        if clients.is_empty() {
            return Task::none();
        }
        let all = self.refinable_defs(&clients);
        let query: Vec<projectcalls::Def> =
            all.iter().filter(|d| changed.contains(&d.file)).cloned().collect();
        let base = self.precise_edges.clone();
        // Even with nothing to re-query (e.g. all changed functions removed), we
        // still rebuild so deleted files' edges drop out.
        self.spawn_refine(clients, all, query, base, Some(changed))
    }

    /// Shared refine launcher. `query_defs` are LSP-queried; `all_defs` is the
    /// full node set the result maps onto; `base` is the starting edge set;
    /// `changed` (when incremental) is the files whose old edges to drop before
    /// re-querying, and also selects incoming+outgoing (vs incoming-only) queries.
    fn spawn_refine(
        &mut self,
        clients: HashMap<String, lsp::client::LspClient>,
        all_defs: Vec<projectcalls::Def>,
        query_defs: Vec<projectcalls::Def>,
        base: projectcalls::SymEdges,
        changed: Option<HashSet<PathBuf>>,
    ) -> Task<Message> {
        let Some(project) = &self.project else {
            return Task::none();
        };
        let root = project.root.clone();
        self.calls_gen += 1;
        let generation = self.calls_gen;
        self.refine_progress = Some((0, query_defs.len()));
        if changed.is_none() {
            self.status = format!("Refining {} functions with LSP…", query_defs.len());
        }
        let stream = iced::stream::channel(256, move |output| {
            refine_stream(output, all_defs, query_defs, base, changed, clients, root, generation)
        });
        Task::run(stream, |m| m)
    }

    /// The explanation target for the active pane's caret: the innermost
    /// function/method it sits in, or the file itself when it's between
    /// functions. Drives the always-on explanation panel.
    fn cursor_target(&self) -> Option<explain::Node> {
        let v = self.active_viewer()?;
        let line1 = v.caret.map(|(l, _)| l + 1)?;
        let name = v
            .symbols
            .iter()
            .filter(|s| matches!(s.kind.as_str(), "function" | "method"))
            .filter(|s| s.line <= line1 && line1 <= s.end_line)
            .min_by_key(|s| s.end_line.saturating_sub(s.line))
            .map(|s| s.name.clone());
        Some(match name {
            Some(name) => explain::Node::Function { file: v.abs.clone(), name },
            None => explain::Node::File(v.abs.clone()),
        })
    }

    /// Context-aware starter questions for the Ask panel, most specific first:
    /// about any pinned selection, the symbol/file under the cursor, then the
    /// codebase. Static templates — instant and free.
    pub fn suggested_questions(&self) -> Vec<String> {
        let mut qs: Vec<String> = Vec::new();
        if !self.ask_pins.is_empty() {
            qs.push("Explain the attached code.".into());
            qs.push("Why is the attached code written this way?".into());
        }
        match self.cursor_target() {
            Some(explain::Node::Function { name, .. }) => {
                qs.push(format!("What calls `{name}`?"));
                qs.push(format!("What are the edge cases in `{name}`?"));
                qs.push(format!("How does `{name}` handle errors?"));
            }
            Some(explain::Node::File(p)) => {
                let f = p.file_name().and_then(|s| s.to_str()).unwrap_or("this file");
                qs.push(format!("What is the role of `{f}`?"));
                qs.push(format!("What are the key types in `{f}`?"));
            }
            _ => {}
        }
        qs.push("What is the entry point of this codebase?".into());
        qs.push("How does data flow through the app?".into());
        qs.truncate(4);
        qs
    }

    /// Point the explanation panel at the function/file under the caret. No-op if
    /// it already shows that target (so moving within one function is free).
    /// `extra` is the caller's own task (e.g. a scroll), run alongside.
    fn follow_caret(&mut self, extra: Task<Message>) -> Task<Message> {
        let Some(target) = self.cursor_target() else {
            return extra;
        };
        if self.explain_view.as_ref() == Some(&target) {
            return extra;
        }
        Task::batch([extra, self.show_explanation(target)])
    }

    /// Show the pre-built explanation for the innermost function/method whose
    /// span contains `line1` (1-based) in `file`. Used by the Outline Cmd+click
    /// and the code context menu. Everything is explained at project startup, so
    /// this is a pure show — no on-demand generation.
    fn explain_symbol_at(&mut self, file: PathBuf, line1: usize) -> Task<Message> {
        self.show_right_panel = true; // explicit action → reveal the panel
        let name = self.panes.iter().flatten().find(|v| v.abs == file).and_then(|v| {
            v.symbols
                .iter()
                .filter(|s| matches!(s.kind.as_str(), "function" | "method"))
                .filter(|s| s.line <= line1 && line1 <= s.end_line)
                .min_by_key(|s| s.end_line.saturating_sub(s.line)) // innermost span
                .map(|s| s.name.clone())
        });
        match name {
            Some(name) => {
                let node = explain::Node::Function { file, name };
                // Reveal the panel now (a cached summary, or the placeholder),
                // then generate the block walkthrough. Without the second step a
                // menu / Cmd+click "Explain" just parked the panel on "Not
                // explained yet" and looked dead; ExplainBlocks shows a cached
                // walkthrough if present, else streams a fresh one.
                let show = self.show_explanation(node.clone());
                Task::batch([show, Task::done(Message::ExplainBlocks(node))])
            }
            None => {
                self.status = "No function here to explain".into();
                Task::none()
            }
        }
    }

    /// Open the explanation overlay for `node`, showing its summary.
    fn show_explanation(&mut self, node: explain::Node) -> Task<Message> {
        let summary = self
            .explanations
            .get(&node)
            .map(|c| c.summary.clone())
            .unwrap_or_else(|| "Not explained yet — press Explain in the toolbar.".to_string());
        self.present(node, &summary, false)
    }

    /// Show a function's block-by-block walkthrough (`detail`) in the overlay.
    fn show_detail(&mut self, node: explain::Node, detail: String) -> Task<Message> {
        self.present(node, &detail, true)
    }

    /// Prepare `content` (an LLM markdown string) into ordered segments — markdown
    /// pre-parsed, math/mermaid keyed — load any already-rendered SVGs from the
    /// session/disk cache, and kick off a background pass to render the rest.
    fn present(&mut self, node: explain::Node, content: &str, detail: bool) -> Task<Message> {
        let (prepared, task) = self.prepare_segments(content);
        self.explain_prepared = prepared;
        self.explain_view = Some(node);
        self.explain_showing_detail = detail;
        // The call-flow strip needs the project call graph; build it lazily while
        // the reader is actually looking at a function in the context panel.
        let build = if self.show_right_panel
            && matches!(self.explain_view, Some(explain::Node::Function { .. }))
        {
            self.ensure_call_graph()
        } else {
            Task::none()
        };
        Task::batch([task, build])
    }

    /// Follow the reading cursor: keep the context panel showing the function
    /// (or, between functions, the file) the caret is in. A cheap no-op when the
    /// panel is closed or the enclosing symbol hasn't changed, so it is safe to
    /// call on every caret move. Never opens the panel on its own — that stays a
    /// deliberate act (toggle, or Cmd+click to explain).
    fn sync_reading_context(&mut self) -> Task<Message> {
        if !self.show_right_panel || self.split {
            return Task::none();
        }
        let Some(v) = self.active_viewer() else {
            return Task::none();
        };
        let abs = v.abs.clone();
        let Some((line0, _)) = v.caret else {
            return Task::none();
        };
        let line1 = line0 + 1;
        // Innermost function/method whose span contains the caret; else the file.
        let target = v
            .symbols
            .iter()
            .filter(|s| matches!(s.kind.as_str(), "function" | "method"))
            .filter(|s| s.line <= line1 && line1 <= s.end_line)
            .min_by_key(|s| s.end_line.saturating_sub(s.line))
            .map(|s| explain::Node::Function { file: abs.clone(), name: s.name.clone() })
            .unwrap_or(explain::Node::File(abs));
        if self.explain_view.as_ref() == Some(&target) {
            return Task::none();
        }
        let show = self.show_explanation(target);
        Task::batch([show, self.outline_scroll_task()])
    }

    /// Scroll the outline so the caret's current symbol is in view (approximate —
    /// row heights are estimated — which is enough to bring it on screen). A no-op
    /// unless the caret is inside a function shown in the outline.
    fn outline_scroll_task(&self) -> Task<Message> {
        let Some(v) = self.active_viewer() else {
            return Task::none();
        };
        let name = match &self.explain_view {
            Some(explain::Node::Function { file, name }) if *file == v.abs => name.clone(),
            _ => return Task::none(),
        };
        let mut y = 0.0f32;
        let mut found = false;
        for s in &v.symbols {
            if matches!(s.kind.as_str(), "function" | "method") && s.name == name {
                found = true;
                break;
            }
            // Mirror ui::outline_content's row layout: a label line, plus a summary
            // line when inline summaries are on and this symbol has a real one.
            let mut h = 27.0;
            let has_summary = self.show_inline_summaries
                && matches!(s.kind.as_str(), "function" | "method")
                && self
                    .explanations
                    .get(&explain::Node::Function { file: v.abs.clone(), name: s.name.clone() })
                    .is_some_and(|c| !explain::is_error_summary(&c.summary));
            if has_summary {
                h += 14.0;
            }
            y += h;
        }
        if !found {
            return Task::none();
        }
        let y = (y - 48.0).max(0.0); // keep a little context above the symbol
        operation::scroll_to(ui::outline_scroll_id(), AbsoluteOffset { x: 0.0, y })
    }

    /// Segment `content` (LLM markdown) for display: parse markdown, key the
    /// math/mermaid, load cached SVGs, and return a background task to render the
    /// rest. Shared by the explanation panel and the architecture overview.
    fn prepare_segments(&mut self, content: &str) -> (Vec<PreparedSeg>, Task<Message>) {
        let segments = richmd::segment(content);
        let root = self.project.as_ref().map(|p| p.root.clone());

        // Pull cached SVGs into memory; collect what still needs rendering.
        let mut missing: Vec<richmd::Renderable> = Vec::new();
        for r in richmd::renderables(&segments) {
            if self.explain_svgs.contains_key(&r.key) {
                continue;
            }
            let cached = root.as_ref().and_then(|rt| richmd::load_raw(rt, r.key));
            if let Some(raw) = cached {
                self.insert_svg(r.key, richmd::prepare_svg(&raw, r.kind == "math"));
            } else {
                missing.push(r);
            }
        }

        // Prepare segments for display (parse markdown once).
        let prepared = segments
            .into_iter()
            .map(|s| match s {
                richmd::Segment::Markdown(md) => {
                    PreparedSeg::Markdown(iced::widget::markdown::parse(&md).collect())
                }
                richmd::Segment::DisplayMath(tex) => {
                    PreparedSeg::DisplayMath(richmd::math_key(&tex, true))
                }
                richmd::Segment::Mermaid(src) => {
                    PreparedSeg::Mermaid(richmd::mermaid_key(&src), src)
                }
                richmd::Segment::InlineLine(parts) => PreparedSeg::InlineLine(
                    parts
                        .into_iter()
                        .map(|p| match p {
                            richmd::Inline::Text(t) => PreparedInline::Text(t),
                            richmd::Inline::Math(tex) => {
                                PreparedInline::Math(richmd::math_key(&tex, false))
                            }
                        })
                        .collect(),
                ),
            })
            .collect();

        // Render any missing diagrams/equations in the background.
        let task = match root {
            Some(root) if !missing.is_empty() => {
                self.explain_svg_gen += 1;
                let generation = self.explain_svg_gen;
                self.status = "Rendering math & diagrams…".into();
                Task::perform(
                    async move {
                        tokio::task::spawn_blocking(move || generate_svgs(missing, root))
                            .await
                            .unwrap_or_default()
                    },
                    move |map| Message::SvgsGenerated { generation, map },
                )
            }
            _ => Task::none(),
        };
        (prepared, task)
    }

    /// Insert a prepared SVG into the session cache, building its iced handle.
    fn insert_svg(&mut self, key: u64, prepared: richmd::PreparedSvg) {
        self.explain_svgs.insert(
            key,
            ExplainSvg {
                handle: iced::widget::svg::Handle::from_memory(prepared.svg.into_bytes()),
                width: prepared.width,
                height: prepared.height,
            },
        );
    }

    /// Assemble the overview prompt inputs from clew's existing artifacts:
    /// folder/file summaries (the explanation cache), entry points and key types
    /// (the symbol index), and a computed module-dependency diagram (imports).
    fn gather_overview_inputs(&self) -> overview::Inputs {
        let root = self.project.as_ref().map(|p| p.root.clone()).unwrap_or_default();
        let project_name =
            root.file_name().and_then(|s| s.to_str()).unwrap_or("project").to_string();

        // Structure: folders then files, each with its summary (rel paths so the
        // model can link them).
        let mut folders: Vec<(String, String)> = Vec::new();
        let mut files: Vec<(String, String)> = Vec::new();
        for (node, cached) in &self.explanations {
            match node {
                explain::Node::Folder(p) => folders.push((self.rel_of(p), cached.summary.clone())),
                explain::Node::File(p) => files.push((self.rel_of(p), cached.summary.clone())),
                explain::Node::Function { .. } => {}
            }
        }
        folders.sort();
        files.sort();
        let mut structure = String::new();
        for (rel, sum) in &folders {
            structure.push_str(&format!("📁 {rel} — {sum}\n"));
        }
        if !folders.is_empty() {
            structure.push('\n');
        }
        for (rel, sum) in &files {
            structure.push_str(&format!("{rel} — {sum}\n"));
        }

        // Entry points: functions named `main`.
        let mut entry_points: Vec<String> = self
            .symbol_index_by_file
            .values()
            .flatten()
            .filter(|s| s.kind == "function" && s.name == "main")
            .map(|s| format!("`fn main` in {}", s.rel))
            .collect();
        entry_points.sort();
        entry_points.dedup();

        // Key types: struct/enum/class/trait symbols (capped, deterministic).
        let mut all_types: Vec<&SymbolEntry> = self
            .symbol_index_by_file
            .values()
            .flatten()
            .filter(|s| matches!(s.kind.as_str(), "struct" | "enum" | "class" | "trait" | "interface"))
            .collect();
        all_types.sort_by(|a, b| a.name.cmp(&b.name).then(a.rel.cmp(&b.rel)));
        let mut seen = HashSet::new();
        let key_types: Vec<String> = all_types
            .into_iter()
            .filter(|s| seen.insert(s.name.clone()))
            .take(24)
            .map(|s| format!("`{}` ({})", s.name, s.rel))
            .collect();

        overview::Inputs { project_name, structure, entry_points, key_types }
    }

    /// Context for the walkthrough planner: the structure + summaries (reused
    /// from the overview inputs) plus the real symbols per file, which the tour
    /// must anchor to (so it can't invent locations).
    fn gather_walkthrough_context(&self) -> String {
        let inputs = self.gather_overview_inputs();
        let mut c = String::new();
        c.push_str("Structure (files, each with a short summary of its role):\n");
        c.push_str(&inputs.structure);
        if !inputs.entry_points.is_empty() {
            c.push_str("\nEntry points:\n");
            for e in &inputs.entry_points {
                c.push_str(&format!("- {e}\n"));
            }
        }
        c.push_str("\nSymbols per file — anchor steps to these exact paths and names:\n");
        let mut by_file: Vec<&PathBuf> = self.symbol_index_by_file.keys().collect();
        by_file.sort_by_key(|p| self.rel_of(p));
        for abs in by_file {
            let names: Vec<&str> = self.symbol_index_by_file[abs]
                .iter()
                .filter(|s| {
                    matches!(
                        s.kind.as_str(),
                        "function" | "method" | "struct" | "enum" | "class" | "trait" | "interface"
                    )
                })
                .map(|s| s.name.as_str())
                .take(40)
                .collect();
            if !names.is_empty() {
                c.push_str(&format!("{}: {}\n", self.rel_of(abs), names.join(", ")));
            }
        }
        c
    }

    /// Resolve a walkthrough step's relative path to an absolute project file.
    fn resolve_walk_file(&self, rel: &str) -> Option<PathBuf> {
        let rel = rel.trim().trim_start_matches("./");
        self.project
            .as_ref()?
            .files
            .iter()
            .find(|f| self.rel_of(&f.abs) == rel)
            .map(|f| f.abs.clone())
    }

    /// Navigate to walkthrough step `i`: open its file and jump to the symbol
    /// (resolved live against the index) or its fallback line.
    fn walkthrough_goto(&mut self, i: usize) -> Task<Message> {
        let Some(step) = self
            .walkthrough_open
            .and_then(|o| self.walkthroughs.get(o))
            .and_then(|w| w.steps.get(i))
            .cloned()
        else {
            return Task::none();
        };
        self.walkthrough_step = i;
        // Prepare the narration (markdown + any mermaid/math → SVG).
        let (prepared, render) = self.prepare_segments(&step.narration);
        self.walkthrough_prepared = prepared;
        let Some(abs) = self.resolve_walk_file(&step.file) else {
            return render;
        };
        let line = step
            .symbol
            .as_ref()
            .and_then(|name| {
                self.symbol_index_by_file
                    .get(&abs)
                    .and_then(|syms| syms.iter().find(|s| &s.name == name))
                    .map(|s| s.line)
            })
            .or(step.line)
            .unwrap_or(1);
        Task::batch([self.open_file(abs, Some(line), true), render])
    }

    /// The `(node, text-to-embed, hash)` set for the semantic index: every
    /// explained function/file, embedding its `name/path — summary` (folders are
    /// too coarse to be useful search hits).
    fn gather_embed_nodes(&self) -> Vec<(explain::Node, String, incremental::Version)> {
        self.explanations
            .iter()
            .filter_map(|(node, cached)| {
                let text = match node {
                    explain::Node::Function { file, name } => {
                        format!("{name} in {} — {}", self.rel_of(file), cached.summary)
                    }
                    explain::Node::File(p) => format!("{} — {}", self.rel_of(p), cached.summary),
                    explain::Node::Folder(_) => return None,
                };
                let hash = embed::text_hash(&text);
                Some((node.clone(), text, hash))
            })
            .collect()
    }

    /// Build the answer context for an Ask question: each retrieved node's
    /// summary and (for functions) its source, capped in total size.
    fn gather_ask_context(&self, nodes: &[explain::Node]) -> String {
        const CAP: usize = 18000;
        let empty: HashMap<String, Option<String>> = HashMap::new();
        let mut ctx = String::new();
        for node in nodes {
            if ctx.len() >= CAP {
                break;
            }
            match node {
                explain::Node::Function { file, name } => {
                    let summary = self.explanations.get(node).map(|c| c.summary.as_str()).unwrap_or("");
                    let body = gather_fn_detail_input(file.clone(), name, &empty)
                        .map(|(_, body, _)| body)
                        .unwrap_or_default();
                    // Include the line so the model can cite an accurate jump anchor.
                    let rel = self.rel_of(file);
                    let loc = match self
                        .symbol_index_by_file
                        .get(file)
                        .and_then(|syms| syms.iter().find(|s| &s.name == name))
                        .map(|s| s.line)
                    {
                        Some(line) => format!("{rel} (L{line})"),
                        None => rel,
                    };
                    ctx.push_str(&format!("### {name} — {loc}\n{summary}\n```\n{body}\n```\n\n"));
                }
                explain::Node::File(p) => {
                    let summary = self.explanations.get(node).map(|c| c.summary.as_str()).unwrap_or("");
                    ctx.push_str(&format!("### {} (file)\n{summary}\n\n", self.rel_of(p)));
                }
                explain::Node::Folder(_) => {}
            }
        }
        ctx
    }

    /// Cosine similarity of a node's indexed embedding to the query vector, or 0
    /// when the node isn't in the index (e.g. a cursor anchor not yet embedded).
    fn node_score(&self, node: &explain::Node, qvec: &[f32]) -> f32 {
        self.embed_index
            .entries
            .iter()
            .find(|e| &e.node == node)
            .map(|e| embed::cosine(qvec, &e.vec))
            .unwrap_or(0.0)
    }

    /// Capture a pane's current text selection as a pinnable Ask context block.
    fn selection_pin(&self, pane: usize) -> Option<AskPin> {
        let v = self.panes.get(pane).and_then(Option::as_ref)?;
        let code = v.selected_text()?;
        let ((start_line, _), _) = v.selection_ordered()?;
        Some(AskPin {
            rel: v.rel.clone(),
            file: v.abs.clone(),
            line: start_line + 1, // 0-based → 1-based
            code,
        })
    }

    /// Resolve an overview markdown link (a project-relative path, optionally with
    /// a `#Lnn` line suffix) to an absolute file + line. Falls back to matching by
    /// file name when the exact path doesn't exist.
    fn resolve_project_link(&self, url: &str) -> Option<(PathBuf, Option<usize>)> {
        let project = self.project.as_ref()?;
        let (path_part, frag) = match url.rsplit_once('#') {
            Some((p, frag)) => (p.trim(), Some(frag.trim())),
            None => (url.trim(), None),
        };
        if path_part.is_empty() {
            return None;
        }
        let candidate = project.root.join(path_part);
        let abs = if candidate.is_file() {
            candidate
        } else {
            let base = std::path::Path::new(path_part).file_name()?;
            project.files.iter().find(|f| f.abs.file_name() == Some(base))?.abs.clone()
        };
        // The fragment is a line number (`L68` / `68`), or a symbol name we
        // resolve to its line against the file's index (`#recompute`).
        let line = frag.and_then(|f| {
            f.trim_start_matches(['L', 'l']).parse::<usize>().ok().or_else(|| {
                self.symbol_index_by_file
                    .get(&abs)
                    .and_then(|syms| syms.iter().find(|s| s.name == f).map(|s| s.line))
            })
        });
        Some((abs, line))
    }

    /// Recompute the node-link layout for whichever overlay is open.
    fn refresh_graph_layout(&mut self) {
        self.graph_layout = match self.overlay {
            Some(Overlay::ProjectImports) => Some(self.import_graph_layout()),
            Some(Overlay::ProjectCalls) => Some(self.calls_graph_layout()),
            None => None,
        };
    }

    /// Force-directed layout of the import graph: nodes are files, sized by
    /// fan-in+fan-out, cycle members highlighted; edges are `use` dependencies.
    fn import_graph_layout(&self) -> graphlayout::Layout {
        let g = &self.import_graph;
        let files = g.files();
        let idx: HashMap<PathBuf, usize> =
            files.iter().cloned().enumerate().map(|(i, f)| (f, i)).collect();
        let cyclic: HashSet<PathBuf> = self.import_cycles.iter().flatten().cloned().collect();
        let nodes = files
            .iter()
            .map(|f| graphlayout::NodeInput {
                label: file_label(f),
                file: f.clone(),
                weight: (g.fan_in(f) + g.fan_out(f) + 1) as f32,
                cyclic: cyclic.contains(f),
            })
            .collect();
        let mut edge_set: HashSet<(usize, usize)> = HashSet::new();
        for f in &files {
            for e in g.imports(f) {
                if let imports::Target::Internal(t) = &e.target
                    && let (Some(&a), Some(&b)) = (idx.get(f), idx.get(t))
                {
                    edge_set.insert((a, b));
                }
            }
        }
        graphlayout::layout(nodes, edge_set.into_iter().collect())
    }

    /// Force-directed layout of the file-aggregated call graph: nodes are files
    /// sized by call degree; edges are cross-file call flow.
    fn calls_graph_layout(&self) -> graphlayout::Layout {
        let (files, edges) = self.project_calls.file_graph();
        let mut degree = vec![0usize; files.len()];
        for &(a, b) in &edges {
            degree[a] += 1;
            degree[b] += 1;
        }
        let nodes = files
            .iter()
            .enumerate()
            .map(|(i, f)| graphlayout::NodeInput {
                label: file_label(f),
                file: f.clone(),
                weight: (degree[i] + 1) as f32,
                cyclic: false,
            })
            .collect();
        graphlayout::layout(nodes, edges)
    }

    /// A resolver over the project's current file set (for building/refreshing
    /// the import graph). Cheap: in-memory path work plus one `go.mod` read.
    fn import_resolver(&self) -> Option<imports::Resolver> {
        let project = self.project.as_ref()?;
        let files: Vec<PathBuf> = project.files.iter().map(|f| f.abs.clone()).collect();
        Some(imports::Resolver::new(&project.root, &files))
    }

    /// Rebuild the whole import graph from the per-file raw imports (after the
    /// index build) and refresh the tree.
    fn rebuild_import_graph(&mut self, raw: HashMap<PathBuf, Vec<imports::RawImport>>) {
        if let Some(resolver) = self.import_resolver() {
            self.import_graph = imports::ImportGraph::build(raw, &resolver, highlight::detect);
        }
        self.import_cycles = self.import_graph.cycles();
        self.refresh_import_tree();
    }

    /// Re-resolve every edge against the current file set (after a file was
    /// created/deleted/renamed, which can change how other files resolve).
    fn reresolve_import_graph(&mut self) {
        if let Some(resolver) = self.import_resolver() {
            self.import_graph.reresolve(&resolver, highlight::detect);
        }
        self.import_cycles = self.import_graph.cycles();
        self.refresh_import_tree();
    }

    /// The file the Imports tab is focused on — the active pane's file.
    fn import_focus(&self) -> Option<PathBuf> {
        self.active_viewer().map(|v| v.abs.clone())
    }

    /// Rebuild the import tree for the focus file, preserving the current
    /// direction and "expand all" state. Cheap — pure in-memory graph lookups.
    fn refresh_import_tree(&mut self) {
        let (Some(root), Some(focus)) = (
            self.project.as_ref().map(|p| p.root.clone()),
            self.import_focus(),
        ) else {
            self.import_tree = None;
            return;
        };
        let was_full = self.import_tree.as_ref().is_some_and(|t| t.full);
        let mut tree = imports::ImportTree::new(&self.import_graph, &root, focus, self.import_dir);
        if was_full {
            tree.expand_all(&self.import_graph, &root);
        }
        self.import_tree = Some(tree);
    }

    fn refresh_finder(&mut self) {
        match self.finder.mode {
            FinderMode::Files => {
                if let Some(p) = &self.project {
                    let files = p.files.clone();
                    self.finder.refresh_files(&files);
                }
            }
            FinderMode::Symbols => {
                let symbols = self.symbol_index.clone();
                self.finder.refresh_symbols(&symbols);
            }
        }
    }

    /// Gate every project open behind `.clew/` consent: an existing `.clew/`
    /// directory counts as consent already given; otherwise ask, and refuse
    /// to open the project when denied or not writable.
    fn request_open(&mut self, root: PathBuf) -> Task<Message> {
        // An existing .clew records consent already given: open straight away.
        if root.join(".clew").is_dir() {
            return self.start_scan(root);
        }
        // Otherwise ask via an in-app modal (see ui::consent_modal).
        self.pending_consent = Some(root);
        Task::none()
    }

    fn start_scan(&mut self, root: PathBuf) -> Task<Message> {
        self.scanning = true;
        self.status = format!("Scanning {}…", root.display());
        // Preferred: let clew-server scan and return the tree (its `Tree` reply
        // builds the project via `handle_server_reply`).
        if self.server_tx.is_some() {
            if self.request_open_project(root.clone()) {
                return Task::none();
            }
            // Channel closed mid-session — fall through to a local scan.
        } else {
            // Server not up yet: defer. `ServerConnected` sends the OpenProject
            // once it is; `ServerUnavailable` falls back to a local scan. This
            // is what removes the duplicate scan at startup.
            self.pending_scan_root = Some(root);
            return Task::none();
        }
        self.local_scan(root)
    }

    /// Add (or update) a saved connection, de-duplicated by `user@host:port`, and
    /// persist the list. Most-recent first, so it heads the Connect modal's list.
    fn remember_connection(&mut self, conn: connect::SavedConnection) {
        self.saved_connections
            .retain(|c| !(c.user_host() == conn.user_host() && c.port == conn.port));
        self.saved_connections.insert(0, conn);
        if let Err(e) = connect::save(&self.saved_connections) {
            self.status = format!("Cannot save connections: {e}");
        }
    }

    /// Switch the server transport to `target`. Drops the current project (it
    /// lives on the old host) and the stale request channel; restarting the
    /// subscription brings up the new transport, which hands back a fresh channel
    /// via `ServerConnected`. The Connect modal, if open, moves to "connecting".
    fn connect_to(&mut self, target: connect::ConnTarget) {
        let label = target.label();
        self.project = None;
        self.panes = [None, None];
        self.split = false;
        self.active = 0;
        self.server_tx = None;
        self.pending_scan_root = None;
        self.scanning = false;
        self.connection = target;
        self.status = format!("Connecting to {label}…");
        if let Some(ui) = &mut self.connect {
            ui.stage = ConnectStage::Connecting { label };
        }
    }

    /// Show the remote folder picker for `path` (home when `None`) and request its
    /// listing. The reply (`DirListing`) fills it in via `handle_server_event`.
    fn enter_remote_browser(&mut self, path: Option<String>) {
        // Keep the current directory shown (dimmed) while the next one loads;
        // start empty when there was no browser yet.
        let (cwd, parent, entries) = match self.connect.as_mut().map(|u| &mut u.stage) {
            Some(ConnectStage::Browsing(b)) => {
                (b.cwd.clone(), b.parent.clone(), std::mem::take(&mut b.entries))
            }
            _ => (String::new(), None, Vec::new()),
        };
        if let Some(ui) = &mut self.connect {
            ui.stage = ConnectStage::Browsing(RemoteBrowser {
                cwd,
                parent,
                entries,
                loading: true,
            });
        }
        self.request_list_dir(path);
    }

    /// Send a `ListDir` for the remote folder picker (`None` = the login home).
    fn request_list_dir(&mut self, path: Option<String>) {
        let Some(tx) = self.server_tx.clone() else {
            return;
        };
        let id = self.next_req_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let _ = tx.send(clew_protocol::ClientMessage {
            id,
            request: clew_protocol::Request::ListDir { path },
        });
    }

    /// Start a streaming answer for the Ask panel: push a pending turn, then feed
    /// it token-by-token — over the server (`ChatStream`, deltas routed by
    /// `handle_server_event`) when connected, else the provider locally. Returns
    /// the Task that pumps tokens into `AskDelta` / `AskStreamEnded`.
    fn start_ask_stream(
        &mut self,
        question: String,
        sources: Vec<(explain::Node, f32)>,
        cfg: llm::Config,
        system: String,
        messages: Vec<llm::ChatMsg>,
    ) -> Task<Message> {
        use iced::futures::SinkExt;
        self.ask_turns.push(AskTurn {
            question,
            answer_md: String::new(),
            answer: Vec::new(),
            sources,
            streaming: true,
        });
        self.asking = false;

        let stream_id = self.next_req_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<ChatStreamPiece>();

        // Server endpoint: register the channel and send the streaming request;
        // the deltas arrive as notifications. Otherwise stream locally.
        let local = if let Some(server_tx) = self.server_tx.clone() {
            self.chat_streams.lock().unwrap().insert(stream_id, tx);
            let msgs: Vec<clew_protocol::AiChatMsg> = messages
                .iter()
                .map(|m| clew_protocol::AiChatMsg {
                    role: m.role_str().to_string(),
                    content: m.content.clone(),
                })
                .collect();
            let _ = server_tx.send(clew_protocol::ClientMessage {
                id: stream_id,
                request: clew_protocol::Request::ChatStream {
                    stream: stream_id,
                    system,
                    messages: msgs,
                    max_tokens: 1024,
                },
            });
            None
        } else {
            Some((cfg, system, messages, tx))
        };

        let stream = iced::stream::channel(256, move |mut output: iced::futures::channel::mpsc::Sender<Message>| async move {
            let mut rx = rx;
            // Local endpoint: run the blocking provider call, feeding the channel.
            if let Some((cfg, system, messages, tx)) = local {
                tokio::task::spawn_blocking(move || {
                    let result = llm::complete_chat_stream(&cfg, &system, &messages, 1024, |d| {
                        let _ = tx.send(ChatStreamPiece::Delta(d.to_string()));
                    });
                    let _ = tx.send(ChatStreamPiece::Done(result.err()));
                });
            }
            while let Some(piece) = rx.recv().await {
                let (msg, done) = match piece {
                    ChatStreamPiece::Delta(t) => (Message::AskDelta(t), false),
                    ChatStreamPiece::Done(err) => (Message::AskStreamEnded(err), true),
                };
                if output.send(msg).await.is_err() || done {
                    break;
                }
            }
        });
        Task::run(stream, |m| m)
    }

    /// Ask the server to (re)build the project's API docs. The `Docs` reply lands
    /// in `handle_server_event`.
    fn request_docs(&mut self) {
        let Some(tx) = self.server_tx.clone() else {
            return;
        };
        let id = self.next_req_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if tx
            .send(clew_protocol::ClientMessage {
                id,
                request: clew_protocol::Request::BuildDocs,
            })
            .is_ok()
        {
            self.docs.loading = true;
        }
    }

    /// Build the main-pane doc page for the item at (`rel`, `line`): the item
    /// itself plus its members (public unless "show all"), each with its doc
    /// comment parsed to markdown. Switches the main pane to the page.
    fn open_doc_page(&mut self, rel: &str, line: usize) {
        let Some(file) = self.docs.files.iter().find(|f| f.rel == rel) else {
            return;
        };
        let Some(item) = find_doc_item(&file.items, line) else {
            return;
        };
        let mut entries = Vec::new();
        flatten_doc(item, 0, self.docs.show_all, &mut entries);
        self.docs.page = Some(DocPage {
            rel: rel.to_string(),
            entries,
        });
        self.show_overview = false;
        self.show_stats = false;
    }

    /// Open the doc page for the symbol named `name` (from "View docs"). Switches
    /// to the DOCS tab. If the index isn't built yet, build it and resolve the
    /// name when it arrives.
    fn view_docs_for(&mut self, name: &str) {
        self.sidebar = SidebarTab::Docs;
        self.show_left_sidebar = true;
        if let Some((rel, line)) = find_doc_by_name(&self.docs.files, name) {
            self.open_doc_page(&rel, line);
        } else if self.docs.files.is_empty() {
            self.docs.pending_view = Some(name.to_string());
            self.request_docs();
        } else {
            self.status = format!("No docs for “{name}”");
        }
    }

    /// Ask the server to open `root` and return its tree. Records `root` as the
    /// pending scan so the `Tree` reply can build the project. Returns false if
    /// the request could not be sent (no server / channel closed).
    fn request_open_project(&mut self, root: PathBuf) -> bool {
        let Some(tx) = self.server_tx.clone() else {
            return false;
        };
        let id = self.next_req_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let request = clew_protocol::Request::OpenProject {
            root: root.to_string_lossy().into_owned(),
        };
        if tx
            .send(clew_protocol::ClientMessage { id, request })
            .is_ok()
        {
            self.pending_scan_root = Some(root);
            true
        } else {
            false
        }
    }

    /// Scan the project on the client (fallback when the server is unavailable).
    fn local_scan(&self, root: PathBuf) -> Task<Message> {
        Task::perform(
            async move {
                let fallback_root = root.clone();
                tokio::task::spawn_blocking(move || fs_scan::scan(root))
                    .await
                    .unwrap_or_else(|_| ScanResult {
                        root: fallback_root,
                        tree: DirNode::default(),
                        files: Vec::new(),
                        truncated: false,
                    })
            },
            Message::ScanDone,
        )
    }

    /// Re-scan the tree off-thread after a structural change, delivering the
    /// result as `TreeUpdated` (a light swap, not a full project reopen).
    fn rescan_tree(&self, root: PathBuf) -> Task<Message> {
        Task::perform(
            async move {
                let fallback = root.clone();
                tokio::task::spawn_blocking(move || fs_scan::scan(root))
                    .await
                    .unwrap_or_else(|_| ScanResult {
                        root: fallback,
                        tree: DirNode::default(),
                        files: Vec::new(),
                        truncated: false,
                    })
            },
            Message::TreeUpdated,
        )
    }

    fn finder_open_index(&mut self, idx: usize) -> Task<Message> {
        self.finder.open = false;
        match self.finder.mode {
            FinderMode::Files => {
                let Some(entry) = self
                    .project
                    .as_ref()
                    .and_then(|p| p.files.get(idx))
                    .cloned()
                else {
                    return Task::none();
                };
                self.open_file(entry.abs, None, true)
            }
            FinderMode::Symbols => {
                let Some(entry) = self.symbol_index.get(idx).cloned() else {
                    return Task::none();
                };
                self.open_file(entry.abs, Some(entry.line), true)
            }
        }
    }

    /// Open a file into the active pane, optionally jumping to a 1-based line.
    /// The function/method defined exactly at `(file, line1)`, if any — recorded
    /// with a history entry so it can be re-anchored across edits.
    fn symbol_name_at(&self, file: &Path, line1: usize) -> Option<String> {
        self.symbol_index_by_file.get(file)?.iter().find_map(|s| {
            (s.line == line1 && matches!(s.kind.as_str(), "function" | "method"))
                .then(|| s.name.clone())
        })
    }

    /// The live 1-based line of a noted symbol, resolved against the current
    /// index — `None` when the symbol no longer exists (an orphaned note).
    pub fn note_symbol_line(&self, rel: &str, symbol: &str) -> Option<usize> {
        let root = &self.project.as_ref()?.root;
        let abs = root.join(rel);
        self.symbol_index_by_file.get(&abs)?.iter().find(|s| s.name == symbol).map(|s| s.line)
    }

    /// Whether `(file, name)` is a test function, per the symbol index.
    pub fn is_test_symbol(&self, file: &Path, name: &str) -> bool {
        self.symbol_index_by_file
            .get(file)
            .is_some_and(|syms| syms.iter().any(|s| s.name == name && s.is_test))
    }

    /// The first 1-based line where `caller` (in `caller_file`) calls `callee`,
    /// found by re-parsing the caller's live source (the open pane, else disk).
    fn call_site_line(&self, caller_file: &Path, caller: &str, callee: &str) -> Option<usize> {
        let lang = crate::highlight::detect(caller_file)?;
        let source = self
            .panes
            .iter()
            .flatten()
            .find(|v| v.abs == caller_file)
            .map(|v| v.source.as_ref().clone())
            .or_else(|| std::fs::read_to_string(caller_file).ok())?;
        projectcalls::calls_of(&source, lang)
            .into_iter()
            .filter(|cs| cs.callee == callee && cs.caller.as_deref() == Some(caller))
            .map(|cs| cs.line)
            .min()
    }

    /// Begin a debug session from the project's `.clew/launch.json`. Spawns the
    /// adapter off-thread and streams its events back as `DapEvent` messages.
    fn start_debug(&mut self) -> Task<Message> {
        if self.debug.is_some() {
            self.status = "A debug session is already running".into();
            return Task::none();
        }
        let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
            return Task::none();
        };
        let cfg = match read_launch_config(&root) {
            Ok(cfg) => cfg,
            Err(e) => {
                self.status = e;
                return Task::none();
            }
        };
        if !cfg.program.exists() {
            self.status =
                format!("Program not found: {} — build it first", cfg.program.display());
            return Task::none();
        }
        // Pick the language (explicit type, else the program's extension).
        let Some(lang) = dap::Lang::detect(cfg.type_hint.as_deref(), &cfg.program) else {
            self.status = format!("Unknown debug type {:?} in launch.json", cfg.type_hint);
            return Task::none();
        };
        let (program, args, cwd) = (cfg.program.clone(), cfg.args.clone(), cfg.cwd.clone());
        self.debug = Some(DebugSession {
            client: None,
            status: DebugStatus::Launching,
            thread_id: None,
            frames: Vec::new(),
            scopes: Vec::new(),
            watches: Vec::new(),
            output: Vec::new(),
            current: None,
            program: program.clone(),
            args: args.clone(),
            cwd: cwd.clone(),
            port: None,
        });
        self.show_bottom = true;
        self.bottom_tab = BottomTab::Debug; // reveal the debug panel
        self.debug_last_fn = None;
        self.status = format!("Starting debugger — {}…", lang.label());

        // Preferred: spawn the debug adapter on clew-server (it must run where the
        // program does). Allocate its proc handle up front; the stream sets up the
        // proxy after it resolves the adapter binary.
        let proc = self.next_proc_id;
        self.next_proc_id += 1;
        let server_tx = self.server_tx.clone();

        let stream = iced::stream::channel(64, move |mut output: iced::futures::channel::mpsc::Sender<Message>| async move {
            use iced::futures::SinkExt;
            // Resolve the adapter for this language (locates its binary + builds
            // the launch arguments). Off the UI thread as it may spawn xcrun/pip.
            let adapter = match dap::adapter::resolve(lang, &program, &args, &cwd) {
                Ok(a) => a,
                Err(e) => {
                    let _ = output.send(Message::DebugFailed(e)).await;
                    return;
                }
            };
            let port = match adapter.transport {
                dap::client::Transport::Tcp(p) => Some(p),
                dap::client::Transport::Stdio => None,
            };
            // Stdio adapters (lldb-dap) run on clew-server, proxied; TCP adapters
            // or a missing server fall back to a local spawn.
            let started = match (&adapter.transport, &server_tx) {
                (dap::client::Transport::Stdio, Some(tx)) => {
                    let spawn = clew_protocol::Request::SpawnProcess {
                        proc,
                        cmd: adapter.command.to_string_lossy().into_owned(),
                        args: adapter.args.clone(),
                        cwd: Some(cwd.to_string_lossy().into_owned()),
                    };
                    let (stdin, stdout, feed) = proxy_transport(tx, proc, spawn);
                    // Register the output feed before the adapter can answer.
                    let _ = output
                        .send(Message::RegisterProcFeed { proc, feed })
                        .await;
                    dap::DapClient::connect(stdin, stdout).await
                }
                _ => dap::DapClient::start(&adapter.command, &adapter.args, &cwd, adapter.transport).await,
            };
            let (client, mut events) = match started {
                Ok(pair) => pair,
                Err(e) => {
                    let _ = output.send(Message::DebugFailed(e)).await;
                    return;
                }
            };
            if let Err(e) = client.initialize().await {
                let _ = output.send(Message::DebugFailed(format!("initialize: {e}"))).await;
                return;
            }
            // Hand the client to the App *before* launching, so it holds the
            // handle when the `initialized` event arrives (it sends breakpoints).
            let _ = output.send(Message::DapStarted { client: client.clone(), port }).await;
            client.launch(adapter.launch);
            while let Some(ev) = events.recv().await {
                if output.send(Message::DapEvent(ev)).await.is_err() {
                    break;
                }
            }
            // Adapter closed: make sure the session tears down.
            let _ = output.send(Message::DapEvent(dap::DapEvent::Terminated)).await;
        });
        Task::run(stream, |m| m)
    }

    /// Fold a DAP adapter event into the session state.
    fn on_dap_event(&mut self, ev: dap::DapEvent) -> Task<Message> {
        let Some(session) = self.debug.as_mut() else {
            return Task::none();
        };
        match ev {
            dap::DapEvent::Initialized => {
                // The adapter is ready for configuration: send every file's
                // breakpoints, then configurationDone to start execution.
                let Some(client) = session.client.clone() else {
                    return Task::none();
                };
                let bps: Vec<(PathBuf, BpList)> = self
                    .breakpoints
                    .iter()
                    .map(|(p, m)| {
                        (p.clone(), m.iter().map(|(l, bp)| (*l, bp.condition.clone())).collect())
                    })
                    .collect();
                Task::perform(
                    async move {
                        for (file, lines) in bps {
                            let _ = client.set_breakpoints(&file, &lines).await;
                        }
                        let _ = client.configuration_done().await;
                    },
                    |()| Message::Noop,
                )
            }
            dap::DapEvent::Stopped(s) => {
                session.status = DebugStatus::Stopped;
                session.thread_id = s.thread_id;
                let Some(client) = session.client.clone() else {
                    return Task::none();
                };
                let tid = s.thread_id.unwrap_or(0);
                self.status = format!("Stopped: {}", s.reason);
                // Load the stack, then the top frame's scopes + variables.
                Task::perform(
                    async move {
                        let frames = client.stack_trace(tid).await.unwrap_or_default();
                        let mut scopes = Vec::new();
                        if let Some(top) = frames.first()
                            && let Ok(scs) = client.scopes(top.id).await
                        {
                            for sc in scs {
                                if sc.expensive || sc.variables_reference == 0 {
                                    continue; // skip Registers etc. by default
                                }
                                let vars =
                                    client.variables(sc.variables_reference).await.unwrap_or_default();
                                scopes.push(DebugScope { name: sc.name, vars });
                            }
                        }
                        (frames, scopes)
                    },
                    |(frames, scopes)| Message::DapStopInspected { frames, scopes },
                )
            }
            dap::DapEvent::Continued { .. } => {
                session.status = DebugStatus::Running;
                session.current = None;
                session.frames.clear();
                session.scopes.clear();
                session.watches.clear();
                Task::none()
            }
            dap::DapEvent::Output(o) => {
                // Keep the tail bounded.
                if session.output.len() >= 500 {
                    session.output.remove(0);
                }
                session.output.push((o.category, o.text));
                Task::none()
            }
            dap::DapEvent::Exited { code } => {
                session.output.push(("console".into(), format!("Process exited with code {code}\n")));
                session.status = DebugStatus::Terminated;
                session.current = None;
                Task::none()
            }
            dap::DapEvent::Terminated => {
                session.status = DebugStatus::Terminated;
                session.current = None;
                session.frames.clear();
                session.scopes.clear();
                Task::none()
            }
            dap::DapEvent::StartDebugging(config) => {
                // js-debug: open a child session on the same adapter for the real
                // target, then drive its handshake (it owns the breakpoints/stack).
                let Some(port) = session.port else {
                    return Task::none();
                };
                let stream = iced::stream::channel(64, move |mut output: iced::futures::channel::mpsc::Sender<Message>| async move {
                    use iced::futures::SinkExt;
                    let (client, mut events) = match dap::DapClient::connect_tcp(port).await {
                        Ok(pair) => pair,
                        Err(e) => {
                            let _ = output.send(Message::DebugFailed(e)).await;
                            return;
                        }
                    };
                    if client.initialize().await.is_err() {
                        return;
                    }
                    let _ = output.send(Message::DapChildStarted(client.clone())).await;
                    client.launch(config);
                    while let Some(ev) = events.recv().await {
                        if output.send(Message::DapEvent(ev)).await.is_err() {
                            break;
                        }
                    }
                });
                Task::run(stream, |m| m)
            }
            dap::DapEvent::Other(_) => Task::none(),
        }
    }

    /// A snapshot of the paused debugger's runtime state (stopped location, call
    /// stack, and variable values), for grounding "Ask clew" answers in what's
    /// actually happening. `None` unless a session is stopped at a point.
    fn debug_context(&self) -> Option<String> {
        let session = self.debug.as_ref()?;
        if session.status != DebugStatus::Stopped {
            return None;
        }
        let mut s = String::from(
            "### Runtime state (the program is PAUSED in the debugger right now)\n",
        );
        if let Some((path, line)) = &session.current {
            s.push_str(&format!("Paused at {}:{}\n", self.rel_of(path), line));
        }
        if !session.frames.is_empty() {
            s.push_str("Call stack (innermost first):\n");
            for f in session.frames.iter().take(8) {
                let loc = f
                    .path
                    .as_ref()
                    .map(|p| format!(" ({}:{})", self.rel_of(p), f.line))
                    .unwrap_or_default();
                s.push_str(&format!("- {}{}\n", f.name, loc));
            }
        }
        for sc in &session.scopes {
            if sc.vars.is_empty() {
                continue;
            }
            s.push_str(&format!("Variables — {} (current frame):\n", sc.name));
            for v in sc.vars.iter().take(40) {
                s.push_str(&format!("- {} = {}\n", v.name, v.value));
            }
        }
        s.push('\n');
        Some(s)
    }

    /// Push one file's breakpoints (line + condition) to a live adapter. No-op
    /// when no session is running.
    fn push_breakpoints(&self, path: &Path) -> Task<Message> {
        let Some(client) = self.debug.as_ref().and_then(|s| s.client.clone()) else {
            return Task::none();
        };
        let lines: BpList = self
            .breakpoints
            .get(path)
            .map(|m| m.iter().map(|(l, bp)| (*l, bp.condition.clone())).collect())
            .unwrap_or_default();
        let p = path.to_path_buf();
        Task::perform(
            async move {
                let _ = client.set_breakpoints(&p, &lines).await;
            },
            |()| Message::Noop,
        )
    }

    /// Re-evaluate all watch expressions in the current frame (on each stop, or
    /// when a watch is added). No-op unless paused with watches set.
    fn eval_watches(&self) -> Task<Message> {
        let Some(session) = self.debug.as_ref() else {
            return Task::none();
        };
        if session.status != DebugStatus::Stopped || self.debug_watches.is_empty() {
            return Task::none();
        }
        let (Some(client), Some(frame)) = (session.client.clone(), session.frames.first()) else {
            return Task::none();
        };
        let frame_id = frame.id;
        let exprs = self.debug_watches.clone();
        Task::perform(
            async move {
                let mut out = Vec::with_capacity(exprs.len());
                for e in exprs {
                    let v = client.evaluate(&e, frame_id).await.unwrap_or_else(|err| format!("⚠ {err}"));
                    out.push((e, v));
                }
                out
            },
            Message::DebugWatchesEvaluated,
        )
    }

    /// Send a stepping / continue command to the adapter.
    fn debug_control(&mut self, cmd: DebugCmd) -> Task<Message> {
        let Some(session) = self.debug.as_mut() else {
            return Task::none();
        };
        let (Some(client), Some(tid)) = (session.client.clone(), session.thread_id) else {
            return Task::none();
        };
        session.status = DebugStatus::Running;
        session.current = None;
        Task::perform(
            async move {
                let _ = match cmd {
                    DebugCmd::Continue => client.continue_(tid).await,
                    DebugCmd::StepOver => client.next(tid).await,
                    DebugCmd::StepIn => client.step_in(tid).await,
                    DebugCmd::StepOut => client.step_out(tid).await,
                };
            },
            |()| Message::Noop,
        )
    }

    /// Persist the navigation tree to the project's `.clew/`, ignoring errors
    /// (a read-only project just keeps its history for the session).
    fn save_history(&self) {
        if let Some(root) = self.project.as_ref().map(|p| &p.root) {
            let _ = history::save(root, &self.history);
        }
    }

    fn save_notes(&mut self) {
        if let Some(root) = self.project.as_ref().map(|p| p.root.clone())
            && let Err(e) = notes::save(&root, &self.notes)
        {
            self.status = format!("Cannot write .clew/notes.json: {e}");
        }
    }

    fn open_file(&mut self, abs: PathBuf, line: Option<usize>, push: bool) -> Task<Message> {
        // Opening a file leaves the overview / stats / docs page for the code, and
        // ends any time-travel session (which would otherwise stay active-but-hidden
        // and keep capturing Esc/←/→ for a file that's no longer shown).
        self.show_overview = false;
        self.show_stats = false;
        self.docs.page = None;
        self.time_travel = None;
        if push {
            // Remember the symbol at the target so the trail can re-anchor to it
            // after edits shift its line (see `reanchor` in FilesRehashed).
            let label = line.and_then(|l| self.symbol_name_at(&abs, l));
            self.history.push(Loc { path: abs.clone(), line }, label);
            self.save_history();
        }
        // A jump lands the reader in the code view.
        self.code_focused = true;
        let pane = self.active;
        let line_height = self.line_height();
        // Same file already in the active pane: move the cursor and scroll.
        if let Some(v) = self.active_viewer_mut()
            && v.abs == abs
        {
            v.target_line = line;
            if let Some(l) = line {
                let l0 = l.saturating_sub(1);
                v.reveal(l0); // expand any fold hiding the jump target
                v.caret = Some((l0, 0));
            }
            let y = v.scroll_offset_for(line, line_height);
            v.scroll_y = y;
            let scroll = operation::scroll_to(ui::code_scroll_id(pane), AbsoluteOffset { x: 0.0, y });
            return self.follow_caret(scroll);
        }
        let rel = self.rel_of(&abs);
        // A go-to-def target outside the project (a dependency or stdlib source
        // the LSP resolved) would be refused by the server, whose ReadFile
        // enforces the project boundary. For a LOCAL server, read it directly,
        // read-only — the client already resolved it via the LSP, so reading a
        // dep's source is safe and is core to a code reader. Keep routing
        // through the server for a REMOTE connection, where the boundary is a
        // real security guard and the file lives on the remote host anyway.
        let external_local = self
            .project
            .as_ref()
            .is_some_and(|p| !abs.starts_with(&p.root))
            && !self.connection.is_remote();
        // Preferred: fetch the file from clew-server — it reads, highlights, and
        // extracts symbols/docs/inactive server-side. The reply arrives as
        // Event::FileContent and lands via `apply_file_content`.
        if !external_local && let Some(tx) = self.server_tx.clone() {
            let id = self.next_req_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let request = clew_protocol::Request::ReadFile {
                rel: rel.clone(),
                target: self.target_spec(),
            };
            if tx
                .send(clew_protocol::ClientMessage { id, request })
                .is_ok()
            {
                self.pending_reads
                    .insert(id, ReadKind::Open { pane, target: line });
                self.status = format!("Loading {rel}…");
                return Task::none();
            }
        }
        // Fallback: server not up — read + highlight locally.
        self.status = format!("Loading {rel}…");
        Task::perform(load_file(pane, abs, line), |(pane, abs, target, result)| {
            Message::FileLoaded {
                pane,
                abs,
                target,
                result,
            }
        })
    }

    fn on_file_loaded(
        &mut self,
        pane: usize,
        abs: PathBuf,
        target: Option<usize>,
        result: Result<String, String>,
    ) -> Task<Message> {
        let rel = self.rel_of(&abs);
        let content = match result {
            Err(e) => {
                self.status = format!("{rel}: {e}");
                return Task::none();
            }
            Ok(content) => content,
        };

        let lang_key = highlight::detect(&abs);
        let source = Arc::new(content);
        let lines = highlight::plain_lines(&source);
        let line_height = self.line_height();
        let old_viewport = self.panes[pane].as_ref().map(|v| v.viewport_h);
        let mut v = Viewer::new(abs.clone(), rel, lang_key, source.clone(), lines);
        if let Some(h) = old_viewport {
            v.viewport_h = h;
        }
        v.target_line = target;
        // Put the block cursor on the jump target (or the top of the file).
        v.caret = Some((target.map(|t| t.saturating_sub(1)).unwrap_or(0), 0));
        let y = v.scroll_offset_for(target, line_height);
        v.scroll_y = y;
        // Just the path here; the right status segment already reports line count.
        self.status = v.rel.clone();
        self.panes[pane] = Some(v);
        // Seed the content hash so the watcher can tell real edits from noise.
        self.registry
            .set(abs.clone(), incremental::content_hash(source.as_bytes()));
        // Point the Imports tab at the newly focused file.
        if pane == self.active {
            self.refresh_import_tree();
        }

        let scroll = operation::scroll_to(ui::code_scroll_id(pane), AbsoluteOffset { x: 0.0, y });
        // Start (or reuse) a language server for this file and open the doc.
        let lsp_task = match lang_key {
            Some(lang) => self.ensure_lsp(lang),
            None => Task::none(),
        };
        let content = self.content_tasks(abs, source, lang_key);
        // Symbols arrive later via `Highlighted`; follow_caret there resolves the
        // enclosing function. Here it shows the file until then.
        self.follow_caret(Task::batch([scroll, lsp_task, content]))
    }

    /// Off-thread re-highlight + git-info tasks for a file's current source,
    /// shared by initial load and live refresh. Both deliver `Highlighted` /
    /// `GitInfoLoaded` keyed by `abs`, so they route to whatever pane shows it.
    fn content_tasks(
        &self,
        abs: PathBuf,
        source: Arc<String>,
        lang_key: Option<&'static str>,
    ) -> Task<Message> {
        let hl_abs = abs.clone();
        let hl_source = source.clone();
        let target = self.reading_target.clone();
        let highlight_task = Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    let lines = highlight::highlight_lines(&hl_source, lang_key);
                    let symbols = lang_key
                        .map(|key| outline::extract(&hl_source, key))
                        .unwrap_or_default();
                    // Author's doc comments, reusing the symbols just parsed.
                    let docs = lang_key
                        .map(|key| docs::extract(&hl_source, key, &symbols))
                        .unwrap_or_default();
                    // Inactive `#[cfg]` lines for the reading target (dimmed).
                    let inactive = lang_key
                        .map(|key| inactive::inactive_lines(&hl_source, key, &target))
                        .unwrap_or_default();
                    (lines, symbols, docs, inactive)
                })
                .await
                .unwrap_or_default()
            },
            move |(lines, symbols, docs, inactive)| Message::Highlighted {
                abs: hl_abs.clone(),
                lines,
                symbols,
                docs,
                inactive,
            },
        );

        let git_task = match self.project.as_ref().map(|p| p.root.clone()) {
            Some(root) => {
                let file = abs.clone();
                Task::perform(
                    async move {
                        tokio::task::spawn_blocking(move || git::info(&root, &file).map(Arc::new))
                            .await
                            .ok()
                            .flatten()
                    },
                    move |info| Message::GitInfoLoaded {
                        abs: abs.clone(),
                        info,
                    },
                )
            }
            None => Task::none(),
        };
        Task::batch([highlight_task, git_task])
    }

    /// Keep the top visible line stable across a line-height change.
    fn rescale_scroll(&mut self, old_line_height: f32) -> Task<Message> {
        let new_line_height = self.line_height();
        if (new_line_height - old_line_height).abs() < f32::EPSILON {
            return Task::none();
        }
        let mut tasks = Vec::new();
        for (pane, slot) in self.panes.iter_mut().enumerate() {
            if let Some(v) = slot {
                let first = v.scroll_y / old_line_height;
                v.scroll_y = first * new_line_height;
                tasks.push(operation::scroll_to(
                    ui::code_scroll_id(pane),
                    AbsoluteOffset {
                        x: 0.0,
                        y: v.scroll_y,
                    },
                ));
            }
        }
        Task::batch(tasks)
    }

    fn handle_key(&mut self, key: keyboard::Key, modifiers: keyboard::Modifiers) -> Task<Message> {
        use keyboard::Key;
        use keyboard::key::Named;

        let cmd = modifiers.command();

        // Rebinding capture takes priority: the next chord becomes the binding.
        if let Some(action) = self.rebinding {
            return self.capture_rebind(action, &key, modifiers);
        }
        // While the shortcuts panel is open (and not capturing), only Esc
        // closes it; swallow other keys so nothing acts behind the modal.
        if self.show_shortcuts {
            if matches!(key.as_ref(), Key::Named(Named::Escape)) {
                self.show_shortcuts = false;
                self.keymap_notice = None;
            }
            return Task::none();
        }
        // Time travel history navigation uses COMMAND chords — Cmd+←/Cmd+h step
        // to an older commit, Cmd+→/Cmd+l to a newer one — so plain ←/→/h/l stay
        // free for reading. Esc exits. Handled before the keymap so these chords
        // drive time travel (overriding e.g. Cmd+← = back) while a session is on.
        if let Some(tt) = self.time_travel.as_ref() {
            let (idx, n) = (tt.idx, tt.commits.len());
            match key.as_ref() {
                Key::Named(Named::Escape) => return self.update(Message::TimeTravelExit),
                Key::Named(Named::ArrowLeft) | Key::Character("h") if cmd && idx + 1 < n => {
                    return self.update(Message::TimeTravelGoto(idx + 1));
                }
                Key::Named(Named::ArrowRight) | Key::Character("l") if cmd && idx > 0 => {
                    return self.update(Message::TimeTravelGoto(idx - 1));
                }
                _ => {}
            }
        }
        // Command chords (those carrying ⌘/⌥/⌃) are dispatched through the
        // customizable keymap. Only modifier-carrying chords are eligible, so
        // the single-key reading motions and text input below stay untouched.
        if cmd || modifiers.alt() || modifiers.control() {
            if let Some(chord) = keymap::Chord::from_event(&key, modifiers) {
                if let Some(action) = self.keymap.action_for(&chord) {
                    if let Some(task) = self.run_command_action(action) {
                        return task;
                    }
                }
            }
        }

        // While time-travelling, swallow any remaining (non-command) keys so
        // plain reading motions don't act on the live file hidden behind the view.
        if self.time_travel.is_some() {
            return Task::none();
        }

        match key.as_ref() {
            // In-file find bar: Enter next, Shift+Enter prev.
            Key::Named(Named::Enter) if self.find.open => {
                self.update(Message::FindStep(if modifiers.shift() { -1 } else { 1 }))
            }
            Key::Named(Named::Escape) => {
                self.pending_g = false;
                self.pending_z = false;
                if self.context_menu.is_some() {
                    self.context_menu = None;
                    return Task::none();
                }
                if self.find.open {
                    return self.update(Message::FindClosed);
                }
                if self.finder.open {
                    return self.update(Message::FinderClosed);
                }
                if let Some(v) = self.active_viewer_mut() {
                    v.selection = None;
                    v.target_line = None;
                }
                Task::none()
            }
            Key::Named(Named::ArrowDown) if self.finder.open => {
                self.finder.move_selection(1);
                Task::none()
            }
            Key::Named(Named::ArrowUp) if self.finder.open => {
                self.finder.move_selection(-1);
                Task::none()
            }
            // -------- Vim-style read-only cursor (only when the code view has
            // focus, so it never steals keys from a text input) --------
            _ if cmd
                || self.finder.open
                || self.context_menu.is_some()
                || !self.code_focused
                || self.active_viewer().is_none() =>
            {
                Task::none()
            }
            // Two-key `g` prefix: gg / gd / gr / gi / gy / gc.
            _ if self.pending_g => {
                self.pending_g = false;
                match key.as_ref() {
                    Key::Character("g") => self.move_cursor(viewer::Motion::FileStart),
                    Key::Character("d") => self.goto_at_cursor(GotoKind::Definition),
                    Key::Character("r") => self.goto_at_cursor(GotoKind::References),
                    Key::Character("i") => self.goto_at_cursor(GotoKind::Implementation),
                    Key::Character("y") => self.goto_at_cursor(GotoKind::TypeDefinition),
                    Key::Character("c") => self.update(Message::CallHierarchyRequested),
                    _ => Task::none(),
                }
            }
            Key::Character("g") => {
                self.pending_g = true;
                Task::none()
            }
            // Two-key `z` prefix for folding: za toggle, zR open all, zM close all.
            _ if self.pending_z => {
                self.pending_z = false;
                match key.as_ref() {
                    Key::Character("a") => self.fold_toggle_at_cursor(),
                    Key::Character("R") => self.fold_all(false),
                    Key::Character("M") => self.fold_all(true),
                    _ => {}
                }
                Task::none()
            }
            Key::Character("z") => {
                self.pending_z = true;
                Task::none()
            }
            Key::Character("h") | Key::Named(Named::ArrowLeft) => {
                self.move_cursor(viewer::Motion::Left)
            }
            Key::Character("l") | Key::Named(Named::ArrowRight) => {
                self.move_cursor(viewer::Motion::Right)
            }
            Key::Character("k") | Key::Named(Named::ArrowUp) => {
                self.move_cursor(viewer::Motion::Up)
            }
            Key::Character("j") | Key::Named(Named::ArrowDown) => {
                self.move_cursor(viewer::Motion::Down)
            }
            Key::Character("w") => self.move_cursor(viewer::Motion::WordForward),
            Key::Character("b") => self.move_cursor(viewer::Motion::WordBack),
            Key::Character("0") => self.move_cursor(viewer::Motion::LineStart),
            Key::Character("$") => self.move_cursor(viewer::Motion::LineEnd),
            Key::Character("G") => self.move_cursor(viewer::Motion::FileEnd),
            _ => Task::none(),
        }
    }

    /// Hover peek assembled locally, no LSP: the same-file symbol's doc comment
    /// plus, for Rust, the project-wide structure of the type or trait under the
    /// cursor ("impl …" / "Implementors …"). `None` when neither applies, so the
    /// caller falls through to the language server.
    fn local_peek(&self, pane: usize, line: usize, col: usize) -> Option<String> {
        let v = self.panes.get(pane)?.as_ref()?;
        let word = analyze::word_at(&v.lines, line, col)?;
        let mut parts: Vec<String> = Vec::new();
        // The author's doc comment, if `word` names a symbol defined here.
        if let Some(sym_line) = v.symbols.iter().find(|s| s.name == word).map(|s| s.line)
            && let Some(doc) = v.docs.get(&sym_line)
        {
            parts.push(doc.clone());
        }
        // Rust type/trait relations, resolved project-wide.
        if v.lang_key == Some("rust")
            && let Some(summary) = self.structure.summary_line(&word)
        {
            parts.push(summary);
        }
        (!parts.is_empty()).then(|| parts.join("\n\n"))
    }

    /// The cached one-line Explain summary for the identifier under `(line, col)`,
    /// if it names an explained function/method. Prefers a definition in the same
    /// file, then a unique match anywhere in the project (so hovering a call to a
    /// function defined elsewhere still shows what it does). `None` when the name
    /// is unknown, ambiguous, or its summary is an error placeholder.
    fn hover_summary(&self, pane: usize, line: usize, col: usize) -> Option<String> {
        let v = self.panes.get(pane)?.as_ref()?;
        let word = analyze::word_at(&v.lines, line, col)?;
        let usable = |s: &str| (!explain::is_error_summary(s)).then(|| ui::first_sentence(s));
        // Same-file definition wins (unambiguous).
        if let Some(c) = self
            .explanations
            .get(&explain::Node::Function { file: v.abs.clone(), name: word.clone() })
        {
            return usable(&c.summary);
        }
        // Otherwise, only if exactly one explained function has this name.
        let mut hit: Option<&str> = None;
        for (node, c) in &self.explanations {
            if let explain::Node::Function { name, .. } = node
                && name == &word
            {
                if hit.is_some() {
                    return None; // ambiguous
                }
                hit = Some(&c.summary);
            }
        }
        hit.and_then(usable)
    }

    /// The LSP diagnostic covering (`line`, `col`) in `pane`, as a labelled
    /// message ("Error: …" / "Warning: …"), so hovering a red-underlined symbol
    /// says what is wrong. Prefers the most severe diagnostic at that spot. Uses
    /// the same char→display-column mapping as the underline rendering.
    fn diagnostic_at(&self, pane: usize, line: usize, col: usize) -> Option<String> {
        let v = self.panes.get(pane)?.as_ref()?;
        let lang = v.lang_key?;
        let LspSlot::Ready(client) = self.lsp.get(lang)? else {
            return None;
        };
        let utf16 = client.encoding == lsp::client::PositionEncoding::Utf16;
        client
            .diagnostics(&v.abs)
            .into_iter()
            .filter(|d| d.line == line)
            .filter(|d| {
                let raw = v.source_line(d.line).unwrap_or("");
                let c0 = viewer::display_col_from_char(raw, d.char_start, utf16);
                let c1 = viewer::display_col_from_char(raw, d.char_end, utf16).max(c0 + 1);
                (c0..c1).contains(&col)
            })
            // Severity 1 is error (most severe) → lowest number sorts first.
            .min_by_key(|d| d.severity)
            .map(|d| {
                let label = match d.severity {
                    1 => "Error",
                    2 => "Warning",
                    3 => "Info",
                    _ => "Hint",
                };
                format!("{label}: {}", d.message.trim())
            })
    }

    /// Run a rebindable command action. Returns `None` when the action declines
    /// in the current context (so the key falls through — e.g. ⌘C inside the
    /// finder input should copy text, not the code selection).
    fn run_command_action(&mut self, action: keymap::Action) -> Option<Task<Message>> {
        use keymap::Action::*;
        Some(match action {
            OpenFile => self.update(Message::FinderOpened(FinderMode::Files)),
            OpenSymbol => self.update(Message::FinderOpened(FinderMode::Symbols)),
            ProjectSearch => self.update(Message::SidebarTabPicked(SidebarTab::Search)),
            FindInFile => self.update(Message::FindOpened),
            CopySelection => {
                if self.finder.open {
                    return None;
                }
                self.update(Message::CopySelection)
            }
            ToggleBookmark => self.update(Message::BookmarkToggled),
            GotoLine => self.update(Message::GotoLineRequested),
            ToggleSplit => self.update(Message::ToggleSplit),
            ZoomIn => self.update(Message::FontSizeDelta(1.0)),
            ZoomOut => self.update(Message::FontSizeDelta(-1.0)),
            ZoomReset => self.update(Message::FontSizeReset),
            GoBack => self.update(Message::GoBack),
            GoForward => self.update(Message::GoForward),
        })
    }

    /// Capture a keypress as the new binding for `action`. Esc cancels; keys
    /// without a ⌘/⌥/⌃ modifier or that collide with another action are
    /// rejected with an inline notice (capture stays active so the user can
    /// try again). A successful bind is persisted immediately.
    fn capture_rebind(
        &mut self,
        action: keymap::Action,
        key: &keyboard::Key,
        modifiers: keyboard::Modifiers,
    ) -> Task<Message> {
        use keyboard::key::Named;
        if matches!(key.as_ref(), keyboard::Key::Named(Named::Escape)) {
            self.rebinding = None;
            self.keymap_notice = None;
            return Task::none();
        }
        let Some(chord) = keymap::Chord::from_event(key, modifiers) else {
            self.keymap_notice = Some("Unsupported key".into());
            return Task::none();
        };
        if !chord.is_command() {
            self.keymap_notice = Some("Shortcut must include ⌘, ⌥, or ⌃".into());
            return Task::none();
        }
        if let Some(other) = self.keymap.conflict(&chord, action) {
            self.keymap_notice = Some(format!("Already used by “{}”", other.label()));
            return Task::none();
        }
        self.keymap.rebind(action, chord);
        self.rebinding = None;
        self.keymap_notice = None;
        if let Err(e) = self.keymap.save() {
            self.status = format!("Could not save shortcuts: {e}");
        }
        Task::none()
    }

    /// Move the active pane's block cursor and scroll it into view.
    fn move_cursor(&mut self, motion: viewer::Motion) -> Task<Message> {
        let pane = self.active;
        let line_height = self.line_height();
        let Some(v) = self.active_viewer_mut() else {
            return Task::none();
        };
        v.move_caret(motion);
        let (line, _) = v.caret.unwrap_or((0, 0));
        // Keep the cursor line within the viewport (in display rows, so folds
        // above it are accounted for).
        let top = v.row_of(line) as f32 * line_height;
        let bottom = top + line_height;
        if top < v.scroll_y {
            v.scroll_y = top;
        } else if bottom > v.scroll_y + v.viewport_h {
            v.scroll_y = bottom - v.viewport_h;
        }
        let y = v.scroll_y;
        let scroll = operation::scroll_to(ui::code_scroll_id(pane), AbsoluteOffset { x: 0.0, y });
        let follow = self.follow_caret(scroll);
        Task::batch([follow, self.sync_reading_context()])
    }

    /// Toggle the fold enclosing the caret (`za`).
    fn fold_toggle_at_cursor(&mut self) {
        if let Some(v) = self.active_viewer_mut() {
            let line = v.caret.map(|(l, _)| l).unwrap_or(0);
            if let Some(header) = v.fold_header_for(line) {
                v.toggle_fold(header);
            }
        }
    }

    /// Collapse (`zM`) or expand (`zR`) every fold in the active pane.
    fn fold_all(&mut self, collapse: bool) {
        if let Some(v) = self.active_viewer_mut() {
            if collapse {
                v.collapse_all();
            } else {
                v.expand_all();
            }
        }
    }

    /// Toggle "skim" for the active file: fold every function/method body down
    /// to its signature (which still shows its inline summary), so the file
    /// reads as an annotated table of contents. Uses the symbol index to fold
    /// only bodies, leaving impl/mod blocks open so every signature stays shown.
    fn skim_active_file(&mut self) {
        let Some(v) = self.active_viewer() else {
            return;
        };
        let sig_lines: Vec<usize> = self
            .symbol_index_by_file
            .get(&v.abs)
            .map(|syms| {
                syms.iter()
                    .filter(|s| matches!(s.kind.as_str(), "function" | "method"))
                    .map(|s| s.line.saturating_sub(1)) // 1-based symbol line → 0-based
                    .collect()
            })
            .unwrap_or_default();
        if let Some(v) = self.active_viewer_mut() {
            v.skim_bodies(&sig_lines);
        }
    }

    /// Move the cursor to the current find match and scroll it into view.
    fn jump_to_find_match(&mut self) -> Task<Message> {
        let Some((line, col, _)) = self.find.current_match() else {
            return Task::none();
        };
        let pane = self.active;
        let line_height = self.line_height();
        let Some(v) = self.active_viewer_mut() else {
            return Task::none();
        };
        v.caret = Some((line, col));
        // Center-ish the match line (display rows account for folds).
        let top = v.row_of(line) as f32 * line_height;
        if top < v.scroll_y || top + line_height > v.scroll_y + v.viewport_h {
            v.scroll_y = (top - v.viewport_h / 3.0).max(0.0);
        }
        let y = v.scroll_y;
        let scroll = operation::scroll_to(ui::code_scroll_id(pane), AbsoluteOffset { x: 0.0, y });
        self.follow_caret(scroll)
    }

    /// Extra span highlights for the code view of `pane`: find matches, or
    /// (when not finding) the occurrences of the identifier under the cursor
    /// and the matching bracket.
    pub fn code_highlights(&self, pane: usize, v: &Viewer) -> Vec<codeview::Hl> {
        use codeview::{Hl, HlKind};
        let mut out = Vec::new();
        if pane != self.active {
            return out;
        }

        // Diagnostic underlines (always shown, from the LSP server).
        if let Some(lang) = v.lang_key
            && let Some(LspSlot::Ready(client)) = self.lsp.get(lang)
        {
            let utf16 = client.encoding == lsp::client::PositionEncoding::Utf16;
            for d in client.diagnostics(&v.abs) {
                let raw = v.source_line(d.line).unwrap_or("");
                let c0 = viewer::display_col_from_char(raw, d.char_start, utf16);
                let c1 = viewer::display_col_from_char(raw, d.char_end, utf16).max(c0 + 1);
                out.push(Hl {
                    line: d.line,
                    col0: c0,
                    col1: c1,
                    kind: match d.severity {
                        1 => HlKind::DiagError,
                        2 => HlKind::DiagWarn,
                        _ => HlKind::DiagHint,
                    },
                });
            }
        }

        if self.find.open {
            for (i, &(line, col0, col1)) in self.find.matches.iter().enumerate() {
                out.push(Hl {
                    line,
                    col0,
                    col1,
                    kind: if i == self.find.current {
                        HlKind::FindCurrent
                    } else {
                        HlKind::FindMatch
                    },
                });
            }
            return out;
        }

        // Cursor-derived aids, only while reading (code has focus).
        if !self.code_focused {
            return out;
        }
        let Some((line, col)) = v.caret else {
            return out;
        };

        // Occurrences of the identifier under the cursor (2+ to be useful).
        if let Some(word) = analyze::word_at(&v.lines, line, col) {
            let occ = analyze::occurrences(&word, &v.lines, 500);
            if occ.len() > 1 {
                for (l, c0, c1) in occ {
                    out.push(Hl {
                        line: l,
                        col0: c0,
                        col1: c1,
                        kind: HlKind::Occurrence,
                    });
                }
            }
        }

        // Matching bracket pair.
        if let Some((ml, mc)) = analyze::matching_bracket(&v.lines, line, col) {
            out.push(Hl {
                line,
                col0: col,
                col1: col + 1,
                kind: HlKind::Bracket,
            });
            out.push(Hl {
                line: ml,
                col0: mc,
                col1: mc + 1,
                kind: HlKind::Bracket,
            });
        }
        out
    }

    /// Inline blame annotation for the caret line: `author, when · summary`.
    pub fn blame_annotation(&self, v: &Viewer) -> Option<(usize, String)> {
        let git = v.git.as_ref()?;
        let (line, _) = v.caret?;
        let b = git.blame_for(line)?;
        if b.commit.is_empty() {
            return None;
        }
        let text = if b.uncommitted {
            "· Uncommitted change".to_string()
        } else {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(b.time);
            let mut summary = b.summary.clone();
            if summary.chars().count() > 60 {
                summary = summary.chars().take(59).collect::<String>() + "…";
            }
            format!(
                "{}, {} · {}",
                b.author,
                git::relative_time(b.time, now),
                summary
            )
        };
        Some((line, text))
    }

    /// Sticky-scroll header lines for a viewer at its current scroll position.
    pub fn sticky_headers(&self, v: &Viewer) -> Vec<usize> {
        let row = (v.scroll_y / self.line_height()) as usize;
        let first_visible = v.line_at_row(row);
        // Read enclosing headers off the precomputed fold ranges — cheap enough
        // to recompute each frame, so sticky scroll stays smooth in huge files.
        analyze::sticky_headers(&v.folds, first_visible, 5)
    }

    fn rel_of(&self, abs: &Path) -> String {
        self.project
            .as_ref()
            .and_then(|p| abs.strip_prefix(&p.root).ok())
            .map(|r| r.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|| abs.display().to_string())
    }
}

/// Set up a proxied-process transport: ask clew-server (via `tx`) to spawn `cmd`
/// and bridge its stdio to two in-memory streams. Returns the caller's (stdin,
/// stdout) ends plus the feed the caller registers so `ProcessOutput` events for
/// `proc` reach the stdout bridge. Shared by the LSP and DAP proxies.
fn proxy_transport(
    tx: &tokio::sync::mpsc::UnboundedSender<clew_protocol::ClientMessage>,
    proc: u64,
    spawn: clew_protocol::Request,
) -> (
    tokio::io::DuplexStream,
    tokio::io::DuplexStream,
    tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
) {
    let (client_stdin, mut stdin_reader) = tokio::io::duplex(64 * 1024);
    let (mut stdout_writer, client_stdout) = tokio::io::duplex(64 * 1024);
    let (feed_tx, mut feed_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();

    // `spawn` is SpawnProcess (client-resolved, e.g. a debug adapter) or SpawnLsp
    // (server-resolved, so a remote runs its own language server).
    let _ = tx.send(clew_protocol::ClientMessage { id: 0, request: spawn });
    // Forward what the client writes → the process's stdin.
    let tx_in = tx.clone();
    tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match stdin_reader.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let msg = clew_protocol::ClientMessage {
                        id: 0,
                        request: clew_protocol::Request::ProcessInput {
                            proc,
                            data: buf[..n].to_vec(),
                        },
                    };
                    if tx_in.send(msg).is_err() {
                        break;
                    }
                }
            }
        }
    });
    // Pump the process's stdout (fed by ProcessOutput events) → the client.
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        while let Some(data) = feed_rx.recv().await {
            if stdout_writer.write_all(&data).await.is_err() {
                break;
            }
        }
    });
    (client_stdin, client_stdout, feed_tx)
}

async fn pick_folder() -> Option<PathBuf> {
    rfd::AsyncFileDialog::new()
        .set_title("Open a project folder")
        .pick_folder()
        .await
        .map(|handle| handle.path().to_path_buf())
}

/// Native picker for an SSH private-key file (the Connect form's "Browse…").
async fn pick_file() -> Option<PathBuf> {
    rfd::AsyncFileDialog::new()
        .set_title("Choose an SSH private key")
        .pick_file()
        .await
        .map(|handle| handle.path().to_path_buf())
}


async fn load_file(
    pane: usize,
    abs: PathBuf,
    target: Option<usize>,
) -> (usize, PathBuf, Option<usize>, Result<String, String>) {
    let read_path = abs.clone();
    let result = tokio::task::spawn_blocking(move || read_text_file(&read_path))
        .await
        .unwrap_or_else(|e| Err(e.to_string()));
    (pane, abs, target, result)
}

fn read_text_file(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(format!(
            "file too large ({:.1} MB, limit {} MB)",
            bytes.len() as f64 / (1024.0 * 1024.0),
            MAX_FILE_BYTES / (1024 * 1024)
        ));
    }
    if bytes.iter().take(8192).any(|&b| b == 0) {
        return Err("binary file".to_string());
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod app_tests {
    use super::*;

    #[test]
    fn dart_fn_detail_extracts_full_body_not_duplicated_header() {
        // A doc-commented Dart block function: Dart tags only the signature line,
        // so without the brace-extension the "body" would be the header twice.
        let dir = std::env::temp_dir().join("clew-dart-detail-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("calc.dart");
        std::fs::write(
            &file,
            "/// Parse everything.\ndouble parseAll(int x) {\n  var e = x + 1;\n  return e.toDouble();\n}\n",
        )
        .unwrap();
        let (sig, body, _) =
            gather_fn_detail_input(file, "parseAll", &HashMap::new()).expect("detail");
        assert!(sig.contains("parseAll"));
        assert!(body.contains("var e = x + 1"), "body missing statements: {body:?}");
        assert!(body.contains("return e.toDouble()"), "body missing return: {body:?}");
        // The header must appear once in the body, not duplicated.
        assert_eq!(body.matches("parseAll").count(), 1, "duplicated header: {body:?}");
    }

    #[test]
    fn fn_body_end_matches_and_handles_nested_and_bodyless() {
        // Signature line + block body → reach the closing brace on line 4.
        let lines = ["Expr parseAll() {", "  var e = expr();", "  return e;", "}", "otherFn()"];
        assert_eq!(fn_body_end(&lines, 0), Some(4)); // lines[0..4] = the function
        // Nested braces are balanced correctly.
        let nested = ["fn f() {", "  if x { g(); }", "}"];
        assert_eq!(fn_body_end(&nested, 0), Some(3));
        // No brace (expression-bodied) → None (caller keeps the single line).
        assert_eq!(fn_body_end(&["double get m => x;"], 0), None);
    }

    #[test]
    fn fn_body_end_skips_dart_named_parameter_braces() {
        // A Dart multi-line signature whose named parameters use `{ }` *inside*
        // the parens. Naive brace matching stops at the named-parameter `}` on
        // line 4 and returns just the signature; fn_body_end must skip those and
        // reach the real body's closing brace on line 7.
        let lines = [
            "Future<void> initializeRust(",              // 0
            "  AssignRustSignal<String, dynamic> sig, {", // 1  (named-param '{')
            "  String? compiledLibPath,",                 // 2
            "}) async {",                                 // 3  ('}' closes params, '{' opens body)
            "  if (compiledLibPath != null) {",           // 4
            "    setPath(compiledLibPath);",              // 5
            "  }",                                        // 6
            "}",                                          // 7  body close
            "void next() {}",                             // 8
        ];
        assert_eq!(fn_body_end(&lines, 0), Some(8)); // lines[0..8] = the whole function
        // A single-line signature + body still works.
        assert_eq!(fn_body_end(&["fn f() {", "  g();", "}"], 0), Some(3));
        // A bodyless declaration (abstract / trait signature) → None.
        assert_eq!(fn_body_end(&["void doThing(int a);"], 0), None);
    }

    /// Each test gets its own directory: tests run in parallel and would
    /// otherwise race on remove_dir_all/create of a shared fixture.
    fn fixture_project(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("clew-app-test-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("src/lib.rs"),
            "pub struct Point { x: f64 }\n\npub fn origin() -> Point {\n    Point { x: 0.0 }\n}\n",
        )
        .unwrap();
        std::fs::write(dir.join("notes.txt"), "needle in notes\n").unwrap();
        dir.canonicalize().unwrap()
    }

    /// Drive the update loop the way the runtime would, executing the
    /// blocking parts inline instead of through iced Tasks.
    fn open_synchronously(app: &mut App, rel: &str, line: Option<usize>) {
        let abs = app.project.as_ref().unwrap().root.join(rel);
        let pane = app.active;
        let _ = app.update(Message::OpenRel {
            rel: rel.to_string(),
            line,
        });
        let content = read_text_file(&abs).unwrap();
        let _ = app.update(Message::FileLoaded {
            pane,
            abs: abs.clone(),
            target: line,
            result: Ok(content.clone()),
        });
        let lang = highlight::detect(&abs);
        let lines = highlight::highlight_lines(&content, lang);
        let symbols = lang
            .map(|k| outline::extract(&content, k))
            .unwrap_or_default();
        let docs = lang.map(|k| docs::extract(&content, k, &symbols)).unwrap_or_default();
        let inactive = lang
            .map(|k| inactive::inactive_lines(&content, k, &inactive::Target::host()))
            .unwrap_or_default();
        let _ = app.update(Message::Highlighted {
            abs,
            lines,
            symbols,
            docs,
            inactive,
        });
    }

    fn scanned_app(tag: &str) -> App {
        let root = fixture_project(tag);
        let mut app = App::blank();
        let _ = app.update(Message::ScanDone(fs_scan::scan(root)));
        app
    }

    #[test]
    fn full_reading_flow() {
        let mut app = scanned_app("reading");
        assert!(app.project.is_some());
        assert_eq!(app.project.as_ref().unwrap().files.len(), 2);

        // Open a file at a line.
        open_synchronously(&mut app, "src/lib.rs", Some(3));
        let v = app.active_viewer().unwrap();
        assert_eq!(v.rel, "src/lib.rs");
        assert!(v.highlighted);
        assert_eq!(v.target_line, Some(3));
        assert_eq!(v.lines.len(), 5);

        // Outline extracted for the current file.
        let names: Vec<&str> = v.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"origin"), "outline: {names:?}");

        // Open a second file, then navigate back and forward.
        open_synchronously(&mut app, "notes.txt", None);
        assert_eq!(app.active_viewer().unwrap().rel, "notes.txt");
        assert!(app.history.can_back());

        let back = app.history.back().unwrap();
        assert!(back.path.ends_with("src/lib.rs"));
        assert_eq!(back.line, Some(3));
        let fwd = app.history.forward().unwrap();
        assert!(fwd.path.ends_with("notes.txt"));
    }

    // ---- Explain-domain handler regressions (guard the eval-campaign fixes) ---

    #[test]
    fn reexplain_on_unexplained_node_does_not_start_a_project_pass() {
        // Fix: a single "Re-explain" click on a never-explained node must NOT
        // kick off the whole-project pass (thousands of LLM calls) — it should
        // point the user at the explicit Explain-All instead.
        let mut app = scanned_app("reexplain-guard");
        app.llm_available = true; // else it returns early on a missing key
        app.explain_view = Some(explain::Node::Function {
            file: app.project.as_ref().unwrap().root.join("src/lib.rs"),
            name: "origin".into(),
        });
        assert!(app.explanations.is_empty());
        let _ = app.update(Message::ReexplainNode);
        assert!(!app.explaining, "must not start a project pass on an unexplained node");
        assert!(app.status.contains("Nothing to re-explain"), "status: {}", app.status);
    }

    #[test]
    fn cancel_explain_stops_and_clears_progress() {
        // Fix: a running Explain pass must be cancellable.
        let mut app = scanned_app("cancel-explain");
        app.explaining = true;
        app.explain_progress = Some((3, 10));
        let _ = app.update(Message::CancelExplain);
        assert!(!app.explaining);
        assert_eq!(app.explain_progress, None);
        assert!(app.status.contains("cancelled"), "status: {}", app.status);
    }

    #[test]
    fn explain_done_from_a_stale_generation_is_ignored() {
        // A result from a superseded pass (older generation) must be dropped, so
        // a cancelled/restarted pass can't be clobbered by a late arrival.
        let mut app = scanned_app("explain-done-stale");
        let root = app.project.as_ref().unwrap().root.clone();
        app.explaining = true;
        app.explain_gen = 5;
        let _ = app.update(Message::ExplainDone {
            root,
            generation: 4, // stale
            cache: explain::Cache::new(),
            failed: 0,
            auth_error: None,
        });
        assert!(app.explaining, "a stale ExplainDone must not clear the running flag");
    }

    #[test]
    fn finder_flow() {
        let mut app = scanned_app("finder");

        let _ = app.update(Message::FinderOpened(FinderMode::Files));
        assert!(app.finder.open);
        assert!(!app.finder.results.is_empty());

        let _ = app.update(Message::FinderQueryChanged("librs".to_string()));
        let files = app.project.as_ref().unwrap().files.clone();
        let top = files[app.finder.results[0]].rel.clone();
        assert_eq!(top, "src/lib.rs");

        // Confirm closes the finder.
        let _ = app.update(Message::FinderConfirm);
        assert!(!app.finder.open);
    }

    #[test]
    fn incremental_reindex_on_change_and_delete() {
        let mut app = scanned_app("reindex");
        let files = app.project.as_ref().unwrap().files.clone();
        let root = app.project.as_ref().unwrap().root.clone();
        let _ = app.update(Message::SymbolIndexDone {
            root,
            indexed: index::build_indexed(files),
        });
        let abs = app.project.as_ref().unwrap().root.join("src/lib.rs");
        assert!(app.symbol_index.iter().any(|e| e.name == "origin"));
        assert!(app.registry.version(&abs).is_some());

        // An external edit that renames the function re-indexes just that file.
        let new = std::sync::Arc::new("pub fn renamed() -> u8 {\n    1\n}\n".to_string());
        let ev = watch::FileEvent::Modified(watch::Changed {
            path: abs.clone(),
            hash: 424242,
            content: new,
        });
        let _ = app.update(Message::FilesRehashed { events: vec![ev], fs_structural: false });
        assert!(app.symbol_index.iter().any(|e| e.name == "renamed"));
        assert!(!app.symbol_index.iter().any(|e| e.name == "origin"));
        assert_eq!(app.registry.version(&abs), Some(424242));

        // Deleting the file drops its symbols and forgets its version.
        let _ = app.update(Message::FilesRehashed {
            events: vec![watch::FileEvent::Deleted(abs.clone())],
            fs_structural: false,
        });
        assert!(!app.symbol_index.iter().any(|e| e.name == "renamed"));
        assert_eq!(app.registry.version(&abs), None);
        assert!(!app.symbol_index_by_file.contains_key(&abs));
    }

    #[test]
    fn tree_update_swaps_files_and_ignores_stale_root() {
        let mut app = scanned_app("tree");
        let root = app.project.as_ref().unwrap().root.clone();
        let before = app.project.as_ref().unwrap().files.len();

        // A new file on disk, applied via a rescan result, grows the file list
        // without a full project reopen.
        std::fs::write(root.join("src/newmod.rs"), "pub fn brand_new() {}\n").unwrap();
        let _ = app.update(Message::TreeUpdated(fs_scan::scan(root.clone())));
        let after = app.project.as_ref().unwrap().files.len();
        assert_eq!(after, before + 1);
        assert!(
            app.project
                .as_ref()
                .unwrap()
                .files
                .iter()
                .any(|f| f.rel.ends_with("newmod.rs"))
        );

        // A rescan for a different root (a stale one) is ignored.
        let stale = fs_scan::ScanResult {
            root: PathBuf::from("/definitely/not/this/project"),
            tree: fs_scan::DirNode::default(),
            files: Vec::new(),
            truncated: false,
        };
        let _ = app.update(Message::TreeUpdated(stale));
        assert_eq!(app.project.as_ref().unwrap().files.len(), after);
    }

    #[test]
    fn symbol_finder_flow() {
        let mut app = scanned_app("symbols");
        // Build the index synchronously (the runtime does this in a task).
        let files = app.project.as_ref().unwrap().files.clone();
        let root = app.project.as_ref().unwrap().root.clone();
        let _ = app.update(Message::SymbolIndexDone {
            root,
            indexed: index::build_indexed(files),
        });
        assert!(!app.indexing);
        assert!(app.symbol_index.len() >= 2, "{:?}", app.symbol_index);

        let _ = app.update(Message::FinderOpened(FinderMode::Symbols));
        let _ = app.update(Message::FinderQueryChanged("origin".to_string()));
        assert!(!app.finder.results.is_empty());
        let entry = &app.symbol_index[app.finder.results[0]];
        assert_eq!(entry.name, "origin");
        assert_eq!(entry.line, 3);

        // Confirm records the jump in history.
        let _ = app.update(Message::FinderConfirm);
        assert!(!app.finder.open);
        let _ = app.update(Message::GoBack); // no-op or previous loc; must not panic
    }

    #[test]
    fn goto_line_via_finder() {
        let mut app = scanned_app("goto");
        open_synchronously(&mut app, "src/lib.rs", None);

        let _ = app.update(Message::GotoLineRequested);
        assert!(app.finder.open);
        let _ = app.update(Message::FinderQueryChanged(":4".to_string()));
        assert_eq!(app.finder.goto_line(), Some(4));
        let _ = app.update(Message::FinderConfirm);
        assert!(!app.finder.open);
        assert_eq!(app.active_viewer().unwrap().target_line, Some(4));
    }

    #[test]
    fn split_view_routes_to_active_pane() {
        let mut app = scanned_app("split");
        open_synchronously(&mut app, "src/lib.rs", None);

        let _ = app.update(Message::ToggleSplit);
        assert!(app.split);
        assert_eq!(app.active, 1);
        // Split duplicates the current file.
        assert_eq!(app.panes[1].as_ref().unwrap().rel, "src/lib.rs");

        // Opening now targets pane 1; pane 0 keeps its file.
        open_synchronously(&mut app, "notes.txt", None);
        assert_eq!(app.panes[1].as_ref().unwrap().rel, "notes.txt");
        assert_eq!(app.panes[0].as_ref().unwrap().rel, "src/lib.rs");

        // Refocus pane 0 and close the split.
        let _ = app.update(Message::PaneFocused(0));
        assert_eq!(app.active, 0);
        let _ = app.update(Message::ToggleSplit);
        assert!(!app.split);
        assert!(app.panes[1].is_none());
    }

    #[test]
    fn selection_and_copy_state() {
        let mut app = scanned_app("select");
        open_synchronously(&mut app, "src/lib.rs", None);

        let _ = app.update(Message::SelectStart { pane: 0, line: 1, col: 4 });
        assert!(app.selecting);
        assert_eq!(app.active_viewer().unwrap().caret, Some((1, 4)));
        let _ = app.update(Message::SelectDrag { pane: 0, line: 3, col: 2 });
        let _ = app.update(Message::SelectEnd);
        assert!(!app.selecting);

        let v = app.active_viewer().unwrap();
        assert_eq!(v.selection_ordered(), Some(((1, 4), (3, 2))));
        assert_eq!(v.caret, Some((3, 2)));
        let text = v.selected_text().unwrap();
        assert!(text.contains("origin"), "{text}");

        // Esc clears the selection.
        let _ = app.update(Message::KeyPressed(
            keyboard::Key::Named(keyboard::key::Named::Escape),
            keyboard::Modifiers::default(),
        ));
        assert!(app.active_viewer().unwrap().selection.is_none());
    }

    #[test]
    fn bookmark_toggle_persists_in_project() {
        let mut app = scanned_app("bookmark");
        let root = app.project.as_ref().unwrap().root.clone();
        open_synchronously(&mut app, "src/lib.rs", Some(3));

        let _ = app.update(Message::BookmarkToggled);
        assert_eq!(app.bookmarks.len(), 1);
        assert_eq!(app.bookmarks[0].rel, "src/lib.rs");
        assert_eq!(app.bookmarks[0].line, 3);
        assert!(root.join(".clew/bookmarks.json").exists());
        assert_eq!(bookmarks::load(&root), app.bookmarks);

        // Toggling again removes it and cleans up the store file; the .clew
        // directory itself stays (consent record).
        let _ = app.update(Message::BookmarkToggled);
        assert!(app.bookmarks.is_empty());
        assert!(!root.join(".clew/bookmarks.json").exists());
    }

    #[test]
    fn consent_gates_project_open() {
        let root = fixture_project("consent");

        // Picking a folder without .clew opens the consent modal, not the project.
        let mut app = App::blank();
        let _ = app.update(Message::FolderPicked(Some(root.clone())));
        assert_eq!(app.pending_consent.as_deref(), Some(root.as_path()));
        assert!(app.project.is_none() && !app.scanning);
        assert!(!root.join(".clew").exists());

        // Denied: nothing is created, no project opens, modal dismissed.
        let _ = app.update(Message::ConsentDenied);
        assert!(app.pending_consent.is_none());
        assert!(app.project.is_none() && !app.scanning);
        assert!(!root.join(".clew").exists());
        assert!(app.status.contains("not allowed"), "{}", app.status);

        // Allowed: .clew is created and the scan starts.
        let mut app = App::blank();
        let _ = app.update(Message::FolderPicked(Some(root.clone())));
        let _ = app.update(Message::ConsentAllowed);
        assert!(root.join(".clew").is_dir());
        assert!(app.scanning);
        assert!(app.pending_consent.is_none());

        // A project with .clew already present skips the consent modal:
        // FolderPicked goes straight to scanning.
        let mut app2 = App::blank();
        let _ = app2.update(Message::FolderPicked(Some(root.clone())));
        assert!(app2.scanning, "existing .clew must skip the prompt");
        assert!(app2.pending_consent.is_none());
    }

    #[test]
    fn auto_refresh_throttles_but_manual_does_not() {
        use std::time::{Duration, Instant};

        let mut app = App::blank();
        app.llm_available = true;

        // Nothing explained yet → auto-refresh is a no-op (first build is manual).
        let _ = app.request_auto_refresh();
        assert!(app.last_auto_refresh.is_none() && !app.refresh_pending);

        // Seed one explanation so there's something to keep fresh.
        app.explanations.insert(
            explain::Node::File(PathBuf::from("a.rs")),
            explain::Cached { summary: "s".into(), prompt_hash: 1, detail: None },
        );

        // First change fires immediately (no prior refresh): stamps the cooldown.
        let _ = app.request_auto_refresh();
        let first = app.last_auto_refresh.expect("cooldown stamped");
        assert!(!app.refresh_pending, "a fresh pass isn't 'pending'");

        // A second change inside the 30s window is deferred, not fired: the stamp
        // is unchanged and the pass is now pending.
        let _ = app.request_auto_refresh();
        assert_eq!(app.last_auto_refresh, Some(first), "cooldown not restamped");
        assert!(app.refresh_pending, "change during cooldown is queued");

        // Once the window has passed, the queued change fires and restamps.
        app.last_auto_refresh = Some(Instant::now() - Duration::from_secs(31));
        let _ = app.request_auto_refresh();
        assert!(app.last_auto_refresh.unwrap() > first, "restamped after cooldown");
        assert!(!app.refresh_pending, "queued pass consumed");

        // A manual refresh ignores the cooldown entirely: fresh stamp even though
        // one was just set microseconds ago.
        let before = app.last_auto_refresh.unwrap();
        let _ = app.update(Message::RefreshAll);
        assert!(app.last_auto_refresh.unwrap() >= before, "manual bypasses cooldown");
        assert!(!app.refresh_pending);
    }

    #[test]
    fn search_flow_message_wiring() {
        let mut app = scanned_app("search");

        let files = app.project.as_ref().unwrap().files.clone();
        let result = search::search(
            files,
            search::SearchOptions {
                query: "needle".to_string(),
                ..Default::default()
            },
        );
        assert_eq!(result.hits.len(), 1);
        let _ = app.update(Message::SearchDone { result });
        assert_eq!(app.search.hits.len(), 1);
        assert_eq!(app.search.hits[0].rel, "notes.txt");

        // Clicking a hit opens the file at that line.
        let hit = app.search.hits[0].clone();
        let _ = app.update(Message::OpenAbs {
            abs: hit.abs.clone(),
            line: Some(hit.line),
            push: true,
        });
        let content = read_text_file(&hit.abs).unwrap();
        let _ = app.update(Message::FileLoaded {
            pane: 0,
            abs: hit.abs,
            target: Some(hit.line),
            result: Ok(content),
        });
        assert_eq!(app.active_viewer().unwrap().target_line, Some(1));
    }

    #[test]
    fn font_size_rescales_scroll() {
        let mut app = scanned_app("font");
        open_synchronously(&mut app, "src/lib.rs", None);
        app.panes[0].as_mut().unwrap().scroll_y = 40.0; // line 2 at 20px
        let _ = app.update(Message::FontSizeDelta(2.0));
        assert_eq!(app.font_size, 15.0);
        let v = app.active_viewer().unwrap();
        assert!((v.scroll_y - 44.0).abs() < 0.01, "{}", v.scroll_y); // 2 * 22px
        let _ = app.update(Message::FontSizeReset);
        assert_eq!(app.font_size, DEFAULT_FONT_SIZE);
    }

    #[test]
    fn binary_and_oversized_files_are_rejected() {
        let dir = std::env::temp_dir().join("clew-guard-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("blob.bin");
        std::fs::write(&bin, [0u8, 159, 146, 150]).unwrap();
        assert!(read_text_file(&bin).unwrap_err().contains("binary"));

        let big = dir.join("huge.txt");
        std::fs::write(&big, vec![b'a'; MAX_FILE_BYTES + 1]).unwrap();
        assert!(read_text_file(&big).unwrap_err().contains("too large"));
    }

    // ---------------------------------------------------------------- LSP

    /// Opening a Rust file with no installed server and no override prompts
    /// for a download; dismissing marks the language unsupported (falls back
    /// to ⌘T).
    #[test]
    fn opening_rust_prompts_for_server_download() {
        // Point the store at a guaranteed-empty dir so nothing is "installed".
        let store = std::env::temp_dir().join("clew-lsp-empty-store");
        let _ = std::fs::remove_dir_all(&store);
        // SAFETY: test-only env mutation.
        unsafe { std::env::set_var("CLEW_DATA_DIR", &store) };

        let mut app = scanned_app("lsp-prompt");
        open_synchronously(&mut app, "src/lib.rs", None);

        let consent = app.pending_lsp_consent.as_ref().expect("download prompt");
        assert_eq!(consent.server_name, "rust-analyzer");
        assert!(matches!(app.lsp.get("rust"), Some(LspSlot::AwaitingConsent)));

        let _ = app.update(Message::LspConsentDismissed);
        assert!(app.pending_lsp_consent.is_none());
        assert!(matches!(app.lsp.get("rust"), Some(LspSlot::Unsupported(_))));

        unsafe { std::env::remove_var("CLEW_DATA_DIR") };
    }

    /// The server panel lists only project-relevant languages — a Rust
    /// project does not show c/cpp.
    #[test]
    fn managed_languages_are_project_relevant() {
        let app = scanned_app("lsp-langs"); // fixture has src/lib.rs (Rust) + notes.txt
        assert_eq!(app.managed_languages(), vec!["rust".to_string()]);
        // notes.txt has no server; c/cpp are not in the project.
        assert!(!app.managed_languages().iter().any(|l| l == "cpp"));
    }

    /// Right-click opens a navigation menu carrying the clicked position;
    /// choosing an action closes it.
    #[test]
    fn context_menu_flow() {
        let mut app = scanned_app("ctxmenu");
        open_synchronously(&mut app, "src/lib.rs", None);

        let _ = app.update(Message::ContextMenuOpened {
            pane: 0,
            line: 2,
            col: 7,
            x: 120.0,
            y: 40.0,
        });
        let menu = app.context_menu.expect("menu open");
        assert_eq!((menu.line, menu.col), (2, 7));

        // Choosing an action closes the menu (and dispatches a goto).
        let _ = app.update(Message::ContextGoto(GotoKind::Definition));
        assert!(app.context_menu.is_none());

        // Outside click / Esc closes without acting.
        let _ = app.update(Message::ContextMenuOpened {
            pane: 0,
            line: 0,
            col: 0,
            x: 0.0,
            y: 0.0,
        });
        let _ = app.update(Message::ContextMenuClosed);
        assert!(app.context_menu.is_none());
    }

    /// A Go project surfaces the gopls row (toolchain-installed server).
    #[test]
    fn go_project_is_served_by_gopls() {
        let dir = std::env::temp_dir().join("clew-go-proj");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("main.go"), "package main\nfunc main() {}\n").unwrap();
        let root = dir.canonicalize().unwrap();

        let mut app = App::blank();
        let _ = app.update(Message::ScanDone(fs_scan::scan(root)));
        assert!(app.managed_languages().contains(&"go".to_string()));
        assert_eq!(
            lsp::registry::default_for_language("go").unwrap().name,
            "gopls"
        );
    }

    /// A custom `command` in `.clew/lsp.toml` bypasses the store and starts
    /// directly — no download prompt.
    #[test]
    fn custom_command_starts_without_prompt() {
        let root = fixture_project("lsp-escape");
        std::fs::create_dir_all(root.join(".clew")).unwrap();
        std::fs::write(
            root.join(".clew/lsp.toml"),
            "[rust]\ncommand = \"/nonexistent/rust-analyzer\"\n",
        )
        .unwrap();
        let mut app = App::blank();
        let _ = app.update(Message::ScanDone(fs_scan::scan(root)));
        open_synchronously(&mut app, "src/lib.rs", None);

        assert!(app.pending_lsp_consent.is_none());
        assert!(matches!(app.lsp.get("rust"), Some(LspSlot::Starting)));
    }

    /// A definition result jumps to the target line and records history.
    #[test]
    fn definition_result_jumps_and_records_history() {
        let mut app = scanned_app("lsp-jump");
        open_synchronously(&mut app, "notes.txt", None);
        let target = app.project.as_ref().unwrap().root.join("src/lib.rs");

        let _ = app.update(Message::DefinitionResult {
            result: Ok(vec![lsp::client::Target {
                path: target.clone(),
                line: 2, // 0-based → jump to line 3
                character: 7,
            }]),
        });
        // open_file kicked off an async load; feed the FileLoaded it awaits.
        let content = read_text_file(&target).unwrap();
        let _ = app.update(Message::FileLoaded {
            pane: 0,
            abs: target,
            target: Some(3),
            result: Ok(content),
        });
        assert_eq!(app.active_viewer().unwrap().rel, "src/lib.rs");
        // The cursor moves to the jump target (line 3 → 0-based line 2).
        assert_eq!(app.active_viewer().unwrap().caret, Some((2, 0)));
        assert!(app.history.can_back(), "definition jump is undoable");
    }

    /// Full chain against a real rust-analyzer via the escape hatch: scan →
    /// open → start server → didOpen → definition → jump. Ignored by default
    /// (spawns rust-analyzer); run explicitly.
    #[tokio::test]
    #[ignore]
    async fn live_goto_definition_through_app() {
        let ra = PathBuf::from(std::env::var("HOME").unwrap()).join(".cargo/bin/rust-analyzer");
        assert!(ra.exists(), "needs rust-analyzer at {ra:?}");

        // Cargo project with origin() defined and called.
        let root = std::env::temp_dir().join("clew-app-live-lsp");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join(".clew")).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"t\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(
            root.join(".clew/lsp.toml"),
            format!("[rust]\ncommand = {:?}\n", ra.to_string_lossy()),
        )
        .unwrap();
        let src = "fn origin() -> i32 {\n    0\n}\n\nfn main() {\n    let _ = origin();\n}\n";
        std::fs::write(root.join("src/main.rs"), src).unwrap();
        let root = root.canonicalize().unwrap();

        let mut app = App::blank();
        let _ = app.update(Message::ScanDone(fs_scan::scan(root.clone())));
        open_synchronously(&mut app, "src/main.rs", None);

        // Start the real server (the escape hatch resolved it) and register it.
        let server = app.lsp_config.resolve("rust").unwrap();
        let client = lsp::client::LspClient::start(&server.command.unwrap(), &[], &root, None)
            .await
            .unwrap();
        let _ = app.update(Message::LspStartResult {
            language: "rust".into(),
            result: Ok(client.clone()),
        });

        // Simulate ⌘-click on the `origin()` call (line 5, inside the name).
        let v = app.active_viewer().unwrap();
        let utf16 = client.encoding == lsp::client::PositionEncoding::Utf16;
        let ch = viewer::character_offset(v.source_line(5).unwrap(), 12, utf16);
        let path = v.abs.clone();

        // Poll until rust-analyzer has indexed.
        let mut targets = Vec::new();
        for _ in 0..40 {
            targets = client.definition(&path, 5, ch).await.unwrap_or_default();
            if !targets.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        assert!(!targets.is_empty(), "expected a definition");

        // Feed the result through the app and complete the jump.
        let _ = app.update(Message::DefinitionResult {
            result: Ok(targets.clone()),
        });
        let content = read_text_file(&targets[0].path).unwrap();
        let _ = app.update(Message::FileLoaded {
            pane: 0,
            abs: targets[0].path.clone(),
            target: Some(targets[0].line + 1),
            result: Ok(content),
        });
        // Jumped to the `origin` definition on line 1 (1-based).
        assert_eq!(app.active_viewer().unwrap().target_line, Some(1));
        assert!(app.history.can_back());
    }
}
