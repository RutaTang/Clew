//! The import graph: a file→file dependency map derived from each file's
//! import statements.
//!
//! Where the call graph is symbol→symbol and needs a language server, the import
//! graph is purely structural and computed locally from tree-sitter — so the
//! whole project's dependency map is available the instant a scan finishes, and
//! offline. It is the cleanest kind of derived artifact for clew's incremental
//! core: a file's *out-edges* (what it imports) are a pure function of that
//! file's own bytes, so a change re-extracts only that file; reverse edges (who
//! imports it) are just the inversion.
//!
//! Two stages, deliberately separated:
//!   * **Extraction** ([`imports_of`]) — a tree-sitter pass that reads the raw
//!     import specifiers a file writes down. Pure per file, cacheable.
//!   * **Resolution** ([`Resolver`]) — per-language rules that map a specifier
//!     to a project file ([`Target::Internal`]), an external package
//!     ([`Target::External`], not penetrated), or nothing ([`Target::Unresolved`]).
//!     This runs over the known file set, so creating/deleting a file can change
//!     how *other* files resolve; the graph re-resolves on such structural change.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Parser, Query, QueryCursor};

/// One import as written in a file, before resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawImport {
    /// The specifier text, normalized per language: a Rust `use` path
    /// (`crate::lsp::client`), a Python dotted/relative module (`os.path`,
    /// `.sibling`), a JS/TS specifier (`./util`, `react`), or a Go import path.
    pub module: String,
    /// 1-based line of the statement (for navigation).
    pub line: usize,
    /// Rust `mod x;` — `module` is a submodule name to resolve to a sibling file,
    /// not a scoped path.
    pub is_mod_decl: bool,
}

/// Where an import points once resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A file (or, for Go, a package directory) inside the project.
    Internal(PathBuf),
    /// An external package / standard library — shown as a leaf, never penetrated.
    External(String),
    /// A specifier we couldn't map to anything (e.g. a `mod x;` whose file is
    /// missing). Kept rather than dropped, so the graph is honest.
    Unresolved(String),
}

/// A resolved out-edge: which file/package an import points to, and where the
/// statement sits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edge {
    pub target: Target,
    pub specifier: String,
    pub line: usize,
}

// ---------------------------------------------------------------------------
// Extraction (tree-sitter)
// ---------------------------------------------------------------------------

/// Extract the raw imports from one file's `source`. Returns an empty list for
/// languages without an import model or when parsing fails. Blocking; run off
/// the UI thread.
pub fn imports_of(source: &str, lang: &str) -> Vec<RawImport> {
    let Some(language) = crate::highlight::language_for(lang) else {
        return Vec::new();
    };
    let mut parser = Parser::new();
    if parser.set_language(&language).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };
    let root = tree.root_node();
    match lang {
        "rust" => rust_imports(root, source, &language),
        "python" => python_imports(root, source, &language),
        "javascript" | "typescript" | "tsx" => js_imports(root, source, &language),
        "go" => go_imports(root, source, &language),
        _ => Vec::new(),
    }
}

fn node_text<'a>(node: Node, src: &'a str) -> &'a str {
    src.get(node.byte_range()).unwrap_or("")
}

/// Run `query_src` against `root`, calling `f` once per match with that match's
/// captures resolved to `(capture_name, node)`.
fn for_each_match(
    root: Node,
    language: &tree_sitter::Language,
    query_src: &str,
    src: &str,
    mut f: impl FnMut(&[(&str, Node)]),
) {
    let Ok(query) = Query::new(language, query_src) else {
        return;
    };
    let names = query.capture_names();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&query, root, src.as_bytes());
    while let Some(m) = matches.next() {
        let caps: Vec<(&str, Node)> = m
            .captures
            .iter()
            .map(|c| (names[c.index as usize], c.node))
            .collect();
        f(&caps);
    }
}

const RUST_QUERY: &str = r#"
(mod_item name: (identifier) @mod_name) @mod_item
(use_declaration argument: (_) @use_arg)
"#;

fn rust_imports(root: Node, src: &str, language: &tree_sitter::Language) -> Vec<RawImport> {
    let mut out = Vec::new();
    for_each_match(root, language, RUST_QUERY, src, |caps| {
        let mod_item = caps.iter().find(|(n, _)| *n == "mod_item").map(|(_, n)| *n);
        if let Some(mi) = mod_item {
            // `mod x { ... }` (has a body) is an inline module, not a file edge.
            if mi.child_by_field_name("body").is_some() {
                return;
            }
            if let Some((_, name)) = caps.iter().find(|(n, _)| *n == "mod_name") {
                out.push(RawImport {
                    module: node_text(*name, src).to_string(),
                    line: mi.start_position().row + 1,
                    is_mod_decl: true,
                });
            }
            return;
        }
        if let Some((_, arg)) = caps.iter().find(|(n, _)| *n == "use_arg") {
            let path = rust_use_path(node_text(*arg, src));
            if !path.is_empty() {
                out.push(RawImport {
                    module: path,
                    line: arg.start_position().row + 1,
                    is_mod_decl: false,
                });
            }
        }
    });
    out
}

/// Normalize a `use` tree's text to its leading module path: cut a grouped
/// `{...}`, an `as` alias or a `*` glob, and trim a trailing `::`. Grouped
/// imports collapse to their common module prefix (a safe over-approximation;
/// LSP refinement can split them later).
fn rust_use_path(text: &str) -> String {
    let mut s = text.trim();
    if let Some(i) = s.find('{') {
        s = s[..i].trim_end();
    }
    if let Some(i) = s.find(" as ") {
        s = s[..i].trim_end();
    }
    s = s.trim_end_matches('*').trim_end();
    s.trim_end_matches("::").trim().to_string()
}

const PYTHON_QUERY: &str = r#"
(import_statement name: (dotted_name) @plain)
(import_statement name: (aliased_import name: (dotted_name) @plain))
(import_from_statement module_name: (_) @from)
"#;

