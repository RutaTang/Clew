//! `plugin.toml`: what a plugin is and what it adds. An agent writes it
//! beside the plugin's code; the build checks it ([`Manifest::parse`]) and
//! embeds it in the plugin's file (`inspect::embed`), where Clew reads it
//! before anything of the plugin runs.
//!
//! Every refusal names the field and says what it takes, in a sentence an
//! agent can act on.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// The most bytes a manifest may take.
pub const MAX_MANIFEST_BYTES: usize = 64 * 1024;
/// The most commands one plugin may add.
pub const MAX_COMMANDS: usize = 32;
/// The most graphs one plugin may add.
pub const MAX_GRAPHS: usize = 16;
/// The most crates a plugin may depend on.
pub const MAX_DEPENDENCIES: usize = 32;

/// The permissions a plugin may hold, as `plugin.toml` names them.
pub const PERMISSIONS: [&str; 2] = ["read", "git"];

/// `plugin.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub plugin: Info,
    #[serde(default, rename = "command", skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<Command>,
    #[serde(default, rename = "graph", skip_serializing_if = "Vec::is_empty")]
    pub graphs: Vec<GraphDecl>,
    /// crates.io crates and their version requirements (`regex = "1"`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub dependencies: BTreeMap<String, String>,
}

/// `[plugin]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Info {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    /// `read`, `git`: what the code uses, which the build checks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub permissions: Vec<String>,
    /// The name and description in other languages, by code (`zh`, `fr`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub translations: BTreeMap<String, Translation>,
}

/// `[plugin.translations.<lang>]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Translation {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Where a command is offered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Place {
    /// The command palette (and so voice and the agent).
    #[default]
    Palette,
    /// The palette, and the code's right-click menu: run there, it is told
    /// the file, the line and the selection.
    Code,
}

/// `[[command]]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Command {
    pub id: String,
    pub title: String,
    #[serde(default, rename = "where")]
    pub place: Place,
    /// Ask the user for a line of text before running, with this prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
    /// The title in other languages, by code.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub translations: BTreeMap<String, String>,
}

/// `[[graph]]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphDecl {
    pub id: String,
    pub title: String,
    /// One line on how to read it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// The title in other languages, by code.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub translations: BTreeMap<String, String>,
}

impl GraphDecl {
    /// Its title in `lang`, or as written.
    pub fn title_in(&self, lang: &str) -> &str {
        self.translations
            .get(lang)
            .map(String::as_str)
            .unwrap_or(&self.title)
    }
}

impl Manifest {
    /// Read and check `plugin.toml`'s text.
    pub fn parse(text: &str) -> Result<Manifest, String> {
        if text.len() > MAX_MANIFEST_BYTES {
            return Err(format!(
                "plugin.toml is {} bytes; it may take {MAX_MANIFEST_BYTES} at most",
                text.len()
            ));
        }
        let manifest: Manifest =
            toml::from_str(text).map_err(|e| format!("plugin.toml: {}", e.message()))?;
        manifest.check()?;
        Ok(manifest)
    }

    /// The manifest as `plugin.toml` text.
    pub fn to_toml(&self) -> String {
        // Every field is a string, a list or a table of them: this cannot
        // fail.
        toml::to_string(self).unwrap_or_default()
    }

    /// The permissions it declares, as bits (`clew_rules::plugin`).
    pub fn declared(&self) -> u8 {
        permission_bits(&self.plugin.permissions)
    }

    /// The command `id`, if it adds one.
    pub fn command(&self, id: &str) -> Option<&Command> {
        self.commands.iter().find(|c| c.id == id)
    }

    /// The graph `id`, if it adds one.
    pub fn graph(&self, id: &str) -> Option<&GraphDecl> {
        self.graphs.iter().find(|g| g.id == id)
    }

    /// Command `command`'s name in the command palette, in English, as an
    /// agent finds it there: "{plugin}: {command}", or the one name when
    /// both are the same.
    pub fn palette_title(&self, command: &Command) -> String {
        if command.title == self.plugin.name {
            command.title.clone()
        } else {
            format!("{}: {}", self.plugin.name, command.title)
        }
    }

