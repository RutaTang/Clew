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

fn cache_path(root: &Path) -> PathBuf {
    root.join(".clew").join("cache").join("walkthrough.json")
}

/// Load the persisted default walkthrough (None on any error / not generated).
pub fn load(root: &Path) -> Option<Walkthrough> {
    std::fs::read_to_string(cache_path(root)).ok().and_then(|s| serde_json::from_str(&s).ok())
}

/// Persist a walkthrough (atomic temp+rename). Only the default tour is cached.
pub fn save(root: &Path, wt: &Walkthrough) -> std::io::Result<()> {
    let path = cache_path(root);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_string(wt).map_err(|e| std::io::Error::other(e.to_string()))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, &path)
}

/// The system prompt for the walkthrough planner. It must return JSON only.
pub const SYSTEM: &str = "You are a senior engineer creating a guided code \
walkthrough for someone who wants to understand a codebase (or a part of it) by \
reading its real code in a sensible order. You are given the project's \
architecture overview, its structure with per-part summaries, and the list of \
real symbols per file. Plan an ordered tour.\n\n\
Return ONLY a JSON object, no prose, no code fences, matching:\n\
{\"title\": string, \"steps\": [{\"title\": string, \"file\": string, \"symbol\": \
string, \"narration\": string}]}\n\n\
Rules:\n\
- 8 to 14 steps for a whole-codebase tour; fewer when scoped to one feature.\n\
- Order so understanding builds: start at the entry point / the core state, \
follow a real end-to-end flow, then the key subsystems.\n\
- `file` MUST be one of the exact relative paths provided. `symbol` MUST be a \
real symbol that exists in that file (from the provided list); omit it only if \
the step is about a whole file.\n\
- `narration` is 2-4 sentences: what this code does, why it matters here, and \
how it connects to the previous step. Be concrete and specific to THIS code — no \
generic filler.\n\
- Never invent files or symbols.";

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
