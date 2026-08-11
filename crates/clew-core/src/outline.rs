//! Symbol outline extraction using tree-sitter tags queries.

use streaming_iterator::StreamingIterator;
use tree_sitter::{Parser, Query, QueryCursor};

// The outline entry is the protocol's wire type: `name`, `kind` ("function",
// "struct", "class", …), `line` (1-based first line), `end_line` (1-based last
// line, for span hashing). Shared so there is no conversion at the wire.
pub use clew_protocol::Symbol;

/// Ordinal of `target` among same-name functions/methods in `symbols`: how
/// many same-name callables appear before it (by line). This is the identity
/// component `explain::Node::Function` uses to keep a file's same-name methods
/// (different impls' `new`, `default`, …) apart — it must be computed the same
/// way everywhere, hence this one shared definition.
pub fn fn_ordinal(symbols: &[Symbol], target: &Symbol) -> u32 {
    symbols
        .iter()
        .filter(|s| {
            matches!(s.kind.as_str(), "function" | "method")
                && s.name == target.name
                && s.line < target.line
        })
        .count() as u32
}

/// `fn_ordinal` for every entry of `symbols`, in one pass: `out[i]` equals
/// `fn_ordinal(symbols, &symbols[i])` for EVERY `i`, with no exception carved
/// out for non-callables — a divergence there would be a silent drift between
/// the two definitions, and the ordinal is a cache identity (it keys
/// `explain::Node::Function`, chosen over the line number precisely so the
/// explanation cache survives edits). A wrong ordinal does not look wrong, it
/// just mis-keys or invalidates a cached explanation.
///
/// Exists because the per-frame outline view calls `fn_ordinal` inside a loop
/// over the same list, twice per callable: O(symbols²) on every repaint, which
/// on a file with thousands of callables stalls the UI thread for ~100 ms per
/// frame. This is O(n log n) and allocates once per distinct name.
///
/// NOT YET ADOPTED, so nothing here is claimed to be fixed: `ui::panes`
/// (`outline_content`) and `app::content` (`outline_scroll_task`) still call
/// `fn_ordinal` per symbol. Converting them is a hoist above each loop plus an
/// `enumerate`, and the equality asserted above is what makes it safe — the
/// ordinal keys a cache, so a substitution that shifted it would silently
/// orphan every stored explanation for the file.
///
/// Makes no assumption about `symbols` being ordered — `extract` sorts by line,
/// but this must not silently break for a list that arrives any other way, so
/// the per-name lines are sorted here rather than assumed ascending.
pub fn fn_ordinals(symbols: &[Symbol]) -> Vec<u32> {
    use std::collections::HashMap;
    let mut lines_by_name: HashMap<&str, Vec<usize>> = HashMap::new();
    for s in symbols {
        if matches!(s.kind.as_str(), "function" | "method") {
            lines_by_name
                .entry(s.name.as_str())
                .or_default()
                .push(s.line);
        }
    }
    for lines in lines_by_name.values_mut() {
        lines.sort_unstable();
    }
    symbols
        .iter()
        .map(|s| {
            // Strictly-less, so same-name callables sharing a line share an
            // ordinal — exactly what `fn_ordinal`'s `s.line < target.line` does.
            lines_by_name
                .get(s.name.as_str())
                .map_or(0, |lines| lines.partition_point(|l| *l < s.line) as u32)
        })
        .collect()
}

