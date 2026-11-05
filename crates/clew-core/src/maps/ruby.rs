//! Ruby's metaprogramming map: the methods a class has that no `def`
//! names, and where they come from.
//!
//! - modules mixed in — `include`, `extend`, `prepend` — and for a concern
//!   (`extend ActiveSupport::Concern`) what its `included do` block adds to
//!   the class and the methods `class_methods do` gives it; a
//!   `self.included(base)` hook's `base.extend(ClassMethods)`;
//! - methods made at load time — `define_method`, `attr_accessor` and
//!   friends, `delegate … to:`, `alias_method`/`alias`;
//! - methods added from outside — `Foo.class_eval do def bar … end end`;
//! - calls by name — `send(:x)`, `public_send(:x)` — to the method they
//!   reach, and the classes that answer any call (`method_missing`,
//!   `respond_to_missing?`).

use clew_protocol::ProjectMap;
use tree_sitter::Node;

use super::{Corpus, MapBuilder, Source, all_of, ancestor, line, text};
use crate::highlight::Lang;

pub fn maps(corpus: &Corpus) -> Vec<ProjectMap> {
    meta(corpus).into_iter().collect()
}

const OWNERS: &[&str] = &["class", "module"];

/// The full name of the class or module `node` is in: `Admin::User`.
fn owner_of<'t>(node: Node<'t>, src: &str) -> Option<(String, Node<'t>)> {
    let first = ancestor(node, OWNERS)?;
    let mut names = Vec::new();
    let mut at = Some(first);
    while let Some(o) = at {
        if let Some(n) = o.child_by_field_name("name") {
            names.push(text(n, src).to_string());
        }
        at = ancestor(o, OWNERS);
    }
    names.reverse();
    Some((names.join("::"), first))
}

/// A symbol's or a string's name: `:foo` and `"foo"` → `foo`; an
/// interpolated string as written (`#{n}?`).
fn name_literal(node: Node<'_>, src: &str) -> Option<String> {
    match node.kind() {
        "simple_symbol" => Some(text(node, src).trim_start_matches(':').to_string()),
        "string" | "delimited_symbol" => {
            let t = text(node, src);
            let t = t.trim_start_matches(':');
            Some(t.trim_matches(|c| c == '"' || c == '\'').to_string())
        }
        "identifier" | "constant" => Some(text(node, src).to_string()),
        _ => None,
    }
}

fn call_args<'t>(call: Node<'t>) -> Vec<Node<'t>> {
    call.child_by_field_name("arguments")
        .map(|a| {
            let mut c = a.walk();
            a.named_children(&mut c).collect()
        })
        .unwrap_or_default()
}

/// Whether `call` is a statement directly in a class or module body (or in
/// a block such as `included do`), not inside a method.
fn at_class_level(call: Node<'_>) -> bool {
    ancestor(call, &["method", "singleton_method", "class", "module"])
        .is_some_and(|a| OWNERS.contains(&a.kind()))
}

