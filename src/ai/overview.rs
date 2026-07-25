//! Project architecture overview: a generated onboarding page shown on the home
//! screen ("what is this codebase, where do I start").
//!
//! It's assembled from artifacts clew already has — the folder/file explanation
//! summaries, the import graph, and the symbol index — plus one LLM call that
//! writes the narrative and reading order. The module-dependency diagram is
//! computed deterministically from the import graph (not hallucinated), then
//! injected into the markdown so it renders as an inline mermaid SVG.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::incremental::Version;

/// A cached overview: the markdown plus the hash of the prompt that produced it,
/// so a changed prompt (structure or any summary changed) misses.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Cached {
    pub markdown: String,
    pub prompt_hash: Version,
}

fn cache_path(root: &Path) -> PathBuf {
    root.join(".clew").join("cache").join("overview.json")
}

/// Load the persisted overview (None on any error / not generated).
pub fn load(root: &Path) -> Option<Cached> {
    std::fs::read_to_string(cache_path(root))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
}

/// Persist the overview (atomic temp+rename).
pub fn save(root: &Path, cached: &Cached) -> std::io::Result<()> {
    let path = cache_path(root);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_string(cached).map_err(|e| std::io::Error::other(e.to_string()))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, &path)
}

/// Everything the overview prompt needs, gathered from clew's existing artifacts.
pub struct Inputs {
    pub project_name: String,
    /// Folders and files with their summaries, in reading order.
    pub structure: String,
    pub entry_points: Vec<String>,
    pub key_types: Vec<String>,
}

/// Build the LLM prompt from the gathered inputs.
pub fn prompt(inputs: &Inputs) -> String {
    let mut p = format!("Project: {}\n\n", inputs.project_name);
    p.push_str("Structure (folders and files, each with a short summary of its role):\n");
    p.push_str(&inputs.structure);
    p.push('\n');
    if !inputs.entry_points.is_empty() {
        p.push_str("\nEntry points (where execution begins):\n");
        for e in &inputs.entry_points {
            p.push_str(&format!("- {e}\n"));
        }
    }
    if !inputs.key_types.is_empty() {
        p.push_str("\nKey types (important data structures):\n");
        p.push_str(&inputs.key_types.join(", "));
        p.push('\n');
    }
    p
}

/// The system prompt for the overview writer.
pub const SYSTEM: &str = "You are writing a concise architecture overview that \
onboards a developer who just opened this codebase and wants to understand it \
fast. You are given the folder/file structure with a short summary of each part, \
the entry points, and the key types. Write GitHub-flavored Markdown with exactly \
these sections:\n\
## What it does — 2-3 sentences on the project's purpose.\n\
## Core modules — the main subsystems/files as bullets, each a one-line role. \
Link each file as [name](relative/path).\n\
## Entry points — where execution begins and the overall flow.\n\
## Key types — the important data structures and what they represent.\n\
## Where to start — an ordered reading list of 3 to 6 items for a newcomer, each \
linking the file (with its relative path) and saying why to read it at that step.\n\
Reference files with Markdown links using the exact relative path given. Be \
concrete and specific to THIS codebase — no generic filler.";

/// Remove a previously-injected "## Module map" section (heading + fenced
/// mermaid block) from `markdown`, so a fresh diagram can be folded in without
/// duplicating it. Leaves markdown without such a section untouched.
pub fn strip_module_map(markdown: &str) -> String {
    const NEEDLE: &str = "## Module map";
    let start = if markdown.starts_with(NEEDLE) {
        Some(0)
    } else {
        markdown.find(&format!("\n{NEEDLE}")).map(|p| p + 1)
    };
    let Some(start) = start else {
        return markdown.to_string();
    };
    // The section runs until the next line-start "## " heading, or end of text.
    let rest = &markdown[start + NEEDLE.len()..];
    let end = rest
        .find("\n## ")
        .map(|p| start + NEEDLE.len() + p + 1)
        .unwrap_or(markdown.len());
    let head = markdown[..start].trim_end();
    let tail = &markdown[end..];
    if tail.is_empty() {
        format!("{head}\n")
    } else {
        format!("{head}\n\n{tail}")
    }
}

