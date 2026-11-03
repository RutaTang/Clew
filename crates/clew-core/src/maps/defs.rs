//! Every definition in a corpus — functions, methods, classes — by name,
//! with the class each is a member of: what a route, a callback or a
//! reference written as text resolves to.

use std::collections::HashMap;

use super::Corpus;

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

/// The definitions of a corpus, by name.
#[derive(Debug, Clone, Default)]
pub struct Defs {
    by_name: HashMap<String, Vec<Def>>,
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

impl Defs {
    pub fn of(corpus: &Corpus) -> Defs {
        let mut by_name: HashMap<String, Vec<Def>> = HashMap::new();
        for source in &corpus.files {
            let (Some(lang), Some(tree)) = (source.lang, source.tree()) else {
                continue;
            };
            let located = crate::outline::located_in(&tree, &source.text, lang);
            let containers: Vec<(&str, usize, usize)> = located
                .iter()
                .filter(|l| CONTAINERS.contains(&l.symbol.kind.as_str()))
                .map(|l| (l.symbol.name.as_str(), l.symbol.line, l.symbol.end_line))
                .collect();
            for l in &located {
                let s = &l.symbol;
                // The innermost container around it, itself aside.
                let class = containers
                    .iter()
                    .filter(|(name, start, end)| {
                        *start <= s.line
                            && s.end_line <= *end
                            && !(*name == s.name && *start == s.line)
                    })
                    .min_by_key(|(_, start, end)| end - start)
                    .map(|(name, _, _)| name.to_string());
                by_name.entry(s.name.clone()).or_default().push(Def {
                    name: s.name.clone(),
                    kind: s.kind.clone(),
                    rel: source.rel.clone(),
                    line: s.line,
                    end_line: s.end_line,
                    class,
                });
            }
        }
        Defs { by_name }
    }

    /// The definitions named `name`.
    pub fn named(&self, name: &str) -> &[Def] {
        self.by_name.get(name).map_or(&[], Vec::as_slice)
    }

    /// The definition `name` most likely meant from `from` (a file): a
    /// member of `class` when one is named, else the one in `from`, then
    /// the one nearest it by path.
    pub fn find(&self, name: &str, class: Option<&str>, from: &str) -> Option<&Def> {
        let candidates = self.named(name);
        let candidates: Vec<&Def> = match class {
            Some(class) => candidates
                .iter()
                .filter(|d| d.class.as_deref() == Some(class))
                .collect(),
            None => candidates.iter().collect(),
        };
        candidates.into_iter().max_by_key(|d| {
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
    pub fn class(&self, name: &str) -> Option<&Def> {
        self.named(name)
            .iter()
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
