//! What an agent writes a plugin with: a plugin's source made from a
//! template, its files read and written (in its own folder and nowhere
//! else), and the guide (`GUIDE.md`) that says how.
//!
//! A plugin's source is `<data>/plugins/source/<id>/`: `plugin.toml` and
//! `src/**/*.rs`. Plugins are the user's, not a project's, so the source is
//! not in the project; an agent of any kind reaches it through these
//! functions, whatever folders its own tools may write.

use std::path::{Path, PathBuf};

use crate::manifest::Manifest;

/// What an agent reads before writing a plugin: the API, the manifest, the
/// SDK, examples, and what is refused and why.
pub const GUIDE: &str = include_str!("../GUIDE.md");

/// The largest file of a plugin's source an agent may write.
pub const MAX_WRITE_BYTES: usize = 1024 * 1024;

/// Where plugin sources are.
pub fn sources_dir(data: &Path) -> PathBuf {
    crate::store::plugins_dir(data).join("source")
}

/// Where the plugin `id`'s source is; refused for what is not an id.
pub fn source_dir(data: &Path, id: &str) -> Result<PathBuf, String> {
    Ok(sources_dir(data).join(crate::store::checked_id(id)?))
}

/// Where plugins are built (`build::build`'s `work`).
pub fn build_dir(data: &Path) -> PathBuf {
    crate::store::plugins_dir(data).join("build")
}

/// The ids of the plugins there is source for, sorted.
pub fn sources(data: &Path) -> Vec<String> {
    let mut ids: Vec<String> = std::fs::read_dir(sources_dir(data))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().join("plugin.toml").is_file())
        .filter_map(|e| e.file_name().to_str().map(String::from))
        .collect();
    ids.sort();
    ids
}

/// Make the source of a new plugin `id` from the template: a `plugin.toml`
/// with one command, and a `src/lib.rs` that answers it. Refused if there is
/// one already — it is edited, not made again.
pub fn new_plugin(data: &Path, id: &str, name: &str, description: &str) -> Result<PathBuf, String> {
    let manifest = template_manifest(id, name, description);
    let toml = manifest.to_toml();
    // The template is checked as a build would check it, so a bad id or
    // name is refused here, not at the build.
    Manifest::parse(&toml)?;
    let dir = source_dir(data, id)?;
    if dir.exists() {
        return Err(format!(
            "a plugin \"{id}\" exists already; change its files instead (plugin_read, plugin_write)"
        ));
    }
    std::fs::create_dir_all(dir.join("src")).map_err(|e| e.to_string())?;
    std::fs::write(dir.join("plugin.toml"), toml).map_err(|e| e.to_string())?;
    std::fs::write(dir.join("src/lib.rs"), TEMPLATE_LIB).map_err(|e| e.to_string())?;
    Ok(dir)
}

fn template_manifest(id: &str, name: &str, description: &str) -> Manifest {
    let text = format!(
        "[plugin]\nid = {}\nname = {}\nversion = \"0.1.0\"\ndescription = {}\n\n[[command]]\nid = \"run\"\ntitle = {}\n",
        toml_string(id),
        toml_string(name),
        toml_string(description),
        toml_string(name),
    );
    toml::from_str(&text).unwrap_or_else(|_| Manifest {
        plugin: crate::manifest::Info {
            id: id.into(),
            name: name.into(),
            version: "0.1.0".into(),
            description: description.into(),
            permissions: Vec::new(),
            translations: Default::default(),
        },
        commands: Vec::new(),
        graphs: Vec::new(),
        dependencies: Default::default(),
    })
}

/// `text` as a TOML string.
fn toml_string(text: &str) -> String {
    toml::Value::String(text.to_string()).to_string()
}

const TEMPLATE_LIB: &str = r#"//! A Clew plugin. GUIDE: plugin_guide.

use clew_plugin_sdk::{Invocation, Outcome, Plugin, View};

struct ThisPlugin;