/// Select the most-connected files and the internal import edges among them, as
/// inputs to clew's native graph layout — the module map is drawn on a canvas
/// (like the Import Graph overlay), not as a mermaid diagram. `scope` maps each
/// file to the internal files it imports. Returns None when there's too little
/// structure to be worth showing.
pub fn module_layout_inputs(
    scope: &HashMap<PathBuf, HashSet<PathBuf>>,
) -> Option<(Vec<crate::graphlayout::NodeInput>, Vec<(usize, usize)>)> {
    const MAX_NODES: usize = 14;

    // Degree = fan-out (files it imports) + fan-in (files importing it).
    let mut fan_in: HashMap<&PathBuf, usize> = HashMap::new();
    for deps in scope.values() {
        for d in deps {
            *fan_in.entry(d).or_default() += 1;
        }
    }
    let deg = |f: &PathBuf| scope.get(f).map(|d| d.len()).unwrap_or(0) + fan_in.get(f).copied().unwrap_or(0);
    // Every file in the graph is a node — both importers (`scope` keys) AND the
    // leaf modules they import (values). Ranking off keys alone would drop a
    // widely-imported file that imports nothing itself (e.g. a `lexer`/`ast`),
    // leaving the map incomplete.
    let mut node_set: HashSet<&PathBuf> = scope.keys().collect();
    for deps in scope.values() {
        node_set.extend(deps.iter());
    }
    let mut ranked: Vec<&PathBuf> = node_set.into_iter().collect();
    ranked.sort_by_key(|f| (std::cmp::Reverse(deg(f)), (*f).clone())); // deterministic tie-break
    let top: Vec<&PathBuf> = ranked.into_iter().take(MAX_NODES).collect();
    let idx: HashMap<&PathBuf, usize> = top.iter().enumerate().map(|(i, f)| (*f, i)).collect();

    let nodes: Vec<crate::graphlayout::NodeInput> = top
        .iter()
        .map(|f| crate::graphlayout::NodeInput {
            label: f.file_stem().and_then(|s| s.to_str()).unwrap_or("?").to_string(),
            file: (*f).clone(),
            weight: deg(f) as f32,
            cyclic: false,
        })
        .collect();
    let mut edges = Vec::new();
    for f in &top {
        if let Some(deps) = scope.get(*f) {
            for d in deps {
                if let (Some(&a), Some(&b)) = (idx.get(f), idx.get(d)) {
                    edges.push((a, b));
                }
            }
        }
    }
    // Need at least a couple of nodes and one edge to be informative.
    (nodes.len() >= 3 && !edges.is_empty()).then_some((nodes, edges))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_inputs_from_imports_or_none() {
        let mut scope: HashMap<PathBuf, HashSet<PathBuf>> = HashMap::new();
        let f = |s: &str| PathBuf::from(s);
        // b and c are imported but import nothing themselves — pure leaf targets,
        // never keys of `scope`. They must still show up as nodes with edges.
        scope.insert(f("a.rs"), HashSet::from([f("b.rs"), f("c.rs")]));
        scope.insert(f("b.rs"), HashSet::from([f("c.rs")]));
        let (nodes, edges) = module_layout_inputs(&scope).expect("inputs");
        let labels: HashSet<&str> = nodes.iter().map(|n| n.label.as_str()).collect();
        assert!(labels.contains("a") && labels.contains("b"), "labels from stems");
        // c is only ever a target, yet must appear (the leaf-node fix).
        assert!(labels.contains("c"), "leaf-only target missing: {labels:?}");
        assert_eq!(edges.len(), 3, "all three import edges kept: {edges:?}");

        // Too flat → None.
        let mut flat: HashMap<PathBuf, HashSet<PathBuf>> = HashMap::new();
        flat.insert(f("x.rs"), HashSet::new());
        assert!(module_layout_inputs(&flat).is_none());
    }

    #[test]
    fn strip_module_map_removes_only_that_section() {
        let md = "## What it does\nFoo.\n\n## Module map\n\n```mermaid\ngraph LR\n n0[\"a\"]\n```\n\n## Core modules\n- bar\n";
        let out = strip_module_map(md);
        assert!(!out.contains("## Module map"), "map not removed: {out}");
        assert!(!out.contains("mermaid"), "fence not removed: {out}");
        assert!(out.contains("## What it does"), "kept prose: {out}");
        assert!(out.contains("## Core modules"), "kept later section: {out}");
        // Nothing to strip is a no-op.
        assert_eq!(strip_module_map("## Only\ntext\n"), "## Only\ntext\n");
    }
}
