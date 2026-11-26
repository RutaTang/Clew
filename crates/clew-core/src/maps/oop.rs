//! Java's, Kotlin's, Scala's and C#'s map:
//!
//! - **overrides** — each method that overrides one of a base class, or
//!   implements one of an interface (a Scala trait), with an arrow to the
//!   nearest one up its type's ancestry: what a call through the base type
//!   may run instead. Kotlin and Scala say `override`, as C# does for a base
//!   class's method; a Java method overrides by its name (`@Override` or
//!   not), as a C# one implements an interface's. Java, Kotlin and Scala
//!   types extend one another's.

use std::collections::HashMap;

use clew_protocol::ProjectMap;

use super::{Corpus, MapBuilder};
use crate::highlight::Lang;
use crate::outline::Located;

const LANGS: &[Lang] = &[Lang::Java, Lang::Kotlin, Lang::Scala, Lang::CSharp];

pub fn maps(corpus: &Corpus) -> Vec<ProjectMap> {
    overrides(corpus).into_iter().collect()
}

/// A class, interface, trait, object, struct or record, with its methods.
struct Type {
    name: String,
    /// Java, Kotlin and Scala are one class path; C# another.
    family: &'static str,
    lang: Lang,
    rel: String,
    dir: String,
    interface: bool,
    bases: Vec<String>,
    /// Each method: its name, whether it says `override`, its line.
    methods: Vec<(String, bool, usize)>,
}

fn is_type(kind: &str) -> bool {
    matches!(
        kind,
        "class" | "interface" | "struct" | "trait" | "enum" | "record" | "object"
    )
}

/// Whether a declaration's text says the word (`override`, `@Override`).
fn says(text: &str, word: &str) -> bool {
    text.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '@'))
        .any(|w| w == word)
}

fn types(corpus: &Corpus) -> Vec<Type> {
    let mut out = Vec::new();
    for source in corpus.in_langs(LANGS) {
        let (Some(lang), Some(tree)) = (source.lang, source.tree()) else {
            continue;
        };
        let src = source.text.as_str();
        let items: Vec<Located> = crate::outline::located_in(&tree, src, lang);
        let signature = |l: &Located| {
            src.get(l.declaration.0..l.signature_end.max(l.declaration.0))
                .unwrap_or("")
                .to_string()
        };
        let family = match lang {
            Lang::CSharp => "cs",
            _ => "jvm",
        };
        let dir = source
            .rel
            .rsplit_once('/')
            .map_or("", |(d, _)| d)
            .to_string();
        let first = out.len();
        for l in items.iter().filter(|l| is_type(&l.symbol.kind)) {
            out.push((
                l.declaration,
                Type {
                    name: l.symbol.name.clone(),
                    family,
                    lang,
                    rel: source.rel.clone(),
                    dir: dir.clone(),
                    interface: matches!(l.symbol.kind.as_str(), "interface" | "trait"),
                    bases: crate::heritage::inherited_names(&signature(l), Some(lang.key())),
                    methods: Vec::new(),
                },
            ));
        }
        // Each method to the innermost type around it: Kotlin's and Scala's
        // members are outlined as functions. One in another function's body
        // is that function's local, no member.
        let callable = |l: &&Located| matches!(l.symbol.kind.as_str(), "method" | "function");
        let inside = |inner: &Located, outer: &Located| {
            outer.declaration.0 < inner.declaration.0 && inner.declaration.1 <= outer.declaration.1
        };
        for m in items.iter().filter(callable) {
            if items.iter().filter(callable).any(|f| inside(m, f)) {
                continue;
            }
            let at = m.declaration.0;
            if let Some((_, owner)) = out[first..]
                .iter_mut()
                .filter(|((from, to), _)| *from < at && at < *to)
                .min_by_key(|((from, to), _)| to - from)
            {
                let text = signature(m);
                let overrides = says(&text, "override") || says(&text, "@Override");
                owner
                    .methods
                    .push((m.symbol.name.clone(), overrides, m.symbol.line));
            }
        }
    }
    out.into_iter().map(|(_, t)| t).collect()
}

