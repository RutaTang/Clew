//! Inactive-code detection for the reading aid that dims code the build does
//! not compile for the reading target.
//!
//! - Rust: each `#[cfg(...)]` is evaluated against a [`Target`] (the host by
//!   default, one the reader picks to study another platform's code, or the
//!   target rust-analyzer is told to check for) and, for `feature = "…"`,
//!   against the features its package builds with ([`BuildFacts`]).
//! - Go: a file whose `//go:build` line (or `// +build` lines) or whose name
//!   (`_windows.go`, `_arm64.go`) leaves it out of the build is dimmed whole —
//!   for the platform gopls loads, and the build tags it is given.
//! - C and C++: the groups of `#if`/`#ifdef`/`#elif`/`#else` the
//!   preprocessor drops, from the platform's macros, the flags' `-D`/`-U`,
//!   and the file's own `#define`s.
//! - C#: the groups of `#if`/`#elif`/`#else` over the symbols the file
//!   `#define`s and `#undef`s at its top. `DEBUG`, `TRACE`, the project's
//!   `DefineConstants` and the framework's `NET8_0` come from the build, so
//!   they are never known.
//! - Haskell: in a file with `{-# LANGUAGE CPP #-}`, the C preprocessor's
//!   groups, from GHC's platform macros for the target (`linux_HOST_OS`,
//!   `x86_64_HOST_ARCH`, …) and the file's own `#define`s.
//! - Erlang: the groups of `-ifdef`/`-ifndef`/`-if`/`-elif`/`-else` over the
//!   macros the file `-define`s and `-undef`s; `TEST`, `OTP_RELEASE` and the
//!   rest come from the build.
//! - Zig: the branches of an `if` or a `switch` on `builtin.os.tag` or
//!   `builtin.cpu.arch` the compiler does not analyze for the target —
//!   comptime-known, so only the one taken is compiled.
//!
//! Conservatively, always: a line is dimmed only when what leaves it out is
//! *definitively* false. A feature another package may turn on, a macro a
//! header may define, a Go version — anything undecidable is left active, so
//! live code is never hidden.

use std::collections::{HashMap, HashSet};

use clew_protocol::{BuildFacts, CrateFeatures};

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

    /// The target a rustc target triple names (`x86_64-unknown-linux-gnu`,
    /// `aarch64-apple-darwin`, `wasm32-unknown-unknown`), in `cfg`'s terms:
    /// its `target_os`, `target_arch` and `target_family` ("" for none).
    pub fn from_triple(triple: &str) -> Target {
        let parts: Vec<&str> = triple.split('-').collect();
        let arch = match parts.first().copied().unwrap_or_default() {
            "arm64" => "aarch64",
            "i386" | "i586" | "i686" => "x86",
            a if a.starts_with("thumb") || a.starts_with("armv") || a == "arm" => "arm",
            a if a.starts_with("riscv64") => "riscv64",
            a if a.starts_with("riscv32") => "riscv32",
            a => a,
        };
        let has = |p: &str| parts.iter().skip(1).any(|x| *x == p || x.starts_with(p));
        let os = if has("android") {
            "android"
        } else if has("linux") {
            "linux"
        } else if has("windows") {
            "windows"
        } else if has("darwin") || has("macos") {
            "macos"
        } else if has("ios") {
            "ios"
        } else if has("freebsd") {
            "freebsd"
        } else if has("netbsd") {
            "netbsd"
        } else if has("openbsd") {
            "openbsd"
        } else if has("wasi") {
            "wasi"
        } else if has("none") {
            "none"
        } else {
            "unknown"
        };
        let family = match (os, arch) {
            ("windows", _) => "windows",
            (_, "wasm32" | "wasm64") => "wasm",
            ("none" | "unknown", _) => "",
            _ => "unix",
        };
        Target {
            label: triple.to_string(),
            os: os.into(),
            arch: arch.into(),
            family: family.into(),
        }
    }

    /// This target's `GOOS`.
    fn goos(&self) -> &str {
        match self.os.as_str() {
            "macos" => "darwin",
            os => os,
        }
    }

    /// This target's `GOARCH`.
    fn goarch(&self) -> &str {
        match self.arch.as_str() {
            "x86_64" => "amd64",
            "aarch64" => "arm64",
            "x86" => "386",
            "wasm32" => "wasm",
            arch => arch,
        }
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

/// Whether what `lang_key`'s build leaves out depends on the platform it is
/// built for: Rust's `cfg`s, Go's build constraints, C's and C++'s
/// predefined macros, Zig's `builtin.os.tag`, Haskell's `<os>_HOST_OS`.
/// C# and Erlang decide by their files' own symbols alone.
pub fn reads_target(lang_key: &str) -> bool {
    matches!(lang_key, "rust" | "go" | "c" | "cpp" | "zig" | "haskell")
}

/// 0-based line numbers the build leaves out for `target`, with nothing
/// known of the project's build beyond it ([`inactive_lines_in`]).
pub fn inactive_lines(source: &str, lang_key: &str, target: &Target) -> HashSet<usize> {
    inactive_lines_in(source, lang_key, target, &BuildFacts::default(), None)
}

/// 0-based line numbers of `source` (the project's file `rel`, when known —
/// its name and its crate decide things too) that the build leaves out for
/// `target`, with what `facts` say of the project's build.
pub fn inactive_lines_in(
    source: &str,
    lang_key: &str,
    target: &Target,
    facts: &BuildFacts,
    rel: Option<&str>,
) -> HashSet<usize> {
    match lang_key {
        "rust" => {
            let target = match &facts.rust_target {
                Some(triple) => Target::from_triple(triple),
                None => target.clone(),
            };
            let cfg = Cfg {
                target: &target,
                features: rel.and_then(|rel| crate_of(facts, rel)),
            };
            rust_inactive(source, &cfg)
        }
        "go" => go_inactive(source, target, facts, rel),
        "c" | "cpp" => c_inactive(source, lang_key, target, facts, rel),
        "csharp" => csharp_inactive(source),
        "haskell" => haskell_inactive(source, target),
        "erlang" => erlang_inactive(source),
        "zig" => zig_inactive(source, target),
        _ => HashSet::new(),
    }
}

/// What a Rust `cfg` is evaluated against.
struct Cfg<'a> {
    target: &'a Target,
    /// The features of the crate the file is in, when they are known.
    features: Option<&'a CrateFeatures>,
}

/// The crate `rel` is in: the deepest crate directory holding it, when its
/// features are known.
fn crate_of<'a>(facts: &'a BuildFacts, rel: &str) -> Option<&'a CrateFeatures> {
    let rel = rel.replace('\\', "/");
    facts
        .crates
        .iter()
        .filter(|c| c.dir.is_empty() || rel.starts_with(&format!("{}/", c.dir)))
        .max_by_key(|c| c.dir.len())
        .filter(|c| c.known)
}

/// The lines of Rust `source` its inactive `#[cfg]`s gate off.
fn rust_inactive(source: &str, cfg: &Cfg) -> HashSet<usize> {
    let mut out = HashSet::new();
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
    let mut gated = Vec::new();
    walk(tree.root_node(), source.as_bytes(), cfg, &mut gated);
    // Nested gated items cover the same lines again: the ranges are merged
    // first, so each line is inserted once (inserting every range's lines cost
    // depth × lines for nested `#[cfg]` modules).
    gated.sort_unstable();
    let mut next = 0;
    for (start, end) in gated {
        for line in start.max(next)..=end {
            out.insert(line);
        }
        next = next.max(end + 1);
    }
    out
}

/// Walk the tree with an explicit stack rather than the call stack — see
/// [`crate::structure`] for why: a deeply nested expression is ~2 bytes per
/// level, so a file inside the size cap can reach a depth that overflows the
/// stack, and this runs on every file opened.
fn walk(root: Node, src: &[u8], host: &Cfg, out: &mut Vec<(usize, usize)>) {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        let mut cursor = node.walk();
        let before = stack.len();
        stack.extend(node.children(&mut cursor));
        stack[before..].reverse();
        walk_node(node, src, host, out);
    }
}

/// The per-node work, split out so the traversal above stays plain: the line
/// ranges `node`'s children gate off. Siblings are looked up in the child list
/// itself — tree-sitter finds a node's sibling through its parent, which it
/// finds by walking down from the root, so `next_named_sibling` cost the
/// node's depth per call.
fn walk_node(node: Node, src: &[u8], host: &Cfg, out: &mut Vec<(usize, usize)>) {
    let mut cursor = node.walk();
    let children: Vec<Node> = node.named_children(&mut cursor).collect();
    // For each child, the first later one that is not an attribute or a
    // comment: found by one pass from the end, the first time an attribute
    // needs it. Searched for from each attribute, a run of them cost its
    // length per attribute.
    let mut items_after: Option<Vec<Option<Node>>> = None;
    for (i, &child) in children.iter().enumerate() {
        // An outer `#[cfg(...)]` is a *preceding sibling* of the item it gates.
        // When it's inactive, dim from the attribute through the item that
        // follows (skipping any further attributes, and any comments, between
        // them). Comments have to be skipped as well: they are named siblings
        // too, so stopping at one dimmed the attribute and the comment while
        // leaving the function the `cfg` actually gates displayed as active.
        if child.kind() == "attribute_item" && cfg_of(child, src, host) == Some(false) {
            let items_after = items_after.get_or_insert_with(|| {
                let mut after = vec![None; children.len()];
                let mut next = None;
                for (j, n) in children.iter().enumerate().rev() {
                    after[j] = next;
                    if !matches!(
                        n.kind(),
                        "attribute_item" | "line_comment" | "block_comment"
                    ) {
                        next = Some(*n);
                    }
                }
                after
            });
            let item = items_after[i];
            // A match arm and a field initializer hold their attributes as
            // their OWN children, so the sibling found above is only the
            // pattern (or the field's name): the gated item is the parent.
            let gated = if matches!(
                node.kind(),
                "match_arm" | "field_initializer" | "shorthand_field_initializer"
            ) {
                node
            } else {
                item.unwrap_or(child)
            };
            out.push((child.start_position().row, gated.end_position().row));
        }
    }
}

/// Evaluate the `cfg` on an `attribute_item`, or `None` if it isn't a
/// `#[cfg(...)]` we can decide (inner `#![…]`, `cfg_attr`, …).
fn cfg_of(attr_item: Node, src: &[u8], host: &Cfg) -> Option<bool> {
    eval_cfg_at(cfg_predicate(attr_item.utf8_text(src).ok()?)?, host, 0)
}

/// The predicate of a Rust `#[cfg(...)]` attribute, `text` being the
/// attribute alone: `#[cfg(windows)]` gives `windows`. Not `cfg_attr`.
pub fn cfg_predicate(text: &str) -> Option<&str> {
    let inner = text.trim().strip_prefix("#[")?.strip_suffix(']')?.trim();
    // `cfg( … )` only — not `cfg_attr(…)` (which starts "cfg_attr" → no "(").
    let pred = inner
        .strip_prefix("cfg")?
        .trim_start()
        .strip_prefix('(')?
        .strip_suffix(')')?;
    Some(pred.trim())
}

/// Why a Rust `cfg` leaves what it gates out of the build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CfgReason {
    /// It is not built for this target, whatever the features.
    Target(Target),
    /// The features its crate builds with leave it out.
    Features,
}

