//! The project glossary: the names a reader keeps meeting — the project's
//! types, its modules and folders, its acronyms — each with a one-line
//! definition, so a term can be looked up where it is met (the hover peek)
//! or browsed as a whole (the Glossary page) without hunting for the
//! declaration first.
//!
//! Definitions are the project's own words, never invented: a type's comes
//! from the author's doc comment (the API-docs index, as the DOCS tab shows
//! it), an acronym's from where the docs spell it out — "Debug Adapter
//! Protocol (DAP)" or "LSP (Language Server Protocol)", the initials
//! matching — and a module's from its own doc comment (Rust's `//!`, a
//! Python docstring), else, like a folder's, from clew's cached explanation
//! of it (Explain All). A name with none of these is not a
//! term — a glossary is definitions, and the DOCS tab already lists every
//! declaration. An all-caps name is no acronym by itself: `MAX` and
//! `SYSTEM` are constants.

use crate::app::prelude::*;
use crate::*;

/// What kind of thing a term names — the section it is listed under.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum TermKind {
    /// A struct, class, enum, interface, trait, union or type alias.
    Type,
    /// A source file or a folder — named by its module label / path.
    Module,
    /// An abbreviation the docs spell out (`DAP`: Debug Adapter Protocol).
    Acronym,
}

impl TermKind {
    /// The section title on the Glossary page.
    pub(crate) fn heading(self) -> &'static str {
        match self {
            TermKind::Type => "Types",
            TermKind::Module => "Modules",
            TermKind::Acronym => "Acronyms",
        }
    }
}

/// One glossary entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Term {
    /// The name as the code spells it (a module's label for a file, its
    /// root-relative path for a folder).
    pub(crate) name: String,
    pub(crate) kind: TermKind,
    /// The declaration's kind for the badge ("struct", "enum", "module", …).
    pub(crate) badge: String,
    /// Root-relative path of the file (or folder) that defines it.
    pub(crate) rel: String,
    /// 1-based declaration line (1 for a file or folder).
    pub(crate) line: usize,
    /// The one-line definition: the doc comment's or the explanation's first
    /// sentence, flattened and capped. Never empty.
    pub(crate) definition: String,
}

impl Term {
    /// The one line the hover peek shows for this term: what it is, and
    /// where it is defined.
    pub(crate) fn peek_line(&self) -> String {
        format!(
            "{}: {} — {}:{}",
            self.name, self.definition, self.rel, self.line
        )
    }
}

/// Longest definition kept, in characters; longer ones are cut with an ellipsis.
pub(crate) const MAX_DEFINITION_CHARS: usize = 160;

/// The glossary of one project, derived from its docs index and its
/// explanation cache (see the module doc). Rebuilt by `App::glossary` when
/// either changes.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Glossary {
    /// Every term, sorted by name (case-insensitively) then by file.
    terms: Vec<Term>,
    /// Name → index into `terms` for the names defined exactly once; a name
    /// two declarations share is ambiguous and is not looked up.
    unique: HashMap<String, usize>,
    /// Per term, its name and definition lowercased — what the filter
    /// matches against, built once rather than per repaint.
    keys: Vec<String>,
}

