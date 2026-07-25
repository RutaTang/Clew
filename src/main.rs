//! clew — a code reader.
//!
//! v2: gitignore-aware file tree, virtualized tree-sitter code viewer,
//! fuzzy file finder (Cmd+P), project symbol search (Cmd+T), full-text
//! search (Cmd+Shift+F), navigation history, outline, split view (Cmd+\),
//! line selection + copy (Cmd+C), bookmarks (Cmd+D), go-to-line (:N).

mod dap;
pub use clew_core::docs;
pub use clew_core::explain;
// Moved into the shared `clew-core` crate (used by both the GUI and the headless
// server); re-exported so existing `crate::fs_scan` / `crate::search` paths hold.
pub use clew_core::fs_scan;
pub use clew_core::git;
pub use clew_core::inactive;
pub use clew_core::incremental;
pub use clew_core::llm;
pub use clew_core::lsp;
#[cfg(target_os = "macos")]
mod macos;
pub use clew_core::outline;
pub use clew_core::search;
mod ui;
pub use clew_core::embed;

// `App`'s update / message-handling methods live here, split by area. The model
// (`App` + its state sub-structs) and the `Message` enum live in `app::state` /
// `app::message`; they are re-exported here so `crate::App` / `crate::Message`
// paths hold across the codebase.

// Feature modules, grouped into folders for navigation. Each group re-exports
// its modules at the crate root so existing `crate::<module>` paths hold.
mod editor;
pub(crate) use editor::{codeview, viewer, highlight, find, analyze};
mod graph;
pub(crate) use graph::{imports, projectcalls, callgraph, graphlayout, structure, index};
mod ai;
pub(crate) use ai::{overview, walkthrough, richmd, render};
mod session;
pub(crate) use session::{history, notes, bookmarks, reading, cache, finder};
mod backend;
pub(crate) use backend::{server, connect, watch, langenv};
mod miscellaneous;
pub(crate) use miscellaneous::{theme, glyph, icons, keymap, resize, stats};

mod app;
pub use app::message::Message;
pub use app::state::{
    App, DEFAULT_FONT_SIZE, DebugState, DocsState, ExplainState, OverviewState, ProjectCallsState,
    SettingsDraft, StatsState, WalkState,
};

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use iced::Size;

use crate::bookmarks::Bookmark;
use crate::fs_scan::{DirNode, FileEntry};
use crate::search::SearchHit;
use crate::viewer::MAX_FILE_BYTES;

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