    /// What an agent is told once the user installed this plugin: where
    /// each command and graph it adds now is.
    pub fn installed_words(&self) -> String {
        let quoted = |names: Vec<String>| {
            names
                .into_iter()
                .map(|n| format!("\"{n}\""))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mut said = format!("The user installed {}.", self.plugin.name);
        if !self.commands.is_empty() {
            said.push_str(&format!(
                " Its commands are in Clew's command palette as {}.",
                quoted(
                    self.commands
                        .iter()
                        .map(|c| self.palette_title(c))
                        .collect()
                )
            ));
        }
        if !self.graphs.is_empty() {
            said.push_str(&format!(
                " Its graphs are in Project Graphs as {}.",
                quoted(self.graphs.iter().map(|g| g.title.clone()).collect())
            ));
        }
        said
    }

    /// The plugin's name in `lang` (`zh`, `fr`, …), or as written.
    pub fn name_in(&self, lang: &str) -> &str {
        self.plugin
            .translations
            .get(lang)
            .and_then(|t| t.name.as_deref())
            .unwrap_or(&self.plugin.name)
    }

    /// The plugin's description in `lang`, or as written.
    pub fn description_in(&self, lang: &str) -> &str {
        self.plugin
            .translations
            .get(lang)
            .and_then(|t| t.description.as_deref())
            .unwrap_or(&self.plugin.description)
    }

    /// Every rule a manifest must keep.
    pub fn check(&self) -> Result<(), String> {
        let p = &self.plugin;
        check_id("[plugin] id", &p.id, 3)?;
        check_line("[plugin] name", &p.name, 60)?;
        check_version(&p.version)?;
        check_line("[plugin] description", &p.description, 300)?;
        let mut seen = BTreeSet::new();
        for permission in &p.permissions {
            if !PERMISSIONS.contains(&permission.as_str()) {
                return Err(format!(
                    "[plugin] permissions: \"{permission}\" is not one Clew grants; the permissions are {}",
                    quoted(&PERMISSIONS)
                ));
            }
            if !seen.insert(permission) {
                return Err(format!(
                    "[plugin] permissions: \"{permission}\" is listed twice"
                ));
            }
        }
        for (lang, t) in &p.translations {
            check_lang(&format!("[plugin.translations.{lang}]"), lang)?;
            if let Some(name) = &t.name {
                check_line(&format!("[plugin.translations.{lang}] name"), name, 60)?;
            }
            if let Some(description) = &t.description {
                check_line(
                    &format!("[plugin.translations.{lang}] description"),
                    description,
                    300,
                )?;
            }
        }

        if self.commands.is_empty() && self.graphs.is_empty() {
            return Err("plugin.toml adds nothing: give it a [[command]] (or a [[graph]])".into());
        }
        if self.commands.len() > MAX_COMMANDS {
            return Err(format!(
                "plugin.toml adds {} commands; a plugin may add {MAX_COMMANDS} at most",
                self.commands.len()
            ));
        }
        let (mut ids, mut titles) = (BTreeSet::new(), BTreeSet::new());
        for c in &self.commands {
            check_id("[[command]] id", &c.id, 1)?;
            check_line(&format!("[[command]] \"{}\" title", c.id), &c.title, 60)?;
            if let Some(input) = &c.input {
                check_line(&format!("[[command]] \"{}\" input", c.id), input, 80)?;
            }
            for (lang, title) in &c.translations {
                check_lang(&format!("[[command]] \"{}\" translations", c.id), lang)?;
                check_line(
                    &format!("[[command]] \"{}\" translations.{lang}", c.id),
                    title,
                    60,
                )?;
            }
            if !ids.insert(c.id.as_str()) {
                return Err(format!("two [[command]]s have the id \"{}\"", c.id));
            }
            if !titles.insert(c.title.as_str()) {
                return Err(format!("two [[command]]s have the title \"{}\"", c.title));
            }
        }

        if self.graphs.len() > MAX_GRAPHS {
            return Err(format!(
                "plugin.toml adds {} graphs; a plugin may add {MAX_GRAPHS} at most",
                self.graphs.len()
            ));
        }
        let mut graph_ids = BTreeSet::new();
        for g in &self.graphs {
            check_id("[[graph]] id", &g.id, 1)?;
            check_line(&format!("[[graph]] \"{}\" title", g.id), &g.title, 60)?;
            for (lang, title) in &g.translations {
                check_lang(&format!("[[graph]] \"{}\" translations", g.id), lang)?;
                check_line(
                    &format!("[[graph]] \"{}\" translations.{lang}", g.id),
                    title,
                    60,
                )?;
            }
            if !g.description.is_empty() {
                check_line(
                    &format!("[[graph]] \"{}\" description", g.id),
                    &g.description,
                    200,
                )?;
            }
            if !graph_ids.insert(g.id.as_str()) {
                return Err(format!("two [[graph]]s have the id \"{}\"", g.id));
            }
        }

        if self.dependencies.len() > MAX_DEPENDENCIES {
            return Err(format!(
                "[dependencies] lists {} crates; a plugin may use {MAX_DEPENDENCIES} at most",
                self.dependencies.len()
            ));
        }
        for (name, version) in &self.dependencies {
            check_crate(name, version)?;
            // Cargo would take a crates.io crate of the plugin's own name
            // for a second copy of it.
            if name.replace('_', "-") == self.plugin.id {
                return Err(format!(
                    "[dependencies] lists \"{name}\", the plugin's own id: a plugin cannot depend on a crate named like itself. Give the plugin another id."
                ));
            }
        }
        Ok(())
    }
}

/// `names` (as `plugin.toml` spells permissions) as bits; a name that is not
/// a permission adds none.
pub fn permission_bits<S: AsRef<str>>(names: &[S]) -> u8 {
    names.iter().fold(0, |bits, name| {
        bits | match name.as_ref() {
            "read" => clew_rules::plugin::READ,
            "git" => clew_rules::plugin::GIT,
            _ => 0,
        }
    })
}

/// The permission names of `bits`, in [`PERMISSIONS`]' order.
pub fn permission_names(bits: u8) -> Vec<String> {
    let mut names = Vec::new();
    if bits & clew_rules::plugin::READ != 0 {
        names.push("read".to_string());
    }
    if bits & clew_rules::plugin::GIT != 0 {
        names.push("git".to_string());
    }
    names
}

fn quoted(names: &[&str]) -> String {
    names
        .iter()
        .map(|n| format!("\"{n}\""))
        .collect::<Vec<_>>()
        .join(", ")
}

/// An id: `a`-`z`, `0`-`9` and `-`, starting with a letter, not ending with
/// or doubling `-`, `min` to 40 characters.
fn check_id(field: &str, id: &str, min: usize) -> Result<(), String> {
    if clew_rules::plugin::id_ok(id.as_bytes(), min) {
        Ok(())
    } else {
        Err(format!(
            "{field} \"{id}\": an id is {min} to 40 of a-z, 0-9 and -, starting with a letter (\"todo-authors\")"
        ))
    }
}

/// One line of text: not empty, no control characters, `max` characters at
/// most.
fn check_line(field: &str, text: &str, max: usize) -> Result<(), String> {
    if text.trim().is_empty() {
        return Err(format!("{field} is empty"));
    }
    if text.chars().any(char::is_control) {
        return Err(format!(
            "{field} has a line break or a control character; it is one line"
        ));
    }
    let n = text.chars().count();
    if n > max {
        return Err(format!(
            "{field} is {n} characters; it may be {max} at most"
        ));
    }
    Ok(())
}

/// A version as Cargo takes one: `1.2.3`, a pre-release or build after it.
fn check_version(version: &str) -> Result<(), String> {
    let (core, _) = version.split_once(['-', '+']).unwrap_or((version, ""));
    let parts: Vec<&str> = core.split('.').collect();
    let numbers = parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.len() <= 9 && p.bytes().all(|b| b.is_ascii_digit()));
    let rest = version
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"-+.".contains(&b));
    if numbers && rest && version.len() <= 40 {
        Ok(())
    } else {
        Err(format!(
            "[plugin] version \"{version}\": a version is three numbers, as \"0.1.0\""
        ))
    }
}

