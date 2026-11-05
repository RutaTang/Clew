//! Erlang's maps:
//!
//! - **supervision tree** — each supervisor (`-behaviour(supervisor)`) and
//!   the children its `init/1` starts (map child specs, tuple child specs,
//!   `?CHILD(M, Type)` macros), its restart strategy; an application's
//!   `start/2` and the supervisor it starts;
//! - **messages** — who sends what: each message tag (a tuple's first atom,
//!   or an atom), the modules that send it (`!`, `gen_server:call`/`cast`)
//!   and the modules that take it (`receive`, `handle_call`,
//!   `handle_cast`, `handle_info`);
//! - **gen_server calls** — each `gen_server:call`/`cast` to a module, an
//!   arrow to the `handle_call`/`handle_cast` clause that answers its
//!   request.

use std::collections::BTreeMap;

use clew_protocol::ProjectMap;
use tree_sitter::Node;

use super::{Corpus, MapBuilder, Source, all_of, ancestor, line, text};
use crate::highlight::Lang;

pub fn maps(corpus: &Corpus) -> Vec<ProjectMap> {
    let modules: Vec<Module> = corpus
        .in_langs(&[Lang::Erlang])
        .filter_map(Module::read)
        .collect();
    let mut out = Vec::new();
    out.extend(supervision(&modules));
    out.extend(messages(&modules));
    out.extend(calls(&modules));
    out
}

struct Module<'a> {
    source: &'a Source,
    name: String,
    line: usize,
    behaviours: Vec<String>,
    tree: tree_sitter::Tree,
}

impl<'a> Module<'a> {
    fn read(source: &'a Source) -> Option<Module<'a>> {
        let tree = source.tree()?;
        let src = source.text.as_str();
        let root = tree.root_node();
        let attribute = all_of(root, "module_attribute").into_iter().next()?;
        let name = text(attribute.child_by_field_name("name")?, src).to_string();
        let behaviours = all_of(root, "behaviour_attribute")
            .into_iter()
            .filter_map(|b| {
                b.child_by_field_name("name")
                    .map(|n| text(n, src).to_string())
            })
            .collect();
        Some(Module {
            line: line(attribute),
            source,
            name,
            behaviours,
            tree,
        })
    }

    fn src(&self) -> &str {
        &self.source.text
    }

    /// The function clauses named `name`.
    fn clauses(&self, name: &str) -> Vec<Node<'_>> {
        all_of(self.tree.root_node(), "function_clause")
            .into_iter()
            .filter(|c| {
                c.child_by_field_name("name")
                    .is_some_and(|n| text(n, self.src()) == name)
            })
            .collect()
    }
}

/// The tag of a message: a tuple's first atom, an atom, else none.
fn tag(node: Node<'_>, src: &str) -> Option<String> {
    match node.kind() {
        "atom" => Some(text(node, src).to_string()),
        "tuple" => node
            .named_child(0)
            .filter(|n| n.kind() == "atom")
            .map(|n| text(n, src).to_string()),
        "record_expr" => node
            .child_by_field_name("name")
            .map(|n| format!("#{}", text(n, src))),
        _ => None,
    }
}

// -- supervision ----------------------------------------------------------------------------

