//! The component tree of React, Vue, Svelte and Astro projects: each
//! component, where it is defined, the props it declares, and an arrow to
//! each of the project's components it renders, labelled with the props it
//! passes.
//!
//! A component is known by its file and name, not its name alone: a Next.js
//! app has a `Page` in every route and a Nuxt app an `index.vue` in every
//! folder, each its own. What a tag names is read the way the bundler reads
//! it — the component of that name in the same file, else the one its
//! import points at (relative, `@/`/`~/`/`$lib/` aliases, a workspace
//! package), else, for a name nothing imports (Nuxt's and
//! unplugin-vue-components' auto-imports, Nuxt's `components/base/Button.vue`
//! as `BaseButton`), the nearest component of that name. A tag imported from
//! a package outside the project names none of the project's components.

use std::collections::{BTreeMap, HashMap, HashSet};

use clew_protocol::ProjectMap;
use tree_sitter::Node;

use super::super::{Corpus, MapBuilder, Source, all_of, ancestor, line, text};
use crate::highlight::Lang;

/// Calls that wrap a component and are still it: `memo(…)`,
/// `forwardRef(…)`, MobX's `observer(…)`, a higher-order `withRouter(…)`.
fn wraps_a_component(callee: &str) -> bool {
    let last = callee.rsplit('.').next().unwrap_or(callee);
    matches!(
        last,
        "memo" | "forwardRef" | "observer" | "connect" | "lazy"
    ) || last
        .strip_prefix("with")
        .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_uppercase()))
}

pub(super) fn capitalised(name: &str) -> bool {
    name.chars().next().is_some_and(|c| c.is_ascii_uppercase())
}

/// `todo-list` → `TodoList`, `nav_bar` → `NavBar`, `index` → `Index`.
pub(super) fn pascal(raw: &str) -> String {
    raw.split(['-', '_', '.', ' '])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut c = w.chars();
            c.next()
                .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                .unwrap_or_default()
        })
        .collect()
}

pub(super) use super::super::is_js_test as is_fixture;

/// A component file's own name for its component: `TodoList.vue` →
/// `TodoList`, `+page.svelte` → `Page`, `index.astro` → its folder's.
pub(super) fn file_component_name(rel: &str) -> String {
    let mut parts = rel.rsplit('/');
    let name = parts.next().unwrap_or(rel);
    let stem = name.split('.').next().unwrap_or(name);
    let stem = stem.trim_start_matches(['+', '_', '[', '(']);
    let stem = stem.trim_end_matches([']', ')']);
    if stem.is_empty() || stem == "index" {
        // `Button/index.tsx` is `Button`; `pages/index.vue`, a route's
        // index, is `Index`.
        if let Some(dir) = parts.next()
            && !matches!(
                dir,
                "pages" | "routes" | "app" | "src" | "views" | "screens" | "lib" | "components"
            )
            && !dir.starts_with(['[', '(', '+', '_'])
        {
            let named = pascal(dir);
            if !named.is_empty() {
                return named;
            }
        }
        return "Index".to_string();
    }
    pascal(stem)
}

/// The name a function or class node is known by, when it is a component's:
/// a capitalised declaration, a capitalised variable holding it (also
/// through `memo(…)`, `forwardRef(…)` and the like), or a file's anonymous
/// default export, named for its file. With where it is declared.
fn declared<'t>(n: Node<'t>, src: &str, rel: &str) -> Option<(String, Node<'t>)> {
    match n.kind() {
        "function_declaration"
        | "generator_function_declaration"
        | "class_declaration"
        | "abstract_class_declaration" => {
            let name = text(n.child_by_field_name("name")?, src);
            return capitalised(name).then(|| (name.to_string(), n));
        }
        "function_expression" | "function" | "arrow_function" | "class" => {}
        "variable_declarator" => {
            // `const Counter = defineComponent({ setup() { return () => <p/> } })`
            let name = text(n.child_by_field_name("name")?, src);
            let value = n.child_by_field_name("value")?;
            let callee = value
                .child_by_field_name("function")
                .map(|f| text(f, src))
                .unwrap_or("");
            return (capitalised(name) && value.kind() == "call_expression")
                .then_some(())
                .filter(|_| callee.ends_with("defineComponent") || wraps_a_component(callee))
                .map(|_| (name.to_string(), n));
        }
        _ => return None,
    }
    // A function or class expression: what holds it.
    let mut at = n;
    while let Some(parent) = at.parent() {
        match parent.kind() {
            "arguments"
            | "parenthesized_expression"
            | "as_expression"
            | "satisfies_expression"
            | "non_null_expression" => at = parent,
            "call_expression" => {
                let callee = parent
                    .child_by_field_name("function")
                    .map(|f| text(f, src))
                    .unwrap_or("");
                // `connect(a, b)(Component)` and `withX(opts)(…)` call twice.
                let callee = callee.split('(').next().unwrap_or(callee);
                if !wraps_a_component(callee) {
                    break;
                }
                at = parent;
            }
            "variable_declarator" => {
                let name = text(parent.child_by_field_name("name")?, src);
                if capitalised(name) {
                    return Some((name.to_string(), parent));
                }
                break;
            }
            "export_statement"
                if parent.child_by_field_name("value").is_some()
                    || text(parent, src).starts_with("export default") =>
            {
                let own = n
                    .child_by_field_name("name")
                    .map(|x| text(x, src).to_string())
                    .filter(|x| capitalised(x));
                return Some((own.unwrap_or_else(|| file_component_name(rel)), parent));
            }
            _ => break,
        }
    }
    let own = text(n.child_by_field_name("name")?, src);
    capitalised(own).then(|| (own.to_string(), n))
}

/// The component (or, with `hooks`, the `use…` hook) a node sits in: the
/// nearest enclosing function or class that is one. Its name and where it is
/// declared.
pub(super) fn enclosing<'t>(
    node: Node<'t>,
    src: &str,
    rel: &str,
    hooks: bool,
) -> Option<(String, Node<'t>)> {
    let mut at = node.parent();
    while let Some(n) = at {
        if let Some(found) = declared(n, src, rel) {
            return Some(found);
        }
        if hooks
            && matches!(n.kind(), "function_declaration" | "variable_declarator")
            && let Some(name) = n.child_by_field_name("name").map(|x| text(x, src))
            && name.starts_with("use")
            && name[3..].starts_with(|c: char| c.is_ascii_uppercase())
            && (n.kind() == "function_declaration"
                || n.child_by_field_name("value").is_some_and(|v| {
                    matches!(
                        v.kind(),
                        "arrow_function" | "function_expression" | "function"
                    )
                }))
        {
            return Some((name.to_string(), n));
        }
        at = n.parent();
    }
    None
}

