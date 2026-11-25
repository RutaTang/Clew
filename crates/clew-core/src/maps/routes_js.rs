//! JavaScript and TypeScript routes beyond Express's own calls (the parent
//! module's `javascript`):
//!
//! - **server frameworks** said by name — Express, Fastify, Koa, Hono,
//!   Elysia — an app made in place and chained (`new Hono().get(…)`), and a
//!   router mounted under a prefix (`app.use("/api", users)`,
//!   `app.route("/books", books)`, in this file or imported from another);
//! - **Bun's** `Bun.serve({ routes })`, **Hapi's** route objects, and
//!   **tRPC's** procedures, by the path a client calls them by
//!   (`post.byId`);
//! - **client routes**: React Router's and Vue Router's route objects and
//!   `<Route>` elements, TanStack Router's file routes, and the pages of
//!   Nuxt, SvelteKit, Remix and Astro, by their files.

use std::collections::HashMap;

use super::*;

/// How each framework's app or router is made, in the order tried.
const MAKERS: &[(&str, &str)] = &[
    ("express.Router(", "Express"),
    ("express(", "Express"),
    ("Router(", "Express"),
    ("fastify(", "Fastify"),
    ("Fastify(", "Fastify"),
    ("new KoaRouter", "Koa"),
    ("new Router", "Koa"),
    ("new Koa", "Koa"),
    ("new Hono", "Hono"),
    ("new OpenAPIHono", "Hono"),
    ("new Elysia", "Elysia"),
    ("polka(", "Polka"),
];

/// The framework a file's server is written with, by what it imports:
/// for an app or router whose making is not in the file (`app` passed in).
pub(super) fn file_framework(src: &str) -> &'static str {
    let imports = |m: &str| {
        src.contains(&format!("from '{m}'"))
            || src.contains(&format!("from \"{m}\""))
            || src.contains(&format!("require('{m}')"))
            || src.contains(&format!("require(\"{m}\")"))
    };
    for (module, name) in [
        ("hono", "Hono"),
        ("fastify", "Fastify"),
        ("koa", "Koa"),
        ("@koa/router", "Koa"),
        ("koa-router", "Koa"),
        ("elysia", "Elysia"),
    ] {
        if imports(module) {
            return name;
        }
    }
    "Express"
}

/// The framework an expression makes an app or router with, if it does.
pub(super) fn made_by(value: &str) -> Option<&'static str> {
    MAKERS
        .iter()
        .find(|(k, _)| {
            value.starts_with(k)
                || value.contains(&format!(".{k}"))
                || value.contains(&format!("= {k}"))
        })
        .map(|(_, f)| *f)
}

/// The expression a chain of method calls hangs from: `new Hono()` of
/// `new Hono().get(…).post(…)`, `app` of `app.get(…)`.
pub(super) fn chain_root<'t>(mut node: Node<'t>) -> Node<'t> {
    while node.kind() == "call_expression" {
        let Some(function) = node.child_by_field_name("function") else {
            break;
        };
        if function.kind() != "member_expression" {
            break;
        }
        let Some(object) = function.child_by_field_name("object") else {
            break;
        };
        node = object;
    }
    node
}

/// Where each file's routes are served under, and each router variable's
/// in its own file: what `app.use("/api", users)`, Hono's
/// `app.route("/books", books)` and Fastify's
/// `register(plugin, { prefix: "/v1" })` mount, followed from the file that
/// mounts a router to the file it imports it from.
#[derive(Default)]
pub(super) struct Mounts {
    /// A file's prefix, for every route in it.
    files: HashMap<String, String>,
    /// A router variable's prefix in its own file: `(rel, name)`.
    vars: HashMap<(String, String), String>,
}

