//! JavaScript and TypeScript's maps:
//!
//! - **component tree** — each React component (a capitalised function or
//!   class that returns JSX) and the project components it renders, the
//!   props passed on each arrow; Vue single-file components and the ones
//!   their templates use, their declared props in the detail;
//! - **workspace packages** — a monorepo's packages (npm/yarn `workspaces`,
//!   `pnpm-workspace.yaml`), the ones each depends on, what each exports.

use std::collections::{BTreeMap, BTreeSet};

use clew_protocol::ProjectMap;
use tree_sitter::Node;

use super::{Corpus, MapBuilder, all_of, ancestor, line, text};
use crate::highlight::Lang;

pub fn maps(corpus: &Corpus) -> Vec<ProjectMap> {
    let mut out = Vec::new();
    out.extend(components(corpus));
    out.extend(workspaces(corpus));
    out
}

fn capitalised(name: &str) -> bool {
    name.chars().next().is_some_and(|c| c.is_ascii_uppercase())
}

/// The component a JSX element sits in: the capitalised function, arrow
/// function's variable, or class around it.
fn component_of<'t>(node: Node<'t>, src: &str) -> Option<(String, Node<'t>)> {
    let mut at = node.parent();
    while let Some(n) = at {
        let name = match n.kind() {
            "function_declaration" | "class_declaration" | "function_expression" => n
                .child_by_field_name("name")
                .map(|x| text(x, src).to_string()),
            "arrow_function" => n
                .parent()
                .filter(|p| p.kind() == "variable_declarator")
                .and_then(|p| p.child_by_field_name("name"))
                .map(|x| text(x, src).to_string()),
            _ => None,
        };
        if let Some(name) = name.filter(|n| capitalised(n)) {
            let place = match n.kind() {
                "arrow_function" => n.parent().unwrap_or(n),
                _ => n,
            };
            return Some((name, place));
        }
        at = n.parent();
    }
    None
}

fn components(corpus: &Corpus) -> Option<ProjectMap> {
    // What a component renders: `(component, its place) → [(tag, props, line)]`.
    struct Component {
        rel: String,
        line: usize,
        renders: Vec<(String, Vec<String>)>,
        props: Vec<String>,
    }
    let mut found: BTreeMap<String, Component> = BTreeMap::new();
    for source in corpus.in_langs(&[Lang::JavaScript, Lang::TypeScript, Lang::Tsx]) {
        let Some(tree) = source.tree() else {
            continue;
        };
        let src = source.text.as_str();
        let root = tree.root_node();
        let elements = all_of(root, "jsx_opening_element")
            .into_iter()
            .chain(all_of(root, "jsx_self_closing_element"));
        for element in elements {
            let Some((component, place)) = component_of(element, src) else {
                continue;
            };
            let entry = found.entry(component).or_insert(Component {
                rel: source.rel.clone(),
                line: line(place),
                renders: Vec::new(),
                props: Vec::new(),
            });
            let Some(name) = element.child_by_field_name("name") else {
                continue;
            };
            let tag = text(name, src);
            let tag = tag.rsplit('.').next().unwrap_or(tag).to_string();
            if !capitalised(&tag) {
                continue;
            }
            let props: Vec<String> = all_of(element, "jsx_attribute")
                .into_iter()
                .filter(|a| {
                    ancestor(*a, &["jsx_opening_element", "jsx_self_closing_element"])
                        == Some(element)
                })
                .filter_map(|a| a.named_child(0).map(|n| text(n, src).to_string()))
                .collect();
            entry.renders.push((tag, props));
        }
    }
    // Vue: `App.vue` uses `<TodoList>` or `<todo-list>`.
    let vue: Vec<&super::Source> = corpus
        .files
        .iter()
        .filter(|s| s.rel.ends_with(".vue"))
        .collect();
    let vue_names: BTreeSet<String> = vue
        .iter()
        .map(|s| s.name().trim_end_matches(".vue").to_string())
        .collect();
    for source in &vue {
        let name = source.name().trim_end_matches(".vue").to_string();
        let template = source
            .text
            .split_once("<template")
            .and_then(|(_, rest)| rest.rsplit_once("</template>"))
            .map(|(t, _)| t)
            .unwrap_or("");
        let mut renders = Vec::new();
        for tag in template.split('<').skip(1) {
            let raw = super::ident_dash(tag);
            if raw.is_empty() {
                continue;
            }
            let pascal = match raw.contains('-') {
                true => raw
                    .split('-')
                    .map(|w| {
                        let mut c = w.chars();
                        c.next()
                            .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                            .unwrap_or_default()
                    })
                    .collect::<String>(),
                false => raw.to_string(),
            };
            if vue_names.contains(&pascal) && pascal != name {
                let props: Vec<String> = tag
                    .split('>')
                    .next()
                    .unwrap_or("")
                    .split_whitespace()
                    .skip(1)
                    .filter_map(|a| a.split('=').next())
                    .map(|a| {
                        a.trim_start_matches([':', '@'])
                            .trim_start_matches("v-bind:")
                            .to_string()
                    })
                    .filter(|a| !a.is_empty() && !a.starts_with('/'))
                    .collect();
                renders.push((pascal, props));
            }
        }
        // A name met in JSX too is the same component: what each renders
        // is kept.
        let entry = found.entry(name).or_insert(Component {
            rel: source.rel.clone(),
            line: 1,
            renders: Vec::new(),
            props: Vec::new(),
        });
        entry.renders.extend(renders);
        entry.props.extend(vue_props(&source.text));
    }
    if !found.values().any(|c| !c.renders.is_empty()) {
        return None;
    }
    let mut map = MapBuilder::new(
        "components",
        "Component tree",
        "each component and an arrow to each of the project's components it renders, labelled with the props it passes",
    );
    for (name, c) in &found {
        let at = map.node("component", name, name);
        map.place(at, &c.rel, c.line);
        if !c.props.is_empty() {
            map.detail(at, format!("props: {}", c.props.join(", ")));
        }
    }
    for (name, c) in &found {
        let from = map.node("component", name, name);
        for (tag, props) in &c.renders {
            if !found.contains_key(tag) {
                continue;
            }
            let to = map.node("component", tag, tag);
            map.edge(from, to, &props.join(", "));
        }
    }
    map.finish()
}

