//! The project's architecture, as the overview draws it: the few parts it
//! is made of — its packages (a workspace's crates, a monorepo's apps and
//! libraries), or, in a project of one package, the areas its source is
//! divided into — each with what it is (an app, a library, its UI, its
//! data…), what it says it is for, what it holds, the libraries it is built
//! on; and which of them uses which, through how many imports.
//!
//! The file graph the overview drew before named a dozen files and the
//! imports between them: a knot that says nothing of how the project is
//! put together. This is read off the same facts — the files, the import
//! graph, the manifests, the doc comments and the AI's folder summaries —
//! one level up.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use crate::tr_noop;

/// What one part of the architecture is, as its badge says it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    /// A program: what runs.
    App,
    WebApp,
    MobileApp,
    DesktopApp,
    Server,
    Cli,
    Library,
    Macros,
    /// What the user sees: a UI's components, a GUI's widgets.
    Ui,
    /// A web app's pages and routes.
    Pages,
    Api,
    State,
    Services,
    Core,
    Data,
    Models,
    Types,
    Hooks,
    Utilities,
    Config,
    Styles,
    Assets,
    Tests,
    Examples,
    Benchmarks,
    Docs,
    Tooling,
    /// Others' code, kept in the project: `vendor/`, `third_party/`.
    Vendored,
}

/// The families the kinds are coloured by: what runs, what is seen, what
/// does the work, what holds the data, what helps, and what is around it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Entry,
    Interface,
    Logic,
    Data,
    Support,
    Around,
}

impl Kind {
    /// The badge, in English (translated where it is drawn).
    pub fn label(self) -> &'static str {
        match self {
            Kind::App => tr_noop!("App"),
            Kind::WebApp => tr_noop!("Web app"),
            Kind::MobileApp => tr_noop!("Mobile app"),
            Kind::DesktopApp => tr_noop!("Desktop app"),
            Kind::Server => tr_noop!("Server"),
            Kind::Cli => tr_noop!("Command line"),
            Kind::Library => tr_noop!("Library"),
            Kind::Macros => tr_noop!("Macros"),
            Kind::Ui => tr_noop!("UI"),
            Kind::Pages => tr_noop!("Pages"),
            Kind::Api => tr_noop!("API"),
            Kind::State => tr_noop!("State"),
            Kind::Services => tr_noop!("Services"),
            Kind::Core => tr_noop!("Core"),
            Kind::Data => tr_noop!("Data"),
            Kind::Models => tr_noop!("Models"),
            Kind::Types => tr_noop!("Types"),
            Kind::Hooks => tr_noop!("Hooks"),
            Kind::Utilities => tr_noop!("Utilities"),
            Kind::Config => tr_noop!("Config"),
            Kind::Styles => tr_noop!("Styles"),
            Kind::Assets => tr_noop!("Assets"),
            Kind::Tests => tr_noop!("Tests"),
            Kind::Examples => tr_noop!("Examples"),
            Kind::Benchmarks => tr_noop!("Benchmarks"),
            Kind::Docs => tr_noop!("Docs"),
            Kind::Tooling => tr_noop!("Tooling"),
            Kind::Vendored => tr_noop!("Vendored"),
        }
    }

    pub fn tier(self) -> Tier {
        match self {
            Kind::App
            | Kind::WebApp
            | Kind::MobileApp
            | Kind::DesktopApp
            | Kind::Server
            | Kind::Cli => Tier::Entry,
            Kind::Ui | Kind::Pages | Kind::Styles | Kind::Hooks => Tier::Interface,
            Kind::Library | Kind::Api | Kind::State | Kind::Services | Kind::Core => Tier::Logic,
            Kind::Data | Kind::Models | Kind::Types => Tier::Data,
            Kind::Macros | Kind::Utilities | Kind::Config | Kind::Assets => Tier::Support,
            Kind::Tests
            | Kind::Examples
            | Kind::Benchmarks
            | Kind::Docs
            | Kind::Tooling
            | Kind::Vendored => Tier::Around,
        }
    }

    /// Whether it is around the project rather than part of what it does:
    /// drawn apart, below the rest.
    pub fn is_around(self) -> bool {
        self.tier() == Tier::Around
    }
}

/// One of a component's parts: a folder or a file in it.
#[derive(Debug, Clone, PartialEq)]
pub struct Part {
    pub name: String,
    /// Relative to the project's root, `/`-separated.
    pub rel: String,
    /// Its source files.
    pub files: usize,
    pub dir: bool,
    /// The file a click on it opens: itself, or the one its folder starts
    /// from.
    pub open: String,
}

/// One part of the architecture.
#[derive(Debug, Clone, PartialEq)]
pub struct Component {
    /// As the project calls it: a package's name, else its folder's.
    pub name: String,
    /// Its folder, relative to the project's root (`""`: the root).
    pub rel: String,
    pub kind: Option<Kind>,
    /// Its most used language, by key (`rust`, `typescript`).
    pub language: Option<&'static str>,
    /// Its source files.
    pub files: usize,
    /// What it is for, in a sentence: its manifest's description, its
    /// README's, its module doc's, or the AI's summary of its folder.
    pub about: Option<String>,
    /// Its largest parts, in order, and how many more there are.
    pub parts: Vec<Part>,
    pub more_parts: usize,
    /// The file it starts from — its `main`, `index`, `lib`: where a click
    /// on it opens.
    pub entry: Option<String>,
    /// The libraries it is built on that it imports most.
    pub built_on: Vec<String>,
    /// Packages drawn as this one: examples, say, each a package.
    pub members: usize,
}

/// `from` uses `to`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Link {
    pub from: usize,
    pub to: usize,
    /// How many of `from`'s imports are of `to`'s files (0: its manifest
    /// says it depends on it, and no import was resolved to it).
    pub imports: usize,
}

/// How the components were found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Basis {
    /// The project's packages.
    #[default]
    Packages,
    /// The folders of its one package's source.
    Folders,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Architecture {
    pub components: Vec<Component>,
    pub links: Vec<Link>,
    pub basis: Basis,
}

/// What the architecture is read from.
pub struct Inputs<'a> {
    /// Every file of the project, relative to its root, `/`-separated.
    pub files: Vec<&'a str>,
    /// Each import of one project file by another, (importer, imported):
    /// the imported may be a folder (a Go package).
    pub imports: Vec<(String, String)>,
    /// Each library a file imports, (importer, library as imported).
    pub externals: Vec<(String, String)>,
    /// A project file's text, by its path; `None` where none can be read
    /// (a remote project's files are not here).
    pub read: &'a dyn Fn(&str) -> Option<String>,
    /// The AI's summary of a folder, by its path.
    pub summaries: HashMap<String, String>,
    /// The files with a program's entry point (`fn main`).
    pub mains: HashSet<String>,
}

/// Libraries nearly every project of a language uses — its serialisation,
/// errors, logging, macros' plumbing — which say nothing of what this one
/// is built on (normalised, [`norm`]).
const PLUMBING: &[&str] = &[
    "serde",
    "serde_json",
    "serde_derive",
    "anyhow",
    "thiserror",
    "log",
    "tracing",
    "once_cell",
    "lazy_static",
    "itertools",
    "base64",
    "sha2",
    "bytes",
    "futures",
    "parking_lot",
    "smallvec",
    "bitflags",
    "cfg_if",
    "libc",
    "tempfile",
    "dirs",
    "proc_macro2",
    "quote",
    "syn",
    "tslib",
    "clsx",
    "classnames",
    "typing_extensions",
    "six",
    "attrs",
];

/// The most components drawn: past it, the smallest are drawn as one.
pub const MAX_COMPONENTS: usize = 14;
/// The most parts a component lists.
pub const MAX_PARTS: usize = 6;
/// The most manifests read: a monorepo of a thousand packages is drawn by
/// the ones nearest its root.
const MAX_MANIFESTS: usize = 400;
/// The most bytes of a manifest, README or doc comment read.
const MAX_READ: usize = 256 * 1024;

/// A package's manifest, as far as the architecture needs it.
#[derive(Debug, Clone, Default)]
struct Manifest {
    name: Option<String>,
    about: Option<String>,
    /// Its dependencies' names, normalised ([`norm`]).
    deps: Vec<String>,
    /// Each dependency's name as the manifest spells it, by its normalised.
    spelled: HashMap<String, String>,
    /// Its development dependencies too (normalised): a package of the
    /// project's it uses only to build or test with is used all the same.
    dev_deps: Vec<String>,
    kind: Option<Kind>,
    /// A workspace's root, which holds none of the code itself.
    aggregate: bool,
}

/// A source file: one the overview counts as the project's code — its
/// programs, markup and styles, not its data, docs or lock files.
fn source_language(rel: &str) -> Option<&'static str> {
    let key = clew_core::highlight::detect(Path::new(rel))?;
    let counted = clew_core::highlight::is_code(key)
        || matches!(
            key,
            "css" | "scss" | "less" | "html" | "vue" | "svelte" | "astro" | "sql"
        );
    counted.then_some(key)
}

/// A name as dependency lists and imports both spell it: lower case,
/// `-` as `_` (Cargo's `tree-sitter` is imported as `tree_sitter`).
fn norm(name: &str) -> String {
    name.trim().to_ascii_lowercase().replace('-', "_")
}

fn parent(rel: &str) -> &str {
    rel.rsplit_once('/').map_or("", |(dir, _)| dir)
}

fn base(rel: &str) -> &str {
    rel.rsplit_once('/').map_or(rel, |(_, name)| name)
}

/// Whether `rel` is `dir` or inside it (`""` holds everything).
fn within(rel: &str, dir: &str) -> bool {
    dir.is_empty()
        || rel == dir
        || (rel.starts_with(dir) && rel.as_bytes().get(dir.len()) == Some(&b'/'))
}