/// Why a Rust `cfg` predicate leaves what it gates out of the build of the
/// file `rel`: the target it is read for, or the features its crate builds
/// with. `None` when it does not leave it out, or that cannot be told.
pub fn rust_cfg_reason(
    pred: &str,
    target: &Target,
    facts: &BuildFacts,
    rel: Option<&str>,
) -> Option<CfgReason> {
    let target = match &facts.rust_target {
        Some(triple) => Target::from_triple(triple),
        None => target.clone(),
    };
    let bare = Cfg {
        target: &target,
        features: None,
    };
    if eval_cfg_at(pred, &bare, 0) == Some(false) {
        return Some(CfgReason::Target(target));
    }
    let cfg = Cfg {
        target: &target,
        features: rel.and_then(|rel| crate_of(facts, rel)),
    };
    (eval_cfg_at(pred, &cfg, 0) == Some(false)).then_some(CfgReason::Features)
}

/// How deeply a `cfg` predicate may nest before clew stops evaluating it.
///
/// `not(` + `)` is five bytes per level, and the predicate comes from a source
/// file the repository controls — so without a limit a few hundred KB of one
/// attribute recurses deep enough to overflow the stack. Beyond this a
/// predicate is not something a human wrote, and "undecidable" (which callers
/// treat as active, i.e. nothing is dimmed) is the safe answer.
const MAX_CFG_DEPTH: usize = 64;

/// Evaluate a `cfg` predicate against `host` alone. `Some(false)` =
/// definitively inactive; `Some(true)` = active; `None` = undecidable
/// (treated as active by callers).
#[cfg(test)]
fn eval_cfg(pred: &str, host: &Target) -> Option<bool> {
    eval_cfg_at(
        pred,
        &Cfg {
            target: host,
            features: None,
        },
        0,
    )
}

fn eval_cfg_at(pred: &str, cfg: &Cfg, depth: usize) -> Option<bool> {
    let host = cfg.target;
    if depth > MAX_CFG_DEPTH {
        return None;
    }
    let pred = pred.trim();
    if let Some(inner) = pred.strip_prefix("all(").and_then(|s| s.strip_suffix(')')) {
        let mut result = Some(true);
        for part in split_top(inner) {
            match eval_cfg_at(&part, cfg, depth + 1) {
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
            match eval_cfg_at(&part, cfg, depth + 1) {
                Some(true) => return Some(true),
                None => result = None,
                Some(false) => {}
            }
        }
        return result;
    }
    if let Some(inner) = pred.strip_prefix("not(").and_then(|s| s.strip_suffix(')')) {
        return eval_cfg_at(inner, cfg, depth + 1).map(|b| !b);
    }
    if let Some((key, val)) = pred.split_once('=') {
        let val = rust_string_value(val.trim())?;
        return match key.trim() {
            "target_os" => Some(host.os == val),
            "target_arch" => Some(host.arch == val),
            "target_family" => Some(host.family == val),
            // On, off, or — another package may turn it on, or it is no
            // feature the package declares — undecided.
            "feature" => {
                let features = cfg.features?;
                if features.on.contains(&val) {
                    Some(true)
                } else if features.maybe.contains(&val) || !features.declared.contains(&val) {
                    None
                } else {
                    Some(false)
                }
            }
            _ => None, // target_env, … — leave active
        };
    }
    match pred {
        "unix" => Some(host.family == "unix"),
        "windows" => Some(host.family == "windows"),
        _ => None, // test, debug_assertions, bare feature, … — leave active
    }
}

/// The value of a Rust string literal, rather than its source spelling. A
/// literal we cannot decode is undecidable: it must never dim live code.
fn rust_string_value(literal: &str) -> Option<String> {
    if let Some(raw) = literal.strip_prefix('r') {
        let hashes = raw.bytes().take_while(|&b| b == b'#').count();
        if hashes > 255 {
            return None;
        }
        let body = raw.get(hashes..)?.strip_prefix('"')?;
        let suffix = format!("\"{}", "#".repeat(hashes));
        return body.strip_suffix(&suffix).map(str::to_string);
    }
    let body = literal.strip_prefix('"')?.strip_suffix('"')?;
    let mut chars = body.chars().peekable();
    let mut value = String::with_capacity(body.len());
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            value.push(ch);
            continue;
        }
        match chars.next()? {
            'n' => value.push('\n'),
            'r' => value.push('\r'),
            't' => value.push('\t'),
            '0' => value.push('\0'),
            '\\' => value.push('\\'),
            '\'' => value.push('\''),
            '"' => value.push('"'),
            'x' => {
                let high = chars.next()?.to_digit(16)?;
                let low = chars.next()?.to_digit(16)?;
                let byte = high * 16 + low;
                if byte > 0x7f {
                    return None;
                }
                value.push(char::from_u32(byte)?);
            }
            'u' => {
                if chars.next()? != '{' {
                    return None;
                }
                let mut codepoint = 0u32;
                let mut digits = 0;
                loop {
                    match chars.next()? {
                        '}' if digits > 0 => break,
                        '_' => {}
                        hex => {
                            digits += 1;
                            if digits > 6 {
                                return None;
                            }
                            codepoint = codepoint * 16 + hex.to_digit(16)?;
                        }
                    }
                }
                value.push(char::from_u32(codepoint)?);
            }
            newline @ ('\n' | '\r') => {
                if newline == '\r' && chars.next()? != '\n' {
                    return None;
                }
                while chars
                    .peek()
                    .is_some_and(|c| matches!(c, ' ' | '\t' | '\n' | '\r'))
                {
                    chars.next();
                }
            }
            _ => return None,
        }
    }
    Some(value)
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

/// Which part of the target a Zig expression names.
#[derive(Clone, Copy, PartialEq)]
enum ZigFact {
    Os,
    Arch,
}

/// The lines of the Zig file `source` in branches the compiler leaves out
/// for `target`: of an `if` (statement or expression) whose condition
/// compares `builtin.os.tag` or `builtin.cpu.arch` with a tag, and of a
/// `switch` on one of them — whole lines only, never one that holds a taken
/// part as well.
fn zig_inactive(source: &str, target: &Target) -> HashSet<usize> {
    let mut out = HashSet::new();
    let Some(lang) = crate::highlight::language_for("zig") else {
        return out;
    };
    let mut parser = Parser::new();
    if parser.set_language(&lang).is_err() {
        return out;
    }
    let Some(tree) = parser.parse(source, None) else {
        return out;
    };
    let zig = ZigTarget::new(source, target);
    let src = source.as_bytes();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor));
        let (dead, live) = match node.kind() {
            "if_statement" | "if_expression" => zig.if_branches(node, src),
            "switch_expression" => zig.switch_branches(node, src),
            _ => continue,
        };
        let live: HashSet<usize> = live
            .iter()
            .flat_map(|n| n.start_position().row..=n.end_position().row)
            .collect();
        for branch in dead {
            for row in branch.start_position().row..=branch.end_position().row {
                if !live.contains(&row) {
                    out.insert(row);
                }
            }
        }
    }
    out
}

/// The target as Zig names it, and the names a file gives `@import("builtin")`
/// and its `os.tag` / `cpu.arch`.
struct ZigTarget {
    os: String,
    arch: String,
    /// What `@import("builtin")` is bound to (`builtin`, by convention).
    builtins: Vec<String>,
    /// Names bound to the OS tag (`const native_os = builtin.os.tag;`) or
    /// the architecture.
    aliases: Vec<(String, ZigFact)>,
}

impl ZigTarget {
    fn new(source: &str, target: &Target) -> ZigTarget {
        let os = match target.os.as_str() {
            "android" => "linux",
            os => os,
        };
        let mut zig = ZigTarget {
            os: os.to_string(),
            arch: target.arch.clone(),
            builtins: Vec::new(),
            aliases: Vec::new(),
        };
        // `const NAME = VALUE;` — at any depth, as the file binds them.
        for line in source.lines() {
            let line: String = line.chars().filter(|c| !c.is_whitespace()).collect();
            let Some(rest) = line
                .strip_prefix("pubconst")
                .or_else(|| line.strip_prefix("const"))
            else {
                continue;
            };
            let Some((name, value)) = rest.split_once('=') else {
                continue;
            };
            let name = name.split(':').next().unwrap_or(name);
            let Some(value) = value.strip_suffix(';') else {
                continue;
            };
            if value == "@import(\"builtin\")" {
                zig.builtins.push(name.to_string());
            } else if let Some(fact) = zig.subject(value) {
                zig.aliases.push((name.to_string(), fact));
            }
        }
        zig
    }

    /// Which part of the target `expr` (its text) names, if one.
    fn subject(&self, expr: &str) -> Option<ZigFact> {
        let expr: String = expr.chars().filter(|c| !c.is_whitespace()).collect();
        if let Some((_, fact)) = self.aliases.iter().find(|(name, _)| *name == expr) {
            return Some(*fact);
        }
        let rest = expr.strip_prefix("@import(\"builtin\")").or_else(|| {
            self.builtins
                .iter()
                .find_map(|b| expr.strip_prefix(b.as_str()))
        })?;
        let rest = rest.strip_prefix(".target").unwrap_or(rest);
        match rest {
            ".os.tag" => Some(ZigFact::Os),
            ".cpu.arch" => Some(ZigFact::Arch),
            _ => None,
        }
    }

    /// Whether `tag` (an enum literal's name) is the target's.
    fn is(&self, fact: ZigFact, tag: &str) -> bool {
        match fact {
            ZigFact::Os => tag == self.os,
            ZigFact::Arch => tag == self.arch,
        }
    }

