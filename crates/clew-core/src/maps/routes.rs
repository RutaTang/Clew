//! A web app's routes: each route (its method and path) and the handler it
//! reaches, read from how each framework declares them —
//!
//! - Python: Flask and FastAPI decorators (`@app.get("/x")`,
//!   `@bp.route("/x", methods=[…])`, a router's or blueprint's prefix),
//!   Django's `urls.py` (`path`, `re_path`, `url`, `include`);
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
//! router mounted at a prefix in another file (`app.use("/api", router)`, a
//! Laravel `prefix` group) is not followed: its routes show their own paths.

use clew_protocol::ProjectMap;
use tree_sitter::Node;

use super::{Corpus, MapBuilder, Source, all_of, last_name, line, text, unquote};
use crate::highlight::Lang;

pub fn maps(corpus: &Corpus) -> Vec<ProjectMap> {
    let mut routes = Routes {
        map: MapBuilder::new(
            "routes",
            "Routes",
            "each route — its method and path — with an arrow to the handler it reaches; an inline handler is shown as the route itself",
        ),
        corpus,
    };
    let next = corpus.files.iter().any(|s| {
        s.name().starts_with("next.config.")
            || (s.name() == "package.json" && s.text.contains("\"next\""))
    });
    for source in &corpus.files {
        match source.lang {
            Some(Lang::Python) => python(&mut routes, source),
            Some(Lang::JavaScript | Lang::TypeScript | Lang::Tsx) => {
                javascript(&mut routes, source);
                if next {
                    nextjs(&mut routes, source);
                }
            }
            Some(Lang::Java) => java(&mut routes, source),
            Some(Lang::CSharp) => csharp(&mut routes, source),
            Some(Lang::Php) => php(&mut routes, source),
            Some(Lang::Ruby) if source.rel.ends_with("config/routes.rb") => {
                rails(&mut routes, source)
            }
            Some(Lang::Go) => go(&mut routes, source),
            _ => {}
        }
    }
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
    // `router = APIRouter(prefix="/x")`, `bp = Blueprint("b", __name__, url_prefix="/x")`.
    let mut prefixes: Vec<(String, String)> = Vec::new();
    for assignment in all_of(root, "assignment") {
        let (Some(left), Some(right)) = (
            assignment.child_by_field_name("left"),
            assignment.child_by_field_name("right"),
        ) else {
            continue;
        };
        if right.kind() != "call" {
            continue;
        }
        for arg in args(right) {
            if arg.kind() == "keyword_argument"
                && let (Some(name), Some(value)) = (
                    arg.child_by_field_name("name"),
                    arg.child_by_field_name("value"),
                )
                && matches!(text(name, src), "prefix" | "url_prefix")
                && let Some(prefix) = string(value, src)
            {
                prefixes.push((text(left, src).to_string(), prefix));
            }
        }
    }
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
            let object = text(object, src);
            let path = match prefixes.iter().find(|(o, _)| o == object) {
                Some((_, prefix)) => join(prefix, &path),
                None => path,
            };
            let framework = if verb == "route" { "Flask" } else { either };
            for method in methods {
                routes.add(
                    framework,
                    &method,
                    &path,
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
    if source.name() == "urls.py" {
        django(routes, source, root);
    }
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
    let Some(tree) = source.tree() else {
        return;
    };
    let src = source.text.as_str();
    let root = tree.root_node();
    // Variables made servers or routers: `const x = express()`, `Router()`, …
    let mut servers: Vec<String> = SERVER_NAMES.iter().map(|s| s.to_string()).collect();
    for declarator in all_of(root, "variable_declarator") {
        if let (Some(name), Some(value)) = (
            declarator.child_by_field_name("name"),
            declarator.child_by_field_name("value"),
        ) {
            let v = text(value, src);
            if [
                "express(",
                "Router(",
                "fastify(",
                "new Koa",
                "new Router",
                "new Hono",
                "express.Router(",
            ]
            .iter()
            .any(|k| v.starts_with(k) || v.contains(&format!(".{k}")))
            {
                servers.push(text(name, src).to_string());
            }
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
        // `router.route("/x").get(h).post(h2)`: the path is the route's,
        // down the chain.
        let (path, handlers) = if let Some(route) = route_call(object, src) {
            let Some(path) = args(route).first().and_then(|a| string(*a, src)) else {
                continue;
            };
            (path, all.as_slice())
        } else {
            if !servers.iter().any(|s| s == text(object, src)) {
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
        routes.add("Express", method, &path, &source.rel, line(call), handler);
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
    let route_of = |parts: &[&str]| -> String {
        parts
            .iter()
            .filter(|p| !(p.starts_with('@') || p.starts_with('(') && p.ends_with(')')))
            .copied()
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
                "GET /posts/[id]",
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