impl Glossary {
    /// Build the glossary from the docs index (`files`) and the explanation
    /// cache, `root` being the project root the cache's absolute paths are
    /// under.
    pub(crate) fn build(
        files: &[clew_protocol::DocFile],
        cache: &explain::Cache,
        root: Option<&Path>,
    ) -> Self {
        let mut terms: Vec<Term> = Vec::new();
        // Files whose own doc defined their module: the author's words win
        // over an explanation of the same file.
        let mut self_documented: HashSet<&str> = HashSet::new();
        for file in files {
            if let Some(definition) = definition_of(&file.doc) {
                terms.push(Term {
                    name: crate::ui::module_label(&file.rel),
                    kind: TermKind::Module,
                    badge: "module".into(),
                    rel: file.rel.clone(),
                    line: 1,
                    definition,
                });
                self_documented.insert(file.rel.as_str());
            }
            for (abbr, long) in acronyms_in(&file.doc) {
                terms.push(Term {
                    name: abbr,
                    kind: TermKind::Acronym,
                    badge: "acronym".into(),
                    rel: file.rel.clone(),
                    line: 1,
                    definition: long,
                });
            }
            for item in &file.items {
                collect_items(item, None, &file.rel, &mut terms);
            }
        }
        if let Some(root) = root {
            let mut nodes: Vec<(&explain::Node, &explain::Cached)> = cache
                .iter()
                .filter(|(n, _)| matches!(n, explain::Node::File(_) | explain::Node::Folder(_)))
                .collect();
            // The cache is a hash map: order the walk so the glossary is the
            // same one for the same project every time it is built.
            nodes.sort_by(|a, b| a.0.path().cmp(b.0.path()));
            for (node, cached) in nodes {
                if explain::is_error_summary(&cached.summary) {
                    continue;
                }
                let Ok(rel) = node.path().strip_prefix(root) else {
                    continue;
                };
                let rel = rel.to_string_lossy().replace('\\', "/");
                if rel.is_empty() || self_documented.contains(rel.as_str()) {
                    continue;
                }
                let Some(definition) = definition_of(&cached.summary) else {
                    continue;
                };
                let (name, badge) = match node {
                    explain::Node::File(_) => (crate::ui::module_label(&rel), "module"),
                    _ => (rel.clone(), "folder"),
                };
                terms.push(Term {
                    name,
                    kind: TermKind::Module,
                    badge: badge.into(),
                    rel,
                    line: 1,
                    definition,
                });
            }
        }
        terms.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.name.cmp(&b.name))
                .then_with(|| a.rel.cmp(&b.rel))
                .then_with(|| a.line.cmp(&b.line))
        });
        terms.dedup();
        // An acronym spelled out in several places is one term (its first
        // place); two different expansions stay two (and ambiguous).
        let mut spelled: HashSet<(String, String)> = HashSet::new();
        terms.retain(|t| {
            t.kind != TermKind::Acronym
                || spelled.insert((t.name.clone(), t.definition.to_lowercase()))
        });
        let mut unique: HashMap<String, usize> = HashMap::new();
        let mut ambiguous: HashSet<String> = HashSet::new();
        for (i, t) in terms.iter().enumerate() {
            if ambiguous.contains(&t.name) {
                continue;
            }
            if unique.insert(t.name.clone(), i).is_some() {
                unique.remove(&t.name);
                ambiguous.insert(t.name.clone());
            }
        }
        let keys = terms
            .iter()
            .map(|t| format!("{}\n{}", t.name.to_lowercase(), t.definition.to_lowercase()))
            .collect();
        Glossary {
            terms,
            unique,
            keys,
        }
    }

    /// The term `name` names, when the project defines exactly one.
    pub(crate) fn lookup(&self, name: &str) -> Option<&Term> {
        self.unique.get(name).map(|&i| &self.terms[i])
    }

    /// Every term, in listing order.
    pub(crate) fn terms(&self) -> &[Term] {
        &self.terms
    }

    /// The terms whose name or definition contains `query` (already trimmed
    /// and lowercased; empty matches all), in listing order.
    pub(crate) fn matching<'a>(&'a self, query: &'a str) -> impl Iterator<Item = &'a Term> + 'a {
        self.terms
            .iter()
            .zip(&self.keys)
            .filter(move |(_, key)| query.is_empty() || key.contains(query))
            .map(|(t, _)| t)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    pub(crate) fn len(&self) -> usize {
        self.terms.len()
    }
}

/// Walk `item` and its children (a nested class, an impl's associated
/// types) for the terms they define.
fn collect_items(
    item: &clew_protocol::DocItem,
    parent_kind: Option<&str>,
    rel: &str,
    out: &mut Vec<Term>,
) {
    // A type declared in a function body is that function's, not the project's.
    let local = parent_kind.is_some_and(crate::typegraph::is_callable_kind);
    if crate::typegraph::TYPE_KINDS.contains(&item.kind.as_str())
        && !local
        && let Some(definition) = definition_of(&item.doc)
    {
        out.push(Term {
            name: item.name.clone(),
            kind: TermKind::Type,
            badge: item.kind.clone(),
            rel: rel.to_string(),
            line: item.line,
            definition,
        });
    }
    for (abbr, long) in acronyms_in(&item.doc) {
        out.push(Term {
            name: abbr,
            kind: TermKind::Acronym,
            badge: "acronym".into(),
            rel: rel.to_string(),
            line: item.line,
            definition: long,
        });
    }
    for child in &item.children {
        collect_items(child, Some(&item.kind), rel, out);
    }
}