/// A file's imports: local name → (module specifier, the name it is
/// exported as — `default` for a default import, `*` for a namespace).
pub(super) fn imports(root: Node<'_>, src: &str) -> HashMap<String, (String, String)> {
    let mut out = HashMap::new();
    for statement in all_of(root, "import_statement") {
        let Some(spec) = statement
            .child_by_field_name("source")
            .and_then(|s| super::super::unquote(text(s, src)))
        else {
            continue;
        };
        let Some(clause) = statement
            .named_children(&mut statement.walk())
            .find(|c| c.kind() == "import_clause")
        else {
            continue;
        };
        for part in clause.named_children(&mut clause.walk()) {
            match part.kind() {
                "identifier" => {
                    out.insert(
                        text(part, src).to_string(),
                        (spec.clone(), "default".into()),
                    );
                }
                "namespace_import" => {
                    if let Some(id) = part
                        .named_children(&mut part.walk())
                        .find(|c| c.kind() == "identifier")
                    {
                        out.insert(text(id, src).to_string(), (spec.clone(), "*".into()));
                    }
                }
                "named_imports" => {
                    for specifier in all_of(part, "import_specifier") {
                        let Some(name) = specifier.child_by_field_name("name") else {
                            continue;
                        };
                        let local = specifier.child_by_field_name("alias").unwrap_or(name);
                        out.insert(
                            text(local, src).to_string(),
                            (
                                spec.clone(),
                                text(name, src).trim_matches(['"', '\'']).to_string(),
                            ),
                        );
                    }
                }
                _ => {}
            }
        }
    }
    out
}

/// The files of the corpus, to resolve imports against.
pub(super) struct Files<'c> {
    rels: HashSet<&'c str>,
    /// A workspace package's name → its folder (`""` at the root).
    packages: HashMap<String, String>,
    /// Each `tsconfig`'s (or `jsconfig`'s) aliases, nearest the root last.
    configs: Vec<Aliases>,
}

/// A TypeScript config's `compilerOptions.paths` and `baseUrl`: the
/// aliases the files under its folder import by (`@components/*` →
/// `./src/components/*`).
struct Aliases {
    /// The folder of the config that holds them.
    dir: String,
    /// What `paths` and bare imports resolve against: `baseUrl`'s folder,
    /// else the config's.
    base: String,
    /// Whether `baseUrl` was set: then a bare `components/x` is the
    /// project's too.
    base_url: bool,
    paths: Vec<(String, Vec<String>)>,
}

