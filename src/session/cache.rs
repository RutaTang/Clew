//! Persistent warm-start cache for the symbol index and content hashes, stored
//! in the project's derived-artifact directory (`clew_core::derived::dir`,
//! inside clew's own data directory — never inside the project).
//!
//! On reopen, most files have not changed while clew was closed. Rather than
//! re-read and re-parse the whole tree, this cache lets the index build confirm
//! each file cheaply and reuse its cached symbols + hash when the content is
//! unchanged, re-parsing only what actually changed.
//!
//! Correctness rules (this is a pure accelerator — it must never yield a wrong
//! result):
//!   * A file's cached symbols/hash are reused **only after the content is
//!     confirmed unchanged**: a `stat()` fast path (mtime + size both match)
//!     or, when that differs, a content-hash match. The mtime fast path is the
//!     industry-standard heuristic (git, make, rust-analyzer); its only hole —
//!     identical mtime *and* size but different bytes — is not reachable by any
//!     normal edit and self-heals on the next real change (the live watcher
//!     re-hashes it).
//!   * The file carries a schema `version`; a mismatch (e.g. after a clew
//!     upgrade that changes symbol extraction) makes the whole cache be ignored
//!     and rebuilt. **Bump [`CACHE_VERSION`] whenever symbol extraction or the
//!     hash changes.** (The content hash itself is frozen — see
//!     `clew_core::incremental::content_hash` — so a toolchain upgrade alone
//!     never changes it.)
//!   * Any read/parse/version error falls back to an empty cache (full rebuild).
//!     Unlike the user's own stores, overwriting an unreadable cache is fine:
//!     it holds nothing that cannot be derived again.
//!   * It lives in clew's data directory, keyed by project, so a repository
//!     can never ship one: every entry is keyed by a content hash the
//!     repository could compute for its own files, and a committed cache would
//!     otherwise be accepted as clew's own work.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::incremental::Version;