/// The words that may sit inside a spelled-out acronym without a letter of
/// their own ("Bring Your Own Key", "Line of Business").
const ACRONYM_FILLERS: &[&str] = &["of", "and", "the", "for", "to", "in", "on", "a", "an", "&"];

/// Every acronym `doc` spells out, with its expansion: "Long Form (ABBR)" or
/// "ABBR (Long Form)", where the long form's initials are the acronym's
/// capitals in order (filler words aside). "The debugger (DAP)" spells
/// nothing out; neither does "UI (it goes in the status bar)".
pub(crate) fn acronyms_in(doc: &str) -> Vec<(String, String)> {
    let flat = strip_inline_markup(&doc.split_whitespace().collect::<Vec<_>>().join(" "));
    let mut found: Vec<(String, String)> = Vec::new();
    let mut rest = flat.as_str();
    let mut before = String::new();
    while let Some(open) = rest.find('(') {
        let Some(len) = rest[open..].find(')') else {
            break;
        };
        let head = &rest[..open];
        let inner = rest[open + 1..open + len].trim();
        before.push_str(head);
        let words_before: Vec<&str> = before.split_whitespace().collect();
        // "Long Form (ABBR)".
        if is_acronym(inner)
            && let Some(long) = spelled_out(inner, &words_before, true)
        {
            found.push((inner.to_string(), long));
        }
        // "ABBR (Long Form)".
        if let Some(abbr) = words_before
            .last()
            .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()))
            && is_acronym(abbr)
        {
            let words: Vec<&str> = inner.split_whitespace().collect();
            if let Some(long) = spelled_out(abbr, &words, false)
                && long.split_whitespace().count() == words.len()
            {
                found.push((abbr.to_string(), long));
            }
        }
        before.push_str(&rest[open..open + len + 1]);
        rest = &rest[open + len + 1..];
    }
    found
}

/// The words of `words` that spell `abbr` out: its capitals are the initials
/// of consecutive words, fillers aside — the last words when `from_end`
/// ("Long Form (ABBR)"), the first otherwise ("ABBR (Long Form)").
fn spelled_out(abbr: &str, words: &[&str], from_end: bool) -> Option<String> {
    let letters: Vec<char> = abbr
        .chars()
        .filter(|c| c.is_ascii_uppercase())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    let clean = |w: &str| {
        w.trim_matches(|c: char| !(c.is_alphanumeric() || c == '-' || c == '&'))
            .to_string()
    };
    let mut taken: Vec<String> = Vec::new();
    let mut want = letters.len();
    let ordered: Vec<&str> = if from_end {
        words.iter().rev().copied().collect()
    } else {
        words.to_vec()
    };
    for w in ordered {
        if want == 0 {
            break;
        }
        let word = clean(w);
        // A hyphenated word gives an initial per part ("Just-In-Time").
        let parts: Vec<&str> = word.split('-').filter(|p| !p.is_empty()).collect();
        let initials: Vec<char> = parts
            .iter()
            .filter_map(|p| p.chars().next())
            .map(|c| c.to_ascii_lowercase())
            .collect();
        let needed: Vec<char> = if from_end {
            letters[want.saturating_sub(initials.len())..want].to_vec()
        } else {
            let done = letters.len() - want;
            letters[done..(done + initials.len()).min(letters.len())].to_vec()
        };
        if !initials.is_empty() && initials == needed {
            want -= initials.len();
            taken.push(word);
        } else if !taken.is_empty() && ACRONYM_FILLERS.contains(&word.to_lowercase().as_str()) {
            taken.push(word);
        } else {
            return None;
        }
    }
    if want != 0 {
        return None;
    }
    if from_end {
        taken.reverse();
    }
    Some(taken.join(" "))
}

/// Whether `name` is shaped like an acronym: two to eight characters, all
/// upper case letters or digits, at least two letters (`LSP`, `DAP`,
/// `HTTP2`; not `A`, `V2`, `MAX_LEN`). The shape alone makes no term: it is
/// only an acronym where the docs spell it out ([`acronyms_in`]).
pub(crate) fn is_acronym(name: &str) -> bool {
    let n = name.chars().count();
    (2..=8).contains(&n)
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        && name.chars().filter(char::is_ascii_uppercase).count() >= 2
}