impl Mounts {
    pub(super) fn read(corpus: &Corpus) -> Mounts {
        // (mounting file, prefix, what is mounted: a name in that file).
        let mut found: Vec<(String, String, String)> = Vec::new();
        for source in corpus.in_langs(&[Lang::JavaScript, Lang::TypeScript, Lang::Tsx]) {
            let Some(tree) = source.tree() else {
                continue;
            };
            let src = source.text.as_str();
            for call in all_of(tree.root_node(), "call_expression") {
                let Some(function) = call
                    .child_by_field_name("function")
                    .filter(|f| f.kind() == "member_expression")
                else {
                    continue;
                };
                let Some(property) = function.child_by_field_name("property") else {
                    continue;
                };
                let all = args(call);
                let (prefix, mounted) = match text(property, src) {
                    "use" | "route" if all.len() >= 2 => {
                        let Some(prefix) = string(all[0], src) else {
                            continue;
                        };
                        (prefix, all[all.len() - 1])
                    }
                    // Fastify: `app.register(users, { prefix: "/users" })`.
                    "register" if all.len() >= 2 => {
                        let options = text(all[1], src);
                        let Some(prefix) = options
                            .split_once("prefix")
                            .and_then(|(_, rest)| rest.split(['\'', '"', '`']).nth(1))
                        else {
                            continue;
                        };
                        (prefix.to_string(), all[0])
                    }
                    _ => continue,
                };
                if !prefix.starts_with('/') || mounted.kind() != "identifier" {
                    continue;
                }
                found.push((source.rel.clone(), prefix, text(mounted, src).to_string()));
            }
        }
        let mut mounts = Mounts::default();
        // A name is a router of the same file, or one imported from another.
        for (rel, prefix, name) in &found {
            match imported_file(corpus, rel, name) {
                Some(target) => {
                    mounts.files.entry(target).or_insert_with(|| prefix.clone());
                }
                None => {
                    mounts
                        .vars
                        .entry((rel.clone(), name.clone()))
                        .or_insert_with(|| prefix.clone());
                }
            }
        }
        // A file mounted under a file mounted somewhere: both prefixes.
        for _ in 0..4 {
            let mut changed = false;
            for (rel, _, name) in &found {
                let Some(target) = imported_file(corpus, rel, name) else {
                    continue;
                };
                let Some(outer) = mounts.files.get(rel).cloned() else {
                    continue;
                };
                let own = found
                    .iter()
                    .find(|(r, _, n)| r == rel && n == name)
                    .map(|(_, p, _)| p.clone())
                    .unwrap_or_default();
                let whole = join(&outer, &own);
                if mounts.files.get(&target) != Some(&whole) {
                    mounts.files.insert(target, whole);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        mounts
    }

    /// The prefix a route made on `var` in `rel` is served under.
    pub(super) fn prefix(&self, rel: &str, var: &str) -> String {
        let file = self.files.get(rel).map(String::as_str).unwrap_or("");
        let own = self
            .vars
            .get(&(rel.to_string(), var.to_string()))
            .map(String::as_str)
            .unwrap_or("");
        match (file, own) {
            ("", own) => own.to_string(),
            (file, "") => file.to_string(),
            (file, own) => join(file, own),
        }
    }
}

/// The project file `name` is imported from in `rel` (`import users from
/// "./routes/users"`, `const users = require("./routes/users")`), when it
/// is a relative import of one.
fn imported_file(corpus: &Corpus, rel: &str, name: &str) -> Option<String> {
    let source = corpus.get(rel)?;
    let text = source.text.as_str();
    let module = text.lines().find_map(|line| {
        let line = line.trim();
        let imports_name = (line.starts_with("import ") || line.contains("require("))
            && line
                .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'))
                .any(|w| w == name);
        if !imports_name {
            return None;
        }
        let quoted = line.split(['\'', '"']).nth(1)?;
        quoted.starts_with('.').then(|| quoted.to_string())
    })?;
    let dir = rel.rsplit_once('/').map_or("", |(d, _)| d);
    let mut parts: Vec<&str> = dir.split('/').filter(|p| !p.is_empty()).collect();
    for seg in module.split('/') {
        match seg {
            "." | "" => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    let base = parts.join("/");
    let stem = base
        .strip_suffix(".js")
        .or_else(|| base.strip_suffix(".mjs"))
        .unwrap_or(&base);
    let candidates = [
        base.clone(),
        format!("{stem}.ts"),
        format!("{stem}.tsx"),
        format!("{stem}.js"),
        format!("{stem}.mjs"),
        format!("{stem}.cjs"),
        format!("{base}/index.ts"),
        format!("{base}/index.js"),
    ];
    candidates.into_iter().find(|c| corpus.get(c).is_some())
}

/// `Bun.serve({ routes: { "/users/:id": handler, "/api/posts": { GET, POST } } })`.
pub(super) fn bun(routes: &mut Routes, source: &Source, root: Node<'_>) {
    let src = source.text.as_str();
    if !src.contains("Bun.serve") {
        return;
    }
    for call in all_of(root, "call_expression") {
        let Some(function) = call.child_by_field_name("function") else {
            continue;
        };
        if text(function, src) != "Bun.serve" {
            continue;
        }
        let Some(options) = args(call)
            .into_iter()
            .next()
            .filter(|a| a.kind() == "object")
        else {
            continue;
        };
        let Some(table) = pairs(options)
            .into_iter()
            .find(|p| key(*p, src).as_deref() == Some("routes"))
            .and_then(|p| p.child_by_field_name("value"))
            .filter(|v| v.kind() == "object")
        else {
            continue;
        };
        for route in pairs(table) {
            let (Some(path), Some(value)) = (key(route, src), route.child_by_field_name("value"))
            else {
                continue;
            };
            if !path.starts_with('/') {
                continue;
            }
            let by_method: Vec<(String, Node<'_>)> = match value.kind() {
                "object" => pairs(value)
                    .into_iter()
                    .filter_map(|p| Some((key(p, src)?, p.child_by_field_name("value")?)))
                    .filter(|(m, _)| VERBS.contains(&m.to_ascii_lowercase().as_str()))
                    .collect(),
                _ => vec![("ANY".to_string(), value)],
            };
            for (method, handler) in by_method {
                let handler = match handler.kind() {
                    "identifier" | "member_expression" => Handler::Named {
                        name: last_name(text(handler, src)),
                        class: None,
                    },
                    _ => Handler::Inline,
                };
                routes.add("Bun", &method, &path, &source.rel, line(route), handler);
            }
        }
    }
}

/// Hapi: `server.route({ method: "GET", path: "/x", handler })`, one or
/// an array of them.
pub(super) fn hapi(routes: &mut Routes, source: &Source, root: Node<'_>) {
    let src = source.text.as_str();
    if !(src.contains("handler") && src.contains("method")) {
        return;
    }
    for object in all_of(root, "object") {
        let entries = pairs(object);
        let value = |name: &str| {
            entries
                .iter()
                .find(|p| key(**p, src).as_deref() == Some(name))
                .and_then(|p| p.child_by_field_name("value"))
        };
        let (Some(method), Some(path), Some(handler)) =
            (value("method"), value("path"), value("handler"))
        else {
            continue;
        };
        let (Some(method), Some(path)) = (string(method, src), string(path, src)) else {
            continue;
        };
        if !path.starts_with('/') {
            continue;
        }
        let handler = match handler.kind() {
            "identifier" | "member_expression" => Handler::Named {
                name: last_name(text(handler, src)),
                class: None,
            },
            _ => Handler::Inline,
        };
        let method = if method == "*" {
            "ANY".to_string()
        } else {
            method
        };
        routes.add("Hapi", &method, &path, &source.rel, line(object), handler);
    }
}

/// An Astro integration's pages: `injectRoute({ pattern: '/blog/[slug]',
/// entrypoint: './src/post.astro' })` serves the entry point at the
/// pattern — a page, or for a script an endpoint.
pub(super) fn inject_route(routes: &mut Routes, source: &Source, root: Node<'_>) {
    let src = source.text.as_str();
    if !src.contains("injectRoute") {
        return;
    }
    for call in all_of(root, "call_expression") {
        let Some(function) = call.child_by_field_name("function") else {
            continue;
        };
        if text(function, src).rsplit('.').next() != Some("injectRoute") {
            continue;
        }
        let Some(options) = args(call)
            .into_iter()
            .next()
            .filter(|a| a.kind() == "object")
        else {
            continue;
        };
        // A value's string, or its first (`prerender ? 'a.astro' : …`).
        let field = |name: &str| {
            let value = pairs(options)
                .into_iter()
                .find(|p| key(*p, src).as_deref() == Some(name))?
                .child_by_field_name("value")?;
            let first = all_of(value, "string").into_iter().next()?;
            string(first, src)
        };
        let (Some(pattern), Some(entrypoint)) = (field("pattern"), field("entrypoint")) else {
            continue;
        };
        let path: Vec<String> = pattern.split('/').filter_map(segment).collect();
        let file = entrypoint.rsplit('/').next().unwrap_or(&entrypoint);
        let page = [".astro", ".md", ".mdx", ".html"]
            .iter()
            .any(|e| file.ends_with(e));
        routes.add(
            "Astro",
            if page { "PAGE" } else { "ANY" },
            &format!("/{}", path.join("/")),
            &source.rel,
            line(call),
            Handler::At {
                rel: source.rel.clone(),
                line: line(call),
                label: file.to_string(),
            },
        );
    }
}

/// The `pair`s of an object literal.
fn pairs<'t>(object: Node<'t>) -> Vec<Node<'t>> {
    let mut cursor = object.walk();
    object
        .named_children(&mut cursor)
        .filter(|c| c.kind() == "pair")
        .collect()
}

/// A pair's key as written: a name, or a string's text.
fn key(pair: Node<'_>, src: &str) -> Option<String> {
    let key = pair.child_by_field_name("key")?;
    match key.kind() {
        "property_identifier" | "identifier" => Some(text(key, src).to_string()),
        "string" => string(key, src),
        _ => None,
    }
}

/// tRPC: each procedure of each router, by the dotted path a client calls
/// it by (`post.byId`) — a router's key in the router it is merged into
/// before its own — as a query, a mutation or a subscription.
pub(super) fn trpc(routes: &mut Routes, corpus: &Corpus) {
    // Each router: its variable (if any), its file, and its entries —
    // a procedure (kind, line) or another router (by name).
    enum Entry {
        Procedure(&'static str, usize),
        Router(String),
    }
    struct Router {
        rel: String,
        entries: Vec<(String, Entry)>,
    }
    let mut routers: Vec<(Option<String>, Router)> = Vec::new();
    for source in corpus.in_langs(&[Lang::JavaScript, Lang::TypeScript, Lang::Tsx]) {
        let src = source.text.as_str();
        if !src.contains("Procedure") && !src.contains("trpc") && !src.contains("TRPC") {
            continue;
        }
        let Some(tree) = source.tree() else {
            continue;
        };
        for object in all_of(tree.root_node(), "object") {
            let mut entries: Vec<(String, Entry)> = Vec::new();
            let mut procedures = 0;
            for pair in pairs(object) {
                let (Some(name), Some(value)) = (key(pair, src), pair.child_by_field_name("value"))
                else {
                    continue;
                };
                let v = text(value, src);
                let kind = [".query(", ".mutation(", ".subscription("]
                    .iter()
                    .filter_map(|k| v.rfind(k).map(|at| (at, *k)))
                    .max_by_key(|(at, _)| *at)
                    .map(|(_, k)| match k {
                        ".query(" => "QUERY",
                        ".mutation(" => "MUTATION",
                        _ => "SUBSCRIPTION",
                    });
                match (kind, value.kind()) {
                    (Some(kind), "call_expression") if v.contains("rocedure") => {
                        procedures += 1;
                        entries.push((name, Entry::Procedure(kind, line(pair))));
                    }
                    (_, "identifier") if v.ends_with("Router") || v.ends_with("router") => {
                        entries.push((name, Entry::Router(v.to_string())));
                    }
                    _ => {}
                }
            }
            // A router: an object of procedures, or one a router is made
            // of (`createTRPCRouter({ post: postRouter })`) — not a call's
            // options that name one (`{ router: appRouter, ctx }`).
            let made_into_router = object
                .parent()
                .filter(|p| p.kind() == "arguments")
                .and_then(|p| p.parent())
                .and_then(|call| call.child_by_field_name("function"))
                .is_some_and(|f| {
                    let name = text(f, src);
                    let last = name.rsplit('.').next().unwrap_or(name);
                    last.ends_with("router") || last.ends_with("Router") || last == "mergeRouters"
                });
            if procedures == 0 && !made_into_router {
                continue;
            }
            // Its variable: `const postRouter = router({…})`, `= {…}
            // satisfies TRPCRouterRecord`.
            let mut at = object.parent();
            while let Some(n) = at {
                if matches!(
                    n.kind(),
                    "variable_declarator" | "statement_block" | "program"
                ) {
                    break;
                }
                at = n.parent();
            }
            let var = at
                .filter(|n| n.kind() == "variable_declarator")
                .and_then(|n| n.child_by_field_name("name"))
                .map(|n| text(n, src).to_string());
            routers.push((
                var,
                Router {
                    rel: source.rel.clone(),
                    entries,
                },
            ));
        }
    }
    // Each router's path: its key in the router that merges it.
    let mut prefix: HashMap<String, String> = HashMap::new();
    for _ in 0..6 {
        let mut changed = false;
        for (var, router) in &routers {
            let own = var
                .as_ref()
                .and_then(|v| prefix.get(v))
                .cloned()
                .unwrap_or_default();
            for (name, entry) in &router.entries {
                if let Entry::Router(child) = entry {
                    let path = if own.is_empty() {
                        name.clone()
                    } else {
                        format!("{own}.{name}")
                    };
                    if prefix.get(child) != Some(&path) {
                        prefix.insert(child.clone(), path);
                        changed = true;
                    }
                }
            }
        }
        if !changed {
            break;
        }
    }
    for (var, router) in &routers {
        let own = var
            .as_ref()
            .and_then(|v| prefix.get(v))
            .cloned()
            .unwrap_or_default();
        for (name, entry) in &router.entries {
            if let Entry::Procedure(kind, at) = entry {
                let path = if own.is_empty() {
                    name.clone()
                } else {
                    format!("{own}.{name}")
                };
                routes.add_named("tRPC", kind, &path, &router.rel, *at);
            }
        }
    }
}

impl Routes<'_> {
    /// A route called by a name, not a path: a tRPC procedure.
    fn add_named(&mut self, framework: &str, method: &str, name: &str, rel: &str, at: usize) {
        let label = format!("{method} {name}");
        let route = self.map.node("route", &label, &label);
        self.map.place(route, rel, at);
        self.map.detail(route, framework);
    }
}

// -- client routes -----------------------------------------------------------------

/// The client router a file's route objects are for, by what it imports.
/// Whether a file may hold routes the per-file readers below find — read
/// off its text, so the rest are not parsed for them.
pub(super) fn may_route(src: &str) -> bool {
    src.contains("Bun.serve")
        || src.contains("injectRoute")
        || (src.contains("handler") && src.contains("method"))
        || src.contains("createFileRoute")
        || client_router(src).is_some()
}

fn client_router(src: &str) -> Option<&'static str> {
    let has = |m: &str| src.contains(&format!("'{m}'")) || src.contains(&format!("\"{m}\""));
    if has("vue-router") {
        Some("Vue Router")
    } else if has("react-router") || has("react-router-dom") {
        Some("React Router")
    } else if has("@angular/router") {
        Some("Angular")
    } else if has("@solidjs/router") {
        Some("Solid Router")
    } else {
        None
    }
}

/// A route object's page: its `component`, `element` or `Component`, by
/// name, or a lazily imported file.
fn page_of(object: Node<'_>, src: &str, rel: &str, corpus: &Corpus) -> Handler {
    for (name, value) in pairs(object)
        .into_iter()
        .filter_map(|p| Some((key(p, src)?, p.child_by_field_name("value")?)))
    {
        match name.as_str() {
            "component" | "Component" | "loadComponent" | "lazy" | "element" => {}
            _ => continue,
        }
        let t = text(value, src);
        // `() => import("./views/Home.vue")`.
        if let Some(path) = t
            .split_once("import(")
            .and_then(|(_, r)| r.split(['\'', '"', '`']).nth(1))
            && path.starts_with('.')
        {
            let dir = rel.rsplit_once('/').map_or("", |(d, _)| d);
            let mut parts: Vec<&str> = dir.split('/').filter(|p| !p.is_empty()).collect();
            for seg in path.split('/') {
                match seg {
                    "." | "" => {}
                    ".." => {
                        parts.pop();
                    }
                    s => parts.push(s),
                }
            }
            let joined = parts.join("/");
            let found = [
                joined.clone(),
                format!("{joined}.tsx"),
                format!("{joined}.ts"),
                format!("{joined}.jsx"),
                format!("{joined}.js"),
                format!("{joined}.vue"),
            ]
            .into_iter()
            .find(|c| corpus.get(c).is_some());
            if let Some(file) = found {
                let label = file.rsplit('/').next().unwrap_or(&file).to_string();
                return Handler::At {
                    rel: file,
                    line: 1,
                    label,
                };
            }
        }
        // `<Home />`.
        if let Some(tag) = t.strip_prefix('<') {
            let tag: String = tag
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '.')
                .collect();
            if !tag.is_empty() {
                return Handler::Named {
                    name: last_name(&tag),
                    class: None,
                };
            }
        }
        if matches!(value.kind(), "identifier" | "member_expression") {
            return Handler::Named {
                name: last_name(t),
                class: None,
            };
        }
    }
    Handler::Inline
}

/// A route's path as written — a string — or as a constant names it
/// (`paths.app.discussions.path`): the string at the end of those keys in
/// the object the project declares by that first name
/// (`export const paths = { app: { discussions: { path: "/app/…" } } }`).
fn path_value(value: Node<'_>, src: &str, rel: &str, corpus: &Corpus) -> Option<String> {
    if let Some(s) = string(value, src) {
        return Some(s);
    }
    if value.kind() != "member_expression" {
        return None;
    }
    let chain: Vec<&str> = text(value, src).split('.').map(str::trim).collect();
    let (first, keys) = chain.split_first()?;
    if keys.is_empty()
        || chain
            .iter()
            .any(|k| k.is_empty() || k.contains(['(', '[', ' ']))
    {
        return None;
    }
    let needle = format!("const {first} =");
    // The declaration nearest the route's file first: a monorepo's apps
    // may each have a `paths` of their own.
    let mut candidates: Vec<&Source> = corpus
        .in_langs(&[Lang::JavaScript, Lang::TypeScript, Lang::Tsx])
        .filter(|s| s.text.contains(&needle))
        .collect();
    let shared = |other: &str| {
        rel.split('/')
            .zip(other.split('/'))
            .take_while(|(a, b)| a == b)
            .count()
    };
    candidates.sort_by_key(|s| std::cmp::Reverse(shared(&s.rel)));
    let lookup = |source: &Source| -> Option<String> {
        let tree = source.tree()?;
        let s = source.text.as_str();
        all_of(tree.root_node(), "variable_declarator")
            .into_iter()
            .filter(|d| d.child_by_field_name("name").map(|n| text(n, s)) == Some(*first))
            .find_map(|declarator| {
                // `= {…}`, `= {…} as const`, `= {…} satisfies X`.
                let mut at = declarator.child_by_field_name("value")?;
                while matches!(
                    at.kind(),
                    "as_expression" | "satisfies_expression" | "parenthesized_expression"
                ) {
                    at = at.named_child(0)?;
                }
                for (k, name) in keys.iter().enumerate() {
                    if at.kind() != "object" {
                        return None;
                    }
                    let value = pairs(at)
                        .into_iter()
                        .find(|p| key(*p, s).as_deref() == Some(*name))?
                        .child_by_field_name("value")?;
                    if k + 1 == keys.len() {
                        return string(value, s);
                    }
                    at = value;
                }
                None
            })
    };
    candidates.into_iter().find_map(lookup)
}

/// The route objects of React Router's and Vue Router's configurations
/// (`createBrowserRouter([…])`, `createRouter({ routes: […] })`, a
/// `routes` array): each object with a `path` and a page, its path joined
/// to those of the route objects whose `children` it is in.
pub(super) fn route_objects(routes: &mut Routes, source: &Source, root: Node<'_>) {
    let src = source.text.as_str();
    let Some(framework) = client_router(src) else {
        return;
    };
    for object in all_of(root, "object") {
        let entries = pairs(object);
        let has = |name: &str| {
            entries
                .iter()
                .any(|p| key(*p, src).as_deref() == Some(name))
        };
        let Some(path) = entries
            .iter()
            .find(|p| key(**p, src).as_deref() == Some("path"))
            .and_then(|p| p.child_by_field_name("value"))
            .and_then(|v| path_value(v, src, &source.rel, routes.corpus))
        else {
            continue;
        };
        let page = [
            "component",
            "Component",
            "element",
            "lazy",
            "loadComponent",
            "loadChildren",
            "children",
            "redirect",
            "redirectTo",
            "components",
        ]
        .iter()
        .any(|k| has(k));
        if !page || has("method") || has("handler") {
            continue;
        }
        // The paths of the route objects it is a child of.
        let mut full = path.clone();
        let mut at = object.parent();
        while let Some(n) = at {
            if n.kind() == "object" {
                let parent = pairs(n)
                    .into_iter()
                    .find(|p| key(*p, src).as_deref() == Some("path"))
                    .and_then(|p| p.child_by_field_name("value"))
                    .and_then(|v| path_value(v, src, &source.rel, routes.corpus));
                if let Some(parent) = parent
                    && !full.starts_with('/')
                {
                    full = join(&parent, &full);
                }
            }
            at = n.parent();
        }
        let handler = page_of(object, src, &source.rel, routes.corpus);
        let shown = if full.is_empty() || full == "*" || full.starts_with('/') {
            full
        } else {
            format!("/{full}")
        };
        routes.add(
            framework,
            "PAGE",
            &shown,
            &source.rel,
            line(object),
            handler,
        );
    }
}

/// React Router's `<Route path="…" element={<Page />}>`, nested in the
/// `<Route>`s around it.
pub(super) fn route_elements(routes: &mut Routes, source: &Source, root: Node<'_>) {
    let src = source.text.as_str();
    if client_router(src) != Some("React Router") {
        return;
    }
    fn attribute<'t>(element: Node<'t>, name: &str, src: &str) -> Option<Node<'t>> {
        let mut cursor = element.walk();
        element
            .named_children(&mut cursor)
            .filter(|a| a.kind() == "jsx_attribute")
            .find(|a| a.named_child(0).is_some_and(|n| text(n, src) == name))
            .and_then(|a| a.named_child(1))
    }
    let opening_tag = |n: Node<'_>| -> bool {
        n.child_by_field_name("name")
            .is_some_and(|t| text(t, src) == "Route")
    };
    let elements = all_of(root, "jsx_opening_element")
        .into_iter()
        .chain(all_of(root, "jsx_self_closing_element"));
    for element in elements.filter(|e| opening_tag(*e)) {
        let Some(path) = attribute(element, "path", src).and_then(|v| string(v, src)) else {
            continue;
        };
        // The paths of the `<Route>`s it is inside.
        let mut full = path.clone();
        // An opening tag's own element is its route, not one around it.
        let mut at = match element.kind() {
            "jsx_opening_element" => element.parent().and_then(|p| p.parent()),
            _ => element.parent(),
        };
        while let Some(n) = at {
            if n.kind() == "jsx_element"
                && let Some(open) = n
                    .child_by_field_name("open_tag")
                    .filter(|o| opening_tag(*o))
                && let Some(parent) = attribute(open, "path", src).and_then(|v| string(v, src))
                && !full.starts_with('/')
            {
                full = join(&parent, &full);
            }
            at = n.parent();
        }
        let handler = attribute(element, "element", src)
            .or_else(|| attribute(element, "component", src))
            .or_else(|| attribute(element, "Component", src))
            .map(|v| {
                let t = text(v, src).trim_start_matches('{').trim_start();
                let tag: String = t
                    .trim_start_matches('<')
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '.')
                    .collect();
                match tag.is_empty() {
                    true => Handler::Inline,
                    false => Handler::Named {
                        name: last_name(&tag),
                        class: None,
                    },
                }
            })
            .unwrap_or(Handler::Inline);
        let shown = if full.starts_with('/') || full == "*" {
            full
        } else {
            format!("/{full}")
        };
        routes.add(
            "React Router",
            "PAGE",
            &shown,
            &source.rel,
            line(element),
            handler,
        );
    }
}