impl Aliases {
    /// A config's aliases, its `extends` followed (within the project) for
    /// what it does not set.
    fn read(corpus: &Corpus, rel: &str, depth: usize) -> Option<Aliases> {
        let source = corpus.get(rel)?;
        let json: serde_json::Value =
            serde_json::from_str(&crate::imports::strip_jsonc(&source.text)).ok()?;
        let dir = rel
            .rsplit_once('/')
            .map(|(d, _)| d)
            .unwrap_or("")
            .to_string();
        let options = json.get("compilerOptions");
        let base_url = options
            .and_then(|o| o.get("baseUrl"))
            .and_then(|b| b.as_str())
            .and_then(|b| join(&dir, b));
        let paths: Vec<(String, Vec<String>)> = options
            .and_then(|o| o.get("paths"))
            .and_then(|p| p.as_object())
            .map(|p| {
                p.iter()
                    .map(|(k, v)| {
                        let targets = v
                            .as_array()
                            .map(|a| {
                                a.iter()
                                    .filter_map(|t| t.as_str().map(String::from))
                                    .collect()
                            })
                            .unwrap_or_default();
                        (k.clone(), targets)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut own = Aliases {
            base: base_url.clone().unwrap_or_else(|| dir.clone()),
            base_url: base_url.is_some(),
            dir: dir.clone(),
            paths,
        };
        if own.paths.is_empty() && depth < 4 {
            let extends = json.get("extends").and_then(|e| e.as_str()).unwrap_or("");
            if extends.starts_with('.')
                && let Some(base) = join(&dir, extends)
            {
                let base = match base.ends_with(".json") {
                    true => base,
                    false => format!("{base}.json"),
                };
                if let Some(inherited) = Aliases::read(corpus, &base, depth + 1) {
                    own.paths = inherited.paths;
                    if !own.base_url {
                        own.base = inherited.base;
                        own.base_url = inherited.base_url;
                    }
                }
            }
        }
        Some(own)
    }

    /// The paths an import may be, by these aliases.
    fn candidates(&self, spec: &str) -> Vec<String> {
        let mut out = Vec::new();
        for (pattern, targets) in &self.paths {
            let star = match pattern.split_once('*') {
                Some((head, tail)) => spec
                    .strip_prefix(head)
                    .and_then(|rest| rest.strip_suffix(tail)),
                None => (spec == pattern).then_some(""),
            };
            let Some(star) = star else {
                continue;
            };
            for target in targets {
                out.extend(join(&self.base, &target.replace('*', star)));
            }
        }
        if self.base_url {
            out.extend(join(&self.base, spec));
        }
        out
    }
}

impl<'c> Files<'c> {
    pub(super) fn of(corpus: &'c Corpus) -> Files<'c> {
        let mut packages = HashMap::new();
        for source in &corpus.files {
            if source.name() != "package.json" {
                continue;
            }
            let Ok(json) = serde_json::from_str::<serde_json::Value>(&source.text) else {
                continue;
            };
            if let Some(name) = json.get("name").and_then(|n| n.as_str()) {
                let dir = source.rel.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
                packages.insert(name.to_string(), dir.to_string());
            }
        }
        let mut configs: Vec<Aliases> = corpus
            .files
            .iter()
            .filter(|s| {
                let name = s.name();
                (name.starts_with("tsconfig") || name.starts_with("jsconfig"))
                    && name.ends_with(".json")
            })
            .filter_map(|s| Aliases::read(corpus, &s.rel, 0))
            .filter(|a| !a.paths.is_empty() || a.base_url)
            .collect();
        // The deepest first: a file's own app's config before the root's.
        configs.sort_by_key(|a| {
            std::cmp::Reverse(a.dir.matches('/').count() + usize::from(!a.dir.is_empty()))
        });
        Files {
            rels: corpus.files.iter().map(|s| s.rel.as_str()).collect(),
            packages,
            configs,
        }
    }

    /// The project file `spec` names, imported from `from` — `None` for a
    /// package outside the project, or a file not found.
    pub(super) fn resolve(&self, spec: &str, from: &str) -> Option<String> {
        let dir = from.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        let relative =
            spec.starts_with("./") || spec.starts_with("../") || spec == "." || spec == "..";
        if !relative {
            // The aliases of the configs over the importing file, nearest
            // first.
            let found = self
                .configs
                .iter()
                .filter(|c| {
                    c.dir.is_empty() || dir == c.dir || dir.starts_with(&format!("{}/", c.dir))
                })
                .flat_map(|c| c.candidates(spec))
                .find_map(|path| self.file_at(&path));
            if found.is_some() {
                return found;
            }
        }
        let bases: Vec<String> = if relative {
            vec![join(dir, spec)?]
        } else if let Some(rest) = ["@/", "~/", "#/", "~~/", "@@/", "src/"]
            .iter()
            .find_map(|p| spec.strip_prefix(p))
        {
            // An alias of the app's own folders: tried from the importing
            // file's folder up, `src/` first, so each app of a monorepo
            // finds its own.
            let mut out = Vec::new();
            for up in ancestors(dir) {
                for base in ["src/", "app/", ""] {
                    out.push(format!("{}{base}{rest}", prefix(&up)));
                }
            }
            out
        } else if let Some(rest) = spec.strip_prefix("$lib/") {
            ancestors(dir)
                .map(|up| format!("{}src/lib/{rest}", prefix(&up)))
                .collect()
        } else {
            // A workspace package, by its name: `@acme/ui/button`.
            let (name, sub) = package_name(spec);
            let at = self.packages.get(name)?;
            let mut out = Vec::new();
            for base in ["src/", ""] {
                let path = match sub {
                    Some(sub) => format!("{}{base}{sub}", prefix(at)),
                    None => format!("{}{base}index", prefix(at)),
                };
                out.push(path);
            }
            out
        };
        bases.iter().find_map(|base| self.file_at(base))
    }

    /// The file a resolved path names, with the extensions and index files
    /// an import may leave out (and TypeScript's `.js` that means `.ts`).
    fn file_at(&self, base: &str) -> Option<String> {
        const TRY: &[&str] = &[
            "",
            ".tsx",
            ".ts",
            ".jsx",
            ".js",
            ".mjs",
            ".vue",
            ".svelte",
            ".astro",
            "/index.tsx",
            "/index.ts",
            "/index.jsx",
            "/index.js",
            "/index.vue",
        ];
        let base = base.trim_end_matches('/');
        let stems = [
            Some(base),
            base.strip_suffix(".js")
                .or_else(|| base.strip_suffix(".jsx"))
                .or_else(|| base.strip_suffix(".mjs")),
        ];
        for stem in stems.into_iter().flatten() {
            for ext in TRY {
                let path = format!("{stem}{ext}");
                if self.rels.contains(path.as_str()) {
                    return Some(path);
                }
            }
        }
        None
    }

    /// The workspace package folder a bare specifier names, when it is one
    /// of the project's.
    pub(super) fn package_dir(&self, spec: &str) -> Option<&str> {
        self.packages.get(package_name(spec).0).map(String::as_str)
    }
}

/// `@scope/name/sub` → (`@scope/name`, `sub`); `name/sub` → (`name`, `sub`).
fn package_name(spec: &str) -> (&str, Option<&str>) {
    let cut = match spec.starts_with('@') {
        true => spec.match_indices('/').nth(1).map(|(i, _)| i),
        false => spec.find('/'),
    };
    match cut {
        Some(i) => (&spec[..i], Some(&spec[i + 1..])),
        None => (spec, None),
    }
}

fn prefix(dir: &str) -> String {
    match dir.is_empty() {
        true => String::new(),
        false => format!("{dir}/"),
    }
}

/// `dir` and each folder above it, up to the root (`""`).
fn ancestors(dir: &str) -> impl Iterator<Item = String> + '_ {
    let mut next = Some(dir.to_string());
    std::iter::from_fn(move || {
        let now = next.take()?;
        if !now.is_empty() {
            next = Some(
                now.rsplit_once('/')
                    .map(|(d, _)| d.to_string())
                    .unwrap_or_default(),
            );
        }
        Some(now)
    })
}

/// `dir` joined with a relative `spec`, `..` taken out; `None` past the root.
fn join(dir: &str, spec: &str) -> Option<String> {
    let mut parts: Vec<&str> = dir.split('/').filter(|p| !p.is_empty()).collect();
    for part in spec.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            p => parts.push(p),
        }
    }
    Some(parts.join("/"))
}

/// How many leading folders two paths share.
pub(super) fn shared_dirs(a: &str, b: &str) -> usize {
    fn dirs(p: &str) -> &str {
        p.rsplit_once('/').map(|(d, _)| d).unwrap_or("")
    }
    dirs(a)
        .split('/')
        .zip(dirs(b).split('/'))
        .take_while(|(x, y)| !x.is_empty() && x == y)
        .count()
}

/// A component found.
struct Component {
    name: String,
    rel: String,
    line: usize,
    /// What it renders: the tag as written, the props passed.
    renders: Vec<(String, Vec<String>)>,
    props: Vec<String>,
    /// Another name it is known by where nothing imports it (Nuxt's
    /// path-prefixed `BaseButton`).
    alias: Option<String>,
}

/// A file read for its components.
struct File {
    rel: String,
    imports: HashMap<String, (String, String)>,
    /// Its components, by name → index in the list.
    components: HashMap<String, usize>,
    /// The name it exports as `default`, when it says.
    default: Option<String>,
}

pub(super) fn map(corpus: &Corpus) -> Option<ProjectMap> {
    let files_index = Files::of(corpus);
    let mut found: Vec<Component> = Vec::new();
    let mut files: Vec<File> = Vec::new();
    for source in &corpus.files {
        if is_fixture(&source.rel) {
            continue;
        }
        let Some(lang) = source.lang else {
            continue;
        };
        let file = match lang {
            Lang::JavaScript | Lang::TypeScript | Lang::Tsx => script_file(source, &mut found),
            Lang::Vue | Lang::Svelte | Lang::Astro => component_file(source, lang, &mut found),
            _ => None,
        };
        files.extend(file);
    }
    if found.is_empty() {
        return None;
    }
    // Every component of a name, for the tags nothing imports.
    let mut by_name: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, c) in found.iter().enumerate() {
        by_name.entry(c.name.as_str()).or_default().push(i);
        if let Some(alias) = &c.alias {
            by_name.entry(alias.as_str()).or_default().push(i);
        }
    }
    let by_rel: HashMap<&str, &File> = files.iter().map(|f| (f.rel.as_str(), f)).collect();
    let nearest = |name: &str, from: &str, within: Option<&str>| -> Option<usize> {
        let candidates = by_name.get(name)?;
        candidates
            .iter()
            .copied()
            .filter(|&i| {
                within.is_none_or(|d| d.is_empty() || found[i].rel.starts_with(&prefix(d)))
            })
            .max_by_key(|&i| (shared_dirs(&found[i].rel, from), std::cmp::Reverse(i)))
    };
    // The component an imported name is, in the file it comes from.
    let in_file = |rel: &str, exported: &str, local: &str| -> Option<usize> {
        let file = by_rel.get(rel)?;
        if exported != "default"
            && let Some(&i) = file.components.get(exported)
        {
            return Some(i);
        }
        if exported == "default" {
            if let Some(&i) = file.default.as_ref().and_then(|d| file.components.get(d)) {
                return Some(i);
            }
            if file.components.len() == 1 {
                return file.components.values().next().copied();
            }
        }
        file.components.get(local).copied()
    };
    let target = |file: &File, tag: &str| -> Option<usize> {
        let (head, name) = match tag.rsplit_once('.') {
            Some((head, name)) => (Some(head), name),
            None => (None, tag),
        };
        if let Some(head) = head {
            // `<UI.Button>` from `import * as UI from './ui'`.
            if let Some((spec, kind)) = file.imports.get(head)
                && kind == "*"
            {
                let rel = files_index.resolve(spec, &file.rel)?;
                let dir = rel.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
                return in_file(&rel, name, name).or_else(|| nearest(name, &rel, Some(dir)));
            }
            // `<Nav.Bar>`: a part hung on a component.
            return nearest(name, &file.rel, None);
        }
        if let Some(&i) = file.components.get(name) {
            return Some(i);
        }
        if let Some((spec, exported)) = file.imports.get(name) {
            return match files_index.resolve(spec, &file.rel) {
                Some(rel) => in_file(&rel, exported, name).or_else(|| {
                    // A barrel (`index.ts` re-exporting): the component of
                    // that name under its folder.
                    let dir = rel.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
                    let wanted = if exported == "default" {
                        name
                    } else {
                        exported
                    };
                    nearest(wanted, &rel, Some(dir))
                }),
                // A workspace package that did not resolve to a file: the
                // component of that name in it; any other package is not
                // the project's.
                None => files_index.package_dir(spec).and_then(|dir| {
                    nearest(
                        if exported == "default" {
                            name
                        } else {
                            exported
                        },
                        &file.rel,
                        Some(dir),
                    )
                }),
            };
        }
        // Nuxt's `<LazyTheFooter>` is `TheFooter`, loaded when shown.
        nearest(name, &file.rel, None).or_else(|| {
            name.strip_prefix("Lazy")
                .filter(|rest| capitalised(rest))
                .and_then(|rest| nearest(rest, &file.rel, None))
        })
    };
    // The arrows, props merged across a component's renders of another.
    let mut arrows: BTreeMap<(usize, usize), Vec<String>> = BTreeMap::new();
    for file in &files {
        for &from in file.components.values() {
            for (tag, props) in &found[from].renders {
                let Some(to) = target(file, tag) else {
                    continue;
                };
                if to == from {
                    continue;
                }
                let merged = arrows.entry((from, to)).or_default();
                for p in props {
                    if !merged.contains(p) {
                        merged.push(p.clone());
                    }
                }
            }
        }
    }
    if arrows.is_empty() {
        return None;
    }
    let named: Vec<(&str, &str)> = found
        .iter()
        .map(|c| (c.name.as_str(), c.rel.as_str()))
        .collect();
    let labels = labels(&named);
    let mut map = MapBuilder::new(
        "components",
        "Component tree",
        "each component and an arrow to each of the project's components it renders, labelled with the props it passes",
    );
    let mut nodes = Vec::with_capacity(found.len());
    for (i, c) in found.iter().enumerate() {
        let at = map.node("component", &format!("{}#{}", c.rel, c.name), &labels[i]);
        map.place(at, &c.rel, c.line);
        if !c.props.is_empty() {
            map.detail(at, format!("props: {}", c.props.join(", ")));
        }
        nodes.push(at);
    }
    for ((from, to), props) in &arrows {
        map.edge(nodes[*from], nodes[*to], &props.join(", "));
    }
    map.finish()
}

/// What each of `found` (name, file) shows: its name, and where several
/// share it, the first of the folders their paths do not have in common
/// that tell it from the others: `Card · web` and `Card · admin` for
/// `apps/web/src/components/card.tsx` and `apps/admin/src/components/card.tsx`,
/// `Page · posts` and `Page · users`.
pub(super) fn labels(found: &[(&str, &str)]) -> Vec<String> {
    let mut groups: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, (name, _)) in found.iter().enumerate() {
        groups.entry(name).or_default().push(i);
    }
    let mut out: Vec<String> = found.iter().map(|(name, _)| name.to_string()).collect();
    for members in groups.values().filter(|m| m.len() > 1) {
        let dirs: Vec<Vec<&str>> = members
            .iter()
            .map(|&i| {
                let mut parts: Vec<&str> = found[i].1.split('/').collect();
                parts.pop();
                parts
            })
            .collect();
        let shortest = dirs.iter().map(Vec::len).min().unwrap_or(0);
        let head = (0..shortest)
            .take_while(|&k| dirs.iter().all(|d| d[k] == dirs[0][k]))
            .count();
        let tail = (0..shortest - head)
            .take_while(|&k| {
                let last = dirs[0][dirs[0].len() - 1 - k];
                dirs.iter().all(|d| d[d.len() - 1 - k] == last)
            })
            .count();
        let middles: Vec<&[&str]> = dirs.iter().map(|d| &d[head..d.len() - tail]).collect();
        let deepest = middles.iter().map(|m| m.len()).max().unwrap_or(0);
        // The first folders of what differs, as few as tell this one from
        // the others: `PostCard · expo`, not `PostCard · expo/src/app`.
        let at = |m: &[&str], k: usize| m[..k.min(m.len())].join("/");
        for (n, &i) in members.iter().enumerate() {
            let mine = middles[n];
            let k = (1..=deepest)
                .find(|&k| {
                    let label = at(mine, k);
                    middles
                        .iter()
                        .enumerate()
                        .all(|(o, other)| o == n || at(other, k) != label)
                })
                .unwrap_or(deepest);
            let place = match &mine[..k.min(mine.len())] {
                // Side by side in one folder: the file tells them apart;
                // one folder above the others: its own folder.
                [] => match dirs[n].last() {
                    Some(last) if dirs.iter().filter(|d| **d == dirs[n]).count() == 1 => {
                        last.to_string()
                    }
                    _ => found[i].1.rsplit('/').next().unwrap_or("").to_string(),
                },
                [one] => one.to_string(),
                [first, second] => format!("{first}/{second}"),
                [first, .., last] => format!("{first}/…/{last}"),
            };
            out[i] = format!("{} · {place}", found[i].0);
        }
    }
    out
}

