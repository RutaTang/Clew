//! The state of a frontend project: each store — a Pinia store, a Redux
//! Toolkit slice and the root store that combines the slices, a Zustand
//! store, a Vuex store and its modules, a Svelte store, a Jotai atom, a
//! nanostore, a MobX store, an NgRx feature or signal store, a React (or
//! Preact, Solid) context — with the state, getters and actions it holds,
//! and an arrow from each component, hook or module that uses it, labelled
//! with what it uses: `cart → useCartStore: items, add`.
//!
//! What a file uses is read through its imports: a name imported from a
//! store's file is that store's (the store itself, or — from a file with one
//! store — its actions, selectors and hooks); what a component then reads
//! off the store (`const cart = useCartStore(); cart.items`,
//! `useBearStore((s) => s.bears)`, `const { a } = storeToRefs(cart)`) is
//! the arrow's label. Redux's `useSelector((s) => s.cart.items)` is read by
//! the slice's key in the root store, Vuex's `mapState('cart', …)` and
//! `dispatch('cart/add')` by the module's namespace.

use std::collections::{BTreeMap, HashMap};

use clew_protocol::ProjectMap;
use tree_sitter::Node;

use super::super::{Corpus, MapBuilder, Source, all_of, ancestor, line, text, unquote};
use super::components::{
    Files, enclosing, file_component_name, imports, is_fixture, labels, pattern_names,
};
use crate::highlight::Lang;

/// A store found.
#[derive(Default)]
struct Store {
    kind: &'static str,
    /// The variable (or class) it is.
    name: String,
    /// Its own id: a Pinia store's, a slice's `name`, a Vuex module's
    /// namespace.
    id: Option<String>,
    rel: String,
    line: usize,
    state: Vec<String>,
    getters: Vec<String>,
    actions: Vec<String>,
    /// Other names of its file that are its: a slice's actions destructured
    /// from `slice.actions`.
    owns: Vec<String>,
    /// What it is made from, as its file names them: a derived atom's or
    /// store's sources.
    from: Vec<String>,
    /// A root store's parts: (key, the part as written — `cartReducer`,
    /// `cartSlice.reducer`, a Vuex module's variable).
    parts: Vec<(String, String)>,
}

/// The names of an object literal's members: `{ a: 1, b() {}, c }`.
fn object_keys(object: Node<'_>, src: &str) -> Vec<String> {
    let mut out = Vec::new();
    for member in object.named_children(&mut object.walk()) {
        let key = match member.kind() {
            "pair" | "method_definition" => member
                .child_by_field_name("key")
                .or_else(|| member.child_by_field_name("name")),
            "shorthand_property_identifier" => Some(member),
            _ => None,
        };
        if let Some(key) = key {
            let k = text(key, src).trim_matches(['"', '\'', '`']);
            if !k.starts_with('[') {
                out.push(k.to_string());
            }
        }
    }
    out
}

/// The member `key` of an object literal: a pair's value, or the method
/// itself.
fn member<'t>(object: Node<'t>, key: &str, src: &str) -> Option<Node<'t>> {
    object
        .named_children(&mut object.walk())
        .find_map(|m| match m.kind() {
            "pair" => {
                let k = m.child_by_field_name("key")?;
                (text(k, src).trim_matches(['"', '\'']) == key)
                    .then(|| m.child_by_field_name("value"))
                    .flatten()
            }
            "method_definition" => {
                (m.child_by_field_name("name").map(|n| text(n, src)) == Some(key)).then_some(m)
            }
            "shorthand_property_identifier" => (text(m, src) == key).then_some(m),
            _ => None,
        })
}

/// `x as T`, `(x)`, `x satisfies T`, `x!` → `x`.
fn bare(mut node: Node<'_>) -> Node<'_> {
    while matches!(
        node.kind(),
        "as_expression"
            | "satisfies_expression"
            | "parenthesized_expression"
            | "non_null_expression"
    ) {
        match node.named_child(0) {
            Some(inner) => node = inner,
            None => break,
        }
    }
    node
}

/// The object a function returns: `() => ({…})`, `() => { return {…} }`,
/// `state() { return {…} }`.
fn returned_object(function: Node<'_>) -> Option<Node<'_>> {
    let body = function.child_by_field_name("body")?;
    let body = bare(body);
    if body.kind() == "object" {
        return Some(body);
    }
    // The last `return {…}` of the body itself, not of a function inside it.
    let mut found = None;
    for statement in body.named_children(&mut body.walk()) {
        if statement.kind() == "return_statement"
            && let Some(value) = statement.named_child(0).map(bare)
            && value.kind() == "object"
        {
            found = Some(value);
        }
    }
    found
}

/// An object literal a value is, or names: the object itself, a function's
/// returned object, or a same-file constant holding one.
fn object_of<'t>(value: Node<'t>, root: Node<'t>, src: &str) -> Option<Node<'t>> {
    let value = bare(value);
    match value.kind() {
        "object" => Some(value),
        "arrow_function" | "function_expression" | "function" | "method_definition" => {
            returned_object(value)
        }
        "identifier" | "shorthand_property_identifier" => {
            let name = text(value, src);
            top_level(root).into_iter().find_map(|(n, v)| {
                (text(n, src) == name)
                    .then(|| object_of(v, root, src))
                    .flatten()
            })
        }
        _ => None,
    }
}

/// A file's top-level `const x = …` (exported or not): (name, value).
fn top_level<'t>(root: Node<'t>) -> Vec<(Node<'t>, Node<'t>)> {
    let mut out = Vec::new();
    for statement in root.named_children(&mut root.walk()) {
        let statement = match statement.kind() {
            "export_statement" => match statement.child_by_field_name("declaration") {
                Some(d) => d,
                None => continue,
            },
            _ => statement,
        };
        if !matches!(
            statement.kind(),
            "lexical_declaration" | "variable_declaration"
        ) {
            continue;
        }
        for declarator in statement.named_children(&mut statement.walk()) {
            if declarator.kind() != "variable_declarator" {
                continue;
            }
            if let (Some(name), Some(value)) = (
                declarator.child_by_field_name("name"),
                declarator.child_by_field_name("value"),
            ) {
                out.push((name, value));
            }
        }
    }
    out
}

/// The function a call calls, as written, and its arguments — through a
/// curried `create<T>()(…)` to the inner name with the outer arguments.
fn call_parts<'t>(value: Node<'t>, src: &'t str) -> Option<(&'t str, Node<'t>)> {
    let value = bare(value);
    match value.kind() {
        "call_expression" => {
            let mut function = value.child_by_field_name("function")?;
            while function.kind() == "call_expression" {
                function = function.child_by_field_name("function")?;
            }
            Some((text(function, src), value.child_by_field_name("arguments")?))
        }
        "new_expression" => Some((
            text(value.child_by_field_name("constructor")?, src),
            value.child_by_field_name("arguments")?,
        )),
        _ => None,
    }
}

