//! The plugin tools an agent makes plugins with — the same for Clew's Ask
//! agent (run by Clew's window) and for every ACP agent (through
//! `clew-server mcp`).
//!
//! [`call`] does what needs no window: the guide, a plugin's source, a
//! build, what is installed. Trying a build on the open project, and asking
//! the user to install one, it hands back as a [`Step`] for the caller to do
//! its own way: the window does them, and `clew-server mcp` files a request
//! for the window (`requests`).

use std::path::Path;
use std::sync::atomic::AtomicBool;

use serde_json::{Value, json};

use crate::authoring;
use crate::build::{self, Built};
use crate::inspect::{Inspected, inspect};
use crate::manifest::Place;
use crate::store::Store;

/// A tool, as an agent is told of it.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    /// JSON Schema of its arguments.
    pub schema: Value,
    /// It only reads.
    pub read_only: bool,
}

/// What [`call`] leaves for its caller.
#[derive(Debug)]
pub enum Step {
    /// The tool's answer, for the agent.
    Done(Result<String, String>),
    /// Run the command `command` of `built` on the open project, or build
    /// its graph `graph`.
    Try {
        built: Box<Built>,
        /// Empty when a graph is tried.
        command: String,
        graph: Option<String>,
        file: Option<String>,
        line: Option<u32>,
        input: Option<String>,
    },
    /// Ask the user to install `built`.
    Install { built: Box<Built> },
}

/// Every plugin tool.
pub fn tool_defs() -> Vec<ToolDef> {
    let id = json!({"type": "string", "description": "The plugin's id, as in its plugin.toml (todo-authors)."});
    vec![
        ToolDef {
            name: "plugin_guide",
            description: "How to write a Clew plugin: what plugins can do, plugin.toml, the Rust API (clew_plugin_sdk), examples and limits. Read it before writing one.",
            schema: json!({"type": "object", "properties": {}}),
            read_only: true,
        },
        ToolDef {
            name: "plugin_new",
            description: "Make a new plugin's source from a template that builds as it is: plugin.toml and src/lib.rs. Then change them with plugin_write.",
            schema: json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string", "description": "3 to 40 of a-z, 0-9 and -, starting with a letter (todo-authors). It never changes."},
                    "name": {"type": "string", "description": "What the user sees, up to 60 characters."},
                    "description": {"type": "string", "description": "One line on what it does."}
                },
                "required": ["id", "name", "description"]
            }),
            read_only: false,
        },
        ToolDef {
            name: "plugin_files",
            description: "List the files of a plugin's source.",
            schema: json!({"type": "object", "properties": {"id": id}, "required": ["id"]}),
            read_only: true,
        },
        ToolDef {
            name: "plugin_read",
            description: "Read a file of a plugin's source.",
            schema: json!({
                "type": "object",
                "properties": {"id": id, "path": {"type": "string", "description": "plugin.toml, or a Rust file under src/ (src/lib.rs)."}},
                "required": ["id", "path"]
            }),
            read_only: true,
        },
        ToolDef {
            name: "plugin_write",
            description: "Write a file of a plugin's source, whole: plugin.toml, or a Rust file under src/. plugin.toml is checked first and a broken one is refused.",
            schema: json!({
                "type": "object",
                "properties": {
                    "id": id,
                    "path": {"type": "string", "description": "plugin.toml, or a Rust file under src/ (src/lib.rs)."},
                    "content": {"type": "string", "description": "The file's whole new text."}
                },
                "required": ["id", "path", "content"]
            }),
            read_only: false,
        },
        ToolDef {
            name: "plugin_build",
            description: "Build a plugin from its source. Answers with the compiler's messages to fix, or with what the plugin is (its permissions and commands). Build again after every change.",
            schema: json!({"type": "object", "properties": {"id": id}, "required": ["id"]}),
            read_only: false,
        },
        ToolDef {
            name: "plugin_try",
            description: "Run a command of the plugin's latest build on the open project, or build one of its graphs, and see what it gives back, to check it works. Nothing is shown to the user. Needs Clew open with the project.",
            schema: json!({
                "type": "object",
                "properties": {
                    "id": id,
                    "command": {"type": "string", "description": "The command's id in plugin.toml. Give this or graph."},
                    "graph": {"type": "string", "description": "A graph's id in plugin.toml, to build it instead of running a command."},
                    "file": {"type": "string", "description": "A file to run it on, from the project root, as if it were shown."},
                    "line": {"type": "integer", "description": "The caret's line in that file, from 1."},
                    "input": {"type": "string", "description": "What the user typed, for a command that asks for input."}
                },
                "required": ["id"]
            }),
            read_only: true,
        },
        ToolDef {
            name: "plugin_install",
            description: "Ask the user, in Clew, to install the plugin's latest build. They see its name, description, what it adds and its permissions, and decide. Answers whether they installed it, and where what it adds is. Needs Clew open. Before calling it, tell the user to switch to Clew to answer: the question waits there, not here.",
            schema: json!({"type": "object", "properties": {"id": id}, "required": ["id"]}),
            read_only: false,
        },
        ToolDef {
            name: "plugin_status",
            description: "The plugins there is source for and the plugins installed: versions, and whether each is on.",
            schema: json!({"type": "object", "properties": {}}),
            read_only: true,
        },
    ]
}