/// A JavaScript or TypeScript file's components: the functions and classes
/// that render JSX, and what each renders.
fn script_file(source: &Source, found: &mut Vec<Component>) -> Option<File> {
    // JSX closes its elements: a file with neither `</` nor `/>` has none
    // (and is not parsed for it).
    if !(source.text.contains("</") || source.text.contains("/>")) {
        return None;
    }
    let (_, src, tree) = source.script()?;
    let root = tree.root_node();
    let elements: Vec<Node> = all_of(root, "jsx_opening_element")
        .into_iter()
        .chain(all_of(root, "jsx_self_closing_element"))
        .collect();
    if elements.is_empty() {
        return None;
    }
    let mut file = File {
        rel: source.rel.clone(),
        imports: imports(root, src),
        components: HashMap::new(),
        default: default_export(root, src),
    };
    for element in elements {
        let Some((name, place)) = enclosing(element, src, &source.rel, false) else {
            continue;
        };
        let at = *file.components.entry(name.clone()).or_insert_with(|| {
            found.push(Component {
                props: react_props(place, src, root),
                name,
                rel: source.rel.clone(),
                line: line(place),
                renders: Vec::new(),
                alias: None,
            });
            found.len() - 1
        });
        let Some(tag) = element.child_by_field_name("name").map(|n| text(n, src)) else {
            continue;
        };
        let last = tag.rsplit('.').next().unwrap_or(tag);
        if !capitalised(last) {
            continue;
        }
        let props: Vec<String> = element
            .named_children(&mut element.walk())
            .filter(|a| a.kind() == "jsx_attribute")
            .filter_map(|a| a.named_child(0).map(|n| text(n, src).to_string()))
            .collect();
        found[at].renders.push((tag.to_string(), props));
    }
    Some(file)
}

