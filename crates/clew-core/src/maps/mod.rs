//! The project's maps: what a language or a framework says about how the
//! code fits together, read from its files — a database's tables and their
//! foreign keys, a web app's routes and the handlers they reach, an OTP
//! application's supervision tree, a Go type and the interfaces it
//! satisfies without saying so.
//!
//! Each map is a [`ProjectMap`]: typed nodes, each where it is defined, and
//! labelled arrows between them. The extractors read what the code says,
//! statically, the way a reader would — a route registered at run time from
//! data, a table created by an ORM, a process started by name from a
//! config file is not seen. A map that finds nothing is not shown.
//!
//! Lives in clew-core so it builds where the files are: the client calls
//! [`build`] for a local project; clew-server answers `ProjectMaps` for a
//! remote one.

use std::collections::HashMap;
use std::path::Path;

use clew_protocol::{MapEdge, MapNode, ProjectMap};
use tree_sitter::{Node, Tree};

use crate::fs_scan::FileEntry;

pub mod cfamily;
pub mod dart;
pub mod defs;
pub mod erlang;
pub mod frameworks;
pub mod go;
pub mod haskell;
pub mod js;
pub mod lean;
pub mod python;
pub mod routes;
pub mod ruby;
pub mod rust;
pub mod sql;
pub mod zig;
use crate::highlight::{self, Lang};

/// The largest file read for a map.
const MAX_FILE_BYTES: u64 = 2 << 20;

/// The most nodes one map keeps: past it, the map says how many it left out.
pub const MAX_NODES: usize = 4000;

/// A file of the project, read for its maps.
#[derive(Debug, Clone)]
pub struct Source {
    /// Project-relative, `/`-separated.
    pub rel: String,
    /// Its language, when it is one Clew parses.
    pub lang: Option<Lang>,
    pub text: String,
    /// Its parse tree, made once for every map that reads it.
    parsed: std::cell::OnceCell<Option<Tree>>,
}

impl Source {
    pub fn new(rel: &str, lang: Option<Lang>, text: String) -> Source {
        Source {
            rel: rel.to_string(),
            lang,
            text,
            parsed: Default::default(),
        }
    }

    /// The file's parse tree, for its language — parsed once.
    pub fn tree(&self) -> Option<Tree> {
        self.parsed
            .get_or_init(|| highlight::parse(&self.text, self.lang?))
            .clone()
    }

    /// The file's name (its last path component).
    pub fn name(&self) -> &str {
        self.rel.rsplit('/').next().unwrap_or(&self.rel)
    }
}

/// The project's files, read once for every map.
#[derive(Debug, Clone, Default)]
pub struct Corpus {
    pub files: Vec<Source>,
    /// Every definition in the files, indexed once for every map.
    defs: std::cell::OnceCell<defs::Defs>,
}

impl Corpus {
    /// Read the project's text files (each at most [`MAX_FILE_BYTES`]; a
    /// binary one, with a NUL in it, skipped).
    pub fn read(files: &[FileEntry]) -> Corpus {
        let mut out = Vec::new();
        for entry in files {
            let lang = highlight::detect(&entry.abs).and_then(Lang::from_key);
            if lang.is_none() && !wanted_without_language(&entry.rel) {
                continue;
            }
            let Ok(meta) = std::fs::metadata(&entry.abs) else {
                continue;
            };
            if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
                continue;
            }
            let Ok(bytes) = std::fs::read(&entry.abs) else {
                continue;
            };
            if bytes.contains(&0) {
                continue;
            }
            let text = String::from_utf8_lossy(&bytes).into_owned();
            let lang = lang.and_then(|l| Lang::for_source(l.key(), &text));
            out.push(Source::new(&entry.rel.replace('\\', "/"), lang, text));
        }
        out.sort_by(|a, b| a.rel.cmp(&b.rel));
        Corpus {
            files: out,
            defs: Default::default(),
        }
    }

    /// A corpus of in-memory files, for tests and callers that hold the
    /// text already: `(rel, text)`, the language from the name.
    pub fn of(files: &[(&str, &str)]) -> Corpus {
        let mut out: Vec<Source> = files
            .iter()
            .map(|(rel, text)| {
                let lang =
                    highlight::detect(Path::new(rel)).and_then(|key| Lang::for_source(key, text));
                Source::new(rel, lang, text.to_string())
            })
            .collect();
        out.sort_by(|a, b| a.rel.cmp(&b.rel));
        Corpus {
            files: out,
            defs: Default::default(),
        }
    }

    /// Every definition in the corpus — indexed once.
    pub fn defs(&self) -> &defs::Defs {
        self.defs.get_or_init(|| defs::Defs::of(self))
    }

    /// The files in any of `langs`.
    pub fn in_langs<'a>(&'a self, langs: &'a [Lang]) -> impl Iterator<Item = &'a Source> + 'a {
        self.files
            .iter()
            .filter(move |s| s.lang.is_some_and(|l| langs.contains(&l)))
    }

    /// The file at `rel`.
    pub fn get(&self, rel: &str) -> Option<&Source> {
        self.files.iter().find(|s| s.rel == rel)
    }
}