/// A Vue component's declared props: `defineProps(['a'])`,
/// `defineProps({ a: String })`, `defineProps<{ a: string }>()`, or an
/// options object's `props:`.
fn vue_props(text: &str) -> Vec<String> {
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

// -- workspaces -----------------------------------------------------------------------------

/// Whether `dir` (a package's directory) is one `pattern` names: `x`,
/// `x/*` (its direct subdirectories), `x/**` (any below).
fn matches_glob(pattern: &str, dir: &str) -> bool {
    let pattern = pattern
        .trim()
        .trim_start_matches("./")
        .trim_end_matches('/');
    if let Some(base) = pattern.strip_suffix("/**") {
        return dir.starts_with(&format!("{base}/"));
    }
    if let Some(base) = pattern.strip_suffix("/*") {
        return dir
            .strip_prefix(&format!("{base}/"))
            .is_some_and(|rest| !rest.is_empty() && !rest.contains('/'));
    }
    pattern == dir
}

fn workspaces(corpus: &Corpus) -> Option<ProjectMap> {
    let mut patterns: Vec<String> = Vec::new();
    if let Some(root) = corpus.get("package.json")
        && let Ok(json) = serde_json::from_str::<serde_json::Value>(&root.text)
    {
        let list = json["workspaces"]
            .as_array()
            .or_else(|| json["workspaces"]["packages"].as_array())
            .cloned()
            .unwrap_or_default();
        patterns.extend(list.iter().filter_map(|v| v.as_str().map(str::to_string)));
    }
    if let Some(pnpm) = corpus.get("pnpm-workspace.yaml") {
        let mut in_packages = false;
        for line in pnpm.text.lines() {
            let t = line.trim();
            if t.starts_with("packages:") {
                in_packages = true;
                continue;
            }
            if in_packages {
                match t.strip_prefix('-') {
                    Some(p) => patterns.push(p.trim().trim_matches(['\'', '"']).to_string()),
                    None if !t.is_empty() && !line.starts_with(' ') => in_packages = false,
                    None => {}
                }
            }
        }
    }
    if patterns.is_empty() {
        return None;
    }
    struct Package {
        rel: String,
        name: String,
        deps: Vec<(String, &'static str)>,
        detail: Vec<String>,
    }
    let mut packages: Vec<Package> = Vec::new();
    for source in corpus
        .files
        .iter()
        .filter(|s| s.name() == "package.json" && s.rel != "package.json")
    {
        let dir = source
            .rel
            .trim_end_matches("package.json")
            .trim_end_matches('/');
        if dir.contains("node_modules")
            || !patterns
                .iter()
                .any(|p| !p.starts_with('!') && matches_glob(p, dir))
        {
            continue;
        }
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&source.text) else {
            continue;
        };
        let name = json["name"].as_str().unwrap_or(dir).to_string();
        let mut deps = Vec::new();
        for (field, label) in [
            ("dependencies", "depends on"),
            ("devDependencies", "dev depends on"),
            ("peerDependencies", "peer depends on"),
        ] {
            if let Some(object) = json[field].as_object() {
                deps.extend(object.keys().map(|k| (k.clone(), label)));
            }
        }
        let mut detail = Vec::new();
        if let Some(v) = json["version"].as_str() {
            detail.push(format!("version {v}"));
        }
        if let Some(main) = json["main"].as_str() {
            detail.push(format!("main: {main}"));
        }
        match &json["exports"] {
            serde_json::Value::String(e) => detail.push(format!("exports {e}")),
            serde_json::Value::Object(o) => detail.push(format!(
                "exports {}",
                o.keys().cloned().collect::<Vec<_>>().join(", ")
            )),
            _ => {}
        }
        packages.push(Package {
            rel: source.rel.clone(),
            name,
            deps,
            detail,
        });
    }
    if packages.is_empty() {
        return None;
    }
    let mut map = MapBuilder::new(
        "workspaces",
        "Workspace packages",
        "each package of the monorepo (its version and exports in the detail) and an arrow to each of the others it depends on",
    );
    let names: BTreeSet<&str> = packages.iter().map(|p| p.name.as_str()).collect();
    for p in &packages {
        let at = map.node("package", &p.name, &p.name);
        map.place(at, &p.rel, 1);
        map.detail(at, p.detail.join("\n"));
    }
    for p in &packages {
        let from = map.node("package", &p.name, &p.name);
        for (dep, label) in &p.deps {
            if names.contains(dep.as_str()) {
                let to = map.node("package", dep, dep);
                map.edge(from, to, label);
            }
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
    fn react_and_vue_components_and_what_they_render() {
        let maps = maps(&Corpus::of(&[
            (
                "src/App.tsx",
                "export function App() {\n  return <Layout title=\"x\"><TodoList items={items} onDone={done} /><div /></Layout>;\n}\n\nconst Layout = ({ children }) => <main>{children}<Nav.Bar /></main>;\n\nclass Nav extends React.Component {\n  render() { return <ul><Bar /></ul>; }\n}\n",
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
        ]));
        let tree = maps.iter().find(|m| m.id == "components").unwrap();
        let got = edges(tree);
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
        assert_eq!((app.file.as_str(), app.line), ("src/App.tsx", 1));
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

    #[test]
    fn workspace_packages_and_their_dependencies() {
        let maps = maps(&Corpus::of(&[
            (
                "package.json",
                "{ \"private\": true, \"workspaces\": [\"packages/*\", \"apps/web\"] }",
            ),
            (
                "packages/core/package.json",
                "{ \"name\": \"@acme/core\", \"version\": \"1.2.0\", \"exports\": { \".\": \"./index.js\", \"./util\": \"./util.js\" } }",
            ),
            (
                "packages/ui/package.json",
                "{ \"name\": \"@acme/ui\", \"dependencies\": { \"@acme/core\": \"*\", \"react\": \"^18\" } }",
            ),
            (
                "apps/web/package.json",
                "{ \"name\": \"web\", \"dependencies\": { \"@acme/ui\": \"*\" }, \"devDependencies\": { \"@acme/core\": \"*\" } }",
            ),
            (
                "apps/other/package.json",
                "{ \"name\": \"other\", \"dependencies\": { \"@acme/ui\": \"*\" } }",
            ),
            (
                "packages/core/node_modules/x/package.json",
                "{ \"name\": \"x\" }",
            ),
        ]));
        let ws = maps.iter().find(|m| m.id == "workspaces").unwrap();
        let got = edges(ws);
        assert_eq!(
            got,
            [
                e("@acme/ui", "@acme/core", "depends on"),
                e("web", "@acme/core", "dev depends on"),
                e("web", "@acme/ui", "depends on"),
            ]
        );
        assert!(
            !ws.nodes.iter().any(|n| n.label == "other"),
            "not a workspace"
        );
        let core = ws.nodes.iter().find(|n| n.label == "@acme/core").unwrap();
        assert_eq!(core.detail, "version 1.2.0\nexports ., ./util");
        let pnpm = maps_of_pnpm();
        assert_eq!(pnpm.nodes.len(), 2);
        assert!(matches_glob("packages/**", "packages/a/b"));
        assert!(!matches_glob("packages/*", "packages/a/b"));
    }

    fn maps_of_pnpm() -> ProjectMap {
        let maps = maps(&Corpus::of(&[
            (
                "pnpm-workspace.yaml",
                "packages:\n  - 'libs/*'\n  - \"tools\"\n",
            ),
            ("libs/a/package.json", "{ \"name\": \"a\" }"),
            (
                "tools/package.json",
                "{ \"name\": \"tools\", \"dependencies\": { \"a\": \"workspace:*\" } }",
            ),
        ]));
        maps.into_iter().find(|m| m.id == "workspaces").unwrap()
    }
}
