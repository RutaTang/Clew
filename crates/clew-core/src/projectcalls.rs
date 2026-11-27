//! Project-wide call graph, built offline from tree-sitter call sites resolved
//! by name against the symbol index.
//!
//! The LSP call hierarchy (the client's `callgraph`) is exact but per-symbol and
//! lazy — asking a server about every function in a large project would be far
//! too slow. This module trades that exactness for a whole-project view that is
//! computed locally and instantly: it finds each call *site* with tree-sitter,
//! reads off the enclosing function and the called name, and links caller to
//! callee by matching that name against the project's symbols.
//!
//! Name resolution is deliberately approximate — a call to `new` links to every
//! `new` in the project — so the graph is best read for its *aggregate* signal:
//! which functions nothing calls (entry points / dead-code candidates) and which
//! are called the most (hubs). Per-symbol precision is the LSP call graph's job.
//!
//! A function is identified everywhere by `(file, name, ordinal)` ([`SymKey`]):
//! the ordinal is its rank among the file's same-name callables by line, the
//! numbering `outline::fn_ordinals` defines and `explain::Node::Function`
//! uses. So two `new`s in one file are two nodes with two keys, in this graph,
//! in the LSP-precise edge set and in the explain engine alike.
//!
//! Lives in clew-core so it builds where the files live: the client builds it
//! directly for a local project; clew-server answers the `ProjectCalls`
//! request with it for a remote one, in its wire form
//! ([`clew_protocol::CallGraph`], project-relative paths — see
//! [`ProjectCallGraph::to_wire`] and [`ProjectCallGraph::from_wire`]).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use tree_sitter::{Node, Tree};

use crate::highlight::Lang;
use crate::outline::{Located, is_callable};

/// Whether a definition of `kind` is a node of the graph: a callable, or a
/// test or group of tests a runner's call opens (`it "adds" do`, `test
/// "adds" {`, `TEST(Cart, Adds)`, `describe`) — which no call reaches, but
/// whose calls are made from it. Without them a call in a test block had
/// no caller, and a function only tests call read "No callers".
pub fn is_node_kind(kind: &str) -> bool {
    is_callable(kind) || calls_only(kind)
}

/// A test or a group of tests: a node that calls, never called — no call
/// of a name lands on one, as a test's title is no function's name.
pub fn calls_only(kind: &str) -> bool {
    matches!(kind, "test" | "suite")
}

/// A stable identity for a function across edits: its file, its name, and
/// its ordinal among the file's same-name callables (by line). The line itself
/// is deliberately excluded so an edge survives lines shifting above it.
pub type SymKey = (PathBuf, String, u32);

/// The LSP-precise edge set, symbol-keyed so it can be patched incrementally as
/// files change without a full re-query.
pub type SymEdges = HashSet<(SymKey, SymKey)>;

/// A function/type definition the graph can link to (a filtered symbol-index
/// entry).
#[derive(Debug, Clone)]
pub struct Def {
    pub name: String,
    pub kind: String,
    pub file: PathBuf,
    pub line: usize,
}

/// A call site found in a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallSite {
    /// The innermost enclosing function that is a callable symbol of the file
    /// (see [`calls_in`]), if the call is inside one.
    pub caller: Option<String>,
    /// The called function/method name (the trailing identifier of the callee).
    pub callee: String,
    /// True when the callee is a `receiver.name(…)` access (a method call). Such
    /// a name belongs to the receiver's type, so it must not fall back to a
    /// globally-unique project function — that's how `.get()` on a std type used
    /// to be mis-attributed to a project `get`.
    pub method: bool,
    /// What a method call's receiver is known to be, where the code says:
    /// the caller's own class (`self.f()`, `this.f()`), or a type — a
    /// variable built by its constructor in the caller (`cart = Cart()`,
    /// `let c = Cart::new()`, `c := &Cart{}`), declared or annotated with it
    /// (`Cart c`, `c: Cart`), or the type itself (`Cart.create()`). `None`
    /// when unknown: the call then links to every method of the name.
    pub receiver: Option<Receiver>,
    /// The name a Scala method call is made on where its receiver is
    /// unknown: `os` in `os.read(p)` — a package, whose object `read` the
    /// call applies, or a value of no known type.
    pub qualifier: Option<String>,
    /// How many arguments an Erlang or Elixir call passes (`f(a, b)` two;
    /// `x |> f(a)` two; `&f/2`, `fun f/2` two): which of the name's
    /// functions it is. `None` elsewhere.
    pub arity: Option<u8>,
    /// What a Java, Kotlin, Scala, C# or C++ call passes, argument by
    /// argument, where any says ([`argument_kind`]): a literal's kind or a
    /// typed value's type, comma-separated — `s,i` for `put("a", 1)`,
    /// `TCart` for `add(cart)` with `Cart cart`. Which of same-arity
    /// overloads it can call. `None` elsewhere, or with an argument named.
    pub arguments: Option<String>,
    /// 1-based line of the call.
    pub line: usize,
}

/// The type a method call's receiver is known to be (see
/// [`CallSite::receiver`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Receiver {
    /// The class the calling method belongs to: `self`, `this`, `$this`.
    Own,
    /// A type, by its name as its definition gives it.
    Type(String),
    /// A type the receiver is declared as — a parameter's, a field's, a
    /// local's (`Owner owner`, `owners: OwnerRepository`) — in a language
    /// with inheritance, where it may hold a subtype: the call is the
    /// type's method, its own or the one it inherits, or a subtype's that
    /// overrides it.
    Declared(String),
}

/// What a file says of where its code lives and of the types it declares:
/// its package (Go's, Java's, Kotlin's, Scala's) or namespace (C#'s), whose
/// other files' names it reaches with no import; and each type it declares
/// with the names its declaration says it extends, implements or embeds
/// ([`crate::heritage::inherited_names`]) — the methods a value of the type
/// has from its bases.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileContext {
    pub package: Option<String>,
    /// The packages a Scala file's chained package clauses open before its
    /// own (`cats.effect` for `package cats.effect` / `package example`):
    /// their members are in scope with no import, as its own package's are.
    pub enclosing: Vec<String>,
    pub types: Vec<(String, Vec<String>)>,
    /// Definition lines of extension methods — C#'s `this` parameter's,
    /// Kotlin's `fun Type.name`, Dart's `extension … on`, Scala's
    /// `extension (x: T)` and implicit class's: a method a value of
    /// another type (a library's too) is called with.
    pub extensions: Vec<usize>,
    /// The modules an Elixir file `use`s (`use Hexpm.Context`, `use
    /// HexpmWeb, :controller`): what their `__using__` imports and aliases
    /// the file has too.
    pub uses: Vec<String>,
    /// The modules an Elixir file `import`s (in a `quote` too, as a
    /// `__using__` does): the only ones whose functions it calls bare — an
    /// `alias` names a module, and brings none of its functions.
    pub imports: Vec<String>,
    /// Each Erlang or Elixir function's arities, by its definition line:
    /// fewest and most arguments it takes (an Elixir default lets a call
    /// leave one out). A BEAM function is its name and its arity: `f/1` is
    /// no call of `f/2`.
    pub arities: Vec<(usize, u8, u8)>,
    /// Each function's declared return type, by its definition line, as
    /// its methods' class is named (`JsonAdapter` for `fun adapter():
    /// JsonAdapter<T>`): what a value assigned from a call of it is.
    pub returns: Vec<(usize, String)>,
    /// The kinds of each Java, Kotlin, Scala, C# and C++ function's
    /// parameters, by its definition line ([`parameter_kinds`]): what
    /// arguments it can take, to tell its overloads of one arity apart.
    pub parameters: Vec<(usize, String)>,
    /// The names of each Kotlin, Scala and C# function's parameters, by its
    /// definition line, comma-separated: which overloads a call naming an
    /// argument (`withTables(excludeSettings = …)`) can be of.
    pub parameter_names: Vec<(usize, String)>,
    /// A Go file's struct fields and their types (`("Context", "Request",
    /// "http.Request")`), pointers and embedding read through — a Zig
    /// file's, a C++ file's classes' (`("logger", "sinks_",
    /// "std::vector")`) alike: what a chain of fields from a value is
    /// ([`THROUGH`]).
    pub fields: Vec<(String, String, String)>,
    /// Definition lines of the functions a Scala, Kotlin, Java, C# or Dart
    /// file declares inside another (a local `def`, an anonymous class's
    /// method): no method call on some other value reaches them.
    pub nested: Vec<usize>,
    /// What a C++ literal converts to: each class's (struct's, enum's)
    /// name, and the kinds of the first parameter of each constructor of
    /// it a value converts by, comma-separated ([`cpp_conversions`]).
    pub converts: Vec<(String, String)>,
    /// What each Haskell definition's signature says its result is, by its
    /// line ([`haskell_signatures`]): a function's one type (`Pandoc`), a
    /// class method's `=` where it returns its class's parameter.
    pub signatures: Vec<(usize, String)>,
    /// Definition lines of a Haskell file's instances' methods (`instance
    /// HasMeta Pandoc where setMeta …`): no name's own definition, reached
    /// by a class method's call at their type.
    pub instance_methods: Vec<usize>,
    /// A Ruby file's singleton methods' lines (`def self.build`, a `def`
    /// in `class << self`): called on the class, not on its instances.
    pub singletons: Vec<usize>,
    /// The type aliases a file declares, each with the type it names
    /// ([`type_aliases`]): `memory_buffer` for `basic_memory_buffer` — one
    /// type, which a value of either passes for exactly.
    pub aliases: Vec<(String, String)>,
    /// A Ruby file's modules (`module Helpers`): what classes include, whose
    /// methods a bare call in any of them may be.
    pub modules: Vec<String>,
}

/// The fewest arguments of a Scala definition with no parameter list (`def
/// total: Int`, its most `NO_LIST` too, or only an `implicit`/`using` one,
/// its most that list's count), and what a Scala member read with no
/// parentheses passes (`r.exitCode`): neither is a call with `()`. As the
/// most of a fewest of 0: an `override def toString()`, which a read calls
/// too (see [`NodeFacts::takes`]).
const NO_LIST: u8 = u8::MAX - 1;

/// What a Scala member read passed as a function passes (`xs.map(Cart.total)`,
/// `laws.evalOn _`, `macro Impl.expand`): an eta-expansion of a method of any
/// parameters, or a parameterless one's value.
const ETA: u8 = u8::MAX - 2;

/// The mark a receiver's type starts with when the receiver was assigned
/// from a call (`val a = moshi.adapter()`): `RETURNED` and the function's
/// name, the type its definitions declare it returns, worked out once the
/// whole project is read.
pub const RETURNED: &str = "\u{2192}";

/// The mark a receiver's type starts with when the receiver is a chain of
/// fields from a value of a known type (Go's `c.Request.Header` with `c
/// *Context`): `THROUGH`, the value's type, then each field, `|`-separated
/// — the type of the last, worked out once every struct's fields are read.
pub const THROUGH: &str = "\u{21a6}";

/// The receiver's type for a call through `super` (Java, Kotlin, Scala,
/// Dart) or C#'s `base`: the caller's class's base's method, never its own
/// override.
pub const SUPER: &str = "\u{2191}";

/// The kinds of definition that are types another can extend or a value be
/// declared as.
fn is_type_kind(kind: &str, lang: Lang) -> bool {
    matches!(
        kind,
        "class" | "interface" | "struct" | "trait" | "type" | "enum" | "object" | "record"
    ) || (kind == "module" && matches!(lang, Lang::Ruby | Lang::Elixir))
        || (kind == "mixin" && lang == Lang::Dart)
}

/// The modules an Elixir file names after `keyword` (`use `, `import `) at
/// the start of a line: `import Shop.Cart` and the multi-alias form
/// `import Shop.{Cart, Order}` (over several lines too). A module computed
/// at compile time (`import unquote(__MODULE__)`) names none.
fn elixir_modules_after(src: &str, keyword: &str) -> Vec<String> {
    let src = src.strip_prefix('\u{feff}').unwrap_or(src);
    let is_name = |c: char| c.is_alphanumeric() || matches!(c, '_' | '.');
    let mut out = Vec::new();
    for (at, line) in src.split('\n').scan(0usize, |offset, line| {
        let at = *offset;
        *offset += line.len() + 1;
        Some((at, line))
    }) {
        let trimmed = line.trim_start();
        let Some(rest) = trimmed.strip_prefix(keyword) else {
            continue;
        };
        let rest = rest.trim_start();
        let head: String = rest.chars().take_while(|&c| is_name(c)).collect();
        if !head.starts_with(|c: char| c.is_uppercase()) {
            continue;
        }
        let after = &rest[head.len()..];
        if let Some(prefix) = head.strip_suffix('.')
            && after.starts_with('{')
        {
            // `Shop.{Cart, Order}`: up to the closing brace, which may be
            // lines below.
            let start = at + (line.len() - trimmed.len()) + keyword.len();
            let tail = &src[start..];
            let Some(open) = tail.find('{') else { continue };
            let Some(close) = tail[open..].find('}') else {
                continue;
            };
            for part in tail[open + 1..open + close].split(',') {
                let part = part.trim();
                if part.starts_with(|c: char| c.is_uppercase()) && part.chars().all(is_name) {
                    out.push(format!("{prefix}.{part}"));
                }
            }
        } else if !head.ends_with('.') {
            out.push(head);
        }
    }
    out
}

/// Whether a class is named as a test class is: `CartTest`, `CartTests`,
/// `CartSpec`, `CartSuite`, `CartIT`, `TestCart` (`Shop.CartTest` too).
fn is_test_class_name(owner: &str) -> bool {
    let name = owner.rsplit(['.', ':']).next().unwrap_or(owner);
    let after_test = name.strip_prefix("Test").map(|rest| rest.chars().next());
    matches!(after_test, Some(None)) // `Test` itself
        || matches!(after_test, Some(Some(c)) if c.is_uppercase() || c == '_')
        || ["Test", "Tests", "Spec", "Specs", "Suite"]
            .iter()
            .any(|suffix| name.len() > suffix.len() && name.ends_with(suffix))
        || name
            .strip_suffix("IT")
            .is_some_and(|rest| rest.ends_with(|c: char| c.is_lowercase()))
}

/// The scope a C++ call names its function in, by its last part: `Cart`
/// for `Cart::make()`, `fmt` for `fmt::format(…)`, `vector` for
/// `std::vector<int>::size_type(…)`.
fn cpp_call_scope(call: Node, src: &str) -> Option<String> {
    let function = call.child_by_field_name("function")?;
    if function.kind() != "qualified_identifier" {
        return None;
    }
    let text = node_text(function, src);
    let (scope, _) = text.rsplit_once("::")?;
    let scope = scope.split('<').next().unwrap_or(scope);
    let last = scope.rsplit("::").next().unwrap_or(scope).trim();
    (!last.is_empty()).then(|| last.to_string())
}

/// `dir` joined with the relative `path`, its `.` and `..` resolved by
/// name alone (`src/../t.zig` is `t.zig`), as an import names a file.
fn lexical_join(dir: &Path, path: &str) -> PathBuf {
    let mut out = dir.to_path_buf();
    for part in path.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            part => out.push(part),
        }
    }
    out
}

/// The files a project build names (its `pom.xml`, `build.gradle`,
/// `build.sbt`, `*.csproj`, `go.mod`, `pubspec.yaml`, `mix.exs`, …).
fn is_build_file(name: &str) -> bool {
    matches!(
        name,
        "pom.xml"
            | "build.gradle"
            | "build.gradle.kts"
            | "settings.gradle"
            | "settings.gradle.kts"
            | "build.sbt"
            | "build.sc"
            | "build.mill"
            | "go.mod"
            | "go.work"
            | "Cargo.toml"
            | "package.json"
            | "pubspec.yaml"
            | "mix.exs"
            | "rebar.config"
            | "cabal.project"
            | "stack.yaml"
            | "build.zig"
            | "Gemfile"
            | "CMakeLists.txt"
            | "meson.build"
            | "Makefile"
            | "WORKSPACE"
            | "MODULE.bazel"
            | "pyproject.toml"
            | "setup.py"
            | "composer.json"
            | "deno.json"
    ) || [".csproj", ".fsproj", ".sln", ".slnx", ".cabal", ".gemspec"]
        .iter()
        .any(|ext| name.ends_with(ext))
}

/// Each node's project, where the folder the files share holds several
/// that build alone ([`is_build_file`]): a file's is the topmost folder
/// under the shared one with a build file of its own, and two that import
/// from one another are one (a package and the examples that use it).
/// `None` where the shared folder builds the whole (one project, the
/// common case) or no build file is found; a node of no project is `None`.
fn independent_projects(
    nodes: &[SymNode],
    common: &Path,
    scope: &HashMap<PathBuf, HashSet<PathBuf>>,
) -> Option<Vec<Option<usize>>> {
    let mut builds: HashMap<PathBuf, bool> = HashMap::new();
    let mut has_build = |dir: &Path| -> bool {
        *builds.entry(dir.to_path_buf()).or_insert_with(|| {
            std::fs::read_dir(dir).is_ok_and(|entries| {
                entries
                    .flatten()
                    .any(|e| e.file_name().to_str().is_some_and(is_build_file))
            })
        })
    };
    if common.as_os_str().is_empty() || has_build(common) {
        return None;
    }
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut root_of_dir: HashMap<PathBuf, Option<usize>> = HashMap::new();
    let mut project_of = |file: &Path| -> Option<usize> {
        let dir = file.parent()?;
        if let Some(&known) = root_of_dir.get(dir) {
            return known;
        }
        let mut found = None;
        let mut at = common.to_path_buf();
        for part in dir.strip_prefix(common).ok()?.components() {
            at.push(part);
            if has_build(&at) {
                found = Some(at.clone());
                break;
            }
        }
        let id = found.map(|root| match roots.iter().position(|r| *r == root) {
            Some(i) => i,
            None => {
                roots.push(root);
                roots.len() - 1
            }
        });
        root_of_dir.insert(dir.to_path_buf(), id);
        id
    };
    let mut project: Vec<Option<usize>> = nodes.iter().map(|n| project_of(&n.file)).collect();
    // (Not a Java, Kotlin, Scala or C# file's: an import names a package or
    // namespace, which a sample beside it may declare too — and such builds
    // meant as one share a build file above them.)
    let by_package = |file: &Path| {
        file.extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| matches!(e, "java" | "kt" | "kts" | "scala" | "sc" | "cs"))
    };
    let pairs: Vec<(usize, usize)> = scope
        .iter()
        .filter(|(file, _)| !by_package(file))
        .flat_map(|(file, imported)| imported.iter().map(move |other| (file, other)))
        .filter_map(|(file, other)| Some((project_of(file)?, project_of(other)?)))
        .collect();
    if roots.len() < 2 {
        return None;
    }
    // One project, whichever imports from the other.
    let mut parent: Vec<usize> = (0..roots.len()).collect();
    fn find(parent: &mut [usize], mut i: usize) -> usize {
        while parent[i] != i {
            parent[i] = parent[parent[i]];
            i = parent[i];
        }
        i
    }
    for (a, b) in pairs {
        let (ra, rb) = (find(&mut parent, a), find(&mut parent, b));
        if ra != rb {
            parent[ra] = rb;
        }
    }
    for p in project.iter_mut().flatten() {
        *p = find(&mut parent, *p);
    }
    Some(project)
}

/// Whether a language's classes name every base they have (Java, Kotlin,
/// C#, Dart — a Dart mixin by `with`): a project class's methods are its
/// own and its project bases', or else a library's.
fn closed_hierarchy(lang: Lang) -> bool {
    matches!(lang, Lang::Java | Lang::Kotlin | Lang::CSharp | Lang::Dart)
}

/// Whether `path` names a Play template's package or a template —
/// `views.html`, `views.html.users.list`: names, the second a template
/// format.
fn template_path(path: &str) -> bool {
    let parts: Vec<&str> = path.split('.').collect();
    parts.len() >= 2
        && matches!(parts[1], "html" | "txt" | "xml" | "js")
        && parts.iter().all(|p| is_plain_name(p))
}

/// Whether `path` names a controller of Play's reverse router —
/// `routes.HomeController`, `controllers.routes.HomeController`,
/// `routes.javascript.HomeController`: generated, no project code.
fn reverse_route(path: &str) -> bool {
    let parts: Vec<&str> = path.split('.').collect();
    let Some(at) = parts.iter().position(|p| *p == "routes") else {
        return false;
    };
    let packages = &parts[..at];
    let rest = &parts[at + 1..];
    packages
        .iter()
        .all(|p| p.starts_with(char::is_lowercase) && is_plain_name(p))
        && match rest {
            [controller] => controller.starts_with(char::is_uppercase),
            ["javascript" | "ref", controller] => controller.starts_with(char::is_uppercase),
            _ => false,
        }
        && rest.iter().all(|p| is_plain_name(p))
}

/// The function a Play template compiles to, by its file
/// (`app/views/users/list.scala.html` is `views.html.users.list`: the
/// source root's first folder, the format, the rest, the name), or `None`
/// for no template.
fn template_name(file: &Path) -> Option<String> {
    let name = file.file_name()?.to_str()?;
    let (stem, format) = name.rsplit_once(".scala.")?;
    if !matches!(format, "html" | "txt" | "xml" | "js") {
        return None;
    }
    let folders: Vec<&str> = file
        .parent()?
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();
    // Play's `app/`, or sbt-twirl's `src/main/twirl/`.
    let root = folders
        .iter()
        .rposition(|f| matches!(*f, "app" | "twirl"))?;
    let (first, rest) = folders[root + 1..].split_first()?;
    let mut parts = vec![*first, format];
    parts.extend(rest);
    parts.push(stem);
    Some(parts.join("."))
}

/// Where a definition's parameter list is in its text ([`parameter_list`]).
enum Parameters {
    /// Its parentheses, open and close.
    At(usize, usize),
    /// None written (Scala's `def total: Int`): where its name ends.
    None(usize),
}

/// The parameter list of the definition of `name` in `text`: the first
/// parenthesis after the name (and past its generics, `<T>`, Scala's
/// `[T]`), with the parenthesis that closes it.
fn parameter_list(text: &str, name: &str, lang: Lang) -> Option<Parameters> {
    let name = name.rsplit(['.', ':']).next().unwrap_or(name);
    // An empty name (`Foo.`) is found everywhere and passes nothing.
    if name.is_empty() {
        return None;
    }
    let is_name = |c: char| c == '_' || c.is_alphanumeric();
    let mut from = 0;
    let open = loop {
        let at = from + text[from..].find(name)?;
        from = at + name.len();
        if text[..at].ends_with(is_name) || text[from..].starts_with(is_name) {
            continue;
        }
        let mut rest = text[from..].trim_start();
        if let Some(first) = rest.chars().next()
            && (first == '<' || (lang == Lang::Scala && first == '['))
        {
            let close = if first == '<' { '>' } else { ']' };
            let mut depth = 0i32;
            let end = rest.char_indices().find_map(|(i, c)| {
                if c == first {
                    depth += 1;
                } else if c == close {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i + 1);
                    }
                }
                None
            })?;
            rest = rest[end..].trim_start();
        }
        if rest.starts_with('(') {
            break text.len() - rest.len();
        }
        if lang == Lang::Scala && (rest.starts_with(':') || rest.starts_with('=')) {
            return Some(Parameters::None(text.len() - rest.len()));
        }
    };
    let mut depth = 0i32;
    let close = text[open..].char_indices().find_map(|(i, c)| {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + i);
                }
            }
            _ => {}
        }
        None
    })?;
    Some(Parameters::At(open, close))
}

/// The type a definition declares it returns, by the name its methods'
/// class has: Kotlin's and Scala's `): Foo`, Go's and Zig's type after the
/// parameters (`(*Foo, error)`, `!Foo`), Java's, C#'s, Dart's and C++'s
/// before the name (`std::shared_ptr<logger> make()`, a C++ `-> T`). A
/// pointer, a reference, an error union or Kotlin's `?` names its type; a
/// C++ smart pointer the type it points to. `None` for no type, a
/// primitive or a type parameter.
fn declared_return(src: &str, l: &Located, lang: Lang) -> Option<String> {
    let text = src.get(l.declaration.0..l.declaration.1)?;
    let name = l
        .symbol
        .name
        .rsplit(['.', ':'])
        .next()
        .unwrap_or(&l.symbol.name);
    let span = parameter_list(text, &l.symbol.name, lang)?;
    let after_params = match span {
        Parameters::At(_, close) => close + 1,
        Parameters::None(at) => at,
    };
    let raw: String = match lang {
        Lang::Kotlin | Lang::Scala => {
            let mut rest = &text[after_params..];
            // Scala's further parameter lists: `def f(a: A)(b: B): C`.
            while lang == Lang::Scala && rest.trim_start().starts_with('(') {
                let t = rest.trim_start();
                let mut depth = 0i32;
                let end = t.char_indices().find_map(|(i, c)| {
                    match c {
                        '(' => depth += 1,
                        ')' => {
                            depth -= 1;
                            if depth == 0 {
                                return Some(i + 1);
                            }
                        }
                        _ => {}
                    }
                    None
                })?;
                rest = &t[end..];
            }
            let rest = rest.trim_start();
            let Some(rest) = rest.strip_prefix(':') else {
                // No type written: what its expression body gives.
                return inferred_result(rest.strip_prefix('=')?, lang);
            };
            let end = rest.find(['{', '=', '\n']).unwrap_or(rest.len());
            rest[..end].trim().to_string()
        }
        Lang::Go | Lang::Zig => {
            let rest = &text[after_params..];
            let end = rest.find('{').unwrap_or(rest.len());
            let ty = rest[..end].trim();
            // Go's `(T, error)`: the first.
            let ty = ty
                .strip_prefix('(')
                .and_then(|t| t.split(',').next())
                .unwrap_or(ty);
            // Zig's `callconv(.c)`, an error set's `E!T`.
            let ty = ty.rsplit('!').next().unwrap_or(ty);
            // A Zig path through the file's own name for a part of the
            // standard library (`Io.net.Stream`): in full, for the aliases
            // to read ([`context_in`]).
            if lang == Lang::Zig
                && let Some(path) = zig_type_name(ty.trim())
                && path.contains('.')
            {
                return Some(path);
            }
            ty.trim().to_string()
        }
        Lang::Java | Lang::CSharp | Lang::Dart | Lang::Cpp => {
            // C++'s trailing `auto f() -> T`.
            let after = text[after_params..].trim_start();
            if lang == Lang::Cpp
                && let Some(t) = after
                    .trim_start_matches(|c: char| c.is_alphabetic() || c == ' ')
                    .strip_prefix("->")
            {
                let end = t.find(['{', ';']).unwrap_or(t.len());
                t[..end].trim().to_string()
            } else {
                let open = match span {
                    Parameters::At(open, _) => open,
                    Parameters::None(at) => at,
                };
                let mut head = text[..open].trim_end().strip_suffix(name)?.trim_end();
                // `Type Class::name`: the class is no part of the type.
                if let Some(qualified) = head.strip_suffix("::") {
                    head = qualified
                        .trim_end_matches(|c: char| c == '_' || c.is_alphanumeric())
                        .trim_end();
                }
                trailing_type_text(head)?.to_string()
            }
        }
        _ => return None,
    };
    return_type_name(&raw, lang)
}

/// What a Kotlin or Scala expression body with no type written gives
/// (`fun repo() = Repo(db)`, `def proc(cmd: Shellable*) = { … os.proc(cmd)
/// }`): the class it builds, or — [`RETURNED`] and the function's name —
/// what a call gives, worked out once the project is read (`proc` a case
/// class: what its `apply` builds). A block gives its last line; an `if`,
/// both its branches alike. `None` where they differ, or say nothing.
fn inferred_result(body: &str, lang: Lang) -> Option<String> {
    let body = body.trim();
    if let Some(block) = body.strip_prefix('{') {
        let block = block.strip_suffix('}')?.trim_end();
        let last = block.lines().rev().map(str::trim).find(|l| !l.is_empty())?;
        // A line that closes a branch of an `if`/`else`: the `if` it ends,
        // the last line to start one.
        if last.starts_with('}') || last.starts_with("else") {
            let mut offset = 0;
            let mut at = None;
            for line in block.split_inclusive('\n') {
                let t = line.trim_start();
                if t.starts_with("if ") || t.starts_with("if(") {
                    at = Some(offset + line.len() - t.len());
                }
                offset += line.len();
            }
            return inferred_result(&block[at?..], lang);
        }
        return inferred_result(last, lang);
    }
    if let Some(rest) = body.strip_prefix("if") {
        let rest = rest.trim_start();
        // Past the condition's parentheses, to its two branches.
        let mut depth = 0i32;
        let cond_end = rest.char_indices().find_map(|(i, c)| {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i + 1);
                    }
                }
                _ => {}
            }
            None
        })?;
        let branches = &rest[cond_end..];
        let mut depth = 0i32;
        let mut split = None;
        for (i, c) in branches.char_indices() {
            match c {
                '(' | '{' | '[' => depth += 1,
                ')' | '}' | ']' => depth -= 1,
                _ if depth == 0 && branches[i..].starts_with("else") => {
                    split = Some(i);
                    break;
                }
                _ => {}
            }
        }
        let split = split?;
        let (then, otherwise) = (&branches[..split], &branches[split + "else".len()..]);
        let a = inferred_result(then, lang)?;
        let b = inferred_result(otherwise, lang)?;
        return (a == b).then_some(a);
    }
    if let Some(ty) = constructed_type(body, lang) {
        return return_type_name(&ty, lang);
    }
    // (Not a `when`/`match` or a lambda: what each case gives is not read.)
    if body.starts_with(['{', '(']) || body.contains(" match ") || body.starts_with("when") {
        return None;
    }
    returned_call(body, lang).map(|function| format!("{RETURNED}{function}"))
}

/// The type written last in `head`, whole: `std::shared_ptr<logger>`,
/// `List<Cart>`, `const Cart&` — its template or generic arguments kept.
fn trailing_type_text(head: &str) -> Option<&str> {
    let head = head.trim_end().trim_end_matches(['*', '&', ' ', '?']);
    let mut end = head.len();
    let mut at = end;
    if head.ends_with('>') {
        let mut depth = 0i32;
        at = head.char_indices().rev().find_map(|(i, c)| {
            match c {
                '>' => depth += 1,
                '<' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                _ => {}
            }
            None
        })?;
    }
    let start = head[..at]
        .char_indices()
        .rev()
        .find(|&(_, c)| !(c == '_' || c == ':' || c == '.' || c.is_alphanumeric()))
        .map_or(0, |(i, c)| i + c.len_utf8());
    end = end.max(start);
    let text = head[start..end].trim();
    (!text.is_empty()).then_some(text)
}

/// The class a written return type names: past pointers, references,
/// nullability and `const`, a C++ smart pointer's pointee, a generic's
/// base (`List<Cart>` is `List`), a path's last part.
fn return_type_name(raw: &str, lang: Lang) -> Option<String> {
    let mut t = raw.trim();
    loop {
        let before = t;
        for p in [
            "const ", "*", "&", "?", "!", "[]", "mut ", "final ", "async ",
        ] {
            t = t.strip_prefix(p).unwrap_or(t).trim_start();
        }
        t = t.trim_end_matches(['?', '*', '&', ' ']);
        if t == before {
            break;
        }
    }
    // A smart pointer hands on its pointee's methods; a future its value's,
    // once awaited, as a call's result is where it is assigned.
    for wrapper in [
        "std::shared_ptr<",
        "std::unique_ptr<",
        "shared_ptr<",
        "unique_ptr<",
        "Future<",
        "FutureOr<",
        "Task<",
        "ValueTask<",
    ] {
        if let Some(inner) = t.strip_prefix(wrapper).and_then(|i| i.strip_suffix('>')) {
            return return_type_name(inner, lang);
        }
    }
    let base_end = t.find(['<', '[', '(', ' ']).unwrap_or(t.len());
    let base = &t[..base_end];
    // C++'s standard library's, whole (`std::string`): a value whose
    // methods are the library's.
    if lang == Lang::Cpp && base.starts_with("std::") && base.len() > "std::".len() {
        return Some(base.to_string());
    }
    // Go's of a package, with it (`flag.FlagSet`): another package's —
    // a library's or the project's — told apart once the project is read.
    if lang == Lang::Go
        && let Some((pkg, name)) = base.split_once('.')
        && is_plain_name(pkg)
        && is_plain_name(name)
        && name.starts_with(char::is_uppercase)
    {
        return Some(base.to_string());
    }
    let last = base.rsplit(['.', ':']).next().unwrap_or(base).trim();
    if last.chars().count() <= 1
        || !last.starts_with(|c: char| c == '_' || c.is_alphabetic())
        || !last.chars().all(|c| c == '_' || c.is_alphanumeric())
    {
        return None;
    }
    let primitive = matches!(
        last,
        "void"
            | "auto"
            | "var"
            | "dynamic"
            | "bool"
            | "int"
            | "long"
            | "short"
            | "char"
            | "float"
            | "double"
            | "byte"
            | "string"
            | "object"
            | "decimal"
            | "uint"
            | "usize"
            | "isize"
            | "u8"
            | "u16"
            | "u32"
            | "u64"
            | "i8"
            | "i16"
            | "i32"
            | "i64"
            | "f32"
            | "f64"
            | "error"
            | "anytype"
            | "noreturn"
            | "type"
            | "size_t"
            | "unsigned"
            | "signed"
            | "boolean"
            | "Unit"
            | "Nothing"
            | "Void"
    );
    // A lower-case name is a type only where types are written so.
    let typed_lower = matches!(lang, Lang::Go | Lang::Cpp | Lang::C | Lang::Zig);
    (!primitive && (typed_lower || last.starts_with(char::is_uppercase))).then(|| last.to_string())
}

/// The fewest and most arguments a function of a statically typed
/// language takes, read from its parameter list: a default (`b: Int = 0`,
/// `int b = 0`, Dart's `{…}` and `[…]`) may be left out, a variadic
/// (`String...`, `vararg`, `params`, `xs: Int*`, `...T`) takes any number
/// more; a C# extension's `this` and a Zig function's first parameter
/// are passed by the dot of a method call. A C++ definition's defaults may
/// be its prototype's, which the graph reads too. A Scala `def` with no
/// parameter list takes [`NO_LIST`]. `None` when no parameter list is found.
fn declared_arity(src: &str, l: &Located, lang: Lang) -> Option<(u8, u8)> {
    const ANY: u8 = u8::MAX;
    let text = src.get(l.declaration.0..l.declaration.1)?;
    let (open, close) = match parameter_list(text, &l.symbol.name, lang)? {
        Parameters::At(open, close) => (open, close),
        // Scala's `def total: Int`.
        Parameters::None(_) if lang == Lang::Scala => return Some((NO_LIST, NO_LIST)),
        Parameters::None(_) => return Some((0, 0)),
    };
    let inner = text[open + 1..close].trim();
    if lang == Lang::Scala {
        // `def f(implicit x: X, y: Y)`, `(using x: X)`: no list a call need
        // write — or its implicits, passed explicitly.
        if let Some(rest) = inner
            .strip_prefix("implicit ")
            .or_else(|| inner.strip_prefix("using "))
        {
            let passed = split_top_level(rest)
                .filter(|p| !p.trim().is_empty())
                .count();
            return Some((NO_LIST, u8::try_from(passed).unwrap_or(ETA).min(ETA - 1)));
        }
        // `override def toString()`: a Java method's override, which Scala
        // calls with no parentheses too.
        if inner.is_empty()
            && text
                .split_whitespace()
                .take_while(|w| *w != "def")
                .any(|w| w == "override")
        {
            return Some((0, NO_LIST));
        }
    }
    if inner.is_empty() || (matches!(lang, Lang::C | Lang::Cpp) && inner == "void") {
        return Some((0, 0));
    }
    let (mut fewest, mut most) = (0u32, 0u32);
    let mut any_default = false;
    let mut variadic = false;
    let mut passed_by_dot = false;
    for (i, part) in split_top_level(inner).enumerate() {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        // Dart's named `{…}` and optional `[…]` parameters.
        if lang == Lang::Dart && (part.starts_with('{') || part.starts_with('[')) {
            let named = part.starts_with('{');
            for p in split_top_level(&part[1..part.len().saturating_sub(1)]) {
                let p = p.trim();
                if p.is_empty() {
                    continue;
                }
                most += 1;
                if named && p.starts_with("required ") {
                    fewest += 1;
                }
            }
            continue;
        }
        let defaulted = {
            let mut depth = 0i32;
            part.char_indices().any(|(j, c)| {
                match c {
                    '(' | '[' | '{' | '<' => depth += 1,
                    ')' | ']' | '}' | '>' => depth -= 1,
                    '=' if depth <= 0 => {
                        let next = part[j + 1..].chars().next();
                        let prev = part[..j].chars().last();
                        return !matches!(next, Some('=' | '>'))
                            && !matches!(prev, Some('=' | '!' | '<' | '>'));
                    }
                    _ => {}
                }
                false
            })
        };
        let spread = spreads(part, lang)
            || part.starts_with("vararg ")
            || part.starts_with("params ")
            || (lang == Lang::Scala && part.trim_end().ends_with('*'));
        if i == 0 && lang == Lang::CSharp && part.starts_with("this ") {
            passed_by_dot = true;
        }
        // Zig passes a function's first argument by the dot of a method
        // call, whatever it is named (`analyser: *Analyser`).
        if i == 0 && lang == Lang::Zig {
            passed_by_dot = true;
        }
        if spread {
            variadic = true;
            continue;
        }
        most += 1;
        if !defaulted {
            fewest += 1;
        }
        any_default |= defaulted;
    }
    if passed_by_dot {
        fewest = fewest.saturating_sub(1);
    }
    // (A C++ definition's defaults may be its prototype's, elsewhere: the
    // graph gives it the fewest its declarations allow.)
    let _ = any_default;
    let most = if variadic { u32::from(ANY) } else { most };
    Some((
        u8::try_from(fewest).unwrap_or(ANY),
        u8::try_from(most).unwrap_or(ANY),
    ))
}

/// The definition lines of the callables inside another callable's span
/// ([`FileContext::nested`]).
fn nested_functions(symbols: &[Located]) -> Vec<usize> {
    let spans: Vec<(usize, usize)> = symbols
        .iter()
        .filter(|l| is_callable(&l.symbol.kind) || calls_only(&l.symbol.kind))
        .map(|l| (l.symbol.line, l.symbol.end_line.max(l.symbol.line)))
        .collect();
    symbols
        .iter()
        .filter(|l| is_callable(&l.symbol.kind))
        .filter(|l| {
            let (line, end) = (l.symbol.line, l.symbol.end_line.max(l.symbol.line));
            spans
                .iter()
                .any(|&(first, last)| first < line && end <= last && (first, last) != (line, end))
        })
        .map(|l| l.symbol.line)
        .collect()
}

/// A Zig file's container fields with their types ([`FileContext::fields`]):
/// a struct's `headers: std.StringHashMap([]const u8)` (`Response`,
/// `headers`, `std.StringHashMap`) — a file struct's own, under the name
/// `@This()` gives it — and the file's own variables, under no container's
/// name (`pub var arena = std.heap.ArenaAllocator.init(a);`).
fn zig_fields(src: &str, symbols: &[Located]) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    let field = |line: &str| -> Option<(String, String)> {
        let line = line.split("//").next()?.trim();
        let (name, rest) = line.split_once(':')?;
        let name = name.trim();
        if !is_plain_name(name) {
            return None;
        }
        let ty = rest.split(['=', ',']).next()?.trim();
        Some((name.to_string(), zig_type_name(ty)?))
    };
    for l in symbols
        .iter()
        .filter(|l| is_type_kind(&l.symbol.kind, Lang::Zig))
    {
        let Some(text) = src.get(l.declaration.0..l.declaration.1) else {
            continue;
        };
        let Some(open) = text.find('{') else {
            continue;
        };
        let mut depth = 0i32;
        for line in text[open..].lines() {
            let at_top = depth == 1;
            depth += line.matches('{').count() as i32 - line.matches('}').count() as i32;
            let t = line.trim_start();
            if at_top
                && !t.starts_with("pub ")
                && !t.starts_with("fn ")
                && !t.starts_with("const ")
                && let Some((name, ty)) = field(t)
            {
                out.push((l.symbol.name.clone(), name, ty));
            }
        }
    }
    // A file that is a struct (`const Server = @This();`, `pub const`
    // too): its own fields,
    // at the file's top level (`diagnostics_collection:
    // DiagnosticsCollection,`), under that name.
    let file_struct = src.lines().find_map(|line| {
        let line = line.trim();
        let rest = line
            .strip_prefix("pub ")
            .unwrap_or(line)
            .strip_prefix("const ")?;
        let (name, value) = rest.split_once('=')?;
        (value.trim().trim_end_matches(';').trim() == "@This()" && is_plain_name(name.trim()))
            .then(|| name.trim().to_string())
    });
    if let Some(file_struct) = file_struct {
        let mut depth = 0i32;
        for line in src.lines() {
            let at_top = depth == 0;
            let code = line.split("//").next().unwrap_or("");
            depth +=
                code.matches(['{', '(']).count() as i32 - code.matches(['}', ')']).count() as i32;
            let t = line.trim_start();
            if at_top
                && t.trim_end().ends_with(',')
                && ![
                    "pub ",
                    "fn ",
                    "const ",
                    "var ",
                    "test ",
                    "comptime",
                    "usingnamespace",
                    "extern ",
                ]
                .iter()
                .any(|w| t.starts_with(w))
                && let Some((name, ty)) = field(t)
            {
                out.push((file_struct.clone(), name, ty));
            }
        }
    }
    // The file's own variables: `var x: T`, `var x = T.init(…)`.
    for line in src.lines() {
        let t = line.strip_prefix("pub ").unwrap_or(line);
        let Some(rest) = t.strip_prefix("var ") else {
            continue;
        };
        if let Some((name, ty)) = field(rest) {
            out.push((String::new(), name, ty));
        } else if let Some((name, value)) = rest.split_once('=')
            && let Some((ty, _)) = value.trim().split_once(".init(")
            && is_plain_name(name.trim())
            && let Some(ty) = zig_type_name(ty)
        {
            out.push((String::new(), name.trim().to_string(), ty));
        }
    }
    // A type another file's re-exported (`pub const Request =
    // request.Request;`, `request` that file's `@import`): the struct.
    let imports: HashSet<&str> = src
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let rest = line
                .strip_prefix("pub ")
                .unwrap_or(line)
                .strip_prefix("const ")?;
            let (name, value) = rest.split_once('=')?;
            value
                .trim()
                .starts_with("@import(")
                .then_some(name.trim())
                .filter(|n| is_plain_name(n))
        })
        .collect();
    for line in src.lines() {
        let line = line.trim();
        let Some(rest) = line
            .strip_prefix("pub ")
            .unwrap_or(line)
            .strip_prefix("const ")
        else {
            continue;
        };
        let Some((name, value)) = rest.split_once('=') else {
            continue;
        };
        let (name, value) = (name.trim(), value.trim().trim_end_matches(';').trim());
        if let Some((module, ty)) = value.split_once('.')
            && imports.contains(module)
            && is_plain_name(name)
            && is_plain_name(ty)
            && ty.starts_with(char::is_uppercase)
        {
            out.push((String::new(), name.to_string(), ty.to_string()));
        }
    }
    out
}

/// A Go chain of fields with an index on a field (`c.Errors[0]`), each
/// index an element (`c.Errors.[]`); `None` for none, or for a chain of
/// anything else.
fn go_indexed_path(receiver: &str) -> Option<String> {
    if !receiver.contains('[') {
        return None;
    }
    let mut out = String::new();
    let mut chars = receiver.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '[' => {
                let mut depth = 1usize;
                for c in chars.by_ref() {
                    match c {
                        '[' => depth += 1,
                        ']' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                if depth != 0 || out.is_empty() || out.ends_with('.') {
                    return None;
                }
                out.push_str(".[]");
            }
            c if c == '_' || c == '.' || c.is_alphanumeric() => out.push(c),
            _ => return None,
        }
    }
    // (An index on the root is a variable's element, of a type unread.)
    let root = out.split('.').next()?;
    (!root.is_empty() && root != "[]" && out.split('.').nth(1) != Some("[]")).then_some(out)
}

/// The element type of Go's slice, array or map type `ty` (`*Error` of
/// `[]*Error`, `Foo` of `map[string]*Foo`), its pointer read through:
/// `None` for no such type, or an element of no plain type.
fn go_element(ty: &str) -> Option<&str> {
    let ty = ty.trim();
    let rest = if ty.starts_with('[') {
        &ty[ty.find(']')? + 1..]
    } else {
        let keyed = ty.strip_prefix("map[")?;
        let mut depth = 1usize;
        let close = keyed.char_indices().find_map(|(i, c)| {
            match c {
                '[' => depth += 1,
                ']' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                _ => {}
            }
            None
        })?;
        &keyed[close + 1..]
    };
    let element = rest.trim().trim_start_matches('*');
    (!element.is_empty()
        && element
            .chars()
            .all(|c| c == '_' || c == '.' || c.is_alphanumeric()))
    .then_some(element)
}

/// A Go file's struct fields with their types ([`FileContext::fields`]):
/// `Request *http.Request` is `Request` of type `http.Request`, `a, b int`
/// two, an embedded `*Engine` the field `Engine` — and `*Engine`, marking
/// it embedded, whose fields and methods the struct has; a field of a slice
/// or map (`Errors []*Error`) of type `[]Error`, and a named slice or map
/// type (`type errorMsgs []*Error`) the field `[]` of its element's type —
/// what an index (`c.Errors[0]`) is. An interface's methods are its
/// `Name()` fields, of the one type each returns (`Params() Params`), and
/// the interfaces it embeds (`http.ResponseWriter`) its `*ResponseWriter`.
/// A field of a function or inline struct type is none this follows.
fn go_struct_fields(src: &str, symbols: &[Located]) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    let simple = |t: &str| {
        let t = t.trim_start_matches('*');
        !t.is_empty()
            && t.chars()
                .all(|c| c == '_' || c == '.' || c.is_alphanumeric())
    };
    for l in symbols
        .iter()
        .filter(|l| is_type_kind(&l.symbol.kind, Lang::Go))
    {
        let Some(text) = src.get(l.declaration.0..l.declaration.1) else {
            continue;
        };
        // `type errorMsgs []*Error`: its elements.
        let spec = text.trim_start();
        let spec = spec.strip_prefix("type ").unwrap_or(spec).trim_start();
        if let Some(rest) = spec.strip_prefix(l.symbol.name.as_str())
            && let Some(element) = go_element(rest.lines().next().unwrap_or(""))
        {
            out.push((l.symbol.name.clone(), "[]".to_string(), element.to_string()));
            continue;
        }
        // `type ResponseWriter interface { http.ResponseWriter; Params()
        // Params }`.
        if let Some(rest) = spec.strip_prefix(l.symbol.name.as_str())
            && let Some(body) = rest.trim_start().strip_prefix("interface")
            && let Some(body) = body.trim_start().strip_prefix('{')
        {
            for line in body.lines() {
                let code = line.split("//").next().unwrap_or("").trim();
                let code = code.trim_end_matches('}').trim();
                if code.is_empty() {
                    continue;
                }
                if !code.contains('(') {
                    if simple(code) && !code.contains(char::is_whitespace) {
                        let ty = code.trim_start_matches('*');
                        let name = ty.rsplit('.').next().unwrap_or(ty);
                        out.push((l.symbol.name.clone(), format!("*{name}"), ty.to_string()));
                    }
                    continue;
                }
                // `Name(params) Result`: one result of a type alone.
                let Some((name, after)) = code.split_once('(') else {
                    continue;
                };
                let name = name.trim();
                let mut depth = 1i32;
                let close = after.char_indices().find_map(|(i, c)| {
                    match c {
                        '(' => depth += 1,
                        ')' => {
                            depth -= 1;
                            if depth == 0 {
                                return Some(i);
                            }
                        }
                        _ => {}
                    }
                    None
                });
                let Some(close) = close else {
                    continue;
                };
                let result = after[close + 1..].trim();
                if is_plain_name(name) && simple(result) {
                    out.push((
                        l.symbol.name.clone(),
                        format!("{name}()"),
                        result.trim_start_matches('*').to_string(),
                    ));
                }
            }
            continue;
        }
        let Some(open) = text
            .find("struct")
            .and_then(|at| text[at..].find('{').map(|b| at + b))
        else {
            continue;
        };
        let mut depth = 0i32;
        for line in text[open..].lines() {
            let code = line.split("//").next().unwrap_or("");
            let code = code.split('`').next().unwrap_or("").trim();
            let at_top = depth == 1;
            depth += code.matches('{').count() as i32 - code.matches('}').count() as i32;
            if !at_top || code.is_empty() || code.contains(['{', '}', '(']) {
                continue;
            }
            let tokens: Vec<&str> = code.split_whitespace().collect();
            // `Errors []*Error`, `byName map[string]*Route`: of its elements.
            if code.contains('[') {
                if let [names @ .., ty] = tokens.as_slice()
                    && !names.is_empty()
                    && let Some(element) = go_element(ty)
                {
                    for name in names.iter().flat_map(|n| n.split(',')) {
                        if is_plain_name(name) {
                            out.push((
                                l.symbol.name.clone(),
                                name.to_string(),
                                format!("[]{element}"),
                            ));
                        }
                    }
                }
                continue;
            }
            match tokens.as_slice() {
                [embedded] if simple(embedded) => {
                    let ty = embedded.trim_start_matches('*');
                    let name = ty.rsplit('.').next().unwrap_or(ty);
                    out.push((l.symbol.name.clone(), name.to_string(), ty.to_string()));
                    out.push((l.symbol.name.clone(), format!("*{name}"), ty.to_string()));
                }
                [names @ .., ty] if !names.is_empty() && simple(ty) => {
                    let ty = ty.trim_start_matches('*');
                    for name in names.iter().flat_map(|n| n.split(',')) {
                        if is_plain_name(name) {
                            out.push((l.symbol.name.clone(), name.to_string(), ty.to_string()));
                        }
                    }
                }
                _ => {}
            }
        }
    }
    out
}

/// [`go_import_names`] of `src`, read once per file (kept among the
/// `fields` it caches).
fn go_imports(src: &str, fields: &mut HashMap<(usize, String), Option<String>>) -> Vec<String> {
    fields
        .entry((usize::MAX - 1, String::new()))
        .or_insert_with(|| Some(go_import_names(src).join(" ")))
        .as_deref()
        .unwrap_or("")
        .split(' ')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// A Go file's package-level variables with their types, as fields of its
/// package ([`FileContext::fields`]): `var Form Binding = formBinding{}` is
/// `Form`, of the `formBinding` it is (a composite literal says the value's
/// type exactly, where the declaration names an interface), `var log =
/// &Logger{}` of `Logger`; one of no type this can read is none.
fn go_package_vars(src: &str, package: &str) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    let mut in_block = false;
    for line in src.lines() {
        let code = line.split("//").next().unwrap_or("").trim_end();
        let spec = if in_block {
            if code.trim_start().starts_with(')') {
                in_block = false;
                continue;
            }
            // Only the block's own lines, not a value's continuation.
            if !line.starts_with('\t') || line.starts_with("\t\t") {
                continue;
            }
            code.trim()
        } else if let Some(rest) = code.strip_prefix("var ") {
            let rest = rest.trim();
            if rest == "(" {
                in_block = true;
                continue;
            }
            rest
        } else {
            continue;
        };
        let (decl, value) = match spec.split_once('=') {
            Some((d, v)) => (d.trim(), Some(v.trim())),
            None => (spec, None),
        };
        let mut words = decl.split_whitespace();
        let Some(names) = words.next() else {
            continue;
        };
        let declared: Option<String> = words
            .next()
            .map(|t| t.trim_start_matches('*'))
            .filter(|t| {
                t.chars()
                    .all(|c| c == '_' || c == '.' || c.is_alphanumeric())
            })
            .map(str::to_string);
        // `formBinding{}`, `&Logger{…}`: the value's own type.
        let literal = value.and_then(|v| {
            let v = v.trim_start_matches('&');
            let end = v.find('{')?;
            let ty = &v[..end];
            (!ty.is_empty()
                && ty
                    .chars()
                    .all(|c| c == '_' || c == '.' || c.is_alphanumeric()))
            .then(|| ty.to_string())
        });
        let Some(ty) = literal.or(declared) else {
            continue;
        };
        for name in names.split(',').map(str::trim) {
            if is_plain_name(name) && name != "_" {
                out.push((package.to_string(), name.to_string(), ty.clone()));
            }
        }
    }
    out
}

/// The names a Go file calls its imports by: an import's alias, else its
/// path's last element — a major version's (`/v10`) or a `.vN` suffix
/// passed over (`validator`, `yaml`). A blank or dot import names none.
fn go_import_names(src: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut in_block = false;
    for line in src.lines() {
        let t = line.trim();
        let spec = if in_block {
            if t.starts_with(')') {
                in_block = false;
                continue;
            }
            t
        } else if let Some(rest) = t.strip_prefix("import") {
            let rest = rest.trim_start();
            if rest.starts_with('(') {
                in_block = true;
                continue;
            }
            rest
        } else if t.starts_with("func ") || t.starts_with("type ") || t.starts_with("var ") {
            break;
        } else {
            continue;
        };
        let Some(open) = spec.find('"') else { continue };
        let alias = spec[..open].trim();
        let Some(path) = spec[open + 1..].split('"').next() else {
            continue;
        };
        let name = if alias.is_empty() {
            let mut parts = path.rsplit('/');
            let mut last = parts.next().unwrap_or(path);
            if last.len() > 1
                && last.starts_with('v')
                && last[1..].chars().all(|c| c.is_ascii_digit())
            {
                last = parts.next().unwrap_or(last);
            }
            last.split(".v").next().unwrap_or(last).to_string()
        } else {
            alias.to_string()
        };
        if name != "_" && name != "." && !name.is_empty() {
            names.push(name);
        }
    }
    names
}

/// The modules a Ruby class or module mixes in: its body's `include`,
/// `extend` and `prepend` (`include Comparable, Helpers`) — whose methods
/// its own call with no receiver.
fn ruby_mixins(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in body.lines() {
        let line = line.trim();
        let Some(rest) = ["include ", "extend ", "prepend "]
            .iter()
            .find_map(|k| line.strip_prefix(k))
        else {
            continue;
        };
        for part in rest.split(',') {
            let name = part.trim().trim_end_matches(';');
            if !name.is_empty()
                && name.starts_with(|c: char| c.is_ascii_uppercase())
                && name
                    .chars()
                    .all(|c| c.is_alphanumeric() || c == '_' || c == ':')
            {
                out.push(name.rsplit("::").next().unwrap_or(name).to_string());
            }
        }
    }
    out
}

/// How C#'s header `text` writes the base `base`: with type arguments,
/// named with their count (`IRequest`1` for `IRequest<TResponse>`), or
/// without — both, where it writes both.
fn csharp_written_bases(text: &str, base: &str) -> Vec<String> {
    let header = text.split('{').next().unwrap_or(text);
    let header = header.split(" where ").next().unwrap_or(header);
    let Some((_, list)) = header.split_once(':') else {
        return vec![base.to_string()];
    };
    let mut out: Vec<String> = Vec::new();
    for part in split_top_level(list) {
        let part = part.trim();
        let name_end = part.find(['<', '(']).unwrap_or(part.len());
        let name = part[..name_end].trim();
        if name.rsplit('.').next() != Some(base) {
            continue;
        }
        let written = match generic_arity(&part[name_end..]) {
            Some(n) => format!("{base}`{n}"),
            None => base.to_string(),
        };
        if !out.contains(&written) {
            out.push(written);
        }
    }
    if out.is_empty() {
        out.push(base.to_string());
    }
    out
}

/// How many type arguments `rest` — what follows a type's name — gives it
/// (`<A, B>` gives 2); `None` for none.
fn generic_arity(rest: &str) -> Option<usize> {
    let inner = rest.trim_start().strip_prefix('<')?;
    let mut depth = 1usize;
    let mut count = 1usize;
    for c in inner.chars() {
        match c {
            '<' | '(' | '[' => depth += 1,
            '>' | ')' | ']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(count);
                }
            }
            ',' if depth == 1 => count += 1,
            _ => {}
        }
    }
    None
}

/// A type's name without the count of its type arguments (`IRequest` for
/// C#'s `IRequest`1`).
fn without_arity(name: &str) -> &str {
    name.split('`').next().unwrap_or(name)
}

/// The bases each C++ class template specialization in `src` names
/// (`template <> struct formatter<point> : nested_formatter<double> {` is
/// `formatter`'s `nested_formatter`).
fn cpp_specialization_bases(src: &str) -> Vec<(String, Vec<String>)> {
    let mut out = Vec::new();
    for line in src.lines() {
        let t = line.trim_start();
        let Some(rest) = t.strip_prefix("template") else {
            continue;
        };
        let Some((_, rest)) = rest.split_once('>') else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(rest) = rest
            .strip_prefix("struct ")
            .or_else(|| rest.strip_prefix("class "))
        else {
            continue;
        };
        let name: String = rest
            .trim_start()
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        let after = &rest.trim_start()[name.len()..];
        if name.is_empty() || !after.starts_with('<') {
            continue;
        }
        // Past the specialization's arguments: its bases, to the body.
        let mut depth = 0i32;
        let mut end = None;
        for (i, c) in after.char_indices() {
            match c {
                '<' => depth += 1,
                '>' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(i + 1);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(end) = end else {
            continue;
        };
        let header = after[end..].split('{').next().unwrap_or("");
        let Some(list) = header.trim_start().strip_prefix(':') else {
            continue;
        };
        let bases =
            crate::heritage::inherited_names(&format!("struct {name} : {list}"), Some("cpp"));
        let bases: Vec<String> = bases.into_iter().filter(|b| *b != name).collect();
        if !bases.is_empty() {
            out.push((name, bases));
        }
    }
    out
}

/// A file's [`FileContext`], read off its source and outline.
/// A file's types with its type aliases among them, each an alias of the
/// type it names as its one base: `using daily_file_sink_st =
/// daily_file_sink<null_mutex>;` (or its `typedef`), Kotlin's `typealias`,
/// Scala's `type`, Go's `type X = Y`. A value declared as the alias calls
/// the aliased type's methods, as a subtype calls its base's.
fn with_aliases(
    mut types: Vec<(String, Vec<String>)>,
    src: &str,
    lang: Lang,
) -> Vec<(String, Vec<String>)> {
    for (name, target) in type_aliases(src, lang) {
        match types.iter_mut().find(|(t, _)| *t == name) {
            Some((_, bases)) if !bases.contains(&target) => bases.push(target),
            Some(_) => {}
            None => types.push((name, vec![target])),
        }
    }
    types
}

/// The type aliases a file declares, one per line: each alias's name and
/// the last segment of the type it names, its type arguments dropped.
/// Function types (`typealias Handler = (Int) -> Unit`), a C function
/// pointer's `typedef` and Scala's `opaque type` name no type whose methods
/// the alias has.
fn type_aliases(src: &str, lang: Lang) -> Vec<(String, String)> {
    if !matches!(lang, Lang::Cpp | Lang::Kotlin | Lang::Scala | Lang::Go) {
        return Vec::new();
    }
    let identifier = |s: &str| {
        s.starts_with(|c: char| c == '_' || c.is_alphabetic())
            && s.chars().all(|c| c == '_' || c.is_alphanumeric())
    };
    let base_of = |target: &str| -> Option<String> {
        let mut target = target.trim();
        for prefix in ["typename ", "const ", "struct ", "class ", "*", "&"] {
            target = target.strip_prefix(prefix).unwrap_or(target).trim_start();
        }
        let path = target
            .split(['<', '[', '(', ' ', '*', '&', '?'])
            .next()
            .unwrap_or("");
        let last = path.rsplit([':', '.']).next().unwrap_or("");
        identifier(last).then(|| last.to_string())
    };
    let mut out = Vec::new();
    for line in src.lines() {
        let mut line = line.trim();
        for modifier in [
            "export ",
            "public ",
            "private ",
            "internal ",
            "protected ",
            "actual ",
        ] {
            line = line.strip_prefix(modifier).unwrap_or(line).trim_start();
        }
        let alias = match lang {
            Lang::Cpp if line.ends_with(';') => {
                let line = line.trim_end_matches(';').trim_end();
                if let Some(rest) = line.strip_prefix("using ") {
                    rest.split_once('=')
                } else if let Some(rest) = line.strip_prefix("typedef ") {
                    if rest.contains(['(', '{', ',']) {
                        continue;
                    }
                    rest.rsplit_once([' ', '*', '&'])
                        .map(|(target, name)| (name, target))
                } else {
                    None
                }
            }
            Lang::Kotlin => line
                .strip_prefix("typealias ")
                .and_then(|r| r.split_once('=')),
            Lang::Scala => line.strip_prefix("type ").and_then(|r| r.split_once('=')),
            Lang::Go => line.strip_prefix("type ").and_then(|r| r.split_once('=')),
            _ => None,
        };
        let Some((name, target)) = alias else {
            continue;
        };
        // Its own type parameters: `typealias Box<T> = …`, `type F[A] = …`.
        let name = name.split(['<', '[']).next().unwrap_or("").trim();
        if !identifier(name) || target.trim_start().starts_with('(') {
            continue;
        }
        if let Some(base) = base_of(target).filter(|b| b != name) {
            out.push((name.to_string(), base));
        }
    }
    out
}

pub fn context_in(src: &str, lang: Lang, symbols: &[Located]) -> FileContext {
    let types = symbols
        .iter()
        .filter(|l| is_type_kind(&l.symbol.kind, lang))
        .filter_map(|l| {
            // Go's embedded types are its body's members; elsewhere the
            // bases are in the header.
            let end = match lang {
                Lang::Go => l.declaration.1,
                _ => l.signature_end.max(l.declaration.0),
            };
            let text = src.get(l.declaration.0..end)?;
            let text = match lang {
                Lang::Go => text.replace('\n', ";"),
                _ => text.to_string(),
            };
            // An Elixir module inherits nothing: it is kept for its name.
            let mut bases: Vec<String> = match lang {
                Lang::Elixir => Vec::new(),
                _ => crate::heritage::inherited_names(&text, Some(lang.key())),
            };
            if lang == Lang::Ruby
                && let Some(body) = src.get(l.declaration.0..l.declaration.1)
            {
                bases.extend(ruby_mixins(body));
            }
            // A Kotlin class's name calls its companion object's members,
            // those it inherits too (`User.new { … }` with `companion
            // object : IntEntityClass<User>(Users)`).
            if lang == Lang::Kotlin
                && let Some(body) = src.get(l.declaration.0..l.declaration.1)
            {
                for (at, _) in body.match_indices("companion object") {
                    let header = &body[at + "companion ".len()..];
                    let header = &header[..header.find(['{', '\n']).unwrap_or(header.len())];
                    if header.contains(':') {
                        bases.extend(crate::heritage::inherited_names(header, Some(lang.key())));
                    }
                }
            }
            // C#'s `IRequest` and `IRequest<T>` are two types: a base
            // written with type arguments is named with their count
            // (`IRequest`1`), as overloads are told apart by it (see
            // `generic_arity`).
            if lang == Lang::CSharp {
                bases = bases
                    .into_iter()
                    .flat_map(|b| csharp_written_bases(&text, &b))
                    .collect();
            }
            bases.retain(|b| *b != l.symbol.name);
            bases.dedup();
            // A Kotlin name in backticks (`` `-JsonUtf8Reader` ``) by the
            // name its members' owner says (`JsonUtf8Reader`).
            let name = if l.symbol.name.contains('`') {
                l.symbol
                    .name
                    .chars()
                    .filter(|c| c.is_alphanumeric() || *c == '_')
                    .collect()
            } else {
                l.symbol.name.clone()
            };
            Some((name, bases))
        })
        .collect();
    let mut types = with_aliases(types, src, lang);
    // A C++ class template's specialization (`template <> struct
    // formatter<point> : nested_formatter<double> {`), no type of the
    // outline's: its bases are the template's too.
    if lang == Lang::Cpp {
        for (name, bases) in cpp_specialization_bases(src) {
            match types.iter_mut().find(|(t, _)| *t == name) {
                Some((_, known)) => {
                    for base in bases {
                        if !known.contains(&base) {
                            known.push(base);
                        }
                    }
                }
                None => types.push((name, bases)),
            }
        }
    }
    // The types whose methods extend another: Dart's `extension … on`,
    // Scala's `implicit class`.
    let extension_spans: Vec<(usize, usize)> = symbols
        .iter()
        .filter(|l| is_type_kind(&l.symbol.kind, lang) || l.symbol.kind == "extension")
        .filter(|l| {
            let head = src
                .get(l.declaration.0..l.signature_end.max(l.declaration.0))
                .unwrap_or("");
            match lang {
                Lang::Dart => head.trim_start().starts_with("extension"),
                Lang::Scala => head.contains("implicit class"),
                _ => false,
            }
        })
        .map(|l| l.declaration)
        .collect();
    let extensions = symbols
        .iter()
        .filter(|l| is_callable(&l.symbol.kind))
        .filter(|l| {
            let head = src
                .get(l.declaration.0..l.signature_end.max(l.declaration.0))
                .unwrap_or("");
            let within = |(a, b): &(usize, usize)| *a <= l.declaration.0 && l.declaration.1 <= *b;
            extension_spans.iter().any(within)
                || match lang {
                    Lang::CSharp => head.contains("(this ") || head.contains("( this "),
                    // `fun Type.name(`, `fun <T> List<T>.name(` — in the
                    // signature, before its body (`{`, `= …`): a body's
                    // `client.name(…)` makes no extension.
                    Lang::Kotlin => head.find("fun ").is_some_and(|fun| {
                        let signature = &head[fun..];
                        let end = [signature.find('{'), signature.find(" = ")]
                            .into_iter()
                            .flatten()
                            .min()
                            .unwrap_or(signature.len());
                        signature[..end].contains(&format!(".{}", l.symbol.name))
                    }),
                    // Scala 3's `extension (c: Circle)` above the `def`,
                    // with no top-level definition between them.
                    Lang::Scala => {
                        let from = src.floor_char_boundary(l.declaration.0.saturating_sub(400));
                        src.get(from..l.declaration.0)
                            .and_then(|before| before.rfind("extension").map(|at| &before[at..]))
                            .is_some_and(|since| {
                                !since.contains("\ndef ") && !since.contains("\nclass ")
                            })
                    }
                    _ => false,
                }
        })
        .map(|l| l.symbol.line)
        .collect();
    let modules_after = |keyword: &str| -> Vec<String> {
        match (lang, keyword) {
            (Lang::Elixir, _) => elixir_modules_after(src, keyword),
            (Lang::Go, "import ") => go_import_names(src),
            _ => Vec::new(),
        }
    };
    let arities = match lang {
        Lang::Erlang | Lang::Elixir => symbols
            .iter()
            .filter(|l| is_callable(&l.symbol.kind))
            .filter_map(|l| {
                let most = match lang {
                    Lang::Erlang => crate::outline::clause_arity(src, l)?,
                    _ => crate::outline::elixir_arity(src, l)?,
                };
                let defaults = match lang {
                    Lang::Elixir => crate::outline::elixir_defaults(src, l),
                    _ => 0,
                };
                let most = u8::try_from(most).ok()?;
                let fewest = most.saturating_sub(u8::try_from(defaults).unwrap_or(0));
                Some((l.symbol.line, fewest, most))
            })
            .collect(),
        // (Not C's: it has no overloads to choose among, and a C++ header
        // read as C says nothing sure of its arguments.)
        Lang::Java
        | Lang::Kotlin
        | Lang::Scala
        | Lang::CSharp
        | Lang::Dart
        | Lang::Cpp
        | Lang::Go
        | Lang::Zig => symbols
            .iter()
            .filter(|l| is_callable(&l.symbol.kind))
            .filter_map(|l| {
                let (fewest, most) = declared_arity(src, l, lang)?;
                Some((l.symbol.line, fewest, most))
            })
            .collect(),
        _ => Vec::new(),
    };
    // (Not Dart's, Go's or Zig's: they have no overloads.)
    let parameters = match lang {
        Lang::Java | Lang::Kotlin | Lang::Scala | Lang::CSharp | Lang::Cpp => symbols
            .iter()
            .filter(|l| is_callable(&l.symbol.kind))
            .filter_map(|l| Some((l.symbol.line, parameter_kinds(src, l, lang)?)))
            .collect(),
        _ => Vec::new(),
    };
    // (Where a call can name its arguments.)
    let parameter_names = match lang {
        Lang::Kotlin | Lang::Scala | Lang::CSharp => symbols
            .iter()
            .filter(|l| is_callable(&l.symbol.kind))
            .filter_map(|l| Some((l.symbol.line, parameter_names_of(src, l, lang)?)))
            .collect(),
        _ => Vec::new(),
    };
    let returns = match lang {
        Lang::Java
        | Lang::Kotlin
        | Lang::Scala
        | Lang::CSharp
        | Lang::Dart
        | Lang::Cpp
        | Lang::Go
        | Lang::Zig => symbols
            .iter()
            .filter(|l| is_callable(&l.symbol.kind))
            .filter_map(|l| Some((l.symbol.line, declared_return(src, l, lang)?)))
            .collect(),
        _ => Vec::new(),
    };
    // A Zig function's result of a standard-library type the file names by
    // an alias (`const Io = std.Io;`, `fn testStream() Io.net.Stream`): the
    // library's, by its path in full.
    let returns = if lang == Lang::Zig {
        let aliases: HashMap<&str, &str> = src
            .lines()
            .filter_map(|line| {
                let line = line.strip_prefix("pub ").unwrap_or(line);
                let (name, value) = line.strip_prefix("const ")?.split_once('=')?;
                let value = value.trim().strip_suffix(';')?.trim_end();
                (value == "std" || value.starts_with("std.")).then_some((name.trim(), value))
            })
            .collect();
        returns
            .into_iter()
            .map(|(line, ty)| {
                let (first, rest) = ty.split_once('.').unwrap_or((ty.as_str(), ""));
                match aliases.get(first) {
                    Some(path) if rest.is_empty() => (line, path.to_string()),
                    Some(path) => (line, format!("{path}.{rest}")),
                    // A project type by its own name (`analysis.Type` is a
                    // `Type`), the standard library's in full.
                    None if first == "std" => (line, ty),
                    None => (line, ty.rsplit('.').next().unwrap_or(&ty).to_string()),
                }
            })
            .collect()
    } else {
        returns
    };
    FileContext {
        package: package_of(src, lang),
        enclosing: enclosing_packages(src, lang),
        types,
        extensions,
        uses: modules_after("use "),
        imports: modules_after("import "),
        arities,
        returns,
        parameters,
        parameter_names,
        fields: match lang {
            Lang::Go => {
                let mut fields = go_struct_fields(src, symbols);
                if let Some(package) = package_of(src, lang) {
                    fields.extend(go_package_vars(src, &package));
                }
                fields
            }
            Lang::Zig => zig_fields(src, symbols),
            Lang::Cpp => cpp_class_fields(src, symbols),
            Lang::Scala => scala_class_fields(src, symbols),
            _ => Vec::new(),
        },
        nested: match lang {
            Lang::Scala | Lang::Kotlin | Lang::Java | Lang::CSharp | Lang::Dart => {
                nested_functions(symbols)
            }
            _ => Vec::new(),
        },
        converts: match lang {
            Lang::Cpp => cpp_conversions(src, symbols),
            _ => Vec::new(),
        },
        signatures: match lang {
            Lang::Haskell => haskell_signatures(src, symbols),
            _ => Vec::new(),
        },
        instance_methods: match lang {
            Lang::Haskell => haskell_instance_methods(src, symbols),
            _ => Vec::new(),
        },
        singletons: match lang {
            Lang::Ruby => ruby_singletons(src, symbols),
            _ => Vec::new(),
        },
        aliases: match lang {
            Lang::Cpp | Lang::Kotlin | Lang::Scala | Lang::CSharp | Lang::Java => {
                type_aliases(src, lang)
            }
            _ => Vec::new(),
        },
        modules: match lang {
            Lang::Ruby => symbols
                .iter()
                .filter(|l| l.symbol.kind == "module")
                .map(|l| l.symbol.name.clone())
                .collect(),
            _ => Vec::new(),
        },
    }
}

/// The lines of a Ruby file's singleton methods ([`FileContext::
/// singletons`]): a `def self.build` (or `def Job.build`), and each `def`
/// inside a `class << self … end`.
fn ruby_singletons(src: &str, symbols: &[Located]) -> Vec<usize> {
    let lines: Vec<&str> = src.lines().collect();
    let indent = |line: &str| line.len() - line.trim_start().len();
    // Each `class << self` block's lines, to its `end` at its indentation.
    let mut blocks: Vec<(usize, usize)> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let t = line.trim();
        if t == "class << self"
            || t.starts_with("class << self ")
            || t.starts_with("class << self;")
        {
            let end = (i + 1..lines.len())
                .find(|&j| {
                    indent(lines[j]) == indent(line) && lines[j].trim_start().starts_with("end")
                })
                .unwrap_or(lines.len());
            blocks.push((i + 1, end + 1));
        }
    }
    symbols
        .iter()
        .filter(|l| l.symbol.kind == "method")
        .filter(|l| {
            let head = src.get(l.declaration.0..l.declaration.1).unwrap_or("");
            let name = head
                .trim_start()
                .strip_prefix("def ")
                .map(str::trim_start)
                .and_then(|rest| rest.split(['(', ' ', '\n', ';', '=']).next())
                .unwrap_or("");
            let line = l.symbol.line;
            (name.contains('.') && !name.starts_with('.'))
                || blocks.iter().any(|&(a, b)| a < line && line < b)
        })
        .map(|l| l.symbol.line)
        .collect()
}

/// Whether the Ruby call `node` is inside a block (`do … end`, `{ … }`)
/// — within its method, which `instance_eval` or `class_eval` may run on
/// another object (Sinatra's `mock_app do get('/') { … } end`), or around
/// the method itself (a `def` in `Module.new { … }`, of no class's).
fn ruby_in_block(node: Node) -> bool {
    let mut at = node.parent();
    while let Some(n) = at {
        match n.kind() {
            "do_block" | "block" => return true,
            "class" | "module" => return false,
            _ => at = n.parent(),
        }
    }
    false
}

/// What a Ruby method call is made on, where its receiver says: the class
/// itself — a constant (`Job.kind`, `Shop::Job.kind`) or `self.class`
/// (`(false, "class")`, `(true, "class")` for the caller's own) — or an
/// instance the caller's own class builds (`new(*args).perform`,
/// `self.new.perform`: `(true, "new")`).
fn ruby_made_on(call: Node, callee: &str, src: &str) -> Option<(bool, &'static str)> {
    let receiver = receiver_text(node_text(call, src), callee)?.trim();
    if receiver == "self.class" {
        return Some((true, "class"));
    }
    let constant = receiver
        .trim_start_matches("::")
        .split("::")
        .all(|part| part.starts_with(char::is_uppercase) && is_plain_name(part));
    if constant {
        return Some((false, "class"));
    }
    let built = receiver.strip_prefix("self.").unwrap_or(receiver);
    let built = built
        .strip_prefix("new")
        .filter(|rest| rest.is_empty() || (rest.starts_with('(') && rest.ends_with(')')));
    built.map(|_| (true, "new"))
}

/// The definition lines of a Haskell file's instances' methods
/// ([`FileContext::instance_methods`]): the methods inside an `instance …
/// where` declaration's span.
fn haskell_instance_methods(src: &str, symbols: &[Located]) -> Vec<usize> {
    // Each instance declaration's byte span, read off its `instance` line
    // and the indented lines after it.
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut offset = 0;
    let mut open: Option<usize> = None;
    for line in src.split_inclusive('\n') {
        let top = !line.starts_with([' ', '\t']) && !line.trim().is_empty();
        if top {
            if let Some(start) = open.take() {
                spans.push((start, offset));
            }
            // (`instance` alone on its line, its head on the next.)
            if line.split_whitespace().next() == Some("instance") {
                open = Some(offset);
            }
        }
        offset += line.len();
    }
    if let Some(start) = open {
        spans.push((start, offset));
    }
    symbols
        .iter()
        .filter(|l| is_callable(&l.symbol.kind))
        .filter(|l| {
            spans
                .iter()
                .any(|&(a, b)| a <= l.declaration.0 && l.declaration.0 < b)
        })
        .map(|l| l.symbol.line)
        .collect()
}

/// What each Haskell definition's signature says its result is
/// ([`FileContext::signatures`]): a function's, the one type it returns
/// whole (`setAuthors :: [Inlines] -> Pandoc -> Pandoc`: `Pandoc`; none for
/// `Maybe (f a)` or a type variable); a single-parameter class's method
/// that returns that parameter (`setMeta :: Text -> b -> a -> a` in `class
/// HasMeta a`), `=`.
fn haskell_signatures(src: &str, symbols: &[Located]) -> Vec<(usize, String)> {
    let lines: Vec<&str> = src.lines().collect();
    let indent = |line: &str| line.len() - line.trim_start().len();
    // The result after a signature's context and its last arrow, whole.
    let result = |text: &str| -> Option<String> {
        let ty = text.split_once("::")?.1;
        let ty = ty.rsplit("=>").next().unwrap_or(ty);
        let ty = ty.rsplit("->").next().unwrap_or(ty).trim();
        let ty = ty.trim_start_matches('(').trim_end_matches(')').trim();
        let plain = !ty.is_empty()
            && ty
                .chars()
                .all(|c| c == '_' || c == '.' || c == '\'' || c.is_alphanumeric());
        plain.then(|| ty.rsplit('.').next().unwrap_or(ty).to_string())
    };
    // Each top-level class's line span and its one parameter, where one.
    let mut classes: Vec<(usize, usize, Option<String>)> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let Some(head) = line.strip_prefix("class ") else {
            continue;
        };
        let end = (i + 1..lines.len())
            .find(|&j| !lines[j].trim().is_empty() && indent(lines[j]) == 0)
            .unwrap_or(lines.len());
        let head = head.split(" where").next().unwrap_or(head);
        let head = head.rsplit("=>").next().unwrap_or(head);
        let words: Vec<&str> = head.split_whitespace().collect();
        let param = match words.as_slice() {
            [_, var] if var.starts_with(char::is_lowercase) => Some(var.to_string()),
            _ => None,
        };
        classes.push((i, end, param));
    }
    let mut out = Vec::new();
    for l in symbols
        .iter()
        .filter(|l| matches!(l.symbol.kind.as_str(), "function" | "method"))
    {
        let name = l.symbol.name.as_str();
        let at = l.symbol.line.saturating_sub(1);
        // A class's method signature: its own line(s).
        if let Some((_, _, param)) = classes.iter().find(|(a, b, _)| *a < at && at < *b) {
            let text: String = lines[at..]
                .iter()
                .take_while(|line| {
                    std::ptr::eq(*line, &lines[at]) || indent(line) > indent(lines[at])
                })
                .copied()
                .collect::<Vec<_>>()
                .join(" ");
            if param.is_some() && result(&text).as_deref() == param.as_deref() {
                out.push((l.symbol.line, "=".to_string()));
            }
            continue;
        }
        // A function's: the signature just above, past its continuation
        // lines, blank lines and comments — no other declaration between.
        let own = lines.get(at).map_or(0, |line| indent(line));
        let mut start = None;
        for i in (at.saturating_sub(40)..at).rev() {
            let line = lines[i];
            let t = line.trim_start();
            if t.strip_prefix(name)
                .is_some_and(|rest| rest.trim_start().starts_with("::"))
            {
                start = Some(i);
                break;
            }
            if !(t.is_empty() || t.starts_with("--") || t.starts_with("{-") || indent(line) > own) {
                break;
            }
        }
        let Some(start) = start else {
            continue;
        };
        let text: String = lines[start..at]
            .iter()
            .filter(|line| !line.trim_start().starts_with("--"))
            .copied()
            .collect::<Vec<_>>()
            .join(" ");
        if let Some(ty) = result(&text).filter(|t| t.starts_with(char::is_uppercase)) {
            out.push((l.symbol.line, ty));
        }
    }
    out
}

/// The package (namespace) a file declares: Go's `package cart`, Java's,
/// Kotlin's and Scala's `package shop.cart` (Scala's chained clauses
/// joined), C#'s `namespace Shop.Cart` (block or file-scoped).
fn package_of(src: &str, lang: Lang) -> Option<String> {
    let parts = package_clauses(src, lang);
    (!parts.is_empty()).then(|| parts.join("."))
}

/// The packages a Scala file's chained package clauses open before its
/// own: `cats.effect` for `package cats.effect` then `package example`,
/// whose members the file sees with no import ([`FileContext::enclosing`]).
fn enclosing_packages(src: &str, lang: Lang) -> Vec<String> {
    if lang != Lang::Scala {
        return Vec::new();
    }
    let parts = package_clauses(src, lang);
    (1..parts.len()).map(|n| parts[..n].join(".")).collect()
}

/// The names a file's package clauses give, in order: one (Go, Java,
/// Kotlin, C#'s namespace), or Scala's chained `package a.b` / `package c`.
fn package_clauses(src: &str, lang: Lang) -> Vec<String> {
    let keyword = match lang {
        Lang::Go | Lang::Java | Lang::Kotlin | Lang::Scala => "package",
        Lang::CSharp => "namespace",
        _ => return Vec::new(),
    };
    // A byte-order mark before the first line (Visual Studio writes one)
    // is no part of its text: `\u{feff}using System;` is a `using`.
    let src = src.strip_prefix('\u{feff}').unwrap_or(src);
    let mut parts: Vec<String> = Vec::new();
    let mut in_block_comment = false;
    for line in src.lines().take(2000) {
        let t = line.trim();
        if in_block_comment {
            in_block_comment = !t.contains("*/");
            continue;
        }
        if t.starts_with("/*") {
            in_block_comment = !t.contains("*/");
            continue;
        }
        if t.is_empty() || t.starts_with("//") || t.starts_with('@') || t.starts_with('#') {
            continue;
        }
        let Some(rest) = t
            .strip_prefix(keyword)
            .filter(|r| r.starts_with(char::is_whitespace))
        else {
            // Past the header: C#'s namespace may follow its usings.
            if lang == Lang::CSharp && (t.starts_with("using ") || t.starts_with("global using ")) {
                continue;
            }
            // Scala's imports may come before a package object
            // (`package os`, imports, `package object watch`).
            if lang == Lang::Scala && t.starts_with("import ") {
                continue;
            }
            break;
        };
        let rest = rest.trim_start();
        // Scala's `package object cart` is the package's own object.
        let rest = rest.strip_prefix("object ").unwrap_or(rest);
        let name: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || matches!(c, '_' | '.' | '`'))
            .filter(|c| *c != '`')
            .collect();
        if name.is_empty() {
            break;
        }
        parts.push(name);
        if lang != Lang::Scala {
            break;
        }
    }
    parts
}

/// A node in the project call graph: one function/method definition plus the
/// nodes that call it and that it calls.
#[derive(Debug, Clone)]
pub struct SymNode {
    pub name: String,
    pub kind: String,
    pub file: PathBuf,
    pub line: usize,
    callers: Vec<usize>,
    callees: Vec<usize>,
}

/// The graph. Its adjacency lists are indices into `nodes`, which every
/// accessor trusts — so a graph that comes from outside (the wire) is checked
/// on the way in ([`ProjectCallGraph::from_wire`]).
#[derive(Default, Clone)]
pub struct ProjectCallGraph {
    nodes: Vec<SymNode>,
    /// Each node's ordinal among its file's same-name callables — the third
    /// part of its [`SymKey`] — computed once, when the node set is fixed.
    ordinals: Vec<u32>,
    /// The nodes by their key's hash ([`key_hash`]), in node order: a lookup
    /// by key ([`ProjectCallGraph::id_of_key`]) read every node, and a view
    /// makes one per stack frame on every draw.
    by_key: HashMap<u64, Vec<usize>>,
}

/// The graph as its nodes say it: the index is theirs, and in no order a
/// graph built again would repeat.
impl std::fmt::Debug for ProjectCallGraph {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProjectCallGraph")
            .field("nodes", &self.nodes)
            .field("ordinals", &self.ordinals)
            .finish()
    }
}

/// The hash a node's `(file, name, ordinal)` key is indexed under.
fn key_hash(file: &Path, name: &str, ordinal: u32) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (file, name, ordinal).hash(&mut h);
    h.finish()
}

impl ProjectCallGraph {
    /// The graph over `nodes`, which the constructors below have finished.
    fn with_nodes(nodes: Vec<SymNode>) -> Self {
        let ordinals = ordinals_of(
            nodes
                .iter()
                .map(|n| (n.file.as_path(), n.name.as_str(), n.kind.as_str(), n.line)),
        );
        let mut by_key: HashMap<u64, Vec<usize>> = HashMap::new();
        for (i, n) in nodes.iter().enumerate() {
            by_key
                .entry(key_hash(&n.file, &n.name, ordinals[i]))
                .or_default()
                .push(i);
        }
        ProjectCallGraph {
            nodes,
            ordinals,
            by_key,
        }
    }
}

impl ProjectCallGraph {
    /// The wire form of this graph, each node's file mapped through `rel` —
    /// to the project-relative path the graph crosses the protocol in (a
    /// remote client must never hold a remote absolute path as anything but
    /// an identity).
    pub fn to_wire(&self, rel: impl Fn(&Path) -> String) -> clew_protocol::CallGraph {
        clew_protocol::CallGraph {
            nodes: self
                .nodes
                .iter()
                .map(|n| clew_protocol::CallGraphNode {
                    name: n.name.clone(),
                    kind: n.kind.clone(),
                    file: rel(&n.file),
                    line: n.line,
                    callers: n.callers.clone(),
                    callees: n.callees.clone(),
                })
                .collect(),
        }
    }

    /// The graph a wire [`clew_protocol::CallGraph`] describes, each node's
    /// file mapped through `abs` (back to this client's identities) —
    /// VALIDATED first. The graph comes from another process, and an
    /// out-of-range adjacency index used to panic the UI on the first frame
    /// that drew it; now it is an error the caller reports.
    pub fn from_wire(
        wire: clew_protocol::CallGraph,
        abs: impl Fn(&str) -> PathBuf,
    ) -> Result<Self, String> {
        let n = wire.nodes.len();
        for (i, node) in wire.nodes.iter().enumerate() {
            if let Some(bad) = node.callers.iter().chain(&node.callees).find(|&&j| j >= n) {
                return Err(format!(
                    "call graph node {i} links to node {bad}, but the graph has {n} nodes"
                ));
            }
        }
        Ok(ProjectCallGraph::with_nodes(
            wire.nodes
                .into_iter()
                .map(|w| SymNode {
                    file: abs(&w.file),
                    name: w.name,
                    kind: w.kind,
                    line: w.line,
                    callers: w.callers,
                    callees: w.callees,
                })
                .collect(),
        ))
    }
}

/// Attach edges to nodes' adjacency lists (self-edges dropped, duplicates
/// collapsed) and finish the graph.
fn finalize(mut nodes: Vec<SymNode>, edges: HashSet<(usize, usize)>) -> ProjectCallGraph {
    for (c, e) in edges {
        if c < nodes.len() && e < nodes.len() {
            nodes[c].callees.push(e);
            // A function calling itself is no caller of its own: a
            // recursive function nothing else calls is still uncalled. It
            // lists itself among what it calls.
            if c != e {
                nodes[e].callers.push(c);
            }
        }
    }
    for n in &mut nodes {
        n.callers.sort_unstable();
        n.callers.dedup();
        n.callees.sort_unstable();
        n.callees.dedup();
    }
    ProjectCallGraph::with_nodes(nodes)
}

/// Each item's ordinal among its file's same-name CALLABLES, by line: the
/// numbering `outline::fn_ordinals` gives within one file, applied per file
/// by the one shared implementation (`outline::Ordinals`). Items are
/// `(file, name, kind, line)`; one that is not a callable is numbered against
/// the callables around it rather than counted among them — so a list that
/// strays a type into it cannot shift the keys of the functions.
fn ordinals_of<'a>(items: impl Iterator<Item = (&'a Path, &'a str, &'a str, usize)>) -> Vec<u32> {
    let items: Vec<_> = items.collect();
    // Tests and groups are numbered among their own kind: a test titled
    // as a function is named shifts no function's key.
    let ordinals = crate::outline::Ordinals::new(items.iter().map(|&(file, name, kind, line)| {
        ((file, name, calls_only(kind)), is_node_kind(kind), line)
    }));
    items
        .iter()
        .map(|&(file, name, kind, line)| ordinals.of(&(file, name, calls_only(kind)), line))
        .collect()
}

/// One file's call sites read off ONE parse, plus the lines of its callables
/// that are bodyless declarations — a C prototype, a TypeScript overload
/// signature, an interface or abstract method. A call lands on a definition;
/// such a declaration stands in only for a name nothing defines.
#[derive(Debug, Clone)]
pub struct FileCalls {
    pub file: PathBuf,
    /// The language the file was parsed as (a C++ `.h` reads as C++).
    pub lang: Lang,
    pub calls: Vec<CallSite>,
    /// 1-based lines of callables declared without a body.
    pub declarations: HashSet<usize>,
    /// Where the definitions of a name more than one callable of the file
    /// shares lie: definition line → the whole definition's 1-based
    /// inclusive `(first, last)` lines. A call site names its enclosing
    /// function only by name, and for such a name the name does not say
    /// which definition the call is in — a call after a nested `f` inside
    /// `f` is the outer one's. Only these names are kept (see
    /// [`resolve_caller`]); every other name resolves by itself.
    pub bodies: HashMap<usize, (usize, usize)>,
    /// The class (or type, impl, trait) each method belongs to: definition
    /// line → its name. What narrows a call whose receiver's type is known
    /// to that type's method of the name ([`CallSite::receiver`]).
    pub owners: HashMap<usize, String>,
    /// Definition lines of the methods read as attributes, never called (a
    /// Python `@property`; see [`crate::outline::Located::accessed`]): no
    /// call reaches them, and none needs to for them to be used.
    pub accessed: HashSet<usize>,
    /// Definition lines of C and C++ functions of the file alone (`static`;
    /// see [`crate::outline::Located::internal`]): no other file's call
    /// reaches them.
    pub internal: HashSet<usize>,
    /// The file's package and the types it declares with their bases
    /// ([`FileContext`]).
    pub context: FileContext,
}

impl FileCalls {
    /// Parse `source` once and read off its call sites. `None` for a file in
    /// a language without a call model.
    pub fn read(file: &Path, source: &str) -> Option<FileCalls> {
        let (lang, source) = crate::highlight::read_as(crate::highlight::detect(file)?, source)?;
        let source = &*source;
        lang_spec(lang)?;
        let tree = crate::highlight::parse(source, lang)?;
        let symbols = crate::outline::located_in(&tree, source, lang);
        let calls = calls_in(&tree, source, lang, &symbols);
        // A Lean declaration's is its namespace, read as `analyze` reads it.
        let owners = match lang {
            Lang::Lean => crate::outline::lean_owners(source, &symbols),
            _ => owners_in(&tree, source, lang, &symbols),
        };
        let context = context_in(source, lang, &symbols);
        FileCalls::of(file.to_path_buf(), lang, calls, &symbols, owners)
            .map(|calls| calls.with_context(context))
    }

    /// These facts with the file's [`FileContext`].
    pub fn with_context(mut self, context: FileContext) -> Self {
        self.context = context;
        self
    }

    /// The call facts of a file parsed for other reasons as well — what
    /// [`read`](Self::read) would have produced, from `calls` and `symbols`
    /// read off a tree of `lang` ([`crate::outline::analyze`] yields all
    /// three from one parse). `None` for a language without a call model,
    /// exactly as `read` answers. This is how the client's symbol index, and
    /// the server's call-graph build (one parse per file, with its symbol
    /// snapshot's extraction), hand their call sites to
    /// [`ProjectCallGraph::build_from_calls`] instead of the graph parsing
    /// every file a second time.
    pub fn of(
        file: PathBuf,
        lang: Lang,
        calls: Vec<CallSite>,
        symbols: &[Located],
        owners: HashMap<usize, String>,
    ) -> Option<FileCalls> {
        lang_spec(lang)?;
        Some(FileCalls::new(file, lang, calls, symbols, owners))
    }

    /// Whether this file contributes nothing to a graph: no call site, no
    /// bodyless declaration to tell a definition from, and no method whose
    /// class a call elsewhere may name.
    pub fn is_empty(&self) -> bool {
        self.calls.is_empty()
            && self.declarations.is_empty()
            && self.owners.is_empty()
            && self.accessed.is_empty()
            && self.internal.is_empty()
            && self.context.types.iter().all(|(_, bases)| bases.is_empty())
    }

    /// Assemble from facts already read off the file's tree (see
    /// `outline::analyze`).
    pub fn new(
        file: PathBuf,
        lang: Lang,
        calls: Vec<CallSite>,
        symbols: &[Located],
        owners: HashMap<usize, String>,
    ) -> FileCalls {
        let defined = || {
            symbols
                .iter()
                .filter(|l| is_node_kind(&l.symbol.kind))
                .filter_map(|l| Some((l.symbol.name.as_str(), l.symbol.line, l.body?)))
        };
        let mut defined_as: HashMap<&str, usize> = HashMap::new();
        for (name, _, _) in defined() {
            *defined_as.entry(name).or_default() += 1;
        }
        FileCalls {
            file,
            lang,
            calls,
            declarations: symbols
                .iter()
                .filter(|l| l.is_bodyless_callable())
                .map(|l| l.symbol.line)
                .collect(),
            bodies: defined()
                .filter(|(name, _, _)| defined_as.get(name).is_some_and(|&n| n > 1))
                .map(|(_, line, span)| (line, span))
                .collect(),
            owners,
            accessed: symbols
                .iter()
                .filter(|l| l.accessed && is_callable(&l.symbol.kind))
                .map(|l| l.symbol.line)
                .collect(),
            internal: symbols
                .iter()
                .filter(|l| l.internal && is_callable(&l.symbol.kind))
                .map(|l| l.symbol.line)
                .collect(),
            context: FileContext::default(),
        }
    }
}

impl ProjectCallGraph {
    /// Build the graph from the project's callable definitions, the current
    /// source of every file (so call sites reflect what's on disk right now),
    /// and each file's import scope (the internal files it imports — used to
    /// resolve a called name to the definition actually in scope).
    ///
    /// Parses every file. A caller that has parsed them already — a symbol
    /// index — uses [`build_from_calls`](Self::build_from_calls).
    pub fn build(
        defs: Vec<Def>,
        sources: &[(PathBuf, String)],
        scope: &HashMap<PathBuf, HashSet<PathBuf>>,
    ) -> Self {
        let files: Vec<FileCalls> = sources
            .iter()
            .filter_map(|(file, content)| FileCalls::read(file, content))
            .collect();
        Self::build_from_calls(defs, &files, scope)
    }

    /// [`build`](Self::build) over call sites that were already extracted,
    /// for a caller that parsed each file once for several purposes: the
    /// client's symbol index keeps each file's [`FileCalls`] (and caches them
    /// across sessions), and the server reads its definitions and call sites
    /// off one parse per file, with the extraction its symbol snapshot uses.
    /// The same graph as `build` over the same files, with no file read or
    /// parsed. A file that contributes nothing ([`FileCalls::is_empty`]) may
    /// be left out.
    pub fn build_from_calls<'a>(
        defs: Vec<Def>,
        files: impl IntoIterator<Item = &'a FileCalls>,
        scope: &HashMap<PathBuf, HashSet<PathBuf>>,
    ) -> Self {
        let files: Vec<&FileCalls> = files.into_iter().collect();
        let nodes: Vec<SymNode> = defs
            .into_iter()
            .filter(|d| is_node_kind(&d.kind))
            .map(|d| SymNode {
                name: d.name,
                kind: d.kind,
                file: d.file,
                line: d.line,
                callers: Vec::new(),
                callees: Vec::new(),
            })
            .collect();

        let declarations: HashMap<&Path, (&HashSet<usize>, &HashSet<usize>)> = files
            .iter()
            .map(|f| (f.file.as_path(), (&f.declarations, &f.internal)))
            .collect();
        let arities: HashMap<(&Path, usize), (u8, u8)> = files
            .iter()
            .flat_map(|f| {
                f.context
                    .arities
                    .iter()
                    .map(|&(line, fewest, most)| ((f.file.as_path(), line), (fewest, most)))
            })
            .collect();
        let mut facts: Vec<NodeFacts> = nodes
            .iter()
            .map(|n| NodeFacts {
                arity: arities.get(&(n.file.as_path(), n.line)).copied(),
                bodyless: declarations
                    .get(n.file.as_path())
                    .is_some_and(|(d, _)| d.contains(&n.line)),
                internal: declarations
                    .get(n.file.as_path())
                    .is_some_and(|(_, i)| i.contains(&n.line)),
                lang: crate::highlight::detect(&n.file).and_then(Lang::from_key),
            })
            .collect();

        // The definitions looked up by name, indexed so a lookup costs what
        // it finds: scanning every same-named definition, or every
        // definition of the file, per call cost calls × definitions.
        let mut defs = Defs::default();
        // A Haskell instance's method (`instance HasMeta Pandoc where
        // setMeta …`): no name's own definition — a call of the class's
        // method reaches it where the caller's signature names its type.
        // Ruby's singleton methods ([`FileContext::singletons`]).
        let ruby_singletons: HashSet<(&Path, usize)> = files
            .iter()
            .filter(|f| f.lang == Lang::Ruby)
            .flat_map(|f| {
                f.context
                    .singletons
                    .iter()
                    .map(move |&line| (f.file.as_path(), line))
            })
            .collect();
        // Ruby's modules, by name ([`FileContext::modules`]).
        let ruby_modules: HashSet<&str> = files
            .iter()
            .filter(|f| f.lang == Lang::Ruby)
            .flat_map(|f| f.context.modules.iter().map(String::as_str))
            .collect();
        let singleton_node: Vec<bool> = nodes
            .iter()
            .map(|n| ruby_singletons.contains(&(n.file.as_path(), n.line)))
            .collect();
        let haskell_instances: HashSet<(&Path, usize)> = files
            .iter()
            .filter(|f| f.lang == Lang::Haskell)
            .flat_map(|f| {
                f.context
                    .instance_methods
                    .iter()
                    .map(move |&line| (f.file.as_path(), line))
            })
            .collect();
        for (i, n) in nodes.iter().enumerate() {
            // A test calls; no call lands on one.
            if calls_only(&n.kind) || haskell_instances.contains(&(n.file.as_path(), n.line)) {
                defs.callers
                    .entry((n.file.as_path(), n.name.as_str()))
                    .or_default()
                    .push(i);
                continue;
            }
            let name = logical_name(&n.name, facts[i].lang);
            let named = defs.named.entry(name).or_default();
            named.all.push(i);
            if !facts[i].bodyless {
                named.defined.push(i);
            }
            let file = n.file.as_path();
            defs.in_file.entry((file, name)).or_default().push(i);
            defs.callers
                .entry((file, n.name.as_str()))
                .or_default()
                .push(i);
        }
        // Definitions within a file, ordered by line, so a call resolves to the
        // nearest preceding same-named definition.
        for v in defs.callers.values_mut() {
            v.sort_by_key(|&i| nodes[i].line);
        }

        // The class each method belongs to, by node: what a call whose
        // receiver's type is known narrows to.
        let owner_at: HashMap<(&Path, usize), &str> = files
            .iter()
            .flat_map(|f| {
                f.owners
                    .iter()
                    .map(move |(&line, name)| ((f.file.as_path(), line), name.as_str()))
            })
            .collect();
        let owner: Vec<Option<&str>> = nodes
            .iter()
            .map(|n| owner_at.get(&(n.file.as_path(), n.line)).copied())
            .collect();
        // A C++ function the project also declares apart (a header's
        // prototype) may leave out what that declaration defaults, its own
        // list showing none (C++ gives a parameter its default once).
        let mut declared_fewest: HashMap<(&str, Option<&str>), u8> = HashMap::new();
        for (i, n) in nodes.iter().enumerate() {
            if facts[i].bodyless && matches!(facts[i].lang, Some(Lang::C | Lang::Cpp)) {
                // (A header read as C says no count: any.)
                let fewest = facts[i].arity.map_or(0, |(fewest, _)| fewest);
                declared_fewest
                    .entry((n.name.as_str(), owner[i]))
                    .and_modify(|f| *f = (*f).min(fewest))
                    .or_insert(fewest);
            }
        }
        for (i, n) in nodes.iter().enumerate() {
            if !facts[i].bodyless
                && let Some((fewest, most)) = facts[i].arity
                && let Some(&declared) = declared_fewest.get(&(n.name.as_str(), owner[i]))
            {
                facts[i].arity = Some((fewest.min(declared), most));
            }
        }
        // The project's own types (and traits): what defines a method here.
        let project_types: HashSet<&str> = owner.iter().flatten().copied().collect();
        // Each owned definition by its owner's last part and its name: a
        // type's (a module's) function, looked up wherever it is defined.
        let mut by_owner: HashMap<(&str, &str), Vec<usize>> = HashMap::new();
        for (i, o) in owner.iter().enumerate() {
            if let Some(o) = o
                && !calls_only(&nodes[i].kind)
            {
                let last = o.rsplit('.').next().unwrap_or(o);
                by_owner
                    .entry((last, logical_name(&nodes[i].name, facts[i].lang)))
                    .or_default()
                    .push(i);
            }
        }

        // Python runs a class's `__init__` when the class is called:
        // `Inventory(...)`, `raise OutOfStock(...)`. By the class's name.
        let mut inits: HashMap<&str, Vec<usize>> = HashMap::new();
        for (i, n) in nodes.iter().enumerate() {
            if n.name == "__init__"
                && facts[i].lang == Some(Lang::Python)
                && let Some(class) = owner[i]
            {
                inits.entry(class).or_default().push(i);
            }
        }

        // The extension methods, by node: what a value of a type they
        // extend — a library's — is called with.
        let extension_lines: HashSet<(&Path, usize)> = files
            .iter()
            .flat_map(|f| {
                f.context
                    .extensions
                    .iter()
                    .map(move |&line| (f.file.as_path(), line))
            })
            .collect();
        let extension_nodes: HashSet<usize> = (0..nodes.len())
            .filter(|&i| extension_lines.contains(&(nodes[i].file.as_path(), nodes[i].line)))
            .collect();
        // The type hierarchy, by name: each type's bases, each base's
        // subtypes, and every type the project declares.
        // (C#'s bases by their type arguments' count too, `IRequest`1`:
        // what an argument passes as — see `generic_arity`.)
        let mut bases_of: HashMap<&str, Vec<&str>> = HashMap::new();
        let mut exact_bases_of: HashMap<&str, Vec<&str>> = HashMap::new();
        let mut subtypes_of: HashMap<&str, Vec<&str>> = HashMap::new();
        let mut declared_types: HashSet<&str> = HashSet::new();
        for f in files.iter().copied() {
            for (ty, bases) in &f.context.types {
                declared_types.insert(ty.as_str());
                for base in bases {
                    exact_bases_of
                        .entry(ty.as_str())
                        .or_default()
                        .push(base.as_str());
                    // (A base named by its path, `kernel.Async[IO]`, is the
                    // class `Async`, as owners are named — but Go's, whose
                    // package says whose it is, `http.Handler`.)
                    let base = without_arity(base);
                    let base = match base.rsplit_once(['.', ':']) {
                        Some((_, last)) if !last.is_empty() && f.lang != Lang::Go => last,
                        _ => base,
                    };
                    bases_of.entry(ty.as_str()).or_default().push(base);
                    subtypes_of.entry(base).or_default().push(ty.as_str());
                }
            }
        }
        for list in bases_of
            .values_mut()
            .chain(subtypes_of.values_mut())
            .chain(exact_bases_of.values_mut())
        {
            list.sort_unstable();
            list.dedup();
        }
        // The methods named `callee` a value of `ty` may run: the type's own
        // or, failing that, its nearest bases' (what it inherits; Go's
        // embedded types'), and — `overrides` — its subtypes' that override
        // it, as a value declared as `ty` may be one of them.
        // Each function's parameter kinds ([`FileContext::parameters`]).
        let parameters: HashMap<(&Path, usize), &str> = files
            .iter()
            .flat_map(|f| {
                f.context
                    .parameters
                    .iter()
                    .map(|(line, kinds)| ((f.file.as_path(), *line), kinds.as_str()))
            })
            .collect();
        let node_parameters: Vec<Option<&str>> = nodes
            .iter()
            .map(|n| parameters.get(&(n.file.as_path(), n.line)).copied())
            .collect();
        // Their names ([`FileContext::parameter_names`]).
        let parameter_names: HashMap<(&Path, usize), &str> = files
            .iter()
            .flat_map(|f| {
                f.context
                    .parameter_names
                    .iter()
                    .map(|(line, names)| ((f.file.as_path(), *line), names.as_str()))
            })
            .collect();
        let node_parameter_names: Vec<Option<&str>> = nodes
            .iter()
            .map(|n| parameter_names.get(&(n.file.as_path(), n.line)).copied())
            .collect();
        let family = |ty: &str, callee: &str, overrides: bool| -> Vec<usize> {
            let last = ty.rsplit(['.', ':']).next().unwrap_or(ty);
            let mut out: Vec<usize> = Vec::new();
            let mut level: Vec<&str> = vec![last];
            let mut seen: HashSet<&str> = HashSet::from([last]);
            for _ in 0..12 {
                let found: Vec<usize> = level
                    .iter()
                    .flat_map(|t| by_owner.get(&(*t, callee)))
                    .flatten()
                    .copied()
                    .filter(|&c| !calls_only(&nodes[c].kind))
                    .collect();
                if !found.is_empty() {
                    out.extend(found);
                    break;
                }
                level = level
                    .iter()
                    .flat_map(|t| bases_of.get(t))
                    .flatten()
                    .copied()
                    .filter(|b| seen.insert(b))
                    .collect();
                if level.is_empty() {
                    break;
                }
            }
            if overrides {
                let mut stack: Vec<&str> = vec![last];
                let mut seen: HashSet<&str> = HashSet::from([last]);
                while let Some(t) = stack.pop() {
                    for &sub in subtypes_of.get(t).into_iter().flatten() {
                        if seen.len() > 2000 || !seen.insert(sub) {
                            continue;
                        }
                        out.extend(by_owner.get(&(sub, callee)).into_iter().flatten().copied());
                        stack.push(sub);
                    }
                }
            }
            out.sort_unstable();
            out.dedup();
            // A definition over a declaration of it (a C++ header's, an
            // interface's abstract one) where there is one taking the same
            // parameters: of overloads, each its own (an abstract
            // `value(String?)` is no `value(BufferedSource)` with a body).
            let bodied: HashSet<Option<&str>> = out
                .iter()
                .filter(|&&c| !facts[c].bodyless)
                .map(|&c| node_parameters[c])
                .collect();
            out.retain(|&c| !facts[c].bodyless || !bodied.contains(&node_parameters[c]));
            out
        };
        // Each type alias's type ([`FileContext::aliases`]), followed to the
        // end: one type, whichever name a value is declared by.
        let alias_of: HashMap<&str, &str> = files
            .iter()
            .flat_map(|f| f.context.aliases.iter())
            .map(|(alias, ty)| (alias.as_str(), ty.as_str()))
            .collect();
        // Whether the type `child` has `base` above it, as written: C#'s
        // `Ping : IRequest<Pong>` is an `IRequest`1`, no `IRequest`.
        let descends_exactly = |child: &str, base: &str| -> bool {
            let child = without_arity(child);
            let mut level: Vec<&str> = vec![child];
            let mut seen: HashSet<&str> = HashSet::from([child]);
            for _ in 0..12 {
                let next: Vec<&str> = level
                    .iter()
                    .flat_map(|t| exact_bases_of.get(without_arity(t)))
                    .flatten()
                    .copied()
                    .filter(|b| seen.insert(b))
                    .collect();
                if next.contains(&base) {
                    return true;
                }
                if next.is_empty() {
                    return false;
                }
                level = next;
            }
            false
        };
        // Whether the type `child` has `base` above it.
        let descends = |child: &str, base: &str| -> bool {
            let mut level: Vec<&str> = vec![child];
            let mut seen: HashSet<&str> = HashSet::from([child]);
            for _ in 0..12 {
                let next: Vec<&str> = level
                    .iter()
                    .flat_map(|t| bases_of.get(t))
                    .flatten()
                    .copied()
                    .filter(|b| seen.insert(b))
                    .collect();
                if next.contains(&base) {
                    return true;
                }
                if next.is_empty() {
                    return false;
                }
                level = next;
            }
            false
        };
        // The Elixir and Erlang modules the project defines, by each name a
        // call may give one: `Shop.Cart`, and `Cart` as an alias says it.
        let module_names: HashSet<&str> = owner
            .iter()
            .flatten()
            .flat_map(|o| {
                std::iter::once(*o).chain(o.match_indices('.').map(move |(at, _)| &o[at + 1..]))
            })
            .collect();
        // Each package's (namespace's) files: what a file of it reaches with
        // no import — Go's in its folder, the JVM languages' wherever they
        // are (a test beside its class under `src/test`), C#'s.
        let package_key = |fc: &FileCalls, package: &str| -> Option<String> {
            match fc.lang {
                Lang::Go => Some(format!("go\0{}\0{package}", fc.file.parent()?.display())),
                Lang::Java | Lang::Kotlin | Lang::Scala => Some(format!("jvm\0{package}")),
                Lang::CSharp => Some(format!("cs\0{package}")),
                _ => None,
            }
        };
        let mut packages: HashMap<String, HashSet<PathBuf>> = HashMap::new();
        for fc in &files {
            if let Some(package) = fc.context.package.as_deref()
                && let Some(key) = package_key(fc, package)
            {
                packages.entry(key).or_default().insert(fc.file.clone());
            }
        }
        // A Rails app's autoloaded code: every class under its `app/` and
        // `lib/` (and its tests) is named anywhere with no `require`, as
        // Zeitwerk loads it. By the app's root, where its
        // `config/application.rb` is.
        let rails_roots: Vec<&Path> = files
            .iter()
            .filter_map(|f| {
                f.file
                    .ends_with("config/application.rb")
                    .then(|| f.file.parent()?.parent())
                    .flatten()
            })
            .collect();
        let rails_root_of = |file: &Path| -> Option<&Path> {
            rails_roots.iter().copied().find(|root| {
                ["app", "lib", "test", "spec"]
                    .iter()
                    .any(|d| file.starts_with(root.join(d)))
            })
        };
        let mut rails: HashMap<&Path, HashSet<PathBuf>> = HashMap::new();
        for fc in &files {
            if fc.lang == Lang::Ruby
                && let Some(root) = rails_root_of(&fc.file)
            {
                rails.entry(root).or_default().insert(fc.file.clone());
            }
        }
        // An Elixir module's file, by the module's name: what a `use` or an
        // `import` names — one with a `__using__` macro alone too.
        let mut module_files: HashMap<&str, &Path> = HashMap::new();
        for f in files.iter().copied() {
            if f.lang == Lang::Elixir {
                let declared = f.context.types.iter().map(|(name, _)| name);
                for module in f.owners.values().chain(declared) {
                    module_files
                        .entry(module.as_str())
                        .or_insert(f.file.as_path());
                }
            }
        }
        // What an Elixir file reaches with no alias of its own: what each
        // module it `use`s injects — that module's own file (a `def` in its
        // `__using__`), the modules it imports and aliases, and in turn what
        // the modules it uses inject.
        let context_of: HashMap<&Path, &FileContext> = files
            .iter()
            .map(|f| (f.file.as_path(), &f.context))
            .collect();
        // Its bare calls reach only its own file, the modules it imports,
        // and the files the modules it uses inject.
        let mut bare_scopes: HashMap<&Path, HashSet<&Path>> = HashMap::new();
        let mut used_scopes: HashMap<&Path, HashSet<PathBuf>> = HashMap::new();
        for fc in files.iter().copied() {
            if fc.lang != Lang::Elixir {
                continue;
            }
            let mut injected: HashSet<&Path> = HashSet::new();
            let mut imported: Vec<&str> = fc.context.imports.iter().map(String::as_str).collect();
            let mut pending: Vec<&str> = fc.context.uses.iter().map(String::as_str).collect();
            let mut used: HashSet<&str> = HashSet::new();
            while let Some(module) = pending.pop() {
                if !used.insert(module) {
                    continue;
                }
                let Some(&file) = module_files.get(module) else {
                    continue;
                };
                injected.insert(file);
                if let Some(ctx) = context_of.get(file) {
                    imported.extend(ctx.imports.iter().map(String::as_str));
                    pending.extend(ctx.uses.iter().map(String::as_str));
                }
            }
            let mut reached = injected.clone();
            reached.extend(
                imported
                    .into_iter()
                    .filter_map(|m| module_files.get(m).copied()),
            );
            let mut seen: HashSet<PathBuf> = reached.iter().map(|f| f.to_path_buf()).collect();
            for file in &injected {
                seen.extend(scope.get(*file).into_iter().flatten().cloned());
            }
            seen.remove(&fc.file);
            if !seen.is_empty() {
                used_scopes.insert(fc.file.as_path(), seen);
            }
            bare_scopes.insert(fc.file.as_path(), reached);
        }
        // What RSpec loads before each spec under a `spec/` folder: its
        // `spec_helper.rb` and `rails_helper.rb` (`.rspec`'s `--require`),
        // and the `spec/support/` files they require by a glob.
        let mut spec_roots: HashMap<&Path, HashSet<PathBuf>> = HashMap::new();
        for f in files.iter().copied() {
            if f.lang == Lang::Ruby
                && matches!(
                    f.file.file_name().and_then(|n| n.to_str()),
                    Some("spec_helper.rb" | "rails_helper.rb")
                )
                && let Some(root) = f.file.parent()
                && root.file_name().is_some_and(|n| n == "spec")
            {
                spec_roots.entry(root).or_default().insert(f.file.clone());
            }
        }
        if !spec_roots.is_empty() {
            for f in files.iter().copied() {
                if f.lang != Lang::Ruby {
                    continue;
                }
                for (root, loaded) in spec_roots.iter_mut() {
                    if f.file.starts_with(root.join("support")) {
                        loaded.insert(f.file.clone());
                    }
                }
            }
        }
        // A Scala package's files by the package's last name (`os` for
        // `package os`, and for `package object os`): what `os.read(p)`
        // names.
        // Every Dart file, for an import's URI to name ([`dart_library_root`]).
        let dart_files: Vec<&Path> = files
            .iter()
            .filter(|f| f.lang == Lang::Dart)
            .map(|f| f.file.as_path())
            .collect();
        let mut scala_packages: HashMap<&str, HashSet<&Path>> = HashMap::new();
        let mut scala_package_of: HashMap<&Path, &str> = HashMap::new();
        for f in files.iter().copied() {
            if f.lang == Lang::Scala
                && let Some(package) = f.context.package.as_deref()
            {
                let last = package.rsplit('.').next().unwrap_or(package);
                scala_packages
                    .entry(last)
                    .or_default()
                    .insert(f.file.as_path());
                scala_package_of.insert(f.file.as_path(), package);
            }
        }
        // A Go package's files by its name (`package binding`).
        let mut go_packages: HashMap<&str, HashSet<&Path>> = HashMap::new();
        for f in files.iter().copied() {
            if f.lang == Lang::Go
                && let Some(package) = f.context.package.as_deref()
            {
                go_packages
                    .entry(package)
                    .or_default()
                    .insert(f.file.as_path());
            }
        }
        // A Scala object's `apply`: what calling the object runs
        // (`RelPath("a")`, `os.read(p)` for `object read`).
        let applies_of = |object: &str| -> Vec<usize> {
            by_owner
                .get(&(object, "apply"))
                .into_iter()
                .flatten()
                .copied()
                .filter(|&i| facts[i].lang == Some(Lang::Scala))
                .collect()
        };
        let siblings_of = |fc: &FileCalls| -> Vec<&HashSet<PathBuf>> {
            let mut extra: Vec<&HashSet<PathBuf>> = Vec::new();
            if fc.lang == Lang::Ruby
                && let Some(group) = rails_root_of(&fc.file).and_then(|root| rails.get(root))
            {
                extra.push(group);
            }
            if let Some(used) = used_scopes.get(fc.file.as_path()) {
                extra.push(used);
            }
            let Some(package) = fc.context.package.as_deref() else {
                return extra;
            };
            // C# sees its enclosing namespaces' types too: `Shop.Cart`'s
            // code names `Shop`'s with no `using`; Scala those its chained
            // package clauses open.
            let mut names = vec![package];
            names.extend(fc.context.enclosing.iter().map(String::as_str));
            if fc.lang == Lang::CSharp {
                let mut rest = package;
                while let Some((outer, _)) = rest.rsplit_once('.') {
                    names.push(outer);
                    rest = outer;
                }
            }
            extra.extend(
                names
                    .into_iter()
                    .filter_map(|name| packages.get(&package_key(fc, name)?)),
            );
            extra
        };

        // Which files are tests', by their path under the files' common
        // folder (the project's): a test calls the code, never the code a
        // test — a helper a test file defines is no callee of the app's.
        let common: PathBuf = {
            let mut paths = nodes.iter().map(|n| n.file.as_path());
            let mut common = paths
                .next()
                .and_then(Path::parent)
                .map(Path::to_path_buf)
                .unwrap_or_default();
            for p in paths {
                while !p.starts_with(&common) && common.pop() {}
            }
            common
        };
        let is_test_file = |file: &Path| {
            file.strip_prefix(&common)
                .ok()
                .and_then(|rel| rel.to_str())
                .is_some_and(crate::weakspots::is_test_path)
        };
        let test_node: Vec<bool> = nodes.iter().map(|n| is_test_file(&n.file)).collect();
        // Each function's declared return type ([`FileContext::returns`]).
        let returns: HashMap<(&Path, usize), &str> = files
            .iter()
            .flat_map(|f| {
                f.context
                    .returns
                    .iter()
                    .map(|(line, ty)| ((f.file.as_path(), *line), ty.as_str()))
            })
            .collect();
        // (A Go type of the project's own packages, `models.User`, by its
        // name; another package's, `flag.FlagSet`, whole: the library's.)
        let written: Vec<Option<&str>> = nodes
            .iter()
            .map(|n| {
                returns
                    .get(&(n.file.as_path(), n.line))
                    .copied()
                    .map(|t| match t.split_once('.') {
                        Some((pkg, name)) if go_packages.contains_key(pkg) => name,
                        _ => t,
                    })
            })
            .collect();
        // A body that gives what a call gives ([`inferred_result`]): the
        // class the function's name is (a Scala case class's `apply`), or
        // else what its definitions all say they return.
        let node_returns: Vec<Option<String>> = written
            .iter()
            .map(|ty| {
                match ty
                    .and_then(|t| t.strip_prefix(RETURNED))
                    // (`@os#proc`, `Type#method`: the function called.)
                    .map(|f| f.rsplit('#').next().unwrap_or(f))
                {
                    None => ty.map(str::to_string),
                    Some(function) if declared_types.contains(function) => {
                        Some(function.to_string())
                    }
                    Some(function) => {
                        let mut types = defs
                            .named
                            .get(function)
                            .into_iter()
                            .flat_map(|named| named.all.iter().copied())
                            .map(|i| written[i].filter(|t| !t.starts_with(RETURNED)));
                        let first = types.next().flatten()?;
                        types.all(|t| t == Some(first)).then(|| first.to_string())
                    }
                }
            })
            .collect();
        let mut returned_types: HashMap<String, Option<String>> = HashMap::new();
        // Its overloads: the definitions of its name and owner (for a free
        // function, its file), whichever take kinds.
        // C#'s type names written with type arguments somewhere (a base's, a
        // parameter's: `IRequest<T>`): an argument of the name with none
        // known (a variable's type, read without them) may be of either.
        let generic_named: HashSet<&str> = node_parameters
            .iter()
            .flatten()
            .flat_map(|kinds| kinds.split(','))
            .chain(exact_bases_of.values().flatten().copied())
            .filter(|k| k.contains('`'))
            .map(|k| without_arity(k.trim_start_matches('T')))
            .collect();
        // (The name, its owner, and a free function's file.)
        type Family<'k> = (&'k str, Option<&'k str>, Option<&'k Path>);
        let mut overloads: HashMap<Family, Vec<usize>> = HashMap::new();
        for (i, n) in nodes.iter().enumerate() {
            if node_parameters[i].is_some() {
                let file = owner[i].is_none().then_some(n.file.as_path());
                overloads
                    .entry((n.name.as_str(), owner[i], file))
                    .or_default()
                    .push(i);
            }
        }
        let declared = |ty: &str| declared_types.contains(ty);
        // Each Haskell function's signature's type names, by its definition.
        let signature_types: HashMap<(&Path, usize), &str> = files
            .iter()
            .flat_map(|f| {
                f.context
                    .signatures
                    .iter()
                    .map(move |(line, types)| ((f.file.as_path(), *line), types.as_str()))
            })
            .collect();
        // Whether a class of the project's has a base above it this does not
        // read — a library's, whose own bases are unknown: a value of it may
        // be of any library type. One whose bases are all the project's is
        // of none it does not name (an `enum class E` is no `List`).
        let reaches_library = |class: &str| -> bool {
            let mut stack: Vec<&str> = vec![without_arity(class)];
            let mut seen: HashSet<&str> = HashSet::new();
            while let Some(t) = stack.pop() {
                if seen.len() > 500 || !seen.insert(t) {
                    continue;
                }
                for &base in bases_of.get(t).into_iter().flatten() {
                    let base = base.rsplit(['.', ':']).next().unwrap_or(base);
                    if !declared_types.contains(base) {
                        return true;
                    }
                    stack.push(base);
                }
            }
            false
        };
        // What each C++ class converts from, by its name: the kinds of its
        // converting constructors' first parameters, all its namesakes'.
        let mut conversions: HashMap<&str, Vec<&str>> = HashMap::new();
        for f in &files {
            for (class, kinds) in &f.context.converts {
                let entry = conversions.entry(class.as_str()).or_default();
                entry.extend(kinds.split(',').filter(|k| !k.is_empty()));
            }
        }
        // Whether a literal of kind `arg` converts to the class `class` —
        // `None` for a class not read: a library's, an alias. A constructor
        // taking another class takes no literal: C++ converts by one
        // constructor at most.
        let converts = |class: &str, arg: &str| -> Option<bool> {
            let kinds = conversions.get(class)?;
            Some(kinds.iter().any(|&k| match k {
                "?" => true,
                "K" => false,
                k if k.starts_with('T') => !conversions.contains_key(&k[1..]),
                k => cpp_passes(arg, k),
            }))
        };
        // The functions declared inside another ([`FileContext::nested`]).
        let nested_lines: HashSet<(&Path, usize)> = files
            .iter()
            .flat_map(|f| {
                f.context
                    .nested
                    .iter()
                    .map(move |&line| (f.file.as_path(), line))
            })
            .collect();
        let nested_node: Vec<bool> = nodes
            .iter()
            .map(|n| nested_lines.contains(&(n.file.as_path(), n.line)))
            .collect();
        // The platforms each definition's source is built for, where its
        // path says ([`source_platforms`]): `core/js/…` for JavaScript alone.
        let node_platforms: Vec<Option<&'static [&'static str]>> =
            nodes.iter().map(|n| source_platforms(&n.file)).collect();
        // Every Go struct's fields, by the struct's name and the field's.
        let field_types: HashMap<(&str, &str), &str> = files
            .iter()
            .flat_map(|f| {
                f.context
                    .fields
                    .iter()
                    .filter(|(owner, _, _)| !owner.is_empty())
                    .map(|(owner, field, ty)| ((owner.as_str(), field.as_str()), ty.as_str()))
            })
            .collect();
        // The same, with the file each is declared in: two files' structs
        // of one name (http.zig's `response.zig` and `testing.zig` each
        // have a `Response` with `headers`) are two types.
        let mut field_sources: HashMap<(&str, &str), Vec<(&Path, &str)>> = HashMap::new();
        for f in files.iter() {
            for (owner, field, ty) in f.context.fields.iter().filter(|(o, _, _)| !o.is_empty()) {
                field_sources
                    .entry((owner.as_str(), field.as_str()))
                    .or_default()
                    .push((f.file.as_path(), ty.as_str()));
            }
        }
        // A field's type, as the struct of the name in the file `from` (where
        // a value of it is used) declares it — or, declared elsewhere, as
        // every such struct agrees.
        let field_type = |ty: &str, field: &str, from: &Path| -> Option<&str> {
            let all = field_sources.get(&(ty, field))?;
            if let Some((_, own)) = all.iter().find(|(file, _)| *file == from) {
                return Some(own);
            }
            let first = all.first()?.1;
            all.iter().all(|(_, t)| *t == first).then_some(first)
        };
        // Whether the class `ty`, or a class above it, has the field `name`.
        let has_field = |ty: &str, name: &str| -> bool {
            let mut level: Vec<&str> = vec![ty.rsplit(['.', ':']).next().unwrap_or(ty)];
            for _ in 0..6 {
                if level.iter().any(|t| field_types.contains_key(&(*t, name))) {
                    return true;
                }
                level = level
                    .iter()
                    .flat_map(|t| bases_of.get(t).into_iter().flatten())
                    .copied()
                    .collect();
                if level.is_empty() {
                    break;
                }
            }
            false
        };
        // A Zig file's own variables, by the file.
        let file_fields: HashMap<(&Path, &str), &str> = files
            .iter()
            .flat_map(|f| {
                f.context
                    .fields
                    .iter()
                    .filter(|(owner, _, _)| owner.is_empty())
                    .map(move |(_, field, ty)| ((f.file.as_path(), field.as_str()), ty.as_str()))
            })
            .collect();
        // A library's type, whose fields and methods are the library's: Go's
        // of a package not the project's, Zig's standard library's.
        let library = |ty: &str, lang: Lang| match lang {
            Lang::Zig => ty == "std" || ty.starts_with("std."),
            Lang::Cpp => ty.starts_with("std::"),
            _ => ty
                .split_once('.')
                .is_some_and(|(pkg, _)| !go_packages.contains_key(pkg)),
        };
        // The types each Go type embeds (`*Engine`, `*ResponseWriter`
        // fields), by its name.
        let mut embeds: HashMap<&str, Vec<&str>> = HashMap::new();
        for f in files.iter().filter(|f| f.lang == Lang::Go) {
            for (owner, field, ty) in &f.context.fields {
                if field.starts_with('*') {
                    embeds.entry(owner.as_str()).or_default().push(ty.as_str());
                }
            }
        }
        // The type a chain of fields ends in ([`THROUGH`]): `None` where a
        // field is none this has read; the field of an embedded struct is
        // its embedder's too. A Go method's result (`Params()`): what the
        // type's own method returns, where its definitions agree, or its
        // interface's method declares — and, a method only a library's type
        // it embeds has (`Header()` of `http.ResponseWriter`), a library's
        // value.
        let through = |chain: &str, lang: Lang, from: &Path| -> Option<String> {
            let mut parts = chain.split('|');
            let root = parts.next()?;
            // `@t.zig`: a variable of that file.
            let mut ty: String = match root.strip_prefix('@') {
                Some(path) => {
                    let file = lexical_join(from.parent()?, path);
                    let field = parts.next()?;
                    file_fields.get(&(file.as_path(), field))?.to_string()
                }
                // What a function returns (`→PerformRequest`, `→T#m`), where
                // its definitions agree.
                None if root.starts_with(RETURNED) => {
                    let function = &root[RETURNED.len()..];
                    let found: Vec<usize> = match function.split_once('#') {
                        Some((owner, method)) => family(owner, method, false),
                        None => defs
                            .named
                            .get(logical_name(function, Some(lang)))
                            .map(|named| named.all.clone())
                            .unwrap_or_default(),
                    };
                    let mut types = found.iter().map(|&i| node_returns[i].as_deref());
                    let first = types.next().flatten()?;
                    if !types.all(|t| t == Some(first)) {
                        return None;
                    }
                    first.to_string()
                }
                None => root.to_string(),
            };
            for field in parts {
                // An element of a slice or map field's (`[]Error`).
                if field == "[]"
                    && let Some(element) = ty.strip_prefix("[]")
                {
                    ty = element.to_string();
                    continue;
                }
                // A library's value: its fields are the library's.
                if library(&ty, lang) {
                    return Some(ty);
                }
                if let Some(method) = field.strip_suffix("()") {
                    let found = family(&ty, method, false);
                    if !found.is_empty() {
                        let mut types = found.iter().map(|&i| node_returns[i].as_deref());
                        let first = types.next().flatten()?;
                        if !types.all(|t| t == Some(first)) {
                            return None;
                        }
                        ty = first.to_string();
                        continue;
                    }
                }
                let mut level: Vec<String> = vec![ty.rsplit('.').next().unwrap_or(&ty).to_string()];
                let mut seen: Vec<String> = Vec::new();
                let mut next = None;
                for _ in 0..4 {
                    if let Some(found) = level
                        .iter()
                        .find_map(|t| field_type(t.as_str(), field, from))
                    {
                        next = Some(found.to_string());
                        break;
                    }
                    seen.extend(level.iter().cloned());
                    level = level
                        .iter()
                        .flat_map(|t| bases_of.get(t.as_str()).into_iter().flatten())
                        .chain(
                            level
                                .iter()
                                .flat_map(|t| embeds.get(t.as_str()).into_iter().flatten())
                                .filter(|e| !library(e, lang)),
                        )
                        .map(|b| b.rsplit('.').next().unwrap_or(b).to_string())
                        .collect();
                    if level.is_empty() {
                        break;
                    }
                }
                // (None of the project's types has it, but one embeds a
                // library's: that type's field or method.)
                if next.is_none() && lang == Lang::Go {
                    next = seen.iter().find_map(|t| {
                        embeds
                            .get(t.as_str())
                            .into_iter()
                            .flatten()
                            .find(|e| library(e, lang))
                            .map(|e| e.to_string())
                    });
                }
                ty = next?;
            }
            Some(ty)
        };
        // Play's templates, by the function each compiles to.
        let templates: HashMap<String, usize> = (0..nodes.len())
            .filter_map(|i| {
                let name = template_name(&nodes[i].file)?;
                name.ends_with(nodes[i].name.as_str()).then_some((name, i))
            })
            .collect();
        // Whether what `cs` passes fits `callee`'s parameters — or, if not,
        // no overload of it fits either (the kinds then say nothing).
        // Whether the arguments `arguments` passes fit the parameters of
        // `i`: each against the parameter it is passed for — its place's,
        // or a named one's (`excludeSettings = …`), of no overload without
        // a parameter of the name.
        let fits_kinds = |i: usize, arguments: &str, lang: Lang| -> bool {
            let kinds: Vec<&str> = node_parameters[i]
                .map(|params| params.split(',').collect())
                .unwrap_or_default();
            let names = node_parameter_names[i];
            arguments.split(',').enumerate().all(|(at, a)| {
                let (at, a) = match a.split_once('=') {
                    Some((name, kind)) => match names {
                        Some(names) => match names.split(',').position(|n| n == name) {
                            Some(at) => (at, kind),
                            None => return false,
                        },
                        None => return true,
                    },
                    None => (at, a),
                };
                kinds.get(at).is_none_or(|p| {
                    passes(
                        a,
                        p,
                        lang,
                        &declared,
                        &descends_exactly,
                        &converts,
                        &reaches_library,
                    )
                })
            })
        };
        let fits_arguments = |callee: usize, cs: &CallSite, lang: Lang| -> bool {
            let Some(arguments) = cs.arguments.as_deref() else {
                return true;
            };
            let named = arguments.contains('=');
            if node_parameters[callee].is_none() && !named {
                return true;
            }
            let fits = |i: usize| fits_kinds(i, arguments, lang);
            let file = owner[callee]
                .is_none()
                .then_some(nodes[callee].file.as_path());
            let name = nodes[callee].name.as_str();
            let mut family: Vec<usize> = overloads
                .get(&(name, owner[callee], file))
                .cloned()
                .unwrap_or_default();
            // An override's overloads are its bases' too: a subclass's
            // `value(String?)` beside its base's `value(Long)`.
            if let Some(own) = owner[callee] {
                let own = own.rsplit(['.', ':']).next().unwrap_or(own);
                let mut level: Vec<&str> = vec![own];
                let mut seen: HashSet<&str> = HashSet::from([own]);
                for _ in 0..12 {
                    level = level
                        .iter()
                        .flat_map(|t| bases_of.get(t))
                        .flatten()
                        .copied()
                        .filter(|b| seen.insert(b))
                        .collect();
                    if level.is_empty() {
                        break;
                    }
                    for base in &level {
                        family.extend(
                            overloads
                                .get(&(name, Some(*base), None))
                                .into_iter()
                                .flatten(),
                        );
                    }
                }
            }
            let facts = &facts;
            let owner = &owner;
            let family = &family;
            let others = || {
                family
                    .iter()
                    .copied()
                    .filter(move |&i| i != callee && facts[i].takes_call(cs, lang, owner[i]))
            };
            if fits(callee) {
                // The most specific of the overloads that fit, by what the
                // arguments of a known class are passed as: the class itself
                // (`sleep(time)` with `time: FiniteDuration` is
                // `sleep(FiniteDuration)`, not `sleep(Duration)`), then a
                // class it extends or implements, or a type parameter —
                // before `Object` or a class this knows nothing of
                // (`Send(ping)` is `Send(IRequest<T>)`, not `Send(object)`).
                let rank = |i: usize| -> usize {
                    node_parameters[i].map_or(0, |params| {
                        arguments
                            .split(',')
                            .zip(params.split(','))
                            .filter(|(a, _)| {
                                (a.starts_with('T') || a.starts_with('G')) && !a.contains('=')
                            })
                            .map(|(a, p)| {
                                // (A C++ template's own type parameter
                                // ranks as its name.)
                                let a = match a.strip_prefix('G') {
                                    Some(name) => format!("T{name}"),
                                    None => a.to_string(),
                                };
                                let a = a.as_str();
                                // (A nullable mark trimmed, a type parameter's
                                // `?` is nothing.)
                                let (a, p) = (a.trim_end_matches('?'), p.trim_end_matches('?'));
                                // (C#'s cast to `IRequest` says it is no
                                // `IRequest<T>`: `0.)
                                let (a, plain) = match a.strip_suffix("`0") {
                                    Some(a) => (a, true),
                                    None => (a, false),
                                };
                                let either = lang == Lang::CSharp
                                    && !plain
                                    && !a.contains('`')
                                    && generic_named.contains(&a[1..])
                                    && without_arity(p) == a;
                                // (Or the one an alias of the other:
                                // `memory_buffer` is `basic_memory_buffer`;
                                // two aliases of one template, of other
                                // arguments — `string_view_t`,
                                // `wstring_view_t` — two types.)
                                let same = a == p
                                    || p.strip_prefix('T').is_some_and(|class| {
                                        unaliased(&alias_of, &a[1..]) == class
                                            || unaliased(&alias_of, class) == &a[1..]
                                    });
                                if same && !either {
                                    2
                                } else if either
                                    || p.is_empty()
                                    // (A C++ template's parameter its
                                    // `enable_if` constrains: a type
                                    // parameter still.)
                                    || p.starts_with('K')
                                    || p.strip_prefix('T')
                                        .is_some_and(|class| descends_exactly(&a[1..], class))
                                {
                                    1
                                } else {
                                    0
                                }
                            })
                            .sum()
                    })
                };
                let own = rank(callee);
                // Of overloads that fit alike, Kotlin and C# take one that
                // needs no default value (`vector(name, dimensions)` before
                // `vector<T>(name, dimensions, format = null)`) — where every
                // argument's kind is known, so that both do fit.
                let count = arguments.split(',').count();
                let defaulted = |i: usize| {
                    node_parameters[i].is_some_and(|params| {
                        !params.split(',').any(|p| p == "v") && params.split(',').count() > count
                    })
                };
                let prefers_exact = matches!(lang, Lang::Kotlin | Lang::CSharp)
                    && !arguments.split(',').any(|a| {
                        let a = a.split_once('=').map_or(a, |(_, kind)| kind);
                        a.trim_end_matches('?').is_empty() || a == "?"
                    })
                    && defaulted(callee);
                return !others().filter(|&i| fits(i)).any(|i| {
                    let other = rank(i);
                    other > own || (prefers_exact && other == own && !defaulted(i))
                });
            }
            !others().any(fits)
        };
        // A test class's methods (`StatusCodesTest.get`): the runner holds
        // its instance, so no other code calls them on a value.
        // So does Rails hold a controller's (`UsersController#github`, an
        // action): `user.github` on a value of no known type is a model's
        // attribute, never the action.
        let test_method: Vec<bool> = (0..nodes.len())
            .map(|i| {
                (test_node[i] && owner[i].is_some_and(is_test_class_name))
                    || (nodes[i].file.extension().is_some_and(|e| e == "rb")
                        && nodes[i]
                            .file
                            .components()
                            .any(|c| c.as_os_str() == "controllers")
                        && owner[i].is_some_and(|o| {
                            o.rsplit("::").next().unwrap_or(o).ends_with("Controller")
                        }))
            })
            .collect();
        let empty_scope: HashSet<PathBuf> = HashSet::new();
        let mut edges: HashSet<(usize, usize)> = HashSet::new();
        for fc in files {
            let in_test = is_test_file(&fc.file);
            let direct = scope.get(&fc.file).unwrap_or(&empty_scope);
            // A C or C++ file sees what every header it includes includes,
            // as the preprocessor pastes them in: `main.cpp` includes
            // `shop.hpp`, which includes `cart.hpp`, and calls `Cart`'s
            // methods.
            // So does a Ruby file see what the files it requires require: a
            // `require` loads a file once, for the whole program. And a Dart
            // library what the libraries it imports export — a package's
            // `shelf.dart` is its `src/` files re-exported.
            // A spec sees what RSpec loaded before it, and what that requires.
            let spec_loaded: Option<HashSet<PathBuf>> = (fc.lang == Lang::Ruby
                && !fc.calls.is_empty())
            .then(|| {
                spec_roots
                    .iter()
                    .filter(|(root, _)| fc.file.starts_with(root))
                    .flat_map(|(_, loaded)| loaded.iter())
                    .filter(|f| **f != fc.file)
                    .cloned()
                    .collect::<HashSet<PathBuf>>()
            })
            .filter(|loaded| !loaded.is_empty())
            .map(|mut loaded| {
                loaded.extend(direct.iter().cloned());
                loaded
            });
            let direct = spec_loaded.as_ref().unwrap_or(direct);
            let included = (matches!(fc.lang, Lang::C | Lang::Cpp | Lang::Ruby | Lang::Dart)
                && !fc.calls.is_empty())
            .then(|| include_closure(&fc.file, direct, scope));
            let imported = included.as_ref().unwrap_or(direct);
            let siblings = siblings_of(fc);
            let site = Site {
                file: &fc.file,
                lang: fc.lang,
                imported,
                siblings: &siblings,
            };
            // What a name called from this file resolves to depends on the
            // name and the call's shape only: worked out once per file.
            // (The name, whether it is a method call and with no receiver
            // known, the arguments it passes, and the scope it names.)
            type Shape<'k> = (&'k str, bool, bool, Option<u8>, Option<&'k str>);
            let mut resolved: HashMap<Shape, Vec<usize>> = HashMap::new();
            for cs in &fc.calls {
                // A receiver assigned from a call is of the type the
                // function's definitions declare, where they agree.
                let callee_key: &str = cs.callee.as_str();
                let qualifier_key: Option<&str> = cs.qualifier.as_deref();
                let typed_call: CallSite;
                // Whether the receiver's type is a factory's guess.
                let mut guessed = false;
                let cs = match &cs.receiver {
                    Some(Receiver::Type(t) | Receiver::Declared(t)) if t.starts_with(RETURNED) => {
                        // A factory's: the type it is called on, where the
                        // function says nothing.
                        let (function, fallback) = match t[RETURNED.len()..].split_once('|') {
                            Some((function, fallback)) => (function, Some(fallback)),
                            None => (&t[RETURNED.len()..], None),
                        };
                        // `Type#method`: that type's method (or the one it
                        // inherits), before every method of the name.
                        let owned = function.split_once('#').and_then(|(ty, method)| {
                            let found = family(ty, method, false);
                            let mut types = found.iter().map(|&i| node_returns[i].as_deref());
                            let first = types.next().flatten()?;
                            (types.all(|t| t == Some(first))
                                && (declared_types.contains(first)
                                    || project_types.contains(first)
                                    || first.starts_with("std.")
                                    || first.starts_with("std::")
                                    || (fc.lang == Lang::Go && library(first, fc.lang))))
                            .then(|| first.to_string())
                        });
                        // A Go interface's method (`w.Params()`): the type it
                        // declares it returns — a library's value, where only
                        // a library's type it embeds has the method
                        // (`w.Header()` of `http.ResponseWriter`).
                        let owned = match owned {
                            None if fc.lang == Lang::Go => {
                                match function.split_once('#').and_then(|(ty, method)| {
                                    through(&format!("{ty}|{method}()"), fc.lang, &fc.file)
                                }) {
                                    Some(ty) if library(&ty, fc.lang) => continue,
                                    ty => ty,
                                }
                            }
                            owned => owned,
                        };
                        let function = function.split_once('#').map_or(function, |(_, m)| m);
                        let returned = owned.or_else(|| {
                            (!function.is_empty())
                                .then(|| {
                                    returned_types
                                        .entry(function.to_string())
                                        .or_insert_with(|| {
                                            let name = logical_name(function, Some(fc.lang));
                                            let mut types = defs
                                                .named
                                                .get(name)
                                                .into_iter()
                                                .flat_map(|named| named.all.iter().copied())
                                                .map(|i| node_returns[i].as_deref());
                                            let agreed = types.next().flatten().and_then(|first| {
                                                // A type the project declares: a library's
                                                // (`Future`, `String`) says less than the
                                                // value of no known type did.
                                                (types.all(|t| t == Some(first))
                                                    && (declared_types.contains(first)
                                                        || project_types.contains(first)
                                                        || first.starts_with("std.")
                                                        || first.starts_with("std::")
                                                        || (fc.lang == Lang::Go
                                                            && library(first, fc.lang))))
                                                .then(|| first.to_string())
                                            });
                                            // A class's own name called, where no
                                            // function of it says otherwise (C++'s
                                            // `basic_memory_buffer<Char>()`): the class
                                            // it builds.
                                            agreed.or_else(|| {
                                                declared_types
                                                    .contains(name)
                                                    .then(|| name.to_string())
                                            })
                                        })
                                        .clone()
                                })
                                .flatten()
                        });
                        guessed = returned.is_none() && fallback.is_some();
                        let returned = returned.or_else(|| fallback.map(str::to_string));
                        let exact = matches!(cs.receiver, Some(Receiver::Type(_)));
                        typed_call = CallSite {
                            receiver: returned.map(|ty| match exact {
                                true => Receiver::Type(ty),
                                false => Receiver::Declared(ty),
                            }),
                            ..cs.clone()
                        };
                        &typed_call
                    }
                    // A chain of fields: its last field's type. Another
                    // package's (`http.Header`) is a library's value, whose
                    // methods are none of the project's.
                    Some(Receiver::Type(t)) if t.starts_with(THROUGH) => {
                        // `.`: the calling file's own package.
                        let chain = &t[THROUGH.len()..];
                        let chain = match chain.strip_prefix(".|") {
                            Some(rest) => {
                                format!("{}|{rest}", fc.context.package.as_deref().unwrap_or(""))
                            }
                            None => chain.to_string(),
                        };
                        let ty = through(&chain, fc.lang, &fc.file);
                        if ty.as_deref().is_some_and(|ty| library(ty, fc.lang)) {
                            continue;
                        }
                        typed_call = CallSite {
                            receiver: ty.map(Receiver::Type),
                            ..cs.clone()
                        };
                        &typed_call
                    }
                    _ => cs,
                };
                // Bare calls to language builtins (`len(x)`, `make(...)`) are not
                // project functions; skip them so they don't resolve to some
                // same-named definition and inflate the graph.
                if is_library_call(fc.lang, cs) {
                    continue;
                }
                // A Kotlin infix call on a value of no type known (`Users.id
                // eq 1`, a property this reads no type of): every overload of
                // the name across the DSL's receivers (`eq` has a dozen) says
                // nothing of which — none, rather than all.
                if fc.lang == Lang::Kotlin
                    && cs.qualifier.as_deref() == Some("infix")
                    && (guessed
                        || !matches!(
                            &cs.receiver,
                            Some(Receiver::Type(t) | Receiver::Declared(t))
                                if !t.starts_with(RETURNED) && !t.starts_with(THROUGH)
                        ))
                {
                    continue;
                }
                let Some(caller_name) = cs.caller.as_deref() else {
                    continue; // a top-level call has no caller function node
                };
                let Some(caller) = resolve_caller(
                    &defs.callers,
                    &fc.file,
                    caller_name,
                    cs.line,
                    &nodes,
                    &fc.bodies,
                ) else {
                    continue;
                };
                // Which kind of Ruby method the call wants: a singleton
                // (`true`) or an instance's — and, of methods of the class,
                // those of that kind where one is (with none, the others:
                // `module_function`, `extend self`).
                let ruby_wants: Option<bool> = (fc.lang == Lang::Ruby)
                    .then(|| match (cs.qualifier.as_deref(), &cs.receiver) {
                        (Some("class"), _) => Some(true),
                        (Some("new"), _) => Some(false),
                        (_, Some(Receiver::Type(_) | Receiver::Declared(_))) => Some(false),
                        (_, Some(Receiver::Own)) => Some(singleton_node[caller]),
                        (_, None) if !cs.method => Some(singleton_node[caller]),
                        _ => None,
                    })
                    .flatten();
                let ruby_kind = |found: &mut Vec<usize>| {
                    if let Some(wants) = ruby_wants
                        && found
                            .iter()
                            .any(|&c| owner[c].is_some() && singleton_node[c] == wants)
                    {
                        found.retain(|&c| owner[c].is_none() || singleton_node[c] == wants);
                    }
                };
                // Ruby's `Cart.new(…)`: the class's own `new` where it (or a
                // class above it) defines one (`Sinatra::Base.new`, a `def
                // self.new`), else `Cart#initialize`, its own or the one it
                // inherits — never another class's `new` (a Rails
                // controller's action).
                // (Bare, or on `self`: the caller's own class's.)
                if fc.lang == Lang::Ruby
                    && cs.callee == "new"
                    && (cs.method || cs.receiver.is_none())
                {
                    let ty = match &cs.receiver {
                        Some(Receiver::Type(ty) | Receiver::Declared(ty)) => Some(ty.as_str()),
                        Some(Receiver::Own) | None => owner[caller],
                    };
                    if let Some(ty) = ty {
                        let mut own_new = family(ty, "new", false);
                        // (A class's `new` is its own singleton's; a
                        // controller's `new` action is no constructor.)
                        own_new.retain(|&c| singleton_node[c]);
                        let found = if own_new.is_empty() {
                            family(ty, "initialize", false)
                        } else {
                            own_new
                        };
                        for callee in found {
                            if edges.len() < MAX_EDGES
                                && (in_test || !test_node[callee])
                                && facts[callee].takes_call(cs, fc.lang, owner[callee])
                            {
                                edges.insert((caller, callee));
                            }
                        }
                    }
                    continue;
                }
                let shape = (
                    callee_key,
                    cs.method,
                    cs.receiver.is_none(),
                    cs.arity,
                    qualifier_key,
                );
                let callees = resolved
                    .entry(shape)
                    .or_insert_with(|| resolve_callees(&defs, &nodes, &facts, &owner, cs, &site));
                // Dart's `http.Response(…)`, `shelf_io.serve(…)`: a top-level
                // function or a class of the library imported under that
                // prefix — none of the project's where it lies outside it.
                if fc.lang == Lang::Dart
                    && let Some(uri) = cs
                        .qualifier
                        .as_deref()
                        .and_then(|q| q.strip_prefix("import:"))
                {
                    let Some(root) = dart_library_root(uri, &fc.file, &dart_files) else {
                        continue;
                    };
                    let callee_name = logical_name(&cs.callee, Some(fc.lang));
                    let found: Vec<usize> = defs
                        .named
                        .get(callee_name)
                        .into_iter()
                        .flat_map(|named| named.all.iter().copied())
                        .filter(|&i| {
                            nodes[i].file.starts_with(&root)
                                && owner[i].is_none_or(|o| {
                                    o.rsplit('.').next() == Some(nodes[i].name.as_str())
                                })
                        })
                        .collect();
                    if found.len() <= MAX_CANDIDATES {
                        for callee in found {
                            if edges.len() < MAX_EDGES
                                && (in_test || !test_node[callee])
                                && facts[callee].takes_call(cs, fc.lang, owner[callee])
                                && fits_arguments(callee, cs, fc.lang)
                            {
                                edges.insert((caller, callee));
                            }
                        }
                    }
                    continue;
                }
                // Scala's `pkg.member(…)`: the package's own function (a
                // package object's) or its object's `apply` — and nothing
                // else, as the package names the member. Through a path,
                // the last package's (`os.watch.watch(…)`), or the object's
                // it ends in (`os.ResourcePath.resource(…)`).
                // (A path names its package whole: `scala.math` is no
                // `com.acme.math`.)
                let scala_path = (fc.lang == Lang::Scala)
                    .then_some(cs.qualifier.as_deref())
                    .flatten()
                    .filter(|path| !template_path(path) && !reverse_route(path))
                    .map(|path| match path.rsplit_once('.') {
                        Some((package, object)) if object.starts_with(char::is_uppercase) => {
                            (package, Some(object))
                        }
                        _ => (path, None),
                    });
                if let Some((path, object)) = scala_path
                    && let package = path.rsplit('.').next().unwrap_or(path)
                    && let Some(package_files) = scala_packages.get(package)
                {
                    let callee_name = logical_name(&cs.callee, Some(fc.lang));
                    let in_package = |i: &usize| {
                        let file = nodes[*i].file.as_path();
                        package_files.contains(file)
                            && (path == package
                                || scala_package_of.get(file).is_some_and(|p| {
                                    *p == path || p.ends_with(&format!(".{path}"))
                                }))
                    };
                    let mut found: Vec<usize> = defs
                        .named
                        .get(callee_name)
                        .into_iter()
                        .flat_map(|named| named.all.iter().copied())
                        .filter(in_package)
                        .filter(|&i| match object {
                            Some(object) => {
                                owner[i].is_some_and(|o| o.rsplit('.').next() == Some(object))
                            }
                            None => owner[i].is_none_or(|o| o.rsplit('.').next() == Some(package)),
                        })
                        .collect();
                    if object.is_none() {
                        found.extend(applies_of(callee_name).into_iter().filter(in_package));
                    }
                    if found.len() <= MAX_CANDIDATES {
                        for callee in found {
                            if edges.len() < MAX_EDGES
                                && (in_test || !test_node[callee])
                                && facts[callee].takes_call(cs, fc.lang, owner[callee])
                                && fits_arguments(callee, cs, fc.lang)
                            {
                                edges.insert((caller, callee));
                            }
                        }
                    }
                    continue;
                }
                // Go's `pkg.Func(…)`: an imported package's function — the
                // project package's of that name, or else a library's, no
                // project function at all.
                if fc.lang == Lang::Go
                    && cs.method
                    && cs.receiver.is_none()
                    && let Some(package) = cs.qualifier.as_deref()
                    && fc.context.imports.iter().any(|i| i == package)
                {
                    let callee_name = logical_name(&cs.callee, Some(fc.lang));
                    let found: Vec<usize> = go_packages
                        .get(package)
                        .map(|package_files| {
                            defs.named
                                .get(callee_name)
                                .into_iter()
                                .flat_map(|named| named.all.iter().copied())
                                .filter(|&i| {
                                    owner[i].is_none()
                                        && package_files.contains(nodes[i].file.as_path())
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    if found.len() <= MAX_CANDIDATES {
                        for callee in found {
                            if edges.len() < MAX_EDGES
                                && (in_test || !test_node[callee])
                                && facts[callee].takes_call(cs, fc.lang, owner[callee])
                                && fits_arguments(callee, cs, fc.lang)
                            {
                                edges.insert((caller, callee));
                            }
                        }
                    }
                    continue;
                }
                // A Play template: `views.html.index(…)`, Java's
                // `views.html.index.render(…)` — the function its file
                // compiles to, no method of the name elsewhere.
                if matches!(fc.lang, Lang::Java | Lang::Scala)
                    && let Some(path) = cs.qualifier.as_deref()
                    && template_path(path)
                {
                    let called = format!("{path}.{}", cs.callee);
                    let template = templates.get(&called).or_else(|| {
                        matches!(cs.callee.as_str(), "render" | "apply" | "f")
                            .then(|| templates.get(path))
                            .flatten()
                    });
                    if let Some(&template) = template
                        && edges.len() < MAX_EDGES
                    {
                        edges.insert((caller, template));
                    }
                    continue;
                }
                // `super.f()`, `base.F()`: the method of the caller's class's
                // bases (the nearest's), which it overrides — a library's
                // where no project class above it has one.
                if matches!(&cs.receiver, Some(Receiver::Declared(t)) if t == SUPER) {
                    let callee_name = logical_name(&cs.callee, Some(fc.lang));
                    let found: Vec<usize> = owner[caller]
                        .map(|own| own.rsplit(['.', ':']).next().unwrap_or(own))
                        .and_then(|own| bases_of.get(own))
                        .into_iter()
                        .flatten()
                        .flat_map(|base| family(base, callee_name, false))
                        .collect();
                    for callee in found {
                        if edges.len() < MAX_EDGES
                            && facts[callee].takes_call(cs, fc.lang, owner[callee])
                            && fits_arguments(callee, cs, fc.lang)
                        {
                            edges.insert((caller, callee));
                        }
                    }
                    continue;
                }
                // Play's reverse router (`routes.HomeController.index`): code
                // the build generates, returning the action's route — no
                // call of the action, nor of any function of the project's.
                if matches!(fc.lang, Lang::Java | Lang::Scala)
                    && cs.qualifier.as_deref().is_some_and(reverse_route)
                {
                    continue;
                }
                // Zig's `posix.read(…)` with `posix = @import("posix.zig")`:
                // that file's own function of the name, nothing else's.
                if fc.lang == Lang::Zig
                    && let Some(path) = cs.qualifier.as_deref()
                    && path.ends_with(".zig")
                {
                    let file = fc
                        .file
                        .parent()
                        .map(|dir| lexical_join(dir, path))
                        .unwrap_or_default();
                    let stem = file.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                    let callee_name = logical_name(&cs.callee, Some(fc.lang));
                    let found: Vec<usize> = defs
                        .named
                        .get(callee_name)
                        .into_iter()
                        .flat_map(|named| named.all.iter().copied())
                        .filter(|&i| nodes[i].file == file && owner[i].is_none_or(|o| o == stem))
                        .collect();
                    for callee in found {
                        if edges.len() < MAX_EDGES
                            && (in_test || !test_node[callee])
                            && facts[callee].takes_call(cs, fc.lang, owner[callee])
                        {
                            edges.insert((caller, callee));
                        }
                    }
                    continue;
                }
                // A Scala object called as a function (`RelPath("a")`), where
                // no function in scope has the name: its `apply`.
                if fc.lang == Lang::Scala && !cs.method && callees.is_empty() {
                    let callee_name = logical_name(&cs.callee, Some(fc.lang));
                    let found: Vec<usize> = applies_of(callee_name)
                        .into_iter()
                        .filter(|&i| nodes[i].file == fc.file || site.sees(&nodes[i].file))
                        .collect();
                    if !found.is_empty() && found.len() <= MAX_CANDIDATES {
                        for callee in found {
                            if edges.len() < MAX_EDGES
                                && (in_test || !test_node[callee])
                                && facts[callee].takes_call(cs, fc.lang, owner[callee])
                                && fits_arguments(callee, cs, fc.lang)
                            {
                                edges.insert((caller, callee));
                            }
                        }
                        continue;
                    }
                }
                // A receiver of a known type calls that type's method of the
                // name: `cart.total()` after `cart = Cart()` is `Cart.total`,
                // not every `total` there is. A type with no method of the
                // name (an inherited one) keeps them all.
                let wanted = match &cs.receiver {
                    Some(Receiver::Own) => owner[caller],
                    Some(Receiver::Type(ty) | Receiver::Declared(ty)) => Some(ty.as_str()),
                    None => None,
                };
                let callee_name = logical_name(&cs.callee, Some(fc.lang));
                // A language whose types inherit (or, Go, embed) methods: a
                // receiver declared as a type, or the caller's own class,
                // calls the type's method, its own or its nearest base's,
                // or a subtype's override — wherever each is defined.
                let inherits = has_inheritance(fc.lang);
                if inherits
                    && let Some(ty) = wanted
                    && matches!(cs.receiver, Some(Receiver::Own | Receiver::Declared(_)))
                {
                    let mut found = family(ty, callee_name, true);
                    ruby_kind(&mut found);
                    // Kotlin: where no member of the name takes what the call
                    // passes (`c eq 3`, `Column.eq(Column)` the member), an
                    // extension of the name that does (the `infix fun <T>
                    // ExpressionWithColumnType<T>.eq(t: T)` a builder
                    // declares).
                    if fc.lang == Lang::Kotlin
                        && let Some(arguments) = cs.arguments.as_deref()
                        && !found.is_empty()
                        && found.iter().all(|&c| !fits_kinds(c, arguments, fc.lang))
                    {
                        let extended: Vec<usize> = defs
                            .named
                            .get(callee_name)
                            .into_iter()
                            .flat_map(|named| named.all.iter().copied())
                            .filter(|&c| {
                                extension_nodes.contains(&c)
                                    && (nodes[c].file == fc.file || site.sees(&nodes[c].file))
                                    && fits_kinds(c, arguments, fc.lang)
                            })
                            .collect();
                        if !extended.is_empty() {
                            found = extended;
                        }
                    }
                    // Classes of one name in several files (a library's
                    // twin modules: `exposed-dao`'s `Entity` and
                    // `exposed-dao-r2dbc`'s): of each name, the ones this
                    // file sees, where it sees one.
                    let visible = |c: usize| nodes[c].file == fc.file || site.sees(&nodes[c].file);
                    let twins: HashSet<&str> = {
                        let mut files_of: HashMap<&str, HashSet<&Path>> = HashMap::new();
                        for &c in &found {
                            if let Some(o) = owner[c] {
                                files_of
                                    .entry(o)
                                    .or_default()
                                    .insert(nodes[c].file.as_path());
                            }
                        }
                        files_of
                            .into_iter()
                            .filter(|(_, files)| files.len() > 1)
                            .map(|(o, _)| o)
                            .collect()
                    };
                    for twin in twins {
                        let of_twin = |c: &usize| owner[*c] == Some(twin);
                        if found.iter().filter(|c| of_twin(c)).any(|&c| visible(c)) {
                            found.retain(|c| !of_twin(c) || visible(*c));
                        }
                    }
                    let declared_here = declared_types
                        .contains(ty.rsplit(['.', ':']).next().unwrap_or(ty))
                        || project_types.contains(ty);
                    let targets: Vec<usize> = match (found.is_empty(), declared_here) {
                        (false, _) if found.len() <= MAX_CANDIDATES => found,
                        (false, _) => Vec::new(),
                        // A type the project does not declare (`List`,
                        // `String`): its methods are not the project's,
                        // but for the project's extensions of it.
                        (true, false) if matches!(cs.receiver, Some(Receiver::Declared(_))) => {
                            callees
                                .iter()
                                .copied()
                                .filter(|c| extension_nodes.contains(c))
                                .collect()
                        }
                        // A project class with no method of the name, nor
                        // any project class above or below it, in a language
                        // whose classes name every base: one it inherits from
                        // a library (`hashCode`, an `Activity`'s `onCreate`),
                        // or one the project extends it with.
                        (true, true) if closed_hierarchy(fc.lang) && !guessed => callees
                            .iter()
                            .copied()
                            .filter(|c| extension_nodes.contains(c))
                            .collect(),
                        // A Scala class's field read (`res.exitCode` on a
                        // case class's value, `c.id`): no method at all.
                        (true, true) if fc.lang == Lang::Scala && has_field(ty, callee_name) => {
                            Vec::new()
                        }
                        // No method of the name on the type or above it
                        // that this can see (a mixin, a library's base): by
                        // the name in scope, as with no type.
                        (true, _) => callees.clone(),
                    };
                    for callee in targets {
                        if edges.len() < MAX_EDGES
                            && (in_test || !test_node[callee])
                            && facts[callee].takes_call(cs, fc.lang, owner[callee])
                            && fits_arguments(callee, cs, fc.lang)
                        {
                            edges.insert((caller, callee));
                        }
                    }
                    continue;
                }
                // A Lean namespace is named from where the call is: `Voice`
                // for `Clew.Extracted.Voice`, opened.
                // So is an Elixir module by the last of its parts, as an
                // `alias Shop.Cart` lets `Cart.add` say it.
                let owned_by = |c: usize, ty: &str| match owner[c] {
                    Some(o) if matches!(fc.lang, Lang::Lean | Lang::Elixir) => {
                        o == ty || o.strip_suffix(ty).is_some_and(|o| o.ends_with('.'))
                    }
                    // A Go or Zig type named through its package or module
                    // (`*httpz.Response`) is its struct's.
                    Some(o) if matches!(fc.lang, Lang::Go | Lang::Zig) => {
                        o == ty.rsplit('.').next().unwrap_or(ty)
                    }
                    // Ruby's class by its path (`DependencyParser::Ruby::Parse`).
                    Some(o) if fc.lang == Lang::Ruby => o == ty.rsplit("::").next().unwrap_or(ty),
                    o => o == Some(ty),
                };
                let mut narrowed: Vec<usize> = wanted
                    .map(|ty| {
                        callees
                            .iter()
                            .copied()
                            .filter(|&c| owned_by(c, ty))
                            .collect()
                    })
                    .unwrap_or_default();
                // A Rust type's function is its own wherever it is defined:
                // `Builder::new()` in a file with other `new`s, `Builder`'s
                // defined in another — the name's nearest were not it. So
                // is an Elixir or Erlang module's (`Cart.add(…)`,
                // `cart:add(…)`), which a call reaches with no import: a
                // test module's every call of the module it tests had no
                // callee.
                // Java's, Kotlin's and Scala's classes in the same package,
                // C#'s in the same namespace and Ruby's once required are
                // named with no import either (`Cart.add(…)` from the
                // test beside it): where nothing in scope is the type's,
                // its own function is.
                if narrowed.is_empty()
                    && matches!(
                        fc.lang,
                        Lang::Rust
                            | Lang::Elixir
                            | Lang::Erlang
                            | Lang::Java
                            | Lang::Kotlin
                            | Lang::Scala
                            | Lang::CSharp
                            | Lang::Ruby
                            | Lang::Go
                            | Lang::Dart
                            | Lang::Zig
                    )
                    && let Some(ty) = wanted
                {
                    let last = ty.rsplit(['.', ':']).next().unwrap_or(ty);
                    narrowed = by_owner
                        .get(&(last, callee_name))
                        .into_iter()
                        .flatten()
                        .copied()
                        .filter(|&c| {
                            owned_by(c, ty)
                                && facts[c].takes_call(cs, fc.lang, owner[c])
                                && fits_arguments(c, cs, fc.lang)
                        })
                        .collect();
                }
                // An exact type with no method of the name: the one it
                // inherits (or, in Go, has from an embedded type).
                if narrowed.is_empty()
                    && inherits
                    && let Some(Receiver::Type(ty)) = &cs.receiver
                {
                    narrowed = family(ty, callee_name, false);
                    if narrowed.len() > MAX_CANDIDATES {
                        narrowed.clear();
                    }
                }
                // Ruby's class by its path: of the classes of its name, the
                // one in the file its path names, as Ruby's autoloading lays
                // them out (`DependencyParser::Ruby::Parse` in
                // `dependency_parser/ruby/parse.rb`), where one is.
                if fc.lang == Lang::Ruby
                    && let Some(ty) = wanted.filter(|ty| ty.contains("::"))
                {
                    let file: String =
                        ty.split("::").map(snake_case).collect::<Vec<_>>().join("/") + ".rb";
                    let at_path = |c: &usize| {
                        nodes[*c]
                            .file
                            .to_string_lossy()
                            .ends_with(&format!("/{file}"))
                    };
                    if narrowed.iter().any(at_path) {
                        narrowed.retain(at_path);
                    }
                }
                // A bare call in a method that names nothing in scope: the
                // class's own method, or the one it inherits (`getName()`
                // in `Owner` is `Person`'s).
                if !cs.method
                    && callees.is_empty()
                    && implicit_self(fc.lang)
                    && let Some(own) = owner[caller]
                {
                    let mut found = family(own, callee_name, true);
                    ruby_kind(&mut found);
                    if found.len() > MAX_CANDIDATES {
                        found.clear();
                    }
                    for callee in found {
                        if edges.len() < MAX_EDGES
                            && (in_test || !test_node[callee])
                            && facts[callee].takes_call(cs, fc.lang, owner[callee])
                            && fits_arguments(callee, cs, fc.lang)
                        {
                            edges.insert((caller, callee));
                        }
                    }
                    continue;
                }
                // A Rust receiver of a type the project defines nothing of —
                // `Vec`, `str`, a type parameter — calls none of its
                // functions: `haystack.len()` on a `&str` drew an edge to
                // every `len` there was. (A language with inheritance keeps
                // them: the method may be a base class's.)
                // So does a Lean name in a namespace the project has none
                // of: `Nat.succ` is no project `succ`.
                let foreign = match (&cs.receiver, fc.lang) {
                    (Some(Receiver::Type(ty)), Lang::Rust) => !project_types.contains(ty.as_str()),
                    (Some(Receiver::Type(ty)), Lang::Lean) => {
                        !(0..nodes.len()).any(|c| owner[c].is_some() && owned_by(c, ty))
                    }
                    // A JavaScript runtime's object, as no project class of
                    // that name is.
                    (Some(Receiver::Type(ty)), Lang::JavaScript | Lang::TypeScript | Lang::Tsx) => {
                        JS_GLOBALS.contains(&ty.as_str()) && !project_types.contains(ty.as_str())
                    }
                    // A module the project defines none of (`Map.put`,
                    // `:crypto.hash`, `lists:map`): the library's.
                    (Some(Receiver::Type(ty)), Lang::Elixir | Lang::Erlang) => {
                        !module_names.contains(ty.as_str())
                    }
                    // A value of a type exactly known and the project's
                    // none (`new AtomicReference(…)`, `std::vector`): the
                    // library's methods, but for the project's extensions.
                    (
                        Some(Receiver::Type(ty)),
                        Lang::Java
                        | Lang::Kotlin
                        | Lang::Scala
                        | Lang::CSharp
                        | Lang::Dart
                        | Lang::Cpp,
                    ) => {
                        let last = ty.rsplit(['.', ':']).next().unwrap_or(ty);
                        let declared = declared_types.contains(last);
                        (!project_types.contains(last)
                            && !project_types.contains(ty.as_str())
                            && !declared)
                            // A project class neither it nor a project class
                            // above it defines the method of: the library's.
                            || (declared && narrowed.is_empty() && closed_hierarchy(fc.lang))
                    }
                    // Zig's standard library's (`writer: *std.Io.Writer`).
                    (Some(Receiver::Type(ty)), Lang::Zig) => ty == "std" || ty.starts_with("std."),
                    // A value an imported library package's function made
                    // (`httptest.NewRecorder()`).
                    (Some(Receiver::Type(ty)), Lang::Go) => {
                        ty.split_once('.').is_some_and(|(pkg, _)| {
                            fc.context.imports.iter().any(|i| i == pkg)
                                && !go_packages.contains_key(pkg)
                        })
                    }
                    _ => false,
                };
                let extensions_only: Vec<usize>;
                let targets: &[usize] = if foreign {
                    extensions_only = callees
                        .iter()
                        .copied()
                        .filter(|c| extension_nodes.contains(c))
                        .collect();
                    &extensions_only
                } else if narrowed.is_empty() {
                    callees
                } else {
                    &narrowed
                };
                // A method called on a value of no known type in Java,
                // Kotlin, Scala or C# is some class's: never a top-level
                // function, but for an extension of a type.
                let methods_only = cs.method
                    && cs.receiver.is_none()
                    && matches!(
                        fc.lang,
                        Lang::Java | Lang::Kotlin | Lang::Scala | Lang::CSharp
                    );
                // A bare call in Dart, JavaScript, Python or PHP is a
                // function's, or one of the caller's own class (or its
                // bases): another class's method needs its object — in
                // C++ too, named with no scope (`run()` in `main` is no
                // `ExampleQt::run`).
                let own_or_free = !cs.method
                    && (matches!(
                        fc.lang,
                        Lang::Dart
                            | Lang::JavaScript
                            | Lang::TypeScript
                            | Lang::Tsx
                            | Lang::Python
                            | Lang::Php
                    ) || (fc.lang == Lang::Cpp && cs.qualifier.is_none()));
                // An Elixir bare call: a function of its own file, or of a
                // module it imports.
                let elixir_bare = !cs.method && fc.lang == Lang::Elixir;
                // A C++ call through `.` or `->` is a member's: among
                // candidates that hold some class's member, a free function
                // of the same name (`spdlog::info` for `logger->info(…)`) is
                // not one. With no member in sight, the free ones stay: the
                // member may be one no outline sees (a macro's).
                let cpp_member = cs.method
                    && fc.lang == Lang::Cpp
                    && targets.iter().any(|&c| owner[c].is_some())
                    && targets.iter().any(|&c| owner[c].is_none());
                // A C++ call that names its scope (`spdlog::info(…)`,
                // `Cart::make(…)`, `Base::draw()`): a member of that class or
                // of one of its bases; a free function only when the scope is
                // no class of the project's (a namespace).
                let cpp_scoped = (!cs.method && fc.lang == Lang::Cpp)
                    .then_some(cs.qualifier.as_deref())
                    .flatten();
                // A C++ bare call in a member function: its class's member
                // (or a base's), where one has the name — name lookup finds
                // the class's scope before any around it (`listeners()` in
                // `UnitTestImpl` is `this->listeners()`, never `UnitTest`'s).
                let cpp_own = (!cs.method && cs.qualifier.is_none() && fc.lang == Lang::Cpp)
                    .then_some(owner[caller])
                    .flatten()
                    .map(|own| own.rsplit(['.', ':']).next().unwrap_or(own))
                    .filter(|own| {
                        targets.iter().any(|&c| {
                            owner[c].is_some_and(|o| {
                                let o = o.rsplit(['.', ':']).next().unwrap_or(o);
                                o == *own || descends(own, o)
                            })
                        })
                    });
                // A method called on a value of no known type: no function
                // declared inside another (a local `def`, an anonymous
                // class's method) is the value's.
                // A bare call of a function the caller declares inside it,
                // before the call: that one, as its scope shadows all others.
                let local = |c: usize| {
                    nested_node[c]
                        && nodes[c].file == fc.file
                        && nodes[caller].line < nodes[c].line
                        && nodes[c].line < cs.line
                };
                let outer_only: Vec<usize>;
                let targets: &[usize] = if cs.method
                    && cs.receiver.is_none()
                    && targets.iter().any(|&c| nested_node[c])
                {
                    outer_only = targets
                        .iter()
                        .copied()
                        .filter(|&c| !nested_node[c])
                        .collect();
                    &outer_only
                } else if !cs.method && targets.iter().any(|&c| local(c)) {
                    outer_only = targets.iter().copied().filter(|&c| local(c)).collect();
                    &outer_only
                } else if !cs.method
                    && targets
                        .iter()
                        .filter(|&&c| nested_node[c] && nodes[c].file == fc.file)
                        .count()
                        >= 2
                    && targets.iter().any(|&c| {
                        nested_node[c] && nodes[c].file == fc.file && nodes[c].line < cs.line
                    })
                {
                    // Of functions of the name declared inside others before
                    // the call — each a closure's own (a Dart test group's
                    // `buildSubject`) — the nearest: the one whose scope the
                    // call is in.
                    outer_only = targets
                        .iter()
                        .copied()
                        .filter(|&c| {
                            nested_node[c] && nodes[c].file == fc.file && nodes[c].line < cs.line
                        })
                        .max_by_key(|&c| nodes[c].line)
                        .into_iter()
                        .collect();
                    &outer_only
                } else {
                    targets
                };
                // A file built for some platforms (cats-effect's `core/js/`)
                // reaches no definition built only for others
                // (`core/jvm-native/`), where one built for its own is.
                let same_platform: Vec<usize>;
                let targets: &[usize] = match source_platforms(&fc.file) {
                    Some(mine)
                        if targets.iter().any(|&c| {
                            node_platforms[c].is_some_and(|t| !t.iter().any(|p| mine.contains(p)))
                        }) =>
                    {
                        same_platform = targets
                            .iter()
                            .copied()
                            .filter(|&c| {
                                node_platforms[c].is_none_or(|t| t.iter().any(|p| mine.contains(p)))
                            })
                            .collect();
                        if same_platform.is_empty() {
                            targets
                        } else {
                            &same_platform
                        }
                    }
                    _ => targets,
                };
                let kept: Vec<usize>;
                let targets: &[usize] = if let Some(own) = cpp_own {
                    kept = targets
                        .iter()
                        .copied()
                        .filter(|&c| {
                            owner[c].is_some_and(|o| {
                                let o = o.rsplit(['.', ':']).next().unwrap_or(o);
                                o == own || descends(own, o)
                            })
                        })
                        .collect();
                    &kept
                } else if cpp_member {
                    kept = targets
                        .iter()
                        .copied()
                        .filter(|&c| owner[c].is_some())
                        .collect();
                    &kept
                } else if let Some(scope) = cpp_scoped {
                    let a_class = declared_types.contains(scope);
                    kept = targets
                        .iter()
                        .copied()
                        .filter(|&c| match owner[c] {
                            None => !a_class,
                            // `fmt::rgb(…)`: a constructor, its class named
                            // in the scope's.
                            Some(o) => {
                                let o = o.rsplit(['.', ':']).next().unwrap_or(o);
                                o == scope || descends(scope, o) || nodes[c].name == o
                            }
                        })
                        .collect();
                    &kept
                } else if fc.lang == Lang::Haskell
                    && !cs.method
                    && cs.arity.is_some_and(|n| n >= 1)
                    && targets.iter().any(|&c| owner[c].is_some())
                {
                    // A Haskell class's method: the instance at a type the
                    // caller's signature names (`setAuthors :: … -> Pandoc`
                    // calls `instance HasMeta Pandoc`'s `setMeta`); with none,
                    // the class's own signature, at a type it cannot tell.
                    let name = logical_name(&cs.callee, Some(fc.lang));
                    let returns_its_type = targets.iter().any(|&c| {
                        owner[c].is_some()
                            && signature_types.get(&(nodes[c].file.as_path(), nodes[c].line))
                                == Some(&"=")
                    });
                    kept = signature_types
                        .get(&(nodes[caller].file.as_path(), nodes[caller].line))
                        .filter(|_| returns_its_type)
                        .into_iter()
                        .flat_map(|ty| by_owner.get(&(*ty, name)).into_iter().flatten())
                        .copied()
                        .filter(|&c| {
                            haskell_instances.contains(&(nodes[c].file.as_path(), nodes[c].line))
                        })
                        .collect();
                    if kept.is_empty() { targets } else { &kept }
                } else if ruby_wants.is_some() {
                    // Ruby: a call on the class itself (`Job.kind`,
                    // `self.class.kind`) is a singleton method's (`def
                    // self.kind`); on an instance (`job.kind`,
                    // `new(*args).perform`) an instance method's; on
                    // `self` or bare, the kind the caller is.
                    kept = {
                        let mut found = targets.to_vec();
                        ruby_kind(&mut found);
                        // A bare call in a class's method is `self`'s: its
                        // class's, one above or below it, a module's (one it
                        // may include) or the top level's — never an
                        // unrelated class's (a test's `doc_methods(:x)`, a
                        // fixture accessor, is no `RepoSubscription#
                        // doc_methods`).
                        if !cs.method
                            && cs.receiver.is_none()
                            && cs.qualifier.is_none()
                            && let Some(own) = owner[caller]
                            && !ruby_modules.contains(own.rsplit("::").next().unwrap_or(own))
                        {
                            let own = own.rsplit("::").next().unwrap_or(own);
                            found.retain(|&c| {
                                owner[c].is_none_or(|o| {
                                    let o = o.rsplit("::").next().unwrap_or(o);
                                    o == own
                                        || descends(own, o)
                                        || descends(o, own)
                                        || ruby_modules.contains(o)
                                })
                            });
                        }
                        found
                    };
                    &kept
                } else if !cs.method && cs.qualifier.is_none() && fc.lang == Lang::Java {
                    // A Java bare call: a method of its class or a base, of a
                    // class around it (its file's), a constructor, or one a
                    // `static` import brings, where one has the name — not
                    // another class's of the package it neither inherits nor
                    // imports. (With none, an anonymous class's base's: `new
                    // PetType() {{ setName(…); }}`.)
                    kept = targets
                        .iter()
                        .copied()
                        .filter(|&c| {
                            nodes[c].file == fc.file
                                || imported.contains(&nodes[c].file)
                                || owner[c].is_none_or(|o| {
                                    let o = o.rsplit('.').next().unwrap_or(o);
                                    nodes[c].name == o
                                        || owner[caller].is_some_and(|own| {
                                            let own = own.rsplit('.').next().unwrap_or(own);
                                            own == o || descends(own, o)
                                        })
                                })
                        })
                        .collect();
                    if kept.is_empty() { targets } else { &kept }
                } else if elixir_bare {
                    let reached = bare_scopes.get(fc.file.as_path());
                    let mut found: Vec<usize> = targets
                        .iter()
                        .copied()
                        .filter(|&c| {
                            nodes[c].file == fc.file
                                || reached.is_some_and(|r| r.contains(nodes[c].file.as_path()))
                        })
                        .collect();
                    // (Its own module's, where one has the name: a module
                    // nested in it is reached by its name, `SamplePlug.call`.)
                    if owner[caller].is_some() && found.iter().any(|&c| owner[c] == owner[caller]) {
                        found.retain(|&c| owner[c] == owner[caller]);
                    } else if owner[caller].is_none() {
                        // (From a test block, of no module of its own: not a
                        // module nested in another of them, `Plug.RouterTest.
                        // SamplePlug` beside `Plug.RouterTest`.)
                        let outer: Vec<&str> = found.iter().filter_map(|&c| owner[c]).collect();
                        found.retain(|&c| {
                            owner[c].is_none_or(|o| {
                                !outer.iter().any(|p| {
                                    o.len() > p.len()
                                        && o.starts_with(p)
                                        && o[p.len()..].starts_with('.')
                                })
                            })
                        });
                    }
                    kept = found;
                    &kept
                } else if methods_only || own_or_free {
                    kept = targets
                        .iter()
                        .copied()
                        .filter(|&c| match (methods_only, owner[c]) {
                            (true, None) => extension_nodes.contains(&c),
                            (true, Some(_)) => true,
                            (false, None) => true,
                            // A constructor (`CounterCubit()`) is called
                            // bare from anywhere.
                            (false, Some(o))
                                if nodes[c].name == o.rsplit('.').next().unwrap_or(o) =>
                            {
                                true
                            }
                            // `Cart::make()`: the class named.
                            (false, Some(o))
                                if cs.qualifier.as_deref().is_some_and(|q| {
                                    q == o.rsplit(['.', ':']).next().unwrap_or(o)
                                }) =>
                            {
                                true
                            }
                            (false, Some(o)) => {
                                owner[caller].is_some_and(|own| own == o || descends(own, o))
                            }
                        })
                        .collect();
                    &kept
                } else {
                    targets
                };
                // A class called: its `__init__`, of a class this file
                // defines or imports.
                let constructed = (fc.lang == Lang::Python)
                    .then(|| inits.get(cs.callee.as_str()))
                    .flatten()
                    .into_iter()
                    .flatten()
                    .copied()
                    .filter(|&i| nodes[i].file == fc.file || imported.contains(&nodes[i].file));
                for callee in targets.iter().copied().chain(constructed) {
                    // `f(a)` is `f/1`: no call of `f/2`.
                    if !facts[callee].takes_call(cs, fc.lang, owner[callee])
                        || !fits_arguments(callee, cs, fc.lang)
                    {
                        continue;
                    }
                    // `client.get(…)`: no value is a test class's, its own
                    // class's methods called on another value included —
                    // but for its extensions, which are called on another
                    // value (`assertThat(x).isSymmetricEqualTo(y)`).
                    if cs.method
                        && cs.receiver.is_none()
                        && test_method[callee]
                        && !extension_nodes.contains(&callee)
                    {
                        continue;
                    }
                    // A self-call is kept as a callee only (see `finalize`).
                    if edges.len() < MAX_EDGES
                        && (in_test || !test_node[callee])
                        && facts[callee].takes_call(cs, fc.lang, owner[callee])
                        && fits_arguments(callee, cs, fc.lang)
                    {
                        edges.insert((caller, callee));
                    }
                }
            }
        }

        // Projects that share a folder and nothing else (a repository of
        // samples, each with its own build): none calls another's code.
        if let Some(universe) = independent_projects(&nodes, &common, scope) {
            edges.retain(
                |&(caller, callee)| match (universe[caller], universe[callee]) {
                    (Some(a), Some(b)) => a == b,
                    _ => true,
                },
            );
        }

        finalize(nodes, edges)
    }

    /// Build from explicit caller→callee edges — used by the LSP-precise pass,
    /// which resolves calls exactly instead of by name. `defs` must already be
    /// the callable definitions (edges index into them).
    pub fn from_callable_defs(defs: Vec<Def>, edges: HashSet<(usize, usize)>) -> Self {
        let nodes = defs
            .into_iter()
            .map(|d| SymNode {
                name: d.name,
                kind: d.kind,
                file: d.file,
                line: d.line,
                callers: Vec::new(),
                callees: Vec::new(),
            })
            .collect();
        finalize(nodes, edges)
    }

    /// The project's callable definitions (functions/methods), in a stable order
    /// — the node set the LSP-precise pass maps call-hierarchy results back
    /// onto.
    pub fn callable(defs: &[Def]) -> Vec<Def> {
        defs.iter()
            .filter(|d| is_callable(&d.kind))
            .cloned()
            .collect()
    }

    /// Every def's [`SymKey`], in `defs` order.
    pub fn keys_of(defs: &[Def]) -> Vec<SymKey> {
        let ordinals = ordinals_of(
            defs.iter()
                .map(|d| (d.file.as_path(), d.name.as_str(), d.kind.as_str(), d.line)),
        );
        defs.iter()
            .zip(ordinals)
            .map(|(d, o)| (d.file.clone(), d.name.clone(), o))
            .collect()
    }

    /// Build the display graph from the full callable node set and symbol-keyed
    /// edges (the LSP-precise edge set, kept stable across edits by keying on
    /// `(file, name, ordinal)` rather than node index). Edges whose endpoints
    /// aren't in `defs` are dropped. `defs` must be the callable definitions.
    pub fn graph_from_sym_edges(defs: Vec<Def>, edges: &SymEdges) -> Self {
        let keys = Self::keys_of(&defs);
        let idx_edges: HashSet<(usize, usize)> = {
            let mut lookup: HashMap<&SymKey, usize> = HashMap::new();
            for (i, k) in keys.iter().enumerate() {
                lookup.entry(k).or_insert(i);
            }
            edges
                .iter()
                .filter_map(|(c, e)| Some((*lookup.get(c)?, *lookup.get(e)?)))
                .collect()
        };
        Self::from_callable_defs(defs, idx_edges)
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// The functions among the nodes: not the tests that call them.
    pub fn function_count(&self) -> usize {
        self.nodes.iter().filter(|n| !calls_only(&n.kind)).count()
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// The node `id`. Panics on an id that is not from this graph; see
    /// [`get`](Self::get) for a checked lookup.
    pub fn node(&self, id: usize) -> &SymNode {
        &self.nodes[id]
    }

    /// The node `id`, or `None` when no such node exists (an id kept across a
    /// rebuild, say).
    pub fn get(&self, id: usize) -> Option<&SymNode> {
        self.nodes.get(id)
    }

    /// Total number of distinct caller→callee edges.
    pub fn edge_count(&self) -> usize {
        self.nodes.iter().map(|n| n.callees.len()).sum()
    }

    /// Every node's ordinal among its file's same-name callables.
    pub fn ordinals(&self) -> &[u32] {
        &self.ordinals
    }

    /// The [`SymKey`] of node `id`.
    pub fn key_of(&self, id: usize) -> Option<SymKey> {
        let n = self.nodes.get(id)?;
        Some((n.file.clone(), n.name.clone(), self.ordinals[id]))
    }

    /// Each function's callees as [`SymKey`]s — the call-graph dependency
    /// edges the explain engine orders by. Keyed by the full identity, so two
    /// same-name functions of one file keep their own callee lists (a
    /// `(file, name)` key kept whichever came last).
    pub fn callee_keys(&self) -> HashMap<SymKey, Vec<SymKey>> {
        let key = |i: usize| {
            (
                self.nodes[i].file.clone(),
                self.nodes[i].name.clone(),
                self.ordinals[i],
            )
        };
        let mut out: HashMap<SymKey, Vec<SymKey>> = HashMap::new();
        for (i, n) in self.nodes.iter().enumerate() {
            // A test's key may be a same-named function's.
            if calls_only(&n.kind) {
                continue;
            }
            let callees = out.entry(key(i)).or_default();
            // Not itself: what it waits on to be explained first.
            callees.extend(n.callees.iter().filter(|&&c| c != i).map(|&c| key(c)));
            callees.sort();
            callees.dedup();
        }
        out
    }

    /// Aggregate the symbol-level graph to file level: the files that hold any
    /// function, and an edge A→B when a function in A calls one in B. This is the
    /// readable "module call-flow" view (601 functions collapse to ~30 files).
    pub fn file_graph(&self) -> (Vec<PathBuf>, Vec<(usize, usize)>) {
        let mut files: Vec<PathBuf> = self.nodes.iter().map(|n| n.file.clone()).collect();
        files.sort();
        files.dedup();
        let idx: HashMap<&Path, usize> = files
            .iter()
            .enumerate()
            .map(|(i, f)| (f.as_path(), i))
            .collect();
        let mut edge_set: HashSet<(usize, usize)> = HashSet::new();
        for n in &self.nodes {
            let a = idx[n.file.as_path()];
            for &c in &n.callees {
                let b = idx[self.nodes[c].file.as_path()];
                if a != b {
                    edge_set.insert((a, b));
                }
            }
        }
        let mut edges: Vec<(usize, usize)> = edge_set.into_iter().collect();
        edges.sort_unstable();
        (files, edges)
    }

    /// The node id for the FIRST (ordinal 0) callable named `name` in `file`,
    /// if present. Prefer [`id_of_key`](Self::id_of_key) when the ordinal is
    /// known: a file's second `new` is not its first.
    pub fn id_of(&self, file: &Path, name: &str) -> Option<usize> {
        self.id_of_key(file, name, 0)
    }

    /// The node id for `(file, name, ordinal)`. Same-line callables share an
    /// ordinal (see `ordinals_of`); of those, the first node is the answer.
    pub fn id_of_key(&self, file: &Path, name: &str, ordinal: u32) -> Option<usize> {
        self.by_key
            .get(&key_hash(file, name, ordinal))?
            .iter()
            .copied()
            .find(|&i| {
                self.ordinals[i] == ordinal
                    && self.nodes[i].name == name
                    && self.nodes[i].file == file
                    && !calls_only(&self.nodes[i].kind)
            })
    }

    pub fn callers_of(&self, id: usize) -> &[usize] {
        &self.nodes[id].callers
    }

    pub fn callees_of(&self, id: usize) -> &[usize] {
        &self.nodes[id].callees
    }

    /// Functions nothing else calls — entry points, public API, or dead code.
    /// Sorted by name for stable display.
    pub fn uncalled(&self) -> Vec<usize> {
        let mut v: Vec<usize> = (0..self.nodes.len())
            .filter(|&i| self.nodes[i].callers.is_empty() && !calls_only(&self.nodes[i].kind))
            .collect();
        v.sort_by_key(|&i| self.sort_key(i));
        v
    }

    /// The most-called functions (hubs), most callers first, capped at `limit`.
    /// Restricted to functions with a project-unique name: a call to a name
    /// shared by many definitions (`new`, `len`) links to all of them, so their
    /// caller counts are inflated and meaningless — filtering to unique names
    /// keeps the list a real measure of a specific function's importance.
    pub fn most_called(&self, limit: usize) -> Vec<usize> {
        let mut name_counts: HashMap<&str, usize> = HashMap::new();
        for n in &self.nodes {
            *name_counts.entry(n.name.as_str()).or_default() += 1;
        }
        let mut v: Vec<usize> = (0..self.nodes.len())
            .filter(|&i| {
                !self.nodes[i].callers.is_empty()
                    && name_counts.get(self.nodes[i].name.as_str()) == Some(&1)
            })
            .collect();
        v.sort_by_key(|&i| {
            (
                std::cmp::Reverse(self.nodes[i].callers.len()),
                self.sort_key(i),
            )
        });
        v.truncate(limit);
        v
    }

    /// Display order: by name, then line. Borrowed, so sorting allocates
    /// nothing per comparison.
    fn sort_key(&self, id: usize) -> (&str, usize) {
        (self.nodes[id].name.as_str(), self.nodes[id].line)
    }

    /// How execution reaches `target` from the project's entry points: for
    /// each entry (a node `entry_class` ranks — `Some(0)` for a main, route,
    /// command or handler, `Some(1)` for a test, `None` for the rest) that
    /// reaches it through at most `max_depth` calls, the shortest chain of
    /// calls from that entry down to `target`, `target` last. Chains are
    /// ordered by class, then length, then the entry's name and line, and
    /// at most `limit` are returned. Empty when `target` is itself an entry
    /// (the reader is already there), or nothing ranked reaches it — a
    /// library's API, a function called through an interface or a callback
    /// the graph does not see.
    ///
    /// A breadth-first walk over callers, so each chain is a shortest one;
    /// recursion and cycles end at the visited set.
    pub fn paths_from_entries(
        &self,
        target: usize,
        entry_class: impl Fn(&SymNode) -> Option<u8>,
        limit: usize,
        max_depth: usize,
    ) -> Vec<Vec<usize>> {
        if target >= self.nodes.len() || limit == 0 || entry_class(&self.nodes[target]).is_some() {
            return Vec::new();
        }
        let mut parent: HashMap<usize, usize> = HashMap::new();
        let mut depth: HashMap<usize, usize> = HashMap::new();
        let mut queue = std::collections::VecDeque::new();
        let mut found: Vec<(u8, usize, usize)> = Vec::new(); // (class, depth, entry)
        depth.insert(target, 0);
        queue.push_back(target);
        while let Some(id) = queue.pop_front() {
            let d = depth[&id];
            if d >= max_depth {
                continue;
            }
            for &caller in &self.nodes[id].callers {
                if depth.contains_key(&caller) {
                    continue;
                }
                depth.insert(caller, d + 1);
                parent.insert(caller, id);
                if let Some(class) = entry_class(&self.nodes[caller]) {
                    found.push((class, d + 1, caller));
                    // An entry is where a chain starts; what calls an entry
                    // (a test of the main, say) is a chain of its own.
                }
                queue.push_back(caller);
            }
        }
        found.sort_by_key(|&(class, d, entry)| (class, d, self.sort_key(entry)));
        found
            .into_iter()
            .take(limit)
            .map(|(_, _, entry)| {
                let mut chain = vec![entry];
                let mut at = entry;
                while let Some(&next) = parent.get(&at) {
                    chain.push(next);
                    at = next;
                }
                chain
            })
            .collect()
    }
}

impl SymNode {
    pub fn caller_count(&self) -> usize {
        self.callers.len()
    }

    pub fn callee_count(&self) -> usize {
        self.callees.len()
    }
}

/// Per-node facts the resolver needs beyond the node itself.
struct NodeFacts {
    /// A declaration without a body (see [`FileCalls::declarations`]).
    bodyless: bool,
    /// A C or C++ function of its file alone (see [`FileCalls::internal`]).
    internal: bool,
    /// The language of the node's file, by extension.
    lang: Option<Lang>,
    /// An Erlang or Elixir function's arities ([`FileContext::arities`]).
    arity: Option<(u8, u8)>,
}

impl NodeFacts {
    /// Whether a call passing `passed` arguments can be of this function:
    /// one its parameter list takes; of a function whose list is unknown,
    /// yes. (Of a Scala `def` of no list, any call: see [`Self::takes_call`].)
    fn takes(&self, passed: Option<u8>) -> bool {
        let (Some(n), Some((fewest, most))) = (passed, self.arity) else {
            return true;
        };
        if self.lang == Some(Lang::Scala) {
            return match n {
                // A read with no parentheses: a `def` of no list (or of
                // implicits alone), or an override of a Java method's
                // (`toString`) — never a plain `def f()`, which Scala 3 calls
                // only as `f()`.
                NO_LIST => fewest == NO_LIST || most == NO_LIST,
                // A read passed as a function: any but a plain `def f()`.
                ETA => (fewest, most) != (0, 0),
                _ if fewest == NO_LIST => true,
                _ if most == NO_LIST => n == fewest,
                _ => (fewest..=most).contains(&n),
            };
        }
        // A Scala read of a Java or Kotlin method (`p.getName`).
        n >= ETA || (fewest..=most).contains(&n)
    }

    /// [`Self::takes`] for the call `cs` in a file of `lang`, of this
    /// function of the class `owner`. A Scala `def` of no list called with
    /// parentheses is its value applied (Mill's `scalaVersion()`, `items(0)`)
    /// — named through its own class alone: bare, on `this` or `super`, or
    /// through the class's name, as `head.get()` on another value is a
    /// library's method far more often; or its implicits passed explicitly
    /// (`Async.asyncForWriterT(async, L0)`). From Java or Kotlin, it is the
    /// JVM method Scala compiles it to: its implicits its parameters.
    fn takes_call(&self, cs: &CallSite, lang: Lang, owner: Option<&str>) -> bool {
        // Zig's decl literal passes every parameter (none by a dot).
        if lang == Lang::Zig
            && cs.qualifier.as_deref() == Some(".")
            && let (Some(n), Some((_, most))) = (cs.arity, self.arity)
        {
            return n == most;
        }
        if !self.takes(cs.arity) {
            return false;
        }
        let (Some(n), Some((NO_LIST, most))) = (cs.arity, self.arity) else {
            return true;
        };
        if self.lang != Some(Lang::Scala) || n >= ETA {
            return true;
        }
        let implicits = (most != NO_LIST).then_some(most);
        if lang != Lang::Scala {
            return n == implicits.unwrap_or(0);
        }
        let last = |s: &str| s.rsplit(['.', ':']).next().unwrap_or(s).to_string();
        let named_owner = owner.is_some_and(|o| {
            let o = last(o);
            cs.qualifier.as_deref().is_some_and(|q| last(q) == o)
                || matches!(&cs.receiver, Some(Receiver::Type(t)) if last(t) == o)
        });
        let own = !cs.method
            || matches!(&cs.receiver, Some(Receiver::Own))
            || matches!(&cs.receiver, Some(Receiver::Declared(t)) if t == SUPER)
            || named_owner;
        own && implicits.is_none_or(|k| n == k)
    }
}

/// The project's definitions, by name (see `ProjectCallGraph::build`).
#[derive(Default)]
struct Defs<'a> {
    /// By logical name ([`logical_name`]).
    named: HashMap<&'a str, Named>,
    /// By file and logical name.
    in_file: HashMap<(&'a Path, &'a str), Vec<usize>>,
    /// By file and name as written, in line order: a call's enclosing
    /// function ([`resolve_caller`]).
    callers: HashMap<(&'a Path, &'a str), Vec<usize>>,
}

/// The definitions of one name: all of them, and those with a body.
#[derive(Default)]
struct Named {
    all: Vec<usize>,
    defined: Vec<usize>,
}

/// Where a call was made.
struct Site<'a> {
    file: &'a Path,
    lang: Lang,
    imported: &'a HashSet<PathBuf>,
    /// The other files of the call's package (namespace), whose names it
    /// reaches with no import ([`FileContext::package`]); a C# file's
    /// enclosing namespaces' too.
    siblings: &'a [&'a HashSet<PathBuf>],
}

impl Site<'_> {
    /// Whether a definition in `file` is in the call's scope.
    fn sees(&self, file: &Path) -> bool {
        self.imported.contains(file) || self.siblings.iter().any(|s| s.contains(file))
    }

    /// How many files are in scope (counting a file twice where it is both
    /// imported and a sibling).
    fn scope_len(&self) -> usize {
        self.imported.len() + self.siblings.iter().map(|s| s.len()).sum::<usize>()
    }

    /// The files in scope.
    fn scope(&self) -> impl Iterator<Item = &PathBuf> {
        self.imported
            .iter()
            .chain(self.siblings.iter().flat_map(|s| s.iter()))
    }
}

/// Per-language node kinds for the call-site walk.
struct LangSpec {
    /// Nodes that are function scopes.
    fn_kinds: &'static [&'static str],
    /// Nodes that are call sites.
    call_kinds: &'static [&'static str],
    /// Subtrees holding no calls of their own, skipped whole (a Rust
    /// `macro_rules!` body or attribute is token soup, not code).
    skip_kinds: &'static [&'static str],
    /// Rust: read `name(…)` call shapes out of macro token trees, whose
    /// arguments tree-sitter does not parse as expressions — without this
    /// `assert_eq!(parse(x), …)` and `vec![build()]` hid their calls.
    macro_calls: bool,
    /// Closures: no callers of their own (their calls are the function's
    /// around them), but where the names a call's receiver may be are bound
    /// — a test's `var request = Request(…)` in `test('b', () { … })` is no
    /// sibling test's `request`; a name it does not bind, the function's
    /// around it.
    scope_kinds: &'static [&'static str],
}

/// Whether a name's type in `lang` is fixed where it is declared (or
/// inferred), whatever is later assigned to it.
fn statically_typed(lang: Lang) -> bool {
    matches!(
        lang,
        Lang::Dart
            | Lang::Kotlin
            | Lang::Java
            | Lang::Scala
            | Lang::CSharp
            | Lang::Go
            | Lang::C
            | Lang::Cpp
            | Lang::Rust
            | Lang::Zig
            | Lang::TypeScript
            | Lang::Tsx
    )
}

/// The field `name` of the class a C++ member function defined apart from
/// its class belongs to (`Class::method`), as a chain of fields from the
/// class ([`THROUGH`]) — what its declaration in another file types.
fn cpp_member_field(call: Node, name: &str, src: &str, lang: Lang) -> Option<Receiver> {
    if lang != Lang::Cpp || !is_plain_name(name) {
        return None;
    }
    let mut node = call.parent();
    while let Some(n) = node {
        if n.kind() == "function_definition" {
            break;
        }
        node = n.parent();
    }
    let mut declarator = node?.child_by_field_name("declarator")?;
    while declarator.kind() != "function_declarator" {
        declarator = declarator.child_by_field_name("declarator")?;
    }
    let qualified = declarator.child_by_field_name("declarator")?;
    if qualified.kind() != "qualified_identifier" {
        return None;
    }
    let scope = node_text(qualified.child_by_field_name("scope")?, src);
    let class = scope.split('<').next()?.rsplit("::").next()?.trim();
    // `logger_->info(…)`: what the field points to.
    let arrow = call
        .child_by_field_name("function")
        .and_then(|f| f.child_by_field_name("operator"))
        .is_some_and(|o| node_text(o, src) == "->");
    let field = if arrow {
        format!("{name}->")
    } else {
        name.to_string()
    };
    is_plain_name(class).then(|| Receiver::Type(format!("{THROUGH}{class}|{field}")))
}

/// Each C++ class's, struct's and enum's name, and what a value converts
/// to it by ([`FileContext::converts`]): the kind of the first parameter of
/// each constructor it declares that is no `explicit` or deleted one and
/// takes one argument (any others defaulted), its own class's (a copy's or
/// move's) aside — `K` for a template's whose constraint names a member of
/// its type (`FMT_ENABLE_IF(sizeof(Locale::collate) != 0)`), which no
/// literal has. An enum, or a class with no such constructor, converts
/// nothing; a class that inherits its base's (`using Base::Base;`) is left
/// out, unread.
fn cpp_conversions(src: &str, symbols: &[Located]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for class in symbols
        .iter()
        .filter(|l| matches!(l.symbol.kind.as_str(), "class" | "struct" | "enum"))
    {
        let name = class.symbol.name.as_str();
        let Some(body) = src.get(class.declaration.0..class.declaration.1) else {
            continue;
        };
        if !body.contains('{') || name.is_empty() {
            continue;
        }
        if class.symbol.kind == "enum" {
            out.push((name.to_string(), String::new()));
            continue;
        }
        let inherits = body.match_indices("using ").any(|(at, _)| {
            let statement = body[at + "using ".len()..].split(';').next().unwrap_or("");
            let mut segments = statement
                .rsplit("::")
                .map(|s| s.split('<').next().unwrap_or(s).trim());
            matches!((segments.next(), segments.next()), (Some(a), Some(b)) if !a.is_empty() && a == b)
        });
        if inherits {
            continue;
        }
        let mut kinds: Vec<String> = Vec::new();
        for f in symbols.iter().filter(|f| {
            f.symbol.name == class.symbol.name
                && is_callable(&f.symbol.kind)
                && f.declaration.0 > class.declaration.0
                && f.declaration.1 <= class.declaration.1
        }) {
            let Some(text) = src.get(f.declaration.0..f.declaration.1) else {
                continue;
            };
            let Some(Parameters::At(open, close)) = parameter_list(text, name, Lang::Cpp) else {
                continue;
            };
            let before = &text[..open];
            let head = text[close + 1..].split(['{', ';']).next().unwrap_or("");
            let explicit = before
                .split(|c: char| !(c == '_' || c.is_alphanumeric()))
                .any(|w| w == "explicit")
                && !before.contains("explicit(false)");
            let deleted = head.contains("delete");
            let parts: Vec<&str> = split_top_level(&text[open + 1..close])
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .collect();
            if explicit || deleted || parts.is_empty() || parts[0] == "void" {
                continue;
            }
            let required = parts
                .iter()
                .filter(|p| !p.contains("...") && split_top_level_at(p, '=').len() == p.len())
                .count();
            if required > 1 {
                continue;
            }
            let first = parts[0];
            let kind = match parameter_type(first, Lang::Cpp) {
                Some(ty) if ty != "..." && !first.contains('[') && !first.contains("(&") => {
                    let base = ty
                        .trim_start_matches("const ")
                        .trim()
                        .split(['<', '&', '*', ' '])
                        .next()
                        .unwrap_or("");
                    let last = base.rsplit("::").next().unwrap_or(base);
                    if last == name || last == "initializer_list" {
                        continue;
                    }
                    if type_parameters(before, Lang::Cpp).iter().any(|g| g == base) {
                        let member = format!("{base}::");
                        if before.contains(&member) || head.contains(&member) {
                            "K".to_string()
                        } else {
                            "?".to_string()
                        }
                    } else {
                        type_kind(ty, Lang::Cpp)
                    }
                }
                _ => "?".to_string(),
            };
            kinds.push(kind);
        }
        kinds.sort_unstable();
        kinds.dedup();
        out.push((name.to_string(), kinds.join(",")));
    }
    // (An enum is no symbol of the outline's: `enum class emphasis : uint8_t
    // {`, read off the text.)
    let is_name = |c: char| c == '_' || c.is_alphanumeric();
    for (at, _) in src.match_indices("enum ") {
        if src[..at].ends_with(is_name) {
            continue;
        }
        let rest = &src[at + "enum ".len()..];
        let rest = rest
            .strip_prefix("class ")
            .or_else(|| rest.strip_prefix("struct "))
            .unwrap_or(rest)
            .trim_start();
        let end = rest.find(|c: char| !is_name(c)).unwrap_or(rest.len());
        let (name, after) = (&rest[..end], rest[end..].trim_start());
        let after = match after.strip_prefix(':') {
            Some(base) if !base.starts_with(':') => base
                .trim_start()
                .trim_start_matches(|c: char| is_name(c) || c == ':' || c == ' '),
            _ => after,
        };
        if !name.is_empty()
            && !name.starts_with(|c: char| c.is_ascii_digit())
            && after.starts_with('{')
        {
            out.push((name.to_string(), String::new()));
        }
    }
    out
}

/// A Scala file's classes' fields ([`FileContext::fields`]): a case
/// class's parameters, a class's `val` and `var` ones, and the `val`s and
/// `var`s of a class's or object's body — what `res.exitCode` reads, no
/// method called. The type is the last part of the one written, where one
/// is.
fn scala_class_fields(src: &str, symbols: &[Located]) -> Vec<(String, String, String)> {
    let classes: Vec<&Located> = symbols
        .iter()
        .filter(|l| l.symbol.kind == "class")
        .collect();
    let type_of = |written: &str| -> String {
        let t = written.split('=').next().unwrap_or("").trim();
        let t = t.split('[').next().unwrap_or(t).trim();
        t.rsplit('.').next().unwrap_or(t).to_string()
    };
    let mut out = Vec::new();
    for class in &classes {
        let name = class.symbol.name.as_str();
        let Some(text) = src.get(class.declaration.0..class.declaration.1) else {
            continue;
        };
        let case = text
            .split(name)
            .next()
            .is_some_and(|before| before.split_whitespace().any(|w| w == "case"));
        let Some(Parameters::At(open, close)) = parameter_list(text, name, Lang::Scala) else {
            continue;
        };
        for part in split_top_level(&text[open + 1..close]) {
            let mut part = part.trim();
            // Past annotations and modifiers (`@JsonProperty("x")`,
            // `private[os] val`, `override val`).
            loop {
                let before = part;
                if part.starts_with('@') {
                    let end = part.find(' ').unwrap_or(part.len());
                    part = part[end..].trim_start();
                }
                for word in [
                    "private",
                    "protected",
                    "override",
                    "final",
                    "implicit",
                    "lazy",
                ] {
                    if let Some(rest) = part.strip_prefix(word) {
                        let rest = match rest.strip_prefix('[') {
                            Some(scoped) => scoped.split_once(']').map_or("", |(_, r)| r),
                            None => rest,
                        };
                        if rest.starts_with(' ') {
                            part = rest.trim_start();
                        }
                    }
                }
                if part == before {
                    break;
                }
            }
            let (member, rest) = match part
                .strip_prefix("val ")
                .or_else(|| part.strip_prefix("var "))
            {
                Some(rest) => (true, rest),
                None => (case, part),
            };
            let Some((field, ty)) = rest.split_once(':') else {
                continue;
            };
            let field = field.trim();
            if member && is_plain_name(field) {
                out.push((name.to_string(), field.to_string(), type_of(ty)));
            }
        }
    }
    // A body's `val`s and `var`s, its innermost class's or object's.
    for l in symbols.iter().filter(|l| l.symbol.kind == "property") {
        let around = classes
            .iter()
            .filter(|c| c.declaration.0 < l.declaration.0 && l.declaration.1 <= c.declaration.1)
            .max_by_key(|c| c.declaration.0);
        if let Some(class) = around {
            let ty = src
                .get(l.declaration.0..l.declaration.1)
                .and_then(|t| t.split_once(':'))
                .map(|(_, t)| type_of(t.lines().next().unwrap_or("")))
                .unwrap_or_default();
            out.push((class.symbol.name.clone(), l.symbol.name.clone(), ty));
        }
    }
    out
}

/// A C++ file's classes' fields and their types ([`FileContext::fields`]):
/// `std::vector<std::unique_ptr<flag_formatter>> formatters_;` is
/// `formatters_` of type `std::vector`, `int a_, b_;` two; a pointer's, a
/// `std::shared_ptr<logger> logger_;`'s pointee is the field `logger_->`'s
/// (its `.` the smart pointer's own `reset`, `get`). What a member function
/// defined apart from its class calls a field's method on.
fn cpp_class_fields(src: &str, symbols: &[Located]) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    for l in symbols
        .iter()
        .filter(|l| matches!(l.symbol.kind.as_str(), "class" | "struct"))
    {
        let Some(text) = src.get(l.declaration.0..l.declaration.1) else {
            continue;
        };
        let Some(open) = text.find('{') else {
            continue;
        };
        let mut depth = 0i32;
        let mut statement = String::new();
        for line in text[open..].lines() {
            let code = line.split("//").next().unwrap_or("");
            let at_top = depth == 1;
            depth += code.matches('{').count() as i32 - code.matches('}').count() as i32;
            if !at_top || code.contains(['{', '}']) || code.trim_start().starts_with('#') {
                statement.clear();
                continue;
            }
            // (After an access label: `private:`, `public slots:`.)
            let mut code = code.trim();
            for label in ["public", "private", "protected"] {
                if let Some(rest) = code.strip_prefix(label)
                    && let Some((words, rest)) = rest.split_once(':')
                    && !rest.starts_with(':')
                    && words
                        .split_whitespace()
                        .all(|w| matches!(w, "slots" | "Q_SLOTS"))
                {
                    statement.clear();
                    code = rest.trim();
                }
            }
            statement.push(' ');
            statement.push_str(code);
            if !statement.trim_end().ends_with(';') {
                continue;
            }
            let decl = std::mem::take(&mut statement);
            let decl = decl.trim().trim_end_matches(';').trim();
            if decl.contains('(')
                || [
                    "using ",
                    "typedef ",
                    "friend ",
                    "static_assert",
                    "enum ",
                    "template",
                    "return ",
                ]
                .iter()
                .any(|w| decl.starts_with(w))
            {
                continue;
            }
            let decl = decl.split(['=', '{']).next().unwrap_or(decl).trim();
            // The type and the first name, then the others (`int a_, b_`):
            // the first split at the last space outside the type's angle
            // brackets.
            let mut angle = 0i32;
            let mut comma = decl.len();
            for (i, c) in decl.char_indices() {
                match c {
                    '<' => angle += 1,
                    '>' => angle -= 1,
                    ',' if angle == 0 => {
                        comma = i;
                        break;
                    }
                    _ => {}
                }
            }
            let (first, others) = (&decl[..comma], decl.get(comma + 1..).unwrap_or(""));
            let mut angle = 0i32;
            let mut split = None;
            for (i, c) in first.char_indices() {
                match c {
                    '<' => angle += 1,
                    '>' => angle -= 1,
                    ' ' | '*' | '&' if angle == 0 => split = Some(i),
                    _ => {}
                }
            }
            let Some(split) = split else {
                continue;
            };
            let ty = first[..=split].trim();
            let names = format!("{},{others}", &first[split + 1..]);
            let names = names.as_str();
            let ty = ty
                .split_whitespace()
                .filter(|w| {
                    !matches!(
                        *w,
                        "static" | "mutable" | "const" | "constexpr" | "inline" | "volatile"
                    )
                })
                .collect::<Vec<_>>()
                .join(" ");
            let pointer = ty.trim_end().ends_with('*');
            let ty = ty.trim_end_matches(['*', '&', ' ']);
            let (base, args) = match ty.split_once('<') {
                Some((base, args)) => (base.trim(), Some(args.trim_end_matches('>').trim())),
                None => (ty.trim(), None),
            };
            // What `->` reaches: a pointer's, a smart pointer's pointee (`.`
            // its own methods, `reset`, `lock`).
            let pointee = match (base, args) {
                ("std::shared_ptr" | "std::unique_ptr", Some(pointee))
                    if !pointee.contains(['<', ',']) =>
                {
                    Some(pointee.trim_start_matches("const ").trim())
                }
                _ => pointer.then_some(base),
            };
            let plain = |t: &str| {
                !t.is_empty()
                    && t.chars()
                        .all(|c| c == '_' || c == ':' || c.is_alphanumeric())
            };
            if !plain(base) {
                continue;
            }
            for name in names.split(',') {
                let name = name.trim().trim_start_matches(['*', '&']).trim();
                let name = name.split('[').next().unwrap_or(name).trim();
                if is_plain_name(name) {
                    out.push((l.symbol.name.clone(), name.to_string(), base.to_string()));
                    if let Some(pointee) = pointee.filter(|p| plain(p)) {
                        out.push((
                            l.symbol.name.clone(),
                            format!("{name}->"),
                            pointee.to_string(),
                        ));
                    }
                }
            }
        }
    }
    out
}

/// A Ruby constant's file name: `DependencyParser` is `dependency_parser`,
/// `HTTPClient` `http_client`.
fn snake_case(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    let mut out = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if c.is_uppercase() {
            let after_lower =
                i > 0 && (chars[i - 1].is_lowercase() || chars[i - 1].is_ascii_digit());
            let before_lower = i > 0
                && chars.get(i + 1).is_some_and(|n| n.is_lowercase())
                && chars[i - 1].is_uppercase();
            if after_lower || before_lower {
                out.push('_');
            }
            out.extend(c.to_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// The URI a Dart file imports under the prefix `name` (`import
/// 'package:http/http.dart' as http;`), if any.
fn dart_import_prefix(src: &str, name: &str) -> Option<String> {
    if !is_plain_name(name) {
        return None;
    }
    src.lines().find_map(|line| {
        let rest = line.trim_start().strip_prefix("import ")?.trim_start();
        let quote = rest.chars().next().filter(|c| matches!(c, '\'' | '"'))?;
        let (uri, after) = rest[1..].split_once(quote)?;
        let after = after.trim_start();
        let after = after
            .strip_prefix("deferred ")
            .unwrap_or(after)
            .trim_start();
        let prefix: String = after
            .strip_prefix("as ")?
            .trim_start()
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        (prefix == name).then(|| uri.to_string())
    })
}

/// Where the library a Dart file imports by `uri` lies in the project: the
/// `lib` folder of the package it names (`package:shelf/shelf.dart` is
/// `…/shelf/lib/`), or the file a relative URI names — `None` for a
/// library outside the project (`dart:math`, `package:http/…`).
fn dart_library_root(uri: &str, from: &Path, dart_files: &[&Path]) -> Option<PathBuf> {
    if uri.starts_with("dart:") {
        return None;
    }
    if let Some(rest) = uri.strip_prefix("package:") {
        let (package, path) = rest.split_once('/')?;
        let named = format!("/{package}/lib/{path}");
        let any = format!("/lib/{path}");
        // (A library of exports alone holds no definition this reads: the
        // package's `lib` folder has the files it exports.)
        let lib = format!("/{package}/lib/");
        if let Some(found) = dart_files
            .iter()
            .find(|f| f.to_string_lossy().ends_with(&named))
            .or_else(|| {
                dart_files
                    .iter()
                    .find(|f| f.to_string_lossy().ends_with(&any))
            })
        {
            let text = found.to_string_lossy();
            let at = text.rfind(&any)?;
            return Some(PathBuf::from(&text[..at + "/lib/".len()]));
        }
        let under = dart_files.iter().find_map(|f| {
            let text = f.to_string_lossy();
            text.find(&lib)
                .map(|at| PathBuf::from(&text[..at + lib.len()]))
        });
        return under;
    }
    let file = lexical_join(from.parent()?, uri);
    dart_files.contains(&file.as_path()).then_some(file)
}

/// A C++ block at a file's or namespace's top level: a test macro's body
/// (`TEST_CASE("adds") { … }`, which the grammar reads as a call, then a
/// block) — its names its own, as a function's are.
const TOP_BLOCK: &str = "compound_statement@top";

/// Whether `node` is of one of `kinds` — a [`TOP_BLOCK`] too.
fn is_place(node: Node, kinds: &[&str]) -> bool {
    let kind = node.kind();
    kinds.contains(&kind)
        || (kind == "compound_statement"
            && kinds.contains(&TOP_BLOCK)
            && node
                .parent()
                .is_some_and(|p| matches!(p.kind(), "translation_unit" | "declaration_list")))
}

/// Each language's closures (see [`LangSpec::scope_kinds`]).
fn closure_kinds(lang: Lang) -> &'static [&'static str] {
    match lang {
        Lang::Dart => &["function_expression_body"],
        Lang::Kotlin => &["lambda_literal"],
        Lang::Java | Lang::Scala => &["lambda_expression"],
        Lang::Cpp => &["lambda_expression", TOP_BLOCK],
        Lang::CSharp => &["lambda_expression", "anonymous_method_expression"],
        Lang::Go => &["func_literal"],
        Lang::Ruby => &["do_block", "block"],
        // A Zig `test "…" { … }` block: no function, its names its own.
        Lang::Zig => &["test_declaration"],
        _ => &[],
    }
}

fn lang_spec(lang: Lang) -> Option<LangSpec> {
    let spec = |fn_kinds, call_kinds| LangSpec {
        fn_kinds,
        call_kinds,
        skip_kinds: &[],
        macro_calls: false,
        scope_kinds: closure_kinds(lang),
    };
    Some(match lang {
        Lang::Rust => LangSpec {
            fn_kinds: &["function_item"],
            call_kinds: &["call_expression"],
            skip_kinds: &["macro_definition", "attribute_item", "inner_attribute_item"],
            macro_calls: true,
            scope_kinds: &[],
        },
        Lang::Python => spec(&["function_definition"], &["call"]),
        Lang::JavaScript | Lang::TypeScript | Lang::Tsx => spec(
            &[
                "function_declaration",
                "generator_function_declaration",
                "method_definition",
                "function_expression",
                // Arrow functions (`const x = () => …`, class fields, object
                // entries, callbacks). Each is a scope only when its inferred
                // name is one of the file's callables (see `calls_in`);
                // otherwise — an anonymous callback, a binding the outline
                // does not list — its calls belong to the enclosing function.
                "arrow_function",
            ],
            // A component rendered (`<Button …/>`) is a call of it, as
            // React calls it.
            &[
                "call_expression",
                "new_expression",
                "jsx_opening_element",
                "jsx_self_closing_element",
            ],
        ),
        Lang::Go => spec(
            &["function_declaration", "method_declaration"],
            &["call_expression"],
        ),
        // Dart has no node wrapping a signature and its body — they are flat
        // siblings (`method_signature`/`function_signature` then `function_body`).
        // The body holds the calls, so it is the enclosing scope; its name is
        // recovered from the preceding signature (see `fn_name`).
        Lang::Dart => spec(
            &["function_body"],
            &["method_invocation", "constructor_invocation"],
        ),
        Lang::C => spec(&["function_definition"], &["call_expression"]),
        Lang::Cpp => spec(
            &["function_definition"],
            &["call_expression", "new_expression"],
        ),
        Lang::Java => spec(
            &["method_declaration", "constructor_declaration"],
            &["method_invocation", "object_creation_expression"],
        ),
        // A closure (`function () use ($x) {…}`, `fn($x) => …`) is no scope:
        // the outline does not list it, so its calls are the method's.
        Lang::Php => spec(
            &["function_definition", "method_declaration"],
            &[
                "function_call_expression",
                "member_call_expression",
                "nullsafe_member_call_expression",
                "scoped_call_expression",
                "object_creation_expression",
            ],
        ),
        // Only `call` nodes: a bare identifier statement (`setup`) is a call
        // to a method or a read of a local variable, and only the method's
        // locals tell which.
        Lang::Ruby => spec(&["method", "singleton_method"], &["call"]),
        Lang::CSharp => spec(
            &[
                "method_declaration",
                "constructor_declaration",
                "local_function_statement",
            ],
            &["invocation_expression", "object_creation_expression"],
        ),
        // Each clause of a function is a `function_clause` naming it (the
        // outline merges a function's clauses into one item); the clauses of
        // an anonymous `fun` are `fun_clause`s, so its calls are the
        // enclosing function's.
        // `fun add/2` and `fun cart:add/2` name a function to be called
        // (`lists:map(fun cart:add/2, …)`): a use of it, as a call is.
        Lang::Erlang => spec(
            &["function_clause"],
            &["call", "internal_fun", "external_fun"],
        ),
        // A `bind` is a scope as well as a `function`: `main = do …` and
        // `f = \x -> …` are binds, and the outline lists every top-level one
        // as a function — without it `main`'s calls had no caller. A bind
        // in a `do` block (`x <- act`) has a pattern, not a name, so it is
        // none.
        // A function passed as a value (`foldr add 0 xs`, `show . add 1`)
        // is used where it is passed, as the function it is passed to calls
        // it: higher-order and point-free code use most functions so.
        Lang::Haskell => spec(
            &["function", "bind"],
            &["apply", "variable", "qualified", "infix", "prefix_id"],
        ),
        Lang::Zig => spec(&["function_declaration"], &["call_expression"]),
        // `Foo(…)` constructs a class as a function is called; `a.b(…)`
        // is a `navigation_expression` callee.
        Lang::Kotlin => spec(
            &["function_declaration"],
            &["call_expression", "infix_expression"],
        ),
        // `new Foo(…)` is an `instance_expression`; `Foo(…)` (a case class,
        // an `apply`) a call.
        Lang::Scala => spec(
            &["function_definition"],
            &["call_expression", "instance_expression", "field_expression"],
        ),
        // A `def` is a call to the grammar too: a `call` is a function's
        // scope when it defines one (see `fn_names`), and a call otherwise.
        // `&add/2` captures a function to be called (`&Cart.add/2`'s
        // `Cart.add` is a call node of its own).
        Lang::Elixir => spec(&["call"], &["call", "unary_operator"]),
        // A schema has no calls; a Lean proof names the lemmas it uses inside
        // tactics (`rw [h]`, `simp [f]`) far more often than it applies them,
        // so a call graph of applications alone would say a proof uses
        // nothing.
        Lang::Json
        | Lang::Bash
        | Lang::Yaml
        | Lang::Toml
        | Lang::Html
        | Lang::Css
        | Lang::Scss
        | Lang::Less
        | Lang::Vue
        | Lang::Svelte
        | Lang::Astro
        | Lang::Sql => {
            return None;
        }
        // Read from the text (see `crate::lean::calls`): no tree's kinds.
        Lang::Lean => spec(&[], &[]),
    })
}

/// Extract every call site from one file's `source`.
pub fn calls_of(source: &str, lang: &str) -> Vec<CallSite> {
    let Some(lang) = Lang::for_source(lang, source) else {
        return Vec::new();
    };
    if lang_spec(lang).is_none() {
        return Vec::new();
    }
    let Some(tree) = crate::highlight::parse(source, lang) else {
        return Vec::new();
    };
    let symbols = crate::outline::located_in(&tree, source, lang);
    calls_in(&tree, source, lang, &symbols)
}

/// The call sites of an already-parsed file, attributed with its outline
/// `symbols` (from the same tree).
///
/// A call's caller is its innermost enclosing function THAT IS ONE OF THE
/// FILE'S CALLABLES — the nodes the graph has. An anonymous function is named
/// after the binding it is assigned to (`const handler = () => …`,
/// `a.b = function () {}`, `{ onClick: () => … }`), and when that binding is
/// not a callable the outline lists, the function is transparent: its calls
/// belong to the named function around it. Attributing them to the unlisted
/// name dropped them instead, since no node carries it. For a language
/// without an outline every function name is accepted.
pub fn calls_in(tree: &Tree, source: &str, lang: Lang, symbols: &[Located]) -> Vec<CallSite> {
    if lang == Lang::Lean {
        return crate::lean::calls(source, symbols);
    }
    let Some(spec) = lang_spec(lang) else {
        return Vec::new();
    };
    let callable: Option<HashSet<&str>> = crate::highlight::tags_query(lang).is_ok().then(|| {
        symbols
            .iter()
            .filter(|l| is_callable(&l.symbol.kind))
            .map(|l| l.symbol.name.as_str())
            .collect()
    });
    let mut out = Vec::new();
    walk(
        tree.root_node(),
        source,
        lang,
        &spec,
        callable.as_ref(),
        &mut out,
    );
    in_test_blocks(&mut out, symbols);
    if lang == Lang::Zig {
        zig_aliases(&mut out, source);
    }
    if lang == Lang::Haskell {
        haskell_locals(&mut out, tree.root_node(), source);
    }
    out
}

/// Haskell calls of a name the definition around them binds itself — a
/// `where` or `let` function (`go`), a parameter or a pattern's variable
/// (`f x` with `f` passed in, `\f -> f x`, `f <- ask`) — go: that binding
/// is what they call, and it hides any function of the name elsewhere.
fn haskell_locals(calls: &mut Vec<CallSite>, root: Node, src: &str) {
    // Each top-level (or class or instance) equation's lines, and the names
    // bound inside it.
    let mut scopes: Vec<(usize, usize, HashSet<String>)> = Vec::new();
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        let top = matches!(n.kind(), "function" | "bind")
            && n.parent().is_some_and(|p| {
                matches!(
                    p.kind(),
                    "declarations" | "class_declarations" | "instance_declarations"
                )
            });
        if !top {
            stack.extend(n.named_children(&mut n.walk()));
            continue;
        }
        let mut names = HashSet::new();
        let mut inner = vec![n];
        while let Some(x) = inner.pop() {
            if x.id() != n.id()
                && matches!(x.kind(), "function" | "bind")
                && let Some(name) = x.child_by_field_name("name")
            {
                let name = match name.kind() {
                    "prefix_id" => name.named_child(0).unwrap_or(name),
                    _ => name,
                };
                names.insert(node_text(name, src).to_string());
            }
            // What a pattern binds: its variables (`go acc (x:xs)`,
            // `p@(a :* b)`); an infix head's operands (`a <+> b = …`).
            let mut cursor = x.walk();
            for (i, child) in x.children(&mut cursor).enumerate() {
                let field = x.field_name_for_child(i as u32);
                let binds = matches!(field, Some("patterns" | "pattern" | "bind"))
                    || (child.kind() == "infix"
                        && x.kind() == "function"
                        && x.child_by_field_name("name").is_none());
                if binds {
                    haskell_pattern_variables(child, src, &mut names);
                }
                if child.is_named() {
                    inner.push(child);
                }
            }
        }
        if !names.is_empty() {
            scopes.push((n.start_position().row + 1, n.end_position().row + 1, names));
        }
    }
    if scopes.is_empty() {
        return;
    }
    calls.retain(|c| {
        c.method
            || c.qualifier.is_some()
            || !scopes
                .iter()
                .any(|(a, b, names)| *a <= c.line && c.line <= *b && names.contains(&c.callee))
    });
}

/// The variables a Haskell pattern binds: `x` and `xs` of `(x:xs)`, `p` of
/// `p@(…)`, a punned field's (`Foo{bar}`) — not a view pattern's function
/// (`(skipSpace -> bs)` binds `bs`), a signature's type, a field's name
/// given a pattern of its own, or a splice's.
fn haskell_pattern_variables(pattern: Node, src: &str, names: &mut HashSet<String>) {
    let mut stack = vec![pattern];
    while let Some(v) = stack.pop() {
        match v.kind() {
            "variable" => {
                names.insert(node_text(v, src).to_string());
            }
            "view_pattern" | "signature" => stack.extend(v.child_by_field_name("pattern")),
            "field_pattern" => match v.child_by_field_name("pattern") {
                Some(p) => stack.push(p),
                None => stack.extend(v.child_by_field_name("field")),
            },
            "splice" | "quasiquote" | "qualified" | "operator" | "literal" => {}
            _ => stack.extend(v.named_children(&mut v.walk())),
        }
    }
}

/// A Zig file's names for what it imports, read off its top-level `const`s:
/// - the standard library's declarations (`const Ast = std.zig.Ast;`,
///   `const Allocator = std.mem.Allocator;`) — a receiver of such a type is
///   the library's (`tree.render(…)` with `tree: Ast` calls
///   `std.zig.Ast.render`, no project function), named by its path in full;
/// - another file (`const posix = @import("posix.zig");`) — `posix.read(…)`
///   is that file's own `read`, the call's qualifier its path;
/// - a type of another file (`const BufferPool = @import("buffer.zig").Pool;`)
///   — `BufferPool.init(…)` is `Pool`'s.
fn zig_aliases(calls: &mut [CallSite], source: &str) {
    let mut std_aliases: HashMap<&str, &str> = HashMap::new();
    let mut files: HashMap<&str, &str> = HashMap::new();
    let mut types: HashMap<&str, &str> = HashMap::new();
    for line in source.lines() {
        let line = line.trim_start();
        let line = line.strip_prefix("pub ").unwrap_or(line);
        let Some((name, value)) = line.strip_prefix("const ").and_then(|l| l.split_once('='))
        else {
            continue;
        };
        let name = name.trim();
        let Some(value) = value.trim().strip_suffix(';').map(str::trim_end) else {
            continue;
        };
        if name == "std" || !is_plain_name(name) {
            continue;
        }
        let path_like = |v: &str| {
            v.chars()
                .all(|c| c == '_' || c == '.' || c.is_alphanumeric())
        };
        if value == "std" || (value.starts_with("std.") && path_like(value)) {
            std_aliases.insert(name, value);
        } else if let Some(rest) = value.strip_prefix("@import(\"")
            && let Some((path, member)) = rest.split_once("\")")
            && path.ends_with(".zig")
        {
            match member.strip_prefix('.') {
                None if member.is_empty() => {
                    files.insert(name, path);
                }
                Some(member) if path_like(member) => {
                    types.insert(name, member.rsplit('.').next().unwrap_or(member));
                }
                _ => {}
            }
        }
    }
    if std_aliases.is_empty() && files.is_empty() && types.is_empty() {
        return;
    }
    for call in calls {
        if let Some(Receiver::Type(ty) | Receiver::Declared(ty)) = &mut call.receiver {
            let (first, rest) = ty.split_once('.').unwrap_or((ty.as_str(), ""));
            if let Some(path) = std_aliases.get(first) {
                *ty = if rest.is_empty() {
                    path.to_string()
                } else {
                    format!("{path}.{rest}")
                };
            } else if let Some(member) = types.get(first).filter(|_| rest.is_empty()) {
                *ty = member.to_string();
            }
        }
        if call.receiver.is_none()
            && let Some(q) = call.qualifier.as_deref()
        {
            if let Some(path) = files.get(q) {
                call.qualifier = Some(path.to_string());
            } else if let Some(path) = std_aliases.get(q) {
                // `mem.eql(…)` with `const mem = std.mem;`: the library's.
                call.receiver = Some(Receiver::Type(path.to_string()));
                call.qualifier = None;
            }
        }
    }
}

/// Each call made inside a test or a group of tests (`it "adds" do … end`,
/// `test "adds" { … }`, `describe`) made from it: from the innermost one
/// around it — unless a function defined inside that test holds it. A test
/// block is no function to the grammar, so its calls had no caller (or, in
/// a Dart or hspec file, the `main` or `spec` its tests sit in).
fn in_test_blocks(calls: &mut [CallSite], symbols: &[Located]) {
    let span = |l: &Located| (l.symbol.line, l.symbol.end_line.max(l.symbol.line));
    let blocks: Vec<(usize, usize, &str)> = symbols
        .iter()
        .filter(|l| calls_only(&l.symbol.kind))
        .map(|l| {
            let (first, last) = span(l);
            (first, last, l.symbol.name.as_str())
        })
        .collect();
    if blocks.is_empty() {
        return;
    }
    let innermost = |spans: &mut dyn Iterator<Item = (usize, usize)>, line: usize| {
        spans
            .filter(|&(first, last)| first <= line && line <= last)
            .min_by_key(|&(first, last)| (last - first, std::cmp::Reverse(first)))
    };
    for call in calls.iter_mut() {
        let Some(block) = blocks
            .iter()
            .filter(|&&(first, last, _)| first <= call.line && call.line <= last)
            .min_by_key(|&&(first, last, _)| (last - first, std::cmp::Reverse(first)))
        else {
            continue;
        };
        // The function the walk found around it, if the file defines it
        // there: a test inside it (Dart's `main`) is nearer; one it is
        // inside (a helper the test defines) is not.
        let function = call.caller.as_deref().and_then(|caller| {
            innermost(
                &mut symbols
                    .iter()
                    .filter(|l| is_callable(&l.symbol.kind) && l.symbol.name == caller)
                    .map(span),
                call.line,
            )
        });
        let in_function = function.is_some_and(|(first, last)| block.0 < first || last < block.1);
        if !in_function {
            call.caller = Some(block.2.to_string());
        }
    }
}

fn node_text<'a>(node: Node, src: &'a str) -> &'a str {
    src.get(node.byte_range()).unwrap_or("")
}

/// One node of [`walk`]: the node, its parent and the sibling before it
/// (carried, as tree-sitter finds a parent by walking down from the root, a
/// cost of the depth per call, and a sibling through its parent), and the
/// enclosing function in scope for it: its name, and the byte its
/// definition starts at (where a receiver's binding is looked for).
type WalkItem<'t> = (
    Node<'t>,
    Option<Node<'t>>,
    Option<Node<'t>>,
    Option<(Rc<str>, usize)>,
    // Where the innermost function around the node starts, named or not:
    // the scope a receiver's name is bound in (a closure's own parameter).
    Option<usize>,
);

/// Iterative depth-first walk carrying the nearest enclosing function name. It is
/// explicit-stack (not recursive) so a pathologically deep tree — e.g. a checked-
/// in minified bundle with 100k-deep nested expressions — can't overflow the
/// stack and abort the process.
fn walk(
    root: Node,
    src: &str,
    lang: Lang,
    spec: &LangSpec,
    callable: Option<&HashSet<&str>>,
    out: &mut Vec<CallSite>,
) {
    let mut stack: Vec<WalkItem> = vec![(root, None, None, None, None)];
    // What each function's names are bound to, read once as far as its
    // calls reach (`Bindings`): reading the function's text again for
    // every call made the walk quadratic in a long function.
    let mut bindings: HashMap<(usize, String), Bindings> = HashMap::new();
    // What each class's fields are declared as, by the class's start and
    // the field's name, read once.
    let mut fields: HashMap<(usize, String), Option<String>> = HashMap::new();
    // Each function's (or closure's) parameters' names, by its node.
    let mut parameters: HashMap<usize, Vec<String>> = HashMap::new();
    let place_kinds: Vec<&str> = spec
        .fn_kinds
        .iter()
        .chain(spec.scope_kinds)
        .copied()
        .collect();
    let places = Places::read(root, &place_kinds);
    while let Some((node, parent, before, enclosing, inner)) = stack.pop() {
        let kind = node.kind();
        if spec.skip_kinds.contains(&kind) {
            continue;
        }
        // A function definition becomes the enclosing scope for its subtree —
        // under the first of its names that the file's outline has.
        let own: Option<(Rc<str>, usize)> = if spec.fn_kinds.contains(&kind) {
            fn_names(node, parent, before, src, lang)
                .into_iter()
                .find(|name| callable.is_none_or(|c| c.contains(name.as_str())))
                .map(|s| (Rc::from(s.as_str()), node.start_byte()))
        } else {
            None
        };
        let current = own.or(enclosing);
        let inner = if spec.fn_kinds.contains(&kind) || is_place(node, spec.scope_kinds) {
            Some(node.start_byte())
        } else {
            inner
        };

        if spec.call_kinds.contains(&kind)
            && let Some((callee, method)) = callee_name(node, parent, src, lang)
            // A parameter called (`emit('done')` in `(event, emit) { … }`,
            // `cb()`) is the value passed, no function of the project's.
            && (method || !calls_a_parameter(node, &callee, src, lang, &mut parameters))
        {
            // The receiver's name where nothing binds it (Scala's `os`).
            let mut unbound: Option<String> = None;
            let receiver = match &current {
                // Scala's `new Column(_)`: the class's own constructor.
                _ if lang == Lang::Scala && kind == "instance_expression" => {
                    Some(Receiver::Type(callee.clone()))
                }
                // `Cart.add(…)`, `cart:add(…)`: the module named.
                _ if method && matches!(lang, Lang::Elixir | Lang::Erlang) => {
                    module_receiver(node, parent, src, lang)
                }
                Some((_, from)) if method => {
                    let scope = Scope {
                        places: &places,
                        from: inner.unwrap_or(*from),
                    };
                    receiver_of(
                        node,
                        &callee,
                        &scope,
                        src,
                        lang,
                        &mut bindings,
                        &mut fields,
                        &mut unbound,
                    )
                }
                // In no function: in a test block (`test("adds") { … }`),
                // whose calls are its own (`in_test_blocks`), its names
                // bound from the file's start.
                None if method && lang != Lang::Rust => {
                    let scope = Scope {
                        places: &places,
                        from: inner.unwrap_or(0),
                    };
                    receiver_of(
                        node,
                        &callee,
                        &scope,
                        src,
                        lang,
                        &mut bindings,
                        &mut fields,
                        &mut unbound,
                    )
                }
                _ if lang == Lang::Rust => rust_path_receiver(node, src),
                _ => None,
            };
            // Ruby's call on the class itself, or on an instance its own
            // class builds ([`ruby_made_on`]): `class` or `new`.
            let made_on = (lang == Lang::Ruby && method)
                .then(|| ruby_made_on(node, &callee, src))
                .flatten();
            let receiver = match made_on {
                Some((true, _)) => Some(Receiver::Own),
                _ => receiver,
            };
            let unbound = match made_on {
                Some((_, on)) if receiver.is_some() => {
                    // (Kept with the receiver, below.)
                    Some(on.to_string())
                }
                _ => unbound,
            };
            let ruby_on = made_on.is_some() && receiver.is_some();
            let qualifier = unbound
                .filter(|_| receiver.is_none() || ruby_on)
                .or_else(|| {
                    // C++'s `Cart::make()`: the class (or namespace) it names.
                    (lang == Lang::Cpp && !method)
                        .then(|| cpp_call_scope(node, src))
                        .flatten()
                });
            // Zig's decl literal (`.of(a, b)`, its type the one expected):
            // `.`, a function every argument of which it passes.
            let qualifier = qualifier.or_else(|| {
                (lang == Lang::Zig
                    && node.child_by_field_name("function").is_some_and(|f| {
                        f.kind() == "field_expression" && f.child_by_field_name("object").is_none()
                    }))
                .then(|| ".".to_string())
            });
            // A Kotlin infix call (`c eq 3`): `infix`, linked only where
            // what it is called on is of a type known (see the graph).
            let qualifier = qualifier.or_else(|| {
                (lang == Lang::Kotlin && kind == "infix_expression").then(|| "infix".to_string())
            });
            // A Ruby bare call in a block (`mock_app do set :views … end`):
            // `block`, as the block may run on another object entirely.
            let qualifier = qualifier.or_else(|| {
                (lang == Lang::Ruby && !method && ruby_in_block(node)).then(|| "block".to_string())
            });
            let arity = call_arity(node, parent, src, lang);
            let arguments = matches!(
                lang,
                Lang::Java | Lang::Kotlin | Lang::Scala | Lang::CSharp | Lang::Cpp
            )
            .then(|| {
                let scope = Scope {
                    places: &places,
                    from: inner
                        .or(current.as_ref().map(|(_, from)| *from))
                        .unwrap_or(0),
                };
                argument_kinds(node, &scope, src, lang, &mut bindings)
            })
            .flatten();
            out.push(CallSite {
                caller: current.as_ref().map(|(name, _)| name.to_string()),
                callee,
                method,
                receiver,
                qualifier,
                arity,
                arguments,
                line: node.start_position().row + 1,
            });
        }
        // Ruby calls a method with no arguments by its bare name, which
        // its grammar cannot tell from a variable: a name standing as a
        // statement that the method has not bound is a call.
        if lang == Lang::Ruby
            && kind == "identifier"
            && ruby_bare_call(node, parent, src, current.as_ref().map(|(_, from)| *from))
        {
            out.push(CallSite {
                caller: current.as_ref().map(|(name, _)| name.to_string()),
                callee: node_text(node, src).to_string(),
                method: false,
                receiver: None,
                qualifier: ruby_in_block(node).then(|| "block".to_string()),
                arity: None,
                arguments: None,
                line: node.start_position().row + 1,
            });
        }
        if spec.macro_calls && kind == "token_tree" {
            macro_calls(node, src, current.as_ref().map(|(name, _)| &**name), out);
        }

        let mut cursor = node.walk();
        let mut before = None;
        for child in node.children(&mut cursor) {
            stack.push((child, Some(node), before, current.clone(), inner));
            before = Some(child);
        }
    }
}

/// Whether the Ruby `identifier` `node` is a method called by its bare name:
/// a statement of its own (`greet`), a modifier's body (`log_it if ok`), a
/// block's — and not a name the method around it binds before it (a local
/// assigned, a parameter, a block's parameter), which is its value: a
/// method's last line `total` returns the local.
fn ruby_bare_call(node: Node, parent: Option<Node>, src: &str, from: Option<usize>) -> bool {
    let Some(parent) = parent else {
        return false;
    };
    let statement = match parent.kind() {
        "body_statement"
        | "block_body"
        | "then"
        | "else"
        | "begin"
        | "ensure"
        | "do"
        | "program"
        | "parenthesized_statements" => true,
        "if_modifier" | "unless_modifier" | "while_modifier" | "until_modifier"
        | "rescue_modifier" => parent
            .child_by_field_name("body")
            .is_some_and(|body| body.id() == node.id()),
        _ => false,
    };
    if !statement {
        return false;
    }
    let name = node_text(node, src);
    let Some(before) = src.get(from.unwrap_or(0)..node.start_byte()) else {
        return false;
    };
    let word = |c: char| c == '_' || c.is_alphanumeric();
    let mut at = 0;
    while let Some(i) = before[at..].find(name) {
        let start = at + i;
        let end = start + name.len();
        at = end;
        let head = &before[..start];
        if head.ends_with(|c: char| word(c) || matches!(c, '.' | '@' | '$' | ':'))
            || before[end..].starts_with(|c: char| word(c) || matches!(c, '?' | '!'))
        {
            continue;
        }
        let after = before[end..].trim_start_matches([' ', '\t']);
        let line_rest = &after[..after.find('\n').unwrap_or(after.len())];
        let is_assign = |text: &str| {
            text.starts_with('=')
                && !text.starts_with("==")
                && !text.starts_with("=~")
                && !text.starts_with("=>")
        };
        // `name = …` (not `==`, `=~`, `=>`), `name += …`, and one of
        // several assigned at once (`name, rest = …`).
        let assigned = is_assign(after)
            || ["+=", "-=", "*=", "/=", "||=", "&&=", "<<="]
                .iter()
                .any(|op| after.starts_with(op))
            || (after.starts_with(',')
                && line_rest.match_indices('=').any(|(i, _)| {
                    is_assign(&line_rest[i..]) && !line_rest[..i].ends_with(['=', '!', '<', '>'])
                }));
        let line_head = &head[head.rfind('\n').map_or(0, |n| n + 1)..];
        // A parameter: in the `def`'s list, or a block's `|…|`.
        let parameter = (line_head.trim_start().starts_with("def ")
            && (line_head.contains('(') || line_head.trim_end().ends_with(',')))
            || line_head.matches('|').count() % 2 == 1;
        if assigned || parameter {
            return false;
        }
    }
    true
}

/// The `name(…)` call shapes among one macro token tree's direct tokens: an
/// identifier immediately followed by a parenthesized token tree. `a.b(…)` is
/// a method call; `foo!(…)` (a nested macro: its `!` sits between the two) and
/// `Foo { … }` / `x[…]` are not calls. Keywords are not identifiers inside a
/// token tree, so `if (x)` never matches. Nested token trees are visited by
/// the walk itself.
fn macro_calls(tree: Node, src: &str, caller: Option<&str>, out: &mut Vec<CallSite>) {
    let mut cursor = tree.walk();
    let tokens: Vec<Node> = tree.children(&mut cursor).collect();
    for (i, token) in tokens.iter().enumerate() {
        if token.kind() != "identifier" {
            continue;
        }
        let Some(next) = tokens.get(i + 1) else {
            continue;
        };
        if next.kind() != "token_tree" || !node_text(*next, src).starts_with('(') {
            continue;
        }
        let prev = i.checked_sub(1).map(|j| tokens[j].kind());
        if prev == Some("fn") {
            continue; // a function declared inside the macro input
        }
        out.push(CallSite {
            caller: caller.map(str::to_string),
            callee: node_text(*token, src).to_string(),
            method: prev == Some("."),
            receiver: None,
            qualifier: None,
            arity: None,
            arguments: None,
            line: token.start_position().row + 1,
        });
    }
}

/// How many arguments an Erlang or Elixir call passes ([`CallSite::arity`]).
fn call_arity(call: Node, parent: Option<Node>, src: &str, lang: Lang) -> Option<u8> {
    // A Scala member read with no parentheses (`r.exitCode`) — or one
    // passed as a function (`xs.map(Cart.total)`, `laws.evalOn _`, `macro
    // Impl.expand`), an eta-expansion of a method of any parameters. (`_.run`
    // reads its argument's member.)
    if lang == Lang::Scala && call.kind() == "field_expression" {
        let placeholder = call
            .child_by_field_name("value")
            .is_some_and(|v| node_text(v, src) == "_");
        let passed = parent.is_some_and(|p| p.kind() == "arguments" || p.kind() == "method_value");
        let before = src[..call.start_byte()].trim_end();
        let expanded = before
            .strip_suffix("macro")
            .is_some_and(|b| !b.ends_with(|c: char| c.is_alphanumeric() || c == '_'));
        return Some(if !placeholder && (passed || expanded) {
            ETA
        } else {
            NO_LIST
        });
    }
    let count = |args: Node| {
        args.named_children(&mut args.walk())
            .filter(|a| a.kind() != "comment")
            .count()
    };
    // Haskell: the arguments the name is applied to, one `apply` each
    // (`setMeta "author"` one; `toJSON` passed as an argument none).
    if lang == Lang::Haskell {
        let mut n = 0u8;
        // (The call may be an `apply` itself, its function a chain of them.)
        let mut head = call;
        while head.kind() == "apply" {
            n = n.saturating_add(1);
            match head.child_by_field_name("function") {
                Some(f) => head = f,
                None => break,
            }
        }
        let mut at = call;
        while let Some(p) = at.parent() {
            if p.kind() == "apply"
                && p.child_by_field_name("function")
                    .is_some_and(|f| f.id() == at.id())
            {
                n = n.saturating_add(1);
                at = p;
            } else {
                break;
            }
        }
        return Some(n);
    }
    let n = match (lang, call.kind()) {
        // `fun f/2`, `fun m:f/2`.
        (Lang::Erlang, "internal_fun" | "external_fun") => {
            node_text(call.child_by_field_name("arity")?, src)
                .trim()
                .trim_start_matches('/')
                .trim()
                .parse()
                .ok()?
        }
        (Lang::Erlang, "call") => count(call.child_by_field_name("args")?),
        // `&f/2`, `&Cart.add/2`.
        (Lang::Elixir, "unary_operator") => {
            let operand = call.child_by_field_name("operand")?;
            node_text(operand.child_by_field_name("right")?, src)
                .trim()
                .parse()
                .ok()?
        }
        (Lang::Elixir, "call") => {
            // `&Cart.add/2`: the capture's arity, not the name's none.
            if let Some(p) = parent
                && p.kind() == "binary_operator"
                && p.child_by_field_name("left")
                    .is_some_and(|l| l.id() == call.id())
                && p.child_by_field_name("operator")
                    .is_some_and(|o| node_text(o, src) == "/")
                && p.parent().is_some_and(|g| g.kind() == "unary_operator")
            {
                return node_text(p.child_by_field_name("right")?, src)
                    .trim()
                    .parse()
                    .ok();
            }
            let mut cursor = call.walk();
            let mut n = 0;
            for child in call.named_children(&mut cursor) {
                match child.kind() {
                    "arguments" => n += count(child),
                    // `with_tx do … end`: the block is its last argument.
                    "do_block" => n += 1,
                    _ => {}
                }
            }
            // `x |> f(a)`: `x` is the first.
            let piped = parent.is_some_and(|p| {
                p.kind() == "binary_operator"
                    && p.child_by_field_name("right")
                        .is_some_and(|r| r.id() == call.id())
                    && p.child_by_field_name("operator")
                        .is_some_and(|o| node_text(o, src) == "|>")
            });
            n + usize::from(piped)
        }
        // Kotlin's `h(3) { … }`: its arguments and its trailing lambda — and
        // the lambda of a call around it (`h(3) { … }` is a call of the call).
        (Lang::Kotlin, "infix_expression") => 1,
        (Lang::Kotlin, "call_expression") => {
            let mut cursor = call.walk();
            let children: Vec<Node> = call.named_children(&mut cursor).collect();
            if children
                .first()
                .is_some_and(|f| f.kind() == "call_expression")
            {
                return None;
            }
            let mut n = 0;
            for child in &children {
                match child.kind() {
                    "value_arguments" => n += count(*child),
                    "call_suffix" => {
                        let mut c = child.walk();
                        for part in child.named_children(&mut c) {
                            match part.kind() {
                                "value_arguments" => n += count(part),
                                "annotated_lambda" => n += 1,
                                _ => {}
                            }
                        }
                    }
                    "annotated_lambda" => n += 1,
                    _ => {}
                }
            }
            let outer_lambda = parent.is_some_and(|p| {
                p.kind() == "call_expression"
                    && p.named_child(0).is_some_and(|f| f.id() == call.id())
                    && p.named_children(&mut p.walk())
                        .any(|c| c.kind() == "annotated_lambda")
            });
            n + usize::from(outer_lambda)
        }
        // Dart's `g(1, x: 2)`: its argument part's arguments.
        (Lang::Dart, _) => {
            let mut cursor = call.walk();
            let part = call
                .named_children(&mut cursor)
                .find(|c| c.kind() == "argument_part")?;
            let mut c = part.walk();
            let args = part
                .named_children(&mut c)
                .find(|c| c.kind() == "arguments")?;
            count(args)
        }
        // Zig passes its arguments after the function, unwrapped.
        (Lang::Zig, "call_expression") => {
            let function = call
                .child_by_field_name("function")
                .or_else(|| call.named_child(0))?;
            call.named_children(&mut call.walk())
                .filter(|c| c.id() != function.id() && c.kind() != "comment")
                .count()
        }
        (Lang::Java | Lang::CSharp | Lang::Cpp | Lang::Go | Lang::Scala, _) => {
            let args = call.child_by_field_name("arguments").or_else(|| {
                call.named_children(&mut call.walk())
                    .find(|c| matches!(c.kind(), "argument_list" | "arguments"))
            })?;
            count(args)
        }
        _ => return None,
    };
    u8::try_from(n).ok()
}

/// The kinds of what `call` passes, argument by argument
/// ([`CallSite::arguments`]), a named one's after its name (`x=i` for
/// `f(x = 1)`, C#'s `f(x: 1)`): `None` where it passes none, or the
/// grammar gives no argument list.
fn argument_kinds(
    call: Node,
    scope: &Scope,
    src: &str,
    lang: Lang,
    bindings: &mut HashMap<(usize, String), Bindings>,
) -> Option<String> {
    let mut lists: Vec<Node> = Vec::new();
    let mut trailing_lambdas = 0;
    match lang {
        // `a eq b`: its right operand.
        Lang::Kotlin if call.kind() == "infix_expression" => {
            let right = call.named_child(2)?;
            let text = node_text(right, src).trim();
            return Some(argument_kind(text, call, scope, src, lang, bindings));
        }
        // `f(a)`, `f(a) { … }`: its value arguments, then its lambda.
        Lang::Kotlin => {
            let mut cursor = call.walk();
            for child in call.named_children(&mut cursor) {
                match child.kind() {
                    "value_arguments" => lists.push(child),
                    "annotated_lambda" => trailing_lambdas += 1,
                    "call_suffix" => {
                        let mut c = child.walk();
                        for part in child.named_children(&mut c) {
                            match part.kind() {
                                "value_arguments" => lists.push(part),
                                "annotated_lambda" => trailing_lambdas += 1,
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
            }
            // `exec("x") { row -> … }`: the call is the first child of one
            // around it, which holds the lambda.
            if let Some(outer) = call.parent()
                && outer.kind() == "call_expression"
                && outer
                    .named_child(0)
                    .is_some_and(|first| first.id() == call.id())
            {
                let mut c = outer.walk();
                trailing_lambdas += outer
                    .named_children(&mut c)
                    .filter(|n| n.kind() == "annotated_lambda")
                    .count();
            }
        }
        _ => lists.push(call.child_by_field_name("arguments").or_else(|| {
            call.named_children(&mut call.walk())
                .find(|c| matches!(c.kind(), "argument_list" | "arguments"))
        })?),
    }
    // A C++ template's own type parameters around the call (`Float` in
    // `template <typename Float> void check()`): of any type the call is
    // made with, whatever else bears the name — `GFloat`, which passes
    // for any parameter and ranks as its name does.
    let template_params = if lang == Lang::Cpp {
        cpp_template_params_around(call, src)
    } else {
        Vec::new()
    };
    let mut kinds: Vec<String> = Vec::new();
    for list in lists {
        let mut cursor = list.walk();
        for argument in list.named_children(&mut cursor) {
            if argument.kind() == "comment" {
                continue;
            }
            let text = node_text(argument, src).trim();
            // A named argument: `x = 1` (Kotlin, Scala), `x: 1` (C#) — the
            // parameter of its name's, `x=i`.
            let mark = match lang {
                Lang::Kotlin | Lang::Scala => Some('='),
                Lang::CSharp => Some(':'),
                _ => None,
            };
            let named = mark.and_then(|mark| {
                let at = text.find(mark)?;
                let (name, rest) = (text[..at].trim(), &text[at + 1..]);
                (is_plain_name(name) && !rest.starts_with(['=', '>', ':']))
                    .then(|| (name, rest.trim()))
            });
            if let Some((name, value)) = named {
                let kind = argument_kind(value, call, scope, src, lang, bindings);
                kinds.push(format!("{name}={kind}"));
                continue;
            }
            let kind = argument_kind(text, call, scope, src, lang, bindings);
            // (`G`: of one, whatever it is called with.)
            let kind = match kind.strip_prefix('T') {
                Some(class) if template_params.iter().any(|t| t == class) => format!("G{class}"),
                _ => kind,
            };
            kinds.push(kind);
        }
    }
    // (A Kotlin lambda, `L`: passed for a function's type, or a library's
    // interface it converts to.)
    kinds.extend(std::iter::repeat_n("L".to_string(), trailing_lambdas));
    (!kinds.is_empty()).then(|| kinds.join(","))
}

/// The type parameters of the C++ templates around `node`: `Float` for a
/// call inside `template <typename Float> void check_isfinite() { … }`.
fn cpp_template_params_around(node: Node, src: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut at = node.parent();
    while let Some(n) = at {
        if n.kind() == "template_declaration"
            && let Some(list) = n.child_by_field_name("parameters")
        {
            let mut cursor = list.walk();
            for parameter in list.named_children(&mut cursor) {
                let name = match parameter.kind() {
                    "type_parameter_declaration" | "variadic_type_parameter_declaration" => {
                        parameter.named_child(0)
                    }
                    "optional_type_parameter_declaration" => parameter.child_by_field_name("name"),
                    _ => None,
                };
                if let Some(name) = name.filter(|n| n.kind() == "type_identifier") {
                    names.push(node_text(name, src).to_string());
                }
            }
        }
        at = n.parent();
    }
    names
}

/// What one argument is, as far as its text says: a literal's kind — `s` a
/// string, `c` a character, `i` an integer (`0` the integer zero), `f` a
/// floating-point number, `b` a boolean, `n` null — a value's type as
/// [`type_kind`] names it (`TCart` for `new Cart()`, or a name declared
/// `Cart cart`, `i` for one declared `int n`), or `?`.
fn argument_kind(
    text: &str,
    call: Node,
    scope: &Scope,
    src: &str,
    lang: Lang,
    bindings: &mut HashMap<(usize, String), Bindings>,
) -> String {
    let t = text.trim();
    // C#'s `ref x`, `out var y`: passed by reference, of the type it names.
    if lang == Lang::CSharp && ["ref ", "out ", "in "].iter().any(|m| t.starts_with(m)) {
        return "?".to_string();
    }
    if let Some(kind) = literal_kind(t, lang) {
        return kind.to_string();
    }
    // A Kotlin lambda (`{ it.name }`).
    if lang == Lang::Kotlin && t.starts_with('{') && t.ends_with('}') {
        return "L".to_string();
    }
    // A class literal: Java's `Pizza.class` and Kotlin's `Pizza::class.java`
    // are a `Class`, Kotlin's `Pizza::class` a `KClass`, Scala's
    // `classOf[Pizza]` a `Class`, C#'s `typeof(Pizza)` a `Type`.
    let literal_of = |prefix: &str| {
        !prefix.is_empty()
            && prefix
                .chars()
                .all(|c| c == '_' || c == '.' || c.is_alphanumeric())
    };
    let class_literal = match lang {
        Lang::Java => t
            .strip_suffix(".class")
            .filter(|p| literal_of(p))
            .map(|_| "TClass"),
        Lang::Kotlin => t
            .strip_suffix("::class.java")
            .filter(|p| literal_of(p))
            .map(|_| "TClass")
            .or_else(|| {
                t.strip_suffix("::class")
                    .filter(|p| literal_of(p))
                    .map(|_| "TKClass")
            }),
        Lang::Scala => (t.starts_with("classOf[") && t.ends_with(']')).then_some("TClass"),
        Lang::CSharp => (t.starts_with("typeof(") && t.ends_with(')')).then_some("TType"),
        _ => None,
    };
    if let Some(kind) = class_literal {
        return kind.to_string();
    }
    // A cast says the type (`(IRequest<TResponse>) request`, `x as Foo`):
    // what an overload is picked by.
    if let Some(ty) = cast_type(t, lang) {
        let kind = type_kind(ty, lang);
        // C#'s `(IRequest) r` is no `IRequest<T>`, which a variable read
        // without its type's arguments may be.
        return if lang == Lang::CSharp && kind.starts_with('T') && !kind.contains('`') {
            format!("{}`0", kind.trim_end_matches('?'))
        } else {
            kind
        };
    }
    if let Some(ty) = constructed_type(t, lang) {
        return type_kind(&ty, lang);
    }
    if is_plain_name(t) {
        let (_, said) = said_around(t, call.start_byte(), scope, src, lang, bindings);
        if let Said::Type(ty, _) = said
            && !ty.starts_with(RETURNED)
        {
            return type_kind(&ty, lang);
        }
    }
    "?".to_string()
}

/// What the bindings before `at` say `name` is, read from the function (or
/// closure) starting at `scope.from` — and where that binds no such name,
/// from the one around it: a closure's captured `store` in `useEffect(() =>
/// store.load())`, as that one says it is bound before the closure and still
/// is at its end (a binding after it may be what the closure sees when it
/// runs). Rust's nested `fn`s capture nothing (its closures are no function
/// here). With where it was read from.
fn said_around(
    name: &str,
    at: usize,
    scope: &Scope,
    src: &str,
    lang: Lang,
    bindings: &mut HashMap<(usize, String), Bindings>,
) -> (usize, Said) {
    let places = scope.places;
    let mut said = |from: usize, to: usize| {
        bindings
            .entry((from, name.to_string()))
            .or_insert_with(|| Bindings::from(from, places))
            .said_before(to, name, &Scope { places, from }, src, lang)
    };
    let (mut from, mut now) = (scope.from, said(scope.from, at));
    while now == Said::Nothing && lang != Lang::Rust {
        let Some((outer, outer_end)) = places.around(from) else {
            break;
        };
        let at_end = said(outer, outer_end);
        let before = said(outer, from);
        // (One type before and at the end, as exactly or not: as declared.)
        now = match (before, at_end) {
            (before, at_end) if before == at_end => before,
            (Said::Type(a, _), Said::Type(b, _)) if a == b => Said::Type(a, false),
            _ => return (outer, Said::Unknown),
        };
        from = outer;
    }
    (from, now)
}

/// The type an argument is cast to: Java's and C#'s `(Type) value`, C#'s
/// and Kotlin's `value as Type` (or `as?`). `None` for
/// no cast — a parenthesized value (`(a + b) * c`, `(count) - 1`) is none.
fn cast_type(t: &str, lang: Lang) -> Option<&str> {
    let type_like = |ty: &str| {
        let ty = ty.trim();
        !ty.is_empty()
            && ty.starts_with(|c: char| c.is_alphabetic() || c == '_')
            && ty.chars().all(|c| {
                c.is_alphanumeric()
                    || matches!(c, '_' | '.' | '<' | '>' | ',' | ' ' | '?' | '[' | ']')
            })
    };
    if matches!(lang, Lang::Java | Lang::CSharp)
        && let Some(rest) = t.strip_prefix('(')
    {
        let mut depth = 1usize;
        let close = rest.char_indices().find_map(|(i, c)| {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                _ => {}
            }
            None
        })?;
        let (ty, value) = (rest[..close].trim(), rest[close + 1..].trim_start());
        let primitive = matches!(
            ty,
            "int"
                | "long"
                | "short"
                | "byte"
                | "double"
                | "float"
                | "char"
                | "bool"
                | "boolean"
                | "string"
                | "object"
                | "decimal"
                | "uint"
                | "ulong"
        );
        // A type (a class's capital, or a primitive), then a value — not an
        // operator.
        let operand = value.starts_with(|c: char| {
            c.is_alphanumeric() || matches!(c, '_' | '(' | '"' | '\'' | '@' | '$')
        });
        return (type_like(ty) && (primitive || ty.starts_with(char::is_uppercase)) && operand)
            .then_some(ty);
    }
    if matches!(lang, Lang::CSharp | Lang::Kotlin) {
        let (_, ty) = t.rsplit_once(" as? ").or_else(|| t.rsplit_once(" as "))?;
        let ty = ty.trim();
        return (type_like(ty) && ty.starts_with(char::is_uppercase)).then_some(ty);
    }
    None
}

/// The kind of a literal ([`argument_kind`]), or `None` for no literal.
fn literal_kind(t: &str, lang: Lang) -> Option<&'static str> {
    let unquoted = |q: char| t.len() >= 2 && t.ends_with(q);
    // A string, prefixed or not: C#'s `@"…"`, `$"…"`; C++'s `L"…"`,
    // `u8"…"`, `R"(…)"`; Scala's `s"…"`, `f"…"`, `raw"…"`.
    let prefix_end = t.find(['"', '\'']).unwrap_or(0);
    let prefix = &t[..prefix_end];
    let string_prefix = match lang {
        Lang::CSharp => matches!(prefix, "" | "@" | "$" | "$@" | "@$"),
        Lang::Cpp => matches!(
            prefix,
            "" | "L" | "u8" | "u" | "U" | "R" | "LR" | "u8R" | "uR" | "UR"
        ),
        Lang::Scala => prefix.is_empty() || is_plain_name(prefix),
        _ => prefix.is_empty(),
    };
    if string_prefix && t[prefix_end..].starts_with('"') && unquoted('"') {
        // C++'s wide `L"…"`: no narrow string's.
        if lang == Lang::Cpp && prefix.starts_with('L') {
            return Some("w");
        }
        return Some("s");
    }
    let char_prefix =
        prefix.is_empty() || (lang == Lang::Cpp && matches!(prefix, "L" | "u8" | "u" | "U"));
    if char_prefix && t[prefix_end..].starts_with('\'') && unquoted('\'') && t.len() <= 12 {
        return Some("c");
    }
    match t {
        "true" | "false" => return Some("b"),
        "null" if lang != Lang::Cpp => return Some("n"),
        "nullptr" if lang == Lang::Cpp => return Some("n"),
        // C's `NULL` is `0`.
        "NULL" if lang == Lang::Cpp => return Some("0"),
        _ => {}
    }
    let digits = t.strip_prefix(['-', '+']).unwrap_or(t);
    if !digits.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    let hex = digits.starts_with("0x") || digits.starts_with("0X");
    let body = digits
        .trim_end_matches(|c: char| c.is_ascii_alphabetic() && !(hex && c.is_ascii_hexdigit()));
    let suffix = &digits[body.len()..];
    if !body
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '\'' | '+' | '-'))
    {
        return None;
    }
    let float = !hex
        && (body.contains('.')
            || body.contains(['e', 'E'])
            || suffix.eq_ignore_ascii_case("f")
            || suffix.eq_ignore_ascii_case("d")
            || suffix.eq_ignore_ascii_case("m"));
    Some(if float {
        "f"
    } else if body.trim_start_matches('0').is_empty() && !hex {
        "0"
    } else {
        "i"
    })
}

/// The kinds of the parameters of the function `l` declares in `src`
/// ([`FileContext::parameters`]): one per parameter by [`type_kind`], `v`
/// for a variadic one (and any after), comma-separated. `None` where its
/// parameter list cannot be read, or it takes none.
fn parameter_kinds(src: &str, l: &Located, lang: Lang) -> Option<String> {
    let text = src.get(l.declaration.0..l.declaration.1)?;
    let Parameters::At(open, close) = parameter_list(text, &l.symbol.name, lang)? else {
        return None;
    };
    let inner = text[open + 1..close].trim();
    if inner.is_empty() || (lang == Lang::Cpp && inner == "void") {
        return None;
    }
    let generic = type_parameters(&text[..open], lang);
    let mut kinds: Vec<String> = Vec::new();
    for (i, part) in split_top_level(inner).enumerate() {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let Some(ty) = parameter_type(part, lang) else {
            // C#'s extension `this`: passed by the dot, not in the list.
            if i == 0 && lang == Lang::CSharp && part.starts_with("this ") {
                continue;
            }
            kinds.push("?".to_string());
            continue;
        };
        if ty == "..." {
            kinds.push("v".to_string());
            break;
        }
        // One of its own type parameters (`const Container&`, `T`): any.
        let base = ty
            .trim_start_matches("const ")
            .trim()
            .split(['<', '[', '&', '*', ' ', '?'])
            .next()
            .unwrap_or("");
        if lang == Lang::Cpp
            && generic.iter().any(|g| g == base)
            && let Some(kind) = cpp_constraint(&text[..open], &text[close + 1..], base)
        {
            kinds.push(kind);
            continue;
        }
        if generic.iter().any(|g| g == base) {
            // C#'s `TRequest request` with `where TRequest : IRequest`: an
            // `IRequest`, as its constraint says.
            let constraint = (lang == Lang::CSharp)
                .then(|| csharp_constraint(&text[close + 1..], base))
                .flatten();
            kinds.push(constraint.map_or_else(|| "?".to_string(), |c| type_kind(c, lang)));
            continue;
        }
        kinds.push(type_kind(ty, lang));
    }
    (!kinds.is_empty()).then(|| kinds.join(","))
}

/// The names of the parameters of the Kotlin, Scala or C# function `l`
/// declares in `src` ([`FileContext::parameter_names`]), in order and
/// comma-separated — a variadic one's and those after it too, which
/// [`parameter_kinds`] leaves out. `None` where its list cannot be read,
/// or it takes none.
fn parameter_names_of(src: &str, l: &Located, lang: Lang) -> Option<String> {
    let text = src.get(l.declaration.0..l.declaration.1)?;
    let Parameters::At(open, close) = parameter_list(text, &l.symbol.name, lang)? else {
        return None;
    };
    let inner = text[open + 1..close].trim();
    let mut names: Vec<String> = Vec::new();
    for (i, part) in split_top_level(inner).enumerate() {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        // C#'s extension `this`: passed by the dot, not in the list.
        if i == 0 && lang == Lang::CSharp && part.starts_with("this ") {
            continue;
        }
        // (Its annotations aside: Kotlin's `@Suppress("x") name: T`.)
        let part = part
            .split_whitespace()
            .skip_while(|w| w.starts_with('@'))
            .collect::<Vec<_>>()
            .join(" ");
        names.push(parameter_name(&part, lang)?);
    }
    (!names.is_empty()).then(|| names.join(","))
}

/// The class or interface C#'s `where` clause after a parameter list
/// (`rest`) constrains the type parameter `name` to (`IRequest` in `where
/// TRequest : IRequest`) — none for `class`, `struct`, `new()` and the
/// like.
fn csharp_constraint<'a>(rest: &'a str, name: &str) -> Option<&'a str> {
    let rest = rest.split(['{', ';']).next().unwrap_or(rest);
    let rest = rest.split("=>").next().unwrap_or(rest);
    let head = format!("where {name}");
    let at = rest.find(&head)?;
    let after = rest[at + head.len()..].trim_start().strip_prefix(':')?;
    let list = after.split(" where ").next().unwrap_or(after);
    split_top_level(list).map(str::trim).find(|c| {
        !c.is_empty()
            && !matches!(
                *c,
                "class" | "class?" | "struct" | "new()" | "notnull" | "unmanaged" | "default"
            )
            && c.starts_with(|ch: char| ch.is_alphabetic() || ch == '_')
    })
}

/// The platforms a source file is built for, where a folder of its path
/// says — sbt's and Mill's cross-built `js/`, `jvm/`, `native/`,
/// `jvm-native/` (and the like), os-lib's `src-jvm/`, Kotlin
/// Multiplatform's `jsMain/`, `jvmMain/`, `nativeMain/` — the innermost
/// such folder's. `None` for a file of every platform (`shared/`, none
/// named).
fn source_platforms(path: &Path) -> Option<&'static [&'static str]> {
    path.components().rev().find_map(|c| {
        let name = c.as_os_str().to_str()?;
        let name = name
            .strip_prefix("src-")
            .or_else(|| name.strip_suffix("Main"))
            .or_else(|| name.strip_suffix("Test"))
            .unwrap_or(name);
        Some(match name {
            "js" => &["js"][..],
            "jvm" | "android" => &["jvm"][..],
            "native" | "ios" | "linuxX64" | "macosArm64" => &["native"][..],
            "jvm-native" | "native-jvm" => &["jvm", "native"][..],
            "js-jvm" | "jvm-js" => &["js", "jvm"][..],
            "js-native" | "native-js" => &["js", "native"][..],
            _ => return None,
        })
    })
}

/// `ty` as the type it aliases ([`FileContext::aliases`]), followed to the
/// end: one type, whichever name a value is declared by.
fn unaliased<'x>(alias_of: &HashMap<&str, &'x str>, ty: &'x str) -> &'x str {
    let mut ty = ty;
    for _ in 0..4 {
        match alias_of.get(ty) {
            Some(&next) if next != ty => ty = next,
            _ => break,
        }
    }
    ty
}

/// The kind of value a C++ template's type parameter `name` is constrained
/// to by its `enable_if` — fmt's `FMT_ENABLE_IF(…)`, `std::enable_if_t<…>`,
/// or C++20's `requires` — in the declaration's text before its parameter
/// list (`head`) or after it (`tail`): `Ki` for `std::is_integral<T>`, `K!i`
/// for its negation, `Kf` a floating-point type, `Ka` an arithmetic one
/// ([`cpp_satisfies`]). `None` where none says, or a disjunction does.
fn cpp_constraint(head: &str, tail: &str, name: &str) -> Option<String> {
    let tail = tail.split('{').next().unwrap_or(tail);
    let mut conditions: Vec<&str> = Vec::new();
    for text in [head, tail] {
        for opener in ["FMT_ENABLE_IF(", "enable_if_t<", "enable_if<"] {
            let mut rest = text;
            while let Some(at) = rest.find(opener) {
                let after = &rest[at + opener.len()..];
                let mut depth = 1i32;
                let mut end = after.len();
                for (i, c) in after.char_indices() {
                    match c {
                        '(' | '<' | '[' => depth += 1,
                        ')' | '>' | ']' => {
                            depth -= 1;
                            if depth == 0 {
                                end = i;
                                break;
                            }
                        }
                        ',' if depth == 1 && opener != "FMT_ENABLE_IF(" => {
                            end = i;
                            break;
                        }
                        _ => {}
                    }
                }
                conditions.push(&after[..end]);
                rest = &after[end..];
            }
        }
        if let Some(at) = text.find("requires ") {
            conditions.push(&text[at + "requires ".len()..]);
        }
    }
    let of = |trait_: &str| [format!("{trait_}<{name}>"), format!("{trait_}_v<{name}>")];
    for condition in conditions {
        if condition.contains("||") {
            continue;
        }
        for conjunct in condition.split("&&") {
            let conjunct = conjunct.trim().trim_start_matches('(').trim();
            let (negated, body) = match conjunct.strip_prefix('!') {
                Some(body) => (true, body.trim_start_matches('(')),
                None => (false, conjunct),
            };
            let kind = if of("is_integral").iter().any(|t| body.contains(t.as_str()))
                || body.contains(&format!("std::integral<{name}>"))
            {
                "i"
            } else if of("is_floating_point")
                .iter()
                .any(|t| body.contains(t.as_str()))
                || body.contains(&format!("std::floating_point<{name}>"))
            {
                "f"
            } else if of("is_arithmetic")
                .iter()
                .any(|t| body.contains(t.as_str()))
            {
                "a"
            } else {
                continue;
            };
            return Some(format!("K{}{kind}", if negated { "!" } else { "" }));
        }
    }
    None
}

/// Whether an argument of kind `arg` satisfies the constraint `rule` of
/// [`cpp_constraint`] (`i`, `!i`, `f`, …): an integer, a character or a
/// boolean is integral, a floating-point number is not; either is
/// arithmetic; a string, or a class the project declares (`declared`), is
/// neither. A type of no class this knows (the caller's own template's
/// `Float`) may be any.
fn cpp_satisfies(arg: &str, rule: &str, declared: &dyn Fn(&str) -> bool) -> bool {
    if arg
        .strip_prefix('T')
        .is_some_and(|class| !declared(class.trim_end_matches('?')))
    {
        return true;
    }
    let integral = matches!(arg, "i" | "0" | "c" | "b");
    let floating = arg == "f";
    let (negated, rule) = match rule.strip_prefix('!') {
        Some(rule) => (true, rule),
        None => (false, rule),
    };
    let holds = match rule {
        "i" => integral,
        "f" => floating,
        _ => integral || floating,
    };
    holds != negated
}

/// The type parameters a declaration's text before its parameter list
/// names: C++'s `template <typename Container, class It>`, Java's and
/// Kotlin's `<T extends X>` / `fun <reified T>`, C#'s `Send<TRequest>`,
/// Scala's `def f[F[_], A]`.
fn type_parameters(signature: &str, lang: Lang) -> Vec<String> {
    let (open, close) = match lang {
        Lang::Scala => ('[', ']'),
        _ => ('<', '>'),
    };
    let mut out = Vec::new();
    // C#'s and Scala's follow the name, the parameter list's opening next
    // (`Task<object> Send<T>(`): a group before is the return type's.
    let mut rest = signature;
    if matches!(lang, Lang::CSharp | Lang::Scala) {
        let trimmed = signature.trim_end();
        if !trimmed.ends_with(close) {
            return out;
        }
        let mut depth = 0i32;
        let start = trimmed.char_indices().rev().find_map(|(i, c)| {
            if c == close {
                depth += 1;
            } else if c == open {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            None
        });
        match start {
            Some(start) => rest = &trimmed[start..],
            None => return out,
        }
    }
    while let Some(at) = rest.find(open) {
        let mut depth = 0i32;
        let mut end = None;
        for (i, c) in rest[at..].char_indices() {
            if c == open {
                depth += 1;
            } else if c == close {
                depth -= 1;
                if depth == 0 {
                    end = Some(at + i);
                    break;
                }
            }
        }
        let Some(end) = end else {
            break;
        };
        // A type's own arguments (`Map<String, Int>`) name no parameter:
        // only a group a keyword or a name's end comes before.
        let before = rest[..at].trim_end();
        let opens_parameters = match lang {
            Lang::Cpp => before.ends_with("template"),
            Lang::Java | Lang::Kotlin => {
                before.is_empty()
                    || before.ends_with("fun")
                    || before.split_whitespace().last().is_some_and(|w| {
                        matches!(
                            w,
                            "public"
                                | "private"
                                | "protected"
                                | "static"
                                | "final"
                                | "abstract"
                                | "synchronized"
                                | "default"
                        )
                    })
            }
            _ => true,
        };
        if opens_parameters {
            for item in split_top_level(&rest[at + 1..end]) {
                let words: Vec<&str> = item
                    .split(|c: char| c.is_whitespace() || c == ':' || c == '[' || c == '=')
                    .filter(|w| {
                        !w.is_empty()
                            && !matches!(
                                *w,
                                "typename"
                                    | "class"
                                    | "reified"
                                    | "in"
                                    | "out"
                                    | "const"
                                    | "+"
                                    | "-"
                            )
                    })
                    .collect();
                if let Some(name) = words.first()
                    && is_plain_name(name.trim_start_matches(['+', '-']))
                {
                    out.push(name.trim_start_matches(['+', '-']).to_string());
                }
            }
        }
        rest = &rest[end + 1..];
    }
    out
}

/// The type a parameter's text declares — `...` for a variadic one — or
/// `None`, where it names none this can read (C#'s `this` receiver).
fn parameter_type(part: &str, lang: Lang) -> Option<&str> {
    let mut p = part.trim();
    // Annotations and attributes: `@NonNull String s`, `[FromBody] Cart c`.
    loop {
        let before = p;
        if p.starts_with('@') {
            let end = p
                .char_indices()
                .skip(1)
                .find(|&(_, c)| !(c == '_' || c == '.' || c == ':' || c.is_alphanumeric()))
                .map_or(p.len(), |(i, _)| i);
            p = p[end..].trim_start();
            if p.starts_with('(') {
                let close = p.find(')')?;
                p = p[close + 1..].trim_start();
            }
        }
        if lang == Lang::CSharp && p.starts_with('[') {
            p = p[p.find(']')? + 1..].trim_start();
        }
        for modifier in [
            "final ",
            "const ",
            "ref ",
            "out ",
            "in ",
            "scoped ",
            "readonly ",
            "val ",
            "var ",
            "crossinline ",
            "noinline ",
            "implicit ",
            "using ",
        ] {
            if lang != Lang::Cpp || modifier == "const " {
                p = p.strip_prefix(modifier).unwrap_or(p).trim_start();
            }
        }
        if p == before {
            break;
        }
    }
    if (lang == Lang::CSharp && p.starts_with("this ")) || p.is_empty() {
        return None;
    }
    if p.starts_with("vararg ") || p.starts_with("params ") || spreads(p, lang) {
        return Some("...");
    }
    // The default value is no part of the type.
    let p = split_top_level_at(p, '=').trim_end();
    match lang {
        // `name: Type`, Scala's by-name `name: => Type`, its repeated `T*`.
        Lang::Kotlin | Lang::Scala => {
            let (_, ty) = p.split_once(':')?;
            let ty = ty.trim().trim_start_matches("=>").trim();
            if lang == Lang::Scala && ty.ends_with('*') {
                return Some("...");
            }
            Some(ty)
        }
        // `Type name`: the name is the last word, unless the type is all
        // there is (C++'s `int`, `const std::string&`).
        _ => {
            let name_start = p
                .char_indices()
                .rev()
                .find(|&(_, c)| !(c == '_' || c.is_alphanumeric()))
                .map_or(0, |(i, c)| i + c.len_utf8());
            let (ty, name) = (p[..name_start].trim_end(), &p[name_start..]);
            let unnamed = ty.is_empty()
                || ty.ends_with("::")
                || (lang == Lang::Cpp
                    && CPP_NOT_TYPES.contains(&name)
                    && !ty.ends_with(['*', '&']));
            Some(if unnamed { p } else { ty })
        }
    }
}

/// Whether the parameter `part` spreads its arguments (`...`): in C++, a
/// pack outside angle brackets (`T&&... args`) — `format_string<T...> fmt`
/// is one parameter, its type's pack expanded.
fn spreads(part: &str, lang: Lang) -> bool {
    if lang != Lang::Cpp {
        return part.contains("...");
    }
    let mut angle = 0i32;
    let outside: String = part
        .chars()
        .filter(|&c| {
            match c {
                '<' => angle += 1,
                '>' => angle -= 1,
                _ => {}
            }
            angle <= 0 && c != '>'
        })
        .collect();
    outside.contains("...")
}

/// `text` up to the first `sep` outside brackets, quotes aside (a default
/// value's `=` after a parameter's type).
fn split_top_level_at(text: &str, sep: char) -> &str {
    let mut depth = 0i32;
    for (i, c) in text.char_indices() {
        match c {
            '(' | '[' | '{' | '<' => depth += 1,
            ')' | ']' | '}' | '>' => depth -= 1,
            c if c == sep && depth <= 0 => {
                // `=>` (Scala's by-name, a lambda's type) is no default.
                if text[i + 1..].starts_with('>') || text[..i].ends_with(['=', '!', '<']) {
                    continue;
                }
                return &text[..i];
            }
            _ => {}
        }
    }
    text
}

/// The kind of value a type is, as argument and parameter kinds name it: `s`
/// a string, `i` an integer, `f` a floating-point number, `b` a boolean,
/// `c` a character, `p` a C or C++ character pointer, `a` an array, `o` the
/// type every value is (`Object`, `Any`, C#'s `object`), `?` one that takes
/// anything this cannot rank (a type parameter, a pointer, `auto`), else
/// `T` and the class's name (`TCart`). A nullable
/// type (`String?`, C#'s `int?`) ends with `?`.
fn type_kind(ty: &str, lang: Lang) -> String {
    let mut t = ty.trim();
    let nullable = matches!(lang, Lang::Kotlin | Lang::CSharp) && t.ends_with('?');
    t = t.trim_end_matches('?').trim_end();
    if lang == Lang::Cpp {
        for word in [
            "const ",
            "volatile ",
            "constexpr ",
            "struct ",
            "class ",
            "typename ",
        ] {
            t = t.strip_prefix(word).unwrap_or(t).trim_start();
        }
        t = t
            .trim_end_matches(['&', ' '])
            .trim_end_matches(" const")
            .trim_end();
        if let Some(base) = t.strip_suffix('*') {
            let base = base.trim_end().trim_start_matches("const ").trim();
            let base = base.trim_end_matches(" const").trim();
            return if base == "wchar_t" {
                "w".to_string()
            } else if matches!(base, "char" | "char8_t" | "char16_t" | "char32_t" | "Char") {
                "p".to_string()
            } else {
                "?".to_string()
            };
        }
    }
    if t.contains("[]") || (lang == Lang::Kotlin && t.ends_with("Array") && t != "Array") {
        return "a".to_string();
    }
    let path = t.split(['<', '[', '(']).next().unwrap_or(t).trim();
    let last = path.rsplit([':', '.']).next().unwrap_or(path).trim();
    let words: Vec<&str> = path.split_whitespace().collect();
    let all = |set: &[&str]| !words.is_empty() && words.iter().all(|w| set.contains(w));
    let kind = match lang {
        Lang::Java => match last {
            "String" | "CharSequence" => "s",
            "int" | "long" | "short" | "byte" | "Integer" | "Long" | "Short" | "Byte" => "i",
            "float" | "double" | "Float" | "Double" => "f",
            "boolean" | "Boolean" => "b",
            "char" | "Character" => "c",
            "Object" => "o",
            _ => "",
        },
        Lang::Kotlin | Lang::Scala => match last {
            "String" | "CharSequence" => "s",
            "Int" | "Long" | "Short" | "Byte" | "UInt" | "ULong" | "UShort" | "UByte" => "i",
            "Float" | "Double" => "f",
            "Boolean" => "b",
            "Char" => "c",
            "Any" | "AnyRef" => "o",
            "AnyVal" => "?",
            _ => "",
        },
        Lang::CSharp => match last {
            "string" | "String" => "s",
            "int" | "long" | "short" | "byte" | "sbyte" | "uint" | "ulong" | "ushort" | "nint"
            | "nuint" | "Int32" | "Int64" | "Int16" | "UInt32" | "UInt64" | "UInt16" | "Byte"
            | "SByte" => "i",
            "float" | "double" | "decimal" | "Single" | "Double" | "Decimal" => "f",
            "bool" | "Boolean" => "b",
            "char" | "Char" => "c",
            "object" | "Object" => "o",
            "dynamic" | "var" => "?",
            _ => "",
        },
        Lang::Cpp => {
            if all(&["int", "long", "short", "unsigned", "signed"])
                || matches!(
                    last,
                    "size_t" | "ssize_t" | "ptrdiff_t" | "intptr_t" | "uintptr_t"
                )
                || ((last.starts_with("int") || last.starts_with("uint")) && last.ends_with("_t"))
            {
                "i"
            } else if all(&["float", "double", "long"]) {
                "f"
            } else if last == "bool" {
                "b"
            } else if matches!(
                last,
                "char" | "wchar_t" | "char8_t" | "char16_t" | "char32_t"
            ) {
                "c"
            } else if matches!(last, "wstring" | "wstring_view" | "wformat_string")
                || (matches!(last, "basic_string" | "basic_string_view")
                    && t[path.len()..]
                        .trim_start_matches('<')
                        .trim_start()
                        .starts_with("wchar_t"))
            {
                "w"
            } else if matches!(
                last,
                "string"
                    | "basic_string"
                    | "string_view"
                    | "basic_string_view"
                    | "u8string"
                    | "u16string"
                    | "u32string"
            ) {
                "s"
            } else if matches!(last, "auto" | "decltype") {
                "?"
            } else {
                ""
            }
        }
        _ => "?",
    };
    let kind = match kind {
        // A class, by name; a type parameter (`T`, `TValue`) takes anything.
        "" if last.starts_with(|c: char| c.is_alphabetic() || c == '_')
            && last.chars().all(|c| c == '_' || c.is_alphanumeric())
            && !(last.len() <= 2 && last.starts_with(char::is_uppercase))
            && !(lang == Lang::CSharp
                && last.len() > 1
                && last.starts_with('T')
                && last[1..].starts_with(char::is_uppercase)) =>
        {
            // C#'s `IRequest<TResponse>` is `IRequest`1`, another type than
            // `IRequest`.
            match (lang == Lang::CSharp)
                .then(|| generic_arity(&t[path.len()..]))
                .flatten()
            {
                Some(n) => format!("T{last}`{n}"),
                None => format!("T{last}"),
            }
        }
        "" => "?".to_string(),
        k => k.to_string(),
    };
    if nullable && kind != "?" {
        format!("{kind}?")
    } else {
        kind
    }
}

/// Whether an argument of kind `arg` can be passed for a parameter of kind
/// `param` in `lang` ([`argument_kind`], [`type_kind`]): `false` only where
/// the language refuses it — a string for a number, a number for a string,
/// null for a primitive (in Kotlin, for any type not nullable) — or, in
/// Java, Kotlin and C#, a project class for a primitive, a string or
/// another project class it is not one of (`declared`, `descends`), and in
/// Java and Kotlin a literal for a project class. C++'s conversions, and
/// Scala's implicit ones, refuse less.
fn passes(
    arg: &str,
    param: &str,
    lang: Lang,
    declared: &dyn Fn(&str) -> bool,
    descends: &dyn Fn(&str, &str) -> bool,
    converts: &dyn Fn(&str, &str) -> Option<bool>,
    reaches_library: &dyn Fn(&str) -> bool,
) -> bool {
    // `Object`, `Any`, `object`: what any value is (but null, for a Kotlin
    // type not nullable).
    if arg == "n" && param == "o" && lang == Lang::Kotlin {
        return false;
    }
    if arg == "?" || matches!(param.trim_end_matches('?'), "" | "o" | "v") {
        return true;
    }
    let nullable = param.ends_with('?');
    let p = param.trim_end_matches('?');
    let a = arg.trim_end_matches('?');
    let primitive = |k: &str| matches!(k, "i" | "f" | "b" | "c");
    if lang == Lang::Cpp {
        // A value of the calling template's own type parameter: any.
        if a.starts_with('G') {
            return true;
        }
        // A template's parameter its `enable_if` constrains.
        if let Some(rule) = p.strip_prefix('K') {
            return cpp_satisfies(a, rule, declared);
        }
        // A literal for a class of the project's: only by a constructor it
        // converts by ([`FileContext::converts`]) — `"{}"` is no
        // `text_style`, whose one takes an `emphasis`.
        if matches!(a, "s" | "w" | "i" | "f" | "b" | "c")
            && let Some(class) = p.strip_prefix('T')
            && let Some(takes) = converts(class, a)
        {
            return takes;
        }
        return cpp_passes(a, p);
    }
    let strict = matches!(lang, Lang::Java | Lang::Kotlin | Lang::CSharp);
    // A Kotlin lambda for a function's type (read as any), or a library's
    // interface it converts to (`Runnable`) — never a string, a number or
    // a class of the project's (`insert { … }` is no `insert(selectQuery:
    // AbstractQuery<*>)`).
    if a == "L" {
        return !(primitive(p)
            || matches!(p, "s" | "a" | "b" | "n")
            || p.strip_prefix('T').is_some_and(|class| {
                declared(without_arity(class))
                    // (A collection or a value, no interface a lambda is.)
                    || matches!(
                        class,
                        "Iterable"
                            | "Collection"
                            | "List"
                            | "MutableList"
                            | "Set"
                            | "MutableSet"
                            | "Map"
                            | "MutableMap"
                            | "Sequence"
                            | "Array"
                            | "Pair"
                            | "Triple"
                            | "Number"
                            | "Any"
                    )
            }));
    }
    // A string, number, character or boolean for a class: Java and Kotlin
    // convert none to one but those above them (`Number`, `Comparable`) —
    // no project class, nor a library's like `BufferedSource` (C# and Scala
    // may, by a conversion the class declares).
    if matches!(lang, Lang::Java | Lang::Kotlin)
        && matches!(a, "s" | "c" | "i" | "0" | "f" | "b")
        && let Some(class) = p.strip_prefix('T')
    {
        return !declared(class)
            && matches!(
                class,
                "Number" | "Comparable" | "Serializable" | "Constable" | "ConstantDesc"
            );
    }
    match (a, p) {
        ("s", k) => !primitive(k) && k != "a",
        ("c", "s" | "b" | "a") => false,
        ("c", "i" | "f") => lang != Lang::Kotlin,
        ("i" | "0", "s" | "b" | "a") => false,
        ("i" | "0", "c") => lang == Lang::Scala,
        ("i" | "0", "f") => lang != Lang::Kotlin,
        ("f", "i" | "b" | "c" | "s" | "a") => false,
        ("b", "i" | "f" | "c" | "s" | "a") => false,
        ("n", k) => nullable || (lang != Lang::Kotlin && !primitive(k)),
        // A project class: no primitive or string, and none of another
        // project class it does not extend or implement.
        // (C#'s `IRequest`1` is `IRequest<…>`: an argument of a class
        // named with no count may be of either.)
        (a, p) if a.starts_with('T') && strict && declared(without_arity(&a[1..])) => {
            match p.strip_prefix('T') {
                Some(class) => {
                    // (`0: a cast's own word that it takes none.)
                    let (arg, plain) = match a[1..].strip_suffix("`0") {
                        Some(arg) => (arg, true),
                        None => (&a[1..], false),
                    };
                    // (A class written to be the plain `IRequest`, never
                    // `IRequest<T>`, is none of the latter, whoever declares
                    // them.)
                    let only_plain = class.contains('`')
                        && descends(arg, without_arity(class))
                        && !descends(arg, class);
                    // (A library's class: one it names above it, one every
                    // value of its kind is, or any where a base of it is a
                    // library's, whose own are unknown.)
                    let library = !declared(without_arity(class))
                        && (matches!(
                            without_arity(class),
                            "Object"
                                | "Any"
                                | "Comparable"
                                | "Serializable"
                                | "Enum"
                                | "Record"
                                | "Cloneable"
                                | "IComparable"
                                | "IEquatable"
                                | "IConvertible"
                                | "IFormattable"
                                | "ValueType"
                        ) || reaches_library(arg));
                    !only_plain
                        && (library
                            || class == arg
                            || (!plain && !arg.contains('`') && without_arity(class) == arg)
                            || descends(arg, class))
                }
                None => !(primitive(p) || p == "s"),
            }
        }
        _ => true,
    }
}

/// Whether C++ passes an argument of kind `arg` for a parameter of kind
/// `param` by a standard conversion: a string is no number, character or
/// array, a narrow one no wide one (`"…"` is no `std::wstring`, `L"…"` no
/// `const char*`), a number, character or boolean no string, null no
/// number.
fn cpp_passes(arg: &str, param: &str) -> bool {
    !matches!(
        (arg, param),
        ("s" | "w", "i" | "f" | "c" | "a")
            | ("s", "w")
            | ("w", "p")
            | ("i" | "f" | "b" | "c", "s" | "p" | "w")
            | ("n", "i" | "f" | "c")
    )
}

/// Whether the bare call `call` of `callee` calls a parameter of a function
/// or closure around it — in a language whose functions are values, called
/// by name like a function.
fn calls_a_parameter(
    call: Node,
    callee: &str,
    src: &str,
    lang: Lang,
    parameters: &mut HashMap<usize, Vec<String>>,
) -> bool {
    if !matches!(
        lang,
        Lang::Dart
            | Lang::JavaScript
            | Lang::TypeScript
            | Lang::Tsx
            | Lang::Python
            | Lang::Kotlin
            | Lang::Scala
            | Lang::Go
            | Lang::C
            | Lang::Cpp
            | Lang::CSharp
            | Lang::Zig
    ) {
        return false;
    }
    let mut at = call.parent();
    while let Some(node) = at {
        // A Dart method's body follows its signature, beside it rather
        // than inside it: `void on(E e, Emitter<int> emit) { … }`.
        let signature = node
            .kind()
            .ends_with("body")
            .then(|| node.prev_named_sibling())
            .flatten();
        for holder in std::iter::once(node).chain(signature) {
            let names = parameters
                .entry(holder.id())
                .or_insert_with(|| parameter_names(holder, src, lang));
            if names.iter().any(|n| n == callee) {
                return true;
            }
        }
        at = node.parent();
    }
    false
}

/// The parameter-list kinds of the grammars [`calls_a_parameter`] reads.
const PARAMETER_LISTS: &[&str] = &[
    "formal_parameter_list",
    "formal_parameters",
    "parameters",
    "parameter_list",
    "function_value_parameters",
    "lambda_parameters",
    "bindings",
    "implicit_parameter",
];

/// The names of the parameters `node` declares, if it is a function or a
/// closure: its own parameter list's, or its signature's or declarator's
/// (Dart's `function_signature`, C's `function_declarator`), or an arrow
/// function's lone parameter (`x => …`).
fn parameter_names(node: Node, src: &str, lang: Lang) -> Vec<String> {
    let mut lists: Vec<Node> = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        let kind = child.kind();
        if PARAMETER_LISTS.contains(&kind) {
            lists.push(child);
        } else if kind.ends_with("signature") || kind.ends_with("declarator") {
            let mut inner = child.walk();
            lists.extend(
                child
                    .named_children(&mut inner)
                    .filter(|c| PARAMETER_LISTS.contains(&c.kind())),
            );
        }
    }
    let mut names = Vec::new();
    if node.kind() == "arrow_function"
        && let Some(lone) = node.child_by_field_name("parameter")
    {
        names.push(node_text(lone, src).to_string());
    }
    for list in lists {
        let text = node_text(list, src);
        let text = text
            .strip_prefix('(')
            .and_then(|t| t.strip_suffix(')'))
            .unwrap_or(text);
        names.extend(split_top_level(text).filter_map(|p| parameter_name(p, lang)));
    }
    names
}

/// `text` split at its commas outside brackets.
fn split_top_level(text: &str) -> impl Iterator<Item = &str> {
    let mut depth = 0i32;
    let mut start = 0;
    let mut parts = Vec::new();
    for (i, c) in text.char_indices() {
        match c {
            '(' | '[' | '{' | '<' => depth += 1,
            ')' | ']' | '}' | '>' => depth -= 1,
            ',' if depth <= 0 => {
                parts.push(&text[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&text[start..]);
    parts.into_iter()
}

/// The name one parameter declares: before its type in a language that
/// writes the name first (`cb: () -> Unit`, `cb func()`, `*args`), after
/// it in one that writes the type first (`Emitter<String> emit`, `Action
/// cb`, `void (*cb)(int)`).
fn parameter_name(param: &str, lang: Lang) -> Option<String> {
    let is_name = |c: char| c == '_' || c == '$' || c.is_alphanumeric();
    // Its default is no part of it.
    let mut depth = 0i32;
    let mut end = param.len();
    for (i, c) in param.char_indices() {
        match c {
            '(' | '[' | '{' | '<' => depth += 1,
            ')' | ']' | '}' | '>' => depth -= 1,
            '=' if depth <= 0 && !param[i + 1..].starts_with(['=', '>']) => {
                end = i;
                break;
            }
            _ => {}
        }
    }
    let param = param[..end]
        .trim()
        .trim_start_matches(['{', '['])
        .trim_end_matches(['}', ']']);
    let name_first = matches!(
        lang,
        Lang::Python
            | Lang::Kotlin
            | Lang::Scala
            | Lang::JavaScript
            | Lang::TypeScript
            | Lang::Tsx
            | Lang::Go
            | Lang::Zig
    );
    let name = if name_first {
        let head = param.split(':').next().unwrap_or(param);
        head.split_whitespace()
            .map(|w| w.trim_start_matches(['*', '.', '&']))
            .find(|w| {
                !w.is_empty()
                    && !matches!(
                        *w,
                        "val"
                            | "var"
                            | "vararg"
                            | "noinline"
                            | "crossinline"
                            | "comptime"
                            | "noalias"
                            | "readonly"
                            | "private"
                            | "public"
                            | "protected"
                            | "override"
                            | "implicit"
                            | "using"
                            | "lazy"
                    )
            })?
            .trim_end_matches('?')
    } else if let Some(at) = param.find("(*") {
        // A C function pointer: `void (*cb)(int)`.
        let rest = &param[at + 2..];
        &rest[..rest.find(|c: char| !is_name(c)).unwrap_or(rest.len())]
    } else {
        let end = param.trim_end_matches(|c: char| !is_name(c));
        let start = end
            .char_indices()
            .rev()
            .find(|&(_, c)| !is_name(c))
            .map_or(0, |(i, c)| i + c.len_utf8());
        &end[start..]
    };
    (!name.is_empty() && name.starts_with(|c: char| !c.is_ascii_digit())).then(|| name.to_string())
}

/// The names a function definition goes by, most preferred first. A
/// declaration has one: its `name` field. A function EXPRESSION is known by
/// the binding it is assigned to (`const handler = () => …` → `handler`,
/// `a.b = function` → `b`, a class field → the field, an object entry → the
/// key) — and a named one (`const x = function named() {}`) also by its own
/// name, which is in scope only inside its own body, for recursion. Callers
/// and the outline know it by the binding, so the binding comes first: taking
/// the own name first attributed its calls to `named`, which a TypeScript
/// outline does not list, so they fell to whatever function enclosed it.
///
/// `parent` is the node's parent and `before` the sibling before it, both
/// carried by the walk.
fn fn_names(
    node: Node,
    parent: Option<Node>,
    before: Option<Node>,
    src: &str,
    lang: Lang,
) -> Vec<String> {
    match lang {
        // `function_definition` names its function inside the declarator.
        Lang::C | Lang::Cpp => return c_function_name(node, src).into_iter().collect(),
        // Dart: a `function_body` is a bare sibling after its signature, so
        // its name comes from the preceding signature (a lambda/arrow body's
        // signature has no name, so those calls fall through to the enclosing
        // named function).
        // `def name(…) do`: the name is the call its first argument is.
        Lang::Elixir => return elixir_def_name(node, src).into_iter().collect(),
        Lang::Haskell => return haskell_fn_name(node, src).into_iter().collect(),
        Lang::Dart if node.kind() == "function_body" => {
            // The sibling the walk carried: `prev_sibling` finds it through
            // `parent()`, a walk down from the root, and a scan of the
            // parent's children cost their number per body.
            return before
                .and_then(|sig| dart_sig_name(sig, src))
                .into_iter()
                .collect();
        }
        _ => {}
    }
    let own = node
        .child_by_field_name("name")
        .map(|n| node_text(n, src).to_string());
    binding_name(parent, src).into_iter().chain(own).collect()
}

/// A Haskell function's name: `add` of `add a b = …`, the operator of
/// `(<+>) a b = …` and of `a <+> b = …`, the function of ``x `plus` y = …``
/// — as the outline names them, the operator without its parentheses.
fn haskell_fn_name(node: Node, src: &str) -> Option<String> {
    if let Some(name) = node.child_by_field_name("name") {
        let name = match name.kind() {
            "prefix_id" => name.named_child(0)?,
            _ => name,
        };
        return Some(node_text(name, src).to_string());
    }
    let head = node.named_child(0).filter(|h| h.kind() == "infix")?;
    haskell_infix_name(head, src).map(|(name, _)| name)
}

/// What an infix application `a <+> b` (or ``x `plus` y``) applies: the
/// operator, or the function between backticks, and whether it is another
/// module's (``x `M.plus` y``). A constructor (`x : xs`, `a :+: b`) applies
/// no function.
fn haskell_infix_name(infix: Node, src: &str) -> Option<(String, bool)> {
    let op = infix.child_by_field_name("operator")?;
    match op.kind() {
        "operator" => Some((node_text(op, src).to_string(), false)),
        "infix_id" => {
            let inner = op.named_child(0)?;
            match inner.kind() {
                "variable" => Some((node_text(inner, src).to_string(), false)),
                "qualified" => {
                    let id = inner.child_by_field_name("id")?;
                    (id.kind() == "variable").then(|| (node_text(id, src).to_string(), true))
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// The base library's operators, which no project defines: an infix use of
/// one is no call of the project's, and is not kept as one.
const HASKELL_BASE_OPERATORS: &[&str] = &[
    "+", "-", "*", "/", "^", "^^", "**", "==", "/=", "<", "<=", ">", ">=", "&&", "||", "$", "$!",
    ".", "<$>", "<$", "$>", "<&>", "<*>", "<*", "*>", ">>=", ">>", "=<<", ">=>", "<=<", "++", "!!",
    "<>", "<|>", "&", "@", "~", "|", "=", "->", "<-", "=>", "::", "\\",
];

/// The function an Elixir `def` (or `defp`, `defmacro`, …) call defines:
/// its first argument's name — `f(x)`, `f(x) when …` or a bare `f`.
fn elixir_def_name(call: Node, src: &str) -> Option<String> {
    let target = call.child_by_field_name("target")?;
    if !matches!(
        node_text(target, src),
        "def" | "defp" | "defmacro" | "defmacrop" | "defguard" | "defguardp" | "defn" | "defnp"
    ) {
        return None;
    }
    let args = call
        .named_children(&mut call.walk())
        .find(|c| c.kind() == "arguments")?;
    let mut head = args.named_child(0)?;
    if head.kind() == "binary_operator" {
        head = head.child_by_field_name("left")?;
    }
    let name = match head.kind() {
        "call" => head.child_by_field_name("target")?,
        "identifier" => head,
        _ => return None,
    };
    (name.kind() == "identifier").then(|| node_text(name, src).to_string())
}

/// The Haskell operators whose operands are functions applied or composed:
/// a function named as one is used there (`show . add 1`, `f <$> xs`).
const HASKELL_FUNCTION_OPERATORS: &[&str] = &[
    "$", "&", ".", "<$>", "<$", "$>", "<&>", "<*>", "<*", "*>", ">>=", "=<<", ">=>", "<=<",
];

/// Whether an Elixir `call` (whose parent is `parent`) is the head of a
/// `def` — `add(a, b)` in `def add(a, b) do`, guarded (`when …`) or not:
/// the function it defines, which it does not call. Read as a call, every
/// function called itself.
fn elixir_def_head(call: Node, parent: Option<Node>, src: &str) -> bool {
    let Some(mut up) = parent else {
        return false;
    };
    if up.kind() == "binary_operator" {
        if up.child_by_field_name("left") != Some(call) {
            return false;
        }
        let Some(next) = up.parent() else {
            return false;
        };
        up = next;
    }
    up.kind() == "arguments"
        && up
            .named_child(0)
            .is_some_and(|first| first == call || Some(first) == parent)
        && up
            .parent()
            .is_some_and(|def| elixir_def_name(def, src).is_some())
}

/// The name of the binding a function expression is assigned to, if it is
/// assigned to one (see [`fn_names`]); `parent` is the expression's parent.
fn binding_name(parent: Option<Node>, src: &str) -> Option<String> {
    let parent = parent?;
    let named = match parent.kind() {
        "variable_declarator" | "public_field_definition" => parent.child_by_field_name("name"),
        // JavaScript's class field names its property `property`.
        "field_definition" => parent.child_by_field_name("property"),
        // `a.b = function () {}` and `Foo.prototype.bar = …` name the member,
        // as the outline does — not the whole `a.b` expression.
        "assignment_expression" => {
            parent
                .child_by_field_name("left")
                .map(|left| match left.kind() {
                    "member_expression" => left.child_by_field_name("property").unwrap_or(left),
                    _ => left,
                })
        }
        "pair" => parent.child_by_field_name("key"),
        _ => None,
    }?;
    Some(node_text(named, src).to_string())
}

/// The function a C/C++ `function_definition` defines: its declarator chain
/// (`*foo(void)`, `(&bar)(int)`, `Ns::Cls::baz() const`) down to the name.
fn c_function_name(def: Node, src: &str) -> Option<String> {
    let mut node = def.child_by_field_name("declarator")?;
    for _ in 0..64 {
        node = match node.kind() {
            "function_declarator"
            | "pointer_declarator"
            | "reference_declarator"
            | "attributed_declarator" => node.child_by_field_name("declarator")?,
            "parenthesized_declarator" => node.named_child(0)?,
            _ => {
                let name = innermost_name(node);
                // `~Foo` reads as `Foo` to `last_identifier`, which attributed
                // a destructor's calls to the constructor. The outline lists
                // no destructor, so there is no node for them.
                if name.kind() == "destructor_name" {
                    return None;
                }
                return last_identifier(node_text(name, src));
            }
        };
    }
    None
}

/// The name from a Dart signature preceding a `function_body`. Direct signatures
/// (`function_`/`getter_`/`setter_signature`) carry a `name` field; a
/// `method_signature` wraps one of those, so look one level in.
fn dart_sig_name(sig: Node, src: &str) -> Option<String> {
    if let Some(n) = sig.child_by_field_name("name") {
        return Some(node_text(n, src).to_string());
    }
    let mut cursor = sig.walk();
    sig.children(&mut cursor).find_map(|c| {
        c.child_by_field_name("name")
            .map(|n| node_text(n, src).to_string())
    })
}

/// The type a Rust path call names its function through: `Self::make()`
/// the impl's own, `Builder::new()` and `Vec::<u8>::new()` the type's. A
/// module's path (`fs::read`, `crate::x::f`) names none: its function is
/// found by the imports.
fn rust_path_receiver(call: Node, src: &str) -> Option<Receiver> {
    let mut function = call.child_by_field_name("function")?;
    if function.kind() == "generic_function" {
        function = function.child_by_field_name("function")?;
    }
    if function.kind() != "scoped_identifier" {
        return None;
    }
    let path = function.child_by_field_name("path")?;
    let path = match path.kind() {
        "generic_type" => path.child_by_field_name("type")?,
        _ => path,
    };
    let last = match path.kind() {
        "scoped_identifier" | "scoped_type_identifier" => path.child_by_field_name("name")?,
        _ => path,
    };
    let name = node_text(last, src);
    match name {
        "Self" => Some(Receiver::Own),
        _ if name.starts_with(|c: char| c.is_uppercase()) => Some(Receiver::Type(name.to_string())),
        _ => None,
    }
}

/// Descend through wrappers that end in a name — a turbofish or template
/// argument list, a `ns::` qualification — to the name node itself, so
/// `ns::f<int>` yields `f`, not `int`.
fn innermost_name(mut node: Node) -> Node {
    loop {
        let inner = match node.kind() {
            "generic_function" => node.child_by_field_name("function"),
            "template_function"
            | "template_method"
            | "template_type"
            | "qualified_identifier"
            | "scoped_identifier" => node.child_by_field_name("name"),
            _ => None,
        };
        match inner {
            Some(n) => node = n,
            None => return node,
        }
    }
}

/// The called name (trailing identifier of the callee expression —
/// `self.foo.bar` → `bar`, `Vec::<u8>::with_capacity` → `with_capacity`) plus
/// whether the call is a `receiver.name(…)` method access. `None` for a call
/// that cannot name a project function. `parent` is the call's parent,
/// carried by the walk: in Erlang and Ruby it says what the call is.
fn callee_name(call: Node, parent: Option<Node>, src: &str, lang: Lang) -> Option<(String, bool)> {
    match (lang, call.kind()) {
        // PHP calls a function by name, or by a qualified one
        // (`\App\helper()`). A name held in a variable (`$f()`, `$o->$m()`,
        // `new $class`) is none the reader can follow.
        (Lang::Php, "function_call_expression") => {
            let function = call.child_by_field_name("function")?;
            return Some((php_name(function, src)?, false));
        }
        // `$o->m()`, `$o?->m()`, and `Foo::m()` / `self::m()` /
        // `parent::m()`, whose receiver is a class: a method each, like
        // Java's `Foo.m()`.
        (
            Lang::Php,
            "member_call_expression" | "nullsafe_member_call_expression" | "scoped_call_expression",
        ) => {
            let name = call.child_by_field_name("name")?;
            return Some((php_name(name, src)?, true));
        }
        // `new Foo(…)` names the class. `new self`, `new static` and
        // `new parent` name the class around the call, which is no name of
        // their own.
        (Lang::Php, "object_creation_expression") => {
            let class = php_name(call.named_child(0)?, src)?;
            if matches!(class.as_str(), "self" | "static" | "parent") {
                return None;
            }
            return Some((class, false));
        }
        // A Ruby call carries its method name in a field, and a receiver
        // when it is not on `self` implicitly. The name is read whole:
        // `valid?` and `save!` are names of their own, not `valid` and
        // `save`. Assigning through a call (`self.total = 0`) calls the
        // setter, which is defined as `total=`.
        (Lang::Ruby, "call") => {
            let mut name = node_text(call.child_by_field_name("method")?, src).to_string();
            let method = call.child_by_field_name("receiver").is_some();
            let assigned = parent.is_some_and(|p| {
                p.kind() == "assignment"
                    && p.child_by_field_name("left")
                        .is_some_and(|left| left.id() == call.id())
            });
            if assigned {
                name.push('=');
            }
            return Some((name, method));
        }
        // C#: `F()`, `F<T>()`, `x.F()`, `x?.F()`. `nameof(x)` is an
        // operator, though it is written as a call.
        (Lang::CSharp, "invocation_expression") => {
            let function = call.child_by_field_name("function")?;
            let (name, method) = match function.kind() {
                "identifier" | "generic_name" => (function, false),
                "member_access_expression" | "member_binding_expression" => {
                    (function.child_by_field_name("name")?, true)
                }
                // `x?.F()`: the member is bound after the condition.
                "conditional_access_expression" => {
                    let mut cursor = function.walk();
                    let bound = function
                        .named_children(&mut cursor)
                        .filter(|n| n.kind() == "member_binding_expression")
                        .last()?;
                    (bound.child_by_field_name("name")?, true)
                }
                _ => return None,
            };
            let name = csharp_name(name, src)?;
            if !method && name == "nameof" {
                return None;
            }
            return Some((name, method));
        }
        // `new Foo(…)`, `new Ns.Foo<T>(…)`: the constructor is the type's
        // base name, as the outline names it. A target-typed `new()` is an
        // `implicit_object_creation_expression`, naming no type.
        (Lang::CSharp, "object_creation_expression") => {
            let ty = call.child_by_field_name("type")?;
            return Some((csharp_name(ty, src)?, false));
        }
        // Erlang's `f(…)` is a local call. In `m:f(…)` the call is the `fun`
        // of a `remote`: another module's function, so method-like. A fun
        // held in a variable (`F(x)`) or written in place names nothing.
        // The atom is taken as written, as the outline takes it: a quoted
        // `'f'` or a `node@host` is no identifier run.
        // `fun add/2`, `fun cart:add/2`: the function named, to be called.
        (Lang::Erlang, "internal_fun" | "external_fun") => {
            let function = call.child_by_field_name("fun")?;
            return (function.kind() == "atom").then(|| {
                (
                    node_text(function, src).to_string(),
                    call.kind() == "external_fun",
                )
            });
        }
        // `&add/2`: the function captured (`&Cart.add/2` is a call node).
        (Lang::Elixir, "unary_operator") => {
            let capture = call.child(0).is_some_and(|op| node_text(op, src) == "&");
            let operand = call.child_by_field_name("operand")?;
            let left = operand.child_by_field_name("left")?;
            return (capture && operand.kind() == "binary_operator" && left.kind() == "identifier")
                .then(|| (node_text(left, src).to_string(), false));
        }
        // A function passed as a value: an application's argument
        // (`foldr add 0`), or the operand of an operator that applies or
        // composes it (`show . add 1`, `f <$> xs`, `print $ x`). As an
        // application's function it is that application's callee.
        // An operator applied infix (`a <+> b`), a function written between
        // backticks (``x `plus` y``): calls of them — the project's own, as
        // the base library's (`+`, `$`, `<$>`) name none of its functions. A
        // definition's head (`a <+> b = …`) defines what it names.
        (Lang::Haskell, "infix") => {
            if parent.is_some_and(|p| p.kind() == "function") {
                return None;
            }
            let (name, other_module) = haskell_infix_name(call, src)?;
            return (!HASKELL_BASE_OPERATORS.contains(&name.as_str()))
                .then_some((name, other_module));
        }
        // An operator passed as a value (`foldr (<+>) 0 xs`): used there,
        // as a function is. As an application's function it is that
        // application's callee.
        (Lang::Haskell, "prefix_id") => {
            let parent = parent?;
            let passed = match parent.kind() {
                "apply" => parent.child_by_field_name("argument") == Some(call),
                "infix" => parent
                    .child_by_field_name("operator")
                    .is_some_and(|op| HASKELL_FUNCTION_OPERATORS.contains(&node_text(op, src))),
                _ => false,
            };
            let op = call.named_child(0).filter(|op| op.kind() == "operator")?;
            let name = node_text(op, src);
            return (passed && !HASKELL_BASE_OPERATORS.contains(&name))
                .then(|| (name.to_string(), false));
        }
        (Lang::Haskell, "variable" | "qualified") => {
            let parent = parent?;
            let passed = match parent.kind() {
                "apply" => parent.child_by_field_name("argument") == Some(call),
                "infix" => parent
                    .child_by_field_name("operator")
                    .is_some_and(|op| HASKELL_FUNCTION_OPERATORS.contains(&node_text(op, src))),
                _ => false,
            };
            if !passed {
                return None;
            }
            return match call.kind() {
                "variable" => Some((node_text(call, src).to_string(), false)),
                _ => {
                    let id = call.child_by_field_name("id")?;
                    (id.kind() == "variable").then(|| (node_text(id, src).to_string(), true))
                }
            };
        }
        (Lang::Erlang, "call") => {
            let function = call.child_by_field_name("expr")?;
            if function.kind() != "atom" {
                return None;
            }
            let remote = parent.is_some_and(|p| p.kind() == "remote");
            return Some((node_text(function, src).to_string(), remote));
        }
        // Haskell's `f x y` is `apply (apply f x) y`: only the innermost
        // application names the function, the outer ones are the same call
        // taking its further arguments. A data constructor (`Just x`) is no
        // call, and a type's `IO ()` is an `apply` of a `constructor`. A
        // qualified `M.f` is another module's, like Python's `mod.f`.
        (Lang::Haskell, "apply") => {
            let function = call.child_by_field_name("function")?;
            return match function.kind() {
                "variable" => Some((node_text(function, src).to_string(), false)),
                // `(<+>) 1 2`.
                "prefix_id" => {
                    let op = function
                        .named_child(0)
                        .filter(|op| op.kind() == "operator")?;
                    let name = node_text(op, src);
                    (!HASKELL_BASE_OPERATORS.contains(&name)).then(|| (name.to_string(), false))
                }
                "qualified" => {
                    let id = function.child_by_field_name("id")?;
                    (id.kind() == "variable").then(|| (node_text(id, src).to_string(), true))
                }
                _ => None,
            };
        }
        // Zig: `f(…)`, or `x.f(…)` through a field — a method, or a
        // namespace's function (`std.debug.print`). A builtin (`@import(…)`)
        // is a `builtin_function`, never a call expression.
        (Lang::Zig, "call_expression") => {
            let function = call.child_by_field_name("function")?;
            return match function.kind() {
                "identifier" => Some((node_text(function, src).to_string(), false)),
                "field_expression" => {
                    let member = function.child_by_field_name("member")?;
                    Some((node_text(member, src).to_string(), true))
                }
                _ => None,
            };
        }
        // Kotlin: the callee is the call's first child — a name, or a
        // navigation whose last part is the method (`repo.findAll()`).
        // An infix call (`Users.id eq 1`): the function between, called
        // on its left.
        // (Only on a name — a parameter or a local, whose type its binding
        // says — or `this`: on a property (`Users.id eq 1`), of a type this
        // reads none of, every overload of the name would be as likely.)
        (Lang::Kotlin, "infix_expression") => {
            let function = call.named_child(1).filter(|f| f.kind() == "identifier")?;
            let left = call.named_child(0)?;
            if !matches!(left.kind(), "identifier" | "this_expression") {
                return None;
            }
            return Some((node_text(function, src).to_string(), true));
        }
        (Lang::Kotlin, "call_expression") => {
            let callee = call.named_child(0)?;
            return match callee.kind() {
                "identifier" => Some((node_text(callee, src).to_string(), false)),
                "navigation_expression" => {
                    let last =
                        callee.named_child(callee.named_child_count().checked_sub(1)? as u32)?;
                    (last.kind() == "identifier").then(|| (node_text(last, src).to_string(), true))
                }
                _ => None,
            };
        }
        // `new Foo[T](…)`: the class it makes.
        // Scala calls a method with no parameters without parentheses
        // (`ref.get`, `io.void`): a member read that no call's parentheses
        // follow, nor an assignment's left side. A field read so links to
        // nothing, as no method has its name.
        (Lang::Scala, "field_expression") => {
            let called = parent.is_some_and(|p| {
                (p.kind() == "call_expression"
                    && p.child_by_field_name("function")
                        .is_some_and(|f| f.id() == call.id()))
                    || (p.kind() == "assignment_expression"
                        && p.child_by_field_name("left")
                            .is_some_and(|l| l.id() == call.id()))
                    || p.kind() == "generic_function"
            });
            if called {
                return None;
            }
            let field = call.child_by_field_name("field")?;
            let name = node_text(field, src);
            return (name.starts_with(char::is_lowercase) && field.kind() == "identifier")
                .then(|| (name.to_string(), true));
        }
        // `new Column(_)` builds a class: its constructor's call (as a
        // method of the class itself, [`receiver_of`]), never a `def Column`.
        (Lang::Scala, "instance_expression") => {
            let mut cursor = call.walk();
            let ty = call
                .named_children(&mut cursor)
                .find(|n| matches!(n.kind(), "type_identifier" | "generic_type"))?;
            let ty = match ty.kind() {
                "generic_type" => ty.named_child(0)?,
                _ => ty,
            };
            return Some((last_identifier(node_text(ty, src))?, true));
        }
        // Elixir: `f(…)`, or another module's `Mod.f(…)` (like Erlang's
        // `mod:f(…)`). A `def` defines rather than calls (`fn_names`).
        (Lang::Elixir, "call") => {
            if elixir_def_name(call, src).is_some() || elixir_def_head(call, parent, src) {
                return None;
            }
            let target = call.child_by_field_name("target")?;
            return match target.kind() {
                "identifier" => Some((node_text(target, src).to_string(), false)),
                "dot" => {
                    let right = target.child_by_field_name("right")?;
                    // `user.name` with no parentheses reads a map's or a
                    // struct's field; only a module's function is called
                    // so (`Cart.total`).
                    let field = target
                        .child_by_field_name("left")
                        .is_some_and(|left| left.kind() != "alias")
                        && !call
                            .named_children(&mut call.walk())
                            .any(|c| c.kind() == "arguments");
                    (right.kind() == "identifier" && !field)
                        .then(|| (node_text(right, src).to_string(), true))
                }
                _ => None,
            };
        }
        // Java names the method in a field of its own; `obj.f()` carries the
        // receiver as `object`.
        (Lang::Java, "method_invocation") => {
            let name = call.child_by_field_name("name")?;
            let method = call.child_by_field_name("object").is_some();
            return Some((node_text(name, src).to_string(), method));
        }
        // `<Button …>` renders the component `Button`; `<Select.Item>` the
        // `Item` of `Select`, as a method is called on its object. A
        // lower-case element (`<div>`) is the page's own, no project
        // code.
        (
            Lang::JavaScript | Lang::TypeScript | Lang::Tsx,
            "jsx_opening_element" | "jsx_self_closing_element",
        ) => {
            let name = call.child_by_field_name("name")?;
            return match name.kind() {
                "identifier" => {
                    let text = node_text(name, src);
                    text.starts_with(|c: char| c.is_ascii_uppercase())
                        .then(|| (text.to_string(), false))
                }
                "member_expression" => {
                    let property = name.child_by_field_name("property")?;
                    Some((node_text(property, src).to_string(), true))
                }
                _ => None,
            };
        }
        // `new Foo<Bar>(…)`: the constructor is the type's base name.
        (Lang::Java, "object_creation_expression") | (Lang::Cpp, "new_expression") => {
            let mut ty = call.child_by_field_name("type")?;
            if ty.kind() == "generic_type" {
                ty = ty.named_child(0)?;
            }
            let ty = innermost_name(ty);
            return Some((last_identifier(node_text(ty, src))?, false));
        }
        // Dart constructor calls name a type via a child, not a `function` field.
        (Lang::Dart, "constructor_invocation") => {
            let mut cursor = call.walk();
            let ty = call
                .children(&mut cursor)
                .find(|n| matches!(n.kind(), "type_identifier" | "identifier"))?;
            return Some((last_identifier(node_text(ty, src))?, false));
        }
        _ => {}
    }
    let mut target = call
        .child_by_field_name("function")
        .or_else(|| call.child_by_field_name("constructor"))
        .or_else(|| call.child(0))?;
    // A trailing turbofish / template argument list wraps the callee; unwrap to
    // the real function so the name and method-flag come from `parse`, not
    // from `i32`.
    while matches!(target.kind(), "generic_function" | "template_function") {
        let inner = match target.kind() {
            "generic_function" => target.child_by_field_name("function"),
            _ => target.child_by_field_name("name"),
        };
        match inner {
            Some(inner) => target = inner,
            None => break,
        }
    }
    // `f(a)(b)`, Scala's `test("adds") { … }`: the outer call calls what
    // the inner one returned, a function of no name — the inner one is the
    // call of `f`. Read by its text, the outer one called the title (`adds`).
    if matches!(
        target.kind(),
        "call_expression" | "call" | "method_invocation"
    ) {
        return None;
    }
    // `std::move(x)` is the standard library, never a project function.
    if lang == Lang::Cpp
        && target.kind() == "qualified_identifier"
        && target
            .child_by_field_name("scope")
            .is_some_and(|s| node_text(s, src) == "std")
    {
        return None;
    }
    // A dotted access (`.`) is a method call; a `::` path is not. In Python/JS/Go
    // module access also uses `.`, so those count as "method-like" here — a safe
    // over-approximation (they still resolve via same-file / import scope).
    // Dart spells `obj.foo()` / `obj?.foo()` as a method invocation whose
    // function is an (un)conditional assignable selector.
    let method = matches!(
        target.kind(),
        "field_expression"
            | "attribute"
            | "member_expression"
            | "selector_expression"
            | "unconditional_assignable_selector"
            | "conditional_assignable_selector"
    );
    let name_node = match target.kind() {
        "field_expression" | "selector_expression" => target.child_by_field_name("field"),
        "member_expression" => target.child_by_field_name("property"),
        "attribute" => target.child_by_field_name("attribute"),
        _ => None,
    }
    .unwrap_or(target);
    let name_node = innermost_name(name_node);
    // Rust's raw identifier prefix is part of the spelling the outline uses.
    // Splitting `r#type` into identifier runs would discard `r#` and prevent
    // the call from matching its own definition, including qualified calls.
    let name =
        if lang == Lang::Rust && matches!(name_node.kind(), "identifier" | "field_identifier") {
            node_text(name_node, src).to_string()
        } else {
            last_identifier(node_text(name_node, src))?
        };
    Some((name, method))
}

/// A PHP name as written (`helper`), or the last part of a qualified one
/// (`App\Util\helper`); `None` for a variable or an expression.
fn php_name(node: Node, src: &str) -> Option<String> {
    match node.kind() {
        "name" => Some(node_text(node, src).to_string()),
        "qualified_name" => last_identifier(node_text(node, src)),
        _ => None,
    }
}

/// The identifier a C# name ends in: `F` of `F`, `F<T>`, `Ns.F` and
/// `global::F`; `None` for anything else (a predefined type, an array).
fn csharp_name(mut node: Node, src: &str) -> Option<String> {
    loop {
        node = match node.kind() {
            "identifier" => return Some(node_text(node, src).to_string()),
            "generic_name" => node.named_child(0)?,
            "qualified_name" | "alias_qualified_name" => node.child_by_field_name("name")?,
            _ => return None,
        };
    }
}

/// The last `[A-Za-z_][A-Za-z0-9_]*` run in `text`.
fn last_identifier(text: &str) -> Option<String> {
    let mut last: Option<String> = None;
    let mut cur = String::new();
    for ch in text.chars() {
        if ch == '_' || ch.is_alphanumeric() {
            cur.push(ch);
        } else if !cur.is_empty() {
            last = Some(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        last = Some(cur);
    }
    // A leading digit means it wasn't an identifier (e.g. a numeric literal).
    last.filter(|s| !s.chars().next().is_some_and(|c| c.is_ascii_digit()))
}

/// What the receiver of the method call `call` (calling `callee`) is known
/// to be ([`CallSite::receiver`]): looked for in the source of the calling
/// function before the call, from `from`. Only a type it is known to be
/// exactly: a type it is declared as may hold a subclass, whose override is
/// the method called — so `self`, `this` and a declared or annotated
/// receiver say nothing in a language with inheritance (Go and Rust have
/// none, and a Rust trait's own methods have no owner to narrow to).
#[allow(clippy::too_many_arguments)]
fn receiver_of(
    call: Node,
    callee: &str,
    scope: &Scope,
    src: &str,
    lang: Lang,
    bindings: &mut HashMap<(usize, String), Bindings>,
    fields: &mut HashMap<(usize, String), Option<String>>,
    unbound: &mut Option<String>,
) -> Option<Receiver> {
    // (A Kotlin infix call's is its left operand: `Users.id` of `Users.id
    // eq 1`.)
    let receiver = match call.kind() {
        "infix_expression" if lang == Lang::Kotlin => node_text(call.named_child(0)?, src).trim(),
        _ => receiver_text(node_text(call, src), callee)?.trim(),
    };
    // A null assertion leaves the value's type as it was: Dart's, C#'s and
    // TypeScript's `cart!.add()`, Kotlin's `cart!!.add()`.
    let receiver = match lang {
        Lang::Dart | Lang::CSharp | Lang::TypeScript | Lang::Tsx | Lang::Kotlin => {
            receiver.trim_end_matches('!').trim_end()
        }
        // Ruby's `::UserMailer`: the top level's `UserMailer`.
        Lang::Ruby => receiver.trim_start_matches("::"),
        _ => receiver,
    };
    // A Scala method eta-expanded (`(this.apply _).tupled`, `f _`): a
    // function value, whose methods are the library's.
    if lang == Lang::Scala
        && receiver
            .trim_start_matches('(')
            .trim_end_matches(')')
            .trim_end()
            .ends_with(" _")
    {
        return Some(Receiver::Declared("Function".to_string()));
    }
    // `super.f()`, C#'s `base.F()`: the base's method.
    if (receiver == "super" && matches!(lang, Lang::Java | Lang::Kotlin | Lang::Scala | Lang::Dart))
        || (receiver == "base" && lang == Lang::CSharp)
    {
        return Some(Receiver::Declared(SUPER.to_string()));
    }
    // Zig's `self` is a parameter like any, its type annotated.
    if lang != Lang::Zig
        && matches!(
            receiver,
            "self" | "this" | "$this" | "Self" | "cls" | "static"
        )
    {
        // The caller's class — its method, inherited or overridden, in a
        // language with inheritance (Go names its receiver).
        return (lang == Lang::Rust || (has_inheritance(lang) && lang != Lang::Go))
            .then_some(Receiver::Own);
    }
    // A field of the caller's class, as its class declares it:
    // `this.owners.findById(…)` with `private final OwnerRepository owners;`.
    if has_inheritance(lang)
        && lang != Lang::Go
        && let Some(field) = ["this.", "self.", "$this->", "this->"]
            .iter()
            .find_map(|p| receiver.strip_prefix(p))
    {
        if !is_plain_name(field) {
            return None;
        }
        return class_field_type(call, field, src, lang, scope.places, fields)
            .map(Receiver::Declared)
            .or_else(|| cpp_member_field(call, field, src, lang));
    }
    // A field of the type's own: `self.items.len()` is `Vec`'s `len`, as
    // its struct declares `items`.
    if lang == Lang::Rust
        && let Some(field) = receiver.strip_prefix("self.")
    {
        return is_plain_name(field)
            .then(|| rust_field_type(call, field, src))
            .flatten()
            .map(Receiver::Type);
    }
    // A Zig file's name for a file it imports (`const posix =
    // @import("posix.zig");`): its functions, whatever binds the name.
    if lang == Lang::Zig && is_plain_name(receiver) {
        let imported = fields
            .entry((usize::MAX, receiver.to_string()))
            .or_insert_with(|| {
                let head = format!("const {receiver} = @import(\"");
                // The file itself, not a declaration of it
                // (`@import("buffer.zig").Pool` names a type).
                src.lines()
                    .any(|line| {
                        let line = line.strip_prefix("pub ").unwrap_or(line);
                        line.starts_with(&head) && line.trim_end().ends_with("\");")
                    })
                    .then(String::new)
            });
        if imported.is_some() {
            *unbound = Some(receiver.to_string());
            return None;
        }
    }
    // A Play template's package (`views.html` in `views.html.index(…)`, or
    // the template itself in Java's `views.html.index.render(…)`).
    if matches!(lang, Lang::Java | Lang::Scala)
        && (template_path(receiver) || reverse_route(receiver))
    {
        *unbound = Some(receiver.to_string());
        return None;
    }
    // A Zig chain of fields: from a value of a known type (`res.headers`
    // with `res: *Response`, `self.pool`), or a file imported by name
    // (`t.arena` with `t = @import("t.zig")`) — the last field's type.
    if lang == Lang::Zig
        && let Some((root, path)) = receiver.split_once('.')
        && is_plain_name(root)
        && path.split('.').all(is_plain_name)
    {
        let head = format!("const {root} = @import(\"");
        let file = src.lines().find_map(|line| {
            let line = line.strip_prefix("pub ").unwrap_or(line);
            line.strip_prefix(&head)?
                .strip_suffix("\");")
                .filter(|p| p.ends_with(".zig"))
        });
        if let Some(file) = file {
            return Some(Receiver::Type(format!(
                "{THROUGH}@{file}|{}",
                path.replace('.', "|")
            )));
        }
        let places = scope.places;
        let said = bindings
            .entry((scope.from, root.to_string()))
            .or_insert_with(|| Bindings::from(scope.from, places))
            .said_before(call.start_byte(), root, scope, src, lang);
        if let Said::Type(ty, _) = &said
            && !ty.starts_with(RETURNED)
            && !ty.starts_with(THROUGH)
        {
            return Some(Receiver::Type(format!(
                "{THROUGH}{ty}|{}",
                path.replace('.', "|")
            )));
        }
    }
    // A Scala path from a name the function does not bind (`os.watch` in
    // `os.watch.watch(…)`, `os.ResourcePath`): a package's — its package
    // object's, or its object's — member, as `os.read(p)` is.
    if lang == Lang::Scala
        && let Some((root, path)) = receiver.split_once('.')
        && is_plain_name(root)
        && root.starts_with(char::is_lowercase)
        && path.split('.').all(is_plain_name)
    {
        let places = scope.places;
        let said = bindings
            .entry((scope.from, root.to_string()))
            .or_insert_with(|| Bindings::from(scope.from, places))
            .said_before(call.start_byte(), root, scope, src, lang);
        if said == Said::Nothing {
            *unbound = Some(receiver.to_string());
            return None;
        }
    }
    // A Go chain of fields from a value of a known type (`c.Request.Header`
    // with `c *Context`): the last field's type, once the structs are read.
    let indexed;
    let receiver = match (lang == Lang::Go)
        .then(|| go_indexed_path(receiver))
        .flatten()
    {
        Some(path) => {
            indexed = path;
            indexed.as_str()
        }
        None => receiver,
    };
    if lang == Lang::Go
        && let Some((root, path)) = receiver.split_once('.')
        && is_plain_name(root)
        && root.starts_with(char::is_lowercase)
        && path.split('.').all(|p| p == "[]" || is_plain_name(p))
    {
        let places = scope.places;
        let said = bindings
            .entry((scope.from, root.to_string()))
            .or_insert_with(|| Bindings::from(scope.from, places))
            .said_before(call.start_byte(), root, scope, src, lang);
        // (Of what a function returns, `w := PerformRequest(…)`: its
        // declared result's fields, `w.Body`.)
        if let Said::Type(ty, _) = &said
            && (!ty.starts_with(RETURNED) || !ty.contains(['@', '|']))
        {
            return Some(Receiver::Type(format!(
                "{THROUGH}{ty}|{}",
                path.replace('.', "|")
            )));
        }
        // A variable of an imported package (`binding.Form.Bind(…)`), or
        // one of this package's (`defaultLogger.out.Write(…)`).
        if said == Said::Nothing {
            let root = if go_imports(src, fields).iter().any(|i| i == root) {
                root
            } else if !path.contains('.') {
                return Some(Receiver::Type(format!("{THROUGH}.|{root}|{path}")));
            } else {
                "."
            };
            let path = if root == "." {
                receiver.to_string()
            } else {
                path.to_string()
            };
            return Some(Receiver::Type(format!(
                "{THROUGH}{root}|{}",
                path.replace('.', "|")
            )));
        }
    }
    if !is_plain_name(receiver) {
        // A chain on a value just built: `IO.pure(1).flatMap(…)`,
        // `new Cart().add(…)`, `Cart(items).total()`.
        if has_inheritance(lang) && lang != Lang::Go {
            if let Some(ty) = constructed_type(receiver, lang) {
                return Some(Receiver::Type(ty));
            }
            if lang == Lang::CSharp
                && let Some(ty) = service_type(receiver)
            {
                return Some(Receiver::Declared(ty));
            }
            // Scala's summoner (`Spawn[IO].background(fa)`): the instance of
            // the type class it names, as declared.
            if lang == Lang::Scala
                && let Some(ty) = scala_summoned(receiver)
            {
                return Some(Receiver::Declared(ty));
            }
            if let Some(ty) = factory_type(receiver, lang) {
                return Some(Receiver::Declared(factory_guess(receiver, &ty, lang)));
            }
        }
        // C++'s `fmt::format_int(42).str()`: what the call returns, or
        // the class it builds.
        if lang == Lang::Cpp
            && let Some(open) = receiver.find('(')
            && receiver.ends_with(')')
            && !receiver[..open].contains(['.', '-', ' ', '<'])
            && receiver[..open].split("::").all(is_plain_name)
        {
            let name = receiver[..open].rsplit("::").next().unwrap_or("");
            return Some(Receiver::Declared(format!("{RETURNED}{name}")));
        }
        // One link of a chain (`const Pipeline().addMiddleware(m)` before
        // `.addHandler(h)`, `repo.find(id)` before `.total()`): what that
        // method returns, on the type of what it is called on.
        if matches!(
            lang,
            Lang::Java
                | Lang::Kotlin
                | Lang::Scala
                | Lang::CSharp
                | Lang::Dart
                | Lang::Cpp
                | Lang::Go
        ) && let Some((base, method)) = chain_link(receiver)
        {
            let base_type = constructed_type(base, lang).or_else(|| {
                is_plain_name(base).then_some(())?;
                let places = scope.places;
                match bindings
                    .entry((scope.from, base.to_string()))
                    .or_insert_with(|| Bindings::from(scope.from, places))
                    .said_before(call.start_byte(), base, scope, src, lang)
                {
                    Said::Type(ty, _) if !ty.starts_with(RETURNED) && !ty.starts_with(THROUGH) => {
                        Some(ty)
                    }
                    _ => None,
                }
            });
            if let Some(ty) = base_type {
                let marker = format!("{RETURNED}{ty}#{method}");
                // (Go has no subtypes: what a method returns is exact.)
                return Some(match lang {
                    Lang::Go => Receiver::Type(marker),
                    _ => Receiver::Declared(marker),
                });
            }
            // Go's method of a field (`c.Writer.Header()`): what it returns,
            // through the chain of fields to it.
            if lang == Lang::Go
                && let Some((root, path)) = base.split_once('.')
                && is_plain_name(root)
                && path.split('.').all(is_plain_name)
            {
                let places = scope.places;
                if let Said::Type(ty, _) = bindings
                    .entry((scope.from, root.to_string()))
                    .or_insert_with(|| Bindings::from(scope.from, places))
                    .said_before(call.start_byte(), root, scope, src, lang)
                    && !ty.starts_with(RETURNED)
                    && !ty.starts_with(THROUGH)
                {
                    return Some(Receiver::Type(format!(
                        "{THROUGH}{ty}|{}|{method}()",
                        path.replace('.', "|")
                    )));
                }
            }
        }
        return None;
    }
    let (from, now) = said_around(receiver, call.start_byte(), scope, src, lang, bindings);
    match now {
        // Zig's `self: *Self` (`const Self = @This();`): the container
        // the function is in.
        Said::Type(ty, _) if lang == Lang::Zig && ty == "Self" => return Some(Receiver::Own),
        // A method's result on a value (`val cell = cells.get(k)`): that
        // value's type's method, where its binding says the type; else the
        // method by its name alone.
        Said::Type(ty, _) if ty.starts_with(RETURNED) && ty[RETURNED.len()..].starts_with('@') => {
            let (value, method) = ty[RETURNED.len() + 1..].split_once('#')?;
            let places = scope.places;
            let said = bindings
                .entry((from, value.to_string()))
                .or_insert_with(|| Bindings::from(from, places))
                .said_before(call.start_byte(), value, &Scope { places, from }, src, lang);
            let marker = match said {
                Said::Type(owner, _)
                    if !owner.starts_with(RETURNED) && !owner.starts_with(THROUGH) =>
                {
                    format!("{RETURNED}{owner}#{method}")
                }
                _ => format!("{RETURNED}{method}"),
            };
            return Some(if matches!(lang, Lang::Go | Lang::Rust | Lang::Zig) {
                Receiver::Type(marker)
            } else {
                Receiver::Declared(marker)
            });
        }
        Said::Type(ty, exact) => {
            // Go, Rust and Zig have no subtypes: a declared type is exact.
            if exact || matches!(lang, Lang::Go | Lang::Rust | Lang::Zig) {
                return Some(Receiver::Type(ty));
            }
            // Declared as the type: it or a subtype.
            return has_inheritance(lang).then_some(Receiver::Declared(ty));
        }
        Said::Unknown => return None,
        Said::Nothing => {}
    }
    // Not bound in the function: a field of its class, used bare
    // (`owners.findById(…)` in Java, Kotlin, C#, Dart, C++).
    // (C# names its properties and static fields in Pascal case, as its
    // types: `DbResetter.DisposeAsync()` is the property's.)
    if implicit_self(lang)
        && (lang == Lang::CSharp || !receiver.starts_with(|c: char| c.is_uppercase()))
        && let Some(ty) = class_field_type(call, receiver, src, lang, scope.places, fields)
    {
        return Some(Receiver::Declared(ty));
    }
    // A C++ member function defined apart from its class (`void
    // pattern_formatter::handle_flag_(…)`): the class's field, as its
    // declaration in the header says — once the classes are read.
    if let Some(field) = cpp_member_field(call, receiver, src, lang) {
        return Some(field);
    }
    // Nor a field: in Scala, a package or an object (`os` in `os.read(p)`),
    // or a member inherited from a trait; in Go, an imported package
    // (`require.Error(t, err)`) or a package-level variable.
    // In Zig, a file imported by name (`posix` for `@import("posix.zig")`).
    if matches!(lang, Lang::Scala | Lang::Go | Lang::Zig)
        && !receiver.starts_with(|c: char| c.is_uppercase())
    {
        *unbound = Some(receiver.to_string());
    }
    // A Dart import's prefix (`http` in `import 'package:http/http.dart' as
    // http;`): that library's, by its URI.
    if lang == Lang::Dart
        && let Some(uri) = dart_import_prefix(src, receiver)
    {
        *unbound = Some(format!("import:{uri}"));
        return None;
    }
    // A Go name no import gives: a variable of the package (`Form` with
    // `var Form Binding = formBinding{}` in another of its files), of the
    // type its declaration says, once the package's files are read.
    if lang == Lang::Go && !go_imports(src, fields).iter().any(|i| i == receiver) {
        return Some(Receiver::Type(format!("{THROUGH}.|{receiver}")));
    }
    // A JavaScript runtime's own object, not bound here (`console`,
    // `document`, `localStorage`): its methods are the runtime's.
    if matches!(lang, Lang::JavaScript | Lang::TypeScript | Lang::Tsx)
        && JS_GLOBALS.contains(&receiver)
    {
        return Some(Receiver::Type(receiver.to_string()));
    }
    // A type as the receiver: its own (static) method. Not in Go, where an
    // exported name is as likely a variable.
    (lang != Lang::Go && receiver.starts_with(|c: char| c.is_uppercase()))
        .then(|| Receiver::Type(receiver.to_string()))
}

/// The type the class around `call` declares its field `field` as —
/// `private final OwnerRepository owners;`, `private val owners:
/// OwnerRepository` (a Kotlin or Scala constructor's property too),
/// `owners: OwnerRepository;`, TypeScript's `constructor(private repo:
/// Repo)` — read once per class and field.
fn class_field_type(
    call: Node,
    field: &str,
    src: &str,
    lang: Lang,
    places: &Places,
    fields: &mut HashMap<(usize, String), Option<String>>,
) -> Option<String> {
    let mut class = call.parent();
    while let Some(n) = class {
        if CLASS_KINDS.contains(&n.kind()) {
            break;
        }
        class = n.parent();
    }
    let class = class?;
    let key = (class.start_byte(), field.to_string());
    if let Some(known) = fields.get(&key) {
        return known.clone();
    }
    let (start, end) = (class.start_byte(), class.end_byte());
    let text = src.get(start..end)?;
    let word = |c: char| c == '_' || c.is_alphanumeric();
    let mut found = None;
    let mut at = 0;
    while let Some(i) = text[at..].find(field) {
        let begin = at + i;
        let stop = begin + field.len();
        at = stop;
        let head = &text[..begin];
        if head.ends_with(|c: char| word(c) || matches!(c, '.' | '$' | '@' | '>'))
            || text[stop..].starts_with(word)
            || places.opaque(start + begin)
        {
            continue;
        }
        // A method's own name is no field; TypeScript's constructor
        // parameter is one when an access word makes it a property.
        if places.in_fn(start + begin).is_some() {
            let property = ["private", "public", "protected", "readonly"]
                .iter()
                .any(|w| head.trim_end().ends_with(w));
            if !property {
                continue;
            }
        }
        if let Binding::Typed(ty, _) = binding_at(head, &text[stop..], lang) {
            found = Some(ty);
            break;
        }
    }
    fields.insert(key, found.clone());
    found
}

/// The objects a JavaScript runtime gives every script — the language's,
/// the browser's, Node's and Bun's: `console.log`, `JSON.parse`,
/// `document.querySelector` are theirs, never a project's `log`, `parse`
/// or `querySelector`.
const JS_GLOBALS: &[&str] = &[
    "console",
    "Math",
    "JSON",
    "Object",
    "Array",
    "Promise",
    "Number",
    "String",
    "Boolean",
    "Date",
    "Reflect",
    "Intl",
    "Symbol",
    "Atomics",
    "BigInt",
    "Error",
    "RegExp",
    "Map",
    "Set",
    "WeakMap",
    "WeakSet",
    "ArrayBuffer",
    "Proxy",
    "globalThis",
    "window",
    "document",
    "navigator",
    "location",
    "history",
    "localStorage",
    "sessionStorage",
    "indexedDB",
    "crypto",
    "performance",
    "screen",
    "customElements",
    "process",
    "Buffer",
    "require",
    "module",
    "exports",
    "Bun",
    "Deno",
    "URL",
    "URLSearchParams",
    "Response",
    "Request",
    "Headers",
    "Notification",
    "Worker",
    "self",
];

/// The receiver expression of a call written `receiver<sep>name(…)`: what
/// precedes the first `.name`, `->name`, `?.name` or `::name` standing as a
/// whole name in `call`'s text — but one the path goes on past (`os.watch`
/// in `os.watch.watch(…)`). `None` for an empty name (a call tree-sitter
/// recovered from half-typed code), which every position would match.
fn receiver_text<'a>(call: &'a str, name: &str) -> Option<&'a str> {
    if name.is_empty() {
        return None;
    }
    let word = |c: char| c == '_' || c.is_alphanumeric();
    const SEPARATORS: [&str; 5] = ["?->", "->", "?.", "::", "."];
    let mut at = 0;
    let mut first = None;
    while let Some(found) = call[at..].find(name) {
        let start = at + found;
        let end = start + name.len();
        let whole = !call[end..].starts_with(word);
        let head = &call[..start];
        let sep = SEPARATORS.into_iter().find(|sep| head.ends_with(sep));
        if whole && let Some(sep) = sep {
            let receiver = &head[..head.len() - sep.len()];
            // A member of the same name that the path goes on past
            // (`os.watch` in `os.watch.watch(…)`) is not the one called.
            let rest = &call[end..];
            if !SEPARATORS.iter().any(|sep| rest.starts_with(sep)) {
                return Some(receiver);
            }
            first.get_or_insert(receiver);
        }
        at = end;
    }
    first
}

/// A name standing alone — `cart`, `$cart`, `@cart` — not an expression.
fn is_plain_name(text: &str) -> bool {
    let text = text.strip_prefix(['$', '@']).unwrap_or(text);
    text.starts_with(|c: char| c == '_' || c.is_alphabetic())
        && text.chars().all(|c| c == '_' || c.is_alphanumeric())
}

/// The function a call is made in: where it starts, and the file's
/// [`Places`], which tell the functions nested in it.
struct Scope<'p> {
    places: &'p Places,
    from: usize,
}

/// Where a file's functions are, nested ones too, and the text no name is
/// bound in — comments and strings — by byte range, read once per file:
/// telling an occurrence's place by walking the tree down to it and back
/// up, for every occurrence, made a long function's reading quadratic.
struct Places {
    /// Each function's start and end, by start, with the index of the one
    /// around it.
    fns: Vec<(usize, usize, Option<usize>)>,
    /// Comments and strings, none inside another, by start.
    opaque: Vec<(usize, usize)>,
}

impl Places {
    fn read(root: Node, fn_kinds: &[&str]) -> Places {
        let mut fns = Vec::new();
        let mut opaque = Vec::new();
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            let kind = node.kind();
            if kind.contains("comment") || kind.contains("string") {
                opaque.push((node.start_byte(), node.end_byte()));
                continue;
            }
            if is_place(node, fn_kinds) {
                fns.push((node.start_byte(), node.end_byte()));
            }
            let mut cursor = node.walk();
            stack.extend(node.children(&mut cursor));
        }
        fns.sort_unstable();
        opaque.sort_unstable();
        // Each one's parent: the nearest before it still open at its start.
        let mut open: Vec<usize> = Vec::new();
        let fns = fns
            .iter()
            .enumerate()
            .map(|(i, &(start, end))| {
                while open.last().is_some_and(|&o| fns[o].1 <= start) {
                    open.pop();
                }
                let parent = open.last().copied();
                open.push(i);
                (start, end, parent)
            })
            .collect();
        Places { fns, opaque }
    }

    /// Whether `at` is in a comment or a string.
    fn opaque(&self, at: usize) -> bool {
        let i = self.opaque.partition_point(|&(start, _)| start <= at);
        i > 0 && self.opaque[i - 1].1 > at
    }

    /// The innermost function `at` is in: where it starts and ends.
    fn in_fn(&self, at: usize) -> Option<(usize, usize)> {
        let mut i = self
            .fns
            .partition_point(|&(start, _, _)| start <= at)
            .checked_sub(1)?;
        loop {
            let (start, end, parent) = self.fns[i];
            if start <= at && at < end {
                return Some((start, end));
            }
            i = parent?;
        }
    }

    /// The function around the one that starts at `from`: where it starts
    /// and ends.
    fn around(&self, from: usize) -> Option<(usize, usize)> {
        let i = self.fns.partition_point(|&(start, _, _)| start < from);
        let (start, _, parent) = *self.fns.get(i)?;
        if start != from {
            return None;
        }
        parent.map(|p| (self.fns[p].0, self.fns[p].1))
    }
}

/// One name's bindings in one function, read once as far as its calls
/// reach: what they say at each of them, so a call is told what the
/// bindings before it say, whichever of the function's calls is read first.
pub(crate) struct Bindings {
    /// Read up to here (a byte offset in the file).
    upto: usize,
    /// Where the function starts: a declaration's type is looked for back
    /// to here.
    from: usize,
    /// At each binding (where it takes effect: its statement's end), what
    /// the bindings up to it say.
    said: Vec<(usize, Said)>,
    /// The next of the file's functions ([`Places::fns`]) not yet come to,
    /// and those come to and still open — the functions nested in this
    /// one around what is being read.
    next_fn: usize,
    open: Vec<usize>,
}

/// What a name's bindings say ([`Bindings::said_before`]).
#[derive(Debug, Clone, PartialEq)]
enum Said {
    /// Bound to this type — exactly (built), or as declared.
    Type(String, bool),
    /// Bound to something that cannot be told.
    Unknown,
    /// Not bound in the function (a parameter with no type, a global).
    Nothing,
}

impl Bindings {
    fn from(from: usize, places: &Places) -> Self {
        Self {
            upto: from,
            from,
            said: Vec::new(),
            next_fn: places.fns.partition_point(|&(start, _, _)| start <= from),
            open: Vec::new(),
        }
    }

    /// The function nested in this one that `at` is in, if any — `at` never
    /// less than the last asked about.
    fn nested_at(&mut self, at: usize, places: &Places) -> Option<(usize, usize)> {
        while let Some(&(start, _, _)) = places.fns.get(self.next_fn)
            && start <= at
        {
            self.open.push(self.next_fn);
            self.next_fn += 1;
        }
        while self.open.last().is_some_and(|&o| places.fns[o].1 <= at) {
            self.open.pop();
        }
        self.open
            .last()
            .map(|&o| (places.fns[o].0, places.fns[o].1))
    }

    /// What `name`'s bindings before `to` in `src` say.
    fn said_before(&mut self, to: usize, name: &str, scope: &Scope, src: &str, lang: Lang) -> Said {
        self.read_to(to, name, scope, src, lang);
        let before = self.said.partition_point(|&(at, _)| at < to);
        match before.checked_sub(1) {
            Some(last) => self.said[last].1.clone(),
            None => Said::Nothing,
        }
    }

    /// Read `name`'s occurrences up to `to` in `src`, once each.
    fn read_to(&mut self, to: usize, name: &str, scope: &Scope, src: &str, lang: Lang) {
        let settled = matches!(self.said.last(), Some((_, Said::Unknown)));
        if to <= self.upto || settled {
            self.upto = self.upto.max(to);
            return;
        }
        let Some(text) = src.get(self.from..to) else {
            return;
        };
        let word = |c: char| c == '_' || c.is_alphanumeric();
        let mut at = self.upto - self.from;
        while let Some(i) = text[at..].find(name) {
            let start = at + i;
            let end = start + name.len();
            at = end;
            let head = &text[..start];
            if head.ends_with(|c: char| word(c) || matches!(c, '.' | '$' | '@' | '>' | ':'))
                || text[end..].starts_with(word)
                || scope.places.opaque(self.from + start)
            {
                continue; // part of a longer name, a member, a comment or a string
            }
            // What follows it in the file, past what is read so far: a
            // statement read up to a call in it goes on after the call.
            let after = &src[self.from + end..];
            let binding = binding_at(head, after, lang);
            if binding == Binding::Not {
                continue;
            }
            // In a statically typed language a name assigned, not declared,
            // keeps the type it was declared with: a value of a type not
            // known exactly — a call's result, what cannot be told — says
            // nothing new of it (`_subscription = stream.listen(…)` with
            // `StreamSubscription<int>? _subscription;`). One built there
            // says which of its type it is (`observer = MockBlocObserver()`).
            if statically_typed(lang)
                && !matches!(binding, Binding::Typed(_, true))
                && plainly_assigned(head, after)
            {
                continue;
            }
            let now = if let Some((nested, nested_end)) =
                self.nested_at(self.from + start, scope.places)
            {
                // In a function nested in this one: a name it declares is its
                // own; one it assigns is this one's, given a value when it
                // runs, which cannot be told. Python's assignment declares,
                // but where it says the name is not its own.
                let said_not_own = |kw: &str| {
                    src[nested..nested_end]
                        .split(kw)
                        .skip(1)
                        .any(|rest| rest.trim_start().starts_with(name))
                };
                let own = if lang == Lang::Python {
                    !(said_not_own("nonlocal ") || said_not_own("global "))
                } else {
                    declares_its_own(head, after, lang)
                };
                if own {
                    continue;
                }
                // In a statically typed language the value it is given is of
                // the name's type still, or one below it: of the type it is
                // built as (`observer = MockBlocObserver()` in `setUp(() {
                // … })`, after `late BlocObserver observer;`), or else of
                // the type said before.
                match (
                    statically_typed(lang),
                    &binding,
                    self.said.last().map(|(_, s)| s),
                ) {
                    (true, Binding::Typed(ty, _), _) => Said::Type(ty.clone(), false),
                    (true, _, Some(Said::Type(seen, _))) => Said::Type(seen.clone(), false),
                    _ => Said::Unknown,
                }
            } else {
                match binding {
                    Binding::Not => continue,
                    Binding::Unknown => Said::Unknown,
                    Binding::Typed(ty, exact) => match self.said.last().map(|(_, s)| s) {
                        Some(Said::Type(seen, _)) if *seen != ty => Said::Unknown,
                        // Built and declared alike: as exact as either says.
                        Some(Said::Type(_, was)) => Said::Type(ty, exact || *was),
                        _ => Said::Type(ty, exact),
                    },
                }
            };
            let unknown = now == Said::Unknown;
            // An assignment is in effect once its statement is over: `item`
            // in `item = Wrapper(item.unwrap())` is still the one before. A
            // parameter, a loop's name, a declaration is at once: `cart` in
            // `cart => cart.total()` is the closure's.
            let value = after.trim_start_matches([' ', '\t']);
            let assigned = value.starts_with(":=")
                || (value.starts_with('=') && !value.starts_with("==") && !value.starts_with("=>"));
            let in_effect = if assigned {
                after.find([';', '\n']).unwrap_or(after.len())
            } else {
                0
            };
            let effect =
                (self.from + end + in_effect).max(self.said.last().map_or(0, |&(at, _)| at));
            self.said.push((effect, now));
            if unknown {
                break;
            }
        }
        self.upto = to;
    }
}

/// What an occurrence of a name says it is bound to.
#[derive(Debug, PartialEq)]
enum Binding {
    /// It is not bound here: a use of it.
    Not,
    /// Bound to this type: exactly (`true`, built by a constructor), or as
    /// declared or annotated (`false`).
    Typed(String, bool),
    /// Bound to what cannot be told: a function's result, a loop's or a
    /// closure's parameter, one of several unpacked.
    Unknown,
}

/// What an occurrence of a name, with `head` before it in its function and
/// `after` after it, says it is bound to: built by a constructor (`name =
/// Cart()`, `name := &Cart{}`, `let name = Cart::new()`, `name = new
/// Cart()`, `name = Cart.new`), declared with it (`Cart name = …`, `Cart*
/// name;`, `(Cart name)`), or annotated with it (`name: Cart`, Go's `name
/// *Cart`) — or bound to what cannot be told.
fn binding_at(head: &str, after: &str, lang: Lang) -> Binding {
    let after = after.trim_start_matches([' ', '\t']);
    let before = head.trim_end();
    let ends_with_word = |w: &str| {
        before.ends_with(w)
            && !before[..before.len() - w.len()]
                .ends_with(|c: char| c == '_' || c.is_alphanumeric())
    };
    // In parentheses (or brackets) opened on its line: a call's argument,
    // keyword or not, binds nothing of the caller's (`show(c=Cart())`); a
    // parameter's default (`def f(c=Cart())`) is what the caller may well
    // pass otherwise; one of a pattern's names (`let (c, d) = …`, `if let
    // Some(c) = …`, `for (i, c) in …`) is bound to what cannot be told.
    let line = &head[head.rfind('\n').map_or(0, |i| i + 1)..];
    // The innermost bracket left open on the line: in `(` or `[`, an
    // argument or a pattern; in a `{` — a block — or none, a statement.
    let mut open: Vec<char> = Vec::new();
    for c in line.chars() {
        match c {
            '(' | '[' | '{' => open.push(c),
            ')' | ']' | '}' => {
                open.pop();
            }
            _ => {}
        }
    }
    let depth = i32::from(matches!(open.last(), Some('(' | '[')));
    let line_rest = &after[..after.find('\n').unwrap_or(after.len())];
    // Go's `c, err := NewCart()`: the first name is the function's first
    // result, of the type it declares first.
    if lang == Lang::Go
        && depth == 0
        && after.starts_with(',')
        && !before.ends_with(',')
        && let Some(at) = line_rest.find(":=")
    {
        return match returned_call(&line_rest[at + 2..], lang) {
            Some(function) => Binding::Typed(format!("{RETURNED}{function}"), false),
            None => Binding::Unknown,
        };
    }
    let assigns = |text: &str| {
        let bytes = text.as_bytes();
        bytes.iter().enumerate().any(|(i, &b)| {
            b == b'='
                && !matches!(bytes.get(i + 1), Some(b'=' | b'>'))
                && !matches!(
                    i.checked_sub(1).map(|j| bytes[j]),
                    Some(b'=' | b'!' | b'<' | b'>' | b':')
                )
        })
    };
    let keyword_line = {
        let line = line.trim_start();
        [
            "def ",
            "fn ",
            "function ",
            "func ",
            "async def ",
            "pub fn ",
            "lambda",
        ]
        .iter()
        .any(|k| line.starts_with(k) || line.contains(&format!(" {k}")))
    };
    let pattern_line = {
        let line = line.trim_start();
        [
            "let ",
            "if let ",
            "while let ",
            "for ",
            "const ",
            "var ",
            "val ",
            "match ",
        ]
        .iter()
        .any(|k| line.starts_with(k) || line.contains(&format!(" {k}")))
    };
    if depth > 0 {
        if after.starts_with('=') && !after.starts_with("==") && !after.starts_with("=>") {
            return if keyword_line || pattern_line {
                Binding::Unknown
            } else {
                Binding::Not
            };
        }
        if pattern_line
            && (after.starts_with([',', ')', ']', '}']))
            && (assigns(line_rest) || line_rest.contains(" in "))
        {
            return Binding::Unknown;
        }
    } else if after.starts_with(',') && assigns(line_rest) {
        // `c, rest = split()`: the first of several unpacked.
        return Binding::Unknown;
    }
    // `for i, c in …`: a loop's names after the first.
    if (before.ends_with(',') || before.ends_with('('))
        && after.trim_start_matches([')', ' ']).starts_with("in ")
    {
        return Binding::Unknown;
    }
    // A loop's, a closure's, a `with`'s or an `except`'s name, or one of
    // several unpacked: bound to what this cannot tell.
    if after.starts_with("=>")
        || before.ends_with('|')
        || (after.starts_with('|') && !after.starts_with("||"))
        || ["for", "lambda", "as", "foreach"]
            .iter()
            .any(|w| ends_with_word(w))
        || (["const", "let", "var", "val"]
            .iter()
            .any(|w| ends_with_word(w))
            && (after.starts_with("of ") || after.starts_with("in ")))
        || (depth <= 0
            && before.ends_with(',')
            && after.starts_with('=')
            && !after.starts_with("=="))
    {
        return Binding::Unknown;
    }
    let assigned = if let Some(rest) = after.strip_prefix(":=") {
        Some(rest)
    } else if after.starts_with('=') && !after.starts_with("==") {
        Some(&after[1..])
    } else {
        None
    };
    // A Zig value the standard library makes (`std.ArrayList(T).init(a)`,
    // `try std.json.parseFromSlice(…)`): its methods are the library's.
    if lang == Lang::Zig
        && let Some(rhs) = assigned
        && rhs
            .trim_start()
            .trim_start_matches("try ")
            .trim_start()
            .starts_with("std.")
    {
        return Binding::Typed("std".to_string(), true);
    }
    // A Zig struct's `init` makes one of it (`var p = try Params.init(a, 5)`).
    if lang == Lang::Zig
        && let Some(rhs) = assigned
        && let Some(ty) = rhs
            .trim_start()
            .trim_start_matches("try ")
            .trim_start()
            .split_once(".init(")
            .map(|(ty, _)| ty.trim())
            .filter(|ty| {
                ty.rsplit('.')
                    .next()
                    .is_some_and(|t| t.starts_with(char::is_uppercase))
            })
            .filter(|ty| {
                ty.chars()
                    .all(|c| c == '_' || c == '.' || c.is_alphanumeric())
            })
    {
        return Binding::Typed(ty.to_string(), true);
    }
    // A Go name given another's value (`b := Form`): a package variable's
    // type, once the package is read.
    if lang == Lang::Go
        && let Some(rhs) = assigned
    {
        let expr = rhs.trim_start();
        let end = expr.find(['\n', ';', ')', ',', '}']).unwrap_or(expr.len());
        let name = expr[..end].trim();
        if is_plain_name(name) && !matches!(name, "nil" | "true" | "false" | "iota") {
            return Binding::Typed(format!("{THROUGH}.|{name}"), true);
        }
    }
    // A Go value another package's function returns (`w :=
    // httptest.NewRecorder()`): named by that function, which the graph
    // tells a library's from the project's by the file's imports.
    if lang == Lang::Go
        && let Some(rhs) = assigned
        && let Some((pkg, rest)) = rhs.trim_start().trim_start_matches('&').split_once('.')
        && pkg.starts_with(char::is_lowercase)
        && pkg.chars().all(|c| c == '_' || c.is_alphanumeric())
        && rest.starts_with(char::is_uppercase)
    {
        let name: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        return Binding::Typed(format!("{pkg}.{name}"), true);
    }
    // .NET's service provider hands out the `T` it is asked for:
    // `sp.GetRequiredService<ApplicationDbContextInitialiser>()`.
    if lang == Lang::CSharp
        && let Some(rhs) = assigned
        && let Some(ty) = service_type(rhs)
    {
        return Binding::Typed(ty, false);
    }
    // `std::make_shared<logger>(…)`, `std::make_unique<ns::Cart>(…)`: a
    // smart pointer to a new `T`, whose methods its `->` calls.
    if lang == Lang::Cpp
        && let Some(rhs) = assigned
        && let Some(ty) = cpp_made_type(rhs)
    {
        return Binding::Typed(ty, true);
    }
    if let Some(rhs) = assigned {
        return match constructed_type(rhs, lang) {
            Some(ty) => Binding::Typed(ty, true),
            // A declaration with a value of no known type still says its
            // own: `Cart cart = make();`.
            None => match declared_type(head, lang) {
                Some(ty) if declares(head, lang) => Binding::Typed(ty, false),
                _ => match factory_type(rhs, lang) {
                    Some(ty) => Binding::Typed(factory_guess(rhs, &ty, lang), false),
                    // A function's result: the type it declares, once the
                    // project is read.
                    None => match returned_call(rhs, lang) {
                        Some(function) => Binding::Typed(format!("{RETURNED}{function}"), false),
                        None => Binding::Unknown,
                    },
                },
            },
        };
    }
    if let Some(rest) = after.strip_prefix(':').filter(|r| !r.starts_with(':')) {
        // An annotation; a Rust trait object or `impl Trait` names no type
        // its methods are owned by.
        let text = rest.trim_start().trim_start_matches(['&', '*']);
        let text = text.strip_prefix("mut ").unwrap_or(text).trim_start();
        if text.starts_with("dyn ") || text.starts_with("impl ") {
            return Binding::Unknown;
        }
        // Zig's type is a path, the standard library's whole
        // (`*std.Io.Writer`), a project's by its struct (`*Analyser`).
        if lang == Lang::Zig {
            return match zig_type_name(rest) {
                Some(ty) => Binding::Typed(ty, false),
                None => Binding::Unknown,
            };
        }
        // Rust's own types are named in lower case (`&str`, `u8`), and a
        // path names its last segment.
        if lang == Lang::Rust {
            return match rust_type_name(rest) {
                Some(ty) => Binding::Typed(ty, false),
                None => Binding::Unknown,
            };
        }
        return match leading_type(rest, lang) {
            Some(ty) => Binding::Typed(ty, false),
            None => Binding::Not,
        };
    }
    // A declaration names the type before the name: `Cart cart;`.
    if after.starts_with([';', ',', ')'])
        && declares(head, lang)
        && let Some(ty) = declared_type(head, lang)
    {
        return Binding::Typed(ty, false);
    }
    // A C# property: `Resetter DbResetter { get; set; }`, `Cart Current =>
    // …;`.
    if lang == Lang::CSharp
        && (after.starts_with('{') || after.starts_with("=>"))
        && declares(head, lang)
        && let Some(ty) = declared_type(head, lang)
    {
        return Binding::Typed(ty, false);
    }
    // C++ builds a value where it declares it: `Formatter f("%+");`,
    // `std::vector<int> v{1, 2};` — exactly that type.
    if lang == Lang::Cpp
        && after.starts_with(['(', '{'])
        && let Some(ty) = declared_type(head, lang)
    {
        return Binding::Typed(ty, true);
    }
    // Go writes the type after the name: `(c *Cart)`, `var c Cart`, and
    // another package's by its package (`w *httptest.ResponseRecorder`,
    // whose methods are that package's).
    if lang == Lang::Go
        && (before.ends_with(['(', ',']) || ends_with_word("var"))
        && let Some(ty) = go_type_path(after).or_else(|| leading_type(after, lang))
    {
        return Binding::Typed(ty, false);
    }
    Binding::Not
}

/// The package-qualified type a Go declaration names after a name — `
/// *httptest.ResponseRecorder)` is `httptest.ResponseRecorder` — or `None`
/// for a type of the file's own package, a slice, a map or a function.
fn go_type_path(after: &str) -> Option<String> {
    let t = after.trim_start().trim_start_matches('*');
    let end = t
        .find(|c: char| !(c == '_' || c == '.' || c.is_alphanumeric()))
        .unwrap_or(t.len());
    let (pkg, name) = t[..end].split_once('.')?;
    (pkg.starts_with(char::is_lowercase)
        && is_plain_name(pkg)
        && name.starts_with(char::is_uppercase)
        && is_plain_name(name))
    .then(|| format!("{pkg}.{name}"))
}

/// Whether a name's occurrence is a plain assignment's target: the
/// statement starts with it (or `this.`/`self.` and it) and `=` follows —
/// no declaration (`final x =`, `auto& x =`, `val x =`), comparison or arrow.
fn plainly_assigned(head: &str, after: &str) -> bool {
    let value = after.trim_start_matches([' ', '\t']);
    if !value.starts_with('=') || value.starts_with("==") || value.starts_with("=>") {
        return false;
    }
    let line = &head[head.rfind('\n').map_or(0, |i| i + 1)..];
    let before = line.trim_end();
    let before = before
        .strip_suffix("this.")
        .or_else(|| before.strip_suffix("self."))
        .unwrap_or(before)
        .trim_end();
    before.is_empty() || before.ends_with(['{', '}', ';', ')']) || before.ends_with("else")
}

/// Whether the occurrence between `head` and `after` declares a name of
/// its own — `let`, `const`, `var`, `:=`, a declared type, a parameter of
/// a closure or a loop — rather than assigning one from around it.
fn declares_its_own(head: &str, after: &str, lang: Lang) -> bool {
    let before = head.trim_end();
    let after = after.trim_start_matches([' ', '\t']);
    let ends_with_word = |w: &str| {
        before.ends_with(w)
            && !before[..before.len() - w.len()]
                .ends_with(|c: char| c == '_' || c.is_alphanumeric())
    };
    [
        "let", "const", "var", "val", "my", "local", "auto", "for", "foreach", "as", "lambda",
    ]
    .iter()
    .any(|w| ends_with_word(w))
        || after.starts_with(":=")
        || after.starts_with("=>")
        || before.ends_with('|')
        || (after.starts_with('|') && !after.starts_with("||"))
        || declares(head, lang)
}

/// Whether `head` ends with a declaration's type, as a C-like language
/// writes one before the name (`Cart`, `final Cart`, `List<Cart>`).
fn declares(head: &str, lang: Lang) -> bool {
    !matches!(lang, Lang::Python | Lang::Ruby | Lang::Go | Lang::Rust)
        && declared_type(head, lang).is_some()
}

/// The type a declaration names before its name ([`trailing_type`]) — in
/// C++ also one in lower case where it is a namespace's or a template's
/// (`std::vector<int>`, `spdlog::pattern_formatter`), as the standard
/// library and many projects name their types.
fn declared_type(head: &str, lang: Lang) -> Option<String> {
    trailing_type(head).or_else(|| match lang {
        Lang::Cpp => cpp_trailing_type(head).or_else(|| built_in_type(head, lang)),
        Lang::Java | Lang::CSharp => built_in_type(head, lang),
        _ => None,
    })
}

/// A Java, C# or C++ built-in type before a declared name, which none
/// writes as a class: `int count`, C#'s `string name`, C++'s `unsigned n`.
fn built_in_type(head: &str, lang: Lang) -> Option<String> {
    let head = head.trim_end();
    // (A C++ reference is of its type; a pointer is none of these.)
    let head = match lang {
        Lang::Cpp => head.trim_end_matches('&').trim_end(),
        _ => head,
    };
    let start = head
        .rfind(|c: char| !(c == '_' || c.is_alphanumeric()))
        .map_or(0, |i| i + 1);
    let word = &head[start..];
    let built_in: &[&str] = match lang {
        Lang::Java => &[
            "int", "long", "short", "byte", "char", "boolean", "float", "double",
        ],
        Lang::Cpp => &[
            "int",
            "long",
            "short",
            "char",
            "bool",
            "float",
            "double",
            "unsigned",
            "signed",
            "size_t",
            "ssize_t",
            "ptrdiff_t",
            "int8_t",
            "int16_t",
            "int32_t",
            "int64_t",
            "uint8_t",
            "uint16_t",
            "uint32_t",
            "uint64_t",
            "wchar_t",
            "char8_t",
            "char16_t",
            "char32_t",
        ],
        _ => &[
            "int", "long", "short", "byte", "sbyte", "uint", "ulong", "ushort", "char", "bool",
            "float", "double", "decimal", "string",
        ],
    };
    built_in.contains(&word).then(|| word.to_string())
}

/// A C++ type before a declared name, written in lower case: qualified by
/// a namespace or given template arguments — `std::vector<basic_format_arg<
/// Context>>` (`vector`), `const fmt::memory_buffer&` (`memory_buffer`) —
/// or a plain name where a declaration starts and written as one (`logger
/// &l`, `(sink *s,`), no keyword or built-in type.
fn cpp_trailing_type(head: &str) -> Option<String> {
    let original = head;
    let mut head = head.trim_end().trim_end_matches(['*', '&', ' ']);
    let templated = head.ends_with('>');
    if templated {
        let mut depth = 0usize;
        let mut cut = None;
        for (i, c) in head.char_indices().rev() {
            match c {
                '>' => depth += 1,
                '<' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        cut = Some(i);
                        break;
                    }
                }
                _ => {}
            }
        }
        head = head[..cut?].trim_end();
    }
    let start = head
        .char_indices()
        .rev()
        .find(|&(_, c)| !(c == '_' || c == ':' || c.is_alphanumeric()))
        .map_or(0, |(i, c)| i + c.len_utf8());
    let qualified = head[start..].trim_start_matches(':');
    let last = qualified.rsplit("::").next()?;
    let named = last.starts_with(|c: char| c == '_' || c.is_alphabetic())
        && !matches!(
            last,
            "return"
                | "delete"
                | "new"
                | "throw"
                | "case"
                | "else"
                | "goto"
                | "sizeof"
                | "co_return"
                | "co_yield"
                | "co_await"
                | "typename"
                | "template"
        );
    // Named by its last segment, as its methods' class is.
    if named && (qualified.contains("::") || templated) {
        return Some(last.to_string());
    }
    // A plain lower-case name (`logger &l`, `const sink *s`, `(buffer
    // buf,`) is a type where a declaration starts — a statement's or a
    // parameter's — and is written as one: `x * y` and `a&b` are sums.
    let declaration_start = {
        let mut before = head[..start].trim_end();
        while let Some(rest) = CPP_QUALIFIERS.iter().find_map(|q| {
            before
                .strip_suffix(q)
                .filter(|r| !r.ends_with(|c: char| c == '_' || c.is_alphanumeric()))
        }) {
            before = rest.trim_end();
        }
        before.is_empty() || before.ends_with(['(', ',', '{', ';', '}'])
    };
    let separator = &original[head.len()..];
    let spaced = |side: &str| side.starts_with(char::is_whitespace);
    let declarator_style = match separator.trim_end().find(['*', '&']) {
        // `T a`
        None => separator.starts_with(char::is_whitespace),
        // `T &a`, `T& a`, not `T * a` or `T*a`.
        Some(at) => {
            let symbols_end = separator[at..]
                .find(|c: char| c != '*' && c != '&')
                .map_or(separator.len(), |i| at + i);
            spaced(separator) != spaced(&separator[symbols_end..])
        }
    };
    (named
        && declaration_start
        && declarator_style
        && qualified == last
        && !CPP_NOT_TYPES.contains(&last))
    .then(|| last.to_string())
}

/// Words that qualify a C++ declaration's type, before it.
const CPP_QUALIFIERS: &[&str] = &[
    "const",
    "static",
    "volatile",
    "mutable",
    "constexpr",
    "thread_local",
    "register",
    "extern",
    "inline",
    "virtual",
    "explicit",
    "friend",
];

/// C++ keywords and built-in types: never a class whose methods a value
/// declared as it calls.
const CPP_NOT_TYPES: &[&str] = &[
    "auto",
    "decltype",
    "void",
    "bool",
    "char",
    "wchar_t",
    "char8_t",
    "char16_t",
    "char32_t",
    "short",
    "int",
    "long",
    "float",
    "double",
    "signed",
    "unsigned",
    "using",
    "namespace",
    "class",
    "struct",
    "union",
    "enum",
    "typedef",
    "operator",
    "public",
    "private",
    "protected",
    "if",
    "else",
    "for",
    "while",
    "do",
    "switch",
    "case",
    "default",
    "break",
    "continue",
    "this",
    "true",
    "false",
    "nullptr",
    "and",
    "or",
    "not",
    "co_await",
    "static_assert",
    "const",
    "static",
    "volatile",
    "mutable",
    "constexpr",
    "thread_local",
    "register",
    "extern",
    "inline",
    "virtual",
    "explicit",
    "friend",
];

/// The type a constructor expression builds: `Cart(…)`, `Cart { … }`,
/// `Cart::new(…)`, `new Cart(…)`, `&Cart{…}`, Go's `new(Cart)`, `Cart.new`,
/// `Cart<T>(…)`,
/// `models.Cart(…)` — when it is the whole expression: what is called or
/// read off it (`Cart(p).load()`) is of a type of its own. A function of a
/// type other than `new` (`Config::builder()`) returns what it returns, and
/// `Shape::Circle(c)` is a value of `Shape`, not a `Circle`.
fn constructed_type(rhs: &str, lang: Lang) -> Option<String> {
    let mut rhs = rhs.trim_start();
    // Go's `new(bytes.Buffer)`: a pointer to the type it names.
    if lang == Lang::Go
        && let Some(inner) = rhs.strip_prefix("new(")
        && let Some(close) = inner.find(')')
        && whole_expression(&inner[close + 1..])
    {
        let ty = inner[..close].trim().trim_start_matches('*');
        return (!ty.is_empty()
            && ty
                .chars()
                .all(|c| c == '_' || c == '.' || c.is_alphanumeric()))
        .then(|| ty.to_string());
    }
    let mut built_with_new = false;
    // (Dart's `const Pipeline()` builds one as `new` does.)
    for prefix in ["new ", "const ", "&mut ", "&"] {
        if let Some(rest) = rhs.strip_prefix(prefix) {
            rhs = rest.trim_start();
            built_with_new |= prefix == "new ";
        }
    }
    // Java and C# build only with `new`: C#'s `MakeCart()` is a method,
    // named in Pascal case as a type is.
    if matches!(lang, Lang::Java | Lang::CSharp) && !built_with_new {
        return None;
    }
    let path_len = rhs
        .find(|c: char| !(c == '_' || c == ':' || c == '.' || c.is_alphanumeric()))
        .unwrap_or(rhs.len());
    let path = &rhs[..path_len];
    let rest = rhs[path_len..].trim_start();
    let segments: Vec<&str> = path.split([':', '.']).filter(|s| !s.is_empty()).collect();
    let typed = |s: &&str| type_like(s, lang);
    // Go builds a value with a composite literal, `Cart{…}`: `newCart(…)`
    // and `pkg.New(…)` are functions, whatever they return.
    let opens: &[char] = if lang == Lang::Go {
        &['{']
    } else {
        &['(', '{', '<']
    };
    // Ruby's `DependencyParser::Ruby::Parse.new(…)`: the class by its
    // whole path, which tells it from `DependencyParser::PHP::Parse`.
    if lang == Lang::Ruby
        && path.contains("::")
        && let [scope @ .., ty, "new"] = segments.as_slice()
        && typed(ty)
        && !scope.is_empty()
    {
        return whole_expression(rest).then(|| format!("{}::{ty}", scope.join("::")));
    }
    let ty = match segments.as_slice() {
        // `Cart::new(…)`, `Cart.new`: the type before its constructor.
        [.., ty, "new"] if lang != Lang::Go && typed(ty) => *ty,
        // `Type::Variant(…)`, `Type::make(…)`, `Outer.Inner(…)`: a value of
        // the type before it, what a function returns, or a type this
        // cannot place. (In Go, `pkg.Cart{…}`, whose package is any name.)
        [.., before, _] if lang != Lang::Go && typed(before) => return None,
        // `Cart(…)`, `Cart { … }`, `Cart<T>(…)`, `models.Cart(…)`.
        [.., last] if typed(last) && rest.starts_with(opens) => *last,
        _ => return None,
    };
    whole_expression(rest).then(|| ty.to_string())
}

/// The type class a Scala summoner names — `Spawn` for `Spawn[IO]`,
/// `Async` for `Async[F]` — its type arguments the whole rest.
fn scala_summoned(receiver: &str) -> Option<String> {
    let (name, rest) = receiver.split_once('[')?;
    let name = name.trim();
    let inner = rest.strip_suffix(']')?;
    let mut depth = 0i32;
    for c in inner.chars() {
        match c {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth < 0 {
                    return None;
                }
            }
            '(' | ')' => return None,
            _ => {}
        }
    }
    if depth != 0 {
        return None;
    }
    // `implicitly[PathConvertible[T]]`, Scala 3's `summon[…]`: the type
    // class it names inside.
    if matches!(name, "implicitly" | "summon") {
        let inner = inner.trim();
        let class = inner.split(['[', ' ']).next().unwrap_or(inner);
        let class = class.rsplit('.').next().unwrap_or(class);
        return type_like(class, Lang::Scala).then(|| class.to_string());
    }
    type_like(name, Lang::Scala).then(|| name.to_string())
}

/// The `T` .NET's dependency injection resolves, the whole expression:
/// `….GetRequiredService<T>()`, `GetService<T>()` and their keyed forms —
/// `T`'s name, its own type arguments dropped (`RoleManager` for
/// `RoleManager<IdentityRole>`).
fn service_type(rhs: &str) -> Option<String> {
    // The statement alone: what follows its `;` is no part of it.
    let rhs = rhs.split(';').next().unwrap_or(rhs).trim();
    let open = [
        "GetRequiredService<",
        "GetService<",
        "GetRequiredKeyedService<",
        "GetKeyedService<",
    ]
    .iter()
    .filter_map(|f| rhs.find(f).map(|at| at + f.len()))
    .min()?;
    let inner = &rhs[open..];
    let mut depth = 0usize;
    let close = inner.char_indices().find_map(|(i, c)| match c {
        '<' => {
            depth += 1;
            None
        }
        '>' if depth == 0 => Some(i),
        '>' => {
            depth -= 1;
            None
        }
        _ => None,
    })?;
    let after = inner[close + 1..].trim_start();
    if !after.starts_with('(') || !whole_expression(after) {
        return None;
    }
    let ty = inner[..close].split('<').next()?.trim();
    let ty = ty.rsplit('.').next().unwrap_or(ty);
    type_like(ty, Lang::CSharp).then(|| ty.to_string())
}

/// The `T` a C++ smart-pointer factory builds, the whole expression:
/// `std::make_shared<T>(…)`, `std::make_unique<ns::T>(…)`,
/// `std::allocate_shared<T>(alloc, …)` — `T`'s last segment, its template
/// arguments dropped.
fn cpp_made_type(rhs: &str) -> Option<String> {
    let rhs = rhs.trim_start();
    let rest = ["make_shared", "make_unique", "allocate_shared"]
        .iter()
        .find_map(|f| {
            rhs.strip_prefix("std::")
                .unwrap_or(rhs)
                .strip_prefix(f)
                .filter(|r| r.trim_start().starts_with('<'))
        })?
        .trim_start();
    let inner = &rest[1..];
    let mut depth = 0usize;
    let close = inner.char_indices().find_map(|(i, c)| match c {
        '<' => {
            depth += 1;
            None
        }
        '>' if depth == 0 => Some(i),
        '>' => {
            depth -= 1;
            None
        }
        _ => None,
    })?;
    let arg = inner[..close].trim();
    let path = arg.split('<').next().unwrap_or(arg).trim();
    let ty = path.rsplit("::").next().unwrap_or(path).trim();
    let identifier = ty.starts_with(|c: char| c == '_' || c.is_alphabetic())
        && ty.chars().all(|c| c == '_' || c.is_alphanumeric());
    let after = inner[close + 1..].trim_start();
    (identifier && after.starts_with('(') && whole_expression(after)).then(|| ty.to_string())
}

/// A factory's value ([`factory_type`]) as a receiver's type: the type its
/// function's definitions declare they return, once the project is read
/// (`UserTable.selectAll()` is a `Query`, not a `UserTable`), else the type
/// it is called on — a guess: `RETURNED`, the function, `|`, the type.
fn factory_guess(rhs: &str, ty: &str, lang: Lang) -> String {
    let function = returned_function(rhs, lang).unwrap_or_default();
    format!("{RETURNED}{function}|{ty}")
}

/// The type a JVM, C# or Dart factory names, its value taken as of that
/// type (or a subtype): Scala's `IO.pure(1)` and `IO.unit`, Kotlin's
/// `Cart.create()`, Java's `List.of(…)`, C#'s `Cart.Create()` — a type's
/// own function called on it, the whole expression. A guess, as a factory
/// may return another type (a builder); so it is a declared type, whose
/// method is the type's own or an override, never exact.
fn factory_type(rhs: &str, lang: Lang) -> Option<String> {
    if !matches!(
        lang,
        Lang::Scala | Lang::Kotlin | Lang::Java | Lang::CSharp | Lang::Dart
    ) {
        return None;
    }
    let rhs = rhs
        .trim_start()
        .strip_prefix("await ")
        .unwrap_or(rhs.trim_start());
    let path_len = rhs
        .find(|c: char| !(c == '_' || c == '.' || c.is_alphanumeric()))
        .unwrap_or(rhs.len());
    let segments: Vec<&str> = rhs[..path_len].split('.').collect();
    let [before @ .., ty, function] = segments.as_slice() else {
        return None;
    };
    // C# names its properties in Pascal case as its types: after a value
    // (`scope.ServiceProvider.GetRequiredService<T>()`) a name is its
    // member, no type — a namespace is Pascal case too.
    if lang == Lang::CSharp && before.iter().any(|s| s.starts_with(char::is_lowercase)) {
        return None;
    }
    let factory = match lang {
        // C# names its methods in Pascal case.
        Lang::CSharp => function.starts_with(char::is_alphabetic),
        _ => function.starts_with(char::is_lowercase),
    };
    let rest = rhs[path_len..].trim_start();
    let ends = rest.is_empty() || rest.starts_with([';', '\n', ')', ',', '}']);
    (type_like(ty, lang) && factory && (ends || whole_expression(rest))).then(|| ty.to_string())
}

/// The function whose result `rhs` is, when it is a call and no more:
/// `adapter` for `Moshi.adapter<T>()`, `stdout_color_mt` for
/// `spdlog::stdout_color_mt("x")` — in a statically typed language whose
/// definitions say what they return — or, a method of a value named
/// alone, `@value#method` (`@repo#find` for `repo.find(id)`): the method
/// of the value's type, once the value's binding is read.
fn returned_call(rhs: &str, lang: Lang) -> Option<String> {
    if !matches!(
        lang,
        Lang::Java
            | Lang::Kotlin
            | Lang::Scala
            | Lang::CSharp
            | Lang::Dart
            | Lang::Cpp
            | Lang::Go
            | Lang::Zig
    ) {
        return None;
    }
    let mut expr = rhs.trim_start();
    for prefix in ["await ", "try "] {
        expr = expr.strip_prefix(prefix).unwrap_or(expr).trim_start();
    }
    // The statement's end.
    let mut depth = 0i32;
    let mut end = expr.len();
    for (i, c) in expr.char_indices() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' if depth == 0 => {
                end = i;
                break;
            }
            ')' | ']' | '}' => depth -= 1,
            ';' | '\n' if depth == 0 => {
                end = i;
                break;
            }
            _ => {}
        }
    }
    let expr = expr[..end]
        .trim_end()
        .trim_end_matches(['!', '?'])
        .trim_end();
    let inner = expr.strip_suffix(')')?;
    // Back to the parenthesis that opens the last call's arguments.
    let mut depth = 1i32;
    let open = inner.char_indices().rev().find_map(|(i, c)| {
        match c {
            ')' => depth += 1,
            '(' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        None
    })?;
    let mut callee = inner[..open].trim_end();
    // Its generic arguments: `adapter<T>()`, Scala's `of[F]()`.
    if let Some(last) = callee.chars().last()
        && (last == '>' || last == ']')
    {
        let first = if last == '>' { '<' } else { '[' };
        let mut depth = 0i32;
        let cut = callee.char_indices().rev().find_map(|(i, c)| {
            if c == last {
                depth += 1;
            } else if c == first {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            None
        })?;
        callee = callee[..cut].trim_end();
    }
    let start = callee
        .char_indices()
        .rev()
        .find(|&(_, c)| !(c == '_' || c.is_alphanumeric()))
        .map_or(0, |(i, c)| i + c.len_utf8());
    let name = &callee[start..];
    // A constructor's type is read where it is built.
    let constructor = matches!(lang, Lang::Java | Lang::Kotlin | Lang::Scala | Lang::Dart)
        && name.starts_with(char::is_uppercase);
    if name.is_empty() || constructor || !name.starts_with(|c: char| c == '_' || c.is_alphabetic())
    {
        return None;
    }
    // A method of a value named by itself (`repo.find(id)`, `l->get()`).
    let before = callee[..start].trim_end();
    let receiver = ["?.", "->", "."]
        .iter()
        .find_map(|sep| before.strip_suffix(sep))
        .map(str::trim)
        .filter(|r| {
            is_plain_name(r)
                && r.starts_with(|c: char| c == '_' || c.is_lowercase())
                && !matches!(*r, "this" | "self" | "super")
        });
    Some(match receiver {
        Some(receiver) => format!("@{receiver}#{name}"),
        None => name.to_string(),
    })
}

/// The last call of a chain, split: `const Pipeline().addMiddleware(m)` is
/// `const Pipeline()` and `addMiddleware`, `repo.find(id)` `repo` and
/// `find` — or `None` where the text is no call on something.
fn chain_link(receiver: &str) -> Option<(&str, &str)> {
    let receiver = receiver.trim();
    let inner = receiver.strip_suffix(')')?;
    let mut depth = 1i32;
    let open = inner.char_indices().rev().find_map(|(i, c)| {
        match c {
            ')' => depth += 1,
            '(' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        None
    })?;
    let callee = inner[..open].trim_end();
    let callee = callee.split('<').next().unwrap_or(callee).trim_end();
    let (base, method) = ["?.", "->", "."]
        .iter()
        .filter_map(|sep| callee.rfind(sep).map(|at| (at, sep.len())))
        .max_by_key(|&(at, _)| at)
        .map(|(at, len)| (&callee[..at], &callee[at + len..]))?;
    (is_plain_name(method) && !base.trim().is_empty()).then(|| (base.trim(), method))
}

/// [`returned_call`]'s function alone, its receiver dropped.
fn returned_function(rhs: &str, lang: Lang) -> Option<String> {
    returned_call(rhs, lang).map(|f| match f.split_once('#') {
        Some((_, name)) => name.to_string(),
        None => f,
    })
}

/// Whether what follows a constructor's name — its generic arguments, its
/// arguments or fields, or nothing — ends the expression: nothing is called
/// or read off the value it builds. Text that ends before its brackets
/// close (a binding read up to a call inside it) is taken as whole.
fn whole_expression(rest: &str) -> bool {
    let mut rest = rest.trim_start();
    for (open, close) in [('<', '>'), ('(', ')'), ('{', '}')] {
        if !rest.starts_with(open) {
            continue;
        }
        let mut depth = 0usize;
        let mut end = None;
        for (i, c) in rest.char_indices() {
            if c == open {
                depth += 1;
            } else if c == close {
                depth -= 1;
                if depth == 0 {
                    end = Some(i + c.len_utf8());
                    break;
                }
            }
        }
        match end {
            Some(end) => rest = rest[end..].trim_start(),
            None => return true,
        }
    }
    let rest = rest.strip_prefix('?').unwrap_or(rest);
    !(rest.starts_with('.')
        || rest.starts_with("->")
        || rest.starts_with('[')
        || rest.starts_with('(')
        || rest.starts_with("::"))
}

/// The type an annotation starts with: `Cart`, `&mut Cart`, `*Cart`.
fn leading_type(text: &str, lang: Lang) -> Option<String> {
    let mut text = text.trim_start();
    for prefix in ["&mut ", "&", "*", "mut ", "dyn "] {
        if let Some(rest) = text.strip_prefix(prefix) {
            text = rest.trim_start();
        }
    }
    // A type nested in another names its own (`EntityTestsData.YEntity`);
    // one of a package (`java.nio.file.Path`) is none of the project's.
    let end = text
        .find(|c: char| !(c == '_' || c == '.' || c.is_alphanumeric()))
        .unwrap_or(text.len());
    let path = text[..end].trim_end_matches('.');
    let ty = match path.rsplit_once('.') {
        Some((outer, inner)) if outer.split('.').all(|s| s.starts_with(char::is_uppercase)) => {
            inner
        }
        _ => path.split('.').next().unwrap_or(path),
    };
    type_like(ty, lang).then(|| ty.to_string())
}

/// The type a Zig annotation names: past pointers, optionals, slices and
/// `const` (`*const std.Io.Writer`, `?*Analyser`, `[]const u8`), its path —
/// the standard library's whole (`std.Io.Writer`), a project's struct
/// (`Analyser`, `DocumentStore.Handle`). `None` for a builtin (`u8`,
/// `anytype`) or a type computed (`@This()`).
fn zig_type_name(text: &str) -> Option<String> {
    let mut t = text.trim_start();
    loop {
        let before = t;
        for prefix in ["*", "?", "[]", "const ", "volatile "] {
            if let Some(rest) = t.strip_prefix(prefix) {
                t = rest.trim_start();
            }
        }
        if let Some(rest) = t.strip_prefix('[')
            && let Some(end) = rest.find(']')
        {
            t = rest[end + 1..].trim_start();
        }
        if t == before {
            break;
        }
    }
    let end = t
        .find(|c: char| !(c == '_' || c == '.' || c.is_alphanumeric()))
        .unwrap_or(t.len());
    let path = t[..end].trim_end_matches('.');
    let last = path.rsplit('.').next().unwrap_or(path);
    (path == "std" || path.starts_with("std.") || last.starts_with(char::is_uppercase))
        .then(|| path.to_string())
}

/// The type a Rust type names, as the one whose methods a value of it has:
/// `&'a mut Vec<u8>` is `Vec`, `crate::a::Set` is `Set`, `&str` is `str`;
/// a `Box<T>`, `Rc<T>` or `Arc<T>` hands on `T`'s methods, so it is `T`.
/// `None` for no one type: a trait object, an `impl Trait`, a tuple, a
/// slice or an array.
fn rust_type_name(text: &str) -> Option<String> {
    let mut t = text.trim();
    // A pointer within a pointer within …: as deep as anyone writes.
    for _ in 0..16 {
        loop {
            let before = t;
            t = t.trim_start_matches('&').trim_start();
            if let Some(rest) = t.strip_prefix('\'') {
                t = rest
                    .trim_start_matches(|c: char| c == '_' || c.is_alphanumeric())
                    .trim_start();
            }
            for prefix in ["*const ", "*mut ", "mut "] {
                if let Some(rest) = t.strip_prefix(prefix) {
                    t = rest.trim_start();
                }
            }
            if t == before {
                break;
            }
        }
        if t.starts_with("dyn ") || t.starts_with("impl ") || t.starts_with(['(', '[']) {
            return None;
        }
        let end = t
            .find(|c: char| !(c == '_' || c == ':' || c.is_alphanumeric()))
            .unwrap_or(t.len());
        let name = t[..end].rsplit("::").next()?;
        if !name.starts_with(|c: char| c == '_' || c.is_alphabetic()) {
            return None;
        }
        if !matches!(name, "Box" | "Rc" | "Arc") {
            return Some(name.to_string());
        }
        // The pointer's content: its first type argument.
        let inner = t[end..].trim_start().strip_prefix('<')?;
        let mut depth = 0usize;
        let mut close = None;
        for (i, c) in inner.char_indices() {
            match c {
                '<' | '(' | '[' => depth += 1,
                '>' | ')' | ']' | ',' if depth == 0 => {
                    close = Some(i);
                    break;
                }
                '>' | ')' | ']' => depth -= 1,
                _ => {}
            }
        }
        t = inner[..close?].trim();
    }
    None
}

/// The type of the field `field` of the type whose `impl` the Rust call
/// `call` is in, as its struct declares it beside that `impl` (in the same
/// module of the file): `items` of `struct Order { items: Vec<u8> }` is
/// `Vec`. `None` where it is not found there.
fn rust_field_type(call: Node, field: &str, src: &str) -> Option<String> {
    // Up to the `impl`: a method's body is a few levels down; a call deeper
    // than this in one is not one anyone wrote.
    let mut at = call;
    let imp = (0..64).find_map(|_| {
        at = at.parent()?;
        (at.kind() == "impl_item").then_some(at)
    })?;
    let owner = rust_type_name(node_text(imp.child_by_field_name("type")?, src))?;
    let named = |n: &Node, name: &str| {
        n.child_by_field_name("name")
            .is_some_and(|x| node_text(x, src) == name)
    };
    let scope = imp.parent()?;
    let mut cursor = scope.walk();
    let declared = scope
        .named_children(&mut cursor)
        .find(|n| n.kind() == "struct_item" && named(n, &owner))?;
    let body = declared.child_by_field_name("body")?;
    let mut cursor = body.walk();
    let decl = body
        .named_children(&mut cursor)
        .find(|n| n.kind() == "field_declaration" && named(n, field))?;
    rust_type_name(node_text(decl.child_by_field_name("type")?, src))
}

/// The type a declaration ends with before its name: `Cart`, `final Cart`,
/// `Cart*`, `List<Cart>` → `List`.
fn trailing_type(head: &str) -> Option<String> {
    // A nullable `Cart?` (C#, Dart) is a `Cart`.
    let mut head = head.trim_end().trim_end_matches(['*', '&', ' ', '?']);
    // A Dart function type (`final void Function(Change<int>)? callback`):
    // a `Function`, whose `call` runs it — no method of the project's.
    if let Some(inner) = head.strip_suffix(')') {
        let mut depth = 1i32;
        let open = inner.char_indices().rev().find_map(|(i, c)| {
            match c {
                ')' => depth += 1,
                '(' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                _ => {}
            }
            None
        });
        let before = open.map(|i| inner[..i].trim_end());
        if let Some(before) = before
            && let Some(rest) = before.strip_suffix("Function")
            && !rest.ends_with(|c: char| c == '_' || c.is_alphanumeric())
        {
            return Some("Function".to_string());
        }
    }
    if head.ends_with('>') {
        let mut depth = 0usize;
        let mut cut = None;
        for (i, c) in head.char_indices().rev() {
            match c {
                '>' => depth += 1,
                '<' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        cut = Some(i);
                        break;
                    }
                }
                _ => {}
            }
        }
        head = head[..cut?].trim_end();
    }
    // After the last character not of a name — however many bytes it is:
    // a line before ending in `。` or `→` put the cut inside it.
    let start = head
        .char_indices()
        .rev()
        .find(|&(_, c)| !(c == '_' || c.is_alphanumeric()))
        .map_or(0, |(i, c)| i + c.len_utf8());
    let ty = &head[start..];
    ty.starts_with(|c: char| c.is_uppercase())
        .then(|| ty.to_string())
}

/// Whether `name` reads as a type: capitalized, as types are written —
/// in Go, any name, as an unexported type is lowercase.
fn type_like(name: &str, lang: Lang) -> bool {
    let first = name.chars().next();
    let identifier = first.is_some_and(|c| c == '_' || c.is_alphabetic())
        && name.chars().all(|c| c == '_' || c.is_alphanumeric());
    identifier && (lang == Lang::Go || first.is_some_and(char::is_uppercase))
}

/// Containers a method belongs to, by node kind, named by their `name`
/// field. An interface's methods are its own too (Java's, C#'s, PHP's):
/// a value declared as it calls them, or an implementation's override,
/// as the type's family reaches its subtypes.
const CLASS_KINDS: &[&str] = &[
    "interface_declaration",
    "class_definition",
    "class_declaration",
    "class",
    "class_specifier",
    "struct_specifier",
    "union_specifier",
    "enum_declaration",
    "record_declaration",
    "struct_declaration",
    "object_declaration",
    "abstract_class_declaration",
    // Dart's `mixin HydratedMixin on BlocBase`.
    "mixin_declaration",
    // Scala's `object Cart` and `trait Pricing`.
    "object_definition",
    "trait_definition",
];

/// The class (type, impl) each callable of a file belongs to, by its
/// definition line ([`FileCalls::owners`]).
pub fn owners_in(
    tree: &Tree,
    src: &str,
    lang: Lang,
    symbols: &[Located],
) -> HashMap<usize, String> {
    let root = tree.root_node();
    symbols
        .iter()
        .filter(|l| is_callable(&l.symbol.kind))
        // A C++ friend defined in its class (`friend auto add_compare(…)`)
        // is no member of it: a free function its arguments find.
        .filter(|l| {
            lang != Lang::Cpp
                || !(src
                    .get(l.declaration.0..l.signature_end.max(l.declaration.0))
                    .is_some_and(cpp_friend)
                    || src
                        .get(..l.declaration.0)
                        .is_some_and(|before| before.trim_end().ends_with("friend")))
        })
        .filter_map(|l| {
            // Inside the signature, past any decorator or doc comment the
            // declaration's span takes in.
            let at = l.signature_end.saturating_sub(1).max(l.declaration.0);
            let leaf = root.descendant_for_byte_range(at, at)?;
            Some((l.symbol.line, owner_from(leaf, src, lang)?))
        })
        .collect()
}

/// Whether a C++ declaration's text declares a friend: `friend` among the
/// words before its parameters, past a template's header (`template <…>
/// friend auto f(…)`). (Where the grammar's friend declaration holds the
/// definition, `friend` is just before its text.)
fn cpp_friend(text: &str) -> bool {
    let head = text.split('(').next().unwrap_or(text);
    let head = match head.trim_start().strip_prefix("template") {
        Some(rest) => rest.rsplit_once('>').map_or(rest, |(_, after)| after),
        None => head,
    };
    head.split_whitespace().any(|w| w == "friend")
}

/// The class a definition at `node` (any node inside it) belongs to.
fn owner_from(node: Node, src: &str, lang: Lang) -> Option<String> {
    match lang {
        Lang::Elixir => return elixir_module_of(node, src),
        Lang::Erlang => return erlang_module_of(node, src),
        Lang::Zig => return zig_container_of(node, src),
        _ => {}
    }
    let mut cur = Some(node);
    while let Some(n) = cur {
        match n.kind() {
            // Go: `func (c *Cart) Total()` belongs to `Cart`.
            "method_declaration" if lang == Lang::Go => {
                let receiver = n.child_by_field_name("receiver")?;
                let mut cursor = receiver.walk();
                let param = receiver.named_children(&mut cursor).next()?;
                return base_type_name(node_text(param.child_by_field_name("type")?, src));
            }
            // Rust: `impl Cart { … }`, `impl Trait for Cart { … }`.
            "impl_item" => return base_type_name(node_text(n.child_by_field_name("type")?, src)),
            // C++: `int Cart::total() { … }`, defined out of its class.
            "function_definition" if lang == Lang::Cpp => {
                let declarator = node_text(n.child_by_field_name("declarator")?, src);
                let name = declarator.split('(').next().unwrap_or("");
                if let Some((scope, _)) = name.rsplit_once("::") {
                    return base_type_name(scope);
                }
            }
            // Ruby's `module Shop` (no other grammar's `module`: Python's
            // is its file).
            "module" if lang == Lang::Ruby => {
                if let Some(name) = n.child_by_field_name("name") {
                    return base_type_name(node_text(name, src));
                }
            }
            // A Haskell instance's method (`instance HasMeta Pandoc where
            // setMeta …`): the type's, the class's where it names none.
            "instance" if lang == Lang::Haskell => {
                let class = n.child_by_field_name("name").map(|c| node_text(c, src));
                // (Of a class of several parameters, `Walkable Inline
                // Block`, no one type's: its head, which a signature names
                // none of.)
                let single = n
                    .child_by_field_name("patterns")
                    .is_some_and(|p| p.named_child_count() == 1);
                let ty = n
                    .child_by_field_name("patterns")
                    .filter(|_| single)
                    .and_then(|p| {
                        let mut stack = vec![p];
                        while let Some(x) = stack.pop() {
                            if x.kind() == "name" {
                                return Some(node_text(x, src));
                            }
                            let mut cursor = x.walk();
                            let children: Vec<Node> = x.named_children(&mut cursor).collect();
                            stack.extend(children.into_iter().rev());
                        }
                        None
                    });
                // (One of no type's, `Walkable a (t b)`: its head's
                // patterns, which name no type a signature does.)
                return ty.map(str::to_string).or_else(|| {
                    let patterns = n.child_by_field_name("patterns").map(|p| node_text(p, src));
                    match (class, patterns) {
                        (Some(class), Some(patterns)) => Some(format!("{class} {patterns}")),
                        (class, _) => class.map(str::to_string),
                    }
                });
            }
            // Dart's `mixin M on B { … }` names itself in an unnamed child.
            "mixin_declaration" if lang == Lang::Dart => {
                let mut cursor = n.walk();
                let name = n
                    .named_children(&mut cursor)
                    .find(|c| c.kind() == "identifier")?;
                return base_type_name(node_text(name, src));
            }
            kind if CLASS_KINDS.contains(&kind) => {
                // A C or C++ class whose node a macro stretched past its
                // `};` (`FMT_BEGIN_NAMESPACE` leaves the grammar guessing):
                // what comes after its body's close is not its own.
                let closed = matches!(lang, Lang::C | Lang::Cpp)
                    && n.child_by_field_name("body").is_some_and(|body| {
                        src.get(body.start_byte()..node.start_byte())
                            .is_some_and(|text| !still_open(text))
                    });
                if !closed && let Some(name) = n.child_by_field_name("name") {
                    return base_type_name(node_text(name, src));
                }
            }
            // A class the grammar could not finish (Kotlin's grammar takes
            // a property named `where` for its keyword): its header is the
            // error's first tokens, and its body's open brace still open.
            "ERROR" => {
                if let Some(name) = unfinished_class(n, node, src) {
                    return Some(name);
                }
            }
            _ => {}
        }
        cur = n.parent();
    }
    None
}

/// The class whose header a parse error kept as its own tokens —
/// `class Query (…) : Base {` — and whose body is still open where
/// `inside` is: no `}` of the error's own closed it before.
fn unfinished_class(error: Node, inside: Node, src: &str) -> Option<String> {
    let mut cursor = error.walk();
    let tokens: Vec<Node> = error.children(&mut cursor).collect();
    let mut open: Option<(String, usize)> = None;
    let mut named: Option<String> = None;
    for (i, token) in tokens.iter().enumerate() {
        if token.start_byte() >= inside.start_byte() {
            break;
        }
        match token.kind() {
            "class" | "object" | "struct" | "enum" | "record" => {
                named = tokens[i + 1..]
                    .iter()
                    .find(|t| t.is_named())
                    .filter(|t| t.kind().ends_with("identifier"))
                    .map(|t| node_text(*t, src).to_string());
            }
            "{" => {
                open = named.take().map(|name| (name, token.start_byte())).or(open);
            }
            "}" => open = None,
            // `struct formatter;`: a declaration, no body opened.
            ";" => named = None,
            _ => {}
        }
    }
    // Its body's `}` may be no token of the error's own (the grammar read
    // part of the body as a node of its own): the text says whether the
    // body is still open where `inside` is.
    let (name, brace) = open?;
    still_open(src.get(brace..inside.start_byte())?)
        .then(|| base_type_name(&name))
        .flatten()
}

/// Whether the brace `text` starts with is still open at its end: no `}`
/// closes it, those in strings, character literals and comments aside.
fn still_open(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => depth += 1,
            b'}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return false;
                }
            }
            quote @ (b'"' | b'\'') => {
                i += 1;
                while i < bytes.len() && bytes[i] != quote && bytes[i] != b'\n' {
                    i += if bytes[i] == b'\\' { 2 } else { 1 };
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i += 1;
            }
            _ => {}
        }
        i += 1;
    }
    depth > 0
}

/// The Zig container a function at `node` is in: the struct (union, enum)
/// its declaration names (`pub const Params = struct { … }`), a generic's
/// function (`fn List(comptime T: type) type { return struct { … } }`), or
/// — a function of the file itself — the file's own name for itself
/// (`const Analyser = @This();`).
fn zig_container_of(node: Node, src: &str) -> Option<String> {
    let mut at = Some(node);
    while let Some(n) = at {
        if matches!(
            n.kind(),
            "struct_declaration" | "union_declaration" | "enum_declaration" | "opaque_declaration"
        ) {
            let mut up = n.parent();
            while let Some(p) = up {
                match p.kind() {
                    "variable_declaration" => {
                        let mut cursor = p.walk();
                        return p
                            .named_children(&mut cursor)
                            .find(|c| c.kind() == "identifier")
                            .map(|c| node_text(c, src).to_string());
                    }
                    "function_declaration" => {
                        return p
                            .child_by_field_name("name")
                            .map(|c| node_text(c, src).to_string());
                    }
                    "source_file" => return None,
                    _ => up = p.parent(),
                }
            }
            return None;
        }
        if n.kind() == "source_file" {
            let mut cursor = n.walk();
            return n
                .named_children(&mut cursor)
                .filter(|d| d.kind() == "variable_declaration")
                // `const Analyser = @This();` itself — not a struct whose
                // method takes `self: *@This()`.
                .find(|d| {
                    node_text(*d, src)
                        .split_once('=')
                        .is_some_and(|(_, value)| {
                            value.trim().trim_end_matches(';').trim() == "@This()"
                        })
                })
                .and_then(|d| {
                    let mut c = d.walk();
                    d.named_children(&mut c)
                        .find(|c| c.kind() == "identifier")
                        .map(|c| node_text(c, src).to_string())
                });
        }
        at = n.parent();
    }
    None
}

/// The Elixir module a definition at `node` is in, by its full name: each
/// `defmodule` (or `defprotocol`) around it, outermost first —
/// `Cart.Inner` for a function of `defmodule Inner` inside `defmodule
/// Cart`. What another module's `Cart.Inner.f(…)` names.
fn elixir_module_of(node: Node, src: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    let mut cur = Some(node);
    while let Some(n) = cur {
        if n.kind() == "call"
            && n.child_by_field_name("target")
                .is_some_and(|t| matches!(node_text(t, src), "defmodule" | "defprotocol"))
            && let Some(name) = n
                .named_children(&mut n.walk())
                .find(|c| c.kind() == "arguments")
                .and_then(|a| a.named_child(0))
                .filter(|a| a.kind() == "alias")
        {
            parts.push(node_text(name, src));
        }
        cur = n.parent();
    }
    (!parts.is_empty()).then(|| {
        parts.reverse();
        parts.join(".")
    })
}

/// The Erlang module a definition at `node` is in: its file's
/// `-module(cart).` — what another module's `cart:add(…)` names.
fn erlang_module_of(node: Node, src: &str) -> Option<String> {
    let mut root = node;
    while let Some(parent) = root.parent() {
        root = parent;
    }
    let mut cursor = root.walk();
    let attribute = root
        .named_children(&mut cursor)
        .find(|n| n.kind() == "module_attribute")?;
    let name = attribute.child_by_field_name("name")?;
    Some(node_text(name, src).trim_matches('\'').to_string())
}

/// The module an Elixir `Cart.add(…)` or an Erlang `cart:add(…)` names —
/// written out, not one a variable holds (`mod.f()`, `?MODULE:f()`).
fn module_receiver(call: Node, parent: Option<Node>, src: &str, lang: Lang) -> Option<Receiver> {
    let module = match lang {
        Lang::Elixir => {
            let target = call.child_by_field_name("target")?;
            let left = target.child_by_field_name("left")?;
            // `:crypto.hash(…)`: an Erlang module, by its atom.
            if target.kind() == "dot" && left.kind() == "atom" {
                let name = node_text(left, src).trim_start_matches(':');
                return Some(Receiver::Type(name.to_string()));
            }
            (target.kind() == "dot" && left.kind() == "alias").then_some(left)?
        }
        // `fun cart:add/2` names its module itself.
        Lang::Erlang if call.kind() == "external_fun" => {
            let module = call.child_by_field_name("module")?;
            let atom = module.child_by_field_name("name")?;
            (atom.kind() == "atom").then_some(atom)?
        }
        Lang::Erlang => {
            let remote = parent.filter(|p| p.kind() == "remote")?;
            let module = remote.child_by_field_name("module")?;
            let atom = module.child_by_field_name("module")?;
            (atom.kind() == "atom").then_some(atom)?
        }
        _ => return None,
    };
    let name: String = node_text(module, src)
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    Some(Receiver::Type(name.trim_matches('\'').to_string()))
}

/// A type's own name in how it is written: `*Cart[T]` → `Cart`,
/// `ns::Cart<T>` → `Cart`, `&'a mut Cart` → `Cart`.
fn base_type_name(text: &str) -> Option<String> {
    let text = text.split(['<', '[', '(']).next()?.trim();
    let name = text
        .rsplit(|c: char| !(c == '_' || c.is_alphanumeric()))
        .find(|s| !s.is_empty())?;
    (name.starts_with(|c: char| c == '_' || c.is_alphabetic())).then(|| name.to_string())
}

/// Raw spelling is useful for display, but `foo` and `r#foo` name the same
/// Rust item. Normalize both sides of resolution without changing node keys.
fn logical_name(name: &str, lang: Option<Lang>) -> &str {
    if lang == Some(Lang::Rust) {
        name.strip_prefix("r#").unwrap_or(name)
    } else {
        name
    }
}

/// Most files a C or C++ file's includes reach, through one another
/// ([`include_closure`]): past it, the rest are not looked in.
const MAX_INCLUDED: usize = 4_096;

/// Every file `file` includes, through the headers it includes too: what
/// the preprocessor pastes into it. `direct` is its own includes, `scope`
/// every file's.
fn include_closure(
    file: &Path,
    direct: &HashSet<PathBuf>,
    scope: &HashMap<PathBuf, HashSet<PathBuf>>,
) -> HashSet<PathBuf> {
    let mut seen: HashSet<PathBuf> = direct.clone();
    let mut queue: Vec<&PathBuf> = direct.iter().collect();
    while let Some(next) = queue.pop() {
        for inner in scope.get(next).into_iter().flatten() {
            if seen.len() >= MAX_INCLUDED {
                return seen;
            }
            if inner.as_path() != file && seen.insert(inner.clone()) {
                queue.push(inner);
            }
        }
    }
    seen
}

/// Most definitions one call resolves to (see [`resolve_callees`]): more
/// than any overload set, or than the `new` of each module a crate root
/// declares.
const MAX_CANDIDATES: usize = 64;

/// Most edges a graph holds: past it, further calls are left out.
const MAX_EDGES: usize = 2_000_000;

/// Resolve a call to the project definitions it plausibly refers to, narrowing
/// by scope so a name isn't sprayed across every same-named definition:
///   1. a definition in the **same file** (a local helper), else
///   2. definitions in files the caller **imports** (in scope via `use`), else
///   3. in C and C++, the definitions a **declaration in scope** names — a
///      header's `int add(int, int);` in `class Cart`, a file's own
///      prototype — with that declaration's owner, wherever they are (the
///      program links its files into one), else
///   4. for a *free* call only, a **globally unique** definition of that name
///      in a language that shares a namespace with the call site (a `.tsx`
///      component calling a `.ts` helper, C++ calling C).
///
/// A definition with a body always beats a bodyless declaration of the same
/// name (a C prototype in a header, an overload signature), which stands in
/// only when nothing defines the name. A method call that matches none of 1–2
/// resolves to nothing rather than guessing (its name belongs to a receiver
/// type we can't see).
fn resolve_callees(
    defs: &Defs,
    nodes: &[SymNode],
    facts: &[NodeFacts],
    owner: &[Option<&str>],
    call: &CallSite,
    site: &Site,
) -> Vec<usize> {
    let name = logical_name(&call.callee, Some(site.lang));
    let Some(named) = defs.named.get(name) else {
        return Vec::new();
    };
    let with_body = !named.defined.is_empty();
    let cands: &[usize] = if with_body {
        &named.defined
    } else {
        &named.all
    };
    // The candidates in `file`, with a body when any definition has one —
    // and, a BEAM function's, of the call's arity.
    fn in_file<'d>(
        defs: &'d Defs<'d>,
        facts: &'d [NodeFacts],
        file: &'d Path,
        name: &'d str,
        with_body: bool,
        arity: Option<u8>,
    ) -> impl Iterator<Item = usize> + 'd {
        defs.in_file
            .get(&(file, name))
            .into_iter()
            .flatten()
            .copied()
            .filter(move |&i| (!with_body || !facts[i].bodyless) && facts[i].takes(arity))
    }
    // A name defined more often than any real overload set is ambiguous: it
    // resolves to none of them. Linking every one made a file of N
    // same-named definitions calling each other N² edges — tens of GB from
    // one generated file within the index's size cap.
    let local: Vec<usize> = in_file(defs, facts, site.file, name, with_body, call.arity)
        .take(MAX_CANDIDATES + 1)
        .collect();
    if local.len() > MAX_CANDIDATES {
        return Vec::new();
    }
    // A same-file definition the call's shape cannot reach — another
    // class's member, for a bare call in a language that calls only free
    // functions, constructors and its own class's members bare; a free
    // function, for a method call — hides none in scope that it can
    // (`fmt::format(…)` in a test file with `formatter<T>::format`s).
    let shape_fits = |i: usize| -> bool {
        let bare = !call.method
            && matches!(
                site.lang,
                Lang::Dart
                    | Lang::JavaScript
                    | Lang::TypeScript
                    | Lang::Tsx
                    | Lang::Python
                    | Lang::Php
            );
        let method = call.method
            && call.receiver.is_none()
            && matches!(
                site.lang,
                Lang::Java | Lang::Kotlin | Lang::Scala | Lang::CSharp | Lang::Cpp
            );
        // C++'s `fmt::format(…)` or `Cart::make(…)`: a member of the class
        // it names (or one it inherits, which the graph looks for further).
        let cpp_scope = (site.lang == Lang::Cpp && !call.method)
            .then_some(call.qualifier.as_deref())
            .flatten();
        match owner[i] {
            None => !method,
            Some(o) => {
                let last = o.rsplit(['.', ':']).next().unwrap_or(o);
                let constructor = nodes[i].name == last;
                constructor || (!bare && cpp_scope.is_none_or(|q| q == last))
            }
        }
    };
    let look_further = !local.is_empty() && !local.iter().any(|&i| shape_fits(i));
    if !local.is_empty() && !look_further {
        return local;
    }
    // From whichever side is smaller: the candidates, or the files in scope.
    let scoped: Vec<usize> = match cands.len() <= site.scope_len() {
        true => cands
            .iter()
            .copied()
            .filter(|&i| facts[i].takes(call.arity) && site.sees(&nodes[i].file))
            .take(MAX_CANDIDATES + 1)
            .collect(),
        false => {
            let mut seen: HashSet<usize> = HashSet::new();
            site.scope()
                .flat_map(|file| in_file(defs, facts, file, name, with_body, call.arity))
                .filter(|&i| seen.insert(i))
                .take(MAX_CANDIDATES + 1)
                .collect()
        }
    };
    if scoped.len() > MAX_CANDIDATES {
        return Vec::new();
    }
    if look_further {
        // The file's own still count: its class's members, called bare
        // from one of its methods.
        let mut both: Vec<usize> = local.into_iter().chain(scoped).collect();
        both.sort_unstable();
        both.dedup();
        if both.len() > MAX_CANDIDATES {
            return Vec::new();
        }
        return both;
    }
    if !scoped.is_empty() {
        let mut scoped = scoped;
        // In the candidates' order, whichever side they came from.
        scoped.sort_unstable();
        return scoped;
    }
    // C and C++: what a header in scope (or the file's own prototype)
    // declares, another file defines — the include brings the declaration
    // only. Its definitions are the ones of its owner: `Cart`'s `add` for a
    // declaration in `class Cart`, a free function's for a free one; never
    // another file's `static` one, which is that file's alone.
    // Looked up from the name's declarations, few, not from the files in
    // scope, which a C++ file's includes make hundreds.
    if matches!(site.lang, Lang::C | Lang::Cpp) && with_body {
        let declared: HashSet<Option<&str>> = named
            .all
            .iter()
            .copied()
            .filter(|&i| facts[i].bodyless)
            .filter(|&i| nodes[i].file == site.file || site.sees(&nodes[i].file))
            .map(|i| owner[i])
            .collect();
        if !declared.is_empty() {
            let linked: Vec<usize> = named
                .defined
                .iter()
                .copied()
                .filter(|&i| {
                    matches!(facts[i].lang, Some(Lang::C | Lang::Cpp))
                        && !facts[i].internal
                        && declared.contains(&owner[i])
                })
                .take(MAX_CANDIDATES + 1)
                .collect();
            if linked.len() > MAX_CANDIDATES {
                return Vec::new();
            }
            if !linked.is_empty() {
                return linked;
            }
        }
    }
    // A lone global definition of this name — accepted only when its language
    // can be reached from the call site's. Otherwise a Go `Foo()` would resolve
    // to a JS function named `Foo`, a spurious cross-language edge.
    if !call.method
        // An Elixir or Erlang name is called bare only where it is defined
        // or imported: one the file reaches neither way is a library's
        // macro (Ecto's `execute`, ExMachina's `insert`), not the
        // project's one function of the name.
        && !matches!(site.lang, Lang::Elixir | Lang::Erlang)
        && let [only] = cands
        && !facts[*only].internal
        && facts[*only]
            .lang
            .is_some_and(|l| l.shares_namespace_with(site.lang))
    {
        return vec![*only];
    }
    Vec::new()
}

/// Whether a language's types have methods from others — a base class's,
/// a trait's or interface's default, Go's embedded type's — so a call on a
/// type may run one it does not define, or a subtype's override.
fn has_inheritance(lang: Lang) -> bool {
    matches!(
        lang,
        Lang::Java
            | Lang::Kotlin
            | Lang::Scala
            | Lang::CSharp
            | Lang::Dart
            | Lang::Cpp
            | Lang::Python
            | Lang::Php
            | Lang::Ruby
            | Lang::JavaScript
            | Lang::TypeScript
            | Lang::Tsx
            | Lang::Go
    )
}

/// Whether a bare call in a method may be one of its own class's methods
/// (an implicit `this`): Java's, Kotlin's, Scala's, C#'s, Dart's, C++'s and
/// Ruby's — not Python's or JavaScript's, which say `self.`/`this.`, nor
/// Go's, whose methods always name their receiver.
fn implicit_self(lang: Lang) -> bool {
    matches!(
        lang,
        Lang::Java
            | Lang::Kotlin
            | Lang::Scala
            | Lang::CSharp
            | Lang::Dart
            | Lang::Cpp
            | Lang::Ruby
    )
}

/// Whether `call` is the language's own, never one of the project's: a
/// bare call of a builtin (`len(x)`, `make(…)`, ExUnit's `test`), or a Ruby
/// value of no known type called with a core method (`items.each`,
/// `user.save`, `scope.where`) — the core library's or Rails', not a
/// project method that happens to share the name.
pub fn is_library_call(lang: Lang, call: &CallSite) -> bool {
    (!call.method && is_builtin(lang, &call.callee))
        || (call.method
            && call.receiver.is_none()
            && lang == Lang::Ruby
            && RUBY_CORE_METHODS.contains(&call.callee.as_str()))
        // Every object's own (`x.toString()`, `x.Equals(y)`) on a value
        // of no known type: whichever class it is, not one the reader can
        // pick.
        || (call.method
            && call.receiver.is_none()
            && matches!(
                lang,
                Lang::Java | Lang::Kotlin | Lang::Scala | Lang::CSharp | Lang::Dart
            )
            && matches!(
                call.callee.as_str(),
                "toString"
                    | "equals"
                    | "hashCode"
                    | "getClass"
                    | "compareTo"
                    | "ToString"
                    | "Equals"
                    | "GetHashCode"
                    | "GetType"
                    | "CompareTo"
                    | "noSuchMethod"
            ))
}

/// The methods Ruby's core classes and Rails' models give every value: a
/// call of one on a value whose type is not known is theirs.
const RUBY_CORE_METHODS: &[&str] = &[
    "each",
    "each_with_index",
    "each_with_object",
    "each_pair",
    "each_key",
    "each_value",
    "map",
    "flat_map",
    "collect",
    "select",
    "filter",
    "filter_map",
    "reject",
    "find",
    "detect",
    "find_index",
    "count",
    "size",
    "length",
    "first",
    "last",
    "empty?",
    "any?",
    "all?",
    "none?",
    "include?",
    "member?",
    "key?",
    "has_key?",
    "merge",
    "merge!",
    "keys",
    "values",
    "to_s",
    "to_h",
    "to_a",
    "to_i",
    "to_f",
    "to_sym",
    "to_json",
    "as_json",
    "inspect",
    "join",
    "split",
    "strip",
    "chomp",
    "gsub",
    "sub",
    "push",
    "pop",
    "shift",
    "unshift",
    "delete",
    "delete_if",
    "fetch",
    "dig",
    "sort",
    "sort_by",
    "uniq",
    "compact",
    "flatten",
    "reverse",
    "min",
    "max",
    "sum",
    "inject",
    "reduce",
    "group_by",
    "partition",
    "zip",
    "take",
    "drop",
    "slice",
    "tap",
    "then",
    "freeze",
    "dup",
    "clone",
    "nil?",
    "present?",
    "blank?",
    "presence",
    "try",
    "send",
    "public_send",
    "respond_to?",
    "is_a?",
    "kind_of?",
    "instance_of?",
    "equal?",
    "eql?",
    "hash",
    "where",
    "find_by",
    "find_each",
    "order",
    "limit",
    "offset",
    "pluck",
    "includes",
    "joins",
    "group",
    "exists?",
    "update",
    "update!",
    "save",
    "save!",
    "destroy",
    "destroy!",
    "reload",
    "touch",
    "valid?",
    "errors",
    "attributes",
    "assign_attributes",
    "new_record?",
    "persisted?",
    "changed?",
    "increment!",
    "decrement!",
    "deliver_now",
    "deliver_later",
    "perform_later",
    "perform_now",
    // `.new` runs `initialize`: never a method of the name (a Rails
    // controller's `new` action).
    "new",
];

/// Bare-name calls to language builtins are not project functions. Without this
/// they resolve to any same-named definition — even one in another language
/// (a JS `make` in a vendored bundle) — and dominate the "most called" ranking.
fn is_builtin(lang: Lang, name: &str) -> bool {
    match lang {
        Lang::Go => matches!(
            name,
            "append"
                | "cap"
                | "clear"
                | "close"
                | "complex"
                | "copy"
                | "delete"
                | "imag"
                | "len"
                | "make"
                | "max"
                | "min"
                | "new"
                | "panic"
                | "print"
                | "println"
                | "real"
                | "recover"
        ),
        Lang::Python => matches!(
            name,
            "abs"
                | "all"
                | "any"
                | "bool"
                | "bytearray"
                | "bytes"
                | "callable"
                | "chr"
                | "dict"
                | "dir"
                | "enumerate"
                | "filter"
                | "float"
                | "format"
                | "frozenset"
                | "getattr"
                | "hasattr"
                | "hash"
                | "hex"
                | "id"
                | "input"
                | "int"
                | "isinstance"
                | "issubclass"
                | "iter"
                | "len"
                | "list"
                | "map"
                | "max"
                | "min"
                | "next"
                | "object"
                | "oct"
                | "open"
                | "ord"
                | "pow"
                | "print"
                | "range"
                | "repr"
                | "reversed"
                | "round"
                | "set"
                | "setattr"
                | "sorted"
                | "str"
                | "sum"
                | "super"
                | "tuple"
                | "type"
                | "vars"
                | "zip"
        ),
        // JS/TS global functions and constructors. A bare `parseInt(...)` or
        // `decodeURIComponent(...)` is a runtime builtin, not a project function
        // — without this it can resolve to a same-named helper (e.g. a polyfill
        // in a test file) and inflate the "most called" ranking.
        Lang::JavaScript | Lang::TypeScript | Lang::Tsx => matches!(
            name,
            "Array"
                | "Boolean"
                | "Date"
                | "Error"
                | "Map"
                | "Number"
                | "Object"
                | "Promise"
                | "Proxy"
                | "RegExp"
                | "Set"
                | "String"
                | "Symbol"
                | "WeakMap"
                | "WeakSet"
                | "clearInterval"
                | "clearTimeout"
                | "decodeURI"
                | "decodeURIComponent"
                | "encodeURI"
                | "encodeURIComponent"
                | "fetch"
                | "isFinite"
                | "isNaN"
                | "parseFloat"
                | "parseInt"
                | "queueMicrotask"
                | "require"
                | "setInterval"
                | "setTimeout"
                | "structuredClone"
        ),
        // A method is called through `$this->`, never bare, so a bare
        // `count($x)` is PHP's own even in a class that defines `count()` —
        // and a `Countable`'s `count`, an iterator's `current`/`key`/`next`,
        // a money type's `round`, are what it would resolve to otherwise.
        // `isset`, `empty` and `die` are language constructs the grammar
        // reads as calls.
        Lang::Php => matches!(
            name,
            "abs"
                | "ceil"
                | "count"
                | "current"
                | "date"
                | "die"
                | "empty"
                | "end"
                | "floor"
                | "implode"
                | "isset"
                | "key"
                | "max"
                | "min"
                | "next"
                | "reset"
                | "round"
                | "sort"
                | "trim"
        ),
        // `Kernel`'s methods, called bare everywhere: an IO wrapper's own
        // `puts` or a formatter's `format` would otherwise take every one of
        // them. (A class that defines one of these and calls its own bare
        // loses that edge; the name alone cannot tell.)
        Lang::Ruby => matches!(
            name,
            "format"
                | "loop"
                | "p"
                | "pp"
                | "print"
                | "printf"
                | "puts"
                | "raise"
                | "require"
                | "require_relative"
                | "sleep"
                | "sprintf"
        ),
        // Erlang's auto-imported BIFs. A module that defines one under the
        // same name and arity cannot call its own bare (unless it declares
        // `no_auto_import`), so a bare `get(K)` or `size(T)` is the BIF;
        // other modules call the project's as `kv:get(K)`, a remote call.
        // A local function of another arity called bare (`get/2`) loses its
        // edge: the name alone cannot tell.
        Lang::Erlang => matches!(
            name,
            "apply"
                | "element"
                | "error"
                | "exit"
                | "get"
                | "length"
                | "max"
                | "min"
                | "node"
                | "put"
                | "round"
                | "self"
                | "size"
                | "spawn"
                | "throw"
        ),
        // The Prelude's functions, in scope unqualified in every module: a
        // project's container module defines its own `lookup` or `filter`
        // and is imported qualified for them, so a bare call elsewhere is
        // the Prelude's. (The module itself, hiding the Prelude's to call
        // its own bare, loses those edges; the name alone cannot tell.)
        Lang::Haskell => matches!(
            name,
            "concat"
                | "elem"
                | "filter"
                | "head"
                | "length"
                | "lookup"
                | "map"
                | "null"
                | "print"
                | "reverse"
                | "show"
                | "sum"
                | "tail"
                | "zip"
        ),
        // No bare-name builtins that a project would also define: a project
        // `malloc` wrapper IS what a C call to `malloc` links against, and
        // C++'s `std::` calls are excluded where they are read. C# has no
        // free functions — a bare `F()` is a method of the class or a local
        // function — and its `nameof` is dropped where it is read; Zig spells
        // its builtins `@name`, and those are no call expressions.
        // Kotlin's standard functions a project is as likely to define.
        Lang::Kotlin => matches!(
            name,
            "println"
                | "print"
                | "listOf"
                | "mutableListOf"
                | "setOf"
                | "mutableSetOf"
                | "mapOf"
                | "mutableMapOf"
                | "arrayOf"
                | "require"
                | "requireNotNull"
                | "check"
                | "checkNotNull"
                | "error"
                | "repeat"
                | "lazy"
                | "run"
                | "let"
                | "also"
                | "apply"
                | "with"
                | "TODO"
        ),
        Lang::Scala => matches!(
            name,
            "println" | "print" | "require" | "assert" | "List" | "Seq" | "Map" | "Set" | "Some"
        ),
        // Elixir's special forms and Kernel macros, ExUnit's too: what a
        // module is written with, never a call of the project's.
        // dart:core's top-level functions, and package:test's (with
        // flutter_test's and bloc_test's) runner and its distinctive
        // matchers: a test's `expect(…)` is the library's.
        Lang::Dart => matches!(
            name,
            "print"
                | "identical"
                | "identityHashCode"
                | "expect"
                | "expectLater"
                | "expectAsync0"
                | "expectAsync1"
                | "expectAsync2"
                | "test"
                | "group"
                | "setUp"
                | "setUpAll"
                | "tearDown"
                | "tearDownAll"
                | "addTearDown"
                | "fail"
                | "markTestSkipped"
                | "printOnFailure"
                | "testWidgets"
                | "blocTest"
                | "isA"
                | "throwsA"
                | "completion"
                | "emits"
                | "emitsInOrder"
                | "emitsDone"
                | "emitsError"
                | "emitsThrough"
                | "neverEmits"
        ),
        Lang::Elixir => matches!(
            name,
            "alias"
                | "import"
                | "use"
                | "require"
                | "if"
                | "unless"
                | "case"
                | "cond"
                | "with"
                | "for"
                | "receive"
                | "try"
                | "quote"
                | "unquote"
                | "raise"
                | "reraise"
                | "throw"
                | "send"
                | "defmodule"
                | "defstruct"
                | "defexception"
                | "defimpl"
                | "defprotocol"
                | "defdelegate"
                | "defoverridable"
                | "describe"
                | "test"
                | "setup"
                | "assert"
                | "refute"
                | "is_nil"
                | "to_string"
                | "inspect"
                | "length"
                | "elem"
                | "hd"
                | "tl"
                | "map_size"
                | "setup_all"
                | "assert_raise"
                | "assert_receive"
                | "assert_received"
                | "refute_receive"
                | "refute_received"
                | "assert_in_delta"
                | "catch_error"
                | "catch_exit"
                | "catch_throw"
                | "flunk"
                | "doctest"
                | "on_exit"
                | "start_supervised"
                | "start_supervised!"
                | "is_atom"
                | "is_binary"
                | "is_list"
                | "is_map"
                | "is_integer"
                | "is_function"
                | "is_struct"
                | "is_tuple"
                | "is_boolean"
                | "is_float"
                | "is_number"
                | "is_pid"
                | "apply"
                | "spawn"
                | "spawn_link"
                | "self"
                | "struct"
                | "struct!"
                | "get_in"
                | "put_in"
                | "update_in"
                | "pop_in"
                | "then"
                | "tap"
                | "dbg"
                | "put_elem"
                | "tuple_size"
                | "byte_size"
                | "div"
                | "rem"
                | "defguard"
        ),
        Lang::Rust
        | Lang::C
        | Lang::Cpp
        | Lang::Java
        | Lang::Json
        | Lang::Bash
        | Lang::Yaml
        | Lang::Toml
        | Lang::Html
        | Lang::Css
        | Lang::Scss
        | Lang::Less
        | Lang::Vue
        | Lang::Svelte
        | Lang::Astro
        | Lang::Zig
        | Lang::Sql
        | Lang::Lean
        | Lang::CSharp => false,
    }
}

/// Resolve the caller a call site names — its enclosing function, by name —
/// to that function's definition in `file`.
///
/// Where several definitions share the name, the one the call is IN: the
/// innermost whose definition span (`bodies`, see [`FileCalls::bodies`])
/// holds the call's line — the rule `outline::Analysis::calls_made_by`
/// reads a function's calls by, so the graph and the reading context agree.
/// "The nearest preceding same-named definition" did not: a call made after
/// a nested same-name function closed went to that nested one. Without a
/// span that holds the call, that nearest preceding definition still
/// decides (falling back to the first when none precedes the call).
///
/// `callers` holds each file's definitions by name, in line order
/// ([`Defs::callers`]). The innermost body is searched for among the closest
/// [`MAX_CALLER_SCAN`] definitions before the call: past them, the closest
/// one stands in.
fn resolve_caller(
    callers: &HashMap<(&Path, &str), Vec<usize>>,
    file: &Path,
    name: &str,
    call_line: usize,
    nodes: &[SymNode],
    bodies: &HashMap<usize, (usize, usize)>,
) -> Option<usize> {
    let ids = callers.get(&(file, name))?;
    let first = *ids.first()?;
    // The ones defined at or before the call: only those can hold it.
    let before = &ids[..ids.partition_point(|&id| nodes[id].line <= call_line)];
    let best = before.last().copied();
    let mut innermost: Option<(usize, usize)> = None; // (id, span length)
    for &id in before.iter().rev().take(MAX_CALLER_SCAN) {
        if let Some(&(lo, hi)) = bodies.get(&nodes[id].line)
            && (lo..=hi).contains(&call_line)
            && innermost.is_none_or(|(_, len)| hi - lo < len)
        {
            innermost = Some((id, hi - lo));
        }
    }
    innermost.map(|(id, _)| id).or(best).or(Some(first))
}

/// How many same-named definitions before a call [`resolve_caller`] looks
/// through for the body holding it. Real nesting reuses a name a few times;
/// a file of thousands of them made each call cost their number.
const MAX_CALLER_SCAN: usize = 64;

#[cfg(test)]
mod tests {
    use super::*;

    /// A component rendered is a call of it: `<Layout>` and `<Button/>`
    /// from `App`, `<Select.Item>` the `Item` of `Select`; a page's own
    /// element (`<div>`) is none, and `new Store()` calls `Store`.
    #[test]
    fn jsx_renders_are_calls() {
        let src = "function App() {\n  const store = new Store();\n  return (\n    <Layout title=\"x\">\n      <div>\n        <Button onClick={() => go()} />\n        <Select.Item value=\"a\" />\n      </div>\n    </Layout>\n  );\n}\n";
        let calls = calls_of(src, "tsx");
        let got: Vec<(&str, &str, bool)> = calls
            .iter()
            .map(|c| {
                (
                    c.caller.as_deref().unwrap_or(""),
                    c.callee.as_str(),
                    c.method,
                )
            })
            .collect();
        for want in [
            ("App", "Store", false),
            ("App", "Layout", false),
            ("App", "Button", false),
            ("App", "Item", true),
            ("App", "go", false),
        ] {
            assert!(got.contains(&want), "{want:?} in {got:?}");
        }
        assert!(
            !got.iter().any(|(_, callee, _)| *callee == "div"),
            "{got:?}"
        );
        assert_eq!(
            got.iter().filter(|(_, c, _)| *c == "Layout").count(),
            1,
            "its opening tag once"
        );
    }

    /// A JavaScript runtime's objects call none of the project's functions:
    /// `console.log`, `JSON.parse` and `document.querySelector` reach no
    /// project `log`, `parse` or `querySelector`, though the project has
    /// them; a call on the project's own `logger` reaches its `log`.
    #[test]
    fn js_globals_call_none_of_the_projects_functions() {
        let util = "export function log(m) {}\nexport function parse(s) {}\n";
        let app = "import { log } from './util';\nconst logger = { log };\nfunction run() {\n  console.log('a');\n  JSON.parse('{}');\n  document.querySelector('#x');\n  logger.log('b');\n}\n";
        let util_path = PathBuf::from("/p/util.js");
        let app_path = PathBuf::from("/p/app.js");
        let sources: Vec<(PathBuf, String)> = vec![
            (util_path.clone(), util.into()),
            (app_path.clone(), app.into()),
        ];
        let mut defs = Vec::new();
        for (file, src) in &sources {
            for l in crate::outline::extract_located(src, "javascript") {
                defs.push(Def {
                    name: l.symbol.name,
                    kind: l.symbol.kind,
                    file: file.clone(),
                    line: l.symbol.line,
                });
            }
        }
        let scope: HashMap<PathBuf, HashSet<PathBuf>> =
            HashMap::from([(app_path.clone(), HashSet::from([util_path.clone()]))]);
        let graph = ProjectCallGraph::build(defs, &sources, &scope);
        let callers_of = |name: &str| -> Vec<String> {
            let id = graph.id_of(&util_path, name).expect(name);
            graph
                .node(id)
                .callers
                .iter()
                .map(|&c| graph.node(c).name.clone())
                .collect()
        };
        assert!(
            callers_of("parse").is_empty(),
            "JSON.parse is no project parse"
        );
        assert_eq!(callers_of("log"), ["run"], "logger.log is");
    }

    #[test]
    fn extracts_rust_call_sites_with_enclosing_fn() {
        let src = "\
fn helper() {}
fn caller() {
    helper();
    let v = Vec::<u8>::with_capacity(4);
    self.method_call();
}
";
        let calls = calls_of(src, "rust");
        // helper() call inside caller.
        let helper = calls.iter().find(|c| c.callee == "helper").unwrap();
        assert_eq!(helper.caller.as_deref(), Some("caller"));
        // Trailing identifier of a scoped/generic call.
        assert!(calls.iter().any(|c| c.callee == "with_capacity"));
        // Method call resolves to the trailing name.
        assert!(calls.iter().any(|c| c.callee == "method_call"));
    }

    #[test]
    fn turbofish_callee_is_the_function_not_the_type_arg() {
        let src = "\
fn caller() {
    let n = parse::<i32>(\"1\");
    let v = data.collect::<Vec<_>>();
}
";
        let calls = calls_of(src, "rust");
        let names: Vec<&str> = calls.iter().map(|c| c.callee.as_str()).collect();
        assert!(names.contains(&"parse"), "turbofish free call: {names:?}");
        assert!(
            names.contains(&"collect"),
            "turbofish method call: {names:?}"
        );
        assert!(
            !names.contains(&"i32"),
            "type arg must not be the callee: {names:?}"
        );
        // The generic method call is still flagged as a method (no unique fallback).
        assert!(calls.iter().find(|c| c.callee == "collect").unwrap().method);
    }

    #[test]
    fn extracts_dart_call_sites_with_enclosing_fn() {
        // A class method and a top-level function, each calling others. Dart's
        // signature/body split means the caller name must be recovered from the
        // signature preceding each `function_body`.
        let src = "\
class Greeter {
  void hello() {
    _build();
    print('hi');
  }
  void _build() {}
}
void main() {
  Greeter().hello();
}
";
        let calls = calls_of(src, "dart");
        // Calls inside the method are attributed to `hello`, not dropped.
        assert!(
            calls
                .iter()
                .any(|c| c.caller.as_deref() == Some("hello") && c.callee == "_build"),
            "hello -> _build: {calls:?}"
        );
        assert!(
            calls
                .iter()
                .any(|c| c.caller.as_deref() == Some("hello") && c.callee == "print"),
            "hello -> print: {calls:?}"
        );
        // The top-level function's method call is attributed to `main`.
        assert!(
            calls
                .iter()
                .any(|c| c.caller.as_deref() == Some("main") && c.callee == "hello"),
            "main -> hello: {calls:?}"
        );
        // `Greeter().hello()` is a method call, so it never takes the "only
        // definition of that name" fallback; a bare `_build()` is not.
        let method = |callee: &str| calls.iter().find(|c| c.callee == callee).unwrap().method;
        assert!(method("hello"), "{calls:?}");
        assert!(!method("_build"), "{calls:?}");
    }

    /// A body's name comes from the signature before it, carried by the
    /// walk: looked up among its parent's children, a file of many
    /// top-level functions cost their number per function.
    #[test]
    fn a_wide_dart_file_is_named_in_one_walk() {
        const N: usize = 20_000;
        let src: String = (0..N)
            .map(|i| format!("void f{i}() {{ g{i}(); }}\n"))
            .collect();
        let started = std::time::Instant::now();
        let calls = calls_of(&src, "dart");
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        assert_eq!(calls.len(), N);
        for i in [0, N / 2, N - 1] {
            let call = calls.iter().find(|c| c.callee == format!("g{i}"));
            assert_eq!(
                call.and_then(|c| c.caller.as_deref()),
                Some(format!("f{i}").as_str())
            );
        }
    }

    #[test]
    fn dart_receiver_calls_are_method_calls() {
        let src = "void f(List<int> xs, Cart? c) {\n  xs.add(1);\n  c?.checkout();\n}\n";
        let calls = calls_of(src, "dart");
        for callee in ["add", "checkout"] {
            let call = calls.iter().find(|c| c.callee == callee);
            assert!(call.is_some_and(|c| c.method), "{callee}: {calls:?}");
        }
    }

    #[test]
    fn arrow_functions_are_enclosing_scopes() {
        let src = "const handler = () => { doThing(); };\nfunction main() { other(); }\n";
        let calls = calls_of(src, "typescript");
        // The call inside the arrow is attributed to its binding name, not dropped.
        let do_thing = calls.iter().find(|c| c.callee == "doThing").unwrap();
        assert_eq!(do_thing.caller.as_deref(), Some("handler"));
        assert_eq!(
            calls
                .iter()
                .find(|c| c.callee == "other")
                .unwrap()
                .caller
                .as_deref(),
            Some("main")
        );
    }

    #[test]
    fn python_and_js_call_sites() {
        let py = "def a():\n    b()\n    obj.c()\n";
        let calls = calls_of(py, "python");
        assert_eq!(
            calls
                .iter()
                .find(|c| c.callee == "b")
                .unwrap()
                .caller
                .as_deref(),
            Some("a")
        );
        assert!(calls.iter().any(|c| c.callee == "c"));

        let js = "function f() { g(); this.h(); new Widget(); }";
        let calls = calls_of(js, "typescript");
        assert_eq!(
            calls
                .iter()
                .find(|c| c.callee == "g")
                .unwrap()
                .caller
                .as_deref(),
            Some("f")
        );
        assert!(calls.iter().any(|c| c.callee == "Widget"));
    }

    fn def(name: &str, file: &str, line: usize) -> Def {
        Def {
            name: name.into(),
            kind: "function".into(),
            file: PathBuf::from(file),
            line,
        }
    }

    /// Lean's calls: across the files a file imports, a namespace's name
    /// called by its namespace (`Voice.toModel`, `Voice` being
    /// `Clew.Voice`), and nothing for a name in a namespace the project has
    /// none of (`Nat.succ` is no project `succ`).
    #[test]
    fn lean_calls_link_across_imports_and_namespaces() {
        let basic = PathBuf::from("/p/Demo/Basic.lean");
        let voice = PathBuf::from("/p/Demo/Voice.lean");
        let usef = PathBuf::from("/p/Demo/Use.lean");
        let sources = vec![
            (
                basic.clone(),
                "def double (n : Nat) : Nat := n + n\n\ndef succ (n : Nat) : Nat := n\n".to_string(),
            ),
            (
                voice.clone(),
                "namespace Clew.Voice\n\ndef toModel (n : Nat) : Nat := n\n\nend Clew.Voice\n".to_string(),
            ),
            (
                usef.clone(),
                "import Demo.Basic\nimport Demo.Voice\n\ndef quad (n : Nat) : Nat :=\n  double (double n)\n\ndef speak (n : Nat) : Nat :=\n  Voice.toModel (Nat.succ n)\n".to_string(),
            ),
        ];
        let mut defs = Vec::new();
        for (file, src) in &sources {
            for l in crate::outline::extract_located(src, "lean") {
                defs.push(Def {
                    name: l.symbol.name,
                    kind: l.symbol.kind,
                    file: file.clone(),
                    line: l.symbol.line,
                });
            }
        }
        let scope: HashMap<PathBuf, HashSet<PathBuf>> =
            HashMap::from([(usef.clone(), HashSet::from([basic.clone(), voice.clone()]))]);
        let g = ProjectCallGraph::build(defs, &sources, &scope);
        let callees = |name: &str| -> Vec<String> {
            let id = g.id_of(&usef, name).expect(name);
            let mut out: Vec<String> = g
                .node(id)
                .callees
                .iter()
                .map(|&c| g.node(c).name.clone())
                .collect();
            out.sort();
            out
        };
        assert_eq!(callees("quad"), ["double"]);
        assert_eq!(callees("speak"), ["toModel"]);
    }

    #[test]
    fn builds_graph_and_finds_hubs_and_uncalled() {
        let defs = vec![
            def("main", "/p/a.rs", 1),
            def("used", "/p/a.rs", 10),
            def("lonely", "/p/b.rs", 1),
        ];
        let sources = vec![
            (
                PathBuf::from("/p/a.rs"),
                "fn main() {\n    used();\n    used();\n}\nfn used() {}\n".to_string(),
            ),
            (PathBuf::from("/p/b.rs"), "fn lonely() {}\n".to_string()),
        ];
        let g = ProjectCallGraph::build(defs, &sources, &HashMap::new());
        assert_eq!(g.node_count(), 3);

        // `used` is called (edge exists, deduped to one); it is a hub.
        let hubs = g.most_called(10);
        assert_eq!(hubs.len(), 1);
        assert_eq!(g.node(hubs[0]).name, "used");
        assert_eq!(g.node(hubs[0]).caller_count(), 1); // main→used, deduped

        // main and lonely have no callers.
        let uncalled: Vec<&str> = g
            .uncalled()
            .iter()
            .map(|&i| g.node(i).name.as_str())
            .collect();
        assert!(uncalled.contains(&"main"));
        assert!(uncalled.contains(&"lonely"));
        assert!(!uncalled.contains(&"used"));
    }

    /// A name defined many times in one file is ambiguous: its calls link to
    /// none of the definitions. Linking all of them made a file of N
    /// same-named functions calling that name N² edges.
    #[test]
    fn a_name_defined_many_times_resolves_to_none() {
        const N: usize = 3_000;
        let src: String = (0..N).map(|_| "function f(){f()}\n").collect();
        let defs: Vec<_> = (0..N).map(|i| def("f", "/p/a.js", i + 1)).collect();
        let sources = vec![(PathBuf::from("/p/a.js"), src)];
        let started = std::time::Instant::now();
        let g = ProjectCallGraph::build(defs, &sources, &HashMap::new());
        assert!(started.elapsed() < std::time::Duration::from_secs(20));
        assert_eq!(g.edge_count(), 0);
        // A small overload set still resolves.
        let defs = vec![
            def("caller", "/p/b.rs", 1),
            def("g", "/p/b.rs", 4),
            def("g", "/p/b.rs", 5),
        ];
        let sources = vec![(
            PathBuf::from("/p/b.rs"),
            "fn caller() {\n    g();\n}\nfn g() {}\nfn g() {}\n".to_string(),
        )];
        let g = ProjectCallGraph::build(defs, &sources, &HashMap::new());
        assert_eq!(g.edge_count(), 2);
    }

    /// Real overload sets, and a crate root's modules each defining `new`,
    /// are not ambiguous: their calls link to every definition.
    #[test]
    fn an_overload_set_or_a_crate_roots_modules_resolve_to_every_definition() {
        // Twelve overloads of `of` in one file, Guava style.
        let mut defs = vec![def("caller", "/p/A.java", 1)];
        defs.extend((0..12).map(|i| def("of", "/p/A.java", i + 3)));
        let src = format!(
            "class A {{\n void caller() {{ of(); }}\n{}}}\n",
            "  void of() {}\n".repeat(12)
        );
        let sources = vec![(PathBuf::from("/p/A.java"), src)];
        let g = ProjectCallGraph::build(defs, &sources, &HashMap::new());
        assert_eq!(g.edge_count(), 12);

        // `lib.rs` declares nine modules, each with a `new`.
        let mut defs = vec![def("start", "/p/lib.rs", 1)];
        let mut sources = vec![(
            PathBuf::from("/p/lib.rs"),
            "fn start() {\n    new();\n}\n".to_string(),
        )];
        let mut scope: HashMap<PathBuf, HashSet<PathBuf>> = HashMap::new();
        for m in 0..9 {
            let file = format!("/p/m{m}.rs");
            defs.push(def("new", &file, 1));
            sources.push((PathBuf::from(&file), "fn new() {}\n".to_string()));
            scope
                .entry(PathBuf::from("/p/lib.rs"))
                .or_default()
                .insert(PathBuf::from(file));
        }
        let g = ProjectCallGraph::build(defs, &sources, &scope);
        assert_eq!(g.edge_count(), 9);
    }

    /// What a call resolves to is looked up, not searched for: many calls
    /// of a name defined in many files, or a file of many functions, cost
    /// neither calls × definitions nor calls × the file's functions.
    #[test]
    fn many_calls_of_a_common_name_resolve_at_once() {
        const FILES: usize = 3_000;
        const CALLS: usize = 3_000;
        let mut defs = Vec::new();
        let mut sources = Vec::new();
        for f in 0..FILES {
            let file = format!("/p/m{f}.rs");
            defs.push(def("new", &file, 1));
            sources.push((PathBuf::from(&file), "fn new() {}\n".to_string()));
        }
        let mut src = String::new();
        for c in 0..CALLS {
            defs.push(def(&format!("c{c}"), "/p/main.rs", c + 1));
            src.push_str(&format!("fn c{c}() {{ new(); }}\n"));
        }
        sources.push((PathBuf::from("/p/main.rs"), src));
        let started = std::time::Instant::now();
        let g = ProjectCallGraph::build(defs, &sources, &HashMap::new());
        assert!(started.elapsed() < std::time::Duration::from_secs(20));
        assert_eq!(g.edge_count(), 0, "a name defined everywhere is no one's");
    }

    #[test]
    fn self_recursion_is_not_a_caller() {
        let defs = vec![def("fac", "/p/a.rs", 1)];
        let sources = vec![(
            PathBuf::from("/p/a.rs"),
            "fn fac(n: u64) -> u64 {\n    fac(n - 1)\n}\n".to_string(),
        )];
        let g = ProjectCallGraph::build(defs, &sources, &HashMap::new());
        // The self-call is no caller, so `fac` still counts as uncalled; it
        // is among what `fac` calls.
        assert_eq!(g.node(g.uncalled()[0]).name, "fac");
        assert_eq!(g.callees_of(0), [0]);
        assert!(g.callee_keys().values().all(Vec::is_empty));
    }

    #[test]
    fn method_call_does_not_spray_across_unimported_files() {
        // Only one project function is named `get`, but the call `x.get()` is a
        // method on some type in an unrelated file that doesn't import it.
        let defs = vec![
            def("get", "/p/store.rs", 1),
            def("caller", "/p/other.rs", 1),
        ];
        let sources = vec![
            (PathBuf::from("/p/store.rs"), "fn get() {}\n".to_string()),
            (
                PathBuf::from("/p/other.rs"),
                "fn caller() {\n    let m = make();\n    m.get();\n}\n".to_string(),
            ),
        ];
        // other.rs does NOT import store.rs.
        let g = ProjectCallGraph::build(defs, &sources, &HashMap::new());
        // The method call is not attributed to the unrelated `get`.
        assert_eq!(g.node(id_named(&g, "get")).caller_count(), 0);
    }

    #[test]
    fn call_resolves_to_an_imported_file() {
        // `caller` in a.rs calls a free function `helper` defined in b.rs, which
        // a.rs imports — so it resolves across files.
        let defs = vec![def("caller", "/p/a.rs", 1), def("helper", "/p/b.rs", 1)];
        let sources = vec![
            (
                PathBuf::from("/p/a.rs"),
                "fn caller() {\n    helper();\n}\n".to_string(),
            ),
            (PathBuf::from("/p/b.rs"), "fn helper() {}\n".to_string()),
        ];
        let mut scope = HashMap::new();
        scope.insert(
            PathBuf::from("/p/a.rs"),
            HashSet::from([PathBuf::from("/p/b.rs")]),
        );
        let g = ProjectCallGraph::build(defs, &sources, &scope);
        assert_eq!(g.node(id_named(&g, "helper")).caller_count(), 1);

        // Without the import in scope, a free call to a unique name still links
        // (globally-unique fallback), but a shared name would not.
        let g2 = ProjectCallGraph::build(
            vec![def("caller", "/p/a.rs", 1), def("helper", "/p/b.rs", 1)],
            &sources,
            &HashMap::new(),
        );
        assert_eq!(g2.node(id_named(&g2, "helper")).caller_count(), 1);
    }

    fn id_named(g: &ProjectCallGraph, name: &str) -> usize {
        (0..g.node_count())
            .find(|&i| g.node(i).name == name)
            .unwrap()
    }

    #[test]
    fn builtin_calls_do_not_link_to_same_named_definitions() {
        // A Go `make(...)` builtin call must not resolve to a project function
        // named `make` (here in a vendored JS bundle) — that inflated the graph.
        let defs = vec![def("run", "/p/a.go", 1), def("make", "/p/vendor.js", 1)];
        let sources = vec![
            (
                PathBuf::from("/p/a.go"),
                "func run() {\n\tmake([]int, 0)\n}\n".to_string(),
            ),
            (
                PathBuf::from("/p/vendor.js"),
                "function make() {}\n".to_string(),
            ),
        ];
        let g = ProjectCallGraph::build(defs, &sources, &HashMap::new());
        assert_eq!(g.node(id_named(&g, "make")).caller_count(), 0);
    }

    #[test]
    fn js_builtin_calls_do_not_link_to_a_same_named_polyfill() {
        // A bare `parseInt(...)` is a JS runtime builtin, not this project's
        // `parseInt` polyfill defined in a helper file.
        let defs = vec![
            def("run", "/p/a.js", 1),
            def("parseInt", "/p/polyfill.js", 1),
        ];
        let sources = vec![
            (
                PathBuf::from("/p/a.js"),
                "function run() {\n  parseInt('10', 10)\n}\n".to_string(),
            ),
            (
                PathBuf::from("/p/polyfill.js"),
                "function parseInt(s, r) { return 0 }\n".to_string(),
            ),
        ];
        let g = ProjectCallGraph::build(defs, &sources, &HashMap::new());
        assert_eq!(g.node(id_named(&g, "parseInt")).caller_count(), 0);
    }

    #[test]
    fn lone_global_fallback_is_restricted_to_the_same_language() {
        // A Go `Widget()` call has one globally-unique definition — but it's in a
        // JS file. A Go call can't invoke a JS function by bare name, so no edge.
        let defs = vec![def("run", "/p/a.go", 1), def("Widget", "/p/vendor.js", 1)];
        let sources = vec![
            (
                PathBuf::from("/p/a.go"),
                "func run() {\n\tWidget()\n}\n".to_string(),
            ),
            (
                PathBuf::from("/p/vendor.js"),
                "function Widget() {}\n".to_string(),
            ),
        ];
        let g = ProjectCallGraph::build(defs, &sources, &HashMap::new());
        assert_eq!(g.node(id_named(&g, "Widget")).caller_count(), 0);
    }

    #[test]
    fn graph_from_sym_edges_maps_and_survives_line_shifts() {
        let defs = vec![
            def("a", "/p/x.rs", 1),
            def("b", "/p/x.rs", 5),
            def("c", "/p/y.rs", 1),
        ];
        let key = |file: &str, name: &str| (PathBuf::from(file), name.to_string(), 0u32);
        let edges: SymEdges = HashSet::from([
            (key("/p/x.rs", "a"), key("/p/x.rs", "b")),    // a → b
            (key("/p/y.rs", "c"), key("/p/x.rs", "a")),    // c → a
            (key("/p/x.rs", "a"), key("/p/gone.rs", "z")), // dangling → dropped
        ]);
        let g = ProjectCallGraph::graph_from_sym_edges(defs, &edges);
        assert_eq!(g.node(id_named(&g, "b")).caller_count(), 1);
        assert_eq!(g.node(id_named(&g, "a")).caller_count(), 1);
        assert_eq!(g.edge_count(), 2, "dangling edge dropped");

        // The same edges resolve even after `a`/`b` move to different lines —
        // the key is (file, name, ordinal), never the line.
        let shifted = vec![
            def("a", "/p/x.rs", 40),
            def("b", "/p/x.rs", 88),
            def("c", "/p/y.rs", 3),
        ];
        let g2 = ProjectCallGraph::graph_from_sym_edges(shifted, &edges);
        assert_eq!(g2.edge_count(), 2);
        assert_eq!(g2.node(id_named(&g2, "b")).caller_count(), 1);
    }

    #[test]
    fn from_callable_defs_builds_from_explicit_edges() {
        let defs = vec![
            def("a", "/p/x.rs", 1),
            def("b", "/p/x.rs", 5),
            def("c", "/p/y.rs", 1),
        ];
        // a→b, a→c (explicit, e.g. LSP-resolved), plus b calling itself.
        let edges = HashSet::from([(0, 1), (0, 2), (1, 1)]);
        let g = ProjectCallGraph::from_callable_defs(defs, edges);
        assert_eq!(g.node_count(), 3);
        assert_eq!(g.node(id_named(&g, "a")).callee_count(), 2);
        // b calls itself, and is no caller of its own: a is its one caller.
        let b = id_named(&g, "b");
        assert_eq!(g.node(b).caller_count(), 1);
        assert_eq!(g.callees_of(b), [b]);
        assert_eq!(g.node(id_named(&g, "c")).caller_count(), 1);
        assert_eq!(g.edge_count(), 3);
    }

    #[test]
    fn paths_from_entries_are_shortest_ordered_bounded_and_end_at_entries() {
        // main → run → handle → work; route → handle; test_work → work;
        // work → work (recursion); helper → main (a caller of an entry).
        let names = [
            "main",
            "run",
            "handle",
            "work",
            "route",
            "test_work",
            "helper",
            "lonely",
        ];
        let defs: Vec<Def> = names
            .iter()
            .enumerate()
            .map(|(i, n)| def(n, "a.rs", i + 1))
            .collect();
        let edges: HashSet<(usize, usize)> =
            [(0, 1), (1, 2), (2, 3), (4, 2), (5, 3), (3, 3), (6, 0)]
                .into_iter()
                .collect();
        let g = ProjectCallGraph::from_callable_defs(defs, edges);
        let class = |n: &SymNode| match n.name.as_str() {
            "main" | "route" => Some(0),
            "test_work" => Some(1),
            _ => None,
        };
        let name = |chain: &[usize]| {
            chain
                .iter()
                .map(|&i| g.node(i).name.as_str())
                .collect::<Vec<_>>()
                .join(" > ")
        };
        let work = g.id_of(Path::new("a.rs"), "work").unwrap();
        let paths = g.paths_from_entries(work, class, 8, 12);
        assert_eq!(
            paths.iter().map(|p| name(p)).collect::<Vec<_>>(),
            [
                "route > handle > work",
                "main > run > handle > work",
                "test_work > work"
            ]
        );
        // The limit and the depth bound each cut the list.
        assert_eq!(g.paths_from_entries(work, class, 1, 12).len(), 1);
        assert_eq!(
            g.paths_from_entries(work, class, 8, 2)
                .iter()
                .map(|p| name(p))
                .collect::<Vec<_>>(),
            ["route > handle > work", "test_work > work"]
        );
        // An entry itself, and a function nothing ranked reaches, have none.
        let main = g.id_of(Path::new("a.rs"), "main").unwrap();
        assert!(g.paths_from_entries(main, class, 8, 12).is_empty());
        let lonely = g.id_of(Path::new("a.rs"), "lonely").unwrap();
        assert!(g.paths_from_entries(lonely, class, 8, 12).is_empty());
        assert!(g.paths_from_entries(999, class, 8, 12).is_empty());
    }

    #[test]
    fn last_identifier_handles_paths_and_rejects_numbers() {
        assert_eq!(last_identifier("self.foo.bar").as_deref(), Some("bar"));
        assert_eq!(
            last_identifier("Vec::<u8>::with_capacity").as_deref(),
            Some("with_capacity")
        );
        assert_eq!(last_identifier("42").as_deref(), None);
    }
}

#[cfg(test)]
mod edge_tests {
    use super::*;

    #[test]
    fn methods_named_after_builtins_keep_their_project_edges() {
        let fixtures = [
            (
                "/p/reader.py",
                "class Reader:\n    def open(self):\n        pass\n    def run(self):\n        self.open()\n",
                "open",
            ),
            (
                "/p/client.ts",
                "class Client {\n  fetch() {}\n  run() { this.fetch(); }\n}\n",
                "fetch",
            ),
            (
                "/p/client.js",
                "class Client {\n  fetch() {}\n  run() { this.fetch(); }\n}\n",
                "fetch",
            ),
            (
                "/p/reader.go",
                "package p\ntype Reader struct {}\nfunc (r Reader) len() int { return 1 }\nfunc (r Reader) run() { r.len() }\n",
                "len",
            ),
        ];
        for (file, source, method) in fixtures {
            let g = graph(&[(file, source)]);
            assert!(
                has(&g, &format!("{file}:run"), &format!("{file}:{method}")),
                "{file}: {:?}",
                edges(&g)
            );
            assert_eq!(g.edge_count(), 1);
        }
    }

    #[test]
    fn rust_raw_calls_match_the_outline_spelling() {
        let file = "/p/raw.rs";
        let source = "fn r#type<T>() {}\nstruct Reader;\nimpl Reader {\n    fn r#match(&self) {}\n}\nfn r#loop() {\n    r#type::<()>();\n    crate::r#type::<()>();\n}\nfn main() {\n    r#loop();\n    Reader.r#match();\n}\n";
        let calls = calls_of(source, "rust");
        assert_eq!(calls.iter().filter(|c| c.callee == "r#type").count(), 2);
        assert!(calls.iter().any(|c| c.callee == "r#match" && c.method));
        let g = graph(&[(file, source)]);
        for (from, to) in [
            ("main", "r#loop"),
            ("r#loop", "r#type"),
            ("main", "r#match"),
        ] {
            assert!(
                has(&g, &format!("{file}:{from}"), &format!("{file}:{to}")),
                "{:?}",
                edges(&g)
            );
        }
        assert_eq!(g.edge_count(), 3);
    }

    #[test]
    fn raw_and_plain_rust_spellings_resolve_to_the_same_item() {
        let file = "/p/spelling.rs";
        let source = "fn r#first() {}\nfn second() {}\nfn run() { first(); r#first(); second(); r#second(); }\n";
        let g = graph(&[(file, source)]);
        assert!(has(&g, "/p/spelling.rs:run", "/p/spelling.rs:r#first"));
        assert!(has(&g, "/p/spelling.rs:run", "/p/spelling.rs:second"));
        assert_eq!(g.edge_count(), 2, "{:?}", edges(&g));
    }

    /// Defs exactly as the indexers produce them: every callable in each
    /// file's outline.
    fn defs_of(files: &[(&str, &str)]) -> Vec<Def> {
        let mut defs = Vec::new();
        for (file, src) in files {
            let lang = crate::highlight::detect(Path::new(file)).unwrap();
            for s in crate::outline::extract(src, lang) {
                defs.push(Def {
                    name: s.name,
                    kind: s.kind,
                    file: PathBuf::from(file),
                    line: s.line,
                });
            }
        }
        defs
    }

    fn graph(files: &[(&str, &str)]) -> ProjectCallGraph {
        let sources: Vec<(PathBuf, String)> = files
            .iter()
            .map(|(f, s)| (PathBuf::from(f), s.to_string()))
            .collect();
        ProjectCallGraph::build(defs_of(files), &sources, &HashMap::new())
    }

    /// A function only tests call is called — by each test, in every
    /// language whose tests are blocks a runner's call opens rather than
    /// functions: ExUnit's `test "adds" do`, RSpec's `it`, Zig's `test`
    /// (a declaration's too), MUnit's `test("adds") {`, Kotest's,
    /// GoogleTest's `TEST(Cart, Adds)`, Catch2's `TEST_CASE`, package:test's
    /// `test('adds', () {` (in `main`) and hspec's `it "adds" $` (in
    /// `spec`). Such a call had no caller — or `main`'s, `spec`'s — and the
    /// function read "No callers", listed as possibly dead. The tests call;
    /// none is called, nor counted among the functions, nor uncalled.
    #[test]
    fn a_test_block_is_the_caller_of_what_it_calls() {
        let cases: &[(&str, &str, &str, &str, &[&str])] = &[
            (
                "/p/lib/cart.ex",
                "defmodule Cart do\n  def add(a, b), do: a + b\nend\n",
                "/p/test/cart_test.exs",
                "defmodule CartTest do\n  use ExUnit.Case\n  describe \"cart\" do\n    test \"adds\" do\n      assert Cart.add(2, 3) == 5\n    end\n  end\n  test \"breaks\", %{x: _} do\n    Cart.add(1, 1)\n  end\nend\n",
                &["adds@4", "breaks@8"],
            ),
            (
                "/p/lib/cart.rb",
                "def add(a, b)\n  a + b\nend\n",
                "/p/spec/cart_spec.rb",
                "RSpec.describe Cart do\n  before { add(0, 0) }\n  it 'adds' do\n    expect(add(2, 3)).to eq(5)\n  end\nend\n",
                &["Cart@1", "adds@3"],
            ),
            (
                "/p/cart.zig",
                "fn add(a: i32, b: i32) i32 {\n    return a + b;\n}\n",
                "/p/cart_test.zig",
                "test \"adds\" {\n    _ = add(2, 3);\n}\ntest add {\n    _ = add(1, 1);\n}\n",
                &["add@4", "adds@1"],
            ),
            (
                "/p/src/main/scala/Cart.scala",
                "object Cart {\n  def add(a: Int, b: Int): Int = a + b\n}\n",
                "/p/src/test/scala/CartSuite.scala",
                "class CartSuite extends munit.FunSuite {\n  test(\"adds\") {\n    assertEquals(Cart.add(2, 3), 5)\n  }\n}\n",
                &["adds@2"],
            ),
            (
                "/p/src/main/kotlin/Cart.kt",
                "fun add(a: Int, b: Int): Int = a + b\n",
                "/p/src/test/kotlin/CartTest.kt",
                "class CartTest : FunSpec({\n  test(\"adds\") {\n    add(2, 3) shouldBe 5\n  }\n})\n",
                &["adds@2"],
            ),
            (
                "/p/cart.cpp",
                "int add(int a, int b) { return a + b; }\n",
                "/p/cart_test.cpp",
                "TEST(Cart, Adds) {\n  EXPECT_EQ(5, add(2, 3));\n}\nTEST_CASE(\"adds again\", \"[cart]\") {\n  REQUIRE(add(2, 2) == 4);\n}\n",
                &["Cart.Adds@1", "adds again@4"],
            ),
            (
                "/p/lib/cart.dart",
                "int add(int a, int b) => a + b;\n",
                "/p/test/cart_test.dart",
                "void main() {\n  group('cart', () {\n    test('adds', () {\n      expect(add(2, 3), 5);\n    });\n  });\n}\n",
                &["adds@3"],
            ),
            (
                "/p/src/Cart.hs",
                "module Cart where\nadd :: Int -> Int -> Int\nadd a b = a + b\n",
                "/p/test/CartSpec.hs",
                "module CartSpec where\nspec :: Spec\nspec = describe \"Cart\" $ do\n  it \"adds\" $ add 2 3 `shouldBe` 5\n",
                &["adds@4"],
            ),
        ];
        for &(lib, lib_src, test_file, test_src, want) in cases {
            // The test file reaches the library's, as the import graph has
            // it (an Elixir module named, a Scala object): what a qualified
            // call (`Cart.add`) resolves in.
            let files = [(lib, lib_src), (test_file, test_src)];
            let sources: Vec<(PathBuf, String)> = files
                .iter()
                .map(|(f, s)| (PathBuf::from(f), s.to_string()))
                .collect();
            let scope = HashMap::from([(
                PathBuf::from(test_file),
                HashSet::from([PathBuf::from(lib)]),
            )]);
            let g = ProjectCallGraph::build(defs_of(&files), &sources, &scope);
            assert_eq!(callers_named(&g, lib, "add", 0), want, "{test_file}");
            let tests = (0..g.node_count()).filter(|&i| calls_only(&g.node(i).kind));
            for t in tests {
                assert!(
                    g.node(t).callers.is_empty(),
                    "{test_file}: {:?} called",
                    g.node(t)
                );
                assert!(
                    !g.uncalled().contains(&t),
                    "{test_file}: a test is uncalled"
                );
            }
            assert!(
                !g.uncalled()
                    .contains(&g.id_of(Path::new(lib), "add").unwrap()),
                "{test_file}: add is uncalled"
            );
            assert!(g.function_count() < g.node_count(), "{test_file}");
        }
    }

    /// An Elixir module's function is reached as `Cart.add(…)` and an
    /// Erlang module's as `cart:add(…)` from any module, with no import —
    /// by the module's full name, or an Elixir alias's last part — and is
    /// the function of that module, not a same-named one of another. No
    /// call of another module's function had a callee: a function only
    /// other modules call (a test module's every call) read "No callers".
    #[test]
    fn a_modules_function_is_reached_by_the_modules_name() {
        let g = graph(&[
            (
                "/p/lib/shop/cart.ex",
                "defmodule Shop.Cart do\n  def add(a, b), do: a + b\n  defmodule Line do\n    def add(l), do: l\n  end\nend\n",
            ),
            (
                "/p/lib/other.ex",
                "defmodule Other do\n  def add(a, b), do: a - b\nend\n",
            ),
            (
                "/p/test/cart_test.exs",
                "defmodule CartTest do\n  alias Shop.Cart\n  test \"adds\" do\n    Cart.add(2, 3)\n  end\n  def line, do: Shop.Cart.Line.add(1)\nend\n",
            ),
        ]);
        assert_eq!(
            callers_named(&g, "/p/lib/shop/cart.ex", "add", 0),
            ["adds@3"]
        );
        assert_eq!(
            callers_named(&g, "/p/lib/shop/cart.ex", "add", 1),
            ["line@6"]
        );
        assert!(callers_named(&g, "/p/lib/other.ex", "add", 0).is_empty());

        let g = graph(&[
            (
                "/p/src/cart.erl",
                "-module(cart).\n-export([add/2]).\nadd(A, B) -> A + B.\n",
            ),
            ("/p/src/other.erl", "-module(other).\nadd(A, B) -> A - B.\n"),
            (
                "/p/test/cart_tests.erl",
                "-module(cart_tests).\nadd_test() -> 5 = cart:add(2, 3).\n",
            ),
        ]);
        assert_eq!(
            callers_named(&g, "/p/src/cart.erl", "add", 0),
            ["add_test@2"]
        );
        assert!(callers_named(&g, "/p/src/other.erl", "add", 0).is_empty());
    }

    /// A class's static function is reached by the class's name from the
    /// test beside it with no import, as the language reaches it: Java's,
    /// Kotlin's and Scala's in the same package, C#'s in the same
    /// namespace, Ruby's once required — Scala's from a test block, whose
    /// calls had no receiver, being in no function. Each had no callee.
    #[test]
    fn a_types_function_is_reached_by_the_types_name_from_beside_it() {
        // The library's file, its source and its function's name; the
        // test's file and source; who calls the function.
        type Case<'a> = (&'a str, &'a str, &'a str, &'a str, &'a str, &'a [&'a str]);
        let cases: &[Case] = &[
            (
                "/p/src/main/java/com/acme/Cart.java",
                "package com.acme;\npublic class Cart {\n    public static int add(int a, int b) { return a + b; }\n}\n",
                "add",
                "/p/src/test/java/com/acme/CartTest.java",
                "package com.acme;\nclass CartTest {\n    @Test\n    void adds() {\n        assertEquals(5, Cart.add(2, 3));\n    }\n}\n",
                &["adds@3"],
            ),
            (
                "/p/src/main/scala/shop/Cart.scala",
                "package shop\n\nobject Cart:\n  def total(items: List[Int]): Int =\n    items.sum\n",
                "total",
                "/p/src/test/scala/shop/CartSuite.scala",
                "package shop\n\nclass CartSuite extends munit.FunSuite:\n  test(\"adds\") {\n    assertEquals(Cart.total(List(2, 3)), 5)\n  }\n",
                &["adds@4"],
            ),
            (
                "/p/src/main/kotlin/shop/Cart.kt",
                "package shop\nobject Cart {\n    fun add(a: Int, b: Int): Int = a + b\n}\n",
                "add",
                "/p/src/test/kotlin/shop/CartTest.kt",
                "package shop\nclass CartTest {\n    @Test\n    fun adds() {\n        assertEquals(5, Cart.add(2, 3))\n    }\n}\n",
                &["adds@3"],
            ),
            (
                "/p/Cart.cs",
                "namespace Shop;\npublic static class Cart {\n  public static int Add(int a, int b) => a + b;\n}\n",
                "Add",
                "/p/Tests/CartTests.cs",
                "namespace Shop.Tests;\npublic class CartTests {\n  [Fact]\n  public void Adds() { Assert.Equal(5, Cart.Add(2, 3)); }\n}\n",
                &["Adds@3"],
            ),
            (
                "/p/lib/cart.rb",
                "class Cart\n  def self.add(a, b) = a + b\nend\n",
                "add",
                "/p/test/cart_test.rb",
                "class CartTest < Minitest::Test\n  def test_adds\n    assert_equal 5, Cart.add(2, 3)\n  end\nend\n",
                &["test_adds@2"],
            ),
        ];
        for &(lib, lib_src, name, test_file, test_src, want) in cases {
            let other = lib.replace("Cart", "Other").replace("cart", "other");
            let other_src = lib_src.replace("Cart", "Other");
            let g = graph(&[(lib, lib_src), (&other, &other_src), (test_file, test_src)]);
            assert_eq!(callers_named(&g, lib, name, 0), want, "{test_file}");
            assert!(
                callers_named(&g, &other, name, 0).is_empty(),
                "{test_file}: another type's {name}"
            );
        }
    }

    /// A function passed as a value is used where it is passed: Haskell's
    /// `foldr add 0 xs` and `show . add 1 <$> ys`, Erlang's `fun add/2`
    /// and `fun cart:add/2`, Elixir's `&add/2`. Only applications counted,
    /// a Haskell program's `main` reached nothing it folded with, and the
    /// function read as reached from no entry point. A function named as
    /// an application's function is that application's call, not two.
    /// A Haskell operator of the project's is called where it is applied
    /// — infix (`1 <+> 2`), in parentheses (`(<+>) 3 4`) or passed
    /// (`foldr (<+>) 0`) — and a function between backticks (``x `plus`
    /// y``) where it stands. An operator's own definition, `a <+> b = …`,
    /// calls nothing by its head, and the base library's operators (`+`,
    /// `$`) are no call of the project's.
    #[test]
    fn a_haskell_operator_and_a_backticked_function_are_called() {
        let src = "\
module Ops where

(<+>) :: Int -> Int -> Int
a <+> b = a + b

x `plus` y = x + y

infixed = 1 <+> 2
prefixed = (<+>) 3 4
folded xs = foldr (<+>) 0 xs
ticked = 5 `plus` 6
base = show $ 1 + 2
";
        let g = graph(&[("/p/src/Ops.hs", src)]);
        let edges = edges(&g);
        for from in ["infixed", "prefixed", "folded"] {
            assert!(
                has(&g, &format!("/p/src/Ops.hs:{from}"), "/p/src/Ops.hs:<+>"),
                "{from} -> <+>: {edges:?}"
            );
        }
        assert!(
            has(&g, "/p/src/Ops.hs:ticked", "/p/src/Ops.hs:plus"),
            "{edges:?}"
        );
        assert!(
            !has(&g, "/p/src/Ops.hs:<+>", "/p/src/Ops.hs:<+>"),
            "a definition's head is no call: {edges:?}"
        );
        assert!(
            edges.iter().all(|(from, _)| !from.ends_with(":base")),
            "{edges:?}"
        );
    }

    #[test]
    fn a_function_passed_as_a_value_is_used_where_it_is_passed() {
        let g = graph(&[
            (
                "/p/src/Cart.hs",
                "module Cart where\nadd :: Int -> Int -> Int\nadd a b = a + b\ndouble :: Int -> Int\ndouble = (* 2)\n",
            ),
            (
                "/p/app/Main.hs",
                "module Main where\nimport Cart\nmain :: IO ()\nmain = do\n  print (foldr add 0 [1, 2])\n  print $ double . add 1 <$> [3]\n",
            ),
        ]);
        assert_eq!(callers_named(&g, "/p/src/Cart.hs", "add", 0), ["main@4"]);
        assert_eq!(callers_named(&g, "/p/src/Cart.hs", "double", 0), ["main@4"]);
        let calls = calls_of("main = print (add 1 2)\n", "haskell");
        assert_eq!(
            calls.iter().filter(|c| c.callee == "add").count(),
            1,
            "{calls:?}"
        );

        let g = graph(&[
            (
                "/p/src/cart.erl",
                "-module(cart).\n-export([add/2]).\nadd(A, B) -> A + B.\n",
            ),
            (
                "/p/src/shop.erl",
                "-module(shop).\ntotal(Xs) -> lists:foldl(fun cart:add/2, 0, Xs).\nsum(Xs) -> lists:foldl(fun plus/2, 0, Xs).\nplus(A, B) -> A + B.\n",
            ),
        ]);
        assert_eq!(callers_named(&g, "/p/src/cart.erl", "add", 0), ["total@2"]);
        assert_eq!(callers_named(&g, "/p/src/shop.erl", "plus", 0), ["sum@3"]);

        let g = graph(&[(
            "/p/lib/cart.ex",
            "defmodule Cart do\n  def add(a), do: a + 1\n  def all(xs), do: Enum.map(xs, &add/1)\nend\n",
        )]);
        assert_eq!(callers_named(&g, "/p/lib/cart.ex", "add", 0), ["all@3"]);
    }

    /// A test titled as a function is named is no call's target, takes no
    /// key of the function's, and is no `callee_keys` entry over it: in
    /// one file, `function adds` and `it('adds', …)`.
    #[test]
    fn a_test_titled_as_a_function_is_not_that_function() {
        let src = "it('adds', () => {\n  adds(1);\n});\nfunction adds(n) {\n  return n + 1;\n}\nfunction other() {\n  adds(2);\n}\n";
        let g = graph(&[("/p/a.test.js", src)]);
        let id = g.id_of(Path::new("/p/a.test.js"), "adds").unwrap();
        assert_eq!(g.node(id).kind, "function");
        assert_eq!(
            callers_named(&g, "/p/a.test.js", "adds", 0),
            ["adds@1", "other@7"]
        );
        let keys = g.callee_keys();
        let key = g.key_of(id).unwrap();
        assert!(keys[&key].is_empty(), "{keys:?}");
    }

    /// An Elixir function's head (`add(a, b)` of `def add(a, b) do`, a
    /// guarded one, a `defp`'s) defines it: no call. Every function called
    /// itself, and listed itself among what it calls.
    #[test]
    fn an_elixir_functions_head_is_no_call_of_it() {
        let src = "defmodule Cart do\n  def add(a, b), do: a + b\n  defp twice(x) when x > 0 do\n    add(x, x)\n  end\n  def count(n) do\n    count(n - 1)\n  end\nend\n";
        let mut sites = calls_of(src, "elixir");
        sites.sort_by_key(|c| c.line);
        let calls: Vec<(Option<String>, String)> = sites
            .into_iter()
            .filter(|c| c.callee != "defmodule")
            .map(|c| (c.caller, c.callee))
            .collect();
        assert_eq!(
            calls,
            [
                (Some("twice".to_string()), "add".to_string()),
                (Some("count".to_string()), "count".to_string()),
            ],
            "only the calls in the bodies, a recursion included"
        );
    }

    /// `f(a)(b)` calls what `f(a)` returned: no function of a name. MUnit's
    /// `test("adds") { … }` read as a call of `adds`, its title.
    #[test]
    fn a_call_of_what_a_call_returned_names_no_function() {
        let calls = calls_of(
            "class S extends munit.FunSuite {\n  test(\"adds\") {\n    run()\n  }\n}\n",
            "scala",
        );
        assert!(!calls.iter().any(|c| c.callee == "adds"), "{calls:?}");
        assert_eq!(
            calls.iter().filter(|c| c.callee == "test").count(),
            1,
            "{calls:?}"
        );
        let js = calls_of("make(1)(2);\n", "javascript");
        assert_eq!(
            js.iter().map(|c| c.callee.as_str()).collect::<Vec<_>>(),
            ["make"]
        );
    }

    /// Who calls the `ordinal`th `name` of `file`, as `caller@line`.
    fn callers_named(g: &ProjectCallGraph, file: &str, name: &str, ordinal: u32) -> Vec<String> {
        let id = g
            .id_of_key(Path::new(file), name, ordinal)
            .unwrap_or_else(|| panic!("no {name} in {file}"));
        let mut out: Vec<String> = g
            .callers_of(id)
            .iter()
            .map(|&c| format!("{}@{}", g.node(c).name, g.node(c).line))
            .collect();
        out.sort();
        out
    }

    /// Calling a Python class runs its `__init__`: `Inventory()` and
    /// `raise OutOfStock(...)` call them, of a class the file defines or
    /// imports; a class of the same name elsewhere is not the one called.
    /// A property is read, not called: it is kept apart.
    #[test]
    fn calling_a_python_class_calls_its_init() {
        let shop = "\
class OutOfStock(Exception):
    def __init__(self, tea):
        self.tea = tea

class Inventory:
    def __init__(self):
        self.tins = {}

    @property
    def count(self):
        return len(self.tins)

    def take(self, tea):
        if tea not in self.tins:
            raise OutOfStock(tea)
";
        let main = "\
from shop import Inventory

def main():
    inventory = Inventory()
    inventory.take('sencha')
";
        let other = "\
class Inventory:
    def __init__(self):
        pass
";
        let files = [
            ("/p/shop.py", shop),
            ("/p/main.py", main),
            ("/p/other.py", other),
        ];
        let sources: Vec<(PathBuf, String)> = files
            .iter()
            .map(|(f, s)| (PathBuf::from(f), s.to_string()))
            .collect();
        let scope = HashMap::from([(
            PathBuf::from("/p/main.py"),
            HashSet::from([PathBuf::from("/p/shop.py")]),
        )]);
        let g = ProjectCallGraph::build(defs_of(&files), &sources, &scope);
        assert_eq!(callers_named(&g, "/p/shop.py", "__init__", 0), ["take@13"]);
        assert_eq!(callers_named(&g, "/p/shop.py", "__init__", 1), ["main@3"]);
        assert!(callers_named(&g, "/p/other.py", "__init__", 0).is_empty());
        let calls = FileCalls::read(Path::new("/p/shop.py"), shop).unwrap();
        assert_eq!(calls.accessed, HashSet::from([10]));
    }

    /// A method called on a receiver whose type the caller states links to
    /// that type's method, not every method of the name: `main` builds a
    /// `Cart` and calls its `total`, and `Item.total` was listed as called
    /// by `main`. `self.f()` is the caller's own class's `f`. A receiver
    /// whose type is not known (a loop variable) still links to them all.
    #[test]
    fn a_typed_receiver_calls_its_own_types_method() {
        let cart = "\
class Item:
    def total(self):
        return 1

class Cart:
    def subtotal(self):
        return sum(item.total() for item in self.items)

    def total(self):
        return self.subtotal()
";
        let main = "\
from cart import Cart

def main():
    cart = Cart()
    cart.total()
";
        let files = [("/p/cart.py", cart), ("/p/main.py", main)];
        let sources: Vec<(PathBuf, String)> = files
            .iter()
            .map(|(f, s)| (PathBuf::from(f), s.to_string()))
            .collect();
        // `main.py` imports `cart.py`.
        let scope = HashMap::from([(
            PathBuf::from("/p/main.py"),
            HashSet::from([PathBuf::from("/p/cart.py")]),
        )]);
        let g = ProjectCallGraph::build(defs_of(&files), &sources, &scope);
        // `Item.total` is the file's first `total`, `Cart.total` its second.
        assert_eq!(callers_named(&g, "/p/cart.py", "total", 0), ["subtotal@6"]);
        assert_eq!(
            callers_named(&g, "/p/cart.py", "total", 1),
            ["main@3", "subtotal@6"],
            "`item` has no stated type, so either total may be the one"
        );
        assert_eq!(callers_named(&g, "/p/cart.py", "subtotal", 0), ["total@9"]);
    }

    /// The class each method belongs to, however the language writes it:
    /// inside its class, in a Rust `impl`, by a Go receiver, or out of its
    /// C++ class as `Cart::total`.
    #[test]
    fn each_language_says_whose_method_it_is() {
        let owners = |file: &str, src: &str| {
            let call = FileCalls::read(Path::new(file), src).expect(file);
            let mut owners: Vec<(usize, String)> = call.owners.into_iter().collect();
            owners.sort();
            owners
        };
        let cart = |line: usize| vec![(line, "Cart".to_string())];
        assert_eq!(
            owners(
                "a.py",
                "class Cart:\n    def total(self):\n        pass\n\ndef main():\n    pass\n"
            ),
            cart(2)
        );
        assert_eq!(
            owners(
                "a.rs",
                "struct Cart;\nimpl Cart {\n    fn total(&self) {}\n}\nfn main() {}\n"
            ),
            cart(3)
        );
        assert_eq!(
            owners(
                "a.rs",
                "struct Cart<T>(T);\nimpl<T> Summed for Cart<T> {\n    fn total(&self) {}\n}\n"
            ),
            cart(3)
        );
        assert_eq!(
            owners(
                "a.go",
                "package m\ntype Cart struct{}\nfunc (c *Cart) Total() int { return 1 }\nfunc main() {}\n"
            ),
            cart(3)
        );
        assert_eq!(
            owners(
                "a.ts",
                "class Cart {\n  total() { return 1 }\n}\nfunction main() {}\n"
            ),
            cart(2)
        );
        assert_eq!(
            owners("A.java", "class Cart {\n  int total() { return 1; }\n}\n"),
            cart(2)
        );
        assert_eq!(
            owners("a.rb", "class Cart\n  def total\n    1\n  end\nend\n"),
            cart(2)
        );
        assert_eq!(
            owners("a.cs", "class Cart {\n  int Total() { return 1; }\n}\n"),
            cart(2)
        );
        assert_eq!(
            owners(
                "a.php",
                "<?php\nclass Cart {\n  function total() { return 1; }\n}\n"
            ),
            cart(3)
        );
        assert_eq!(
            owners(
                "a.cpp",
                "class Cart {\n public:\n  int total() { return 1; }\n};\nint Cart::other() { return 2; }\n"
            ),
            vec![(3, "Cart".to_string()), (5, "Cart".to_string())]
        );
        // A trait's own methods are any implementation's: no owner.
        assert_eq!(
            owners(
                "b.rs",
                "trait Summed {\n    fn total(&self) -> u32 { 0 }\n}\n"
            ),
            vec![]
        );
    }

    /// What a receiver is known to be, as each language binds it.
    #[test]
    fn receivers_are_typed_as_their_bindings_say() {
        let receiver = |file: &str, src: &str, callee: &str| {
            let calls = FileCalls::read(Path::new(file), src).expect(file).calls;
            calls
                .into_iter()
                .find(|c| c.callee == callee)
                .unwrap_or_else(|| panic!("no call of {callee} in {file}"))
                .receiver
        };
        let cart = Some(Receiver::Type("Cart".into()));
        assert_eq!(
            receiver("a.py", "def f():\n    c = Cart()\n    c.total()\n", "total"),
            cart
        );
        // Declared or annotated, or the class's own: it may be a subclass,
        // whose override is the method called — the type's method and its
        // subtypes' overrides, not one known exactly.
        let declared = Some(Receiver::Declared("Cart".into()));
        assert_eq!(
            receiver("a.py", "def f(c: Cart):\n    c.total()\n", "total"),
            declared
        );
        assert_eq!(
            receiver(
                "a.py",
                "class A:\n    def f(self):\n        self.total()\n",
                "total"
            ),
            Some(Receiver::Own)
        );
        assert_eq!(
            receiver(
                "A.java",
                "class A {\n  void f() {\n    Cart c = make();\n    c.total();\n  }\n}\n",
                "total"
            ),
            declared
        );
        // A value a type's factory makes: what the factory's definitions
        // say it returns, else (a guess) the type it is called on — Scala's
        // `IO.pure`, Kotlin's, Java's, C#'s; an annotation says otherwise
        // first.
        let guess = |function: &str, ty: &str| {
            Some(Receiver::Declared(format!("{RETURNED}{function}|{ty}")))
        };
        assert_eq!(
            receiver(
                "a.scala",
                "def f() = {\n  val io = IO.pure(1)\n  io.flatMap(g)\n}\n",
                "flatMap"
            ),
            guess("pure", "IO")
        );
        assert_eq!(
            receiver(
                "a.scala",
                "def f() = {\n  val io = IO.unit\n  io.flatMap(g)\n}\n",
                "flatMap"
            ),
            guess("", "IO")
        );
        assert_eq!(
            receiver(
                "a.kt",
                "fun f() {\n  val c = Cart.create()\n  c.total()\n}\n",
                "total"
            ),
            guess("create", "Cart")
        );
        assert_eq!(
            receiver(
                "a.kt",
                "fun f() {\n  val c: Order = Cart.create()\n  c.total()\n}\n",
                "total"
            ),
            Some(Receiver::Declared("Order".into()))
        );
        assert_eq!(
            receiver(
                "A.cs",
                "class A {\n  void F() {\n    var c = Cart.Create();\n    c.Total();\n  }\n}\n",
                "Total"
            ),
            guess("Create", "Cart")
        );
        // A field, by what its class declares it as: Java's, Kotlin's
        // constructor property, TypeScript's parameter property; with
        // `this.` or bare.
        assert_eq!(
            receiver(
                "A.java",
                "class A {\n  private final Cart cart;\n  void f() {\n    this.cart.total();\n  }\n}\n",
                "total"
            ),
            declared
        );
        assert_eq!(
            receiver(
                "A.java",
                "class A {\n  private final Cart cart;\n  void f() {\n    cart.total();\n  }\n}\n",
                "total"
            ),
            declared
        );
        assert_eq!(
            receiver(
                "a.kt",
                "class A(private val cart: Cart) {\n  fun f() {\n    cart.total()\n  }\n}\n",
                "total"
            ),
            declared
        );
        assert_eq!(
            receiver(
                "a.ts",
                "class A {\n  constructor(private cart: Cart) {}\n  f() {\n    this.cart.total();\n  }\n}\n",
                "total"
            ),
            declared
        );
        // In Rust, with no inheritance, the impl's own type.
        assert_eq!(
            receiver(
                "a.rs",
                "struct A;\nimpl A {\n    fn f(&self) {\n        self.total();\n    }\n}\n",
                "total"
            ),
            Some(Receiver::Own)
        );
        assert_eq!(
            receiver(
                "a.rs",
                "fn f() {\n    let c = Cart::new();\n    c.total();\n}\n",
                "total"
            ),
            cart
        );
        assert_eq!(
            receiver("a.rs", "fn f(c: &mut Cart) {\n    c.total();\n}\n", "total"),
            cart
        );
        // A trait object is any type implementing it.
        assert_eq!(
            receiver("a.rs", "fn f(c: &dyn Cart) {\n    c.total();\n}\n", "total"),
            None
        );
        assert_eq!(
            receiver(
                "a.go",
                "package m\nfunc f() {\n\tc := &Cart{}\n\tc.Total()\n}\n",
                "Total"
            ),
            cart
        );
        assert_eq!(
            receiver(
                "a.go",
                "package m\nfunc (c *Cart) f() {\n\tc.Total()\n}\n",
                "Total"
            ),
            cart
        );
        assert_eq!(
            receiver(
                "a.ts",
                "function f() {\n  const c = new Cart();\n  c.total();\n}\n",
                "total"
            ),
            cart
        );
        assert_eq!(
            receiver(
                "A.java",
                "class A {\n  void f() {\n    Cart c = new Cart();\n    c.total();\n  }\n}\n",
                "total"
            ),
            cart
        );
        assert_eq!(
            receiver("a.rb", "def f\n  c = Cart.new\n  c.total\nend\n", "total"),
            cart
        );
        assert_eq!(
            receiver(
                "a.php",
                "<?php\nfunction f() {\n  $c = new Cart();\n  $c->total();\n}\n",
                "total"
            ),
            cart
        );
        assert_eq!(
            receiver(
                "a.cs",
                "class A {\n  void F() {\n    var c = new Cart();\n    c.Total();\n  }\n}\n",
                "Total"
            ),
            cart
        );
        // A type called on itself.
        assert_eq!(
            receiver(
                "A.java",
                "class A {\n  void f() {\n    Cart.total();\n  }\n}\n",
                "total"
            ),
            cart
        );
        // Unknown: no binding, a function's result, an expression, two
        // bindings that disagree, a Go function that builds one.
        assert_eq!(
            receiver("a.py", "def f(c):\n    c.total()\n", "total"),
            None
        );
        assert_eq!(
            receiver("a.py", "def f():\n    c = make()\n    c.total()\n", "total"),
            None
        );
        assert_eq!(
            receiver("a.py", "def f(self):\n    self.c.total()\n", "total"),
            None
        );
        assert_eq!(
            receiver(
                "a.py",
                "def f(x):\n    c = Cart()\n    c = Item()\n    c.total()\n",
                "total"
            ),
            None
        );
        // A function's result: of the type it declares, once the project
        // is read.
        assert_eq!(
            receiver(
                "a.go",
                "package m\nfunc f() {\n\tc := newCart()\n\tc.Total()\n}\n",
                "Total"
            ),
            Some(Receiver::Type(format!("{RETURNED}newCart")))
        );
        // What a type's function other than `new` returns, a variant of an
        // enum, and what is called off a value built: of types of their own.
        assert_eq!(
            receiver(
                "a.rs",
                "fn f() {\n    let b = Config::builder();\n    b.port(80);\n}\n",
                "port"
            ),
            None
        );
        assert_eq!(
            receiver(
                "a.rs",
                "fn f(c: Circle) {\n    let s = Shape::Circle(c);\n    s.area();\n}\n",
                "area"
            ),
            None
        );
        assert_eq!(
            receiver(
                "a.py",
                "def f(p):\n    x = Config(p).load()\n    x.run()\n",
                "run"
            ),
            None
        );
        assert_eq!(
            receiver(
                "a.ts",
                "function f(p) {\n  const x = new Config(p).load();\n  x.run();\n}\n",
                "run"
            ),
            None
        );
        // Built once, then bound again to what cannot be told: a function's
        // result, a closure's parameter, a loop's.
        assert_eq!(
            receiver(
                "a.py",
                "def f():\n    s = Shape()\n    s = make_circle()\n    s.area()\n",
                "area"
            ),
            None
        );
        assert_eq!(
            receiver(
                "a.ts",
                "function f(orders) {\n  const cart = new Cart();\n  orders.forEach(cart => cart.total());\n}\n",
                "total"
            ),
            None
        );
        assert_eq!(
            receiver(
                "a.py",
                "def f(carts):\n    c = Cart()\n    for c in carts:\n        c.total()\n",
                "total"
            ),
            None
        );
    }

    /// A call is told what the bindings before it say, not those after it,
    /// whichever of the function's calls is read first; and a name bound in
    /// a function nested in the caller is that function's own.
    #[test]
    fn a_receiver_is_typed_by_the_bindings_before_its_call() {
        let receivers = |file: &str, src: &str, callee: &str| {
            let mut calls: Vec<CallSite> = FileCalls::read(Path::new(file), src)
                .expect(file)
                .calls
                .into_iter()
                .filter(|c| c.callee == callee)
                .collect();
            calls.sort_by_key(|c| c.line);
            calls.into_iter().map(|c| c.receiver).collect::<Vec<_>>()
        };
        let item = Some(Receiver::Type("Item".into()));
        assert_eq!(
            receivers(
                "a.py",
                "def f(c):\n    c.total()\n    c = Item()\n    c.total()\n",
                "total"
            ),
            [None, item.clone()]
        );
        assert_eq!(
            receivers(
                "a.py",
                "def f(c):\n    def h():\n        c = Item()\n    c.total()\n",
                "total"
            ),
            [None]
        );
    }

    /// A receiver's bindings are read where they are: a closure's captured
    /// name as the function around it binds it; an assignment in a nested
    /// function leaves the name untold, a declaration there is its own; a
    /// binding is in effect after its statement; a pattern's names, a
    /// keyword argument, a parameter's default, a comment are what they are
    /// — and a character of several bytes before a name breaks nothing.
    #[test]
    fn a_receivers_bindings_are_read_where_they_are() {
        let receiver = |file: &str, src: &str, callee: &str| {
            let calls = FileCalls::read(Path::new(file), src).expect(file).calls;
            calls
                .into_iter()
                .find(|c| c.callee == callee)
                .unwrap_or_else(|| panic!("no call of {callee} in {file}"))
                .receiver
        };
        let typed = |ty: &str| Some(Receiver::Type(ty.into()));
        // The line before ends in a character of several bytes.
        assert_eq!(
            receiver(
                "a.py",
                "def load(path):\n    # 读取配置文件。\n    cfg = read(path)\n    cfg.validate()\n",
                "validate"
            ),
            None
        );
        // Captured by a closure.
        assert_eq!(
            receiver(
                "a.js",
                "function f() {\n  const store = new Store();\n  useEffect(() => store.load());\n}\n",
                "load"
            ),
            typed("Store")
        );
        // ...but bound again after it: what the closure sees when it runs.
        assert_eq!(
            receiver(
                "a.js",
                "function f() {\n  let store = new Store();\n  const cb = () => store.load();\n  store = make();\n}\n",
                "load"
            ),
            None
        );
        // Assigned in a nested function; declared there, its own.
        assert_eq!(
            receiver(
                "a.js",
                "function f() {\n  let c = new Cart();\n  reset(() => { c = new Item(); });\n  c.total();\n}\n",
                "total"
            ),
            None
        );
        assert_eq!(
            receiver(
                "a.js",
                "function f() {\n  const c = new Cart();\n  reset(() => { const c = new Item(); });\n  c.total();\n}\n",
                "total"
            ),
            typed("Cart")
        );
        assert_eq!(
            receiver(
                "a.py",
                "def f():\n    c = Cart()\n    def g():\n        c = Item()\n    c.total()\n",
                "total"
            ),
            typed("Cart")
        );
        assert_eq!(
            receiver(
                "a.py",
                "def f():\n    c = Cart()\n    def g():\n        nonlocal c\n        c = Item()\n    c.total()\n",
                "total"
            ),
            None
        );
        // In the value of its own binding, the name is still the one before.
        assert_eq!(
            receiver(
                "a.py",
                "def wrap(item):\n    item = Wrapper(item.unwrap())\n",
                "unwrap"
            ),
            None
        );
        // One of a pattern's names.
        assert_eq!(
            receiver(
                "a.py",
                "def f():\n    c = Cart()\n    c, rest = split()\n    c.total()\n",
                "total"
            ),
            None
        );
        assert_eq!(
            receiver(
                "a.py",
                "def f(xs):\n    c = Cart()\n    for i, c in enumerate(xs):\n        c.total()\n",
                "total"
            ),
            None
        );
        assert_eq!(
            receiver(
                "a.rs",
                "fn f(x: Option<Item>) {\n    let c = Cart::new();\n    if let Some(c) = x {\n        c.total();\n    }\n}\n",
                "total"
            ),
            None
        );
        // A comment, a keyword argument, a parameter's default.
        assert_eq!(
            receiver(
                "a.py",
                "def f(c):\n    # c = Cart() usually\n    c.total()\n",
                "total"
            ),
            None
        );
        assert_eq!(
            receiver(
                "a.py",
                "def f(c):\n    show(c=Cart())\n    c.total()\n",
                "total"
            ),
            None
        );
        assert_eq!(
            receiver("a.py", "def f(c=Cart()):\n    c.total()\n", "total"),
            None
        );
        assert_eq!(
            receiver(
                "a.py",
                "def f():\n    c = Cart()\n    show(c=c)\n    c.total()\n",
                "total"
            ),
            typed("Cart")
        );
    }

    /// A call tree-sitter recovered from half-typed code can name nothing:
    /// no receiver is looked for, where every position matched the empty
    /// name and the search never moved on.
    #[test]
    fn an_empty_callee_has_no_receiver() {
        assert_eq!(receiver_text("x.(y)", ""), None);
        assert_eq!(receiver_text("cart.total()", "total"), Some("cart"));
    }

    /// A receiver is what precedes the member called: past a member of the
    /// same name that the path goes on beyond (Scala's `os.watch.watch(…)`,
    /// its package object's `watch`), at the first a call follows.
    #[test]
    fn a_receiver_is_what_precedes_the_member_called() {
        assert_eq!(
            receiver_text("os.watch.watch(p)", "watch"),
            Some("os.watch")
        );
        assert_eq!(receiver_text("a.f.g.f()", "f"), Some("a.f.g"));
        assert_eq!(receiver_text("b.with(x).with(y)", "with"), Some("b"));
    }

    /// A method that overrides another is called where its base's is, on
    /// `self` or on a value typed as the base: the base's caller is the
    /// override's too.
    #[test]
    fn an_override_keeps_its_bases_callers() {
        let src = "\
class Shape:
    def describe(self):
        return self.area()

    def area(self):
        raise NotImplementedError

class Circle(Shape):
    def area(self):
        return 3.14

def show(s: Shape):
    s.area()
";
        let files = [("/p/shapes.py", src)];
        let sources: Vec<(PathBuf, String)> = files
            .iter()
            .map(|(f, s)| (PathBuf::from(f), s.to_string()))
            .collect();
        let g = ProjectCallGraph::build(defs_of(&files), &sources, &HashMap::new());
        // `Shape.area` is the file's first `area`, `Circle.area` its second.
        assert_eq!(
            callers_named(&g, "/p/shapes.py", "area", 1),
            ["describe@2", "show@12"]
        );
    }

    /// The bindings of a receiver are read once in a function, however many
    /// calls are made on it: read again for every call, a long generated
    /// function took the time of its length times its calls.
    #[test]
    fn a_long_functions_receivers_are_read_once() {
        let mut src = String::from("def f():\n    r = Runner()\n");
        for _ in 0..20_000 {
            src.push_str("    r.step(a, b, c)\n");
        }
        let started = std::time::Instant::now();
        let calls: Vec<CallSite> = FileCalls::read(Path::new("gen.py"), &src)
            .unwrap()
            .calls
            .into_iter()
            .filter(|c| c.callee == "step")
            .collect();
        let took = started.elapsed();
        assert_eq!(calls.len(), 20_000);
        assert!(
            calls
                .iter()
                .all(|c| c.receiver == Some(Receiver::Type("Runner".into())))
        );
        assert!(took < std::time::Duration::from_secs(5), "{took:?}");
    }

    /// A known type with no method of the name calls the one it inherits,
    /// not another class's of the name.
    #[test]
    fn a_type_without_the_method_calls_the_one_it_inherits() {
        let src = "\
class Base:
    def run(self):
        pass

class Other:
    def run(self):
        pass

class Sub(Base):
    pass

def main():
    s = Sub()
    s.run()
";
        let g = graph(&[("/p/a.py", src)]);
        assert_eq!(callers_named(&g, "/p/a.py", "run", 0), ["main@12"]);
        assert!(callers_named(&g, "/p/a.py", "run", 1).is_empty());
    }

    /// The call facts a symbol index keeps — `FileCalls::of` over the one
    /// `outline::analyze` it makes of each file — are exactly what
    /// `FileCalls::read` parses for, and `build_from_calls` over them is the
    /// graph `build` gets by parsing every file again: for no parse at all.
    #[test]
    fn calls_kept_from_the_index_parse_build_the_parsed_graph_without_a_parse() {
        let files: &[(&str, &str)] = &[
            (
                "/p/a.rs",
                "fn helper() {}\nfn run() { helper(); other(); }\n\
                 struct S;\nimpl S {\n    fn new() -> S { helper(); S }\n}\n",
            ),
            (
                "/p/b.py",
                "def other():\n    run()\n\ndef run():\n    other()\n",
            ),
            (
                "/p/c.h",
                "int proto(int x);\nint use_it(void) { return proto(1); }\n\
                 int proto(int x) { return x; }\n",
            ),
            ("/p/d.js", "function a() { b(); }\nconst b = () => a();\n"),
            ("/p/e.toml", "a = 1\n"),
        ];
        let defs = defs_of(files);
        let mut kept = Vec::new();
        for (file, src) in files {
            let lang = crate::highlight::detect(Path::new(file)).unwrap();
            let Some(a) = crate::outline::analyze(src, lang) else {
                continue;
            };
            let of = FileCalls::of(PathBuf::from(file), a.lang, a.calls, &a.symbols, a.owners);
            let read = FileCalls::read(Path::new(file), src);
            assert_eq!(
                of.as_ref()
                    .map(|c| (&c.file, c.lang, &c.calls, &c.declarations, &c.owners)),
                read.as_ref()
                    .map(|c| (&c.file, c.lang, &c.calls, &c.declarations, &c.owners)),
                "{file}"
            );
            kept.extend(of);
        }
        assert!(
            kept.iter().any(|c| !c.declarations.is_empty()),
            "a prototype is kept as a declaration"
        );
        let reads = crate::highlight::parses_on_this_thread();
        let kept_graph = ProjectCallGraph::build_from_calls(defs.clone(), &kept, &HashMap::new());
        assert_eq!(
            crate::highlight::parses_on_this_thread(),
            reads,
            "linking kept call sites parses nothing"
        );
        let parsed = graph(files);
        let wire = |g: &ProjectCallGraph| g.to_wire(|p| p.to_string_lossy().into_owned());
        assert_eq!(wire(&kept_graph), wire(&parsed));
        assert!(kept_graph.edge_count() >= 6, "{:?}", edges(&kept_graph));
    }

    /// Every `caller -> callee` edge as `(caller file:name, callee file:name)`.
    fn edges(g: &ProjectCallGraph) -> Vec<(String, String)> {
        let label = |i: usize| format!("{}:{}", g.node(i).file.display(), g.node(i).name);
        let mut out = Vec::new();
        for i in 0..g.node_count() {
            for &c in g.callees_of(i) {
                out.push((label(i), label(c)));
            }
        }
        out.sort();
        out
    }

    fn has(g: &ProjectCallGraph, from: &str, to: &str) -> bool {
        edges(g).contains(&(from.to_string(), to.to_string()))
    }

    /// A Go package's files call one another with no import: a method on a
    /// parameter's type, defined in the package's other file, is the call's.
    #[test]
    fn a_go_package_calls_across_its_files() {
        let g = graph(&[
            (
                "/p/context.go",
                "package gin\n\ntype Context struct{}\n\nfunc (c *Context) AbortWithStatus(code int) {}\n",
            ),
            (
                "/p/auth.go",
                "package gin\n\nfunc BasicAuth() HandlerFunc {\n\treturn func(c *Context) {\n\t\tc.AbortWithStatus(401)\n\t}\n}\n",
            ),
            (
                "/p/other/ctx.go",
                "package other\n\ntype Context struct{}\n\nfunc (c *Context) AbortWithStatus(code int) {}\n",
            ),
        ]);
        assert!(
            has(&g, "/p/auth.go:BasicAuth", "/p/context.go:AbortWithStatus"),
            "{:?}",
            edges(&g)
        );
        assert!(!has(
            &g,
            "/p/auth.go:BasicAuth",
            "/p/other/ctx.go:AbortWithStatus"
        ));
    }

    /// Java's classes of one package name one another with no import, and a
    /// value is what it is declared as — a field, a parameter, a local — or
    /// a subtype: `this.owners.findById(…)` on an `OwnerRepository` field
    /// calls the class that implements it; `owner.getLastName()` on an
    /// `Owner` the `Person` method it inherits; a bare `getLastName()` in
    /// `Owner` the same. A value of a library's type (`List`) calls none of
    /// the project's methods of the name.
    #[test]
    fn java_calls_resolve_by_package_and_declared_type() {
        let g = graph(&[
            (
                "/p/model/Person.java",
                "package app.model;\n\npublic class Person {\n  public String getLastName() { return \"\"; }\n}\n",
            ),
            (
                "/p/owner/Owner.java",
                "package app.owner;\n\nimport app.model.Person;\n\npublic class Owner extends Person {\n  public String label() { return getLastName(); }\n  public void add(Object o) {}\n}\n",
            ),
            (
                "/p/owner/OwnerRepository.java",
                "package app.owner;\n\npublic interface OwnerRepository {\n  Owner findById(int id);\n}\n",
            ),
            (
                "/p/owner/JpaOwners.java",
                "package app.owner;\n\nclass JpaOwners implements OwnerRepository {\n  public Owner findById(int id) { return null; }\n}\n",
            ),
            (
                "/p/owner/OwnerController.java",
                "package app.owner;\n\nimport java.util.List;\n\nclass OwnerController {\n  private final OwnerRepository owners;\n  String show(int id, Owner owner) {\n    this.owners.findById(id);\n    List<String> names = List.of();\n    names.add(\"x\");\n    return owner.getLastName();\n  }\n}\n",
            ),
        ]);
        let show = "/p/owner/OwnerController.java:show";
        assert!(
            has(&g, show, "/p/owner/JpaOwners.java:findById"),
            "{:?}",
            edges(&g)
        );
        assert!(
            !has(&g, show, "/p/owner/OwnerRepository.java:findById"),
            "the definition, not the interface's declaration"
        );
        assert!(has(&g, show, "/p/model/Person.java:getLastName"));
        assert!(!has(&g, show, "/p/owner/Owner.java:add"), "a List's add");
        assert!(has(
            &g,
            "/p/owner/Owner.java:label",
            "/p/model/Person.java:getLastName"
        ));
    }

    /// A Dart constructor is a definition, with a body or none, plain,
    /// `const`, named or `factory`: `CounterCubit()` calls it, from a file
    /// that imports its class's.
    #[test]
    fn dart_constructors_are_called() {
        let src = "class Cart {\n  const Cart({this.items = const []});\n  Cart.named() : items = const [];\n  factory Cart.empty() = _EmptyCart;\n  final List<int> items;\n}\nclass CounterCubit extends Cubit<int> {\n  CounterCubit() : super(0);\n}\n";
        let names: Vec<String> = crate::outline::extract(src, "dart")
            .into_iter()
            .filter(|s| s.kind == "method")
            .map(|s| format!("{}@{}", s.name, s.line))
            .collect();
        assert!(names.contains(&"Cart@2".to_string()), "{names:?}");
        assert!(names.contains(&"named@3".to_string()), "{names:?}");
        assert!(names.contains(&"empty@4".to_string()), "{names:?}");
        assert!(names.contains(&"CounterCubit@8".to_string()), "{names:?}");
        let g = graph(&[
            ("/p/lib/counter_cubit.dart", src),
            (
                "/p/lib/page.dart",
                "import 'counter_cubit.dart';\nWidget build() {\n  final c = CounterCubit();\n  return Cart.named();\n}\n",
            ),
        ]);
        assert!(
            has(
                &g,
                "/p/lib/page.dart:build",
                "/p/lib/counter_cubit.dart:CounterCubit"
            ),
            "{:?}",
            edges(&g)
        );
        assert!(has(
            &g,
            "/p/lib/page.dart:build",
            "/p/lib/counter_cubit.dart:named"
        ));
    }

    /// Scala calls a method with no parameters without parentheses
    /// (`ref.get`), and a chain on a value a factory just made is that
    /// type's (`IO.pure(1).flatMap(…)`); a field read so links to nothing.
    #[test]
    fn scala_parameterless_calls_and_factory_chains_link() {
        let g = graph(&[
            (
                "/p/Ref.scala",
                "package fx\n\nclass Ref[A](var value: A) {\n  def get: A = value\n}\n\nclass IO[A] {\n  def flatMap[B](f: A => IO[B]): IO[B] = ???\n}\nobject IO {\n  def pure[A](a: A): IO[A] = ???\n}\n",
            ),
            (
                "/p/Use.scala",
                "package fx\n\nobject Use {\n  def run(ref: Ref[Int]): Int = {\n    val now = ref.get\n    IO.pure(now).flatMap(n => IO.pure(n))\n    ref.value\n  }\n}\n",
            ),
        ]);
        assert!(
            has(&g, "/p/Use.scala:run", "/p/Ref.scala:get"),
            "{:?}",
            edges(&g)
        );
        assert!(has(&g, "/p/Use.scala:run", "/p/Ref.scala:flatMap"));
        assert!(has(&g, "/p/Use.scala:run", "/p/Ref.scala:pure"));
        let reads = FileCalls::read(
            Path::new("/p/Use.scala"),
            "object Use {\n  def run(ref: Ref[Int]) = {\n    ref.get\n    f(ref.value)\n  }\n}\n",
        )
        .unwrap();
        assert!(reads.calls.iter().any(|c| c.callee == "get" && c.method));
        assert!(
            !reads.calls.iter().any(|c| c.callee == "f" && c.method),
            "a call's own function is no member read"
        );
    }

    /// A test calls the code; the code never calls a test's helpers: an
    /// app's call reaches the app's method of the name, not a test file's,
    /// while a test's call reaches both.
    #[test]
    fn app_code_does_not_call_into_tests() {
        let files = [
            (
                "/p/app/models/user.rb",
                "class User\n  def email_time\n    1\n  end\nend\n",
            ),
            (
                "/p/app/jobs/mailer_job.rb",
                "class MailerJob\n  def perform(user)\n    user.email_time\n  end\nend\n",
            ),
            (
                "/p/test/support/helpers.rb",
                "module Helpers\n  def email_time\n    2\n  end\nend\n",
            ),
            (
                "/p/test/mailer_job_test.rb",
                "class MailerJobTest\n  def test_it\n    email_time\n  end\nend\n",
            ),
        ];
        let sources: Vec<(PathBuf, String)> = files
            .iter()
            .map(|(f, s)| (PathBuf::from(f), s.to_string()))
            .collect();
        let scope: HashMap<PathBuf, HashSet<PathBuf>> = [
            (
                PathBuf::from("/p/app/jobs/mailer_job.rb"),
                HashSet::from([
                    PathBuf::from("/p/app/models/user.rb"),
                    PathBuf::from("/p/test/support/helpers.rb"),
                ]),
            ),
            (
                PathBuf::from("/p/test/mailer_job_test.rb"),
                HashSet::from([PathBuf::from("/p/test/support/helpers.rb")]),
            ),
        ]
        .into_iter()
        .collect();
        let g = ProjectCallGraph::build(defs_of(&files), &sources, &scope);
        let perform = "/p/app/jobs/mailer_job.rb:perform";
        assert!(
            !has(&g, perform, "/p/test/support/helpers.rb:email_time"),
            "{:?}",
            edges(&g)
        );
        assert!(has(
            &g,
            "/p/test/mailer_job_test.rb:test_it",
            "/p/test/support/helpers.rb:email_time"
        ));
    }

    /// What a call cannot be, by each language's rules: an Elixir bare name
    /// neither defined nor imported is a library macro's, not the project's
    /// one function of the name; an Erlang module (`:crypto.hash`) or one the
    /// project defines none of (`Map.put`) calls none of its functions; a
    /// Kotlin method on a value of no known type is no top-level function,
    /// nor is `x.toString()` some class's override; a Dart bare call is no
    /// other class's method.
    #[test]
    fn calls_reach_only_what_their_language_lets_them() {
        let g = graph(&[
            (
                "/p/lib/limits.ex",
                "defmodule Shop.Limits do\n  def sum(xs), do: Enum.sum(xs)\n  def hash(x), do: x\n  def put(m, k, v), do: m\nend\n",
            ),
            (
                "/p/lib/metrics.ex",
                "defmodule Shop.Metrics do\n  def metrics do\n    sum(\"bytes\")\n    :crypto.hash(:sha256, \"x\")\n    Map.put(%{}, :a, 1)\n  end\nend\n",
            ),
            (
                "/p/src/Helpers.kt",
                "package shop\n\nfun get(path: String): String = path\n\nclass Model {\n  override fun toString(): String = \"m\"\n}\n",
            ),
            (
                "/p/src/Client.kt",
                "package shop\n\nfun fetch(client: Any) {\n  client.get(\"/x\")\n  client.toString()\n  get(\"/y\")\n}\n",
            ),
            (
                "/p/lib/cache.dart",
                "class Cache {\n  bool contains(String key) => false;\n}\n",
            ),
            (
                "/p/lib/test_it.dart",
                "import 'cache.dart';\nvoid main() {\n  expect(1, contains(1));\n}\n",
            ),
        ]);
        let metrics = "/p/lib/metrics.ex:metrics";
        for to in ["sum", "hash", "put"] {
            assert!(
                !has(&g, metrics, &format!("/p/lib/limits.ex:{to}")),
                "{to}: {:?}",
                edges(&g)
            );
        }
        let fetch = "/p/src/Client.kt:fetch";
        assert!(
            has(&g, fetch, "/p/src/Helpers.kt:get"),
            "the bare call: {:?}",
            edges(&g)
        );
        let calls_get = g
            .callees_of(g.id_of(Path::new("/p/src/Client.kt"), "fetch").unwrap())
            .iter()
            .filter(|&&c| g.node(c).name == "get")
            .count();
        assert_eq!(calls_get, 1, "only the bare call reaches `get`");
        assert!(!has(&g, fetch, "/p/src/Helpers.kt:toString"));
        assert!(!has(
            &g,
            "/p/lib/test_it.dart:main",
            "/p/lib/cache.dart:contains"
        ));
    }

    /// A test class's method is the runner's to call: `client.get(…)` in
    /// one test class is no call of another's `get` test, while a test
    /// helper class's method still is.
    #[test]
    fn a_test_classs_methods_are_no_value_s() {
        let g = graph(&[
            (
                "/p/src/main/kotlin/shop/Server.kt",
                "package shop\n\nfun module() {}\n",
            ),
            (
                "/p/src/test/kotlin/shop/StatusCodesTest.kt",
                "package shop\n\nclass StatusCodesTest {\n    @Test\n    fun get() {\n        module()\n    }\n}\n",
            ),
            (
                "/p/src/test/kotlin/shop/FakeClient.kt",
                "package shop\n\nclass FakeClient {\n    fun post(path: String) {}\n}\n",
            ),
            (
                "/p/src/test/kotlin/shop/RedirectsTest.kt",
                "package shop\n\nclass RedirectsTest {\n    @Test\n    fun follows() {\n        client.get(\"/x\")\n        client.post(\"/y\")\n    }\n}\n",
            ),
            (
                "/p/test/cart_test.exs",
                "defmodule Shop.CartTest do\n  test \"adds\" do\n    make_cart()\n  end\n\n  defp make_cart, do: %{}\nend\n",
            ),
        ]);
        // A test's own helper, called bare from the test's block.
        assert!(
            has(
                &g,
                "/p/test/cart_test.exs:adds",
                "/p/test/cart_test.exs:make_cart"
            ),
            "{:?}",
            edges(&g)
        );
        let follows = "/p/src/test/kotlin/shop/RedirectsTest.kt:follows";
        assert!(
            !has(
                &g,
                follows,
                "/p/src/test/kotlin/shop/StatusCodesTest.kt:get"
            ),
            "{:?}",
            edges(&g)
        );
        assert!(
            has(&g, follows, "/p/src/test/kotlin/shop/FakeClient.kt:post"),
            "{:?}",
            edges(&g)
        );
        for name in [
            "CartTest",
            "Shop.CartTests",
            "CartSpec",
            "CartIT",
            "TestCart",
            "Test",
        ] {
            assert!(is_test_class_name(name), "{name}");
        }
        for name in ["Contest", "Latest", "Testimony", "AUDIT", "Spec"] {
            assert!(!is_test_class_name(name), "{name}");
        }
    }

    /// A Scala class calls, bare, a method of a trait it mixes in, the
    /// trait mixed in through another trait (`with DequeueTests`, which
    /// `extends QueueTests[Dequeue]`), each in its own file.
    #[test]
    fn a_scala_bare_call_reaches_a_mixed_in_traits_method() {
        let g = graph(&[
            (
                "/p/tests/src/test/scala/shop/QueueSuite.scala",
                "package shop\n\ntrait QueueTests[Q[_[_], _]] { self: BaseSuite =>\n  def batchTests(name: String): Unit = ()\n}\n",
            ),
            (
                "/p/tests/src/test/scala/shop/DequeueSuite.scala",
                "package shop\n\nclass BoundedDequeueSuite extends BaseSuite with DequeueTests {\n  private def boundedTests(name: String) = {\n    batchTests(name)\n  }\n}\n\ntrait DequeueTests extends QueueTests[Dequeue] { self: BaseSuite =>\n  def reverse(name: String) = ()\n}\n",
            ),
        ]);
        assert!(
            has(
                &g,
                "/p/tests/src/test/scala/shop/DequeueSuite.scala:boundedTests",
                "/p/tests/src/test/scala/shop/QueueSuite.scala:batchTests"
            ),
            "{:?}",
            edges(&g)
        );
    }

    /// Through a path, a Scala call is the last package's member
    /// (`os.watch.watch(…)`, its package object's, after the file's
    /// imports) or the object's it ends
    /// in (`os.ResourcePath.resource(r)`, its implicit passed explicitly) —
    /// a path naming its package whole: `scala.math.max` is no
    /// `com.acme.math`'s.
    #[test]
    fn a_scala_path_names_its_package_or_object() {
        let g = graph(&[
            (
                "/p/os/watch/package.scala",
                "package os\n\nimport scala.util.Random\n\npackage object watch {\n  def watch(roots: Seq[String], onEvent: Int => Unit, filter: Int = 0): Int = 0\n}\n",
            ),
            (
                "/p/os/ResourcePath.scala",
                "package os\n\nobject ResourcePath {\n  def resource(implicit root: Int = 0): Int = root\n}\n",
            ),
            (
                "/p/acme/Math.scala",
                "package com.acme\n\npackage object math {\n  def max(a: Int, b: Int): Int = a\n}\n",
            ),
            (
                "/p/os/test/WatchTests.scala",
                "package test\n\nobject WatchTests {\n  def go(): Unit = {\n    val w = os.watch.watch(Seq(\"a\"), onEvent = x => ())\n    os.ResourcePath.resource(3)\n    scala.math.max(1, 2)\n  }\n}\n",
            ),
        ]);
        let go = "/p/os/test/WatchTests.scala:go";
        let e = edges(&g);
        assert!(has(&g, go, "/p/os/watch/package.scala:watch"), "{e:?}");
        assert!(has(&g, go, "/p/os/ResourcePath.scala:resource"), "{e:?}");
        assert!(!has(&g, go, "/p/acme/Math.scala:max"), "{e:?}");
    }

    /// A Scala package's member is called through the package: `os.read(p)`
    /// is `object read`'s `apply`, `os.pwd` the package object's `pwd` —
    /// never another class's `read`; and an object called as a function
    /// (`RelPath("a")`) runs its `apply`. A local value of the package's
    /// name is the value, not the package.
    #[test]
    fn a_scala_package_member_and_an_objects_apply_are_called() {
        let g = graph(&[
            (
                "/p/os/src/package.scala",
                "package object os {\n  def pwd: Path = ???\n}\n",
            ),
            (
                "/p/os/src/ReadWriteOps.scala",
                "package os\n\nobject read extends Function1[Path, String] {\n  def apply(p: Path): String = \"\"\n}\n",
            ),
            (
                "/p/os/src/Model.scala",
                "package os\n\nobject RelPath {\n  def apply(s: String): RelPath = ???\n}\n\nclass Reader {\n  def read(): Int = 0\n  def close(): Unit = ()\n}\n",
            ),
            (
                "/p/os/src/Use.scala",
                "package os\n\nobject Use {\n  def go() = RelPath(\"a\")\n}\n",
            ),
            (
                "/p/app/src/Conn.scala",
                "package app\n\nclass Conn {\n  def close(): Unit = ()\n}\n",
            ),
            (
                "/p/app/src/Main.scala",
                "package app\n\nobject Main {\n  def run(): Unit = {\n    os.read(p)\n    os.pwd\n  }\n  def shut(): Unit = {\n    val os = open()\n    os.close()\n  }\n}\n",
            ),
        ]);
        let run = "/p/app/src/Main.scala:run";
        let e = edges(&g);
        assert!(has(&g, run, "/p/os/src/ReadWriteOps.scala:apply"), "{e:?}");
        assert!(has(&g, run, "/p/os/src/package.scala:pwd"), "{e:?}");
        assert!(!has(&g, run, "/p/os/src/Model.scala:read"), "{e:?}");
        assert!(
            has(&g, "/p/os/src/Use.scala:go", "/p/os/src/Model.scala:apply"),
            "{e:?}"
        );
        // `os` bound here is a value: its `close` is a method's, as before.
        assert!(
            has(
                &g,
                "/p/app/src/Main.scala:shut",
                "/p/app/src/Conn.scala:close"
            ),
            "{e:?}"
        );
    }

    /// A spec sees what RSpec loads before it: `spec_helper.rb` (which
    /// `.rspec` requires), what that requires, and `spec/support/`'s files
    /// (required by a glob) — a helper `config.include`s, called bare.
    #[test]
    fn a_spec_sees_what_rspec_loads_before_it() {
        let g = graph(&[
            (
                "/p/spec/spec_helper.rb",
                "require 'shop'\nRSpec.configure do |config|\n  config.include SpecHelpers\nend\n",
            ),
            (
                "/p/spec/support/spec_helpers.rb",
                "module SpecHelpers\n  def mock_app(app)\n    app\n  end\nend\n",
            ),
            (
                "/p/spec/lib/app_spec.rb",
                "describe 'app' do\n  it 'serves' do\n    mock_app(1)\n  end\nend\n",
            ),
            // Another `mock_app`, which nothing loads.
            (
                "/p/contrib/lib/helpers.rb",
                "module Contrib\n  def mock_app(app)\n    app\n  end\nend\n",
            ),
        ]);
        let e = edges(&g);
        assert!(
            e.iter()
                .any(|(a, b)| a.starts_with("/p/spec/lib/app_spec.rb:")
                    && b == "/p/spec/support/spec_helpers.rb:mock_app"),
            "{e:?}"
        );
        assert!(
            !e.iter()
                .any(|(_, b)| b == "/p/contrib/lib/helpers.rb:mock_app"),
            "{e:?}"
        );
    }

    /// A class the grammar could not finish still owns its methods: Kotlin's
    /// grammar reads a property named `where` as its keyword, and the whole
    /// class became an error, every method no class's — so `q.limit(1)` on a
    /// value of no known type, a class's method, reached none.
    #[test]
    fn an_unfinished_class_still_owns_its_methods() {
        let query = "package shop\n\nopen class Query(where: Int?) {\n    fun limit(n: Int): Query = this\n\n    fun where(predicate: Int): Query {\n        where?.let { error(\"twice\") }\n        where = predicate\n        return this\n    }\n}\n";
        // The grammar's error, which the owner is recovered through.
        let tree = crate::highlight::parse(query, Lang::Kotlin).unwrap();
        assert!(tree.root_node().has_error());
        let calls = FileCalls::read(Path::new("/p/src/Query.kt"), query).unwrap();
        assert_eq!(calls.owners.get(&4).map(String::as_str), Some("Query"));
        let g = graph(&[
            ("/p/src/Query.kt", query),
            (
                "/p/src/Run.kt",
                "package shop\n\nfun run() {\n    val q = open()\n    q.limit(1)\n}\n",
            ),
        ]);
        assert!(
            has(&g, "/p/src/Run.kt:run", "/p/src/Query.kt:limit"),
            "{:?}",
            edges(&g)
        );
    }

    /// A C# file Visual Studio saved starts with a byte-order mark: still
    /// in its namespace, so `new CacheEntry<T>(…)` beside it is a call of the
    /// constructor there.
    #[test]
    fn a_file_with_a_byte_order_mark_is_in_its_namespace() {
        let entry = "\u{feff}using System;\n\nnamespace Shop.Services;\n\npublic class CacheEntry<T>\n{\n    public CacheEntry(T item)\n    {\n        Value = item;\n    }\n    public T Value { get; set; }\n}\n";
        assert_eq!(
            package_of(entry, Lang::CSharp).as_deref(),
            Some("Shop.Services")
        );
        let g = graph(&[
            ("/p/src/Services/CacheEntry.cs", entry),
            (
                "/p/src/Services/Decorator.cs",
                "\u{feff}using System;\n\nnamespace Shop.Services;\n\npublic class Decorator\n{\n    public void Refresh(int items)\n    {\n        var entry = new CacheEntry<int>(items);\n    }\n}\n",
            ),
        ]);
        assert!(
            has(
                &g,
                "/p/src/Services/Decorator.cs:Refresh",
                "/p/src/Services/CacheEntry.cs:CacheEntry"
            ),
            "{:?}",
            edges(&g)
        );
    }

    /// A parameter called is the value passed, no function of the
    /// project's that shares its name: bloc's `emit('done')` in an event
    /// handler `(event, emit) { … }` is the handler's emitter, not
    /// `Bloc.emit`; so is a callback in every language whose functions are
    /// values.
    #[test]
    fn a_parameter_called_is_no_project_function() {
        let cases: &[(&str, &str)] = &[
            (
                "/p/lib/counter.dart",
                "class Bloc {\n  void emit(int s) {}\n}\nclass Counter extends Bloc {\n  void setUp() {\n    on((event, emit) {\n      emit(1);\n    });\n  }\n}\n",
            ),
            (
                "/p/lib/handler.dart",
                "class Bloc {\n  void emit(int s) {}\n}\nclass Counter extends Bloc {\n  void _onEvent(CounterEvent event, Emitter<int> emit) {\n    return emit(state + 1);\n  }\n}\n",
            ),
            (
                "/p/src/run.ts",
                "function emit(s: number) {}\nexport function run(emit: (s: number) => void) {\n  emit(1);\n}\nexport const go = (emit) => emit(2);\n",
            ),
            (
                "/p/src/run.py",
                "def emit(s):\n    pass\n\ndef run(emit, *rest):\n    emit(1)\n",
            ),
            (
                "/p/src/Run.kt",
                "package p\n\nfun emit(s: Int) {}\n\nfun run(emit: (Int) -> Unit) {\n    emit(1)\n}\n",
            ),
            (
                "/p/src/run.go",
                "package p\n\nfunc emit(s int) {}\n\nfunc run(emit func(int)) {\n\temit(1)\n}\n",
            ),
            (
                "/p/src/run.c",
                "void emit(int s) {}\n\nvoid run(void (*emit)(int)) {\n    emit(1);\n}\n",
            ),
            (
                "/p/src/Run.cs",
                "namespace P;\n\nclass Run\n{\n    void Emit(int s) {}\n    void Go(Action<int> Emit)\n    {\n        Emit(1);\n    }\n}\n",
            ),
        ];
        for (file, src) in cases {
            let g = graph(&[(file, src)]);
            let e = edges(&g);
            assert!(
                !e.iter()
                    .any(|(_, b)| b.ends_with(":emit") || b.ends_with(":Emit")),
                "{file}: {e:?}"
            );
        }
        // A parameter's default is no parameter: `compute()` in it, and in
        // the body, is the function's.
        let g = graph(&[(
            "/p/src/defaults.py",
            "def compute():\n    return 1\n\ndef run(x=compute()):\n    compute()\n",
        )]);
        assert!(has(
            &g,
            "/p/src/defaults.py:run",
            "/p/src/defaults.py:compute"
        ));
    }

    /// A value of a type the project does not declare has the library's
    /// methods: a C++ field declared as `std::vector<…>`, a Zig parameter
    /// of `*std.Io.Writer`, a Java `AtomicReference` built — none reaches
    /// the project's `size`, `writeAll` or `get`. A C++ local built where it is declared
    /// (`spdlog::pattern_formatter f("%+");`) is its class's exactly. And a
    /// test class's own method is never another value's.
    #[test]
    fn a_library_typed_value_calls_no_project_method() {
        let g = graph(&[
            (
                "/p/include/store.h",
                "#include <vector>\nclass store {\n  std::vector<int> data_;\n public:\n  int size() const { return data_.size(); }\n};\n",
            ),
            (
                "/p/include/pattern_formatter.h",
                "namespace spdlog {\nclass pattern_formatter {\n public:\n  void format(int msg) {}\n};\nclass custom_flag {\n public:\n  void format(int msg) {}\n};\n}\nvoid use() {\n  spdlog::pattern_formatter formatter(\"%+\");\n  formatter.format(1);\n}\n",
            ),
            (
                "/p/src/print.zig",
                "const std = @import(\"std\");\nconst Escaping = struct {\n    fn writeAll(self: *Escaping, s: []const u8) void {}\n};\nfn render(writer: *std.Io.Writer) !void {\n    try writer.writeAll(\"x\");\n}\n",
            ),
            (
                "/p/src/main/java/p/Pool.java",
                "package p;\n\nclass Pool {\n  Object get() { return null; }\n  void park() {\n    AtomicReference<Object> signal = new AtomicReference<>();\n    signal.get();\n  }\n}\n",
            ),
            (
                "/p/src/test/kotlin/p/StatusCodesTest.kt",
                "package p\n\nclass StatusCodesTest {\n    @Test\n    fun put() {\n        client.put(\"/x\")\n        assertThat(1).isSymmetric(2)\n    }\n\n    fun <T : Any> Assert<T>.isSymmetric(expected: T) = expected\n}\n",
            ),
        ]);
        // A test class's extension is called on another value.
        assert!(
            has(
                &g,
                "/p/src/test/kotlin/p/StatusCodesTest.kt:put",
                "/p/src/test/kotlin/p/StatusCodesTest.kt:isSymmetric"
            ),
            "{:?}",
            edges(&g)
        );
        let e = edges(&g);
        for (from, to) in [
            ("/p/include/store.h:size", "/p/include/store.h:size"),
            ("/p/src/print.zig:render", "/p/src/print.zig:writeAll"),
            (
                "/p/src/main/java/p/Pool.java:park",
                "/p/src/main/java/p/Pool.java:get",
            ),
            (
                "/p/src/test/kotlin/p/StatusCodesTest.kt:put",
                "/p/src/test/kotlin/p/StatusCodesTest.kt:put",
            ),
        ] {
            assert!(!has(&g, from, to), "{from} -> {to}: {e:?}");
        }
        let formats: Vec<usize> = g
            .callees_of(
                g.id_of(Path::new("/p/include/pattern_formatter.h"), "use")
                    .unwrap(),
            )
            .iter()
            .copied()
            .filter(|&c| g.node(c).name == "format")
            .collect();
        assert_eq!(formats.len(), 1, "{e:?}");
        assert_eq!(g.node(formats[0]).line, 4, "{e:?}");
    }

    /// A BEAM function is its name and its arity: `parse(A, B)` is
    /// `parse/2`, never `parse/5` — nor is `x |> parse(y)` (two, the piped
    /// value first), `&parse/2` or `fun parse/2`; an Elixir default lets a
    /// call leave its argument out.
    #[test]
    fn a_beam_call_reaches_the_function_of_its_arity() {
        let g = graph(&[
            (
                "/p/src/getopt.erl",
                "-module(getopt).\n-export([run/1]).\n\nparse(A, B) -> parse(A, B, [], [], []).\n\nparse(A, B, C, D, E) -> {A, B, C, D, E}.\n\nrun(X) -> parse(X, 1), F = fun parse/2, F.\n",
            ),
            (
                "/p/lib/cart.ex",
                "defmodule Cart do\n  def total(items), do: items\n  def total(items, tax), do: {items, tax}\n  def price(item, qty \\\\ 1), do: {item, qty}\n  def run(items) do\n    items |> total(0.2)\n    price(:a)\n    Enum.map(items, &total/1)\n  end\nend\n",
            ),
            (
                "/p/lib/checkout.ex",
                "defmodule Checkout do\n  def go(items), do: Enum.map(items, &Cart.total/2)\nend\n",
            ),
        ]);
        let e = edges(&g);
        let calls = |from: &str, to_line: usize| {
            let from = g.id_of(
                Path::new(from.split(':').next().unwrap()),
                from.split(':').nth(1).unwrap(),
            );
            from.is_some_and(|f| g.callees_of(f).iter().any(|&c| g.node(c).line == to_line))
        };
        // `parse/2` calls `parse/5`; `run` calls `parse/2` only.
        assert!(calls("/p/src/getopt.erl:run", 4), "{e:?}");
        assert!(!calls("/p/src/getopt.erl:run", 6), "{e:?}");
        // The pipe passes two: `total/2`, and the capture `total/1`.
        assert!(calls("/p/lib/cart.ex:run", 3), "{e:?}");
        assert!(calls("/p/lib/cart.ex:run", 2), "{e:?}");
        // `price(:a)` leaves the default out.
        assert!(calls("/p/lib/cart.ex:run", 4), "{e:?}");
        // `&Cart.total/2` is `total/2`, not `total/1`.
        assert!(calls("/p/lib/checkout.ex:go", 3), "{e:?}");
        assert!(!calls("/p/lib/checkout.ex:go", 2), "{e:?}");
        let calls_line = |line: usize| {
            let run = g.id_of(Path::new("/p/src/getopt.erl"), "parse").unwrap();
            g.callees_of(run).iter().any(|&c| g.node(c).line == line)
        };
        assert!(calls_line(6), "{e:?}");
    }

    /// A function's arity is read from its own head: past a binary
    /// pattern's commas (`<<1:1, X:7, Tl/binary>>`), past the `@doc` before
    /// it, and with every default of a head written over several lines —
    /// Elixir's `fun` a name, no `fn`.
    #[test]
    fn beam_arities_are_read_from_the_head() {
        let erl = "-module(pb).\n\n-spec read_gr_b(binary(), integer()) -> integer().\n%% read (a, b, c)\nread_gr_b(<<1:1, X:7, Tl/binary>>, N)\n  when N < (32-7) ->\n    read_gr_b(Tl, N + X);\nread_gr_b(<<0:1, _:7, _/binary>>, $,) -> 0.\n";
        let fc = FileCalls::read(Path::new("/p/src/pb.erl"), erl).unwrap();
        assert_eq!(fc.context.arities, [(5, 2, 2)], "{:?}", fc.context.arities);
        let ex = "defmodule Plug.Conn.Query do\n  @doc \"\"\"\n  Decodes (a query), with defaults.\n  \"\"\"\n  def decode(\n        query,\n        initial \\\\ [],\n        invalid \\\\ Plug.Conn.InvalidQueryError,\n        validate_utf8 \\\\ true\n      ) do\n    {query, initial, invalid, validate_utf8}\n  end\nend\n";
        let fc = FileCalls::read(Path::new("/p/lib/query.ex"), ex).unwrap();
        let decode = fc.context.arities.iter().find(|a| a.2 == 4);
        assert_eq!(
            decode.map(|a| (a.1, a.2)),
            Some((1, 4)),
            "{:?}",
            fc.context.arities
        );
        // A default that is an anonymous function: its commas are its own.
        let stub = "defmodule SsoTest do\n  defp stub_uri(\n         assertion \\\\ fn _c, _t, _r, _s -> :ok end\n       ) do\n    assertion\n  end\nend\n";
        let fc = FileCalls::read(Path::new("/p/test/sso_test.exs"), stub).unwrap();
        assert_eq!(fc.context.arities, [(2, 0, 1)], "{:?}", fc.context.arities);
        // Elixir's `fun` is a name (`{mod, fun}`), no anonymous function.
        let forward = "defmodule Plug do\n  defp do_forward({mod, fun}, conn, opts) do\n    apply(mod, fun, [conn, opts])\n  end\nend\n";
        let fc = FileCalls::read(Path::new("/p/lib/plug.ex"), forward).unwrap();
        assert_eq!(fc.context.arities, [(2, 3, 3)], "{:?}", fc.context.arities);
        // Erlang's anonymous `fun (…) -> … end` has commas of its own; its
        // `fun f/1` none to hide.
        let erl = "-module(m).\nf(X, Y) -> lists:map(fun (A, B) -> A end, X), Y.\n";
        let fc = FileCalls::read(Path::new("/p/src/m.erl"), erl).unwrap();
        assert_eq!(fc.context.arities, [(2, 2, 2)], "{:?}", fc.context.arities);
    }

    /// A Go call qualified by an imported package is that package's: a
    /// library's (`require.Error(t, err)`, a value `httptest.NewRecorder()`
    /// made) reaches no project method, a project package's only its own
    /// functions. A Zig struct's `init` makes one of it, and its methods are
    /// its struct's.
    #[test]
    fn package_and_class_qualified_calls_reach_what_they_name() {
        let g = graph(&[
            (
                "/p/errors.go",
                "package gin\n\ntype SliceValidationError []error\n\nfunc (e SliceValidationError) Error() string { return \"\" }\n\ntype mockWriter struct{}\n\nfunc (m *mockWriter) Header() int { return 0 }\n",
            ),
            (
                "/p/binding/default.go",
                "package binding\n\nfunc Validate(v int) error { return nil }\n",
            ),
            (
                "/p/context_test.go",
                "package gin\n\nimport (\n\t\"net/http/httptest\"\n\t\"testing\"\n\n\t\"github.com/gin-gonic/gin/binding\"\n\t\"github.com/stretchr/testify/require\"\n)\n\nfunc TestHeader(t *testing.T) {\n\tw := httptest.NewRecorder()\n\tw.Header()\n\terr := binding.Validate(1)\n\trequire.Error(t, err)\n}\n",
            ),
            (
                "/p/src/params.zig",
                "pub const Params = struct {\n    pub fn init(n: usize) !Params { return .{}; }\n    pub fn get(self: *Params, k: []const u8) ?[]const u8 { return null; }\n};\npub const Router = struct {\n    pub fn get(self: *Router, path: []const u8) void {}\n};\ntest \"params\" {\n    var params = try Params.init(5);\n    _ = params.get(\"p2\");\n}\n",
            ),
        ]);
        let e = edges(&g);
        let from_test = |to: &str| has(&g, "/p/context_test.go:TestHeader", to);
        assert!(!from_test("/p/errors.go:Header"), "{e:?}");
        assert!(!from_test("/p/errors.go:Error"), "{e:?}");
        assert!(from_test("/p/binding/default.go:Validate"), "{e:?}");
        let params_get: Vec<usize> = (0..g.node_count())
            .filter(|&n| g.node(n).file == Path::new("/p/src/params.zig"))
            .flat_map(|n| g.callees_of(n).iter().copied())
            .filter(|&c| g.node(c).name == "get")
            .map(|c| g.node(c).line)
            .collect();
        assert_eq!(params_get, [3], "{e:?}");
    }

    /// A call reaches the overloads that take as many arguments as it
    /// passes: a default may be left out, a variadic takes more, a trailing
    /// lambda is one, a C# extension's `this` and a Zig method's `self` are
    /// passed by the dot, and a C++ default lives in the header.
    #[test]
    fn a_call_reaches_the_overloads_of_its_argument_count() {
        // Each file: its overloads (the lines the call may reach) and a
        // caller `run`.
        let cases: &[(&str, &str, &[usize])] = &[
            (
                "/p/src/W.java",
                "class W {\n  void value(String s) {}\n  void value(String s, int n) {}\n  void spread(String... xs) {}\n  void run() { value(\"a\"); spread(\"a\", \"b\", \"c\"); }\n}\n",
                &[2, 4],
            ),
            (
                "/p/src/K.kt",
                "fun root(w: Int, depth: Int = 0) = w\nfun root() = 0\nfun each(xs: Int, block: (Int) -> Unit) {}\nfun many(vararg xs: Int) {}\nfun run() {\n    root(1)\n    each(1) { it }\n    many(1, 2, 3)\n}\n",
                &[1, 3, 4],
            ),
            (
                "/p/src/E.cs",
                "static class E\n{\n    public static int Twice(this int x) => x * 2;\n    public static int Twice(this int x, int y, int z) => x;\n    public static void Log(params string[] lines) {}\n    public static void Run(int a)\n    {\n        a.Twice();\n        Log(\"a\", \"b\");\n    }\n}\n",
                &[3, 5],
            ),
            (
                "/p/lib/d.dart",
                "void show(int a, {required int b, int c = 0}) {}\nvoid show2(int a, [int? b]) {}\nvoid run() {\n  show(1, b: 2);\n  show2(1);\n}\n",
                &[1, 2],
            ),
            (
                "/p/src/g.go",
                "package g\n\nfunc Join(sep string, parts ...string) string { return sep }\n\nfunc run() {\n\tJoin(\",\", \"a\", \"b\")\n}\n",
                &[3],
            ),
            (
                "/p/src/z.zig",
                "const List = struct {\n    fn insert(self: *List, at: usize, v: u8) void {}\n};\nfn run(l: *List) void {\n    l.insert(0, 1);\n}\n",
                &[2],
            ),
            (
                "/p/src/S.scala",
                "object S {\n  def make(a: Int, b: Int = 1): Int = a\n  def make(): Int = 0\n  def run(): Unit = { make(1) }\n}\n",
                &[2],
            ),
        ];
        for (file, src, lines) in cases {
            let g = graph(&[(file, src)]);
            let run = (0..g.node_count())
                .find(|&n| g.node(n).name.eq_ignore_ascii_case("run"))
                .unwrap();
            let mut reached: Vec<usize> =
                g.callees_of(run).iter().map(|&c| g.node(c).line).collect();
            reached.sort_unstable();
            reached.dedup();
            assert_eq!(&reached, lines, "{file}: {:?}", edges(&g));
        }
        // C++: the header's declaration has the default; the call reaches
        // the definition elsewhere all the same.
        let header = "class Cart {\npublic:\n    int add(int a, int b = 1);\n};\n";
        let source = "#include \"cart.hpp\"\nint Cart::add(int a, int b) { return a + b; }\nint run(Cart &c) { return c.add(1); }\n";
        let g = graph(&[("/p/cart.hpp", header), ("/p/cart.cpp", source)]);
        assert!(
            has(&g, "/p/cart.cpp:run", "/p/cart.cpp:add"),
            "{:?}",
            edges(&g)
        );
    }

    /// A value assigned from a call is of the type the function declares it
    /// returns: `val adapter = moshi.adapter<T>()` then `adapter.fromJson`
    /// is `JsonAdapter`'s, not another class's `fromJson`; a C++
    /// `std::shared_ptr<logger>` hands on `logger`'s methods.
    /// Of overloads that take as many arguments, a call reaches those its
    /// arguments can be passed to — a literal's kind, a value's declared or
    /// built type — and all of them where the arguments say nothing.
    #[test]
    fn a_call_reaches_the_overloads_its_arguments_fit() {
        // (file, source, caller → the lines of `put` it reaches)
        type Reaches<'c> = &'c [(&'c str, &'c [usize])];
        let cases: &[(&str, &str, Reaches)] = &[
            (
                "/p/src/Shop.java",
                "class Cart {}\nclass Order {}\nclass Shop {\n  void put(String k) {}\n  void put(int n) {}\n  void put(Cart c) {}\n  void put(Order o) {}\n  void a() { put(\"x\"); }\n  void b() { put(1); }\n  void c(Cart cart) { put(cart); }\n  void d() { put(new Order()); }\n  void e(Thing t) { put(t.get()); }\n  void f() { put(null); }\n}\n",
                &[
                    ("a", &[4]),
                    ("b", &[5]),
                    ("c", &[6]),
                    ("d", &[7]),
                    ("e", &[4, 5, 6, 7]),
                    ("f", &[4, 6, 7]),
                ],
            ),
            (
                "/p/src/Shop.kt",
                "class Shop {\n    fun put(k: String) {}\n    fun put(n: Int) {}\n    fun put(d: Double?) {}\n    fun a() { put(\"x\") }\n    fun b() { put(1) }\n    fun c() { put(null) }\n    fun d() { put(2.5) }\n}\n",
                &[("a", &[2]), ("b", &[3]), ("c", &[4]), ("d", &[4])],
            ),
            (
                "/p/src/Shop.cs",
                "class Shop\n{\n    void Put(string k) {}\n    void Put(int n) {}\n    void A() { Put(\"x\"); }\n    void B() { Put(1); }\n    void C(int count) { Put(count); }\n}\n",
                &[("A", &[3]), ("B", &[4]), ("C", &[4])],
            ),
            (
                "/p/src/shop.cpp",
                "#include <string>\nvoid put(const std::string& s) {}\nvoid put(int n) {}\nvoid a() { put(\"x\"); }\nvoid b() { put(1); }\nvoid c(std::string s) { put(s); }\nvoid d(Thing t) { put(t); }\n",
                &[("a", &[2]), ("b", &[3]), ("c", &[2]), ("d", &[2, 3])],
            ),
            (
                "/p/src/Shop.scala",
                "object Shop {\n  def put(k: String): Unit = ()\n  def put(n: Int): Unit = ()\n  def a(): Unit = put(\"x\")\n  def b(): Unit = put(1)\n}\n",
                &[("a", &[2]), ("b", &[3])],
            ),
        ];
        for (file, src, expected) in cases {
            let g = graph(&[(file, src)]);
            for (caller, lines) in expected.iter() {
                let at = (0..g.node_count())
                    .find(|&n| g.node(n).name == *caller)
                    .unwrap_or_else(|| panic!("{file}: no {caller}"));
                let mut reached: Vec<usize> = g
                    .callees_of(at)
                    .iter()
                    .filter(|&&c| g.node(c).name.eq_ignore_ascii_case("put"))
                    .map(|&c| g.node(c).line)
                    .collect();
                reached.sort_unstable();
                assert_eq!(reached, *lines, "{file} {caller}: {:?}", edges(&g));
            }
        }
        // The kinds read off the text.
        for (text, lang, kind) in [
            ("\"a\"", Lang::Java, Some("s")),
            ("@\"a\"", Lang::CSharp, Some("s")),
            ("s\"a$b\"", Lang::Scala, Some("s")),
            ("u8\"a\"", Lang::Cpp, Some("s")),
            ("'a'", Lang::Java, Some("c")),
            ("42L", Lang::Kotlin, Some("i")),
            ("0", Lang::Java, Some("0")),
            ("0x1F", Lang::Cpp, Some("i")),
            ("1.5f", Lang::Java, Some("f")),
            ("1e9", Lang::CSharp, Some("f")),
            ("NULL", Lang::Cpp, Some("0")),
            ("nullptr", Lang::Cpp, Some("n")),
            ("\"a\" + b", Lang::Java, None),
            ("a * 2", Lang::Java, None),
        ] {
            assert_eq!(literal_kind(text, lang), kind, "{text}");
        }
        for (ty, lang, kind) in [
            ("const std::string&", Lang::Cpp, "s"),
            ("const char*", Lang::Cpp, "p"),
            ("unsigned long", Lang::Cpp, "i"),
            ("T&&", Lang::Cpp, "?"),
            ("String?", Lang::Kotlin, "s?"),
            ("int?", Lang::CSharp, "i?"),
            ("TValue", Lang::CSharp, "?"),
            ("List<String>", Lang::Java, "TList"),
            ("int[]", Lang::Java, "a"),
            ("IntArray", Lang::Kotlin, "a"),
        ] {
            assert_eq!(type_kind(ty, lang), kind, "{ty}");
        }
    }

    /// Scala's chained package clauses (`package cats.effect` then `package
    /// example`) put the outer package's members in scope, as its own
    /// package's are; one clause naming the whole package does not.
    #[test]
    fn a_scala_chained_package_clause_sees_the_outer_package() {
        let io = "package cats.effect\nsealed abstract class IO[+A] {\n  def as[B](b: B): IO[B] = ???\n  def replicateA_(n: Int): IO[Unit] = ???\n}\n";
        let chained = "package cats.effect\npackage example\n\nobject Example {\n  def run(io: Thing): Unit =\n    (io.step()).as(ExitCode(2))\n}\n";
        let single = "package cats.effect.example\n\nobject Other {\n  def run(io: Thing): Unit =\n    (io.step()).as(ExitCode(2))\n}\n";
        let g = graph(&[
            ("/p/core/IO.scala", io),
            ("/p/example/Example.scala", chained),
            ("/p/example/Other.scala", single),
        ]);
        let edges = edges(&g);
        assert!(
            edges.contains(&(
                "/p/example/Example.scala:run".to_string(),
                "/p/core/IO.scala:as".to_string()
            )),
            "{edges:?}"
        );
        assert!(
            !edges
                .iter()
                .any(|(from, _)| from.starts_with("/p/example/Other.scala")),
            "{edges:?}"
        );
        assert_eq!(
            enclosing_packages("package cats\npackage effect\npackage std\n", Lang::Scala),
            ["cats", "cats.effect"]
        );
    }

    /// A Play template is the function its file compiles to:
    /// `app/views/users/list.scala.html` is `views.html.users.list`, called
    /// from Scala as `views.html.users.list(…)` and from Java as
    /// `views.html.users.list.render(…)` — never a controller's method of
    /// the template's name.
    #[test]
    fn a_play_template_is_called_by_its_package_path() {
        let scala = "package controllers\nclass HomeController {\n  def index() = Ok(views.html.index(\"hi\"))\n  def users() = Ok(views.html.users.list(Nil))\n}\n";
        let java = "package controllers;\npublic class TimeController {\n  public Result index() {\n    return ok(views.html.index.render(\"hi\"));\n  }\n}\n";
        let index = "@(title: String)\n<h1>@title</h1>\n";
        let list = "@import models.User\n@(users: Seq[User])\n<ul></ul>\n";
        let files = [
            ("/p/app/controllers/HomeController.scala", scala),
            ("/p/app/controllers/TimeController.java", java),
        ];
        let mut defs = defs_of(&files);
        for (rel, src) in [
            ("app/views/index.scala.html", index),
            ("app/views/users/list.scala.html", list),
        ] {
            let t = crate::outline::twirl_template(rel, src).expect("a template");
            defs.push(Def {
                name: t.symbol.name,
                kind: t.symbol.kind,
                file: PathBuf::from(format!("/p/{rel}")),
                line: t.symbol.line,
            });
        }
        let sources: Vec<(PathBuf, String)> = files
            .iter()
            .map(|(f, s)| (PathBuf::from(f), s.to_string()))
            .collect();
        let g = ProjectCallGraph::build(defs, &sources, &HashMap::new());
        let edges = edges(&g);
        for (from, to) in [
            (
                "/p/app/controllers/HomeController.scala:index",
                "/p/app/views/index.scala.html:index",
            ),
            (
                "/p/app/controllers/HomeController.scala:users",
                "/p/app/views/users/list.scala.html:list",
            ),
            (
                "/p/app/controllers/TimeController.java:index",
                "/p/app/views/index.scala.html:index",
            ),
        ] {
            assert!(
                edges.contains(&(from.to_string(), to.to_string())),
                "{from} -> {to}: {edges:?}"
            );
        }
        assert_eq!(edges.len(), 3, "{edges:?}");
        assert_eq!(
            template_name(Path::new("/p/app/views/users/list.scala.html")).as_deref(),
            Some("views.html.users.list")
        );
        assert_eq!(
            template_name(Path::new("/p/src/main/twirl/mail/welcome.scala.txt")).as_deref(),
            Some("mail.txt.welcome")
        );
        assert_eq!(template_name(Path::new("/p/app/views/index.html")), None);
        let t = crate::outline::twirl_template("app/views/users/list.scala.html", list).unwrap();
        assert_eq!((t.symbol.name.as_str(), t.symbol.line), ("list", 2));
        assert!(crate::outline::twirl_template("app/views/index.html", index).is_none());
    }

    /// The fourth audit's misses, each a rule of its language:
    /// - a project class neither it nor a project class above it defines a
    ///   method of inherits it from a library (Java's `hashCode`), though
    ///   the project's extensions of it are its own;
    /// - a C++ bare call in a member function is its class's member first;
    /// - a Zig file's import names that file (`posix.read`) or a type of it
    ///   (`BufferPool = @import("buffer.zig").Pool`);
    /// - Dart's named factory is no call of its class, and a mixin's
    ///   override is the method an exact type runs;
    /// - Play's reverse router is generated code;
    /// - an argument of exactly a parameter's class picks that overload;
    /// - a C++ list that shows a default holds the fewest arguments.
    #[test]
    fn fourth_audit_rules() {
        let reached = |g: &ProjectCallGraph, caller: &str| {
            let mut out: Vec<String> = (0..g.node_count())
                .filter(|&n| g.node(n).name == caller)
                .flat_map(|at| g.callees_of(at).to_vec())
                .map(|c| format!("{}:{}", g.node(c).name, g.node(c).line))
                .collect();
            out.sort();
            out.dedup();
            out
        };
        // Library-inherited methods.
        let java = "class TreeMap2 extends java.util.AbstractMap<String, String> {}\nclass Other {\n  public int hashCode() { return 1; }\n}\nclass Use {\n  void a(TreeMap2 m) { m.hashCode(); }\n  void b() { new TreeMap2().hashCode(); }\n}\n";
        let g = graph(&[("/p/src/Use.java", java)]);
        assert_eq!(reached(&g, "a"), Vec::<String>::new());
        assert_eq!(reached(&g, "b"), Vec::<String>::new());
        let kotlin = "class Box : java.util.AbstractList<Int>()\nfun Box.extra(): Int = 1\nclass Other {\n    fun extra(): Int = 2\n    override fun hashCode(): Int = 1\n}\nfun use(b: Box) {\n    b.hashCode()\n    b.extra()\n}\n";
        let g = graph(&[("/p/src/Use.kt", kotlin)]);
        assert_eq!(reached(&g, "use"), ["extra:2"]);
        // C++ class scope.
        let cpp = "class UnitTest {\n public:\n  int listeners() { return 0; }\n};\nclass UnitTestImpl {\n public:\n  int listeners() { return 1; }\n  void configure();\n};\nvoid UnitTestImpl::configure() { listeners(); }\n";
        let g = graph(&[("/p/src/gtest.cc", cpp)]);
        assert_eq!(reached(&g, "configure"), ["listeners:7"]);
        // Zig imports.
        let g = graph(&[
            (
                "/p/src/posix.zig",
                "pub fn read(fd: i32) usize {\n    return 0;\n}\n",
            ),
            (
                "/p/src/t.zig",
                "pub fn reset() void {}\npub const Context = struct {\n    pub fn reset(self: *Context) void {}\n    pub fn read(self: *Context) void {}\n};\npub const Result = struct {\n    pub fn deinit(self: *@This()) void {}\n};\npub fn collect() Result {\n    return .{};\n}\n",
            ),
            (
                "/p/src/sub/use.zig",
                "const t = @import(\"../t.zig\");\ntest \"collects\" {\n    _ = t.collect();\n}\nfn go() void {\n    _ = t.collect();\n}\n",
            ),
            (
                "/p/src/buffer.zig",
                "pub const Pool = struct {\n    pub fn init(a: u8) Pool {\n        return .{};\n    }\n};\n",
            ),
            (
                "/p/src/main.zig",
                "const posix = @import(\"posix.zig\");\nconst t = @import(\"t.zig\");\nconst BufferPool = @import(\"buffer.zig\").Pool;\nconst Conn = struct {\n    pub fn init(a: u8) Conn {\n        return .{};\n    }\n};\nfn run() void {\n    _ = posix.read(1);\n    t.reset();\n    _ = BufferPool.init(2);\n}\n",
            ),
        ]);
        assert_eq!(
            reached(&g, "run"),
            ["init:2", "read:1", "reset:1"],
            "{:?}",
            edges(&g)
        );
        // A file's own function after a struct whose method takes
        // `self: *@This()`, imported from a folder below.
        assert_eq!(reached(&g, "go"), ["collect:9"], "{:?}", edges(&g));
        // Dart.
        let dart = "class Temperature {\n  const Temperature({required this.value});\n  factory Temperature.fromJson(Map<String, dynamic> json) => Temperature(value: 0);\n  final int value;\n}\nclass BlocBase {\n  void onChange(int c) {}\n}\nmixin Hydrated on BlocBase {\n  void onChange(int c) {}\n}\nclass Bloc extends BlocBase {}\nclass HydratedBloc extends Bloc with Hydrated {}\nclass MyBloc extends HydratedBloc {}\nvoid a() {\n  Temperature(value: 1);\n}\nvoid b() {\n  MyBloc().onChange(1);\n}\n";
        let g = graph(&[("/p/lib/weather.dart", dart)]);
        assert_eq!(reached(&g, "a"), ["Temperature:2"]);
        assert_eq!(reached(&g, "b"), ["onChange:10"], "{:?}", edges(&g));
        // Play's reverse router.
        let scala = "package controllers\nclass HomeController {\n  def index() = Ok(\"hi\")\n}\nclass LogoutController {\n  def logout() = Redirect(routes.HomeController.index)\n}\n";
        let g = graph(&[("/p/app/controllers/Logout.scala", scala)]);
        assert_eq!(reached(&g, "logout"), Vec::<String>::new());
        assert!(reverse_route("controllers.routes.HomeController"));
        assert!(reverse_route("routes.javascript.HomeController"));
        assert!(!reverse_route("routes.home"));
        // Exact overloads.
        let scala = "trait GenTemporal {\n  def sleep(time: Duration): Unit = ()\n  def sleep(time: FiniteDuration): Unit = ()\n  def delayBy(time: FiniteDuration): Unit = sleep(time)\n}\n";
        let g = graph(&[("/p/src/GenTemporal.scala", scala)]);
        assert_eq!(reached(&g, "delayBy"), ["sleep:3"]);
        // C++ defaults.
        let cpp = "#include <vector>\ntemplate <typename C>\nint to_hex(const C& container, size_t per_line = 32) { return 0; }\ntemplate <typename It>\nint to_hex(const It first, const It last, size_t per_line = 32) { return 1; }\nvoid run(std::vector<char> buf) {\n  to_hex(buf);\n}\n";
        let g = graph(&[("/p/src/hex.cpp", cpp)]);
        assert_eq!(reached(&g, "run"), ["to_hex:3"]);
        // A destructor a C++ header read as C hides.
        let names: Vec<String> = crate::outline::extract_located(
            "class basic_memory_buffer {\n public:\n  FMT_CONSTEXPR explicit basic_memory_buffer(\n      const Allocator& alloc = Allocator())\n      : alloc_(alloc) {\n    this->set(store_, SIZE);\n  }\n  FMT_CONSTEXPR20 ~basic_memory_buffer() { deallocate(); }\n};\n",
            "c",
        )
        .into_iter()
        .map(|l| l.symbol.name)
        .collect();
        assert_eq!(
            names,
            [
                "basic_memory_buffer",
                "basic_memory_buffer",
                "~basic_memory_buffer"
            ],
            "the class, its constructor and its destructor"
        );
    }

    /// A Go chain of fields from a typed value is of its last field's type:
    /// `c.Request.Header.Set(…)` with `Request *http.Request` is the
    /// library's `Header.Set`, never the project's `Context.Set`;
    /// `c.engine.router.Handle(…)` is `Router`'s; a chain from a library's
    /// value (`w.Body.String()`) is the library's.
    #[test]
    fn a_go_field_chain_has_its_last_fields_type() {
        let src = "package gin\n\nimport (\n\t\"net/http\"\n\t\"net/http/httptest\"\n)\n\ntype Context struct {\n\tRequest *http.Request\n\tengine  *Engine\n\tKeys    map[string]any\n}\n\ntype Engine struct {\n\trouter *Router\n}\n\ntype Router struct{}\n\nfunc (r *Router) Handle(p string) {}\n\nfunc (c *Context) Set(k string, v any) {}\n\ntype Key string\n\nfunc (k Key) String() string { return \"\" }\n\nfunc (c *Context) Header() {\n\tc.Request.Header.Set(\"a\", \"b\")\n}\n\nfunc (c *Context) Route() {\n\tc.engine.router.Handle(\"/\")\n}\n\nfunc probe(w *httptest.ResponseRecorder) {\n\t_ = w.Body.String()\n}\n";
        let g = graph(&[("/p/context.go", src)]);
        let reached = |caller: &str| {
            let at = (0..g.node_count())
                .find(|&n| g.node(n).name == caller)
                .unwrap();
            let mut out: Vec<String> = g
                .callees_of(at)
                .iter()
                .map(|&c| format!("{}:{}", g.node(c).name, g.node(c).line))
                .collect();
            out.sort();
            out
        };
        assert_eq!(reached("Header"), Vec::<String>::new(), "{:?}", edges(&g));
        assert_eq!(reached("Route"), ["Handle:20"], "{:?}", edges(&g));
        assert_eq!(reached("probe"), Vec::<String>::new(), "{:?}", edges(&g));
        let fc = FileCalls::read(Path::new("/p/context.go"), src).unwrap();
        assert!(fc.context.fields.contains(&(
            "Context".to_string(),
            "Request".to_string(),
            "http.Request".to_string()
        )));
        // A map's field is of its elements (`c.Keys["a"]` an `any`).
        assert!(fc.context.fields.contains(&(
            "Context".to_string(),
            "Keys".to_string(),
            "[]any".to_string()
        )));
    }

    /// A Kotlin class's name calls its companion object's members, those it
    /// inherits too; a factory's value is of the type its function returns
    /// (`Users.selectAll()` is a `Query`), the type it is called on a guess
    /// that never makes a method the library's.
    #[test]
    fn a_kotlin_companion_and_a_factorys_return() {
        let src = "open class Query {\n    fun where(op: Int): Query = this\n}\nopen class Table {\n    fun selectAll(): Query = Query()\n}\nobject Users : Table()\nopen class EntityClass<T> {\n    fun new(init: Int): T = TODO()\n}\nopen class IntEntityClass<T> : EntityClass<T>()\nclass User {\n    companion object : IntEntityClass<User>()\n}\nfun a() {\n    Users.selectAll().where(1)\n}\nfun b() {\n    User.new(1)\n}\n";
        let g = graph(&[("/p/src/Dao.kt", src)]);
        let reached = |caller: &str| {
            let at = (0..g.node_count())
                .find(|&n| g.node(n).name == caller)
                .unwrap();
            let mut out: Vec<String> = g
                .callees_of(at)
                .iter()
                .map(|&c| format!("{}:{}", g.node(c).name, g.node(c).line))
                .collect();
            out.sort();
            out
        };
        assert_eq!(reached("a"), ["selectAll:5", "where:2"], "{:?}", edges(&g));
        assert_eq!(reached("b"), ["new:9"], "{:?}", edges(&g));
    }

    /// .NET's service provider hands out the `T` asked for — a project
    /// class's methods, or a library's (`RoleManager<IdentityRole>`); a C#
    /// property in Pascal case after a value is no factory's type.
    #[test]
    fn a_csharp_service_is_the_type_asked_for() {
        let src = "public class ApplicationDbContextInitialiser\n{\n    public Task InitialiseAsync() => Task.CompletedTask;\n}\npublic class DatabaseResetter\n{\n    public static Task<DatabaseResetter> CreateAsync(string c) => null;\n}\npublic static class Init\n{\n    public static async Task A(IServiceScope scope)\n    {\n        var initialiser = scope.ServiceProvider.GetRequiredService<ApplicationDbContextInitialiser>();\n        await initialiser.InitialiseAsync();\n    }\n    public static async Task B(IServiceScope scope)\n    {\n        var roleManager = scope.ServiceProvider.GetRequiredService<RoleManager<IdentityRole>>();\n        await roleManager.CreateAsync(new IdentityRole(\"x\"));\n    }\n    public static async Task C(IServiceProvider sp)\n    {\n        await sp.GetService<ApplicationDbContextInitialiser>().InitialiseAsync();\n    }\n}\n";
        let g = graph(&[("/p/src/Init.cs", src)]);
        let reached = |caller: &str| {
            let at = (0..g.node_count())
                .find(|&n| g.node(n).name == caller)
                .unwrap();
            let mut out: Vec<String> = g
                .callees_of(at)
                .iter()
                .map(|&c| format!("{}:{}", g.node(c).name, g.node(c).line))
                .collect();
            out.sort();
            out
        };
        assert_eq!(reached("A"), ["InitialiseAsync:3"], "{:?}", edges(&g));
        assert_eq!(reached("B"), Vec::<String>::new(), "{:?}", edges(&g));
        assert_eq!(reached("C"), ["InitialiseAsync:3"], "{:?}", edges(&g));
        assert_eq!(
            factory_type("scope.ServiceProvider.GetRequiredService()", Lang::CSharp),
            None
        );
        assert_eq!(
            factory_type("Cart.Create()", Lang::CSharp).as_deref(),
            Some("Cart")
        );
    }

    /// Projects that only share a folder — samples, each with its own build —
    /// call none of each other's code, a Scala file's package import (which
    /// every sample's `models` answers) joining none; one that imports a
    /// file of another by a path or package name of its own (a Dart example
    /// of the package beside it) is one with it.
    #[test]
    fn independent_projects_do_not_call_each_other() {
        let dir = crate::testutil::TempDir::new("independent-projects");
        let root = dir.path();
        let write = |rel: &str, text: &str| {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, text).unwrap();
            path
        };
        write("anorm/build.sbt", "name := \"anorm\"\n");
        write("slick/build.sbt", "name := \"slick\"\n");
        write("packages/bloc/pubspec.yaml", "name: bloc\n");
        write("examples/app/pubspec.yaml", "name: app\n");
        let repo =
            "package models\nclass ComputerRepository {\n  def delete(id: Long): Unit = ()\n}\n";
        let files = [
            write("anorm/app/models/ComputerRepository.scala", repo),
            write(
                "anorm/app/controllers/HomeController.scala",
                "package controllers\nimport models._\nclass HomeController(repo: ComputerRepository) {\n  def remove(id: Long): Unit = repo.delete(id)\n}\n",
            ),
            write("slick/app/models/ComputerRepository.scala", repo),
            write(
                "packages/bloc/lib/bloc.dart",
                "class Bloc {\n  void add(int e) {}\n}\n",
            ),
            write(
                "examples/app/lib/main.dart",
                "import 'package:bloc/bloc.dart';\nvoid go(Bloc b) {\n  b.add(1);\n}\n",
            ),
        ];
        let sources: Vec<(PathBuf, String)> = files
            .iter()
            .map(|f| (f.clone(), std::fs::read_to_string(f).unwrap()))
            .collect();
        let named: Vec<(String, String)> = sources
            .iter()
            .map(|(f, s)| (f.display().to_string(), s.clone()))
            .collect();
        let pairs: Vec<(&str, &str)> = named
            .iter()
            .map(|(f, s)| (f.as_str(), s.as_str()))
            .collect();
        let scope: HashMap<PathBuf, HashSet<PathBuf>> = HashMap::from([
            (
                files[1].clone(),
                HashSet::from([files[0].clone(), files[2].clone()]),
            ),
            (files[4].clone(), HashSet::from([files[3].clone()])),
        ]);
        let g = ProjectCallGraph::build(defs_of(&pairs), &sources, &scope);
        let reached = |caller: &str| {
            let at = (0..g.node_count())
                .find(|&n| g.node(n).name == caller)
                .unwrap();
            let mut out: Vec<String> = g
                .callees_of(at)
                .iter()
                .map(|&c| {
                    let file = &g.node(c).file;
                    file.strip_prefix(root)
                        .unwrap_or(file)
                        .display()
                        .to_string()
                })
                .collect();
            out.sort();
            out
        };
        assert_eq!(
            reached("remove"),
            ["anorm/app/models/ComputerRepository.scala"]
        );
        assert_eq!(reached("go"), ["packages/bloc/lib/bloc.dart"]);
    }

    /// A Go package's variables, declared in one file and used in another,
    /// are of the type their value is (`Form = formBinding{}` declared a
    /// `Binding`): `Form.Bind(…)`, `b := Form; b.Name()` and, from another
    /// package, `binding.Form.Name()` reach `formBinding`'s.
    #[test]
    fn a_go_package_variable_has_its_values_type() {
        let decls = "package binding\n\ntype Binding interface {\n\tName() string\n\tBind(r int) error\n}\n\ntype formBinding struct{}\n\nfunc (formBinding) Name() string { return \"form\" }\n\nfunc (formBinding) Bind(r int) error { return nil }\n\ntype uriBinding struct{}\n\nfunc (uriBinding) Name() string { return \"uri\" }\n\nvar (\n\tForm Binding = formBinding{}\n\tURI          = uriBinding{}\n)\n";
        let user = "package binding\n\nfunc a() {\n\t_ = Form.Bind(1)\n}\n\nfunc b() {\n\tf := Form\n\t_ = f.Name()\n}\n";
        let other = "package gin\n\nimport \"example.com/gin/binding\"\n\nfunc c() {\n\t_ = binding.Form.Name()\n}\n";
        let g = graph(&[
            ("/p/binding/binding.go", decls),
            ("/p/binding/use.go", user),
            ("/p/gin.go", other),
        ]);
        let reached = |caller: &str| {
            let at = (0..g.node_count())
                .find(|&n| g.node(n).name == caller)
                .unwrap();
            let mut out: Vec<String> = g
                .callees_of(at)
                .iter()
                .map(|&c| format!("{}:{}", g.node(c).name, g.node(c).line))
                .collect();
            out.sort();
            out
        };
        assert_eq!(reached("a"), ["Bind:12"], "{:?}", edges(&g));
        assert_eq!(reached("b"), ["Name:10"], "{:?}", edges(&g));
        assert_eq!(reached("c"), ["Name:10"], "{:?}", edges(&g));
        assert_eq!(
            go_package_vars(decls, "binding"),
            [
                (
                    "binding".to_string(),
                    "Form".to_string(),
                    "formBinding".to_string()
                ),
                (
                    "binding".to_string(),
                    "URI".to_string(),
                    "uriBinding".to_string()
                )
            ]
        );
    }

    /// A Scala summoner (`Spawn[IO].background(fa)`) is the type class's
    /// instance: its method, its base's, or an instance's override.
    #[test]
    fn a_scala_summoner_is_its_type_class() {
        let src = "trait GenSpawn[F[_]] {\n  def background[A](fa: F[A]): Int = 0\n}\ntrait Spawn[F[_]] extends GenSpawn[F]\nobject Spawn {\n  def apply[F[_]](implicit F: Spawn[F]): Spawn[F] = F\n}\nclass Other {\n  def background(x: Int): Int = 1\n}\nclass IOLike {\n  def bg(): Int = Spawn[IO].background(this)\n}\n";
        let g = graph(&[("/p/src/Spawn.scala", src)]);
        let at = (0..g.node_count())
            .find(|&n| g.node(n).name == "bg")
            .unwrap();
        let reached: Vec<String> = g
            .callees_of(at)
            .iter()
            .map(|&c| format!("{}:{}", g.node(c).name, g.node(c).line))
            .collect();
        assert_eq!(reached, ["background:2"], "{:?}", edges(&g));
        assert_eq!(scala_summoned("Async[F]").as_deref(), Some("Async"));
        assert_eq!(
            scala_summoned("MonadCancel[F, Throwable]").as_deref(),
            Some("MonadCancel")
        );
        assert_eq!(scala_summoned("xs[0]"), None);
        assert_eq!(
            scala_summoned("implicitly[PathConvertible[T]]").as_deref(),
            Some("PathConvertible")
        );
        assert_eq!(scala_summoned("summon[Async[F]]").as_deref(), Some("Async"));
        assert_eq!(scala_summoned("Ref[IO](1)"), None);
    }

    /// A Ruby class named by its path (`DependencyParser::Ruby::Parse.new`)
    /// is the one in the file its path names, as autoloading lays them out
    /// — never `DependencyParser::PHP::Parse`; one named bare keeps both.
    #[test]
    fn a_ruby_class_by_its_path_is_the_one_its_path_names() {
        let ruby = "module DependencyParser\n  module Ruby\n    class Parse\n      def call\n      end\n    end\n  end\nend\n";
        let php = "module DependencyParser\n  module PHP\n    class Parse\n      def call\n      end\n    end\n  end\nend\n";
        let test = "require_relative '../app/models/dependency_parser/ruby/parse'\nrequire_relative '../app/models/dependency_parser/php/parse'\nclass T\n  def go\n    parser = DependencyParser::Ruby::Parse.new(x)\n    parser.call\n  end\n  def bare\n    Parse.new(y).call\n  end\nend\n";
        let g = graph(&[
            ("/p/app/models/dependency_parser/ruby/parse.rb", ruby),
            ("/p/app/models/dependency_parser/php/parse.rb", php),
            ("/p/test/parse_test.rb", test),
        ]);
        let reached = |caller: &str| {
            let at = (0..g.node_count())
                .find(|&n| g.node(n).name == caller)
                .unwrap();
            let mut out: Vec<String> = g
                .callees_of(at)
                .iter()
                .map(|&c| g.node(c).file.to_string_lossy().to_string())
                .collect();
            out.sort();
            out
        };
        assert_eq!(
            reached("go"),
            ["/p/app/models/dependency_parser/ruby/parse.rb"],
            "{:?}",
            edges(&g)
        );
        assert_eq!(reached("bare").len(), 2, "{:?}", edges(&g));
        assert_eq!(snake_case("DependencyParser"), "dependency_parser");
        assert_eq!(snake_case("HTTPClient"), "http_client");
        assert_eq!(snake_case("PHP"), "php");
    }

    /// A C++ member function defined apart from its class calls its fields'
    /// methods as the class's header declares them: `formatters_.push_back`
    /// with `std::vector<…> formatters_;` is the library's, never another
    /// class's `push_back`; `logger_->info` with `std::shared_ptr<logger>
    /// logger_;` is `logger`'s — `this->logger_` alike — and its
    /// `logger_.reset()` the smart pointer's own.
    #[test]
    fn a_cpp_member_defined_apart_calls_its_fields_as_declared() {
        let header = "namespace spdlog {\nclass flag_formatter {};\nclass logger {\npublic:\n    void info(int a) {}\n};\nclass pattern_formatter {\npublic:\n    void handle_flag_(char flag);\nprivate:\n    std::vector<std::unique_ptr<flag_formatter>> formatters_;\n    std::shared_ptr<logger> logger_;\n    int a_, b_;\n};\n}\n";
        let inl = "namespace fmt {\nclass buffer {\npublic:\n    void push_back(int v) {}\n    void info(int v) {}\n    void reset() {}\n};\n}\nnamespace spdlog {\nvoid pattern_formatter::handle_flag_(char flag) {\n    formatters_.push_back(1);\n    logger_->info(2);\n    this->logger_->info(3);\n    logger_.reset();\n}\n}\n";
        let g = graph(&[
            ("/p/include/spdlog/pattern_formatter.h", header),
            ("/p/include/spdlog/pattern_formatter-inl.h", inl),
        ]);
        let at = (0..g.node_count())
            .find(|&n| {
                g.node(n).name == "handle_flag_"
                    && g.node(n).file.ends_with("pattern_formatter-inl.h")
            })
            .unwrap();
        let reached: Vec<String> = g
            .callees_of(at)
            .iter()
            .map(|&c| {
                format!(
                    "{}:{}",
                    g.node(c).file.file_name().unwrap().to_string_lossy(),
                    g.node(c).name
                )
            })
            .collect();
        assert_eq!(reached, ["pattern_formatter.h:info"], "{:?}", edges(&g));
        let fc =
            FileCalls::read(Path::new("/p/include/spdlog/pattern_formatter.h"), header).unwrap();
        for (field, ty) in [
            ("formatters_", "std::vector"),
            ("logger_", "std::shared_ptr"),
            ("logger_->", "logger"),
            ("a_", "int"),
            ("b_", "int"),
        ] {
            assert!(
                fc.context.fields.contains(&(
                    "pattern_formatter".to_string(),
                    field.to_string(),
                    ty.to_string()
                )),
                "{field}: {:?}",
                fc.context.fields
            );
        }
    }

    /// A Go index on a field is an element of its slice or map: of a named
    /// slice type (`Errors errorMsgs` with `type errorMsgs []*Error`), of a
    /// field's own (`Items []*Item`, `ByName map[string]*Item`).
    #[test]
    fn a_go_index_on_a_field_is_its_element() {
        let go = "package gin\n\ntype Error struct{ Err error }\n\nfunc (msg *Error) Error() string { return \"\" }\n\ntype errorMsgs []*Error\n\ntype Context struct {\n\tErrors errorMsgs\n\tItems  []*Item\n\tByName map[string]*Item\n}\n\ntype Item struct{}\n\nfunc (i *Item) Error() string { return \"\" }\n\ntype other struct{}\n\nfunc (o other) Error() string { return \"\" }\n\nfunc use(c *Context) {\n\t_ = c.Errors[0].Error()\n}\n\nfunc use2(c *Context) { _ = c.Items[1].Error() }\n\nfunc use3(c *Context) { _ = c.ByName[\"a\"].Error() }\n";
        let g = graph(&[("/p/gin/context.go", go)]);
        let reached = |caller: &str| {
            let at = (0..g.node_count())
                .find(|&n| g.node(n).name == caller)
                .unwrap();
            let mut lines: Vec<usize> = g.callees_of(at).iter().map(|&c| g.node(c).line).collect();
            lines.sort_unstable();
            lines
        };
        assert_eq!(reached("use"), [5], "{:?}", edges(&g));
        assert_eq!(reached("use2"), [17], "{:?}", edges(&g));
        assert_eq!(reached("use3"), [17], "{:?}", edges(&g));
        assert_eq!(
            go_indexed_path("c.Errors[0]").as_deref(),
            Some("c.Errors.[]")
        );
        assert_eq!(go_indexed_path("items[0].Name"), None);
        assert_eq!(go_element("map[string]*Route"), Some("Route"));
    }

    /// A Kotlin lambda is passed for a function's type, never for a class of
    /// the project's: `t.insert { … }` is `insert(body: T.(Int) -> Unit)`,
    /// no `insert(selectQuery: AbstractQuery, …)`.
    #[test]
    fn a_kotlin_lambda_is_passed_for_a_function_type() {
        let kt = "abstract class AbstractQuery\nopen class Table\nfun <T : Table> T.insert(body: T.(Int) -> Unit): Int = 0\nfun <T : Table> T.insert(selectQuery: AbstractQuery, columns: List<Int>? = null): Int = 0\nfun go(t: Table) {\n    t.insert { it }\n}\nfun copy(t: Table, q: AbstractQuery) {\n    t.insert(q)\n}\n";
        let g = graph(&[("/p/src/Queries.kt", kt)]);
        let reached = |caller: &str| {
            let at = (0..g.node_count())
                .find(|&n| g.node(n).name == caller)
                .unwrap();
            let mut lines: Vec<usize> = g.callees_of(at).iter().map(|&c| g.node(c).line).collect();
            lines.sort_unstable();
            lines
        };
        assert_eq!(reached("go"), [3], "{:?}", edges(&g));
        assert_eq!(reached("copy"), [4], "{:?}", edges(&g));
    }

    /// A cast picks the overload: `Handle((IRequest<int>) request)` is
    /// `Handle(IRequest<int>)`, `Handle((IRequest) r)` is `Handle(IRequest)`
    /// — never `Handle(object)` around them.
    #[test]
    fn a_cast_picks_the_overload() {
        let cs = "public interface IRequest {}\npublic interface IRequest<out T> {}\npublic class W\n{\n    public void Handle(object request) { Handle((IRequest<int>) request); }\n    public void Handle(IRequest<int> request) { }\n    public void Run(object r) { Handle((IRequest) r); }\n    public void Handle(IRequest request) { }\n    public void Go(object r) { Handle(r as IRequest); }\n}\n";
        let g = graph(&[("/p/src/W.cs", cs)]);
        let reached = |caller: &str, line: usize| {
            let at = (0..g.node_count())
                .find(|&n| g.node(n).name == caller && g.node(n).line == line)
                .unwrap();
            let mut lines: Vec<usize> = g.callees_of(at).iter().map(|&c| g.node(c).line).collect();
            lines.sort_unstable();
            lines
        };
        assert_eq!(reached("Handle", 5), [6], "{:?}", edges(&g));
        assert_eq!(reached("Run", 7), [8], "{:?}", edges(&g));
        assert_eq!(reached("Go", 9), [8], "{:?}", edges(&g));
        assert_eq!(cast_type("(Foo) x", Lang::Java), Some("Foo"));
        assert_eq!(cast_type("(int) n", Lang::CSharp), Some("int"));
        assert_eq!(cast_type("(count) - 1", Lang::Java), None);
        assert_eq!(cast_type("(a + b)", Lang::Java), None);
        assert_eq!(cast_type("x as? Foo", Lang::Kotlin), Some("Foo"));
    }

    /// A Dart call through an import's prefix is that library's: `shelf.
    /// Response(200)` the project's shelf package's class, `http.Response(…)`
    /// (`package:http`, outside the project) none of the project's.
    #[test]
    fn a_dart_prefixed_call_is_its_librarys() {
        let response = "class Response {\n  Response(int code);\n}\n";
        let shelf = "export 'src/response.dart';\n";
        let test = "import 'package:http/http.dart' as http;\nimport 'package:shelf/shelf.dart' as shelf;\n\nvoid viaHttp() {\n  var r = http.Response('x', 200);\n}\nvoid viaShelf() {\n  var s = shelf.Response(200);\n}\n";
        let g = graph(&[
            ("/p/pkgs/shelf/lib/src/response.dart", response),
            ("/p/pkgs/shelf/lib/shelf.dart", shelf),
            ("/p/pkgs/shelf_proxy/test/proxy_test.dart", test),
        ]);
        // (Its library of exports alone read or not, the package's folder.)
        let without_exports = graph(&[
            ("/p/pkgs/shelf/lib/src/response.dart", response),
            ("/p/pkgs/shelf_proxy/test/proxy_test.dart", test),
        ]);
        let at = (0..without_exports.node_count())
            .find(|&n| without_exports.node(n).name == "viaShelf")
            .unwrap();
        assert_eq!(
            without_exports.callees_of(at).len(),
            1,
            "{:?}",
            edges(&without_exports)
        );
        let reached = |caller: &str| -> Vec<String> {
            let at = (0..g.node_count())
                .find(|&n| g.node(n).name == caller)
                .unwrap();
            g.callees_of(at)
                .iter()
                .map(|&c| format!("{}:{}", g.node(c).name, g.node(c).line))
                .collect()
        };
        assert_eq!(reached("viaHttp"), Vec::<String>::new(), "{:?}", edges(&g));
        assert_eq!(reached("viaShelf"), ["Response:2"], "{:?}", edges(&g));
        assert_eq!(
            dart_import_prefix("import 'package:http/http.dart' as http;", "http").as_deref(),
            Some("package:http/http.dart")
        );
    }

    /// A class template's member defined outside it (`template <typename T>
    /// int box<T>::size() const`, `ns::cls::f`) is a function, its class's,
    /// whose calls are its own; a Catch2 `TEST_CASE("…") { … }` (read as a
    /// call, then a block) binds its own names — `auto l = make_a(1)` in one
    /// test, `make_b(1)` in the next.
    #[test]
    fn a_cpp_template_member_and_a_test_body_are_their_own() {
        let header = "class logger {\n public:\n  void info(int x) {}\n};\nclass sink {\n public:\n  void info(int x) {}\n};\nstd::shared_ptr<logger> make_a(int n);\nstd::shared_ptr<sink> make_b(int n);\ntemplate <typename T> struct box {\n  int size() const;\n};\nint count(int n) { return n; }\ntemplate <typename T>\nint box<T>::size() const {\n  return count(1);\n}\nint ns::cls::twice(int n) {\n  return count(n);\n}\n";
        let tests = "#include \"log.h\"\nTEST_CASE(\"a\", \"[log]\") {\n  auto l = make_a(1);\n  l->info(2);\n}\nTEST_CASE(\"b\", \"[log]\") {\n  auto l = make_b(1);\n  l->info(3);\n}\n";
        let files = [("/p/log.h", header), ("/p/log_test.cpp", tests)];
        let sources: Vec<(PathBuf, String)> = files
            .iter()
            .map(|(f, s)| (PathBuf::from(f), s.to_string()))
            .collect();
        let scope: HashMap<PathBuf, HashSet<PathBuf>> = HashMap::from([(
            PathBuf::from("/p/log_test.cpp"),
            HashSet::from([PathBuf::from("/p/log.h")]),
        )]);
        let g = ProjectCallGraph::build(defs_of(&files), &sources, &scope);
        let reached = |name: &str| -> Vec<String> {
            let at = (0..g.node_count())
                .rfind(|&n| g.node(n).name == name)
                .unwrap_or_else(|| panic!("{name}: {:?}", edges(&g)));
            let mut out: Vec<String> = g
                .callees_of(at)
                .iter()
                .map(|&c| format!("{}@{}", g.node(c).name, g.node(c).line))
                .collect();
            out.sort();
            out
        };
        assert_eq!(reached("size"), ["count@14"], "{:?}", edges(&g));
        assert_eq!(reached("twice"), ["count@14"], "{:?}", edges(&g));
        assert_eq!(reached("a"), ["info@3", "make_a@9"], "{:?}", edges(&g));
        assert_eq!(reached("b"), ["info@7", "make_b@10"], "{:?}", edges(&g));
        let fc = FileCalls::read(Path::new("/p/log.h"), header).unwrap();
        assert_eq!(fc.owners.get(&16).map(String::as_str), Some("box"));
        assert_eq!(fc.owners.get(&19).map(String::as_str), Some("cls"));
    }

    /// A Dart library and its parts are one library: the bloc's call of its
    /// state's `copyWith` (`part 'edit_state.dart';`) reaches the part's,
    /// and the part's calls reach what the library imports.
    #[test]
    fn a_dart_library_sees_its_parts_and_they_its_imports() {
        let bloc = "import 'package:bloc/bloc.dart';\nimport 'models/todo.dart';\npart 'edit_state.dart';\nclass EditBloc extends Bloc<EditEvent, EditState> {\n  void onTitle(String t) {\n    final s = EditState(title: t);\n    emit(s.copyWith(title: t));\n  }\n}\n";
        let part = "part of 'edit_bloc.dart';\nclass EditState {\n  EditState({this.title});\n  final String? title;\n  EditState copyWith({String? title}) => EditState(title: title);\n  Todo toTodo() => Todo.make(title);\n}\n";
        let todo = "class Todo {\n  static Todo make(String? t) => Todo();\n  Todo copyWith({String? title}) => Todo();\n}\n";
        let files = [
            ("/p/lib/edit_bloc.dart", bloc),
            ("/p/lib/edit_state.dart", part),
            ("/p/lib/models/todo.dart", todo),
        ];
        let sources: Vec<(PathBuf, String)> = files
            .iter()
            .map(|(f, s)| (PathBuf::from(f), s.to_string()))
            .collect();
        let p = PathBuf::from;
        let scope: HashMap<PathBuf, HashSet<PathBuf>> = HashMap::from([
            (
                p("/p/lib/edit_bloc.dart"),
                HashSet::from([p("/p/lib/edit_state.dart"), p("/p/lib/models/todo.dart")]),
            ),
            (
                p("/p/lib/edit_state.dart"),
                HashSet::from([p("/p/lib/edit_bloc.dart")]),
            ),
        ]);
        let g = ProjectCallGraph::build(defs_of(&files), &sources, &scope);
        let reached = |name: &str| -> Vec<String> {
            let at = (0..g.node_count())
                .find(|&n| g.node(n).name == name)
                .unwrap_or_else(|| panic!("{name}: {:?}", edges(&g)));
            let mut out: Vec<String> = g
                .callees_of(at)
                .iter()
                .map(|&c| {
                    format!(
                        "{}:{}",
                        g.node(c).file.file_name().unwrap().to_string_lossy(),
                        g.node(c).name
                    )
                })
                .collect();
            out.sort();
            out
        };
        assert_eq!(
            reached("onTitle"),
            ["edit_state.dart:EditState", "edit_state.dart:copyWith"],
            "{:?}",
            edges(&g)
        );
        assert_eq!(reached("toTodo"), ["todo.dart:make"], "{:?}", edges(&g));
    }

    /// A name of a statically typed language keeps the type it is declared
    /// with: assigned a call's result (`_subscription = stream.listen(…)`,
    /// in a closure or not), it is still the field's `StreamSubscription`;
    /// a Dart function type (`void Function(int)? callback`) is a
    /// `Function`, whose `call` is no project method; and a null assertion
    /// (`cart!.add()`, Kotlin's `cart!!`) leaves the type as it was.
    #[test]
    fn a_declared_type_outlasts_what_is_assigned() {
        let dart = "class Bloc {\n  StreamSubscription<int>? _subscription;\n  final void Function(int)? callback;\n  Cart? cart;\n  Bloc(Stream<int> stream) {\n    on((_, emit) {\n      _subscription?.cancel();\n      _subscription = stream.listen((i) {});\n    });\n  }\n  void close(Stream<int> stream) {\n    _subscription = stream.listen((i) {});\n    _subscription?.cancel();\n    callback?.call(1);\n    cart!.add(2);\n  }\n}\n";
        let fc = FileCalls::read(Path::new("/p/lib/bloc.dart"), dart).unwrap();
        let receivers: Vec<(usize, &str, Option<&Receiver>)> = fc
            .calls
            .iter()
            .filter(|c| matches!(c.callee.as_str(), "cancel" | "call" | "add"))
            .map(|c| (c.line, c.callee.as_str(), c.receiver.as_ref()))
            .collect();
        let declared = |t: &str| Receiver::Declared(t.to_string());
        assert!(
            receivers.contains(&(7, "cancel", Some(&declared("StreamSubscription")))),
            "{receivers:?}"
        );
        assert!(
            receivers.contains(&(13, "cancel", Some(&declared("StreamSubscription")))),
            "{receivers:?}"
        );
        assert!(
            receivers.contains(&(14, "call", Some(&declared("Function")))),
            "{receivers:?}"
        );
        assert!(
            receivers.contains(&(15, "add", Some(&declared("Cart")))),
            "{receivers:?}"
        );
        // A Kotlin `var` delegated (`by Delegates.notNull()`), of a type
        // nested in another, is of that type, whatever a lambda assigns.
        let kotlin = "fun t() = runTest {\n  var y1: Data.YEntity by Delegates.notNull()\n  run {\n    y1 = Data.YEntity.newSuspend { }\n  }\n  y1.refresh()\n}\n";
        let fc = FileCalls::read(Path::new("/p/src/T.kt"), kotlin).unwrap();
        let refresh = fc.calls.iter().find(|c| c.callee == "refresh").unwrap();
        assert_eq!(refresh.receiver, Some(declared("YEntity")));
        // (Built there, it says which of its type it is.)
        let built = "class T {\n  late Observer observer;\n  void setUp() {\n    observer = MockObserver();\n    observer.onEvent(1);\n  }\n}\n";
        let fc = FileCalls::read(Path::new("/p/test/t_test.dart"), built).unwrap();
        let on_event = fc.calls.iter().find(|c| c.callee == "onEvent").unwrap();
        assert!(
            matches!(&on_event.receiver, Some(Receiver::Declared(t) | Receiver::Type(t)) if t == "MockObserver"),
            "{:?}",
            on_event.receiver
        );
    }

    /// A Kotlin or Scala function with no type written gives what its body
    /// builds (`fun repo() = Repo(db)`), or what the call it ends in gives —
    /// os-lib's `TestUtil.proc(…)`, whose branches both end in
    /// `os.proc(…)`, builds the case class `proc`, so its `.call()` is
    /// `proc`'s, not `ProcGroup`'s.
    #[test]
    fn a_body_with_no_type_written_gives_what_it_builds() {
        let ops = "package os\ncase class proc(command: Shellable*) {\n  def call(cwd: Path = null): CommandResult = ???\n}\ncase class ProcGroup(commands: Seq[proc]) {\n  def call(cwd: Path = null): CommandResult = ???\n}\n";
        let util = "package os\nobject TestUtil {\n  def proc(command: os.Shellable*) = {\n    if (scala.util.Properties.isWin) {\n      val cmd = command.toList\n      os.proc(cmd: _*)\n    } else os.proc(command)\n  }\n}\n";
        let tests = "package os\nobject SubProcessTests {\n  def run() = {\n    val res = TestUtil.proc(\"ls\").call()\n    res\n  }\n}\n";
        let g = graph(&[
            ("/p/os/src/ProcessOps.scala", ops),
            ("/p/os/test/src/TestUtil.scala", util),
            ("/p/os/test/src/SubProcessTests.scala", tests),
        ]);
        let run = (0..g.node_count())
            .find(|&n| g.node(n).name == "run")
            .unwrap();
        let mut reached: Vec<String> = g
            .callees_of(run)
            .iter()
            .map(|&c| format!("{}:{}", g.node(c).name, g.node(c).line))
            .collect();
        reached.sort();
        assert_eq!(reached, ["call:3", "proc:3"], "{:?}", edges(&g));
        let fc = FileCalls::read(Path::new("/p/os/test/src/TestUtil.scala"), util).unwrap();
        assert_eq!(fc.context.returns, [(3, format!("{RETURNED}@os#proc"))]);
        let kotlin = "class Repo(val db: Db) {\n  fun save() {}\n}\nfun repo(db: Db) = Repo(db)\nfun either(a: Boolean) = if (a) Repo(x) else Repo(y)\n";
        let fc = FileCalls::read(Path::new("/p/src/Repo.kt"), kotlin).unwrap();
        assert_eq!(
            fc.context.returns,
            [(4, "Repo".to_string()), (5, "Repo".to_string())]
        );
    }

    /// A value of a project class reaches an overload taking a library's
    /// class only where it can be one: the class names it above itself, it
    /// is what every value of its kind is (`Comparable`, `Any`), or a base
    /// of it is a library's, whose own bases are unknown — an `enum class
    /// Color` is no `List`, a `class Rows : ArrayList<Int>()` may be.
    #[test]
    fn a_project_class_is_no_library_type_it_does_not_reach() {
        let src = "enum class Color { RED }\nclass Rows : ArrayList<Int>()\nfun <T> same(exp: T, act: T) {}\nfun <T> same(exp: T, act: List<T>) {}\nfun <T> order(x: Comparable<T>) {}\nfun total(xs: List<Int>) {}\nfun total(c: Color) {}\nfun one(c: Color) {\n  same(1, c)\n}\nfun many(r: Rows) {\n  total(r)\n}\nfun ordered(c: Color) {\n  order(c)\n}\nfun sums(c: Color) {\n  total(c)\n}\n";
        let g = graph(&[("/p/src/Check.kt", src)]);
        let lines = |caller: &str| -> Vec<usize> {
            let at = (0..g.node_count())
                .find(|&n| g.node(n).name == caller)
                .unwrap();
            let mut out: Vec<usize> = g.callees_of(at).iter().map(|&c| g.node(c).line).collect();
            out.sort_unstable();
            out
        };
        assert_eq!(lines("one"), [3], "{:?}", edges(&g));
        assert_eq!(lines("many"), [6], "{:?}", edges(&g));
        assert_eq!(lines("ordered"), [5], "{:?}", edges(&g));
        assert_eq!(lines("sums"), [7], "{:?}", edges(&g));
    }

    /// A Rails controller's actions run on the instance Rails holds: a
    /// method called on a value of no known type (`user.github`, a model's
    /// attribute) is none of them, while the controller's own bare call of
    /// one is; and `Generator.new(…)` is `Generator#initialize`, never the
    /// `new` action.
    #[test]
    fn a_rails_action_is_called_by_rails() {
        let callbacks = "class Users::OmniauthCallbacksController < ApplicationController\n  def github\n    redirect\n  end\n  def redirect\n    github_login\n  end\n  def github_login\n  end\nend\n";
        let api = "class ApiInfoController < ApplicationController\n  def show\n    user = @repo.subscribers.first\n    user.github\n  end\nend\n";
        let g = graph(&[
            (
                "/p/app/controllers/users/omniauth_callbacks_controller.rb",
                callbacks,
            ),
            ("/p/app/controllers/api_info_controller.rb", api),
        ]);
        let calls = |from: &str, to: &str| {
            edges(&g)
                .iter()
                .any(|(a, b)| a.ends_with(&format!(":{from}")) && b.ends_with(&format!(":{to}")))
        };
        assert!(!calls("show", "github"), "{:?}", edges(&g));
        assert!(calls("github", "redirect"), "{:?}", edges(&g));
        // `Generator.new(…)` runs `Generator#initialize`, never the
        // controller's `new` action.
        let repos = "class ReposController < ApplicationController\n  def new\n  end\nend\n";
        let generator = "class Generator\n  def initialize(name)\n  end\nend\nclass Other\nend\n";
        let test = "class GeneratorTest\n  def test_builds\n    Generator.new(\"x\")\n    Other.new\n  end\nend\n";
        let g = graph(&[
            ("/p/app/controllers/repos_controller.rb", repos),
            ("/p/app/models/generator.rb", generator),
            ("/p/test/unit/generator_test.rb", test),
        ]);
        let builds = (0..g.node_count())
            .find(|&n| g.node(n).name == "test_builds")
            .unwrap();
        let reached: Vec<String> = g
            .callees_of(builds)
            .iter()
            .map(|&c| format!("{}:{}", g.node(c).name, g.node(c).line))
            .collect();
        assert_eq!(reached, ["initialize:2"], "{:?}", edges(&g));
        // A class's own `new` (`class << self; def new`), its subclass's
        // too, and a module's `def self.new`: that `new`.
        let base = "module Sinatra\n  class Base\n    class << self\n      def new(*args)\n      end\n    end\n  end\n  def self.new(base = Base)\n  end\nend\nclass TestMiddleware < Sinatra::Base\nend\n";
        let test = "class BaseTest\n  def test_new\n    TestMiddleware.new(1)\n    Sinatra.new\n  end\nend\n";
        let g = graph(&[
            ("/p/lib/sinatra/base.rb", base),
            ("/p/test/base_test.rb", test),
        ]);
        let at = (0..g.node_count())
            .find(|&n| g.node(n).name == "test_new")
            .unwrap();
        let mut reached: Vec<usize> = g.callees_of(at).iter().map(|&c| g.node(c).line).collect();
        reached.sort_unstable();
        assert_eq!(reached, [4, 8], "{:?}", edges(&g));
    }

    /// Scala reads that call no `def`: a case class's field (`res.exitCode`,
    /// `c.id`) is no other class's method of the name; `new Column(_)`
    /// builds the class (a Java class's constructor, where it is one) and
    /// is no `def Column`; and a method eta-expanded (`(this.apply _)`) is a
    /// function value, whose `tupled` is the library's.
    #[test]
    fn a_scala_read_or_new_calls_no_def_of_the_name() {
        let scala = "package shop\ncase class CommandResult(command: Seq[String], exitCode: Int)\nclass SubProcess {\n  def exitCode(): Int = 0\n}\ncase class Company(id: Option[Long], name: String)\nobject Company {\n  def tupled = (this.apply _).tupled\n}\nobject Dog {\n  def tupled = 1\n}\nobject Repo {\n  def id = 1\n}\nclass Gen {\n  def Column = new Column(1)\n  def make() = new JavaThing()\n}\nobject Use {\n  def run(res: CommandResult, c: Company) = {\n    res.exitCode\n    c.id\n  }\n}\n";
        let java = "package shop;\nclass JavaThing {\n  JavaThing() {}\n  void go() {}\n}\n";
        let g = graph(&[
            ("/p/src/main/scala/shop/Shop.scala", scala),
            ("/p/src/main/java/shop/JavaThing.java", java),
        ]);
        let reached = |name: &str| -> Vec<String> {
            let at = (0..g.node_count())
                .find(|&n| g.node(n).name == name)
                .unwrap_or_else(|| panic!("{name}: {:?}", edges(&g)));
            g.callees_of(at)
                .iter()
                .map(|&c| format!("{}:{}", g.node(c).name, g.node(c).line))
                .collect()
        };
        assert!(reached("run").is_empty(), "{:?}", edges(&g));
        assert!(reached("tupled").is_empty(), "{:?}", edges(&g));
        assert!(reached("Column").is_empty(), "{:?}", edges(&g));
        assert_eq!(reached("make"), ["JavaThing:3"], "{:?}", edges(&g));
        let fc = FileCalls::read(Path::new("/p/src/main/scala/shop/Shop.scala"), scala).unwrap();
        let fields: Vec<(&str, &str)> = fc
            .context
            .fields
            .iter()
            .map(|(c, f, _)| (c.as_str(), f.as_str()))
            .collect();
        assert!(
            fields.contains(&("CommandResult", "exitCode")),
            "{fields:?}"
        );
        assert!(fields.contains(&("Company", "id")), "{fields:?}");
        assert!(
            !fields.iter().any(|(c, _)| *c == "SubProcess"),
            "{fields:?}"
        );
    }

    /// A C++ value built by its class's name (`auto&& buf =
    /// basic_memory_buffer<Char>();`) is of that class — `buf.size()` its
    /// base's `size`, no other class's — and a parameter of a built-in type
    /// (`int id`) passes as one: `args_.get(id)` is `get(int)`, never
    /// `get(basic_string_view<Char>)`.
    #[test]
    fn a_cpp_value_built_or_declared_built_in_is_of_its_type() {
        let src = "template <typename T> class buffer {\n public:\n  auto size() const -> size_t { return 0; }\n};\ntemplate <typename T> class basic_memory_buffer : public buffer<T> {};\nclass format_int {\n public:\n  auto size() const -> size_t { return 1; }\n};\nclass format_args {\n public:\n  auto get(int id) const -> int { return id; }\n  template <typename Char> auto get(basic_string_view<Char> name) const -> int { return 0; }\n};\nclass context {\n  format_args args_;\n public:\n  auto arg(int id) const -> int { return args_.get(id); }\n};\ntemplate <typename Char> void write() {\n  auto&& buf = basic_memory_buffer<Char>();\n  buf.size();\n}\n";
        let g = graph(&[("/p/include/fmt/format.hpp", src)]);
        let reached = |name: &str| -> Vec<usize> {
            let at = (0..g.node_count())
                .find(|&n| g.node(n).name == name)
                .unwrap();
            g.callees_of(at).iter().map(|&c| g.node(c).line).collect()
        };
        assert_eq!(reached("write"), [3], "{:?}", edges(&g));
        assert_eq!(reached("arg"), [12], "{:?}", edges(&g));
    }

    /// Of Kotlin overloads that fit alike, the one that needs no default
    /// value: `vector("e", 4)` is `vector(name, dimensions)`, not `vector<T>(
    /// name, dimensions, format = null)` — which a third argument reaches;
    /// an argument of no known kind fits either.
    #[test]
    fn a_kotlin_call_takes_the_overload_needing_no_default() {
        let src = "open class Table {\n  fun vector(name: String, dimensions: Int?): Int = 0\n  inline fun <reified T : Any> vector(name: String, dimensions: Int?, format: String? = null): Int = 1\n  fun exec(stmt: String, args: List<Int> = emptyList()) {}\n  fun exec(stmt: Statement) {}\n}\nclass Items : Table() {\n  fun two() = vector(\"embedding\", 4)\n  fun three() = vector(\"embedding\", 4, \"f\")\n  fun drop() = exec(load())\n}\n";
        let g = graph(&[("/p/src/Items.kt", src)]);
        let reached = |name: &str| -> Vec<usize> {
            let at = (0..g.node_count())
                .find(|&n| g.node(n).name == name)
                .unwrap();
            g.callees_of(at).iter().map(|&c| g.node(c).line).collect()
        };
        assert_eq!(reached("two"), [2], "{:?}", edges(&g));
        assert_eq!(reached("three"), [3], "{:?}", edges(&g));
        // (An argument of no known kind may be either's.)
        let mut drop = reached("drop");
        drop.sort_unstable();
        assert_eq!(drop, [4, 5], "{:?}", edges(&g));
    }

    /// A Go interface's method returns what it declares (`Params()
    /// Params`), through a chain of fields too (`c.Writer.Params().Get`);
    /// a method only a library's interface it embeds has (`Header()` of
    /// `http.ResponseWriter`) returns a library's value, whose methods are
    /// none of the project's.
    #[test]
    fn a_go_interface_method_returns_what_it_declares() {
        let go = "package gin\n\nimport \"net/http\"\n\ntype ResponseWriter interface {\n\thttp.ResponseWriter\n\tStatus() int\n\tParams() Params\n}\n\ntype Context struct {\n\tWriter ResponseWriter\n}\n\ntype Params []string\n\nfunc (ps Params) Get(name string) (string, bool) { return \"\", false }\n\nfunc (c *Context) Get(key string) (any, bool) { return nil, false }\n\nfunc header(c *Context) {\n\tc.Writer.Header().Get(\"x\")\n}\n\nfunc params(c *Context) {\n\tc.Writer.Params().Get(\"a\")\n}\n\nfunc direct(w ResponseWriter) {\n\tw.Header().Get(\"x\")\n\tw.Params().Get(\"a\")\n}\n";
        let g = graph(&[("/p/gin/context.go", go)]);
        let reached = |name: &str| -> Vec<usize> {
            let at = (0..g.node_count())
                .find(|&n| g.node(n).name == name)
                .unwrap();
            g.callees_of(at).iter().map(|&c| g.node(c).line).collect()
        };
        assert_eq!(reached("header"), [] as [usize; 0], "{:?}", edges(&g));
        assert_eq!(reached("params"), [17], "{:?}", edges(&g));
        assert_eq!(reached("direct"), [17], "{:?}", edges(&g));
    }

    /// A Ruby call on the class itself — a constant (`Job.kind`) or
    /// `self.class` — is a singleton method's (`def self.kind`, a `def` in
    /// `class << self`); one on an instance (`x.perform` after `x =
    /// Job.new`, `new(*args).perform` in a singleton method) an instance
    /// method's; one on `self`, or bare, of the caller's own kind. A bare
    /// `new` builds the caller's own class.
    #[test]
    fn a_ruby_call_on_the_class_is_its_singleton_methods() {
        let rb = "class Job\n  def self.build\n    Job.kind\n    x = Job.new\n    x.perform\n    self.kind\n    kind\n  end\n\n  class << self\n    def made\n      kind\n    end\n  end\n\n  def self.perform(*args)\n    new(*args).perform\n  end\n\n  def initialize(*args)\n  end\n\n  def perform\n  end\n\n  def kind\n    self.class.kind\n  end\n\n  def self.kind\n    \"job\"\n  end\nend\n\nclass Other\n  def perform\n  end\n\n  def kind\n  end\nend\n\nmodule Util\n  module_function\n\n  def helper\n  end\nend\n\nclass Caller\n  def call\n    Util.helper\n  end\nend\n";
        let g = graph(&[("/p/app/job.rb", rb)]);
        let reached = |line: usize| -> Vec<usize> {
            let at = (0..g.node_count())
                .find(|&n| g.node(n).line == line)
                .unwrap();
            let mut lines: Vec<usize> = g.callees_of(at).iter().map(|&c| g.node(c).line).collect();
            lines.sort_unstable();
            lines
        };
        assert_eq!(reached(2), [20, 23, 30], "build: {:?}", edges(&g));
        assert_eq!(reached(11), [30], "made: {:?}", edges(&g));
        assert_eq!(reached(16), [20, 23], "self.perform: {:?}", edges(&g));
        assert_eq!(reached(26), [30], "kind: {:?}", edges(&g));
        // (A module's `module_function`: an instance method called on it.)
        assert_eq!(reached(51), [46], "Util.helper: {:?}", edges(&g));
    }

    /// A C++ value of a type alias (`memory_buffer`) passes for the type it
    /// names exactly (`basic_memory_buffer<T, SIZE>`), before a template's
    /// any type; and a template parameter its `enable_if` constrains takes
    /// only what satisfies it — `42` is `to_string(T)` with `T` integral,
    /// never the one for types not integral.
    #[test]
    fn a_cpp_alias_and_an_enable_if_choose_the_overload() {
        let cpp = "#include <string>\nnamespace fmt {\ntemplate <typename T, size_t SIZE = 500> class basic_memory_buffer {};\nusing memory_buffer = basic_memory_buffer<char>;\ntemplate <typename T, FMT_ENABLE_IF(std::is_integral<T>::value)>\nauto to_string(T value) -> std::string { return {}; }\ntemplate <typename T, FMT_ENABLE_IF(!std::is_integral<T>::value && detail::has_format_as<T>::value)>\nauto to_string(const T& value) -> std::string { return {}; }\ntemplate <typename T, size_t SIZE>\nauto to_string(const basic_memory_buffer<T, SIZE>& buf) -> std::basic_string<T> { return {}; }\ntemplate <typename T, typename = std::enable_if_t<std::is_floating_point<T>::value>>\nauto round(T value) -> T { return value; }\nauto round(const std::string& s) -> std::string { return s; }\nvoid use() { memory_buffer out; to_string(out); }\nvoid use_int() { to_string(42); }\nvoid use_float() { round(1.5); }\ntemplate <typename Float> void use_own() { round(Float(0.5)); }\nclass Float {};\n}\n";
        let g = graph(&[("/p/fmt/format.h", cpp)]);
        let reached = |name: &str| -> Vec<usize> {
            let at = (0..g.node_count())
                .find(|&n| g.node(n).name == name)
                .unwrap();
            g.callees_of(at).iter().map(|&c| g.node(c).line).collect()
        };
        assert_eq!(reached("use"), [10], "{:?}", edges(&g));
        assert_eq!(reached("use_int"), [6], "{:?}", edges(&g));
        assert_eq!(reached("use_float"), [12], "{:?}", edges(&g));
        // (Its own template's `Float`, whatever else bears the name: of any
        // type, the template's overload before one of a class.)
        let mut own = reached("use_own");
        own.sort_unstable();
        assert_eq!(own, [12], "{:?}", edges(&g));
    }

    /// An argument of a class whose name holds a `G` (`GetTodosQuery`) is
    /// ranked by that name: `SendAsync(IRequest<T>)` before
    /// `SendAsync(IBaseRequest)`, as `IRequest<T>` (MediatR's) is the base it
    /// names.
    #[test]
    fn a_class_named_with_a_g_is_ranked_by_its_name() {
        let cs = "using MediatR;\nclass TodosVm {}\nclass GetTodosQuery : IRequest<TodosVm> {}\nstatic class TestApp {\n  public static Task<TResponse> SendAsync<TResponse>(IRequest<TResponse> request) { return null; }\n  public static Task SendAsync(IBaseRequest request) { return null; }\n}\nclass Tests {\n  void Gets() { var query = new GetTodosQuery(); TestApp.SendAsync(query); }\n}\n";
        let g = graph(&[("/p/tests/TestApp.cs", cs)]);
        let at = (0..g.node_count())
            .find(|&n| g.node(n).name == "Gets")
            .unwrap();
        let lines: Vec<usize> = g.callees_of(at).iter().map(|&c| g.node(c).line).collect();
        assert_eq!(lines, [5], "{:?}", edges(&g));
    }

    /// A Kotlin infix call (`c eq 3`) is a call of the function between,
    /// on its left with its right: the member of the name where one takes
    /// it, else an extension of the name that does (Exposed's `infix fun <T>
    /// ExpressionWithColumnType<T>.eq(t: T)` in `ISqlExpressionBuilder`).
    #[test]
    fn a_kotlin_infix_call_is_its_function() {
        let core = "package org.x\ninterface ISqlExpressionBuilder {\n    infix fun <T> ExpressionWithColumnType<T>.eq(t: T): Op<Boolean> = EqOp(this, t)\n}\nobject SqlExpressionBuilder : ISqlExpressionBuilder\nopen class ExpressionWithColumnType<T>\nclass Column<T> : ExpressionWithColumnType<T>() {\n    infix fun eq(other: Column<T>): Boolean = true\n}\n";
        let user = "package org.x\nfun literal(c: Column<Int>) {\n    c eq 3\n}\nfun column(c: Column<Int>, d: Column<Int>) {\n    c eq d\n}\n";
        let g = graph(&[
            ("/p/core/src/main/kotlin/org/x/Op.kt", core),
            ("/p/app/src/main/kotlin/org/x/User.kt", user),
        ]);
        let reached = |name: &str| -> Vec<usize> {
            let at = (0..g.node_count())
                .find(|&n| g.node(n).name == name)
                .unwrap();
            g.callees_of(at).iter().map(|&c| g.node(c).line).collect()
        };
        assert_eq!(reached("literal"), [3], "{:?}", edges(&g));
        assert_eq!(reached("column"), [8], "{:?}", edges(&g));
    }

    /// A named argument (`excludeSettings = …`, C#'s `flag: true`) is
    /// passed for the parameter of its name: only overloads that have one
    /// take the call, and its value must fit it. A Scala lambda (`x => …`)
    /// names nothing.
    #[test]
    fn a_named_argument_takes_the_overloads_with_its_name() {
        let kt = "open class Base {\n  fun withTables(excludeSettings: Collection<Int>, vararg tables: Table, statement: () -> Unit) {}\n  fun withTables(vararg tables: Table, statement: () -> Unit) {}\n}\nobject Users : Table()\nclass T : Base() {\n  fun excluding() = withTables(excludeSettings = listOf(1), Users) { }\n  fun last() = withTables(Users, statement = { })\n}\n";
        let cs = "class S {\n  void Put(string key, int count) {}\n  void Put(string name, bool flag) {}\n  void Counted() { Put(key: \"a\", count: 1); }\n  void Flagged() { Put(name: \"b\", flag: true); }\n  void Wrong() { Put(name: \"b\", flag: 1); }\n}\n";
        let scala = "object M {\n  def f(a: Int, b: String): Int = 0\n  def f(x: Int, y: Int): Int = 1\n  def g(h: Int => Int): Int = 2\n  def g(h: Int => Int, n: Int): Int = 3\n  def named() = f(x = 1, y = 2)\n  def lambda() = g(x => x + 1)\n}\n";
        let g = graph(&[
            ("/p/src/T.kt", kt),
            ("/p/src/S.cs", cs),
            ("/p/src/M.scala", scala),
        ]);
        let reached = |name: &str| -> Vec<usize> {
            let at = (0..g.node_count())
                .find(|&n| g.node(n).name == name)
                .unwrap();
            let mut lines: Vec<usize> = g.callees_of(at).iter().map(|&c| g.node(c).line).collect();
            lines.sort_unstable();
            lines
        };
        assert_eq!(reached("excluding"), [2], "{:?}", edges(&g));
        assert_eq!(reached("last"), [2, 3], "{:?}", edges(&g));
        assert_eq!(reached("Counted"), [2], "{:?}", edges(&g));
        assert_eq!(reached("Flagged"), [3], "{:?}", edges(&g));
        // (Fitting none by its kinds, it says nothing of which.)
        assert_eq!(reached("Wrong"), [2, 3], "{:?}", edges(&g));
        assert_eq!(reached("named"), [3], "{:?}", edges(&g));
        assert_eq!(reached("lambda"), [4], "{:?}", edges(&g));
    }

    /// A base named by its path (`extends kernel.Async[IO]`) is the class
    /// `Async`: a trait's call of its own abstract method reaches the
    /// override there, which it missed under the name `kernel.Async`.
    #[test]
    fn a_base_named_by_its_path_is_its_class() {
        let kernel = "package cats.effect.kernel\ntrait Async[F[_]] {\n  def flatMap[A, B](fa: F[A])(f: A => F[B]): F[B]\n  def fromFuture[A](fut: F[A]): F[A] =\n    flatMap(fut)(a => fut)\n}\n";
        let io = "package cats.effect\nimport cats.effect.kernel\nfinal class IOAsync extends kernel.Async[IO] {\n  def flatMap[A, B](fa: IO[A])(f: A => IO[B]): IO[B] = fa\n}\n";
        let g = graph(&[
            (
                "/p/kernel/src/main/scala/cats/effect/kernel/Async.scala",
                kernel,
            ),
            ("/p/core/src/main/scala/cats/effect/IO.scala", io),
        ]);
        let at = (0..g.node_count())
            .find(|&n| g.node(n).name == "fromFuture")
            .unwrap();
        let mut files: Vec<String> = g
            .callees_of(at)
            .iter()
            .map(|&c| {
                g.node(c)
                    .file
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        files.sort();
        // (The abstract declaration gives way to the definition taking the
        // same parameters.)
        assert_eq!(files, ["IO.scala"], "{:?}", edges(&g));
    }

    /// A Go method's result of another package's type (`PersistentFlags()
    /// *flag.FlagSet`) is the library's value: `c.PersistentFlags().
    /// HasFlags()` calls no `HasFlags` of the project's, while a result of
    /// the project's (`Root() *Command`) calls its own.
    #[test]
    fn a_go_result_of_a_library_type_calls_the_library() {
        let src = "package cobra\n\nimport (\n\tflag \"github.com/spf13/pflag\"\n)\n\ntype Command struct{}\n\nfunc (c *Command) PersistentFlags() *flag.FlagSet { return nil }\n\nfunc (c *Command) HasFlags() bool { return false }\n\nfunc (c *Command) Root() *Command { return c }\n\nfunc run(cmd *Command) {\n\tcmd.PersistentFlags().HasFlags()\n\tcmd.Root().HasFlags()\n}\n";
        let g = graph(&[("/p/cobra/command.go", src)]);
        let run = (0..g.node_count())
            .find(|&n| g.node(n).name == "run")
            .unwrap();
        let mut reached: Vec<&str> = g
            .callees_of(run)
            .iter()
            .map(|&c| g.node(c).name.as_str())
            .collect();
        reached.sort_unstable();
        // (`cmd.Root().HasFlags()` is the project's: `Root` returns a
        // `*Command`.)
        assert_eq!(
            reached,
            ["HasFlags", "PersistentFlags", "Root"],
            "{:?}",
            edges(&g)
        );
        let fc = FileCalls::read(Path::new("/p/cobra/command.go"), src).unwrap();
        let first = fc
            .calls
            .iter()
            .find(|c| c.callee == "HasFlags" && c.line == 16)
            .unwrap();
        assert_eq!(
            first.receiver,
            Some(Receiver::Type(format!("{RETURNED}Command#PersistentFlags")))
        );
    }

    /// A Java bare call reaches its own class's method, a base's, or one a
    /// `static` import brings (`newReader` from `TestUtil`) — never the
    /// same-named method of another class of the package; and a class
    /// literal (`Pizza.class`) is a `Class`, which picks `adapter(Class<T>)`
    /// over `adapter(Type)`.
    #[test]
    fn a_java_bare_call_reaches_what_it_inherits_or_imports() {
        let util = "package com.squareup.moshi;\npublic final class TestUtil {\n  public static JsonReader newReader(String json) { return null; }\n}\n";
        let other = "package com.squareup.moshi;\nclass JsonReaderTest {\n  JsonReader newReader(String json) { return null; }\n}\n";
        let moshi = "package com.squareup.moshi;\npublic class Moshi {\n  public <T> JsonAdapter<T> adapter(Type type) { return null; }\n  public <T> JsonAdapter<T> adapter(Class<T> type) { return null; }\n}\n";
        let test = "package com.squareup.moshi;\nimport static com.squareup.moshi.TestUtil.newReader;\nclass JsonUtf8ReaderTest {\n  void reads(Moshi moshi) {\n    newReader(\"[]\");\n    moshi.adapter(Pizza.class);\n  }\n  void types() {\n    new Named() {{\n      setName(\"Dog\");\n    }};\n  }\n}\n";
        let named = "package com.squareup.moshi;\nclass Named {\n  void setName(String n) {}\n}\n";
        let dir = "/p/moshi/src/test/java/com/squareup/moshi";
        let files = [
            (format!("{dir}/TestUtil.java"), util),
            (format!("{dir}/JsonReaderTest.java"), other),
            (format!("{dir}/Moshi.java"), moshi),
            (format!("{dir}/JsonUtf8ReaderTest.java"), test),
            (format!("{dir}/Named.java"), named),
        ];
        let files: Vec<(&str, &str)> = files.iter().map(|(f, s)| (f.as_str(), *s)).collect();
        let sources: Vec<(PathBuf, String)> = files
            .iter()
            .map(|(f, s)| (PathBuf::from(f), s.to_string()))
            .collect();
        let scope: HashMap<PathBuf, HashSet<PathBuf>> = HashMap::from([(
            PathBuf::from(format!("{dir}/JsonUtf8ReaderTest.java")),
            HashSet::from([PathBuf::from(format!("{dir}/TestUtil.java"))]),
        )]);
        let g = ProjectCallGraph::build(defs_of(&files), &sources, &scope);
        let at = (0..g.node_count())
            .find(|&n| g.node(n).name == "reads")
            .unwrap();
        let mut reached: Vec<String> = g
            .callees_of(at)
            .iter()
            .map(|&c| {
                format!(
                    "{}:{}",
                    g.node(c).file.file_name().unwrap().to_string_lossy(),
                    g.node(c).line
                )
            })
            .collect();
        reached.sort();
        assert_eq!(
            reached,
            ["Moshi.java:4", "TestUtil.java:3"],
            "{:?}",
            edges(&g)
        );
        // (In an anonymous class's body, with none of its own class's
        // nor imported, its base's: `new Named() {{ setName(…); }}`.)
        let types = (0..g.node_count())
            .find(|&n| g.node(n).name == "types")
            .unwrap();
        let reached: Vec<String> = g
            .callees_of(types)
            .iter()
            .map(|&c| {
                g.node(c)
                    .file
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(reached, ["Named.java"], "{:?}", edges(&g));
    }

    /// A Haskell class method applied where the caller's signature's result
    /// is a type with an instance (`setAuthors :: [Inlines] -> Pandoc ->
    /// Pandoc`) is that instance's — the code that runs; at a type variable
    /// (`setAny :: HasMeta a => a -> a`), or passed rather than applied, the
    /// class's own signature.
    #[test]
    fn a_haskell_class_method_is_its_instance_at_the_type_named() {
        let src = "module Text.Pandoc.Builder where\nclass HasMeta a where\n  setMeta :: Text -> b -> a -> a\ninstance HasMeta Pandoc where\n  setMeta k v (Pandoc m bs) = Pandoc m bs\ninstance HasMeta Meta where\n  setMeta k v m = m\nsetAuthors :: [Inlines] -> Pandoc -> Pandoc\nsetAuthors = setMeta \"author\"\nsetAny :: HasMeta a => a -> a\nsetAny = setMeta \"x\" 1\nsetAll :: [Text] -> Pandoc\nsetAll = foldr setMeta mempty\n";
        let g = graph(&[("/p/src/Text/Pandoc/Builder.hs", src)]);
        let reached = |name: &str| -> Vec<usize> {
            let at = (0..g.node_count())
                .find(|&n| g.node(n).name == name)
                .unwrap();
            g.callees_of(at).iter().map(|&c| g.node(c).line).collect()
        };
        assert_eq!(reached("setAuthors"), [5], "{:?}", edges(&g));
        assert_eq!(reached("setAny"), [3], "{:?}", edges(&g));
        // (Passed, not applied — `foldr setMeta`: at whatever type `foldr`
        // gives it, the class's.)
        assert_eq!(reached("setAll"), [3], "{:?}", edges(&g));
        // An instance whose head follows `instance` on lines of its own is
        // one too: its method's own call, at another type, is the class's.
        let multi = "module M where\nclass FromUntaggedValue arity f where\n    parseUntaggedValue :: TypeName :* Options :* FromArgs arity a\n                       -> Value\n                       -> Parser (f a)\n\ninstance\n    ( FromUntaggedValue    arity a\n    , FromUntaggedValue    arity b\n    ) => FromUntaggedValue arity (a :+: b)\n  where\n    parseUntaggedValue p value =\n        L1 <$> parseUntaggedValue p value <|>\n        R1 <$> parseUntaggedValue p value\n    {-# INLINE parseUntaggedValue #-}\n\ninstance {-# OVERLAPPABLE #-}\n    ( ConsFromJSON arity a\n    ) => FromUntaggedValue arity (C1 c a)\n  where\n    parseUntaggedValue p = consParseJSON p\n\nparseNonAllNullarySum :: forall f c arity.\n                         ( FromUntaggedValue arity f\n                         ) => TypeName :* Options :* FromArgs arity c\n                           -> Value -> Parser (f c)\nparseNonAllNullarySum p@(tname :* opts :* _) =\n    case sumEncoding opts of\n      UntaggedValue -> parseUntaggedValue p\n";
        let g = graph(&[("/p/src/M.hs", multi)]);
        let by_line = |line: usize| -> Vec<usize> {
            let at = (0..g.node_count())
                .find(|&n| g.node(n).line == line)
                .unwrap();
            g.callees_of(at).iter().map(|&c| g.node(c).line).collect()
        };
        assert_eq!(by_line(12), [3], "{:?}", edges(&g));
        assert_eq!(by_line(27), [3], "{:?}", edges(&g));
    }

    /// A Haskell function a definition binds itself — in its `where`, a
    /// `let`, as a parameter or a pattern's variable — is what a call of the
    /// name inside it calls, not a function of the name elsewhere.
    #[test]
    fn a_haskell_local_binding_hides_a_function_of_its_name() {
        let a = "module A where\nclass C f where\n    parseU :: Int -> Parser (f a)\nrun :: Int -> Int\nrun x = x\n";
        let b = "module B where\nimport A\nconsFrom :: Int -> Q Exp\nconsFrom jc = do\n  pure (lamExpr 1)\n  where\n    lamExpr value = case value of\n      _ -> parseU value 2\n\n    parseU v w =\n        v\nwith :: (Int -> Int) -> Int\nwith run = run 1\nlambda :: Int\nlambda = (\\run -> run 2) id\nbound :: IO Int\nbound = do\n  run <- pure id\n  pure (run 3)\nlet' :: Int\nlet' = let parseU = id in parseU 4\nglobal :: Int\nglobal = run 5 + parseU 6\nviewed :: Int -> Int\nviewed (run -> n) = run n\n";
        let g = graph(&[("/p/src/A.hs", a), ("/p/src/B.hs", b)]);
        let reached = |name: &str| -> Vec<usize> {
            let at = (0..g.node_count())
                .find(|&n| g.node(n).name == name)
                .unwrap();
            let mut lines: Vec<usize> = g.callees_of(at).iter().map(|&c| g.node(c).line).collect();
            lines.sort_unstable();
            lines
        };
        for local in ["consFrom", "with", "lambda", "bound", "let'"] {
            assert_eq!(reached(local), [] as [usize; 0], "{local}: {:?}", edges(&g));
        }
        assert_eq!(reached("global"), [3, 5], "{:?}", edges(&g));
        // (A view pattern binds what it matches, not the function it
        // applies: `(run -> n)` calls `run`.)
        assert_eq!(reached("viewed"), [5], "{:?}", edges(&g));
    }

    /// Audit 11's shapes: Ruby's `::UserMailer.send_triage` is
    /// `UserMailer`'s, and a bare call in a class's method no unrelated
    /// class's (`doc_methods(:x)`, a fixture accessor); an Elixir bare call
    /// from a test reaches no module nested in its own; C++'s
    /// `fmt::format_int(42).str()` is `format_int`'s; Go's `w.Body.String()`
    /// with `w := PerformRequest(…)` of `*httptest.ResponseRecorder` is the
    /// library's; a Dart test group's local `buildSubject` is its own.
    #[test]
    fn audit_11_shapes() {
        let rb = "class UserMailer\n  def send_triage(user)\n  end\nend\n\nclass UserPreview\n  def send_triage\n    ::UserMailer.send_triage(1)\n  end\nend\n\nclass RepoSubscription\n  def doc_methods\n  end\nend\n\nclass DocsTest < ActiveSupport::TestCase\n  def test_docs\n    doc_methods(:issue)\n  end\nend\n";
        let ex = "defmodule Plug.RouterTest do\n  defmodule SamplePlug do\n    def call(conn, _), do: conn\n  end\n\n  test \"x\" do\n    call(Sample, 1)\n  end\n\n  defp call(mod, conn), do: mod.call(conn, [])\nend\n";
        let cpp = "namespace fmt {\nclass format_int {\n public:\n  explicit format_int(int value) {}\n  auto str() const -> int { return 0; }\n};\nclass string_buffer {\n public:\n  auto str() const -> int { return 0; }\n};\n}\nvoid t() { auto s = fmt::format_int(42).str(); }\n";
        let go = "package gin\n\nimport \"net/http/httptest\"\n\ntype errorMsgs []string\n\nfunc (a errorMsgs) String() string { return \"\" }\n\nfunc PerformRequest(r int) *httptest.ResponseRecorder { return nil }\n\nfunc TestX(t *testing.T) {\n\tw := PerformRequest(1)\n\t_ = w.Body.String()\n}\n";
        let dart = "void main() {\n  group('A', () {\n    Widget buildSubject() => A();\n    test('a', () { buildSubject(); });\n  });\n  group('B', () {\n    Widget buildSubject() => B();\n    test('b', () { buildSubject(); });\n  });\n}\n";
        let files = [
            ("/p/app/x.rb", rb),
            ("/p/test/router_test.exs", ex),
            ("/p/fmt/format.h", cpp),
            ("/p/gin/x_test.go", go),
            ("/p/test/x_test.dart", dart),
        ];
        let g = graph(&files);
        let reached = |file: &str, line: usize| -> Vec<usize> {
            let at = (0..g.node_count())
                .find(|&n| g.node(n).line == line && g.node(n).file.ends_with(file))
                .unwrap();
            let mut lines: Vec<usize> = g.callees_of(at).iter().map(|&c| g.node(c).line).collect();
            lines.sort_unstable();
            lines
        };
        assert_eq!(reached("x.rb", 7), [2], "{:?}", edges(&g));
        assert_eq!(reached("x.rb", 18), [] as [usize; 0], "{:?}", edges(&g));
        assert_eq!(reached("router_test.exs", 6), [10], "{:?}", edges(&g));
        assert_eq!(reached("format.h", 12), [4, 5], "{:?}", edges(&g));
        assert_eq!(reached("x_test.go", 11), [9], "{:?}", edges(&g));
        assert_eq!(reached("x_test.dart", 4), [3], "{:?}", edges(&g));
        assert_eq!(reached("x_test.dart", 8), [7], "{:?}", edges(&g));
    }

    /// A file of a cross-built project's JavaScript sources (`core/js/`)
    /// reaches no definition built for the JVM and Native alone
    /// (`core/jvm-native/`), where one of its own platforms has the name.
    #[test]
    fn a_platforms_source_reaches_its_platforms_definitions() {
        let js = "package cats.effect\nobject IOPlatform {\n  def unsafeRunSync(): Int = 1\n  def run(): Int = unsafeRunSync()\n}\n";
        let jvm = "package cats.effect\nobject IOPlatform {\n  def unsafeRunSync(): Int = 2\n}\n";
        let g = graph(&[
            ("/p/core/js/src/main/scala/cats/effect/IOPlatform.scala", js),
            (
                "/p/core/jvm-native/src/main/scala/cats/effect/IOPlatform.scala",
                jvm,
            ),
        ]);
        let at = (0..g.node_count())
            .find(|&n| g.node(n).name == "run")
            .unwrap();
        let reached: Vec<String> = g
            .callees_of(at)
            .iter()
            .map(|&c| format!("{}:{}", g.node(c).file.display(), g.node(c).line))
            .collect();
        assert_eq!(
            reached,
            ["/p/core/js/src/main/scala/cats/effect/IOPlatform.scala:3"],
            "{:?}",
            edges(&g)
        );
        assert_eq!(
            source_platforms(Path::new("/p/os/src-jvm/x.scala")),
            Some(&["jvm"][..])
        );
        assert_eq!(source_platforms(Path::new("/p/shared/src/x.scala")), None);
    }

    /// Audit 8's shapes: Go's `new(bytes.Buffer)` is the library's value; a
    /// C++ bare call in a free function reaches no class's member; a Kotlin
    /// lambda is passed for no `Iterable`; Zig's decl literal `.of(1, 2)`
    /// takes every argument it passes (no `of(a, b, c)`); a C# class
    /// written to be the plain `IRequest` (a library's) is no
    /// `IRequest<T>`.
    /// A C++ literal reaches only the overloads whose parameter's class it
    /// converts to: by a constructor neither `explicit` nor constrained to a
    /// class (fmt's `locale_ref`), taking what the literal is (`text_style`
    /// takes an `emphasis`); `"…"` is no wide string, `L"…"` no narrow one.
    #[test]
    fn a_cpp_literal_reaches_the_overloads_it_converts_to() {
        let header = "namespace fmt {\nenum class emphasis { bold = 1 };\nclass text_style {\n public:\n  FMT_CONSTEXPR text_style(emphasis em = emphasis()) noexcept : set(false) {}\n  bool set;\n};\nclass locale_ref {\n public:\n  constexpr locale_ref() : locale_(nullptr) {}\n  template <typename Locale, FMT_ENABLE_IF(sizeof(Locale::collate) != 0)>\n  locale_ref(const Locale& loc) : locale_(&loc) {}\n private:\n  const void* locale_;\n};\nclass path_ref {\n public:\n  path_ref(const char* p, int flags = 0) {}\n};\nclass file_ref {\n public:\n  explicit file_ref(const char* p) {}\n};\ntemplate <typename... T>\nauto format(locale_ref loc, format_string<T...> fmt, T&&... args) -> std::string {\n  return {};\n}\ntemplate <typename... T>\nauto format(format_string<T...> fmt, T&&... args) -> std::string {\n  return {};\n}\ntemplate <typename... T>\nauto format(text_style ts, format_string<T...> fmt, T&&... args) -> std::string {\n  return {};\n}\ntemplate <typename... T>\nauto format(wformat_string<T...> fmt, T&&... args) -> std::wstring {\n  return {};\n}\nvoid load(path_ref p) {}\nvoid load(file_ref f) {}\nvoid put(const std::string& s) {}\nvoid put(const std::wstring& s) {}\n}\n";
        let main = "#include \"fmt.h\"\nvoid narrow() { fmt::format(\"{}\", 42); }\nvoid wide() { fmt::format(L\"{}\", 42); }\nvoid loads() { fmt::load(\"x\"); }\nvoid puts() {\n  fmt::put(\"a\");\n  fmt::put(L\"b\");\n}\n";
        let files = [("/p/fmt.h", header), ("/p/main.cc", main)];
        let sources: Vec<(PathBuf, String)> = files
            .iter()
            .map(|(f, s)| (PathBuf::from(f), s.to_string()))
            .collect();
        let scope: HashMap<PathBuf, HashSet<PathBuf>> = HashMap::from([(
            PathBuf::from("/p/main.cc"),
            HashSet::from([PathBuf::from("/p/fmt.h")]),
        )]);
        let g = ProjectCallGraph::build(defs_of(&files), &sources, &scope);
        let lines = |caller: &str| -> Vec<usize> {
            let at = (0..g.node_count())
                .find(|&n| g.node(n).name == caller)
                .unwrap();
            let mut lines: Vec<usize> = g.callees_of(at).iter().map(|&c| g.node(c).line).collect();
            lines.sort_unstable();
            lines
        };
        let fc = FileCalls::read(Path::new("/p/fmt.h"), header).unwrap();
        assert_eq!(lines("narrow"), [29], "{:?}", edges(&g));
        assert_eq!(lines("wide"), [29, 37], "{:?}", edges(&g));
        assert_eq!(lines("loads"), [40], "{:?}", edges(&g));
        assert_eq!(lines("puts"), [42, 43], "{:?}", edges(&g));
        let converts: HashMap<&str, &str> = fc
            .context
            .converts
            .iter()
            .map(|(c, k)| (c.as_str(), k.as_str()))
            .collect();
        assert_eq!(converts.get("emphasis"), Some(&""));
        assert_eq!(converts.get("text_style"), Some(&"Temphasis"));
        assert_eq!(converts.get("locale_ref"), Some(&"K"));
        assert_eq!(converts.get("path_ref"), Some(&"p"));
        assert_eq!(converts.get("file_ref"), Some(&""));
    }

    #[test]
    fn audit_8_shapes_reach_what_they_name() {
        let reached = |g: &ProjectCallGraph, caller: &str| -> Vec<usize> {
            let at = (0..g.node_count())
                .find(|&n| g.node(n).name == caller)
                .unwrap();
            let mut lines: Vec<usize> = g.callees_of(at).iter().map(|&c| g.node(c).line).collect();
            lines.sort_unstable();
            lines
        };
        let go = "package cobra\n\nimport \"bytes\"\n\ntype multi []string\n\nfunc (c *multi) String() string { return \"\" }\n\nfunc TestX(t *testing.T) {\n\tbuf := new(bytes.Buffer)\n\t_ = buf.String()\n}\n";
        let g = graph(&[("/p/x_test.go", go)]);
        assert_eq!(reached(&g, "TestX"), Vec::<usize>::new(), "{:?}", edges(&g));
        let cpp = "class ExampleQt {\npublic:\n    void run() {}\n    void start() { run(); }\n};\nint main() {\n    run();\n    return 0;\n}\n";
        let g = graph(&[("/p/example.cpp", cpp)]);
        assert_eq!(reached(&g, "main"), Vec::<usize>::new(), "{:?}", edges(&g));
        assert_eq!(reached(&g, "start"), [3], "{:?}", edges(&g));
        // A friend defined in its class is a free function.
        let friend = "class bigint {\n  friend auto add_compare(const bigint& a, const bigint& b) -> int { return 0; }\n};\nint dragon(bigint& n) {\n  return add_compare(n, n);\n}\n";
        let g = graph(&[("/p/format.h", friend)]);
        assert_eq!(reached(&g, "dragon"), [2], "{:?}", edges(&g));
        // A specialization's member calls the template's base's bare.
        let nested = "template <typename U> struct nested_formatter {\n  auto nested(const U& value) const -> int { return 0; }\n};\ntemplate <> struct formatter<point> : nested_formatter<double> {\n  auto format(point p) const -> int {\n    return nested(p.x);\n  }\n};\n";
        let g = graph(&[("/p/test/format-test.cc", nested)]);
        assert_eq!(reached(&g, "format"), [2], "{:?}", edges(&g));
        assert!(cpp_friend(
            "template <typename T>\nfriend FMT_CONSTEXPR auto f(T t)"
        ));
        assert!(!cpp_friend("auto friendly(int a)"));
        let kt = "fun exec(stmt: String, args: Iterable<Int> = emptyList()): Int = 0\nfun exec(stmt: String, transform: (Int) -> Int): Int = 0\nfun go() {\n    exec(\"x\") { it }\n}\n";
        let g = graph(&[("/p/src/Exec.kt", kt)]);
        assert_eq!(reached(&g, "go"), [2], "{:?}", edges(&g));
        let zig = "const ResolveOptions = struct {\n    pub fn of(a: u8, b: u8) ResolveOptions {\n        return .{};\n    }\n};\nconst ReferencedType = struct {\n    pub fn of(a: u8, b: u8, c: u8) ReferencedType {\n        return .{};\n    }\n};\nfn resolve(opts: ResolveOptions) void {}\nfn go() void {\n    resolve(.of(1, 2));\n}\n";
        let g = graph(&[("/p/src/a.zig", zig)]);
        assert_eq!(reached(&g, "go"), [2, 11], "{:?}", edges(&g));
        let cs = "public class DeleteCmd : IRequest {}\npublic class TestApp\n{\n    public static Task<T> SendAsync<T>(IRequest<T> request) => null;\n    public static Task SendAsync(IBaseRequest request) => null;\n}\npublic class Use\n{\n    public void A() { TestApp.SendAsync(new DeleteCmd()); }\n}\n";
        let g = graph(&[("/p/src/Use.cs", cs)]);
        assert_eq!(reached(&g, "A"), [5], "{:?}", edges(&g));
    }

    /// A Kotlin class named in backticks (`` `-JsonUtf8Reader` ``) is a
    /// subtype like any: a call on its base reaches its override.
    #[test]
    fn a_kotlin_class_in_backticks_overrides_its_base() {
        let base = "package m\n\nabstract class JsonReader {\n  abstract fun nextLong(): Long\n}\n";
        let sub = "package m\n\ninternal class `-JsonUtf8Reader` : JsonReader() {\n  override fun nextLong(): Long {\n    return 0\n  }\n}\n";
        let use_ = "package m\n\nclass T {\n  fun go(reader: JsonReader) {\n    reader.nextLong()\n  }\n}\n";
        let g = graph(&[
            ("/p/m/JsonReader.kt", base),
            ("/p/m/-JsonUtf8Reader.kt", sub),
            ("/p/m/T.kt", use_),
        ]);
        let e = edges(&g);
        assert!(
            has(&g, "/p/m/T.kt:go", "/p/m/-JsonUtf8Reader.kt:nextLong"),
            "{e:?}"
        );
    }

    /// Of overloads, each its own: an abstract `value(String?)` is reached
    /// through its override, an abstract `value(Long)` no class overrides
    /// is reached itself — neither hidden by a concrete `value(source:
    /// BufferedSource)`, which a Kotlin string never passes for.
    #[test]
    fn an_abstract_overload_is_its_own() {
        let kt = "abstract class JsonWriter {\n  abstract fun value(value: String?): JsonWriter\n  abstract fun value(value: Long): JsonWriter\n  fun value(source: BufferedSource): JsonWriter {\n    return this\n  }\n}\ninternal class JsonValueWriter : JsonWriter() {\n  override fun value(value: String?): JsonWriter {\n    return this\n  }\n}\nclass A {\n  fun text(writer: JsonWriter, value: String) {\n    writer.value(value)\n  }\n  fun number(writer: JsonWriter) {\n    writer.value(5L)\n  }\n}\n";
        let g = graph(&[("/p/src/W.kt", kt)]);
        let reached = |caller: &str| {
            let at = (0..g.node_count())
                .find(|&n| g.node(n).name == caller)
                .unwrap();
            let mut lines: Vec<usize> = g.callees_of(at).iter().map(|&c| g.node(c).line).collect();
            lines.sort_unstable();
            lines
        };
        assert_eq!(reached("text"), [9], "{:?}", edges(&g));
        assert_eq!(reached("number"), [3], "{:?}", edges(&g));
    }

    /// Classes of one name in two modules (a library's jdbc and r2dbc
    /// twins): a value declared as one calls the twin its file sees.
    #[test]
    fn a_value_calls_the_twin_class_its_file_sees() {
        let jdbc = "/p/dao/src/main/kotlin/org/x/dao/Entity.kt";
        let r2dbc = "/p/dao-r2dbc/src/main/kotlin/org/x/dao/r2dbc/Entity.kt";
        let test = "/p/dao-r2dbc-tests/src/test/kotlin/org/x/t/BoardTest.kt";
        let files = [
            (
                jdbc,
                "package org.x.dao\n\nopen class Entity {\n    fun refresh() {}\n}\n",
            ),
            (
                r2dbc,
                "package org.x.dao.r2dbc\n\nopen class Entity {\n    fun refresh() {}\n}\n",
            ),
            (
                test,
                "package org.x.t\n\nimport org.x.dao.r2dbc.Entity\n\nclass BoardTest {\n    fun go(e: Entity) {\n        e.refresh()\n    }\n}\n",
            ),
        ];
        let sources: Vec<(PathBuf, String)> = files
            .iter()
            .map(|(f, s)| (PathBuf::from(f), s.to_string()))
            .collect();
        // (The import resolved: the r2dbc twin is in the test's scope.)
        let scope = HashMap::from([(PathBuf::from(test), HashSet::from([PathBuf::from(r2dbc)]))]);
        let g = ProjectCallGraph::build(defs_of(&files), &sources, &scope);
        let go = format!("{test}:go");
        let e = edges(&g);
        assert!(has(&g, &go, &format!("{r2dbc}:refresh")), "{e:?}");
        assert!(!has(&g, &go, &format!("{jdbc}:refresh")), "{e:?}");
    }

    /// A closure is where its own names are bound: sibling tests' `var
    /// request = make()` and `var request = Request(…)` are two values, each
    /// test's call on its own; a name a closure does not bind is the
    /// function's around it (`cart` captured) — in a statically typed
    /// language, of its type still where another closure assigns it.
    #[test]
    fn a_closure_binds_its_own_names() {
        let typed = |file: &str, src: &str, callee: &str| -> Vec<Option<Receiver>> {
            FileCalls::read(Path::new(file), src)
                .unwrap()
                .calls
                .into_iter()
                .filter(|c| c.callee == callee)
                .map(|c| c.receiver)
                .collect()
        };
        let request = || Some(Receiver::Type("Request".to_string()));
        let dart = "void main() {\n  group('g', () {\n    test('a', () {\n      var request = make();\n      request.x();\n    });\n    test('b', () {\n      var request = Request(\n        'GET',\n      );\n      var copy = request.change(path: 'a');\n    });\n  });\n}\n";
        assert_eq!(typed("/p/test/a_test.dart", dart, "change"), [request()]);
        let captured =
            "void main() {\n  var cart = Cart();\n  test('a', () {\n    cart.add(1);\n  });\n}\n";
        assert_eq!(
            typed("/p/test/b_test.dart", captured, "add"),
            [Some(Receiver::Type("Cart".to_string()))]
        );
        // Assigned in another closure (`setUp`): of the type it is built as
        // there, or one below it.
        let set_up = "void main() {\n  group('g', () {\n    late BlocObserver observer;\n    setUp(() {\n      observer = MockBlocObserver();\n    });\n    test('a', () async {\n      verify(() => observer.onClose(bloc)).called(1);\n    });\n  });\n}\n";
        assert_eq!(
            typed("/p/test/c_test.dart", set_up, "onClose"),
            [Some(Receiver::Declared("MockBlocObserver".to_string()))]
        );
        let go = "package p\n\nfunc TestX(t *testing.T) {\n\tt.Run(\"a\", func(t *testing.T) {\n\t\tr := Make()\n\t\tr.X()\n\t})\n\tt.Run(\"b\", func(t *testing.T) {\n\t\tr := Request{}\n\t\tr.Change()\n\t})\n}\n";
        assert_eq!(typed("/p/x_test.go", go, "Change"), [request()]);
        let ruby = "describe Request do\n  it 'a' do\n    request = make\n    request.x\n  end\n  it 'b' do\n    request = Request.new\n    request.change\n  end\nend\n";
        assert_eq!(
            typed("/p/spec/request_spec.rb", ruby, "change"),
            [request()]
        );
        // Zig's `test` blocks alike.
        let zig = "test \"a\" {\n    var r = make();\n    r.x();\n}\ntest \"b\" {\n    var r = Request.init();\n    r.change();\n}\n";
        let reached = typed("/p/src/request.zig", zig, "change");
        assert!(reached[0].is_some(), "{reached:?}");
    }

    /// C#'s `IRequest` and `IRequest<T>` are two types: `new PairCommand()`
    /// (an `IRequest`) is passed to `Send<TRequest>(TRequest)` constrained
    /// `where TRequest : IRequest`, `new Ping()` (an `IRequest<Pong>`) to
    /// `Send<TResponse>(IRequest<TResponse>)` — each to one overload alone;
    /// a value of a type named both ways, with its arguments unknown, to
    /// either.
    #[test]
    fn a_csharp_generic_type_is_another_than_its_plain_namesake() {
        let cs = "public interface IBaseRequest {}\npublic interface IRequest : IBaseRequest {}\npublic interface IRequest<out TResponse> : IBaseRequest {}\npublic class Pong {}\npublic class Ping : IRequest<Pong> {}\npublic class PairCommand : IRequest {}\npublic class Mediator\n{\n    public Task<TResponse> Send<TResponse>(IRequest<TResponse> request, CancellationToken ct = default) => null;\n    public Task Send<TRequest>(TRequest request, CancellationToken ct = default)\n        where TRequest : IRequest\n    {\n        return null;\n    }\n}\npublic class Use\n{\n    public void A(Mediator m) { m.Send(new PairCommand()); }\n    public void B(Mediator m) { m.Send(new Ping()); }\n    public void C(Mediator m, IRequest<Pong> r) { m.Send(r); }\n}\n";
        let g = graph(&[("/p/src/Mediator.cs", cs)]);
        let reached = |caller: &str| {
            let at = (0..g.node_count())
                .find(|&n| g.node(n).name == caller)
                .unwrap();
            let mut lines: Vec<usize> = g.callees_of(at).iter().map(|&c| g.node(c).line).collect();
            lines.sort_unstable();
            lines
        };
        assert_eq!(reached("A"), [10], "{:?}", edges(&g));
        assert_eq!(reached("B"), [9], "{:?}", edges(&g));
        // A variable's type is read without its type arguments: either.
        assert_eq!(reached("C"), [9, 10], "{:?}", edges(&g));
        assert_eq!(
            csharp_written_bases(
                "public class Ping : IRequest<Pong>, IDisposable",
                "IRequest"
            ),
            ["IRequest`1"]
        );
        assert_eq!(
            csharp_constraint(" where TRequest : class, IRequest\n{", "TRequest"),
            Some("IRequest")
        );
        assert_eq!(csharp_constraint(" where T : new()\n{", "T"), None);
    }

    /// Of the overloads an argument of a known class fits, the most specific:
    /// its own class, then a class it extends or implements, before
    /// `Object`/`object`/`Any` or a class the project knows nothing of.
    #[test]
    fn the_most_specific_overload_is_reached() {
        let cs = "public interface IBaseRequest {}\npublic interface IRequest<T> : IBaseRequest {}\npublic class Ping : IRequest<Pong> {}\npublic class Mediator\n{\n    public Task<T> Send<T>(IRequest<T> request) => null;\n    public Task<object> Send(object request) => null;\n}\npublic class Use\n{\n    public void A(Mediator m)\n    {\n        var ping = new Ping();\n        m.Send(ping);\n    }\n}\n";
        let java = "class Cart {}\nclass Shop {\n  void put(Object o) {}\n  void put(Cart c) {}\n  void a(Cart c) { put(c); }\n  void b(Thing t) { put(t); }\n}\n";
        // A template's own parameter takes the vector; `std::span` is another
        // class.
        let cpp = "#include <vector>\ntemplate <typename Container>\nint to_hex(const Container& c, size_t n = 32) { return 0; }\ntemplate <typename Value, size_t Extent>\nint to_hex(const std::span<Value, Extent>& c, size_t n = 32) { return 1; }\nvoid run(std::vector<char> buf) {\n  to_hex(buf);\n}\n";
        assert_eq!(
            type_parameters(
                "template <typename Container, class It>\nint to_hex",
                Lang::Cpp
            ),
            ["Container", "It"]
        );
        assert_eq!(
            type_parameters("public <T extends Comparable<T>> T max", Lang::Java),
            ["T"]
        );
        assert_eq!(
            type_parameters("fun <reified T : Any> parse", Lang::Kotlin),
            ["T"]
        );
        assert_eq!(
            type_parameters("public Task<TResponse> Send<TResponse>", Lang::CSharp),
            ["TResponse"]
        );
        assert!(type_parameters("public Task<object> Send", Lang::CSharp).is_empty());
        assert_eq!(
            type_parameters("def sleep[F[_], A]", Lang::Scala),
            ["F", "A"]
        );
        assert!(type_parameters("public Map<String, Integer> build", Lang::Java).is_empty());
        for (file, src, caller, callee, want) in [
            ("/p/src/Mediator.cs", cs, "A", "Send", vec![6]),
            ("/p/src/Shop.java", java, "a", "put", vec![4]),
            ("/p/src/Shop.java", java, "b", "put", vec![3, 4]),
            ("/p/src/hex.cpp", cpp, "run", "to_hex", vec![3]),
        ] {
            let g = graph(&[(file, src)]);
            let at = (0..g.node_count())
                .find(|&n| g.node(n).name == caller)
                .unwrap();
            let mut reached: Vec<usize> = g
                .callees_of(at)
                .iter()
                .filter(|&&c| g.node(c).name == callee)
                .map(|&c| g.node(c).line)
                .collect();
            reached.sort_unstable();
            assert_eq!(reached, want, "{file} {caller}: {:?}", edges(&g));
        }
    }

    /// A value assigned from a method of a value of a known type is of the
    /// type that method returns, though other methods of its name return
    /// others (`cells.get(k)` a `Cell`, `refs.get(k)` a `Ref`).
    #[test]
    fn a_methods_result_is_of_its_owners_return_type() {
        let cases: &[(&str, &str)] = &[
            (
                "/p/src/Cells.kt",
                "class Cell {\n    fun getAndSet(v: Int): Int = 0\n}\nclass Ref {\n    fun getAndSet(v: Int): Int = 1\n}\nclass Cells {\n    fun get(k: Int): Cell = Cell()\n}\nclass Refs {\n    fun get(k: Int): Ref = Ref()\n}\nfun run(cells: Cells) {\n    val cell = cells.get(1)\n    cell.getAndSet(2)\n}\n",
            ),
            (
                "/p/src/Cells.java",
                "class Cell {\n  int getAndSet(int v) { return 0; }\n}\nclass Ref {\n  int getAndSet(int v) { return 1; }\n}\nclass Cells {\n  Cell get(int k) { return new Cell(); }\n}\nclass Refs {\n  Ref get(int k) { return new Ref(); }\n}\nclass Use {\n  void run(Cells cells) {\n    var cell = cells.get(1);\n    cell.getAndSet(2);\n  }\n}\n",
            ),
            (
                "/p/cells.go",
                "package cells\n\ntype Cell struct{}\n\nfunc (c *Cell) getAndSet(v int) int { return 0 }\n\ntype Ref struct{}\n\nfunc (r *Ref) getAndSet(v int) int { return 1 }\n\ntype Cells struct{}\n\nfunc (c *Cells) get(k int) *Cell { return &Cell{} }\n\ntype Refs struct{}\n\nfunc (r *Refs) get(k int) *Ref { return &Ref{} }\n\nfunc run(cells *Cells) {\n\tcell := cells.get(1)\n\tcell.getAndSet(2)\n}\n",
            ),
        ];
        for (file, src) in cases {
            let g = graph(&[(file, src)]);
            let at = (0..g.node_count())
                .find(|&n| g.node(n).name == "run")
                .unwrap();
            let reached: Vec<String> = g
                .callees_of(at)
                .iter()
                .filter(|&&c| g.node(c).name == "getAndSet")
                .map(|&c| g.node(c).line.to_string())
                .collect();
            let want = if file.ends_with(".go") { "5" } else { "2" };
            assert_eq!(reached, [want], "{file}: {:?}", edges(&g));
        }
    }

    /// A C++ class declared with its export macro (`class SPDLOG_API logger
    /// {`) — after a namespace's macro alone on its line, too — is the class
    /// its name says, its members its own: a call on a logger reaches the
    /// logger's `info`, not a namespace's.
    #[test]
    fn a_cpp_export_macro_is_no_class_name() {
        let src = "namespace spdlog {\nvoid info(int m) {}\nclass SPDLOG_API logger {\npublic:\n    template <typename T>\n    void info(const T &msg) {}\n};\nclass FMT_API sink : public base {\n    void flush() {}\n};\n}\nvoid run(spdlog::logger &l) {\n    l.info(1);\n}\n";
        let names: Vec<(String, String)> = crate::outline::extract_located(src, "cpp")
            .into_iter()
            .map(|l| (l.symbol.name, l.symbol.kind))
            .collect();
        assert!(
            names.contains(&("logger".to_string(), "class".to_string())),
            "{names:?}"
        );
        assert!(
            names.contains(&("sink".to_string(), "class".to_string())),
            "{names:?}"
        );
        let fc = FileCalls::read(Path::new("/p/logger.h"), src).unwrap();
        assert_eq!(
            fc.owners.get(&6).map(String::as_str),
            Some("logger"),
            "{:?}",
            fc.owners
        );
        // A function's specifier macro before its type hides no function,
        // and a macro called (`FMT_ASSERT(…)`) is left as it is.
        let specified = "namespace spdlog {\nSPDLOG_INLINE std::shared_ptr<logger> get(const std::string &name) {\n    FMT_ASSERT(name.size(), \"x\");\n    return nullptr;\n}\nFMT_CONSTEXPR20 FMT_INLINE auto size() const -> int { return 0; }\n}\n";
        let names: Vec<String> = crate::outline::extract_located(specified, "cpp")
            .into_iter()
            .map(|l| l.symbol.name)
            .collect();
        assert!(names.contains(&"get".to_string()), "{names:?}");
        assert!(names.contains(&"size".to_string()), "{names:?}");
        // A namespace's macro alone on its line before the class
        // (`SPDLOG_NAMESPACE_BEGIN`) is no type the class is read as; an
        // enumerator alone on its line before `}` is left as it is.
        let opened = "#include <string>\nSPDLOG_NAMESPACE_BEGIN\n\nclass SPDLOG_API logger {\npublic:\n    void info(int m) {}\n};\n\nenum class level {\n    trace,\n    LAST_LEVEL\n};\n\nSPDLOG_NAMESPACE_END\n";
        let fc = FileCalls::read(Path::new("/p/logger.h"), opened).unwrap();
        assert_eq!(
            fc.owners.get(&6).map(String::as_str),
            Some("logger"),
            "{:?}",
            fc.owners
        );
        assert!(
            crate::highlight::parse(opened, crate::highlight::Lang::Cpp)
                .is_some_and(|t| !t.root_node().has_error()),
            "a lone macro blanked, the enumerator kept"
        );
        // A macro called alone with no `;` (`FMT_PRAGMA_GCC(push_options)`),
        // and a specifier macro after a template's header on its line, are
        // no C++ either; `TEST(Cart, Adds)` keeps its body.
        let pragmas = "FMT_PRAGMA_GCC(push_options)\nFMT_PRAGMA_MSVC(warning(disable : 4702))\n\nnamespace fmt {\ntemplate <typename... T> FMT_CONSTEXPR void ignore_unused(const T&...) {}\nclass view {\n public:\n  auto size() const -> int { return 0; }\n};\n}\nTEST(Cart, Adds)\n{\n  run();\n}\n";
        let tree = crate::highlight::parse(pragmas, crate::highlight::Lang::Cpp).unwrap();
        assert!(
            !tree.root_node().has_error(),
            "{}",
            tree.root_node().to_sexp()
        );
        let fc = FileCalls::read(Path::new("/p/core.h"), pragmas).unwrap();
        assert_eq!(
            fc.owners.get(&8).map(String::as_str),
            Some("view"),
            "{:?}",
            fc.owners
        );
        let names: Vec<String> = crate::outline::extract_located(pragmas, "cpp")
            .into_iter()
            .map(|l| l.symbol.name)
            .collect();
        assert!(names.contains(&"ignore_unused".to_string()), "{names:?}");
        let g = graph(&[("/p/src/logger.cpp", src)]);
        let at = (0..g.node_count())
            .find(|&n| g.node(n).name == "run")
            .unwrap();
        let reached: Vec<usize> = g.callees_of(at).iter().map(|&c| g.node(c).line).collect();
        assert_eq!(reached, [6], "{:?}", edges(&g));
    }

    /// `super.f()` and C#'s `base.F()` call the base class's method, never
    /// the caller's own override; with no project class above it, a
    /// library's.
    #[test]
    fn a_super_call_is_the_bases_method() {
        let cases: &[(&str, &str, &str, &[usize])] = &[
            (
                "/p/src/Shop.java",
                "class Base {\n  void save() {}\n}\nclass Cart extends Base {\n  void save() {\n    super.save();\n  }\n}\n",
                "save",
                &[2],
            ),
            (
                "/p/src/BlazorComponent.cs",
                "public class BlazorComponent : ComponentBase\n{\n    protected override void OnInitialized()\n    {\n        base.OnInitialized();\n    }\n}\n",
                "OnInitialized",
                &[],
            ),
            (
                "/p/src/Cart.kt",
                "open class Base {\n    open fun save() {}\n}\nclass Cart : Base() {\n    override fun save() {\n        super.save()\n    }\n}\n",
                "save",
                &[2],
            ),
        ];
        for (file, src, name, want) in cases {
            let g = graph(&[(file, src)]);
            let mut reached: Vec<usize> = (0..g.node_count())
                .filter(|&n| g.node(n).name == *name)
                .flat_map(|n| g.callees_of(n).to_vec())
                .map(|c| g.node(c).line)
                .collect();
            reached.sort_unstable();
            reached.dedup();
            assert_eq!(reached, *want, "{file}: {:?}", edges(&g));
        }
    }

    /// A C++ function takes as few arguments as its own list says, unless
    /// the project declares it apart (a header's prototype holding its
    /// defaults): then as few as that declaration allows.
    #[test]
    fn a_cpp_definitions_fewest_is_its_declarations() {
        let header = "int add(int a, int b = 0);\n";
        let source = "#include \"add.h\"\nint add(int a, int b) { return a + b; }\nint pair(int a, int b) { return a; }\nvoid run() {\n  add(1);\n  pair(1);\n}\n";
        let files = [("/p/src/add.h", header), ("/p/src/add.cpp", source)];
        let sources: Vec<(PathBuf, String)> = files
            .iter()
            .map(|(f, s)| (PathBuf::from(f), s.to_string()))
            .collect();
        let scope: HashMap<PathBuf, HashSet<PathBuf>> = HashMap::from([(
            PathBuf::from("/p/src/add.cpp"),
            HashSet::from([PathBuf::from("/p/src/add.h")]),
        )]);
        let g = ProjectCallGraph::build(defs_of(&files), &sources, &scope);
        let run = (0..g.node_count())
            .find(|&n| g.node(n).name == "run")
            .unwrap();
        let mut reached: Vec<String> = g
            .callees_of(run)
            .iter()
            .map(|&c| g.node(c).name.clone())
            .collect();
        reached.sort();
        reached.dedup();
        assert_eq!(reached, ["add"], "{:?}", edges(&g));
    }

    /// A Zig chain of fields is of its last field's type: a standard
    /// library's (`res.headers.get(…)` with `headers: std.StringHashMap`,
    /// `t.arena.allocator()` with `t.zig`'s `var arena =
    /// std.heap.ArenaAllocator.init(…)`) is the library's; a struct's
    /// (`srv.pool.acquire()`) is that struct's — a file struct's own fields
    /// too (`server.collection.push(…)` in `Server.zig`).
    #[test]
    fn a_zig_field_chain_has_its_last_fields_type() {
        let g = graph(&[
            (
                "/p/src/t.zig",
                "const std = @import(\"std\");\npub var arena = std.heap.ArenaAllocator.init(std.heap.page_allocator);\n",
            ),
            (
                "/p/src/kv.zig",
                "pub const KeyValue = struct {\n    pub fn get(self: *KeyValue, k: []const u8) ?u8 {\n        return null;\n    }\n};\npub const Fallback = struct {\n    pub fn allocator(self: *Fallback) u8 {\n        return 0;\n    }\n};\n",
            ),
            (
                "/p/src/main.zig",
                "const std = @import(\"std\");\nconst t = @import(\"t.zig\");\npub const Pool = struct {\n    pub fn acquire(self: *Pool) void {}\n};\npub const Response = struct {\n    headers: std.StringHashMap([]const u8),\n};\npub const Server = struct {\n    pool: Pool,\n};\nfn run(res: *Response) void {\n    _ = res.headers.get(\"a\");\n}\nfn go() void {\n    _ = t.arena.allocator();\n}\nfn serve(srv: *Server) void {\n    srv.pool.acquire();\n}\n",
            ),
        ]);
        let reached = |caller: &str| {
            let at = (0..g.node_count())
                .find(|&n| g.node(n).name == caller)
                .unwrap();
            let mut out: Vec<String> = g
                .callees_of(at)
                .iter()
                .map(|&c| format!("{}:{}", g.node(c).name, g.node(c).line))
                .collect();
            out.sort();
            out
        };
        assert_eq!(reached("run"), Vec::<String>::new(), "{:?}", edges(&g));
        assert_eq!(reached("go"), Vec::<String>::new(), "{:?}", edges(&g));
        assert_eq!(reached("serve"), ["acquire:4"], "{:?}", edges(&g));
        // A file that is a struct: its own fields, at its top level.
        let g = graph(&[
            (
                "/p/src/Collection.zig",
                "const Collection = @This();\nallocator: u8,\n\npub fn push(collection: *Collection, tag: u8) void {}\n",
            ),
            (
                "/p/src/Server.zig",
                "const Server = @This();\nconst Collection = @import(\"Collection.zig\");\n\ncollection: Collection,\nother: u8 = 0,\n\npub fn run(server: *Server) void {\n    server.collection.push(1);\n}\n",
            ),
        ]);
        let at = (0..g.node_count())
            .find(|&n| g.node(n).name == "run")
            .unwrap();
        let reached: Vec<String> = g
            .callees_of(at)
            .iter()
            .map(|&c| g.node(c).name.clone())
            .collect();
        assert_eq!(reached, ["push"], "{:?}", edges(&g));
        // (`pub const` too; a field of the standard library's type calls
        // none of the project's methods of the name.)
        let store = "pub const Store = @This();\ndeclarations: std.MultiArrayList(Declaration),\n\npub fn add(store: *Store) void {\n    store.declarations.append(1);\n}\nconst Filter = struct {\n    pub fn append(f: *Filter, x: u8) void {}\n};\n";
        let g = graph(&[("/p/src/Store.zig", store)]);
        let add = (0..g.node_count())
            .find(|&n| g.node(n).name == "add")
            .unwrap();
        assert!(g.callees_of(add).is_empty(), "{:?}", edges(&g));
        // A type another file re-exports (`pub const Request =
        // request.Request;`): that file's struct's method, no other `init`
        // of the re-exporting file.
        let root = "const request = @import(\"request.zig\");\npub const Request = request.Request;\nconst Handler = struct {\n    pub fn init(a: u8, b: u8) void {}\n};\n";
        let request = "pub const Request = struct {\n    pub fn init(a: u8, b: u8) Request {\n        return .{};\n    }\n};\n";
        let user = "const httpz = @import(\"httpz.zig\");\npub fn make() void {\n    _ = httpz.Request.init(1, 2);\n}\n";
        let g = graph(&[
            ("/p/src/httpz.zig", root),
            ("/p/src/request.zig", request),
            ("/p/src/t.zig", user),
        ]);
        let make = (0..g.node_count())
            .find(|&n| g.node(n).name == "make")
            .unwrap();
        let reached: Vec<String> = g
            .callees_of(make)
            .iter()
            .map(|&c| format!("{}", g.node(c).file.display()))
            .collect();
        assert_eq!(reached, ["/p/src/request.zig"], "{:?}", edges(&g));
    }

    /// One link of a chain is of what its method returns on the type it is
    /// called on: `const Pipeline().addMiddleware(m).addHandler(h)` calls
    /// `Pipeline`'s `addHandler`, not an extension of the same name on
    /// another type; `repo.find(id).total()` the found `Cart`'s.
    #[test]
    fn a_chain_link_is_of_its_methods_return_type() {
        let dart = "class Pipeline {\n  const Pipeline();\n  Pipeline addMiddleware(int m) => this;\n  int addHandler(int h) => 0;\n}\nextension MiddlewareExtensions on Function {\n  int addHandler(int h) => 1;\n}\nvoid run() {\n  const Pipeline().addMiddleware(1).addHandler(2);\n}\n";
        let java = "class Cart {\n  int total() { return 0; }\n}\nclass Order {\n  int total() { return 1; }\n}\nclass Repo {\n  Cart find(int id) { return null; }\n}\nclass Use {\n  void run(Repo repo) {\n    repo.find(1).total();\n  }\n}\n";
        for (file, src, method, want) in [
            ("/p/lib/pipeline.dart", dart, "addHandler", 4),
            ("/p/src/Use.java", java, "total", 2),
        ] {
            let g = graph(&[(file, src)]);
            let at = (0..g.node_count())
                .find(|&n| g.node(n).name == "run")
                .unwrap();
            let reached: Vec<usize> = g
                .callees_of(at)
                .iter()
                .filter(|&&c| g.node(c).name == method)
                .map(|&c| g.node(c).line)
                .collect();
            assert_eq!(reached, [want], "{file}: {:?}", edges(&g));
        }
        assert_eq!(
            chain_link("const Pipeline().addMiddleware(m)"),
            Some(("const Pipeline()", "addMiddleware"))
        );
        assert_eq!(chain_link("repo.find(id)"), Some(("repo", "find")));
        assert_eq!(chain_link("find(id)"), None);
    }

    /// A method called on a value of no known type reaches no function
    /// declared inside another (a local `def`), which a bare call in its
    /// function still does; a Zig function's result of a standard-library
    /// type the file names by an alias is the library's.
    #[test]
    fn nested_functions_and_zig_library_results() {
        let scala = "class IOFiber {\n  def cancel(): Unit = ()\n}\nobject IOPlatform {\n  def unsafeRunTimed(): Unit = {\n    def cancel(): Unit = ()\n    cancel()\n  }\n}\nclass Suite {\n  def t(): Unit = make().cancel()\n}\n";
        let g = graph(&[("/p/src/IO.scala", scala)]);
        let reached = |g: &ProjectCallGraph, caller: &str| {
            let at = (0..g.node_count())
                .find(|&n| g.node(n).name == caller)
                .unwrap();
            let mut out: Vec<usize> = g.callees_of(at).iter().map(|&c| g.node(c).line).collect();
            out.sort_unstable();
            out
        };
        assert_eq!(reached(&g, "t"), [2], "{:?}", edges(&g));
        assert_eq!(reached(&g, "unsafeRunTimed"), [6], "{:?}", edges(&g));
        let zig = "const std = @import(\"std\");\nconst Io = std.Io;\npub const Socket = struct {\n    pub fn close(self: *Socket) void {}\n};\nfn testStream() Io.net.Stream {\n    return undefined;\n}\nfn run() void {\n    const stream = testStream();\n    stream.close();\n}\n";
        let g = graph(&[("/p/src/net.zig", zig)]);
        assert_eq!(
            reached(&g, "run").into_iter().filter(|&l| l == 4).count(),
            0,
            "{:?}",
            edges(&g)
        );
    }

    /// A Scala member read with no parentheses (`res.exitCode`, a case
    /// class's field) is no call of a Scala `def exitCode()`, which Scala 3
    /// calls only as `exitCode()`; it is one of a `def isAlive: Boolean`,
    /// and `exitCode()` is that method's.
    #[test]
    fn a_scala_read_without_parentheses_is_no_call_of_a_parenthesised_def() {
        let src = "case class CommandResult(exitCode: Int)\nclass SubProcess {\n  def exitCode(): Int = 0\n  def isAlive: Boolean = true\n}\nclass Use {\n  def a(): Int = make().exitCode\n  def b(): Boolean = make().isAlive\n  def c(): Int = make().exitCode()\n}\n";
        let g = graph(&[("/p/src/Proc.scala", src)]);
        let reached = |caller: &str| {
            let at = (0..g.node_count())
                .find(|&n| g.node(n).name == caller)
                .unwrap();
            let mut out: Vec<String> = g
                .callees_of(at)
                .iter()
                .map(|&c| format!("{}:{}", g.node(c).name, g.node(c).line))
                .collect();
            out.sort();
            out
        };
        assert_eq!(reached("a"), Vec::<String>::new(), "{:?}", edges(&g));
        assert_eq!(reached("b"), ["isAlive:4"], "{:?}", edges(&g));
        assert_eq!(reached("c"), ["exitCode:3"], "{:?}", edges(&g));
    }

    /// A Scala `def` of no list called with parentheses is its value
    /// applied, through its own class alone (Mill's `scalaVersion()`,
    /// `super.scalacOptions()`); its implicits may be passed explicitly. A
    /// read passed as a function expands a method of any parameters (`xs
    /// .foreach(Seg.check)`, `laws.evalOn _`, `macro Macros.impl`) — but
    /// `_.exitCode` reads its argument's member. An `override def
    /// toString()` is read bare too; Java calls a `def total: Int` as
    /// `total()`.
    #[test]
    fn a_scala_def_of_no_list_is_applied_through_its_class_and_expanded_where_passed() {
        let build = "trait Base {\n  def scalacOptions: Seq[String] = Seq()\n}\ntrait Module extends Base {\n  def scalaVersion: String = \"3\"\n  def opts = if (scalaVersion().startsWith(\"3\")) 1 else 2\n  override def scalacOptions = super.scalacOptions() ++ Seq(\"-x\")\n}\n";
        let model = "class Holder {\n  def get: Int = 1\n  def exitCode(): Int = 0\n  override def toString() = \"h\"\n}\nobject Seg {\n  def check(s: String): Unit = ()\n}\nobject Laws {\n  def evalOn(n: Int): Boolean = true\n}\nobject Async {\n  implicit def forW[F](implicit a: Int, b: Int): Int = a\n}\nobject Macros {\n  def impl(c: Any)(s: Any): Any = s\n}\nclass Cart {\n  def total: Int = 1\n}\n";
        let uses = "class Use {\n  def a(): Int = make().get()\n  def b(xs: Seq[String]): Unit = xs.foreach(Seg.check)\n  def c(): Unit = forAll(Laws.evalOn _)\n  def d(xs: Seq[Holder]) = xs.map(_.exitCode)\n  def e(h: Holder): String = h.toString\n  def f(): Int = Async.forW(1, 2)\n  def g(): Int = make().forW(1, 2)\n  def h(): Int = Async.forW\n  def i(s: String): Int = macro Macros.impl\n}\n";
        let java = "class J {\n  void j(Cart c) { c.total(); }\n}\n";
        let g = graph(&[
            ("/p/build.mill", build),
            ("/p/src/Model.scala", model),
            ("/p/src/Use.scala", uses),
            ("/p/src/J.java", java),
        ]);
        // (The last of a name: `scalacOptions`, the override.)
        let reached = |caller: &str| {
            let at = (0..g.node_count())
                .rfind(|&n| g.node(n).name == caller)
                .unwrap();
            let mut out: Vec<String> = g
                .callees_of(at)
                .iter()
                .map(|&c| format!("{}:{}", g.node(c).name, g.node(c).line))
                .collect();
            out.sort();
            out
        };
        for (caller, expected) in [
            ("opts", &["scalaVersion:5"][..]),
            ("scalacOptions", &["scalacOptions:2"]),
            ("a", &[]),
            ("b", &["check:7"]),
            ("c", &["evalOn:10"]),
            ("d", &[]),
            ("e", &["toString:4"]),
            ("f", &["forW:13"]),
            ("g", &[]),
            ("h", &["forW:13"]),
            ("i", &["impl:16"]),
            ("j", &["total:19"]),
        ] {
            assert_eq!(reached(caller), expected, "{caller}: {:?}", edges(&g));
        }
    }

    /// A class's body is open where its braces say, those in strings,
    /// character literals and comments aside: what follows its `};` is not
    /// its own, however the grammar nested it.
    #[test]
    fn a_class_body_is_open_until_its_brace_closes() {
        for (text, open) in [
            ("{ int a; }", false),
            ("{ void f() { g(); }\n  int x;\n", true),
            ("{ auto s = \"}\"; char c = '}';\n", true),
            ("{ // }\n /* } */ int x;\n", true),
            ("{ };\ninline auto runtime() -> int { return 0; }\n", false),
        ] {
            assert_eq!(still_open(text), open, "{text:?}");
        }
    }

    /// Zig's `self` is the parameter its annotation types: `self:
    /// *NonBlocking` calls `NonBlocking`'s method, `self: *Self` the
    /// container's own. A file's name for a standard-library type (`const
    /// Ast = std.zig.Ast;`) makes its values the library's.
    #[test]
    fn a_zig_self_is_its_annotated_type() {
        let src = "const std = @import(\"std\");\nconst Ast = std.zig.Ast;\npub const NonBlocking = struct {\n    pub fn release(self: *NonBlocking) void {\n        _ = self;\n    }\n    pub fn run(self: *NonBlocking) void {\n        self.release();\n    }\n};\npub const Blocking = struct {\n    const Self = @This();\n    pub fn release(self: *Self) void {\n        _ = self;\n    }\n    pub fn render(self: *Self) void {\n        _ = self;\n    }\n    pub fn go(self: *Self) void {\n        self.release();\n    }\n};\nfn show(tree: Ast) void {\n    tree.render();\n}\n";
        let g = graph(&[("/p/src/pool.zig", src)]);
        let reached = |name: &str| {
            let at = (0..g.node_count())
                .find(|&n| g.node(n).name == name)
                .unwrap_or_else(|| panic!("no {name}: {:?}", edges(&g)));
            let mut lines: Vec<usize> = g.callees_of(at).iter().map(|&c| g.node(c).line).collect();
            lines.sort_unstable();
            lines
        };
        assert_eq!(reached("run"), [4], "{:?}", edges(&g));
        assert_eq!(reached("go"), [13], "{:?}", edges(&g));
        assert_eq!(reached("show"), Vec::<usize>::new(), "{:?}", edges(&g));
    }

    /// A value declared as a type alias calls the aliased type's methods:
    /// C++'s `using` and `typedef`, Kotlin's `typealias`, Scala's `type`,
    /// Go's `type X = Y`.
    #[test]
    fn a_type_alias_reaches_the_aliased_type() {
        let cases: &[(&str, &str, &str, &str)] = &[
            (
                "/p/src/sink.cpp",
                "template <typename M> class daily_sink {\n public:\n  void flush() {}\n};\nclass other {\n public:\n  void flush() {}\n};\nusing daily_sink_st = daily_sink<int>;\ntypedef daily_sink<long> daily_sink_mt;\nvoid run(daily_sink_st &a, daily_sink_mt *b) {\n  a.flush();\n  b->flush();\n}\n",
                "flush",
                "3",
            ),
            (
                "/p/src/Cart.kt",
                "class Cart {\n    fun total(): Int = 0\n}\nclass Order {\n    fun total(): Int = 1\n}\ntypealias Basket = Cart\nfun run(b: Basket) {\n    b.total()\n}\n",
                "total",
                "2",
            ),
            (
                "/p/src/Cart.scala",
                "class Cart {\n  def total(): Int = 0\n}\nclass Order {\n  def total(): Int = 1\n}\nobject Shop {\n  type Basket = Cart\n  def run(b: Basket): Int = b.total()\n}\n",
                "total",
                "2",
            ),
            (
                "/p/src/cart.go",
                "package shop\n\ntype Cart struct{}\n\nfunc (c *Cart) Total() int { return 0 }\n\ntype Order struct{}\n\nfunc (o *Order) Total() int { return 1 }\n\ntype Basket = Cart\n\nfunc run(b *Basket) {\n\tb.Total()\n}\n",
                "Total",
                "5",
            ),
        ];
        for (file, src, method, line) in cases {
            let g = graph(&[(file, src)]);
            let run = (0..g.node_count())
                .find(|&n| g.node(n).name == "run")
                .unwrap();
            let mut reached: Vec<String> = g
                .callees_of(run)
                .iter()
                .filter(|&&c| g.node(c).name == *method)
                .map(|&c| g.node(c).line.to_string())
                .collect();
            reached.sort();
            reached.dedup();
            assert_eq!(reached, [line.to_string()], "{file}: {:?}", edges(&g));
        }
        assert_eq!(
            type_aliases(
                "typedef void (*cb)(int);\nusing namespace std;\nusing Base::Base;\n",
                Lang::Cpp
            ),
            []
        );
        // A plain lower-case C++ type, where a declaration starts and
        // written as one; never a product, a keyword or a built-in type.
        for (head, want) in [
            ("void run(logger &", Some("logger")),
            ("void run(int n, logger& ", Some("logger")),
            ("{\n  const sink *", Some("sink")),
            ("{\n  static registry ", Some("registry")),
            ("{\n  x = y * ", None),
            ("{\n  f(a & ", None),
            ("{\n  f(a*", None),
            ("{\n  return ", None),
            ("{\n  auto ", None),
            ("{\n  unsigned ", None),
            ("{\n  else ", None),
        ] {
            assert_eq!(cpp_trailing_type(head).as_deref(), want, "{head:?}");
        }
        assert_eq!(
            type_aliases(
                "typealias Handler = (Int) -> Unit\ntypealias Box<T> = List<T>\n",
                Lang::Kotlin
            ),
            [("Box".to_string(), "List".to_string())]
        );
    }

    /// `std::make_shared<T>(…)` and `std::make_unique<T>(…)` build a `T`:
    /// its `->` calls reach `T`'s methods, not a same-name method of another
    /// class; a statement in a body that reads as a declarator (`emit
    /// finished();`) is no function for a call to reach.
    #[test]
    fn a_cpp_smart_pointer_factory_builds_its_type() {
        let src = "#include <memory>\nclass logger {\n public:\n  void flush() {}\n};\nclass sink {\n public:\n  void flush() {}\n};\nnamespace ns { class cart { public: void flush() {} }; }\nvoid run() {\n  auto l = std::make_shared<logger>(\"x\", 1);\n  l->flush();\n  auto c = std::make_unique<ns::cart>();\n  c->flush();\n}\nvoid tick() {\n  emit finished();\n}\nvoid done() {\n  finished();\n}\n";
        let g = graph(&[("/p/src/log.cpp", src)]);
        let node = |name: &str| {
            (0..g.node_count())
                .find(|&n| g.node(n).name == name)
                .unwrap_or_else(|| panic!("no {name}: {:?}", edges(&g)))
        };
        let mut reached: Vec<usize> = g
            .callees_of(node("run"))
            .iter()
            .map(|&c| g.node(c).line)
            .collect();
        reached.sort_unstable();
        assert_eq!(reached, [4, 10], "{:?}", edges(&g));
        assert!(
            (0..g.node_count()).all(|n| g.node(n).name != "finished"),
            "{:?}",
            edges(&g)
        );
        // A member call on a value of no known type: a class's `info`, not
        // the free `info` of a namespace; with no member of the name, the
        // free one stays.
        let src = "namespace spdlog {\nvoid info(int m) {}\nvoid warn(int m) {}\nclass logger {\n public:\n  void info(int m) {}\n};\n}\nvoid use(Thing t) {\n  t.get()->info(1);\n  t.get()->warn(2);\n}\nvoid plain() {\n  spdlog::info(3);\n}\n";
        let g = graph(&[("/p/src/use.cpp", src)]);
        let reached = |name: &str| {
            let at = (0..g.node_count())
                .find(|&n| g.node(n).name == name)
                .unwrap();
            let mut reached: Vec<(String, usize)> = g
                .callees_of(at)
                .iter()
                .map(|&c| (g.node(c).name.clone(), g.node(c).line))
                .collect();
            reached.sort();
            reached
        };
        assert_eq!(
            reached("use"),
            [("info".to_string(), 6), ("warn".to_string(), 3)],
            "{:?}",
            edges(&g)
        );
        assert_eq!(
            reached("plain"),
            [("info".to_string(), 2)],
            "{:?}",
            edges(&g)
        );

        // `fmt::format(…)` in a file whose own `format`s are a formatter's
        // members: the namespace's function in the header it includes.
        let header = "namespace fmt {\ntemplate <typename... T>\nauto format(const char* f, T&&... args) -> int { return 0; }\n}\n";
        let test = "#include \"fmt/format.h\"\ntemplate <> struct formatter<point> {\n  auto format(point p, int& ctx) const -> int { return 0; }\n};\nvoid check() {\n  fmt::format(\"{}\", 1);\n}\n";
        let files = [
            ("/p/include/fmt/format.h", header),
            ("/p/test/format-test.cc", test),
        ];
        let sources: Vec<(PathBuf, String)> = files
            .iter()
            .map(|(f, s)| (PathBuf::from(f), s.to_string()))
            .collect();
        let scope: HashMap<PathBuf, HashSet<PathBuf>> = HashMap::from([(
            PathBuf::from("/p/test/format-test.cc"),
            HashSet::from([PathBuf::from("/p/include/fmt/format.h")]),
        )]);
        let g = ProjectCallGraph::build(defs_of(&files), &sources, &scope);
        let check = (0..g.node_count())
            .find(|&n| g.node(n).name == "check")
            .unwrap();
        let reached: Vec<String> = g
            .callees_of(check)
            .iter()
            .map(|&c| format!("{}:{}", g.node(c).file.display(), g.node(c).line))
            .collect();
        assert_eq!(reached, ["/p/include/fmt/format.h:3"], "{:?}", edges(&g));
        assert_eq!(
            cpp_made_type("std::make_shared<logger>(\"x\")").as_deref(),
            Some("logger")
        );
        assert_eq!(
            cpp_made_type("make_unique<ns::Box<int>>()").as_deref(),
            Some("Box")
        );
        assert_eq!(cpp_made_type("std::make_shared<logger>(a)->clone()"), None);
        assert_eq!(cpp_made_type("std::make_pair(a, b)"), None);
    }

    #[test]
    fn a_value_from_a_call_has_its_declared_return_type() {
        let cases: &[(&str, &str, &str, &str)] = &[
            (
                "/p/src/Moshi.kt",
                "class JsonAdapter<T> {\n    fun fromJson(s: String): T? = null\n}\nclass Other {\n    fun fromJson(s: String): Int = 0\n}\nclass Moshi {\n    fun <T> adapter(): JsonAdapter<T> = JsonAdapter()\n}\nfun run(moshi: Moshi) {\n    val adapter = moshi.adapter<Int>()\n    adapter.fromJson(\"1\")\n}\n",
                "fromJson",
                "2",
            ),
            (
                "/p/src/Shop.java",
                "class Cart { int total() { return 0; } }\nclass Order { int total() { return 1; } }\nclass Shop {\n  static Cart cart() { return new Cart(); }\n  void run() {\n    var c = cart();\n    c.total();\n  }\n}\n",
                "total",
                "1",
            ),
            (
                "/p/src/Shop.cs",
                "class Cart { public int Total() => 0; }\nclass Order { public int Total() => 1; }\nclass Shop\n{\n    static Cart MakeCart() => new Cart();\n    void Run()\n    {\n        var c = MakeCart();\n        c.Total();\n    }\n}\n",
                "Total",
                "1",
            ),
            (
                "/p/src/log.cpp",
                "#include <memory>\nclass logger {\n public:\n  void set_pattern(int p) {}\n};\nclass formatter {\n public:\n  void set_pattern(int p) {}\n};\nstd::shared_ptr<logger> stdout_mt(int n) { return nullptr; }\nvoid run() {\n  auto l = stdout_mt(1);\n  l->set_pattern(2);\n}\n",
                "set_pattern",
                "4",
            ),
            (
                "/p/src/shop.go",
                "package shop\n\ntype Cart struct{}\n\nfunc (c *Cart) Total() int { return 0 }\n\ntype Order struct{}\n\nfunc (o *Order) Total() int { return 1 }\n\nfunc NewCart() (*Cart, error) { return &Cart{}, nil }\n\nfunc run() {\n\tc, _ := NewCart()\n\tc.Total()\n\tk := NewCart2()\n\tk.Total()\n}\n\nfunc NewCart2() *Cart { return &Cart{} }\n",
                "Total",
                "5",
            ),
            (
                "/p/lib/shop.dart",
                "class Cart {\n  int total() => 0;\n}\nclass Order {\n  int total() => 1;\n}\nCart makeCart() => Cart();\nvoid run() {\n  final c = makeCart();\n  c.total();\n}\n",
                "total",
                "2",
            ),
        ];
        for (file, src, method, line) in cases {
            let g = graph(&[(file, src)]);
            let run = (0..g.node_count())
                .find(|&n| g.node(n).name.eq_ignore_ascii_case("run"))
                .unwrap();
            let mut reached: Vec<String> = g
                .callees_of(run)
                .iter()
                .filter(|&&c| g.node(c).name == *method)
                .map(|&c| g.node(c).line.to_string())
                .collect();
            reached.sort();
            reached.dedup();
            assert_eq!(reached, [line.to_string()], "{file}: {:?}", edges(&g));
        }
        // Awaited, a future's value; a Zig type through its module; a C#
        // property named in Pascal case.
        let more: &[(&str, &str, &str, &str)] = &[
            (
                "/p/lib/fetch.dart",
                "class Cart {\n  int total() => 0;\n}\nclass Order {\n  int total() => 1;\n}\nFuture<Cart> fetchCart() async => Cart();\nvoid run() async {\n  var c = await fetchCart();\n  c.total();\n}\n",
                "total",
                "2",
            ),
            (
                "/p/src/basic.zig",
                "pub const Response = struct {\n    pub fn writer(self: *Response) u8 { return 0; }\n};\npub const Other = struct {\n    pub fn writer(self: *Other) u8 { return 1; }\n};\nfn run(res: *httpz.Response) void {\n    _ = res.writer();\n}\n",
                "writer",
                "2",
            ),
            (
                "/p/src/Setup.cs",
                "class Resetter { public void DisposeAsync() {} }\nclass Other { public void DisposeAsync() {} }\nclass Setup\n{\n    internal static Resetter? DbResetter { get; private set; }\n    public void Run()\n    {\n        DbResetter.DisposeAsync();\n    }\n}\n",
                "DisposeAsync",
                "1",
            ),
        ];
        for (file, src, method, line) in more {
            let g = graph(&[(file, src)]);
            let run = (0..g.node_count())
                .find(|&n| g.node(n).name.eq_ignore_ascii_case("run"))
                .unwrap();
            let mut reached: Vec<String> = g
                .callees_of(run)
                .iter()
                .filter(|&&c| g.node(c).name == *method)
                .map(|&c| g.node(c).line.to_string())
                .collect();
            reached.sort();
            reached.dedup();
            assert_eq!(reached, [line.to_string()], "{file}: {:?}", edges(&g));
        }
        // Two methods of the name declaring different types: the receiver's
        // type's says which (`a.make()` with `a: A` is a `Cart`); a receiver
        // of no known type leaves it unknown — every `total` there is.
        for (receiver, want) in [("A a", 1), ("Thing a", 2)] {
            let src = format!(
                "class Cart {{ int total() {{ return 0; }} }}\nclass Order {{ int total() {{ return 1; }} }}\nclass A {{ Cart make() {{ return null; }} }}\nclass B {{ Order make() {{ return null; }} }}\nclass Shop {{\n  void run({receiver}) {{\n    var c = a.make();\n    c.total();\n  }}\n}}\n"
            );
            let g = graph(&[("/p/src/Two.java", &src)]);
            let run = g.id_of(Path::new("/p/src/Two.java"), "run").unwrap();
            let totals = g
                .callees_of(run)
                .iter()
                .filter(|&&c| g.node(c).name == "total")
                .count();
            assert_eq!(totals, want, "{receiver}: {:?}", edges(&g));
        }
    }

    /// A definition whose name's last part is empty (`Foo.`) has no
    /// parameter list to find — and finding none ends.
    #[test]
    fn an_empty_name_has_no_parameter_list() {
        assert!(parameter_list("fun Foo.(x: Int) = x", "Foo.", Lang::Kotlin).is_none());
    }

    /// A value declared as an interface calls the interface's method (or an
    /// implementation's), never a same-named method of the class around the
    /// call; and a call on a declared type reaches only the overloads of its
    /// argument count.
    #[test]
    fn a_declared_interface_and_its_overloads_are_reached_exactly() {
        let g = graph(&[
            (
                "/p/src/main/java/p/VetRepository.java",
                "package p;\n\npublic interface VetRepository {\n  Object findAll();\n}\n",
            ),
            (
                "/p/src/test/java/p/PostgresIntegrationTests.java",
                "package p;\n\nclass PostgresIntegrationTests {\n  private VetRepository vets;\n\n  void findAll() {\n    vets.findAll();\n  }\n}\n",
            ),
            (
                "/p/src/main/java/p/JsonAdapter.java",
                "package p;\n\nabstract class JsonAdapter {\n  String toJson(Object value) { return null; }\n  abstract void toJson(Object writer, Object value);\n}\n",
            ),
            (
                "/p/src/main/java/p/Use.java",
                "package p;\n\nclass Use {\n  void run(JsonAdapter adapter) {\n    adapter.toJson(1);\n  }\n}\n",
            ),
        ]);
        let e = edges(&g);
        let test = "/p/src/test/java/p/PostgresIntegrationTests.java:findAll";
        assert!(
            has(&g, test, "/p/src/main/java/p/VetRepository.java:findAll"),
            "{e:?}"
        );
        let to_itself = g
            .callees_of(
                g.id_of(
                    Path::new("/p/src/test/java/p/PostgresIntegrationTests.java"),
                    "findAll",
                )
                .unwrap(),
            )
            .iter()
            .any(|&c| g.node(c).file.ends_with("PostgresIntegrationTests.java"));
        assert!(!to_itself, "{e:?}");
        let run = g
            .id_of(Path::new("/p/src/main/java/p/Use.java"), "run")
            .unwrap();
        let lines: Vec<usize> = g.callees_of(run).iter().map(|&c| g.node(c).line).collect();
        assert_eq!(lines, [4], "{e:?}");
    }

    /// An Elixir file's `use`s and `import`s, the multi-alias form too.
    #[test]
    fn elixir_imports_are_read_in_every_form() {
        let src = "defmodule Shop.Case do\n  use ExUnit.CaseTemplate\n  quote do\n    import Ecto.Query, only: [from: 2]\n    import Shop.{Factory, TestHelpers}\n    import Shop.Web.{\n      Auth,\n      Forms\n    }\n    import unquote(__MODULE__)\n  end\nend\n";
        assert_eq!(elixir_modules_after(src, "use "), ["ExUnit.CaseTemplate"]);
        assert_eq!(
            elixir_modules_after(src, "import "),
            [
                "Ecto.Query",
                "Shop.Factory",
                "Shop.TestHelpers",
                "Shop.Web.Auth",
                "Shop.Web.Forms"
            ]
        );
    }

    /// An Elixir bare call reaches its own module, the modules it imports,
    /// and what a `use`d module's `__using__` injects (its quoted imports,
    /// its own `def`s, and through the modules it uses in turn) — never
    /// another module's function that only shares the name.
    #[test]
    fn an_elixir_bare_call_reaches_only_what_is_imported() {
        let g = graph(&[
            (
                "/p/lib/invites.ex",
                "defmodule Shop.Invites do\n  def insert(org, user), do: {org, user}\n  def get(id), do: id\nend\n",
            ),
            (
                "/p/test/support/factory.ex",
                "defmodule Shop.Factory do\n  def insert(kind), do: kind\nend\n",
            ),
            (
                "/p/test/support/helpers.ex",
                "defmodule Shop.Helpers do\n  def login(conn), do: conn\nend\n",
            ),
            (
                "/p/test/support/data_case.ex",
                "defmodule Shop.DataCase do\n  defmacro __using__(_) do\n    quote do\n      import Shop.Factory\n      def fixture, do: 1\n    end\n  end\nend\n",
            ),
            (
                "/p/test/support/conn_case.ex",
                "defmodule Shop.ConnCase do\n  defmacro __using__(_) do\n    quote do\n      use Shop.DataCase\n      import Shop.{\n        Helpers,\n        Missing\n      }\n    end\n  end\nend\n",
            ),
            (
                "/p/test/invite_test.exs",
                "defmodule Shop.InviteTest do\n  use Shop.ConnCase\n  test \"invites\" do\n    insert(:user)\n    login(conn)\n    fixture()\n    get(1)\n  end\nend\n",
            ),
        ]);
        let edges = edges(&g);
        let from = |to: &str| {
            edges
                .iter()
                .any(|(a, b)| a.starts_with("/p/test/invite_test.exs:") && b == to)
        };
        assert!(from("/p/test/support/factory.ex:insert"), "{edges:?}");
        assert!(from("/p/test/support/helpers.ex:login"), "{edges:?}");
        assert!(from("/p/test/support/data_case.ex:fixture"), "{edges:?}");
        assert!(!from("/p/lib/invites.ex:insert"), "{edges:?}");
        assert!(!from("/p/lib/invites.ex:get"), "{edges:?}");
    }

    /// An extension method is called on a value of the type it extends,
    /// a library's too: C#'s `this` parameter's, Kotlin's `fun Type.name`.
    #[test]
    fn extension_methods_are_called_on_library_types() {
        let g = graph(&[
            (
                "/p/Ext.cs",
                "namespace Shop\n{\n  public static class ServiceExtensions\n  {\n    public static IServiceCollection AddShop(this IServiceCollection services) { return services; }\n  }\n}\n",
            ),
            (
                "/p/Startup.cs",
                "namespace Shop\n{\n  public class Startup\n  {\n    public void Configure(IServiceCollection services) { services.AddShop(); }\n  }\n}\n",
            ),
            (
                "/p/text.kt",
                "package shop\n\nfun String.shout(): String = uppercase()\n",
            ),
            (
                "/p/use.kt",
                "package shop\n\nfun greet(name: String): String = name.shout()\n",
            ),
        ]);
        assert!(
            has(&g, "/p/Startup.cs:Configure", "/p/Ext.cs:AddShop"),
            "{:?}",
            edges(&g)
        );
        assert!(has(&g, "/p/use.kt:greet", "/p/text.kt:shout"));
    }

    /// Kotlin's files of one package, wherever they are filed, and a
    /// constructor's property, typed: the class's method. C#'s namespace
    /// and the namespaces around it, with no `using`. A Ruby class's
    /// mixed-in module's method, called bare.
    #[test]
    fn kotlin_csharp_and_ruby_reach_their_packages_and_mixins() {
        let g = graph(&[
            (
                "/p/a/Repo.kt",
                "package shop\n\nclass Repo {\n  fun load(): Int = 1\n}\n",
            ),
            (
                "/p/b/Service.kt",
                "package shop\n\nclass Service(private val repo: Repo) {\n  fun run() = repo.load()\n}\n",
            ),
            (
                "/p/Core/Clock.cs",
                "namespace Shop\n{\n  public class Clock\n  {\n    public int Now() { return 0; }\n  }\n}\n",
            ),
            (
                "/p/Web/Home.cs",
                "namespace Shop.Web\n{\n  public class Home\n  {\n    private readonly Clock clock;\n    public int Index() { return clock.Now(); }\n  }\n}\n",
            ),
            (
                "/p/greeting.rb",
                "module Greeting\n  def greet\n    'hi'\n  end\nend\n",
            ),
            (
                "/p/shop.rb",
                "class Shop\n  include Greeting\n\n  def run\n    greet\n  end\nend\n",
            ),
        ]);
        assert!(
            has(&g, "/p/b/Service.kt:run", "/p/a/Repo.kt:load"),
            "{:?}",
            edges(&g)
        );
        assert!(has(&g, "/p/Web/Home.cs:Index", "/p/Core/Clock.cs:Now"));
        assert!(
            has(&g, "/p/shop.rb:run", "/p/greeting.rb:greet"),
            "{:?}",
            edges(&g)
        );
    }

    /// Calls inside unnamed functions belong to the nearest function the
    /// outline has. They used to be attributed to the binding's name — a
    /// class field, `a.b`, `Foo.prototype.bar`, a computed key — and dropped,
    /// since no node carried that name.
    #[test]
    fn calls_in_anonymous_functions_reach_a_real_node() {
        let js = "\
function helper() {}
class Widget {
  onClick = () => { helper(); };
}
const api = {};
api.load = function () { helper(); };
function Legacy() {}
Legacy.prototype.render = function () { helper(); };
function build() {
  return { [KEY]: () => helper() };
}
";
        let g = graph(&[("/p/w.js", js)]);
        for from in ["onClick", "load", "render", "build"] {
            assert!(
                has(&g, &format!("/p/w.js:{from}"), "/p/w.js:helper"),
                "{from} -> helper missing: {:?}",
                edges(&g)
            );
        }

        let ts = "\
function helper(): void {}
export class Widget {
  onClick = (): void => { helper(); };
}
export const handlers = {
  save: () => helper(),
};
function build() {
  return { [KEY]: () => helper() };
}
";
        let g = graph(&[("/p/w.ts", ts)]);
        for from in ["onClick", "save", "build"] {
            assert!(
                has(&g, &format!("/p/w.ts:{from}"), "/p/w.ts:helper"),
                "{from} -> helper missing: {:?}",
                edges(&g)
            );
        }
    }

    /// C: calls resolve to the DEFINITION, never to a header prototype, and
    /// across files by the unique-name rule — a GNU-style definition (its
    /// type on the line above its name) too. (A `static` one is its file's
    /// alone: see `a_c_or_cpp_function_declared_in_a_header_is_reached_through_it`.)
    #[test]
    fn c_calls_link_definitions_not_prototypes() {
        let header = "int helper(void);\nint unused_decl(int);\n";
        let lib = "int\nhelper(void)\n{\n  return 1;\n}\n";
        let main = "#include \"util.h\"\nint main(void) {\n  return helper() + local();\n}\nint local(void) { return 0; }\n";
        let g = graph(&[
            ("/p/util.h", header),
            ("/p/util.c", lib),
            ("/p/main.c", main),
        ]);
        assert!(
            has(&g, "/p/main.c:main", "/p/util.c:helper"),
            "{:?}",
            edges(&g)
        );
        assert!(
            has(&g, "/p/main.c:main", "/p/main.c:local"),
            "{:?}",
            edges(&g)
        );
        assert!(
            !has(&g, "/p/main.c:main", "/p/util.h:helper"),
            "a call landed on the prototype: {:?}",
            edges(&g)
        );
    }

    /// C and C++ link a program's files into one: a header declares what
    /// another file defines (`int add(int, int);` in `class Cart`, defined
    /// as `int Cart::add` in `cart.cpp`), and a file that includes it calls
    /// the definition — the include brings the declaration only. `c.add(…)`
    /// in `main.cpp` read "No callers" on `Cart::add`. A declaration in
    /// scope (the header included, or the file's own prototype) reaches the
    /// definitions of its name with its owner: `Cart`'s `add`, not
    /// `Order`'s, which no header in scope declares.
    #[test]
    fn a_c_or_cpp_function_declared_in_a_header_is_reached_through_it() {
        let header = "#pragma once\n\nclass Cart {\npublic:\n    int add(int a, int b);\n    static Cart make();\n};\n";
        let source = "#include \"cart.hpp\"\n\nint Cart::add(int a, int b) { return a + b; }\n\nCart Cart::make() { return Cart(); }\n";
        let order_h = "#pragma once\nstruct Order { int add(int a); };\n";
        let order = "#include \"order.hpp\"\nint Order::add(int a) { return a; }\n";
        let main = "#include <cstdio>\n#include \"cart.hpp\"\n\nint total(Cart &c) { return c.add(1, 2) + c.add(3, 4); }\n\nint main() {\n    Cart c = Cart::make();\n    std::printf(\"%d\\n\", total(c) + c.add(5, 6));\n    return 0;\n}\n";
        // Through a header that includes the class's.
        let shop_h = "#pragma once\n#include \"cart.hpp\"\n";
        let app = "#include \"shop.hpp\"\nint run_shop(Cart &c) { return c.add(7, 8); }\n";
        let c_h = "int twice(int x);\n";
        let c_lib = "#include \"calc.h\"\nint twice(int x) { return x * 2; }\n";
        let c_main = "#include \"calc.h\"\nint run(void) { return twice(2); }\n";
        // In C, the other program's `twice` is no header's in scope.
        let other = "static int twice(int x) { return x; }\nint other(void) { return twice(1); }\n";
        let files: &[(&str, &str)] = &[
            ("/p/cart.hpp", header),
            ("/p/cart.cpp", source),
            ("/p/order.hpp", order_h),
            ("/p/order.cpp", order),
            ("/p/main.cpp", main),
            ("/p/shop.hpp", shop_h),
            ("/p/app.cpp", app),
            ("/p/calc.h", c_h),
            ("/p/calc.c", c_lib),
            ("/p/run.c", c_main),
            ("/p/tool/other.c", other),
        ];
        let sources: Vec<(PathBuf, String)> = files
            .iter()
            .map(|(f, s)| (PathBuf::from(f), s.to_string()))
            .collect();
        let includes = |from: &str, to: &[&str]| {
            (
                PathBuf::from(from),
                to.iter().map(PathBuf::from).collect::<HashSet<PathBuf>>(),
            )
        };
        let scope: HashMap<PathBuf, HashSet<PathBuf>> = [
            includes("/p/cart.cpp", &["/p/cart.hpp"]),
            includes("/p/order.cpp", &["/p/order.hpp"]),
            includes("/p/main.cpp", &["/p/cart.hpp"]),
            includes("/p/shop.hpp", &["/p/cart.hpp"]),
            includes("/p/app.cpp", &["/p/shop.hpp"]),
            includes("/p/calc.c", &["/p/calc.h"]),
            includes("/p/run.c", &["/p/calc.h"]),
        ]
        .into_iter()
        .collect();
        let g = ProjectCallGraph::build(defs_of(files), &sources, &scope);
        for (from, to) in [
            ("/p/main.cpp:total", "/p/cart.cpp:add"),
            ("/p/main.cpp:main", "/p/cart.cpp:add"),
            ("/p/main.cpp:main", "/p/cart.cpp:make"),
            ("/p/main.cpp:main", "/p/main.cpp:total"),
            ("/p/app.cpp:run_shop", "/p/cart.cpp:add"),
            ("/p/run.c:run", "/p/calc.c:twice"),
            ("/p/tool/other.c:other", "/p/tool/other.c:twice"),
        ] {
            assert!(has(&g, from, to), "{from} -> {to}: {:?}", edges(&g));
        }
        for (from, to) in [
            ("/p/main.cpp:total", "/p/order.cpp:add"),
            ("/p/main.cpp:main", "/p/order.cpp:add"),
            ("/p/main.cpp:total", "/p/cart.hpp:add"),
            ("/p/run.c:run", "/p/tool/other.c:twice"),
        ] {
            assert!(!has(&g, from, to), "not {from} -> {to}: {:?}", edges(&g));
        }
    }

    #[test]
    fn cpp_calls_through_qualified_template_and_member_forms() {
        let src = "\
namespace util { template <typename T> T twice(T x) { return x; } }
void helper() {}
struct Box { void open(); };
void Box::open() {
  util::twice<int>(1);
  helper();
  std::move(1);
}
void run() {
  Box b;
  b.open();
  Box *p = new Box();
}
void move(int) {}
";
        let g = graph(&[("/p/box.cpp", src)]);
        assert!(
            has(&g, "/p/box.cpp:open", "/p/box.cpp:twice"),
            "{:?}",
            edges(&g)
        );
        assert!(
            has(&g, "/p/box.cpp:open", "/p/box.cpp:helper"),
            "{:?}",
            edges(&g)
        );
        assert!(
            has(&g, "/p/box.cpp:run", "/p/box.cpp:open"),
            "{:?}",
            edges(&g)
        );
        assert!(
            !has(&g, "/p/box.cpp:open", "/p/box.cpp:move"),
            "std::move is not the project's move: {:?}",
            edges(&g)
        );
    }

    #[test]
    fn a_cpp_destructor_s_calls_are_not_the_constructor_s() {
        let src = "\
void cleanup() {}
void setup() {}
struct Foo {
  Foo() { setup(); }
  ~Foo() { cleanup(); }
};
";
        let g = graph(&[("/p/foo.cpp", src)]);
        assert!(
            has(&g, "/p/foo.cpp:Foo", "/p/foo.cpp:setup"),
            "{:?}",
            edges(&g)
        );
        assert!(
            !has(&g, "/p/foo.cpp:Foo", "/p/foo.cpp:cleanup"),
            "{:?}",
            edges(&g)
        );
    }

    #[test]
    fn java_methods_and_constructors_are_linked() {
        let src = "\
class Account {
  Account() { init(); }
  void init() {}
  void transfer(Account other) {
    audit();
    other.init();
    Account copy = new Account();
  }
  static void audit() {}
}
";
        let g = graph(&[("/p/Account.java", src)]);
        assert!(
            has(&g, "/p/Account.java:Account", "/p/Account.java:init"),
            "{:?}",
            edges(&g)
        );
        assert!(
            has(&g, "/p/Account.java:transfer", "/p/Account.java:audit"),
            "{:?}",
            edges(&g)
        );
        assert!(
            has(&g, "/p/Account.java:transfer", "/p/Account.java:init"),
            "{:?}",
            edges(&g)
        );
        assert!(
            has(&g, "/p/Account.java:transfer", "/p/Account.java:Account"),
            "`new Account()` reaches the constructor: {:?}",
            edges(&g)
        );
    }

    /// PHP: functions, methods through `->`, `?->` and `::`, `new`, and a
    /// closure's calls as its method's. A call through a variable and a
    /// `new static` name nothing, and a bare `count()` is PHP's own even
    /// beside a class's `count` method, which only `$this->` reaches.
    #[test]
    fn php_functions_methods_and_constructors_are_linked() {
        let file = "/p/Cart.php";
        let src = "\
<?php
function helper($x) { return $x; }
class Cart {
  public function __construct() { $this->reset(); }
  public function reset() {}
  public function count() { return 0; }
  public function total($o) {
    helper(1);
    $o?->reset();
    self::audit();
    $copy = new Cart();
    $same = new static();
    $f = 'helper';
    $f(2);
    $g = function () { audit_log(); };
    return count([]);
  }
  public static function audit() {}
}
function audit_log() {}
";
        let g = graph(&[(file, src)]);
        for (from, to) in [
            ("__construct", "reset"),
            ("total", "helper"),
            ("total", "reset"),
            ("total", "audit"),
            ("total", "audit_log"),
        ] {
            assert!(
                has(&g, &format!("{file}:{from}"), &format!("{file}:{to}")),
                "{from} -> {to}: {:?}",
                edges(&g)
            );
        }
        assert!(
            !has(&g, &format!("{file}:total"), &format!("{file}:count")),
            "a bare count() is PHP's: {:?}",
            edges(&g)
        );
        let calls = calls_of(src, "php");
        let call = |callee: &str| calls.iter().find(|c| c.callee == callee);
        assert!(call("reset").is_some_and(|c| c.method), "{calls:?}");
        assert!(call("audit").is_some_and(|c| c.method), "{calls:?}");
        assert!(call("helper").is_some_and(|c| !c.method), "{calls:?}");
        assert!(
            call("Cart").is_some_and(|c| !c.method),
            "new Cart: {calls:?}"
        );
        assert!(call("f").is_none(), "a variable call: {calls:?}");
        assert!(call("static").is_none(), "new static: {calls:?}");
    }

    /// Ruby: a call with a receiver is a method call, one without is on
    /// `self`; `valid?` keeps its `?`, and `self.total = 0` calls the
    /// setter. A bare name standing as a statement (`setup`) is a call of
    /// the method — unless the method binds it first (`sum = 0 … sum`, a
    /// parameter, a block's), when it is the local's value; and a bare
    /// `puts` is `Kernel`'s, not a project `puts` in another class.
    #[test]
    fn ruby_calls_with_and_without_receivers_are_linked() {
        let file = "/p/account.rb";
        let account = "\
class Account
  def initialize
    setup
    self.total = 0
  end
  def setup; end
  def total; 0; end
  def total=(v); end
  def valid?; true; end
  def deposit(x)
    audit(x)
    return unless valid?
    [x].each { |i| audit(i) }
    puts 'deposited'
  end
  def sum; end
  def balance(x)
    sum = 0
    x
    sum
  end
  def self.open
    audit(1)
  end
  def audit(x); end
end
";
        let shell = "class Shell\n  def puts(msg); end\nend\n";
        let g = graph(&[(file, account), ("/p/shell.rb", shell)]);
        for (from, to) in [
            ("initialize", "total="),
            ("initialize", "setup"),
            ("deposit", "audit"),
            ("deposit", "valid?"),
            ("open", "audit"),
        ] {
            assert!(
                has(&g, &format!("{file}:{from}"), &format!("{file}:{to}")),
                "{from} -> {to}: {:?}",
                edges(&g)
            );
        }
        for (from, to) in [
            ("/p/account.rb:initialize", "/p/account.rb:total"),
            ("/p/account.rb:balance", "/p/account.rb:sum"),
            ("/p/account.rb:deposit", "/p/shell.rb:puts"),
        ] {
            assert!(!has(&g, from, to), "{from} -> {to}: {:?}", edges(&g));
        }
        let calls = calls_of(account, "ruby");
        assert!(
            calls.iter().any(|c| c.callee == "total=" && c.method),
            "{calls:?}"
        );
        assert!(
            calls.iter().any(|c| c.callee == "audit" && !c.method),
            "{calls:?}"
        );
    }

    /// C#: methods, constructors through `new`, generic and `?.` calls, and
    /// local functions as scopes of their own; a lambda's calls are its
    /// method's. `nameof(x)` is no call.
    #[test]
    fn csharp_methods_constructors_and_local_functions_are_linked() {
        let file = "/p/Account.cs";
        let src = "\
class Account {
  Account() { Init(); }
  void Init() {}
  void Transfer(Account other) {
    Audit();
    other?.Init();
    var copy = new Account();
    Make<int>();
    var name = nameof(Audit);
    int Local(int v) { return Helper(v); }
    Local(1);
    System.Func<int> f = () => Helper(2);
  }
  static void Audit() {}
  static T Make<T>() => default;
  static int Helper(int v) => v;
}
";
        let g = graph(&[(file, src)]);
        for (from, to) in [
            ("Account", "Init"),
            ("Transfer", "Audit"),
            ("Transfer", "Init"),
            ("Transfer", "Account"),
            ("Transfer", "Make"),
            ("Transfer", "Local"),
            ("Transfer", "Helper"),
            ("Local", "Helper"),
        ] {
            assert!(
                has(&g, &format!("{file}:{from}"), &format!("{file}:{to}")),
                "{from} -> {to}: {:?}",
                edges(&g)
            );
        }
        let calls = calls_of(src, "csharp");
        assert!(
            !calls.iter().any(|c| c.callee == "nameof"),
            "nameof is an operator: {calls:?}"
        );
        assert!(
            calls.iter().any(|c| c.callee == "Init" && c.method),
            "other?.Init(): {calls:?}"
        );
        assert!(
            calls.iter().any(|c| c.callee == "Make" && !c.method),
            "{calls:?}"
        );
    }

    /// Erlang: local and remote calls, a `fun`'s calls as its function's,
    /// each clause attributed to its own arity's function; a fun in a
    /// variable names nothing, and a bare `size(X)` is the BIF, not
    /// another module's `size`.
    #[test]
    fn erlang_local_and_remote_calls_are_linked_per_function() {
        let file = "/p/shop.erl";
        let src = "\
-module(shop).
-export([run/1]).
run([]) -> helper(0);
run(X) ->
    lists:map(fun(Y) -> inner(Y) end, X),
    shop:helper(X),
    F = fun helper/1,
    F(2),
    size(X).
helper(X) -> X.
inner(X) -> X.
price(X) -> price(X, 1).
price(X, Y) -> tax(X * Y).
tax(X) -> X.
";
        let util = "-module(util).\nsize(_) -> 0.\n";
        let g = graph(&[(file, src), ("/p/util.erl", util)]);
        for (from, to) in [("run", "helper"), ("run", "inner")] {
            assert!(
                has(&g, &format!("{file}:{from}"), &format!("{file}:{to}")),
                "{from} -> {to}: {:?}",
                edges(&g)
            );
        }
        assert!(
            !has(&g, &format!("{file}:run"), "/p/util.erl:size"),
            "size(X) is the BIF: {:?}",
            edges(&g)
        );
        // `price/1` calls `price/2`, which calls `tax`: each clause is its
        // own arity's.
        let key = |name: &str, ordinal: u32| (PathBuf::from(file), name.to_string(), ordinal);
        let keys = g.callee_keys();
        assert_eq!(keys[&key("price", 0)], vec![key("price", 1)], "{keys:?}");
        assert_eq!(keys[&key("price", 1)], vec![key("tax", 0)], "{keys:?}");

        let mut calls = calls_of(src, "erlang");
        calls.sort_by_key(|c| c.line);
        // `helper(0)`, then `shop:helper(X)`, then `fun helper/1` (a use:
        // what the fun is called with calls it).
        let method = |callee: &str| -> Vec<bool> {
            calls
                .iter()
                .filter(|c| c.callee == callee)
                .map(|c| c.method)
                .collect()
        };
        assert_eq!(method("helper"), [false, true, false]);
        assert_eq!(method("map"), [true]);
        assert!(!calls.iter().any(|c| c.callee == "F"), "{calls:?}");
    }

    /// Haskell: an application is a call of its head, once however many
    /// arguments it takes; a function passed as an argument (`map price`)
    /// is used there, as `map` calls it; a constructor (`Just`) or a type
    /// (`IO ()`) is no call,
    /// and a `where`-bound helper's calls are its function's. `main = do …`
    /// is a scope. A bare `lookup` is the Prelude's, not another module's.
    #[test]
    fn haskell_applications_link_by_their_head() {
        let file = "/p/Shop.hs";
        let src = "\
module Shop where
import qualified Data.Map as Map

total :: [Int] -> Int
total xs = sum (map price xs) + helper 1 2
  where
    go y = inner y

price :: Int -> Int
price x = Map.findWithDefault 0 x table + lookup x

wrap :: Int -> Maybe Int
wrap x = Just (inner x)

helper :: Int -> Int -> Int
helper a b = a + b

inner :: Int -> Int
inner = id

main :: IO ()
main = do
  print (total [1])
  let r = helper 1 2
  pure ()

table = Map.fromList []
";
        let store = "module Store where\n\nlookup :: Int -> Int\nlookup k = k\n";
        let g = graph(&[(file, src), ("/p/Store.hs", store)]);
        for (from, to) in [
            ("total", "helper"),
            ("total", "inner"),
            ("wrap", "inner"),
            ("main", "total"),
            ("main", "helper"),
            ("total", "price"),
        ] {
            assert!(
                has(&g, &format!("{file}:{from}"), &format!("{file}:{to}")),
                "{from} -> {to}: {:?}",
                edges(&g)
            );
        }
        assert!(
            !has(&g, "/p/Shop.hs:price", "/p/Store.hs:lookup"),
            "price -> lookup: {:?}",
            edges(&g)
        );
        let calls = calls_of(src, "haskell");
        assert_eq!(
            calls
                .iter()
                .filter(|c| c.callee == "helper" && c.caller.as_deref() == Some("total"))
                .count(),
            1,
            "{calls:?}"
        );
        assert!(
            !calls
                .iter()
                .any(|c| matches!(c.callee.as_str(), "Just" | "IO" | "Maybe")),
            "{calls:?}"
        );
        assert!(
            calls
                .iter()
                .any(|c| c.callee == "findWithDefault" && c.method),
            "{calls:?}"
        );
    }

    /// Zig: functions, and methods through a field; `@import` and `@as` are
    /// builtins, not calls.
    #[test]
    fn zig_functions_and_methods_are_linked_and_builtins_are_not_calls() {
        let file = "/p/main.zig";
        let src = "\
const std = @import(\"std\");
const Account = struct {
    fn deposit(self: *Account) void {
        self.audit();
    }
    fn audit(self: *Account) void {
        _ = self;
    }
};
fn helper() void {}
pub fn main() void {
    helper();
    var a = Account{};
    a.deposit();
    std.debug.print(\"x\", .{});
    const n = @as(u8, 2);
    _ = n;
}
";
        let g = graph(&[(file, src)]);
        for (from, to) in [
            ("main", "helper"),
            ("main", "deposit"),
            ("deposit", "audit"),
        ] {
            assert!(
                has(&g, &format!("{file}:{from}"), &format!("{file}:{to}")),
                "{from} -> {to}: {:?}",
                edges(&g)
            );
        }
        let calls = calls_of(src, "zig");
        assert!(
            !calls
                .iter()
                .any(|c| matches!(c.callee.as_str(), "import" | "as")),
            "{calls:?}"
        );
        assert!(
            calls.iter().any(|c| c.callee == "print" && c.method),
            "{calls:?}"
        );
        assert!(
            calls.iter().any(|c| c.callee == "helper" && !c.method),
            "{calls:?}"
        );
    }

    /// A Rust path call is its type's function: `Self::make()` the impl's
    /// own, `B::make()` B's — even where B's is in another file and this one
    /// has a `make` of its own — not every `make` there is. A call on a
    /// type the project defines nothing of (`Vec::new()`, `v.len()` on a
    /// `Vec`) is none of the project's functions.
    #[test]
    fn rust_path_calls_go_to_their_types_function() {
        let lib = "\
struct A;
struct Order;
impl A {
    fn make() -> A { A }
    fn build() -> A { Self::make() }
    fn other() { let _ = b::B::make(); }
    fn len(&self) -> usize { 0 }
}
impl Order {
    fn len(&self) -> usize { 1 }
}
fn count() -> usize { let v: Vec<u8> = Vec::new(); v.len() }
fn fresh() -> Vec<u8> { Vec::new() }
fn new() {}
";
        let b = "pub struct B;\nimpl B {\n    pub fn make() -> B { B }\n}\n";
        let g = graph(&[("/p/lib.rs", lib), ("/p/b.rs", b)]);
        assert_eq!(callers_named(&g, "/p/lib.rs", "make", 0), ["build@5"]);
        assert_eq!(callers_named(&g, "/p/b.rs", "make", 0), ["other@6"]);
        for (name, ordinal) in [("len", 0), ("len", 1), ("new", 0)] {
            assert!(
                callers_named(&g, "/p/lib.rs", name, ordinal).is_empty(),
                "{name}#{ordinal}: {:?}",
                edges(&g)
            );
        }
    }

    /// A method called on a parameter or a field of a type the project does
    /// not define (`&str`, `Vec`) is none of the project's; on one of a
    /// project type, that type's. Direct recursion is an edge of its own.
    #[test]
    fn rust_receivers_are_typed_by_parameters_and_fields() {
        let lib = "\
struct RegexSet;
struct SetMatches;
struct Order { items: Vec<u8>, set: RegexSet }
impl RegexSet {
    fn len(&self) -> usize { 0 }
}
impl SetMatches {
    fn len(&self) -> usize { 1 }
}
impl Order {
    fn size(&self) -> usize { self.items.len() }
    fn count(&self) -> usize { self.set.len() }
}
fn find(haystack: &str) -> usize { haystack.len() }
fn of_set(set: &RegexSet) -> usize { set.len() }
fn fact(n: u64) -> u64 { if n == 0 { 1 } else { n * fact(n - 1) } }
";
        let g = graph(&[("/p/lib.rs", lib)]);
        assert_eq!(
            callers_named(&g, "/p/lib.rs", "len", 0),
            ["count@12", "of_set@15"],
            "{:?}",
            edges(&g)
        );
        assert!(
            callers_named(&g, "/p/lib.rs", "len", 1).is_empty(),
            "{:?}",
            edges(&g)
        );
        let fact = g.id_of_key(Path::new("/p/lib.rs"), "fact", 0).unwrap();
        assert_eq!(g.callees_of(fact), [fact]);
        assert!(g.callers_of(fact).is_empty());
    }

    /// Macro arguments are token trees, not expressions; the calls in them
    /// are read off the tokens. Attributes and `macro_rules!` bodies are not
    /// calls.
    #[test]
    fn rust_calls_inside_macro_arguments_are_edges() {
        let src = "\
fn helper() -> i32 { 1 }
fn compute(x: i32) -> i32 { x }
fn check(v: &[i32]) -> bool { true }
#[cfg(all(test, unix))]
fn run() {
    println!(\"{}\", helper());
    assert_eq!(compute(1), 2);
    let v = vec![helper(), compute(2)];
    assert!(check(&v));
}
macro_rules! m { () => { compute(3) }; }
";
        let g = graph(&[("/p/lib.rs", src)]);
        for callee in ["helper", "compute", "check"] {
            assert!(
                has(&g, "/p/lib.rs:run", &format!("/p/lib.rs:{callee}")),
                "run -> {callee} missing: {:?}",
                edges(&g)
            );
        }
        let calls = calls_of(src, "rust");
        assert!(
            !calls.iter().any(|c| c.callee == "all"),
            "an attribute is not a call: {calls:?}"
        );
        assert!(
            !calls
                .iter()
                .any(|c| c.callee == "compute" && c.caller.is_none() && c.line == 11),
            "a macro_rules! body is not a call site: {calls:?}"
        );
    }

    /// A bare call reaches a unique definition in any language that links
    /// with the caller's: `.tsx` → `.ts`, C++ → C. Never Go → JavaScript.
    #[test]
    fn the_unique_name_fallback_crosses_into_the_same_runtime_only() {
        let g = graph(&[
            (
                "/p/App.tsx",
                "export function App() { return formatName(); }\n",
            ),
            (
                "/p/names.ts",
                "export function formatName(): string { return ''; }\n",
            ),
        ]);
        assert!(
            has(&g, "/p/App.tsx:App", "/p/names.ts:formatName"),
            "{:?}",
            edges(&g)
        );

        let g = graph(&[
            ("/p/a.cpp", "void run() { c_helper(); }\n"),
            ("/p/b.c", "void c_helper(void) {}\n"),
        ]);
        assert!(
            has(&g, "/p/a.cpp:run", "/p/b.c:c_helper"),
            "{:?}",
            edges(&g)
        );

        let g = graph(&[
            ("/p/a.go", "package p\nfunc run() {\n\tWidget()\n}\n"),
            ("/p/w.js", "function Widget() {}\n"),
        ]);
        assert!(edges(&g).is_empty(), "{:?}", edges(&g));
    }

    /// Same-name functions in one file are distinct identities everywhere:
    /// `callee_keys` used to keep only the last one's callees, and
    /// `graph_from_sym_edges` mapped every key onto the first.
    #[test]
    fn same_name_functions_keep_their_own_identity() {
        let src = "\
struct A;
struct B;
impl A {
    fn new() -> A { build_a() }
}
impl B {
    fn new() -> B { build_b() }
}
fn build_a() -> A { A }
fn build_b() -> B { B }
";
        let g = graph(&[("/p/ab.rs", src)]);
        let keys = g.callee_keys();
        let key = |name: &str, ordinal: u32| (PathBuf::from("/p/ab.rs"), name.to_string(), ordinal);
        assert_eq!(keys[&key("new", 0)], vec![key("build_a", 0)], "{keys:?}");
        assert_eq!(keys[&key("new", 1)], vec![key("build_b", 0)], "{keys:?}");

        let second = g.id_of_key(Path::new("/p/ab.rs"), "new", 1).unwrap();
        assert_eq!(g.node(second).line, 7);
        assert_eq!(g.key_of(second), Some(key("new", 1)));
        assert_eq!(
            g.id_of(Path::new("/p/ab.rs"), "new")
                .map(|i| g.node(i).line),
            Some(4)
        );
        assert_eq!(g.id_of_key(Path::new("/p/ab.rs"), "new", 2), None);

        // The LSP-precise path keys the same way.
        let defs = ProjectCallGraph::callable(&defs_of(&[("/p/ab.rs", src)]));
        let edges: SymEdges = HashSet::from([(key("new", 1), key("build_b", 0))]);
        let g2 = ProjectCallGraph::graph_from_sym_edges(defs, &edges);
        let from = g2.id_of_key(Path::new("/p/ab.rs"), "new", 1).unwrap();
        assert_eq!(g2.callees_of(from).len(), 1);
        let first = g2.id_of_key(Path::new("/p/ab.rs"), "new", 0).unwrap();
        assert!(
            g2.callees_of(first).is_empty(),
            "the edge landed on the other `new`"
        );
    }

    /// Nested same-name functions: a call's caller is the definition it is
    /// IN — the innermost whose span holds it — not the nearest one above
    /// it. So a call the outer `f` makes after the inner `f` closed is the
    /// outer's, and the graph lists for each `f` exactly the calls the
    /// reading context (`calls_made_by`) lists for it.
    #[test]
    fn a_call_after_a_nested_same_name_function_is_the_outer_ones() {
        let file = "/p/n.js";
        let src = "\
function f() {
  function f() {
    inner();
  }
  outer();
}
function inner() {}
function outer() {}
";
        let g = graph(&[(file, src)]);
        let key = |name: &str, ordinal: u32| (PathBuf::from(file), name.to_string(), ordinal);
        let keys = g.callee_keys();
        assert_eq!(keys[&key("f", 0)], vec![key("outer", 0)], "{keys:?}");
        assert_eq!(keys[&key("f", 1)], vec![key("inner", 0)], "{keys:?}");

        let analysis = crate::outline::analyze(src, "javascript").unwrap();
        for (ordinal, expected) in [(0, "outer"), (1, "inner")] {
            let item = analysis.function("f", ordinal).unwrap();
            let read: Vec<&str> = analysis
                .calls_made_by(item)
                .map(|c| c.callee.as_str())
                .collect();
            assert_eq!(
                read,
                [expected],
                "the reading context agrees for f#{ordinal}"
            );
        }
        // Only a shared name needs its spans kept.
        let kept = FileCalls::of(
            PathBuf::from(file),
            analysis.lang,
            analysis.calls.clone(),
            &analysis.symbols,
            analysis.owners.clone(),
        )
        .unwrap();
        assert_eq!(kept.bodies.len(), 2, "{:?}", kept.bodies);
    }

    /// A named function expression is known by its binding: its calls belong
    /// to `x`, the node callers reach, not to `named`, which only its own body
    /// can see — and which the TypeScript outline does not list, so the calls
    /// used to fall to whatever function enclosed it (or be dropped).
    #[test]
    fn a_named_function_expression_is_scoped_by_its_binding() {
        for (file, src) in [
            (
                "/p/a.ts",
                "function helper(): void {}\nfunction outer() {\n  const x = function named() { helper(); };\n  x();\n}\n",
            ),
            (
                "/p/a.js",
                "function helper() {}\nfunction outer() {\n  const x = function named() { helper(); };\n  x();\n}\n",
            ),
        ] {
            let g = graph(&[(file, src)]);
            assert!(
                has(&g, &format!("{file}:x"), &format!("{file}:helper")),
                "{file}: {:?}",
                edges(&g)
            );
            assert!(
                !has(&g, &format!("{file}:outer"), &format!("{file}:helper")),
                "{file}: the call was attributed to the enclosing function: {:?}",
                edges(&g)
            );
        }
        // An object entry's function is known by its key the same way.
        let src = "function helper(): void {}\nexport const api = {\n  save: function saveImpl() { helper(); },\n};\n";
        let g = graph(&[("/p/b.ts", src)]);
        assert!(has(&g, "/p/b.ts:save", "/p/b.ts:helper"), "{:?}", edges(&g));
    }

    /// One numbering everywhere: the graph's keys are the outline's ordinals
    /// (`outline::fn_ordinals`, which keys the explain cache), including when
    /// a same-name TYPE sits among the functions, and `key_of` / `id_of_key`
    /// invert each other — also for callables that share a line.
    #[test]
    fn graph_keys_are_the_outline_ordinals() {
        let src = "\
struct new;
impl A {
    fn new() -> A { A }
}
impl B {
    fn new() -> B { B }
}
";
        let symbols = crate::outline::extract(src, "rust");
        let outline = crate::outline::fn_ordinals(&symbols);
        let all_defs: Vec<Def> = symbols
            .iter()
            .map(|s| Def {
                name: s.name.clone(),
                kind: s.kind.clone(),
                file: PathBuf::from("/p/k.rs"),
                line: s.line,
            })
            .collect();
        // `keys_of` over every def, the type included: the callables keep
        // the outline's numbers (the type used to count as a `new`).
        let keys = ProjectCallGraph::keys_of(&all_defs);
        for ((s, key), want) in symbols.iter().zip(&keys).zip(&outline) {
            if crate::outline::is_callable(&s.kind) {
                assert_eq!(key.2, *want, "{s:?}");
            }
        }

        let g = graph(&[("/p/k.rs", src)]);
        for id in 0..g.node_count() {
            let n = g.node(id);
            let s = symbols
                .iter()
                .position(|s| s.name == n.name && s.line == n.line)
                .expect("every node is an outline entry");
            assert_eq!(g.ordinals()[id], outline[s], "{n:?}");
            let key = g.key_of(id).unwrap();
            let back = g.id_of_key(&key.0, &key.1, key.2).unwrap();
            assert_eq!(
                (g.node(back).name.as_str(), g.node(back).line),
                (n.name.as_str(), n.line),
                "{key:?}"
            );
        }
        assert!(g.id_of_key(Path::new("/p/k.rs"), "new", 1).is_some());
        assert_eq!(g.id_of_key(Path::new("/p/k.rs"), "new", 2), None);

        // Two callables on one line share an ordinal, and the next one counts
        // both: 0, 0, 2 — and ordinal 0 names the first of the two.
        let def = |line: usize| Def {
            name: "pair".into(),
            kind: "function".into(),
            file: PathBuf::from("/p/k.rs"),
            line,
        };
        let g = ProjectCallGraph::from_callable_defs(vec![def(8), def(8), def(9)], HashSet::new());
        assert_eq!(g.ordinals(), &[0, 0, 2]);
        let file = Path::new("/p/k.rs");
        assert_eq!(g.id_of_key(file, "pair", 0), Some(0));
        assert_eq!(g.id_of_key(file, "pair", 1), None);
        assert_eq!(g.id_of_key(file, "pair", 2), Some(2));
    }

    /// A graph from the wire is checked before anything indexes with it, and
    /// a good one survives the trip with its edges and identities intact.
    #[test]
    fn a_malformed_graph_is_rejected_not_trusted() {
        let good = graph(&[("/p/a.rs", "fn a() { b(); }\nfn b() {}\n")]);
        let wire = good.to_wire(|p| p.strip_prefix("/p").unwrap().to_string_lossy().into_owned());
        assert!(wire.nodes.iter().all(|n| n.file == "a.rs"));
        let back = ProjectCallGraph::from_wire(wire, |rel| Path::new("/q").join(rel))
            .expect("a real graph converts");
        assert_eq!(back.edge_count(), 1);
        assert!(back.id_of(Path::new("/q/a.rs"), "b").is_some());

        let node = |callers: Vec<usize>, callees: Vec<usize>| clew_protocol::CallGraphNode {
            name: "a".into(),
            kind: "function".into(),
            file: "a.rs".into(),
            line: 1,
            callers,
            callees,
        };
        let bad = clew_protocol::CallGraph {
            nodes: vec![node(vec![], vec![7])],
        };
        let err = ProjectCallGraph::from_wire(bad, |rel| PathBuf::from(rel))
            .expect_err("index 7 of 1 node");
        assert!(err.contains("links to node 7"), "{err}");
        let bad = clew_protocol::CallGraph {
            nodes: vec![node(vec![1], vec![])],
        };
        assert!(ProjectCallGraph::from_wire(bad, |rel| PathBuf::from(rel)).is_err());
        assert!(back.get(5).is_none());
    }
}