/// Files read though Clew parses no language of theirs: what a map is
/// built from that is not code (a Rails app's routes are Ruby; these are
/// manifests and templates).
fn wanted_without_language(rel: &str) -> bool {
    let name = rel.rsplit(['/', '\\']).next().unwrap_or(rel);
    name.ends_with(".vue")
        || name.ends_with(".svelte")
        || name.ends_with(".cabal")
        || name == "pnpm-workspace.yaml"
        || name == "go.mod"
        || name.ends_with(".lean")
}

/// Every map of `files` (under `root`), none empty.
pub fn build(_root: &Path, files: &[FileEntry]) -> Vec<ProjectMap> {
    build_from(&Corpus::read(files))
}

/// Every map of `corpus`, none empty, in the order they are listed.
pub fn build_from(corpus: &Corpus) -> Vec<ProjectMap> {
    let extractors: &[fn(&Corpus) -> Vec<ProjectMap>] = &[
        sql::maps,
        routes::maps,
        frameworks::maps,
        python::maps,
        go::maps,
        js::maps,
        rust::maps,
        cfamily::maps,
        erlang::maps,
        haskell::maps,
        zig::maps,
        ruby::maps,
        dart::maps,
        lean::maps,
    ];
    extractors
        .iter()
        .flat_map(|extract| extract(corpus))
        .filter(|map| !map.nodes.is_empty())
        .collect()
}

// -- building a map -------------------------------------------------------------

/// A map being built: nodes found by a key (each node once, however often it
/// is met), edges once each.
pub struct MapBuilder {
    map: ProjectMap,
    by_key: HashMap<(String, String), usize>,
    edges: std::collections::HashSet<(usize, usize, String)>,
    /// Nodes left out past [`MAX_NODES`].
    dropped: usize,
}

impl MapBuilder {
    pub fn new(id: &str, title: &str, description: &str) -> MapBuilder {
        MapBuilder {
            map: ProjectMap {
                id: id.into(),
                title: title.into(),
                description: description.into(),
                nodes: Vec::new(),
                edges: Vec::new(),
            },
            by_key: HashMap::new(),
            edges: Default::default(),
            dropped: 0,
        }
    }

    /// The node of `kind` known as `key`, made if new — `label` what it
    /// shows. A node met first only by name (a referenced table) takes the
    /// place it is defined at, when it is met there.
    pub fn node(&mut self, kind: &str, key: &str, label: &str) -> Option<usize> {
        let id = (kind.to_string(), key.to_string());
        if let Some(&at) = self.by_key.get(&id) {
            return Some(at);
        }
        if self.map.nodes.len() >= MAX_NODES {
            self.dropped += 1;
            return None;
        }
        self.map.nodes.push(MapNode {
            label: label.into(),
            kind: kind.into(),
            ..Default::default()
        });
        let at = self.map.nodes.len() - 1;
        self.by_key.insert(id, at);
        Some(at)
    }

    /// The node `at`'s place: `file` and `line`, when it has none yet.
    pub fn place(&mut self, at: Option<usize>, file: &str, line: usize) {
        if let Some(node) = at.and_then(|at| self.map.nodes.get_mut(at))
            && node.file.is_empty()
        {
            node.file = file.into();
            node.line = line;
        }
    }

    /// The node `at`'s detail, replaced.
    pub fn detail(&mut self, at: Option<usize>, detail: impl Into<String>) {
        if let Some(node) = at.and_then(|at| self.map.nodes.get_mut(at)) {
            node.detail = detail.into();
        }
    }