impl Plugin for ThisPlugin {
    fn run(command: &str, _context: Invocation) -> Result<Outcome, String> {
        match command {
            "run" => Ok(Outcome::Show(View::new("Hello").text("This plugin works."))),
            other => Err(format!("no command {other}")),
        }
    }
}

clew_plugin_sdk::export!(ThisPlugin);
"#;

/// How deep a plugin's source may go: `src/` and eight folders under it.
const MAX_DEPTH: usize = 9;

/// The file `rel` of a plugin's source, and whether it is the manifest:
/// `plugin.toml`, or a Rust file under `src/` (`clew_rules::plugin::source_kind`,
/// which is proved to keep it under the plugin's folder). Anything else is
/// refused: a plugin's source is nothing more. `rel` is read as written,
/// spaces and a leading `./` aside.
fn source_file(data: &Path, id: &str, rel: &str) -> Result<(PathBuf, bool), String> {
    use clew_rules::plugin::{SOURCE_MANIFEST, SOURCE_RUST, source_kind};
    let dir = source_dir(data, id)?;
    if !dir.join("plugin.toml").is_file() {
        return Err(format!(
            "there is no plugin \"{id}\"; make it first (plugin_new)"
        ));
    }
    let rel = rel.trim();
    let rel = rel.strip_prefix("./").unwrap_or(rel);
    let kind = source_kind(rel.as_bytes());
    let deep = rel.split('/').count() > MAX_DEPTH;
    if kind == SOURCE_MANIFEST || (kind == SOURCE_RUST && !deep) {
        Ok((dir.join(rel), kind == SOURCE_MANIFEST))
    } else {
        Err(format!(
            "\"{rel}\" is not a file of a plugin: write plugin.toml, or Rust files under src/ (letters, digits, _ and - in their names, no piece starting with .)"
        ))
    }
}

/// Read the file `rel` of the plugin `id`'s source.
pub fn read_file(data: &Path, id: &str, rel: &str) -> Result<String, String> {
    let (path, _) = source_file(data, id, rel)?;
    match clew_core::statefile::read_capped_checked(&path, MAX_WRITE_BYTES as u64) {
        Ok(Some(text)) => Ok(text),
        Ok(None) => Err(format!("{rel} does not exist")),
        Err(e) => Err(format!("{rel}: {e}")),
    }
}

/// Write the file `rel` of the plugin `id`'s source. `plugin.toml` is
/// checked before it is written, so a broken one never replaces a good one.
pub fn write_file(data: &Path, id: &str, rel: &str, contents: &str) -> Result<(), String> {
    if contents.len() > MAX_WRITE_BYTES {
        return Err(format!(
            "{rel} is {} KB; a file may be {} KB at most",
            contents.len() / 1024,
            MAX_WRITE_BYTES / 1024
        ));
    }
    let (path, manifest) = source_file(data, id, rel)?;
    if manifest {
        let manifest = Manifest::parse(contents)?;
        if manifest.plugin.id != id {
            return Err(format!(
                "plugin.toml says id = \"{}\", but this is the plugin \"{id}\"; an id cannot change",
                manifest.plugin.id
            ));
        }
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(format!("{rel} is a symlink; it is not written through"));
    }
    clew_core::statefile::write_atomic(&path, contents.as_bytes()).map_err(|e| e.to_string())
}

/// The files of the plugin `id`'s source, by path, sorted.
pub fn list_files(data: &Path, id: &str) -> Result<Vec<String>, String> {
    let dir = source_dir(data, id)?;
    if !dir.join("plugin.toml").is_file() {
        return Err(format!("there is no plugin \"{id}\""));
    }
    let mut files = vec!["plugin.toml".to_string()];
    walk(&dir.join("src"), "src", 0, &mut files);
    files.sort();
    Ok(files)
}

