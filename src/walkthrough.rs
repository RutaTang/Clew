//! Guided walkthroughs: an ordered, code-anchored tour the reader steps through.
//!
//! Unlike the architecture [`overview`](crate::overview) (a static document) or
//! per-symbol [`explain`](crate::explain) summaries, a walkthrough is a *path*:
//! an ordered list of steps, each anchored to a real file + symbol with a short
//! narration. Stepping through it drives the editor — clew opens the file, jumps
//! to the anchor and highlights it — so you read the actual code alongside the
//! explanation.
//!
//! It's a synthesis layer over artifacts clew already has (the overview, the
//! per-symbol summaries, the symbol index), so one LLM call plans the tour from
//! distilled understanding rather than raw source. Anchors are symbol-keyed and
//! validated against the index, so a step survives edits and never points at a
//! hallucinated location.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A generated tour: a title, its scope (empty = whole codebase, else the user's
/// prompt), and the ordered steps.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Walkthrough {
    pub title: String,
    #[serde(default)]
    pub scope: String,
    pub steps: Vec<Step>,
}

/// One stop on the tour, anchored to a symbol (preferred) or a line.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    pub title: String,
    /// Project-relative path of the file this step is about.
    pub file: String,
    /// The symbol to anchor on; resolved to a live line at navigation time.
    #[serde(default)]
    pub symbol: Option<String>,
    /// Fallback 1-based line when there's no symbol.
    #[serde(default)]
    pub line: Option<usize>,
    /// Markdown: what this code does, why it matters, how it connects.
    pub narration: String,
}

fn library_path(root: &Path) -> PathBuf {
    root.join(".clew").join("cache").join("walkthroughs.json")
}

/// Load the saved library of walkthroughs (empty when none). Migrates a legacy
/// single-tour `walkthrough.json` into a one-element library.
pub fn load_library(root: &Path) -> Vec<Walkthrough> {
    if let Ok(text) = std::fs::read_to_string(library_path(root))
        && let Ok(v) = serde_json::from_str::<Vec<Walkthrough>>(&text)
    {
        return v;
    }
    let legacy = root.join(".clew").join("cache").join("walkthrough.json");
    if let Ok(text) = std::fs::read_to_string(legacy)
        && let Ok(wt) = serde_json::from_str::<Walkthrough>(&text)
    {
        return vec![wt];
    }
    Vec::new()
}

/// Persist the whole library (atomic temp+rename). Each tour keeps its `scope`
/// (the prompt), so custom tours survive across sessions with the project.
pub fn save_library(root: &Path, tours: &[Walkthrough]) -> std::io::Result<()> {
    let path = library_path(root);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_string(tours).map_err(|e| std::io::Error::other(e.to_string()))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, &path)
}

/// The system prompt for the walkthrough planner. It must return JSON only.
pub const SYSTEM: &str = "You are a staff engineer giving a new teammate a \
guided tour of a codebase so they grasp its SKELETON (how it's structured and \
how control/data flows) and its ESSENCE (the core ideas, patterns and design \
decisions that make it tick) by reading the real code in the right order. You \
are given the architecture overview, the structure with per-part summaries, and \
the real symbols per file. Plan the tour.\n\n\
Return ONLY a JSON object — no prose, no code fences — matching:\n\
{\"title\": string, \"steps\": [{\"title\": string, \"file\": string, \"symbol\": \
string, \"narration\": string}]}\n\n\
Rules:\n\
- For a whole-codebase tour aim for 12 to 18 steps; a focused/feature tour is \
shorter but still complete. Do not stop at 5 — cover the real spine.\n\
- Sequence for building a mental model: (1) what it is + the entry point, (2) \
the central architecture/loop and the core state, (3) one real end-to-end flow \
traced through the code, (4) each major subsystem in turn, (5) the cross-cutting \
ideas (persistence, incremental work, the key abstractions). Every important \
part a newcomer must read should appear.\n\
- `file` MUST be an exact relative path from the list. `symbol` MUST be a real \
symbol in that file (from the provided list); omit it only for a whole-file step.\n\
- `narration` is rich **GitHub-flavored Markdown** — 4 to 8 sentences plus \
formatting: `backticks` for every identifier/type/file, **bold** for the key \
idea, and short bullet lists for the moving parts. Say what this code does, why \
it matters, the non-obvious insight or design pattern, and how it connects to \
the previous step. Be concrete and specific to THIS code (name real functions \
and types) — no generic filler like \"handles user actions\".\n\
- DRAW DIAGRAMS. When a picture clarifies structure or a flow, embed a small \
Mermaid diagram in a fenced ```mermaid block inside the narration (`graph LR` \
or `graph TD`, under ~12 nodes). The FIRST step MUST include a big-picture \
architecture diagram showing the main modules and how control/data flows between \
them; later flow-heavy steps (opening a file, a request round-trip) should \
include a sequence or flow diagram too. Use real module/type names as nodes.\n\
- Never invent files or symbols.";