    /// A condition's truth, when it compares the target with tags: `==`,
    /// `!=`, `and`, `or`, `!` and parentheses.
    fn condition(&self, node: Node, src: &[u8], depth: usize) -> Option<bool> {
        if depth > MAX_CFG_DEPTH {
            return None;
        }
        let text = node.utf8_text(src).ok()?.trim();
        match node.kind() {
            "parenthesized_expression" | "grouped_expression" => {
                let inner = node.named_child(0)?;
                self.condition(inner, src, depth + 1)
            }
            "unary_expression" if text.starts_with('!') => {
                let inner = node.named_child(0)?;
                self.condition(inner, src, depth + 1).map(|b| !b)
            }
            "binary_expression" => {
                let left = node.child_by_field_name("left")?;
                let right = node.child_by_field_name("right")?;
                let op = std::str::from_utf8(&src[left.end_byte()..right.start_byte()])
                    .ok()?
                    .trim();
                match op {
                    "and" => and(
                        self.condition(left, src, depth + 1),
                        self.condition(right, src, depth + 1),
                    ),
                    "or" => or(
                        self.condition(left, src, depth + 1),
                        self.condition(right, src, depth + 1),
                    ),
                    "==" | "!=" => {
                        let l = left.utf8_text(src).ok()?;
                        let r = right.utf8_text(src).ok()?;
                        let (fact, tag) = match (self.subject(l), enum_literal(r)) {
                            (Some(fact), Some(tag)) => (fact, tag),
                            _ => (self.subject(r)?, enum_literal(l)?),
                        };
                        let equal = self.is(fact, tag);
                        Some(if op == "==" { equal } else { !equal })
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// An `if`'s branches the target leaves out, and those it takes.
    fn if_branches<'t>(&self, node: Node<'t>, src: &[u8]) -> (Vec<Node<'t>>, Vec<Node<'t>>) {
        let Some(condition) = node.child_by_field_name("condition") else {
            return (Vec::new(), Vec::new());
        };
        let Some(value) = self.condition(condition, src, 0) else {
            return (Vec::new(), Vec::new());
        };
        // The branches after the condition: the one taken when it holds,
        // then the `else`'s.
        let mut cursor = node.walk();
        let branches: Vec<Node> = node
            .named_children(&mut cursor)
            .filter(|c| c.id() != condition.id())
            .collect();
        let (then, otherwise) = match branches.as_slice() {
            [then, otherwise, ..] => (*then, Some(*otherwise)),
            [then] => (*then, None),
            [] => return (Vec::new(), Vec::new()),
        };
        // An `else` clause: its alternative is the branch.
        let otherwise = otherwise.map(|o| {
            o.child_by_field_name("alternative")
                .filter(|_| o.kind() == "else_clause")
                .unwrap_or(o)
        });
        let mut live = vec![condition];
        let mut dead = Vec::new();
        match (value, otherwise) {
            (true, Some(otherwise)) => {
                live.push(then);
                dead.push(otherwise);
            }
            (true, None) => live.push(then),
            (false, otherwise) => {
                dead.push(then);
                live.extend(otherwise);
            }
        }
        (dead, live)
    }

    /// A `switch` on the target's OS or architecture: the cases it does not
    /// take, and the one it does. Undecided — nothing left out — when a
    /// case names anything but tags.
    fn switch_branches<'t>(&self, node: Node<'t>, src: &[u8]) -> (Vec<Node<'t>>, Vec<Node<'t>>) {
        let none = (Vec::new(), Vec::new());
        let Some(subject) = node.named_child(0) else {
            return none;
        };
        let Some(fact) = subject.utf8_text(src).ok().and_then(|t| self.subject(t)) else {
            return none;
        };
        let mut cursor = node.walk();
        let cases: Vec<Node> = node
            .named_children(&mut cursor)
            .filter(|c| c.kind() == "switch_case")
            .collect();
        let mut matched: Vec<(Node, Option<bool>)> = Vec::new();
        for case in cases {
            let Ok(text) = case.utf8_text(src) else {
                return none;
            };
            let Some((items, _)) = text.split_once("=>") else {
                return none;
            };
            let items = items.trim();
            if items == "else" {
                matched.push((case, None));
                continue;
            }
            let mut tags = Vec::new();
            for item in items.split(',').map(str::trim).filter(|i| !i.is_empty()) {
                match enum_literal(item) {
                    Some(tag) => tags.push(tag),
                    None => return none,
                }
            }
            matched.push((case, Some(tags.iter().any(|t| self.is(fact, t)))));
        }
        let any = matched.iter().any(|(_, m)| *m == Some(true));
        let (mut dead, mut live) = (Vec::new(), vec![subject]);
        for (case, taken) in matched {
            // `else` is taken when no case names the target.
            match taken.unwrap_or(!any) {
                true => live.push(case),
                false => dead.push(case),
            }
        }
        (dead, live)
    }
}

/// The name of a Zig enum literal (`.windows`), if `text` is one.
fn enum_literal(text: &str) -> Option<&str> {
    let name = text.trim().strip_prefix('.')?;
    (!name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
        .then_some(name)
}

/// Every line of `source`./// Every line of `source`.
fn all_lines(source: &str) -> HashSet<usize> {
    (0..source.lines().count().max(1)).collect()
}

/// The lines of the Go file `source` (named `rel`) the build leaves out:
/// all of them, when its name or its build constraint excludes it from the
/// build for the platform (the one gopls loads, else `target`) and the tags
/// set; none otherwise.
fn go_inactive(
    source: &str,
    target: &Target,
    facts: &BuildFacts,
    rel: Option<&str>,
) -> HashSet<usize> {
    let go = GoBuild {
        goos: facts.goos.as_deref().unwrap_or(target.goos()),
        goarch: facts.goarch.as_deref().unwrap_or(target.goarch()),
        tags: facts.go_tags.as_deref(),
    };
    let by_name = rel.and_then(|rel| {
        let name = rel.rsplit(['/', '\\']).next().unwrap_or(rel);
        go.file_name_matches(name)
    });
    if by_name == Some(false) || go.constraint_matches(source) == Some(false) {
        return all_lines(source);
    }
    HashSet::new()
}

/// What a Go build is: the platform, and the tags set (`None` when they
/// cannot be known).
struct GoBuild<'a> {
    goos: &'a str,
    goarch: &'a str,
    tags: Option<&'a [String]>,
}

/// The operating systems Go counts as Unix (the `unix` constraint).
const GO_UNIX: &[&str] = &[
    "aix",
    "android",
    "darwin",
    "dragonfly",
    "freebsd",
    "hurd",
    "illumos",
    "ios",
    "linux",
    "netbsd",
    "openbsd",
    "solaris",
];

impl GoBuild<'_> {
    /// Whether build constraint name `tag` holds; `None` when it cannot be
    /// known (a Go version, `cgo`, a tag when the tags are unknown).
    fn holds(&self, tag: &str) -> Option<bool> {
        let os = self.goos;
        let implied = matches!(
            (tag, os),
            ("linux", "android") | ("solaris", "illumos") | ("darwin", "ios")
        );
        if tag == os || implied || tag == self.goarch {
            return Some(true);
        }
        if crate::lsp::langopts::GOOS.contains(&tag) || crate::lsp::langopts::GOARCH.contains(&tag)
        {
            return Some(false);
        }
        match tag {
            "unix" => Some(GO_UNIX.contains(&os)),
            "gc" => Some(true),
            "gccgo" => Some(false),
            "cgo" => None,
            t if t.starts_with("go1.") => None,
            t => self.tags.map(|tags| tags.iter().any(|x| x == t)),
        }
    }

    /// Whether a file named `name` is built, by the `_GOOS`/`_GOARCH`
    /// suffixes of its name — as the go command reads them: never the part
    /// before the first `_`, and `_test` aside.
    fn file_name_matches(&self, name: &str) -> Option<bool> {
        let stem = name.strip_suffix(".go")?;
        let stem = stem.strip_suffix("_test").unwrap_or(stem);
        let (_, constrained) = stem.split_once('_')?;
        let parts: Vec<&str> = constrained.split('_').collect();
        let known_os = |p: &str| crate::lsp::langopts::GOOS.contains(&p);
        let known_arch = |p: &str| crate::lsp::langopts::GOARCH.contains(&p);
        let n = parts.len();
        if n >= 2 && known_os(parts[n - 2]) && known_arch(parts[n - 1]) {
            return Some(self.holds(parts[n - 2])? && self.holds(parts[n - 1])?);
        }
        let last = parts[n - 1];
        if known_os(last) || known_arch(last) {
            return self.holds(last);
        }
        None
    }

    /// Whether the build constraint at the head of `source` holds: its
    /// `//go:build` line, else its `// +build` lines (each an OR of
    /// space-separated ANDs of comma-separated terms, all of them ANDed).
    /// `None` when there is none, or it cannot be decided.
    fn constraint_matches(&self, source: &str) -> Option<bool> {
        let mut plus: Vec<&str> = Vec::new();
        let mut in_block = false;
        for line in source.lines().map(str::trim) {
            if in_block {
                in_block = !line.contains("*/");
                continue;
            }
            if line.is_empty() {
                continue;
            }
            if let Some(expr) = line.strip_prefix("//go:build") {
                if !expr.starts_with([' ', '\t']) {
                    continue;
                }
                return self.expr(expr);
            }
            if let Some(expr) = line.strip_prefix("// +build ") {
                plus.push(expr);
                continue;
            }
            if line.starts_with("//") {
                continue;
            }
            if line.starts_with("/*") {
                in_block = !line.contains("*/");
                continue;
            }
            break; // the package clause, or anything else: the header ends
        }
        if plus.is_empty() {
            return None;
        }
        let mut all = Some(true);
        for line in plus {
            let mut any = Some(false);
            for option in line.split_whitespace() {
                let mut both = Some(true);
                for term in option.split(',') {
                    let value = match term.strip_prefix('!') {
                        Some(t) => self.holds(t).map(|b| !b),
                        None => self.holds(term),
                    };
                    both = and(both, value);
                }
                any = or(any, both);
            }
            all = and(all, any);
        }
        all
    }

    /// A `//go:build` expression: `||`, `&&`, `!`, parentheses and names.
    fn expr(&self, expr: &str) -> Option<bool> {
        let tokens = go_tokens(expr)?;
        let mut parser = GoExpr {
            tokens: &tokens,
            at: 0,
            build: self,
            depth: 0,
        };
        let value = parser.or()?;
        (parser.at == tokens.len()).then_some(value)?
    }
}

/// The tokens of a `//go:build` expression, or `None` for a character it
/// may not hold.
fn go_tokens(expr: &str) -> Option<Vec<String>> {
    let mut tokens = Vec::new();
    let mut chars = expr.chars().peekable();
    while let Some(&c) = chars.peek() {
        match c {
            ' ' | '\t' => {
                chars.next();
            }
            '!' | '(' | ')' => {
                tokens.push(c.to_string());
                chars.next();
            }
            '&' | '|' => {
                chars.next();
                if chars.next() != Some(c) {
                    return None;
                }
                tokens.push(format!("{c}{c}"));
            }
            c if c.is_ascii_alphanumeric() || c == '_' || c == '.' => {
                let mut name = String::new();
                while let Some(&c) = chars.peek() {
                    if c.is_ascii_alphanumeric() || c == '_' || c == '.' {
                        name.push(c);
                        chars.next();
                    } else {
                        break;
                    }
                }
                tokens.push(name);
            }
            _ => return None,
        }
    }
    Some(tokens)
}

/// A recursive-descent reader of a `//go:build` expression, three-valued:
/// `Some(Some(b))` decided, `Some(None)` undecidable, `None` malformed.
struct GoExpr<'a> {
    tokens: &'a [String],
    at: usize,
    build: &'a GoBuild<'a>,
    depth: usize,
}

impl GoExpr<'_> {
    fn peek(&self) -> Option<&str> {
        self.tokens.get(self.at).map(String::as_str)
    }

    fn or(&mut self) -> Option<Option<bool>> {
        let mut value = self.and()?;
        while self.peek() == Some("||") {
            self.at += 1;
            value = or(value, self.and()?);
        }
        Some(value)
    }

    fn and(&mut self) -> Option<Option<bool>> {
        let mut value = self.not()?;
        while self.peek() == Some("&&") {
            self.at += 1;
            value = and(value, self.not()?);
        }
        Some(value)
    }

    fn not(&mut self) -> Option<Option<bool>> {
        self.depth += 1;
        if self.depth > MAX_CFG_DEPTH {
            return None;
        }
        let value = match self.peek()? {
            "!" => {
                self.at += 1;
                self.not()?.map(|b| !b)
            }
            "(" => {
                self.at += 1;
                let value = self.or()?;
                if self.peek()? != ")" {
                    return None;
                }
                self.at += 1;
                value
            }
            "&&" | "||" | ")" => return None,
            name => {
                let value = self.build.holds(name);
                self.at += 1;
                value
            }
        };
        self.depth -= 1;
        Some(value)
    }
}

/// Three-valued AND: false wins, else unknown wins.
fn and(a: Option<bool>, b: Option<bool>) -> Option<bool> {
    match (a, b) {
        (Some(false), _) | (_, Some(false)) => Some(false),
        (Some(true), Some(true)) => Some(true),
        _ => None,
    }
}

/// Three-valued OR: true wins, else unknown wins.
fn or(a: Option<bool>, b: Option<bool>) -> Option<bool> {
    match (a, b) {
        (Some(true), _) | (_, Some(true)) => Some(true),
        (Some(false), Some(false)) => Some(false),
        _ => None,
    }
}

/// What the preprocessor knows of a macro.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Macro {
    /// Defined, with its value when it is a number.
    Defined(Option<i64>),
    /// Not defined.
    Undefined,
}

/// The platform macros C compilers define, by the platform they mean: each
/// is defined for its own and undefined for every other.
const PLATFORM_MACROS: &[(&str, &[&str])] = &[
    ("_WIN32", &["windows"]),
    ("__linux__", &["linux", "android"]),
    ("__linux", &["linux", "android"]),
    ("__ANDROID__", &["android"]),
    ("__APPLE__", &["macos", "ios"]),
    ("__MACH__", &["macos", "ios"]),
    ("__FreeBSD__", &["freebsd"]),
    (
        "__unix__",
        &["linux", "android", "freebsd", "netbsd", "openbsd"],
    ),
    (
        "__unix",
        &["linux", "android", "freebsd", "netbsd", "openbsd"],
    ),
];

