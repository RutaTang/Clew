//! Elixir's maps, as Erlang's are read for the same runtime:
//!
//! - **supervision tree** — each supervisor (`use Supervisor`) and
//!   application (`use Application`) and the children it starts: the list
//!   its `init/1` or `start/2` hands `Supervisor.init` or
//!   `Supervisor.start_link` (`Shop.Repo`, `{Shop.Cart, opts}`, a map's
//!   `start: {Shop.Pay, :start_link, []}`, `Supervisor.child_spec({…}, …)`),
//!   with its strategy; and what a `DynamicSupervisor.start_child` starts
//!   under one;
//! - **GenServer calls** — each `GenServer.call`/`cast` to a module
//!   (`__MODULE__`, one by name, or a pid of the module it is written in),
//!   an arrow to the `handle_call`/`handle_cast` clause that answers its
//!   request;
//! - **protocols and behaviours** — each protocol and the types
//!   `defimpl` implements it for, each behaviour (a module of `@callback`s,
//!   or one a `@behaviour` names) and the modules that adopt it.

use clew_protocol::ProjectMap;
use tree_sitter::Node;

use super::{Corpus, MapBuilder, Source, all_of, line, text};
use crate::highlight::Lang;

pub fn maps(corpus: &Corpus) -> Vec<ProjectMap> {
    let sources: Vec<(&Source, tree_sitter::Tree)> = corpus
        .in_langs(&[Lang::Elixir])
        .filter_map(|s| Some((s, s.tree()?)))
        .collect();
    let modules: Vec<Module> = sources
        .iter()
        .flat_map(|(source, tree)| Module::all_in(source, tree.root_node()))
        .collect();
    let mut out = Vec::new();
    out.extend(supervision(&modules));
    out.extend(calls(&modules));
    out.extend(protocols(&sources, &modules));
    out
}

/// A `defmodule`, by its full name (a nested one's with its parents').
struct Module<'a> {
    source: &'a Source,
    name: String,
    line: usize,
    /// Its `do` block.
    body: Node<'a>,
    /// What it `use`s (`Supervisor`, `GenServer`), as written.
    uses: Vec<String>,
    /// What its `@behaviour`s name, as written.
    behaviours: Vec<String>,
    /// Whether it declares `@callback`s: it is a behaviour.
    callbacks: bool,
}

/// The name of the call `node` is (`def`, `use`, `defmodule`), when its
/// target is a plain identifier.
fn call_name<'s>(node: Node<'_>, src: &'s str) -> Option<&'s str> {
    let target = node.child_by_field_name("target")?;
    (node.kind() == "call" && target.kind() == "identifier").then(|| text(target, src))
}

/// A call's arguments node.
fn arguments(node: Node<'_>) -> Option<Node<'_>> {
    node.named_children(&mut node.walk())
        .find(|c| c.kind() == "arguments")
}

/// A call's `do` block.
fn do_block(node: Node<'_>) -> Option<Node<'_>> {
    node.named_children(&mut node.walk())
        .find(|c| c.kind() == "do_block")
}

/// `Mod.fun(…)`: `(Mod, fun)`.
fn remote<'s>(node: Node<'_>, src: &'s str) -> Option<(&'s str, &'s str)> {
    let target = node.child_by_field_name("target")?;
    if node.kind() != "call" || target.kind() != "dot" {
        return None;
    }
    let left = target.child_by_field_name("left")?;
    let right = target.child_by_field_name("right")?;
    (left.kind() == "alias").then(|| (text(left, src), text(right, src)))
}

/// The value of `key:` among a call's keyword arguments (or a list's).
fn keyword<'t>(node: Node<'t>, key: &str, src: &str) -> Option<Node<'t>> {
    all_of(node, "pair").into_iter().find_map(|pair| {
        let k = pair.child_by_field_name("key")?;
        (text(k, src).trim().trim_end_matches(':').trim() == key)
            .then(|| pair.child_by_field_name("value"))
            .flatten()
    })
}

