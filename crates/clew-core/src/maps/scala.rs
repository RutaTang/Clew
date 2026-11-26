//! Scala's map, as Haskell's is read for its classes:
//!
//! - **type class instances** — each type and an arrow to each type class
//!   it has an instance of: a `given` (`given Ordering[Int] with …`,
//!   `given intOrd: Ordering[Int] = …`), an `implicit` `val`, `def` or
//!   `object` (`implicit object CartJson extends Encoder[Cart]`), and a
//!   class's `derives` clause — what the compiler finds where a type class
//!   is asked for, written nowhere near the call that uses it.

use clew_protocol::ProjectMap;
use tree_sitter::Node;

use super::{Corpus, MapBuilder, all_of, line, text};
use crate::highlight::Lang;

pub fn maps(corpus: &Corpus) -> Vec<ProjectMap> {
    instances(corpus).into_iter().collect()
}

/// `Show[Cart]`: the class (`Show`) and what it is for (`Cart`).
fn class_and_type<'s>(ty: Node<'_>, src: &'s str) -> Option<(&'s str, &'s str)> {
    if ty.kind() != "generic_type" {
        return None;
    }
    let class = ty.child_by_field_name("type")?;
    let args = ty.child_by_field_name("type_arguments")?;
    let inner = text(args, src);
    let inner = inner.strip_prefix('[')?.strip_suffix(']')?;
    Some((text(class, src).rsplit('.').next()?, inner.trim()))
}

fn implicit(node: Node<'_>, src: &str) -> bool {
    node.named_children(&mut node.walk()).any(|c| {
        c.kind() == "modifiers" && text(c, src).split_whitespace().any(|w| w == "implicit")
    })
}

fn instances(corpus: &Corpus) -> Option<ProjectMap> {
    let mut map = MapBuilder::new(
        "scala.instances",
        "Type class instances",
        "each type and an arrow to each type class it has an instance of, from given and implicit definitions and derives clauses",
    );
    for source in corpus.in_langs(&[Lang::Scala]) {
        let Some(tree) = source.tree() else {
            continue;
        };
        let src = source.text.as_str();
        let root = tree.root_node();
        let mut link = |class: &str, ty: &str, how: &str, at: usize| {
            if class.is_empty() || ty.is_empty() {
                return;
            }
            let from = map.node("type", ty, ty);
            let to = map.node("class", class, class);
            map.edge(from, to, how);
            map.add_detail(to, &format!("{how} for {ty} at {}:{at}", source.rel));
        };
        for given in all_of(root, "given_definition") {
            if let Some((class, ty)) = given
                .child_by_field_name("return_type")
                .and_then(|t| class_and_type(t, src))
            {
                link(class, ty, "given", line(given));
            }
        }
        for kind in ["val_definition", "function_definition", "object_definition"] {
            for def in all_of(root, kind) {
                if !implicit(def, src) {
                    continue;
                }
                let ty = match kind {
                    "val_definition" => def.child_by_field_name("type"),
                    "function_definition" => def.child_by_field_name("return_type"),
                    _ => def
                        .child_by_field_name("extend")
                        .and_then(|e| e.child_by_field_name("type")),
                };
                if let Some((class, ty)) = ty.and_then(|t| class_and_type(t, src)) {
                    link(class, ty, "implicit", line(def));
                }
            }
        }
        for class in all_of(root, "class_definition") {
            let (Some(name), Some(derives)) = (
                class.child_by_field_name("name"),
                class.child_by_field_name("derive"),
            ) else {
                continue;
            };
            let mut cursor = derives.walk();
            for tc in derives.children_by_field_name("type", &mut cursor) {
                link(text(tc, src), text(name, src), "derives", line(class));
            }
        }
    }
    map.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn givens_implicits_and_derives_are_instances() {
        let maps = maps(&Corpus::of(&[(
            "src/main/scala/shop/Instances.scala",
            "package shop\nobject Instances {\n  given intOrd: Ordering[Int] with\n    def compare(a: Int, b: Int) = a - b\n  given Show[Cart] = c => c.toString\n  implicit val cartEq: Eq[Cart] = Eq.fromUniversalEquals\n  implicit object CartJson extends Encoder[Cart] { def apply(c: Cart) = ??? }\n  implicit def listShow[A: Show]: Show[List[A]] = ???\n  val plain: Show[Int] = ???\n}\ncase class Cart(n: Int) derives Codec, Eq\n",
        )]));
        let m = maps.iter().find(|m| m.id == "scala.instances").unwrap();
        let mut got: Vec<(String, String, String)> = m
            .edges
            .iter()
            .map(|e| {
                (
                    m.nodes[e.from].label.clone(),
                    m.nodes[e.to].label.clone(),
                    e.label.clone(),
                )
            })
            .collect();
        got.sort();
        let e = |a: &str, b: &str, l: &str| (a.to_string(), b.to_string(), l.to_string());
        assert_eq!(
            got,
            [
                e("Cart", "Codec", "derives"),
                e("Cart", "Encoder", "implicit"),
                e("Cart", "Eq", "derives"),
                e("Cart", "Eq", "implicit"),
                e("Cart", "Show", "given"),
                e("Int", "Ordering", "given"),
                e("List[A]", "Show", "implicit"),
            ]
        );
    }
}