/// The architecture macros (GCC's and Clang's spelling), by architecture.
const ARCH_MACROS: &[(&str, &[&str])] = &[
    ("__x86_64__", &["x86_64"]),
    ("__amd64__", &["x86_64"]),
    ("__aarch64__", &["aarch64"]),
    ("__i386__", &["x86"]),
    ("__arm__", &["arm"]),
];

/// The lines of the C or C++ file `source` (named `rel`) its preprocessor
/// conditionals drop: the groups whose condition is definitively false for
/// the platform's macros, the flags' `-D`/`-U` (when known), and the file's
/// own `#define`/`#undef`s before them.
fn c_inactive(
    source: &str,
    lang_key: &str,
    target: &Target,
    facts: &BuildFacts,
    rel: Option<&str>,
) -> HashSet<usize> {
    let mut macros: HashMap<String, Macro> = HashMap::new();
    for (table, value) in [
        (PLATFORM_MACROS, target.os.as_str()),
        (ARCH_MACROS, target.arch.as_str()),
    ] {
        for (name, on) in table {
            let state = match on.contains(&value) {
                true => Macro::Defined(Some(1)),
                false => Macro::Undefined,
            };
            macros.insert(name.to_string(), state);
        }
    }
    if target.os == "windows" && matches!(target.arch.as_str(), "x86_64" | "aarch64") {
        macros.insert("_WIN64".into(), Macro::Defined(Some(1)));
    } else {
        macros.insert("_WIN64".into(), Macro::Undefined);
    }
    // C++ or C, where the file says which it is compiled as.
    let ext = rel
        .and_then(|r| r.rsplit_once('.'))
        .map(|(_, e)| e.to_ascii_lowercase());
    match (lang_key, ext.as_deref()) {
        (_, Some("h")) | (_, None) => {}
        ("cpp", _) => {
            macros.insert("__cplusplus".into(), Macro::Defined(None));
        }
        (_, Some("c")) => {
            macros.insert("__cplusplus".into(), Macro::Undefined);
        }
        _ => {}
    }
    if let Some(defines) = &facts.c_defines {
        for m in defines {
            macros.insert(m.name.clone(), Macro::Defined(m.value));
        }
        for name in &facts.c_undefines {
            macros.insert(name.clone(), Macro::Undefined);
        }
    }
    Preprocessor::new(macros, Dialect::C).run(source)
}

/// The lines of the C# file `source` its `#if` groups drop. Nothing of the
/// build is known: `DEBUG`, `TRACE`, the project's `DefineConstants` and the
/// framework's symbols (`NET8_0`, …) come from the project file and the
/// configuration built. A symbol is known only where the file `#define`s or
/// `#undef`s it — which C# allows only at its top, before any code, so it
/// holds for the whole file.
fn csharp_inactive(source: &str) -> HashSet<usize> {
    Preprocessor::new(HashMap::new(), Dialect::CSharp).run(source)
}

/// GHC's operating-system macros, by the target OS each means. Only these
/// targets decide them: GHC names a platform's macro after the OS part of
/// its own target triple, and for any other target here (Android, iOS, a
/// BSD) that name is not one clew can be sure of — so on those all of them
/// stay unknown.
const GHC_OS_MACROS: &[(&str, &str)] = &[
    ("linux_HOST_OS", "linux"),
    ("darwin_HOST_OS", "macos"),
    ("mingw32_HOST_OS", "windows"),
];

/// GHC's architecture macros, by the target architecture each means; as
/// with [`GHC_OS_MACROS`], only these architectures decide them.
const GHC_ARCH_MACROS: &[(&str, &str)] = &[
    ("x86_64_HOST_ARCH", "x86_64"),
    ("aarch64_HOST_ARCH", "aarch64"),
];

/// The lines of the Haskell file `source` its C preprocessor drops for
/// `target` — none unless the file turns the preprocessor on. GHC's own
/// `<os>_HOST_OS` and `<arch>_HOST_ARCH` are known, one of each defined,
/// and so are the file's own `#define`s. Everything else stays unknown: C's
/// platform macros (`__linux__`) are not GHC's to say, and what a
/// package's `cpp-options` define, `__GLASGOW_HASKELL__` and
/// `MIN_VERSION_base(…)` are the build's.
fn haskell_inactive(source: &str, target: &Target) -> HashSet<usize> {
    if !haskell_uses_cpp(source) {
        return HashSet::new();
    }
    let mut macros: HashMap<String, Macro> = HashMap::new();
    for (table, value) in [
        (GHC_OS_MACROS, target.os.as_str()),
        (GHC_ARCH_MACROS, target.arch.as_str()),
    ] {
        if !table.iter().any(|&(_, on)| on == value) {
            continue;
        }
        for &(name, on) in table {
            let state = match on == value {
                true => Macro::Defined(Some(1)),
                false => Macro::Undefined,
            };
            macros.insert(name.to_string(), state);
        }
    }
    Preprocessor::new(macros, Dialect::C).run(source)
}

/// Whether the Haskell file `source` turns the C preprocessor on itself: a
/// `{-# LANGUAGE … CPP … #-}` pragma, or `-XCPP` (or the older `-cpp`) in an
/// `OPTIONS_GHC` one. A package can turn it on for all its files in its
/// `.cabal` instead; such a file is not known to run it, so nothing in it is
/// dimmed.
fn haskell_uses_cpp(source: &str) -> bool {
    let mut rest = source;
    while let Some(start) = rest.find("{-#") {
        let after = &rest[start + 3..];
        let Some(end) = after.find("#-}") else {
            break;
        };
        let mut words = after[..end]
            .split(|c: char| c.is_whitespace() || c == ',')
            .filter(|w| !w.is_empty());
        let pragma = words.next().map(str::to_ascii_uppercase);
        let on = match pragma.as_deref() {
            Some("LANGUAGE") => words.any(|w| w == "CPP"),
            Some("OPTIONS_GHC" | "OPTIONS") => words.any(|w| w == "-XCPP" || w == "-cpp"),
            _ => false,
        };
        if on {
            return true;
        }
        rest = &after[end + 3..];
    }
    false
}

/// Whether a region of a C file is compiled.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Live {
    Yes,
    No,
    Maybe,
}

impl Live {
    fn of(value: Option<bool>) -> Live {
        match value {
            Some(true) => Live::Yes,
            Some(false) => Live::No,
            None => Live::Maybe,
        }
    }

    fn known(self) -> Option<bool> {
        match self {
            Live::Yes => Some(true),
            Live::No => Some(false),
            Live::Maybe => None,
        }
    }

    fn and(self, other: Live) -> Live {
        Live::of(and(self.known(), other.known()))
    }
}

/// One open `#if` chain.
struct Group {
    /// Whether the region around the chain is compiled.
    outer: Live,
    /// Whether a branch of the chain was already taken.
    taken: Option<bool>,
    /// Whether the current branch is compiled (by its condition alone).
    branch: Live,
}

/// Whose `#` directives a file's are.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Dialect {
    /// C's — C++'s too, and GHC's for a Haskell file: a line ending in a
    /// backslash goes on to the next, macros have values, and `#if` does
    /// arithmetic.
    C,
    /// C#'s: no line continues another, and `#if` reads symbols that are
    /// only defined or not, joined by `!`, `&&`, `||`, `==`, `!=` and
    /// parentheses, with `true` and `false`. It has no `#ifdef`.
    CSharp,
}

/// The preprocessor's view of a file, as far as it can be known.
struct Preprocessor {
    macros: HashMap<String, Macro>,
    groups: Vec<Group>,
    dialect: Dialect,
}

/// Most nested `#if`s followed; past it, nothing more is dimmed.
const MAX_GROUPS: usize = 256;

impl Preprocessor {
    fn new(macros: HashMap<String, Macro>, dialect: Dialect) -> Preprocessor {
        Preprocessor {
            macros,
            groups: Vec::new(),
            dialect,
        }
    }

    /// Whether a directive's own lines are dropped here. They are only
    /// inside a region dropped around its whole chain: one that `continues`
    /// a chain (an `#elif`, `#else` or `#endif`) is judged by the region
    /// around the chain, not by the branch before it — so an `#else` between
    /// two dropped branches stays in sight, and the reader still sees where
    /// each branch begins and what it hangs on.
    fn drops_directive(&self, continues: bool) -> bool {
        let around = match continues {
            true => self.groups.last().map_or(Live::Yes, |g| g.outer),
            false => self.live(),
        };
        around == Live::No
    }

    /// What a `#define` or `#undef` of `name` here makes of it: nothing in a
    /// dropped region; in one that may not be compiled, no longer known.
    fn set(&mut self, name: &str, value: Macro) {
        match self.live() {
            Live::No => {}
            Live::Maybe => {
                self.macros.remove(name);
            }
            Live::Yes => {
                self.macros.insert(name.to_string(), value);
            }
        }
    }

    /// Whether the code here is compiled.
    fn live(&self) -> Live {
        self.groups
            .last()
            .map(|g| g.outer.and(g.branch))
            .unwrap_or(Live::Yes)
    }

    /// The lines of `source` in groups the preprocessor drops.
    fn run(mut self, source: &str) -> HashSet<usize> {
        let mut out = HashSet::new();
        let lines: Vec<&str> = source.lines().collect();
        let mut in_comment = false;
        let mut i = 0;
        while i < lines.len() {
            let first = i;
            // In C a directive continues over lines ending in a backslash.
            // C# has no such thing: a comment ending in `C:\temp\` would
            // have swallowed the `#endif` below it.
            let mut text = lines[i].to_string();
            while self.dialect == Dialect::C && text.ends_with('\\') && i + 1 < lines.len() {
                text.pop();
                i += 1;
                text.push(' ');
                text.push_str(lines[i]);
            }
            let directive = (!in_comment)
                .then(|| text.trim_start().strip_prefix('#'))
                .flatten()
                .map(|d| strip_comments(d.trim()));
            match directive {
                Some(directive) => {
                    let continues =
                        ["elif", "elifdef", "elifndef", "else", "endif"]
                            .iter()
                            .any(|w| {
                                directive.strip_prefix(w).is_some_and(|rest| {
                                    rest.is_empty() || rest.starts_with([' ', '\t', '('])
                                })
                            });
                    if self.drops_directive(continues) {
                        out.extend(first..=i);
                    }
                    let next = lines[i + 1..]
                        .iter()
                        .map(|l| l.trim())
                        .find(|l| l.starts_with('#'))
                        .map(|l| strip_comments(l[1..].trim()));
                    self.directive(&directive, next.as_deref());
                }
                None => {
                    in_comment = ends_in_comment(&text, in_comment);
                    if self.live() == Live::No {
                        out.extend(first..=i);
                    }
                }
            }
            i += 1;
        }
        out
    }