fn python_imports(root: Node, src: &str, language: &tree_sitter::Language) -> Vec<RawImport> {
    let mut out = Vec::new();
    for_each_match(root, language, PYTHON_QUERY, src, |caps| {
        for (name, node) in caps {
            let text = node_text(*node, src).trim().to_string();
            if text.is_empty() {
                continue;
            }
            // `plain` is always absolute; `from` may carry leading dots (relative).
            let _ = name;
            out.push(RawImport {
                module: text,
                line: node.start_position().row + 1,
                is_mod_decl: false,
            });
        }
    });
    out
}

const JS_QUERY: &str = r#"
(import_statement source: (string) @src)
(export_statement source: (string) @src)
"#;

fn js_imports(root: Node, src: &str, language: &tree_sitter::Language) -> Vec<RawImport> {
    let mut out = Vec::new();
    for_each_match(root, language, JS_QUERY, src, |caps| {
        if let Some((_, node)) = caps.iter().find(|(n, _)| *n == "src") {
            let spec = strip_quotes(node_text(*node, src));
            if !spec.is_empty() {
                out.push(RawImport {
                    module: spec.to_string(),
                    line: node.start_position().row + 1,
                    is_mod_decl: false,
                });
            }
        }
    });
    out
}

const GO_QUERY: &str = r#"
(import_spec path: (interpreted_string_literal) @path)
(import_spec path: (raw_string_literal) @path)
"#;

fn go_imports(root: Node, src: &str, language: &tree_sitter::Language) -> Vec<RawImport> {
    let mut out = Vec::new();
    for_each_match(root, language, GO_QUERY, src, |caps| {
        if let Some((_, node)) = caps.iter().find(|(n, _)| *n == "path") {
            let spec = strip_quotes(node_text(*node, src));
            if !spec.is_empty() {
                out.push(RawImport {
                    module: spec.to_string(),
                    line: node.start_position().row + 1,
                    is_mod_decl: false,
                });
            }
        }
    });
    out
}

/// Strip a single pair of surrounding `"`, `'` or backtick quotes.
fn strip_quotes(s: &str) -> &str {
    let s = s.trim();
    let bytes = s.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' || first == b'\'' || first == b'`') && first == last {
            return &s[1..s.len() - 1];
        }
    }
    s
}

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

/// Everything resolution needs about the project's file layout: the set of files
/// (for existence checks), the Rust crate source root (where `crate::` starts),
/// and the Go module prefix (from `go.mod`). Built once per full resolve pass;
/// cheap in-memory lookups after that.
pub struct Resolver {
    root: PathBuf,
    files: HashSet<PathBuf>,
    dirs: HashSet<PathBuf>,
    /// Directory that `crate::` resolves against (holds `lib.rs`/`main.rs`).
    rust_crate_src: Option<PathBuf>,
    /// The `module` line from `go.mod`, if any.
    go_module: Option<String>,
}

impl Resolver {
    pub fn new(root: &Path, files: &[PathBuf]) -> Self {
        let file_set: HashSet<PathBuf> = files.iter().cloned().collect();
        let mut dirs = HashSet::new();
        for f in files {
            let mut p = f.parent();
            while let Some(d) = p {
                if !dirs.insert(d.to_path_buf()) {
                    break; // ancestors already recorded
                }
                if d == root {
                    break;
                }
                p = d.parent();
            }
        }
        let rust_crate_src = file_set
            .iter()
            .filter(|f| {
                matches!(
                    f.file_name().and_then(|n| n.to_str()),
                    Some("lib.rs") | Some("main.rs")
                )
            })
            // Prefer a crate root under a `src/` directory, else the shallowest.
            .min_by_key(|f| {
                let in_src = f.parent().and_then(|p| p.file_name()) == Some(std::ffi::OsStr::new("src"));
                (!in_src, f.components().count())
            })
            .and_then(|f| f.parent().map(Path::to_path_buf));
        let go_module = read_go_module(root);
        Resolver {
            root: root.to_path_buf(),
            files: file_set,
            dirs,
            rust_crate_src,
            go_module,
        }
    }

    fn has_file(&self, p: &Path) -> bool {
        self.files.contains(p)
    }

    /// Resolve one raw import from `from` (the importing file).
    pub fn resolve(&self, raw: &RawImport, from: &Path, lang: &str) -> Target {
        match lang {
            "rust" => self.resolve_rust(raw, from),
            "python" => self.resolve_python(raw, from),
            "javascript" | "typescript" | "tsx" => self.resolve_js(raw, from),
            "go" => self.resolve_go(raw),
            _ => Target::External(raw.module.clone()),
        }
    }

    // --- Rust ---

    fn resolve_rust(&self, raw: &RawImport, from: &Path) -> Target {
        if raw.is_mod_decl {
            // Submodules live beside a `mod.rs`/`lib.rs`/`main.rs`, or in a
            // directory named after a plain module file.
            let dir = self.rust_mod_dir(from);
            return self
                .rust_module_file(&dir, &[raw.module.as_str()])
                .map(Target::Internal)
                .unwrap_or_else(|| Target::Unresolved(raw.module.clone()));
        }
        let segs: Vec<&str> = raw.module.split("::").filter(|s| !s.is_empty()).collect();
        let Some((&head, rest)) = segs.split_first() else {
            return Target::Unresolved(raw.module.clone());
        };
        let (base, path): (PathBuf, &[&str]) = match head {
            "crate" => match &self.rust_crate_src {
                Some(d) => (d.clone(), rest),
                None => return Target::External(raw.module.clone()),
            },
            // `super::x` → a sibling module of the current one.
            "super" => (from.parent().unwrap_or(&self.root).to_path_buf(), rest),
            // `self::x` → a child module of the current one.
            "self" => (self.rust_mod_dir(from), rest),
            _ => return Target::External(head.to_string()),
        };
        // The trailing segment(s) may be items, not modules: resolve the longest
        // prefix of `path` that maps to an existing module file.
        match self.rust_resolve_prefix(&base, path) {
            Some(p) => Target::Internal(p),
            None => Target::Unresolved(raw.module.clone()),
        }
    }