/// Bump on any change to symbol/import extraction or the content hash so stale
/// caches from an older clew are discarded rather than trusted.
///
/// 6: Rust imports are rescoped per inline module, `use` groups are expanded,
/// and CommonJS / dynamic-import specifiers are extracted, so an index built by
/// an older clew describes different edges.
/// 7: Rust specifiers record re-exports (`pub `) and globs (`::*`), and Python
/// `from . import x` is recorded as `.:x` (`clew_core::rustscope`,
/// `clew_core::imports::PY_NAME_SEP`).
/// 8: each file's call sites (and the lines of its bodyless callables) are
/// cached beside its symbols ([`CachedCalls`]), so the project call graph is
/// built from the index instead of a second parse of every file; an older
/// entry has none to offer.
/// 9: the definition spans of a file's same-name callables are cached with
/// its call sites ([`CachedCalls::bodies`]), which tell a call after a nested
/// same-name function from one inside it; an older entry would resolve such
/// a call by the old rule.
/// 10: Rust `const` and `static` items are symbols (kind `constant`), so a
/// `use crate::LIMIT` resolves to the module defining the const instead of
/// being inferred; an older entry lists none of them.
/// 11: each function's entry-point kind is cached ([`CachedSymbol::entry`]);
/// an older entry has none.
/// 12: Rust raw call names retain their source spelling, same-line inline
/// imports keep their own module scope, and distinct same-line declarations
/// survive outline extraction. Rebuild older snapshots with those fixes.
/// 13: entry points are told more strictly — a Go handler by its whole
/// signature, a Java HTTP verb only as JAX-RS or Micronaut writes it, no
/// database annotation, a wrapped Rust attribute read whole; an older entry
/// keeps the looser kinds.
/// 14: Java, PHP, Ruby, C#, Erlang, Haskell and Zig imports are extracted —
/// and a Java package or C# namespace a file declares, recorded beside them
/// (`clew_core::imports::RawImport::is_mod_decl`); an older entry lists none.
/// 15: each call site's receiver type and each method's class are kept
/// ([`CachedCalls::receivers`], [`CachedCalls::owners`]), which narrow a
/// method call to its class's method; an older entry has neither.
/// 16: a receiver is typed only as what it is exactly — built by a
/// constructor that is the whole expression, or any value in Go and Rust —
/// and a name bound again to what cannot be told is untyped; an older entry
/// kept types that narrowed a call off the override or the builder it made.
/// 17: a closure's captured name is typed as the function around it binds
/// it; an assignment in a nested function, a pattern's names, a parameter's
/// default leave a receiver untyped, and a comment or a keyword argument
/// binds nothing; an older entry typed by them or lost the captured type.
/// 18: a minifier's output is indexed empty
/// (`clew_core::fs_scan::is_minified_source`), and files up to the viewer's
/// size are read; an older store holds a bundle's names, or nothing of a
/// file between the old and the new cap.
/// 19: Python's `from shop import payments` is recorded with the name it
/// imports (`shop:payments`), so the edge reaches `shop/payments.py`, not
/// `shop/__init__.py`; an older entry has only the module.
/// 20: the methods read as attributes (a Python `@property`) are cached with
/// a file's call sites ([`CachedCalls::accessed`]); an older entry has none.
/// 21: a Rust struct's fields, an enum's variants and a `static` are
/// symbols of their own (a `static` no longer a constant); a module
/// declaration keeps its `#[path]`; a receiver is typed by a parameter or a
/// field; a method keeps its owner ([`CachedSymbol::owner`]). An older entry
/// has none of these.
/// 22: tests and groups of tests a runner's call opens (ExUnit's, RSpec's,
/// Kotest's, MUnit's, hspec's, package:test's, GoogleTest's and Catch2's)
/// are symbols, and the calls made in them are theirs; an older entry has
/// neither, nor a C++ test but as a function `TEST`.
/// 23: a call in no function has its receiver (`Cart.total(…)` in a test
/// block), and a function passed as a value is a call of it (Haskell's
/// `foldr add`, Erlang's `fun add/2`, Elixir's `&add/2`); an older entry
/// has neither.
/// 24: a C or C++ function of its file alone (`static`, an anonymous
/// namespace's) is marked so; an older entry marks none, and would link
/// another file's call to it.
/// 25: Haskell's operators (`a <+> b = …`) and backticked functions are
/// symbols, and their uses calls; an older entry has neither.
/// 26: a receiver declared as a type (a parameter's, a field's, a local's)
/// or `this`/`self` is kept, and each file's package and its types' bases
/// ([`CachedCalls::package`], [`CachedCalls::types`]) — what a call is
/// resolved through to the type's method, inherited or overridden, and to
/// another file of its package; an older entry has none of them.
/// 27: a Dart constructor with no body (`CounterCubit() : super(0);`,
/// `const BlocProvider({…});`) is a symbol, and an Elixir file's `use`s and
/// `import`s are kept ([`CachedCalls::uses`], [`CachedCalls::imports`]), and
/// a Scala call's qualifier ([`CachedCalls::qualifiers`]); an older entry has
/// none of them.
/// 28: a method of a class its grammar could not finish (Kotlin's, with a
/// property named `where`) is still its class's ([`CachedCalls::owners`]).
/// 29: a file saved with a byte-order mark is in its package or namespace
/// ([`CachedCalls::package`]).
/// 30: an Erlang or Elixir function's arities and a call's
/// ([`CachedCalls::arities`], [`CachedCalls::call_arities`]).
/// 31: a Kotlin function whose body calls a method of its own name is no
/// extension ([`CachedCalls::extensions`]); an Elixir default that is an
/// anonymous function is one argument ([`CachedCalls::arities`]).
/// 32: a Go file's imported package names ([`CachedCalls::imports`]), a
/// Zig function's container ([`CachedCalls::owners`]), and a C++ call's
/// scope (`Cart` in `Cart::make()`, [`CachedCalls::qualifiers`]).
/// 33: the arities of Java, Kotlin, Scala, C#, Dart, C, C++, Go and Zig
/// functions and calls ([`CachedCalls::arities`],
/// [`CachedCalls::call_arities`]).
/// 34: each function's declared return type ([`CachedCalls::returns`]),
/// and a value assigned from a call typed by it.
/// 35: an interface's methods are its own ([`CachedCalls::owners`]).
/// 36: a type alias is a type whose base is the type it names; C++
/// declarators in a function body, and conversion operators a macro hid,
/// are no functions; C++ plain lower-case types, smart-pointer factories,
/// and Zig's `self` and standard-library aliases type receivers; a C or C++
/// class owns nothing past its body's close.
/// 37: what each call passes and each function takes
/// ([`CachedCalls::call_arguments`], [`CachedCalls::parameters`]), Java's
/// and C#'s built-in types bind, and a Scala file's chained package clauses
/// open their packages ([`CachedCalls::enclosing`]).
/// 38: a Play template is a function; a destructor a C++ header read as C
/// declares is `~Name`; a Dart named constructor is its second name's and a
/// mixin owns its methods; a C++ parameter list that shows a default gives
/// its fewest arguments; Zig's file imports qualify their calls.
/// 39: a Go file's struct fields ([`CachedCalls::fields`]), a chain of
/// fields from a typed value as a receiver, and a Go parameter's package;
/// a factory's value by its function's return; a Kotlin class's companion
/// object's bases among its own.
/// 40: a Go package's variables among its fields; `Object`/`Any`/`object`
/// a parameter kind of its own, below a class the argument is.
/// 41: a value assigned from a method of a value is of that value's type's
/// method's return (`@value#method` receivers).
/// 42: a C++ class's export macro is no name of it; a C++ function's
/// fewest arguments its own list's; Zig's fields and file variables.
pub(crate) const CACHE_VERSION: u32 = 42;