    /// Apply one directive (without its `#`); `next` is the directive after
    /// it, for an include guard.
    fn directive(&mut self, directive: &str, next: Option<&str>) {
        let (word, rest) = directive
            .split_once(|c: char| c.is_whitespace() || c == '(')
            .map(|(w, _)| (w, directive[w.len()..].trim()))
            .unwrap_or((directive, ""));
        let name = || {
            rest.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .next()
                .unwrap_or_default()
        };
        // C#'s `#region`, `#pragma`, `#nullable` and the rest open nothing.
        // It has no `#ifdef`: one is read as a group that decides nothing,
        // so that its `#endif` still closes it rather than the group around.
        if self.dialect == Dialect::CSharp {
            match word {
                "if" | "elif" | "else" | "endif" | "define" | "undef" => {}
                "ifdef" | "ifndef" => return self.open(None),
                "elifdef" | "elifndef" => return self.other(None),
                _ => return,
            }
        }
        match word {
            "if" => self.open(self.condition(rest)),
            "ifdef" => self.open(self.defined(name())),
            "ifndef" => {
                // An include guard (`#ifndef X` then `#define X`) is open
                // the first time the file is read — as it is read here.
                let guard = next.is_some_and(|n| {
                    n.strip_prefix("define")
                        .is_some_and(|d| d.split_whitespace().next() == Some(name()))
                });
                let value = match guard && self.defined(name()).is_none() {
                    true => Some(true),
                    false => self.defined(name()).map(|b| !b),
                };
                self.open(value)
            }
            "elif" => self.other(self.condition(rest)),
            "elifdef" => self.other(self.defined(name())),
            "elifndef" => self.other(self.defined(name()).map(|b| !b)),
            "else" => self.other(Some(true)),
            "endif" => {
                self.groups.pop();
            }
            "define" | "undef" => {
                let name = name();
                if name.is_empty() {
                    return;
                }
                let value = match (word, self.dialect) {
                    ("undef", _) => Macro::Undefined,
                    // A C# symbol has no value: defined, it reads as true.
                    (_, Dialect::CSharp) => Macro::Defined(Some(1)),
                    _ => {
                        let body = &rest[name.len()..];
                        match body.starts_with('(') {
                            true => Macro::Defined(None),
                            false => Macro::Defined(crate::buildfacts::number(body)),
                        }
                    }
                };
                self.set(name, value);
            }
            _ => {}
        }
    }

    /// Open an `#if` chain whose first condition is `value`.
    fn open(&mut self, value: Option<bool>) {
        if self.groups.len() >= MAX_GROUPS {
            // Followed no deeper: everything below is undecided.
            self.groups.push(Group {
                outer: Live::Maybe,
                taken: None,
                branch: Live::Maybe,
            });
            return;
        }
        let outer = self.live();
        self.groups.push(Group {
            outer,
            taken: value,
            branch: Live::of(value),
        });
    }

    /// The next branch of the open chain, whose condition is `value`.
    fn other(&mut self, value: Option<bool>) {
        let Some(group) = self.groups.last_mut() else {
            return;
        };
        let untaken = group.taken.map(|t| !t);
        group.branch = Live::of(and(untaken, value));
        group.taken = or(group.taken, value);
    }

    /// Whether macro `name` is defined, when known.
    fn defined(&self, name: &str) -> Option<bool> {
        match self.macros.get(name)? {
            Macro::Defined(_) => Some(true),
            Macro::Undefined => Some(false),
        }
    }

    /// An `#if` condition's truth, when it can be known.
    fn condition(&self, expr: &str) -> Option<bool> {
        let tokens = c_tokens(expr)?;
        // C#'s `#if` has no numbers, arithmetic, comparisons of order or
        // `defined`: a condition that uses them is no C#, and decides
        // nothing.
        if self.dialect == Dialect::CSharp && !tokens.iter().all(|t| is_csharp_token(t)) {
            return None;
        }
        let mut parser = CExpr {
            tokens: &tokens,
            at: 0,
            pre: self,
            depth: 0,
        };
        let value = parser.or()?;
        if parser.at != tokens.len() {
            return None;
        }
        value.map(|v| v != 0)
    }
}

/// Whether `token` (of [`c_tokens`]) may be in a C# `#if`: an operator it
/// has, or a symbol — `true` and `false` among them.
fn is_csharp_token(token: &str) -> bool {
    matches!(token, "!" | "&&" | "||" | "==" | "!=" | "(" | ")")
        || (token != "defined"
            && token.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            && token.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
}

/// `line` (a directive) without its comments.
fn strip_comments(line: &str) -> String {
    let mut out = String::new();
    let mut rest = line;
    loop {
        let line_comment = rest.find("//");
        let block = rest.find("/*");
        match (line_comment, block) {
            (Some(l), b) if b.is_none_or(|b| l < b) => {
                out.push_str(&rest[..l]);
                break;
            }
            (_, Some(b)) => {
                out.push_str(&rest[..b]);
                out.push(' ');
                match rest[b + 2..].find("*/") {
                    Some(end) => rest = &rest[b + 2 + end + 2..],
                    None => break,
                }
            }
            _ => {
                out.push_str(rest);
                break;
            }
        }
    }
    out.trim().to_string()
}

/// Whether a block comment is open at the end of `line`, given whether one
/// was at its start. Strings are not looked into: a `/*` in one is rare in a
/// line a directive could follow.
fn ends_in_comment(line: &str, mut open: bool) -> bool {
    let mut rest = line;
    loop {
        if open {
            match rest.find("*/") {
                Some(end) => {
                    open = false;
                    rest = &rest[end + 2..];
                }
                None => return true,
            }
        } else {
            let line_comment = rest.find("//");
            match rest.find("/*") {
                Some(start) if line_comment.is_none_or(|l| start < l) => {
                    open = true;
                    rest = &rest[start + 2..];
                }
                _ => return false,
            }
        }
    }
}

/// The tokens of an `#if` expression, or `None` for one it may not be read
/// in (a string, a character constant, an operator not handled).
fn c_tokens(expr: &str) -> Option<Vec<String>> {
    let mut tokens = Vec::new();
    let chars: Vec<char> = expr.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c.is_ascii_alphanumeric() || c == '_' {
            let start = i;
            while i < chars.len()
                && (chars[i].is_ascii_alphanumeric() || chars[i] == '_' || chars[i] == '\'')
            {
                i += 1;
            }
            tokens.push(chars[start..i].iter().collect());
            continue;
        }
        let two: String = chars[i..(i + 2).min(chars.len())].iter().collect();
        if ["&&", "||", "==", "!=", "<=", ">="].contains(&two.as_str()) {
            tokens.push(two);
            i += 2;
            continue;
        }
        if "!()<>+-*".contains(c) {
            tokens.push(c.to_string());
            i += 1;
            continue;
        }
        return None;
    }
    Some(tokens)
}

/// A recursive-descent reader of an `#if` expression over what is known of
/// the macros: `Some(Some(v))` its value, `Some(None)` undecidable, `None`
/// malformed (or holding something not handled).
struct CExpr<'a> {
    tokens: &'a [String],
    at: usize,
    pre: &'a Preprocessor,
    depth: usize,
}

impl CExpr<'_> {
    fn peek(&self) -> Option<&str> {
        self.tokens.get(self.at).map(String::as_str)
    }

    fn eat(&mut self, token: &str) -> bool {
        let found = self.peek() == Some(token);
        if found {
            self.at += 1;
        }
        found
    }

    fn or(&mut self) -> Option<Option<i64>> {
        let mut value = self.and()?;
        while self.eat("||") {
            let right = self.and()?;
            value = match (value, right) {
                (Some(a), _) if a != 0 => Some(1),
                (_, Some(b)) if b != 0 => Some(1),
                (Some(_), Some(_)) => Some(0),
                _ => None,
            };
        }
        Some(value)
    }

    fn and(&mut self) -> Option<Option<i64>> {
        let mut value = self.compare()?;
        while self.eat("&&") {
            let right = self.compare()?;
            value = match (value, right) {
                (Some(0), _) | (_, Some(0)) => Some(0),
                (Some(_), Some(_)) => Some(1),
                _ => None,
            };
        }
        Some(value)
    }

    fn compare(&mut self) -> Option<Option<i64>> {
        let mut value = self.sum()?;
        while let Some(op) = self
            .peek()
            .filter(|t| ["==", "!=", "<", ">", "<=", ">="].contains(t))
            .map(str::to_string)
        {
            self.at += 1;
            let right = self.sum()?;
            value = match (value, right) {
                (Some(a), Some(b)) => Some(i64::from(match op.as_str() {
                    "==" => a == b,
                    "!=" => a != b,
                    "<" => a < b,
                    ">" => a > b,
                    "<=" => a <= b,
                    _ => a >= b,
                })),
                _ => None,
            };
        }
        Some(value)
    }

    fn sum(&mut self) -> Option<Option<i64>> {
        let mut value = self.product()?;
        while let Some(op) = self
            .peek()
            .filter(|t| *t == "+" || *t == "-")
            .map(str::to_string)
        {
            self.at += 1;
            let right = self.product()?;
            value = match (value, right) {
                (Some(a), Some(b)) if op == "+" => a.checked_add(b),
                (Some(a), Some(b)) => a.checked_sub(b),
                _ => None,
            };
        }
        Some(value)
    }

    fn product(&mut self) -> Option<Option<i64>> {
        let mut value = self.unary()?;
        while self.eat("*") {
            let right = self.unary()?;
            value = match (value, right) {
                (Some(a), Some(b)) => a.checked_mul(b),
                _ => None,
            };
        }
        Some(value)
    }

    fn unary(&mut self) -> Option<Option<i64>> {
        self.depth += 1;
        if self.depth > MAX_CFG_DEPTH {
            return None;
        }
        let value = match self.peek()? {
            "!" => {
                self.at += 1;
                self.unary()?.map(|v| i64::from(v == 0))
            }
            "-" => {
                self.at += 1;
                self.unary()?.and_then(i64::checked_neg)
            }
            "+" => {
                self.at += 1;
                self.unary()?
            }
            "(" => {
                self.at += 1;
                let value = self.or()?;
                if !self.eat(")") {
                    return None;
                }
                value
            }
            "defined" => {
                self.at += 1;
                let parens = self.eat("(");
                let name = self.peek()?.to_string();
                self.at += 1;
                if parens && !self.eat(")") {
                    return None;
                }
                self.pre.defined(&name).map(i64::from)
            }
            token if token.starts_with(|c: char| c.is_ascii_digit()) => {
                let value = crate::buildfacts::number(token);
                self.at += 1;
                Some(value?)
            }
            token => {
                let name = token.to_string();
                self.at += 1;
                // A function-like macro's call is not followed.
                if self.peek() == Some("(") {
                    return None;
                }
                match (name.as_str(), self.pre.macros.get(&name)) {
                    ("true", None) => Some(1),
                    ("false", None) => Some(0),
                    (_, Some(Macro::Defined(value))) => *value,
                    // An undefined name is 0 in an `#if`.
                    (_, Some(Macro::Undefined)) => Some(0),
                    (_, None) => None,
                }
            }
        };
        self.depth -= 1;
        Some(value)
    }
}

/// Most lines one Erlang attribute is read over to its full stop (a
/// `-define` with a long body, an `-if` with a long condition); past it,
/// the attribute is taken as it stands.
const MAX_ATTRIBUTE_LINES: usize = 64;

/// The lines of the Erlang file `source` its preprocessor drops: the groups
/// of `-ifdef`, `-ifndef`, `-if`/`-elif` and `-else`, over what the file's
/// own `-define`s and `-undef`s say. Every other macro is unknown — `TEST`
/// and `EUNIT` come from the build (`{d, 'TEST'}`, `rebar3 eunit`),
/// `OTP_RELEASE` from the compiler — and nothing that hangs on one is
/// dimmed. As in C, a chain's own attributes stay lit unless the whole
/// chain is in a dropped group (see [`Preprocessor::drops_directive`]).
fn erlang_inactive(source: &str) -> HashSet<usize> {
    let mut pre = Preprocessor::new(HashMap::new(), Dialect::C);
    let mut out = HashSet::new();
    let lines: Vec<&str> = source.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let first = i;
        let Some((word, mut text)) = erlang_directive(lines[i]) else {
            if pre.live() == Live::No {
                out.insert(i);
            }
            i += 1;
            continue;
        };
        // An attribute runs to its full stop, which may be lines on. A line
        // that begins another attribute ends it all the same: a full stop
        // missed must not swallow the `-endif` after it.
        while !text.ends_with('.')
            && i + 1 < lines.len()
            && i + 1 - first < MAX_ATTRIBUTE_LINES
            && erlang_directive(lines[i + 1]).is_none()
        {
            i += 1;
            text.push(' ');
            text.push_str(erlang_code(lines[i]).trim());
        }
        if pre.drops_directive(matches!(word, "elif" | "else" | "endif")) {
            out.extend(first..=i);
        }
        let next = match word {
            "ifndef" => lines[i + 1..].iter().find_map(|l| erlang_directive(l)),
            _ => None,
        };
        pre.erlang(word, &text, next);
        i += 1;
    }
    out
}