/// The function (or the innermost one, through middleware such as
/// `persist(devtools((set) => …))`) a call's first argument is.
fn first_function(arguments: Node<'_>) -> Option<Node<'_>> {
    let mut at = arguments.named_child(0).map(bare)?;
    loop {
        match at.kind() {
            "arrow_function" | "function_expression" | "function" => return Some(at),
            "call_expression" => {
                at = at
                    .child_by_field_name("arguments")?
                    .named_child(0)
                    .map(bare)?
            }
            _ => return None,
        }
    }
}

/// An object's members split into state (values) and actions (functions).
fn state_and_actions(object: Node<'_>, src: &str) -> (Vec<String>, Vec<String>) {
    let (mut state, mut actions) = (Vec::new(), Vec::new());
    for name in object_keys(object, src) {
        let value = member(object, &name, src).map(bare);
        let is_function = value.is_some_and(|v| {
            matches!(
                v.kind(),
                "arrow_function" | "function_expression" | "function" | "method_definition"
            )
        });
        match is_function {
            true => actions.push(name),
            false => state.push(name),
        }
    }
    (state, actions)
}

/// The identifiers a derived store reads: `get(a)` in a Jotai atom,
/// `derived([a, b], …)`'s sources.
fn reads(node: Node<'_>, src: &str, call: &str) -> Vec<String> {
    let mut out = Vec::new();
    for c in all_of(node, "call_expression") {
        if c.child_by_field_name("function").map(|f| text(f, src)) == Some(call)
            && let Some(first) = c
                .child_by_field_name("arguments")
                .and_then(|a| a.named_child(0))
            && first.kind() == "identifier"
        {
            let name = text(first, src).to_string();
            if !out.contains(&name) {
                out.push(name);
            }
        }
    }
    out
}

/// What a store's file says, one of which any file that defines a store
/// does: most files are no store's, and are not parsed for one.
const MARKS: &[&str] = &[
    "defineStore",
    "createSlice",
    "configureStore",
    "combineReducers",
    "zustand",
    "vuex",
    "svelte/store",
    "jotai",
    "nanostores",
    "createContext",
    "makeAutoObservable",
    "makeObservable",
    "@ngrx/",
];

/// The stores a file defines.
fn stores_in(source: &Source) -> Vec<Store> {
    if !MARKS.iter().any(|m| source.text.contains(m)) {
        return Vec::new();
    }
    let Some((_, src, tree)) = source.script() else {
        return Vec::new();
    };
    let root = tree.root_node();

    let imported = imports(root, src);
    let from = |local: &str, packages: &[&str]| -> bool {
        let head = local.split(['.', '<']).next().unwrap_or(local);
        imported.get(head).is_some_and(|(spec, _)| {
            packages
                .iter()
                .any(|p| spec == p || spec.starts_with(&format!("{p}/")))
        })
    };
    let mut out = Vec::new();
    let mut combined_into_root: Vec<String> = Vec::new();
    for (name, value) in top_level(root) {
        if name.kind() != "identifier" {
            // `export const { add, remove } = cartSlice.actions`
            if name.kind() == "object_pattern"
                && let Some(slice) = text(bare(value), src).strip_suffix(".actions")
            {
                let names = pattern_names(name, src);
                if let Some(store) = out.iter_mut().find(|s: &&mut Store| s.name == slice) {
                    store.owns.extend(names);
                }
            }
            continue;
        }
        let var = text(name, src).to_string();
        let Some((callee, args)) = call_parts(value, src) else {
            continue;
        };
        let last = callee.rsplit('.').next().unwrap_or(callee);
        let mut store = Store {
            name: var.clone(),
            rel: source.rel.clone(),
            line: line(name),
            ..Default::default()
        };
        let first = args.named_child(0).map(bare);
        match last {
            "defineStore" => {
                store.kind = "Pinia store";
                let mut cursor = args.walk();
                let mut rest = args.named_children(&mut cursor).map(bare);
                let head = rest.next();
                let options = match head {
                    Some(h) if h.kind() == "object" => {
                        store.id = member(h, "id", src).and_then(|v| unquote(text(v, src)));
                        Some(h)
                    }
                    Some(h) => {
                        store.id = unquote(text(h, src));
                        rest.next()
                    }
                    None => None,
                };
                match options {
                    Some(o) if o.kind() == "object" => {
                        if let Some(state) =
                            member(o, "state", src).and_then(|v| object_of(v, root, src))
                        {
                            store.state = object_keys(state, src);
                        }
                        if let Some(g) = member(o, "getters", src)
                            .map(bare)
                            .filter(|g| g.kind() == "object")
                        {
                            store.getters = object_keys(g, src);
                        }
                        if let Some(a) = member(o, "actions", src)
                            .map(bare)
                            .filter(|a| a.kind() == "object")
                        {
                            store.actions = object_keys(a, src);
                        }
                    }
                    // A setup store: what it returns, by what each is.
                    Some(setup) => {
                        if let Some(returned) = returned_object(setup) {
                            let body = setup.child_by_field_name("body");
                            for key in object_keys(returned, src) {
                                let made = body.and_then(|b| {
                                    all_of(b, "variable_declarator").into_iter().find(|d| {
                                        d.child_by_field_name("name").map(|n| text(n, src))
                                            == Some(key.as_str())
                                    })
                                });
                                let value =
                                    made.and_then(|d| d.child_by_field_name("value")).map(bare);
                                let callee = value
                                    .and_then(|v| v.child_by_field_name("function"))
                                    .map(|f| text(f, src))
                                    .unwrap_or("");
                                let declared_fn = body.is_some_and(|b| {
                                    all_of(b, "function_declaration").into_iter().any(|f| {
                                        f.child_by_field_name("name").map(|n| text(n, src))
                                            == Some(key.as_str())
                                    })
                                });
                                if callee == "computed" {
                                    store.getters.push(key);
                                } else if declared_fn
                                    || value.is_some_and(|v| {
                                        matches!(v.kind(), "arrow_function" | "function_expression")
                                    })
                                {
                                    store.actions.push(key);
                                } else {
                                    store.state.push(key);
                                }
                            }
                        }
                    }
                    None => {}
                }
            }
            "createSlice" => {
                let Some(options) = first.filter(|f| f.kind() == "object") else {
                    continue;
                };
                store.kind = "Redux slice";
                store.id = member(options, "name", src).and_then(|v| unquote(text(v, src)));
                if let Some(state) =
                    member(options, "initialState", src).and_then(|v| object_of(v, root, src))
                {
                    store.state = object_keys(state, src);
                }
                if let Some(r) =
                    member(options, "reducers", src).and_then(|v| object_of(v, root, src))
                {
                    store.actions = object_keys(r, src);
                }
            }
            "configureStore" | "combineReducers" => {
                let reducers = match last {
                    "configureStore" => first
                        .filter(|f| f.kind() == "object")
                        .and_then(|o| member(o, "reducer", src)),
                    _ => first,
                };
                let Some(reducers) = reducers.map(bare) else {
                    continue;
                };
                if reducers.kind() == "identifier" {
                    // `configureStore({ reducer: rootReducer })`: the
                    // combined reducers are this store's parts.
                    combined_into_root.push(text(reducers, src).to_string());
                    if let Some(combined) = out
                        .iter()
                        .position(|s: &Store| s.name == text(reducers, src))
                    {
                        store.parts = out.remove(combined).parts;
                    }
                } else if reducers.kind() == "object" {
                    for key in object_keys(reducers, src) {
                        let part = member(reducers, &key, src)
                            .map(|v| text(v, src).to_string())
                            .unwrap_or_else(|| key.clone());
                        store.parts.push((key, part));
                    }
                }
                store.kind = "Redux store";
            }
            "create" | "createStore" | "createWithEqualityFn" if from(callee, &["zustand"]) => {
                store.kind = "Zustand store";
                if let Some(object) = first_function(args).and_then(returned_object) {
                    (store.state, store.actions) = state_and_actions(object, src);
                }
            }
            "createStore" | "Store" if from(callee, &["vuex"]) || callee == "Vuex.Store" => {
                store.kind = "Vuex store";
                if let Some(options) = first.filter(|f| f.kind() == "object") {
                    vuex_options(&mut store, options, root, src);
                    if let Some(modules) = member(options, "modules", src)
                        .map(bare)
                        .filter(|m| m.kind() == "object")
                    {
                        for key in object_keys(modules, src) {
                            let part = member(modules, &key, src)
                                .map(|v| text(v, src).to_string())
                                .unwrap_or_else(|| key.clone());
                            store.parts.push((key, part));
                        }
                    }
                }
            }
            "writable" | "readable" | "derived" if from(callee, &["svelte"]) => {
                store.kind = "Svelte store";
                if last == "derived"
                    && let Some(first) = first
                {
                    store.from = match first.kind() {
                        "array" => first
                            .named_children(&mut first.walk())
                            .filter(|c| c.kind() == "identifier")
                            .map(|c| text(c, src).to_string())
                            .collect(),
                        _ => vec![text(first, src).to_string()],
                    };
                }
            }
            l if l.starts_with("atom") && from(callee, &["jotai", "jotai-family"]) => {
                store.kind = "Jotai atom";
                store.from = reads(value, src, "get");
                for written in reads(value, src, "set") {
                    if !store.from.contains(&written) {
                        store.from.push(written);
                    }
                }
            }
            "atom" | "map" | "computed" | "deepMap" | "persistentAtom" | "persistentMap"
                if from(callee, &["nanostores", "@nanostores"]) =>
            {
                store.kind = "Nanostore";
                if last == "computed"
                    && let Some(first) = first
                {
                    store.from = match first.kind() {
                        "array" => first
                            .named_children(&mut first.walk())
                            .map(|c| text(c, src).to_string())
                            .collect(),
                        _ => vec![text(first, src).to_string()],
                    };
                }
            }
            "createContext" => {
                store.kind = "Context";
            }
            "createFeature" if from(callee, &["@ngrx/store"]) => {
                store.kind = "NgRx feature";
                if let Some(options) = first.filter(|f| f.kind() == "object") {
                    store.id = member(options, "name", src).and_then(|v| unquote(text(v, src)));
                }
            }
            "signalStore" if from(callee, &["@ngrx/signals"]) => {
                store.kind = "NgRx signal store";
                for feature in args.named_children(&mut args.walk()) {
                    let Some((made_by, inner)) = call_parts(feature, src) else {
                        continue;
                    };
                    let object = inner.named_child(0).and_then(|v| object_of(v, root, src));
                    let Some(object) = object else {
                        continue;
                    };
                    match made_by {
                        "withState" => store.state.extend(object_keys(object, src)),
                        "withComputed" => store.getters.extend(object_keys(object, src)),
                        "withMethods" => store.actions.extend(object_keys(object, src)),
                        _ => {}
                    }
                }
            }
            _ => continue,
        }
        out.push(store);
    }
    out.retain(|s| !(s.kind == "Redux store" && combined_into_root.contains(&s.name)));
    // MobX: a class that makes itself observable.
    for class in all_of(root, "class_declaration") {
        let body = text(class, src);
        if !(body.contains("makeAutoObservable(this") || body.contains("makeObservable(this")) {
            continue;
        }
        let Some(name) = class.child_by_field_name("name") else {
            continue;
        };
        let mut store = Store {
            kind: "MobX store",
            name: text(name, src).to_string(),
            rel: source.rel.clone(),
            line: line(class),
            ..Default::default()
        };
        if let Some(members) = class.child_by_field_name("body") {
            for m in members.named_children(&mut members.walk()) {
                let Some(n) = m
                    .child_by_field_name("name")
                    .or_else(|| m.child_by_field_name("property"))
                else {
                    continue;
                };
                let n = text(n, src).to_string();
                match m.kind() {
                    "method_definition" if n != "constructor" => {
                        match text(m, src).starts_with("get ") {
                            true => store.getters.push(n),
                            false => store.actions.push(n),
                        }
                    }
                    "public_field_definition" | "field_definition" => store.state.push(n),
                    _ => {}
                }
            }
        }
        // `export const todos = new TodoStore()`: the instance is it too.
        for (n, v) in top_level(root) {
            if call_parts(v, src).is_some_and(|(c, _)| c == store.name) {
                store.owns.push(text(n, src).to_string());
            }
        }
        out.push(store);
    }
    out
}

/// A Vuex store's or module's state, getters, mutations and actions.
fn vuex_options(store: &mut Store, options: Node<'_>, root: Node<'_>, src: &str) {
    if let Some(state) = member(options, "state", src).and_then(|v| object_of(v, root, src)) {
        store.state = object_keys(state, src);
    }
    if let Some(g) = member(options, "getters", src).and_then(|v| object_of(v, root, src)) {
        store.getters = object_keys(g, src);
    }
    for part in ["mutations", "actions"] {
        if let Some(a) = member(options, part, src).and_then(|v| object_of(v, root, src)) {
            for k in object_keys(a, src) {
                if !store.actions.contains(&k) {
                    store.actions.push(k);
                }
            }
        }
    }
}

/// A Vuex module a root store names: the object its file exports.
fn vuex_module(source: &Source, namespace: &str) -> Option<Store> {
    let (_, src, tree) = source.script()?;
    let root = tree.root_node();
    let exported = root
        .named_children(&mut root.walk())
        .filter(|n| n.kind() == "export_statement" && text(*n, src).starts_with("export default"))
        .find_map(|n| n.child_by_field_name("value"))?;
    let options = object_of(exported, root, src)?;
    let mut store = Store {
        kind: "Vuex module",
        name: namespace.to_string(),
        id: Some(namespace.to_string()),
        rel: source.rel.clone(),
        line: line(exported),
        ..Default::default()
    };
    vuex_options(&mut store, options, root, src);
    Some(store)
}

/// Who uses a store at `site`: the component or `use…` hook it is in, a
/// component file's own component, or the module (file) itself.
fn user_of(site: Node<'_>, src: &str, source: &Source) -> (&'static str, String, String, usize) {
    if let Some((name, place)) = enclosing(site, src, &source.rel, true) {
        let kind = match name.starts_with("use") {
            true => "hook",
            false => "component",
        };
        return (kind, format!("{}#{name}", source.rel), name, line(place));
    }
    component_file_user(source)
        .unwrap_or_else(|| ("module", source.rel.clone(), source.name().to_string(), 1))
}

fn component_file_user(source: &Source) -> Option<(&'static str, String, String, usize)> {
    matches!(source.lang, Some(Lang::Vue | Lang::Svelte | Lang::Astro)).then(|| {
        let name = file_component_name(&source.rel);
        ("component", format!("{}#{name}", source.rel), name, 1)
    })
}

/// What is read off a store handle at `site`: `useBear((s) => s.bears)`,
/// `const cart = useCart(); cart.items`, `const { a } = useCart()`,
/// `storeToRefs(cart)`, `useBear.getState().reset()`, `todos.add()`.
fn read_off(site: Node<'_>, src: &str) -> Vec<String> {
    const PLUMBING: &[&str] = &[
        "getState",
        "setState",
        "subscribe",
        "value",
        "$patch",
        "$reset",
        "$state",
        "get",
        "set",
    ];
    let mut out = Vec::new();
    let push = |name: &str, out: &mut Vec<String>| {
        if !PLUMBING.contains(&name) && !out.iter().any(|n| n == name) {
            out.push(name.to_string());
        }
    };
    let Some(parent) = site.parent() else {
        return out;
    };
    // `handle.member`, through `getState()`.
    if parent.kind() == "member_expression" && parent.child_by_field_name("object") == Some(site) {
        let mut at = parent;
        while let Some(property) = at.child_by_field_name("property").map(|p| text(p, src)) {
            if !PLUMBING.contains(&property) {
                push(property, &mut out);
                break;
            }
            // `.getState().x`: past the call to the next member.
            let next = at
                .parent()
                .filter(|p| p.kind() == "call_expression")
                .and_then(|c| c.parent())
                .filter(|m| m.kind() == "member_expression");
            match next {
                Some(m) => at = m,
                None => break,
            }
        }
        return out;
    }
    if parent.kind() != "call_expression" || parent.child_by_field_name("function") != Some(site) {
        return out;
    }
    // `storeToRefs(useCart())`: what is taken off the refs.
    let mut call = parent;
    if let Some(outer) = call
        .parent()
        .filter(|a| a.kind() == "arguments")
        .and_then(|a| a.parent())
        .filter(|c| {
            c.kind() == "call_expression"
                && matches!(
                    c.child_by_field_name("function").map(|f| text(f, src)),
                    Some("storeToRefs" | "toRefs")
                )
        })
    {
        call = outer;
    }
    // `useBear((s) => s.bears)`, `useBear(useShallow((s) => ({ a: s.a })))`
    if let Some(selector) = parent
        .child_by_field_name("arguments")
        .and_then(first_function)
    {
        let param = selector
            .child_by_field_name("parameters")
            .and_then(|p| p.named_child(0))
            .or_else(|| selector.child_by_field_name("parameter"))
            .map(|p| {
                text(p, src)
                    .split(':')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_string()
            });
        if let Some(param) = param.filter(|p| !p.is_empty()) {
            for m in all_of(selector, "member_expression") {
                if m.child_by_field_name("object").map(|o| text(o, src)) == Some(param.as_str())
                    && let Some(p) = m.child_by_field_name("property")
                {
                    push(text(p, src), &mut out);
                }
            }
        }
        return out;
    }
    let holder = call.parent();
    match holder.map(|h| h.kind()) {
        Some("member_expression") => {
            if let Some(p) = holder.and_then(|h| h.child_by_field_name("property")) {
                push(text(p, src), &mut out);
            }
        }
        Some("variable_declarator") => {
            let holder = holder.unwrap();
            let Some(name) = holder.child_by_field_name("name") else {
                return out;
            };
            if name.kind() == "object_pattern" {
                for n in pattern_names(name, src) {
                    push(&n, &mut out);
                }
                return out;
            }
            let var = text(name, src);
            let scope = ancestor(holder, &["statement_block", "program"]).unwrap_or(holder);
            for m in all_of(scope, "member_expression") {
                if m.child_by_field_name("object").map(|o| text(o, src)) == Some(var)
                    && let Some(p) = m.child_by_field_name("property")
                {
                    push(text(p, src), &mut out);
                }
            }
            // `const { items } = storeToRefs(cart)`
            for d in all_of(scope, "variable_declarator") {
                let Some(value) = d.child_by_field_name("value") else {
                    continue;
                };
                if value.kind() == "call_expression"
                    && value.child_by_field_name("function").map(|f| text(f, src))
                        == Some("storeToRefs")
                    && value
                        .child_by_field_name("arguments")
                        .and_then(|a| a.named_child(0))
                        .map(|a| text(a, src))
                        == Some(var)
                    && let Some(pattern) = d
                        .child_by_field_name("name")
                        .filter(|n| n.kind() == "object_pattern")
                {
                    for n in pattern_names(pattern, src) {
                        push(&n, &mut out);
                    }
                }
            }
        }
        _ => {}
    }
    out
}

/// The quoted words after each `needle(` in `text`: `mapState('cart'` → cart.
fn quoted_after<'a>(text: &'a str, needle: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find(needle) {
        rest = &rest[at + needle.len()..];
        let s = rest.trim_start();
        let Some(quote) = s.chars().next().filter(|c| matches!(c, '\'' | '"' | '`')) else {
            continue;
        };
        if let Some(end) = s[1..].find(quote) {
            out.push(&s[1..1 + end]);
        }
    }
    out
}