/// TanStack Router's file routes: `createFileRoute("/posts/$postId")`.
pub(super) fn tanstack(routes: &mut Routes, source: &Source, root: Node<'_>) {
    let src = source.text.as_str();
    if !src.contains("createFileRoute") {
        return;
    }
    for call in all_of(root, "call_expression") {
        let Some(function) = call.child_by_field_name("function") else {
            continue;
        };
        if text(function, src) != "createFileRoute" {
            continue;
        }
        let Some(path) = args(call).first().and_then(|a| string(*a, src)) else {
            continue;
        };
        let path = path
            .split('/')
            .filter(|s| !(s.starts_with('_') || s.starts_with('(') && s.ends_with(')')))
            .map(|s| match s.strip_prefix('$') {
                Some("") => "*".to_string(),
                Some(param) => format!(":{param}"),
                None => s.to_string(),
            })
            .collect::<Vec<_>>()
            .join("/");
        routes.add(
            "TanStack Router",
            "PAGE",
            &path,
            &source.rel,
            line(call),
            Handler::At {
                rel: source.rel.clone(),
                line: line(call),
                label: source.name().to_string(),
            },
        );
    }
}

/// One folder or file name of a file-routed page, as a route's path says
/// it: `[id]` is `:id`, `[...slug]` and `[[...slug]]` are `*slug`, an
/// optional `[[lang]]` is `:lang?`, SvelteKit's matched `[id=integer]` is
/// `:id`, a parameter inside a name (`@[user]`) is one there (`@:user`),
/// Remix's `$id` is `:id` and its `$` is `*`; a group (`(marketing)`) and
/// an index name nothing.
pub(super) fn segment(s: &str) -> Option<String> {
    if (s.starts_with('(') && s.ends_with(')')) || s.is_empty() || s == "index" || s == "_index" {
        return None;
    }
    if s == "$" {
        return Some("*".to_string());
    }
    if let Some(param) = s.strip_prefix('$') {
        return Some(format!(":{param}"));
    }
    let mut out = String::new();
    let mut rest = s;
    while let Some(open) = rest.find('[') {
        out.push_str(&rest[..open]);
        let optional = rest[open..].starts_with("[[");
        let (start, close) = match optional {
            true => (open + 2, "]]"),
            false => (open + 1, "]"),
        };
        let Some(len) = rest[start..].find(close) else {
            // An unclosed bracket is the name's own.
            out.push_str(&rest[open..]);
            rest = "";
            break;
        };
        let inner = &rest[start..start + len];
        let name = inner.split('=').next().unwrap_or(inner);
        match name.strip_prefix("...") {
            Some(name) => {
                out.push('*');
                out.push_str(name);
            }
            None => {
                out.push(':');
                out.push_str(name);
                if optional {
                    out.push('?');
                }
            }
        }
        rest = &rest[start + len + close.len()..];
    }
    out.push_str(rest);
    Some(out)
}

