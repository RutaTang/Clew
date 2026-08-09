//! The API documentation index for the Docs view.
//!
//! Assembles what clew already extracts — the tree-sitter symbol outline plus
//! per-symbol doc comments — into a browsable, nested API surface for a file:
//! signature, doc, visibility, and members nested under their enclosing type.
//! No build, no language doc tool, no webview: every language clew already
//! parses. The client enriches a selected entry via LSP hover.

use clew_protocol::DocItem;

/// Build the documented API of one file: top-level items, with members nested
/// under their enclosing type/module by source-range containment. Returns an
/// empty list when the language has no outline.
pub fn build_file(source: &str, lang_key: &str) -> Vec<DocItem> {
    let symbols = crate::outline::extract(source, lang_key);
    if symbols.is_empty() {
        return Vec::new();
    }
    let docs = crate::docs::extract_full(source, lang_key, &symbols);
    let lines: Vec<&str> = source.lines().collect();

    // A flat record per symbol, in line order (outline is already sorted).
    struct Raw {
        name: String,
        kind: String,
        line: usize,
        end_line: usize,
        signature: String,
        doc: String,
        decl: String,
    }
    let raws: Vec<Raw> = symbols
        .iter()
        .map(|s| Raw {
            name: s.name.clone(),
            kind: s.kind.clone(),
            line: s.line,
            end_line: s.end_line,
            signature: signature(&lines, s.line),
            doc: docs.get(&s.line).cloned().unwrap_or_default(),
            decl: lines
                .get(s.line.saturating_sub(1))
                .copied()
                .unwrap_or("")
                .to_string(),
        })
        .collect();

    // Nest by containment: symbol B is a child of the closest earlier symbol A
    // whose range [line, end_line] still encloses B's start line. A stack of
    // open ancestors gives this in one pass.
    let n = raws.len();
    let mut parent: Vec<Option<usize>> = vec![None; n];
    let mut stack: Vec<usize> = Vec::new();
    for i in 0..n {
        while let Some(&top) = stack.last() {
            if raws[top].end_line < raws[i].line {
                stack.pop();
            } else {
                break;
            }
        }
        parent[i] = stack.last().copied();
        stack.push(i);
    }
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut roots: Vec<usize> = Vec::new();
    for (i, p) in parent.iter().enumerate() {
        match p {
            Some(p) => children[*p].push(i),
            None => roots.push(i),
        }
    }

    // Visibility needs the nesting, so it is decided here rather than with the
    // rest of each record: in the C-family languages a top-level declaration
    // and a class member follow OPPOSITE defaults, and judging both by the
    // member rule published every unexported top-level helper as public API.
    let exported = reexported_names(source, lang_key);
    let public: Vec<bool> = (0..n)
        .map(|i| {
            // "Has a parent" is NOT "is a member". Nesting here is pure
            // source-range containment, so a function declared inside another
            // function has a parent too — and judging it by the member rule
            // ("public unless marked private") published every local helper of
            // an exported function as public API. Only a type-like enclosing
            // symbol makes its children members.
            let is_member = parent[i].is_some_and(|p| kind_takes_members(&raws[p].kind));
            // C++ access is section-based, so a member's own declaration line
            // says nothing about it; its type's `public:`/`private:` labels do.
            if lang_key == "cpp" && is_member {
                let p = parent[i].expect("is_member implies a parent");
                return cpp_member_is_public(&lines, &raws[p].decl, raws[p].line, raws[i].line);
            }
            is_public(&raws[i].decl, &raws[i].name, lang_key, is_member)
                || (parent[i].is_none() && exported.contains(raws[i].name.as_str()))
        })
        .collect();

    // Assemble bottom-up rather than recursively: the nesting depth is the
    // source's, over a file the repository controls, and a recursive build
    // would overflow the stack on a deeply nested one. The containment pass
    // above pushes in increasing order, so every child's index is greater
    // than its parent's — walking indices downwards therefore always finds a
    // node's children already built.
    let mut built: Vec<Option<DocItem>> = vec![None; n];
    for i in (0..n).rev() {
        let kids = children[i]
            .iter()
            .filter_map(|&c| built[c].take())
            .collect();
        let r = &raws[i];
        built[i] = Some(DocItem {
            name: r.name.clone(),
            kind: r.kind.clone(),
            signature: r.signature.clone(),
            doc: r.doc.clone(),
            line: r.line,
            public: public[i],
            children: kids,
        });
    }
    roots.iter().filter_map(|&i| built[i].take()).collect()
}