/// The name a file's `export default X` gives (`export default memo(X)`
/// too).
fn default_export(root: Node<'_>, src: &str) -> Option<String> {
    root.named_children(&mut root.walk())
        .filter(|n| n.kind() == "export_statement" && text(*n, src).starts_with("export default"))
        .find_map(|n| {
            if let Some(declaration) = n.child_by_field_name("declaration") {
                return declaration
                    .child_by_field_name("name")
                    .map(|x| text(x, src).to_string());
            }
            let value = n.child_by_field_name("value")?;
            let mut at = value;
            loop {
                match at.kind() {
                    "identifier" => return Some(text(at, src).to_string()),
                    "call_expression" => {
                        at = at.child_by_field_name("arguments")?.named_child(0)?;
                    }
                    "parenthesized_expression" | "as_expression" => at = at.named_child(0)?,
                    _ => return None,
                }
            }
        })
}

/// A React component's props: its first parameter's destructured names, or
/// the members of the interface or type its first parameter is annotated
/// with, when the file declares it.
fn react_props(place: Node<'_>, src: &str, root: Node<'_>) -> Vec<String> {
    let held = match place.kind() {
        "variable_declarator" => place.child_by_field_name("value"),
        "export_statement" => place
            .child_by_field_name("declaration")
            .or_else(|| place.child_by_field_name("value")),
        _ => Some(place),
    };
    // Through `memo(…)` and `forwardRef(…)` to the function.
    let mut function = held;
    while let Some(n) = function {
        match n.kind() {
            "arrow_function" | "function_expression" | "function" | "function_declaration" => {
                break;
            }
            "call_expression" => {
                function = n
                    .child_by_field_name("arguments")
                    .and_then(|a| a.named_child(0));
            }
            "parenthesized_expression" | "as_expression" => function = n.named_child(0),
            _ => function = None,
        }
    }
    let Some(function) = function else {
        return Vec::new();
    };
    let first = function
        .child_by_field_name("parameters")
        .and_then(|p| p.named_child(0))
        .or_else(|| function.child_by_field_name("parameter"));
    let Some(first) = first else {
        return Vec::new();
    };
    // `({ title, done = false, ...rest }: Props)`
    let pattern = match first.kind() {
        "required_parameter" | "optional_parameter" => first.child_by_field_name("pattern"),
        _ => Some(first),
    };
    if let Some(pattern) = pattern.filter(|p| p.kind() == "object_pattern") {
        return pattern_names(pattern, src);
    }
    // `(props: Props)`: the type, when the file declares it.
    let Some(written) = first
        .child_by_field_name("type")
        .and_then(|t| t.named_child(0))
        .map(|t| text(t, src))
    else {
        return Vec::new();
    };
    type_members(root, src, written)
}

/// The names an object pattern takes: `{ a, b: c, d = 1, ...rest }` → a, b, d.
pub(super) fn pattern_names(pattern: Node<'_>, src: &str) -> Vec<String> {
    let mut out = Vec::new();
    for part in pattern.named_children(&mut pattern.walk()) {
        let name = match part.kind() {
            "shorthand_property_identifier_pattern" | "shorthand_property_identifier" => {
                Some(text(part, src))
            }
            "pair_pattern" => part.child_by_field_name("key").map(|k| text(k, src)),
            "object_assignment_pattern" => part.child_by_field_name("left").map(|k| text(k, src)),
            _ => None,
        };
        if let Some(name) = name {
            out.push(name.trim_matches(['"', '\'']).to_string());
        }
    }
    out
}

