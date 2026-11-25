//! JavaScript and TypeScript's maps:
//!
//! - **component tree** — React, Vue, Svelte and Astro components, each
//!   known by its file, and the project's components each renders, the
//!   props passed on each arrow (see `js_components.rs`);
//! - **state** — each store (Pinia, Redux Toolkit, Zustand, Vuex, Svelte,
//!   Jotai, nanostores, MobX, NgRx, contexts), what it holds and does, and
//!   who uses it (see `js_stores.rs`);
//! - **workspace packages** — a monorepo's packages (npm/yarn `workspaces`,
//!   `pnpm-workspace.yaml`), the ones each depends on, what each exports.

use std::collections::BTreeSet;

use clew_protocol::ProjectMap;

use super::{Corpus, MapBuilder};

#[path = "js_components.rs"]
mod components;
#[path = "js_stores.rs"]
mod stores;

pub fn maps(corpus: &Corpus) -> Vec<ProjectMap> {
    let mut out = Vec::new();
    out.extend(components::map(corpus));
    out.extend(stores::map(corpus));
    out.extend(workspaces(corpus));
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