/// The declaration text for the item at `line1` (1-based): join lines from the
/// definition until the body opens (`{`/`;`) or the signature looks complete
/// (balanced parens and not obviously continued), so multi-line signatures are
/// captured but bodies are not.
fn signature(lines: &[&str], line1: usize) -> String {
    let start = line1.saturating_sub(1);
    let mut acc = String::new();
    let mut depth: i32 = 0;
    for l in lines.iter().skip(start).take(8) {
        let cut = l.find(['{', ';']);
        let seg = match cut {
            Some(i) => &l[..i],
            None => l,
        };
        for c in seg.chars() {
            match c {
                '(' | '[' | '<' => depth += 1,
                ')' | ']' | '>' => depth -= 1,
                _ => {}
            }
        }
        if !acc.is_empty() {
            acc.push(' ');
        }
        acc.push_str(seg.trim());
        if cut.is_some() {
            break;
        }
        let t = seg.trim_end();
        if depth <= 0 && !t.is_empty() && !t.ends_with(',') && !t.ends_with('(') {
            break;
        }
    }
    acc.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whether a symbol of this kind gives its children the visibility defaults of
/// MEMBERS. Anything else — a function, a method, a field — encloses locals,
/// which are never part of the API.
fn kind_takes_members(kind: &str) -> bool {
    matches!(
        kind,
        "class" | "interface" | "enum" | "struct" | "trait" | "module" | "type" | "union"
    )
}

/// Whether a C++ member is public. Access there is section-based: the last
/// `public:` / `private:` / `protected:` label above the member inside its own
/// type decides, defaulting to private for `class` and public for
/// `struct`/`union`. Nothing on the member's declaration line says which.
///
/// Brace counting keeps a nested type's labels from leaking out, but it counts
/// braces in strings and comments too, so an unusual file can be misjudged —
/// still strictly better than the previous answer, which was "everything is
/// public".
fn cpp_member_is_public(
    lines: &[&str],
    parent_decl: &str,
    parent_line: usize,
    line: usize,
) -> bool {
    let mut public = !parent_decl.trim_start().starts_with("class");
    let mut depth = 0i32;
    for l in lines
        .iter()
        .take(line.saturating_sub(1))
        .skip(parent_line.saturating_sub(1))
    {
        let t = l.trim_start();
        if depth == 1 {
            if t.starts_with("public:") {
                public = true;
            } else if t.starts_with("private:") || t.starts_with("protected:") {
                public = false;
            }
        }
        depth += l.matches('{').count() as i32;
        depth -= l.matches('}').count() as i32;
    }
    public
}

/// Names a JS/TS file exports through a separate statement rather than an
/// `export` keyword on the declaration itself (`export { a, b as c }`,
/// `export default a`, `module.exports = { a }`, `exports.a = a`). Without
/// these, requiring `export` on the declaration line would hide a genuinely
/// public API — the opposite mistake from treating every top-level helper as
/// public.
fn reexported_names(source: &str, lang: &str) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    if !matches!(lang, "typescript" | "tsx" | "javascript" | "jsx") {
        return out;
    }
    // An `export { … }` clause or a `module.exports = { … }` object may span
    // lines, so accumulate until its braces balance. Only those two shapes:
    // accumulating any unbalanced line swallowed the BODY of
    // `module.exports = function () {` and published every local inside it.
    let mut pending: Option<String> = None;
    for line in source.lines() {
        let t = line.trim();
        let statement = match pending.take() {
            Some(acc) => format!("{acc} {t}"),
            None if opens_export_statement(t) => t.to_string(),
            None => continue,
        };
        if statement.matches('{').count() > statement.matches('}').count()
            && continues_onto_later_lines(&statement)
        {
            pending = Some(statement);
            continue;
        }
        collect_exported_names(&statement, &mut out);
    }
    out
}