/// A Remix flat route's file name as its path: each `.` a `/`, `$id` a
/// parameter and a bare `$` the rest, `($lang)` an optional parameter and
/// `(about)` an optional segment, `[.]` (or any bracketed text) the
/// characters themselves, a `_layout` part nothing and a trailing `_` (out
/// of its parent's layout) dropped.
fn remix_path(name: &str) -> String {
    // Each part's characters, each with whether brackets escaped it.
    let mut parts: Vec<Vec<(char, bool)>> = vec![Vec::new()];
    let mut escaped = false;
    for c in name.chars() {
        match c {
            '[' if !escaped => escaped = true,
            ']' if escaped => escaped = false,
            '.' if !escaped => parts.push(Vec::new()),
            c => parts.last_mut().expect("one part").push((c, escaped)),
        }
    }
    let mut path = Vec::new();
    for mut part in parts {
        let plain = |part: &[(char, bool)], at: usize, c: char| part.get(at) == Some(&(c, false));
        if plain(&part, 0, '_') || part.is_empty() {
            continue;
        }
        if part.len() > 1 && plain(&part, part.len() - 1, '_') {
            part.pop();
        }
        let optional = part.len() > 2 && plain(&part, 0, '(') && plain(&part, part.len() - 1, ')');
        if optional {
            part = part[1..part.len() - 1].to_vec();
        }
        let text: String = part.iter().map(|(c, _)| c).collect();
        let mut out = match plain(&part, 0, '$') {
            true if text == "$" => "*".to_string(),
            true => format!(":{}", &text[1..]),
            false => text,
        };
        if optional {
            out.push('?');
        }
        path.push(out);
    }
    format!("/{}", path.join("/"))
}

