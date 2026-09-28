//! Language registry and tree-sitter based syntax highlighting.
//!
//! [`Lang`] is the single source of truth for which languages clew
//! understands. Every per-language table in clew-core — grammar, queries, file
//! extensions, fence tags, doc-comment style, visibility rules, import model,
//! call-site walk, builtins, test detection — is an exhaustive `match` over it,
//! so adding a language is a compile error in every table that forgot it
//! instead of a silent "no support" at run time. The `&str` keys (`"rust"`,
//! `"tsx"`, …) stay the currency of the wire and of every `&str`-keyed API;
//! [`Lang::from_key`] converts at that boundary.
//!
//! Highlighting output is a list of lines, each line a list of
//! `(text, style index)` spans. The style index points into
//! [`HIGHLIGHT_NAMES`]; mapping an index to a color is the client's job (the
//! theme lives on the GUI side), so this crate stays GUI-free and the same
//! tokenizer runs in the client and in the headless server.

use std::path::Path;
use std::sync::OnceLock;

use tree_sitter::{Language, Parser, Query, Tree};
use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent, Highlighter};

/// Capture names recognized by the highlighter. Index = style id.
pub const HIGHLIGHT_NAMES: &[&str] = &[
    "attribute",
    "comment",
    "constant",
    "constant.builtin",
    "constructor",
    "embedded",
    "escape",
    "function",
    "function.builtin",
    "function.method",
    "keyword",
    "label",
    "module",
    "number",
    "operator",
    "property",
    "punctuation",
    "punctuation.bracket",
    "punctuation.delimiter",
    "punctuation.special",
    "string",
    "string.escape",
    "string.special",
    "tag",
    "type",
    "type.builtin",
    "variable",
    "variable.builtin",
    "variable.parameter",
];

/// The styles that mark literal text — strings, comments and escapes — where
/// brackets and identifiers are not code. `embedded` is deliberately absent: it
/// is the CODE inside a string (a template substitution), and the highlighter
/// reports the innermost style per span, so a `${…}` body is judged by its own
/// tokens rather than by the string around it.
const LITERAL_STYLES: &[&str] = &[
    "comment",
    "escape",
    "string",
    "string.escape",
    "string.special",
];

/// One source line as a list of `(text, style index)` spans.
///
/// This is the protocol's wire type for a highlighted line — the tokenizer
/// produces exactly what the client renders and the server transmits, with no
/// conversion in between.
pub use clew_protocol::HlLine;

/// Declares [`Lang`], its key table and [`Lang::ALL`] from ONE list, so the
/// enum, the keys and the iteration order can never disagree.
macro_rules! languages {
    ($($variant:ident => $key:literal,)*) => {
        /// A language clew understands. See the module docs for why every
        /// per-language table matches on this exhaustively.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum Lang {
            $(#[doc = concat!("Key `", $key, "`.")] $variant,)*
        }

        impl Lang {
            /// Every language, in declaration order: `ALL[l as usize] == l`.
            pub const ALL: &'static [Lang] = &[$(Lang::$variant,)*];

            /// The stable string key (`"rust"`, `"tsx"`, …) used on the wire,
            /// in persisted caches and by every `&str`-keyed API.
            pub const fn key(self) -> &'static str {
                match self {
                    $(Lang::$variant => $key,)*
                }
            }
        }
    };
}

languages! {
    Rust => "rust",
    Python => "python",
    JavaScript => "javascript",
    TypeScript => "typescript",
    Tsx => "tsx",
    Go => "go",
    Dart => "dart",
    C => "c",
    Cpp => "cpp",
    Java => "java",
    Json => "json",
    Bash => "bash",
    Yaml => "yaml",
    Toml => "toml",
    Html => "html",
    Css => "css",
    Zig => "zig",
}

/// How many languages there are; sizes the per-language caches below.
const LANG_COUNT: usize = Lang::ALL.len();

/// Languages whose code links into one program, so a bare call written in one
/// can reach a definition written in another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    /// JavaScript, TypeScript and TSX share one module system.
    Ecma,
    /// C and C++ link together (and `.h` headers are read as either).
    CLike,
    /// Everything else only reaches its own language.
    Alone(Lang),
}

impl Lang {
    /// The language for a key, or `None` for a key clew does not know.
    pub fn from_key(key: &str) -> Option<Lang> {
        Lang::ALL.iter().copied().find(|l| l.key() == key)
    }

    /// The language to PARSE `source` with when a caller names `key`.
    ///
    /// Identical to [`Lang::from_key`] except for C: C++ headers are routinely
    /// named `.h`, which [`detect`] can only call C, and parsing C++ with the C
    /// grammar loses every class, namespace and template (and highlights
    /// `class` as an identifier). A `"c"` source that uses syntax only C++ has
    /// is therefore handled as C++ — whatever its extension, since the key
    /// alone cannot tell a `.h` from a `.c`. The check reads only syntax that
    /// real C does not write (see [`looks_like_cpp`] for the contrived
    /// exceptions), so C that uses `class`, `namespace`, `template` or
    /// `public` as ordinary identifiers or labels stays C.
    pub fn for_source(key: &str, source: &str) -> Option<Lang> {
        let lang = Lang::from_key(key)?;
        Some(if lang == Lang::C && looks_like_cpp(source) {
            Lang::Cpp
        } else {
            lang
        })
    }