/// Whether `name` is a plugin tool.
pub fn is_plugin_tool(name: &str) -> bool {
    tool_defs().iter().any(|t| t.name == name)
}

fn text<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("\"{key}\" is missing"))
}

fn optional(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(String::from)
}

/// Do the tool `tool` with `args`, keeping everything under `data` (Clew's
/// data folder).
pub fn call(data: &Path, tool: &str, args: &Value, cancel: &AtomicBool) -> Step {
    let done = |r: Result<String, String>| Step::Done(r);
    match tool {
        "plugin_guide" => done(Ok(authoring::GUIDE.to_string())),
        "plugin_new" => done((|| {
            let id = text(args, "id")?;
            authoring::new_plugin(data, id, text(args, "name")?, text(args, "description")?)?;
            Ok(format!(
                "Made the plugin \"{id}\": plugin.toml and src/lib.rs. Read plugin_guide if you have not, write the code with plugin_write, then plugin_build."
            ))
        })()),
        "plugin_files" => done(
            text(args, "id")
                .and_then(|id| authoring::list_files(data, id))
                .map(|f| f.join("\n")),
        ),
        "plugin_read" => done((|| {
            authoring::read_file(data, text(args, "id")?, text(args, "path")?)
        })()),
        "plugin_write" => done((|| {
            let path = text(args, "path")?;
            authoring::write_file(data, text(args, "id")?, path, text(args, "content")?)?;
            Ok(format!("Wrote {path}."))
        })()),
        "plugin_build" => done(text(args, "id").and_then(|id| build_report(data, id, cancel))),
        "plugin_try" => {
            let id = match text(args, "id") {
                Ok(id) => id,
                Err(e) => return done(Err(e)),
            };
            let (command, graph) = match (optional(args, "command"), optional(args, "graph")) {
                (Some(command), None) => (command, None),
                (None, Some(graph)) => (String::new(), Some(graph)),
                _ => {
                    return done(Err(
                        "give the command to run, or the graph to build: one of them".into(),
                    ));
                }
            };
            match last_build(data, id) {
                Ok(built) => {
                    let manifest = &built.inspected.manifest;
                    let known = match &graph {
                        Some(g) => manifest.graph(g).is_some(),
                        None => manifest.command(&command).is_some(),
                    };
                    if !known {
                        let named = graph
                            .as_deref()
                            .map_or(format!("command \"{command}\""), |g| {
                                format!("graph \"{g}\"")
                            });
                        return done(Err(format!(
                            "the build of {id} has no {named}; it has {}",
                            commands_list(&built.inspected)
                        )));
                    }
                    Step::Try {
                        built: Box::new(built),
                        command,
                        graph,
                        file: optional(args, "file"),
                        line: args
                            .get("line")
                            .and_then(Value::as_u64)
                            .map(|l| l.min(u32::MAX as u64) as u32),
                        input: optional(args, "input"),
                    }
                }
                Err(e) => done(Err(e)),
            }
        }
        "plugin_install" => match text(args, "id").and_then(|id| last_build(data, id)) {
            Ok(built) => match already_installed(data, &built) {
                Some(said) => done(Ok(said)),
                None => Step::Install {
                    built: Box::new(built),
                },
            },
            Err(e) => done(Err(e)),
        },
        "plugin_status" => done(Ok(status(data))),
        other => done(Err(format!("there is no plugin tool {other}"))),
    }
}