impl<'a> Module<'a> {
    fn all_in(source: &'a Source, root: Node<'a>) -> Vec<Module<'a>> {
        let src = source.text.as_str();
        all_of(root, "call")
            .into_iter()
            .filter(|c| call_name(*c, src) == Some("defmodule"))
            .filter_map(|def| {
                let own = arguments(def)?.named_child(0)?;
                if own.kind() != "alias" {
                    return None;
                }
                // `defmodule Inner` in `defmodule Shop` is `Shop.Inner`.
                let mut parts = vec![text(own, src).to_string()];
                let mut up = def.parent();
                while let Some(n) = up {
                    if call_name(n, src) == Some("defmodule")
                        && let Some(outer) = arguments(n).and_then(|a| a.named_child(0))
                    {
                        parts.push(text(outer, src).to_string());
                    }
                    up = n.parent();
                }
                parts.reverse();
                let body = do_block(def)?;
                let (mut uses, mut behaviours, mut callbacks) = (Vec::new(), Vec::new(), false);
                for statement in body.named_children(&mut body.walk()) {
                    let (call, attribute) = match statement.kind() {
                        "unary_operator" => (statement.child_by_field_name("operand"), true),
                        _ => (Some(statement), false),
                    };
                    let Some(call) = call else { continue };
                    let first = arguments(call)
                        .and_then(|a| a.named_child(0))
                        .filter(|a| a.kind() == "alias")
                        .map(|a| text(a, src).to_string());
                    match (attribute, call_name(call, src)) {
                        (false, Some("use")) => uses.extend(first),
                        (true, Some("behaviour")) => behaviours.extend(first),
                        (true, Some("callback" | "macrocallback")) => callbacks = true,
                        _ => {}
                    }
                }
                Some(Module {
                    source,
                    name: parts.join("."),
                    line: line(def),
                    body,
                    uses,
                    behaviours,
                    callbacks,
                })
            })
            .collect()
    }

    fn src(&self) -> &'a str {
        self.source.text.as_str()
    }

    fn uses(&self, what: &str) -> bool {
        self.uses
            .iter()
            .any(|u| u == what || u.ends_with(&format!(".{what}")))
    }

    /// The clauses of `def name(…)` in the module's own body — not a
    /// nested module's.
    fn clauses(&self, name: &str) -> Vec<Node<'a>> {
        let src = self.src();
        self.body
            .named_children(&mut self.body.walk())
            .filter(|c| matches!(call_name(*c, src), Some("def" | "defp")))
            .filter(|c| def_name(*c, src).as_deref() == Some(name))
            .collect()
    }

    /// The module named `written` from inside this one: itself for
    /// `__MODULE__`, else the project's module of that name, or the one
    /// whose name ends in it (`alias Shop.Cart` lets `Cart` say it).
    fn resolve<'m>(&self, written: &str, modules: &'m [Module<'a>]) -> Option<&'m Module<'a>> {
        let written = written.trim();
        if written == "__MODULE__" {
            return modules.iter().find(|m| m.name == self.name);
        }
        modules.iter().find(|m| m.name == written).or_else(|| {
            let mut ends = modules
                .iter()
                .filter(|m| m.name.ends_with(&format!(".{written}")));
            let one = ends.next()?;
            ends.next().is_none().then_some(one)
        })
    }
}

/// The function a `def` clause defines, and its head (the call with its
/// parameters).
fn def_head<'t>(def: Node<'t>, src: &str) -> Option<(String, Node<'t>)> {
    let mut head = arguments(def)?.named_child(0)?;
    if head.kind() == "binary_operator" {
        head = head.child_by_field_name("left")?;
    }
    let name = match head.kind() {
        "call" => head.child_by_field_name("target")?,
        "identifier" => head,
        _ => return None,
    };
    Some((text(name, src).to_string(), head))
}

fn def_name(def: Node<'_>, src: &str) -> Option<String> {
    def_head(def, src).map(|(name, _)| name)
}

/// A message's tag: a tuple's first atom (`{:add, x}`), or an atom.
fn tag(node: Node<'_>, src: &str) -> Option<String> {
    let atom = match node.kind() {
        "tuple" => node.named_child(0)?,
        _ => node,
    };
    (atom.kind() == "atom").then(|| text(atom, src).trim_start_matches(':').to_string())
}