    /// Human-readable name.
    pub const fn name(self) -> &'static str {
        match self {
            Lang::Rust => "Rust",
            Lang::Python => "Python",
            Lang::JavaScript => "JavaScript",
            Lang::TypeScript => "TypeScript",
            Lang::Tsx => "TSX",
            Lang::Go => "Go",
            Lang::Dart => "Dart",
            Lang::C => "C",
            Lang::Cpp => "C++",
            Lang::Java => "Java",
            Lang::Json => "JSON",
            Lang::Bash => "Shell",
            Lang::Yaml => "YAML",
            Lang::Toml => "TOML",
            Lang::Html => "HTML",
            Lang::Css => "CSS",
            Lang::Zig => "Zig",
        }
    }

    /// File extensions (lowercase, no dot) that select this language by path.
    /// A test pins that no extension is claimed twice.
    pub const fn extensions(self) -> &'static [&'static str] {
        match self {
            Lang::Rust => &["rs"],
            Lang::Python => &["py", "pyi"],
            Lang::JavaScript => &["js", "mjs", "cjs", "jsx"],
            Lang::TypeScript => &["ts", "mts", "cts"],
            Lang::Tsx => &["tsx"],
            Lang::Go => &["go"],
            Lang::Dart => &["dart"],
            Lang::C => &["c", "h"],
            Lang::Cpp => &["cc", "cpp", "cxx", "hpp", "hh", "hxx", "ipp"],
            Lang::Java => &["java"],
            Lang::Json => &["json", "jsonc"],
            Lang::Bash => &["sh", "bash", "zsh"],
            Lang::Yaml => &["yml", "yaml"],
            Lang::Toml => &["toml"],
            Lang::Html => &["html", "htm"],
            Lang::Css => &["css"],
            Lang::Zig => &["zig"],
        }
    }

    /// Markdown fence tags (lowercase) that name this language besides its key
    /// — the aliases LLMs actually emit.
    const fn fence_aliases(self) -> &'static [&'static str] {
        match self {
            Lang::Rust => &["rs"],
            Lang::Python => &["py"],
            Lang::JavaScript => &["js", "jsx"],
            Lang::TypeScript => &["ts"],
            Lang::Tsx => &[],
            Lang::Go => &["golang"],
            Lang::Dart => &[],
            Lang::C => &["h"],
            Lang::Cpp => &["c++", "cxx"],
            Lang::Java => &[],
            Lang::Json => &["jsonc"],
            Lang::Bash => &["sh", "shell", "zsh", "console"],
            Lang::Yaml => &["yml"],
            Lang::Toml => &[],
            Lang::Html => &[],
            Lang::Css => &[],
            Lang::Zig => &[],
        }
    }

    /// The tree-sitter grammar.
    pub fn grammar(self) -> Language {
        use tree_sitter_typescript as ts;
        match self {
            Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
            Lang::Python => tree_sitter_python::LANGUAGE.into(),
            Lang::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Lang::TypeScript => ts::LANGUAGE_TYPESCRIPT.into(),
            Lang::Tsx => ts::LANGUAGE_TSX.into(),
            Lang::Go => tree_sitter_go::LANGUAGE.into(),
            Lang::Dart => tree_sitter_dart_orchard::LANGUAGE.into(),
            Lang::C => tree_sitter_c::LANGUAGE.into(),
            Lang::Cpp => tree_sitter_cpp::LANGUAGE.into(),
            Lang::Java => tree_sitter_java::LANGUAGE.into(),
            Lang::Json => tree_sitter_json::LANGUAGE.into(),
            Lang::Bash => tree_sitter_bash::LANGUAGE.into(),
            Lang::Yaml => tree_sitter_yaml::LANGUAGE.into(),
            Lang::Toml => tree_sitter_toml_ng::LANGUAGE.into(),
            Lang::Html => tree_sitter_html::LANGUAGE.into(),
            Lang::Css => tree_sitter_css::LANGUAGE.into(),
            Lang::Zig => tree_sitter_zig::LANGUAGE.into(),
        }
    }

    /// The highlight query, as the parts it is assembled from, BASE FIRST.
    ///
    /// Several grammars ship only the delta over the language they extend:
    /// tree-sitter-typescript's query is a 35-line add-on to JavaScript's (no
    /// strings, comments, numbers or most keywords of its own), and
    /// tree-sitter-cpp's is an add-on to C's. Used alone, a `.ts` or `.cpp`
    /// file rendered its strings and comments as plain code — and because
    /// [`style_is_literal`] drives the bracket matcher and the occurrence
    /// highlighter, brackets inside those strings were matched as code. The
    /// order follows each grammar's own `tree-sitter.json`: in
    /// tree-sitter-highlight the LAST pattern matching a node wins, so the
    /// dialect's query comes after the query it refines.
    fn highlight_parts(self) -> &'static [&'static str] {
        use tree_sitter_javascript as js;
        use tree_sitter_typescript as ts;
        match self {
            Lang::Rust => &[tree_sitter_rust::HIGHLIGHTS_QUERY],
            Lang::Python => &[tree_sitter_python::HIGHLIGHTS_QUERY],
            Lang::JavaScript => &[js::HIGHLIGHT_QUERY, js::JSX_HIGHLIGHT_QUERY],
            Lang::TypeScript => &[js::HIGHLIGHT_QUERY, ts::HIGHLIGHTS_QUERY],
            Lang::Tsx => &[
                js::HIGHLIGHT_QUERY,
                js::JSX_HIGHLIGHT_QUERY,
                ts::HIGHLIGHTS_QUERY,
            ],
            Lang::Go => &[tree_sitter_go::HIGHLIGHTS_QUERY],
            Lang::Dart => &[tree_sitter_dart_orchard::HIGHLIGHTS_QUERY],
            Lang::C => &[tree_sitter_c::HIGHLIGHT_QUERY, C_LITERAL_OVERRIDES],
            Lang::Cpp => &[
                tree_sitter_c::HIGHLIGHT_QUERY,
                tree_sitter_cpp::HIGHLIGHT_QUERY,
                C_LITERAL_OVERRIDES,
            ],
            Lang::Java => &[tree_sitter_java::HIGHLIGHTS_QUERY],
            Lang::Json => &[tree_sitter_json::HIGHLIGHTS_QUERY],
            Lang::Bash => &[tree_sitter_bash::HIGHLIGHT_QUERY],
            Lang::Yaml => &[tree_sitter_yaml::HIGHLIGHTS_QUERY],
            Lang::Toml => &[tree_sitter_toml_ng::HIGHLIGHTS_QUERY],
            Lang::Html => &[tree_sitter_html::HIGHLIGHTS_QUERY],
            Lang::Css => &[tree_sitter_css::HIGHLIGHTS_QUERY],
            Lang::Zig => &[tree_sitter_zig::HIGHLIGHTS_QUERY],
        }
    }

    /// The outline (tags) query, as the parts it is assembled from; empty for
    /// a language without an outline.
    fn tags_parts(self) -> &'static [&'static str] {
        match self {
            Lang::Rust => &[RUST_TAGS],
            Lang::Python => &[tree_sitter_python::TAGS_QUERY],
            Lang::JavaScript => &[tree_sitter_javascript::TAGS_QUERY, JS_EXTRA_TAGS],
            // The crate's bundled TAGS_QUERY only captures declaration-file
            // constructs (function_signature, method_signature, interface, …),
            // so real .ts source (class/function/method/type) yields almost no
            // symbols. Use a source-oriented tags query instead.
            Lang::TypeScript | Lang::Tsx => &[TS_TAGS],
            Lang::Go => &[tree_sitter_go::TAGS_QUERY],
            Lang::Dart => &[tree_sitter_dart_orchard::TAGS_QUERY],
            Lang::C => &[tree_sitter_c::TAGS_QUERY],
            Lang::Cpp => &[tree_sitter_cpp::TAGS_QUERY],
            Lang::Java => &[tree_sitter_java::TAGS_QUERY, JAVA_EXTRA_TAGS],
            Lang::Json
            | Lang::Bash
            | Lang::Yaml
            | Lang::Toml
            | Lang::Html
            | Lang::Css
            | Lang::Zig => &[],
        }
    }

    fn family(self) -> Family {
        match self {
            Lang::JavaScript | Lang::TypeScript | Lang::Tsx => Family::Ecma,
            Lang::C | Lang::Cpp => Family::CLike,
            Lang::Rust
            | Lang::Python
            | Lang::Go
            | Lang::Dart
            | Lang::Java
            | Lang::Json
            | Lang::Bash
            | Lang::Yaml
            | Lang::Toml
            | Lang::Html
            | Lang::Css
            | Lang::Zig => Family::Alone(self),
        }
    }

    /// Whether a bare call written in `self` can name a definition written in
    /// `other`: the same language, or two that link into one program (a `.tsx`
    /// component calling a `.ts` helper, C++ calling a C function).
    pub fn shares_namespace_with(self, other: Lang) -> bool {
        self.family() == other.family()
    }

    /// The full outline query text, or `None` for a language without one.
    fn tags_text(self) -> Option<&'static str> {
        static TEXT: [OnceLock<String>; LANG_COUNT] = [const { OnceLock::new() }; LANG_COUNT];
        let parts = self.tags_parts();
        if parts.is_empty() {
            return None;
        }
        Some(TEXT[self as usize].get_or_init(|| parts.concat()).as_str())
    }
}

