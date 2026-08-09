//! Inactive `#[cfg(...)]` detection for the reading aid that dims code which
//! isn't compiled for the reading target.
//!
//! We evaluate each Rust `cfg` predicate against a [`Target`] (the host by
//! default, or one the reader picks to study another platform's code),
//! conservatively: a line is dimmed only when its `cfg` is *definitively* false
//! (a target predicate that doesn't match). Feature flags and anything we can't
//! decide are left active, so we never hide live code.
//!
//! Rust only for now; Go `//go:build` and C/C++ `#ifdef` can follow.

use std::collections::HashSet;

use tree_sitter::{Node, Parser};

/// The target facts clew evaluates `cfg` predicates against. Selectable per
/// project so you can read another platform's branches as the live ones.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub label: String,
    pub os: String,
    pub arch: String,
    pub family: String,
}

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label)
    }
}

impl Target {
    fn new(label: &str, os: &str, arch: &str, family: &str) -> Target {
        Target {
            label: label.into(),
            os: os.into(),
            arch: arch.into(),
            family: family.into(),
        }
    }

    /// The machine clew runs on.
    pub fn host() -> Target {
        let os = std::env::consts::OS;
        Target {
            label: format!("Host ({os})"),
            os: os.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            family: std::env::consts::FAMILY.to_string(),
        }
    }

    /// Host first, then the common cross-compilation targets for the picker.
    pub fn presets() -> Vec<Target> {
        vec![
            Target::host(),
            Target::new("macOS (arm64)", "macos", "aarch64", "unix"),
            Target::new("macOS (x86_64)", "macos", "x86_64", "unix"),
            Target::new("Linux (x86_64)", "linux", "x86_64", "unix"),
            Target::new("Linux (arm64)", "linux", "aarch64", "unix"),
            Target::new("Windows (x86_64)", "windows", "x86_64", "windows"),
            Target::new("Windows (arm64)", "windows", "aarch64", "windows"),
        ]
    }

    /// Match a stored label back to a preset, falling back to the host.
    pub fn from_label(label: &str) -> Target {
        Target::presets()
            .into_iter()
            .find(|t| t.label == label)
            .unwrap_or_else(Target::host)
    }
}

/// Reconstruct a `Target` from its protocol wire form (sent with `ReadFile`).
impl From<clew_protocol::TargetSpec> for Target {
    fn from(spec: clew_protocol::TargetSpec) -> Target {
        Target {
            label: spec.label,
            os: spec.os,
            arch: spec.arch,
            family: spec.family,
        }
    }
}

/// 0-based line numbers whose enclosing item is gated off by a `cfg` that is
/// inactive for `target`.
pub fn inactive_lines(source: &str, lang_key: &str, target: &Target) -> HashSet<usize> {
    let mut out = HashSet::new();
    if lang_key != "rust" {
        return out;
    }
    let Some(lang) = crate::highlight::language_for("rust") else {
        return out;
    };
    let mut parser = Parser::new();
    if parser.set_language(&lang).is_err() {
        return out;
    }
    let Some(tree) = parser.parse(source, None) else {
        return out;
    };
    walk(tree.root_node(), source.as_bytes(), target, &mut out);
    out
}

/// Walk the tree with an explicit stack rather than the call stack — see
/// [`crate::structure`] for why: a deeply nested expression is ~2 bytes per
/// level, so a file inside the size cap can reach a depth that overflows the
/// stack, and this runs on every file opened.
fn walk(root: Node, src: &[u8], host: &Target, out: &mut HashSet<usize>) {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        let mut cursor = node.walk();
        let before = stack.len();
        stack.extend(node.children(&mut cursor));
        stack[before..].reverse();
        walk_node(node, src, host, out);
    }
}

/// The per-node work, split out so the traversal above stays plain.
fn walk_node(node: Node, src: &[u8], host: &Target, out: &mut HashSet<usize>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        // An outer `#[cfg(...)]` is a *preceding sibling* of the item it gates.
        // When it's inactive, dim from the attribute through the item that
        // follows (skipping any further attributes, and any comments, between
        // them). Comments have to be skipped as well: they are named siblings
        // too, so stopping at one dimmed the attribute and the comment while
        // leaving the function the `cfg` actually gates displayed as active.
        if child.kind() == "attribute_item" && cfg_of(child, src, host) == Some(false) {
            let mut item = child.next_named_sibling();
            while let Some(n) = item {
                if matches!(
                    n.kind(),
                    "attribute_item" | "line_comment" | "block_comment"
                ) {
                    item = n.next_named_sibling();
                } else {
                    break;
                }
            }
            let start = child.start_position().row;
            let end = item.unwrap_or(child).end_position().row;
            for line in start..=end {
                out.insert(line);
            }
        }
    }
}

