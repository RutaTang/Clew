//! A web app's routes: each route (its method and path) and the handler it
//! reaches, read from how each framework declares them —
//!
//! - Python: Flask and FastAPI decorators (`@app.get("/x")`,
//!   `@bp.route("/x", methods=[…])`, `add_url_rule`, `add_api_route`), a
//!   router's or blueprint's prefix, and where it is mounted, in any file
//!   (`include_router(…, prefix=…)`, `register_blueprint(…, url_prefix=…)`,
//!   nested); Django's `urls.py` (`path`, `re_path`, `url`, `include`);
//! - JavaScript and TypeScript: Express-style servers (`app.get("/x", h)`,
//!   `router.route("/x").post(h)`), NestJS controllers (`@Controller` and
//!   `@Get`), Next.js pages and app-router files;
//! - Java: Spring's `@RequestMapping`, `@GetMapping`, …;
//! - C#: ASP.NET Core's `[Route]` and `[HttpGet]` attributes, and minimal
//!   APIs (`app.MapGet("/x", …)`);
//! - PHP: Laravel's `Route::get(…)` and `Route::resource(…)`, Symfony's
//!   `#[Route]`;
//! - Ruby: Rails' `config/routes.rb` (verbs, `resources`, `resource`,
//!   `root`, `namespace` and `scope` blocks);
//! - Go: `net/http` (`HandleFunc`, Go 1.22's `"GET /x"` patterns), gin,
//!   echo and chi.
//!
//! A handler named rather than written in place is found by name — a member
//! of the class named, else the definition nearest the route's file. A
//! JavaScript router mounted at a prefix in another file (`app.use("/api",
//! router)`) or a Laravel `prefix` group is not followed: its routes show
//! their own paths.

use std::collections::HashMap;

use clew_protocol::ProjectMap;
use tree_sitter::Node;

use super::{Corpus, MapBuilder, Source, all_of, last_name, line, text, unquote};
use crate::highlight::Lang;

#[path = "routes_js.rs"]
mod js;

pub fn maps(corpus: &Corpus) -> Vec<ProjectMap> {
    let mut routes = Routes {
        map: MapBuilder::new(
            "routes",
            "Routes",
            "each route, with its method and path, and an arrow to the handler it reaches (an inline handler is shown as the route itself)",
        ),
        corpus,
        mounts: python_mounts(corpus),
        js_mounts: js::Mounts::read(corpus),
    };
    let next = corpus.files.iter().any(|s| {
        s.name().starts_with("next.config.")
            || (s.name() == "package.json" && s.text.contains("\"next\""))
    });
    for source in &corpus.files {
        match source.lang {
            Some(Lang::Python) => python(&mut routes, source),
            Some(Lang::JavaScript | Lang::TypeScript | Lang::Tsx)
                if !super::is_js_test(&source.rel) =>
            {
                javascript(&mut routes, source);
                if next {
                    nextjs(&mut routes, source);
                }
                if js::may_route(&source.text)
                    && let Some(tree) = source.tree()
                {
                    let root = tree.root_node();
                    js::bun(&mut routes, source, root);
                    js::hapi(&mut routes, source, root);
                    js::route_objects(&mut routes, source, root);
                    js::route_elements(&mut routes, source, root);
                    js::tanstack(&mut routes, source, root);
                    js::inject_route(&mut routes, source, root);
                }
            }
            Some(Lang::Java) => java(&mut routes, source),
            Some(Lang::CSharp) => csharp(&mut routes, source),
            Some(Lang::Php) => php(&mut routes, source),
            Some(Lang::Ruby) if source.rel.ends_with("config/routes.rb") => {
                rails(&mut routes, source)
            }
            Some(Lang::Go) => go(&mut routes, source),
            Some(Lang::Kotlin) => kotlin(&mut routes, source),
            Some(Lang::Elixir) if source.rel.ends_with("router.ex") => phoenix(&mut routes, source),
            None if super::is_play_routes(&source.rel) => play(&mut routes, source),
            _ => {}
        }
    }
    js::trpc(&mut routes, corpus);
    js::file_routes(&mut routes, corpus);
    routes.map.finish().into_iter().collect()
}

/// What a route reaches.
enum Handler {
    /// Defined right here: `label`, at `line` of the route's file (or of
    /// `rel`, when another).
    At {
        rel: String,
        line: usize,
        label: String,
    },
    /// Named: found by name — a member of `class`, when one is named.
    Named { name: String, class: Option<String> },
    /// Written in place, in the route's own call.
    Inline,
}

struct Routes<'c> {
    map: MapBuilder,
    corpus: &'c Corpus,
    /// Where each Python router or blueprint is served ([`python_mounts`]).
    mounts: PyMounts,
    /// Where each JavaScript router is mounted ([`js::Mounts`]).
    js_mounts: js::Mounts,
}

impl Routes<'_> {
    fn add(
        &mut self,
        framework: &str,
        method: &str,
        path: &str,
        rel: &str,
        at: usize,
        handler: Handler,
    ) {
        let path = normalize(path);
        let label = format!("{} {path}", method.to_ascii_uppercase());
        let route = self.map.node("route", &label, &label);
        self.map.place(route, rel, at);
        if self
            .map
            .get(route.unwrap_or(usize::MAX))
            .is_some_and(|n| n.detail.is_empty())
        {
            self.map.detail(route, framework);
        }
        let target = match handler {
            Handler::Inline => return,
            Handler::At { rel, line, label } => {
                let h = self.map.node("handler", &format!("{rel}:{line}"), &label);
                self.map.place(h, &rel, line);
                h
            }
            Handler::Named { name, class } => {
                match self.corpus.defs().find(&name, class.as_deref(), rel) {
                    Some(def) => {
                        let label = match &def.class {
                            Some(c) => format!("{c}.{}", def.name),
                            None => def.name.clone(),
                        };
                        let h =
                            self.map
                                .node("handler", &format!("{}:{}", def.rel, def.line), &label);
                        self.map.place(h, &def.rel, def.line);
                        h
                    }
                    None => {
                        let label = match &class {
                            Some(c) => format!("{c}.{name}"),
                            None => name,
                        };
                        self.map.node("handler", &label, &label)
                    }
                }
            }
        };
        self.map.edge(route, target, "handled by");
    }
}

/// A path with one leading `/`, `//` folded, no trailing `/` but the root's.
fn normalize(path: &str) -> String {
    let mut out = String::from("/");
    for part in path.split('/').filter(|p| !p.is_empty()) {
        if out.len() > 1 {
            out.push('/');
        }
        out.push_str(part);
    }
    out
}

fn join(prefix: &str, path: &str) -> String {
    format!(
        "{}/{}",
        prefix.trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}

/// A string literal's content.
fn string(node: Node<'_>, src: &str) -> Option<String> {
    unquote(text(node, src))
}

/// The named arguments of a call (`arguments` field), comments aside.
fn args<'t>(call: Node<'t>) -> Vec<Node<'t>> {
    let Some(list) = call.child_by_field_name("arguments") else {
        return Vec::new();
    };
    let mut cursor = list.walk();
    list.named_children(&mut cursor)
        .filter(|n| !n.kind().contains("comment"))
        .collect()
}

const VERBS: &[&str] = &["get", "post", "put", "patch", "delete", "head", "options"];

// -- Python --------------------------------------------------------------------------

fn python(routes: &mut Routes, source: &Source) {
    let Some(tree) = source.tree() else {
        return;
    };
    let src = source.text.as_str();
    let root = tree.root_node();
    // `@app.get(...)` is Flask's (2.0 on) and FastAPI's alike: the file's
    // own import says whose. A file importing neither says both.
    let imports = |module: &str| {
        let named = |token: &str| {
            token == module
                || token
                    .strip_prefix(module)
                    .is_some_and(|rest| rest.starts_with('.'))
        };
        src.lines().map(str::trim_start).any(|line| {
            if let Some(rest) = line.strip_prefix("from ") {
                rest.split_whitespace().next().is_some_and(named)
            } else if let Some(rest) = line.strip_prefix("import ") {
                rest.split(',')
                    .filter_map(|part| part.split_whitespace().next())
                    .any(named)
            } else {
                false
            }
        })
    };
    let either = match (imports("flask"), imports("fastapi")) {
        (true, false) => "Flask",
        (false, true) => "FastAPI",
        _ => "FastAPI / Flask",
    };
    for decorated in all_of(root, "decorated_definition") {
        let Some(def) = decorated.child_by_field_name("definition") else {
            continue;
        };
        let Some(name) = def.child_by_field_name("name") else {
            continue;
        };
        let handler_name = text(name, src).to_string();
        let mut cursor = decorated.walk();
        for decorator in decorated
            .named_children(&mut cursor)
            .filter(|n| n.kind() == "decorator")
        {
            let Some(call) = decorator.named_child(0).filter(|c| c.kind() == "call") else {
                continue;
            };
            let Some(function) = call
                .child_by_field_name("function")
                .filter(|f| f.kind() == "attribute")
            else {
                continue;
            };
            let (Some(object), Some(attr)) = (
                function.child_by_field_name("object"),
                function.child_by_field_name("attribute"),
            ) else {
                continue;
            };
            let verb = text(attr, src);
            let mut methods: Vec<String> = match verb {
                "route" | "api_route" => Vec::new(),
                "websocket" => vec!["WS".into()],
                v if VERBS.contains(&v) => vec![v.to_ascii_uppercase()],
                _ => continue,
            };
            let all = args(call);
            let Some(path) = all
                .first()
                .filter(|a| a.kind() == "string")
                .and_then(|a| string(*a, src))
            else {
                continue;
            };
            if !path.starts_with('/') && !path.is_empty() {
                continue;
            }
            for arg in &all {
                if arg.kind() == "keyword_argument"
                    && arg
                        .child_by_field_name("name")
                        .is_some_and(|n| text(n, src) == "methods")
                    && let Some(list) = arg.child_by_field_name("value")
                {
                    let mut c = list.walk();
                    methods.extend(
                        list.named_children(&mut c)
                            .filter_map(|m| string(m, src))
                            .map(|m| m.to_ascii_uppercase()),
                    );
                }
            }
            if methods.is_empty() {
                methods.push("GET".into());
            }
            let framework = if verb == "route" { "Flask" } else { either };
            for prefix in routes.mounts.prefixes(&source.rel, text(object, src)) {
                for method in &methods {
                    routes.add(
                        framework,
                        method,
                        &join(&prefix, &path),
                        &source.rel,
                        line(decorator),
                        Handler::At {
                            rel: source.rel.clone(),
                            line: line(def),
                            label: handler_name.clone(),
                        },
                    );
                }
            }
        }
    }
    // `app.add_url_rule("/x", "name", view)` (Flask) and
    // `app.add_api_route("/x", endpoint)` (FastAPI): the handler is named.
    for call in all_of(root, "call") {
        let Some(function) = call
            .child_by_field_name("function")
            .filter(|f| f.kind() == "attribute")
        else {
            continue;
        };
        let (Some(object), Some(attr)) = (
            function.child_by_field_name("object"),
            function.child_by_field_name("attribute"),
        ) else {
            continue;
        };
        let (framework, handler_at) = match text(attr, src) {
            "add_url_rule" => ("Flask", 2),
            "add_api_route" => ("FastAPI", 1),
            _ => continue,
        };
        let all = args(call);
        let positional: Vec<Node> = all
            .iter()
            .copied()
            .filter(|a| a.kind() != "keyword_argument")
            .collect();
        let keyword = |key: &str| {
            all.iter().copied().find_map(|a| {
                (a.kind() == "keyword_argument"
                    && a.child_by_field_name("name")
                        .is_some_and(|n| text(n, src) == key))
                .then(|| a.child_by_field_name("value"))
                .flatten()
            })
        };
        let Some(path) = positional
            .first()
            .copied()
            .or_else(|| keyword("rule"))
            .or_else(|| keyword("path"))
            .and_then(|a| string(a, src))
            .filter(|p| p.starts_with('/'))
        else {
            continue;
        };
        let view = positional
            .get(handler_at)
            .copied()
            .or_else(|| keyword("view_func"))
            .or_else(|| keyword("endpoint"));
        let handler = match view.map(|v| view_name(v, src)) {
            Some(name) if !name.is_empty() => Handler::Named { name, class: None },
            _ => Handler::Inline,
        };
        let mut methods: Vec<String> = keyword("methods")
            .map(|list| {
                let mut c = list.walk();
                list.named_children(&mut c)
                    .filter_map(|m| string(m, src))
                    .map(|m| m.to_ascii_uppercase())
                    .collect()
            })
            .unwrap_or_default();
        if methods.is_empty() {
            methods.push("GET".into());
        }
        for prefix in routes.mounts.prefixes(&source.rel, text(object, src)) {
            for method in &methods {
                let handler = match &handler {
                    Handler::Named { name, class } => Handler::Named {
                        name: name.clone(),
                        class: class.clone(),
                    },
                    _ => Handler::Inline,
                };
                routes.add(
                    framework,
                    method,
                    &join(&prefix, &path),
                    &source.rel,
                    line(call),
                    handler,
                );
            }
        }
    }
    if source.name() == "urls.py" {
        django(routes, source, root);
    }
}