/// C's highlight query tags a character literal `@number`, which put the
/// bracket in `'{'` into the bracket matcher as code. Appended last so it wins
/// (see [`Lang::highlight_parts`]).
const C_LITERAL_OVERRIDES: &str = "\n(char_literal) @string\n";

/// Source-oriented tags query for TypeScript/TSX — captures the symbols in real
/// `.ts` source (classes, functions, methods, type aliases, enums, interfaces,
/// arrow-function consts), which the crate's declaration-focused query misses.
const TS_TAGS: &str = r#"
(class_declaration name: (type_identifier) @name) @definition.class
(abstract_class_declaration name: (type_identifier) @name) @definition.class
(function_declaration name: (identifier) @name) @definition.function
(generator_function_declaration name: (identifier) @name) @definition.function
(function_signature name: (identifier) @name) @definition.function
(method_definition name: (property_identifier) @name) @definition.method
(method_signature name: (property_identifier) @name) @definition.method
(abstract_method_signature name: (property_identifier) @name) @definition.method
(interface_declaration name: (type_identifier) @name) @definition.interface
(type_alias_declaration name: (type_identifier) @name) @definition.type
(enum_declaration name: (identifier) @name) @definition.enum
(lexical_declaration
  (variable_declarator
    name: (identifier) @name
    value: [(arrow_function) (function_expression)])) @definition.function
(variable_declaration
  (variable_declarator
    name: (identifier) @name
    value: [(arrow_function) (function_expression)])) @definition.function
;; The same function-valued bindings the JavaScript tags query names: an
;; assignment (`a.b = function () {}`, `Foo.prototype.bar = () => {}`) and an
;; object-literal entry (`{ onClick: () => {} }`). Without them the call graph
;; had no node to attribute the calls in those bodies to, and dropped them.
(assignment_expression
  left: [
    (identifier) @name
    (member_expression property: (property_identifier) @name)
  ]
  right: [(arrow_function) (function_expression)]) @definition.function
(pair
  key: (property_identifier) @name
  value: [(arrow_function) (function_expression)]) @definition.function