fn walk(dir: &Path, rel: &str, depth: usize, out: &mut Vec<String>) {
    if depth > 8 {
        return;
    }
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let Some(name) = entry.file_name().to_str().map(String::from) else {
            continue;
        };
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            walk(&entry.path(), &format!("{rel}/{name}"), depth + 1, out);
        } else if kind.is_file() && name.ends_with(".rs") {
            out.push(format!("{rel}/{name}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "clew-plugin-authoring-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_new_plugin_is_made_from_the_template_once() {
        let data = temp("new");
        let dir = new_plugin(&data, "todo-authors", "TODOs \"by\" author", "Lists them.").unwrap();
        let manifest =
            Manifest::parse(&std::fs::read_to_string(dir.join("plugin.toml")).unwrap()).unwrap();
        assert_eq!(manifest.plugin.name, "TODOs \"by\" author");
        assert_eq!(manifest.commands[0].id, "run");
        assert!(dir.join("src/lib.rs").is_file());
        assert!(
            new_plugin(&data, "todo-authors", "x", "y")
                .unwrap_err()
                .contains("exists already")
        );
        assert!(new_plugin(&data, "Bad Id", "x", "y").is_err());
        assert_eq!(sources(&data), ["todo-authors"]);
        assert_eq!(
            list_files(&data, "todo-authors").unwrap(),
            ["plugin.toml", "src/lib.rs"]
        );
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn only_the_plugins_own_files_are_written() {
        let data = temp("write");
        new_plugin(&data, "sample", "Sample", "A sample.").unwrap();
        write_file(&data, "sample", "src/views/list.rs", "// list").unwrap();
        assert_eq!(
            read_file(&data, "sample", "src/views/list.rs").unwrap(),
            "// list"
        );
        for bad in [
            "../other/plugin.toml",
            "src/../../escape.rs",
            "/etc/passwd",
            "build.rs",
            "Cargo.toml",
            ".cargo/config.toml",
            "src/notes.txt",
            "src/.hidden.rs",
            "rust-toolchain.toml",
        ] {
            assert!(write_file(&data, "sample", bad, "x").is_err(), "{bad}");
        }
        assert!(
            write_file(&data, "nope", "src/lib.rs", "x")
                .unwrap_err()
                .contains("no plugin")
        );
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn a_broken_plugin_toml_never_replaces_a_good_one() {
        let data = temp("manifest");
        new_plugin(&data, "sample", "Sample", "A sample.").unwrap();
        let good = read_file(&data, "sample", "plugin.toml").unwrap();
        assert!(write_file(&data, "sample", "plugin.toml", "[plugin\n").is_err());
        let renamed = good.replace("id = \"sample\"", "id = \"other\"");
        assert!(
            write_file(&data, "sample", "plugin.toml", &renamed)
                .unwrap_err()
                .contains("cannot change")
        );
        assert_eq!(read_file(&data, "sample", "plugin.toml").unwrap(), good);
        // Written as `./plugin.toml`, it is the same file, checked the same
        // way: the leading `./` once let a broken or renamed manifest through.
        for spelled in ["./plugin.toml", " ./plugin.toml "] {
            assert!(write_file(&data, "sample", spelled, "[plugin\n").is_err());
            assert!(
                write_file(&data, "sample", spelled, &renamed)
                    .unwrap_err()
                    .contains("cannot change"),
                "{spelled:?}"
            );
        }
        assert_eq!(read_file(&data, "sample", "plugin.toml").unwrap(), good);
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn a_plugin_id_is_never_a_path() {
        let data = temp("ids");
        for id in ["../escape", "a/b", "..", "/etc", "Sample", ""] {
            assert!(source_dir(&data, id).is_err(), "{id:?}");
            assert!(read_file(&data, id, "plugin.toml").is_err(), "{id:?}");
            assert!(write_file(&data, id, "src/lib.rs", "x").is_err(), "{id:?}");
            assert!(list_files(&data, id).is_err(), "{id:?}");
            assert!(crate::tools::last_build(&data, id).is_err(), "{id:?}");
        }
        assert!(!data.join("escape").exists());
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn the_guide_covers_what_an_agent_needs() {
        for needed in [
            "plugin.toml",
            "export!",
            "Outcome::Show",
            "project::read",
            "permissions",
            "plugin_build",
        ] {
            assert!(GUIDE.contains(needed), "GUIDE.md says nothing of {needed}");
        }
    }
}
