//! What the user chose, in Settings, for each project's languages — its
//! Python interpreter — kept in clew's own data directory
//! (`<data>/languages.toml`), keyed by project the way the trust record is
//! ([`crate::derived::project_key`]).
//!
//! Never in the project: its `.clew/lsp.toml` travels with the repository
//! and is the team's, and the repository has no say over what the user
//! chose for their own machine. A remote project's choices are kept here
//! too, on the client, and sent with each request that sets a language up on
//! its host ([`clew_protocol::LanguageChoices`]).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use clew_protocol::LanguageChoices;
use serde::{Deserialize, Serialize};

/// On-disk shape of `<data_root>/languages.toml`: each project's choices,
/// by project key.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Record {
    #[serde(default)]
    projects: BTreeMap<String, LanguageChoices>,
}

/// Byte cap for `languages.toml`: a path per project. Anything larger is
/// not a file clew wrote.
const MAX_CHOICES_BYTES: u64 = 4 * 1024 * 1024;

/// Serializes [`save`] within this process (multi-window).
static SAVE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn path() -> Option<PathBuf> {
    Some(crate::lsp::store::data_root()?.join("languages.toml"))
}

/// The record as it is on disk: `Ok` empty when there is none yet, `Err`
/// when it cannot be read or parsed — never taken for an empty one by a
/// writer, which would drop every other project's choices.
fn load_checked() -> Result<Record, String> {
    let path = path().ok_or("no data directory")?;
    match crate::statefile::read_capped_checked(&path, MAX_CHOICES_BYTES) {
        Ok(None) => Ok(Record::default()),
        Ok(Some(text)) => {
            toml::from_str(&text).map_err(|e| format!("{} is not valid TOML: {e}", path.display()))
        }
        Err(e) => Err(format!("{} cannot be read: {e}", path.display())),
    }
}

/// The user's choices for the project at `root` on `host` (`None` = this
/// machine): none, when they made none or the record cannot be read.
pub fn load(host: Option<&str>, root: &Path) -> LanguageChoices {
    load_checked()
        .ok()
        .and_then(|mut record| {
            record
                .projects
                .remove(&crate::derived::project_key(host, root))
        })
        .unwrap_or_default()
}

/// Record `choices` for the project at `root` on `host`, replacing what was
/// there; choices of none are no entry. Re-reads the record under a lock and
/// writes it back atomically, so two windows saving at once each keep the
/// other's projects.
pub fn save(host: Option<&str>, root: &Path, choices: &LanguageChoices) -> Result<(), String> {
    let _serialized = SAVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let path = path().ok_or("no data directory")?;
    if let Some(dir) = path.parent() {
        crate::derived::ensure_private_dir(dir).map_err(|e| e.to_string())?;
    }
    let _exclusive = crate::statefile::lock(&path)
        .map_err(|e| format!("cannot lock {}: {e}", path.display()))?;
    let mut record = load_checked()?;
    let key = crate::derived::project_key(host, root);
    if *choices == LanguageChoices::default() {
        record.projects.remove(&key);
    } else {
        record.projects.insert(key, choices.clone());
    }
    let text = toml::to_string(&record).map_err(|e| e.to_string())?;
    crate::statefile::write_atomic(&path, text.as_bytes()).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{DataDir, TempDir};

    /// A choice is the project's, on its host: kept across loads, apart from
    /// another project's and from the same path on another host, and gone
    /// when cleared.
    #[test]
    fn a_choice_is_kept_per_project_and_host() {
        let _data = DataDir::new("choices-per-project");
        let dir = TempDir::new("choices-projects");
        let (a, b) = (dir.join("a"), dir.join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let choice = |p: &str| LanguageChoices {
            python: Some(p.to_string()),
            ..Default::default()
        };
        save(None, &a, &choice("/envs/a/bin/python")).unwrap();
        save(None, &b, &choice("/envs/b/bin/python")).unwrap();
        save(Some("box"), &a, &choice("/srv/bin/python")).unwrap();
        assert_eq!(load(None, &a), choice("/envs/a/bin/python"));
        assert_eq!(load(None, &b), choice("/envs/b/bin/python"));
        assert_eq!(load(Some("box"), &a), choice("/srv/bin/python"));
        // The same project by another spelling of its path.
        assert_eq!(load(None, &a.join("../a")), choice("/envs/a/bin/python"));

        save(None, &a, &LanguageChoices::default()).unwrap();
        assert_eq!(load(None, &a), LanguageChoices::default());
        assert_eq!(load(None, &b), choice("/envs/b/bin/python"));
    }

    /// Choices of every kind are kept as made — nested, with what is left
    /// unset left out — and read back the same.
    #[test]
    fn choices_of_every_kind_are_kept_as_made() {
        let _data = DataDir::new("choices-every-kind");
        let choices = LanguageChoices {
            python: None,
            rust: Some(clew_protocol::RustChoices {
                all_features: false,
                features: vec!["serde".into(), "cli/color".into()],
                no_default_features: true,
                target: None,
            }),
            go: Some(clew_protocol::GoChoices {
                tags: vec!["e2e".into()],
                goos: Some("windows".into()),
                goarch: None,
            }),
            c: Some(clew_protocol::CChoices {
                compile_commands: Some("/p/build".into()),
                flags: vec!["-DNAME=a b".into()],
            }),
            typescript: Some("/p/node_modules/typescript/lib/tsserver.js".into()),
            zig: None,
            servers: Some(
                [
                    (
                        "lean".to_string(),
                        clew_protocol::ServerChoice {
                            command: "/home/u/.elan/bin/lake".into(),
                            args: None,
                        },
                    ),
                    (
                        "lua".to_string(),
                        clew_protocol::ServerChoice {
                            command: "lua-language-server".into(),
                            args: Some(vec![]),
                        },
                    ),
                ]
                .into(),
            ),
        };
        save(None, Path::new("/p"), &choices).unwrap();
        assert_eq!(load(None, Path::new("/p")), choices);
    }

    /// A record that cannot be parsed is not written over: saving fails and
    /// leaves its bytes for the user to fix.
    #[test]
    fn an_unreadable_record_is_not_written_over() {
        let data = DataDir::new("choices-broken");
        let file = data.path().join("languages.toml");
        std::fs::write(&file, "projects = [not toml").unwrap();
        let err = save(None, Path::new("/p"), &LanguageChoices::default()).unwrap_err();
        assert!(err.contains("not valid TOML"), "{err}");
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "projects = [not toml"
        );
        assert_eq!(load(None, Path::new("/p")), LanguageChoices::default());
    }
}