;; Interface members and class fields, scoped to the interface/class body so
;; inline object-type properties (e.g. a `{a: number}` parameter annotation)
;; are not swept in. This matches the VS Code / Zed outline, and surfaces the
;; JSDoc that libraries put on function-typed members (chalk's `rgb`, `hex`…).
;; A field whose value is a function is reported as a method by the outline
;; (`outline::located_in`), which is what it is in all but syntax.
(interface_body
  (property_signature name: (property_identifier) @name) @definition.property)
(class_body
  (public_field_definition name: (property_identifier) @name) @definition.field)
"#;

/// Added to tree-sitter-javascript's own tags query: a class field holding a
/// function (`handler = () => {…}`), which that query leaves out entirely, so
/// the calls in its body had no function to belong to.
const JS_EXTRA_TAGS: &str = r#"
(field_definition
  property: (property_identifier) @name
  value: [(arrow_function) (function_expression)]) @definition.method
"#;

/// Added to tree-sitter-java's own tags query, which has no constructors: the
/// calls a constructor makes had no function to belong to, and `new Foo()` had
/// nothing to resolve to.
const JAVA_EXTRA_TAGS: &str = r#"
(constructor_declaration name: (identifier) @name) @definition.method
"#;

/// Source-oriented tags query for Rust. The crate's bundled query lumps every
/// ADT — struct, enum, union, and type alias — under `@definition.class`, so the
/// outline mislabels a Rust `enum` as "class". This mirrors the bundled query
/// (same patterns / order, so method-vs-function dedup is unchanged) but tags
/// each ADT and traits with their real kind.
const RUST_TAGS: &str = r#"
(struct_item name: (type_identifier) @name) @definition.struct
(enum_item name: (type_identifier) @name) @definition.enum
(union_item name: (type_identifier) @name) @definition.union
(type_item name: (type_identifier) @name) @definition.type
(declaration_list
    (function_item name: (identifier) @name) @definition.method)
(function_item name: (identifier) @name) @definition.function
(trait_item name: (type_identifier) @name) @definition.trait
(mod_item name: (identifier) @name) @definition.module
(macro_definition name: (identifier) @name) @definition.macro
(const_item name: (identifier) @name) @definition.constant
(static_item name: (identifier) @name) @definition.constant
"#;

/// Whether C-family `source` uses syntax that only C++ has.
///
/// The markers are shapes C cannot write: a namespace opener (`namespace` —
/// or `using namespace` — followed by a NAME or `{`), a `template <` clause, a
/// `class Name {`/`:`/`;` head, an access label (`public:`…) INSIDE a
/// struct/union/class body, a `::` outside a C23 `[[attribute]]`, and an
/// extension-less standard header (`<vector>`) or a C++ header include. The
/// keywords alone are not markers: C code may use `namespace`, `template` or
/// `class` as ordinary identifiers (`template = load();`, `namespace = ns;`),
/// and `public:` as a `goto` label in a function body. Only contrived C still
/// trips a marker — one of those words declared a typedef name and used as a
/// type (`class x;`), or a discarded comparison opening a line
/// (`template < n;`). `extern "C"` is NOT a marker — C headers carry it inside
/// `#ifdef __cplusplus` for C++ callers. Only the first 256 KiB are read, so a
/// huge generated header costs O(1).
fn looks_like_cpp(source: &str) -> bool {
    const SCAN: usize = 256 * 1024;
    let mut end = source.len().min(SCAN);
    while !source.is_char_boundary(end) {
        end -= 1;
    }
    let mut in_block_comment = false;
    // Per open `{`, whether it opened a struct/union/class body: the one place
    // an access label is C++ rather than a C `goto` label.
    let mut records: Vec<bool> = Vec::new();
    // The last non-empty code line, whose text heads a `{` that sits alone on
    // the next line (`struct S` / `{`).
    let mut prev = String::new();
    for raw in source[..end].lines() {
        let mut line = raw.trim();
        if in_block_comment {
            match line.find("*/") {
                Some(close) => {
                    in_block_comment = false;
                    line = line[close + 2..].trim();
                }
                None => continue,
            }
        }
        if let Some(open) = line.find("/*") {
            in_block_comment = !line[open..].contains("*/");
            line = line[..open].trim();
        }
        if let Some(open) = line.find("//") {
            line = line[..open].trim();
        }
        if line.is_empty() {
            continue;
        }
        // Read before the literals go: a quoted include's header name IS a
        // string literal, and `#include "x.hpp"` is C++ by that name alone.
        if includes_cpp_header(line) {
            return true;
        }
        let code = without_string_literals(line, &['"', '\'']);
        if is_cpp_line(&code) || access_label_in_record(&code, &prev, &mut records) {
            return true;
        }
        prev = code;
    }
    false
}

/// Keep `records` (see [`looks_like_cpp`]) current across `line`'s braces,
/// and say whether one of its statements starts with an access label while
/// the innermost open brace is a struct/union/class body. `prev` is the code
/// line before, which heads a `{` that opens this line.
fn access_label_in_record(line: &str, prev: &str, records: &mut Vec<bool>) -> bool {
    let mut rest = line;
    let mut first = true;
    loop {
        let cut = rest.find(['{', '}', ';']);
        let statement = rest[..cut.unwrap_or(rest.len())].trim();
        if records.last() == Some(&true) && starts_with_access_label(statement) {
            return true;
        }
        let Some(at) = cut else {
            return false;
        };
        match rest.as_bytes()[at] {
            b'{' => {
                let head = if statement.is_empty() && first {
                    prev
                } else {
                    statement
                };
                records.push(is_record_head(head));
            }
            b'}' => {
                records.pop();
            }
            _ => {}
        }
        rest = &rest[at + 1..];
        first = false;
    }
}

/// `public:`, `private:` or `protected:` (a space before the colon allowed),
/// but not a `public::` scope.
fn starts_with_access_label(statement: &str) -> bool {
    ["public", "private", "protected"].iter().any(|label| {
        statement.strip_prefix(label).is_some_and(|rest| {
            let rest = rest.trim_start();
            rest.starts_with(':') && !rest.starts_with("::")
        })
    })
}

/// Whether the text before a `{` opens a struct/union/class body — not a
/// function returning one (`struct s *make(void) {`) or an initializer
/// (`struct s v = {`).
fn is_record_head(head: &str) -> bool {
    let h = head.trim();
    let h = h
        .strip_prefix("typedef")
        .filter(|r| r.starts_with(char::is_whitespace))
        .map_or(h, str::trim_start);
    let keyword = h
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .next()
        .unwrap_or("");
    matches!(keyword, "struct" | "union" | "class") && !h.contains(['(', '='])
}

/// One comment-free, string-free line of [`looks_like_cpp`].
fn is_cpp_line(line: &str) -> bool {
    if opens_namespace(line) || opens_template(line) {
        return true;
    }
    if let Some(rest) = line.strip_prefix("class ") {
        let rest = rest.trim_start();
        let name_len = rest
            .find(|c: char| !(c.is_alphanumeric() || c == '_'))
            .unwrap_or(rest.len());
        let after = rest[name_len..].trim_start();
        if name_len > 0
            && (after.is_empty()
                || after.starts_with(['{', ':', ';'])
                || after.starts_with("final"))
        {
            return true;
        }
    }
    if let Some(at) = line.find("::")
        && !line[..at].contains("[[")
    {
        return true;
    }
    false
}

/// Whether a comment-free line (string literals KEPT: a quoted header name
/// is one) includes a C++ header: one with a C++ extension, in either form
/// (`#include "x.hpp"`, `#include <x.hh>`), or an extension-less standard
/// header (`<vector>`). The directive may be spaced (`#  include`).
fn includes_cpp_header(line: &str) -> bool {
    let Some(header) = line
        .strip_prefix('#')
        .and_then(|directive| directive.trim_start().strip_prefix("include"))
    else {
        return false;
    };
    let header = header.trim();
    if !header.starts_with(['<', '"']) {
        return false; // `#include_next`, `#includes`, a macro-named header
    }
    let name = header.trim_matches(|c| matches!(c, '<' | '>' | '"'));
    let cpp_ext = [".hpp", ".hh", ".hxx", ".ipp", ".h++"]
        .iter()
        .any(|e| name.ends_with(e));
    cpp_ext || (header.starts_with('<') && !name.contains('.'))
}

/// `line` without a leading `word` that is followed by whitespace.
fn after_word<'a>(line: &'a str, word: &str) -> Option<&'a str> {
    line.strip_prefix(word)
        .filter(|rest| rest.starts_with(char::is_whitespace))
        .map(str::trim_start)
}

