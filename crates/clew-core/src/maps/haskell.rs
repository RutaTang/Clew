//! Haskell's maps:
//!
//! - **type class instances** — each type and the classes it is an
//!   instance of: `instance C T`, `deriving (C, …)` clauses (any strategy)
//!   and standalone `deriving instance C T`;
//! - **effect stacks** — the monad each function runs in, from its type
//!   signature: a transformer stack (`ReaderT Env (StateT S IO)`, its layers
//!   in the detail), a concrete monad (`IO`, `STM`), or an mtl-style
//!   polymorphic `m` and the constraints on it.

use clew_protocol::ProjectMap;
use tree_sitter::Node;

use super::{Corpus, MapBuilder, all_of, line, text};
use crate::highlight::Lang;

pub fn maps(corpus: &Corpus) -> Vec<ProjectMap> {
    let mut out = Vec::new();
    out.extend(instances(corpus));
    out.extend(effects(corpus));
    out
}

/// The type a head names: its constructor (`Box a` → `Box`, `(Maybe a)` →
/// `Maybe`, `[a]` → `[]`).
fn head(node: Node<'_>, src: &str) -> String {
    let mut n = node;
    loop {
        match n.kind() {
            "parens" | "type_patterns" => match n.named_child(0) {
                Some(c) => n = c,
                None => return String::new(),
            },
            "apply" => match n.child_by_field_name("constructor") {
                Some(c) => n = c,
                None => return String::new(),
            },
            "list" => return "[]".into(),
            _ => return text(n, src).trim().to_string(),
        }
    }
}

fn instances(corpus: &Corpus) -> Option<ProjectMap> {
    let mut map = MapBuilder::new(
        "haskell.instances",
        "Type class instances",
        "each type and an arrow to each class it is an instance of — instance declarations, deriving clauses and standalone deriving",
    );
    for source in corpus.in_langs(&[Lang::Haskell]) {
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
        for instance in all_of(root, "instance")
            .into_iter()
            .chain(all_of(root, "deriving_instance"))
        {
            let (Some(class), Some(patterns)) = (
                instance.child_by_field_name("name"),
                instance.child_by_field_name("patterns"),
            ) else {
                continue;
            };
            let how = if instance.kind() == "instance" {
                "instance"
            } else {
                "deriving"
            };
            link(text(class, src), &head(patterns, src), how, line(instance));
        }
        for decl in all_of(root, "data_type")
            .into_iter()
            .chain(all_of(root, "newtype"))
        {
            let Some(name) = decl.child_by_field_name("name") else {
                continue;
            };
            let ty = text(name, src).to_string();
            let mut cursor = decl.walk();
            for deriving in decl.children_by_field_name("deriving", &mut cursor) {
                let Some(classes) = deriving.child_by_field_name("classes") else {
                    continue;
                };
                let names: Vec<String> = match classes.kind() {
                    "name" | "qualified" => vec![text(classes, src).to_string()],
                    _ => all_of(classes, "name")
                        .into_iter()
                        .map(|n| text(n, src).to_string())
                        .collect(),
                };
                for class in names {
                    link(&class, &ty, "deriving", line(deriving));
                }
            }
        }
    }
    // Each type and class where it is declared.
    let defs = corpus.defs();
    for at in 0.. {
        let Some(node) = map.get(at) else {
            break;
        };
        if node.file.is_empty() {
            let label = node.label.clone();
            if let Some(def) = defs.named(&label).iter().find(|d| d.rel.ends_with(".hs")) {
                let (rel, line) = (def.rel.clone(), def.line);
                map.place(Some(at), &rel, line);
            }
        }
    }
    map.finish()
}

/// Monads and transformers a stack is made of.
const MONADS: &[&str] = &[
    "IO",
    "STM",
    "ST",
    "ReaderT",
    "StateT",
    "WriterT",
    "ExceptT",
    "MaybeT",
    "RWST",
    "ContT",
    "Reader",
    "State",
    "Writer",
    "Except",
    "RWS",
    "Eff",
    "Sem",
    "Handler",
    "App",
    "AppM",
    "ResourceT",
    "LoggingT",
    "IdentityT",
];