/// Build the plugin `id` and say what came of it, for the agent.
fn build_report(data: &Path, id: &str, cancel: &AtomicBool) -> Result<String, String> {
    let source = authoring::source_dir(data, id)?;
    if !source.join("plugin.toml").is_file() {
        return Err(format!(
            "there is no plugin \"{id}\"; make it first (plugin_new)"
        ));
    }
    let found = crate::toolchain::find(Some(data));
    // Rust being set up, or Clew's own removed, is half there: why it is
    // not ready yet, not what it lacks partway. (Said of Clew, not of
    // another window: the window the agent works in may be the one.)
    if !matches!(&found, Ok(t) if t.cargo_home.is_none()) && crate::toolchain::busy(data) {
        return Err(
            "Rust cannot build plugins yet: Clew is setting it up or removing it right now. Build again when that is done; Clew's Settings → Plugins shows when."
                .into(),
        );
    }
    let toolchain = found.map_err(|missing| missing.into_build_error().report())?;
    let work = authoring::build_dir(data);
    std::fs::create_dir_all(&work).map_err(|e| e.to_string())?;
    let built = build::build(&source, &work, &toolchain, cancel).map_err(|e| e.report())?;
    let m = &built.inspected.manifest;
    let mut report = format!(
        "Built {} {} ({} KB) with Rust {}.\n{}\nAdds: {}.",
        m.plugin.id,
        m.plugin.version,
        built.bytes.len().div_ceil(1024),
        toolchain.version,
        permissions_line(&built.inspected),
        commands_list(&built.inspected),
    );
    if !built.warnings.is_empty() {
        report.push_str("\n\nWarnings to fix:\n\n");
        report.push_str(&built.warnings.join("\n"));
    }
    report.push_str("\n\nNext: plugin_try to check it does what was asked, then plugin_install.");
    Ok(report)
}

/// What to tell the agent when `built` is the very build installed, its
/// copy intact: the user approved this file, and is not asked again about
/// it. `None` when it is not, or its copy needs installing again.
fn already_installed(data: &Path, built: &Built) -> Option<String> {
    let manifest = &built.inspected.manifest;
    let store = crate::store::Store::load(data);
    let record = store.plugin.get(&manifest.plugin.id)?;
    if record.fingerprint != built.inspected.fingerprint()
        || crate::store::read_approved(data, record).is_err()
    {
        return None;
    }
    let name = &manifest.plugin.name;
    Some(match record.enabled {
        true => format!(
            "This exact build of {name} is already installed, so the user was not asked again. If you meant to change it, write the change, build it (plugin_build), then install it."
        ),
        false => format!(
            "This exact build of {name} is already installed, but the user turned it off in Settings → Plugins; it stays off until they turn it on there."
        ),
    })
}

/// The plugin `id`'s latest build, as an install would read it.
pub fn last_build(data: &Path, id: &str) -> Result<Built, String> {
    let id = crate::store::checked_id(id)?;
    let artifact = authoring::build_dir(data)
        .join(id)
        .join(format!("{id}.wasm"));
    let bytes = std::fs::read(&artifact).map_err(|_| {
        format!("the plugin \"{id}\" has not been built; build it first (plugin_build)")
    })?;
    // A build of an older source — written to since, or its last build
    // failed — is not what the agent just wrote: tried or offered, it
    // was the last good build, an "update" to the version installed.
    let source = authoring::source_dir(data, id)?;
    let crate_dir = artifact.parent().unwrap_or(&artifact);
    if !crate::build::built_from(crate_dir, &source) {
        return Err(format!(
            "the plugin \"{id}\" changed since it was last built, or its last build failed; build it again first (plugin_build)"
        ));
    }
    let inspected = inspect(&bytes)?;
    Ok(Built {
        bytes,
        inspected,
        artifact,
        warnings: Vec::new(),
    })
}

/// What a plugin may do, as a line for an agent.
pub fn permissions_line(inspected: &Inspected) -> String {
    let mut what = Vec::new();
    if inspected.needs & clew_rules::plugin::READ != 0 {
        what.push("read the project's files");
    }
    if inspected.needs & clew_rules::plugin::GIT != 0 {
        what.push("read the project's git history");
    }
    if what.is_empty() {
        "Permissions: none (it works on what it is given).".into()
    } else {
        format!("Permissions: {}.", what.join(", "))
    }
}

/// A plugin's commands and graphs, as a list for an agent.
fn commands_list(inspected: &Inspected) -> String {
    let m = &inspected.manifest;
    let mut parts: Vec<String> = m
        .commands
        .iter()
        .map(|c| {
            format!(
                "command {} (\"{}\"{})",
                c.id,
                c.title,
                match c.place {
                    Place::Code => ", also in the code's menu",
                    Place::Palette => "",
                }
            )
        })
        .collect();
    parts.extend(
        m.graphs
            .iter()
            .map(|g| format!("graph {} (\"{}\")", g.id, g.title)),
    );
    parts.join(", ")
}