/// A namespace opener, alias or using-directive: `namespace` followed by a
/// NAME (`namespace util {`, `namespace a::b {`, `namespace fs = …;`) or by
/// `{` (an anonymous namespace), optionally after `inline` or `using`. C can
/// use the word as an identifier (`namespace = ns;`, `namespace(x)`), which
/// never puts a name or a `{` right after it.
fn opens_namespace(line: &str) -> bool {
    let line = after_word(line, "inline")
        .or_else(|| after_word(line, "using"))
        .unwrap_or(line);
    let Some(rest) = line.strip_prefix("namespace") else {
        return false;
    };
    let after = rest.trim_start();
    let named =
        after.len() < rest.len() && after.starts_with(|c: char| c.is_alphabetic() || c == '_');
    named || after.starts_with('{')
}

/// A `template <…>` clause (`template<` too). C can use `template` as an
/// identifier (`template = load();`), which is never followed by a `<` that
/// opens a line.
fn opens_template(line: &str) -> bool {
    line.strip_prefix("template")
        .is_some_and(|rest| rest.trim_start().starts_with('<'))
}

/// `line` with the contents of every literal delimited by one of `quotes`
/// removed (the delimiters stay, so the literal still separates what is on
/// either side of it), with `\` escaping the next character inside one. For
/// the text scans that must not read inside a literal: a `"a::b"` in a C
/// string is not C++ scope syntax, and a `"test-utils"` feature name in a Rust
/// attribute is not a test marker. An unterminated literal runs to the end.
pub(crate) fn without_string_literals(line: &str, quotes: &[char]) -> String {
    let mut out = String::with_capacity(line.len());
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for c in line.chars() {
        match quote {
            Some(q) => {
                if escaped {
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == q {
                    quote = None;
                    out.push(c);
                }
            }
            None => {
                if quotes.contains(&c) {
                    quote = Some(c);
                }
                out.push(c);
            }
        }
    }
    out
}

/// Detect the language key from a file path (by extension).
pub fn detect(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Lang::ALL
        .iter()
        .find(|l| l.extensions().contains(&ext.as_str()))
        .map(|l| l.key())
}

/// Language key for a markdown fence tag (```rust, ```py, …), accepting the
/// common aliases LLMs emit. `None` for unknown/absent tags → plain rendering.
pub fn lang_for_fence(tag: &str) -> Option<&'static str> {
    let tag = tag.trim().to_ascii_lowercase();
    Lang::ALL
        .iter()
        .find(|l| l.key() == tag || l.fence_aliases().contains(&tag.as_str()))
        .map(|l| l.key())
}

/// Resolve a runtime language string (e.g. notebook kernel metadata) to the
/// `'static` key `highlight_lines` accepts, when it names a supported language.
pub fn static_key(name: &str) -> Option<&'static str> {
    Lang::from_key(name).map(Lang::key)
}

/// Human-readable language name for a key.
pub fn lang_name(key: &str) -> Option<&'static str> {
    Lang::from_key(key).map(Lang::name)
}

/// Language + tags query used by the outline extractor, or `None` when the
/// language has no outline.
pub fn tags_for(key: &str) -> Option<(Language, &'static str)> {
    let lang = Lang::from_key(key)?;
    Some((lang.grammar(), lang.tags_text()?))
}

/// Bare tree-sitter grammar for a language key, for callers that run their own
/// queries (e.g. the import extractor).
pub fn language_for(key: &str) -> Option<Language> {
    Lang::from_key(key).map(Lang::grammar)
}

/// Parse `source` with `lang`'s grammar. Blocking; run off the UI thread.
/// Callers that need several facts about one file parse it ONCE with this and
/// hand the tree to each extractor.
pub fn parse(source: &str, lang: Lang) -> Option<Tree> {
    PARSES.with(|n| n.set(n.get() + 1));
    let mut parser = Parser::new();
    parser.set_language(&lang.grammar()).ok()?;
    parser.parse(source, None)
}

thread_local! {
    /// Parses [`parse`] has run on this thread (see [`parses_on_this_thread`]).
    static PARSES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// How many times [`parse`] has run on the calling thread: a cost meter for
/// the tests that pin how often a pass parses each file. The project call
/// graph used to parse every file again right after the symbol index had; a
/// count, unlike a timing, cannot pass by luck on a fast machine or fail on a
/// loaded one. Per thread, so tests running side by side never see each
/// other's parses. Public for the other crates' tests (a `cfg(test)` item is
/// invisible to them); nothing in a production path reads it, and keeping it
/// costs one thread-local add per parse.
#[doc(hidden)]
pub fn parses_on_this_thread() -> u64 {
    PARSES.with(std::cell::Cell::get)
}

/// Why a per-language query is unavailable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryError {
    /// The language has no such query (JSON has no outline, …).
    Unsupported,
    /// The query failed to compile against the grammar. A test compiles every
    /// shipped query, so this means a grammar bump broke one — reported once
    /// on stderr when first hit, never retried.
    Invalid(String),
}

impl std::fmt::Display for QueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QueryError::Unsupported => f.write_str("not supported for this language"),
            QueryError::Invalid(e) => write!(f, "query does not compile: {e}"),
        }
    }
}

/// A per-language cache of compiled queries. Each query compiles at most once
/// per process — they used to be recompiled on every call, i.e. for every
/// file of every scan — and a failure is remembered, reported once, and
/// returned as an error rather than silently producing an empty result.
pub(crate) struct QueryCache {
    what: &'static str,
    slots: [OnceLock<Result<Query, QueryError>>; LANG_COUNT],
}

impl QueryCache {
    pub(crate) const fn new(what: &'static str) -> Self {
        QueryCache {
            what,
            slots: [const { OnceLock::new() }; LANG_COUNT],
        }
    }

    /// The compiled query for `lang`, compiling `text` on first use.
    /// `text` returns `None` when the language has no such query.
    pub(crate) fn get(
        &'static self,
        lang: Lang,
        text: impl FnOnce() -> Option<&'static str>,
    ) -> Result<&'static Query, QueryError> {
        self.slots[lang as usize]
            .get_or_init(|| {
                let text = text().ok_or(QueryError::Unsupported)?;
                Query::new(&lang.grammar(), text).map_err(|e| {
                    eprintln!(
                        "clew: the {} {} query does not compile: {e}",
                        lang.name(),
                        self.what
                    );
                    QueryError::Invalid(e.to_string())
                })
            })
            .as_ref()
            .map_err(Clone::clone)
    }
}

/// The compiled outline (tags) query for `lang`.
pub fn tags_query(lang: Lang) -> Result<&'static Query, QueryError> {
    static TAGS: QueryCache = QueryCache::new("outline");
    TAGS.get(lang, || lang.tags_text())
}