fn supervision(modules: &[Module]) -> Option<ProjectMap> {
    let mut map = MapBuilder::new(
        "erlang.supervision",
        "Supervision tree",
        "each supervisor and an arrow to each child its init/1 starts (labelled worker or supervisor); an application to the supervisor its start/2 starts",
    );
    let place = |map: &mut MapBuilder, name: &str| -> Option<usize> {
        let at = map.node("process", name, name);
        if let Some(m) = modules.iter().find(|m| m.name == name) {
            map.place(at, &m.source.rel, m.line);
        }
        at
    };
    for m in modules {
        let src = m.src();
        if m.behaviours.iter().any(|b| b == "supervisor") {
            let sup = place(&mut map, &m.name);
            for clause in m.clauses("init") {
                let Some(body) = clause.child_by_field_name("body") else {
                    continue;
                };
                // The strategy: `#{strategy => S}` or `{S, Intensity, Period}`.
                let strategy = all_of(body, "map_field")
                    .into_iter()
                    .find(|f| {
                        f.child_by_field_name("key")
                            .is_some_and(|k| text(k, src) == "strategy")
                    })
                    .and_then(|f| f.child_by_field_name("value"))
                    .map(|v| text(v, src).to_string())
                    .or_else(|| {
                        all_of(body, "tuple").into_iter().find_map(|t| {
                            let first = t.named_child(0).filter(|f| f.kind() == "atom")?;
                            let s = text(first, src);
                            [
                                "one_for_one",
                                "one_for_all",
                                "rest_for_one",
                                "simple_one_for_one",
                            ]
                            .contains(&s)
                            .then(|| s.to_string())
                        })
                    });
                if let Some(s) = strategy {
                    map.detail(sup, format!("supervisor · {s}"));
                }
                for (child, kind) in children(body, src) {
                    let at = place(&mut map, &child);
                    if kind == "supervisor"
                        && map
                            .get(at.unwrap_or(usize::MAX))
                            .is_some_and(|n| n.detail.is_empty())
                    {
                        map.detail(at, "supervisor");
                    }
                    map.edge(sup, at, &kind);
                }
            }
        }
        if m.behaviours.iter().any(|b| b == "application") {
            let app = place(&mut map, &m.name);
            map.detail(app, "application");
            for clause in m.clauses("start") {
                for remote in all_of(clause, "remote") {
                    let module = remote
                        .child_by_field_name("module")
                        .map(|n| text(n, src).trim_end_matches(':').to_string())
                        .unwrap_or_default();
                    let fun = remote
                        .child_by_field_name("fun")
                        .and_then(|f| f.child_by_field_name("expr").or_else(|| f.named_child(0)))
                        .map(|n| text(n, src).to_string())
                        .unwrap_or_default();
                    if fun == "start_link" && modules.iter().any(|x| x.name == module) {
                        let at = place(&mut map, &module);
                        map.edge(app, at, "starts");
                    }
                }
            }
        }
    }
    map.finish()
}

/// The children an `init/1` body starts: `(module, worker|supervisor)`.
fn children(body: Node<'_>, src: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    // `#{id => x, start => {M, F, A}, type => worker}`
    for spec in all_of(body, "map_expr") {
        let fields: BTreeMap<String, Node> = all_of(spec, "map_field")
            .into_iter()
            .filter(|f| f.parent() == Some(spec))
            .filter_map(|f| {
                Some((
                    text(f.child_by_field_name("key")?, src).to_string(),
                    f.child_by_field_name("value")?,
                ))
            })
            .collect();
        let Some(start) = fields.get("start") else {
            continue;
        };
        let Some(module) = start.named_child(0).filter(|n| n.kind() == "atom") else {
            continue;
        };
        let kind = fields
            .get("type")
            .map(|t| text(*t, src).to_string())
            .unwrap_or_else(|| "worker".into());
        out.push((text(module, src).to_string(), kind));
    }
    // `{Id, {M, F, A}, Restart, Shutdown, Type, Modules}`
    for spec in all_of(body, "tuple") {
        if spec.named_child_count() != 6 {
            continue;
        }
        let Some(mfa) = spec
            .named_child(1)
            .filter(|n| n.kind() == "tuple" && n.named_child_count() == 3)
        else {
            continue;
        };
        let Some(module) = mfa.named_child(0).filter(|n| n.kind() == "atom") else {
            continue;
        };
        let kind = spec
            .named_child(4)
            .map(|t| text(t, src).to_string())
            .unwrap_or_else(|| "worker".into());
        out.push((text(module, src).to_string(), kind));
    }
    // `?CHILD(Module, Type)`
    for call in all_of(body, "macro_call_expr") {
        if call
            .child_by_field_name("name")
            .is_none_or(|n| !text(n, src).contains("CHILD"))
        {
            continue;
        }
        let args: Vec<String> = all_of(call, "macro_expr")
            .into_iter()
            .map(|a| text(a, src).to_string())
            .collect();
        if let Some(module) = args
            .first()
            .filter(|a| a.chars().next().is_some_and(|c| c.is_ascii_lowercase()))
        {
            out.push((
                module.clone(),
                args.get(1).cloned().unwrap_or_else(|| "worker".into()),
            ));
        }
    }
    out
}

// -- messages -------------------------------------------------------------------------------------