/// The pages of the file-routed frameworks, by their files: Nuxt's
/// `pages/`, SvelteKit's `src/routes/**/+page.svelte` and `+server` files,
/// Remix's flat `app/routes/` files, and Astro's `src/pages/`.
pub(super) fn file_routes(routes: &mut Routes, corpus: &Corpus) {
    let has_file = |pred: &dyn Fn(&str) -> bool| corpus.files.iter().any(|s| pred(s.name()));
    let package_has = |dep: &str| {
        corpus
            .files
            .iter()
            .any(|s| s.name() == "package.json" && s.text.contains(&format!("\"{dep}")))
    };
    let nuxt = has_file(&|n| n.starts_with("nuxt.config.")) || package_has("nuxt\"");
    let sveltekit = has_file(&|n| n.starts_with("svelte.config.")) && package_has("@sveltejs/kit");
    let remix = package_has("@remix-run/");
    let astro = has_file(&|n| n.starts_with("astro.config."));
    let rels: Vec<&Source> = corpus.files.iter().collect();
    for source in rels {
        let rel = source.rel.as_str();
        // A test's fixture app (`__e2e__/fixtures/basics/src/pages/`) is
        // no page of this one.
        if crate::maps::is_js_test(rel) {
            continue;
        }
        let (dir, file) = rel.rsplit_once('/').unwrap_or(("", rel));
        let (stem, ext) = file.rsplit_once('.').unwrap_or((file, ""));
        let dirs: Vec<&str> = dir.split('/').filter(|s| !s.is_empty()).collect();
        let under = |root: &[&str]| -> Option<Vec<&str>> {
            (0..dirs.len().saturating_sub(root.len() - 1))
                .find(|&i| dirs[i..].starts_with(root))
                .map(|i| dirs[i + root.len()..].to_vec())
        };
        let page = |parts: Vec<&str>| -> String {
            let path: Vec<String> = parts.into_iter().filter_map(segment).collect();
            format!("/{}", path.join("/"))
        };
        let at = |label: &str| Handler::At {
            rel: rel.to_string(),
            line: 1,
            label: label.to_string(),
        };
        if nuxt
            && ext == "vue"
            && let Some(mut inside) = under(&["pages"])
        {
            inside.push(stem);
            routes.add("Nuxt", "PAGE", &page(inside), rel, 1, at(file));
            continue;
        }
        if nuxt
            && matches!(ext, "ts" | "js")
            && let Some(mut inside) = under(&["server", "api"])
        {
            // `server/api/users/[id].get.ts`: its method by its name.
            let (name, method) = match stem.rsplit_once('.') {
                Some((name, m)) if VERBS.contains(&m) => (name, m.to_ascii_uppercase()),
                _ => (stem, "ANY".to_string()),
            };
            inside.insert(0, "api");
            inside.push(name);
            routes.add("Nuxt", &method, &page(inside), rel, 1, at(file));
            continue;
        }
        if sveltekit && let Some(inside) = under(&["src", "routes"]) {
            match (stem, ext) {
                ("+page", "svelte") => {
                    routes.add("SvelteKit", "PAGE", &page(inside), rel, 1, at(file));
                }
                ("+server", "ts" | "js") => {
                    server_exports(routes, source, "SvelteKit", &page(inside))
                }
                ("+page.server", "ts" | "js") => form_actions(routes, source, &page(inside)),
                _ => {}
            }
            continue;
        }
        if remix
            && matches!(ext, "tsx" | "jsx" | "ts" | "js")
            && let Some(inside) = under(&["app", "routes"])
        {
            // Flat routes: `posts.$id.tsx`, `_auth.login.tsx`; a folder's
            // `route.tsx` is the folder's.
            let name = if stem == "route" && !inside.is_empty() {
                inside.join(".")
            } else {
                stem.to_string()
            };
            if !inside.is_empty() && stem != "route" {
                continue;
            }
            let path = remix_path(&name);
            // A page is its default export; its `loader` serves its GETs
            // and its `action` its form posts. A resource route (no
            // default export) is its loader and action alone.
            let page_too = source.text.contains("export default");
            if page_too {
                routes.add("Remix", "PAGE", &path, rel, 1, at(file));
            }
            let mut served = false;
            for (name, at_line) in exports(source) {
                let method = match name {
                    "action" => "POST",
                    "loader" if !page_too => "GET",
                    _ => continue,
                };
                served = true;
                let handler = Handler::At {
                    rel: rel.to_string(),
                    line: at_line,
                    label: name.to_string(),
                };
                routes.add("Remix", method, &path, rel, at_line, handler);
            }
            if !page_too && !served {
                routes.add("Remix", "ANY", &path, rel, 1, at(file));
            }
            continue;
        }
        if astro && let Some(mut inside) = under(&["src", "pages"]) {
            match ext {
                "astro" | "md" | "mdx" | "html" => {
                    inside.push(stem);
                    routes.add("Astro", "PAGE", &page(inside), rel, 1, at(file));
                }
                "ts" | "js" => {
                    inside.push(stem.trim_end_matches(".json"));
                    server_exports(routes, source, "Astro", &page(inside));
                }
                _ => {}
            }
        }
    }
}