/// The members of the interface or object type `name` the file declares.
fn type_members(root: Node<'_>, src: &str, name: &str) -> Vec<String> {
    let name = name.split('<').next().unwrap_or(name).trim();
    for kind in ["interface_declaration", "type_alias_declaration"] {
        for declaration in all_of(root, kind) {
            if declaration
                .child_by_field_name("name")
                .map(|n| text(n, src))
                != Some(name)
            {
                continue;
            }
            let body = declaration
                .child_by_field_name("body")
                .or_else(|| declaration.child_by_field_name("value"));
            let Some(body) = body.filter(|b| matches!(b.kind(), "interface_body" | "object_type"))
            else {
                continue;
            };
            return body
                .named_children(&mut body.walk())
                .filter(|m| m.kind() == "property_signature")
                .filter_map(|m| m.child_by_field_name("name"))
                .map(|n| text(n, src).trim_matches(['"', '\'']).to_string())
                .collect();
        }
    }
    Vec::new()
}

/// A Vue, Svelte or Astro file's component: the file is it, named for the
/// file; what its template renders and the props its script declares.
fn component_file(source: &Source, lang: Lang, found: &mut Vec<Component>) -> Option<File> {
    let name = file_component_name(&source.rel);
    let markup = markup(&source.text, lang);
    let mut renders = Vec::new();
    for tag in markup.split('<').skip(1) {
        let raw = super::super::ident_dash(tag);
        // `<svelte:self>`, `<Foo.Bar>` (a namespace's part)
        let raw = match tag[raw.len()..].starts_with('.') {
            true => {
                let rest = &tag[raw.len() + 1..];
                let part = super::super::ident_dash(rest);
                &tag[..raw.len() + 1 + part.len()]
            }
            false => raw,
        };
        if raw.is_empty() {
            continue;
        }
        let tag_name = match lang {
            // Vue writes `<todo-list>` for `TodoList`.
            Lang::Vue if raw.contains('-') => pascal(raw),
            _ => raw.to_string(),
        };
        let last = tag_name.rsplit('.').next().unwrap_or(&tag_name);
        if !capitalised(last) {
            continue;
        }
        let head = tag[raw.len()..].split('>').next().unwrap_or("");
        let props: Vec<String> = head
            .split_whitespace()
            .filter_map(|a| a.split('=').next())
            .map(|a| {
                a.trim_start_matches("v-bind:")
                    .trim_start_matches("v-on:")
                    .trim_start_matches("bind:")
                    .trim_start_matches("on:")
                    .trim_start_matches([':', '@', '{', '.'])
                    .trim_end_matches(['}', '/'])
                    .to_string()
            })
            .filter(|a| {
                !a.is_empty()
                    && a.chars().next().is_some_and(|c| c.is_alphabetic())
                    && a.chars().all(|c| c.is_alphanumeric() || matches!(c, '-' | '_'))
                    // What the template does with the element, not a prop
                    // it passes: `v-if`, `v-for`, `class`, `key`.
                    && !(a.starts_with("v-") && a != "v-model")
                    && !matches!(a.as_str(), "class" | "style" | "key" | "ref" | "slot" | "is")
            })
            .collect();
        renders.push((tag_name, props));
    }
    let (imports, props) = match source.script() {
        Some((_, src, tree)) => {
            let root = tree.root_node();
            let props = match lang {
                Lang::Vue => vue_props(src),
                Lang::Svelte => svelte_props(root, src),
                _ => astro_props(root, src),
            };
            (imports(root, src), props)
        }
        None => (HashMap::new(), Vec::new()),
    };
    found.push(Component {
        alias: nuxt_name(&source.rel, &name),
        name: name.clone(),
        rel: source.rel.clone(),
        line: 1,
        renders,
        props,
    });
    Some(File {
        rel: source.rel.clone(),
        imports,
        components: HashMap::from([(name.clone(), found.len() - 1)]),
        default: Some(name),
    })
}

/// A component file's markup: a Vue file's `<template>`, a Svelte file
/// without its scripts and styles, an Astro file after its frontmatter
/// without them.
fn markup(text: &str, lang: Lang) -> String {
    if lang == Lang::Vue {
        return text
            .split_once("<template")
            .and_then(|(_, rest)| rest.rsplit_once("</template>"))
            .map(|(t, _)| t.to_string())
            .unwrap_or_default();
    }
    let mut body = text;
    if lang == Lang::Astro
        && let Some(rest) = body.trim_start().strip_prefix("---")
        && let Some((_, after)) = rest.split_once("\n---")
    {
        body = after;
    }
    let mut out = String::new();
    let mut rest = body;
    loop {
        let open = ["<script", "<style"]
            .iter()
            .filter_map(|t| rest.find(t).map(|at| (at, *t)))
            .min();
        let Some((at, tag)) = open else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..at]);
        let close = format!("</{}>", &tag[1..]);
        match rest[at..].find(&close) {
            Some(end) => rest = &rest[at + end + close.len()..],
            None => break,
        }
    }
    out
}

/// Nuxt's name for a component under `components/`: its folders and name
/// joined — `components/base/Button.vue` is `BaseButton` (and
/// `components/base/BaseButton.vue` too, the repeated prefix said once).
fn nuxt_name(rel: &str, name: &str) -> Option<String> {
    let (before, after) = rel.split_once("components/")?;
    if !(before.is_empty() || before.ends_with('/')) {
        return None;
    }
    let dirs: Vec<&str> = after.split('/').collect();
    let dirs = &dirs[..dirs.len().saturating_sub(1)];
    if dirs.is_empty() {
        return None;
    }
    let prefix: String = dirs.iter().map(|d| pascal(d)).collect();
    let alias = match name.to_lowercase().starts_with(&prefix.to_lowercase()) {
        true => name.to_string(),
        false => format!("{prefix}{name}"),
    };
    (alias != name).then_some(alias)
}