/// Evaluate the `cfg` on an `attribute_item`, or `None` if it isn't a
/// `#[cfg(...)]` we can decide (inner `#![…]`, `cfg_attr`, features, …).
fn cfg_of(attr_item: Node, src: &[u8], host: &Target) -> Option<bool> {
    let text = attr_item.utf8_text(src).ok()?.trim();
    let inner = text.strip_prefix("#[")?.strip_suffix(']')?.trim();
    // `cfg( … )` only — not `cfg_attr(…)` (which starts "cfg_attr" → no "(").
    let pred = inner
        .strip_prefix("cfg")?
        .trim_start()
        .strip_prefix('(')?
        .strip_suffix(')')?;
    eval_cfg(pred.trim(), host)
}

/// How deeply a `cfg` predicate may nest before clew stops evaluating it.
///
/// `not(` + `)` is five bytes per level, and the predicate comes from a source
/// file the repository controls — so without a limit a few hundred KB of one
/// attribute recurses deep enough to overflow the stack. Beyond this a
/// predicate is not something a human wrote, and "undecidable" (which callers
/// treat as active, i.e. nothing is dimmed) is the safe answer.
const MAX_CFG_DEPTH: usize = 64;

/// Evaluate a `cfg` predicate. `Some(false)` = definitively inactive; `Some(true)`
/// = active; `None` = undecidable (treated as active by callers).
fn eval_cfg(pred: &str, host: &Target) -> Option<bool> {
    eval_cfg_at(pred, host, 0)
}

fn eval_cfg_at(pred: &str, host: &Target, depth: usize) -> Option<bool> {
    if depth > MAX_CFG_DEPTH {
        return None;
    }
    let pred = pred.trim();
    if let Some(inner) = pred.strip_prefix("all(").and_then(|s| s.strip_suffix(')')) {
        let mut result = Some(true);
        for part in split_top(inner) {
            match eval_cfg_at(&part, host, depth + 1) {
                Some(false) => return Some(false),
                None => result = None,
                Some(true) => {}
            }
        }
        return result;
    }
    if let Some(inner) = pred.strip_prefix("any(").and_then(|s| s.strip_suffix(')')) {
        let mut result = Some(false);
        for part in split_top(inner) {
            match eval_cfg_at(&part, host, depth + 1) {
                Some(true) => return Some(true),
                None => result = None,
                Some(false) => {}
            }
        }
        return result;
    }
    if let Some(inner) = pred.strip_prefix("not(").and_then(|s| s.strip_suffix(')')) {
        return eval_cfg_at(inner, host, depth + 1).map(|b| !b);
    }
    if let Some((key, val)) = pred.split_once('=') {
        let val = val.trim().trim_matches('"');
        return match key.trim() {
            "target_os" => Some(host.os == val),
            "target_arch" => Some(host.arch == val),
            "target_family" => Some(host.family == val),
            _ => None, // target_env, feature, … — leave active
        };
    }
    match pred {
        "unix" => Some(host.family == "unix"),
        "windows" => Some(host.family == "windows"),
        _ => None, // test, debug_assertions, bare feature, … — leave active
    }
}