/// The name of a view given by value: `views.index` → `index`, a class's
/// `ItemView.as_view("item")` → `ItemView`.
fn view_name(view: Node<'_>, src: &str) -> String {
    let written = match view.kind() {
        "call" => view
            .child_by_field_name("function")
            .map(|f| text(f, src).trim_end_matches(".as_view"))
            .unwrap_or(""),
        "identifier" | "attribute" => text(view, src),
        _ => "",
    };
    last_name(written)
}

/// Most mounts followed up from a router to its app: nesting deeper than
/// this, or a cycle, is not a real app's.
const MAX_MOUNT_DEPTH: usize = 8;

/// Where each Python router (FastAPI's `APIRouter`) and blueprint (Flask's
/// `Blueprint`) is served: its own prefix, where it is mounted in any file
/// (`include_router(r, prefix=…)`, `register_blueprint(bp, url_prefix=…)`),
/// and where what it is mounted on is in turn.
#[derive(Default)]
struct PyMounts {
    /// `(file, name)` of each router or blueprint, and its own prefix.
    routers: HashMap<(String, String), RouterDef>,
    /// Each mount: what is mounted where.
    mounts: Vec<Mount>,
    /// Each file's imported names: local name → (module, the name imported
    /// from it, if one was).
    imports: HashMap<String, HashMap<String, (String, Option<String>)>>,
}

struct RouterDef {
    prefix: String,
    flask: bool,
}

struct Mount {
    rel: String,
    /// What it is mounted on, as written in `rel`.
    parent: String,
    /// What is mounted, as written in `rel`.
    child: String,
    prefix: Option<String>,
}

impl PyMounts {
    /// The router or blueprint `written` names in `rel`: one defined there,
    /// or one imported from the module whose file it is (or the only one of
    /// its name).
    fn resolve(&self, rel: &str, written: &str) -> Option<(String, String)> {
        let here = (rel.to_string(), written.to_string());
        if self.routers.contains_key(&here) {
            return Some(here);
        }
        let imports = self.imports.get(rel);
        let (module, name) = match written.rsplit_once('.') {
            // `users.router`: the module `users` (imported as such), name `router`.
            Some((module, name)) => {
                let module = match imports.and_then(|i| i.get(module)) {
                    Some((from, Some(imported))) => format!("{from}.{imported}"),
                    Some((from, None)) => from.clone(),
                    None => module.to_string(),
                };
                (Some(module), name.to_string())
            }
            None => match imports.and_then(|i| i.get(written)) {
                Some((from, imported)) => (
                    Some(from.clone()),
                    imported.clone().unwrap_or_else(|| written.to_string()),
                ),
                None => (None, written.to_string()),
            },
        };
        let named: Vec<&(String, String)> =
            self.routers.keys().filter(|(_, n)| *n == name).collect();
        let stem = module
            .as_deref()
            .map(|m| m.trim_start_matches('.').rsplit('.').next().unwrap_or(m));
        let of_module = |key: &&&(String, String)| {
            let Some(stem) = stem else {
                return false;
            };
            let path = std::path::Path::new(&key.0);
            let file = path.file_stem().and_then(|s| s.to_str());
            let dir = path
                .parent()
                .and_then(|d| d.file_name())
                .and_then(|d| d.to_str());
            file == Some(stem) || (file == Some("__init__") && dir == Some(stem))
        };
        match named
            .iter()
            .filter(of_module)
            .collect::<Vec<_>>()
            .as_slice()
        {
            [one] => Some((**one).clone()),
            _ if named.len() == 1 => Some(named[0].clone()),
            _ => None,
        }
    }

    /// Every full prefix `written`'s routes in `rel` are served under: one
    /// per place it is mounted; its own prefix where it is mounted nowhere
    /// known; "" for an app.
    fn prefixes(&self, rel: &str, written: &str) -> Vec<String> {
        match self.resolve(rel, written) {
            Some(key) => self.served(&key, 0),
            None => vec![String::new()],
        }
    }

    fn served(&self, key: &(String, String), depth: usize) -> Vec<String> {
        let Some(def) = self.routers.get(key) else {
            return vec![String::new()];
        };
        let mut out: Vec<String> = Vec::new();
        if depth < MAX_MOUNT_DEPTH {
            for mount in &self.mounts {
                if self.resolve(&mount.rel, &mount.child).as_ref() != Some(key) {
                    continue;
                }
                let above = match self.resolve(&mount.rel, &mount.parent) {
                    Some(parent) if &parent != key => self.served(&parent, depth + 1),
                    Some(_) => continue,
                    None => vec![String::new()],
                };
                // Flask's `url_prefix` at registration replaces the
                // blueprint's own; FastAPI's goes before the router's.
                let own = match (def.flask, &mount.prefix) {
                    (true, Some(given)) => given.clone(),
                    (true, None) => def.prefix.clone(),
                    (false, given) => join(given.as_deref().unwrap_or(""), &def.prefix),
                };
                for above in above {
                    let full = join(&above, &own);
                    if !out.contains(&full) {
                        out.push(full);
                    }
                }
            }
        }
        if out.is_empty() {
            out.push(def.prefix.clone());
        }
        out
    }
}

/// [`PyMounts`] of every Python file.
fn python_mounts(corpus: &Corpus) -> PyMounts {
    let mut mounts = PyMounts::default();
    for source in corpus.in_langs(&[Lang::Python]) {
        let Some(tree) = source.tree() else {
            continue;
        };
        let src = source.text.as_str();
        let root = tree.root_node();
        let keyword = |call: Node<'_>, keys: &[&str]| {
            args(call).into_iter().find_map(|a| {
                (a.kind() == "keyword_argument"
                    && a.child_by_field_name("name")
                        .is_some_and(|n| keys.contains(&text(n, src))))
                .then(|| a.child_by_field_name("value"))
                .flatten()
                .and_then(|v| string(v, src))
            })
        };
        for assignment in all_of(root, "assignment") {
            let (Some(left), Some(right)) = (
                assignment.child_by_field_name("left"),
                assignment.child_by_field_name("right"),
            ) else {
                continue;
            };
            if right.kind() != "call" || left.kind() != "identifier" {
                continue;
            }
            let made = right
                .child_by_field_name("function")
                .map(|f| last_name(text(f, src)))
                .unwrap_or_default();
            let flask = match made.as_str() {
                "APIRouter" => false,
                "Blueprint" => true,
                _ => continue,
            };
            mounts.routers.insert(
                (source.rel.clone(), text(left, src).to_string()),
                RouterDef {
                    prefix: keyword(right, &["prefix", "url_prefix"]).unwrap_or_default(),
                    flask,
                },
            );
        }
        for call in all_of(root, "call") {
            let Some(function) = call
                .child_by_field_name("function")
                .filter(|f| f.kind() == "attribute")
            else {
                continue;
            };
            let (Some(object), Some(attr)) = (
                function.child_by_field_name("object"),
                function.child_by_field_name("attribute"),
            ) else {
                continue;
            };
            if !matches!(text(attr, src), "include_router" | "register_blueprint") {
                continue;
            }
            let Some(child) = args(call)
                .into_iter()
                .find(|a| matches!(a.kind(), "identifier" | "attribute"))
            else {
                continue;
            };
            mounts.mounts.push(Mount {
                rel: source.rel.clone(),
                parent: text(object, src).to_string(),
                child: text(child, src).to_string(),
                prefix: keyword(call, &["prefix", "url_prefix"]),
            });
        }
        let mut names: HashMap<String, (String, Option<String>)> = HashMap::new();
        for statement in all_of(root, "import_from_statement") {
            let Some(module) = statement.child_by_field_name("module_name") else {
                continue;
            };
            let module = text(module, src).to_string();
            let mut cursor = statement.walk();
            for name in statement.children_by_field_name("name", &mut cursor) {
                let (imported, local) = match name.kind() {
                    "aliased_import" => (
                        name.child_by_field_name("name").map(|n| text(n, src)),
                        name.child_by_field_name("alias").map(|n| text(n, src)),
                    ),
                    _ => (Some(text(name, src)), Some(text(name, src))),
                };
                if let (Some(imported), Some(local)) = (imported, local) {
                    names.insert(
                        local.to_string(),
                        (module.clone(), Some(imported.to_string())),
                    );
                }
            }
        }
        for statement in all_of(root, "import_statement") {
            let mut cursor = statement.walk();
            for name in statement.children_by_field_name("name", &mut cursor) {
                let (module, local) = match name.kind() {
                    "aliased_import" => (
                        name.child_by_field_name("name").map(|n| text(n, src)),
                        name.child_by_field_name("alias").map(|n| text(n, src)),
                    ),
                    _ => (Some(text(name, src)), Some(text(name, src))),
                };
                if let (Some(module), Some(local)) = (module, local) {
                    names.insert(local.to_string(), (module.to_string(), None));
                }
            }
        }
        mounts.imports.insert(source.rel.clone(), names);
    }
    mounts
}

fn django(routes: &mut Routes, source: &Source, root: Node<'_>) {
    let src = source.text.as_str();
    for call in all_of(root, "call") {
        let Some(function) = call.child_by_field_name("function") else {
            continue;
        };
        if !matches!(text(function, src), "path" | "re_path" | "url") {
            continue;
        }
        let all = args(call);
        let (Some(route), Some(view)) = (all.first(), all.get(1)) else {
            continue;
        };
        let Some(path) = string(*route, src) else {
            continue;
        };
        let path = path
            .trim_start_matches('^')
            .trim_end_matches('$')
            .to_string();
        // `include("app.urls")`: the routes of another module, under this path.
        if view.kind() == "call"
            && view
                .child_by_field_name("function")
                .is_some_and(|f| text(f, src) == "include")
        {
            let module = args(*view)
                .first()
                .and_then(|m| string(*m, src))
                .unwrap_or_else(|| text(*view, src).to_string());
            let label = format!("ANY {}", normalize(&path));
            let at = routes.map.node("route", &label, &label);
            routes.map.place(at, &source.rel, line(call));
            routes.map.detail(at, "Django");
            let rel = format!("{}.py", module.replace('.', "/"));
            let included = routes.map.node("urls", &module, &module);
            if let Some(found) = routes.corpus.files.iter().find(|s| s.rel.ends_with(&rel)) {
                routes.map.place(included, &found.rel, 1);
            }
            routes.map.edge(at, included, "includes");
            continue;
        }
        // `views.user`, or `views.UserView.as_view()`.
        let reference = match view.kind() {
            "call" => view
                .child_by_field_name("function")
                .map(|f| text(f, src).trim_end_matches(".as_view").to_string())
                .unwrap_or_default(),
            _ => text(*view, src).to_string(),
        };
        routes.add(
            "Django",
            "ANY",
            &path,
            &source.rel,
            line(call),
            Handler::Named {
                name: last_name(&reference),
                class: None,
            },
        );
    }
}

// -- JavaScript and TypeScript -------------------------------------------------------

/// Names an Express-style server or router goes by, when the file does not
/// say what it is.
const SERVER_NAMES: &[&str] = &[
    "app", "router", "server", "api", "routes", "fastify", "r", "route",
];