/// The modules a children list names, each with the node that names it.
fn children<'t>(list: Node<'t>, src: &str) -> Vec<(String, Node<'t>)> {
    fn child<'t>(node: Node<'t>, src: &str) -> Option<String> {
        match node.kind() {
            "alias" => Some(text(node, src).to_string()),
            // `{Shop.Cart, opts}`.
            "tuple" => node
                .named_child(0)
                .filter(|n| n.kind() == "alias")
                .map(|n| text(n, src).to_string()),
            // `%{id: …, start: {Shop.Pay, :start_link, []}}`.
            "map" => keyword(node, "start", src).and_then(|start| child(start, src)),
            // `Supervisor.child_spec({Shop.Stock, []}, id: :s)`.
            "call" => {
                let (_, fun) = remote(node, src)?;
                (fun == "child_spec")
                    .then(|| arguments(node)?.named_child(0))
                    .flatten()
                    .and_then(|spec| child(spec, src))
            }
            _ => None,
        }
    }
    list.named_children(&mut list.walk())
        .filter_map(|n| Some((child(n, src)?, n)))
        .collect()
}

fn supervision(modules: &[Module]) -> Option<ProjectMap> {
    let mut map = MapBuilder::new(
        "elixir.supervision",
        "Supervision tree",
        "each supervisor and application and an arrow to each child it starts (labelled worker or supervisor), and from each DynamicSupervisor to what is started under it",
    );
    let place = |map: &mut MapBuilder, from: &Module, name: &str| -> Option<usize> {
        let known = from.resolve(name, modules);
        let full = known.map_or(name, |m| m.name.as_str());
        let at = map.node("process", full, full);
        if let Some(m) = known {
            map.place(at, &m.source.rel, m.line);
            if (m.uses("Supervisor") || m.uses("DynamicSupervisor"))
                && map
                    .get(at.unwrap_or(usize::MAX))
                    .is_some_and(|n| n.detail.is_empty())
            {
                map.detail(at, "supervisor");
            }
        }
        at
    };
    for m in modules {
        let src = m.src();
        let (callback, role) = if m.uses("Application") {
            ("start", "application")
        } else if m.uses("Supervisor") {
            ("init", "supervisor")
        } else {
            continue;
        };
        let at = place(&mut map, m, &m.name);
        map.detail(at, role);
        for clause in m.clauses(callback) {
            for call in all_of(clause, "call") {
                let Some(("Supervisor", "init" | "start_link")) = remote(call, src) else {
                    continue;
                };
                let Some(args) = arguments(call) else {
                    continue;
                };
                let Some(first) = args.named_child(0) else {
                    continue;
                };
                // The list itself, or the variable bound to it in the clause.
                let list = match first.kind() {
                    "list" => Some(first),
                    "identifier" => all_of(clause, "binary_operator").into_iter().find_map(|b| {
                        let left = b.child_by_field_name("left")?;
                        let right = b.child_by_field_name("right")?;
                        (text(left, src) == text(first, src) && right.kind() == "list")
                            .then_some(right)
                    }),
                    _ => None,
                };
                if let Some(strategy) = keyword(args, "strategy", src) {
                    let strategy = text(strategy, src).trim_start_matches(':');
                    map.detail(at, format!("{role} ({strategy})"));
                }
                for (child, _) in list.map(|l| children(l, src)).unwrap_or_default() {
                    let to = place(&mut map, m, &child);
                    let supervises = m
                        .resolve(&child, modules)
                        .is_some_and(|c| c.uses("Supervisor") || c.uses("DynamicSupervisor"));
                    map.edge(at, to, if supervises { "supervisor" } else { "worker" });
                }
            }
        }
    }
    // `DynamicSupervisor.start_child(Sup, {Mod, …})`, wherever it is called.
    for m in modules {
        let src = m.src();
        for call in all_of(m.body, "call") {
            let Some(("DynamicSupervisor", "start_child")) = remote(call, src) else {
                continue;
            };
            let Some(args) = arguments(call) else {
                continue;
            };
            let (Some(sup), Some(spec)) = (args.named_child(0), args.named_child(1)) else {
                continue;
            };
            let sup = match sup.kind() {
                "alias" => text(sup, src),
                "identifier" if text(sup, src) == "__MODULE__" => "__MODULE__",
                _ => continue,
            };
            let Some((child, _)) = children_of_one(spec, src) else {
                continue;
            };
            let from = place(&mut map, m, sup);
            if map
                .get(from.unwrap_or(usize::MAX))
                .is_some_and(|n| n.detail.is_empty())
            {
                map.detail(from, "supervisor");
            }
            let to = place(&mut map, m, &child);
            map.edge(from, to, "starts");
        }
    }
    map.finish()
}