/// A cached symbol (the index entry minus the paths, which are reconstructed
/// from the project root + relative path on load).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedSymbol {
    pub name: String,
    pub kind: String,
    pub line: usize,
    /// Whether this is a test function (a `#[…test…]` attribute or test-name
    /// convention). Cached so the outline/call-graph don't re-scan per frame.
    #[serde(default)]
    pub is_test: bool,
    /// The entry-point kind's key (`clew_core::outline::EntryKind::key`), for
    /// a function execution enters the project through.
    #[serde(default)]
    pub entry: Option<String>,
    /// The type, class or trait a method belongs to.
    #[serde(default)]
    pub owner: Option<String>,
}

/// A cached raw import (the extraction result, resolved lazily against the live
/// file set — resolution is cheap and depends on files that may have moved).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedImport {
    pub module: String,
    pub line: usize,
    #[serde(default)]
    pub is_mod: bool,
}

/// A file's call sites as cached (`clew_core::projectcalls::FileCalls`).
///
/// Compact on purpose. A file makes many more calls than it defines symbols,
/// and `index.json` is read through a 64 MiB cap
/// (`clew_core::statefile::MAX_STATE_BYTES`) past which the WHOLE cache is
/// ignored and every open parses the project again — so each name is stored
/// once per file and each call site is three small numbers in one flat
/// array, instead of an object repeating its field names and both names.
/// (On clew's own tree: about 13 bytes a call site, names included — 57k
/// sites add 0.75 MB to a 0.54 MB index.)
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedCalls {
    /// The language the file was PARSED as (`Lang::key`): a C++ `.h` reads
    /// as C++, which its extension alone does not say.
    pub lang: String,
    /// Every name a call site mentions, as its caller or its callee, once.
    pub names: Vec<String>,
    /// Three numbers per call site, in the file's order: the caller (its
    /// index in `names` plus one; `0` for a call outside any function), the
    /// callee (its index in `names` times two, plus one for a
    /// `receiver.name(…)` call), and the line as its distance from the
    /// previous site's line — small, since sites come in source order.
    pub sites: Vec<i64>,
    /// 1-based lines of the file's callables declared without a body.
    pub declarations: Vec<usize>,
    /// Three numbers per definition of a name several callables of the file
    /// share (`clew_core::projectcalls::FileCalls::bodies`), by line: its
    /// definition line, then the first and last lines of the whole
    /// definition. Empty for most files.
    #[serde(default)]
    pub bodies: Vec<usize>,
    /// One number per call site, in `sites`' order: what its receiver is
    /// known to be (`clew_core::projectcalls::CallSite::receiver`) — `0`
    /// unknown, `1` the caller's own class, a type's index in `names` plus
    /// two; a type it is declared as, that index plus two, negated.
    #[serde(default)]
    pub receivers: Vec<i64>,
    /// One number per call site, in `sites`' order, where any site has one:
    /// the name a Scala call is made on
    /// (`clew_core::projectcalls::CallSite::qualifier`) — `0` none, else
    /// its index in `names` plus one. Empty when no site has one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub qualifiers: Vec<i64>,
    /// One number per call site, in `sites`' order, where any site has one:
    /// how many arguments an Erlang or Elixir call passes
    /// (`clew_core::projectcalls::CallSite::arity`) — `0` unknown, else the
    /// count plus one. Empty when no site has one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub call_arities: Vec<i64>,
    /// One number per call site, in `sites`' order, where any site has one:
    /// the kinds of the arguments it passes
    /// (`clew_core::projectcalls::CallSite::arguments`) — `0` unknown, else
    /// their index in `names` plus one. Empty when no site has one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub call_arguments: Vec<i64>,
    /// Two numbers per method whose class is known
    /// (`clew_core::projectcalls::FileCalls::owners`): its definition line,
    /// then the class's index in `names`.
    #[serde(default)]
    pub owners: Vec<i64>,
    /// Definition lines of the methods read as attributes, never called
    /// (`clew_core::projectcalls::FileCalls::accessed`).
    #[serde(default)]
    pub accessed: Vec<usize>,
    /// Definition lines of C and C++ functions of the file alone
    /// (`clew_core::projectcalls::FileCalls::internal`).
    #[serde(default)]
    pub internal: Vec<usize>,
    /// The file's package (`clew_core::projectcalls::FileContext::package`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
    /// The packages a Scala file's chained package clauses open
    /// (`clew_core::projectcalls::FileContext::enclosing`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enclosing: Vec<String>,
    /// The types the file declares with their bases
    /// (`clew_core::projectcalls::FileContext::types`), only those with any.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub types: Vec<(String, Vec<String>)>,
    /// Definition lines of extension methods
    /// (`clew_core::projectcalls::FileContext::extensions`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extensions: Vec<usize>,
    /// The modules an Elixir file `use`s
    /// (`clew_core::projectcalls::FileContext::uses`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub uses: Vec<String>,
    /// The modules an Elixir file `import`s
    /// (`clew_core::projectcalls::FileContext::imports`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub imports: Vec<String>,
    /// Three numbers per Erlang or Elixir function: its definition line,
    /// and the fewest and most arguments it takes
    /// (`clew_core::projectcalls::FileContext::arities`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub arities: Vec<usize>,
    /// Each function's declared return type, by its definition line
    /// (`clew_core::projectcalls::FileContext::returns`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub returns: Vec<(usize, String)>,
    /// The kinds of each function's parameters, by its definition line
    /// (`clew_core::projectcalls::FileContext::parameters`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parameters: Vec<(usize, String)>,
    /// A Go file's struct fields and their types
    /// (`clew_core::projectcalls::FileContext::fields`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<(String, String, String)>,
}