/// Whether this line starts a statement that can name exports.
fn opens_export_statement(t: &str) -> bool {
    t.starts_with("export {")
        || t.starts_with("export type {")
        || t.starts_with("export default ")
        || t.starts_with("exports.")
        || t.starts_with("module.exports")
}

/// Whether an unbalanced `{` in this statement opens something that still
/// names exports on later lines. An export clause always does; a CommonJS
/// assignment does only in its object form. Treating every unbalanced line as
/// continuable swallowed the BODY of `module.exports = function () {` and
/// published every local declared inside it.
fn continues_onto_later_lines(statement: &str) -> bool {
    statement.starts_with("export {")
        || statement.starts_with("export type {")
        || statement
            .split_once('=')
            .is_some_and(|(_, rhs)| rhs.trim_start().starts_with('{'))
}

/// Add every LOCAL declaration `statement` publishes to `out`.
fn collect_exported_names(statement: &str, out: &mut std::collections::HashSet<String>) {
    // CommonJS first: `exports.a = b` also starts with "export".
    //
    // `exports.a = b` publishes the local `b` (and names it `a`),
    // `module.exports = { a, b }` publishes each listed local. Taking every
    // identifier is deliberately loose — both names are usually the same one.
    if statement.starts_with("module.exports") || statement.starts_with("exports.") {
        for tok in statement.split(|c: char| !c.is_alphanumeric() && c != '_' && c != '$') {
            if !tok.is_empty() && !matches!(tok, "module" | "exports" | "function" | "require") {
                out.insert(tok.to_string());
            }
        }
        return;
    }
    if let Some(rest) = statement.strip_prefix("export default ") {
        // `export default function f` carries the keyword on the declaration,
        // where `is_public` already sees it; only the bare-identifier form
        // needs naming here.
        let name = rest.trim().trim_end_matches(';').trim();
        if is_plain_ident(name) {
            out.insert(name.to_string());
        }
        return;
    }
    if statement.starts_with("export") {
        // `export { a } from "./dep"` re-exports somebody ELSE's names. A
        // local declaration that happens to share one is not exported by it,
        // and neither is a local named after a path segment of the specifier.
        let Some(close) = statement.rfind('}') else {
            return;
        };
        if statement[close..].contains(" from ") {
            return;
        }
        let Some(open) = statement.find('{') else {
            return;
        };
        for clause in statement[open + 1..close].split(',') {
            // `a as b` publishes the LOCAL `a` under the name `b`; `b` names
            // nothing in this file.
            let local = clause.split(" as ").next().unwrap_or(clause).trim();
            let local = local.trim_start_matches("type ").trim();
            if is_plain_ident(local) {
                out.insert(local.to_string());
            }
        }
        return;
    }
    // CommonJS: `exports.a = b` publishes the local `b` (and names it `a`),
    // `module.exports = { a, b }` publishes each listed local. Taking every
    // identifier is deliberately loose — both names are usually the same one.
    for tok in statement.split(|c: char| !c.is_alphanumeric() && c != '_' && c != '$') {
        if !tok.is_empty() && !matches!(tok, "module" | "exports" | "function" | "require") {
            out.insert(tok.to_string());
        }
    }
}

/// Whether `s` is a single JS identifier and nothing else.
fn is_plain_ident(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with(|c: char| c.is_ascii_digit())
        && s.chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '$')
}

