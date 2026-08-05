//! The Ask agent: a server-side tool loop answering one question about the
//! open project.
//!
//! The model explores with read-only tools (search / read / outline / files /
//! history / semantic find / cached explanations) until it can answer, then
//! writes a cited answer. Every tool call streams back to the client as an
//! `AgentStep` notification (the step chips in the Ask panel), the answer as
//! `AgentDelta` chunks, and the turn closes with `AgentDone`. The whole run is
//! blocking — callers run it inside `spawn_blocking`.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use clew_core::fs_scan::FileEntry;
use clew_core::{embed, explain, git, highlight, llm, outline, search};
use clew_protocol::{AgentRef, AiChatMsg, Event, ServerMessage};
use tokio::sync::mpsc::UnboundedSender;

/// Exploration budget: at most this many model steps (a step may batch several
/// tool calls). Past it, the model is told to answer with what it has.
const MAX_STEPS: usize = 12;
/// Per-tool-result cap, so one grep of a vendored file can't flood the context.
const MAX_RESULT_CHARS: usize = 6_000;
/// Lines `read` returns per call at most.
const MAX_READ_LINES: usize = 250;
/// Tokens per exploration step; the final answer gets a larger budget.
const STEP_TOKENS: u32 = 1_500;
const ANSWER_TOKENS: u32 = 2_000;

/// Everything a tool needs to run, resolved once per turn.
struct Ctx {
    root: PathBuf,
    files: Arc<Vec<FileEntry>>,
    embed_cfg: Option<embed::Config>,
    /// Lazily-loaded project caches (only read if a tool needs them).
    explain_cache: std::cell::OnceCell<explain::Cache>,
    embed_index: std::cell::OnceCell<embed::Index>,
}

/// Run one agent turn. Blocking; emits notifications on `out` throughout.
#[allow(clippy::too_many_arguments)]
pub fn run(
    root: PathBuf,
    files: Arc<Vec<FileEntry>>,
    chat: llm::Config,
    embed_cfg: Option<embed::Config>,
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

    // The loop: let the model explore until it answers or the budget runs out.
    let mut seen: HashSet<String> = HashSet::new();
    let mut answer = String::new();
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
        }
        let result = llm::complete_tools(&chat, &system, &msgs, &tools, {
            if step == MAX_STEPS {
                ANSWER_TOKENS
            } else {
                STEP_TOKENS
            }
        });
        let step_out = match result {
            Ok(o) => o,
            Err(e) => {
                done(Some(e));
                return;
            }
        };
        if step_out.calls.is_empty() || step == MAX_STEPS {
            answer = step_out.text;
            break;
        }
        msgs.push(llm::AgentMsg::Assistant {
            text: step_out.text.clone(),
            calls: step_out.calls.clone(),
        });
        for call in step_out.calls {
            if stopped() {
                done(Some("stopped".into()));
                return;
            }
            // An exact repeat gains nothing — nudge the model onward instead.
            let key = format!("{}:{}", call.name, call.args);
            let (content, title, refs) = if !seen.insert(key) {
                (
                    "You already ran this exact call — its result is above. \
                     Try a different query or tool."
                        .to_string(),
                    format!("{} (repeat skipped)", call.name),
                    Vec::new(),
                )
            } else {
                exec_tool(&ctx, &call.name, &call.args)
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
    }

    if answer.trim().is_empty() {
        done(Some("the model returned an empty answer".into()));
        return;
    }
    // The answer arrives whole from the last step; feed it to the panel in
    // chunks so the existing delta path renders it incrementally.
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
         - `history` for how a file evolved, `explanations` for cached AI summaries\n\
         Explore purposefully; stop as soon as you can answer. Do not guess at code you \
         have not read.\n\
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
    ]
}