/// How plugin version `a` stands to `b`, as Cargo orders versions: by
/// their three numbers, then a pre-release (`1.0.0-beta`) before its
/// release; what follows a `+` is not compared. Both as [`check_version`]
/// takes them.
pub fn version_order(a: &str, b: &str) -> std::cmp::Ordering {
    fn parts(version: &str) -> ([u64; 3], Option<&str>) {
        let version = version.split_once('+').map_or(version, |(v, _)| v);
        let (core, pre) = match version.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (version, None),
        };
        let mut numbers = [0; 3];
        for (slot, part) in numbers.iter_mut().zip(core.split('.')) {
            *slot = part.parse().unwrap_or(0);
        }
        (numbers, pre)
    }
    let (a_numbers, a_pre) = parts(a);
    let (b_numbers, b_pre) = parts(b);
    a_numbers
        .cmp(&b_numbers)
        .then_with(|| match (a_pre, b_pre) {
            (None, None) => std::cmp::Ordering::Equal,
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (Some(a), Some(b)) => a.cmp(b),
        })
}

/// A language code: `zh`, `fr`, `pt-BR`.
fn check_lang(field: &str, lang: &str) -> Result<(), String> {
    let ok = (2..=8).contains(&lang.len())
        && lang.starts_with(|c: char| c.is_ascii_lowercase())
        && lang.bytes().all(|b| b.is_ascii_alphabetic() || b == b'-');
    if ok {
        Ok(())
    } else {
        Err(format!(
            "{field}: \"{lang}\" is not a language code (\"zh\", \"fr\", \"pt-BR\")"
        ))
    }
}