/// Whether the item is part of the public API, per each language's convention.
/// `decl` is its declaration line, `name` its identifier, and `is_member`
/// says whether it is nested inside another item (a class member, a method)
/// rather than declared at the top level of the file.
fn is_public(decl: &str, name: &str, lang: &str, is_member: bool) -> bool {
    let d = decl.trim_start();
    match lang {
        // Any `pub` (including pub(crate)/pub(super)) counts for the surface.
        "rust" => d.starts_with("pub"),
        "typescript" | "tsx" | "javascript" | "jsx" => {
            if is_member {
                // Class members are public unless they say otherwise.
                !(d.contains("private ") || name.starts_with('#'))
            } else {
                // A top-level declaration is reachable only if it is
                // EXPORTED. Applying the member default here — public unless
                // marked private, which has no meaning at file scope — put
                // every internal helper in the default public-only Docs view.
                d.starts_with("export") || d.starts_with("module.exports")
            }
        }
        // Exported = capitalized identifier.
        "go" => name.chars().next().is_some_and(char::is_uppercase),
        // Convention: a leading underscore marks non-public.
        "python" | "dart" => !name.starts_with('_'),
        // Java says it outright, in any modifier order. Package-private (no
        // modifier at all) counts as public here: it is the default for a lot
        // of ordinary API, and calling it private would empty the Docs view
        // of most files.
        "java" => !modifiers(d).any(|w| w == "private" || w == "protected"),
        // C and C++: a top-level definition marked `static` has internal
        // linkage, so it is not part of the file's API. C++ MEMBERS never get
        // here — their access comes from their type's sections, decided in
        // `build_file`.
        "c" | "cpp" => !modifiers(d).any(|w| w == "static"),
        // An unknown language gets the benefit of the doubt: showing an item
        // that turns out to be private is a smaller failure than an empty
        // Docs view.
        _ => true,
    }
}

/// The words before a declaration's parameter list — where every language in
/// the C family puts its visibility and linkage keywords.
fn modifiers(decl: &str) -> impl Iterator<Item = &str> {
    decl.split('(').next().unwrap_or(decl).split_whitespace()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_nests_and_marks_visibility() {
        let src = "\
/// A point.
pub struct Point {
    x: f64,
}

fn helper() {}
";
        let items = build_file(src, "rust");
        // `Point` is public + documented; `helper` is private.
        let point = items.iter().find(|i| i.name == "Point").unwrap();
        assert!(point.public);
        assert!(point.doc.contains("A point"));
        assert!(point.signature.contains("pub struct Point"));
        assert!(items.iter().any(|i| i.name == "helper" && !i.public));
    }

    #[test]
    fn python_methods_nest_under_class() {
        let src = "\
class Greeter:
    def hello(self):
        pass
    def _secret(self):
        pass
";
        let items = build_file(src, "python");
        let cls = items.iter().find(|i| i.name == "Greeter").unwrap();
        assert!(cls.public);
        assert!(cls.children.iter().any(|c| c.name == "hello" && c.public));
        assert!(
            cls.children
                .iter()
                .any(|c| c.name == "_secret" && !c.public)
        );
    }

    #[test]
    fn signature_stops_at_body() {
        let lines = vec!["pub fn add(a: i32, b: i32) -> i32 {", "    a + b", "}"];
        assert_eq!(signature(&lines, 1), "pub fn add(a: i32, b: i32) -> i32");
    }
}

#[cfg(test)]
mod visibility_tests {
    use super::*;