    /// Directory that `from`'s submodules live in.
    fn rust_mod_dir(&self, from: &Path) -> PathBuf {
        let stem = from.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        let parent = from.parent().unwrap_or(&self.root);
        if matches!(stem, "mod" | "lib" | "main") {
            parent.to_path_buf()
        } else {
            parent.join(stem)
        }
    }

    /// File for a module reached by `segs` under `base` (`segs` are dirs except
    /// the last, which is `<name>.rs` or `<name>/mod.rs`).
    fn rust_module_file(&self, base: &Path, segs: &[&str]) -> Option<PathBuf> {
        let (last, dirs) = segs.split_last()?;
        let mut dir = base.to_path_buf();
        for d in dirs {
            dir = dir.join(d);
        }
        let flat = dir.join(format!("{last}.rs"));
        if self.has_file(&flat) {
            return Some(flat);
        }
        let nested = dir.join(last).join("mod.rs");
        if self.has_file(&nested) {
            return Some(nested);
        }
        None
    }

    /// Longest prefix of `path` (dropping trailing item segments) that resolves
    /// to a module file; empty `path` maps to the crate-root file at `base`.
    fn rust_resolve_prefix(&self, base: &Path, path: &[&str]) -> Option<PathBuf> {
        for k in (1..=path.len()).rev() {
            if let Some(p) = self.rust_module_file(base, &path[..k]) {
                return Some(p);
            }
        }
        // `crate::Item` — an item in the crate root itself.
        for name in ["lib.rs", "main.rs"] {
            let p = base.join(name);
            if self.has_file(&p) {
                return Some(p);
            }
        }
        None
    }

    // --- Python ---

    fn resolve_python(&self, raw: &RawImport, from: &Path) -> Target {
        let level = raw.module.chars().take_while(|&c| c == '.').count();
        let body = &raw.module[level..];
        let segs: Vec<&str> = body.split('.').filter(|s| !s.is_empty()).collect();

        if level > 0 {
            // Relative: `.` = current package (dir), each extra dot goes up one.
            let mut base = from.parent().unwrap_or(&self.root).to_path_buf();
            for _ in 1..level {
                base = base.parent().unwrap_or(&self.root).to_path_buf();
            }
            return self
                .python_module_file(&base, &segs)
                .map(Target::Internal)
                .unwrap_or_else(|| Target::Unresolved(raw.module.clone()));
        }
        // Absolute: try the project root and each ancestor of `from` as a source
        // root (covers `src/`-layout and package-relative resolution).
        let mut bases: Vec<PathBuf> = vec![self.root.clone()];
        let mut p = from.parent();
        while let Some(d) = p {
            bases.push(d.to_path_buf());
            if d == self.root {
                break;
            }
            p = d.parent();
        }
        for base in bases {
            if let Some(f) = self.python_module_file(&base, &segs) {
                return Target::Internal(f);
            }
        }
        Target::External(segs.first().copied().unwrap_or(&raw.module).to_string())
    }

    fn python_module_file(&self, base: &Path, segs: &[&str]) -> Option<PathBuf> {
        if segs.is_empty() {
            let init = base.join("__init__.py");
            return self.has_file(&init).then_some(init);
        }
        let (last, dirs) = segs.split_last()?;
        let mut dir = base.to_path_buf();
        for d in dirs {
            dir = dir.join(d);
        }
        let flat = dir.join(format!("{last}.py"));
        if self.has_file(&flat) {
            return Some(flat);
        }
        let pkg = dir.join(last).join("__init__.py");
        if self.has_file(&pkg) {
            return Some(pkg);
        }
        None
    }

    // --- JS / TS ---

    fn resolve_js(&self, raw: &RawImport, from: &Path) -> Target {
        let spec = raw.module.as_str();
        let is_relative = spec.starts_with("./") || spec.starts_with("../") || spec == "." || spec == "..";
        if !is_relative {
            if spec.starts_with('/') {
                return Target::Unresolved(spec.to_string()); // absolute FS path, rare
            }
            return Target::External(js_package(spec));
        }
        let base = from.parent().unwrap_or(&self.root);
        let joined = normalize(&base.join(spec));
        // Try the path as written, then common extensions, then index files.
        const EXTS: &[&str] = &["ts", "tsx", "d.ts", "js", "jsx", "mjs", "cjs", "json"];
        if self.has_file(&joined) {
            return Target::Internal(joined);
        }
        for ext in EXTS {
            let cand = with_added_ext(&joined, ext);
            if self.has_file(&cand) {
                return Target::Internal(cand);
            }
        }
        for ext in EXTS {
            let cand = joined.join(format!("index.{ext}"));
            if self.has_file(&cand) {
                return Target::Internal(cand);
            }
        }
        Target::Unresolved(spec.to_string())
    }

    // --- Go ---

    fn resolve_go(&self, raw: &RawImport) -> Target {
        let path = raw.module.as_str();
        if let Some(prefix) = &self.go_module
            && let Some(rest) = path.strip_prefix(prefix.as_str())
        {
            let rest = rest.trim_start_matches('/');
            let dir = if rest.is_empty() {
                self.root.clone()
            } else {
                self.root.join(rest)
            };
            // A Go package is a directory of files; point the edge at it.
            if self.dirs.contains(&dir) {
                return Target::Internal(dir);
            }
            return Target::Unresolved(path.to_string());
        }
        // Standard library / third-party.
        Target::External(path.to_string())
    }
}

/// First path segment of a JS bare specifier, keeping an `@scope/name` together.
fn js_package(spec: &str) -> String {
    if let Some(rest) = spec.strip_prefix('@') {
        let mut it = rest.splitn(3, '/');
        let scope = it.next().unwrap_or("");
        let name = it.next().unwrap_or("");
        return format!("@{scope}/{name}");
    }
    spec.split('/').next().unwrap_or(spec).to_string()
}