/// Extract definition symbols from `source`. Returns an empty list when the
/// language has no tags query or parsing fails. Blocking; run off the UI thread.
pub fn extract(source: &str, lang_key: &str) -> Vec<Symbol> {
    let Some((language, tags)) = crate::highlight::tags_for(lang_key) else {
        return Vec::new();
    };
    let mut parser = Parser::new();
    if parser.set_language(&language).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };
    let Ok(query) = Query::new(&language, tags) else {
        return Vec::new();
    };

    let mut cursor = QueryCursor::new();
    let mut symbols = Vec::new();
    let mut matches = cursor.matches(&query, tree.root_node(), source.as_bytes());
    while let Some(m) = matches.next() {
        let mut kind: Option<&str> = None;
        let mut name: Option<&str> = None;
        let mut line = 0usize;
        let mut end_line = 0usize;
        for capture in m.captures {
            let capture_name = query.capture_names()[capture.index as usize];
            if let Some(k) = capture_name.strip_prefix("definition.") {
                kind = Some(k);
                line = capture.node.start_position().row + 1;
                end_line = capture.node.end_position().row + 1;
            } else if capture_name == "name" {
                name = source.get(capture.node.byte_range()).or(name);
            }
        }
        if let (Some(kind), Some(name)) = (kind, name)
            && line > 0
        {
            symbols.push(Symbol {
                name: name.to_string(),
                kind: kind.to_string(),
                line,
                end_line: end_line.max(line),
            });
        }
    }
    symbols.sort_by(|a, b| a.line.cmp(&b.line).then_with(|| a.name.cmp(&b.name)));
    symbols.dedup_by(|a, b| a.line == b.line && a.name == b.name);
    symbols
}

/// Whether the function/method named `name` at 1-based `line1` in `lines` (the
/// file split into lines) is a test. Rust: a `#[…test…]` attribute above the
/// definition (`#[test]`, `#[tokio::test]`, `#[rstest]`, `#[test_case(…)]`, …).
/// Go/Python: the standard test-name convention. Pure text, no tree-sitter.
/// Lives in clew-core so the client's index and the server's project-symbol
/// snapshot classify identically.
pub fn is_test_fn(lines: &[&str], line1: usize, name: &str, lang: &str) -> bool {
    match lang {
        "rust" => {
            if line1 == 0 || line1 > lines.len() {
                return false;
            }
            // Scan upward over attributes, doc-comments and blank lines; a test
            // attribute anywhere in that run marks it. Stop at the first real line.
            let mut i = line1 - 1; // 0-based index of the definition line
            while i > 0 {
                i -= 1;
                let t = lines[i].trim();
                if t.is_empty() || t.starts_with("//") || t.starts_with("#!") {
                    continue;
                }
                if let Some(rest) = t.strip_prefix("#[") {
                    if attr_marks_test(rest) {
                        return true;
                    }
                    continue; // another attribute (e.g. #[cfg(...)]) — keep scanning
                }
                break; // a code line — the attribute run has ended
            }
            false
        }
        "go" => {
            name.starts_with("Test") || name.starts_with("Benchmark") || name.starts_with("Fuzz")
        }
        "python" => name.starts_with("test") || name.starts_with("Test"),
        _ => false,
    }
}

/// Whether one Rust attribute marks a test. `rest` is its text after `#[`.
///
/// Only the attribute PATH decides, plus the two attributes that carry a
/// condition. A `test` substring anywhere else belongs to somebody's feature
/// name or string literal — `#[cfg(feature = "contest")]`,
/// `#[serde(rename = "latest")]` — and matching those filed ordinary
/// functions under Tests and dropped them from the uncalled-function analysis.
fn attr_marks_test(rest: &str) -> bool {
    // Blank the string literals FIRST, so no later step can read inside one.
    // Doing it with a bare-token scan instead was not enough: the tokenizer
    // split on every non-word character, so `feature = "test-utils"` still
    // yielded a `test` token off the hyphen.
    let rest = without_string_literals(rest);
    let path = rest
        .split(['(', '=', ']', ' ', '\t'])
        .next()
        .unwrap_or_default()
        .trim();
    // `#[tokio::test]`, `#[test_log::test]`: the final segment is the marker.
    let seg = path.rsplit("::").next().unwrap_or(path);
    // `#[cfg(test)]` directly on the item — it exists only in a test build.
    if seg == "cfg" {
        return attr_args(&rest).is_some_and(mentions_cfg_test);
    }
    // `#[cfg_attr(<condition>, <attr>, …)]` applies the attributes when the
    // condition holds, so only the attributes decide. Reading the condition
    // too would call `#[cfg_attr(test, derive(Debug))]` a test.
    if seg == "cfg_attr" {
        let Some(args) = attr_args(&rest) else {
            return false;
        };
        return top_level_parts(args)
            .into_iter()
            .skip(1)
            .any(attr_marks_test);
    }
    // `test`, `test_case`, `wasm_bindgen_test`, `traced_test`, `rstest`.
    seg == "test" || seg.starts_with("test_") || seg.ends_with("_test") || seg == "rstest"
}