/// The plugins there is source for, and those installed.
fn status(data: &Path) -> String {
    let mut lines = Vec::new();
    // Unreadable is not "no plugins": the agent is told, and does not make
    // a new one in place of plugins it cannot see.
    let store = match Store::load_checked(data) {
        Ok(store) => Some(store),
        Err(why) => {
            lines.push(format!(
                "Clew could not read its list of installed plugins ({why}); ask the user to look at Settings → Plugins."
            ));
            None
        }
    };
    // Unread, whether each source is installed is not known: "not
    // installed" would have the agent offer it again.
    let Some(store) = store else {
        for id in authoring::sources(data) {
            lines.push(format!(
                "{id}: source; whether it is installed is not known"
            ));
        }
        return lines.join("\n");
    };
    for (id, record) in &store.plugin {
        let version = record
            .manifest()
            .map(|m| m.plugin.version)
            .unwrap_or_else(|| "?".into());
        lines.push(format!(
            "{id} {version}: installed, {}",
            if record.enabled { "on" } else { "off" }
        ));
    }
    for id in authoring::sources(data) {
        if !store.plugin.contains_key(&id) {
            lines.push(format!("{id}: source only, not installed"));
        }
    }
    if lines.is_empty() {
        "No plugins yet. Make one with plugin_new.".into()
    } else {
        lines.join("\n")
    }
}

/// A graph a try built, as an agent reads it: how big it is, then its
/// first nodes and arrows, so it can see the graph is what was asked.
#[cfg(feature = "runtime")]
pub fn describe_graph(graph: &crate::runtime::Graph) -> String {
    const SHOWN: usize = 40;
    let mut said = format!(
        "It built a graph of {} nodes and {} arrows (after Clew's checks: at most {} nodes, arrows only between nodes there are).",
        graph.nodes.len(),
        graph.edges.len(),
        crate::runtime::MAX_NODES
    );
    if graph.nodes.is_empty() {
        said.push_str(" Project Graphs would show it empty.");
        return said;
    }
    said.push_str("\n\nNodes:");
    for node in graph.nodes.iter().take(SHOWN) {
        said.push_str(&format!("\n- [{}] {}", node.kind, node.label));
        if let Some(at) = &node.at {
            said.push_str(&format!(" ({}:{})", at.path, at.line));
        }
    }
    if graph.nodes.len() > SHOWN {
        said.push_str(&format!("\n… and {} more", graph.nodes.len() - SHOWN));
    }
    if !graph.edges.is_empty() {
        said.push_str("\n\nArrows:");
        let label = |n: u32| {
            graph
                .nodes
                .get(n as usize)
                .map_or("?", |node| node.label.as_str())
        };
        for edge in graph.edges.iter().take(SHOWN) {
            said.push_str(&format!("\n- {} → {}", label(edge.from), label(edge.to)));
            if !edge.label.is_empty() {
                said.push_str(&format!(": {}", edge.label));
            }
        }
        if graph.edges.len() > SHOWN {
            said.push_str(&format!("\n… and {} more", graph.edges.len() - SHOWN));
        }
    }
    said
}

/// What a command's outcome is, as an agent reads it in `plugin_try`.
#[cfg(feature = "runtime")]
pub fn describe_outcome(outcome: &crate::runtime::Outcome) -> String {
    use crate::runtime::{Block, Outcome};
    match outcome {
        Outcome::Nothing => "It gave back nothing to show.".into(),
        Outcome::Say(text) => format!("It says, in the status bar: {text}"),
        Outcome::Open(at) => format!("It opens {} at line {}.", at.path, at.line),
        Outcome::Show(view) => {
            let mut out = format!("It shows a page titled \"{}\":\n", view.title);
            for block in &view.blocks {
                let line = match block {
                    Block::Heading(t) => format!("# {t}"),
                    Block::Text(t) => t.lines().take(5).collect::<Vec<_>>().join("\n"),
                    Block::Notice(n) => format!("[{:?}] {}", n.level, n.text),
                    Block::Items(items) => list(
                        items.iter().map(|i| {
                            let at =
                                i.at.as_ref()
                                    .map(|l| format!(" → {}:{}", l.path, l.line))
                                    .unwrap_or_default();
                            format!("- {}{at}", i.title)
                        }),
                        items.len(),
                    ),
                    Block::Tree(rows) => list(
                        rows.iter()
                            .map(|r| format!("{}- {}", "  ".repeat(r.depth as usize), r.title)),
                        rows.len(),
                    ),
                    Block::Table(t) => {
                        let mut s = format!("| {} |", t.columns.join(" | "));
                        s.push('\n');
                        s.push_str(&list(
                            t.rows
                                .iter()
                                .map(|r| format!("| {} |", r.cells.join(" | "))),
                            t.rows.len(),
                        ));
                        s
                    }
                    Block::Facts(facts) => facts
                        .iter()
                        .map(|f| format!("{}: {}", f.name, f.value))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    Block::Code(c) => format!(
                        "```\n{}\n```",
                        c.text.lines().take(10).collect::<Vec<_>>().join("\n")
                    ),
                };
                out.push_str(&line);
                out.push('\n');
            }
            out
        }
    }
}

