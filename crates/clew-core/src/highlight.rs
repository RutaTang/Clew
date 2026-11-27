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
    // Appended, so the indices above — the style ids on the wire — stay.
    "function.definition",
    "function.macro",
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
    Sql => "sql",
    Lean => "lean",
    Php => "php",
    Ruby => "ruby",
    CSharp => "csharp",
    Erlang => "erlang",
    Haskell => "haskell",
    Vue => "vue",
    Svelte => "svelte",
    Astro => "astro",
    Scss => "scss",
    Less => "less",
    Kotlin => "kotlin",
    Scala => "scala",
    Elixir => "elixir",
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
    /// Java, Kotlin and Scala compile to one JVM program, each calling the
    /// others' classes and methods.
    Jvm,
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
    ///
    /// Likewise a `"json"` source written as YAML — a tool's configuration
    /// file that takes either (`.eslintrc`, `.prettierrc`) — is YAML.
    pub fn for_source(key: &str, source: &str) -> Option<Lang> {
        let lang = Lang::from_key(key)?;
        Some(match lang {
            Lang::C if looks_like_cpp(source) => Lang::Cpp,
            Lang::Json if looks_like_yaml(source) => Lang::Yaml,
            lang => lang,
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
            Lang::Sql => "SQL",
            Lang::Lean => "Lean",
            Lang::Php => "PHP",
            Lang::Ruby => "Ruby",
            Lang::CSharp => "C#",
            Lang::Erlang => "Erlang",
            Lang::Haskell => "Haskell",
            Lang::Vue => "Vue",
            Lang::Svelte => "Svelte",
            Lang::Astro => "Astro",
            Lang::Scss => "SCSS",
            Lang::Less => "Less",
            Lang::Kotlin => "Kotlin",
            Lang::Scala => "Scala",
            Lang::Elixir => "Elixir",
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
            Lang::Json => &["json", "jsonc", "json5"],
            Lang::Bash => &["sh", "bash", "zsh"],
            Lang::Yaml => &["yml", "yaml"],
            Lang::Toml => &["toml"],
            Lang::Html => &["html", "htm"],
            Lang::Css => &["css"],
            Lang::Zig => &["zig"],
            Lang::Sql => &["sql", "pgsql"],
            Lang::Lean => &["lean"],
            Lang::Php => &["php", "phtml", "php8", "php7"],
            Lang::Ruby => &["rb", "rake", "gemspec", "ru", "rbi"],
            Lang::CSharp => &["cs", "csx"],
            Lang::Erlang => &["erl", "hrl", "escript", "app.src"],
            Lang::Haskell => &["hs", "hs-boot"],
            Lang::Vue => &["vue"],
            Lang::Svelte => &["svelte"],
            Lang::Astro => &["astro"],
            Lang::Scss => &["scss"],
            Lang::Less => &["less"],
            Lang::Kotlin => &["kt", "kts"],
            // sbt's build definitions are Scala, and Mill's.
            Lang::Scala => &["scala", "sc", "sbt", "mill"],
            Lang::Elixir => &["ex", "exs"],
        }
    }

    /// Whole file names (no extension to go by) that select this language.
    pub const fn file_names(self) -> &'static [&'static str] {
        match self {
            Lang::Ruby => &[
                "Gemfile",
                "Rakefile",
                "Guardfile",
                "Podfile",
                "Vagrantfile",
                "Fastfile",
                "Brewfile",
                "Capfile",
                "Dangerfile",
            ],
            Lang::Erlang => &["rebar.config", "sys.config"],
            // The JavaScript tools' configuration files without an
            // extension: JSON, or YAML for some (`.eslintrc`, `.prettierrc`,
            // `.stylelintrc`), which [`Lang::for_source`] tells by reading.
            Lang::Json => &[
                ".babelrc",
                ".swcrc",
                ".jshintrc",
                ".eslintrc",
                ".prettierrc",
                ".stylelintrc",
                ".postcssrc",
                ".lintstagedrc",
                ".nycrc",
                ".mocharc",
            ],
            _ => &[],
        }
    }

    /// Markdown fence tags (lowercase) that name this language besides its key
    /// — the aliases LLMs actually emit.
    const fn fence_aliases(self) -> &'static [&'static str] {
        match self {
            Lang::Rust => &["rs"],
            Lang::Python => &["py"],
            Lang::JavaScript => &["js", "jsx", "mjs", "cjs", "javascriptreact", "node"],
            Lang::TypeScript => &["ts", "mts", "cts"],
            Lang::Tsx => &["typescriptreact"],
            Lang::Go => &["golang"],
            Lang::Dart => &[],
            Lang::C => &["h"],
            Lang::Cpp => &["c++", "cxx"],
            Lang::Java => &[],
            Lang::Json => &["jsonc"],
            Lang::Bash => &["sh", "shell", "zsh", "console"],
            Lang::Yaml => &["yml"],
            Lang::Toml => &[],
            Lang::Html => &["htm", "xhtml"],
            Lang::Css => &["postcss"],
            Lang::Zig => &[],
            Lang::Sql => &[
                "postgresql",
                "postgres",
                "pgsql",
                "plpgsql",
                "mysql",
                "sqlite",
                "tsql",
                "plsql",
            ],
            Lang::Lean => &["lean4"],
            Lang::Php => &[],
            Lang::Ruby => &["rb"],
            Lang::CSharp => &["cs", "c#", "c-sharp"],
            Lang::Erlang => &["erl"],
            Lang::Haskell => &["hs"],
            Lang::Vue => &[],
            Lang::Svelte => &[],
            Lang::Astro => &[],
            Lang::Scss => &[],
            Lang::Less => &[],
            Lang::Kotlin => &["kt", "kts"],
            Lang::Scala => &["sc"],
            Lang::Elixir => &["ex", "exs", "iex"],
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
            Lang::Sql => tree_sitter_sequel::LANGUAGE.into(),
            Lang::Lean => tree_sitter_lean4::LANGUAGE.into(),
            Lang::Php => tree_sitter_php::LANGUAGE_PHP.into(),
            Lang::Ruby => tree_sitter_ruby::LANGUAGE.into(),
            Lang::CSharp => tree_sitter_c_sharp::LANGUAGE.into(),
            Lang::Erlang => tree_sitter_erlang::LANGUAGE.into(),
            Lang::Haskell => tree_sitter_haskell::LANGUAGE.into(),
            Lang::Vue => tree_sitter_vue_next::LANGUAGE.into(),
            Lang::Svelte => tree_sitter_svelte_ng::LANGUAGE.into(),
            // Astro's markup is HTML's; its frontmatter is highlighted as the
            // TypeScript it is (see `highlight_lines`).
            Lang::Astro => tree_sitter_html::LANGUAGE.into(),
            Lang::Scss => tree_sitter_scss::language(),
            Lang::Less => tree_sitter_less::language(),
            Lang::Kotlin => tree_sitter_kotlin_ng::LANGUAGE.into(),
            Lang::Scala => tree_sitter_scala::LANGUAGE.into(),
            Lang::Elixir => tree_sitter_elixir::LANGUAGE.into(),
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
            Lang::Rust => &[tree_sitter_rust::HIGHLIGHTS_QUERY, RUST_REFINEMENTS],
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
            Lang::Sql => &[
                tree_sitter_sequel::HIGHLIGHTS_QUERY,
                SQL_HIGHLIGHT_OVERRIDES,
            ],
            Lang::Lean => &[tree_sitter_lean4::HIGHLIGHTS_QUERY],
            Lang::Php => &[tree_sitter_php::HIGHLIGHTS_QUERY],
            Lang::Ruby => &[tree_sitter_ruby::HIGHLIGHTS_QUERY],
            Lang::CSharp => &[tree_sitter_c_sharp::HIGHLIGHTS_QUERY],
            Lang::Erlang => &[tree_sitter_erlang::HIGHLIGHTS_QUERY],
            Lang::Haskell => &[tree_sitter_haskell::HIGHLIGHTS_QUERY],
            Lang::Vue => &[MARKUP_HIGHLIGHTS, VUE_HIGHLIGHTS],
            Lang::Svelte => &[MARKUP_HIGHLIGHTS, SVELTE_HIGHLIGHTS],
            Lang::Astro => &[tree_sitter_html::HIGHLIGHTS_QUERY],
            Lang::Scss => &[
                tree_sitter_css::HIGHLIGHTS_QUERY,
                tree_sitter_scss::HIGHLIGHTS_QUERY,
                SCSS_REFINEMENTS,
            ],
            Lang::Less => &[
                tree_sitter_css::HIGHLIGHTS_QUERY,
                tree_sitter_less::HIGHLIGHTS_QUERY,
                LESS_REFINEMENTS,
            ],
            // The grammar's crate ships no query of its own.
            Lang::Kotlin => &[KOTLIN_HIGHLIGHTS],
            Lang::Scala => &[tree_sitter_scala::HIGHLIGHTS_QUERY],
            Lang::Elixir => &[tree_sitter_elixir::HIGHLIGHTS_QUERY],
        }
    }

    /// Capture names of this language's highlight query that are not clew's,
    /// and the style each stands for (see [`SQL_CAPTURE_NAMES`]).
    fn capture_names(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Lang::Sql => SQL_CAPTURE_NAMES,
            Lang::Haskell => HASKELL_CAPTURE_NAMES,
            Lang::Scala => SCALA_CAPTURE_NAMES,
            Lang::Php | Lang::Ruby | Lang::CSharp | Lang::Erlang | Lang::Kotlin | Lang::Elixir => {
                &[]
            }
            Lang::Rust
            | Lang::Python
            | Lang::JavaScript
            | Lang::TypeScript
            | Lang::Tsx
            | Lang::Go
            | Lang::Dart
            | Lang::C
            | Lang::Cpp
            | Lang::Java
            | Lang::Json
            | Lang::Bash
            | Lang::Yaml
            | Lang::Toml
            | Lang::Html
            | Lang::Css
            | Lang::Zig
            | Lang::Lean
            | Lang::Vue
            | Lang::Svelte
            | Lang::Astro
            | Lang::Scss
            | Lang::Less => &[],
        }
    }

    /// The injection query, as the parts it is assembled from: where code of
    /// another language sits in this one's — a `<script>` or `<style>` in a
    /// page, a component file's blocks, a template's expressions, CSS or HTML
    /// in a tagged template — each highlighted as that language
    /// ([`injected`] names it).
    fn injection_parts(self) -> &'static [&'static str] {
        match self {
            Lang::Html => &[HTML_SCRIPT, HTML_BLOCKS],
            Lang::Astro => &[ASTRO_SCRIPT, HTML_BLOCKS],
            Lang::Vue => &[HTML_SCRIPT, HTML_BLOCKS, VUE_INJECTIONS],
            Lang::Svelte => &[HTML_SCRIPT, HTML_BLOCKS, SVELTE_INJECTIONS],
            Lang::JavaScript | Lang::TypeScript | Lang::Tsx => &[ECMA_INJECTIONS],
            Lang::Rust
            | Lang::Python
            | Lang::Go
            | Lang::Dart
            | Lang::C
            | Lang::Cpp
            | Lang::Java
            | Lang::Json
            | Lang::Bash
            | Lang::Yaml
            | Lang::Toml
            | Lang::Css
            | Lang::Zig
            | Lang::Sql
            | Lang::Lean
            | Lang::Php
            | Lang::Ruby
            | Lang::CSharp
            | Lang::Erlang
            | Lang::Haskell
            | Lang::Scss
            | Lang::Less
            | Lang::Kotlin
            | Lang::Scala
            | Lang::Elixir => &[],
        }
    }

    /// The outline (tags) query, as the parts it is assembled from; empty for
    /// a language without an outline.
    fn tags_parts(self) -> &'static [&'static str] {
        match self {
            Lang::Rust => &[RUST_TAGS],
            Lang::Python => &[tree_sitter_python::TAGS_QUERY, PYTHON_EXTRA_TAGS],
            Lang::JavaScript => &[tree_sitter_javascript::TAGS_QUERY, JS_EXTRA_TAGS, ECMA_TAGS],
            // The crate's bundled TAGS_QUERY only captures declaration-file
            // constructs (function_signature, method_signature, interface, …),
            // so real .ts source (class/function/method/type) yields almost no
            // symbols. Use a source-oriented tags query instead.
            Lang::TypeScript | Lang::Tsx => &[TS_TAGS, ECMA_TAGS],
            Lang::Go => &[tree_sitter_go::TAGS_QUERY],
            Lang::Dart => &[
                tree_sitter_dart_orchard::TAGS_QUERY,
                DART_TEST_TAGS,
                DART_CONSTRUCTOR_TAGS,
            ],
            Lang::C => &[tree_sitter_c::TAGS_QUERY],
            Lang::Cpp => &[tree_sitter_cpp::TAGS_QUERY, CPP_EXTRA_TAGS],
            Lang::Java => &[tree_sitter_java::TAGS_QUERY, JAVA_EXTRA_TAGS],
            Lang::Sql => &[SQL_TAGS],
            Lang::Lean => &[LEAN_TAGS],
            Lang::Php => &[PHP_TAGS],
            Lang::Ruby => &[tree_sitter_ruby::TAGS_QUERY, RUBY_TEST_TAGS],
            Lang::CSharp => &[CSHARP_TAGS],
            Lang::Erlang => &[ERLANG_TAGS],
            Lang::Haskell => &[HASKELL_TAGS],
            Lang::Zig => &[ZIG_TAGS],
            Lang::Css => &[CSS_TAGS],
            Lang::Scss => &[CSS_TAGS, SCSS_TAGS],
            Lang::Less => &[CSS_TAGS, LESS_TAGS],
            Lang::Html => &[HTML_TAGS],
            Lang::Json => &[JSON_TAGS],
            Lang::Kotlin => &[KOTLIN_TAGS],
            Lang::Scala => &[SCALA_TAGS],
            Lang::Elixir => &[ELIXIR_TAGS],
            // A component file's outline is its script's ([`script_view`]).
            Lang::Bash | Lang::Yaml | Lang::Toml | Lang::Vue | Lang::Svelte | Lang::Astro => &[],
        }
    }

    fn family(self) -> Family {
        match self {
            Lang::JavaScript
            | Lang::TypeScript
            | Lang::Tsx
            | Lang::Vue
            | Lang::Svelte
            | Lang::Astro => Family::Ecma,
            Lang::C | Lang::Cpp => Family::CLike,
            Lang::Java | Lang::Kotlin | Lang::Scala => Family::Jvm,
            Lang::Rust
            | Lang::Python
            | Lang::Go
            | Lang::Dart
            | Lang::Json
            | Lang::Bash
            | Lang::Yaml
            | Lang::Toml
            | Lang::Html
            | Lang::Css
            | Lang::Zig
            | Lang::Sql
            | Lang::Lean
            | Lang::Php
            | Lang::Ruby
            | Lang::CSharp
            | Lang::Erlang
            | Lang::Haskell
            | Lang::Scss
            | Lang::Less
            | Lang::Elixir => Family::Alone(self),
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

/// tree-sitter-sequel's number patterns are written for Neovim's Lua matcher
/// (`%d`), which tree-sitter's regex reads as a literal `%d`: no literal ever
/// matched them, and every number was drawn as a string. Appended last so it
/// wins (see [`Lang::highlight_parts`]).
const SQL_HIGHLIGHT_OVERRIDES: &str = r#"
((literal) @number
  (#match? @number "^[-+]?[0-9]+$"))
((literal) @number
  (#match? @number "^[-+]?[0-9]*\\.[0-9]+$"))
"#;

/// The capture names of tree-sitter-sequel's highlight query that are not
/// clew's, and the style each stands for (empty: none). A capture
/// tree-sitter-highlight does not know is not merely unstyled — the LAST
/// capture on a node wins, so the query's `(comment) @comment @spell` left
/// every comment uncoloured.
/// What Clew's palette tells apart in Rust and the grammar's query does not:
/// a function where it is defined (not where it is called), the module path
/// before a name (`std::path::` in `std::path::Path`), a macro, and an
/// attribute as one whole — its brackets and arguments in its own colour.
///
/// Attribute arguments nest (`cfg(all(unix, not(windows)))`); a query cannot
/// say "anywhere inside", so the depths are spelled out, four deep.
const RUST_REFINEMENTS: &str = r##"
(function_item name: (identifier) @function.definition)
(function_signature_item name: (identifier) @function.definition)

(macro_invocation macro: (identifier) @function.macro "!" @function.macro)
(macro_invocation
  macro: (scoped_identifier name: (identifier) @function.macro)
  "!" @function.macro)

((scoped_identifier path: (identifier) @module . "::" @module.separator)
 (#match? @module "^[a-z_]"))
((scoped_type_identifier path: (identifier) @module . "::" @module.separator)
 (#match? @module "^[a-z_]"))
((scoped_identifier path: (scoped_identifier name: (identifier) @module) . "::" @module.separator)
 (#match? @module "^[a-z_]"))
((scoped_type_identifier path: (scoped_identifier name: (identifier) @module) . "::" @module.separator)
 (#match? @module "^[a-z_]"))

(attribute_item ["#" "[" "]"] @attribute)
(inner_attribute_item ["#" "!" "[" "]"] @attribute)
(attribute (identifier) @attribute)
(attribute (token_tree ["(" ")" "[" "]" "," "="] @attribute))
(attribute (token_tree (identifier) @attribute))
(attribute (token_tree (token_tree ["(" ")" "[" "]" "," "="] @attribute)))
(attribute (token_tree (token_tree (identifier) @attribute)))
(attribute (token_tree (token_tree (token_tree ["(" ")" "[" "]" "," "="] @attribute))))
(attribute (token_tree (token_tree (token_tree (identifier) @attribute))))
(attribute (token_tree (token_tree (token_tree (token_tree ["(" ")" "[" "]" "," "="] @attribute)))))
(attribute (token_tree (token_tree (token_tree (token_tree (identifier) @attribute)))))
"##;

const SQL_CAPTURE_NAMES: &[(&str, &str)] = &[
    ("boolean", "constant.builtin"),
    ("conditional", "keyword"),
    ("field", "property"),
    ("float", "number"),
    ("parameter", "variable.parameter"),
    ("spell", ""),
    ("storageclass", "keyword"),
    ("type.qualifier", "keyword"),
];

/// The capture names of tree-sitter-scala's highlight query that are not
/// clew's: it is written in Neovim's older names (`@parameter`,
/// `@namespace`, `@conditional`, `@method`), none of which matches a style.
const SCALA_CAPTURE_NAMES: &[(&str, &str)] = &[
    ("boolean", "constant.builtin"),
    ("conditional", "keyword"),
    ("exception", "keyword"),
    ("float", "number"),
    ("include", "keyword"),
    ("method", "function.method"),
    ("method.call", "function.method"),
    ("namespace", "module"),
    ("none", ""),
    ("parameter", "variable.parameter"),
    ("repeat", "keyword"),
    ("spell", ""),
    ("storageclass", "keyword"),
    ("type.qualifier", "keyword"),
];

/// Kotlin's highlights, for tree-sitter-kotlin-ng, whose crate ships none:
/// its keywords and literals; declarations, calls, types, annotations and
/// string templates. `true`, `false` and `null` are identifiers to the
/// grammar. Later patterns win (see [`Lang::highlight_parts`]).
const KOTLIN_HIGHLIGHTS: &str = r#"
(identifier) @variable

[
  "abstract" "actual" "annotation" "as" "as?" "by" "catch" "class" "companion"
  "const" "constructor" "crossinline" "data" "delegate" "do" "dynamic" "else"
  "enum" "expect" "external" "field" "file" "final" "finally" "for" "fun" "get"
  "if" "import" "in" "!in" "infix" "init" "inline" "inner" "interface"
  "internal" "is" "!is" "lateinit" "noinline" "object" "open" "operator" "out"
  "override" "package" "param" "private" "property" "protected" "public"
  "receiver" "return" "return@" "sealed" "set" "setparam" "super@" "suspend"
  "tailrec" "this@" "throw" "try" "typealias" "val" "value" "var" "vararg"
  "when" "where" "while"
] @keyword

[
  "!" "!!" "!=" "!==" "%" "%=" "&&" "*" "*=" "+" "++" "+=" "-" "--" "-=" "->"
  ".." "..<" "/" "/=" "<" "<=" "=" "==" "===" ">" ">=" "?:" "||" "::" "?."
] @operator

["(" ")" "[" "]" "{" "}"] @punctuation.bracket
["," "." ";" ":"] @punctuation.delimiter

((identifier) @constant.builtin
  (#any-of? @constant.builtin "true" "false" "null"))
(this_expression) @variable.builtin
(super_expression) @variable.builtin

(package_header (qualified_identifier) @module)
(import (qualified_identifier) @module)

(user_type (identifier) @type)
(class_declaration name: (identifier) @type)
(object_declaration name: (identifier) @type)
(companion_object name: (identifier) @type)
(type_alias type: (identifier) @type)
(enum_entry (identifier) @constant)

(function_declaration name: (identifier) @function.definition)
(call_expression . (identifier) @function)
(call_expression . (navigation_expression (identifier) @function.method .))
(navigation_expression . (_) (identifier) @property .)

(annotation) @attribute
(annotation (user_type (identifier) @attribute))
(annotation (constructor_invocation (user_type (identifier) @attribute)))
(label) @label

[(number_literal) (float_literal)] @number
[(string_literal) (multiline_string_literal) (character_literal)] @string
(string_content) @string
(escape_sequence) @string.escape
(interpolation) @embedded
(interpolation ["$" "${" "}"] @punctuation.special)

[(line_comment) (block_comment) (shebang)] @comment
"#;

/// The Kotlin outline: the package, classes (an interface, an enum by its
/// keyword), objects, functions, a class's or a file's properties (not a
/// function's local `val`s), type aliases and enum entries; and Kotest's
/// tests (`test("adds") { … }`, `should`, `it`) and their groups
/// (`context`, `describe`), each a block a call opens.
const KOTLIN_TAGS: &str = r#"
(package_header (qualified_identifier) @name) @definition.module
(class_declaration "interface" name: (identifier) @name) @definition.interface
(class_declaration
  (modifiers (class_modifier) @_modifier)
  name: (identifier) @name
  (#eq? @_modifier "enum")) @definition.enum
(class_declaration name: (identifier) @name) @definition.class
(object_declaration name: (identifier) @name) @definition.class
(companion_object name: (identifier) @name) @definition.class
(function_declaration name: (identifier) @name) @definition.function
(class_body (property_declaration (variable_declaration (identifier) @name)) @definition.property)
(source_file (property_declaration (variable_declaration (identifier) @name)) @definition.property)
(type_alias type: (identifier) @name) @definition.type
(enum_class_body (enum_entry (identifier) @name) @definition.constant)
(call_expression
  (call_expression
    (identifier) @_fn
    (value_arguments . (value_argument (string_literal (string_content) @name))))
  (annotated_lambda)
  (#any-of? @_fn "test" "should" "it" "expect" "scenario")) @definition.test
(call_expression
  (call_expression
    (identifier) @_fn
    (value_arguments . (value_argument (string_literal (string_content) @name))))
  (annotated_lambda)
  (#any-of? @_fn "context" "describe" "feature")) @definition.suite
"#;

/// The Scala outline (2 and 3): packages, traits, enums and their cases,
/// classes, objects, functions (an abstract one too), a template's or a
/// file's `val`s and `var`s, givens and type definitions; and MUnit's and
/// ScalaTest's tests (`test("adds") { … }`, `it`, `property`) and their
/// groups (`describe`).
const SCALA_TAGS: &str = r#"
(package_clause name: (package_identifier) @name) @definition.module
(trait_definition name: (identifier) @name) @definition.interface
(enum_definition name: (identifier) @name) @definition.enum
(simple_enum_case name: (identifier) @name) @definition.constant
(full_enum_case name: (identifier) @name) @definition.constant
(class_definition name: (identifier) @name) @definition.class
(object_definition name: (identifier) @name) @definition.class
(function_definition name: (identifier) @name) @definition.function
(function_declaration name: (identifier) @name) @definition.function
(template_body (val_definition pattern: (identifier) @name) @definition.property)
(template_body (var_definition pattern: (identifier) @name) @definition.property)
(compilation_unit (val_definition pattern: (identifier) @name) @definition.property)
(template_body (val_declaration name: (identifier) @name) @definition.property)
(given_definition name: (identifier) @name) @definition.property
(type_definition name: (type_identifier) @name) @definition.type
(call_expression
  function: (call_expression
    function: (identifier) @_fn
    arguments: (arguments . (string) @name))
  arguments: (block)
  (#any-of? @_fn "test" "it" "property")) @definition.test
(call_expression
  function: (call_expression
    function: (identifier) @_fn
    arguments: (arguments . (string) @name))
  arguments: (block)
  (#eq? @_fn "describe")) @definition.suite
"#;

/// RSpec's examples (`it "adds" do`, `specify`, `scenario`; `test` of
/// Rails' and Minitest's spec style) and their groups (`describe Cart`,
/// `context "when empty"`), beside the grammar's own outline.
const RUBY_TEST_TAGS: &str = r#"
(call
  method: (identifier) @_fn
  arguments: (argument_list . (string (string_content) @name))
  block: [(do_block) (block)]
  (#any-of? @_fn "it" "specify" "example" "scenario" "test")) @definition.test
(call
  method: (identifier) @_fn
  arguments: (argument_list . [(string (string_content) @name) (constant) @name (scope_resolution) @name])
  block: [(do_block) (block)]
  (#any-of? @_fn "describe" "context" "feature")) @definition.suite
"#;

/// A Dart constructor written with no body — `CounterCubit() : super(0);`,
/// `const BlocProvider({…});`, `factory Cart.empty() = _EmptyCart;` — which
/// the grammar's outline leaves out: what `CounterCubit()` calls.
const DART_CONSTRUCTOR_TAGS: &str = r#"
(declaration (constructor_signature name: (identifier) @name)) @definition.method
(declaration (constant_constructor_signature (identifier) @name)) @definition.method
(declaration (factory_constructor_signature (identifier) @name)) @definition.method
(declaration (redirecting_factory_constructor_signature (identifier) @name)) @definition.method
"#;

/// package:test's tests (`test('adds', () { … })`, Flutter's `testWidgets`)
/// and groups (`group`), beside the grammar's own outline.
const DART_TEST_TAGS: &str = r#"
(method_invocation
  function: (identifier) @_fn
  arguments: (argument_part (arguments . (argument (string_literal) @name)))
  (#any-of? @_fn "test" "testWidgets")) @definition.test
(method_invocation
  function: (identifier) @_fn
  arguments: (argument_part (arguments . (argument (string_literal) @name)))
  (#eq? @_fn "group")) @definition.suite
"#;

/// The Elixir outline: modules and protocols, functions (each clause —
/// merged, see `outline::merge_clauses` — a guarded one and a zero-arity
/// one without parentheses too), macros and `@type`s; and ExUnit's tests
/// (`test "adds" do`, a property's) and `describe` groups. Every definition
/// is a call to the grammar: `def` names the function its first argument
/// calls.
const ELIXIR_TAGS: &str = r#"
(call
  target: (identifier) @_kw
  (arguments (alias) @name)
  (#any-of? @_kw "defmodule" "defprotocol")) @definition.module
(call
  target: (identifier) @_kw
  (arguments
    [
      (identifier) @name
      (call target: (identifier) @name)
      (binary_operator left: (call target: (identifier) @name) operator: "when")
    ])
  (#any-of? @_kw "def" "defp" "defdelegate" "defguard" "defguardp" "defn" "defnp")) @definition.function
(call
  target: (identifier) @_kw
  (arguments
    [
      (identifier) @name
      (call target: (identifier) @name)
      (binary_operator left: (call target: (identifier) @name) operator: "when")
    ])
  (#any-of? @_kw "defmacro" "defmacrop")) @definition.macro
(unary_operator
  operator: "@"
  operand: (call
    target: (identifier) @_attr
    (arguments
      (binary_operator
        left: [(call target: (identifier) @name) (identifier) @name]
        operator: "::")))
  (#any-of? @_attr "type" "typep" "opaque")) @definition.type
(call
  target: (identifier) @_kw
  (arguments . (string (quoted_content) @name))
  (do_block)
  (#any-of? @_kw "test" "property")) @definition.test
(call
  target: (identifier) @_kw
  (arguments . (string (quoted_content) @name))
  (do_block)
  (#eq? @_kw "describe")) @definition.suite
"#;

/// The capture names of tree-sitter-haskell's highlight query that are not
/// clew's (see [`SQL_CAPTURE_NAMES`]): its `@spell` follows `@comment` on
/// comments, which would leave them unstyled.
const HASKELL_CAPTURE_NAMES: &[(&str, &str)] = &[
    ("boolean", "constant.builtin"),
    ("character", "string"),
    ("spell", ""),
];

/// `query` with each capture renamed as `names` says (`(from, to)`; an empty
/// `to` drops the capture). Only whole capture names are renamed: `@field`
/// is not the start of `@fields`, and an `@` inside a string literal (`"@"`)
/// names no capture.
fn rename_captures(query: &str, names: &[(&str, &str)]) -> String {
    let name_char = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-');
    let mut out = String::with_capacity(query.len());
    let mut rest = query;
    while let Some(at) = rest.find('@') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        let len = after.find(|c| !name_char(c)).unwrap_or(after.len());
        let name = &after[..len];
        match names.iter().find(|(from, _)| *from == name) {
            Some((_, "")) => {}
            Some((_, to)) => {
                out.push('@');
                out.push_str(to);
            }
            None => {
                out.push('@');
                out.push_str(name);
            }
        }
        rest = &after[len..];
    }
    out.push_str(rest);
    out
}

/// The SQL outline: tables and their columns, views, functions, triggers and
/// types. Each `CREATE` names its object right after its keyword (or after
/// `IF NOT EXISTS`); the anchor keeps a trigger's `ON table` and a function's
/// `RETURNS SETOF table` from naming it too.
const SQL_TAGS: &str = r#"
(create_table
  [(keyword_table) (keyword_exists)] .
  (object_reference name: (identifier) @name)) @definition.table
(column_definition name: (identifier) @name) @definition.field
(create_view
  [(keyword_view) (keyword_exists)] .
  (object_reference name: (identifier) @name)) @definition.view
(create_materialized_view
  [(keyword_view) (keyword_exists)] .
  (object_reference name: (identifier) @name)) @definition.view
(create_function
  [(keyword_function) (keyword_exists)] .
  (object_reference name: (identifier) @name)) @definition.function
(create_trigger
  [(keyword_trigger) (keyword_exists)] .
  (object_reference name: (identifier) @name)) @definition.trigger
(create_type
  [(keyword_type) (keyword_exists)] .
  (object_reference name: (identifier) @name)) @definition.type
"#;

/// The Lean outline. A dotted name (`Group.toMonoid`) is several `name`
/// nodes; the outline reads it whole (`outline::located_in`), and the kind of
/// a `definition` — `def`, `theorem`, an `instance` — from its keyword
/// (`outline::refine_kind`).
const LEAN_TAGS: &str = r#"
(definition name: (identifier) @name) @definition.function
(constant name: (identifier) @name) @definition.constant
(opaque name: (identifier) @name) @definition.function
(axiom name: (identifier) @name) @definition.axiom
(structure name: (identifier) @name) @definition.struct
(structure_field name: (identifier) @name) @definition.field
(inductive name: (identifier) @name) @definition.inductive
(class_inductive name: (identifier) @name) @definition.class
(constructor name: (identifier) @name) @definition.constructor
(namespace name: (identifier) @name) @definition.module
"#;

/// The PHP outline. The crate's own query calls a method a function, a trait
/// an interface, and has no enums.
const PHP_TAGS: &str = r#"
(namespace_definition name: (namespace_name) @name) @definition.module
(class_declaration name: (name) @name) @definition.class
(interface_declaration name: (name) @name) @definition.interface
(trait_declaration name: (name) @name) @definition.trait
(enum_declaration name: (name) @name) @definition.enum
(enum_case name: (name) @name) @definition.constant
(method_declaration name: (name) @name) @definition.method
(function_definition name: (name) @name) @definition.function
(property_declaration (property_element name: (variable_name (name) @name))) @definition.field
"#;

/// The C# outline. The crate's own query has classes, interfaces and methods
/// only: no namespaces, structs, enums, records, properties or fields.
const CSHARP_TAGS: &str = r#"
(namespace_declaration name: (_) @name) @definition.module
(file_scoped_namespace_declaration name: (_) @name) @definition.module
(class_declaration name: (identifier) @name) @definition.class
(record_declaration name: (identifier) @name) @definition.class
(interface_declaration name: (identifier) @name) @definition.interface
(struct_declaration name: (identifier) @name) @definition.struct
(enum_declaration name: (identifier) @name) @definition.enum
(enum_member_declaration name: (identifier) @name) @definition.constant
(delegate_declaration name: (identifier) @name) @definition.type
(method_declaration name: (identifier) @name) @definition.method
(constructor_declaration name: (identifier) @name) @definition.method
(local_function_statement name: (identifier) @name) @definition.function
(property_declaration name: (identifier) @name) @definition.property
(field_declaration (variable_declaration (variable_declarator name: (identifier) @name))) @definition.field
"#;

/// The Erlang outline: the module, its functions (one per clause — the
/// outline merges a function's clauses, see `outline::merge_clauses`),
/// records and their fields, types and macros.
const ERLANG_TAGS: &str = r#"
(module_attribute name: (atom) @name) @definition.module
(fun_decl clause: (function_clause name: (atom) @name)) @definition.function
(record_decl name: (atom) @name) @definition.struct
(record_field name: (atom) @name) @definition.field
(type_alias name: (type_name name: (atom) @name)) @definition.type
(opaque name: (type_name name: (atom) @name)) @definition.type
(pp_define lhs: (macro_lhs name: (_) @name)) @definition.macro
"#;

/// The Haskell outline: functions and bindings (one per equation — merged,
/// see `outline::merge_clauses`), data types and their constructors and
/// fields, newtypes, type synonyms, classes and their methods; and hspec's
/// and tasty's tests (`it "adds" $ …`, `testCase`) and groups (`describe`,
/// `testGroup`).
const HASKELL_TAGS: &str = r#"
(declarations (function name: (variable) @name) @definition.function)
(declarations (bind name: (variable) @name) @definition.function)
(declarations (function name: (prefix_id (operator) @name)) @definition.function)
(declarations (function (infix operator: (operator) @name)) @definition.function)
(declarations (function (infix operator: (infix_id (variable) @name))) @definition.function)
(data_type name: (name) @name) @definition.type
(newtype name: (name) @name) @definition.type
(type_synonym name: (name) @name) @definition.type
(type_family name: (name) @name) @definition.type
(data_constructor constructor: (record name: (constructor) @name)) @definition.constructor
(data_constructor constructor: (prefix name: (constructor) @name)) @definition.constructor
(field name: (field_name (variable) @name)) @definition.field
(class name: (name) @name) @definition.class
(class_declarations (signature name: (variable) @name) @definition.method)
(class_declarations (signature name: (prefix_id (operator) @name)) @definition.method)
(infix
  left_operand: (apply function: (variable) @_fn argument: (literal (string) @name))
  (#any-of? @_fn "it" "specify" "prop" "testCase" "testProperty")) @definition.test
(apply
  function: (apply function: (variable) @_fn argument: (literal (string) @name))
  (#any-of? @_fn "it" "specify" "prop" "testCase" "testProperty")) @definition.test
(infix
  left_operand: (apply function: (variable) @_fn argument: (literal (string) @name))
  (#any-of? @_fn "describe" "context" "testGroup")) @definition.suite
(apply
  function: (apply function: (variable) @_fn argument: (literal (string) @name))
  (#any-of? @_fn "describe" "context" "testGroup")) @definition.suite
"#;

/// The Zig outline: containers bound to a name (structs, enums, unions,
/// opaques), their fields and the functions in them (methods), free
/// functions, and tests. Methods come first, so a function is one or the
/// other (as in [`RUST_TAGS`]).
const ZIG_TAGS: &str = r#"
(variable_declaration (identifier) @name (struct_declaration)) @definition.struct
(variable_declaration (identifier) @name (enum_declaration)) @definition.enum
(variable_declaration (identifier) @name (union_declaration)) @definition.union
(variable_declaration (identifier) @name (opaque_declaration)) @definition.type
(container_field name: (identifier) @name) @definition.field
(struct_declaration (function_declaration name: (identifier) @name) @definition.method)
(enum_declaration (function_declaration name: (identifier) @name) @definition.method)
(union_declaration (function_declaration name: (identifier) @name) @definition.method)
(function_declaration name: (identifier) @name) @definition.function
(test_declaration (string (string_content) @name)) @definition.test
(test_declaration (identifier) @name) @definition.test
"#;

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
  (public_field_definition name: [(property_identifier) (private_property_identifier)] @name) @definition.field)
;; A type alias's object type, which declares its members as an interface
;; does (`type Props = { title: string }`).
(type_alias_declaration
  value: (object_type
    (property_signature name: (property_identifier) @name) @definition.property))
;; A namespace (`namespace Api {}`, `module Api {}`) and an ambient module
;; (`declare module "vue" {}`), with what each declares inside.
(internal_module name: [(identifier) (nested_identifier)] @name) @definition.module
(module name: [(identifier) (nested_identifier)] @name) @definition.module
(module name: (string (string_fragment) @name)) @definition.module
;; An enum's members.
(enum_body (property_identifier) @name @definition.variant)
(enum_body (enum_assignment name: (property_identifier) @name) @definition.variant)
"#;

/// Added to tree-sitter-javascript's own tags query: a class field holding a
/// function (`handler = () => {…}`), which that query leaves out entirely, so
/// the calls in its body had no function to belong to.
/// SCSS over CSS's and its own query: a variable declared (`$gap: 4px`) is
/// a variable, as it is where it is used, and a `//` comment a comment.
const SCSS_REFINEMENTS: &str = r#"
((property_name) @variable (#match? @variable "^\\$"))
(js_comment) @comment
"#;

/// Less over CSS's and its own query: a variable declared (`@gap: 4px`) is
/// a variable, as it is where it is used, and a `//` comment a comment
/// (Less's query also captures it `@spell`, which drew it as text).
const LESS_REFINEMENTS: &str = r#"
((property_name) @variable (#match? @variable "^@"))
(js_comment) @comment
"#;

/// A component file's markup, as HTML's query reads a page's — except that
/// only a plain attribute's value is a string: a directive's (Vue's
/// `@click="save(item)"`) is code, highlighted as the expression it is, and
/// a string over it would be drawn over the expression's first word.
const MARKUP_HIGHLIGHTS: &str = r#"
(tag_name) @tag
(erroneous_end_tag_name) @tag.error
(doctype) @constant
(attribute_name) @attribute
(attribute (attribute_value) @string)
(attribute (quoted_attribute_value) @string)
(comment) @comment
["<" ">" "</" "/>"] @punctuation.bracket
"#;

/// A Vue component's template over HTML's: a directive's name (`v-if`,
/// `v-model`) as a keyword, the attribute or event it binds (`:title`,
/// `@click`) and its modifiers (`.prevent`), and the `{{ }}` around an
/// interpolation. The expressions themselves are TypeScript
/// ([`VUE_INJECTIONS`]).
const VUE_HIGHLIGHTS: &str = r##"
(directive_name) @keyword
(directive_value) @attribute
(dynamic_directive_inner_value) @variable
(directive_modifier) @attribute
["{{" "}}"] @punctuation.special
[":" "@" "#" "."] @punctuation.delimiter
["[" "]"] @punctuation.bracket
"##;

/// A Svelte component's template over HTML's: the words of its blocks and
/// tags (`{#if}`, `{:else}`, `{#each … as …}`, `{@html}`) as keywords, their
/// braces and sigils, and a snippet's name. The expressions are TypeScript
/// ([`SVELTE_INJECTIONS`]).
const SVELTE_HIGHLIGHTS: &str = r##"
["if" "else" "else if" "each" "await" "then" "catch" "as" "key" "html" "snippet" "render" "const" "debug"] @keyword
["{" "}"] @punctuation.special
["#" ":" "/" "@"] @punctuation.delimiter
["(" ")"] @punctuation.bracket
(snippet_name) @function
"##;

/// A page's `<script>` that names no language (no `lang`, no `type`): the
/// JavaScript a browser runs.
const HTML_SCRIPT: &str = r#"
((script_element (start_tag) @_tag (raw_text) @injection.content)
 (#not-match? @_tag "\\s(lang|type)\\s*=")
 (#set! injection.language "javascript"))
"#;

/// An Astro component's `<script>` that names no language: TypeScript,
/// which Astro compiles its scripts as.
const ASTRO_SCRIPT: &str = r#"
((script_element (start_tag) @_tag (raw_text) @injection.content)
 (#not-match? @_tag "\\s(lang|type)\\s*=")
 (#set! injection.language "typescript"))
"#;

/// A page's (and a component file's) other `<script>`s and its `<style>`s:
/// JavaScript for a JavaScript `type`, JSON for a data block
/// (`application/json`, an import map, JSON-LD), and the language a `lang`
/// names (`<script lang="ts">`, `<style lang="scss">`); a `<style>` with no
/// `lang` is CSS. A script of another `type` (a template) is left as text.
const HTML_BLOCKS: &str = r#"
((script_element (start_tag) @_tag (raw_text) @injection.content)
 (#not-match? @_tag "\\slang\\s*=")
 (#match? @_tag "\\stype\\s*=\\s*[\"']?(module|text/javascript|application/javascript|text/ecmascript|application/ecmascript|text/babel|text/jsx)[\"'\\s>]")
 (#set! injection.language "javascript"))
((script_element (start_tag) @_tag (raw_text) @injection.content)
 (#not-match? @_tag "\\slang\\s*=")
 (#match? @_tag "\\stype\\s*=\\s*[\"']?(application/json|importmap|application/ld\\+json|speculationrules)[\"'\\s>]")
 (#set! injection.language "json"))
(script_element
  (start_tag
    (attribute
      (attribute_name) @_attr
      [(attribute_value) @injection.language
       (quoted_attribute_value (attribute_value) @injection.language)]))
  (raw_text) @injection.content
  (#eq? @_attr "lang"))
((style_element (start_tag) @_tag (raw_text) @injection.content)
 (#not-match? @_tag "\\slang\\s*=")
 (#set! injection.language "css"))
(style_element
  (start_tag
    (attribute
      (attribute_name) @_attr
      [(attribute_value) @injection.language
       (quoted_attribute_value (attribute_value) @injection.language)]))
  (raw_text) @injection.content
  (#eq? @_attr "lang"))
"#;

/// A Vue template's expressions — an interpolation's (`{{ user.name }}`)
/// and a directive's value (`v-if="open"`, `:title="t('x')"`,
/// `@click="save"`) — read as the TypeScript they are.
const VUE_INJECTIONS: &str = r#"
((interpolation (raw_text) @injection.content)
 (#set! injection.language "typescript"))
(directive_attribute
  (quoted_attribute_value (attribute_value) @injection.content)
  (#set! injection.language "typescript"))
"#;

/// A Svelte template's expressions — `{count}`, a block's condition, an
/// attribute's `{value}` — read as the TypeScript they are.
const SVELTE_INJECTIONS: &str = r#"
((svelte_raw_text) @injection.content
 (#set! injection.language "typescript"))
"#;

/// CSS, HTML and SQL written in a tagged template: CSS-in-JS (`css`,
/// `keyframes`, `createGlobalStyle`, styled-components' `styled.div`,
/// `styled(Button)` and `.attrs(…)`), lit's `html` and `svg`, and `sql`.
///
/// A file's CSS templates are read together as one stylesheet, so that a
/// declaration a `${…}` splits (`color: ${c};`) still reads as one. That is
/// the only combined pattern: tree-sitter-highlight orders the layers of
/// several combined patterns by pattern rather than by place, and the
/// highlighting of all but one went missing. HTML and SQL are read a piece
/// at a time, between their `${…}`.
const ECMA_INJECTIONS: &str = r#"
(call_expression
  function: [
    (identifier) @_tag
    (member_expression object: (identifier) @_tag)
    (call_expression function: (identifier) @_tag)
    (call_expression function: (member_expression object: (identifier) @_tag))
    (call_expression function: (member_expression object: (member_expression object: (identifier) @_tag)))
    (call_expression function: (member_expression object: (call_expression function: (identifier) @_tag)))
  ]
  arguments: (template_string (string_fragment) @injection.content)
  (#any-of? @_tag "css" "keyframes" "createGlobalStyle" "injectGlobal" "styled")
  (#set! injection.language "css")
  (#set! injection.combined)
  (#set! injection.include-children))
(call_expression
  function: (identifier) @_tag
  arguments: (template_string (string_fragment) @injection.content)
  (#any-of? @_tag "html" "svg")
  (#set! injection.language "html"))
(call_expression
  function: (identifier) @_tag
  arguments: (template_string (string_fragment) @injection.content)
  (#eq? @_tag "sql")
  (#set! injection.language "sql"))
"#;

const JS_EXTRA_TAGS: &str = r#"
(field_definition
  property: (property_identifier) @name
  value: [(arrow_function) (function_expression)]) @definition.method
;; A class's fields, `#private` ones too; one holding a function is a method
;; (`outline::refine_kind`).
(field_definition
  property: [(property_identifier) (private_property_identifier)] @name) @definition.field
"#;

/// JavaScript's and TypeScript's outline, beyond each one's own query.
///
/// - A module's own bindings — `export const routes = [...]`, `const store =
///   defineStore(…)`, a styled component, `let cache = new Map()` — each a
///   constant or a variable, or a function when its value is one: a
///   component made by `memo(…)` or `forwardRef(…)` too
///   (`outline::refine_kind`). A function's locals are not listed.
/// - `export default function () {}` and `export default () => …`, which
///   have no name of their own: the module's `default`.
/// - A class's `#private` methods.
/// - A test file's suites and tests as Jest, Vitest, Mocha, node:test, Bun,
///   Playwright and Deno write them: `describe('cart', …)`, `it.only('adds',
///   …)`, `test.each(rows)('adds %i', …)`, `test.describe('login', …)`,
///   `Deno.test('parses', …)` — each named by its title, a test in the
///   suite that holds it.
const ECMA_TAGS: &str = r#"
(program
  (lexical_declaration
    (variable_declarator name: (identifier) @name)) @definition.constant)
(program
  (variable_declaration
    (variable_declarator name: (identifier) @name)) @definition.constant)
(program
  (export_statement
    declaration: (lexical_declaration
      (variable_declarator name: (identifier) @name)) @definition.constant))
(program
  (export_statement
    declaration: (variable_declaration
      (variable_declarator name: (identifier) @name)) @definition.constant))
(export_statement
  "default" @name
  value: [(function_expression) (arrow_function)]) @definition.function
(export_statement
  "default" @name
  value: (class)) @definition.class
(method_definition name: (private_property_identifier) @name) @definition.method
(call_expression
  function: [
    (identifier) @_fn
    (member_expression object: (identifier) @_fn)
    (member_expression object: (member_expression object: (identifier) @_fn))
    (call_expression function: (member_expression object: (identifier) @_fn))
  ]
  arguments: (arguments . [(string (string_fragment) @name) (template_string) @name])
  (#any-of? @_fn "describe" "suite" "context")) @definition.suite
(call_expression
  function: [
    (member_expression object: (identifier) @_obj property: (property_identifier) @_fn)
    (member_expression object: (member_expression object: (identifier) @_obj property: (property_identifier) @_fn))
  ]
  arguments: (arguments . [(string (string_fragment) @name) (template_string) @name])
  (#eq? @_obj "test")
  (#eq? @_fn "describe")) @definition.suite
(call_expression
  function: [
    (identifier) @_fn
    (member_expression object: (identifier) @_fn property: (property_identifier) @_how)
    (member_expression object: (member_expression object: (identifier) @_fn property: (property_identifier) @_how))
    (call_expression function: (member_expression object: (identifier) @_fn))
  ]
  arguments: (arguments . [(string (string_fragment) @name) (template_string) @name])
  (#any-of? @_fn "it" "test" "specify" "bench")
  (#not-any-of? @_how "describe" "beforeEach" "afterEach" "beforeAll" "afterAll" "use" "extend" "step" "fixme")) @definition.test
(call_expression
  function: (member_expression object: (identifier) @_obj property: (property_identifier) @_fn)
  arguments: (arguments . [(string (string_fragment) @name) (template_string) @name])
  (#eq? @_obj "Deno")
  (#eq? @_fn "test")) @definition.test
"#;

/// A stylesheet's outline (CSS's, and the base of SCSS's and Less's): each
/// rule by its selectors, each `@media`, `@supports`, `@container`,
/// `@layer` or other block at-rule holding the rules inside it (its name
/// the at-rule's head, `outline::stylesheet_name`), each `@keyframes`, and
/// each custom property (`--brand: #333`).
const CSS_TAGS: &str = r#"
(rule_set (selectors) @name) @definition.rule
(media_statement) @name @definition.module
(supports_statement) @name @definition.module
(at_rule (block)) @name @definition.module
(keyframes_statement) @name @definition.module
(declaration (property_name) @name (#match? @name "^--")) @definition.variable
"#;

/// SCSS's own declarations over CSS's outline: a mixin, a function, and a
/// variable the stylesheet declares at its top (`$gap: 4px`).
const SCSS_TAGS: &str = r#"
(mixin_statement name: (identifier) @name) @definition.macro
(function_statement name: (identifier) @name) @definition.function
(stylesheet (declaration (property_name) @name (#match? @name "^\\$")) @definition.variable)
"#;

/// Less's own declarations over CSS's outline: a mixin (`.bordered() {}`)
/// and a variable declared at the top (`@gap: 4px`).
const LESS_TAGS: &str = r#"
(mixin_definition [(class_name) (id_name)] @name) @definition.macro
(stylesheet (declaration (property_name) @name (#match? @name "^@")) @definition.variable)
"#;

/// A page's outline: its headings (`<h2>Pricing</h2>`, by their text), and
/// each element with an `id` — what a link or a script names it by — as
/// `#id` (`outline::markup_name`).
const HTML_TAGS: &str = r#"
(element
  (start_tag (tag_name) @_tag)
  (text) @name
  (#match? @_tag "^[hH][1-6]$")) @definition.section
(element
  (start_tag
    (attribute
      (attribute_name) @_attr
      [(attribute_value) @name (quoted_attribute_value (attribute_value) @name)]))
  (#eq? @_attr "id")) @definition.field
"#;

/// A JSON file's outline: its top-level keys and the keys of each of their
/// objects — a `package.json`'s `scripts` and each script, a `tsconfig`'s
/// `compilerOptions` and each option — two levels, however deep the file
/// goes.
const JSON_TAGS: &str = r#"
(document
  (object
    (pair key: (string (string_content) @name)) @definition.field))
(document
  (object
    (pair
      value: (object
        (pair key: (string (string_content) @name)) @definition.field))))
"#;

/// Added to tree-sitter-python's own tags query, which has a module's
/// assignments, its classes and its functions: a class's fields — a
/// dataclass's, a pydantic or Django model's, an enum's members, each an
/// assignment in the class's body. A model read in the outline showed its
/// methods and none of what it holds. And a module's type aliases (PEP
/// 695's `type Vector = list[float]`, generic ones too), which the
/// outline left out.
const PYTHON_EXTRA_TAGS: &str = r#"
(class_definition
  body: (block
    (expression_statement
      (assignment left: (identifier) @name) @definition.field)))
(module
  (type_alias_statement
    left: [
      (type (identifier) @name)
      (type (generic_type (identifier) @name))
    ]) @definition.type)
"#;

/// Added to tree-sitter-cpp's own tags query, whose member defined outside
/// its class is only `Class::name` with a plain class name: a class
/// template's (`template <typename T> int Box<T>::size() const`) and one
/// named through a namespace too (`detail::Box<T>::size`, `ns::Cls::f`) were
/// no definitions, and the calls in their bodies had no function to belong
/// to.
const CPP_EXTRA_TAGS: &str = r#"
(function_declarator
  declarator: (qualified_identifier
    scope: (template_type)
    name: (identifier) @name)) @definition.method
(function_declarator
  declarator: (qualified_identifier
    name: (qualified_identifier
      name: (identifier) @name))) @definition.method
(function_declarator
  declarator: (qualified_identifier
    name: (qualified_identifier
      name: (qualified_identifier
        name: (identifier) @name)))) @definition.method
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
/// each ADT and traits with their real kind. A function is a method in an
/// `impl` or a trait only: any `declaration_list` — an inline `mod`'s body,
/// an `extern` block's — made the functions of a module methods. A trait's
/// required methods (`fn area(&self) -> f64;`) are its items too, declared
/// without a body as an interface's are: a trait's outline listed only the
/// methods it provides. Its associated types (`type Item;`) are its items as
/// well; and an `extern` block's functions (`fn abs(x: i32) -> i32;`) are the
/// module's, foreign, without a body.
const RUST_TAGS: &str = r#"
(struct_item name: (type_identifier) @name) @definition.struct
(enum_item name: (type_identifier) @name) @definition.enum
(union_item name: (type_identifier) @name) @definition.union
(type_item name: (type_identifier) @name) @definition.type
(trait_item body: (declaration_list
    (associated_type name: (type_identifier) @name) @definition.type))
(foreign_mod_item body: (declaration_list
    (function_signature_item name: (identifier) @name) @definition.function))
(impl_item body: (declaration_list
    (function_item name: (identifier) @name) @definition.method))
(trait_item body: (declaration_list
    (function_item name: (identifier) @name) @definition.method))
(trait_item body: (declaration_list
    (function_signature_item name: (identifier) @name) @definition.method))
(function_item name: (identifier) @name) @definition.function
(trait_item name: (type_identifier) @name) @definition.trait
(mod_item name: (identifier) @name) @definition.module
(macro_definition name: (identifier) @name) @definition.macro
(const_item name: (identifier) @name) @definition.constant
(static_item name: (identifier) @name) @definition.static
(field_declaration_list
    (field_declaration name: (field_identifier) @name) @definition.field)
(enum_variant_list (enum_variant name: (identifier) @name) @definition.variant)
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
/// Whether a source named as JSON is YAML: its first line that holds
/// anything (a `#` comment aside) is a mapping's `key: value`, a list's
/// `- item`, or the `---` that opens a document — which JSON (a `{`, `[`,
/// `"`, number, `true`/`false`/`null`, or JSONC's `//` comment) never is.
fn looks_like_yaml(source: &str) -> bool {
    let Some(first) = source
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))
    else {
        return false;
    };
    if first == "---" || first.starts_with("- ") {
        return true;
    }
    let key: String = first
        .chars()
        .take_while(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | '@'))
        .collect();
    !key.is_empty()
        && !matches!(key.as_str(), "true" | "false" | "null")
        && !key.starts_with(|c: char| c.is_ascii_digit() || c == '-')
        && first[key.len()..].trim_start().starts_with(':')
}

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
    let name = path.file_name()?.to_str()?;
    if let Some(lang) = Lang::ALL.iter().find(|l| l.file_names().contains(&name)) {
        return Some(lang.key());
    }
    // A two-part extension (`.app.src`) before the plain one.
    let lower = name.to_ascii_lowercase();
    if let Some(lang) = Lang::ALL.iter().find(|l| {
        l.extensions()
            .iter()
            .any(|e| e.contains('.') && lower.ends_with(&format!(".{e}")))
    }) {
        return Some(lang.key());
    }
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

/// The code a component file runs, as the language it is written in: a Vue
/// or Svelte component's `<script>` blocks (`<script setup>` and the
/// module script alike) and an Astro component's frontmatter and scripts,
/// with everything else in the file — the template, the styles — blanked to
/// spaces, so every byte and line is where the file has it. TypeScript
/// where a script says `lang="ts"` (`"tsx"`, TSX) and in Astro, which
/// compiles its scripts as TypeScript; JavaScript otherwise. A script of
/// another `type` (JSON, a template) is not code. `None` for a file of any
/// other language, or a component with no script.
pub fn script_view(source: &str, lang: Lang) -> Option<(Lang, String)> {
    if !matches!(lang, Lang::Vue | Lang::Svelte | Lang::Astro) {
        return None;
    }
    let tree = parse(source, lang)?;
    let mut ranges: Vec<std::ops::Range<usize>> = Vec::new();
    let mut script = match lang {
        Lang::Astro => Lang::TypeScript,
        _ => Lang::JavaScript,
    };
    if lang == Lang::Astro
        && let Some((inner, close, _)) = astro_frontmatter(source)
    {
        ranges.push(inner..close);
    }
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.kind() != "script_element" {
            let mut cursor = node.walk();
            stack.extend(node.named_children(&mut cursor));
            continue;
        }
        let mut cursor = node.walk();
        let children: Vec<_> = node.named_children(&mut cursor).collect();
        let tag = children
            .iter()
            .find(|c| c.kind() == "start_tag")
            .and_then(|t| source.get(t.byte_range()))
            .unwrap_or("")
            .to_ascii_lowercase();
        let attr = |name: &str| -> Option<String> {
            let at = tag.find(&format!(" {name}="))? + name.len() + 2;
            let value = tag[at..].trim_start_matches(['"', '\'']);
            Some(
                value
                    .split(['"', '\'', ' ', '>'])
                    .next()
                    .unwrap_or("")
                    .to_string(),
            )
        };
        let code = match attr("type").as_deref() {
            None
            | Some("module" | "text/javascript" | "application/javascript" | "text/typescript") => {
                true
            }
            Some(_) => false,
        };
        if !code {
            continue;
        }
        match attr("lang").as_deref() {
            Some("ts" | "typescript") if script == Lang::JavaScript => script = Lang::TypeScript,
            Some("tsx") => script = Lang::Tsx,
            _ => {}
        }
        if let Some(raw) = children.iter().find(|c| c.kind() == "raw_text") {
            ranges.push(raw.byte_range());
        }
    }
    if ranges.is_empty() {
        return None;
    }
    let mut view: Vec<u8> = source
        .bytes()
        .map(|b| if matches!(b, b'\n' | b'\r') { b } else { b' ' })
        .collect();
    for range in ranges {
        view[range.clone()].copy_from_slice(&source.as_bytes()[range]);
    }
    Some((script, String::from_utf8(view).ok()?))
}

/// The language to read `source` as for what is IN its code — its
/// outline, imports, calls, docs and weak spots — and the text to read:
/// [`Lang::for_source`]'s, with a component file's script read as the
/// code it is ([`script_view`]).
pub fn read_as<'a>(key: &str, source: &'a str) -> Option<(Lang, std::borrow::Cow<'a, str>)> {
    let lang = Lang::for_source(key, source)?;
    Some(match script_view(source, lang) {
        Some((script, view)) => (script, std::borrow::Cow::Owned(view)),
        None => (lang, std::borrow::Cow::Borrowed(source)),
    })
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

/// Whether files of `key` have an outline: a language with an outline
/// query, or a component file, whose outline is its script's
/// ([`script_view`]).
pub fn has_outline(key: &str) -> bool {
    tags_for(key).is_some() || matches!(key, "vue" | "svelte" | "astro")
}

/// Whether `key` is a language of code — what the symbol index, the call
/// graph and Explain read — rather than a stylesheet, a page or data, whose
/// outline is for reading that file (a stylesheet's rules, a JSON file's
/// keys), not symbols for the project's.
pub fn is_code(key: &str) -> bool {
    has_outline(key) && !matches!(key, "json" | "css" | "scss" | "less" | "html")
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
    // A C++ class's export macro (`class SPDLOG_API logger {`) read as its
    // name made the class a function and its members no class's: parsed
    // as blanks, every byte where it was, so the text the tree's ranges
    // point into is the source's own.
    match (lang == Lang::Cpp)
        .then(|| without_export_macros(source))
        .flatten()
    {
        Some(blanked) => parser.parse(&blanked, None),
        None => parser.parse(source, None),
    }
}

/// `source` with each export macro between `class`, `struct` or `union`
/// and the type's name blanked — an all-capitals word (`SPDLOG_API`,
/// `Q_CORE_EXPORT`, `FMT_API`) a name, a base list or a body follows —
/// or `None` where there is none.
fn without_export_macros(source: &str) -> Option<String> {
    let bytes = source.as_bytes();
    let word = |c: u8| c == b'_' || c.is_ascii_alphanumeric();
    let mut spans: Vec<(usize, usize)> = Vec::new();
    for keyword in ["class ", "struct ", "union "] {
        for (at, _) in source.match_indices(keyword) {
            if at > 0 && word(bytes[at - 1]) {
                continue;
            }
            let mut i = at + keyword.len();
            while i < bytes.len() && bytes[i] == b' ' {
                i += 1;
            }
            let start = i;
            while i < bytes.len() && word(bytes[i]) {
                i += 1;
            }
            let macro_word = &source[start..i];
            let capitals = macro_word.len() > 2
                && macro_word
                    .bytes()
                    .all(|c| c == b'_' || c.is_ascii_uppercase() || c.is_ascii_digit())
                && macro_word.bytes().any(|c| c == b'_');
            if !capitals {
                continue;
            }
            // A name follows it, then a base list, `final` or a body.
            let mut j = i;
            while j < bytes.len() && bytes[j] == b' ' {
                j += 1;
            }
            let name_start = j;
            while j < bytes.len() && word(bytes[j]) {
                j += 1;
            }
            if j == name_start {
                continue;
            }
            let rest = source[j..].trim_start();
            if rest.starts_with(['{', ':']) || rest.starts_with("final") || rest.starts_with('<') {
                spans.push((start, i));
            }
        }
    }
    // A function's specifier macros before its type, at a line's start
    // (`SPDLOG_INLINE std::shared_ptr<logger> get(…)`, `FMT_CONSTEXPR20
    // FMT_INLINE auto size() const`): blanked too, a word following each.
    let mut line_start = 0;
    for line in source.split_inclusive('\n') {
        let mut i = line_start + (line.len() - line.trim_start().len());
        // (After a template's header on the line: `template <typename...
        // T> FMT_CONSTEXPR void ignore_unused(…)`.)
        if let Some(header) = line.trim_start().strip_prefix("template")
            && header.trim_start().starts_with('<')
        {
            let mut depth = 0usize;
            let open = line.len() - header.len();
            for (k, c) in line[open..].char_indices() {
                match c {
                    '<' => depth += 1,
                    '>' => {
                        depth = depth.saturating_sub(1);
                        if depth == 0 {
                            let after = open + k + 1;
                            let skip = line[after..].len() - line[after..].trim_start().len();
                            i = line_start + after + skip;
                            break;
                        }
                    }
                    _ => {}
                }
            }
        }
        loop {
            let start = i;
            while i < bytes.len() && word(bytes[i]) {
                i += 1;
            }
            let w = &source[start..i];
            let capitals = w.len() > 2
                && w.bytes()
                    .all(|c| c == b'_' || c.is_ascii_uppercase() || c.is_ascii_digit())
                && w.bytes().any(|c| c == b'_')
                && w.bytes().next().is_some_and(|c| c.is_ascii_uppercase());
            if !capitals || i >= bytes.len() || bytes[i] != b' ' {
                break;
            }
            let mut j = i;
            while j < bytes.len() && bytes[j] == b' ' {
                j += 1;
            }
            // A type or a name follows (not `(`, `=` or `;`: a macro's call
            // or a value).
            if j >= bytes.len() || !(word(bytes[j]) || bytes[j] == b':') {
                break;
            }
            spans.push((start, i));
            i = j;
        }
        line_start += line.len();
    }
    // A macro standing alone on its line before a declaration
    // (`SPDLOG_NAMESPACE_BEGIN` above `class SPDLOG_API logger {`,
    // `FMT_BEGIN_NAMESPACE`): the namespace it opens is no type of the
    // class's — read so, the class was a function's return type and its
    // members no class's. (An enumerator stands alone only before `}` or
    // another.)
    let lines: Vec<(usize, &str)> = {
        let mut at = 0;
        source
            .split_inclusive('\n')
            .map(|line| {
                let start = at;
                at += line.len();
                (start, line)
            })
            .collect()
    };
    const DECLARATION: &[&str] = &[
        "class ",
        "struct ",
        "union ",
        "namespace ",
        "template",
        "enum ",
        "typedef ",
        "using ",
        "inline ",
        "static ",
        "extern ",
        "constexpr ",
        "#",
    ];
    // So is a macro called alone on its line with no `;` before one
    // (`FMT_PRAGMA_GCC(push_options)`, `FMT_PRAGMA_MSVC(warning(disable :
    // 4702))`): a statement of the preprocessor's, no C++ the grammar reads.
    // (`TEST(Cart, Adds)` has its body after it.)
    let capitals = |w: &str| {
        w.len() > 2
            && w.bytes()
                .all(|c| c == b'_' || c.is_ascii_uppercase() || c.is_ascii_digit())
            && w.bytes().any(|c| c == b'_')
            && w.bytes().next().is_some_and(|c| c.is_ascii_uppercase())
    };
    let alone = |w: &str| -> bool {
        if capitals(w) {
            return true;
        }
        let Some((name, rest)) = w.split_once('(') else {
            return false;
        };
        if !capitals(name) || !w.ends_with(')') {
            return false;
        }
        let mut depth = 1i32;
        for c in rest.chars() {
            match c {
                '(' => depth += 1,
                ')' => depth -= 1,
                _ => {}
            }
        }
        depth == 0
    };
    for (n, &(start, line)) in lines.iter().enumerate() {
        let w = line.trim();
        if !alone(w) || line.trim_end().ends_with('\\') {
            continue;
        }
        // (Past comments: a doc comment's `/**`, `*` and `*/` lines.)
        let next = lines[n + 1..].iter().map(|(_, l)| l.trim()).find(|l| {
            !l.is_empty() && !l.starts_with("//") && !l.starts_with("/*") && !l.starts_with('*')
        });
        if next.is_none_or(|l| DECLARATION.iter().any(|d| l.starts_with(d)) || alone(l)) {
            let at = start + (line.len() - line.trim_start().len());
            spans.push((at, at + w.len()));
        }
    }
    if spans.is_empty() {
        return None;
    }
    let mut out = source.as_bytes().to_vec();
    for (start, end) in spans {
        out[start..end].fill(b' ');
    }
    String::from_utf8(out).ok()
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
            let query = rename_captures(&lang.highlight_parts().concat(), lang.capture_names());
            let injections = lang.injection_parts().concat();
            HighlightConfiguration::new(lang.grammar(), lang.name(), &query, &injections, "")
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

/// Normalize a text fragment for display: strip CR, expand tabs, and draw a
/// character that would rearrange or end the line as U+FFFD, one for one,
/// so the columns stay those of the line's chars.
///
/// - A line break other than `\n` (a form feed, U+001C–U+001E, NEL, U+2028,
///   U+2029) ends the shaper's paragraph there: a code line is one row, and
///   what followed it was not drawn — code a file could hide in plain sight.
/// - A bidi embedding, override or isolate (U+202A–U+202E, U+2066–U+2069) and
///   the invisible direction marks reorder how the code around them reads
///   (the "Trojan Source" attack): shown, they cannot.
fn clean(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\r' => {}
            '\t' => out.push_str("    "),
            c if rearranges_text(c) => out.push('\u{FFFD}'),
            c => out.push(c),
        }
    }
    out
}

/// `text` — a file's name, say — with what would rearrange or end it
/// ([`rearranges_text`], `\n` and `\r` too) drawn as U+FFFD, one for one: a
/// name holding a direction override reads in another order than it is
/// (`evil\u{202E}txt.exe` showed as `evilexe.txt`), and one holding a line
/// break (`a.rs\nb.rs`, a valid file name) as two.
pub fn in_order(text: &str) -> std::borrow::Cow<'_, str> {
    let breaks = |c: char| rearranges_text(c) || matches!(c, '\n' | '\r');
    if !text.chars().any(breaks) {
        return std::borrow::Cow::Borrowed(text);
    }
    std::borrow::Cow::Owned(
        text.chars()
            .map(|c| if breaks(c) { '\u{FFFD}' } else { c })
            .collect(),
    )
}

/// A line of a file shown outside the code view — in a diff, a search or
/// reference preview — as the code view draws it ([`clean`]): its `\r`
/// dropped (the end of a CRLF line, or one in the middle that split the
/// line where the code view shows none), and what else would rearrange it
/// drawn as U+FFFD ([`in_order`]).
pub fn line_in_order(line: &str) -> std::borrow::Cow<'_, str> {
    match line.contains('\r') {
        true => std::borrow::Cow::Owned(in_order(&line.replace('\r', "")).into_owned()),
        false => in_order(line),
    }
}

/// Whether `c` ends a line of text where it stands (a mandatory break, `\n`
/// and `\r` aside) or steers the direction of the text around it — what
/// display text must not carry as itself (see [`clean`]).
pub fn rearranges_text(c: char) -> bool {
    matches!(
        c,
        '\u{0B}'
            | '\u{0C}'
            | '\u{1C}'..='\u{1E}'
            | '\u{85}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{061C}'
            | '\u{200E}'
            | '\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2066}'..='\u{2069}'
    )
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
    let Some(lang) = lang_key.and_then(|key| Lang::for_source(key, source)) else {
        return plain_lines(source);
    };
    let Some(config) = config_for(lang) else {
        return plain_lines(source);
    };
    match lang {
        // One command at a time (see `crate::lean`): a command the grammar
        // cannot read would otherwise draw every line after it as one
        // expression.
        // Each read as the outline reads it (`crate::lean::readable`): an
        // attribute's arguments (`@[to_additive (attr := simp)]`) or an
        // instance's priority the grammar does not take, blanked for it —
        // the declaration's keyword and name were drawn as an expression's
        // words — and its text drawn as written.
        Lang::Lean => crate::lean::commands(source)
            .into_iter()
            .flat_map(|range| {
                let command = &source[range];
                let readable = crate::lean::readable(command);
                highlight_read_as(command, &readable, config)
                    .unwrap_or_else(|| plain_lines(command))
            })
            .collect(),
        Lang::Astro => highlight_astro(source, config),
        _ => highlight_with(source, config).unwrap_or_else(|| plain_lines(source)),
    }
}

/// The highlighting of code another language's carries ([`Lang::injection_parts`]),
/// by the name the carrier gives it: a key or fence tag (`ts`, `scss`,
/// `javascript`), or a CSS-in-JS tag. `None` — the code left as the
/// carrier's text — for a language Clew does not highlight (`stylus`, `pug`).
fn injected<'a>(name: &str) -> Option<&'a HighlightConfiguration> {
    let lang = match name.to_ascii_lowercase().as_str() {
        "keyframes" | "createglobalstyle" | "injectglobal" | "styled" => Lang::Css,
        "svg" => Lang::Html,
        name => Lang::from_key(lang_for_fence(name)?)?,
    };
    config_for(lang)
}

/// Where an Astro component's frontmatter ends: the byte after the line
/// that closes it, when the file opens with one (`---` on its first line,
/// blank lines aside, and again on a later one).
fn astro_frontmatter(source: &str) -> Option<(usize, usize, usize)> {
    let mut at = 0;
    let mut open = None;
    for line in source.split_inclusive('\n') {
        let start = at;
        at += line.len();
        let bare = line.trim_end_matches(['\n', '\r']).trim_end();
        match open {
            None if bare.trim().is_empty() => continue,
            None if bare == "---" => open = Some(at),
            None => return None,
            Some(inner) if bare == "---" => return Some((inner, start, at)),
            Some(_) => {}
        }
    }
    None
}

/// An Astro component highlighted: its frontmatter (between the `---`
/// lines) as TypeScript, and its markup as HTML, with its scripts and
/// styles each in their own language.
fn highlight_astro(source: &str, config: &HighlightConfiguration) -> Vec<HlLine> {
    let Some((inner, close, end)) = astro_frontmatter(source) else {
        return highlight_with(source, config).unwrap_or_else(|| plain_lines(source));
    };
    let delimiter = HIGHLIGHT_NAMES
        .iter()
        .position(|n| *n == "punctuation.delimiter")
        .map(|i| i as u8);
    let fence = |text: &str| HlLine {
        spans: vec![(clean(text.trim_end_matches(['\n', '\r'])), delimiter)],
    };
    let head = &source[..inner];
    let mut lines: Vec<HlLine> = plain_lines(&head[..head.trim_end_matches(['\n', '\r']).len()]);
    if let Some(last) = lines.pop() {
        // The blank lines before the opening `---`, then the `---` itself.
        lines.push(HlLine {
            spans: last
                .spans
                .into_iter()
                .map(|(text, _)| (text, delimiter))
                .collect(),
        });
    }
    let front = &source[inner..close];
    match config_for(Lang::TypeScript) {
        Some(ts) => lines.extend(highlight_with(front, ts).unwrap_or_else(|| plain_lines(front))),
        None => lines.extend(plain_lines(front)),
    }
    lines.push(fence(&source[close..end]));
    let rest = &source[end..];
    lines.extend(highlight_with(rest, config).unwrap_or_else(|| plain_lines(rest)));
    lines
}

fn highlight_with(source: &str, config: &HighlightConfiguration) -> Option<Vec<HlLine>> {
    highlight_read_as(source, source, config)
}

/// `source` highlighted as `read` (the same length, byte for byte) is: its
/// styles read off `read`, its text `source`'s own.
fn highlight_read_as(
    source: &str,
    read: &str,
    config: &HighlightConfiguration,
) -> Option<Vec<HlLine>> {
    if read.len() != source.len() {
        return None;
    }
    let mut highlighter = Highlighter::new();
    let events = highlighter
        .highlight(config, read.as_bytes(), None, injected)
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

/// Whether style `idx` is a comment's (a doc comment's too).
pub fn style_is_comment(idx: u8) -> bool {
    HIGHLIGHT_NAMES
        .get(idx as usize)
        .is_some_and(|name| name.starts_with("comment"))
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

    /// What a Rust span is called, by its text, on the line it is on.
    fn rust_kinds(src: &str) -> Vec<Vec<(String, &'static str)>> {
        highlight_lines(src, Some("rust"))
            .into_iter()
            .map(|l| {
                l.spans
                    .into_iter()
                    .filter(|(t, _)| !t.trim().is_empty())
                    .map(|(t, s)| (t, s.map_or("-", |i| HIGHLIGHT_NAMES[i as usize])))
                    .collect()
            })
            .collect()
    }

    /// Rust's spans tell apart what Clew's palette colours apart: a function
    /// where it is defined from one called, the module path before a name,
    /// a macro, and an attribute as one whole, brackets and all.
    #[test]
    fn rust_spans_tell_apart_what_the_palette_does() {
        let lines = rust_kinds(
            "#[cfg(not(windows))]\n\
             pub fn rel_string(rel: &std::path::Path) -> String {\n\
             \x20   if cfg!(windows) { std::env::var_os(v) } else { PathBuf::from(x) }\n\
             \x20   tokio::select! { _ = a => {} }\n\
             }\n",
        );
        let kind = |line: usize, text: &str| {
            lines[line]
                .iter()
                .filter(|(t, _)| t.trim() == text)
                .map(|(_, k)| *k)
                .collect::<Vec<_>>()
        };
        assert!(
            lines[0].iter().all(|(_, k)| *k == "attribute"),
            "{:?}",
            lines[0]
        );
        assert_eq!(kind(1, "rel_string"), ["function.definition"]);
        assert_eq!(kind(1, "std"), ["module"]);
        assert_eq!(kind(1, "path"), ["module"]);
        assert_eq!(kind(1, "::"), ["module", "module"]);
        assert_eq!(kind(1, "Path"), ["type"]);
        assert_eq!(kind(2, "cfg"), ["function.macro"]);
        assert_eq!(kind(2, "!"), ["function.macro"]);
        assert_eq!(kind(2, "var_os"), ["function"]);
        // A macro named by its path is a macro too.
        assert_eq!(kind(3, "select"), ["function.macro"]);
        assert_eq!(kind(3, "tokio"), ["module"]);
        // A type's `::` is not a module's.
        assert_eq!(kind(2, "::"), ["module", "module", "punctuation.delimiter"]);
    }

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

    /// What would end a code line where it stands, or reorder it, is drawn
    /// as U+FFFD, one for one: plain or highlighted, the line keeps its
    /// columns and shows everything it holds, in order.
    #[test]
    fn breaks_and_direction_controls_are_drawn_as_replacements() {
        let src = "let ok = check();\u{2029}evil(); // \u{202E}dlrow\u{2066}x\u{2069}\u{1C}\u{85}";
        let shown =
            "let ok = check();\u{FFFD}evil(); // \u{FFFD}dlrow\u{FFFD}x\u{FFFD}\u{FFFD}\u{FFFD}";
        for lines in [plain_lines(src), highlight_lines(src, Some("rust"))] {
            assert_eq!(lines.len(), 1);
            let text: String = lines[0].spans.iter().map(|(t, _)| t.as_str()).collect();
            assert_eq!(text, shown);
            assert_eq!(text.chars().count(), src.chars().count());
        }
        // Text that only reads right to left is left alone.
        assert_eq!(plain_lines("s = \"שלום\"")[0].spans[0].0, "s = \"שלום\"");
        // Names and previews shown outside the code view, the same way.
        assert_eq!(in_order("evil\u{202E}txt.exe"), "evil\u{FFFD}txt.exe");
        assert!(matches!(
            in_order("plain.rs"),
            std::borrow::Cow::Borrowed(_)
        ));
        // A name holding a line break is one name.
        assert_eq!(in_order("a.rs\nb.rs\r"), "a.rs\u{FFFD}b.rs\u{FFFD}");
        // A line drops its `\r`, as the code view does: the end of a CRLF
        // line is no character, and one inside does not split it.
        assert_eq!(line_in_order("a\rb\u{202E}c\r"), "ab\u{FFFD}c");
        assert!(matches!(
            line_in_order("plain"),
            std::borrow::Cow::Borrowed(_)
        ));
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

    /// The languages with grammars of their own are highlighted through and
    /// through: their keywords, functions, types, comments and strings each
    /// take a style of clew's.
    #[test]
    fn the_newer_languages_are_highlighted_through_and_through() {
        for (lang, source) in [
            ("php", include_str!("../tests/fixtures/langs/a.php")),
            ("ruby", include_str!("../tests/fixtures/langs/a.rb")),
            ("csharp", include_str!("../tests/fixtures/langs/a.cs")),
            ("erlang", include_str!("../tests/fixtures/langs/a.erl")),
            ("haskell", include_str!("../tests/fixtures/langs/a.hs")),
            ("zig", include_str!("../tests/fixtures/langs/a.zig")),
        ] {
            let lines = highlight_lines(source, static_key(lang));
            let styles: std::collections::BTreeSet<&str> = lines
                .iter()
                .flat_map(|l| &l.spans)
                .filter_map(|(_, s)| *s)
                .map(|s| {
                    HIGHLIGHT_NAMES[s as usize]
                        .split('.')
                        .next()
                        .unwrap_or_default()
                })
                .collect();
            for wanted in ["keyword", "comment", "string", "function"] {
                assert!(styles.contains(wanted), "{lang}: no {wanted} in {styles:?}");
            }
            assert!(
                styles.contains("type") || styles.contains("constructor"),
                "{lang}: no type in {styles:?}"
            );
        }
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
            ("sql", "SELECT 's{'; -- c{\n", Some("s{"), Some("-- c{")),
            // tree-sitter-lean 0.3.0 reads every `{` in a string as an
            // interpolation, `s!` or not, so `"s{"` does not parse; a `(` in
            // one is the bracket this checks.
            ("lean", "def s := \"s(\" -- c(\n", Some("s("), Some("-- c(")),
            (
                "php",
                "<?php $s = \"s{\"; // c{\n",
                Some("s{"),
                Some("// c{"),
            ),
            ("ruby", "s = \"s{\" # c{\n", Some("s{"), Some("# c{")),
            (
                "csharp",
                "class A { string s = \"s{\"; // c{\n}\n",
                Some("s{"),
                Some("// c{"),
            ),
            ("erlang", "s() -> \"s{\". % c{\n", Some("s{"), Some("% c{")),
            ("haskell", "s = \"s{\" -- c{\n", Some("s{"), Some("-- c{")),
            (
                "vue",
                "<template><p title=\"s{\">{{ 'i{' }}</p><!-- c{ --></template>\n<script>const t = \"j{\" // d{\n</script>\n",
                Some("s{"),
                Some("<!-- c{ -->"),
            ),
            // A `{` in a Svelte attribute's value opens an expression
            // (`title="a {b}"`), so a `(` is the bracket this checks there.
            (
                "svelte",
                "<p title=\"s(\">{'i{'}</p><!-- c{ -->\n<script>const t = \"j{\" // d{\n</script>\n",
                Some("s("),
                Some("<!-- c{ -->"),
            ),
            (
                "astro",
                "---\nconst t = \"j{\" // d{\n---\n<p title=\"s{\"></p><!-- c{ -->\n",
                Some("s{"),
                Some("<!-- c{ -->"),
            ),
            (
                "scss",
                "a { content: \"s{\"; } // c{\n",
                Some("s{"),
                Some("// c{"),
            ),
            (
                "less",
                "a { content: \"s{\"; } // c{\n",
                Some("s{"),
                Some("// c{"),
            ),
            (
                "kotlin",
                "val s = \"s{\" // c{\n",
                Some("s{"),
                Some("// c{"),
            ),
            ("scala", "val s = \"s{\" // c{\n", Some("s{"), Some("// c{")),
            ("elixir", "s = \"s{\" # c{\n", Some("s{"), Some("# c{")),
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

    /// The style index of a highlight name.
    fn style(name: &str) -> u8 {
        HIGHLIGHT_NAMES.iter().position(|n| *n == name).unwrap() as u8
    }

    /// SQL and Lean files are known by their extensions, and code fences by
    /// the tags models write for them.
    #[test]
    fn sql_and_lean_are_detected_by_path_and_fence() {
        assert_eq!(detect(Path::new("db/schema.sql")), Some("sql"));
        assert_eq!(detect(Path::new("init.PGSQL")), Some("sql"));
        assert_eq!(detect(Path::new("Mathlib/Order/Basic.lean")), Some("lean"));
        for tag in ["sql", "postgresql", "mysql", "sqlite", "plpgsql"] {
            assert_eq!(lang_for_fence(tag), Some("sql"), "{tag}");
        }
        assert_eq!(lang_for_fence("lean4"), Some("lean"));
        assert_eq!(lang_name("sql"), Some("SQL"));
        assert_eq!(lang_name("lean"), Some("Lean"));
    }

    /// Only whole capture names are renamed, an empty name drops the capture,
    /// and an `@` in a string literal is left alone.
    #[test]
    fn captures_are_renamed_by_their_whole_name() {
        let names = [("field", "property"), ("spell", ""), ("float", "number")];
        assert_eq!(
            rename_captures(
                "(a) @field (b) @fields \"@\" (c) @comment @spell (d) @float)",
                &names
            ),
            "(a) @property (b) @fields \"@\" (c) @comment  (d) @number)"
        );
    }

    /// tree-sitter-sequel's query, as clew loads it: a comment is a comment
    /// (its `@spell` capture, last, left it unstyled), numbers are numbers
    /// (its patterns for them never matched), booleans constants.
    #[test]
    fn sql_comments_numbers_and_booleans_are_styled() {
        let lines = highlight_lines(
            "-- totals\nSELECT 42, 2.5, 'x', TRUE FROM t WHERE a = 1;\n",
            Some("sql"),
        );
        assert_eq!(
            style_of(&lines, "-- totals"),
            Some(Some(style("comment"))),
            "{lines:?}"
        );
        assert_eq!(
            style_of(&lines, "42"),
            Some(Some(style("number"))),
            "{lines:?}"
        );
        assert_eq!(
            style_of(&lines, "2.5"),
            Some(Some(style("number"))),
            "{lines:?}"
        );
        assert_eq!(
            style_of(&lines, "'x'"),
            Some(Some(style("string"))),
            "{lines:?}"
        );
        assert_eq!(
            style_of(&lines, "TRUE"),
            Some(Some(style("constant.builtin"))),
            "{lines:?}"
        );
        assert_eq!(
            style_of(&lines, "SELECT"),
            Some(Some(style("keyword"))),
            "{lines:?}"
        );
    }

    /// Lean's declarations: their keyword, their name, a type, a comment.
    #[test]
    fn lean_keywords_and_names_are_styled() {
        let lines = highlight_lines(
            "-- note\ntheorem two : 1 + 1 = 2 := rfl\nstructure Point where\n  x : Nat\n",
            Some("lean"),
        );
        assert_eq!(
            style_of(&lines, "-- note"),
            Some(Some(style("comment"))),
            "{lines:?}"
        );
        assert_eq!(
            style_of(&lines, "theorem"),
            Some(Some(style("keyword"))),
            "{lines:?}"
        );
        assert_eq!(
            style_of(&lines, "two"),
            Some(Some(style("function"))),
            "{lines:?}"
        );
        assert_eq!(
            style_of(&lines, "Point"),
            Some(Some(style("type"))),
            "{lines:?}"
        );
        assert_eq!(
            style_of(&lines, "2"),
            Some(Some(style("number"))),
            "{lines:?}"
        );
    }

    /// A declaration carrying what the grammar does not take — attribute
    /// arguments, an instance's priority — and one in an indented `mutual`
    /// block: its keyword and its name drawn as such, its text as written.
    #[test]
    fn lean_declarations_with_attribute_arguments_are_styled() {
        let src = "@[to_additive (attr := simp)]\ntheorem a4 : True := trivial\n\
                   instance (priority := 100) a10 : Inhabited Nat := ⟨0⟩\n\
                   mutual\n  def isEven : Nat → Bool\n    | _ => true\nend\n";
        let lines = highlight_lines(src, Some("lean"));
        assert_eq!(
            style_of(&lines, "theorem"),
            Some(Some(style("keyword"))),
            "{lines:?}"
        );
        assert_eq!(
            style_of(&lines, "a4"),
            Some(Some(style("function"))),
            "{lines:?}"
        );
        assert_eq!(
            style_of(&lines, "a10"),
            Some(Some(style("function"))),
            "{lines:?}"
        );
        assert_eq!(
            style_of(&lines, "isEven"),
            Some(Some(style("function"))),
            "{lines:?}"
        );
        // Drawn as written: the arguments are there.
        let text: String = lines[0].spans.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(text, "@[to_additive (attr := simp)]");
        assert_eq!(lines.len(), src.lines().count());
    }

    /// Lean is highlighted a command at a time: a command the grammar does
    /// not know draws the lines after it as what they are, not as one
    /// expression. Every line is there, once.
    #[test]
    fn a_lean_command_the_grammar_cannot_read_does_not_spread() {
        let src = "assert_not_exists Field\n\ntheorem t : True := trivial\n-- done\n";
        let lines = highlight_lines(src, Some("lean"));
        assert_eq!(lines.len(), src.lines().count());
        assert_eq!(
            style_of(&lines, "theorem"),
            Some(Some(style("keyword"))),
            "{lines:?}"
        );
        assert_eq!(
            style_of(&lines, "-- done"),
            Some(Some(style("comment"))),
            "{lines:?}"
        );
        let text: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|(t, _)| t.as_str()).collect())
            .collect();
        assert_eq!(text, src.lines().collect::<Vec<_>>());
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

#[cfg(test)]
mod frontend_tests {
    use super::*;

    /// Each word of `src` read as `key` is, with the style it is drawn in.
    fn words(src: &str, key: &'static str) -> Vec<(String, &'static str)> {
        highlight_lines(src, Some(key))
            .into_iter()
            .flat_map(|l| l.spans)
            .filter(|(t, _)| !t.trim().is_empty())
            .map(|(t, s)| {
                (
                    t.trim().to_string(),
                    s.map_or("-", |i| HIGHLIGHT_NAMES[i as usize]),
                )
            })
            .collect()
    }

    /// The style of the first span reading exactly `word`.
    fn style(words: &[(String, &'static str)], word: &str) -> &'static str {
        words
            .iter()
            .find(|(t, _)| t == word)
            .unwrap_or_else(|| panic!("no span {word:?} in {words:?}"))
            .1
    }

    /// The styles of every span reading exactly `word`, in order.
    fn styles(words: &[(String, &'static str)], word: &str) -> Vec<&'static str> {
        words
            .iter()
            .filter(|(t, _)| t == word)
            .map(|(_, s)| *s)
            .collect()
    }

    #[test]
    fn a_tools_configuration_file_is_read_as_json_or_yaml_as_it_is_written() {
        assert_eq!(detect(Path::new("web/.babelrc")), Some("json"));
        assert_eq!(detect(Path::new(".eslintrc")), Some("json"));
        assert_eq!(detect(Path::new("settings.json5")), Some("json"));
        let json = "{\n  \"extends\": \"eslint:recommended\"\n}\n";
        let jsonc = "// the base\n{ \"semi\": false }\n";
        let yaml = "# shared\nextends: eslint:recommended\nrules:\n  semi: off\n";
        let list = "- a\n- b\n";
        assert_eq!(Lang::for_source("json", json), Some(Lang::Json));
        assert_eq!(Lang::for_source("json", jsonc), Some(Lang::Json));
        assert_eq!(Lang::for_source("json", "true\n"), Some(Lang::Json));
        assert_eq!(Lang::for_source("json", yaml), Some(Lang::Yaml));
        assert_eq!(Lang::for_source("json", list), Some(Lang::Yaml));
        assert_eq!(Lang::for_source("json", "---\na: 1\n"), Some(Lang::Yaml));
        let w = words(yaml, "json");
        assert_eq!(style(&w, "extends"), "property", "read as the YAML it is");
    }

    #[test]
    fn component_and_style_files_are_known_by_their_extensions() {
        for (file, key) in [
            ("App.vue", "vue"),
            ("Counter.svelte", "svelte"),
            ("index.astro", "astro"),
            ("theme.scss", "scss"),
            ("theme.less", "less"),
            ("vite.config.mts", "typescript"),
            ("server.cjs", "javascript"),
        ] {
            assert_eq!(detect(Path::new(file)), Some(key), "{file}");
        }
        for (fence, key) in [
            ("vue", "vue"),
            ("svelte", "svelte"),
            ("scss", "scss"),
            ("less", "less"),
            ("typescriptreact", "tsx"),
            ("javascriptreact", "javascript"),
            ("mjs", "javascript"),
            ("postcss", "css"),
        ] {
            assert_eq!(lang_for_fence(fence), Some(key), "{fence}");
        }
        for key in [
            "vue",
            "svelte",
            "astro",
            "scss",
            "less",
            "html",
            "javascript",
            "typescript",
            "tsx",
        ] {
            assert_eq!(highlight_error(key), None, "{key}");
        }
    }

    /// A Vue component reads as the three languages it is written in: its
    /// template's directives and expressions, its `<script setup lang="ts">`
    /// as TypeScript, and its `<style lang="scss">` as SCSS.
    #[test]
    fn a_vue_component_reads_as_its_template_script_and_style() {
        let w = words(
            "<template>\n  <button :title=\"label\" @click.prevent=\"save(item)\" v-if=\"open\">{{ count + 1 }}</button>\n</template>\n<script setup lang=\"ts\">\nlet count: number = 1\n</script>\n<style scoped lang=\"scss\">\n$c: red;\n.a { color: $c; }\n</style>\n",
            "vue",
        );
        assert_eq!(style(&w, "button"), "tag");
        assert_eq!(style(&w, "v-if"), "keyword", "a directive's name");
        assert_eq!(style(&w, "click"), "attribute", "the event it binds");
        assert_eq!(style(&w, "prevent"), "attribute", "its modifier");
        assert_eq!(
            style(&w, "save"),
            "function",
            "a directive's value is code, its call a call"
        );
        assert_eq!(style(&w, "label"), "variable");
        assert_eq!(style(&w, "{{"), "punctuation.special");
        assert_eq!(
            styles(&w, "count"),
            ["variable", "variable"],
            "an interpolation's name, and the script's"
        );
        assert_eq!(
            style(&w, "number"),
            "type.builtin",
            "lang=\"ts\" is TypeScript"
        );
        assert_eq!(
            styles(&w, "$c"),
            ["variable", "variable"],
            "lang=\"scss\" is SCSS"
        );
        assert_eq!(style(&w, "color"), "property");
    }

    #[test]
    fn a_svelte_component_reads_as_its_blocks_expressions_script_and_style() {
        let w = words(
            "<script lang=\"ts\">\nlet count: number = 0\n</script>\n{#if open}<p class={cls}>{count * 2}</p>{:else}<p>no</p>{/if}\n{#each items as item}{item.name}{/each}\n<style>p { color: red }</style>\n",
            "svelte",
        );
        assert_eq!(style(&w, "number"), "type.builtin");
        assert_eq!(styles(&w, "if"), ["keyword", "keyword"]);
        assert_eq!(style(&w, "each"), "keyword");
        assert_eq!(style(&w, "else"), "keyword");
        assert_eq!(style(&w, "cls"), "variable", "an attribute's {{value}}");
        assert_eq!(style(&w, "*"), "operator", "an expression tag's expression");
        assert_eq!(style(&w, "name"), "property");
        assert_eq!(style(&w, "color"), "property", "its <style> is CSS");
    }

    #[test]
    fn an_astro_components_frontmatter_is_typescript_and_its_script_too() {
        let src = "---\nimport Card from '../components/Card.astro'\nconst title: string = 'Hi'\n---\n<h1>{title}</h1>\n<script>\nlet n: number = 1\n</script>\n";
        let lines = highlight_lines(src, Some("astro"));
        assert_eq!(
            lines.len(),
            src.lines().count(),
            "one highlighted line per line"
        );
        let w = words(src, "astro");
        assert_eq!(
            styles(&w, "---"),
            ["punctuation.delimiter", "punctuation.delimiter"]
        );
        assert_eq!(style(&w, "import"), "keyword");
        assert_eq!(style(&w, "string"), "type.builtin");
        assert_eq!(style(&w, "h1"), "tag");
        assert_eq!(
            style(&w, "number"),
            "type.builtin",
            "an Astro <script> is TypeScript"
        );
        // No frontmatter: the whole file is markup.
        let w = words("<p>hi</p>\n", "astro");
        assert_eq!(style(&w, "p"), "tag");
    }

    /// A page's scripts and styles are each read as theirs: JavaScript, CSS,
    /// JSON for a data block; a template's text is left alone.
    #[test]
    fn a_pages_scripts_and_styles_are_read_as_their_languages() {
        let w = words(
            "<script>let x = 1</script>\n<style>a { color: red }</style>\n<script type=\"application/json\">{\"a\": 1}</script>\n<script type=\"text/template\"><b>x</b></script>\n<script type=\"module\">import a from 'b'</script>\n",
            "html",
        );
        assert_eq!(style(&w, "let"), "keyword");
        assert_eq!(style(&w, "color"), "property");
        assert_eq!(style(&w, "\"a\""), "string");
        assert_eq!(style(&w, "<b>x</b>"), "-", "a template's text stays text");
        assert_eq!(style(&w, "import"), "keyword", "a module is JavaScript");
    }

    /// CSS-in-JS, lit's html and sql templates are read as their languages,
    /// in any order in a file, a `${…}` in a declaration leaving it whole.
    #[test]
    fn tagged_templates_are_read_as_the_language_their_tag_names() {
        let w = words(
            "const t = html`<p class=\"a\">hi</p>`;\nconst B = styled.div`\n  color: red;\n  margin: ${(p) => p.m}px;\n`;\nconst q = sql`SELECT * FROM users WHERE id = ${id}`;\nconst g = css`display: flex;`;\nconst L = styled(Link).attrs({})`gap: 1px;`;\n",
            "tsx",
        );
        assert_eq!(style(&w, "p"), "tag", "html`…`");
        assert_eq!(style(&w, "color"), "property", "styled.div`…`");
        assert_eq!(
            style(&w, "margin"),
            "property",
            "a declaration a ${{…}} splits"
        );
        assert_eq!(style(&w, "SELECT"), "keyword", "sql`…`");
        assert_eq!(style(&w, "display"), "property", "css`…`");
        assert_eq!(style(&w, "gap"), "property", "styled(X).attrs(…)`…`");
        assert_eq!(style(&w, "m"), "property", "the ${{…}} stays code");
    }

    #[test]
    fn scss_and_less_read_their_own_syntax() {
        let w = words(
            "@use 'sass:math';\n$gap: 4px;\n@mixin pad($n) { padding: $n * $gap; }\n.card { @include pad(2); &__title { color: darken($c, 10%); } }\n// note\n",
            "scss",
        );
        assert_eq!(style(&w, "@use"), "keyword");
        assert_eq!(style(&w, "$gap"), "variable", "a variable declared");
        assert_eq!(style(&w, "@mixin"), "keyword");
        assert_eq!(style(&w, "@include"), "keyword");
        assert_eq!(style(&w, "darken"), "function");
        assert_eq!(style(&w, "// note"), "comment");
        let w = words(
            "@gap: 4px;\n.card { padding: @gap; &:hover { color: blue; } }\n// note\n",
            "less",
        );
        assert_eq!(styles(&w, "@gap"), ["variable", "variable"]);
        assert_eq!(style(&w, "padding"), "property");
        assert_eq!(style(&w, "// note"), "comment");
    }
}