/// `s` with every double-quoted literal emptied out. A raw string's `\` is
/// not an escape, so `r"a\"` is blanked one character short — harmless here,
/// since the result is only ever scanned for bare word tokens.
fn without_string_literals(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_string = false;
    let mut escaped = false;
    for c in s.chars() {
        if in_string {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
        } else if c == '"' {
            in_string = true;
        } else {
            out.push(c);
        }
    }
    out
}

/// What sits between an attribute's first `(` and its matching `)`, or `None`
/// when it takes no arguments.
fn attr_args(rest: &str) -> Option<&str> {
    let open = rest.find('(')?;
    let mut depth = 0usize;
    for (i, c) in rest[open..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&rest[open + 1..open + i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// `args` split on the commas at paren depth 0, each part trimmed.
fn top_level_parts(args: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;
    for (i, c) in args.char_indices() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                out.push(args[start..i].trim());
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    out.push(args[start..].trim());
    out
}

/// Whether a cfg predicate holds only in a test build: a bare `test` token
/// under an EVEN number of enclosing `not( … )` groups. `#[cfg(not(test))]`
/// marks the opposite — an item that exists everywhere BUT a test build — and
/// reading it as a marker filed ordinary functions under Tests.
fn mentions_cfg_test(args: &str) -> bool {
    let hit = |token: &str, groups: &[bool]| {
        token == "test" && groups.iter().filter(|negated| **negated).count() % 2 == 0
    };
    // One entry per open group, saying whether it is a `not( … )`.
    let mut groups: Vec<bool> = Vec::new();
    let mut token = String::new();
    for c in args.chars() {
        if c.is_alphanumeric() || c == '_' {
            token.push(c);
            continue;
        }
        if hit(&token, &groups) {
            return true;
        }
        let opens_negation = token == "not";
        token.clear();
        match c {
            '(' => groups.push(opens_negation),
            ')' => {
                groups.pop();
            }
            _ => {}
        }
    }
    hit(&token, &groups)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_rust_symbols() {
        let src =
            "pub struct Point { x: f64 }\n\npub fn origin() -> Point {\n    Point { x: 0.0 }\n}\n";
        let symbols = extract(src, "rust");
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Point"), "symbols: {symbols:?}");
        assert!(names.contains(&"origin"), "symbols: {symbols:?}");
        let origin = symbols.iter().find(|s| s.name == "origin").unwrap();
        assert_eq!(origin.line, 3);
    }

    #[test]
    fn extracts_dart_symbols() {
        let src = "class Point {\n  final double x;\n  Point(this.x);\n  double get magnitude => x;\n}\n\nPoint origin() => Point(0.0);\n";
        let symbols = extract(src, "dart");
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Point"), "symbols: {symbols:?}");
        assert!(names.contains(&"origin"), "symbols: {symbols:?}");
    }

    #[test]
    fn extracts_typescript_source_symbols() {
        let src = "export type Kind = \"a\" | \"b\";\n\
                   export interface Token { kind: Kind }\n\
                   export function tokenize(s: string): Token[] { return []; }\n\
                   const isDigit = (c: string): boolean => c >= \"0\";\n\
                   export class Parser {\n  parse(): number { return 0; }\n}\n";
        let symbols = extract(src, "typescript");
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        // The bundled query only found `Token`; the source-oriented one gets all.
        for want in ["Kind", "Token", "tokenize", "isDigit", "Parser", "parse"] {
            assert!(names.contains(&want), "missing {want} in {names:?}");
        }
    }

    #[test]
    fn extracts_typescript_interface_members_and_class_fields() {
        // Interface members (incl. function-typed properties, where libraries put
        // JSDoc) and class fields are surfaced — but an inline object-type
        // property in a parameter annotation is NOT swept in.
        let src = "export interface Chalk {\n\
                   \x20 rgb: (r: number, g: number, b: number) => Chalk;\n\
                   \x20 level: number;\n\
                   \x20 apply(opts: { inline: boolean }): void;\n\
                   }\n\
                   export class Styler {\n  cache = new Map();\n  build() {}\n}\n";
        let symbols = extract(src, "typescript");
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        // Interface property members and the class field are present.
        for want in ["rgb", "level", "apply", "cache", "build"] {
            assert!(names.contains(&want), "missing {want} in {names:?}");
        }
        // The inline object-type property `inline` must NOT be captured.
        assert!(
            !names.contains(&"inline"),
            "over-captured inline type prop: {names:?}"
        );
        // Kinds are tagged correctly.
        let kind = |n: &str| {
            symbols
                .iter()
                .find(|s| s.name == n)
                .map(|s| s.kind.as_str())
        };
        assert_eq!(kind("rgb"), Some("property"));
        assert_eq!(kind("level"), Some("property"));
        assert_eq!(kind("cache"), Some("field"));
    }

    #[test]
    fn language_without_tags_query_yields_empty() {
        assert!(extract("{\"a\": 1}", "json").is_empty());
    }
}

#[cfg(test)]
mod ordinal_tests {
    use super::*;

    fn sym(name: &str, kind: &str, line: usize) -> Symbol {
        Symbol {
            name: name.to_string(),
            kind: kind.to_string(),
            line,
            end_line: line,
        }
    }

    /// A list exercising every case the two definitions could disagree on:
    /// same-name callables of both kinds, a same-name NON-callable before and
    /// after them, two same-name callables on one line, and a name that only
    /// ever appears as a type.
    fn representative() -> Vec<Symbol> {
        vec![
            sym("helper", "function", 1),
            sym("new", "struct", 5),
            sym("new", "method", 10),
            sym("new", "function", 20),
            sym("new", "struct", 25),
            sym("new", "method", 30),
            sym("default", "method", 40),
            sym("dup", "method", 50),
            sym("dup", "function", 50),
            sym("dup", "method", 60),
            sym("Point", "struct", 70),
        ]
    }

    /// The ordinal is a cache identity, so `fn_ordinals` must reproduce the
    /// exact numbers the one-at-a-time scan produced, not merely agree with
    /// itself. These literals were captured by running the pre-existing
    /// `fn_ordinal` over `representative()` before `fn_ordinals` existed.
    #[test]
    fn batch_ordinals_match_the_captured_pre_change_output() {
        let syms = representative();
        assert_eq!(fn_ordinals(&syms), vec![0, 0, 0, 1, 2, 2, 0, 0, 0, 2, 0]);

        // Same multiset in the opposite order: `fn_ordinals` groups and sorts
        // rather than trusting `extract`'s line ordering, so a differently
        // ordered list must still reproduce the per-symbol scan.
        let mut rev = representative();
        rev.reverse();
        assert_eq!(fn_ordinals(&rev), vec![0, 2, 0, 0, 0, 2, 2, 1, 0, 0, 0]);
    }

    /// The invariant that keeps the two from drifting: equality at EVERY index,
    /// including the non-callables.
    #[test]
    fn batch_ordinals_equal_the_per_symbol_scan_at_every_index() {
        for syms in [
            representative(),
            extract(
                "impl A { fn new() {} fn go(&self) {} }\nimpl B { fn new() {} }\nfn new() {}\n",
                "rust",
            ),
        ] {
            let batch = fn_ordinals(&syms);
            let one_by_one: Vec<u32> = syms.iter().map(|s| fn_ordinal(&syms, s)).collect();
            assert_eq!(batch, one_by_one, "symbols: {syms:?}");
        }
    }

    /// Sizing note: the quadratic form this replaced needs ~6.7 s at this size
    /// in release and ~62 s unoptimized, measured on the machine that wrote the
    /// test; the batch pass needs milliseconds. The budget is therefore a real
    /// discriminator and not merely "faster", while still leaving two orders of
    /// magnitude of slack for a loaded CI box.
    #[test]
    fn all_same_name_callables_stay_subquadratic() {
        const N: usize = 60_000;
        let syms: Vec<Symbol> = (1..=N).map(|i| sym("same", "method", i)).collect();
        let started = std::time::Instant::now();
        let ordinals = fn_ordinals(&syms);
        let elapsed = started.elapsed();
        // Every entry is preceded by exactly the ones on lower lines.
        assert!(
            ordinals.iter().enumerate().all(|(i, o)| *o == i as u32),
            "ordinals diverged at large N"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "fn_ordinals took {elapsed:?} for {N} symbols — the per-symbol rescan is back"
        );
    }
}

#[cfg(test)]
mod test_attr_tests {
    use super::*;

    /// A `test` substring in an attribute's ARGUMENTS is not a test marker.
    /// Matching it filed ordinary functions under Tests and excluded them
    /// from the uncalled-function analysis.
    #[test]
    fn arguments_containing_test_do_not_mark_a_test() {
        let src = "\
#[cfg(feature = \"contest\")]
fn helper() {}

#[serde(rename = \"latest\")]
fn renamed() {}
";
        let lines: Vec<&str> = src.lines().collect();
        assert!(!is_test_fn(&lines, 2, "helper", "rust"));
        assert!(!is_test_fn(&lines, 5, "renamed", "rust"));
    }

    /// The shapes that must keep matching.
    #[test]
    fn test_attribute_paths_still_match() {
        for attr in [
            "#[test]",
            "#[tokio::test]",
            "#[test_log::test]",
            "#[rstest]",
            "#[test_case(1, 2)]",
            "#[wasm_bindgen_test]",
            "#[cfg(test)]",
            "#[cfg(any(test, feature = \"x\"))]",
            "#[cfg(all(test, unix))]",
            // Double negation is still a test build.
            "#[cfg(not(not(test)))]",
            // `cfg_attr` applies the attribute, so the attribute decides.
            "#[cfg_attr(feature = \"e2e\", test)]",
            "#[cfg_attr(not(target_arch = \"wasm32\"), tokio::test)]",
        ] {
            let src = format!("{attr}\nfn f() {{}}\n");
            let lines: Vec<&str> = src.lines().collect();
            assert!(
                is_test_fn(&lines, 2, "f", "rust"),
                "{attr} must mark a test"
            );
        }
    }

    /// A `test` the predicate NEGATES, or one that only ever appears inside a
    /// string literal, marks the opposite of a test. Both used to match: the
    /// bare-token scan split `"test-utils"` on the hyphen, and it could not
    /// see `not(…)` at all.
    #[test]
    fn negated_and_quoted_tests_do_not_mark_a_test() {
        for attr in [
            "#[cfg(not(test))]",
            "#[cfg(all(not(test), unix))]",
            "#[cfg(feature = \"test-utils\")]",
            "#[cfg(feature = \"test\")]",
            "#[cfg(feature = \"integration-test\")]",
            // The CONDITION of a cfg_attr is not what gets applied.
            "#[cfg_attr(test, derive(Debug))]",
            "#[cfg_attr(all(test, unix), ignore)]",
        ] {
            let src = format!("{attr}\nfn f() {{}}\n");
            let lines: Vec<&str> = src.lines().collect();
            assert!(
                !is_test_fn(&lines, 2, "f", "rust"),
                "{attr} must NOT mark a test"
            );
        }
    }
}