/// A Vue component's declared props: `defineProps(['a'])`,
/// `defineProps({ a: String })`, `defineProps<{ a: string }>()`, or an
/// options object's `props:`.
pub(super) fn vue_props(text: &str) -> Vec<String> {
    let at = text.find("defineProps").or_else(|| text.find("props:"));
    let Some(at) = at else {
        return Vec::new();
    };
    let rest = &text[at..];
    let open = rest.find(['[', '{', '<']).unwrap_or(rest.len());
    let close = match rest.as_bytes().get(open) {
        Some(b'[') => ']',
        Some(b'<') => '>',
        _ => '}',
    };
    let body = rest.get(open + 1..).unwrap_or("");
    let mut depth = 0;
    let mut end = body.len();
    for (i, c) in body.char_indices() {
        match c {
            '{' | '[' | '(' => depth += 1,
            '}' | ']' | ')' if depth > 0 => depth -= 1,
            c if c == close && depth == 0 => {
                end = i;
                break;
            }
            _ => {}
        }
    }
    let mut body = body[..end].trim();
    // `defineProps<{ … }>()`: the type's braces, one level in.
    if let Some(inner) = body.strip_prefix('{').and_then(|b| b.strip_suffix('}')) {
        body = inner;
    }
    let mut out = Vec::new();
    let mut depth = 0;
    for part in body.split_inclusive([',', ';', '\n']) {
        let trimmed = part.trim().trim_end_matches([',', ';']);
        if depth == 0 {
            let name = trimmed
                .trim_matches(['\'', '"'])
                .split([':', '?'])
                .next()
                .unwrap_or("")
                .trim();
            if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_') {
                out.push(name.to_string());
            }
        }
        depth += part.matches(['{', '[', '(']).count() as i32
            - part.matches(['}', ']', ')']).count() as i32;
        depth = depth.max(0);
    }
    out
}

/// A Svelte component's props: Svelte 5's `let { a, b } = $props()`, or
/// Svelte 4's `export let a`.
fn svelte_props(root: Node<'_>, src: &str) -> Vec<String> {
    let mut out = Vec::new();
    for declarator in all_of(root, "variable_declarator") {
        let value = declarator.child_by_field_name("value");
        let name = declarator.child_by_field_name("name");
        if let (Some(value), Some(name)) = (value, name)
            && value.kind() == "call_expression"
            && value.child_by_field_name("function").map(|f| text(f, src)) == Some("$props")
            && name.kind() == "object_pattern"
        {
            out.extend(pattern_names(name, src));
        }
        if ancestor(declarator, &["export_statement"]).is_some()
            && let Some(name) = name.filter(|n| n.kind() == "identifier")
            && ancestor(declarator, &["lexical_declaration", "variable_declaration"])
                .is_some_and(|d| text(d, src).starts_with("let") || text(d, src).starts_with("var"))
        {
            out.push(text(name, src).to_string());
        }
    }
    out
}

