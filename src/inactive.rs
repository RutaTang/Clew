//! Inactive `#[cfg(...)]` detection for the reading aid that dims code which
//! isn't compiled on the reader's platform.
//!
//! We evaluate each Rust `cfg` predicate against the host's target (the machine
//! clew runs on), conservatively: a line is dimmed only when its `cfg` is
//! *definitively* false (a target predicate that doesn't match). Feature flags
//! and anything we can't decide are left active, so we never hide live code.
//!
//! Rust only for now; Go `//go:build` and C/C++ `#ifdef` can follow.

use std::collections::HashSet;

use tree_sitter::{Node, Parser};

/// 0-based line numbers whose enclosing item is gated off by a `cfg` that is
/// inactive for the host target.
pub fn inactive_lines(source: &str, lang_key: &str) -> HashSet<usize> {
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
    let host = HostCfg::current();
    walk(tree.root_node(), source.as_bytes(), &host, &mut out);
    out
}

/// The host target facts clew evaluates `cfg` predicates against.
struct HostCfg {
    os: String,
    arch: String,
    family: String,
}

impl HostCfg {
    fn current() -> Self {
        HostCfg {
            os: std::env::consts::OS.to_string(),      // "macos", "linux", …
            arch: std::env::consts::ARCH.to_string(),  // "aarch64", "x86_64", …
            family: std::env::consts::FAMILY.to_string(), // "unix" / "windows"
        }
    }
}

fn walk(node: Node, src: &[u8], host: &HostCfg, out: &mut HashSet<usize>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        // An outer `#[cfg(...)]` is a *preceding sibling* of the item it gates.
        // When it's inactive, dim from the attribute through the item that
        // follows (skipping any further attributes between them).
        if child.kind() == "attribute_item" && cfg_of(child, src, host) == Some(false) {
            let mut item = child.next_named_sibling();
            while let Some(n) = item {
                if n.kind() == "attribute_item" {
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
        walk(child, src, host, out);
    }
}

/// Evaluate the `cfg` on an `attribute_item`, or `None` if it isn't a
/// `#[cfg(...)]` we can decide (inner `#![…]`, `cfg_attr`, features, …).
fn cfg_of(attr_item: Node, src: &[u8], host: &HostCfg) -> Option<bool> {
    let text = attr_item.utf8_text(src).ok()?.trim();
    let inner = text.strip_prefix("#[")?.strip_suffix(']')?.trim();
    // `cfg( … )` only — not `cfg_attr(…)` (which starts "cfg_attr" → no "(").
    let pred = inner.strip_prefix("cfg")?.trim_start().strip_prefix('(')?.strip_suffix(')')?;
    eval_cfg(pred.trim(), host)
}

/// Evaluate a `cfg` predicate. `Some(false)` = definitively inactive; `Some(true)`
/// = active; `None` = undecidable (treated as active by callers).
fn eval_cfg(pred: &str, host: &HostCfg) -> Option<bool> {
    let pred = pred.trim();
    if let Some(inner) = pred.strip_prefix("all(").and_then(|s| s.strip_suffix(')')) {
        let mut result = Some(true);
        for part in split_top(inner) {
            match eval_cfg(&part, host) {
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
            match eval_cfg(&part, host) {
                Some(true) => return Some(true),
                None => result = None,
                Some(false) => {}
            }
        }
        return result;
    }
    if let Some(inner) = pred.strip_prefix("not(").and_then(|s| s.strip_suffix(')')) {
        return eval_cfg(inner, host).map(|b| !b);
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

    fn host_macos() -> HostCfg {
        HostCfg { os: "macos".into(), arch: "aarch64".into(), family: "unix".into() }
    }

    #[test]
    fn evaluates_target_predicates() {
        let h = host_macos();
        assert_eq!(eval_cfg("target_os = \"windows\"", &h), Some(false));
        assert_eq!(eval_cfg("target_os = \"macos\"", &h), Some(true));
        assert_eq!(eval_cfg("unix", &h), Some(true));
        assert_eq!(eval_cfg("windows", &h), Some(false));
        assert_eq!(eval_cfg("not(windows)", &h), Some(true));
        assert_eq!(eval_cfg("all(unix, target_os = \"linux\")", &h), Some(false));
        assert_eq!(eval_cfg("any(windows, target_os = \"macos\")", &h), Some(true));
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
        let lines = inactive_lines(src, "rust");
        // The windows fn (lines 0..=3) is dimmed; the unix fn is not.
        assert!(lines.contains(&0) && lines.contains(&1) && lines.contains(&3), "{lines:?}");
        assert!(!lines.contains(&6) && !lines.contains(&7), "{lines:?}");
    }
}