/// The highlight configuration for `lang`, built at most once per process;
/// a failure is cached (and reported once) instead of being retried on every
/// file open.
fn config_for(lang: Lang) -> Option<&'static HighlightConfiguration> {
    static CONFIGS: [OnceLock<Result<HighlightConfiguration, String>>; LANG_COUNT] =
        [const { OnceLock::new() }; LANG_COUNT];
    CONFIGS[lang as usize]
        .get_or_init(|| {
            let query = lang.highlight_parts().concat();
            HighlightConfiguration::new(lang.grammar(), lang.name(), &query, "", "")
                .map(|mut config| {
                    config.configure(HIGHLIGHT_NAMES);
                    config
                })
                .map_err(|e| {
                    eprintln!(
                        "clew: the {} highlight query does not compile: {e}",
                        lang.name()
                    );
                    e.to_string()
                })
        })
        .as_ref()
        .ok()
}

/// Why `key` cannot be highlighted, or `None` when it can. Surfaces a broken
/// highlight query as an error rather than as silently uncolored text.
pub fn highlight_error(key: &str) -> Option<String> {
    let Some(lang) = Lang::from_key(key) else {
        return Some(format!("unknown language {key:?}"));
    };
    match config_for(lang) {
        Some(_) => None,
        None => Some(format!(
            "the {} highlight query does not compile",
            lang.name()
        )),
    }
}

/// Normalize a text fragment for display: strip CR, expand tabs.
fn clean(s: &str) -> String {
    s.replace('\r', "").replace('\t', "    ")
}

/// Split a source file into unstyled lines.
pub fn plain_lines(source: &str) -> Vec<HlLine> {
    source
        .lines()
        .map(|l| HlLine {
            spans: vec![(clean(l), None)],
        })
        .collect()
}

/// Highlight a source file into per-line styled spans.
/// Falls back to plain lines when the language is unknown or parsing fails.
/// A `"c"` source written in C++ is highlighted as C++ (see
/// [`Lang::for_source`]).
pub fn highlight_lines(source: &str, lang_key: Option<&'static str>) -> Vec<HlLine> {
    lang_key
        .and_then(|key| Lang::for_source(key, source))
        .and_then(config_for)
        .and_then(|config| highlight_with(source, config))
        .unwrap_or_else(|| plain_lines(source))
}

fn highlight_with(source: &str, config: &HighlightConfiguration) -> Option<Vec<HlLine>> {
    let mut highlighter = Highlighter::new();
    let events = highlighter
        .highlight(config, source.as_bytes(), None, |_| None)
        .ok()?;

    let mut lines: Vec<HlLine> = Vec::new();
    let mut current = HlLine::default();
    let mut stack: Vec<u8> = Vec::new();
    for event in events {
        match event.ok()? {
            HighlightEvent::HighlightStart(h) => stack.push(h.0 as u8),
            HighlightEvent::HighlightEnd => {
                stack.pop();
            }
            HighlightEvent::Source { start, end } => {
                let style = stack.last().copied();
                let mut rest = source.get(start..end)?;
                while let Some(pos) = rest.find('\n') {
                    let (head, tail) = rest.split_at(pos);
                    if !head.is_empty() {
                        current.spans.push((clean(head), style));
                    }
                    lines.push(std::mem::take(&mut current));
                    rest = &tail[1..];
                }
                if !rest.is_empty() {
                    current.spans.push((clean(rest), style));
                }
            }
        }
    }
    if !current.spans.is_empty() {
        lines.push(current);
    }
    Some(lines)
}