    /// A line added to the node `at`'s detail.
    pub fn add_detail(&mut self, at: Option<usize>, line: &str) {
        if let Some(node) = at.and_then(|at| self.map.nodes.get_mut(at)) {
            if !node.detail.is_empty() {
                node.detail.push('\n');
            }
            node.detail.push_str(line);
        }
    }

    /// The node `at`, as it is now.
    pub fn get(&self, at: usize) -> Option<&MapNode> {
        self.map.nodes.get(at)
    }

    /// An arrow `from` → `to`, once.
    pub fn edge(&mut self, from: Option<usize>, to: Option<usize>, label: &str) {
        let (Some(from), Some(to)) = (from, to) else {
            return;
        };
        if self.edges.insert((from, to, label.to_string())) {
            self.map.edges.push(MapEdge {
                from,
                to,
                label: label.into(),
            });
        }
    }

    pub fn is_empty(&self) -> bool {
        self.map.nodes.is_empty()
    }

    /// The map — none when it found nothing. Nodes are left in the order
    /// they were found.
    pub fn finish(mut self) -> Option<ProjectMap> {
        if self.map.nodes.is_empty() {
            return None;
        }
        if self.dropped > 0 {
            self.map.description = format!(
                "{} — the first {MAX_NODES} shown of {}",
                self.map.description,
                MAX_NODES + self.dropped
            );
        }
        Some(self.map)
    }
}

// -- reading code ----------------------------------------------------------------

/// The text of `node` in `source`.
pub fn text<'a>(node: Node<'_>, source: &'a str) -> &'a str {
    source.get(node.byte_range()).unwrap_or("")
}

/// The 1-based line `node` starts on.
pub fn line(node: Node<'_>) -> usize {
    node.start_position().row + 1
}

/// The 1-based line of byte `offset` in `source`.
pub fn line_at(source: &str, offset: usize) -> usize {
    source
        .get(..offset.min(source.len()))
        .map_or(0, |s| s.bytes().filter(|b| *b == b'\n').count())
        + 1
}

/// Every node under `root` (itself included), depth first, in source order.
pub fn walk(root: Node<'_>, mut each: impl FnMut(Node<'_>)) {
    let mut cursor = root.walk();
    loop {
        each(cursor.node());
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() || cursor.node() == root {
                return;
            }
        }
    }
}

/// The nodes of `kind` under `root`.
pub fn all_of<'t>(root: Node<'t>, kind: &str) -> Vec<Node<'t>> {
    let mut out = Vec::new();
    let mut cursor = root.walk();
    loop {
        let node = cursor.node();
        if node.kind() == kind {
            out.push(node);
        }
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() || cursor.node() == root {
                return out;
            }
        }
    }
}

/// The first ancestor of `node` of any of `kinds`.
pub fn ancestor<'t>(node: Node<'t>, kinds: &[&str]) -> Option<Node<'t>> {
    let mut at = node.parent();
    while let Some(n) = at {
        if kinds.contains(&n.kind()) {
            return Some(n);
        }
        at = n.parent();
    }
    None
}

/// A string literal's text without its quotes (and a Python/JS prefix),
/// escapes left as written; `None` for what is not a plain literal.
pub fn unquote(literal: &str) -> Option<String> {
    let s = literal.trim();
    let s = s.trim_start_matches(['r', 'b', 'u', 'R', 'B', 'U', 'f', 'F', '@']);
    for quote in ["\"\"\"", "'''", "\"", "'", "`"] {
        if let Some(inner) = s.strip_prefix(quote).and_then(|r| r.strip_suffix(quote))
            && !(quote == "`" && inner.contains("${"))
        {
            return Some(inner.to_string());
        }
    }
    None
}

/// The last name of a qualified one (`a.b::C` → `C`).
pub fn last_name(reference: &str) -> String {
    reference
        .rsplit(['.', ':', '\\', '/'])
        .next()
        .unwrap_or(reference)
        .trim()
        .to_string()
}

/// A word of identifier characters and dashes (a kebab-case tag), at the
/// front of `s`.
pub fn ident_dash(s: &str) -> &str {
    let end = s
        .char_indices()
        .find(|(_, c)| !(c.is_alphanumeric() || *c == '_' || *c == '-'))
        .map_or(s.len(), |(i, _)| i);
    &s[..end]
}