fn meta(corpus: &Corpus) -> Option<ProjectMap> {
    let mut map = MapBuilder::new(
        "ruby.meta",
        "Metaprogramming",
        "the methods a class has that no def names, and where they come from: modules mixed in, concerns' included blocks and class_methods, define_method, attr_*, delegate, alias, class_eval; calls made by name (send) and the classes that answer any call (method_missing)",
    );
    let defs = corpus.defs();
    let sources: Vec<(&Source, tree_sitter::Tree)> = corpus
        .in_langs(&[Lang::Ruby])
        .filter_map(|s| Some((s, s.tree()?)))
        .collect();
    for (source, tree) in &sources {
        let src = source.text.as_str();
        let rel = source.rel.as_str();
        let root = tree.root_node();
        let owner_node = |map: &mut MapBuilder, name: &str, decl: Node<'_>| {
            let kind = if decl.kind() == "module" {
                "module"
            } else {
                "class"
            };
            let at = map.node(kind, name, name);
            map.place(at, rel, line(decl));
            at
        };
        let method_node = |map: &mut MapBuilder, owner: &str, sep: &str, name: &str, at: usize| {
            let key = format!("{owner}{sep}{name}");
            let n = map.node("method", &key, &key);
            map.place(n, rel, at);
            n
        };
        for call in all_of(root, "call") {
            let Some(method) = call.child_by_field_name("method").map(|m| text(m, src)) else {
                continue;
            };
            let receiver = call.child_by_field_name("receiver");
            let args = call_args(call);
            let at = line(call);
            // `Foo.class_eval do … end`: methods added from outside.
            if matches!(
                method,
                "class_eval" | "module_eval" | "class_exec" | "instance_eval"
            ) && let Some(r) =
                receiver.filter(|r| matches!(r.kind(), "constant" | "scope_resolution"))
            {
                let target = text(r, src).to_string();
                let to = map.node("class", &target, &target);
                if let Some(def) = defs.find(&target, None, rel) {
                    let (r, l) = (def.rel.clone(), def.line);
                    map.place(to, &r, l);
                }
                let sep = if method == "instance_eval" { "." } else { "#" };
                let block = call.child_by_field_name("block");
                let mut any = false;
                for def in block.map(|b| all_of(b, "method")).unwrap_or_default() {
                    if let Some(name) = def.child_by_field_name("name") {
                        let m = method_node(&mut map, &target, sep, text(name, src), line(def));
                        map.edge(to, m, method);
                        any = true;
                    }
                }
                if !any {
                    map.add_detail(to, &format!("{method} at {rel}:{at}"));
                }
                continue;
            }
            if receiver.is_some()
                && !matches!(
                    method,
                    "send" | "public_send" | "__send__" | "extend" | "include"
                )
            {
                continue;
            }
            let Some((owner, decl)) = owner_of(call, src) else {
                continue;
            };
            match method {
                "include" | "extend" | "prepend" if at_class_level(call) && receiver.is_none() => {
                    for a in &args {
                        if !matches!(a.kind(), "constant" | "scope_resolution") {
                            continue;
                        }
                        let module = text(*a, src).to_string();
                        let from = owner_node(&mut map, &owner, decl);
                        if module == "ActiveSupport::Concern" {
                            map.add_detail(from, "concern");
                            continue;
                        }
                        let to = map.node("module", &module, &module);
                        if let Some(def) =
                            defs.find(module.rsplit("::").next().unwrap_or(&module), None, rel)
                        {
                            let (r, l) = (def.rel.clone(), def.line);
                            map.place(to, &r, l);
                        }
                        map.edge(from, to, method);
                    }
                }
                // `def self.included(base); base.extend(ClassMethods)`.
                "extend" | "include"
                    if receiver.is_some()
                        && ancestor(call, &["singleton_method"])
                            .and_then(|m| m.child_by_field_name("name"))
                            .is_some_and(|n| text(n, src) == "included") =>
                {
                    for a in &args {
                        if !matches!(a.kind(), "constant" | "scope_resolution") {
                            continue;
                        }
                        let module = text(*a, src).to_string();
                        let full = format!("{owner}::{module}");
                        let from = owner_node(&mut map, &owner, decl);
                        let to = map.node("module", &full, &full);
                        if let Some(def) = defs.find(&module, None, rel) {
                            let (r, l) = (def.rel.clone(), def.line);
                            map.place(to, &r, l);
                        }
                        map.edge(from, to, &format!("{method}s the includer"));
                    }
                }
                "define_method" | "define_singleton_method" => {
                    let Some(name) = args.first().and_then(|a| name_literal(*a, src)) else {
                        continue;
                    };
                    let sep = if method == "define_method" { "#" } else { "." };
                    let from = owner_node(&mut map, &owner, decl);
                    let m = method_node(&mut map, &owner, sep, &name, at);
                    map.edge(from, m, method);
                }
                "attr_accessor" | "attr_reader" | "attr_writer" | "cattr_accessor"
                | "mattr_accessor" => {
                    let from = owner_node(&mut map, &owner, decl);
                    for a in &args {
                        let Some(name) = name_literal(*a, src) else {
                            continue;
                        };
                        let mut made = Vec::new();
                        if method != "attr_writer" {
                            made.push(name.clone());
                        }
                        if method != "attr_reader" {
                            made.push(format!("{name}="));
                        }
                        for m in made {
                            let n = method_node(&mut map, &owner, "#", &m, at);
                            map.edge(from, n, method);
                        }
                    }
                }
                "delegate" => {
                    let to_target = args
                        .iter()
                        .filter(|a| a.kind() == "pair")
                        .find(|p| {
                            p.child_by_field_name("key")
                                .is_some_and(|k| text(k, src).trim_end_matches(':') == "to")
                        })
                        .and_then(|p| p.child_by_field_name("value"))
                        .and_then(|v| name_literal(v, src))
                        .unwrap_or_default();
                    let from = owner_node(&mut map, &owner, decl);
                    for a in args.iter().filter(|a| a.kind() != "pair") {
                        if let Some(name) = name_literal(*a, src) {
                            let n = method_node(&mut map, &owner, "#", &name, at);
                            map.edge(from, n, &format!("delegate to {to_target}"));
                        }
                    }
                }
                "alias_method" => {
                    let (Some(new), Some(old)) = (
                        args.first().and_then(|a| name_literal(*a, src)),
                        args.get(1).and_then(|a| name_literal(*a, src)),
                    ) else {
                        continue;
                    };
                    let from = owner_node(&mut map, &owner, decl);
                    let n = method_node(&mut map, &owner, "#", &new, at);
                    map.edge(from, n, &format!("alias of {old}"));
                }
                "included" | "class_methods" | "prepended"
                    if call.child_by_field_name("block").is_some() =>
                {
                    let block = call.child_by_field_name("block").unwrap();
                    let from = owner_node(&mut map, &owner, decl);
                    if method == "class_methods" {
                        for def in all_of(block, "method") {
                            if let Some(name) = def.child_by_field_name("name") {
                                let n =
                                    method_node(&mut map, &owner, ".", text(name, src), line(def));
                                map.edge(from, n, "class_methods");
                            }
                        }
                        continue;
                    }
                    // What the block runs in the class that includes it.
                    let body = block.child_by_field_name("body").unwrap_or(block);
                    let mut c = body.walk();
                    for statement in body.named_children(&mut c).filter(|s| s.kind() == "call") {
                        let shown = text(statement, src)
                            .lines()
                            .next()
                            .unwrap_or("")
                            .trim()
                            .to_string();
                        map.add_detail(from, &format!("{method}: {shown}"));
                    }
                }
                "send" | "public_send" | "__send__" => {
                    let Some(name) = args.first().and_then(|a| name_literal(*a, src)) else {
                        continue;
                    };
                    if args.first().is_some_and(|a| a.kind() == "identifier") {
                        // `send(name)`: a name only known at run time.
                        continue;
                    }
                    let caller = ancestor(call, &["method", "singleton_method"]);
                    let from = match caller.and_then(|m| m.child_by_field_name("name")) {
                        Some(n) => {
                            let sep = if caller.is_some_and(|c| c.kind() == "singleton_method") {
                                "."
                            } else {
                                "#"
                            };
                            let key = format!("{owner}{sep}{}", text(n, src));
                            let f = map.node("method", &key, &key);
                            map.place(f, rel, line(caller.unwrap()));
                            f
                        }
                        None => owner_node(&mut map, &owner, decl),
                    };
                    // To self's method when there is no receiver, else to a
                    // method of that name wherever it is.
                    let target = match receiver {
                        None => Some(owner.as_str()),
                        Some(_) => None,
                    };
                    let def = defs
                        .find(
                            &name,
                            target.map(|t| t.rsplit("::").next().unwrap_or(t)),
                            rel,
                        )
                        .or_else(|| defs.find(&name, None, rel));
                    let key = match (target, def.and_then(|d| d.class.clone())) {
                        (Some(t), _) => format!("{t}#{name}"),
                        (None, Some(class)) => format!("{class}#{name}"),
                        (None, None) => format!("#{name}"),
                    };
                    let to = map.node("method", &key, &key);
                    if let Some(d) = def {
                        let (r, l) = (d.rel.clone(), d.line);
                        map.place(to, &r, l);
                    }
                    map.edge(from, to, method);
                }
                _ => {}
            }
        }
        for alias in all_of(root, "alias") {
            let (Some(new), Some(old)) = (alias.named_child(0), alias.named_child(1)) else {
                continue;
            };
            let Some((owner, decl)) = owner_of(alias, src) else {
                continue;
            };
            let from = owner_node(&mut map, &owner, decl);
            let n = method_node(
                &mut map,
                &owner,
                "#",
                text(new, src).trim_start_matches(':'),
                line(alias),
            );
            map.edge(
                from,
                n,
                &format!("alias of {}", text(old, src).trim_start_matches(':')),
            );
        }
        for def in all_of(root, "method") {
            let Some(name) = def.child_by_field_name("name").map(|n| text(n, src)) else {
                continue;
            };
            if !matches!(name, "method_missing" | "respond_to_missing?") {
                continue;
            }
            let Some((owner, decl)) = owner_of(def, src) else {
                continue;
            };
            let from = owner_node(&mut map, &owner, decl);
            let n = method_node(&mut map, &owner, "#", name, line(def));
            map.detail(n, "answers calls to methods the class does not define");
            map.edge(from, n, name);
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

    const APP: &[(&str, &str)] = &[
        (
            "app/models/user.rb",
            "class User < ApplicationRecord\n  include Auditable\n  extend Finders\n  prepend Loud\n  [:admin, :guest].each { |n| define_method(\"#{n}?\") { role == n } }\n  define_method(:greet) do\n    \"hi\"\n  end\n  attr_accessor :token\n  attr_reader :age\n  delegate :name, :email, to: :profile\n  alias_method :login, :name\n  alias nick name\n\n  def method_missing(name, *args)\n    super\n  end\n\n  def go\n    public_send(:greet)\n    profile.send(:refresh, 1)\n    send(dynamic)\n  end\nend\n",
        ),
        (
            "app/models/concerns/auditable.rb",
            "module Auditable\n  extend ActiveSupport::Concern\n  included do\n    has_many :audits\n    before_save :stamp\n  end\n  class_methods do\n    def audited\n    end\n  end\nend\n",
        ),
        (
            "lib/finders.rb",
            "module Finders\n  def self.included(base)\n    base.extend(ClassMethods)\n  end\n  module ClassMethods\n    def find_all; end\n  end\nend\n",
        ),
        (
            "app/models/profile.rb",
            "class Profile\n  def refresh(n)\n  end\nend\n",
        ),
        (
            "config/initializers/patch.rb",
            "User.class_eval do\n  def shout\n  end\nend\n",
        ),
    ];

    #[test]
    fn methods_no_def_names() {
        let maps = maps(&Corpus::of(APP));
        let meta = maps.iter().find(|m| m.id == "ruby.meta").unwrap();
        let got = edges(meta);
        for want in [
            e("User", "Auditable", "include"),
            e("User", "Finders", "extend"),
            e("User", "Loud", "prepend"),
            e("User", "User##{n}?", "define_method"),
            e("User", "User#greet", "define_method"),
            e("User", "User#token", "attr_accessor"),
            e("User", "User#token=", "attr_accessor"),
            e("User", "User#age", "attr_reader"),
            e("User", "User#name", "delegate to profile"),
            e("User", "User#email", "delegate to profile"),
            e("User", "User#login", "alias of name"),
            e("User", "User#nick", "alias of name"),
            e("User", "User#method_missing", "method_missing"),
            e("User#go", "User#greet", "public_send"),
            e("User#go", "Profile#refresh", "send"),
            e("Auditable", "Auditable.audited", "class_methods"),
            e("Finders", "Finders::ClassMethods", "extends the includer"),
            e("User", "User#shout", "class_eval"),
        ] {
            assert!(got.contains(&want), "{want:?} in {got:?}");
        }
        assert!(
            !got.iter().any(|(_, b, _)| b.contains("dynamic")),
            "{got:?}"
        );
        assert!(
            !got.iter()
                .any(|(_, b, l)| b == "User#age=" || l == "attr_writer"),
            "{got:?}"
        );
        let node = |label: &str| meta.nodes.iter().find(|n| n.label == label).unwrap();
        let auditable = node("Auditable");
        assert_eq!(auditable.file, "app/models/concerns/auditable.rb");
        assert!(auditable.detail.contains("concern"), "{}", auditable.detail);
        assert!(
            auditable.detail.contains("included: has_many :audits"),
            "{}",
            auditable.detail
        );
        assert!(
            auditable.detail.contains("included: before_save :stamp"),
            "{}",
            auditable.detail
        );
        assert_eq!(
            (node("User#greet").file.as_str(), node("User#greet").line),
            ("app/models/user.rb", 6)
        );
        assert_eq!(
            (
                node("Profile#refresh").file.as_str(),
                node("Profile#refresh").line
            ),
            ("app/models/profile.rb", 2)
        );
        assert_eq!(
            (node("User#shout").file.as_str(), node("User#shout").line),
            ("config/initializers/patch.rb", 2)
        );
        assert_eq!(node("User").file, "app/models/user.rb");
        assert_eq!(node("Finders::ClassMethods").line, 5);
    }
}