/// Split on top-level commas (ignoring commas nested inside parentheses).
fn split_top(s: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    for (i, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(s[start..i].trim().to_string());
                start = i + 1;
            }
            _ => {}
        }
    }
    let tail = s[start..].trim();
    if !tail.is_empty() {
        parts.push(tail.to_string());
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host_macos() -> Target {
        Target {
            label: "test".into(),
            os: "macos".into(),
            arch: "aarch64".into(),
            family: "unix".into(),
        }
    }

    #[test]
    fn evaluates_target_predicates() {
        let h = host_macos();
        assert_eq!(eval_cfg("target_os = \"windows\"", &h), Some(false));
        assert_eq!(eval_cfg("target_os = \"macos\"", &h), Some(true));
        assert_eq!(eval_cfg("unix", &h), Some(true));
        assert_eq!(eval_cfg("windows", &h), Some(false));
        assert_eq!(eval_cfg("not(windows)", &h), Some(true));
        assert_eq!(
            eval_cfg("all(unix, target_os = \"linux\")", &h),
            Some(false)
        );
        assert_eq!(
            eval_cfg("any(windows, target_os = \"macos\")", &h),
            Some(true)
        );
        // Features / unknowns stay active (undecidable).
        assert_eq!(eval_cfg("feature = \"foo\"", &h), None);
        assert_eq!(eval_cfg("all(unix, feature = \"foo\")", &h), None);
    }

    #[test]
    fn dims_only_the_inactive_item() {
        let src = "\
#[cfg(target_os = \"windows\")]
fn only_windows() {
    win();
}

#[cfg(unix)]
fn on_unix() {
    nix();
}
";
        let lines = inactive_lines(src, "rust", &host_macos());
        // The windows fn (lines 0..=3) is dimmed; the unix fn is not.
        assert!(
            lines.contains(&0) && lines.contains(&1) && lines.contains(&3),
            "{lines:?}"
        );
        assert!(!lines.contains(&6) && !lines.contains(&7), "{lines:?}");
    }

    /// Comments sit between the attribute and the item as named siblings, so
    /// the walk has to step over them. It used to stop at the first one and
    /// leave the gated function displayed as active.
    #[test]
    fn dims_through_comments_between_the_cfg_and_its_item() {
        let src = "\
#[cfg(target_os = \"windows\")]
// Why this exists.
/* and a block one */
fn only_windows() {
    win();
}
";
        let lines = inactive_lines(src, "rust", &host_macos());
        // Attribute, both comments, and the whole function body.
        for l in 0..=5 {
            assert!(lines.contains(&l), "line {l} not dimmed: {lines:?}");
        }
    }

    /// Doc comments on the gated item are the common shape of the above.
    #[test]
    fn dims_through_a_doc_comment() {
        let src = "\
#[cfg(target_os = \"windows\")]
/// Windows-only helper.
fn only_windows() {}
";
        let lines = inactive_lines(src, "rust", &host_macos());
        assert!(lines.contains(&2), "the gated fn is dimmed: {lines:?}");
    }
}

#[cfg(test)]
mod depth_tests {
    use super::*;

    /// A `cfg` predicate is text from the same file: `not(` + `)` is five
    /// bytes per level, so a few hundred KB of one attribute recursed deep
    /// enough to overflow the stack. Past the depth limit the predicate reads
    /// as undecidable, which dims nothing — the safe direction.
    #[test]
    fn a_deeply_nested_cfg_predicate_does_not_overflow_the_stack() {
        const DEPTH: usize = 100_000;
        let src = format!(
            "#[cfg({}target_os = \"nonesuch\"{})]\nfn gated() {{}}\n",
            "not(".repeat(DEPTH),
            ")".repeat(DEPTH)
        );
        std::thread::Builder::new()
            .stack_size(512 * 1024)
            .spawn(move || {
                // Undecidable, so nothing is reported inactive.
                assert!(inactive_lines(&src, "rust", &Target::host()).is_empty());
            })
            .expect("spawn")
            .join()
            .expect("evaluating the predicate must not overflow the stack");
    }

    /// A deeply nested expression costs about two bytes per level, so a file
    /// well inside every size limit reaches a syntax-tree depth that overflows
    /// the call stack. That is a SIGSEGV, not a catchable panic — and this
    /// runs on every file the user opens, over whatever the repository holds.
    #[test]
    fn a_deeply_nested_file_does_not_overflow_the_stack() {
        const DEPTH: usize = 50_000;
        let src = format!(
            "fn f() -> i32 {{ {}1{} }}\n",
            "(".repeat(DEPTH),
            ")".repeat(DEPTH)
        );
        assert!(src.len() < 512 * 1024, "stays inside the indexer's cap");
        // Measured: this parses to a syntax tree 50_004 nodes deep, so the
        // recursion this replaced needed that many stack frames.
        // Run on a deliberately small stack: the recursive walk this replaced
        // died here, the iterative one does not care.
        std::thread::Builder::new()
            .stack_size(512 * 1024)
            .spawn(move || {
                let _ = inactive_lines(&src, "rust", &Target::host());
            })
            .expect("spawn")
            .join()
            .expect("the walk must not overflow the stack");
    }
}