/// The first rows of a list, and how many more there are.
#[cfg(feature = "runtime")]
fn list(rows: impl Iterator<Item = String>, total: usize) -> String {
    const SHOWN: usize = 20;
    let mut out: Vec<String> = rows.take(SHOWN).collect();
    if total > SHOWN {
        out.push(format!("… and {} more rows", total - SHOWN));
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("clew-plugin-tools-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn answer(step: Step) -> Result<String, String> {
        match step {
            Step::Done(r) => r,
            other => panic!("not done: {other:?}"),
        }
    }

    #[test]
    fn every_tool_has_a_schema_and_is_known() {
        let defs = tool_defs();
        assert_eq!(defs.len(), 9);
        for def in &defs {
            assert!(def.name.starts_with("plugin_"));
            assert_eq!(def.schema["type"], "object");
            assert!(is_plugin_tool(def.name));
        }
        assert!(!is_plugin_tool("search"));
    }

    #[test]
    fn an_agent_makes_reads_and_writes_a_plugin() {
        let data = temp("flow");
        let cancel = AtomicBool::new(false);
        let made = answer(call(
            &data,
            "plugin_new",
            &json!({"id": "sample", "name": "Sample", "description": "A sample."}),
            &cancel,
        ))
        .unwrap();
        assert!(made.contains("plugin_build"));
        answer(call(
            &data,
            "plugin_write",
            &json!({"id": "sample", "path": "src/lib.rs", "content": "// new"}),
            &cancel,
        ))
        .unwrap();
        assert_eq!(
            answer(call(
                &data,
                "plugin_read",
                &json!({"id": "sample", "path": "src/lib.rs"}),
                &cancel
            ))
            .unwrap(),
            "// new"
        );
        assert_eq!(
            answer(call(
                &data,
                "plugin_files",
                &json!({"id": "sample"}),
                &cancel
            ))
            .unwrap(),
            "plugin.toml\nsrc/lib.rs"
        );
        assert!(
            answer(call(&data, "plugin_status", &json!({}), &cancel))
                .unwrap()
                .contains("sample: source only")
        );
        let unbuilt = answer(call(
            &data,
            "plugin_install",
            &json!({"id": "sample"}),
            &cancel,
        ))
        .unwrap_err();
        assert!(unbuilt.contains("build it first"));
        assert!(
            answer(call(
                &data,
                "plugin_write",
                &json!({"id": "sample"}),
                &cancel
            ))
            .unwrap_err()
            .contains("missing")
        );
        assert!(answer(call(&data, "nope", &json!({}), &cancel)).is_err());
        let _ = std::fs::remove_dir_all(&data);
    }

    /// The list of plugins unreadable: the agent is told so, and is not
    /// told its plugin is not installed — it would offer it again.
    #[test]
    fn status_says_what_it_cannot_know_when_the_list_is_unreadable() {
        let data = temp("status-unread");
        let cancel = AtomicBool::new(false);
        answer(call(
            &data,
            "plugin_new",
            &json!({"id": "sample", "name": "Sample", "description": "A sample."}),
            &cancel,
        ))
        .unwrap();
        std::fs::write(crate::store::store_path(&data), "[plugin.sample\nnot TOML").unwrap();
        let said = answer(call(&data, "plugin_status", &json!({}), &cancel)).unwrap();
        assert!(said.contains("could not read its list"), "{said}");
        assert!(
            said.contains("sample: source; whether it is installed is not known"),
            "{said}"
        );
        assert!(!said.contains("not installed"), "{said}");
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn try_and_install_are_handed_back_with_the_build() {
        let data = temp("handback");
        let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        let dir = authoring::build_dir(&data).join("views");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::copy(fixtures.join("views.wasm"), dir.join("views.wasm")).unwrap();
        // Its source, as built.
        let source = authoring::source_dir(&data, "views").unwrap();
        std::fs::create_dir_all(source.join("src")).unwrap();
        for file in ["plugin.toml", "src/lib.rs"] {
            std::fs::copy(fixtures.join("views").join(file), source.join(file)).unwrap();
        }
        crate::build::record_built_from(&dir, crate::build::source_digest(&source)).unwrap();
        let cancel = AtomicBool::new(false);
        match call(
            &data,
            "plugin_try",
            &json!({"id": "views", "command": "say", "line": 3}),
            &cancel,
        ) {
            Step::Try {
                built,
                command,
                line,
                ..
            } => {
                assert_eq!(built.inspected.manifest.plugin.id, "views");
                assert_eq!(command, "say");
                assert_eq!(line, Some(3));
            }
            other => panic!("{other:?}"),
        }
        let unknown = answer(call(
            &data,
            "plugin_try",
            &json!({"id": "views", "command": "nope"}),
            &cancel,
        ))
        .unwrap_err();
        assert!(unknown.contains("no command \"nope\""), "{unknown}");
        // A graph is tried by its id, in place of a command.
        match call(
            &data,
            "plugin_try",
            &json!({"id": "views", "graph": "pair"}),
            &cancel,
        ) {
            Step::Try { command, graph, .. } => {
                assert_eq!((command.as_str(), graph.as_deref()), ("", Some("pair")));
            }
            other => panic!("{other:?}"),
        }
        let unknown = answer(call(
            &data,
            "plugin_try",
            &json!({"id": "views", "graph": "nope"}),
            &cancel,
        ))
        .unwrap_err();
        assert!(
            unknown.contains("no graph \"nope\"") && unknown.contains("graph pair"),
            "{unknown}"
        );
        for both_or_neither in [
            json!({"id": "views"}),
            json!({"id": "views", "command": "say", "graph": "pair"}),
        ] {
            let refused = answer(call(&data, "plugin_try", &both_or_neither, &cancel)).unwrap_err();
            assert!(refused.contains("one of them"), "{refused}");
        }
        assert!(matches!(
            call(&data, "plugin_install", &json!({"id": "views"}), &cancel),
            Step::Install { .. }
        ));
        // Written to since it was built — or built since and that build
        // failed, which leaves this build — it is not what was written:
        // neither tried nor offered, so an older build is never offered as
        // an update.
        let lib = authoring::read_file(&data, "views", "src/lib.rs").unwrap();
        authoring::write_file(&data, "views", "src/lib.rs", &format!("{lib}\n// more\n")).unwrap();
        for (tool, args) in [
            ("plugin_try", json!({"id": "views", "command": "say"})),
            ("plugin_install", json!({"id": "views"})),
        ] {
            let stale = answer(call(&data, tool, &args, &cancel)).unwrap_err();
            assert!(stale.contains("build it again first"), "{tool}: {stale}");
        }
        // Put back as it was built, it is that build again.
        authoring::write_file(&data, "views", "src/lib.rs", &lib).unwrap();
        let built = match call(&data, "plugin_install", &json!({"id": "views"}), &cancel) {
            Step::Install { built } => built,
            other => panic!("{other:?}"),
        };
        // Installed, the very build is not asked about again: the agent is
        // told so, and told it is off when the user turned it off.
        crate::store::install(
            &data,
            &built.bytes,
            &built.inspected,
            crate::store::Origin::Ask,
        )
        .unwrap();
        let again = answer(call(
            &data,
            "plugin_install",
            &json!({"id": "views"}),
            &cancel,
        ))
        .unwrap();
        assert!(
            again.contains("already installed") && !again.contains("turned it off"),
            "{again}"
        );
        crate::store::set_enabled(&data, "views", false).unwrap();
        let off = answer(call(
            &data,
            "plugin_install",
            &json!({"id": "views"}),
            &cancel,
        ))
        .unwrap();
        assert!(off.contains("turned it off in Settings"), "{off}");
        // Its copy gone bad, installing it again is what mends it: asked.
        let record = crate::store::Store::load(&data).plugin["views"].clone();
        std::fs::write(
            crate::store::installed_path(&data, &record.fingerprint),
            b"bad",
        )
        .unwrap();
        assert!(matches!(
            call(&data, "plugin_install", &json!({"id": "views"}), &cancel),
            Step::Install { .. }
        ));
        let _ = std::fs::remove_dir_all(&data);
    }
}
