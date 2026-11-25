//! Every definition in a corpus — functions, methods, classes — by name,
//! with the class each is a member of: what a route, a callback or a
//! reference written as text resolves to.

use super::{Corpus, Source};
use crate::highlight::Lang;

/// One definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Def {
    pub name: String,
    pub kind: String,
    pub rel: String,
    pub line: usize,
    pub end_line: usize,
    /// The class (struct, module, interface) it is declared in.
    pub class: Option<String>,
}

/// The definitions of a corpus, looked up by name: each file outlined the
/// first time a name it says is looked for, and kept. A map that places a
/// few names reads the few files that say them, not every file of the
/// project (bulletproof-react's routes spent a third of a second outlining
/// all 500 to place a dozen handlers).
#[derive(Clone, Copy)]
pub struct Defs<'c> {
    corpus: &'c Corpus,
}

/// Kinds a member is declared in.
const CONTAINERS: &[&str] = &[
    "class",
    "struct",
    "interface",
    "trait",
    "module",
    "object",
    "impl",
    "enum",
    "record",
    "protocol",
    "mixin",
    "extension",
    "namespace",
];

/// The definitions of one file: each with the innermost container around it.
pub(super) fn of_file(source: &Source) -> Vec<Def> {
    // Code's definitions only: a schema dump's thousands of tables
    // and columns (GitLab's: 1,500 and 30,000) are no class a map
    // looks for, and took most of a minute to index.
    if matches!(
        source.lang,
        Some(Lang::Sql | Lang::Json | Lang::Yaml | Lang::Toml | Lang::Html | Lang::Css)
    ) {
        return Vec::new();
    }
    let (Some(lang), Some(tree)) = (source.lang, source.tree()) else {
        return Vec::new();
    };
    let located = crate::outline::located_in(&tree, &source.text, lang);
    let containers: Vec<(&str, usize, usize)> = located
        .iter()
        .filter(|l| CONTAINERS.contains(&l.symbol.kind.as_str()))
        .map(|l| (l.symbol.name.as_str(), l.symbol.line, l.symbol.end_line))
        .collect();
    let mut out = Vec::with_capacity(located.len());
    for l in &located {
        let s = &l.symbol;
        // The innermost container around it, itself aside.
        let class = containers
            .iter()
            .filter(|(name, start, end)| {
                *start <= s.line && s.end_line <= *end && !(*name == s.name && *start == s.line)
            })
            .min_by_key(|(_, start, end)| end - start)
            .map(|(name, _, _)| name.to_string());
        out.push(Def {
            name: s.name.clone(),
            kind: s.kind.clone(),
            rel: source.rel.clone(),
            line: s.line,
            end_line: s.end_line,
            class,
        });
    }
    out
}

impl<'c> Defs<'c> {
    pub fn of(corpus: &'c Corpus) -> Defs<'c> {
        Defs { corpus }
    }

    /// The definitions named `name`.
    pub fn named(&self, name: &str) -> Vec<&'c Def> {
        if name.is_empty() {
            return Vec::new();
        }
        self.corpus
            .files
            .iter()
            .filter(|s| s.text.contains(name))
            .flat_map(|s| s.defs())
            .filter(|d| d.name == name)
            .collect()
    }

    /// The definition `name` most likely meant from `from` (a file): a
    /// member of `class` when one is named, else the one in `from`, then
    /// the one nearest it by path.
    pub fn find(&self, name: &str, class: Option<&str>, from: &str) -> Option<&'c Def> {
        self.named(name)
            .into_iter()
            .filter(|d| class.is_none() || d.class.as_deref() == class)
            .max_by_key(|d| {
                let shared = d
                    .rel
                    .split('/')
                    .zip(from.split('/'))
                    .take_while(|(a, b)| a == b)
                    .count();
                (d.rel == from, shared, std::cmp::Reverse(d.line))
            })
    }

    /// The class (or other container) called `name`.
    pub fn class(&self, name: &str) -> Option<&'c Def> {
        self.named(name)
            .into_iter()
            .find(|d| CONTAINERS.contains(&d.kind.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn definitions_are_found_by_name_and_class() {
        let corpus = Corpus::of(&[
            (
                "app/users.py",
                "class UserView:\n    def get(self):\n        pass\n\ndef index():\n    pass\n",
            ),
            ("app/other.py", "def index():\n    pass\n"),
            ("lib/x.py", "def index():\n    pass\n"),
        ]);
        let defs = corpus.defs();
        let get = defs.find("get", Some("UserView"), "app/urls.py").unwrap();
        assert_eq!((get.rel.as_str(), get.line), ("app/users.py", 2));
        assert_eq!(defs.find("get", Some("Other"), "x"), None);
        let index = defs.find("index", None, "app/users.py").unwrap();
        assert_eq!(index.rel, "app/users.py", "the same file first");
        let index = defs.find("index", None, "app/urls.py").unwrap();
        assert!(index.rel.starts_with("app/"), "then the nearest: {index:?}");
        assert_eq!(defs.class("UserView").unwrap().line, 1);
        assert_eq!(defs.named("index").len(), 3);
    }
}