pub(super) fn map(corpus: &Corpus) -> Option<ProjectMap> {
    let mut stores: Vec<Store> = Vec::new();
    for source in &corpus.files {
        if is_fixture(&source.rel) {
            continue;
        }
        stores.extend(stores_in(source));
    }
    if stores.is_empty() {
        return None;
    }
    let files = Files::of(corpus);
    // Vuex modules, read from the files the root store names.
    let mut modules = Vec::new();
    for store in stores.iter().filter(|s| s.kind == "Vuex store") {
        let Some(source) = corpus.get(&store.rel) else {
            continue;
        };
        let Some((_, src, tree)) = source.script() else {
            continue;
        };
        let imported = imports(tree.root_node(), src);
        for (namespace, part) in &store.parts {
            let module = imported
                .get(part.as_str())
                .and_then(|(spec, _)| files.resolve(spec, &store.rel))
                .and_then(|rel| corpus.get(&rel))
                .and_then(|s| vuex_module(s, namespace));
            modules.extend(module);
        }
    }
    stores.extend(modules);
    let by_rel: HashMap<&str, Vec<usize>> =
        stores
            .iter()
            .enumerate()
            .fold(HashMap::new(), |mut m, (i, s)| {
                m.entry(s.rel.as_str()).or_default().push(i);
                m
            });
    // The store a name imported from `rel` is.
    let store_for = |rel: &str, exported: &str, local: &str| -> Option<usize> {
        let here = by_rel.get(rel)?;
        let named = |n: &str| {
            here.iter()
                .copied()
                .find(|&i| stores[i].name == n || stores[i].owns.iter().any(|o| o == n))
        };
        named(exported).or_else(|| named(local)).or_else(|| {
            // One store in its file: whatever the file exports is the
            // store's — its actions, selectors, hooks, provider.
            (here.len() == 1).then(|| here[0])
        })
    };
    // A store's key in its root (`state.cart`), and the arrows root → part.
    let mut by_key: HashMap<String, usize> = HashMap::new();
    let mut part_arrows: Vec<(usize, usize, String)> = Vec::new();
    for (i, root) in stores.iter().enumerate() {
        if !matches!(root.kind, "Redux store" | "Vuex store") {
            continue;
        }
        let imported = corpus
            .get(&root.rel)
            .and_then(|s| s.script())
            .map(|(_, src, tree)| imports(tree.root_node(), src))
            .unwrap_or_default();
        for (key, part) in &root.parts {
            let head = part.split('.').next().unwrap_or(part);
            let target = if root.kind == "Vuex store" {
                stores
                    .iter()
                    .position(|s| s.kind == "Vuex module" && s.id.as_deref() == Some(key.as_str()))
            } else if let Some((spec, exported)) = imported.get(head) {
                files
                    .resolve(spec, &root.rel)
                    .and_then(|rel| store_for(&rel, exported, head))
            } else {
                stores
                    .iter()
                    .position(|s| s.rel == root.rel && s.name == head)
            };
            if let Some(t) = target {
                by_key.insert(key.clone(), t);
                part_arrows.push((i, t, key.clone()));
            }
        }
    }
    for (i, s) in stores.iter().enumerate() {
        if s.kind == "Redux slice"
            && let Some(id) = &s.id
        {
            by_key.entry(id.clone()).or_insert(i);
        }
    }
    let vuex_root = stores.iter().position(|s| s.kind == "Vuex store");

    // Who uses what: user key → (kind, label, rel, line), and the arrows.
    let mut users: BTreeMap<String, (&'static str, String, String, usize)> = BTreeMap::new();
    let mut arrows: BTreeMap<(String, usize), Vec<String>> = BTreeMap::new();
    let mut add = |user: (&'static str, String, String, usize), store: usize, what: Vec<String>| {
        let (kind, key, label, line) = user;
        let rel_of_user = key.split('#').next().unwrap_or(&key).to_string();
        // A store's own file using it is the store's business.
        if rel_of_user == stores[store].rel {
            return;
        }
        users
            .entry(key.clone())
            .or_insert((kind, label, rel_of_user, line));
        let merged = arrows.entry((key, store)).or_default();
        for w in what {
            if !merged.contains(&w) {
                merged.push(w);
            }
        }
    };
    // What a file that uses a store says: the store file's name in its
    // import (`./cart`, `@/stores/cart`; an index's folder), or a store's
    // own name. Files that say none are not parsed for it.
    let mut mentions: Vec<&str> = Vec::new();
    for s in &stores {
        let mut parts = s.rel.rsplit('/');
        let name = parts.next().unwrap_or("");
        let stem = name.split('.').next().unwrap_or(name);
        let stem = match stem {
            "index" => parts.next().unwrap_or(stem),
            _ => stem,
        };
        for word in [stem, s.name.as_str()]
            .into_iter()
            .chain(s.owns.iter().map(String::as_str))
        {
            if !word.is_empty() && !mentions.contains(&word) {
                mentions.push(word);
            }
        }
    }
    for source in &corpus.files {
        if is_fixture(&source.rel) {
            continue;
        }
        let selects = !by_key.is_empty() && source.text.contains("Selector");
        let parsed = match selects || mentions.iter().any(|m| source.text.contains(m)) {
            true => source.script(),
            false => None,
        };
        if let Some((_, src, tree)) = parsed {
            let root = tree.root_node();
            // Every name the file says, once: where each import is used.
            let mut sites: HashMap<&str, Vec<Node>> = HashMap::new();
            for site in all_of(root, "identifier")
                .into_iter()
                .chain(all_of(root, "shorthand_property_identifier"))
            {
                sites.entry(text(site, src)).or_default().push(site);
            }
            let mut imported: Vec<_> = imports(root, src).into_iter().collect();
            imported.sort();
            for (local, (spec, exported)) in imported {
                let Some(rel) = files.resolve(&spec, &source.rel) else {
                    continue;
                };
                let Some(store) = store_for(&rel, &exported, &local) else {
                    continue;
                };
                let handle = exported == "*"
                    || exported == stores[store].name
                    || local == stores[store].name
                    || (exported == "default"
                        && by_rel.get(rel.as_str()).is_some_and(|h| h.len() == 1))
                    || stores[store].owns.contains(&exported) && stores[store].kind == "MobX store";
                let mut used = false;
                for &site in sites.get(local.as_str()).into_iter().flatten() {
                    if ancestor(site, &["import_statement"]).is_some() {
                        continue;
                    }
                    // The root store's wiring is drawn as root → part.
                    let wiring = ancestor(site, &["call_expression"])
                        .and_then(|c| c.child_by_field_name("function"))
                        .is_some_and(|f| {
                            matches!(
                                text(f, src).rsplit('.').next(),
                                Some(
                                    "configureStore" | "combineReducers" | "createStore" | "Store"
                                )
                            )
                        });
                    if wiring {
                        continue;
                    }
                    used = true;
                    let what = match handle && stores[store].kind != "Redux slice" {
                        true => read_off(site, src),
                        false => vec![exported.clone()],
                    };
                    add(user_of(site, src, source), store, what);
                }
                // Svelte's `$count` in a component's markup.
                if !used
                    && source.lang == Some(Lang::Svelte)
                    && source.text.contains(&format!("${local}"))
                    && let Some(user) = component_file_user(source)
                {
                    add(user, store, Vec::new());
                }
            }
            // Redux: `useSelector((state) => state.cart.items)`.
            if selects {
                for call in all_of(root, "call_expression") {
                    let Some(function) = call.child_by_field_name("function").map(|f| text(f, src))
                    else {
                        continue;
                    };
                    if !(function == "useSelector"
                        || function.ends_with("Selector") && function.starts_with("use"))
                    {
                        continue;
                    }
                    let Some(selector) = call
                        .child_by_field_name("arguments")
                        .and_then(first_function)
                    else {
                        continue;
                    };
                    let Some(param) = selector
                        .child_by_field_name("parameters")
                        .and_then(|p| p.named_child(0))
                        .or_else(|| selector.child_by_field_name("parameter"))
                        .map(|p| {
                            text(p, src)
                                .split(':')
                                .next()
                                .unwrap_or("")
                                .trim()
                                .to_string()
                        })
                    else {
                        continue;
                    };
                    for m in all_of(selector, "member_expression") {
                        if m.child_by_field_name("object").map(|o| text(o, src))
                            != Some(param.as_str())
                        {
                            continue;
                        }
                        let Some(key) = m.child_by_field_name("property").map(|p| text(p, src))
                        else {
                            continue;
                        };
                        let Some(&store) = by_key.get(key) else {
                            continue;
                        };
                        let field = m
                            .parent()
                            .filter(|p| p.kind() == "member_expression")
                            .and_then(|p| p.child_by_field_name("property"))
                            .map(|p| text(p, src).to_string());
                        add(
                            user_of(call, src, source),
                            store,
                            field.into_iter().collect(),
                        );
                    }
                }
            }
        }
        // Vuex: `mapState('cart', …)`, `dispatch('cart/add')`,
        // `$store.state.cart` — in the script and a Vue file's template.
        if let Some(vuex) = vuex_root {
            let whole = source.text.as_str();
            if !(whole.contains("$store")
                || whole.contains("mapState")
                || whole.contains("mapActions")
                || whole.contains("mapGetters")
                || whole.contains("mapMutations")
                || whole.contains("useStore") && whole.contains("vuex"))
            {
                continue;
            }
            let user = component_file_user(source)
                .unwrap_or_else(|| ("module", source.rel.clone(), source.name().to_string(), 1));
            let module = |namespace: &str| {
                stores
                    .iter()
                    .position(|s| s.kind == "Vuex module" && s.id.as_deref() == Some(namespace))
            };
            let mut hit = false;
            for helper in ["mapState(", "mapGetters(", "mapActions(", "mapMutations("] {
                for namespace in quoted_after(whole, helper) {
                    if let Some(m) = module(namespace) {
                        add(user.clone(), m, Vec::new());
                        hit = true;
                    }
                }
            }
            for call in ["dispatch(", "commit("] {
                for path in quoted_after(whole, call) {
                    match path.split_once('/') {
                        Some((namespace, action)) => {
                            if let Some(m) = module(namespace) {
                                add(user.clone(), m, vec![action.to_string()]);
                                hit = true;
                            }
                        }
                        None => {
                            add(user.clone(), vuex, vec![path.to_string()]);
                            hit = true;
                        }
                    }
                }
            }
            let mut rest = whole;
            while let Some(at) = rest.find("$store.state.") {
                rest = &rest[at + "$store.state.".len()..];
                let key = super::super::ident(rest);
                match module(key) {
                    Some(m) => add(user.clone(), m, Vec::new()),
                    None => add(user.clone(), vuex, vec![key.to_string()]),
                }
                hit = true;
            }
            if !hit && whole.contains("$store") {
                add(user, vuex, Vec::new());
            }
        }
    }

    // A context only its own file uses is that file's business.
    let used: std::collections::HashSet<usize> = arrows.keys().map(|(_, s)| *s).collect();
    let shown: Vec<bool> = stores
        .iter()
        .enumerate()
        .map(|(i, s)| s.kind != "Context" || used.contains(&i))
        .collect();
    let mut named: Vec<(&str, &str)> = stores
        .iter()
        .map(|s| (s.name.as_str(), s.rel.as_str()))
        .collect();
    named.extend(
        users
            .values()
            .map(|(_, label, rel, _)| (label.as_str(), rel.as_str())),
    );
    let shown_labels = labels(&named);
    let mut map = MapBuilder::new(
        "stores",
        "State management",
        "each store — Pinia, Redux, Zustand, Vuex, Svelte, Jotai, nanostores, MobX, NgRx, contexts — with the state and actions it holds, and an arrow from each component, hook or module that uses it, labelled with what it uses",
    );
    let mut nodes = Vec::with_capacity(stores.len());
    for (i, s) in stores.iter().enumerate() {
        if !shown[i] {
            nodes.push(None);
            continue;
        }
        let at = map.node("store", &format!("{}#{}", s.rel, s.name), &shown_labels[i]);
        map.place(at, &s.rel, s.line);
        let mut head = s.kind.to_string();
        if let Some(id) = s.id.as_deref().filter(|id| *id != s.name) {
            head.push_str(&format!(" · {id}"));
        }
        map.add_detail(at, &head);
        for (what, names) in [
            ("state", &s.state),
            ("getters", &s.getters),
            ("actions", &s.actions),
        ] {
            if !names.is_empty() {
                map.add_detail(at, &format!("{what}: {}", names.join(", ")));
            }
        }
        nodes.push(at);
    }
    for (root, part, key) in &part_arrows {
        map.edge(nodes[*root], nodes[*part], key);
    }
    for (i, s) in stores.iter().enumerate() {
        for source in &s.from {
            if let Some(j) = stores
                .iter()
                .position(|t| t.rel == s.rel && &t.name == source)
            {
                map.edge(nodes[i], nodes[j], "derives from");
            }
        }
    }
    let mut user_nodes = HashMap::new();
    for (n, (key, (kind, _, rel, line))) in users.iter().enumerate() {
        let at = map.node(kind, key, &shown_labels[stores.len() + n]);
        map.place(at, rel, *line);
        user_nodes.insert(key.clone(), at);
    }
    for ((user, store), what) in &arrows {
        let mut label: Vec<&str> = what.iter().map(String::as_str).take(5).collect();
        if what.len() > 5 {
            label.push("…");
        }
        map.edge(user_nodes[user], nodes[*store], &label.join(", "));
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

    fn detail(map: &ProjectMap, label: &str) -> String {
        map.nodes
            .iter()
            .find(|n| n.label == label)
            .unwrap_or_else(|| panic!("{label} in {:?}", map.nodes))
            .detail
            .clone()
    }

    #[test]
    fn pinia_vuex_and_their_components() {
        let map = map(&Corpus::of(&[
            (
                "src/stores/cart.ts",
                "import { defineStore } from 'pinia'\nexport const useCartStore = defineStore('cart', {\n  state: () => ({ items: [], coupon: null }),\n  getters: { total: (s) => 0 },\n  actions: { add(item) {}, clear() {} },\n})\n",
            ),
            (
                "src/stores/user.ts",
                "import { defineStore } from 'pinia'\nimport { ref, computed } from 'vue'\nexport const useUserStore = defineStore('user', () => {\n  const name = ref('')\n  const loggedIn = computed(() => !!name.value)\n  function logout() {}\n  return { name, loggedIn, logout }\n})\n",
            ),
            (
                "src/components/Cart.vue",
                "<template><p>{{ cart.total }}</p></template>\n<script setup lang=\"ts\">\nimport { storeToRefs } from 'pinia'\nimport { useCartStore } from '@/stores/cart'\nconst cart = useCartStore()\nconst { items } = storeToRefs(cart)\ncart.add(1)\n</script>\n",
            ),
            (
                "src/composables/useSession.ts",
                "import { useUserStore } from '../stores/user'\nexport function useSession() {\n  const { name, logout } = useUserStore()\n  return { name, logout }\n}\n",
            ),
            (
                "legacy/store/index.js",
                "import { createStore } from 'vuex'\nimport cart from './modules/cart'\nconst store = createStore({\n  state: { ready: false },\n  mutations: { setReady(s) {} },\n  modules: { cart },\n})\nexport default store\n",
            ),
            (
                "legacy/store/modules/cart.js",
                "export default {\n  namespaced: true,\n  state: () => ({ lines: [] }),\n  getters: { count: (s) => s.lines.length },\n  actions: { checkout() {} },\n}\n",
            ),
            (
                "legacy/components/CartBadge.vue",
                "<template><b>{{ $store.state.ready }}</b></template>\n<script>\nimport { mapGetters } from 'vuex'\nexport default { computed: { ...mapGetters('cart', ['count']) }, methods: { go() { this.$store.dispatch('cart/checkout') } } }\n</script>\n",
            ),
        ]))
        .unwrap();
        assert_eq!(
            detail(&map, "useCartStore"),
            "Pinia store · cart\nstate: items, coupon\ngetters: total\nactions: add, clear"
        );
        assert_eq!(
            detail(&map, "useUserStore"),
            "Pinia store · user\nstate: name\ngetters: loggedIn\nactions: logout"
        );
        assert_eq!(
            detail(&map, "cart"),
            "Vuex module\nstate: lines\ngetters: count\nactions: checkout"
        );
        let got = edges(&map);
        for want in [
            e("Cart", "useCartStore", "add, items"),
            e("useSession", "useUserStore", "name, logout"),
            e("store", "cart", "cart"),
            e("CartBadge", "cart", "checkout"),
            e("CartBadge", "store", "ready"),
        ] {
            assert!(got.contains(&want), "{want:?} in {got:?}");
        }
    }

    #[test]
    fn redux_zustand_jotai_svelte_and_contexts() {
        let map = map(&Corpus::of(&[
            (
                "src/features/cart/cartSlice.ts",
                "import { createSlice } from '@reduxjs/toolkit'\nconst initialState: CartState = { lines: [], open: false }\nexport const cartSlice = createSlice({\n  name: 'cart',\n  initialState,\n  reducers: { addLine(state, a) {}, toggle: (state) => {} },\n})\nexport const { addLine, toggle } = cartSlice.actions\nexport const selectLines = (s: RootState) => s.cart.lines\nexport default cartSlice.reducer\n",
            ),
            (
                "src/app/store.ts",
                "import { configureStore } from '@reduxjs/toolkit'\nimport cartReducer from '../features/cart/cartSlice'\nexport const store = configureStore({ reducer: { cart: cartReducer } })\n",
            ),
            (
                "src/features/cart/Cart.tsx",
                "import { useSelector, useDispatch } from 'react-redux'\nimport { addLine, selectLines } from './cartSlice'\nexport function Cart() {\n  const open = useSelector((state: RootState) => state.cart.open)\n  const lines = useSelector(selectLines)\n  return <button onClick={() => dispatch(addLine(1))} />\n}\n",
            ),
            (
                "src/bears.ts",
                "import { create } from 'zustand'\nimport { persist } from 'zustand/middleware'\nexport const useBearStore = create<BearState>()(persist((set) => ({\n  bears: 0,\n  honey: 10,\n  increase: () => set((s) => ({ bears: s.bears + 1 })),\n}), { name: 'bears' }))\n",
            ),
            (
                "src/Bears.tsx",
                "import { useBearStore } from './bears'\nexport const Bears = () => {\n  const bears = useBearStore((s) => s.bears)\n  return <p onClick={() => useBearStore.getState().increase()}>{bears}</p>\n}\n",
            ),
            (
                "src/atoms.ts",
                "import { atom } from 'jotai'\nexport const countAtom = atom(0)\nexport const doubleAtom = atom((get) => get(countAtom) * 2)\n",
            ),
            (
                "src/Count.tsx",
                "import { useAtomValue } from 'jotai'\nimport { doubleAtom } from './atoms'\nexport function Count() { const d = useAtomValue(doubleAtom); return <p>{d}</p> }\n",
            ),
            (
                "src/lib/stores.ts",
                "import { writable, derived } from 'svelte/store'\nexport const todos = writable([])\nexport const left = derived(todos, ($t) => $t.length)\n",
            ),
            (
                "src/routes/+page.svelte",
                "<script>\n  import { left } from '$lib/stores'\n</script>\n<p>{$left} left</p>\n",
            ),
            (
                "src/auth.tsx",
                "import { createContext, useContext } from 'react'\nconst AuthContext = createContext(null)\nexport function AuthProvider({ children }) { return <AuthContext.Provider value={1}>{children}</AuthContext.Provider> }\nexport const useAuth = () => useContext(AuthContext)\n",
            ),
            (
                "src/Header.tsx",
                "import { useAuth } from './auth'\nexport function Header() { const auth = useAuth(); return <h1 /> }\n",
            ),
            (
                "src/todo-store.ts",
                "import { makeAutoObservable } from 'mobx'\nclass TodoStore {\n  todos = []\n  constructor() { makeAutoObservable(this) }\n  get left() { return 0 }\n  add(t) {}\n}\nexport const todoStore = new TodoStore()\n",
            ),
            (
                "src/api.ts",
                "import { todoStore } from './todo-store'\nexport async function load() { todoStore.add(1) }\n",
            ),
        ]))
        .unwrap();
        assert_eq!(
            detail(&map, "cartSlice"),
            "Redux slice · cart\nstate: lines, open\nactions: addLine, toggle"
        );
        assert_eq!(
            detail(&map, "useBearStore"),
            "Zustand store\nstate: bears, honey\nactions: increase"
        );
        assert_eq!(
            detail(&map, "TodoStore"),
            "MobX store\nstate: todos\ngetters: left\nactions: add"
        );
        assert_eq!(detail(&map, "AuthContext"), "Context");
        let got = edges(&map);
        for want in [
            e("store", "cartSlice", "cart"),
            e("Cart", "cartSlice", "addLine, selectLines, open"),
            e("Bears", "useBearStore", "bears, increase"),
            e("doubleAtom", "countAtom", "derives from"),
            e("Count", "doubleAtom", ""),
            e("left", "todos", "derives from"),
            e("Page", "left", ""),
            e("Header", "AuthContext", "useAuth"),
            e("api.ts", "TodoStore", "add"),
        ] {
            assert!(got.contains(&want), "{want:?} in {got:?}");
        }
    }
}