/// Everything cached about one file: how to confirm it is unchanged (mtime,
/// size, content hash) and the derived artifacts to reuse when it is.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileCache {
    pub mtime_ns: u64,
    pub size: u64,
    pub hash: Version,
    pub symbols: Vec<CachedSymbol>,
    /// Raw imports (only the definition text is extracted; resolution is not
    /// cached). Defaulted so a partial/older entry still loads.
    #[serde(default)]
    pub imports: Vec<CachedImport>,
    /// The call sites, for a file whose language has a call model and that
    /// makes or declares any; `None` otherwise.
    #[serde(default)]
    pub calls: Option<CachedCalls>,
}

/// The on-disk, versioned envelope (owned form, for loading).
#[derive(Debug, Deserialize)]
struct Persisted {
    version: u32,
    entries: HashMap<String, FileCache>, // key = project-relative path
}

/// Borrowing form, for saving without cloning the whole map.
#[derive(Serialize)]
struct PersistedRef<'a> {
    version: u32,
    entries: &'a HashMap<String, FileCache>,
}

/// In-memory cache keyed by project-relative path.
#[derive(Debug, Default)]
pub struct Store {
    entries: HashMap<String, FileCache>,
}

fn cache_path(store: &Path) -> PathBuf {
    store.join("index.json")
}