/// The first sentence of `text`, plain, as one line of at most `max`
/// characters (cut at a word, with an ellipsis) — the next with it when it
/// is too short to say much ("Your IDE is where you write code.").
pub fn first_sentence(text: &str, max: usize) -> Option<String> {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    // Where each sentence ends: a full stop followed by a space; `e.g.` and
    // `v1.2` end none.
    let bytes = text.as_bytes();
    let mut ends: Vec<usize> = Vec::new();
    for (i, c) in text.char_indices() {
        let stop = matches!(c, '.' | '!' | '?') && bytes.get(i + 1) == Some(&b' ');
        let abbreviation =
            text[..i].ends_with("e.g") || text[..i].ends_with("i.e") || text[..i].ends_with("etc");
        if (stop && !abbreviation) || matches!(c, '。' | '！' | '？') {
            ends.push(i + c.len_utf8());
        }
    }
    ends.push(text.len());
    // A character of Chinese or Japanese says about what two of English do.
    let weight = |s: &str| {
        s.chars()
            .map(|c| if c as u32 >= 0x2E80 { 2 } else { 1 })
            .sum::<usize>()
    };
    let short = |end: usize| weight(&text[..end]) < 48;
    let mut end = ends[0];
    if short(end)
        && let Some(&next) = ends.get(1)
        && text[..next].chars().count() <= max
    {
        end = next;
    }
    let sentence = text[..end].trim();
    if sentence.chars().count() <= max {
        return Some(sentence.to_string());
    }
    let cut: String = sentence.chars().take(max).collect();
    let cut = cut.rsplit_once(' ').map_or(cut.as_str(), |(head, _)| head);
    Some(format!("{}…", cut.trim_end_matches([',', ';', ':', ' '])))
}

/// A README's first paragraph of prose: not its title, badges, pictures or
/// HTML.
fn readme_about(text: &str) -> Option<String> {
    let mut paragraph: Vec<&str> = Vec::new();
    let mut fenced = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        // Setting it up is not saying what it is.
        let instruction = paragraph.is_empty()
            && [
                "To ",
                "Install",
                "Run ",
                "Clone",
                "First",
                "Make sure",
                "Before",
                "Please",
                "Note",
                "Prerequisite",
                "Usage",
                "Getting started",
            ]
            .iter()
            .any(|w| t.starts_with(w));
        if instruction {
            continue;
        }
        let prose = !t.is_empty()
            && !t.starts_with('#')
            && !t.starts_with('<')
            && !t.starts_with('[')
            && !t.starts_with('!')
            && !t.starts_with('|')
            && !t.starts_with('>')
            && !t.starts_with("---")
            && !t.starts_with("===")
            && !t.starts_with('-')
            && !t.starts_with('*');
        if prose {
            paragraph.push(t);
            continue;
        }
        // A paragraph of a few words, or one that leads into a list
        // ("Prerequisites:"), says nothing of what it is: the next.
        let words = paragraph
            .iter()
            .map(|l| l.split_whitespace().count())
            .sum::<usize>();
        let leads_in = paragraph.last().is_some_and(|l| l.ends_with(':'));
        if !paragraph.is_empty() && words >= 5 && !leads_in {
            break;
        }
        paragraph.clear();
    }
    let words = paragraph
        .iter()
        .map(|l| l.split_whitespace().count())
        .sum::<usize>();
    if words < 5 || paragraph.last().is_some_and(|l| l.ends_with(':')) {
        return None;
    }
    let text = paragraph.join(" ");
    // Links read as their text, code as its text.
    let mut plain = String::new();
    let mut rest = text.as_str();
    while let Some(open) = rest.find('[') {
        plain.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        match (after.find(']'), after.find("](")) {
            (Some(close), Some(link)) if close == link => {
                plain.push_str(&after[..close]);
                rest = after[close + 2..].split_once(')').map_or("", |(_, r)| r);
            }
            _ => {
                plain.push('[');
                rest = after;
            }
        }
    }
    plain.push_str(rest);
    let plain = plain.replace(['`', '*'], "");
    // HTML in it (`<br>`, `<b>`) as the words around it.
    let mut words = String::new();
    let mut in_tag = false;
    for c in plain.chars() {
        match c {
            '<' => {
                in_tag = true;
                words.push(' ');
            }
            '>' if in_tag => in_tag = false,
            c if !in_tag => words.push(c),
            _ => {}
        }
    }
    first_sentence(&words, 160)
}

/// The value of `key = "…"` in a TOML table's lines (the first).
fn toml_string(text: &str, table: &str, key: &str) -> Option<String> {
    let mut in_table = table.is_empty();
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_table = t.trim_start_matches('[').trim_end_matches(']').trim() == table;
            continue;
        }
        if !in_table {
            continue;
        }
        let Some((k, v)) = t.split_once('=') else {
            continue;
        };
        if k.trim() != key {
            continue;
        }
        let v = v.trim();
        let quote = v.chars().next().filter(|c| *c == '"' || *c == '\'')?;
        return v[1..].split(quote).next().map(str::to_string);
    }
    None
}

/// The keys of a TOML table (`[dependencies]`), and of its dotted
/// subtables (`[dependencies.serde]`).
fn toml_keys(text: &str, table: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let mut in_table = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            let name = t.trim_start_matches('[').trim_end_matches(']').trim();
            in_table = name == table;
            if let Some(dep) = name.strip_prefix(table).and_then(|r| r.strip_prefix('.')) {
                keys.push(dep.trim_matches('"').to_string());
            }
            continue;
        }
        if in_table && let Some((k, _)) = t.split_once('=') {
            let k = k.trim().trim_matches('"');
            if !k.is_empty() && !k.starts_with('#') {
                keys.push(k.split('.').next().unwrap_or(k).to_string());
            }
        }
    }
    keys
}

/// Which of `deps` (normalised) is among `names`.
fn depends_on(deps: &[String], names: &[&str]) -> bool {
    names.iter().any(|n| deps.iter().any(|d| d == &norm(n)))
}

/// A Cargo package's manifest.
fn cargo_manifest(text: &str, dir_files: &dyn Fn(&str) -> bool) -> Manifest {
    let name = toml_string(text, "package", "name");
    let aggregate = name.is_none();
    let mut names = toml_keys(text, "dependencies");
    names.extend(toml_keys(text, "build-dependencies"));
    let dev_deps: Vec<String> = toml_keys(text, "dev-dependencies")
        .iter()
        .map(|d| norm(d))
        .collect();
    let spelled: HashMap<String, String> = names.iter().map(|d| (norm(d), d.clone())).collect();
    let deps: Vec<String> = names.iter().map(|d| norm(d)).collect();
    let proc_macro = text.lines().any(|l| {
        let l = l.replace(' ', "");
        l == "proc-macro=true" || l == "proc_macro=true"
    });
    let has_main = dir_files("src/main.rs") || text.contains("[[bin]]");
    let kind = if proc_macro {
        Some(Kind::Macros)
    } else if has_main {
        Some(
            if depends_on(
                &deps,
                &[
                    "axum",
                    "actix-web",
                    "salvo",
                    "warp",
                    "rocket",
                    "poem",
                    "tide",
                    "hyper",
                    "tonic",
                ],
            ) {
                Kind::Server
            } else if depends_on(
                &deps,
                &[
                    "iced", "egui", "eframe", "tauri", "gtk", "gtk4", "slint", "dioxus", "winit",
                ],
            ) {
                Kind::DesktopApp
            } else if depends_on(&deps, &["clap", "argh", "structopt", "pico-args", "lexopt"]) {
                Kind::Cli
            } else {
                Kind::App
            },
        )
    } else {
        Some(Kind::Library)
    };
    Manifest {
        name,
        about: toml_string(text, "package", "description"),
        deps,
        spelled,
        dev_deps,
        kind,
        aggregate,
    }
}

/// A `package.json`.
fn npm_manifest(text: &str, dir_files: &dyn Fn(&str) -> bool) -> Manifest {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(text) else {
        return Manifest::default();
    };
    let string = |key: &str| json.get(key).and_then(|v| v.as_str()).map(str::to_string);
    // What it runs on: its dependencies and peers. Its dev dependencies —
    // its build, test and lint tools — say what kind of package it is
    // (`vite` an app), not what it is built on.
    let mut deps: Vec<String> = Vec::new();
    let mut all: Vec<String> = Vec::new();
    let mut dev_deps: Vec<String> = Vec::new();
    let mut spelled: HashMap<String, String> = HashMap::new();
    for table in ["dependencies", "peerDependencies", "devDependencies"] {
        if let Some(obj) = json.get(table).and_then(|v| v.as_object()) {
            all.extend(obj.keys().map(|k| norm(k)));
            if table == "devDependencies" {
                dev_deps.extend(obj.keys().map(|k| norm(k)));
            }
            if table != "devDependencies" {
                deps.extend(obj.keys().map(|k| norm(k)));
                spelled.extend(obj.keys().map(|k| (norm(k), k.clone())));
            }
        }
    }
    let runtime: Vec<String> = json
        .get("dependencies")
        .and_then(|v| v.as_object())
        .map(|o| o.keys().map(|k| norm(k)).collect())
        .unwrap_or_default();
    let has = |names: &[&str]| depends_on(&all, names);
    let kind = if has(&["expo", "react-native"]) {
        Kind::MobileApp
    } else if has(&["electron", "@tauri-apps/api"]) {
        Kind::DesktopApp
    } else if has(&[
        "next",
        "nuxt",
        "@remix-run/react",
        "@sveltejs/kit",
        "astro",
        "gatsby",
        "@angular/core",
        "@react-router/dev",
        "@tanstack/react-start",
        "vitepress",
    ]) || ((has(&["vite", "react-scripts", "@vue/cli-service", "parcel"]))
        && dir_files("index.html"))
        || dir_files("index.html") && has(&["react", "vue", "svelte", "solid-js", "preact"])
    {
        Kind::WebApp
    } else if depends_on(
        &runtime,
        &[
            "express",
            "fastify",
            "hono",
            "koa",
            "@nestjs/core",
            "elysia",
            "@hapi/hapi",
            "@trpc/server",
        ],
    ) && json.get("exports").is_none_or(|e| e.is_null())
    {
        Kind::Server
    } else if json.get("bin").is_some() {
        Kind::Cli
    } else if has(&[
        "prisma",
        "@prisma/client",
        "drizzle-orm",
        "typeorm",
        "kysely",
        "mongoose",
        "sequelize",
    ]) && !has(&["react", "vue", "svelte"])
    {
        Kind::Data
    } else if has(&["react", "vue", "svelte", "solid-js"])
        && json.get("main").is_none()
        && json.get("exports").is_none()
    {
        Kind::WebApp
    } else {
        Kind::Library
    };
    Manifest {
        name: string("name"),
        about: string("description"),
        deps,
        spelled,
        dev_deps,
        kind: Some(kind),
        aggregate: json.get("workspaces").is_some(),
    }
}