/// A crates.io crate and a version requirement for it.
fn check_crate(name: &str, version: &str) -> Result<(), String> {
    let named = (1..=64).contains(&name.len())
        && name.starts_with(|c: char| c.is_ascii_alphabetic())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !named {
        return Err(format!("[dependencies] \"{name}\" is not a crate name"));
    }
    if name == "clew-plugin-sdk" || name == "clew_plugin_sdk" {
        return Err(
            "[dependencies] lists clew-plugin-sdk: Clew adds it itself; leave it out".into(),
        );
    }
    let requirement = !version.trim().is_empty()
        && version.len() <= 40
        && version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b" .,*^~<>=-+".contains(&b));
    if requirement {
        Ok(())
    } else {
        Err(format!(
            "[dependencies] {name} = \"{version}\": give a version requirement, as \"1\" or \"0.4.2\" (crates.io only: no path, git or features)"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"
[plugin]
id = "todo-authors"
name = "TODOs by author"
version = "0.1.0"
description = "Lists TODO comments by who last changed the line."
permissions = ["read", "git"]

[plugin.translations.zh]
name = "按作者列出 TODO"

[[command]]
id = "list"
title = "TODOs by author"
where = "code"
translations = { zh = "按作者列出 TODO" }

[[graph]]
id = "owners"
title = "Owners"

[dependencies]
regex = "1"
"#;

    #[test]
    fn versions_are_ordered_as_cargo_orders_them() {
        use std::cmp::Ordering::{Equal, Greater, Less};
        assert_eq!(version_order("0.2.0", "0.10.0"), Less);
        assert_eq!(version_order("1.0.0", "0.9.9"), Greater);
        assert_eq!(version_order("1.0.0-beta", "1.0.0"), Less);
        assert_eq!(version_order("1.0.0", "1.0.0-rc.1"), Greater);
        assert_eq!(version_order("1.0.0-alpha", "1.0.0-beta"), Less);
        assert_eq!(version_order("1.2.3+build.5", "1.2.3"), Equal);
    }

    #[test]
    fn a_whole_manifest_reads_and_round_trips() {
        let m = Manifest::parse(GOOD).unwrap();
        assert_eq!(m.plugin.id, "todo-authors");
        assert_eq!(m.commands[0].place, Place::Code);
        assert_eq!(m.command("list").unwrap().title, "TODOs by author");
        assert_eq!(m.graph("owners").unwrap().title, "Owners");
        assert_eq!(m.name_in("zh"), "按作者列出 TODO");
        assert_eq!(m.name_in("fr"), "TODOs by author");
        assert_eq!(
            m.declared(),
            clew_rules::plugin::READ | clew_rules::plugin::GIT
        );
        assert_eq!(Manifest::parse(&m.to_toml()).unwrap(), m);
    }

    #[test]
    fn a_command_is_offered_in_the_palette_unless_said() {
        let m = Manifest::parse(
            "[plugin]\nid = \"abc\"\nname = \"A\"\nversion = \"1.0.0\"\ndescription = \"d\"\n[[command]]\nid = \"x\"\ntitle = \"X\"\n",
        )
        .unwrap();
        assert_eq!(m.commands[0].place, Place::Palette);
        assert_eq!(m.declared(), 0);
    }

    fn refusal(edit: impl Fn(&mut Manifest)) -> String {
        let mut m = Manifest::parse(GOOD).unwrap();
        edit(&mut m);
        m.check().unwrap_err()
    }

    #[test]
    fn each_rule_refuses_with_the_field_named() {
        assert!(refusal(|m| m.plugin.id = "Todo".into()).contains("[plugin] id"));
        assert!(refusal(|m| m.plugin.id = "ab".into()).contains("3 to 40"));
        assert!(refusal(|m| m.plugin.id = "a--b".into()).contains("[plugin] id"));
        assert!(refusal(|m| m.plugin.id = "abc-".into()).contains("[plugin] id"));
        assert!(refusal(|m| m.plugin.name = " ".into()).contains("name is empty"));
        assert!(refusal(|m| m.plugin.name = "a\nb".into()).contains("one line"));
        assert!(refusal(|m| m.plugin.name = "x".repeat(61)).contains("60 at most"));
        assert!(refusal(|m| m.plugin.version = "1.0".into()).contains("three numbers"));
        assert!(
            refusal(|m| m.plugin.permissions = vec!["net".into()]).contains("\"read\", \"git\"")
        );
        assert!(
            refusal(|m| m.plugin.permissions = vec!["read".into(), "read".into()])
                .contains("twice")
        );
        assert!(
            refusal(|m| {
                m.commands.clear();
                m.graphs.clear();
            })
            .contains("adds nothing")
        );
        assert!(
            refusal(|m| {
                let again = m.commands[0].clone();
                m.commands.push(again);
            })
            .contains("id \"list\"")
        );
        assert!(
            refusal(|m| {
                let mut other = m.commands[0].clone();
                other.id = "other".into();
                m.commands.push(other);
            })
            .contains("title")
        );
        assert!(
            refusal(|m| {
                m.dependencies
                    .insert("clew-plugin-sdk".into(), "0.1".into());
            })
            .contains("adds it itself")
        );
        assert!(
            refusal(|m| {
                m.dependencies
                    .insert("serde".into(), "{ path = \"..\" }".into());
            })
            .contains("crates.io only")
        );
        assert!(
            refusal(|m| {
                m.commands[0].translations.insert("Z!".into(), "x".into());
            })
            .contains("language code")
        );
    }

    #[test]
    fn unknown_fields_and_bad_toml_are_refused() {
        let extra = GOOD.replace(
            "[dependencies]",
            "[profile.release]\nlto = true\n[dependencies]",
        );
        assert!(Manifest::parse(&extra).unwrap_err().contains("plugin.toml"));
        let build = GOOD.replace("permissions =", "build = \"build.rs\"\npermissions =");
        assert!(Manifest::parse(&build).is_err());
        assert!(Manifest::parse("[plugin").is_err());
        assert!(Manifest::parse(&"#".repeat(MAX_MANIFEST_BYTES + 1)).is_err());
    }

    #[test]
    fn permissions_and_bits_agree() {
        assert_eq!(permission_bits(&["git"]), clew_rules::plugin::GIT);
        assert_eq!(
            permission_names(clew_rules::plugin::READ | clew_rules::plugin::GIT),
            ["read", "git"]
        );
        assert_eq!(permission_names(0), Vec::<String>::new());
    }
}

/// A manifest for tests elsewhere in the crate.
#[cfg(test)]
pub(crate) mod tests_support {
    use super::Manifest;

    /// A plugin `sample` with one command, `hello`, and no permissions.
    pub(crate) fn manifest() -> Manifest {
        Manifest::parse(
            "[plugin]\nid = \"sample\"\nname = \"Sample\"\nversion = \"0.1.0\"\ndescription = \"A sample.\"\n\n[[command]]\nid = \"hello\"\ntitle = \"Say hello\"\n",
        )
        .unwrap()
    }
    #[test]
    fn a_dependency_may_not_be_named_like_the_plugin() {
        for named in ["todo-authors", "todo_authors"] {
            let text = format!(
                "[plugin]\nid = \"todo-authors\"\nname = \"T\"\nversion = \"0.1.0\"\ndescription = \"d\"\n\n[[command]]\nid = \"a\"\ntitle = \"A\"\n\n[dependencies]\n{named} = \"1\"\n"
            );
            let why = Manifest::parse(&text).unwrap_err();
            assert!(why.contains("the plugin's own id"), "{why}");
        }
    }
    #[test]
    fn an_agent_is_told_where_what_was_installed_is() {
        let text = "[plugin]\nid = \"todo\"\nname = \"TODOs\"\nversion = \"0.1.0\"\ndescription = \"d\"\n\n\
                    [[command]]\nid = \"list\"\ntitle = \"List TODOs\"\n\n\
                    [[command]]\nid = \"all\"\ntitle = \"TODOs\"\n\n\
                    [[graph]]\nid = \"owners\"\ntitle = \"TODO owners\"\n";
        let m = Manifest::parse(text).unwrap();
        assert_eq!(
            m.installed_words(),
            "The user installed TODOs. Its commands are in Clew's command palette as \"TODOs: List TODOs\", \"TODOs\". Its graphs are in Project Graphs as \"TODO owners\"."
        );
    }
}