/// Lexically normalize a path, resolving `.` and `..` without touching disk.
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Append `.ext` to a path's file name (so `foo` → `foo.ts`, keeping any dots).
fn with_added_ext(p: &Path, ext: &str) -> PathBuf {
    let mut name = p.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".");
    name.push(ext);
    p.with_file_name(name)
}

fn read_go_module(root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(root.join("go.mod")).ok()?;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("module ") {
            return Some(rest.trim().to_string());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// The graph
// ---------------------------------------------------------------------------

/// Whole-project import graph. Out-edges (resolved from each file's raw imports)
/// are the source of truth; the reverse index over internal edges is derived.
#[derive(Debug, Default, Clone)]
pub struct ImportGraph {
    /// Cached extraction per file — kept so a structural change can re-resolve
    /// without re-reading files.
    raw: HashMap<PathBuf, Vec<RawImport>>,
    out: HashMap<PathBuf, Vec<Edge>>,
    /// Internal target file → files that import it.
    rev: HashMap<PathBuf, Vec<PathBuf>>,
}

impl ImportGraph {
    /// Rebuild the whole graph from every file's raw imports and a resolver.
    pub fn build(raw: HashMap<PathBuf, Vec<RawImport>>, resolver: &Resolver, lang_of: impl Fn(&Path) -> Option<&'static str>) -> Self {
        let mut g = ImportGraph {
            raw,
            out: HashMap::new(),
            rev: HashMap::new(),
        };
        g.resolve_all(resolver, &lang_of);
        g
    }

    /// Re-resolve every file's edges in place (after a structural change to the
    /// file set). Cheap: pure in-memory string/path work, no file reads.
    pub fn reresolve(&mut self, resolver: &Resolver, lang_of: impl Fn(&Path) -> Option<&'static str>) {
        self.resolve_all(resolver, &lang_of);
    }

    fn resolve_all(&mut self, resolver: &Resolver, lang_of: &impl Fn(&Path) -> Option<&'static str>) {
        self.out.clear();
        self.rev.clear();
        // Collect first to avoid borrowing `self.raw` while mutating `self.out`.
        let files: Vec<PathBuf> = self.raw.keys().cloned().collect();
        for file in files {
            let edges = self.resolve_file(&file, resolver, lang_of);
            self.install(file, edges);
        }
    }

    fn resolve_file(
        &self,
        file: &Path,
        resolver: &Resolver,
        lang_of: &impl Fn(&Path) -> Option<&'static str>,
    ) -> Vec<Edge> {
        let Some(lang) = lang_of(file) else {
            return Vec::new();
        };
        let raws = self.raw.get(file).map(Vec::as_slice).unwrap_or(&[]);
        raws.iter()
            .map(|r| Edge {
                target: resolver.resolve(r, file, lang),
                specifier: r.module.clone(),
                line: r.line,
            })
            .collect()
    }

    /// Insert/replace `file`'s edges and update the reverse index.
    fn install(&mut self, file: PathBuf, edges: Vec<Edge>) {
        self.retract(&file);
        for e in &edges {
            if let Target::Internal(t) = &e.target {
                self.rev.entry(t.clone()).or_default().push(file.clone());
            }
        }
        self.out.insert(file, edges);
    }

    /// Remove `file`'s contribution to the reverse index (before re-inserting).
    fn retract(&mut self, file: &Path) {
        if let Some(old) = self.out.get(file) {
            for e in old {
                if let Target::Internal(t) = &e.target
                    && let Some(importers) = self.rev.get_mut(t)
                {
                    importers.retain(|p| p != file);
                    if importers.is_empty() {
                        self.rev.remove(t);
                    }
                }
            }
        }
    }

    /// Replace one file's raw imports and re-resolve just its out-edges (a pure
    /// content change — no file added or removed). Returns whether its edges
    /// changed. For structural changes call [`ImportGraph::reresolve`] after.
    pub fn set_file(
        &mut self,
        file: PathBuf,
        raw: Vec<RawImport>,
        resolver: &Resolver,
        lang_of: impl Fn(&Path) -> Option<&'static str>,
    ) -> bool {
        self.raw.insert(file.clone(), raw);
        let edges = self.resolve_file(&file, resolver, &lang_of);
        let changed = self.out.get(&file) != Some(&edges);
        self.install(file, edges);
        changed
    }

    /// Forget a deleted file (removes both its out-edges and its reverse
    /// contributions). Edges *pointing to* it become stale — call
    /// [`ImportGraph::reresolve`] to turn them into `Unresolved`.
    pub fn remove_file(&mut self, file: &Path) {
        self.retract(file);
        self.out.remove(file);
        self.raw.remove(file);
    }

    /// Files this file imports (internal + external + unresolved), in source order.
    pub fn imports(&self, file: &Path) -> &[Edge] {
        self.out.get(file).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Files that import this file (internal edges only), sorted for stable display.
    pub fn importers(&self, file: &Path) -> Vec<PathBuf> {
        let mut v = self.rev.get(file).cloned().unwrap_or_default();
        v.sort();
        v.dedup();
        v
    }

    pub fn is_empty(&self) -> bool {
        self.out.values().all(|e| e.is_empty())
    }

    pub fn file_count(&self) -> usize {
        self.raw.len()
    }

    /// Every project-internal file that appears in the graph (as a source of
    /// imports or a target of one), sorted for stable display.
    pub fn files(&self) -> Vec<PathBuf> {
        let mut set: HashSet<PathBuf> = self.out.keys().cloned().collect();
        set.extend(self.rev.keys().cloned());
        let mut v: Vec<PathBuf> = set.into_iter().collect();
        v.sort();
        v
    }

    /// Number of distinct internal file→file edges.
    pub fn internal_edge_count(&self) -> usize {
        self.out
            .values()
            .flat_map(|edges| edges.iter())
            .filter(|e| matches!(e.target, Target::Internal(_)))
            .count()
    }

    /// Unique external packages depended on anywhere, sorted.
    pub fn external_packages(&self) -> Vec<String> {
        let mut set: HashSet<&str> = HashSet::new();
        for edges in self.out.values() {
            for e in edges {
                if let Target::External(name) = &e.target {
                    set.insert(name.as_str());
                }
            }
        }
        let mut v: Vec<String> = set.into_iter().map(str::to_string).collect();
        v.sort();
        v
    }

    /// Number of internal files that import `file` (its fan-in).
    pub fn fan_in(&self, file: &Path) -> usize {
        self.rev.get(file).map(|v| v.len()).unwrap_or(0)
    }

    /// Number of distinct internal files `file` imports (its fan-out).
    pub fn fan_out(&self, file: &Path) -> usize {
        self.out
            .get(file)
            .map(|edges| {
                let mut set: HashSet<&Path> = HashSet::new();
                for e in edges {
                    if let Target::Internal(t) = &e.target {
                        set.insert(t.as_path());
                    }
                }
                set.len()
            })
            .unwrap_or(0)
    }

    /// Import cycles among project-internal files (each a strongly connected
    /// component of size > 1, or a self-import), via Tarjan's algorithm.
    pub fn cycles(&self) -> Vec<Vec<PathBuf>> {
        Tarjan::new(self).run()
    }
}

/// Tarjan's strongly-connected-components over the internal edges, for cycle
/// detection. Iterative would be sturdier on pathological depths, but project
/// import graphs are shallow; recursion is clearer here.
struct Tarjan<'a> {
    graph: &'a ImportGraph,
    index: HashMap<PathBuf, usize>,
    low: HashMap<PathBuf, usize>,
    on_stack: HashSet<PathBuf>,
    stack: Vec<PathBuf>,
    next: usize,
    out: Vec<Vec<PathBuf>>,
}

impl<'a> Tarjan<'a> {
    fn new(graph: &'a ImportGraph) -> Self {
        Tarjan {
            graph,
            index: HashMap::new(),
            low: HashMap::new(),
            on_stack: HashSet::new(),
            stack: Vec::new(),
            next: 0,
            out: Vec::new(),
        }
    }

    fn run(mut self) -> Vec<Vec<PathBuf>> {
        let mut nodes: Vec<PathBuf> = self.graph.out.keys().cloned().collect();
        nodes.sort();
        for n in nodes {
            if !self.index.contains_key(&n) {
                self.connect(&n);
            }
        }
        self.out
    }

    fn internal_targets(&self, node: &Path) -> Vec<PathBuf> {
        self.graph
            .out
            .get(node)
            .map(|edges| {
                edges
                    .iter()
                    .filter_map(|e| match &e.target {
                        Target::Internal(p) => Some(p.clone()),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn connect(&mut self, v: &Path) {
        let v = v.to_path_buf();
        self.index.insert(v.clone(), self.next);
        self.low.insert(v.clone(), self.next);
        self.next += 1;
        self.stack.push(v.clone());
        self.on_stack.insert(v.clone());

        for w in self.internal_targets(&v) {
            if !self.index.contains_key(&w) {
                self.connect(&w);
                let lw = self.low[&w];
                let lv = self.low[&v];
                self.low.insert(v.clone(), lv.min(lw));
            } else if self.on_stack.contains(&w) {
                let iw = self.index[&w];
                let lv = self.low[&v];
                self.low.insert(v.clone(), lv.min(iw));
            }
        }

        if self.low[&v] == self.index[&v] {
            let mut comp = Vec::new();
            while let Some(w) = self.stack.pop() {
                self.on_stack.remove(&w);
                comp.push(w.clone());
                if w == v {
                    break;
                }
            }
            // Report only real cycles: a multi-node SCC, or a self-import.
            let self_loop = self.internal_targets(&v).iter().any(|t| t == &v);
            if comp.len() > 1 || self_loop {
                comp.sort();
                self.out.push(comp);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The view tree (synchronous; expands straight from the in-memory graph)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    /// What this file imports.
    Imports,
    /// What imports this file.
    Importers,
}

impl Dir {
    pub fn label(self) -> &'static str {
        match self {
            Dir::Imports => "Imports",
            Dir::Importers => "Importers",
        }
    }

    pub fn toggled(self) -> Dir {
        match self {
            Dir::Imports => Dir::Importers,
            Dir::Importers => Dir::Imports,
        }
    }
}

/// A node in the import view tree.
#[derive(Debug, Clone)]
pub struct INode {
    pub target: Target,
    /// Display label (file name, external package, or unresolved specifier).
    pub label: String,
    /// Secondary text (relative path for internal, "external"/specifier otherwise).
    pub detail: String,
    /// The import site line in the *parent* file, for navigation (0 for the root).
    pub line: usize,
    pub depth: usize,
    pub parent: Option<usize>,
    pub children: Option<Vec<usize>>,
    pub expanded: bool,
    /// The target already appears on the ancestor path — a cyclic leaf.
    pub cyclic: bool,
}

/// A lazily-expanded view of the import graph around one focus file. Unlike the
/// call tree it expands synchronously (the whole graph is already in memory).
#[derive(Debug, Clone)]
pub struct ImportTree {
    pub direction: Dir,
    pub root_path: PathBuf,
    pub root_name: String,
    pub full: bool,
    nodes: Vec<INode>,
    roots: Vec<usize>,
}

/// Cap on tree size so "expand all" on a hub file can't fan out unboundedly.
pub const MAX_NODES: usize = 2000;

impl ImportTree {
    /// Build a tree rooted at `focus`, expanded one level.
    pub fn new(graph: &ImportGraph, root: &Path, focus: PathBuf, direction: Dir) -> Self {
        let root_name = rel_name(root, &focus);
        let mut tree = ImportTree {
            direction,
            root_path: focus.clone(),
            root_name,
            full: false,
            nodes: Vec::new(),
            roots: Vec::new(),
        };
        let id = tree.push(
            INode {
                target: Target::Internal(focus.clone()),
                label: rel_name(root, &focus),
                detail: rel_path(root, &focus),
                line: 0,
                depth: 0,
                parent: None,
                children: None,
                expanded: false,
                cyclic: false,
            },
        );
        tree.roots.push(id);
        tree.expand(id, graph, root);
        tree
    }

    fn push(&mut self, node: INode) -> usize {
        let id = self.nodes.len();
        self.nodes.push(node);
        id
    }

    pub fn roots(&self) -> &[usize] {
        &self.roots
    }

    pub fn node(&self, id: usize) -> &INode {
        &self.nodes[id]
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Whether `id` can be expanded to reveal children (internal, not yet
    /// expanded, not cyclic).
    pub fn expandable(&self, id: usize) -> bool {
        let n = &self.nodes[id];
        matches!(n.target, Target::Internal(_)) && !n.cyclic
    }

    fn is_ancestor(&self, mut parent: Option<usize>, path: &Path) -> bool {
        while let Some(p) = parent {
            let n = &self.nodes[p];
            if matches!(&n.target, Target::Internal(t) if t == path) {
                return true;
            }
            parent = n.parent;
        }
        false
    }

    /// Expand `id` in place, pulling its children from the graph. Idempotent.
    pub fn expand(&mut self, id: usize, graph: &ImportGraph, root: &Path) {
        if self.nodes[id].children.is_some() {
            self.nodes[id].expanded = true;
            return;
        }
        let Target::Internal(path) = self.nodes[id].target.clone() else {
            self.nodes[id].children = Some(Vec::new());
            return;
        };
        let depth = self.nodes[id].depth + 1;
        let children = self.children_of(&path, graph, root, id, depth);
        let ids: Vec<usize> = children.into_iter().map(|c| self.push(c)).collect();
        self.nodes[id].children = Some(ids);
        self.nodes[id].expanded = true;
    }

    /// Build (but don't insert) the child nodes for internal file `path`.
    fn children_of(
        &self,
        path: &Path,
        graph: &ImportGraph,
        root: &Path,
        parent: usize,
        depth: usize,
    ) -> Vec<INode> {
        let mut out = Vec::new();
        match self.direction {
            Dir::Imports => {
                let mut seen_ext: HashSet<String> = HashSet::new();
                let mut seen_int: HashSet<PathBuf> = HashSet::new();
                for e in graph.imports(path) {
                    match &e.target {
                        Target::Internal(t) => {
                            if !seen_int.insert(t.clone()) {
                                continue; // dedup repeated imports of the same file
                            }
                            let cyclic = self.is_ancestor(Some(parent), t);
                            out.push(INode {
                                target: Target::Internal(t.clone()),
                                label: rel_name(root, t),
                                detail: rel_path(root, t),
                                line: e.line,
                                depth,
                                parent: Some(parent),
                                children: None,
                                expanded: false,
                                cyclic,
                            });
                        }
                        Target::External(name) => {
                            if seen_ext.insert(name.clone()) {
                                out.push(leaf(e, name.clone(), "external", depth, parent));
                            }
                        }
                        Target::Unresolved(spec) => {
                            if seen_ext.insert(format!("?{spec}")) {
                                out.push(leaf(e, spec.clone(), "unresolved", depth, parent));
                            }
                        }
                    }
                }
            }
            Dir::Importers => {
                for t in graph.importers(path) {
                    let cyclic = self.is_ancestor(Some(parent), &t);
                    out.push(INode {
                        target: Target::Internal(t.clone()),
                        label: rel_name(root, &t),
                        detail: rel_path(root, &t),
                        line: 0,
                        depth,
                        parent: Some(parent),
                        children: None,
                        expanded: false,
                        cyclic,
                    });
                }
            }
        }
        out
    }

    /// Collapse/expand a node whose children are already loaded, else expand it.
    pub fn toggle(&mut self, id: usize, graph: &ImportGraph, root: &Path) {
        if self.nodes[id].children.is_some() {
            self.nodes[id].expanded = !self.nodes[id].expanded;
        } else {
            self.expand(id, graph, root);
        }
    }

    /// Recursively expand every internal node up to the node cap.
    pub fn expand_all(&mut self, graph: &ImportGraph, root: &Path) {
        self.full = true;
        let mut i = 0;
        while i < self.nodes.len() {
            if self.nodes.len() >= MAX_NODES {
                break;
            }
            if self.expandable(i) && self.nodes[i].children.is_none() {
                self.expand(i, graph, root);
            }
            i += 1;
        }
    }

    /// Node ids in display order (depth-first, children only under expanded nodes).
    pub fn visible(&self) -> Vec<usize> {
        let mut out = Vec::new();
        for &r in &self.roots {
            self.walk(r, &mut out);
        }
        out
    }

    fn walk(&self, id: usize, out: &mut Vec<usize>) {
        out.push(id);
        let n = &self.nodes[id];
        if n.expanded && let Some(children) = &n.children {
            for &c in children {
                self.walk(c, out);
            }
        }
    }
}

fn leaf(e: &Edge, label: String, detail: &str, depth: usize, parent: usize) -> INode {
    INode {
        target: e.target.clone(),
        label,
        detail: detail.to_string(),
        line: e.line,
        depth,
        parent: Some(parent),
        children: Some(Vec::new()),
        expanded: false,
        cyclic: false,
    }
}

/// File name for display (`client.rs`).
fn rel_name(_root: &Path, path: &Path) -> String {
    path.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string()
}

/// Path relative to the project root for the secondary label (`src/lsp`).
fn rel_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| ".".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ri(module: &str) -> RawImport {
        RawImport { module: module.into(), line: 1, is_mod_decl: false }
    }

    // --- extraction ---

    #[test]
    fn extracts_rust_mod_and_use() {
        let src = "mod alpha;\nmod inline { fn x() {} }\nuse crate::beta::Thing;\nuse std::collections::HashMap;\nuse super::gamma::{a, b};\n";
        let imports = imports_of(src, "rust");
        let mods: Vec<&RawImport> = imports.iter().filter(|i| i.is_mod_decl).collect();
        assert_eq!(mods.len(), 1, "only the file-mod, not the inline mod: {imports:?}");
        assert_eq!(mods[0].module, "alpha");
        let uses: Vec<&str> = imports.iter().filter(|i| !i.is_mod_decl).map(|i| i.module.as_str()).collect();
        assert!(uses.contains(&"crate::beta::Thing"), "{uses:?}");
        assert!(uses.contains(&"std::collections::HashMap"), "{uses:?}");
        assert!(uses.contains(&"super::gamma"), "grouped import collapses to module prefix: {uses:?}");
    }

    #[test]
    fn extracts_python_imports() {
        let src = "import os\nimport a.b.c as x\nfrom .sibling import thing\nfrom ..pkg import other\nfrom mod import y\n";
        let extracted = imports_of(src, "python");
        let mods: Vec<&str> = extracted.iter().map(|i| i.module.as_str()).collect();
        assert!(mods.contains(&"os"), "{mods:?}");
        assert!(mods.contains(&"a.b.c"), "{mods:?}");
        assert!(mods.contains(&".sibling"), "{mods:?}");
        assert!(mods.contains(&"..pkg"), "{mods:?}");
        assert!(mods.contains(&"mod"), "{mods:?}");
    }

    #[test]
    fn extracts_js_imports_and_reexports() {
        let src = "import a from './a';\nimport {b} from '../lib/b';\nexport {c} from './c';\nimport 'react';\n";
        let extracted = imports_of(src, "typescript");
        let mods: Vec<&str> = extracted.iter().map(|i| i.module.as_str()).collect();
        assert!(mods.contains(&"./a"), "{mods:?}");
        assert!(mods.contains(&"../lib/b"), "{mods:?}");
        assert!(mods.contains(&"./c"), "re-export source: {mods:?}");
        assert!(mods.contains(&"react"), "{mods:?}");
    }

    // --- resolution ---

    fn resolver(root: &Path, files: &[&str]) -> Resolver {
        let paths: Vec<PathBuf> = files.iter().map(|f| root.join(f)).collect();
        Resolver::new(root, &paths)
    }

    #[test]
    fn resolves_rust_mod_and_use_paths() {
        let root = Path::new("/proj");
        let r = resolver(root, &["src/main.rs", "src/beta.rs", "src/lsp/mod.rs", "src/lsp/client.rs"]);

        // `mod beta;` in main.rs → src/beta.rs
        let mod_decl = RawImport { module: "beta".into(), line: 1, is_mod_decl: true };
        assert_eq!(
            r.resolve(&mod_decl, &root.join("src/main.rs"), "rust"),
            Target::Internal(root.join("src/beta.rs"))
        );

        // `use crate::lsp::client::CallItem;` → drops the item, resolves the module file.
        assert_eq!(
            r.resolve(&ri("crate::lsp::client::CallItem"), &root.join("src/main.rs"), "rust"),
            Target::Internal(root.join("src/lsp/client.rs"))
        );

        // `use crate::lsp::X` → the module dir's mod.rs.
        assert_eq!(
            r.resolve(&ri("crate::lsp::Something"), &root.join("src/main.rs"), "rust"),
            Target::Internal(root.join("src/lsp/mod.rs"))
        );

        // External crate.
        assert_eq!(
            r.resolve(&ri("serde::Deserialize"), &root.join("src/main.rs"), "rust"),
            Target::External("serde".into())
        );
        // Std is external too.
        assert_eq!(
            r.resolve(&ri("std::collections::HashMap"), &root.join("src/main.rs"), "rust"),
            Target::External("std".into())
        );
    }

    #[test]
    fn resolves_rust_super_relative() {
        let root = Path::new("/proj");
        let r = resolver(root, &["src/main.rs", "src/lsp/mod.rs", "src/lsp/client.rs", "src/lsp/config.rs"]);
        // From client.rs, `use super::config::X` → sibling config.rs.
        assert_eq!(
            r.resolve(&ri("super::config::Setting"), &root.join("src/lsp/client.rs"), "rust"),
            Target::Internal(root.join("src/lsp/config.rs"))
        );
    }

    #[test]
    fn resolves_python_relative_and_absolute() {
        let root = Path::new("/proj");
        let r = resolver(root, &["pkg/__init__.py", "pkg/mod_a.py", "pkg/sub/__init__.py", "top.py"]);

        // `from .mod_a import x` inside pkg/__init__.py → pkg/mod_a.py
        let rel = RawImport { module: ".mod_a".into(), line: 1, is_mod_decl: false };
        assert_eq!(
            r.resolve(&rel, &root.join("pkg/__init__.py"), "python"),
            Target::Internal(root.join("pkg/mod_a.py"))
        );

        // `import top` → top.py at the project root.
        assert_eq!(
            r.resolve(&ri("top"), &root.join("pkg/mod_a.py"), "python"),
            Target::Internal(root.join("top.py"))
        );

        // `import numpy` → external.
        assert_eq!(
            r.resolve(&ri("numpy"), &root.join("top.py"), "python"),
            Target::External("numpy".into())
        );
    }

    #[test]
    fn resolves_js_relative_with_extensions_and_index() {
        let root = Path::new("/proj");
        let r = resolver(root, &["src/app.ts", "src/util.ts", "src/widget/index.tsx"]);

        // './util' → util.ts (extension added)
        assert_eq!(
            r.resolve(&ri("./util"), &root.join("src/app.ts"), "typescript"),
            Target::Internal(root.join("src/util.ts"))
        );
        // './widget' → widget/index.tsx (index file)
        assert_eq!(
            r.resolve(&ri("./widget"), &root.join("src/app.ts"), "typescript"),
            Target::Internal(root.join("src/widget/index.tsx"))
        );
        // 'react' → external; '@scope/pkg' keeps the scope.
        assert_eq!(r.resolve(&ri("react"), &root.join("src/app.ts"), "typescript"), Target::External("react".into()));
        assert_eq!(
            r.resolve(&ri("@scope/pkg/sub"), &root.join("src/app.ts"), "typescript"),
            Target::External("@scope/pkg".into())
        );
    }

    // --- graph ---

    fn lang_of(p: &Path) -> Option<&'static str> {
        crate::highlight::detect(p)
    }

    #[test]
    fn builds_graph_with_forward_and_reverse_edges() {
        let root = Path::new("/proj");
        let files = ["src/main.rs", "src/a.rs", "src/b.rs"];
        let r = resolver(root, &files);
        let mut raw = HashMap::new();
        // main imports a and b; a imports b.
        raw.insert(root.join("src/main.rs"), vec![
            RawImport { module: "a".into(), line: 1, is_mod_decl: true },
            RawImport { module: "b".into(), line: 2, is_mod_decl: true },
        ]);
        raw.insert(root.join("src/a.rs"), vec![ri("crate::b::Thing")]);
        raw.insert(root.join("src/b.rs"), vec![]);

        let g = ImportGraph::build(raw, &r, lang_of);
        // b is imported by main and a.
        assert_eq!(g.importers(&root.join("src/b.rs")), vec![root.join("src/a.rs"), root.join("src/main.rs")]);
        // main imports two internal files.
        let internal: Vec<_> = g.imports(&root.join("src/main.rs")).iter()
            .filter(|e| matches!(e.target, Target::Internal(_))).collect();
        assert_eq!(internal.len(), 2);
    }

    #[test]
    fn set_file_updates_reverse_index() {
        let root = Path::new("/proj");
        let files = ["src/main.rs", "src/a.rs", "src/b.rs"];
        let r = resolver(root, &files);
        let mut raw = HashMap::new();
        raw.insert(root.join("src/main.rs"), vec![RawImport { module: "a".into(), line: 1, is_mod_decl: true }]);
        raw.insert(root.join("src/a.rs"), vec![]);
        raw.insert(root.join("src/b.rs"), vec![]);
        let mut g = ImportGraph::build(raw, &r, lang_of);
        assert_eq!(g.importers(&root.join("src/a.rs")), vec![root.join("src/main.rs")]);
        assert!(g.importers(&root.join("src/b.rs")).is_empty());

        // main now imports b instead of a.
        let changed = g.set_file(
            root.join("src/main.rs"),
            vec![RawImport { module: "b".into(), line: 1, is_mod_decl: true }],
            &r,
            lang_of,
        );
        assert!(changed);
        assert!(g.importers(&root.join("src/a.rs")).is_empty(), "a's importer was retracted");
        assert_eq!(g.importers(&root.join("src/b.rs")), vec![root.join("src/main.rs")]);
    }

    #[test]
    fn detects_import_cycles() {
        let root = Path::new("/proj");
        let files = ["src/lib.rs", "src/a.rs", "src/b.rs", "src/c.rs"];
        let r = resolver(root, &files);
        let mut raw = HashMap::new();
        // a → b → a is a cycle; c is standalone.
        raw.insert(root.join("src/a.rs"), vec![ri("crate::b::X")]);
        raw.insert(root.join("src/b.rs"), vec![ri("crate::a::Y")]);
        raw.insert(root.join("src/c.rs"), vec![]);
        let g = ImportGraph::build(raw, &r, lang_of);
        let cycles = g.cycles();
        assert_eq!(cycles.len(), 1, "{cycles:?}");
        assert_eq!(cycles[0], vec![root.join("src/a.rs"), root.join("src/b.rs")]);
    }

    // --- tree ---

    #[test]
    fn tree_expands_imports_and_marks_cycles() {
        let root = Path::new("/proj");
        let files = ["src/lib.rs", "src/a.rs", "src/b.rs"];
        let r = resolver(root, &files);
        let mut raw = HashMap::new();
        raw.insert(root.join("src/a.rs"), vec![ri("crate::b::X")]);
        raw.insert(root.join("src/b.rs"), vec![ri("crate::a::Y")]);
        let g = ImportGraph::build(raw, &r, lang_of);

        let mut tree = ImportTree::new(&g, root, root.join("src/a.rs"), Dir::Imports);
        // root a, expanded once, shows b.
        let vis = tree.visible();
        assert_eq!(vis.len(), 2);
        assert_eq!(tree.node(vis[1]).label, "b.rs");

        // Expanding b reveals a again — but it's on the ancestor path → cyclic leaf.
        let b_id = vis[1];
        tree.expand(b_id, &g, root);
        let vis = tree.visible();
        let a_again = *vis.last().unwrap();
        assert_eq!(tree.node(a_again).label, "a.rs");
        assert!(tree.node(a_again).cyclic);
    }

    #[test]
    fn tree_importers_direction() {
        let root = Path::new("/proj");
        let files = ["src/main.rs", "src/util.rs"];
        let r = resolver(root, &files);
        let mut raw = HashMap::new();
        raw.insert(root.join("src/main.rs"), vec![RawImport { module: "util".into(), line: 1, is_mod_decl: true }]);
        raw.insert(root.join("src/util.rs"), vec![]);
        let g = ImportGraph::build(raw, &r, lang_of);

        // Who imports util? → main.
        let tree = ImportTree::new(&g, root, root.join("src/util.rs"), Dir::Importers);
        let vis = tree.visible();
        assert_eq!(vis.len(), 2);
        assert_eq!(tree.node(vis[1]).label, "main.rs");
    }
}