/// The monad a function's result runs in, as written, and its layers.
fn effect_of(signature: Node<'_>, src: &str) -> Option<(String, Vec<String>)> {
    let mut ty = signature.child_by_field_name("type")?;
    let mut context = None;
    if ty.kind() == "context" {
        context = ty
            .child_by_field_name("context")
            .map(|c| text(c, src).to_string());
        ty = ty.child_by_field_name("type")?;
    }
    while ty.kind() == "function" {
        ty = ty.child_by_field_name("result")?;
    }
    while ty.kind() == "parens" {
        ty = ty.named_child(0)?;
    }
    // `M a`: the monad is the application less its last argument.
    let monad = match ty.kind() {
        "apply" => ty.child_by_field_name("constructor")?,
        _ => return None,
    };
    let head_name = head(monad, src);
    if head_name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_lowercase())
    {
        // `m a` under constraints: mtl style.
        let context = context?;
        if !context.contains("Monad") {
            return None;
        }
        return Some((format!("{context} => {head_name}"), vec![context]));
    }
    if !MONADS.contains(&head_name.as_str()) {
        return None;
    }
    let written = text(monad, src)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    // The layers: each transformer's own arguments, then the one inside.
    let mut layers = Vec::new();
    let mut at = monad;
    loop {
        let mut args = Vec::new();
        let mut n = at;
        while n.kind() == "apply" {
            if let Some(a) = n.child_by_field_name("argument") {
                args.push(a);
            }
            n = n.child_by_field_name("constructor")?;
        }
        args.reverse();
        let name = text(n, src).to_string();
        let inner = args
            .last()
            .copied()
            .filter(|a| a.kind() == "parens" || MONADS.contains(&head(*a, src).as_str()));
        let own: Vec<String> = args
            .iter()
            .filter(|a| Some(**a) != inner)
            .map(|a| text(*a, src).to_string())
            .collect();
        layers.push(match own.is_empty() {
            true => name,
            false => format!("{name} {}", own.join(" ")),
        });
        match inner {
            Some(i) => {
                at = i;
                while at.kind() == "parens" {
                    at = at.named_child(0)?;
                }
            }
            None => break,
        }
    }
    Some((written, layers))
}

fn effects(corpus: &Corpus) -> Option<ProjectMap> {
    let mut map = MapBuilder::new(
        "haskell.effects",
        "Effect stacks",
        "each monad stack a function's type signature runs it in (its layers, outermost first, in the detail), and an arrow from each function to its stack",
    );
    for source in corpus.in_langs(&[Lang::Haskell]) {
        let Some(tree) = source.tree() else {
            continue;
        };
        let src = source.text.as_str();
        for signature in all_of(tree.root_node(), "signature") {
            let Some(name) = signature.child_by_field_name("name") else {
                continue;
            };
            let Some((stack, layers)) = effect_of(signature, src) else {
                continue;
            };
            let s = map.node("stack", &stack, &stack);
            map.detail(s, layers.join(" → "));
            let f = map.node(
                "function",
                &format!("{}:{}", source.rel, line(signature)),
                text(name, src),
            );
            map.place(f, &source.rel, line(signature));
            map.edge(f, s, "runs in");
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

    #[test]
    fn instances_from_declarations_and_deriving() {
        let maps = maps(&Corpus::of(&[(
            "src/Shop.hs",
            "module Shop where\n\ndata Item = Item Int deriving (Show, Eq)\n\nnewtype Sku = Sku Text deriving stock Show\n\ndata Box a = Box a\n\ninstance (Eq a) => Eq (Box a) where\n  Box a == Box b = a == b\n\ninstance Pretty Item where\n  pretty _ = \"item\"\n\nderiving instance Ord Item\n\nclass Pretty a where\n  pretty :: a -> String\n",
        )]));
        let inst = maps.iter().find(|m| m.id == "haskell.instances").unwrap();
        let got = edges(inst);
        for want in [
            e("Item", "Show", "deriving"),
            e("Item", "Eq", "deriving"),
            e("Sku", "Show", "deriving"),
            e("Box", "Eq", "instance"),
            e("Item", "Pretty", "instance"),
            e("Item", "Ord", "deriving"),
        ] {
            assert!(got.contains(&want), "{want:?} in {got:?}");
        }
        let item = inst.nodes.iter().find(|n| n.label == "Item").unwrap();
        assert_eq!((item.file.as_str(), item.line), ("src/Shop.hs", 3));
    }

    #[test]
    fn functions_by_the_monad_they_run_in() {
        let maps = maps(&Corpus::of(&[(
            "src/App.hs",
            "module App where\n\nrun :: Config -> ReaderT Env (StateT S IO) ()\nrun = undefined\n\nfetch :: (MonadIO m, MonadReader Env m) => Text -> m Int\nfetch = undefined\n\nmain :: IO ()\nmain = pure ()\n\npureFn :: Int -> Int\npureFn = id\n\natomic :: STM Bool\natomic = pure True\n",
        )]));
        let eff = maps.iter().find(|m| m.id == "haskell.effects").unwrap();
        let got = edges(eff);
        for want in [
            e("run", "ReaderT Env (StateT S IO)", "runs in"),
            e("fetch", "(MonadIO m, MonadReader Env m) => m", "runs in"),
            e("main", "IO", "runs in"),
            e("atomic", "STM", "runs in"),
        ] {
            assert!(got.contains(&want), "{want:?} in {got:?}");
        }
        assert!(!got.iter().any(|(a, _, _)| a == "pureFn"), "pure: {got:?}");
        let stack = eff
            .nodes
            .iter()
            .find(|n| n.label == "ReaderT Env (StateT S IO)")
            .unwrap();
        assert_eq!(stack.detail, "ReaderT Env → StateT S → IO");
    }
}