/// A module's named exports, each visited with its name, its line and,
/// for a `const`, its value (`export function GET()`, `export const
/// actions = { … }`).
fn each_export<'s>(source: &'s Source, mut visit: impl FnMut(&'s str, usize, Option<Node<'_>>)) {
    let Some(tree) = source.tree() else {
        return;
    };
    let src = source.text.as_str();
    for export in all_of(tree.root_node(), "export_statement") {
        let Some(declaration) = export.child_by_field_name("declaration") else {
            continue;
        };
        let (name, value) = match declaration.kind() {
            "function_declaration" => (declaration.child_by_field_name("name"), None),
            "lexical_declaration" => {
                let first = all_of(declaration, "variable_declarator").first().copied();
                (
                    first.and_then(|d| d.child_by_field_name("name")),
                    first.and_then(|d| d.child_by_field_name("value")),
                )
            }
            _ => (None, None),
        };
        if let Some(name) = name {
            visit(text(name, src), line(export), value);
        }
    }
}

/// A module's named exports and their lines.
fn exports(source: &Source) -> Vec<(&str, usize)> {
    let mut out = Vec::new();
    each_export(source, |name, at, _| out.push((name, at)));
    out
}

/// A route file's handlers by method: `export function GET()`,
/// `export const POST = …`.
fn server_exports(routes: &mut Routes, source: &Source, framework: &str, path: &str) {
    for (name, at_line) in exports(source) {
        let method = match name {
            "ALL" => "ANY",
            name if VERBS.contains(&name.to_ascii_lowercase().as_str()) => name,
            _ => continue,
        };
        routes.add(
            framework,
            method,
            path,
            &source.rel,
            at_line,
            Handler::At {
                rel: source.rel.clone(),
                line: at_line,
                label: name.to_string(),
            },
        );
    }
}