/// Whether a style index denotes a string, comment or escape scope — i.e. text
/// where brackets and identifiers should not be treated as code. Used by the
/// bracket matcher and occurrence highlighter to ignore literal text.
pub fn style_is_literal(idx: u8) -> bool {
    HIGHLIGHT_NAMES
        .get(idx as usize)
        .is_some_and(|name| LITERAL_STYLES.contains(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUST_SRC: &str = "/// Doc comment.\npub fn add(a: i32, b: i32) -> i32 {\n\tlet s = \"x\\n\";\n    a + b\n}\n\nstruct Point {\n    x: f64,\n}\n";

    #[test]
    fn highlighted_line_count_matches_plain() {
        let plain = plain_lines(RUST_SRC);
        let highlighted = highlight_lines(RUST_SRC, Some("rust"));
        assert_eq!(plain.len(), highlighted.len());
        assert_eq!(plain.len(), RUST_SRC.lines().count());
    }

    #[test]
    fn highlighting_produces_styled_spans() {
        let lines = highlight_lines(RUST_SRC, Some("rust"));
        let styled = lines
            .iter()
            .flat_map(|l| &l.spans)
            .filter(|(_, style)| style.is_some())
            .count();
        assert!(styled > 3, "expected styled spans, got {styled}");
    }

    #[test]
    fn line_text_is_preserved() {
        let lines = highlight_lines(RUST_SRC, Some("rust"));
        let reassembled: String = lines[3].spans.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(reassembled, "    a + b");
    }

    #[test]
    fn tabs_and_crlf_are_cleaned() {
        let lines = plain_lines("a\tb\r\nc\r\n");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].spans[0].0, "a    b");
        assert_eq!(lines[1].spans[0].0, "c");
    }

    #[test]
    fn unknown_language_falls_back_to_plain() {
        let lines = highlight_lines("hello\nworld", None);
        assert_eq!(lines.len(), 2);
        assert!(
            lines
                .iter()
                .all(|l| l.spans.iter().all(|(_, s)| s.is_none()))
        );
    }

    #[test]
    fn detect_by_extension() {
        assert_eq!(detect(Path::new("a/b.rs")), Some("rust"));
        assert_eq!(detect(Path::new("x.tsx")), Some("tsx"));
        assert_eq!(detect(Path::new("x.zig")), Some("zig"));
        assert_eq!(detect(Path::new("x.H")), Some("c"));
        assert_eq!(detect(Path::new("x.unknown")), None);
    }

    #[test]
    fn zig_highlights() {
        let src = "const std = @import(\"std\");\npub fn main() void {}\n";
        let lines = highlight_lines(src, Some("zig"));
        assert_eq!(lines.len(), 2);
        let styled = lines
            .iter()
            .flat_map(|l| &l.spans)
            .filter(|(_, s)| s.is_some())
            .count();
        assert!(styled > 2, "expected Zig highlighting, got {styled}");
        assert_eq!(lang_name("zig"), Some("Zig"));
    }

    /// The style of the span holding `needle` (every span is searched, so the
    /// needle must be unique to the construct under test).
    fn style_of(lines: &[HlLine], needle: &str) -> Option<Option<u8>> {
        lines
            .iter()
            .flat_map(|l| &l.spans)
            .find(|(t, _)| t.contains(needle))
            .map(|(_, s)| *s)
    }

    /// Every language that has strings and comments must style BOTH as
    /// literal text — and the bracket inside each must read as literal, which
    /// is what the bracket matcher and occurrence highlighter ask. TypeScript,
    /// TSX and C++ used to fail this outright: their grammar crates ship only
    /// a delta over JavaScript / C, and that delta was used alone.
    #[test]
    fn strings_and_comments_are_literal_in_every_language() {
        // (key, source, string needle, comment needle)
        let cases: &[(&str, &str, Option<&str>, Option<&str>)] = &[
            (
                "rust",
                "fn f() { let s = \"s{\"; } // c{\n",
                Some("s{"),
                Some("// c{"),
            ),
            ("python", "s = \"s{\"  # c{\n", Some("s{"), Some("# c{")),
            (
                "javascript",
                "const s = \"s{\"; // c{\n",
                Some("s{"),
                Some("// c{"),
            ),
            (
                "typescript",
                "const s: string = \"s{\"; // c{\n",
                Some("s{"),
                Some("// c{"),
            ),
            (
                "tsx",
                "const s = \"s{\"; // c{\nconst e = <div/>;\n",
                Some("s{"),
                Some("// c{"),
            ),
            (
                "go",
                "package p\nvar s = \"s{\" // c{\n",
                Some("s{"),
                Some("// c{"),
            ),
            ("dart", "var s = 's{'; // c{\n", Some("s{"), Some("// c{")),
            (
                "c",
                "char *s = \"s{\"; /* c{ */\nchar b = '{';\n",
                Some("s{"),
                Some("/* c{ */"),
            ),
            (
                "cpp",
                "auto s = \"s{\"; // c{\nchar b = '{';\n",
                Some("s{"),
                Some("// c{"),
            ),
            (
                "java",
                "class A { String s = \"s{\"; // c{\n}\n",
                Some("s{"),
                Some("// c{"),
            ),
            ("json", "{\"k\": \"s{\"}\n", Some("s{"), None),
            ("bash", "s=\"s{\" # c{\n", Some("s{"), Some("# c{")),
            ("yaml", "k: \"s{\" # c{\n", Some("s{"), Some("# c{")),
            ("toml", "k = \"s{\" # c{\n", Some("s{"), Some("# c{")),
            (
                "html",
                "<p title=\"s{\"></p><!-- c{ -->\n",
                Some("s{"),
                Some("<!-- c{ -->"),
            ),
            (
                "css",
                "a { content: \"s{\"; } /* c{ */\n",
                Some("s{"),
                Some("/* c{ */"),
            ),
            (
                "zig",
                "const s = \"s{\"; // c{\n",
                Some("s{"),
                Some("// c{"),
            ),
        ];
        assert_eq!(cases.len(), Lang::ALL.len(), "a language is missing here");
        for (key, src, string, comment) in cases {
            let lines = highlight_lines(src, static_key(key));
            for needle in [string, comment].into_iter().flatten() {
                let style = style_of(&lines, needle)
                    .unwrap_or_else(|| panic!("{key}: no span holds {needle:?} in {lines:?}"));
                assert!(
                    style.is_some_and(style_is_literal),
                    "{key}: {needle:?} is not literal (style {style:?}) in {lines:?}"
                );
            }
        }
        // C/C++ character literals are text too: the C query calls them numbers.
        for key in ["c", "cpp"] {
            let lines = highlight_lines("char b = '{';\n", static_key(key));
            let style = style_of(&lines, "'{'").flatten();
            assert!(style.is_some_and(style_is_literal), "{key}: {lines:?}");
        }
    }

    /// The composed queries keep each dialect's own captures: TypeScript
    /// keywords the JavaScript query does not know, and C++ keywords the C
    /// query does not know.
    #[test]
    fn dialect_queries_still_apply_on_top_of_their_base() {
        let keyword = HIGHLIGHT_NAMES
            .iter()
            .position(|n| *n == "keyword")
            .unwrap() as u8;
        let ts = highlight_lines(
            "interface A { x: number }\nconst y = 1;\n",
            Some("typescript"),
        );
        assert_eq!(style_of(&ts, "interface"), Some(Some(keyword)), "{ts:?}");
        assert_eq!(style_of(&ts, "const"), Some(Some(keyword)), "{ts:?}");
        let cpp = highlight_lines(
            "namespace n { class A {}; }\nint main() { return 0; }\n",
            Some("cpp"),
        );
        assert_eq!(style_of(&cpp, "namespace"), Some(Some(keyword)), "{cpp:?}");
        assert_eq!(style_of(&cpp, "return"), Some(Some(keyword)), "{cpp:?}");
    }

    /// Every shipped query compiles against its grammar, so none of them can
    /// fail silently at run time (which read as "this file has no symbols" or
    /// as uncolored text).
    #[test]
    fn every_shipped_query_compiles() {
        for &lang in Lang::ALL {
            assert_eq!(highlight_error(lang.key()), None, "{lang:?} highlights");
            match tags_query(lang) {
                Ok(_) => assert!(!lang.tags_parts().is_empty()),
                Err(QueryError::Unsupported) => assert!(lang.tags_parts().is_empty()),
                Err(e) => panic!("{lang:?} tags: {e}"),
            }
        }
    }

    /// The table is the single source of truth: keys round-trip, no extension
    /// or fence alias is claimed by two languages, and the enum order is the
    /// `ALL` order the per-language caches index by.
    #[test]
    fn the_language_table_is_consistent() {
        for (i, &lang) in Lang::ALL.iter().enumerate() {
            assert_eq!(lang as usize, i);
            assert_eq!(Lang::from_key(lang.key()), Some(lang));
            assert_eq!(static_key(lang.key()), Some(lang.key()));
            assert_eq!(lang_for_fence(lang.key()), Some(lang.key()));
            assert!(!lang.extensions().is_empty(), "{lang:?} has no extension");
            for ext in lang.extensions() {
                assert_eq!(
                    detect(Path::new(&format!("f.{ext}"))),
                    Some(lang.key()),
                    "extension {ext} is claimed twice"
                );
            }
            for alias in lang.fence_aliases() {
                assert_eq!(lang_for_fence(alias), Some(lang.key()), "alias {alias}");
            }
        }
        assert_eq!(lang_for_fence(" C++ "), Some("cpp"));
        assert_eq!(lang_for_fence("console"), Some("bash"));
        assert_eq!(lang_for_fence("brainfuck"), None);
        assert_eq!(static_key("klingon"), None);
    }

    /// The key a file at `path` holding `content` is parsed as.
    fn parsed_as(path: &str, content: &str) -> Option<&'static str> {
        Lang::for_source(detect(Path::new(path))?, content).map(Lang::key)
    }

    /// `.h` is shared by C and C++; the content decides. Only syntax that is
    /// invalid C counts, so a C header — `extern "C"` guard and all — stays C.
    #[test]
    fn a_cpp_header_named_dot_h_is_read_as_cpp() {
        let cpp =
            "#pragma once\nnamespace util {\nclass Buffer {\npublic:\n  int size() const;\n};\n}\n";
        assert_eq!(parsed_as("buf.h", cpp), Some("cpp"));
        let template = "template <typename T>\nT max(T a, T b);\n";
        assert_eq!(parsed_as("m.h", template), Some("cpp"));
        let std_header = "#include <vector>\nint f(void);\n";
        assert_eq!(parsed_as("v.h", std_header), Some("cpp"));
        // A quoted include names its header in a string literal, which the
        // other markers are read without: `x.hpp` still decides it.
        for include in [
            "#include \"widget.hpp\"",
            "#  include \"detail/impl.hh\" // local",
            "#include <boost/any.hpp>",
        ] {
            let text = format!("{include}\nint f(void);\n");
            assert_eq!(parsed_as("w.h", &text), Some("cpp"), "{text}");
        }
        for include in [
            "#include \"widget.h\"",
            "#include_next <stdio.h>",
            "#include HEADER",
        ] {
            let text = format!("{include}\nint f(void);\n");
            assert_eq!(parsed_as("w.h", &text), Some("c"), "{text}");
        }

        let c = "#ifndef A_H\n#define A_H\n#ifdef __cplusplus\nextern \"C\" {\n#endif\n\
                 /* a class of problems: see foo::bar in the docs */\n\
                 int class_count(void); // namespace-free\n\
                 static const char *s = \"std::string\";\n\
                 [[gnu::unused]] static int x;\n#include <stdio.h>\n#endif\n";
        assert_eq!(parsed_as("a.h", c), Some("c"), "{c}");
        // Other languages are never refined.
        assert_eq!(parsed_as("a.rs", cpp), Some("rust"));

        // The refinement reaches the highlighter: `class` is a C++ keyword.
        let keyword = HIGHLIGHT_NAMES
            .iter()
            .position(|n| *n == "keyword")
            .unwrap() as u8;
        let lines = highlight_lines(cpp, detect(Path::new("buf.h")));
        assert_eq!(style_of(&lines, "class"), Some(Some(keyword)), "{lines:?}");
    }

    /// Valid C that uses the C++ keywords as what they are in C — ordinary
    /// identifiers, and a `goto` label — is still C, in a `.c` file and a
    /// `.h` alike. Each of these used to read as C++ (and be parsed with the
    /// C++ grammar) on the keyword alone.
    #[test]
    fn c_that_uses_cpp_words_as_identifiers_stays_c() {
        let c = "\
struct node { int value; };
static int template, namespace;
int load(void);
int use(int ns) {
    template = load();
    namespace = ns;
    if (template < namespace) goto public;
    return namespace;
public:
    return template;
}
";
        assert_eq!(parsed_as("a.c", c), Some("c"));
        assert_eq!(parsed_as("a.h", c), Some("c"));
        // A struct's initializer and a function returning a struct are not
        // record bodies; a label in them is still a label.
        let c2 = "struct s make(void) {\nprotected:\n  return (struct s){0};\n}\n\
                  struct s v = {\npublic: 0 };\n";
        assert_eq!(parsed_as("b.c", c2), Some("c"));
    }

    /// The C++ shapes the tightened markers must still see.
    #[test]
    fn cpp_shapes_are_still_detected() {
        for (why, src) in [
            ("named namespace", "namespace util {\nint f();\n}\n"),
            ("anonymous namespace", "namespace {\nint helper();\n}\n"),
            ("nested namespace", "namespace a::b {\n}\n"),
            ("namespace alias", "namespace fs = files;\n"),
            ("inline namespace", "inline namespace v1 {\n}\n"),
            ("using-directive", "using namespace util;\n"),
            ("template", "template<typename T>\nT id(T x);\n"),
            ("template, spaced", "template <class T> struct Box;\n"),
            (
                "access label in a struct",
                "struct Buffer {\n  public:\n    int size;\n};\n",
            ),
            (
                "access label, brace on its own line",
                "struct Buffer\n{\nprivate:\n  int size;\n};\n",
            ),
            (
                "access label, one line",
                "typedef struct B { protected: int x; } B;\n",
            ),
            (
                "class head with an export macro",
                "class API Widget : Base\n{\npublic:\n};\n",
            ),
        ] {
            assert_eq!(parsed_as("x.h", src), Some("cpp"), "{why}: {src}");
        }
    }

    #[test]
    fn only_strings_comments_and_escapes_are_literal() {
        for name in LITERAL_STYLES {
            assert!(HIGHLIGHT_NAMES.contains(name), "{name} is not a style");
        }
        for (i, name) in HIGHLIGHT_NAMES.iter().enumerate() {
            let literal =
                name.starts_with("comment") || name.starts_with("string") || *name == "escape";
            assert_eq!(style_is_literal(i as u8), literal, "{name}");
        }
        assert!(!style_is_literal(u8::MAX));
    }

    #[test]
    fn shared_namespaces_follow_the_runtime() {
        assert!(Lang::Tsx.shares_namespace_with(Lang::TypeScript));
        assert!(Lang::JavaScript.shares_namespace_with(Lang::Tsx));
        assert!(Lang::Cpp.shares_namespace_with(Lang::C));
        assert!(Lang::Go.shares_namespace_with(Lang::Go));
        assert!(!Lang::Go.shares_namespace_with(Lang::JavaScript));
        assert!(!Lang::Rust.shares_namespace_with(Lang::C));
    }
}