fn javascript(routes: &mut Routes, source: &Source) {
    // A route is a verb's call or a NestJS controller's: a file with
    // neither has none (and is not parsed for it).
    let says = |word: &str| source.text.contains(word);
    let calls = VERBS
        .iter()
        .chain(&["all", "route", "use", "on"])
        .any(|v| says(&format!(".{v}(")));
    if !(calls || says("@Controller")) {
        return;
    }
    let Some(tree) = source.tree() else {
        return;
    };
    let src = source.text.as_str();
    let root = tree.root_node();
    // Variables made servers or routers — `const x = express()`,
    // `Router()`, `new Hono()` — each with its framework; the usual names
    // with the file's.
    let usual = js::file_framework(src);
    let mut servers: Vec<(String, &str)> = SERVER_NAMES
        .iter()
        .map(|s| (s.to_string(), usual))
        .collect();
    for declarator in all_of(root, "variable_declarator") {
        if let (Some(name), Some(value)) = (
            declarator.child_by_field_name("name"),
            declarator.child_by_field_name("value"),
        ) && let Some(framework) = js::made_by(text(value, src))
        {
            let name = text(name, src).to_string();
            servers.retain(|(n, _)| *n != name);
            servers.push((name, framework));
        }
    }
    for call in all_of(root, "call_expression") {
        let Some(function) = call
            .child_by_field_name("function")
            .filter(|f| f.kind() == "member_expression")
        else {
            continue;
        };
        let (Some(object), Some(property)) = (
            function.child_by_field_name("object"),
            function.child_by_field_name("property"),
        ) else {
            continue;
        };
        let verb = text(property, src);
        if !(VERBS.contains(&verb) || verb == "all") {
            continue;
        }
        let all = args(call);
        // What the route is made on: a server variable, or an app made in
        // place and chained (`new Hono().get(…).post(…)`).
        let root = js::chain_root(object);
        let on = text(root, src);
        let server = servers
            .iter()
            .find(|(n, _)| n == on)
            .map(|(n, f)| (n.clone(), *f))
            .or_else(|| js::made_by(on).map(|f| (String::new(), f)));
        // `router.route("/x").get(h).post(h2)`: the path is the route's,
        // down the chain.
        let (path, handlers) = if let Some(route) = route_call(object, src) {
            let Some(path) = args(route).first().and_then(|a| string(*a, src)) else {
                continue;
            };
            (path, all.as_slice())
        } else {
            if server.is_none() {
                continue;
            }
            let Some(path) = all.first().and_then(|a| string(*a, src)) else {
                continue;
            };
            if all.len() < 2 {
                continue;
            }
            (path, &all[1..])
        };
        let (var, framework) = server.unwrap_or_else(|| (on.to_string(), usual));
        if !path.starts_with('/') && path != "*" {
            continue;
        }
        let handler = match handlers.last() {
            Some(h) if matches!(h.kind(), "identifier" | "member_expression") => Handler::Named {
                name: last_name(text(*h, src)),
                class: None,
            },
            _ => Handler::Inline,
        };
        let method = if verb == "all" { "ANY" } else { verb };
        let path = join(&routes.js_mounts.prefix(&source.rel, &var), &path);
        routes.add(framework, method, &path, &source.rel, line(call), handler);
    }
    nest(routes, source, root);
}

/// The `.route(path)` call a chain of verb calls hangs from
/// (`r.route("/x").get(a).post(b)`).
fn route_call<'t>(mut node: Node<'t>, src: &str) -> Option<Node<'t>> {
    while node.kind() == "call_expression" {
        let function = node.child_by_field_name("function")?;
        if function.kind() != "member_expression" {
            return None;
        }
        let property = text(function.child_by_field_name("property")?, src);
        if property == "route" {
            return Some(node);
        }
        if !(VERBS.contains(&property) || property == "all") {
            return None;
        }
        node = function.child_by_field_name("object")?;
    }
    None
}

/// The decorators on a class or method: its own `decorator` children and, in
/// a class body or an export, the ones just before it.
fn decorators<'t>(node: Node<'t>) -> Vec<Node<'t>> {
    let mut out = Vec::new();
    let mut cursor = node.walk();
    out.extend(
        node.children(&mut cursor)
            .filter(|c| c.kind() == "decorator"),
    );
    let mut previous = node.prev_sibling();
    while let Some(p) = previous {
        if p.kind() != "decorator" {
            break;
        }
        out.push(p);
        previous = p.prev_sibling();
    }
    if let Some(parent) = node.parent().filter(|p| p.kind() == "export_statement") {
        let mut cursor = parent.walk();
        out.extend(
            parent
                .children(&mut cursor)
                .filter(|c| c.kind() == "decorator"),
        );
    }
    out
}

/// A decorator's name and first string argument (`@Get(':id')` →
/// `("Get", Some(":id"))`).
fn decorator_call(decorator: Node<'_>, src: &str) -> Option<(String, Option<String>)> {
    let inner = decorator.named_child(0)?;
    match inner.kind() {
        "call_expression" => {
            let name = text(inner.child_by_field_name("function")?, src).to_string();
            let arg = args(inner).first().and_then(|a| string(*a, src));
            Some((name, arg))
        }
        "identifier" => Some((text(inner, src).to_string(), None)),
        _ => None,
    }
}

fn nest(routes: &mut Routes, source: &Source, root: Node<'_>) {
    let src = source.text.as_str();
    for class in all_of(root, "class_declaration") {
        let Some(prefix) = decorators(class)
            .into_iter()
            .filter_map(|d| decorator_call(d, src))
            .find(|(n, _)| n == "Controller")
            .map(|(_, p)| p.unwrap_or_default())
        else {
            continue;
        };
        let class_name = class
            .child_by_field_name("name")
            .map(|n| text(n, src).to_string())
            .unwrap_or_default();
        for method in all_of(class, "method_definition") {
            let Some(name) = method.child_by_field_name("name") else {
                continue;
            };
            for (verb, path) in decorators(method)
                .into_iter()
                .filter_map(|d| decorator_call(d, src))
            {
                let verb = match verb.as_str() {
                    "Get" | "Post" | "Put" | "Patch" | "Delete" | "Head" | "Options" => {
                        verb.to_ascii_uppercase()
                    }
                    "All" => "ANY".into(),
                    _ => continue,
                };
                routes.add(
                    "NestJS",
                    &verb,
                    &join(&prefix, &path.unwrap_or_default()),
                    &source.rel,
                    line(method),
                    Handler::At {
                        rel: source.rel.clone(),
                        line: line(method),
                        label: format!("{class_name}.{}", text(name, src)),
                    },
                );
            }
        }
    }
}

/// Next.js: a page under `pages/`, or `page`/`route` files under `app/`.
fn nextjs(routes: &mut Routes, source: &Source) {
    let rel = source.rel.as_str();
    let Some((dir, file)) = rel.rsplit_once('/').or(Some(("", rel))) else {
        return;
    };
    let stem = file.rsplit_once('.').map_or(file, |(s, _)| s);
    let segments: Vec<&str> = dir.split('/').filter(|s| !s.is_empty()).collect();
    // `[id]` read as `:id`, `[...slug]` as `*slug`, as every other
    // framework's routes are said.
    let route_of = |parts: &[&str]| -> String {
        parts
            .iter()
            .filter(|p| !p.starts_with('@'))
            .filter_map(|p| js::segment(p))
            .collect::<Vec<_>>()
            .join("/")
    };
    if let Some(at) = segments.iter().position(|s| *s == "app") {
        let inside = &segments[at + 1..];
        if stem == "page" {
            routes.add(
                "Next.js",
                "GET",
                &route_of(inside),
                rel,
                1,
                Handler::At {
                    rel: rel.to_string(),
                    line: 1,
                    label: format!("{} (page)", rel),
                },
            );
        } else if stem == "route"
            && let Some(tree) = source.tree()
        {
            let src = source.text.as_str();
            for function in all_of(tree.root_node(), "function_declaration") {
                let Some(name) = function.child_by_field_name("name") else {
                    continue;
                };
                let name = text(name, src);
                if ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"].contains(&name) {
                    routes.add(
                        "Next.js",
                        name,
                        &route_of(inside),
                        rel,
                        line(function),
                        Handler::At {
                            rel: rel.to_string(),
                            line: line(function),
                            label: name.to_string(),
                        },
                    );
                }
            }
        }
        return;
    }
    if let Some(at) = segments.iter().position(|s| *s == "pages")
        && !stem.starts_with('_')
    {
        let mut parts: Vec<&str> = segments[at + 1..].to_vec();
        if stem != "index" {
            parts.push(stem);
        }
        let method = if parts.first() == Some(&"api") {
            "ANY"
        } else {
            "GET"
        };
        routes.add(
            "Next.js",
            method,
            &route_of(&parts),
            rel,
            1,
            Handler::At {
                rel: rel.to_string(),
                line: 1,
                label: rel.to_string(),
            },
        );
    }
}

// -- Java ----------------------------------------------------------------------------

/// A Java annotation's name and its string values (the plain argument, or
/// `value`/`path`), and the methods a `method = RequestMethod.X` names.
fn java_annotation(annotation: Node<'_>, src: &str) -> Option<(String, Vec<String>, Vec<String>)> {
    let name = text(annotation.child_by_field_name("name")?, src).to_string();
    let mut paths = Vec::new();
    let mut methods = Vec::new();
    if let Some(list) = annotation.child_by_field_name("arguments") {
        let mut cursor = list.walk();
        for arg in list.named_children(&mut cursor) {
            match arg.kind() {
                "element_value_pair" => {
                    let key = arg
                        .child_by_field_name("key")
                        .map(|k| text(k, src))
                        .unwrap_or("");
                    let Some(value) = arg.child_by_field_name("value") else {
                        continue;
                    };
                    match key {
                        "value" | "path" => paths.extend(java_strings(value, src)),
                        "method" => {
                            for verb in ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"]
                            {
                                if text(value, src).contains(&format!(".{verb}")) {
                                    methods.push(verb.to_string());
                                }
                            }
                        }
                        _ => {}
                    }
                }
                _ => paths.extend(java_strings(arg, src)),
            }
        }
    }
    Some((name, paths, methods))
}

/// A string, or each string of an `{…}` array.
fn java_strings(node: Node<'_>, src: &str) -> Vec<String> {
    match node.kind() {
        "string_literal" => string(node, src).into_iter().collect(),
        _ => {
            let mut cursor = node.walk();
            node.named_children(&mut cursor)
                .filter(|n| n.kind() == "string_literal")
                .filter_map(|n| string(n, src))
                .collect()
        }
    }
}

fn annotations<'t>(declaration: Node<'t>) -> Vec<Node<'t>> {
    let mut out = Vec::new();
    let mut cursor = declaration.walk();
    for child in declaration.children(&mut cursor) {
        if child.kind() == "modifiers" {
            let mut c = child.walk();
            out.extend(
                child
                    .children(&mut c)
                    .filter(|a| matches!(a.kind(), "annotation" | "marker_annotation")),
            );
        }
    }
    out
}

/// The first quoted string of `text` (`"…"`), as written.
fn first_quoted(text: &str) -> Option<String> {
    let start = text.find('"')? + 1;
    let len = text[start..].find('"')?;
    Some(text[start..start + len].to_string())
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// Kotlin's routes: a Spring controller's mappings, as Java's are
/// (`@GetMapping("/{id}") fun show(…)` under a class's `@RequestMapping`),
/// and Ktor's routing (`route("/api") { get("/users") { … } }`).
fn kotlin(routes: &mut Routes, source: &Source) {
    let src = source.text.as_str();
    let lines: Vec<&str> = src.lines().collect();
    if src.contains("Mapping") {
        // The class each `@RequestMapping` prefix is on, by indent.
        let mut class_prefix: Vec<(usize, String, String)> = Vec::new();
        let mut pending: Vec<(String, Option<String>)> = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            let t = line.trim();
            let depth = indent_of(line);
            if let Some(rest) = t.strip_prefix('@') {
                let name: String = rest.chars().take_while(|c| c.is_alphanumeric()).collect();
                if name.ends_with("Mapping") {
                    pending.push((name, first_quoted(rest)));
                }
                continue;
            }
            let class = t
                .split_whitespace()
                .skip_while(|w| *w != "class")
                .nth(1)
                .map(|w| {
                    w.chars()
                        .take_while(|c| c.is_alphanumeric() || *c == '_')
                        .collect::<String>()
                });
            if let Some(class) = class.filter(|c| !c.is_empty()) {
                let prefix = pending
                    .iter()
                    .find(|(n, _)| n == "RequestMapping")
                    .and_then(|(_, p)| p.clone())
                    .unwrap_or_default();
                class_prefix.retain(|(d, _, _)| *d < depth);
                class_prefix.push((depth, class, prefix));
                pending.clear();
                continue;
            }
            let Some(fun) = t.split_once("fun ").map(|(_, r)| r) else {
                if !t.is_empty() {
                    pending.clear();
                }
                continue;
            };
            let name: String = fun
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            let Some((_, class, prefix)) = class_prefix.iter().rev().find(|(d, _, _)| *d < depth)
            else {
                pending.clear();
                continue;
            };
            for (mapping, path) in pending.drain(..) {
                let method = match mapping.as_str() {
                    "GetMapping" => "GET",
                    "PostMapping" => "POST",
                    "PutMapping" => "PUT",
                    "DeleteMapping" => "DELETE",
                    "PatchMapping" => "PATCH",
                    "RequestMapping" => "ANY",
                    _ => continue,
                };
                routes.add(
                    "Spring",
                    method,
                    &join(prefix, path.as_deref().unwrap_or("")),
                    &source.rel,
                    i + 1,
                    Handler::At {
                        rel: source.rel.clone(),
                        line: i + 1,
                        label: format!("{class}.{name}"),
                    },
                );
            }
        }
    }
    if src.contains("routing") {
        // Each `route("/x") {` open around a line, by its indent.
        let mut open: Vec<(usize, String)> = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            let t = line.trim_start();
            let depth = indent_of(line);
            if t.is_empty() {
                continue;
            }
            open.retain(|(d, _)| *d < depth);
            let word: String = t.chars().take_while(|c| c.is_alphabetic()).collect();
            let after = &t[word.len()..];
            // Its block opens on its line (`get("/x") {`, or a one-line
            // `post("/y") { }`).
            let opens = after.contains('{');
            if !(after.starts_with('(') || after.trim_start().starts_with('{')) {
                continue;
            }
            let path = after
                .strip_prefix('(')
                .and_then(|a| a.split(')').next())
                .and_then(first_quoted)
                .unwrap_or_default();
            let prefix = open.last().map(|(_, p)| p.clone()).unwrap_or_default();
            match word.as_str() {
                "route" if opens => open.push((depth, join(&prefix, &path))),
                "get" | "post" | "put" | "delete" | "patch" | "head" | "options" if opens => {
                    routes.add(
                        "Ktor",
                        &word.to_ascii_uppercase(),
                        &join(&prefix, &path),
                        &source.rel,
                        i + 1,
                        Handler::Inline,
                    );
                }
                _ => {}
            }
        }
    }
}