/// Run one tool call. Returns `(result_for_model, chip_title, chip_refs)` —
/// tools never fail the turn; problems come back as text the model can react to.
fn exec_tool(ctx: &Ctx, name: &str, args: &serde_json::Value) -> (String, String, Vec<AgentRef>) {
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
            let source = std::fs::read_to_string(&abs).map(|s| {
                if clew_core::notebook::is_notebook(&abs) {
                    clew_core::notebook::parse(&s)
                        .map(|nb| nb.projection)
                        .unwrap_or(s)
                } else {
                    s
                }
            });
            let Ok(source) = source else {
                return (
                    format!("cannot read {rel} (missing or not text)"),
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
            let end = args
                .get("end_line")
                .and_then(|v| v.as_u64())
                .map(|v| v as usize)
                .unwrap_or(start + MAX_READ_LINES - 1)
                .min(start + MAX_READ_LINES - 1)
                .min(total);
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
            // Notebooks outline as their cells.
            if clew_core::notebook::is_notebook(&abs) {
                let Some(nb) = std::fs::read_to_string(&abs)
                    .ok()
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
            let (Ok(source), Some(key)) = (std::fs::read_to_string(&abs), highlight::detect(&abs))
            else {
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
            let index = ctx.embed_index.get_or_init(|| embed::load(&ctx.root));
            if index.entries.is_empty() {
                return (
                    "semantic index not built for this project — use `search` instead".into(),
                    format!("find \"{query}\" (no index)"),
                    Vec::new(),
                );
            }
            let qvec = match embed::embed_batch(ecfg, std::slice::from_ref(&query.to_string())) {
                Ok(mut v) if !v.is_empty() => v.remove(0),
                Ok(_) | Err(_) => {
                    return (
                        "embedding the query failed — use `search` instead".into(),
                        format!("find \"{query}\" (failed)"),
                        Vec::new(),
                    );
                }
            };
            let cache = ctx.explain_cache.get_or_init(|| explain::load(&ctx.root));
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
            let cache = ctx.explain_cache.get_or_init(|| explain::load(&ctx.root));
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
/// and treated as untrusted).
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
    let abs = root.join(rel_path);
    let canon = abs.canonicalize().ok()?;
    let root_canon = root.canonicalize().ok()?;
    canon.starts_with(&root_canon).then_some(abs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_for(dir: &std::path::Path, rels: &[&str]) -> Ctx {
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
        }
    }

    #[test]
    fn read_tool_windows_and_numbers_lines() {
        let dir = std::env::temp_dir().join("clew-agent-read-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let body: String = (1..=30).map(|i| format!("line {i}\n")).collect();
        std::fs::write(dir.join("a.txt"), body).unwrap();
        let ctx = ctx_for(&dir, &["a.txt"]);

        let (content, title, refs) = exec_tool(
            &ctx,
            "read",
            &serde_json::json!({ "file": "a.txt", "start_line": 10, "end_line": 12 }),
        );
        assert!(content.contains("   10| line 10"));
        assert!(content.contains("   12| line 12"));
        assert!(!content.contains("line 13"));
        assert_eq!(title, "read a.txt:10-12");
        assert_eq!(refs[0].line, Some(10));
    }

    #[test]
    fn read_tool_refuses_escapes() {
        let dir = std::env::temp_dir().join("clew-agent-confine-test");
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = ctx_for(&dir, &[]);
        let (content, _, _) = exec_tool(
            &ctx,
            "read",
            &serde_json::json!({ "file": "../secret.txt" }),
        );
        assert!(content.starts_with("refused"));
        let (content, _, _) =
            exec_tool(&ctx, "read", &serde_json::json!({ "file": "/etc/passwd" }));
        assert!(content.starts_with("refused"));
    }

    #[test]
    fn files_tool_filters_by_prefix() {
        let dir = std::env::temp_dir().join("clew-agent-files-test");
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = ctx_for(&dir, &["src/a.rs", "src/b.rs", "docs/c.md"]);
        let (content, title, _) =
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
        let (content, _, _) = exec_tool(&ctx, "nope", &serde_json::json!({}));
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