/// The preprocessor attribute `line` begins, if it begins one: its name,
/// and the line's code.
fn erlang_directive(line: &str) -> Option<(&'static str, String)> {
    let code = erlang_code(line).trim();
    let rest = code.strip_prefix('-')?.trim_start();
    let end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(rest.len());
    let word = match &rest[..end] {
        "ifdef" => "ifdef",
        "ifndef" => "ifndef",
        "if" => "if",
        "elif" => "elif",
        "else" => "else",
        "endif" => "endif",
        "define" => "define",
        "undef" => "undef",
        "include" => "include",
        "include_lib" => "include_lib",
        _ => return None,
    };
    // An attribute's parentheses or full stop follow its name.
    let after = rest[end..].trim_start();
    (after.is_empty() || after.starts_with(['(', '.'])).then(|| (word, code.to_string()))
}

/// `line` up to its comment. The first `%` ends it, even one in a string or
/// a `$%`: that cuts a `-define`'s body short, which costs nothing — only
/// the macro's name is read, and the lines the attribute then runs on to
/// are judged as they would be on their own. A conditional has neither.
fn erlang_code(line: &str) -> &str {
    line.split('%').next().unwrap_or(line)
}

/// What an attribute's parentheses hold: `TEST` of `-ifdef(TEST).`.
fn erlang_args(text: &str) -> Option<&str> {
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    (close > open).then(|| text[open + 1..close].trim())
}

/// The macro an attribute names, from what its parentheses hold: `M` of
/// `M`, `M, Body` and `M(Args), Body`. A quoted atom is its name unquoted
/// (`'M'` is the macro `M`); one with an escape in it is not read.
fn erlang_macro_name(args: &str) -> Option<String> {
    let args = args.trim_start();
    let (name, rest) = match args.strip_prefix('\'') {
        Some(quoted) => {
            let end = quoted.find('\'')?;
            let name = &quoted[..end];
            if name.is_empty() || name.contains('\\') {
                return None;
            }
            (name, &quoted[end + 1..])
        }
        None => {
            let end = args
                .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '@'))
                .unwrap_or(args.len());
            let name = &args[..end];
            if !name.starts_with(|c: char| c.is_alphabetic() || c == '_') {
                return None;
            }
            (name, &args[end..])
        }
    };
    let rest = rest.trim_start();
    (rest.is_empty() || rest.starts_with([',', '('])).then(|| name.to_string())
}

impl Preprocessor {
    /// Apply one Erlang preprocessor attribute: `word` is its name, `text`
    /// the whole attribute without its comments, and `next` the attribute
    /// after it (for an `-ifndef`'s include guard).
    fn erlang(&mut self, word: &str, text: &str, next: Option<(&str, String)>) {
        let args = erlang_args(text);
        let name = args.and_then(erlang_macro_name);
        let defined = name.as_deref().and_then(|n| self.defined(n));
        match word {
            "ifdef" => self.open(defined),
            "ifndef" => {
                // An include guard (`-ifndef(X).` then `-define(X, …).`) is
                // open the first time the header is read — as it is read
                // here.
                let guard = name.is_some()
                    && next.is_some_and(|(next, text)| {
                        next == "define" && erlang_args(&text).and_then(erlang_macro_name) == name
                    });
                let value = match guard && defined.is_none() {
                    true => Some(true),
                    false => defined.map(|b| !b),
                };
                self.open(value);
            }
            "if" => self.open(args.and_then(|a| self.erlang_condition(a))),
            "elif" => self.other(args.and_then(|a| self.erlang_condition(a))),
            "else" => self.other(Some(true)),
            "endif" => {
                self.groups.pop();
            }
            "define" | "undef" => match name {
                Some(name) => {
                    let value = match word {
                        "define" => Macro::Defined(None),
                        _ => Macro::Undefined,
                    };
                    self.set(&name, value);
                }
                // A macro whose name is not read could be any of them.
                None => self.forget(),
            },
            // A header may define or undefine any macro: what the file said
            // of them before it is no longer known.
            "include" | "include_lib" => self.forget(),
            _ => {}
        }
    }

    /// Know nothing more of any macro, unless here is dropped (and so not
    /// read at all).
    fn forget(&mut self) {
        if self.live() != Live::No {
            self.macros.clear();
        }
    }

    /// An Erlang `-if`/`-elif` condition's truth, when it can be known.
    fn erlang_condition(&self, expr: &str) -> Option<bool> {
        let tokens = erlang_tokens(expr)?;
        let mut parser = ErlExpr {
            tokens: &tokens,
            at: 0,
            pre: self,
            depth: 0,
        };
        let value = parser.orelse()?;
        (parser.at == tokens.len()).then_some(value)?
    }
}

/// The tokens of an Erlang `-if` condition this reader follows: names and
/// parentheses. Anything else is `None`: a macro's value (`?OTP_RELEASE >=
/// 21`) is whatever Erlang terms its body holds, compared in the term order,
/// and only whether a macro is defined is followed.
fn erlang_tokens(expr: &str) -> Option<Vec<&str>> {
    let mut tokens = Vec::new();
    let mut rest = expr.trim_start();
    while let Some(c) = rest.chars().next() {
        let len = match c {
            '(' | ')' => 1,
            c if c.is_ascii_alphabetic() || c == '_' => rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '@'))
                .unwrap_or(rest.len()),
            _ => return None,
        };
        tokens.push(&rest[..len]);
        rest = rest[len..].trim_start();
    }
    Some(tokens)
}

/// A recursive-descent reader of an Erlang `-if` condition over what is
/// known of the macros — `defined(M)`, `true` and `false`, joined by `not`,
/// `and`, `or`, `andalso`, `orelse` and parentheses — three-valued like
/// [`GoExpr`]: `Some(Some(b))` decided, `Some(None)` undecidable, `None`
/// not followed.
struct ErlExpr<'a> {
    tokens: &'a [&'a str],
    at: usize,
    pre: &'a Preprocessor,
    depth: usize,
}

/// One of [`ErlExpr`]'s levels of precedence.
type ErlLevel<'a> = fn(&mut ErlExpr<'a>) -> Option<Option<bool>>;