/// A Phoenix router's routes: each verb's (`get "/users", UserController,
/// :index`), a `resources`' seven (less what `only:` or `except:` leave
/// out), a LiveView's (`live "/x", PageLive`) and a `forward`'s, under the
/// `scope`s around it — their paths joined, their aliases naming the
/// controller's module.
fn phoenix(routes: &mut Routes, source: &Source) {
    let src = source.text.as_str();
    if !src.contains("Phoenix.Router") && !src.contains(":router") {
        return;
    }
    // Each `scope … do` open around a line: its indent, path and alias.
    let mut open: Vec<(usize, String, String)> = Vec::new();
    for (i, line) in src.lines().enumerate() {
        let t = line.trim_start();
        let depth = indent_of(line);
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        open.retain(|(d, _, _)| *d < depth || (*d == depth && !t.starts_with("end")));
        if t == "end" || t.starts_with("end ") {
            open.retain(|(d, _, _)| *d < depth);
            continue;
        }
        let (prefix, alias) = open
            .last()
            .map(|(_, p, a)| (p.clone(), a.clone()))
            .unwrap_or_default();
        let word: String = t
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        let args = t[word.len()..].trim_start();
        // The quoted path, then the module, then the action.
        let parts: Vec<&str> = args
            .trim_end_matches(" do")
            .split(',')
            .map(str::trim)
            .collect();
        let path = parts
            .first()
            .and_then(|p| first_quoted(p))
            .unwrap_or_default();
        let module = parts.get(1).copied().unwrap_or("").trim();
        let full = |m: &str| match alias.is_empty() {
            true => m.to_string(),
            false => format!("{alias}.{m}"),
        };
        let action = |k: usize| {
            parts.get(k).and_then(|a| a.strip_prefix(':')).map(|a| {
                a.chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect::<String>()
            })
        };
        let named = |name: String, m: &str| Handler::Named {
            name,
            class: Some(full(m)),
        };
        match word.as_str() {
            "scope" => {
                let scope_alias = match module.is_empty() || module.contains(':') {
                    true => alias.clone(),
                    false => full(module),
                };
                open.push((depth, join(&prefix, &path), scope_alias));
            }
            "get" | "post" | "put" | "patch" | "delete" | "options" | "head" => {
                let Some(action) = action(2) else {
                    continue;
                };
                routes.add(
                    "Phoenix",
                    &word.to_ascii_uppercase(),
                    &join(&prefix, &path),
                    &source.rel,
                    i + 1,
                    named(action, module),
                );
            }
            "live" => {
                routes.add(
                    "Phoenix",
                    "PAGE",
                    &join(&prefix, &path),
                    &source.rel,
                    i + 1,
                    named("mount".into(), module),
                );
            }
            "forward" => {
                routes.add(
                    "Phoenix",
                    "ANY",
                    &join(&prefix, &path),
                    &source.rel,
                    i + 1,
                    Handler::Named {
                        name: "call".into(),
                        class: Some(full(module)),
                    },
                );
            }
            "resources" => {
                let only = list_option(args, "only");
                let except = list_option(args, "except");
                let base = join(&prefix, &path);
                for (action, method, sub) in [
                    ("index", "GET", ""),
                    ("new", "GET", "/new"),
                    ("create", "POST", ""),
                    ("show", "GET", "/:id"),
                    ("edit", "GET", "/:id/edit"),
                    ("update", "PATCH", "/:id"),
                    ("update", "PUT", "/:id"),
                    ("delete", "DELETE", "/:id"),
                ] {
                    let kept = only.as_ref().is_none_or(|o| o.iter().any(|a| a == action))
                        && !except
                            .as_ref()
                            .is_some_and(|e| e.iter().any(|a| a == action));
                    if kept {
                        routes.add(
                            "Phoenix",
                            method,
                            &format!("{base}{sub}"),
                            &source.rel,
                            i + 1,
                            named(action.into(), module),
                        );
                    }
                }
            }
            _ => {}
        }
    }
}

fn java(routes: &mut Routes, source: &Source) {
    let Some(tree) = source.tree() else {
        return;
    };
    let src = source.text.as_str();
    for class in all_of(tree.root_node(), "class_declaration") {
        let class_annotations: Vec<_> = annotations(class)
            .into_iter()
            .filter_map(|a| java_annotation(a, src))
            .collect();
        if !class_annotations
            .iter()
            .any(|(n, _, _)| n.ends_with("Controller") || n == "RequestMapping")
        {
            continue;
        }
        let prefix = class_annotations
            .iter()
            .find(|(n, _, _)| n == "RequestMapping")
            .and_then(|(_, p, _)| p.first().cloned())
            .unwrap_or_default();
        let class_name = class
            .child_by_field_name("name")
            .map(|n| text(n, src).to_string())
            .unwrap_or_default();
        let Some(body) = class.child_by_field_name("body") else {
            continue;
        };
        let mut cursor = body.walk();
        for method in body
            .named_children(&mut cursor)
            .filter(|n| n.kind() == "method_declaration")
        {
            let Some(name) = method.child_by_field_name("name") else {
                continue;
            };
            for (annotation, paths, methods) in annotations(method)
                .into_iter()
                .filter_map(|a| java_annotation(a, src))
            {
                let verbs: Vec<String> = match annotation.as_str() {
                    "GetMapping" => vec!["GET".into()],
                    "PostMapping" => vec!["POST".into()],
                    "PutMapping" => vec!["PUT".into()],
                    "PatchMapping" => vec!["PATCH".into()],
                    "DeleteMapping" => vec!["DELETE".into()],
                    "RequestMapping" if methods.is_empty() => vec!["ANY".into()],
                    "RequestMapping" => methods,
                    _ => continue,
                };
                let paths = if paths.is_empty() {
                    vec![String::new()]
                } else {
                    paths
                };
                for path in &paths {
                    for verb in &verbs {
                        routes.add(
                            "Spring",
                            verb,
                            &join(&prefix, path),
                            &source.rel,
                            line(method),
                            Handler::At {
                                rel: source.rel.clone(),
                                line: line(method),
                                label: format!("{class_name}.{}", text(name, src)),
                            },
                        );
                    }
                }
            }
        }
    }
}

// -- C# --------------------------------------------------------------------------------

/// A C# declaration's attributes: each name and its first string argument.
fn cs_attributes(declaration: Node<'_>, src: &str) -> Vec<(String, Option<String>)> {
    let mut out = Vec::new();
    let mut cursor = declaration.walk();
    for list in declaration
        .children(&mut cursor)
        .filter(|c| c.kind() == "attribute_list")
    {
        for attribute in all_of(list, "attribute") {
            let Some(name) = attribute.child_by_field_name("name") else {
                continue;
            };
            let arg = all_of(attribute, "string_literal")
                .first()
                .and_then(|s| string(*s, src));
            let name = text(name, src).trim_end_matches("Attribute").to_string();
            out.push((name, arg));
        }
    }
    out
}

fn csharp(routes: &mut Routes, source: &Source) {
    let Some(tree) = source.tree() else {
        return;
    };
    let src = source.text.as_str();
    let root = tree.root_node();
    for class in all_of(root, "class_declaration") {
        let class_name = class
            .child_by_field_name("name")
            .map(|n| text(n, src).to_string())
            .unwrap_or_default();
        let attributes = cs_attributes(class, src);
        let controller = class_name.ends_with("Controller")
            || attributes.iter().any(|(n, _)| n == "ApiController");
        if !controller {
            continue;
        }
        let token = class_name.trim_end_matches("Controller").to_string();
        let prefix = attributes
            .iter()
            .find(|(n, _)| n == "Route")
            .and_then(|(_, p)| p.clone())
            .unwrap_or_default()
            .replace("[controller]", &token);
        for method in all_of(class, "method_declaration") {
            let Some(name) = method.child_by_field_name("name") else {
                continue;
            };
            let method_name = text(name, src);
            let attributes = cs_attributes(method, src);
            let mut own_route = attributes
                .iter()
                .find(|(n, _)| n == "Route")
                .and_then(|(_, p)| p.clone());
            for (attribute, path) in &attributes {
                let verb = match attribute.as_str() {
                    "HttpGet" => "GET",
                    "HttpPost" => "POST",
                    "HttpPut" => "PUT",
                    "HttpPatch" => "PATCH",
                    "HttpDelete" => "DELETE",
                    "HttpHead" => "HEAD",
                    "HttpOptions" => "OPTIONS",
                    _ => continue,
                };
                let sub = path
                    .clone()
                    .or_else(|| own_route.take())
                    .unwrap_or_default();
                let full = match sub.strip_prefix("~/").or_else(|| sub.strip_prefix('/')) {
                    Some(absolute) => absolute.to_string(),
                    None => join(&prefix, &sub),
                }
                .replace("[controller]", &token)
                .replace("[action]", method_name);
                routes.add(
                    "ASP.NET Core",
                    verb,
                    &full,
                    &source.rel,
                    line(method),
                    Handler::At {
                        rel: source.rel.clone(),
                        line: line(method),
                        label: format!("{class_name}.{method_name}"),
                    },
                );
            }
        }
    }
    // Minimal APIs: `app.MapGet("/x", handler)`.
    for call in all_of(root, "invocation_expression") {
        let Some(function) = call
            .child_by_field_name("function")
            .filter(|f| f.kind() == "member_access_expression")
        else {
            continue;
        };
        let Some(name) = function.child_by_field_name("name") else {
            continue;
        };
        let verb = match text(name, src) {
            "MapGet" => "GET",
            "MapPost" => "POST",
            "MapPut" => "PUT",
            "MapPatch" => "PATCH",
            "MapDelete" => "DELETE",
            _ => continue,
        };
        let all = args(call);
        let Some(path) = all
            .first()
            .and_then(|a| a.named_child(0))
            .and_then(|s| string(s, src))
        else {
            continue;
        };
        let handler = match all.get(1).and_then(|a| a.named_child(0)) {
            Some(h) if matches!(h.kind(), "identifier" | "member_access_expression") => {
                Handler::Named {
                    name: last_name(text(h, src)),
                    class: None,
                }
            }
            _ => Handler::Inline,
        };
        routes.add(
            "ASP.NET Core",
            verb,
            &path,
            &source.rel,
            line(call),
            handler,
        );
    }
}

// -- PHP --------------------------------------------------------------------------------

const RESOURCE_ACTIONS: &[(&str, &str, &str)] = &[
    ("GET", "", "index"),
    ("GET", "/create", "create"),
    ("POST", "", "store"),
    ("GET", "/{id}", "show"),
    ("GET", "/{id}/edit", "edit"),
    ("PUT", "/{id}", "update"),
    ("DELETE", "/{id}", "destroy"),
];