impl Store {
    /// Load the cache for `root`, or an empty store when it is missing, corrupt,
    /// or from a different schema version (any of which forces a full rebuild).
    pub fn load(store: &Path) -> Store {
        let entries = clew_core::statefile::read(&cache_path(store))
            .and_then(|s| serde_json::from_str::<Persisted>(&s).ok())
            .filter(|p| p.version == CACHE_VERSION)
            .map(|p| p.entries)
            .unwrap_or_default();
        Store { entries }
    }

    /// The cached record for a relative path, if present.
    pub fn get(&self, rel: &str) -> Option<&FileCache> {
        self.entries.get(rel)
    }

    pub fn insert(&mut self, rel: String, entry: FileCache) {
        self.entries.insert(rel, entry);
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Persist the cache into the derived-artifact directory `store`.
    /// Best-effort: a write error is surfaced but never corrupts anything,
    /// since a missing/partial cache just triggers a rebuild next time.
    pub fn save(&self, store: &Path) -> std::io::Result<()> {
        // Compact (not pretty): this is machine data and can be large.
        let json = serde_json::to_string(&PersistedRef {
            version: CACHE_VERSION,
            entries: &self.entries,
        })
        .map_err(|e| std::io::Error::other(e.to_string()))?;
        clew_core::statefile::write_atomic(&cache_path(store), json.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(hash: Version) -> FileCache {
        FileCache {
            mtime_ns: 1,
            size: 2,
            hash,
            symbols: vec![CachedSymbol {
                owner: None,
                name: "foo".into(),
                kind: "function".into(),
                line: 3,
                is_test: false,
                entry: None,
            }],
            imports: vec![CachedImport {
                module: "crate::bar".into(),
                line: 1,
                is_mod: false,
            }],
            calls: Some(CachedCalls {
                lang: "rust".into(),
                names: vec!["foo".into(), "bar".into()],
                sites: vec![1, 2, 4],
                declarations: Vec::new(),
                bodies: vec![3, 3, 9],
                receivers: vec![1],
                qualifiers: vec![2],
                call_arities: vec![3],
                call_arguments: vec![1],
                owners: vec![3, 1],
                accessed: vec![3],
                internal: vec![9],
                package: Some("shop".into()),
                enclosing: vec!["shop".into()],
                types: vec![("Cart".into(), vec!["Base".into()])],
                extensions: vec![3],
                uses: vec!["Shop.Context".into()],
                imports: vec!["Ecto.Query".into()],
                arities: vec![3, 1, 2],
                returns: vec![(3, "Cart".into())],
                parameters: vec![(3, "s,i".into())],
                fields: vec![("Context".into(), "Request".into(), "http.Request".into())],
            }),
        }
    }

    #[test]
    fn round_trips_through_disk() {
        // The store is a derived-artifact directory (never a project).
        let root = clew_core::testutil::TempDir::new("cache-rt");

        let mut store = Store::default();
        store.insert("src/a.rs".into(), entry(42));
        store.save(&root).unwrap();

        let loaded = Store::load(&root);
        assert_eq!(loaded.len(), 1);
        let e = loaded.get("src/a.rs").unwrap();
        assert_eq!(e.hash, 42);
        assert_eq!(e.symbols[0].name, "foo");
        assert_eq!(e.calls, entry(42).calls, "the call sites survive the trip");
    }

    #[test]
    fn version_mismatch_and_corruption_yield_empty() {
        let root = clew_core::testutil::TempDir::new("cache-bad");
        let path = cache_path(&root);

        // Wrong version → ignored.
        std::fs::write(&path, r#"{"version":999,"entries":{}}"#).unwrap();
        assert!(Store::load(&root).is_empty());

        // Corrupt JSON → ignored (rebuild), never a panic.
        std::fs::write(&path, "not json at all").unwrap();
        assert!(Store::load(&root).is_empty());

        // Missing file → empty.
        std::fs::remove_file(&path).unwrap();
        assert!(Store::load(&root).is_empty());
    }
}