/// The one-line definition in `doc` (markdown: a doc comment or an
/// explanation): its first sentence, flattened onto one line, with inline
/// markup stripped and capped at [`MAX_DEFINITION_CHARS`]. `None` when there
/// is no prose — empty, or only a heading / code fence / list marker.
pub(crate) fn definition_of(doc: &str) -> Option<String> {
    // The first paragraph, skipping headings, code fences and blank lines.
    let mut para: Vec<&str> = Vec::new();
    let mut in_fence = false;
    for line in doc.lines() {
        let t = line.trim();
        if t.starts_with("```") || t.starts_with("~~~") {
            in_fence = !in_fence;
            if !para.is_empty() {
                break;
            }
            continue;
        }
        if in_fence {
            continue;
        }
        if t.is_empty() {
            if para.is_empty() {
                continue;
            }
            break;
        }
        if para.is_empty() && (t.starts_with('#') || t == "---") {
            continue;
        }
        // A list or a quote is its own block: it ends a paragraph before
        // it, and a doc that opens with one is defined by its first item.
        let marker = t.starts_with("- ")
            || t.starts_with("* ")
            || t.starts_with("> ")
            || t.starts_with("+ ")
            || t.split_once(". ")
                .is_some_and(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
        if marker && !para.is_empty() {
            break;
        }
        let t = if marker {
            t.trim_start_matches(['-', '*', '>', '+'])
                .trim_start_matches(|c: char| c.is_ascii_digit() || c == '.')
                .trim_start()
        } else {
            t
        };
        if !t.is_empty() {
            para.push(t);
        }
        if marker {
            break;
        }
    }
    if para.is_empty() {
        return None;
    }
    let flat = para.join(" ");
    let plain = strip_inline_markup(&flat);
    let first = first_sentence(&plain);
    let first = first.trim();
    if first.is_empty() {
        return None;
    }
    let n = first.chars().count();
    Some(if n > MAX_DEFINITION_CHARS {
        let cut: String = first.chars().take(MAX_DEFINITION_CHARS - 1).collect();
        format!("{}…", cut.trim_end())
    } else {
        first.to_string()
    })
}

/// The text up to the first sentence end — a `.`, `!` or `?` followed by a
/// space or the end — the terminal `.` dropped. A `.` inside a word (a
/// path, a version, `e.g.`) is not an end.
fn first_sentence(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if matches!(c, '.' | '!' | '?') {
            let at_end = i + 1 == chars.len();
            let before_space = chars.get(i + 1).is_some_and(|n| n.is_whitespace());
            if at_end || before_space {
                // `e.g.` / `i.e.` / `vs.` / a single-letter abbreviation:
                // the sentence goes on.
                let word: String = chars[..i]
                    .iter()
                    .rev()
                    .take_while(|c| !c.is_whitespace())
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                let abbreviation = c == '.'
                    && (matches!(word.as_str(), "e.g" | "i.e" | "vs" | "etc" | "cf")
                        || (word.chars().count() == 1 && word.chars().all(char::is_alphabetic)));
                if abbreviation && !at_end {
                    continue;
                }
                let end = if c == '.' { i } else { i + 1 };
                return chars[..end].iter().collect();
            }
        }
    }
    s.to_string()
}

/// Drops inline markdown: `code` ticks, `**` / `_` emphasis, `[text](url)`
/// links to their text, and a trailing `:` a doc starts a list with.
fn strip_inline_markup(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    // Links to their text: `[text](url)`, a reference `[text][ref]`, and a
    // rustdoc intra-doc link `[path]` / `[`path`]`, whose text is the path.
    while let Some(open) = rest.find('[') {
        let (before, from_open) = rest.split_at(open);
        out.push_str(before);
        let Some(close) = from_open.find(']') else {
            out.push_str(from_open);
            rest = "";
            break;
        };
        out.push_str(&from_open[1..close]);
        let after = &from_open[close + 1..];
        let skip = match after.chars().next() {
            Some('(') => after.find(')').map(|e| e + 1),
            Some('[') => after.find(']').map(|e| e + 1),
            _ => None,
        };
        rest = &after[skip.unwrap_or(0)..];
    }
    out.push_str(rest);
    let out: String = out.chars().filter(|c| !matches!(c, '`' | '*')).collect();
    let out = out.replace("\\_", "_");
    out.trim().trim_end_matches(':').trim().to_string()
}