/// A `pyproject.toml`.
fn python_manifest(text: &str) -> Manifest {
    let name =
        toml_string(text, "project", "name").or_else(|| toml_string(text, "tool.poetry", "name"));
    let about = toml_string(text, "project", "description")
        .or_else(|| toml_string(text, "tool.poetry", "description"));
    // `dependencies = ["requests>=2", …]` as names.
    let mut deps: Vec<String> = Vec::new();
    if let Some(start) = text.find("dependencies = [") {
        let list = &text[start + "dependencies = [".len()..];
        let list = list.split(']').next().unwrap_or("");
        for item in list.split(',') {
            let item = item.trim().trim_matches(['"', '\'']);
            let name: String = item
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
                .collect();
            if !name.is_empty() {
                deps.push(norm(&name));
            }
        }
    }
    deps.extend(
        toml_keys(text, "tool.poetry.dependencies")
            .iter()
            .map(|d| norm(d)),
    );
    let scripts = text.contains("[project.scripts]") || text.contains("[tool.poetry.scripts]");
    let kind = if depends_on(
        &deps,
        &[
            "django",
            "flask",
            "fastapi",
            "starlette",
            "aiohttp",
            "tornado",
            "sanic",
            "litestar",
        ],
    ) {
        Kind::Server
    } else if scripts {
        Kind::Cli
    } else {
        Kind::Library
    };
    Manifest {
        name,
        about,
        deps,
        spelled: HashMap::new(),
        dev_deps: Vec::new(),
        kind: Some(kind),
        aggregate: false,
    }
}

/// A `go.mod`.
fn go_manifest(text: &str, has_main: bool) -> Manifest {
    let name = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("module "))
        .map(|m| m.trim().to_string());
    let mut deps = Vec::new();
    let mut in_require = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with("require (") {
            in_require = true;
            continue;
        }
        if in_require && t == ")" {
            in_require = false;
            continue;
        }
        let entry = if in_require {
            Some(t)
        } else {
            t.strip_prefix("require ")
        };
        if let Some(module) = entry.and_then(|e| e.split_whitespace().next())
            && !module.starts_with("//")
        {
            deps.push(norm(module));
        }
    }
    let server = depends_on(
        &deps,
        &[
            "github.com/gin-gonic/gin",
            "github.com/labstack/echo/v4",
            "github.com/gofiber/fiber/v2",
            "github.com/go-chi/chi/v5",
            "github.com/gorilla/mux",
            "google.golang.org/grpc",
        ],
    );
    let kind = match (has_main, server) {
        (true, true) => Kind::Server,
        (true, false) => Kind::App,
        (false, _) => Kind::Library,
    };
    Manifest {
        name: name.map(|n| base(&n).to_string()),
        about: None,
        deps,
        spelled: HashMap::new(),
        dev_deps: Vec::new(),
        kind: Some(kind),
        aggregate: false,
    }
}

/// The manifest file names that make a folder a package, and whether a
/// file is one.
fn is_manifest(name: &str) -> bool {
    matches!(
        name,
        "Cargo.toml"
            | "package.json"
            | "go.mod"
            | "pyproject.toml"
            | "setup.py"
            | "pom.xml"
            | "build.gradle"
            | "build.gradle.kts"
            | "composer.json"
            | "pubspec.yaml"
            | "Package.swift"
            | "mix.exs"
            | "deno.json"
            | "deno.jsonc"
            | "lakefile.lean"
            | "lakefile.toml"
            | "build.sbt"
    ) || name.ends_with(".csproj")
        || name.ends_with(".fsproj")
        || name.ends_with(".gemspec")
        || name.ends_with(".cabal")
}

/// What a folder's name says it is, in a project of `language`: web
/// names (pages, components, hooks) read as a web app's only in one.
fn folder_kind(name: &str, language: Option<&str>) -> Option<Kind> {
    let lower = name.to_ascii_lowercase();
    // A dot folder (`.storybook`, `.github`, `.husky`) is a tool's.
    if lower.starts_with('.') {
        return Some(Kind::Tooling);
    }
    let n = lower
        .trim_start_matches(['_', '@', '('])
        .trim_end_matches(')');
    let web = matches!(
        language,
        Some(
            "javascript"
                | "typescript"
                | "tsx"
                | "vue"
                | "svelte"
                | "astro"
                | "html"
                | "css"
                | "scss"
                | "less"
        )
    );
    Some(match n {
        "test" | "tests" | "spec" | "specs" | "__tests__" | "e2e" | "integration" | "testing"
        | "test_utils" | "testutils" | "testdata" | "fixtures" | "mocks" | "__mocks__"
        | "playwright" | "cypress" | "mocks__" | "tests__" | "verification" | "proofs"
        | "formal" => Kind::Tests,
        "example" | "examples" | "sample" | "samples" | "demo" | "demos" | "playground" => {
            Kind::Examples
        }
        "bench" | "benches" | "benchmark" | "benchmarks" => Kind::Benchmarks,
        "doc" | "docs" | "documentation" | "website" | "site" | "book" => Kind::Docs,
        "script" | "scripts" | "tool" | "tools" | "tooling" | "xtask" | "ci" | "build"
        | "devtools" | "generators" | "templates" => Kind::Tooling,
        "pages" | "routes" | "views" | "screens" | "layouts" | "app" if web => Kind::Pages,
        "components" | "component" | "widgets" | "ui" | "gui" | "view" | "views" | "screens"
        | "editor" | "render" | "rendering" | "tui" | "frontend" | "client" | "web" => Kind::Ui,
        "api" | "apis" | "handlers" | "controllers" | "routes" | "router" | "routers"
        | "endpoints" | "rpc" | "trpc" | "graphql" => Kind::Api,
        "server" | "backend" | "service" if !web => Kind::Server,
        "server" | "backend" => Kind::Api,
        "store" | "stores" | "state" | "redux" | "slices" | "atoms" | "signals" => Kind::State,
        "services" | "service" | "usecases" | "domain" | "features" | "modules" | "logic" => {
            Kind::Services
        }
        "core" | "engine" | "kernel" | "runtime" | "internal" => Kind::Core,
        "db" | "database" | "data" | "storage" | "persistence" | "repository" | "repositories"
        | "migrations" | "prisma" | "drizzle" | "sql" | "dao" => Kind::Data,
        "models" | "model" | "entities" | "entity" | "schema" | "schemas" | "dto" | "dtos" => {
            Kind::Models
        }
        "types" | "typings" | "interfaces" | "@types" => Kind::Types,
        "hooks" | "composables" | "composable" => Kind::Hooks,
        "lib" | "libs" | "util" | "utils" | "utilities" | "helpers" | "common" | "shared"
        | "misc" | "miscellaneous" | "support" | "pkg" => Kind::Utilities,
        "config" | "configs" | "configuration" | "settings" | "env" | "constants" => Kind::Config,
        "styles" | "style" | "css" | "scss" | "theme" | "themes" => Kind::Styles,
        "assets" | "public" | "static" | "images" | "icons" | "fonts" | "locales" | "i18n" => {
            Kind::Assets
        }
        "cmd" | "cli" | "bin" | "commands" => Kind::Cli,
        "vendor" | "vendored" | "third_party" | "thirdparty" | "third-party" | "external"
        | "externals" | "deps" | "extern" => Kind::Vendored,
        "app" | "application" => Kind::App,
        _ => return None,
    })
}

/// A test, by its name: `*.test.*`, `*.spec.*`, `*_test.*`, `test_*.py`,
/// `*Test.java`, or one in a `__tests__` folder.
fn is_test_file(rel: &str) -> bool {
    let name = base(rel);
    let stem = name.split('.').next().unwrap_or(name);
    name.contains(".test.")
        || name.contains(".spec.")
        || name.contains(".e2e.")
        || stem.ends_with("_test")
        || stem.ends_with("_spec")
        || (name.ends_with(".py") && stem.starts_with("test_"))
        || ((name.ends_with(".java") || name.ends_with(".kt") || name.ends_with(".cs"))
            && (stem.ends_with("Test") || stem.ends_with("Tests")))
        || rel.split('/').any(|seg| seg == "__tests__")
}

/// Whether `rel` is in a folder of what is around the code: its tests,
/// examples, docs.
fn in_around_folder(rel: &str) -> bool {
    let segs: Vec<&str> = rel.split('/').collect();
    segs[..segs.len().saturating_sub(1)]
        .iter()
        .any(|seg| folder_kind(seg, None).is_some_and(|k| k.is_around()))
}