/// The module one child spec starts.
fn children_of_one<'t>(spec: Node<'t>, src: &str) -> Option<(String, Node<'t>)> {
    let tuple_list = spec.parent()?;
    children(tuple_list, src)
        .into_iter()
        .find(|(_, n)| n.id() == spec.id())
}

fn calls(modules: &[Module]) -> Option<ProjectMap> {
    let mut map = MapBuilder::new(
        "elixir.calls",
        "GenServer calls",
        "each function that calls or casts to a GenServer, an arrow to the handle_call or handle_cast clause that answers its request",
    );
    for m in modules {
        let src = m.src();
        for call in all_of(m.body, "call") {
            let kind = match remote(call, src) {
                Some(("GenServer", "call")) => "call",
                Some(("GenServer", "cast")) => "cast",
                _ => continue,
            };
            let Some(args) = arguments(call) else {
                continue;
            };
            let (Some(server), Some(message)) = (args.named_child(0), args.named_child(1)) else {
                continue;
            };
            let Some(t) = tag(message, src) else { continue };
            let callback = if kind == "call" {
                "handle_call"
            } else {
                "handle_cast"
            };
            let answers = |target: &Module<'_>| answer(target, callback, &t).is_some();
            // A pid in a variable is the module the call is written in when
            // that module answers the request (a client API's `pid`).
            let target = match server.kind() {
                "alias" | "identifier" if text(server, src) == "__MODULE__" => {
                    m.resolve("__MODULE__", modules)
                }
                "alias" => m.resolve(text(server, src), modules),
                _ => m.resolve("__MODULE__", modules).filter(|own| answers(own)),
            };
            let Some(target) = target else { continue };
            let Some(clause) = answer(target, callback, &t) else {
                continue;
            };
            // The function the call is in.
            let Some(def) = std::iter::successors(call.parent(), |n| n.parent())
                .find(|n| matches!(call_name(*n, src), Some("def" | "defp")))
            else {
                continue;
            };
            let Some(function) = def_name(def, src) else {
                continue;
            };
            let from_label = format!("{}.{function}", m.name);
            let from = map.node("function", &from_label, &from_label);
            map.place(from, &m.source.rel, line(call));
            let to_label = format!("{}.{callback}(:{t})", target.name);
            let to = map.node("clause", &to_label, &to_label);
            map.place(to, &target.source.rel, line(clause));
            map.edge(from, to, kind);
        }
    }
    map.finish()
}

/// The `callback` clause of `target` that answers a request tagged `t`.
fn answer<'a>(target: &Module<'a>, callback: &str, t: &str) -> Option<Node<'a>> {
    let src = target.src();
    target.clauses(callback).into_iter().find(|clause| {
        def_head(*clause, src)
            .and_then(|(_, head)| arguments(head)?.named_child(0))
            .and_then(|pattern| tag(pattern, src))
            .as_deref()
            == Some(t)
    })
}