/// An Astro component's props: `const { a, b } = Astro.props`, else the
/// members of its `interface Props`.
fn astro_props(root: Node<'_>, src: &str) -> Vec<String> {
    for declarator in all_of(root, "variable_declarator") {
        if let (Some(value), Some(name)) = (
            declarator.child_by_field_name("value"),
            declarator.child_by_field_name("name"),
        ) && text(value, src) == "Astro.props"
            && name.kind() == "object_pattern"
        {
            return pattern_names(name, src);
        }
    }
    type_members(root, src, "Props")
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
    fn react_and_vue_components_and_what_they_render() {
        let tree = map(&Corpus::of(&[
            (
                "src/App.tsx",
                "import { TodoList } from './TodoList';\nexport function App() {\n  return <Layout title=\"x\"><TodoList items={items} onDone={done} /><div /></Layout>;\n}\n\nconst Layout = ({ children }) => <main>{children}<Nav.Bar /></main>;\n\nclass Nav extends React.Component {\n  render() { return <ul><Bar /></ul>; }\n}\n",
            ),
            (
                "src/TodoList.tsx",
                "export const TodoList = (p) => <ol>{p.items.map(i => <Todo key={i.id} item={i} />)}</ol>;\nfunction Todo() { return <li />; }\nfunction Bar() { return <b />; }\n",
            ),
            (
                "web/Shell.vue",
                "<template>\n  <task-list :items=\"items\" @done=\"finish\" />\n  <Footer/>\n</template>\n<script setup>\nimport TaskList from './TaskList.vue'\n</script>\n",
            ),
            (
                "web/TaskList.vue",
                "<template><ul/></template>\n<script setup>\nconst props = defineProps({ items: Array, max: { type: Number, default: 3 } })\n</script>\n",
            ),
            ("web/Footer.vue", "<template><p/></template>\n"),
        ]))
        .unwrap();
        let got = edges(&tree);
        for want in [
            e("App", "Layout", "title"),
            e("App", "TodoList", "items, onDone"),
            e("TodoList", "Todo", "key, item"),
            e("Nav", "Bar", ""),
            e("Layout", "Bar", ""),
            e("Shell", "TaskList", "items, done"),
            e("Shell", "Footer", ""),
        ] {
            assert!(got.contains(&want), "{want:?} in {got:?}");
        }
        let list = tree.nodes.iter().find(|n| n.label == "TaskList").unwrap();
        assert_eq!(
            (list.file.as_str(), list.detail.as_str()),
            ("web/TaskList.vue", "props: items, max")
        );
        let app = tree.nodes.iter().find(|n| n.label == "App").unwrap();
        assert_eq!((app.file.as_str(), app.line), ("src/App.tsx", 2));
        let layout = tree.nodes.iter().find(|n| n.label == "Layout").unwrap();
        assert_eq!(layout.detail, "props: children");
        assert_eq!(
            vue_props("defineProps<{ a: string; b?: number }>()"),
            ["a", "b"]
        );
        assert_eq!(vue_props("defineProps(['x', 'y'])"), ["x", "y"]);
        assert_eq!(
            vue_props("defineProps({ items: Array, max: { type: Number, default: 3 } })"),
            ["items", "max"]
        );
    }

    /// Components of one name in several files are each their own, and a
    /// tag is the one its file imports — not whichever was met first.
    #[test]
    fn components_are_known_by_file_and_found_through_imports() {
        let tree = map(&Corpus::of(&[
            (
                "apps/web/src/app/posts/[id]/page.tsx",
                "import { Card } from '@/components/card';\nexport default function Page() { return <Card title=\"post\" />; }\n",
            ),
            (
                "apps/web/src/app/users/page.tsx",
                "import Card from '../../components/card';\nexport default function Page() { return <Card wide />; }\n",
            ),
            (
                "apps/web/src/components/card.tsx",
                "export function Card({ title, wide }: { title?: string; wide?: boolean }) { return <div />; }\nexport default Card;\n",
            ),
            (
                "apps/admin/src/components/card.tsx",
                "export function Card() { return <section />; }\n",
            ),
            (
                "apps/admin/src/app/page.tsx",
                "import { Card } from '@/components/card';\nimport { Button } from '@mui/material';\nexport default function Home() { return <><Card /><Button /></>; }\n",
            ),
            (
                "packages/ui/package.json",
                "{ \"name\": \"@acme/ui\" }",
            ),
            (
                "packages/ui/src/button.tsx",
                "export const Button = React.forwardRef<HTMLButtonElement, Props>(({ variant, size }, ref) => <button ref={ref} />);\nexport const Icon = memo(function Icon() { return <svg />; });\n",
            ),
            (
                "apps/web/src/app/layout.tsx",
                "import { Button } from '@acme/ui/button';\nimport * as UI from '@acme/ui/button';\nexport default function ({ children }) { return <body><Button variant=\"x\" /><UI.Icon /></body>; }\n",
            ),
            (
                "apps/web/src/app/card.test.tsx",
                "function Fixture() { return <Card />; }\n",
            ),
        ]))
        .unwrap();
        let got = edges(&tree);
        for want in [
            e("Page · posts", "Card · web", "title"),
            e("Page · users", "Card · web", "wide"),
            e("Home", "Card · admin", ""),
            e("Layout", "Button", "variant"),
            e("Layout", "Icon", ""),
        ] {
            assert!(got.contains(&want), "{want:?} in {got:?}");
        }
        assert_eq!(got.len(), 5, "{got:?}");
        let button = tree.nodes.iter().find(|n| n.label == "Button").unwrap();
        assert_eq!(button.detail, "props: variant, size");
        let card = tree
            .nodes
            .iter()
            .find(|n| n.file == "apps/web/src/components/card.tsx")
            .unwrap();
        assert_eq!(card.detail, "props: title, wide");
        assert!(
            !tree.nodes.iter().any(|n| n.label.contains("Fixture")),
            "a test's components are not the app's"
        );
    }

    /// A tsconfig's `paths` (its `extends` followed, comments allowed) and
    /// `baseUrl` are where an aliased import is.
    #[test]
    fn tsconfig_aliases_name_the_components() {
        let tree = map(&Corpus::of(&[
            (
                "site/tsconfig.json",
                "{\n  // the site's own\n  \"extends\": \"./tsconfig.base.json\",\n}\n",
            ),
            (
                "site/tsconfig.base.json",
                "{ \"compilerOptions\": { \"paths\": { \"@shortcodes/*\": [\"./src/components/shortcodes/*\"], \"@layouts/*\": [\"./src/layouts/*\"] } } }",
            ),
            (
                "site/src/pages/index.astro",
                "---\nimport Callout from '@shortcodes/Callout.astro'\nimport Base from '@layouts/Base.astro'\n---\n<Base><Callout type=\"info\" /></Base>\n",
            ),
            (
                "site/src/components/shortcodes/Callout.astro",
                "<aside />\n",
            ),
            ("site/src/layouts/Base.astro", "<html><slot /></html>\n"),
            (
                "web/jsconfig.json",
                "{ \"compilerOptions\": { \"baseUrl\": \"src\" } }",
            ),
            (
                "web/src/App.jsx",
                "import Nav from 'components/Nav';\nexport default function App() { return <Nav open />; }\n",
            ),
            (
                "web/src/components/Nav.jsx",
                "export default function Nav() { return <nav />; }\n",
            ),
        ]))
        .unwrap();
        let got = edges(&tree);
        for want in [
            e("Index", "Callout", "type"),
            e("Index", "Base", ""),
            e("App", "Nav", "open"),
        ] {
            assert!(got.contains(&want), "{want:?} in {got:?}");
        }
    }

    /// Svelte and Astro components, Nuxt's auto-imported names and Vue's
    /// `defineComponent`.
    #[test]
    fn svelte_astro_and_auto_imported_components() {
        let tree = map(&Corpus::of(&[
            (
                "src/routes/+page.svelte",
                "<script lang=\"ts\">\n  import Counter from '$lib/Counter.svelte';\n  let { data } = $props();\n</script>\n\n<Counter start={1} bind:value on:change={go} />\n<style>.x { color: red }</style>\n",
            ),
            (
                "src/lib/Counter.svelte",
                "<script>\n  export let start = 0;\n  export let value;\n</script>\n<button>{value}</button>\n",
            ),
            (
                "site/src/pages/index.astro",
                "---\nimport Hero from '../components/Hero.astro';\nimport Counter from '../components/Counter.tsx';\n---\n<Hero title=\"Hi\" /><Counter client:load />\n",
            ),
            (
                "site/src/components/Hero.astro",
                "---\ninterface Props { title: string; tagline?: string }\nconst { title } = Astro.props;\n---\n<h1>{title}</h1>\n",
            ),
            (
                "site/src/components/Counter.tsx",
                "export default function Counter() { return <button />; }\n",
            ),
            (
                "nuxt/pages/index.vue",
                "<template><BaseButton size=\"lg\" /><LazyTheFooter /></template>\n",
            ),
            ("nuxt/components/base/Button.vue", "<template><button/></template>\n"),
            (
                "nuxt/components/TheFooter.vue",
                "<template><footer/></template>\n",
            ),
        ]))
        .unwrap();
        let got = edges(&tree);
        for want in [
            e("Page", "Counter · src", "start, value, change"),
            e("Index · site", "Hero", "title"),
            e("Index · site", "Counter · site", ""),
            e("Index · nuxt", "Button", "size"),
            e("Index · nuxt", "TheFooter", ""),
        ] {
            assert!(got.contains(&want), "{want:?} in {got:?}");
        }
        let counter = tree
            .nodes
            .iter()
            .find(|n| n.file == "src/lib/Counter.svelte")
            .unwrap();
        assert_eq!(counter.detail, "props: start, value");
        let page = tree.nodes.iter().find(|n| n.label == "Page").unwrap();
        assert_eq!(page.detail, "props: data");
        let hero = tree.nodes.iter().find(|n| n.label == "Hero").unwrap();
        assert_eq!(hero.detail, "props: title");
    }
}