fn php(routes: &mut Routes, source: &Source) {
    let Some(tree) = source.tree() else {
        return;
    };
    let src = source.text.as_str();
    let root = tree.root_node();
    // Laravel: `Route::get('/x', [Controller::class, 'm'])`.
    for call in all_of(root, "scoped_call_expression") {
        let (Some(scope), Some(name)) = (
            call.child_by_field_name("scope"),
            call.child_by_field_name("name"),
        ) else {
            continue;
        };
        if text(scope, src) != "Route" {
            continue;
        }
        let verb = text(name, src);
        let all: Vec<Node> = args(call)
            .into_iter()
            .map(|a| {
                a.named_child(a.named_child_count().saturating_sub(1) as u32)
                    .unwrap_or(a)
            })
            .collect();
        let Some(path) = all.first().and_then(|a| string(*a, src)) else {
            continue;
        };
        let handler = |node: Option<&Node>| -> (Option<String>, Option<String>) {
            match node {
                Some(n) if n.kind() == "array_creation_expression" => {
                    let parts: Vec<Node> = all_of(*n, "array_element_initializer");
                    let class = parts
                        .first()
                        .map(|p| text(*p, src).trim_end_matches("::class").to_string());
                    let method = parts
                        .get(1)
                        .and_then(|p| p.named_child(0))
                        .and_then(|s| string(s, src));
                    (class.map(|c| last_name(&c)), method)
                }
                Some(n) if n.kind() == "class_constant_access_expression" => (
                    Some(last_name(text(*n, src).trim_end_matches("::class"))),
                    Some("__invoke".into()),
                ),
                Some(n) => match string(*n, src) {
                    Some(s) if s.contains('@') => {
                        let (c, m) = s.split_once('@').unwrap_or_default();
                        (Some(last_name(c)), Some(m.to_string()))
                    }
                    _ => (None, None),
                },
                None => (None, None),
            }
        };
        match verb {
            "resource" | "apiResource" => {
                let (class, _) = handler(all.get(1));
                let Some(class) = class else {
                    continue;
                };
                for (method, suffix, action) in RESOURCE_ACTIONS {
                    if verb == "apiResource" && matches!(*action, "create" | "edit") {
                        continue;
                    }
                    routes.add(
                        "Laravel",
                        method,
                        &format!("{path}{suffix}"),
                        &source.rel,
                        line(call),
                        Handler::Named {
                            name: action.to_string(),
                            class: Some(class.clone()),
                        },
                    );
                }
            }
            v if VERBS.contains(&v) || v == "any" => {
                let method = if v == "any" { "ANY" } else { v };
                let target = match handler(all.get(1)) {
                    (Some(class), Some(name)) => Handler::Named {
                        name,
                        class: Some(class),
                    },
                    _ => Handler::Inline,
                };
                routes.add("Laravel", method, &path, &source.rel, line(call), target);
            }
            _ => {}
        }
    }
    // Symfony: `#[Route('/x', methods: ['GET'])]` on a controller's methods.
    for class in all_of(root, "class_declaration") {
        let class_name = class
            .child_by_field_name("name")
            .map(|n| text(n, src).to_string())
            .unwrap_or_default();
        let prefix = php_routes(class, src)
            .into_iter()
            .next()
            .map(|(p, _)| p)
            .unwrap_or_default();
        for method in all_of(class, "method_declaration") {
            let Some(name) = method.child_by_field_name("name") else {
                continue;
            };
            for (path, methods) in php_routes(method, src) {
                let methods = if methods.is_empty() {
                    vec!["ANY".to_string()]
                } else {
                    methods
                };
                for verb in methods {
                    routes.add(
                        "Symfony",
                        &verb,
                        &join(&prefix, &path),
                        &source.rel,
                        line(method),
                        Handler::At {
                            rel: source.rel.clone(),
                            line: line(method),
                            label: format!("{class_name}.{}", text(name, src)),
                        },
                    );
                }
            }
        }
    }
}

/// A PHP declaration's own `#[Route(…)]` attributes: each path and its
/// `methods`.
fn php_routes(declaration: Node<'_>, src: &str) -> Vec<(String, Vec<String>)> {
    let mut out = Vec::new();
    let Some(attributes) = declaration.child_by_field_name("attributes") else {
        return out;
    };
    for attribute in all_of(attributes, "attribute") {
        let name = attribute.named_child(0).map(|n| text(n, src)).unwrap_or("");
        if last_name(name) != "Route" {
            continue;
        }
        let Some(params) = attribute.child_by_field_name("parameters") else {
            continue;
        };
        let mut path = None;
        let mut methods = Vec::new();
        let mut cursor = params.walk();
        for arg in params.named_children(&mut cursor) {
            let key = arg.child_by_field_name("name").map(|n| text(n, src));
            match key {
                None | Some("path") if path.is_none() => {
                    path = all_of(arg, "string").first().and_then(|s| string(*s, src));
                }
                Some("methods") => {
                    methods.extend(
                        all_of(arg, "string")
                            .into_iter()
                            .filter_map(|s| string(s, src))
                            .map(|m| m.to_ascii_uppercase()),
                    );
                }
                _ => {}
            }
        }
        if let Some(path) = path {
            out.push((path, methods));
        }
    }
    out
}

// -- Ruby: Rails ----------------------------------------------------------------------------

/// A block of `routes.rb`: the path and controller module it adds, and the
/// resource whose `:id` nested routes sit under.
#[derive(Debug, Clone, Default)]
struct RailsScope {
    path: String,
    module: String,
}

fn rails(routes: &mut Routes, source: &Source) {
    let mut stack: Vec<RailsScope> = vec![RailsScope::default()];
    for (n, raw) in source.text.lines().enumerate() {
        let line = raw.trim();
        let lineno = n + 1;
        if line == "end" {
            if stack.len() > 1 {
                stack.pop();
            }
            continue;
        }
        let opens = line.ends_with(" do") || line.contains(" do |");
        let current = stack.last().cloned().unwrap_or_default();
        let word = super::ident(line);
        let rest = line[word.len()..].trim().trim_end_matches(" do").trim();
        let first = rails_value(rest);
        match word {
            "namespace" | "scope" => {
                let name = first.unwrap_or_default();
                let module = match word {
                    "namespace" => format!("{}{name}/", current.module),
                    _ => match option(rest, "module") {
                        Some(m) => format!("{}{m}/", current.module),
                        None => current.module.clone(),
                    },
                };
                if opens {
                    stack.push(RailsScope {
                        path: join(&current.path, &name),
                        module,
                    });
                }
            }
            "resources" | "resource" => {
                let Some(name) = first else {
                    if opens {
                        stack.push(current);
                    }
                    continue;
                };
                let plural = word == "resources";
                let controller = option(rest, "controller").unwrap_or_else(|| match plural {
                    true => name.clone(),
                    false => super::sql::pluralize(&name),
                });
                let base = join(&current.path, &name);
                let only = list_option(rest, "only");
                let except = list_option(rest, "except");
                for (method, suffix, action) in rails_actions(plural) {
                    if only
                        .as_ref()
                        .is_some_and(|o| !o.iter().any(|a| a == action))
                        || except.iter().flatten().any(|a| a == action)
                    {
                        continue;
                    }
                    rails_route(
                        routes,
                        source,
                        lineno,
                        method,
                        &format!("{base}{suffix}"),
                        &current.module,
                        &controller,
                        action,
                    );
                }
                if opens {
                    let singular = name.strip_suffix('s').unwrap_or(&name);
                    stack.push(RailsScope {
                        path: match plural {
                            true => format!("{base}/:{singular}_id"),
                            false => base,
                        },
                        module: current.module.clone(),
                    });
                }
            }
            "get" | "post" | "put" | "patch" | "delete" | "match" | "root" => {
                let target = option(rest, "to").or_else(|| {
                    rest.split_once("=>")
                        .and_then(|(_, t)| rails_value(t.trim()))
                });
                let (path, target) = match word {
                    "root" => ("/".to_string(), target.or(first)),
                    _ => match first {
                        Some(path) => (path, target),
                        None => continue,
                    },
                };
                let method = match word {
                    "match" => "ANY".to_string(),
                    "root" => "GET".to_string(),
                    w => w.to_ascii_uppercase(),
                };
                let full = join(&current.path, &path);
                match target.as_deref().and_then(|t| t.split_once('#')) {
                    Some((controller, action)) => rails_route(
                        routes,
                        source,
                        lineno,
                        &method,
                        &full,
                        &current.module,
                        controller,
                        action,
                    ),
                    None => routes.add(
                        "Rails",
                        &method,
                        &full,
                        &source.rel,
                        lineno,
                        Handler::Inline,
                    ),
                }
                if opens {
                    stack.push(current);
                }
            }
            _ => {
                if opens {
                    stack.push(current);
                }
            }
        }
    }
}