/// The Glossary page's state (per project).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct GlossaryState {
    /// The page covers the code panes.
    pub showing: bool,
    /// The page's filter box.
    pub filter: String,
}

impl App {
    /// The project's glossary, rebuilt when the docs index or the explanation
    /// cache changed since it was last built (the view memo keeps it, so a
    /// hover and the page share one build).
    pub(crate) fn glossary(&self) -> Arc<Glossary> {
        let key = (self.proj.docs.generation, self.proj.explain.cache_seq);
        self.proj.view_memo.glossary.get_or(key, || {
            Glossary::build(
                &self.proj.docs.files,
                &self.proj.explain.cache,
                self.proj.project.as_ref().map(|p| p.root.as_path()),
            )
        })
    }

    /// Handle a [`GlossaryMsg`]: this feature's share of what `dispatch`
    /// routes.
    pub(crate) fn update_glossary(&mut self, message: GlossaryMsg) -> Task<Message> {
        match message {
            GlossaryMsg::Open => {
                if self.proj.project.is_none() {
                    self.status = "Open a project to see its glossary".into();
                    return Task::none();
                }
                self.proj.glossary.showing = true;
                self.proj.overview.showing = false;
                self.proj.stats.showing = false;
                self.proj.docs.page = None;
                // The terms come from the docs index, which is only built
                // on demand: bring it up to date for the page (single-flight,
                // a no-op while it is fresh).
                self.ensure_docs();
                Task::none()
            }
            GlossaryMsg::Close => {
                self.proj.glossary.showing = false;
                Task::none()
            }
            GlossaryMsg::FilterChanged(filter) => {
                self.proj.glossary.filter = filter;
                Task::none()
            }
        }
    }
}

#[cfg(test)]
mod glossary_tests {
    use super::*;
    use clew_protocol::{DocFile, DocItem};

    fn item(name: &str, kind: &str, doc: &str, line: usize) -> DocItem {
        DocItem {
            name: name.into(),
            kind: kind.into(),
            signature: format!("{kind} {name}"),
            doc: doc.into(),
            line,
            public: true,
            children: Vec::new(),
            refs: Vec::new(),
        }
    }

    #[test]
    fn definition_is_the_first_sentence_flattened_and_stripped() {
        assert_eq!(
            definition_of("The `Foo` **thing**. It does more.\nAnd more."),
            Some("The Foo thing".into())
        );
        assert_eq!(
            definition_of("# Heading\n\nWraps a [client](../x.md)\nover two lines! Then more."),
            Some("Wraps a client over two lines!".into())
        );
        assert_eq!(
            definition_of("```rust\nlet x = 1;\n```\nAfter the fence."),
            Some("After the fence".into())
        );
        assert_eq!(
            definition_of("Reads e.g. the file. Then parses."),
            Some("Reads e.g. the file".into())
        );
        assert_eq!(
            definition_of("See v1.2.3 for details"),
            Some("See v1.2.3 for details".into())
        );
        assert_eq!(definition_of("Keeps:\n- one\n- two"), Some("Keeps".into()));
        assert_eq!(
            definition_of("- first item\n- second"),
            Some("first item".into())
        );
        assert_eq!(
            definition_of("1. step one. Then more\n2. step two"),
            Some("step one".into())
        );
        assert_eq!(definition_of("> quoted\nnext"), Some("quoted".into()));
        assert_eq!(definition_of(""), None);
        assert_eq!(definition_of("   \n\n"), None);
        assert_eq!(definition_of("```\nonly code\n```"), None);
        let long = "x".repeat(400);
        let d = definition_of(&long).unwrap();
        assert_eq!(d.chars().count(), MAX_DEFINITION_CHARS);
        assert!(d.ends_with('…'));
    }