/// A word made of identifier characters, starting at the front of `s`.
pub fn ident(s: &str) -> &str {
    let end = s
        .char_indices()
        .find(|(_, c)| !(c.is_alphanumeric() || *c == '_'))
        .map_or(s.len(), |(i, _)| i);
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_map_holds_each_node_and_edge_once() {
        let mut map = MapBuilder::new("t", "Test", "how to read it");
        let users = map.node("table", "users", "users");
        let orders = map.node("table", "orders", "orders");
        assert_eq!(
            map.node("table", "users", "users"),
            users,
            "found again by key"
        );
        let other = map.node("view", "users", "users");
        assert_ne!(other, users, "a key is per kind");
        map.place(users, "db.sql", 3);
        map.place(users, "other.sql", 9);
        map.edge(orders, users, "user_id");
        map.edge(orders, users, "user_id");
        map.edge(orders, users, "buyer_id");
        map.edge(orders, None, "nowhere");
        map.add_detail(users, "id");
        map.add_detail(users, "name");
        let map = map.finish().unwrap();
        assert_eq!(map.nodes.len(), 3);
        assert_eq!(
            (map.nodes[0].file.as_str(), map.nodes[0].line),
            ("db.sql", 3),
            "the first place stays"
        );
        assert_eq!(map.nodes[0].detail, "id\nname");
        assert_eq!(map.edges.len(), 2);
        assert!(map.is_well_formed());
        assert!(MapBuilder::new("e", "Empty", "").finish().is_none());
    }

    #[test]
    fn a_map_past_its_cap_says_how_much_it_left_out() {
        let mut map = MapBuilder::new("big", "Big", "nodes");
        for i in 0..MAX_NODES + 5 {
            map.node("n", &i.to_string(), "n");
        }
        let map = map.finish().unwrap();
        assert_eq!(map.nodes.len(), MAX_NODES);
        assert!(
            map.description.contains(&format!("of {}", MAX_NODES + 5)),
            "{}",
            map.description
        );
    }

    #[test]
    fn the_corpus_reads_text_files_and_leaves_the_rest() {
        let dir = crate::testutil::TempDir::new("maps-corpus");
        std::fs::write(dir.join("a.sql"), "create table t (id int);").unwrap();
        std::fs::write(dir.join("b.bin"), b"\x00\x01").unwrap();
        std::fs::write(dir.join("c.py"), "x = 1\x00").unwrap();
        std::fs::write(dir.join("App.vue"), "<template><Child/></template>").unwrap();
        std::fs::write(dir.join("notes.txt"), "plain").unwrap();
        let entries: Vec<FileEntry> = ["a.sql", "b.bin", "c.py", "App.vue", "notes.txt", "gone.rs"]
            .iter()
            .map(|rel| FileEntry {
                abs: dir.join(rel),
                rel: rel.to_string(),
            })
            .collect();
        let corpus = Corpus::read(&entries);
        let rels: Vec<&str> = corpus.files.iter().map(|s| s.rel.as_str()).collect();
        assert_eq!(rels, ["App.vue", "a.sql"]);
        assert_eq!(corpus.get("a.sql").unwrap().lang, Some(Lang::Sql));
        assert_eq!(corpus.get("App.vue").unwrap().lang, None);
    }

    #[test]
    fn code_is_read_by_its_parse_tree() {
        let corpus = Corpus::of(&[("a.py", "def f():\n    return g('x')\n")]);
        let source = &corpus.files[0];
        let tree = source.tree().unwrap();
        let calls = all_of(tree.root_node(), "call");
        assert_eq!(calls.len(), 1);
        assert_eq!(text(calls[0], &source.text), "g('x')");
        assert_eq!(line(calls[0]), 2);
        let def = ancestor(calls[0], &["function_definition"]).unwrap();
        assert_eq!(line(def), 1);
        let mut kinds = 0;
        walk(tree.root_node(), |_| kinds += 1);
        assert!(kinds > 5);
        assert_eq!(line_at("a\nb\nc", 2), 2);
        assert_eq!(unquote("'x'").as_deref(), Some("x"));
        assert_eq!(unquote("r\"a\\b\"").as_deref(), Some("a\\b"));
        assert_eq!(unquote("`/u/${id}`"), None);
        assert_eq!(unquote("name"), None);
        assert_eq!(ident("user_id) x"), "user_id");
    }
}