/// System prompt for the "review changes" walkthrough: a tour of a diff.
pub const DIFF_SYSTEM: &str = "You are a senior engineer walking a teammate \
through a set of code CHANGES (a branch / PR diff) so they understand WHAT \
changed and WHY, in the clearest order. You are given the commit messages (the \
intent), the changed files with their symbols, and the unified diff. Plan an \
ordered walkthrough of the change.\n\n\
Return ONLY a JSON object — no prose, no code fences — matching:\n\
{\"title\": string, \"steps\": [{\"title\": string, \"file\": string, \"symbol\": \
string, \"narration\": string}]}\n\n\
Rules:\n\
- Order for understanding the CHANGE, not file-by-file: (1) one step summarising \
what this change does and why (from the commit messages), (2) the core edit(s) \
that make it happen, (3) the supporting / plumbing edits, (4) the tests that \
prove it. Aim for 6 to 14 steps.\n\
- Anchor each step to a REAL changed file plus a symbol that actually changed \
(pick from the provided per-file symbol lists). `file` MUST be an exact relative \
path from the changed-files list; `symbol` MUST be a real symbol in that file.\n\
- `narration` is rich **GitHub-flavored Markdown**, 3 to 6 sentences: what \
changed here, WHY (tie it to the intent / commit messages), and how it connects \
to the other changes. Use `backticks` for identifiers/types, **bold** for the \
key point, and quote the essential added/removed lines when it clarifies.\n\
- Focus ONLY on the changes — do not tour unchanged code. Never invent files or \
symbols.";

/// Build the "review changes" prompt from the collected diff context.
pub fn diff_prompt(
    project_name: &str,
    label: &str,
    commits: &[String],
    changed: &str,
    patch: &str,
) -> String {
    let intent = if commits.is_empty() {
        "(no commit messages — infer intent from the diff)".to_string()
    } else {
        commits.iter().map(|s| format!("- {s}")).collect::<Vec<_>>().join("\n")
    };
    format!(
        "Project: {project_name}\nReviewing: the current work {label}.\n\n\
         Intent (commit messages, oldest first):\n{intent}\n\n\
         Changed files and their symbols:\n{changed}\n\n\
         Unified diff:\n{patch}\n"
    )
}

/// Build the user prompt from the gathered context.
pub fn prompt(project_name: &str, overview: Option<&str>, context: &str, scope: Option<&str>) -> String {
    let mut p = format!("Project: {project_name}\n\n");
    match scope {
        Some(s) => p.push_str(&format!(
            "Scope: walk through this specific part — \"{s}\". Only include steps \
             relevant to it, in the order best for understanding it.\n\n"
        )),
        None => p.push_str(
            "Scope: the whole codebase — its main ideas, architecture and the \
             key code a newcomer must read to get oriented.\n\n",
        ),
    }
    if let Some(ov) = overview {
        p.push_str("Architecture overview:\n");
        p.push_str(ov);
        p.push_str("\n\n");
    }
    p.push_str(context);
    p
}

/// Parse the planner's response into a walkthrough, tolerating a code fence or
/// stray prose around the JSON object.
pub fn parse(response: &str) -> Result<Walkthrough, String> {
    let start = response.find('{').ok_or("no JSON object in the response")?;
    let end = response.rfind('}').ok_or("no JSON object in the response")?;
    if end < start {
        return Err("malformed JSON in the response".into());
    }
    let mut wt: Walkthrough =
        serde_json::from_str(&response[start..=end]).map_err(|e| format!("parse: {e}"))?;
    wt.steps.retain(|s| !s.file.trim().is_empty());
    if wt.steps.is_empty() {
        return Err("the walkthrough had no usable steps".into());
    }
    Ok(wt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tolerates_fences_and_prose() {
        let resp = "Sure! Here it is:\n```json\n{\"title\":\"Tour\",\"steps\":[\
            {\"title\":\"Start\",\"file\":\"src/main.rs\",\"symbol\":\"main\",\"narration\":\"Entry.\"},\
            {\"title\":\"State\",\"file\":\"src/main.rs\",\"narration\":\"The App struct.\"}\
        ]}\n```\nHope that helps!";
        let wt = parse(resp).unwrap();
        assert_eq!(wt.title, "Tour");
        assert_eq!(wt.steps.len(), 2);
        assert_eq!(wt.steps[0].symbol.as_deref(), Some("main"));
        assert_eq!(wt.steps[1].symbol, None); // omitted → whole-file step
    }

    #[test]
    fn parse_rejects_empty() {
        assert!(parse("no json here").is_err());
        assert!(parse("{\"title\":\"x\",\"steps\":[]}").is_err());
    }

    #[test]
    fn roundtrips_through_json() {
        let wt = Walkthrough {
            title: "T".into(),
            scope: "lsp".into(),
            steps: vec![Step {
                title: "s".into(),
                file: "src/lsp/client.rs".into(),
                symbol: Some("LspClient".into()),
                line: None,
                narration: "n".into(),
            }],
        };
        let json = serde_json::to_string(&wt).unwrap();
        let back: Walkthrough = serde_json::from_str(&json).unwrap();
        assert_eq!(back.steps[0].file, "src/lsp/client.rs");
        assert_eq!(back.scope, "lsp");
    }
}