/// Each `gen_server:call/cast(Target, Request)` in a module: the target (a
/// module, `?MODULE` resolved), the request's tag, `call` or `cast`, the
/// enclosing function and the line.
fn gen_calls<'m>(m: &'m Module<'m>) -> Vec<(String, String, &'static str, String, usize)> {
    let src = m.src();
    let mut out = Vec::new();
    for remote in all_of(m.tree.root_node(), "remote") {
        let module = remote
            .child_by_field_name("module")
            .map(|n| text(n, src).trim_end_matches(':'))
            .unwrap_or("");
        if !matches!(module, "gen_server" | "gen_statem") {
            continue;
        }
        let Some(call) = remote.child_by_field_name("fun") else {
            continue;
        };
        let kind = match call.child_by_field_name("expr").map(|n| text(n, src)) {
            Some("call") => "call",
            Some("cast") => "cast",
            _ => continue,
        };
        let args: Vec<Node> = call
            .child_by_field_name("args")
            .map(|a| {
                let mut c = a.walk();
                a.named_children(&mut c).collect()
            })
            .unwrap_or_default();
        let (Some(target), Some(request)) = (args.first(), args.get(1)) else {
            continue;
        };
        let target = match target.kind() {
            "atom" => text(*target, src).to_string(),
            "macro_call_expr" if text(*target, src) == "?MODULE" => m.name.clone(),
            _ => continue,
        };
        let Some(t) = tag(*request, src) else {
            continue;
        };
        let function = ancestor(remote, &["function_clause"])
            .and_then(|c| c.child_by_field_name("name"))
            .map(|n| text(n, src).to_string())
            .unwrap_or_default();
        out.push((target, t, kind, function, line(remote)));
    }
    out
}

fn messages(modules: &[Module]) -> Option<ProjectMap> {
    let mut map = MapBuilder::new(
        "erlang.messages",
        "Messages",
        "each message tag (a tuple's first atom); an arrow from each module that sends it — !, gen_server:call or cast — and to each module that takes it — receive, handle_call, handle_cast, handle_info",
    );
    let module_node = |map: &mut MapBuilder, m: &Module| -> Option<usize> {
        let at = map.node("module", &m.name, &m.name);
        map.place(at, &m.source.rel, m.line);
        at
    };
    for m in modules {
        let src = m.src();
        let root = m.tree.root_node();
        // `Pid ! Msg`
        for op in all_of(root, "binary_op_expr") {
            if op.child(1).is_none_or(|o| o.kind() != "!") {
                continue;
            }
            let Some(t) = op.child_by_field_name("rhs").and_then(|r| tag(r, src)) else {
                continue;
            };
            let from = module_node(&mut map, m);
            let msg = map.node("message", &t, &t);
            map.edge(from, msg, "sends");
        }
        for (_, t, kind, _, _) in gen_calls(m) {
            let from = module_node(&mut map, m);
            let msg = map.node("message", &t, &t);
            map.edge(from, msg, &format!("gen_server:{kind}"));
        }
        // `receive {Tag, …} -> …`
        for receive in all_of(root, "receive_expr") {
            for clause in all_of(receive, "cr_clause") {
                if let Some(t) = clause.child_by_field_name("pat").and_then(|p| tag(p, src)) {
                    let to = module_node(&mut map, m);
                    let msg = map.node("message", &t, &t);
                    map.edge(msg, to, "receive");
                }
            }
        }
        for callback in ["handle_call", "handle_cast", "handle_info"] {
            for clause in m.clauses(callback) {
                let first = clause
                    .child_by_field_name("args")
                    .and_then(|a| a.named_child(0));
                if let Some(t) = first.and_then(|p| tag(p, src)) {
                    let to = module_node(&mut map, m);
                    let msg = map.node("message", &t, &t);
                    map.edge(msg, to, callback);
                }
            }
        }
    }
    map.finish()
}

