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
    /// Run the command `command` of `built` on the open project.
    Try {
        built: Box<Built>,
        command: String,
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
            description: "Run a command of the plugin's latest build on the open project and see what it gives back, to check it works. Nothing is shown to the user.",
            schema: json!({
                "type": "object",
                "properties": {
                    "id": id,
                    "command": {"type": "string", "description": "The command's id in plugin.toml."},
                    "file": {"type": "string", "description": "A file to run it on, from the project root, as if it were shown."},
                    "line": {"type": "integer", "description": "The caret's line in that file, from 1."},
                    "input": {"type": "string", "description": "What the user typed, for a command that asks for input."}
                },
                "required": ["id", "command"]
            }),
            read_only: true,
        },
        ToolDef {
            name: "plugin_install",
            description: "Ask the user, in Clew, to install the plugin's latest build. They see its name, description, commands and permissions, and decide. Answers whether they installed it.",
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
            let command = match text(args, "command") {
                Ok(c) => c.to_string(),
                Err(e) => return done(Err(e)),
            };
            match last_build(data, id) {
                Ok(built) => {
                    if built.inspected.manifest.command(&command).is_none() {
                        return done(Err(format!(
                            "the build of {id} has no command \"{command}\"; it has {}",
                            commands_list(&built.inspected)
                        )));
                    }
                    Step::Try {
                        built: Box::new(built),
                        command,
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
            Ok(built) => Step::Install {
                built: Box::new(built),
            },
            Err(e) => done(Err(e)),
        },
        "plugin_status" => done(Ok(status(data))),
        other => done(Err(format!("there is no plugin tool {other}"))),
    }
}

/// Build the plugin `id` and say what came of it, for the agent.
fn build_report(data: &Path, id: &str, cancel: &AtomicBool) -> Result<String, String> {
    let source = authoring::source_dir(data, id);
    if !source.join("plugin.toml").is_file() {
        return Err(format!(
            "there is no plugin \"{id}\"; make it first (plugin_new)"
        ));
    }
    let toolchain = build::find_toolchain().map_err(|e| e.report())?;
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

/// The plugin `id`'s latest build, as an install would read it.
pub fn last_build(data: &Path, id: &str) -> Result<Built, String> {
    let artifact = authoring::build_dir(data)
        .join(id)
        .join(format!("{id}.wasm"));
    let bytes = std::fs::read(&artifact).map_err(|_| {
        format!("the plugin \"{id}\" has not been built; build it first (plugin_build)")
    })?;
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
    let store = Store::load(data);
    let mut lines = Vec::new();
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

    #[test]
    fn try_and_install_are_handed_back_with_the_build() {
        let data = temp("handback");
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/views.wasm");
        let dir = authoring::build_dir(&data).join("views");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::copy(&fixture, dir.join("views.wasm")).unwrap();
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
        assert!(matches!(
            call(&data, "plugin_install", &json!({"id": "views"}), &cancel),
            Step::Install { .. }
        ));
        let _ = std::fs::remove_dir_all(&data);
    }
}