    #[test]
    fn acronyms_are_where_the_docs_spell_them_out() {
        let pairs = |doc: &str| acronyms_in(doc);
        assert_eq!(
            pairs("Talks the Debug Adapter Protocol (DAP) to the adapter."),
            [("DAP".to_string(), "Debug Adapter Protocol".to_string())]
        );
        assert_eq!(
            pairs("Asks the LSP (Language Server Protocol) server."),
            [("LSP".to_string(), "Language Server Protocol".to_string())]
        );
        assert_eq!(
            pairs("Identified by its Stock Keeping Unit (SKU)."),
            [("SKU".to_string(), "Stock Keeping Unit".to_string())]
        );
        assert_eq!(
            pairs("Bring Your Own Key (BYOK), and a Line of Business (LOB) report"),
            [
                ("BYOK".to_string(), "Bring Your Own Key".to_string()),
                ("LOB".to_string(), "Line of Business".to_string()),
            ]
        );
        assert_eq!(
            pairs("Compiled Just-In-Time (JIT)."),
            [("JIT".to_string(), "Just-In-Time".to_string())]
        );
        // Spelled across a wrapped doc comment, with markup.
        assert_eq!(
            pairs("the `Debug\nAdapter` **Protocol** (DAP)"),
            [("DAP".to_string(), "Debug Adapter Protocol".to_string())]
        );
        for nothing in [
            "The debugger (DAP) runs.",
            "UI (it goes in the status bar)",
            "Counts (2) of them",
            "A constant, SYSTEM.",
            "",
        ] {
            assert!(
                pairs(nothing).is_empty(),
                "{nothing:?}: {:?}",
                pairs(nothing)
            );
        }
    }

    #[test]
    fn links_read_as_their_text() {
        assert_eq!(
            definition_of("Dropped before [Abandon::keep] — see [`lookup`]."),
            Some("Dropped before Abandon::keep — see lookup".into())
        );
        assert_eq!(
            definition_of("Wraps a [client][c] and a [server](https://x) here."),
            Some("Wraps a client and a server here".into())
        );
        assert_eq!(
            definition_of("An [a] and a [b](u) together."),
            Some("An a and a b together".into())
        );
    }

    #[test]
    fn acronyms_are_short_all_caps_names() {
        for yes in ["LSP", "DAP", "HTTP2", "IO", "UUID"] {
            assert!(is_acronym(yes), "{yes}");
        }
        for no in ["A", "V2", "MAX_LEN", "Lsp", "TOOLONGNAME", "42"] {
            assert!(!is_acronym(no), "{no}");
        }
    }