impl<'a> ErlExpr<'a> {
    fn peek(&self) -> Option<&'a str> {
        self.tokens.get(self.at).copied()
    }

    fn eat(&mut self, token: &str) -> bool {
        let found = self.peek() == Some(token);
        if found {
            self.at += 1;
        }
        found
    }

    /// Operands read by `operand`, joined by the operator `op` as `join`
    /// joins two values.
    fn chain(
        &mut self,
        op: &str,
        operand: ErlLevel<'a>,
        join: fn(Option<bool>, Option<bool>) -> Option<bool>,
    ) -> Option<Option<bool>> {
        let mut value = operand(self)?;
        while self.eat(op) {
            value = join(value, operand(self)?);
        }
        Some(value)
    }

    // Erlang's precedence, loosest first: `orelse`, `andalso`, then `or`
    // (with `+`), `and` (with `*`) and `not`.
    fn orelse(&mut self) -> Option<Option<bool>> {
        self.chain("orelse", Self::andalso, or)
    }

    fn andalso(&mut self) -> Option<Option<bool>> {
        self.chain("andalso", Self::or, and)
    }

    fn or(&mut self) -> Option<Option<bool>> {
        self.chain("or", Self::and, or)
    }

    fn and(&mut self) -> Option<Option<bool>> {
        self.chain("and", Self::not, and)
    }

    fn not(&mut self) -> Option<Option<bool>> {
        self.depth += 1;
        if self.depth > MAX_CFG_DEPTH {
            return None;
        }
        let value = match self.peek()? {
            "not" => {
                self.at += 1;
                self.not()?.map(|b| !b)
            }
            "(" => {
                self.at += 1;
                let value = self.orelse()?;
                if !self.eat(")") {
                    return None;
                }
                value
            }
            "true" => {
                self.at += 1;
                Some(true)
            }
            "false" => {
                self.at += 1;
                Some(false)
            }
            "defined" => {
                self.at += 1;
                if !self.eat("(") {
                    return None;
                }
                let name = self.peek().filter(|t| !matches!(*t, "(" | ")"))?;
                self.at += 1;
                if !self.eat(")") {
                    return None;
                }
                self.pre.defined(name)
            }
            _ => return None,
        };
        self.depth -= 1;
        Some(value)
    }
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
    fn target_string_literals_are_compared_by_value() {
        let h = host_macos();
        for literal in [
            r#""macos""#,
            r#"r"macos""#,
            r##"r#"macos"#"##,
            r###"r##"macos"##"###,
            r#""mac\x6fs""#,
            r#""mac\u{6f}s""#,
            r#""mac\u{0000_6f}s""#,
            "\"mac\\\n    os\"",
            "\"mac\\\r\n    os\"",
        ] {
            let predicate = format!("target_os = {literal}");
            assert_eq!(eval_cfg(&predicate, &h), Some(true), "{predicate}");
            let source = format!("#[cfg({predicate})]\nfn active() {{}}\n");
            assert!(inactive_lines(&source, "rust", &h).is_empty(), "{source}");
        }
        assert_eq!(eval_cfg(r#"target_arch = "aa\x72ch64""#, &h), Some(true));
        assert_eq!(eval_cfg(r##"target_os = r#"windows"#"##, &h), Some(false));
        for literal in [
            r#""mac\qos""#,
            r#""mac\xFFs""#,
            "macos",
            "r#\"macos\"",
            r#""mac\u{D800}s""#,
        ] {
            assert_eq!(eval_cfg(&format!("target_os = {literal}"), &h), None);
        }
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

    /// A match arm holds its attribute as its own child, so the attribute's
    /// next sibling is only the pattern: the arm's body was left undimmed.
    #[test]
    fn dims_a_whole_gated_match_arm_and_field() {
        let src = "\
fn f(os: Os) -> S {
    match os {
        #[cfg(target_os = \"windows\")]
        Os::Win => {
            win_only();
        }
        _ => {}
    }
    S {
        #[cfg(target_os = \"windows\")]
        handle:
            win_handle(),
        other: 1,
    }
}
";
        let lines = inactive_lines(src, "rust", &host_macos());
        for l in [2, 3, 4, 5, 9, 10, 11] {
            assert!(lines.contains(&l), "line {l} not dimmed: {lines:?}");
        }
        for l in [6, 12] {
            assert!(!lines.contains(&l), "line {l} dimmed: {lines:?}");
        }
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

    /// Nested gated items: every line dimmed once, and at once — not each
    /// level re-dimming the lines below it, nor a sibling lookup per level
    /// costing the depth (tree-sitter finds a sibling through the parent).
    #[test]
    fn nested_gated_modules_are_dimmed_in_one_pass() {
        const DEPTH: usize = 20_000;
        let src = format!(
            "{}{}",
            "#[cfg(target_os = \"nonesuch\")] mod a {\n".repeat(DEPTH),
            "}\n".repeat(DEPTH)
        );
        let started = std::time::Instant::now();
        let lines = inactive_lines(&src, "rust", &Target::host());
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        assert_eq!(lines.len(), 2 * DEPTH);
    }

    /// A run of gated attributes: each finds the item they gate without
    /// searching the rest of the run (which cost the run's length each).
    #[test]
    fn a_run_of_gated_attributes_is_dimmed_in_one_pass() {
        const RUN: usize = 32_000;
        let src = format!(
            "{}fn gated() {{}}\nfn shown() {{}}\n",
            "#[cfg(target_os = \"nonesuch\")]\n".repeat(RUN)
        );
        let started = std::time::Instant::now();
        let lines = inactive_lines(&src, "rust", &Target::host());
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        assert_eq!(lines.len(), RUN + 1);
        assert!(lines.contains(&RUN) && !lines.contains(&(RUN + 1)));
    }

    fn linux() -> Target {
        Target::new("Linux", "linux", "x86_64", "unix")
    }

    fn lines(set: HashSet<usize>) -> Vec<usize> {
        let mut v: Vec<usize> = set.into_iter().collect();
        v.sort();
        v
    }

    fn crate_features(dir: &str, declared: &[&str], on: &[&str], maybe: &[&str]) -> CrateFeatures {
        let v = |l: &[&str]| l.iter().map(|s| s.to_string()).collect();
        CrateFeatures {
            dir: dir.into(),
            known: true,
            declared: v(declared),
            on: v(on),
            maybe: v(maybe),
        }
    }

    /// A Rust `cfg(feature)` is off when its package builds without the
    /// feature — and left alone when another package may turn it on, when
    /// the package does not declare it, or when the file's crate is not one
    /// whose build is known.
    #[test]
    fn rust_features_dim_by_the_packages_build() {
        let source = "\
#[cfg(feature = \"fast\")]
fn fast() {}
#[cfg(feature = \"tls\")]
fn tls() {}
#[cfg(feature = \"maybe\")]
fn maybe() {}
#[cfg(feature = \"nope\")]
fn nope() {}
#[cfg(not(feature = \"fast\"))]
fn slow() {}
";
        let facts = BuildFacts {
            crates: vec![
                crate_features(
                    "",
                    &["fast", "tls", "maybe", "default"],
                    &["default", "tls"],
                    &["maybe"],
                ),
                // Not known, whatever it lists: nothing is decided by it.
                CrateFeatures {
                    known: false,
                    ..crate_features("fuzz", &["fast"], &[], &[])
                },
            ],
            ..Default::default()
        };
        let dimmed = |rel| lines(inactive_lines_in(source, "rust", &linux(), &facts, rel));
        assert_eq!(dimmed(Some("src/lib.rs")), [0, 1]);
        // Another crate, whose build is not known: nothing.
        assert!(dimmed(Some("fuzz/src/lib.rs")).is_empty());
        // No file, or no facts: features undecided, as before.
        assert!(dimmed(None).is_empty());
        assert!(inactive_lines(source, "rust", &linux()).is_empty());
    }

    /// The target rust-analyzer is told to check for decides Rust's
    /// platform `cfg`s, over the reading target.
    #[test]
    fn a_chosen_rust_target_decides_its_cfgs() {
        let source = "#[cfg(target_os = \"linux\")]\nfn l() {}\n#[cfg(target_family = \"wasm\")]\nfn w() {}\n";
        let facts = BuildFacts {
            rust_target: Some("wasm32-unknown-unknown".into()),
            ..Default::default()
        };
        assert_eq!(
            lines(inactive_lines_in(source, "rust", &linux(), &facts, None)),
            [0, 1]
        );
        let t = Target::from_triple("aarch64-apple-darwin");
        assert_eq!(
            (t.os.as_str(), t.arch.as_str(), t.family.as_str()),
            ("macos", "aarch64", "unix")
        );
        let t = Target::from_triple("x86_64-pc-windows-msvc");
        assert_eq!((t.os.as_str(), t.family.as_str()), ("windows", "windows"));
        let t = Target::from_triple("aarch64-linux-android");
        assert_eq!(t.os, "android");
        let t = Target::from_triple("thumbv7em-none-eabihf");
        assert_eq!(
            (t.os.as_str(), t.arch.as_str(), t.family.as_str()),
            ("none", "arm", "")
        );
    }

    /// A Go file is left out whole when its name or its build constraint
    /// excludes it — for the platform gopls loads, and the tags set — and
    /// kept when what excludes it cannot be known.
    #[test]
    fn go_files_dim_whole_by_their_name_and_constraint() {
        let body = "package p\n\nfunc F() {}\n";
        let tagged = |c: &str| format!("// Copyright\n\n{c}\n\n{body}");
        let tags = BuildFacts {
            go_tags: Some(vec!["e2e".into()]),
            ..Default::default()
        };
        let none = BuildFacts::default();
        let dimmed = |source: &str, facts: &BuildFacts, rel: &str| {
            !inactive_lines_in(source, "go", &linux(), facts, Some(rel)).is_empty()
        };
        // By name.
        assert!(dimmed(body, &none, "x/conn_windows.go"));
        assert!(dimmed(body, &none, "x/conn_darwin_arm64_test.go"));
        assert!(!dimmed(body, &none, "x/conn_linux_amd64.go"));
        assert!(
            !dimmed(body, &none, "x/windows.go"),
            "no `_`: no constraint"
        );
        // Android is Linux too, as the go command has it — not the reverse.
        assert!(dimmed(body, &none, "x/conn_android.go"));
        let android = BuildFacts {
            goos: Some("android".into()),
            ..Default::default()
        };
        assert!(!dimmed(body, &android, "x/conn_linux.go"));
        // By constraint, its tags known…
        assert!(dimmed(&tagged("//go:build integration"), &tags, "a.go"));
        assert!(!dimmed(
            &tagged("//go:build e2e && !windows"),
            &tags,
            "a.go"
        ));
        assert!(dimmed(
            &tagged("//go:build darwin || windows"),
            &tags,
            "a.go"
        ));
        assert!(!dimmed(&tagged("//go:build unix"), &tags, "a.go"));
        assert!(dimmed(
            &tagged("// +build windows,amd64 darwin\n// +build e2e"),
            &tags,
            "a.go"
        ));
        assert!(!dimmed(&tagged("// +build linux,e2e"), &tags, "a.go"));
        // …or not: undecided, kept.
        assert!(!dimmed(&tagged("//go:build integration"), &none, "a.go"));
        assert!(!dimmed(&tagged("//go:build linux && cgo"), &tags, "a.go"));
        assert!(!dimmed(&tagged("//go:build go1.21"), &tags, "a.go"));
        // A false term decides it even beside one that is not known.
        assert!(dimmed(&tagged("//go:build windows && cgo"), &tags, "a.go"));
        // After the package clause it is no constraint.
        assert!(!dimmed(
            &format!("{body}//go:build windows\n"),
            &tags,
            "a.go"
        ));
        // gopls's platform, chosen, over the reading target.
        let windows = BuildFacts {
            goos: Some("windows".into()),
            ..Default::default()
        };
        assert!(!dimmed(body, &windows, "x/conn_windows.go"));
        assert!(dimmed(body, &windows, "x/conn_linux.go"));
        // A malformed constraint decides nothing.
        assert!(!dimmed(&tagged("//go:build windows &&"), &tags, "a.go"));
    }

    /// C's preprocessor groups are dropped for the platform's macros, the
    /// flags' `-D`/`-U` and the file's own `#define`s — a whole `#if`
    /// chain, an include guard, comments and continued lines read as the
    /// preprocessor reads them — and never for a macro a header may define.
    #[test]
    fn c_preprocessor_groups_dim_by_what_is_known() {
        let source = "\
#ifndef APP_H
#define APP_H
#define USE_X 1
#if defined(_WIN32)
win();
#elif defined(__linux__) && \\
      USE_X
linux_x();
#else
other();
#endif
#ifdef SOMETHING_A_HEADER_DEFINES
maybe();
#else
maybe_too();
#endif
#if 0 /* off */
off();
#if 1
nested_off();
#endif
#endif
/*
#if 0
*/
kept();
#ifndef DEBUG
release();
#endif
#if DEBUG >= 2
verbose();
#endif
#endif
";
        let facts = BuildFacts {
            c_defines: Some(vec![clew_protocol::CMacro {
                name: "DEBUG".into(),
                value: Some(2),
            }]),
            ..Default::default()
        };
        let dimmed = lines(inactive_lines_in(
            source,
            "c",
            &linux(),
            &facts,
            Some("app.h"),
        ));
        assert_eq!(
            dimmed,
            [
                4,  // win();
                9,  // other();
                17, // off();
                18, 19, 20, // the nested #if 1 … #endif, inside the dropped group
                27, // release(): DEBUG is defined
            ]
        );
        // Flags not known (a compilation database decides them): DEBUG is
        // undecided, and so is all that hangs on it.
        let unknown = lines(inactive_lines_in(
            source,
            "c",
            &linux(),
            &BuildFacts::default(),
            Some("app.h"),
        ));
        assert_eq!(unknown, [4, 9, 17, 18, 19, 20]);
        // On Windows the other branch.
        let windows = Target::new("Windows", "windows", "x86_64", "windows");
        let on_windows = lines(inactive_lines_in(
            source,
            "c",
            &windows,
            &facts,
            Some("app.h"),
        ));
        assert_eq!(&on_windows[..3], [7, 9, 17]);
    }

    /// An `#if` the reader cannot follow — a function-like macro, a cast,
    /// a character constant — decides nothing.
    #[test]
    fn a_c_condition_not_followed_decides_nothing() {
        for condition in ["FOO(1)", "(int)1 == 0", "'a' == 97", "1 ? 0 : 1"] {
            let source = format!("#if {condition}\nx();\n#endif\n");
            assert!(
                inactive_lines_in(&source, "c", &linux(), &BuildFacts::default(), None).is_empty(),
                "{condition}"
            );
        }
        // C++ alone defines `__cplusplus`.
        let source = "#ifdef __cplusplus\ncpp();\n#else\nc();\n#endif\n";
        let facts = BuildFacts::default();
        assert_eq!(
            lines(inactive_lines_in(
                source,
                "cpp",
                &linux(),
                &facts,
                Some("a.cpp")
            )),
            [3]
        );
        assert_eq!(
            lines(inactive_lines_in(
                source,
                "c",
                &linux(),
                &facts,
                Some("a.c")
            )),
            [1]
        );
        assert!(inactive_lines_in(source, "c", &linux(), &facts, Some("a.h")).is_empty());
    }

    /// Zig's comptime branches on the target's OS and architecture — an
    /// `if`, an `else if` chain, a `switch` with an `else` — dimmed for the
    /// ones not taken, in whole lines only, through the names the file binds
    /// them to; and a condition on anything else decides nothing.
    #[test]
    fn zig_comptime_branches_dim_by_the_target() {
        let source = "\
const builtin = @import(\"builtin\");
const native_os = builtin.os.tag;
pub fn f() void {
    if (builtin.os.tag == .windows) {
        windows();
    } else if (native_os == .macos) {
        mac();
    } else {
        other();
    }
    switch (builtin.cpu.arch) {
        .x86_64 => {
            x64();
        },
        .aarch64, .arm => {
            arm();
        },
        else => {
            rest();
        },
    }
    if (std.debug.runtime_safety) {
        safe();
    }
    const x = if (native_os != .linux) 1 else 2;
    if (builtin.os.tag != .windows) {
        posix();
    }
}
";
        let linux = Target::new("Linux", "linux", "x86_64", "unix");
        assert_eq!(
            lines(inactive_lines_in(
                source,
                "zig",
                &linux,
                &BuildFacts::default(),
                None
            )),
            // The `} else if` line holds a condition that is evaluated: lit.
            [4, 6, 14, 15, 16, 17, 18, 19],
        );
        let windows = Target::new("Windows", "windows", "x86_64", "windows");
        let on_windows = lines(inactive_lines_in(
            source,
            "zig",
            &windows,
            &BuildFacts::default(),
            None,
        ));
        assert_eq!(on_windows, [6, 7, 8, 9, 14, 15, 16, 17, 18, 19, 26, 27]);
        let mac = Target::new("macOS", "macos", "aarch64", "unix");
        assert_eq!(
            lines(inactive_lines_in(
                source,
                "zig",
                &mac,
                &BuildFacts::default(),
                None
            )),
            [4, 8, 9, 11, 12, 13, 17, 18, 19],
        );
    }

    /// C#'s `#if` groups are dropped over the symbols the file `#define`s
    /// and `#undef`s — `true`, `false`, `!`, `&&`, `||`, `==` and
    /// parentheses read as C# reads them — and never for a symbol the build
    /// may define (`DEBUG` unless the file undefines it, `TRACE`, `NET8_0`).
    /// A `#region` is no group, a condition C# does not have decides
    /// nothing, and a line ending in a backslash does not go on to the next.
    #[test]
    fn csharp_if_groups_dim_by_the_files_own_symbols() {
        let source = "\
#define TRACE_ON
#undef DEBUG
using System;
class A {
#if DEBUG
    void Dbg() {}
#elif TRACE_ON && !(DEBUG == true)
    void Traced() {}
#else
    void Other() {}
#endif
#if false
#region Off
    void Off() {}
#endregion
    void OffToo() {}
#endif
#if NET8_0 || TRACE
    void Maybe() {}
#else
    void MaybeToo() {}
#endif
#if TRACE_ON || NET8_0
    void Sure() {}
#else
    void Never() {}
#endif
#if !DEBUG_LOGGING
    void Unknown() {}
#endif
#if 0
    void NotCSharp() {}
#endif
#if false
    // C:\\temp\\
#endif
    void After() {}
}
";
        let dimmed = |source: &str| {
            lines(inactive_lines_in(
                source,
                "csharp",
                &linux(),
                &BuildFacts::default(),
                None,
            ))
        };
        assert_eq!(
            dimmed(source),
            [
                5,  // Dbg: DEBUG is undefined here
                9,  // Other: the #elif was taken
                12, // #region, inside a dropped group
                13, 14, // Off, #endregion
                15, // OffToo: #endregion closed no group
                25, // Never: true || unknown is true
                34, // the comment, and no more: #endif is read
            ]
        );
        // Without the file's own `#undef`, DEBUG is the build's.
        let built = source.replacen("#undef DEBUG", "", 1);
        assert_eq!(dimmed(&built), [12, 13, 14, 15, 25, 34]);
        // C#'s preprocessor has no `#ifndef`: it decides nothing, and its
        // `#endif` is its own, not that of the group around it.
        let c_only = "\
#define TRACE_ON
#ifndef TRACE_ON
    void NotCSharp() {}
#endif
#if false
#ifndef TRACE_ON
    void Off() {}
#endif
    void StillOff() {}
#endif
";
        assert_eq!(dimmed(c_only), [5, 6, 7, 8]);
    }

    /// A Haskell file with the CPP pragma has its `#if`s read as C's, over
    /// GHC's platform macros for the target and the file's own `#define`s;
    /// `MIN_VERSION_…` and `__GLASGOW_HASKELL__` decide nothing, a target
    /// whose GHC macros are not known decides no OS, nor do C's own platform
    /// macros, and a file without the pragma has no preprocessor.
    #[test]
    fn haskell_cpp_groups_dim_by_ghcs_platform_macros() {
        let body = "\
module Platform where

#if defined(mingw32_HOST_OS)
sep = \"win\"
#elif defined(darwin_HOST_OS) || defined(linux_HOST_OS)
sep = \"posix\"
#else
sep = \"other\"
#endif

#ifdef aarch64_HOST_ARCH
arch = \"arm\"
#else
arch = \"x86\"
#endif

#if MIN_VERSION_base(4,9,0)
modern = 1
#else
legacy = 1
#endif

#if __GLASGOW_HASKELL__ >= 900
ghc9 = 1
#endif

#define USE_FAST 1
#if USE_FAST
fast = 1
#else
slow = 1
#endif
";
        let source = format!("{{-# LANGUAGE CPP, OverloadedStrings #-}}\n{body}");
        let dimmed = |source: &str, target: &Target| {
            lines(inactive_lines_in(
                source,
                "haskell",
                target,
                &BuildFacts::default(),
                None,
            ))
        };
        assert_eq!(dimmed(&source, &linux()), [4, 8, 12, 31]);
        let windows = Target::new("Windows", "windows", "x86_64", "windows");
        assert_eq!(dimmed(&source, &windows), [6, 8, 12, 31]);
        let mac = Target::new("macOS", "macos", "aarch64", "unix");
        assert_eq!(dimmed(&source, &mac), [4, 8, 14, 31]);
        let bsd = Target::new("FreeBSD", "freebsd", "x86_64", "unix");
        assert_eq!(dimmed(&source, &bsd), [12, 31]);
        // The pragma spelled otherwise, or as a flag.
        for pragma in ["{-# language CPP #-}", "{-# OPTIONS_GHC -Wall -XCPP #-}"] {
            let source = format!("{pragma}\n{body}");
            assert_eq!(dimmed(&source, &linux()), [4, 8, 12, 31], "{pragma}");
        }
        // No pragma, no preprocessor.
        let plain = format!("{{-# LANGUAGE OverloadedStrings #-}}\n{body}");
        assert!(dimmed(&plain, &linux()).is_empty());
        // C's platform macros are not among what is known.
        let c_macro = "{-# LANGUAGE CPP #-}\n#ifdef __linux__\nx = 1\n#else\ny = 1\n#endif\n";
        assert!(dimmed(c_macro, &linux()).is_empty());
    }

    /// Erlang's `-ifdef`/`-ifndef`/`-if`/`-elif`/`-else` groups are dropped
    /// over the macros the file `-define`s (with arguments or without) and
    /// `-undef`s; `TEST` and `OTP_RELEASE` decide nothing, nor does a macro
    /// defined only in a dropped group. A chain's own attributes stay lit.
    #[test]
    fn erlang_groups_dim_by_the_files_own_macros() {
        let source = "\
-module(m).
-define(USE_ETS, true).
-define(LOG(X), io:format(\"~p~n\", [X])).
-undef(LEGACY).

-ifdef(USE_ETS).
store() -> ets.
-else.
store() -> dict.
-endif.

-ifndef(LOG).
log(_) -> ok.
-endif.

-ifdef(TEST).
test_only() -> ok.
-else.
prod_only() -> ok.
-endif.

-if(defined(USE_ETS) andalso not defined(LEGACY)).
fast() -> ok.
-elif(?OTP_RELEASE >= 21).
mid() -> ok.
-else.
slow() -> ok.
-endif.

-ifdef(LEGACY).
-define(OLD, 1).
-endif.
-ifdef(OLD).
old() -> ok.
-endif.

-if(?OTP_RELEASE >= 21).
otp21() -> ok.
-endif.

-if(defined(TEST) orelse defined(USE_ETS)). % either way
either() -> ok.
-else. % never
neither() -> ok.
-endif.
";
        assert_eq!(
            lines(inactive_lines_in(
                source,
                "erlang",
                &linux(),
                &BuildFacts::default(),
                None,
            )),
            [
                8,  // store() -> dict: USE_ETS is defined
                12, // log/1: LOG is defined, with arguments
                24, // mid(): the -if before it was taken
                26, // slow()
                30, // -define(OLD, 1), in a dropped group
                43, // neither(): unknown orelse true is true
            ]
        );
    }

    /// What may have changed a macro leaves it unknown: a `-define` or
    /// `-undef` in a group that may not be read, and a header the file
    /// includes. A condition goes on over lines to its full stop, and a
    /// header's include guard is open the first time it is read.
    #[test]
    fn erlang_macros_the_build_or_a_header_may_change_decide_nothing() {
        let source = "\
-define(A, 1).
-ifdef(TEST).
-define(B, 1).
-undef(A).
-endif.
-ifndef(A).
a() -> ok.
-endif.
-ifndef(B).
b() -> ok.
-endif.
-define(C, 1).
-include(\"c.hrl\").
-ifndef(C).
c() -> ok.
-endif.
-undef(D).
-if(not defined(D)
    andalso true).
d() -> ok.
-else.
nd() -> ok.
-endif.
";
        let dimmed = |source: &str| {
            lines(inactive_lines_in(
                source,
                "erlang",
                &linux(),
                &BuildFacts::default(),
                None,
            ))
        };
        assert_eq!(dimmed(source), [21]);
        let header = "\
-ifndef(SHOP_HRL).
-define(SHOP_HRL, true).
-define(USE_X, 1).
-ifdef(USE_X).
x() -> ok.
-else.
y() -> ok.
-endif.
-endif.
";
        assert_eq!(dimmed(header), [6]);
    }

    /// The languages said to read the target are the ones whose dimming
    /// changes with it; C#'s and Erlang's do not.
    #[test]
    fn the_languages_that_read_the_target_are_named() {
        let linux = Target::from_triple("x86_64-unknown-linux-gnu");
        let windows = Target::from_triple("x86_64-pc-windows-msvc");
        let differs = |source: &str, lang: &str| {
            inactive_lines(source, lang, &linux) != inactive_lines(source, lang, &windows)
        };
        for (lang, source) in [
            ("rust", "#[cfg(windows)]\nfn a() {}\n"),
            ("go", "//go:build windows\n\npackage a\n"),
            ("c", "#ifdef _WIN32\nint a;\n#endif\n"),
            ("cpp", "#ifdef _WIN32\nint a;\n#endif\n"),
            (
                "zig",
                "const builtin = @import(\"builtin\");\nfn a() void {\n    if (builtin.os.tag == .windows) {\n        b();\n    }\n}\n",
            ),
            (
                "haskell",
                "{-# LANGUAGE CPP #-}\nmodule A where\n#ifdef mingw32_HOST_OS\na = 1\n#endif\n",
            ),
        ] {
            assert!(reads_target(lang), "{lang}");
            assert!(differs(source, lang), "{lang}'s dimming reads the target");
        }
        for (lang, source) in [
            ("csharp", "#if WINDOWS\nclass A {}\n#endif\n"),
            ("erlang", "-ifdef(WINDOWS).\na() -> 1.\n-endif.\n"),
            ("python", "import sys\n"),
        ] {
            assert!(!reads_target(lang), "{lang}");
            assert!(!differs(source, lang), "{lang}");
        }
    }

    #[test]
    fn a_cfg_says_why_it_leaves_code_out() {
        let linux = Target::from_triple("x86_64-unknown-linux-gnu");
        let facts = BuildFacts::default();
        assert_eq!(cfg_predicate("  #[cfg(windows)] "), Some("windows"));
        assert_eq!(cfg_predicate("#[cfg_attr(windows, inline)]"), None);
        assert_eq!(cfg_predicate("fn main() {}"), None);
        assert_eq!(
            rust_cfg_reason("windows", &linux, &facts, None),
            Some(CfgReason::Target(linux.clone()))
        );
        assert_eq!(rust_cfg_reason("unix", &linux, &facts, None), None);
        // Undecided without the crate's features: no reason given.
        assert_eq!(
            rust_cfg_reason("feature = \"fast\"", &linux, &facts, None),
            None
        );
        // Off among the crate's known features: they are the reason.
        let facts = BuildFacts {
            crates: vec![CrateFeatures {
                dir: String::new(),
                known: true,
                declared: vec!["fast".into()],
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(
            rust_cfg_reason("feature = \"fast\"", &linux, &facts, Some("src/lib.rs")),
            Some(CfgReason::Features)
        );
    }
}