    fn find<'a>(items: &'a [DocItem], name: &str) -> &'a DocItem {
        items
            .iter()
            .find(|i| i.name == name)
            .unwrap_or_else(|| panic!("no item {name} in {items:?}"))
    }

    /// A top-level declaration and a class member follow opposite defaults.
    /// Judging both by the member rule ("public unless marked private") put
    /// every unexported helper into the default public-only Docs view.
    #[test]
    fn top_level_js_needs_export_but_members_do_not() {
        let src = "\
export function shown() {}
function internalOnly() {}
export class Widget {
  render() {}
  private hidden() {}
}
";
        let items = build_file(src, "typescript");
        assert!(find(&items, "shown").public);
        assert!(!find(&items, "internalOnly").public, "{items:?}");

        let widget = find(&items, "Widget");
        assert!(widget.public);
        assert!(find(&widget.children, "render").public);
        assert!(!find(&widget.children, "hidden").public);
    }

    /// Exporting through a separate statement still counts, so requiring the
    /// keyword on the declaration does not hide a real public API.
    #[test]
    fn a_separate_export_statement_marks_the_declaration_public() {
        let src = "\
function alpha() {}
function beta() {}
function unexported() {}
export { alpha, beta as renamed };
";
        let items = build_file(src, "javascript");
        assert!(find(&items, "alpha").public);
        assert!(find(&items, "beta").public);
        assert!(!find(&items, "unexported").public, "{items:?}");
    }

    /// The other two ways a JS file names an existing declaration as its API.
    /// Missing them hid a module's real entry point from the Docs view.
    #[test]
    fn default_and_commonjs_exports_count() {
        let src = "\
function helper() {}
export default helper;
function shipped() {}
exports.shipped = shipped;
function listed() {}
module.exports = {
  listed
};
";
        let items = build_file(src, "javascript");
        for name in ["helper", "shipped", "listed"] {
            assert!(find(&items, name).public, "{name} in {items:?}");
        }
    }

    /// `export { … } from "…"` re-exports another module's names, so a local
    /// declaration that merely shares one is NOT part of this file's API —
    /// and neither is one named after a word in the specifier.
    #[test]
    fn a_re_export_does_not_publish_unrelated_locals() {
        let src = "\
function outer() {}
function from() {}
function dep() {}
export { outer } from \"./dep\";
";
        let items = build_file(src, "javascript");
        for name in ["outer", "from", "dep"] {
            assert!(!find(&items, name).public, "{name} in {items:?}");
        }
    }

    /// An `as` rename publishes the LOCAL name. The alias names nothing in
    /// this file, so a local that happens to match it stays private.
    #[test]
    fn an_alias_does_not_publish_a_local_of_the_same_name() {
        let src = "\
function beta() {}
function renamed() {}
export { beta as renamed };
";
        let items = build_file(src, "javascript");
        assert!(find(&items, "beta").public);
        assert!(!find(&items, "renamed").public, "{items:?}");
    }

    /// `module.exports = function () {` is not an object literal, so the
    /// scan must stop at that line instead of swallowing the body and
    /// publishing every local inside it.
    #[test]
    fn a_commonjs_function_body_is_not_swallowed() {
        let src = "\
function hidden() {}
module.exports = function () {
  return hidden();
};
";
        let items = build_file(src, "javascript");
        assert!(!find(&items, "hidden").public, "{items:?}");
    }

    /// Nesting is source-range containment, so a local declared inside a
    /// function has a parent. Judging it by the CLASS-MEMBER rule published
    /// every helper of an exported function as public API.
    #[test]
    fn locals_of_an_exported_function_are_not_members() {
        let src = "\
export function outer() {
  function inner() {}
  return inner;
}
";
        let items = build_file(src, "typescript");
        let outer = find(&items, "outer");
        assert!(outer.public);
        assert!(!find(&outer.children, "inner").public, "{items:?}");
    }

    /// Java, C and C++ all say what is private; answering `true` for every
    /// language without a special case published all of it.
    #[test]
    fn c_family_private_symbols_are_not_public() {
        let java = build_file(
            "public class A {\n  private void hidden() {}\n  public void shown() {}\n}\n",
            "java",
        );
        let a = find(&java, "A");
        assert!(a.public);
        assert!(!find(&a.children, "hidden").public, "{java:?}");
        assert!(find(&a.children, "shown").public, "{java:?}");

        let c = build_file(
            "static int hidden(void) { return 1; }\nint shown(void) { return 2; }\n",
            "c",
        );
        assert!(!find(&c, "hidden").public, "{c:?}");
        assert!(find(&c, "shown").public, "{c:?}");

        // C++ access is section-based, and `class` starts out private.
        let cpp = build_file(
            "class A {\n  void implicitly_private();\npublic:\n  void shown();\nprivate:\n  void hidden();\n};\n",
            "cpp",
        );
        let a = find(&cpp, "A");
        assert!(!find(&a.children, "implicitly_private").public, "{cpp:?}");
        assert!(find(&a.children, "shown").public, "{cpp:?}");
        assert!(!find(&a.children, "hidden").public, "{cpp:?}");

        // A `struct` starts out public.
        let cpp = build_file(
            "struct B {\n  void shown();\nprivate:\n  void hidden();\n};\n",
            "cpp",
        );
        let b = find(&cpp, "B");
        assert!(find(&b.children, "shown").public, "{cpp:?}");
        assert!(!find(&b.children, "hidden").public, "{cpp:?}");
    }
}