fn overrides(corpus: &Corpus) -> Option<ProjectMap> {
    let types = types(corpus);
    let mut by_name: HashMap<(&str, &str), Vec<usize>> = HashMap::new();
    for (i, t) in types.iter().enumerate() {
        by_name
            .entry((t.family, t.name.as_str()))
            .or_default()
            .push(i);
    }
    // A base by its name: the one in the same file, else the same folder,
    // else the only one; past that it is ambiguous, and no arrow is drawn.
    let resolve = |from: &Type, name: &str| -> Option<usize> {
        let found = by_name.get(&(from.family, name))?;
        if let [one] = found.as_slice() {
            return Some(*one);
        }
        found
            .iter()
            .copied()
            .find(|&i| types[i].rel == from.rel)
            .or_else(|| {
                let mut near = found.iter().copied().filter(|&i| types[i].dir == from.dir);
                let one = near.next()?;
                near.next().is_none().then_some(one)
            })
    };
    let mut map = MapBuilder::new(
        "oop.overrides",
        "Overrides",
        "each method that overrides one of a base class or implements one of an interface, with an arrow to the nearest one up its type's ancestry (Java, Kotlin, Scala and C#)",
    );
    for (i, t) in types.iter().enumerate() {
        for (method, says_override, line) in &t.methods {
            // Up the ancestry, nearest first, to a type with the method.
            let mut seen = vec![i];
            let mut queue: std::collections::VecDeque<usize> =
                t.bases.iter().filter_map(|b| resolve(t, b)).collect();
            let mut found = None;
            while let Some(b) = queue.pop_front() {
                if seen.contains(&b) {
                    continue;
                }
                seen.push(b);
                let base = &types[b];
                if let Some((_, _, at)) = base.methods.iter().find(|(n, _, _)| n == method) {
                    found = Some((b, *at));
                    break;
                }
                queue.extend(base.bases.iter().filter_map(|n| resolve(base, n)));
            }
            let Some((b, base_line)) = found else {
                continue;
            };
            let base = &types[b];
            // What each language counts as overriding: Kotlin and Scala
            // say so; C# says so of a class's method, and implements an
            // interface's by its name; Java's override by their name.
            let counts = match t.lang {
                Lang::Kotlin | Lang::Scala => *says_override,
                Lang::CSharp => *says_override || base.interface,
                _ => true,
            };
            if !counts {
                continue;
            }
            let from_label = format!("{}.{method}", t.name);
            let from = map.node("method", &format!("{}:{from_label}", t.rel), &from_label);
            map.place(from, &t.rel, *line);
            let to_label = format!("{}.{method}", base.name);
            let to = map.node("method", &format!("{}:{to_label}", base.rel), &to_label);
            map.place(to, &base.rel, base_line);
            map.edge(
                from,
                to,
                if base.interface {
                    "implements"
                } else {
                    "overrides"
                },
            );
        }
    }
    map.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edges(map: &ProjectMap) -> Vec<(String, String, String)> {
        let mut out: Vec<_> = map
            .edges
            .iter()
            .map(|e| {
                (
                    map.nodes[e.from].label.clone(),
                    map.nodes[e.to].label.clone(),
                    e.label.clone(),
                )
            })
            .collect();
        out.sort();
        out
    }

    fn e(a: &str, b: &str, l: &str) -> (String, String, String) {
        (a.into(), b.into(), l.into())
    }

    /// Java's by name, Kotlin's and Scala's by `override`, C#'s by
    /// `override` or an interface's name; up the ancestry to the nearest,
    /// across Java and Kotlin; an overload of another name is none.
    #[test]
    fn a_method_reaches_the_one_it_overrides_or_implements() {
        let maps = maps(&Corpus::of(&[
            (
                "src/main/java/shop/Shape.java",
                "package shop;\npublic abstract class Shape {\n  public abstract double area();\n  public String name() { return \"shape\"; }\n}\n",
            ),
            (
                "src/main/java/shop/Drawable.java",
                "package shop;\npublic interface Drawable {\n  void draw();\n}\n",
            ),
            (
                "src/main/java/shop/Circle.java",
                "package shop;\npublic class Circle extends Shape implements Drawable {\n  @Override public double area() { return 1; }\n  public void draw() {}\n  public void roll() {}\n}\n",
            ),
            (
                "src/main/kotlin/shop/Ring.kt",
                "package shop\nclass Ring : Circle() {\n    override fun area(): Double = 2.0\n    fun name(x: Int): String = \"ring\"\n}\n",
            ),
            (
                "src/main/scala/shop/Tri.scala",
                "package shop\ntrait Pricing {\n  def price: Double\n}\nclass Tri extends Shape with Pricing {\n  override def area(): Double = 3\n  def price: Double = 4\n}\n",
            ),
            (
                "Shapes.cs",
                "public interface IArea { double Area(); }\npublic class Base { public virtual string Name() => \"b\"; }\npublic class Square : Base, IArea {\n    public double Area() => 4;\n    public override string Name() => \"s\";\n}\n",
            ),
        ]));
        let o = maps.iter().find(|m| m.id == "oop.overrides").unwrap();
        let got = edges(o);
        for want in [
            e("Circle.area", "Shape.area", "overrides"),
            e("Circle.draw", "Drawable.draw", "implements"),
            e("Ring.area", "Circle.area", "overrides"),
            e("Tri.area", "Shape.area", "overrides"),
            e("Square.Area", "IArea.Area", "implements"),
            e("Square.Name", "Base.Name", "overrides"),
        ] {
            assert!(got.contains(&want), "{want:?} in {got:?}");
        }
        for (from, _, _) in &got {
            assert!(
                !matches!(from.as_str(), "Circle.roll" | "Tri.price"),
                "{from}: {got:?}"
            );
        }
        // Kotlin's `name(x)` says no `override`: an overload, not an override.
        assert!(!got.iter().any(|(f, _, _)| f == "Ring.name"), "{got:?}");
    }
}