/// Read the architecture from `inputs`.
pub fn read(inputs: &Inputs<'_>) -> Architecture {
    // The source files, each with its language.
    // A test beside the code it tests (`cart.test.ts`, `parse_test.go`)
    // is no part of what that code does: it is left out — those in a
    // folder of tests are that folder's.
    let sources: Vec<(&str, &'static str)> = inputs
        .files
        .iter()
        .filter(|rel| !is_test_file(rel) || in_around_folder(rel))
        .filter_map(|rel| source_language(rel).map(|lang| (*rel, lang)))
        .collect();
    if sources.is_empty() {
        return Architecture::default();
    }
    let file_set: HashSet<&str> = inputs.files.iter().copied().collect();

    // The packages: folders with a manifest, nearest the root first.
    let mut manifest_files: Vec<&str> = inputs
        .files
        .iter()
        .copied()
        .filter(|rel| is_manifest(base(rel)))
        .filter(|rel| {
            !rel.split('/')
                .any(|seg| seg == "node_modules" || seg == "vendor")
        })
        .collect();
    manifest_files.sort_by_key(|rel| (rel.matches('/').count(), *rel));
    manifest_files.truncate(MAX_MANIFESTS);
    let mut packages: BTreeMap<String, Manifest> = BTreeMap::new();
    for rel in manifest_files {
        let dir = parent(rel).to_string();
        let name = base(rel);
        let dir_files = |inner: &str| {
            let path = if dir.is_empty() {
                inner.to_string()
            } else {
                format!("{dir}/{inner}")
            };
            file_set.contains(path.as_str())
        };
        let text = || (inputs.read)(rel).map(|t| t.chars().take(MAX_READ).collect::<String>());
        let manifest = match name {
            "Cargo.toml" => text().map(|t| cargo_manifest(&t, &dir_files)),
            "package.json" => text().map(|t| npm_manifest(&t, &dir_files)),
            "pyproject.toml" => text().map(|t| python_manifest(&t)),
            "go.mod" => {
                let has_main = sources.iter().any(|(f, _)| {
                    within(f, &dir) && (base(f) == "main.go" || inputs.mains.contains(*f))
                });
                text().map(|t| go_manifest(&t, has_main))
            }
            _ => None,
        };
        // Unread (a remote project, or one Clew reads no further): a
        // package by its folder, all the same.
        let mut manifest = manifest.unwrap_or_default();
        // A monorepo's root: its own files are its tooling's. (Each of
        // Turborepo's packages may have a `turbo.json` of its own.)
        if dir.is_empty()
            && [
                "pnpm-workspace.yaml",
                "turbo.json",
                "lerna.json",
                "nx.json",
                "rush.json",
            ]
            .iter()
            .any(|f| dir_files(f))
        {
            manifest.aggregate = true;
        }
        if manifest.aggregate {
            manifest.kind = Some(Kind::Tooling);
        }
        let entry = packages.entry(dir).or_default();
        // A folder's first manifest says most (Cargo.toml before a
        // package.json beside it for, say, its web assets).
        if entry.name.is_none() && entry.kind.is_none() {
            *entry = manifest;
        } else if entry.about.is_none() {
            entry.about = manifest.about;
        }
    }
    // A workspace's root, holding none of the code itself, is no package
    // of its own; nor is a folder with no source in it.
    let owner_of = |rel: &str, packages: &BTreeMap<String, Manifest>| -> Option<String> {
        let mut dir = parent(rel);
        loop {
            if packages.contains_key(dir) {
                return Some(dir.to_string());
            }
            if dir.is_empty() {
                return None;
            }
            dir = parent(dir);
        }
    };
    let mut counts: HashMap<String, usize> = HashMap::new();
    for (rel, _) in &sources {
        if let Some(owner) = owner_of(rel, &packages) {
            *counts.entry(owner).or_default() += 1;
        }
    }
    packages.retain(|dir, m| {
        counts.get(dir).copied().unwrap_or(0) > 0 && !(m.aggregate && m.name.is_none())
    });

    let mut arch = if packages.len() >= 2 {
        by_packages(inputs, &sources, &packages)
    } else {
        let (dir, manifest) = packages
            .iter()
            .next()
            .map_or((String::new(), None), |(d, m)| (d.clone(), Some(m.clone())));
        by_folders(inputs, &sources, &dir, manifest.as_ref())
    };
    link(inputs, &mut arch, &packages);
    arch
}

/// The languages of `files`, most used first.
fn main_language<'a>(
    files: impl Iterator<Item = &'a (&'a str, &'static str)>,
) -> Option<&'static str> {
    let mut by: HashMap<&'static str, usize> = HashMap::new();
    for (_, lang) in files {
        *by.entry(lang).or_default() += 1;
    }
    let mut by: Vec<(&'static str, usize)> = by.into_iter().collect();
    // Markup and styles count, but the code is what a component is in.
    by.sort_by_key(|(lang, n)| {
        let code = clew_core::highlight::is_code(lang);
        (std::cmp::Reverse(code), std::cmp::Reverse(*n), *lang)
    });
    by.first().map(|(lang, _)| *lang)
}