fn protocols(sources: &[(&Source, tree_sitter::Tree)], modules: &[Module]) -> Option<ProjectMap> {
    let mut map = MapBuilder::new(
        "elixir.protocols",
        "Protocols and behaviours",
        "each protocol and an arrow to each type it is implemented for, and each behaviour and an arrow to each module that adopts it",
    );
    // Protocols, and what implements them.
    for (source, tree) in sources {
        let src = source.text.as_str();
        for call in all_of(tree.root_node(), "call") {
            match call_name(call, src) {
                Some("defprotocol") => {
                    let Some(name) = arguments(call).and_then(|a| a.named_child(0)) else {
                        continue;
                    };
                    let name = text(name, src);
                    let at = map.node("protocol", name, name);
                    map.place(at, &source.rel, line(call));
                }
                Some("defimpl") => {
                    let Some(args) = arguments(call) else {
                        continue;
                    };
                    let Some(protocol) = args.named_child(0).filter(|p| p.kind() == "alias") else {
                        continue;
                    };
                    // `for: Type`, `for: [A, B]`, or the module it is in.
                    let types: Vec<String> = match keyword(args, "for", src) {
                        Some(list) if list.kind() == "list" => list
                            .named_children(&mut list.walk())
                            .map(|t| text(t, src).to_string())
                            .collect(),
                        Some(one) => vec![text(one, src).to_string()],
                        None => std::iter::successors(call.parent(), |n| n.parent())
                            .find(|n| call_name(*n, src) == Some("defmodule"))
                            .and_then(|m| arguments(m)?.named_child(0))
                            .map(|m| vec![text(m, src).to_string()])
                            .unwrap_or_default(),
                    };
                    let protocol = text(protocol, src);
                    let from = map.node("protocol", protocol, protocol);
                    for t in types {
                        let to = map.node("type", &t, &t);
                        map.place(to, &source.rel, line(call));
                        map.edge(from, to, "implemented for");
                    }
                }
                _ => {}
            }
        }
    }
    // Behaviours: a module of `@callback`s, and every `@behaviour`.
    for m in modules.iter().filter(|m| m.callbacks) {
        let at = map.node("behaviour", &m.name, &m.name);
        map.place(at, &m.source.rel, m.line);
    }
    for m in modules {
        for written in &m.behaviours {
            let name = m
                .resolve(written, modules)
                .map_or(written.as_str(), |b| b.name.as_str());
            let from = map.node("behaviour", name, name);
            if let Some(b) = m.resolve(written, modules) {
                map.place(from, &b.source.rel, b.line);
            }
            let to = map.node("module", &m.name, &m.name);
            map.place(to, &m.source.rel, m.line);
            map.edge(from, to, "adopted by");
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

    fn node<'m>(map: &'m ProjectMap, label: &str) -> &'m clew_protocol::MapNode {
        map.nodes
            .iter()
            .find(|n| n.label == label)
            .unwrap_or_else(|| panic!("{label} in {:?}", map.nodes))
    }

    const SHOP: &[(&str, &str)] = &[
        (
            "lib/shop/application.ex",
            "defmodule Shop.Application do\n  use Application\n\n  def start(_type, _args) do\n    children = [\n      Shop.Repo,\n      {Shop.Cart, name: Shop.Cart},\n      %{id: :pay, start: {Shop.Pay, :start_link, []}},\n      Supervisor.child_spec({Shop.Stock, []}, id: :stock),\n      Shop.Workers\n    ]\n    Supervisor.start_link(children, strategy: :one_for_one, name: Shop.Supervisor)\n  end\nend\n",
        ),
        (
            "lib/shop/workers.ex",
            "defmodule Shop.Workers do\n  use DynamicSupervisor\n  def init(_), do: DynamicSupervisor.init(strategy: :one_for_one)\n  def start_job(arg), do: DynamicSupervisor.start_child(__MODULE__, {Shop.Job, arg})\nend\n",
        ),
        (
            "lib/shop/cart.ex",
            "defmodule Shop.Cart do\n  use GenServer\n  @behaviour Shop.Store\n\n  def add(x), do: GenServer.call(__MODULE__, {:add, x})\n  def total(pid), do: GenServer.call(pid, :total)\n  def clear, do: GenServer.cast(__MODULE__, :clear)\n\n  def handle_call({:add, x}, _from, s), do: {:reply, :ok, [x | s]}\n  def handle_call(:total, _from, s), do: {:reply, Enum.sum(s), s}\n  def handle_cast(:clear, _s), do: {:noreply, []}\nend\n",
        ),
        (
            "lib/shop/store.ex",
            "defmodule Shop.Store do\n  @callback put(term) :: :ok\nend\n",
        ),
        (
            "lib/shop/shape.ex",
            "defprotocol Shop.Shape do\n  def area(s)\nend\n\ndefimpl Shop.Shape, for: Shop.Circle do\n  def area(c), do: c.r * c.r\nend\n\ndefimpl Shop.Shape, for: [Integer, Float] do\n  def area(n), do: n\nend\n\ndefmodule Shop.Square do\n  defstruct [:side]\n  defimpl Shop.Shape do\n    def area(s), do: s.side * s.side\n  end\nend\n",
        ),
        (
            "lib/shop/reporter.ex",
            "defmodule Shop.Reporter do\n  def report, do: GenServer.call(Shop.Cart, :total)\nend\n",
        ),
    ];

    /// An application's children, a DynamicSupervisor's, and their
    /// strategies.
    #[test]
    fn the_supervision_tree_is_what_applications_and_supervisors_start() {
        let maps = maps(&Corpus::of(SHOP));
        let tree = maps.iter().find(|m| m.id == "elixir.supervision").unwrap();
        let got = edges(tree);
        for want in [
            e("Shop.Application", "Shop.Repo", "worker"),
            e("Shop.Application", "Shop.Cart", "worker"),
            e("Shop.Application", "Shop.Pay", "worker"),
            e("Shop.Application", "Shop.Stock", "worker"),
            e("Shop.Application", "Shop.Workers", "supervisor"),
            e("Shop.Workers", "Shop.Job", "starts"),
        ] {
            assert!(got.contains(&want), "{want:?} in {got:?}");
        }
        assert_eq!(
            node(tree, "Shop.Application").detail,
            "application (one_for_one)"
        );
        assert_eq!(node(tree, "Shop.Workers").detail, "supervisor");
        assert_eq!(node(tree, "Shop.Cart").file, "lib/shop/cart.ex");
    }

    /// Each GenServer call or cast, to the clause that answers it: by
    /// `__MODULE__`, a pid of the module it is in, or the module's name.
    #[test]
    fn a_genserver_call_reaches_the_clause_that_answers_it() {
        let maps = maps(&Corpus::of(SHOP));
        let calls = maps.iter().find(|m| m.id == "elixir.calls").unwrap();
        let got = edges(calls);
        for want in [
            e("Shop.Cart.add", "Shop.Cart.handle_call(:add)", "call"),
            e("Shop.Cart.total", "Shop.Cart.handle_call(:total)", "call"),
            e("Shop.Cart.clear", "Shop.Cart.handle_cast(:clear)", "cast"),
            e(
                "Shop.Reporter.report",
                "Shop.Cart.handle_call(:total)",
                "call",
            ),
        ] {
            assert!(got.contains(&want), "{want:?} in {got:?}");
        }
        assert_eq!(node(calls, "Shop.Cart.handle_call(:total)").line, 10);
    }

    /// A protocol and the types it is implemented for (one, a list, or the
    /// module a `defimpl` is in); a behaviour and who adopts it.
    #[test]
    fn protocols_reach_their_implementations_and_behaviours_their_adopters() {
        let maps = maps(&Corpus::of(SHOP));
        let p = maps.iter().find(|m| m.id == "elixir.protocols").unwrap();
        let got = edges(p);
        for want in [
            e("Shop.Shape", "Shop.Circle", "implemented for"),
            e("Shop.Shape", "Integer", "implemented for"),
            e("Shop.Shape", "Float", "implemented for"),
            e("Shop.Shape", "Shop.Square", "implemented for"),
            e("Shop.Store", "Shop.Cart", "adopted by"),
        ] {
            assert!(got.contains(&want), "{want:?} in {got:?}");
        }
        assert_eq!(node(p, "Shop.Store").file, "lib/shop/store.ex");
        assert_eq!(node(p, "Shop.Shape").kind, "protocol");
    }
}