/// A SvelteKit page's form actions (`export const actions = { default,
/// login }`), each a POST to the page: the default one at its path, a
/// named one at `?/name`, as a form's `action` attribute says it.
fn form_actions(routes: &mut Routes, source: &Source, path: &str) {
    let src = source.text.as_str();
    each_export(source, |name, _, value| {
        let Some(mut value) = value.filter(|_| name == "actions") else {
            return;
        };
        // `{ … } satisfies Actions`
        while matches!(value.kind(), "satisfies_expression" | "as_expression")
            && let Some(inner) = value.named_child(0)
        {
            value = inner;
        }
        if value.kind() != "object" {
            return;
        }
        let mut cursor = value.walk();
        for member in value.named_children(&mut cursor) {
            let key = match member.kind() {
                "pair" | "method_definition" => member
                    .child_by_field_name("key")
                    .or_else(|| member.child_by_field_name("name")),
                "shorthand_property_identifier" => Some(member),
                _ => None,
            };
            let Some(key) = key.map(|k| text(k, src).trim_matches(['"', '\''])) else {
                continue;
            };
            let route = match key {
                "default" => path.to_string(),
                key => format!("{path}?/{key}"),
            };
            routes.add(
                "SvelteKit",
                "POST",
                &route,
                &source.rel,
                line(member),
                Handler::At {
                    rel: source.rel.clone(),
                    line: line(member),
                    label: format!("actions.{key}"),
                },
            );
        }
    });
}