/// The parts of the folder `dir`: its subfolders and files that hold
/// source, the largest first, and how many more.
fn parts_of(
    dir: &str,
    sources: &[(&str, &'static str)],
    mains: &HashSet<String>,
) -> (Vec<Part>, usize) {
    let mut folders: BTreeMap<String, usize> = BTreeMap::new();
    let mut files: Vec<&str> = Vec::new();
    for (rel, _) in sources.iter().filter(|(rel, _)| within(rel, dir)) {
        let rest = if dir.is_empty() {
            *rel
        } else {
            &rel[dir.len() + 1..]
        };
        match rest.split_once('/') {
            Some((folder, _)) => *folders.entry(folder.to_string()).or_default() += 1,
            None => files.push(rel),
        }
    }
    let join = |name: &str| {
        if dir.is_empty() {
            name.to_string()
        } else {
            format!("{dir}/{name}")
        }
    };
    let mut parts: Vec<Part> = folders
        .into_iter()
        .map(|(name, n)| {
            let rel = join(&name);
            Part {
                open: opens(&rel, sources, mains),
                rel,
                name,
                files: n,
                dir: true,
            }
        })
        .collect();
    parts.sort_by(|a, b| b.files.cmp(&a.files).then(a.name.cmp(&b.name)));
    // Its own files after its folders: the ones it starts from first.
    // Its own files — not its tools' configurations (`eslint.config.ts`,
    // `vite.config.ts`), which say nothing of what it does.
    let mut files: Vec<Part> = files
        .into_iter()
        .filter(|rel| !is_tool_config(base(rel)))
        .map(|rel| Part {
            name: base(rel).to_string(),
            rel: rel.to_string(),
            files: 1,
            dir: false,
            open: rel.to_string(),
        })
        .collect();
    files.sort_by_key(|p| {
        (
            !(mains.contains(&p.rel) || is_entry_name(&p.name)),
            p.name.clone(),
        )
    });
    parts.extend(files);
    let more = parts.len().saturating_sub(MAX_PARTS);
    parts.truncate(MAX_PARTS);
    (parts, more)
}

/// The file a click on the folder `dir` opens: a Rust module's own file
/// beside it (`ui.rs` of `ui/`), the one it starts from, else its first.
fn opens(dir: &str, sources: &[(&str, &'static str)], mains: &HashSet<String>) -> String {
    let module = format!("{dir}.rs");
    if sources.iter().any(|(r, _)| *r == module) {
        return module;
    }
    entry_of(dir, sources, mains)
        .or_else(|| {
            sources
                .iter()
                .map(|(r, _)| *r)
                .filter(|r| within(r, dir))
                .min_by_key(|r| (r.matches('/').count(), *r))
                .map(str::to_string)
        })
        .unwrap_or_default()
}

/// A tool's configuration, by its name: `vite.config.ts`, `.eslintrc.js`,
/// `jest.setup.ts`, `env.d.ts`.
fn is_tool_config(name: &str) -> bool {
    name.contains(".config.")
        || name.starts_with('.')
        || name.ends_with("rc.js")
        || name.ends_with("rc.cjs")
        || name.contains(".setup.")
        || name.ends_with(".d.ts")
        || name == "build.rs"
}

/// A file a program or a library is entered by.
fn is_entry_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name);
    matches!(
        stem,
        "main"
            | "lib"
            | "index"
            | "mod"
            | "__init__"
            | "__main__"
            | "app"
            | "App"
            | "server"
            | "manage"
            | "Program"
            | "Main"
            | "cli"
    )
}

/// The file `dir`'s component is entered by: a `main` the index found,
/// else the conventional one nearest its root.
fn entry_of(
    dir: &str,
    sources: &[(&str, &'static str)],
    mains: &HashSet<String>,
) -> Option<String> {
    // Not a tool's (`.storybook/main.ts`) or a test's.
    let inside: Vec<&str> = sources
        .iter()
        .map(|(r, _)| *r)
        .filter(|r| within(r, dir))
        .filter(|r| {
            let rest = if dir.is_empty() {
                *r
            } else {
                &r[dir.len() + 1..]
            };
            !rest.split('/').rev().skip(1).any(|seg| {
                folder_kind(seg, None).is_some_and(|k| k.is_around() || k == Kind::Assets)
            })
        })
        .collect();
    let depth = |r: &str| r.matches('/').count();
    // A Cargo build script has a `main` too: it is not where the crate starts.
    let program = |r: &&&str| mains.contains(**r) && base(r) != "build.rs";
    if let Some(main) = inside
        .iter()
        .filter(program)
        .min_by_key(|r| (depth(r), r.len()))
    {
        return Some(main.to_string());
    }
    inside
        .iter()
        .filter(|r| is_entry_name(base(r)))
        .min_by_key(|r| {
            (
                depth(r),
                !base(r).starts_with("main"),
                !base(r).starts_with("lib"),
                r.len(),
            )
        })
        .map(|r| r.to_string())
}

/// What a folder says it is for: the AI's summary of it, its README, or
/// its module's doc comment.
fn folder_about(
    inputs: &Inputs<'_>,
    dir: &str,
    sources: &[(&str, &'static str)],
) -> Option<String> {
    if let Some(summary) = inputs.summaries.get(dir) {
        return first_sentence(summary, 160);
    }
    let join = |name: &str| {
        if dir.is_empty() {
            name.to_string()
        } else {
            format!("{dir}/{name}")
        }
    };
    for readme in ["README.md", "readme.md", "README", "README.rst"] {
        if let Some(about) = (inputs.read)(&join(readme)).and_then(|t| readme_about(&t)) {
            return Some(about);
        }
    }
    // A module's doc comment: Rust's `//!`, a Python package's docstring,
    // Go's `// Package …`, a JS module's leading `/** … */`.
    let rust_module = if dir.is_empty() {
        None
    } else {
        Some(format!("{dir}.rs"))
    };
    let candidates = [
        join("mod.rs"),
        join("lib.rs"),
        join("main.rs"),
        rust_module.unwrap_or_default(),
        join("__init__.py"),
        join("doc.go"),
        join("index.ts"),
        join("index.js"),
    ];
    for file in candidates.iter().filter(|f| !f.is_empty()) {
        let Some(text) = (inputs.read)(file) else {
            continue;
        };
        if let Some(about) = module_doc(file, &text) {
            return Some(about);
        }
    }
    // A folder of one file: that file's.
    let own: Vec<&str> = sources
        .iter()
        .map(|(r, _)| *r)
        .filter(|r| within(r, dir))
        .collect();
    if let [only] = own.as_slice()
        && let Some(about) = (inputs.read)(only).and_then(|t| module_doc(only, &t))
    {
        return Some(about);
    }
    // A Go package's comment, in whichever of its files has it.
    for (rel, lang) in sources.iter().filter(|(r, _)| parent(r) == dir) {
        if *lang == "go"
            && let Some(about) = (inputs.read)(rel).and_then(|t| module_doc(rel, &t))
        {
            return Some(about);
        }
    }
    None
}

/// A source file's own doc comment, as a sentence.
fn module_doc(rel: &str, text: &str) -> Option<String> {
    let head: String = text.chars().take(8 * 1024).collect();
    let mut lines: Vec<String> = Vec::new();
    if rel.ends_with(".rs") {
        for line in head.lines() {
            let t = line.trim_start();
            if let Some(doc) = t.strip_prefix("//!") {
                lines.push(doc.trim().to_string());
            } else if !(t.is_empty() || t.starts_with("#![") || t.starts_with("//"))
                || (!lines.is_empty() && t.is_empty())
            {
                break;
            }
        }
    } else if rel.ends_with(".py") {
        let t = head.trim_start();
        for quote in ["\"\"\"", "'''"] {
            if let Some(body) = t.strip_prefix(quote) {
                lines.push(body.split(quote).next().unwrap_or("").to_string());
            }
        }
    } else if rel.ends_with(".go") {
        let mut doc: Vec<String> = Vec::new();
        for line in head.lines() {
            let t = line.trim();
            if let Some(c) = t.strip_prefix("//") {
                doc.push(c.trim().to_string());
            } else if t.starts_with("package ") {
                if doc.first().is_some_and(|d| d.starts_with("Package ")) {
                    lines = doc;
                }
                break;
            } else {
                doc.clear();
            }
        }
    } else {
        let t = head.trim_start();
        if let Some(body) = t.strip_prefix("/**") {
            let body = body.split("*/").next().unwrap_or("");
            for line in body.lines() {
                let l = line.trim().trim_start_matches('*').trim();
                if l.starts_with('@') {
                    break;
                }
                lines.push(l.to_string());
            }
        }
    }
    let text = lines.join(" ");
    // A Markdown heading or link in a doc comment reads as its words.
    let text = text
        .replace(['`', '#'], "")
        .replace("[", "")
        .replace("]", "");
    first_sentence(&text, 160)
}

/// The architecture of a project of several packages: each a component.
fn by_packages(
    inputs: &Inputs<'_>,
    sources: &[(&str, &'static str)],
    packages: &BTreeMap<String, Manifest>,
) -> Architecture {
    let dirs: Vec<&String> = packages.keys().collect();
    // Each source file to its package: the deepest that holds it.
    let mut owned: HashMap<&str, Vec<(&str, &'static str)>> = HashMap::new();
    for (rel, lang) in sources {
        if let Some(dir) = dirs
            .iter()
            .filter(|d| within(rel, d))
            .max_by_key(|d| d.len())
        {
            owned.entry(dir.as_str()).or_default().push((rel, lang));
        }
    }
    let mut components: Vec<Component> = Vec::new();
    for (dir, manifest) in packages {
        let Some(files) = owned.get(dir.as_str()) else {
            continue;
        };
        let folder_name = if dir.is_empty() {
            "(root)".to_string()
        } else {
            base(dir).to_string()
        };
        // A package in examples/, tests/, benches/ is one of those, as
        // its folder says, whatever its manifest does.
        let around = dir
            .split('/')
            .filter_map(|seg| folder_kind(seg, None))
            .find(|k| k.is_around());
        // A library named for what it is (`@acme/api`, `packages/db`):
        // that, before "library".
        let named = || {
            let last = manifest
                .name
                .as_deref()
                .map(|n| base(n).to_string())
                .unwrap_or_default();
            folder_kind(&last, None)
                .or_else(|| folder_kind(base(dir), None))
                .filter(|k| !k.is_around() && k.tier() != Tier::Entry)
        };
        let kind = around.or(match manifest.kind {
            Some(Kind::Library) | None => named().or(manifest.kind),
            kind => kind,
        });
        let language = main_language(files.iter());
        // The package's own files only, not a nested package's.
        let own: Vec<(&str, &'static str)> = files.clone();
        // Its parts as its source divides: `src/`'s folders, not `src/`.
        let source = source_root(dir, &own, language);
        let (parts, more_parts) = parts_of(&source, &own, &inputs.mains);
        components.push(Component {
            name: manifest.name.clone().unwrap_or(folder_name),
            rel: dir.clone(),
            kind,
            language,
            files: files.len(),
            about: manifest
                .about
                .as_deref()
                .and_then(|a| first_sentence(a, 160))
                .or_else(|| folder_about(inputs, dir, &own))
                // A crate's doc is in `src/lib.rs`.
                .or_else(|| {
                    (source != *dir)
                        .then(|| folder_about(inputs, &source, &own))
                        .flatten()
                }),
            parts,
            more_parts,
            entry: entry_of(&source, &own, &inputs.mains)
                .or_else(|| entry_of(dir, &own, &inputs.mains)),
            built_on: Vec::new(),
            members: 1,
        });
    }
    // Two packages of one name (a copied manifest) told apart by their
    // folders.
    let mut seen: HashMap<String, usize> = HashMap::new();
    for c in &components {
        *seen.entry(c.name.clone()).or_default() += 1;
    }
    for c in &mut components {
        if seen.get(&c.name).copied().unwrap_or(0) > 1 && !c.rel.is_empty() {
            c.name = base(&c.rel).to_string();
        }
    }
    // Many packages in one folder around the project — examples/*,
    // each a package — as one.
    let mut groups: BTreeMap<(String, Kind), Vec<usize>> = BTreeMap::new();
    for (i, c) in components.iter().enumerate() {
        if let Some(kind) = c.kind.filter(|k| k.is_around()) {
            groups
                .entry((first_around_dir(&c.rel), kind))
                .or_default()
                .push(i);
        }
    }
    let mut merged: HashSet<usize> = HashSet::new();
    let mut extra: Vec<Component> = Vec::new();
    for ((dir, kind), members) in groups {
        if members.len() < 3 {
            continue;
        }
        let files: usize = members.iter().map(|&i| components[i].files).sum();
        let mut parts: Vec<Part> = members
            .iter()
            .map(|&i| Part {
                name: components[i].name.clone(),
                rel: components[i].rel.clone(),
                files: components[i].files,
                dir: true,
                open: opens_component(&components[i]),
            })
            .collect();
        parts.sort_by(|a, b| b.files.cmp(&a.files).then(a.name.cmp(&b.name)));
        let more_parts = parts.len().saturating_sub(MAX_PARTS);
        parts.truncate(MAX_PARTS);
        let language = main_language(
            sources
                .iter()
                .filter(|(r, _)| members.iter().any(|&i| within(r, &components[i].rel))),
        );
        extra.push(Component {
            name: base(&dir).to_string(),
            rel: dir.clone(),
            kind: Some(kind),
            language,
            files,
            about: None,
            parts,
            more_parts,
            entry: None,
            built_on: Vec::new(),
            members: members.len(),
        });
        merged.extend(members);
    }
    let mut components: Vec<Component> = components
        .into_iter()
        .enumerate()
        .filter(|(i, _)| !merged.contains(i))
        .map(|(_, c)| c)
        .chain(extra)
        .collect();
    cap(&mut components);
    Architecture {
        components,
        links: Vec::new(),
        basis: Basis::Packages,
    }
}

/// The file a click on a component opens: where it starts, else its first
/// part's.
pub fn opens_component(c: &Component) -> String {
    c.entry
        .clone()
        .or_else(|| c.parts.first().map(|p| p.open.clone()))
        .unwrap_or_default()
}

/// The folder of `rel`'s that makes it around the project (`examples` of
/// `examples/chat`), else its parent.
fn first_around_dir(rel: &str) -> String {
    let segs: Vec<&str> = rel.split('/').collect();
    for (i, seg) in segs.iter().enumerate() {
        if folder_kind(seg, None).is_some_and(|k| k.is_around()) {
            return segs[..=i].join("/");
        }
    }
    parent(rel).to_string()
}

/// At most [`MAX_COMPONENTS`]: the smallest of the project's own parts
/// past it drawn as one, "Other".
fn cap(components: &mut Vec<Component>) {
    if components.len() <= MAX_COMPONENTS {
        return;
    }
    let mut order: Vec<usize> = (0..components.len()).collect();
    // Around-the-project parts go first, then the smallest.
    order.sort_by_key(|&i| {
        let c = &components[i];
        (!c.kind.is_some_and(|k| k.is_around()), c.files)
    });
    let spill = components.len() - (MAX_COMPONENTS - 1);
    let gone: HashSet<usize> = order.into_iter().take(spill).collect();
    let mut parts: Vec<Part> = Vec::new();
    let mut files = 0;
    let mut members = 0;
    for i in &gone {
        let c = &components[*i];
        files += c.files;
        members += c.members;
        parts.push(Part {
            name: c.name.clone(),
            rel: c.rel.clone(),
            files: c.files,
            dir: true,
            open: opens_component(c),
        });
    }
    parts.sort_by(|a, b| b.files.cmp(&a.files).then(a.name.cmp(&b.name)));
    let more_parts = parts.len().saturating_sub(MAX_PARTS);
    parts.truncate(MAX_PARTS);
    let mut kept: Vec<Component> = components
        .drain(..)
        .enumerate()
        .filter(|(i, _)| !gone.contains(i))
        .map(|(_, c)| c)
        .collect();
    kept.push(Component {
        name: OTHER.to_string(),
        rel: String::new(),
        kind: None,
        language: None,
        files,
        about: None,
        parts,
        more_parts,
        entry: None,
        built_on: Vec::new(),
        members,
    });
    *components = kept;
}

/// The architecture of a project of one package (or none): the folders its
/// source is divided into, each a component, under the folder that holds
/// it — `src/`, a Java project's `src/main/java/org/acme/` — and what is
/// around them: its tests, examples, scripts.
fn by_folders(
    inputs: &Inputs<'_>,
    sources: &[(&str, &'static str)],
    package: &str,
    manifest: Option<&Manifest>,
) -> Architecture {
    let in_package: Vec<(&str, &'static str)> = sources
        .iter()
        .copied()
        .filter(|(r, _)| within(r, package))
        .collect();
    let language = main_language(in_package.iter());
    let root = source_root(package, &in_package, language);
    let (areas, _) = parts_of_all(&root, &in_package);
    let mut components: Vec<Component> = Vec::new();
    let mut root_files: Vec<(&str, &'static str)> = Vec::new();
    for area in &areas {
        if !area.dir {
            root_files.extend(in_package.iter().copied().filter(|(r, _)| *r == area.rel));
            continue;
        }
        let files: Vec<(&str, &'static str)> = in_package
            .iter()
            .copied()
            .filter(|(r, _)| within(r, &area.rel))
            .collect();
        let lang = main_language(files.iter());
        let (parts, more_parts) = parts_of(&area.rel, &files, &inputs.mains);
        components.push(Component {
            name: area.name.clone(),
            rel: area.rel.clone(),
            kind: folder_kind(&area.name, lang.or(language)),
            language: lang,
            files: files.len(),
            about: folder_about(inputs, &area.rel, &files),
            parts,
            more_parts,
            // A Rust module's own file is beside its folder (`app.rs`).
            entry: Some(format!("{}.rs", area.rel))
                .filter(|m| in_package.iter().any(|(r, _)| r == m))
                .or_else(|| entry_of(&area.rel, &files, &inputs.mains)),
            built_on: Vec::new(),
            members: 1,
        });
    }
    // The files beside the areas: where the program starts, as a component
    // of its own, named after its folder.
    if !root_files.is_empty() {
        let (parts, more_parts) = parts_of_own(&root_files, &inputs.mains);
        let entry = entry_of(&root, &root_files, &inputs.mains);
        let name = if root.is_empty() {
            manifest
                .and_then(|m| m.name.clone())
                .unwrap_or_else(|| "(root)".to_string())
        } else {
            base(&root).to_string()
        };
        // Where the program starts (`main.rs`, a web app's `main.ts`),
        // an app; a root of tools' configurations, its config.
        let program = root_files.iter().any(|(r, _)| {
            let name = base(r);
            let stem = name.split('.').next().unwrap_or(name);
            inputs.mains.contains(*r) && name != "build.rs"
                || matches!(
                    stem,
                    "main" | "App" | "app" | "server" | "manage" | "__main__"
                )
        });
        let configs = root_files
            .iter()
            .filter(|(r, _)| is_tool_config(base(r)) || base(r).starts_with("env."))
            .count();
        let kind = if program {
            manifest
                .and_then(|m| m.kind)
                .filter(|k| k.tier() == Tier::Entry)
                .or(Some(Kind::App))
        } else if configs * 2 >= root_files.len() {
            Some(Kind::Config)
        } else {
            None
        };
        components.insert(
            0,
            Component {
                name,
                rel: root.clone(),
                kind,
                language: main_language(root_files.iter()),
                files: root_files.len(),
                about: manifest
                    .and_then(|m| m.about.as_deref())
                    .and_then(|a| first_sentence(a, 160))
                    .or_else(|| {
                        if root.is_empty() {
                            (inputs.read)("README.md").and_then(|t| readme_about(&t))
                        } else {
                            root_files.iter().find_map(|(r, _)| {
                                is_entry_name(base(r))
                                    .then(|| (inputs.read)(r).and_then(|t| module_doc(r, &t)))
                                    .flatten()
                            })
                        }
                    }),
                parts,
                more_parts,
                entry,
                built_on: Vec::new(),
                members: 1,
            },
        );
    }
    // What is around the source, beside the folder it is in: tests/,
    // benches/, examples/ next to src/.
    if root != package {
        let (siblings, _) = parts_of_all(package, &in_package);
        for sib in siblings.iter().filter(|p| p.dir && !within(&root, &p.rel)) {
            let files: Vec<(&str, &'static str)> = in_package
                .iter()
                .copied()
                .filter(|(r, _)| within(r, &sib.rel))
                .collect();
            let lang = main_language(files.iter());
            let (parts, more_parts) = parts_of(&sib.rel, &files, &inputs.mains);
            components.push(Component {
                name: sib.name.clone(),
                rel: sib.rel.clone(),
                kind: folder_kind(&sib.name, lang.or(language)),
                language: lang,
                files: files.len(),
                about: folder_about(inputs, &sib.rel, &files),
                parts,
                more_parts,
                entry: entry_of(&sib.rel, &files, &inputs.mains),
                built_on: Vec::new(),
                members: 1,
            });
        }
    }
    cap(&mut components);
    Architecture {
        components,
        links: Vec::new(),
        basis: Basis::Folders,
    }
}

/// Where `dir`'s source divides: down through each folder that holds
/// nearly all of it, with next to nothing beside it — `src/`, a Java
/// project's `src/main/java/org/acme/`.
fn source_root(dir: &str, sources: &[(&str, &'static str)], language: Option<&str>) -> String {
    let mut root = dir.to_string();
    loop {
        let (parts, _) = parts_of_all(&root, sources);
        // What is around the code — tests/ beside src/ — is no part of
        // its share.
        let around = |p: &Part| {
            p.dir
                && folder_kind(&p.name, language)
                    .is_some_and(|k| k.is_around() || k == Kind::Assets)
        };
        let total: usize = parts.iter().filter(|p| !around(p)).map(|p| p.files).sum();
        let own_files = parts.iter().filter(|p| !p.dir).count();
        let Some(biggest) = parts.iter().filter(|p| p.dir).max_by_key(|p| p.files) else {
            return root;
        };
        // `src/` is where the source is, holding most of it; any other
        // folder, nearly all, with next to nothing beside it.
        let conventional = matches!(biggest.name.as_str(), "src" | "source" | "sources" | "lib")
            && biggest.files * 100 >= total * 60;
        let dominant = own_files <= 2 && biggest.files * 100 >= total * 85;
        if total == 0 || around(biggest) || !(conventional || dominant) {
            return root;
        }
        root = biggest.rel.clone();
    }
}

/// Every part of `dir` (no cap): its source folders and files.
fn parts_of_all(dir: &str, sources: &[(&str, &'static str)]) -> (Vec<Part>, usize) {
    let mut folders: BTreeMap<String, usize> = BTreeMap::new();
    let mut parts: Vec<Part> = Vec::new();
    for (rel, _) in sources
        .iter()
        .filter(|(rel, _)| within(rel, dir) && *rel != dir)
    {
        let rest = if dir.is_empty() {
            *rel
        } else {
            &rel[dir.len() + 1..]
        };
        match rest.split_once('/') {
            Some((folder, _)) => *folders.entry(folder.to_string()).or_default() += 1,
            None => parts.push(Part {
                name: rest.to_string(),
                rel: rel.to_string(),
                files: 1,
                dir: false,
                open: rel.to_string(),
            }),
        }
    }
    let join = |name: &str| {
        if dir.is_empty() {
            name.to_string()
        } else {
            format!("{dir}/{name}")
        }
    };
    let mut all: Vec<Part> = folders
        .into_iter()
        .map(|(name, files)| Part {
            rel: join(&name),
            name,
            files,
            dir: true,
            open: String::new(),
        })
        .collect();
    all.sort_by(|a, b| b.files.cmp(&a.files).then(a.name.cmp(&b.name)));
    all.extend(parts);
    (all, 0)
}

/// `files` as parts, the ones a program starts from first.
fn parts_of_own(files: &[(&str, &'static str)], mains: &HashSet<String>) -> (Vec<Part>, usize) {
    let mut parts: Vec<Part> = files
        .iter()
        .map(|(rel, _)| Part {
            name: base(rel).to_string(),
            rel: rel.to_string(),
            files: 1,
            dir: false,
            open: rel.to_string(),
        })
        .collect();
    parts.sort_by_key(|p| {
        (
            !(mains.contains(&p.rel) || is_entry_name(&p.name)),
            p.name.clone(),
        )
    });
    let more = parts.len().saturating_sub(MAX_PARTS);
    parts.truncate(MAX_PARTS);
    (parts, more)
}

/// Which component holds `rel`: the one of the deepest folder holding it
/// (a component of the root's own files holds only those).
fn holder(
    components: &[Component],
    rel: &str,
    root_files_of: &[Option<HashSet<String>>],
) -> Option<usize> {
    let mut best: Option<(usize, usize)> = None;
    for (i, c) in components.iter().enumerate() {
        if is_other(c) {
            continue;
        }
        let holds = match &root_files_of[i] {
            Some(own) => own.contains(rel),
            None => within(rel, &c.rel),
        };
        if holds && best.is_none_or(|(_, depth)| c.rel.len() > depth) {
            best = Some((i, c.rel.len()));
        }
    }
    // "Other" holds what its parts do.
    best.map(|(i, _)| i).or_else(|| {
        components
            .iter()
            .position(|c| is_other(c) && c.parts.iter().any(|p| within(rel, &p.rel)))
    })
}

/// The component the smallest are drawn as, past [`MAX_COMPONENTS`].
pub fn is_other(c: &Component) -> bool {
    c.name == OTHER && c.rel.is_empty() && c.kind.is_none() && c.language.is_none()
}

/// What the component of the rest is called (translated where drawn).
pub const OTHER: &str = tr_noop!("Other");

/// The links between the components: each import of one's file by
/// another's, counted; and, between packages, each dependency a manifest
/// names on another of the project's packages, though no import of it
/// was resolved. And what each is built on.
fn link(inputs: &Inputs<'_>, arch: &mut Architecture, packages: &BTreeMap<String, Manifest>) {
    let comps = &arch.components;
    // A component of a folder's own files (the root's, `src/`'s, beside
    // its areas) holds just those, not the areas under it.
    let root_files_of: Vec<Option<HashSet<String>>> = comps
        .iter()
        .map(|c| {
            let areas_under = comps
                .iter()
                .any(|o| o.rel != c.rel && within(&o.rel, &c.rel));
            (arch.basis == Basis::Folders && areas_under).then(|| {
                inputs
                    .files
                    .iter()
                    .filter(|f| parent(f) == c.rel)
                    .map(|f| f.to_string())
                    .collect()
            })
        })
        .collect();
    let mut holder_of: HashMap<&str, Option<usize>> = HashMap::new();
    let held = |rel: &'_ str| -> Option<usize> { holder(comps, rel, &root_files_of) };
    let mut counts: BTreeMap<(usize, usize), usize> = BTreeMap::new();
    // What a tool's configuration imports (`eslint.config.ts` its shared
    // rules) is the tool's, not the part's: no link.
    for (from, to) in inputs
        .imports
        .iter()
        .filter(|(from, _)| !is_tool_config(base(from)))
    {
        let a = *holder_of.entry(from.as_str()).or_insert_with(|| held(from));
        let b = held(to);
        if let (Some(a), Some(b)) = (a, b)
            && a != b
        {
            *counts.entry((a, b)).or_default() += 1;
        }
    }
    // Declared, between packages: a crate's path dependency, a package's
    // `workspace:` one, named by the other's name.
    if arch.basis == Basis::Packages {
        let by_name: HashMap<String, usize> = comps
            .iter()
            .enumerate()
            .filter(|(_, c)| c.members <= 1)
            .map(|(i, c)| (norm(&c.name), i))
            .collect();
        for (i, c) in comps.iter().enumerate() {
            let Some(manifest) = packages.get(&c.rel) else {
                continue;
            };
            for dep in manifest.deps.iter().chain(&manifest.dev_deps) {
                if let Some(&j) = by_name.get(dep)
                    && j != i
                {
                    counts.entry((i, j)).or_default();
                }
            }
        }
    }
    arch.links = counts
        .into_iter()
        .map(|((from, to), imports)| Link { from, to, imports })
        .collect();

    // What each is built on: the libraries its files import most — those
    // its package's manifest names, where it has one.
    let mut used: Vec<HashMap<String, usize>> = vec![HashMap::new(); arch.components.len()];
    let mut seen: HashSet<(&str, &str)> = HashSet::new();
    for (from, library) in &inputs.externals {
        if !seen.insert((from.as_str(), library.as_str())) {
            continue;
        }
        if let Some(i) = holder(&arch.components, from, &root_files_of) {
            *used[i].entry(library.clone()).or_default() += 1;
        }
    }
    let owners: Vec<String> = packages.keys().cloned().collect();
    let project_names: HashSet<String> = arch
        .components
        .iter()
        .map(|c| norm(&c.name))
        .chain(
            packages
                .values()
                .filter_map(|m| m.name.as_deref().map(norm)),
        )
        .collect();
    for (i, c) in arch.components.iter_mut().enumerate() {
        let owner = owners
            .iter()
            .filter(|d| within(&c.rel, d))
            .max_by_key(|d| d.len());
        let manifest = owner.and_then(|o| packages.get(o));
        let deps: Option<&Vec<String>> = manifest.map(|m| &m.deps);
        let mut libs: Vec<(String, usize)> = used[i]
            .iter()
            .filter(|(lib, _)| {
                let n = norm(lib);
                !project_names.contains(&n)
                    && !PLUMBING.contains(&n.as_str())
                    && !n.starts_with("@types/")
                    && match deps {
                        Some(deps) if !deps.is_empty() => deps
                            .iter()
                            .any(|d| d == &n || n.starts_with(&format!("{d}/"))),
                        _ => false,
                    }
            })
            .map(|(lib, n)| (lib.clone(), *n))
            .collect();
        libs.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        c.built_on = libs
            .into_iter()
            .take(3)
            .map(|(lib, _)| {
                // A Go module by its last part (`gin` of
                // github.com/gin-gonic/gin), a Rust crate as Cargo names it
                // (`tree-sitter`, imported as `tree_sitter`).
                if let Some(spelled) = manifest.and_then(|m| m.spelled.get(&norm(&lib))) {
                    spelled.clone()
                } else if lib.contains('.') && lib.contains('/') {
                    lib.trim_end_matches(|ch: char| ch == 'v' || ch.is_ascii_digit() || ch == '/')
                        .rsplit('/')
                        .next()
                        .unwrap_or(&lib)
                        .to_string()
                } else {
                    lib
                }
            })
            .collect();
    }
}

/// The rows the diagram is drawn in, top to bottom: each component in the
/// row below every one that uses it — what runs at the top, what
/// everything rests on at the bottom — components that use one another
/// in a circle side by side; and what is around the project (its tests,
/// examples, docs) in a row of its own at the foot.
pub fn layers(arch: &Architecture) -> Vec<Vec<usize>> {
    let n = arch.components.len();
    let around: Vec<bool> = arch
        .components
        .iter()
        .map(|c| c.kind.is_some_and(Kind::is_around))
        .collect();
    // The links that place them: a circle of imports broken where it is
    // weakest — the import made least — until none is left but between
    // parts that import one another as much; those share a row.
    let mut kept: Vec<&Link> = arch
        .links
        .iter()
        .filter(|l| !around[l.from] && !around[l.to] && l.from != l.to)
        .collect();
    loop {
        let mut uses: Vec<Vec<usize>> = vec![Vec::new(); n];
        for l in &kept {
            uses[l.from].push(l.to);
        }
        let mut comp_of = vec![0; n];
        for (k, members) in sccs(&uses).iter().enumerate() {
            for &m in members {
                comp_of[m] = k;
            }
        }
        let inside = |l: &Link| comp_of[l.from] == comp_of[l.to];
        let circling: Vec<usize> = kept
            .iter()
            .filter(|l| inside(l))
            .map(|l| l.imports)
            .collect();
        let (Some(&weakest), Some(&strongest)) = (circling.iter().min(), circling.iter().max())
        else {
            break;
        };
        if weakest == strongest {
            break;
        }
        kept.retain(|l| !(inside(l) && l.imports == weakest));
    }
    let mut uses: Vec<Vec<usize>> = vec![Vec::new(); n];
    for l in &kept {
        uses[l.from].push(l.to);
    }
    // Strongly connected components (Tarjan), so a cycle is one node.
    let scc = sccs(&uses);
    let mut comp_of = vec![0; n];
    for (k, members) in scc.iter().enumerate() {
        for &m in members {
            comp_of[m] = k;
        }
    }
    // Depth: the longest chain of users above each.
    let k = scc.len();
    let mut cuses: Vec<HashSet<usize>> = vec![HashSet::new(); k];
    for (a, targets) in uses.iter().enumerate() {
        for &b in targets {
            if comp_of[a] != comp_of[b] {
                cuses[comp_of[a]].insert(comp_of[b]);
            }
        }
    }
    let mut depth = vec![0usize; k];
    // Tarjan returns components in reverse topological order (a component
    // after those it uses), so users come last: walk it backwards.
    for c in (0..k).rev() {
        for &d in &cuses[c] {
            depth[d] = depth[d].max(depth[c] + 1);
        }
    }
    // One that uses none and none uses rests on nothing: at the bottom,
    // with the foundations, not over the apps at the top.
    let linked: Vec<bool> = (0..n)
        .map(|i| kept.iter().any(|l| l.from == i || l.to == i))
        .collect();
    let deepest = (0..n)
        .filter(|&i| !around[i] && linked[i])
        .map(|i| depth[comp_of[i]])
        .max()
        .unwrap_or(0);
    let mut rows: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    let mut foot: Vec<usize> = Vec::new();
    for i in 0..n {
        if around[i] {
            foot.push(i);
        } else if !linked[i] {
            rows.entry(deepest).or_default().push(i);
        } else {
            rows.entry(depth[comp_of[i]]).or_default().push(i);
        }
    }
    let mut rows: Vec<Vec<usize>> = rows.into_values().collect();
    for row in &mut rows {
        // The largest first, so a row reads from its main part.
        row.sort_by(|&a, &b| {
            let (ca, cb) = (&arch.components[a], &arch.components[b]);
            cb.files.cmp(&ca.files).then(ca.name.cmp(&cb.name))
        });
    }
    if !foot.is_empty() {
        foot.sort_by(|&a, &b| arch.components[b].files.cmp(&arch.components[a].files));
        rows.push(foot);
    }
    rows
}

/// Tarjan's strongly connected components of the graph `edges`: each a
/// list of nodes, in reverse topological order.
fn sccs(edges: &[Vec<usize>]) -> Vec<Vec<usize>> {
    struct State<'a> {
        edges: &'a [Vec<usize>],
        index: Vec<Option<usize>>,
        low: Vec<usize>,
        on: Vec<bool>,
        stack: Vec<usize>,
        next: usize,
        out: Vec<Vec<usize>>,
    }
    fn visit(s: &mut State<'_>, v: usize) {
        s.index[v] = Some(s.next);
        s.low[v] = s.next;
        s.next += 1;
        s.stack.push(v);
        s.on[v] = true;
        for &w in &s.edges[v] {
            match s.index[w] {
                None => {
                    visit(s, w);
                    s.low[v] = s.low[v].min(s.low[w]);
                }
                Some(iw) if s.on[w] => s.low[v] = s.low[v].min(iw),
                _ => {}
            }
        }
        if Some(s.low[v]) == s.index[v] {
            let mut comp = Vec::new();
            while let Some(w) = s.stack.pop() {
                s.on[w] = false;
                comp.push(w);
                if w == v {
                    break;
                }
            }
            s.out.push(comp);
        }
    }
    let n = edges.len();
    let mut s = State {
        edges,
        index: vec![None; n],
        low: vec![0; n],
        on: vec![false; n],
        stack: Vec::new(),
        next: 0,
        out: Vec::new(),
    };
    for v in 0..n {
        if s.index[v].is_none() {
            visit(&mut s, v);
        }
    }
    s.out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arch(
        files: &[(&str, &str)],
        imports: &[(&str, &str)],
        externals: &[(&str, &str)],
    ) -> Architecture {
        let texts: HashMap<String, String> = files
            .iter()
            .map(|(rel, text)| (rel.to_string(), text.to_string()))
            .collect();
        let read = |rel: &str| texts.get(rel).cloned();
        let inputs = Inputs {
            files: files.iter().map(|(rel, _)| *rel).collect(),
            imports: imports
                .iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect(),
            externals: externals
                .iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect(),
            read: &read,
            summaries: HashMap::new(),
            mains: files
                .iter()
                .filter(|(_, text)| text.contains("fn main"))
                .map(|(rel, _)| rel.to_string())
                .collect(),
        };
        read_inputs(&inputs)
    }

    fn read_inputs(inputs: &Inputs<'_>) -> Architecture {
        read(inputs)
    }

    fn named<'a>(a: &'a Architecture, name: &str) -> (usize, &'a Component) {
        a.components
            .iter()
            .enumerate()
            .find(|(_, c)| c.name == name)
            .unwrap_or_else(|| {
                panic!(
                    "no {name} in {:?}",
                    a.components.iter().map(|c| &c.name).collect::<Vec<_>>()
                )
            })
    }

    fn linked(a: &Architecture, from: &str, to: &str) -> Option<usize> {
        let (f, _) = named(a, from);
        let (t, _) = named(a, to);
        a.links
            .iter()
            .find(|l| l.from == f && l.to == t)
            .map(|l| l.imports)
    }

    /// A Cargo workspace: each crate a component — the app over its
    /// libraries, each with its manifest's description and kind — the
    /// imports between them counted, a path dependency with none resolved
    /// linked all the same, and the examples, each a crate, drawn as one.
    #[test]
    fn a_workspace_is_drawn_as_its_crates() {
        let files = [
            ("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n"),
            (
                "crates/app/Cargo.toml",
                "[package]\nname = \"acme\"\ndescription = \"The acme editor. It edits.\"\n[dependencies]\nacme-core = { path = \"../core\" }\niced = \"0.13\"\n",
            ),
            ("crates/app/src/main.rs", "fn main() {}"),
            ("crates/app/src/ui/mod.rs", ""),
            ("crates/app/src/ui/view.rs", ""),
            (
                "crates/core/Cargo.toml",
                "[package]\nname = \"acme-core\"\ndescription = \"Parsing and analysis\"\n[dependencies]\nacme-proto = { path = \"../proto\" }\nserde = \"1\"\ntree-sitter = \"0.25\"\n",
            ),
            ("crates/core/src/lib.rs", "//! Core."),
            ("crates/core/src/parse.rs", ""),
            (
                "crates/proto/Cargo.toml",
                "[package]\nname = \"acme-proto\"\n",
            ),
            (
                "crates/proto/src/lib.rs",
                "//! The wire protocol: what the client and server say.",
            ),
            ("examples/one/Cargo.toml", "[package]\nname = \"one\"\n"),
            ("examples/one/src/main.rs", "fn main() {}"),
            ("examples/two/Cargo.toml", "[package]\nname = \"two\"\n"),
            ("examples/two/src/main.rs", "fn main() {}"),
            ("examples/three/Cargo.toml", "[package]\nname = \"three\"\n"),
            ("examples/three/src/main.rs", "fn main() {}"),
        ];
        let a = arch(
            &files,
            &[
                ("crates/app/src/main.rs", "crates/core/src/lib.rs"),
                ("crates/app/src/ui/view.rs", "crates/core/src/parse.rs"),
                ("examples/one/src/main.rs", "crates/core/src/lib.rs"),
            ],
            &[
                ("crates/app/src/ui/view.rs", "iced"),
                ("crates/app/src/main.rs", "iced"),
                ("crates/app/src/main.rs", "std"),
                ("crates/core/src/lib.rs", "serde"),
                ("crates/core/src/parse.rs", "tree_sitter"),
            ],
        );
        assert_eq!(a.basis, Basis::Packages);
        let (_, app) = named(&a, "acme");
        assert_eq!(app.kind, Some(Kind::DesktopApp));
        assert_eq!(app.about.as_deref(), Some("The acme editor. It edits."));
        assert_eq!(app.entry.as_deref(), Some("crates/app/src/main.rs"));
        assert_eq!(
            app.built_on,
            ["iced"],
            "a dependency, not the standard library"
        );
        assert_eq!(app.language, Some("rust"));
        let (_, core) = named(&a, "acme-core");
        assert_eq!(core.kind, Some(Kind::Core), "named for what it is");
        assert_eq!(core.about.as_deref(), Some("Parsing and analysis"));
        assert_eq!(
            core.built_on,
            ["tree-sitter"],
            "as Cargo spells it; not serde, which all use"
        );
        let (_, proto) = named(&a, "acme-proto");
        assert_eq!(
            proto.about.as_deref(),
            Some("The wire protocol: what the client and server say.")
        );
        assert_eq!(linked(&a, "acme", "acme-core"), Some(2));
        assert_eq!(linked(&a, "acme-core", "acme-proto"), Some(0), "declared");
        let (_, examples) = named(&a, "examples");
        assert_eq!(
            (examples.kind, examples.members, examples.files),
            (Some(Kind::Examples), 3, 3)
        );
        assert_eq!(linked(&a, "examples", "acme-core"), Some(1));
        // The app above its library, the library above the protocol, the
        // examples at the foot.
        let rows = layers(&a);
        let row_of = |name: &str| {
            rows.iter()
                .position(|r| r.contains(&named(&a, name).0))
                .unwrap()
        };
        assert!(row_of("acme") < row_of("acme-core"));
        assert!(row_of("acme-core") < row_of("acme-proto"));
        assert_eq!(row_of("examples"), rows.len() - 1);
    }

    /// A project of one package: its source's folders, under `src/`, each
    /// a component named by what it is for — the program's own files one
    /// of them — and what is around the source, its tests, beside them.
    #[test]
    fn one_package_is_drawn_as_its_sources_folders() {
        let files = [
            (
                "package.json",
                r#"{"name":"shop","description":"A shop.","dependencies":{"react":"18","@tanstack/react-query":"5"},"devDependencies":{"vite":"5"}}"#,
            ),
            ("index.html", "<div id=root>"),
            ("src/main.tsx", ""),
            ("src/App.tsx", ""),
            ("src/components/Button.tsx", ""),
            ("src/components/Card.tsx", ""),
            ("src/pages/Home.tsx", ""),
            (
                "src/hooks/useCart.ts",
                "/** The cart, as the pages see it. */",
            ),
            ("src/api/client.ts", ""),
            ("src/api/products.ts", ""),
            ("tests/cart.test.ts", ""),
            ("README.md", "# Shop\n\nA small shop."),
        ];
        let a = arch(
            &files,
            &[
                ("src/App.tsx", "src/pages/Home.tsx"),
                ("src/pages/Home.tsx", "src/components/Card.tsx"),
                ("src/pages/Home.tsx", "src/hooks/useCart.ts"),
                ("src/hooks/useCart.ts", "src/api/client.ts"),
                ("src/main.tsx", "src/App.tsx"),
                ("tests/cart.test.ts", "src/hooks/useCart.ts"),
            ],
            &[
                ("src/api/client.ts", "@tanstack/react-query"),
                ("src/pages/Home.tsx", "react"),
            ],
        );
        assert_eq!(a.basis, Basis::Folders);
        let (_, src) = named(&a, "src");
        assert_eq!(src.files, 2, "the program's own files");
        assert_eq!(src.parts[0].name, "App.tsx");
        let (_, components) = named(&a, "components");
        assert_eq!(components.kind, Some(Kind::Ui));
        assert_eq!(named(&a, "pages").1.kind, Some(Kind::Pages));
        assert_eq!(named(&a, "hooks").1.kind, Some(Kind::Hooks));
        assert_eq!(
            named(&a, "hooks").1.about.as_deref(),
            Some("The cart, as the pages see it.")
        );
        assert_eq!(named(&a, "api").1.kind, Some(Kind::Api));
        assert_eq!(named(&a, "api").1.built_on, ["@tanstack/react-query"]);
        assert_eq!(named(&a, "tests").1.kind, Some(Kind::Tests));
        assert_eq!(linked(&a, "src", "pages"), Some(1), "the root's own files");
        assert_eq!(linked(&a, "pages", "components"), Some(1));
        assert_eq!(linked(&a, "hooks", "api"), Some(1));
        assert_eq!(linked(&a, "tests", "hooks"), Some(1));
        let rows = layers(&a);
        let row_of = |name: &str| {
            rows.iter()
                .position(|r| r.contains(&named(&a, name).0))
                .unwrap()
        };
        assert!(row_of("src") < row_of("pages"));
        assert!(row_of("pages") < row_of("hooks"));
        assert!(row_of("hooks") < row_of("api"));
        assert_eq!(row_of("tests"), rows.len() - 1);
    }

    /// Down through a Java project's folders to where its source divides.
    #[test]
    fn a_deep_source_root_is_found() {
        let files = [
            ("pom.xml", "<project/>"),
            ("src/main/java/org/acme/App.java", ""),
            ("src/main/java/org/acme/web/Controller.java", ""),
            ("src/main/java/org/acme/web/Routes.java", ""),
            ("src/main/java/org/acme/data/Repo.java", ""),
            ("src/main/java/org/acme/data/Entity.java", ""),
        ];
        let a = arch(&files, &[], &[]);
        let names: Vec<&str> = a.components.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["acme", "data", "web"]);
        assert_eq!(named(&a, "data").1.kind, Some(Kind::Data));
    }

    /// Components that use one another in a circle share a row.
    #[test]
    fn a_cycle_shares_a_row() {
        let files = [("src/a/x.py", ""), ("src/b/y.py", ""), ("src/c/z.py", "")];
        let a = arch(
            &files,
            &[
                ("src/a/x.py", "src/b/y.py"),
                ("src/b/y.py", "src/a/x.py"),
                ("src/b/y.py", "src/c/z.py"),
            ],
            &[],
        );
        let rows = layers(&a);
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(rows[0].len(), 2);
    }

    /// A circle of imports is broken where it is weakest: the part that
    /// imports the other most sits above it.
    #[test]
    fn a_circle_is_broken_where_it_is_weakest() {
        let files = [("src/a/x.py", ""), ("src/b/y.py", ""), ("src/c/z.py", "")];
        let a = arch(
            &files,
            &[
                ("src/a/x.py", "src/b/y.py"),
                ("src/a/x.py", "src/b/y.py"),
                ("src/b/y.py", "src/a/x.py"),
                ("src/b/y.py", "src/c/z.py"),
            ],
            &[],
        );
        let rows = layers(&a);
        let row_of = |name: &str| {
            rows.iter()
                .position(|r| r.contains(&named(&a, name).0))
                .unwrap()
        };
        assert!(row_of("a") < row_of("b"), "{rows:?}");
        assert!(row_of("b") < row_of("c"), "{rows:?}");
        assert!(linked(&a, "b", "a").is_some(), "drawn all the same");
    }

    #[test]
    fn a_first_sentence_is_one_line() {
        let long = "Parses the file into its tree, then resolves every name in it.";
        assert_eq!(
            first_sentence(&format!("{long}  Then more."), 200).as_deref(),
            Some(long)
        );
        // Too short to say much: the next sentence with it.
        assert_eq!(
            first_sentence(
                "Your IDE is where you write code. Clew is where you understand it. More.",
                200
            )
            .as_deref(),
            Some("Your IDE is where you write code. Clew is where you understand it.")
        );
        assert_eq!(
            first_sentence(
                "Uses e.g. the parser to read every file it is given, as it comes. And",
                200
            )
            .as_deref(),
            Some("Uses e.g. the parser to read every file it is given, as it comes.")
        );
        assert_eq!(
            first_sentence(
                "解析项目中的每一个文件，并找出其中所有名字的定义。然后",
                200
            )
            .as_deref(),
            Some("解析项目中的每一个文件，并找出其中所有名字的定义。")
        );
        assert_eq!(
            first_sentence("a very long sentence that goes on", 12).as_deref(),
            Some("a very long…")
        );
        assert_eq!(
            readme_about("# T\n\n[![ci](x)](y)\n\nA [fast](https://x) `tool` for **you**. More.")
                .as_deref(),
            Some("A fast tool for you. More.")
        );
    }
}