fn calls(modules: &[Module]) -> Option<ProjectMap> {
    let mut map = MapBuilder::new(
        "erlang.calls",
        "gen_server calls",
        "each function that calls or casts to a gen_server, an arrow to the handle_call or handle_cast clause that answers its request",
    );
    for m in modules {
        for (target, t, kind, function, at) in gen_calls(m) {
            let Some(server) = modules.iter().find(|x| x.name == target) else {
                continue;
            };
            let callback = if kind == "call" {
                "handle_call"
            } else {
                "handle_cast"
            };
            let clause = server.clauses(callback).into_iter().find(|c| {
                c.child_by_field_name("args")
                    .and_then(|a| a.named_child(0))
                    .and_then(|p| tag(p, server.src()))
                    .as_deref()
                    == Some(t.as_str())
            });
            let Some(clause) = clause else {
                continue;
            };
            let from = map.node(
                "function",
                &format!("{}:{function}", m.name),
                &format!("{}:{function}", m.name),
            );
            map.place(from, &m.source.rel, at);
            let label = format!("{}:{callback}({t})", server.name);
            let to = map.node("clause", &label, &label);
            map.place(to, &server.source.rel, line(clause));
            map.edge(from, to, kind);
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
            "src/shop_app.erl",
            "-module(shop_app).\n-behaviour(application).\n-export([start/2]).\nstart(_Type, _Args) -> shop_sup:start_link().\n",
        ),
        (
            "src/shop_sup.erl",
            "-module(shop_sup).\n-behaviour(supervisor).\n-define(CHILD(I, Type), {I, {I, start_link, []}, permanent, 5000, Type, [I]}).\ninit([]) ->\n    {ok, {#{strategy => one_for_all}, [\n        #{id => cart, start => {cart, start_link, []}},\n        #{id => pay_sup, start => {pay_sup, start_link, []}, type => supervisor},\n        {stock, {stock, start_link, []}, permanent, 5000, worker, [stock]},\n        ?CHILD(audit, worker)\n    ]}}.\n",
        ),
        (
            "src/pay_sup.erl",
            "-module(pay_sup).\n-behaviour(supervisor).\ninit(_) -> {ok, {{one_for_one, 5, 10}, []}}.\n",
        ),
        (
            "src/cart.erl",
            "-module(cart).\n-behaviour(gen_server).\nadd(Item) -> gen_server:call(?MODULE, {add, Item}).\nclear() -> gen_server:cast(cart, clear).\nhandle_call({add, Item}, _From, S) -> {reply, ok, [Item | S]};\nhandle_call({total}, _From, S) -> {reply, 0, S}.\nhandle_cast(clear, _S) -> {noreply, []}.\nhandle_info({stock_low, Sku}, S) -> {noreply, S}.\n",
        ),
        (
            "src/stock.erl",
            "-module(stock).\nwatch(Cart) -> Cart ! {stock_low, \"sku\"}, gen_server:call(cart, {total}).\nloop() -> receive {reserve, N} -> N; stop -> ok end.\n",
        ),
    ];

    #[test]
    fn the_supervision_tree() {
        let maps = maps(&Corpus::of(APP));
        let tree = maps.iter().find(|m| m.id == "erlang.supervision").unwrap();
        let got = edges(tree);
        for want in [
            e("shop_app", "shop_sup", "starts"),
            e("shop_sup", "cart", "worker"),
            e("shop_sup", "pay_sup", "supervisor"),
            e("shop_sup", "stock", "worker"),
            e("shop_sup", "audit", "worker"),
        ] {
            assert!(got.contains(&want), "{want:?} in {got:?}");
        }
        let detail = |n: &str| {
            tree.nodes
                .iter()
                .find(|x| x.label == n)
                .unwrap()
                .detail
                .clone()
        };
        assert_eq!(detail("shop_sup"), "supervisor · one_for_all");
        assert_eq!(detail("pay_sup"), "supervisor · one_for_one");
        let cart = tree.nodes.iter().find(|x| x.label == "cart").unwrap();
        assert_eq!((cart.file.as_str(), cart.line), ("src/cart.erl", 1));
    }

    #[test]
    fn messages_sent_and_taken() {
        let maps = maps(&Corpus::of(APP));
        let msgs = maps.iter().find(|m| m.id == "erlang.messages").unwrap();
        let got = edges(msgs);
        for want in [
            e("cart", "add", "gen_server:call"),
            e("cart", "clear", "gen_server:cast"),
            e("stock", "stock_low", "sends"),
            e("stock", "total", "gen_server:call"),
            e("add", "cart", "handle_call"),
            e("clear", "cart", "handle_cast"),
            e("stock_low", "cart", "handle_info"),
            e("reserve", "stock", "receive"),
            e("stop", "stock", "receive"),
        ] {
            assert!(got.contains(&want), "{want:?} in {got:?}");
        }
    }

    #[test]
    fn gen_server_calls_reach_their_clause() {
        let maps = maps(&Corpus::of(APP));
        let calls = maps.iter().find(|m| m.id == "erlang.calls").unwrap();
        let got = edges(calls);
        assert_eq!(
            got,
            [
                e("cart:add", "cart:handle_call(add)", "call"),
                e("cart:clear", "cart:handle_cast(clear)", "cast"),
                e("stock:watch", "cart:handle_call(total)", "call"),
            ]
        );
        let clause = calls
            .nodes
            .iter()
            .find(|n| n.label == "cart:handle_call(total)")
            .unwrap();
        assert_eq!((clause.file.as_str(), clause.line), ("src/cart.erl", 6));
    }
}