    #[test]
    fn terms_come_from_documented_types_acronyms_and_explained_modules() {
        let root = PathBuf::from("/p");
        let files = vec![
            DocFile {
                doc: String::new(),
                rel: "src/net/client.rs".into(),
                items: vec![
                    item("Client", "struct", "A connection to one server.", 10),
                    item("undocumented", "struct", "", 20),
                    item(
                        "SYSTEM",
                        "constant",
                        "The system prompt. An all-caps name is no acronym.",
                        25,
                    ),
                    item(
                        "frame",
                        "function",
                        "Frames a Remote Procedure Call (RPC) for the wire.",
                        30,
                    ),
                    item("connect", "function", "Opens a client. Not a term.", 40),
                    {
                        let mut run = item("run", "function", "", 45);
                        run.children = vec![item("Local", "struct", "A helper of run.", 46)];
                        run
                    },
                    {
                        let mut outer = item("Outer", "class", "", 50);
                        outer.children = vec![item("Inner", "enum", "Nested state.", 52)];
                        outer
                    },
                ],
            },
            DocFile {
                doc: String::new(),
                rel: "src/other.rs".into(),
                items: vec![
                    item("Client", "struct", "Another one.", 3),
                    // The same spelling elsewhere is the same term.
                    item(
                        "send",
                        "function",
                        "Sends a remote procedure call (RPC).",
                        9,
                    ),
                ],
            },
        ];
        let mut cache = explain::Cache::new();
        let cached = |s: &str| explain::Cached {
            summary: s.into(),
            prompt_hash: 1,
            detail: None,
            basis: None,
        };
        cache.insert(
            explain::Node::File(root.join("src/net/client.rs")),
            cached("Talks to the server. Details follow."),
        );
        cache.insert(
            explain::Node::Folder(root.join("src/net")),
            cached("Networking."),
        );
        cache.insert(
            explain::Node::File(root.join("src/broken.rs")),
            cached("(explanation unavailable: the model timed out)"),
        );
        cache.insert(
            explain::Node::Function {
                file: root.join("src/net/client.rs"),
                name: "connect".into(),
                ordinal: 0,
            },
            cached("Not a module."),
        );
        let g = Glossary::build(&files, &cache, Some(&root));
        let names: Vec<(&str, TermKind)> = g
            .terms()
            .iter()
            .map(|t| (t.name.as_str(), t.kind))
            .collect();
        assert_eq!(
            names,
            vec![
                ("Client", TermKind::Type),
                ("Client", TermKind::Type),
                ("Inner", TermKind::Type),
                ("net::client", TermKind::Module),
                ("RPC", TermKind::Acronym),
                ("src/net", TermKind::Module),
            ]
        );
        // A name defined twice is ambiguous: not looked up. The rest are.
        assert!(g.lookup("Client").is_none());
        let rpc = g.lookup("RPC").unwrap();
        assert_eq!(rpc.definition, "Remote Procedure Call");
        assert_eq!(rpc.badge, "acronym");
        assert_eq!((rpc.rel.as_str(), rpc.line), ("src/net/client.rs", 30));
        assert!(g.lookup("SYSTEM").is_none(), "a constant is no acronym");
        assert!(
            g.lookup("Local").is_none(),
            "a function's own type is no term"
        );
        assert_eq!(
            g.lookup("Inner").unwrap().peek_line(),
            "Inner: Nested state — src/net/client.rs:52"
        );
        let module = g.lookup("net::client").unwrap();
        assert_eq!(module.definition, "Talks to the server");
        assert_eq!(
            (module.badge.as_str(), module.rel.as_str(), module.line),
            ("module", "src/net/client.rs", 1)
        );
        assert_eq!(g.lookup("src/net").unwrap().badge, "folder");
        assert!(g.lookup("undocumented").is_none());
        assert!(g.lookup("connect").is_none());
        // The filter matches names and definitions, case-insensitively.
        let hits: Vec<&str> = g.matching("server").map(|t| t.name.as_str()).collect();
        assert_eq!(hits, vec!["Client", "net::client"]);
        assert_eq!(g.matching("").count(), g.len());
        // The build is deterministic whatever order the cache iterates in.
        assert_eq!(Glossary::build(&files, &cache, Some(&root)), g);
        // Without a root the cache contributes nothing; the docs still do.
        assert_eq!(Glossary::build(&files, &cache, None).len(), 4);
        assert!(Glossary::build(&[], &explain::Cache::new(), Some(&root)).is_empty());
    }

    /// A file's own doc defines its module, ahead of an explanation of the
    /// same file, and spells out acronyms like an item's doc does — the
    /// usual place for them, at the top of a file.
    #[test]
    fn a_files_own_doc_defines_its_module_and_its_acronyms() {
        let root = PathBuf::from("/p");
        let files = vec![
            DocFile {
                rel: "src/dap/mod.rs".into(),
                items: Vec::new(),
                doc: "Debug Adapter Protocol (DAP) support: clew drives an adapter.".into(),
            },
            DocFile {
                rel: "shop/catalog.py".into(),
                items: Vec::new(),
                doc: "The product catalog.".into(),
            },
        ];
        let mut cache = explain::Cache::new();
        cache.insert(
            explain::Node::File(root.join("shop/catalog.py")),
            explain::Cached {
                summary: "Model words about the catalog.".into(),
                prompt_hash: 1,
                detail: None,
                basis: None,
            },
        );
        let g = Glossary::build(&files, &cache, Some(&root));
        let dap = g.lookup("DAP").unwrap();
        assert_eq!(dap.definition, "Debug Adapter Protocol");
        assert_eq!((dap.rel.as_str(), dap.line), ("src/dap/mod.rs", 1));
        assert_eq!(
            g.lookup("dap").unwrap().definition,
            "Debug Adapter Protocol (DAP) support: clew drives an adapter"
        );
        let catalog = g.lookup("shop.catalog").unwrap();
        assert_eq!(
            catalog.definition, "The product catalog",
            "the author's words win"
        );
        assert_eq!(
            g.terms()
                .iter()
                .filter(|t| t.rel == "shop/catalog.py")
                .count(),
            1,
            "{:?}",
            g.terms()
        );
    }
}