/// Play's routes file (`conf/routes`, or a sub-router's `conf/api.routes`):
/// each `GET /items/:id controllers.Items.show(id: Long)` line a route to
/// its controller's action; a sub-router's routes under the path that
/// includes it (`-> /api api.Routes`).
fn play(routes: &mut Routes, source: &Source) {
    // The path another routes file includes this one at.
    let stem = source.name().strip_suffix(".routes").map(|s| s.to_string());
    let prefix = stem
        .as_deref()
        .and_then(|stem| {
            routes
                .corpus
                .files
                .iter()
                .filter(|s| super::is_play_routes(&s.rel))
                .find_map(|s| {
                    s.text.lines().find_map(|l| {
                        let words: Vec<&str> = l.split_whitespace().collect();
                        match words.as_slice() {
                            ["->", path, router, ..]
                                if router.trim_end_matches(".Routes") == stem =>
                            {
                                Some(path.to_string())
                            }
                            _ => None,
                        }
                    })
                })
        })
        .unwrap_or_default();
    for (n, raw) in source.text.lines().enumerate() {
        let words: Vec<&str> = raw.split_whitespace().collect();
        let [method, path, action @ ..] = words.as_slice() else {
            continue;
        };
        let method = method.to_ascii_uppercase();
        if !matches!(
            method.as_str(),
            "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD" | "OPTIONS" | "*"
        ) || action.is_empty()
        {
            continue;
        }
        // `controllers.Items.show(id: Long)`: the class, and the action.
        let call = action.join(" ");
        let call = call.split('(').next().unwrap_or(&call).trim();
        let mut parts = call.rsplit('.');
        let name = parts.next().unwrap_or(call).to_string();
        let class = parts.next().map(str::to_string);
        let method = if method == "*" {
            "ANY"
        } else {
            method.as_str()
        };
        routes.add(
            "Play",
            method,
            &join(&prefix, path),
            &source.rel,
            n + 1,
            Handler::Named { name, class },
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn rails_route(
    routes: &mut Routes,
    source: &Source,
    lineno: usize,
    method: &str,
    path: &str,
    module: &str,
    controller: &str,
    action: &str,
) {
    let full = format!("{module}{controller}");
    let class = full
        .rsplit('/')
        .next()
        .unwrap_or(&full)
        .split('_')
        .map(|w| {
            let mut c = w.chars();
            c.next()
                .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                .unwrap_or_default()
        })
        .collect::<String>()
        + "Controller";
    routes.add(
        "Rails",
        method,
        path,
        &source.rel,
        lineno,
        Handler::Named {
            name: action.to_string(),
            class: Some(class),
        },
    );
}

fn rails_actions(plural: bool) -> Vec<(&'static str, &'static str, &'static str)> {
    match plural {
        true => vec![
            ("GET", "", "index"),
            ("GET", "/new", "new"),
            ("POST", "", "create"),
            ("GET", "/:id", "show"),
            ("GET", "/:id/edit", "edit"),
            ("PATCH", "/:id", "update"),
            ("DELETE", "/:id", "destroy"),
        ],
        false => vec![
            ("GET", "/new", "new"),
            ("POST", "", "create"),
            ("GET", "", "show"),
            ("GET", "/edit", "edit"),
            ("PATCH", "", "update"),
            ("DELETE", "", "destroy"),
        ],
    }
}

/// A Ruby symbol or string at the front of `s` (`:photos` / `'photos/:id'`).
fn rails_value(s: &str) -> Option<String> {
    let s = s.trim().trim_start_matches('(');
    if let Some(sym) = s.strip_prefix(':') {
        let v = super::ident(sym);
        return (!v.is_empty()).then(|| v.to_string());
    }
    let quote = s.chars().next().filter(|c| *c == '\'' || *c == '"')?;
    let inner = &s[1..];
    inner.find(quote).map(|end| inner[..end].to_string())
}

/// A `key: value` option's symbol or string value.
fn option(s: &str, key: &str) -> Option<String> {
    let at = s
        .find(&format!("{key}:"))
        .or_else(|| s.find(&format!(":{key} =>")))?;
    let rest = &s[at..];
    let rest = rest
        .split_once(':')
        .map(|(_, r)| r)?
        .trim_start_matches(' ')
        .trim_start_matches("=>")
        .trim();
    let rest = rest
        .strip_prefix(':')
        .map_or(rest.to_string(), |r| format!(":{r}"));
    rails_value(&rest)
}

/// A `key: [:a, :b]` option's values.
fn list_option(s: &str, key: &str) -> Option<Vec<String>> {
    let at = s.find(&format!("{key}:"))?;
    let rest = &s[at + key.len() + 1..];
    let inner = rest.trim().strip_prefix('[')?.split(']').next()?;
    Some(inner.split(',').filter_map(rails_value).collect())
}

// -- Go -------------------------------------------------------------------------------------------

fn go(routes: &mut Routes, source: &Source) {
    let Some(tree) = source.tree() else {
        return;
    };
    let src = source.text.as_str();
    for call in all_of(tree.root_node(), "call_expression") {
        let Some(function) = call
            .child_by_field_name("function")
            .filter(|f| f.kind() == "selector_expression")
        else {
            continue;
        };
        let Some(field) = function.child_by_field_name("field") else {
            continue;
        };
        let name = text(field, src);
        let method = match name {
            "HandleFunc" | "Handle" | "Any" | "HandleFunc_" => "ANY",
            "GET" | "Get" => "GET",
            "POST" | "Post" => "POST",
            "PUT" | "Put" => "PUT",
            "PATCH" | "Patch" => "PATCH",
            "DELETE" | "Delete" => "DELETE",
            _ => continue,
        };
        let all = args(call);
        let Some(pattern) = all.first().and_then(|a| string(*a, src)) else {
            continue;
        };
        // Go 1.22: `"GET /x"`.
        let (method, path) = match pattern.split_once(' ') {
            Some((m, p)) if m.chars().all(|c| c.is_ascii_uppercase()) && p.starts_with('/') => {
                (m.to_string(), p.to_string())
            }
            _ => (method.to_string(), pattern),
        };
        if !path.starts_with('/') || all.len() < 2 {
            continue;
        }
        let handler = match all.last() {
            Some(h) if matches!(h.kind(), "identifier" | "selector_expression") => Handler::Named {
                name: last_name(text(*h, src)),
                class: None,
            },
            Some(h) if h.kind() == "call_expression" => Handler::Named {
                name: last_name(
                    h.child_by_field_name("function")
                        .map(|f| text(f, src))
                        .unwrap_or(""),
                ),
                class: None,
            },
            _ => Handler::Inline,
        };
        routes.add("Go", &method, &path, &source.rel, line(call), handler);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(route, handler label, handler place)` of every arrow.
    fn routes_of(files: &[(&str, &str)]) -> Vec<(String, String, String)> {
        let maps = maps(&Corpus::of(files));
        let Some(map) = maps.first() else {
            return Vec::new();
        };
        assert!(map.is_well_formed());
        let mut out: Vec<(String, String, String)> = map
            .edges
            .iter()
            .map(|e| {
                let h = &map.nodes[e.to];
                (
                    map.nodes[e.from].label.clone(),
                    h.label.clone(),
                    format!("{}:{}", h.file, h.line),
                )
            })
            .collect();
        out.sort();
        out
    }

    fn route_labels(files: &[(&str, &str)]) -> Vec<String> {
        let maps = maps(&Corpus::of(files));
        let mut out: Vec<String> = maps
            .first()
            .map(|m| {
                m.nodes
                    .iter()
                    .filter(|n| n.kind == "route")
                    .map(|n| n.label.clone())
                    .collect()
            })
            .unwrap_or_default();
        out.sort();
        out
    }

    fn r(route: &str, handler: &str, at: &str) -> (String, String, String) {
        (route.into(), handler.into(), at.into())
    }

    #[test]
    fn flask_fastapi_and_django_routes() {
        let got = routes_of(&[
            (
                "app/api.py",
                "from fastapi import APIRouter\nrouter = APIRouter(prefix=\"/users\")\n\n@router.get(\"/{id}\")\ndef read(id: int):\n    pass\n\n@app.route('/login', methods=['GET', 'POST'])\ndef login():\n    pass\n\n@app.get('/health')\nasync def health():\n    pass\n\n@pytest.fixture\ndef not_a_route():\n    pass\n",
            ),
            (
                "shop/urls.py",
                "from django.urls import path, include\nfrom . import views\nurlpatterns = [\n    path('items/<int:pk>/', views.item_detail, name='item'),\n    path('cart/', views.CartView.as_view()),\n    path('api/', include('shop.api.urls')),\n]\n",
            ),
            (
                "shop/views.py",
                "def item_detail(request, pk):\n    pass\n\nclass CartView(View):\n    pass\n",
            ),
            ("shop/api/urls.py", "urlpatterns = []\n"),
        ]);
        assert_eq!(
            got,
            [
                r("ANY /api", "shop.api.urls", "shop/api/urls.py:1"),
                r("ANY /cart", "CartView", "shop/views.py:4"),
                r("ANY /items/<int:pk>", "item_detail", "shop/views.py:1"),
                r("GET /health", "health", "app/api.py:13"),
                r("GET /login", "login", "app/api.py:9"),
                r("GET /users/{id}", "read", "app/api.py:5"),
                r("POST /login", "login", "app/api.py:9"),
            ]
        );
    }

    /// A router or blueprint mounted in another file is served where it is
    /// mounted: FastAPI's `include_router` prefix goes before the router's
    /// own, nested mounts add up, and Flask's `url_prefix` at registration
    /// replaces the blueprint's own. `add_url_rule` and `add_api_route`
    /// routes name their handler.
    #[test]
    fn python_routes_are_served_where_their_router_is_mounted() {
        let got = routes_of(&[
            (
                "app/main.py",
                "from fastapi import FastAPI\nfrom app.api import api_router\napp = FastAPI()\napp.include_router(api_router, prefix=\"/api\")\n",
            ),
            (
                "app/api/__init__.py",
                "from fastapi import APIRouter\nfrom app.api import users, items as it\napi_router = APIRouter()\napi_router.include_router(users.router, prefix=\"/v1\")\napi_router.include_router(it.router)\n",
            ),
            (
                "app/api/users.py",
                "from fastapi import APIRouter\nrouter = APIRouter(prefix=\"/users\")\n\n@router.get(\"/{id}\")\ndef read(id: int):\n    pass\n",
            ),
            (
                "app/api/items.py",
                "from fastapi import APIRouter\nrouter = APIRouter(prefix=\"/items\")\n\n@router.post(\"/\")\ndef create():\n    pass\n\ndef listing():\n    pass\n\nrouter.add_api_route(\"/all\", listing, methods=[\"GET\"])\n",
            ),
            (
                "shop/__init__.py",
                "from flask import Flask\nfrom shop.auth import bp as auth_bp\napp = Flask(__name__)\napp.register_blueprint(auth_bp, url_prefix=\"/account\")\napp.add_url_rule(\"/\", \"index\", views.index)\napp.add_url_rule(\"/teas/<int:id>\", view_func=TeaView.as_view(\"tea\"), methods=[\"GET\", \"PUT\"])\n",
            ),
            (
                "shop/auth.py",
                "from flask import Blueprint\nbp = Blueprint(\"auth\", __name__, url_prefix=\"/auth\")\n\n@bp.route(\"/login\")\ndef login():\n    pass\n",
            ),
            (
                "shop/views.py",
                "def index():\n    pass\n\nclass TeaView(MethodView):\n    pass\n",
            ),
        ]);
        for want in [
            r("GET /api/v1/users/{id}", "read", "app/api/users.py:5"),
            r("POST /api/items", "create", "app/api/items.py:5"),
            r("GET /api/items/all", "listing", "app/api/items.py:8"),
            r("GET /account/login", "login", "shop/auth.py:5"),
            r("GET /", "index", "shop/views.py:1"),
            r("GET /teas/<int:id>", "TeaView", "shop/views.py:4"),
            r("PUT /teas/<int:id>", "TeaView", "shop/views.py:4"),
        ] {
            assert!(got.contains(&want), "{want:?} in {got:?}");
        }
        // Not also under the router's own prefix alone.
        assert!(
            !got.iter().any(|(route, _, _)| route == "GET /users/{id}"),
            "{got:?}"
        );
        assert!(
            !got.iter().any(|(route, _, _)| route == "GET /auth/login"),
            "{got:?}"
        );
    }

    /// `@app.get` is Flask's and FastAPI's both: the file's import says
    /// whose route it is, and only a file importing neither is said to be
    /// either's.
    #[test]
    fn a_python_route_names_the_framework_its_file_imports() {
        let framework = |src: &str| {
            let maps = maps(&Corpus::of(&[("api.py", src)]));
            let map = maps.first().expect("a routes graph");
            map.nodes
                .iter()
                .find(|n| n.label == "GET /teas")
                .expect("the route")
                .detail
                .clone()
        };
        let route = "\n@app.get('/teas')\ndef teas():\n    pass\n";
        assert_eq!(
            framework(&format!("from flask import Flask{route}")),
            "Flask"
        );
        assert_eq!(framework(&format!("import flask{route}")), "Flask");
        assert_eq!(
            framework(&format!("from flask.views import View{route}")),
            "Flask"
        );
        assert_eq!(
            framework(&format!("from fastapi import FastAPI{route}")),
            "FastAPI"
        );
        assert_eq!(framework(&format!("import fastapi, os{route}")), "FastAPI");
        assert_eq!(
            framework(&format!("import flask_login{route}")),
            "FastAPI / Flask"
        );
        assert_eq!(framework(route), "FastAPI / Flask");
    }

    /// A route node's framework (its detail).
    fn frameworks_of(files: &[(&str, &str)]) -> Vec<(String, String)> {
        let maps = maps(&Corpus::of(files));
        let mut out: Vec<(String, String)> = maps
            .first()
            .map(|m| {
                m.nodes
                    .iter()
                    .filter(|n| n.kind == "route")
                    .map(|n| (n.label.clone(), n.detail.clone()))
                    .collect()
            })
            .unwrap_or_default();
        out.sort();
        out
    }

    /// Each server framework by its name; an app made in place and
    /// chained; a router mounted under a prefix, in its own file or
    /// imported from another, and a file mounted in a mounted file under
    /// both prefixes.
    #[test]
    fn js_server_frameworks_and_mounts() {
        let got = frameworks_of(&[
            (
                "src/index.ts",
                "import { Hono } from 'hono'\nimport books from './books'\nconst app = new Hono()\napp.get('/', (c) => c.text('hi'))\napp.route('/books', books)\nexport default app\n",
            ),
            (
                "src/books.ts",
                "import { Hono } from 'hono'\nconst app = new Hono().get('/', list).post('/:id', create)\nexport default app\n",
            ),
            (
                "api/server.js",
                "const express = require('express')\nconst users = require('./users')\nconst app = express()\nconst admin = express.Router()\nadmin.get('/stats', stats)\napp.use('/admin', admin)\napp.use('/api', users)\n",
            ),
            (
                "api/users.js",
                "const router = require('express').Router()\nrouter.get('/:id', show)\nmodule.exports = router\n",
            ),
            (
                "f/app.ts",
                "import Fastify from 'fastify'\nconst app = Fastify()\napp.get('/ping', async () => 'pong')\n",
            ),
            (
                "e/app.ts",
                "import { Elysia } from 'elysia'\nnew Elysia().get('/e', () => 'x').listen(3000)\n",
            ),
        ]);
        for (label, framework) in [
            ("GET /", "Hono"),
            ("GET /books", "Hono"),
            ("POST /books/:id", "Hono"),
            ("GET /admin/stats", "Express"),
            ("GET /api/:id", "Express"),
            ("GET /ping", "Fastify"),
            ("GET /e", "Elysia"),
        ] {
            assert!(
                got.contains(&(label.to_string(), framework.to_string())),
                "{label} {framework} in {got:?}"
            );
        }
    }

    /// Bun's routes table, Hapi's route objects and tRPC's procedures, by
    /// the path a client calls them by.
    #[test]
    fn bun_hapi_and_trpc_routes() {
        let got = frameworks_of(&[
            (
                "server.ts",
                "Bun.serve({ routes: { '/api/status': new Response('OK'), '/users/:id': getUser, '/api/posts': { GET: listPosts, POST: async (req) => create(req) } } })\n",
            ),
            (
                "hapi.js",
                "server.route({ method: 'GET', path: '/h/{id}', handler: show });\n",
            ),
            (
                "packages/api/src/router/post.ts",
                "export const postRouter = {\n  all: publicProcedure.query(({ ctx }) => ctx.db.query.Post.findMany()),\n  create: protectedProcedure.input(z.object({})).mutation(({ input }) => input),\n} satisfies TRPCRouterRecord;\n",
            ),
            (
                "packages/api/src/root.ts",
                "export const appRouter = createTRPCRouter({\n  post: postRouter,\n  health: publicProcedure.query(() => 'ok'),\n});\n",
            ),
        ]);
        for (label, framework) in [
            ("ANY /api/status", "Bun"),
            ("ANY /users/:id", "Bun"),
            ("GET /api/posts", "Bun"),
            ("POST /api/posts", "Bun"),
            ("GET /h/{id}", "Hapi"),
            ("QUERY post.all", "tRPC"),
            ("MUTATION post.create", "tRPC"),
            ("QUERY health", "tRPC"),
        ] {
            assert!(
                got.contains(&(label.to_string(), framework.to_string())),
                "{label} {framework} in {got:?}"
            );
        }
    }

    /// The client's routes: React Router's route objects and `<Route>`
    /// elements, nested; Vue Router's, a lazy page by its file; TanStack
    /// Router's file routes; and the file-routed frameworks' pages.
    #[test]
    fn client_routes_and_file_routed_pages() {
        let got = routes_of(&[
            (
                "src/router.tsx",
                "import { createBrowserRouter } from 'react-router-dom'\nexport const router = createBrowserRouter([\n  { path: '/', element: <Root />, children: [\n    { path: 'posts/:id', element: <Post /> },\n  ] },\n])\n",
            ),
            (
                "src/App.tsx",
                "import { Routes, Route } from 'react-router-dom'\nexport function App() {\n  return <Routes><Route path=\"/shop\" element={<Shop />}><Route path=\"cart\" element={<Cart />} /></Route></Routes>\n}\nfunction Root() {}\nfunction Post() {}\nfunction Shop() {}\nfunction Cart() {}\n",
            ),
            (
                "web/src/router/index.ts",
                "import { createRouter } from 'vue-router'\nexport default createRouter({ routes: [\n  { path: '/', component: Home },\n  { path: '/about', component: () => import('../views/About.vue') },\n] })\n",
            ),
            (
                "web/src/views/About.vue",
                "<template><p>about</p></template>\n",
            ),
        ]);
        for (route, handler) in [
            ("PAGE /", "Root"),
            ("PAGE /posts/:id", "Post"),
            ("PAGE /shop", "Shop"),
            ("PAGE /shop/cart", "Cart"),
            ("PAGE /about", "About.vue"),
        ] {
            assert!(
                got.iter().any(|(r, h, _)| r == route && h == handler),
                "{route} → {handler} in {got:?}"
            );
        }
        let pages = route_labels(&[
            ("nuxt.config.ts", "export default defineNuxtConfig({})\n"),
            ("pages/index.vue", "<template/>\n"),
            ("pages/posts/[id].vue", "<template/>\n"),
            (
                "server/api/users/[id].get.ts",
                "export default defineEventHandler(() => 1)\n",
            ),
            (
                "src/routes/posts.$postId.tsx",
                "export const Route = createFileRoute('/posts/$postId')({ component: Post })\n",
            ),
        ]);
        for page in [
            "PAGE /",
            "PAGE /posts/:id",
            "GET /api/users/:id",
            "PAGE /posts/:postId",
        ] {
            assert!(pages.contains(&page.to_string()), "{page} in {pages:?}");
        }
        let kit = route_labels(&[
            ("svelte.config.js", "export default {}\n"),
            (
                "package.json",
                "{\"devDependencies\": {\"@sveltejs/kit\": \"2\"}}",
            ),
            ("src/routes/+page.svelte", "<h1/>\n"),
            ("src/routes/blog/[slug]/+page.svelte", "<h1/>\n"),
            ("src/routes/profile/@[user]/+page.svelte", "<h1/>\n"),
            ("src/routes/[[lang]]/about/+page.svelte", "<h1/>\n"),
            ("src/routes/items/[id=integer]/+page.svelte", "<h1/>\n"),
            ("src/routes/files/[...path]/+page.svelte", "<h1/>\n"),
            (
                "src/routes/api/(v1)/items/+server.ts",
                "export async function GET() {}\nexport const POST = async () => {}\n",
            ),
            (
                "src/routes/login/+page.server.ts",
                "export const load = async () => ({})\nexport const actions = {\n  default: async () => {},\n  'reset': async () => {},\n  logout() {},\n} satisfies Actions\n",
            ),
            (
                "tests/__e2e__/fixtures/app/src/routes/+page.svelte",
                "<h1/>\n",
            ),
        ]);
        for page in [
            "PAGE /",
            "PAGE /blog/:slug",
            "PAGE /profile/@:user",
            "PAGE /:lang?/about",
            "PAGE /items/:id",
            "PAGE /files/*path",
            "GET /api/items",
            "POST /api/items",
            "POST /login",
            "POST /login?/reset",
            "POST /login?/logout",
        ] {
            assert!(kit.contains(&page.to_string()), "{page} in {kit:?}");
        }
        assert_eq!(
            kit.iter().filter(|r| *r == "PAGE /").count(),
            1,
            "a test fixture's app is none of this one's pages: {kit:?}"
        );
        let remix = route_labels(&[
            (
                "package.json",
                "{\"dependencies\": {\"@remix-run/react\": \"2\"}}",
            ),
            (
                "app/routes/_index.tsx",
                "export default function Index() {}\n",
            ),
            (
                "app/routes/posts.$id.tsx",
                "export default function Post() {}\n",
            ),
            (
                "app/routes/_auth.login.tsx",
                "export default function Login() {}\n",
            ),
            (
                "app/routes/api.health.ts",
                "export const loader = () => 1\n",
            ),
            (
                "app/routes/logout.tsx",
                "export const action = async () => 1\nexport const loader = async () => 2\n",
            ),
            (
                "app/routes/join.tsx",
                "export const loader = async () => 1\nexport const action = async () => 2\nexport default function Join() {}\n",
            ),
            ("app/routes/webhook.ts", "export const config = {}\n"),
            (
                "app/routes/sitemap[.]xml.ts",
                "export const loader = () => 1\n",
            ),
            (
                "app/routes/($lang).about.tsx",
                "export default function About() {}\n",
            ),
            (
                "app/routes/posts_.$id.edit.tsx",
                "export default function Edit() {}\n",
            ),
            (
                "app/routes/files.$.tsx",
                "export default function Files() {}\n",
            ),
        ]);
        for page in [
            "PAGE /",
            "PAGE /posts/:id",
            "PAGE /login",
            "GET /api/health",
            "GET /sitemap.xml",
            "GET /logout",
            "POST /logout",
            "PAGE /join",
            "POST /join",
            "ANY /webhook",
            "PAGE /:lang?/about",
            "PAGE /posts/:id/edit",
            "PAGE /files/*",
        ] {
            assert!(remix.contains(&page.to_string()), "{page} in {remix:?}");
        }
        let astro = route_labels(&[
            ("astro.config.mjs", "export default {}\n"),
            ("src/pages/index.astro", "---\n---\n<h1/>\n"),
            ("src/pages/blog/[slug].md", "# x\n"),
            (
                "src/pages/api/search.json.ts",
                "export const GET = () => new Response('[]')\n",
            ),
        ]);
        for page in ["PAGE /", "PAGE /blog/:slug", "GET /api/search"] {
            assert!(astro.contains(&page.to_string()), "{page} in {astro:?}");
        }
        let integration = route_labels(&[(
            "packages/docs/src/index.ts",
            "export default function docs() {\n  return { hooks: { 'astro:config:setup': ({ injectRoute }) => {\n    injectRoute({ pattern: '404', entrypoint: prerender ? '@x/docs/static/404.astro' : '@x/docs/ssr/404.astro' })\n    injectRoute({ pattern: '/[...slug]', entrypoint: '@x/docs/index.astro' })\n    injectRoute({ pattern: 'feed.xml', entrypoint: './feed.ts' })\n  } } }\n}\n",
        )]);
        assert_eq!(
            integration,
            ["ANY /feed.xml", "PAGE /*slug", "PAGE /404"],
            "{integration:?}"
        );
    }

    /// Kotlin's Spring controllers and Ktor routing; a Phoenix router's
    /// verbs, resources (less `only:`'s), LiveViews and scopes — each
    /// action found in its controller's module.
    /// Play's routes file: each line a route to its controller's action,
    /// a sub-router's under the path that includes it.
    #[test]
    fn play_routes() {
        let files: &[(&str, &str)] = &[
            (
                "conf/routes",
                "# Routes\nGET     /                     controllers.HomeController.index()\nPOST    /items/:id            controllers.ItemController.update(id: Long)\n->      /api                  api.Routes\n",
            ),
            (
                "conf/api.routes",
                "GET /status controllers.api.Status.check\n",
            ),
            (
                "app/controllers/ItemController.scala",
                "package controllers\n\nclass ItemController {\n  def update(id: Long) = Action { Ok }\n}\n",
            ),
        ];
        let labels = route_labels(files);
        for route in ["GET /", "POST /items/:id", "GET /api/status"] {
            assert!(labels.iter().any(|l| l == route), "{route} in {labels:?}");
        }
        let got = routes_of(files);
        assert!(
            got.iter()
                .any(|(r, h, _)| r == "POST /items/:id" && h.ends_with("update")),
            "{got:?}"
        );
    }

    #[test]
    fn kotlin_and_phoenix_routes() {
        let got = routes_of(&[
            (
                "src/main/kotlin/com/acme/UserController.kt",
                "@RestController\n@RequestMapping(\"/api/users\")\nclass UserController(private val repo: Repo) {\n    @GetMapping\n    fun list(): List<User> = repo.all()\n\n    @GetMapping(\"/{id}\")\n    fun show(@PathVariable id: Long) = repo.find(id)\n\n    @PostMapping\n    fun create(@RequestBody u: User) = repo.save(u)\n}\n",
            ),
            (
                "src/main/kotlin/com/acme/Routes.kt",
                "fun Application.module() {\n    routing {\n        route(\"/v2\") {\n            get(\"/health\") {\n                call.respondText(\"ok\")\n            }\n        }\n        post(\"/login\") { }\n    }\n}\n",
            ),
        ]);
        for (route, handler) in [
            ("GET /api/users", "UserController.list"),
            ("GET /api/users/{id}", "UserController.show"),
            ("POST /api/users", "UserController.create"),
        ] {
            assert!(
                got.iter().any(|(r, h, _)| r == route && h == handler),
                "{route} → {handler} in {got:?}"
            );
        }
        let ktor = route_labels(&[(
            "src/main/kotlin/com/acme/Routes.kt",
            "fun Application.module() {\n    routing {\n        route(\"/v2\") {\n            get(\"/health\") {\n                call.respondText(\"ok\")\n            }\n        }\n        post(\"/login\") { }\n    }\n}\n",
        )]);
        assert_eq!(ktor, ["GET /v2/health", "POST /login"], "{ktor:?}");

        let router = "defmodule ShopWeb.Router do\n  use ShopWeb, :router\n\n  scope \"/\", ShopWeb do\n    pipe_through :browser\n\n    get \"/\", PageController, :home\n    live \"/cart\", CartLive, :index\n    resources \"/users\", UserController, only: [:index, :show]\n  end\n\n  scope \"/api\", ShopWeb.Api do\n    post \"/orders\", OrderController, :create\n  end\nend\n";
        let got = routes_of(&[
            ("lib/shop_web/router.ex", router),
            (
                "lib/shop_web/controllers/user_controller.ex",
                "defmodule ShopWeb.UserController do\n  def index(conn, _), do: conn\n  def show(conn, _), do: conn\nend\n",
            ),
            (
                "lib/shop_web/controllers/api/order_controller.ex",
                "defmodule ShopWeb.Api.OrderController do\n  def create(conn, _), do: conn\nend\n",
            ),
        ]);
        for (route, handler) in [
            ("GET /", "ShopWeb.PageController.home"),
            ("PAGE /cart", "ShopWeb.CartLive.mount"),
            ("GET /users", "ShopWeb.UserController.index"),
            ("GET /users/:id", "ShopWeb.UserController.show"),
            ("POST /api/orders", "ShopWeb.Api.OrderController.create"),
        ] {
            assert!(
                got.iter().any(|(r, h, _)| r == route && h == handler),
                "{route} → {handler} in {got:?}"
            );
        }
        assert!(
            !got.iter().any(|(r, _, _)| r == "POST /users"),
            "only: index and show: {got:?}"
        );
        // An action found is placed at its function.
        assert!(
            got.iter()
                .any(|(r, _, at)| r == "GET /users/:id" && at.contains("user_controller.ex")),
            "{got:?}"
        );
    }

    /// A route's path by a constant the project declares
    /// (`paths.app.root.path`), in another file.
    #[test]
    fn route_paths_by_constants() {
        let got = route_labels(&[
            (
                "src/config/paths.ts",
                "export const paths = {\n  home: { path: '/', getHref: () => '/' },\n  app: { root: { path: '/app' }, users: { path: 'users' } },\n} as const;\n",
            ),
            (
                "src/app/router.tsx",
                "import { createBrowserRouter } from 'react-router';\nimport { paths } from '@/config/paths';\nexport const r = () => createBrowserRouter([\n  { path: paths.home.path, lazy: () => import('./routes/landing') },\n  { path: paths.app.root.path, element: <Root />, children: [\n    { path: paths.app.users.path, lazy: () => import('./routes/users') },\n  ] },\n]);\n",
            ),
        ]);
        for page in ["PAGE /", "PAGE /app", "PAGE /app/users"] {
            assert!(got.contains(&page.to_string()), "{page} in {got:?}");
        }
    }

    #[test]
    fn express_nest_and_next_routes() {
        let got = routes_of(&[
            (
                "src/server.js",
                "const api = express.Router();\napi.get('/users/:id', users.show);\napp.post('/login', (req, res) => res.end());\nrouter.route('/items').get(listItems).post(auth, createItem);\naxios.get('/not/a/route', config);\napi.all('*', notFound);\n",
            ),
            (
                "src/users.js",
                "exports.show = function show(req, res) {}\nfunction listItems() {}\nfunction createItem() {}\nfunction notFound() {}\n",
            ),
            (
                "src/cats.controller.ts",
                "@Controller('cats')\nexport class CatsController {\n  @Get(':id')\n  findOne() {}\n\n  @Post()\n  create() {}\n\n  helper() {}\n}\n",
            ),
        ]);
        assert!(
            got.contains(&r("GET /items", "listItems", "src/users.js:2")),
            "{got:?}"
        );
        assert!(
            got.contains(&r("POST /items", "createItem", "src/users.js:3")),
            "{got:?}"
        );
        assert!(
            got.contains(&r(
                "GET /cats/:id",
                "CatsController.findOne",
                "src/cats.controller.ts:4"
            )),
            "{got:?}"
        );
        assert!(
            got.contains(&r(
                "POST /cats",
                "CatsController.create",
                "src/cats.controller.ts:7"
            )),
            "{got:?}"
        );
        assert!(
            got.contains(&r("ANY /*", "notFound", "src/users.js:4")),
            "{got:?}"
        );
        assert!(
            got.contains(&r("GET /users/:id", "show", "src/users.js:1")),
            "{got:?}"
        );
        assert!(
            !got.iter().any(|(r, _, _)| r.contains("not/a/route")),
            "a client call is no route: {got:?}"
        );
        let labels = route_labels(&[
            (
                "src/server.js",
                "app.post('/login', (req, res) => res.end());\naxios.get('/x', c);\n",
            ),
            ("next.config.js", "module.exports = {}\n"),
            ("pages/index.tsx", "export default function Home() {}\n"),
            (
                "pages/posts/[id].tsx",
                "export default function Post() {}\n",
            ),
            (
                "pages/api/hello.ts",
                "export default function handler() {}\n",
            ),
            ("pages/_app.tsx", "export default function App() {}\n"),
            (
                "app/(shop)/cart/page.tsx",
                "export default function Cart() {}\n",
            ),
            (
                "app/api/orders/route.ts",
                "export async function GET() {}\nexport async function POST() {}\n",
            ),
        ]);
        assert_eq!(
            labels,
            [
                "ANY /api/hello",
                "GET /",
                "GET /api/orders",
                "GET /cart",
                "GET /posts/:id",
                "POST /api/orders",
                "POST /login",
            ]
        );
    }

    #[test]
    fn spring_and_aspnet_routes() {
        let got = routes_of(&[
            (
                "src/UserController.java",
                "@RestController\n@RequestMapping(\"/api/users\")\npublic class UserController {\n    @GetMapping(\"/{id}\")\n    public User get() { return null; }\n\n    @RequestMapping(value = \"/search\", method = RequestMethod.POST)\n    public List<User> search() { return null; }\n\n    @DeleteMapping(path = {\"/{id}\", \"/remove/{id}\"})\n    public void delete() {}\n}\n",
            ),
            (
                "Controllers/OrdersController.cs",
                "[ApiController]\n[Route(\"api/[controller]\")]\npublic class OrdersController : ControllerBase {\n    [HttpGet(\"{id}\")]\n    public Order Get(int id) => null;\n\n    [HttpPost]\n    public void Create() {}\n\n    [HttpGet(\"/health\")]\n    public string Health() => \"ok\";\n}\n",
            ),
            (
                "Program.cs",
                "var app = builder.Build();\napp.MapGet(\"/ping\", () => \"pong\");\napp.MapPost(\"/orders\", Handlers.Create);\n",
            ),
            (
                "Handlers.cs",
                "static class Handlers {\n    public static void Create() {}\n}\n",
            ),
        ]);
        for want in [
            r(
                "GET /api/users/{id}",
                "UserController.get",
                "src/UserController.java:4",
            ),
            r(
                "POST /api/users/search",
                "UserController.search",
                "src/UserController.java:7",
            ),
            r(
                "DELETE /api/users/remove/{id}",
                "UserController.delete",
                "src/UserController.java:10",
            ),
            r(
                "DELETE /api/users/{id}",
                "UserController.delete",
                "src/UserController.java:10",
            ),
            r(
                "GET /api/Orders/{id}",
                "OrdersController.Get",
                "Controllers/OrdersController.cs:4",
            ),
            r(
                "POST /api/Orders",
                "OrdersController.Create",
                "Controllers/OrdersController.cs:7",
            ),
            r(
                "GET /health",
                "OrdersController.Health",
                "Controllers/OrdersController.cs:10",
            ),
            r("POST /orders", "Handlers.Create", "Handlers.cs:2"),
        ] {
            assert!(got.contains(&want), "{want:?} in {got:?}");
        }
        let labels = route_labels(&[("Program.cs", "app.MapGet(\"/ping\", () => \"pong\");\n")]);
        assert_eq!(labels, ["GET /ping"], "an inline handler: the route alone");
    }

    #[test]
    fn laravel_and_symfony_routes() {
        let got = routes_of(&[
            (
                "routes/web.php",
                "<?php\nuse App\\Http\\Controllers\\UserController;\nRoute::get('/users', [UserController::class, 'index']);\nRoute::post('/users', 'UserController@store');\nRoute::get('/about', function () { return view('about'); });\nRoute::apiResource('photos', PhotoController::class);\n",
            ),
            (
                "app/Http/Controllers/UserController.php",
                "<?php\nclass UserController {\n    public function index() {}\n    public function store() {}\n}\n",
            ),
            (
                "app/Http/Controllers/PhotoController.php",
                "<?php\nclass PhotoController {\n    public function index() {}\n    public function show($id) {}\n}\n",
            ),
            (
                "src/Controller/BlogController.php",
                "<?php\n#[Route('/blog')]\nclass BlogController {\n    #[Route('/{slug}', name: 'blog_show', methods: ['GET', 'HEAD'])]\n    public function show(string $slug) {}\n}\n",
            ),
        ]);
        for want in [
            r(
                "GET /users",
                "UserController.index",
                "app/Http/Controllers/UserController.php:3",
            ),
            r(
                "POST /users",
                "UserController.store",
                "app/Http/Controllers/UserController.php:4",
            ),
            r(
                "GET /photos",
                "PhotoController.index",
                "app/Http/Controllers/PhotoController.php:3",
            ),
            r(
                "GET /photos/{id}",
                "PhotoController.show",
                "app/Http/Controllers/PhotoController.php:4",
            ),
            r("PUT /photos/{id}", "PhotoController.update", ":0"),
            r(
                "GET /blog/{slug}",
                "BlogController.show",
                "src/Controller/BlogController.php:4",
            ),
            r(
                "HEAD /blog/{slug}",
                "BlogController.show",
                "src/Controller/BlogController.php:4",
            ),
        ] {
            assert!(got.contains(&want), "{want:?} in {got:?}");
        }
        assert!(
            !got.iter().any(|(r, _, _)| r == "GET /photos/create"),
            "an API resource has no form routes: {got:?}"
        );
        assert!(
            route_labels(&[(
                "routes/web.php",
                "<?php\nRoute::get('/about', function () { return 1; });\n"
            )]) == ["GET /about"]
        );
    }

    #[test]
    fn rails_routes() {
        let got = routes_of(&[
            (
                "config/routes.rb",
                "Rails.application.routes.draw do\n  root 'pages#home'\n  get 'about', to: 'pages#about'\n  resources :photos, only: [:index, :show] do\n    resources :comments, only: [:create]\n  end\n  namespace :admin do\n    resources :users, except: [:destroy, :new, :edit, :create, :update]\n  end\n  resource :profile, only: [:show]\n  post '/hooks' => 'hooks#receive'\nend\n",
            ),
            (
                "app/controllers/pages_controller.rb",
                "class PagesController < ApplicationController\n  def home\n  end\n\n  def about\n  end\nend\n",
            ),
            (
                "app/controllers/photos_controller.rb",
                "class PhotosController < ApplicationController\n  def index\n  end\n\n  def show\n  end\nend\n",
            ),
        ]);
        for want in [
            r(
                "GET /",
                "PagesController.home",
                "app/controllers/pages_controller.rb:2",
            ),
            r(
                "GET /about",
                "PagesController.about",
                "app/controllers/pages_controller.rb:5",
            ),
            r(
                "GET /photos",
                "PhotosController.index",
                "app/controllers/photos_controller.rb:2",
            ),
            r(
                "GET /photos/:id",
                "PhotosController.show",
                "app/controllers/photos_controller.rb:5",
            ),
            r(
                "POST /photos/:photo_id/comments",
                "CommentsController.create",
                ":0",
            ),
            r("GET /admin/users", "UsersController.index", ":0"),
            r("GET /admin/users/:id", "UsersController.show", ":0"),
            r("GET /profile", "ProfilesController.show", ":0"),
            r("POST /hooks", "HooksController.receive", ":0"),
        ] {
            assert!(got.contains(&want), "{want:?} in {got:?}");
        }
        assert_eq!(got.len(), 9, "only and except honoured: {got:?}");
    }

    #[test]
    fn go_routes() {
        let got = routes_of(&[
            (
                "cmd/server.go",
                "package main\n\nfunc main() {\n\thttp.HandleFunc(\"/health\", health)\n\tmux.HandleFunc(\"POST /orders/{id}\", h.CreateOrder)\n\tr.GET(\"/users/:id\", getUser)\n\tr.Post(\"/login\", func(w http.ResponseWriter, r *http.Request) {})\n\tresp, _ := http.Get(\"https://example.com/x\")\n}\n\nfunc health(w http.ResponseWriter, r *http.Request) {}\n\nfunc getUser(c *gin.Context) {}\n",
            ),
            (
                "internal/h.go",
                "package internal\n\nfunc (h *Handler) CreateOrder(w http.ResponseWriter, r *http.Request) {}\n",
            ),
        ]);
        assert_eq!(
            got,
            [
                r("ANY /health", "health", "cmd/server.go:11"),
                r("GET /users/:id", "getUser", "cmd/server.go:13"),
                r("POST /orders/{id}", "CreateOrder", "internal/h.go:3"),
            ]
        );
        let labels = route_labels(&[(
            "x.go",
            "package x\nfunc f() { r.Post(\"/login\", func() {}) }\n",
        )]);
        assert_eq!(labels, ["POST /login"]);
        assert_eq!(normalize("api//users/"), "/api/users");
        assert_eq!(normalize(""), "/");
    }
}
