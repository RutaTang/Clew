//! The one way to read and write clew's global `config.toml`.
//!
//! Five unrelated features keep a section in this single file: the chat
//! provider and the embedding provider (both holding an API key), the theme,
//! the keymap, and the updater preference. Each used to do its own
//! read-parse-mutate-write, which made two independent bugs:
//!
//!   - **A parse failure read as "empty".** Every writer did
//!     `read_to_string().ok().and_then(toml::from_str).ok().unwrap_or_default()`,
//!     so a config that failed to parse — a hand edit with a typo, a partially
//!     written file — turned into an empty table. Saving the *theme* then wrote
//!     that empty table back with only `[appearance]` in it, silently deleting
//!     the user's API keys. A file we cannot parse is an error, never a blank
//!     slate: refusing the write keeps the bytes for the user to fix.
//!
//!   - **No mutual exclusion.** Two windows saving different sections at the
//!     same time both read the old file, and the later `rename` won — dropping
//!     the other's section. The write itself is atomic, which is what made this
//!     hard to see: nothing is ever torn, entries just vanish.
//!
//! [`update`] closes both: it holds an exclusive lock across the whole
//! read-modify-write, and propagates a parse error instead of defaulting.

use std::path::PathBuf;

/// The global config file (`<data_root>/config.toml`).
pub fn path() -> Option<PathBuf> {
    Some(crate::lsp::store::data_root()?.join("config.toml"))
}

/// The whole config as a table. `Ok(None)` when the file does not exist yet;
/// `Err` when it exists but cannot be read or parsed — callers that only want
/// a value should treat that as "no value", but callers about to WRITE must
/// not, or they would overwrite a file they failed to understand.
pub fn read() -> Result<Option<toml::Table>, String> {
    let path = path().ok_or("no data directory")?;
    let text = match crate::statefile::read_capped(&path, MAX_CONFIG_BYTES) {
        Some(text) => text,
        None if !path.exists() => return Ok(None),
        None => return Err(format!("{} is unreadable", path.display())),
    };
    toml::from_str(&text)
        .map(Some)
        .map_err(|e| format!("{} is not valid TOML: {e}", path.display()))
}

/// Byte cap for the global config. It holds a handful of small sections; a
/// larger file is not one clew wrote.
const MAX_CONFIG_BYTES: u64 = 4 * 1024 * 1024;

/// The `[section]` table of the global config, or `None` when absent (or the
/// file is missing or malformed — a reader has nothing better to do than fall
/// back to defaults).
pub fn section(name: &str) -> Option<toml::Table> {
    read().ok().flatten()?.get(name)?.as_table().cloned()
}

/// Apply `f` to the whole config and persist the result, leaving everything
/// `f` did not touch exactly as it was.
///
/// This is the ONLY writer. It holds an exclusive lock across the whole
/// read-modify-write, so a concurrent save of another section cannot be lost;
/// it refuses to write when the existing file cannot be parsed, rather than
/// treating it as empty and deleting what it could not read; and it always
/// writes user-only. That last part matters even for settings that are not
/// themselves secret: the theme, keymap and updater preferences share this
/// file with two API keys, and a plain `fs::write` from any of them republished
/// those keys with default permissions.
pub fn edit(f: impl FnOnce(&mut toml::Table)) -> Result<(), String> {
    let path = path().ok_or("no data directory")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let _guard = Lock::acquire(&path)?;
    let mut root = read()?.unwrap_or_default();
    f(&mut root);
    let text = toml::to_string(&root).map_err(|e| e.to_string())?;
    crate::statefile::write_atomic_secret(&path, text.as_bytes()).map_err(|e| e.to_string())
}

/// Replace one `[section]`, leaving every other section untouched.
pub fn update(name: &str, value: toml::Table) -> Result<(), String> {
    edit(|root| {
        root.insert(name.to_string(), toml::Value::Table(value));
    })
}

/// Write string `fields` into `[section]`, keeping any value that has changed
/// on disk since the writer read it. Each entry is `(key, previous, new)`,
/// where `previous` is what the caller saw when it took its snapshot. Returns
/// the keys that were KEPT — the caller's edits that were dropped because
/// someone else owns a newer value.
///
/// [`update`] replaces a whole section, which is right when the caller's copy
/// is fresh and wrong when it is not. The settings modal's is not: it reads
/// the AI sections when it OPENS and writes them back on Save, so a second
/// window that stored an API key in between had it overwritten with the blank
/// the first window's form still held — from a Save the user may have clicked
/// only to commit a theme change, since that is the same button. The lock in
/// [`edit`] cannot help there: both writers legitimately own `[llm]`, and the
/// loser is writing values it read minutes earlier. Comparing against
/// `previous` INSIDE the lock is what distinguishes "the user cleared this
/// field" from "the user never touched it".
pub fn update_fields(
    section: &str,
    fields: &[(&str, String, String)],
) -> Result<Vec<String>, String> {
    let mut kept = Vec::new();
    edit(|root| {
        let mut table = root
            .get(section)
            .and_then(|v| v.as_table())
            .cloned()
            .unwrap_or_default();
        for (key, previous, new) in fields {
            // An ABSENT key is nobody's value, so ours wins. This is not
            // pedantry: a form pre-fills a blank `model` with the provider
            // default, so its snapshot legitimately differs from the nothing
            // that is on disk, and treating that as a conflict would drop
            // every first save.
            let Some(on_disk) = table.get(*key).and_then(|v| v.as_str()) else {
                table.insert((*key).to_string(), new.clone().into());
                continue;
            };
            if on_disk != previous {
                // Someone else wrote this after our snapshot was taken. Their
                // value stands; ours was computed against a stale view.
                if on_disk != new {
                    kept.push((*key).to_string());
                }
                continue;
            }
            table.insert((*key).to_string(), new.clone().into());
        }
        root.insert(section.to_string(), toml::Value::Table(table));
    })?;
    Ok(kept)
}

/// Replace one `[section]`, or remove it entirely when `value` is `None` (an
/// empty override set should leave no section behind).
pub fn update_opt(name: &str, value: Option<toml::Table>) -> Result<(), String> {
    edit(|root| match value {
        Some(table) => {
            root.insert(name.to_string(), toml::Value::Table(table));
        }
        None => {
            root.remove(name);
        }
    })
}

/// Set top-level keys (the appearance and updater preferences are scalars at
/// the root, not sections).
pub fn set_keys(pairs: Vec<(String, toml::Value)>) -> Result<(), String> {
    edit(|root| {
        for (k, v) in pairs {
            root.insert(k, v);
        }
    })
}

/// A top-level key's value, or `None` when absent (or the file is missing or
/// malformed — a reader falls back to its default).
pub fn get(key: &str) -> Option<toml::Value> {
    read().ok().flatten()?.get(key).cloned()
}

/// An exclusive advisory lock on the config, held for one read-modify-write.
///
/// The lock lives on a sidecar file rather than on `config.toml` itself: the
/// write replaces the config by `rename`, so a lock held on the old inode
/// would guard a file that no longer exists at that name.
struct Lock {
    #[cfg(unix)]
    file: std::fs::File,
}

impl Lock {
    #[cfg(unix)]
    fn acquire(config: &std::path::Path) -> Result<Lock, String> {
        use std::os::unix::io::AsRawFd;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(config.with_extension("toml.lock"))
            .map_err(|e| format!("cannot lock the config: {e}"))?;
        // Blocking: the critical section is a few milliseconds of file I/O,
        // and failing the user's save because another window happened to be
        // saving would be worse than waiting for it.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(format!(
                "cannot lock the config: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(Lock { file })
    }

    /// No advisory locking here; the read-modify-write is unsynchronized, as
    /// it was everywhere before. The parse-failure guard still applies.
    #[cfg(not(unix))]
    fn acquire(_config: &std::path::Path) -> Result<Lock, String> {
        Ok(Lock {})
    }
}

#[cfg(unix)]
impl Drop for Lock {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd;
        unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialize the tests: `CLEW_DATA_DIR` is process-global. The lock has to
    /// be the crate-wide [`crate::env_lock`], not one private to this module —
    /// the config tests in `embed`, `llm`, `trust` and `lsp::store` read the
    /// very directory this helper repoints, and a second mutex serializes this
    /// module against itself while letting those run straight through it.
    fn with_data_dir<T>(name: &str, f: impl FnOnce() -> T) -> T {
        let _guard = crate::env_lock();
        let dir = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let prev = std::env::var_os("CLEW_DATA_DIR");
        // SAFETY: the mutex above makes this the only thread touching the env.
        unsafe { std::env::set_var("CLEW_DATA_DIR", &dir) };
        let out = f();
        match prev {
            Some(p) => unsafe { std::env::set_var("CLEW_DATA_DIR", p) },
            None => unsafe { std::env::remove_var("CLEW_DATA_DIR") },
        }
        out
    }

    fn table(pairs: &[(&str, &str)]) -> toml::Table {
        let mut t = toml::Table::new();
        for (k, v) in pairs {
            t.insert((*k).into(), toml::Value::String((*v).into()));
        }
        t
    }

    #[test]
    fn updating_one_section_preserves_the_others() {
        with_data_dir("clew-globalconfig-preserve", || {
            update("llm", table(&[("api_key", "sk-secret")])).unwrap();
            update("appearance", table(&[("theme", "dark")])).unwrap();
            update("keymap", table(&[("save", "cmd+s")])).unwrap();

            let root = read().unwrap().expect("config exists");
            assert_eq!(root["llm"]["api_key"].as_str(), Some("sk-secret"));
            assert_eq!(root["appearance"]["theme"].as_str(), Some("dark"));
            assert_eq!(root["keymap"]["save"].as_str(), Some("cmd+s"));
        });
    }

    /// The bug this module exists for: an unparseable config used to read as
    /// an empty table, so saving any one section wrote a file containing ONLY
    /// that section — deleting the user's API keys because a different
    /// section had a typo in it.
    #[test]
    fn a_malformed_config_is_never_silently_replaced() {
        with_data_dir("clew-globalconfig-malformed", || {
            let path = path().unwrap();
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let broken = "[llm]\napi_key = \"sk-secret\"\n[appearance\ntheme = ";
            std::fs::write(&path, broken).unwrap();

            let err = update("appearance", table(&[("theme", "dark")]))
                .expect_err("a config we cannot parse must not be overwritten");
            assert!(err.contains("not valid TOML"), "{err}");
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                broken,
                "the user's bytes must survive for them to fix"
            );
        });
    }

    /// Concurrent saves of different sections must not lose each other. Without
    /// the lock both writers read the same old file and the later rename won.
    #[test]
    fn concurrent_updates_of_different_sections_all_survive() {
        with_data_dir("clew-globalconfig-concurrent", || {
            update("llm", table(&[("api_key", "sk-secret")])).unwrap();
            std::thread::scope(|s| {
                for i in 0..8 {
                    s.spawn(move || {
                        let name = if i % 2 == 0 { "appearance" } else { "keymap" };
                        let _ = update(name, table(&[("n", "v")]));
                    });
                }
            });
            let root = read().unwrap().expect("config exists");
            assert_eq!(
                root["llm"]["api_key"].as_str(),
                Some("sk-secret"),
                "a section nobody wrote must survive every concurrent write"
            );
            assert!(root.contains_key("appearance"));
            assert!(root.contains_key("keymap"));
        });
    }

    /// Two windows with the Settings modal open write the SAME section, so the
    /// lock cannot help: the loser writes values it read when its modal
    /// opened. The field-level compare against that snapshot is what keeps the
    /// key the other window stored, while still applying the edit this one
    /// actually made.
    #[test]
    fn a_stale_settings_form_keeps_the_key_another_window_stored() {
        with_data_dir("clew-globalconfig-stale-form", || {
            // Window A opens Settings on a fresh install: no key, no model.
            let (snap_key, snap_model) = (String::new(), "claude".to_string());
            update_fields(
                "llm",
                &[
                    ("api_key", snap_key.clone(), snap_key.clone()),
                    ("model", snap_model.clone(), snap_model.clone()),
                ],
            )
            .unwrap();

            // Window B pastes the key and saves.
            update("llm", table(&[("api_key", "sk-real"), ("model", "claude")])).unwrap();

            // Window A, whose form still holds the blank key, changes only the
            // model and saves. What a whole-section write did:
            update("llm", table(&[("api_key", ""), ("model", "gpt-4")])).unwrap();
            assert_eq!(
                read().unwrap().unwrap()["llm"]["api_key"].as_str(),
                Some(""),
                "the whole-section write is what destroyed the key"
            );

            // The same save through the snapshot-aware writer.
            update("llm", table(&[("api_key", "sk-real"), ("model", "claude")])).unwrap();
            let kept = update_fields(
                "llm",
                &[
                    ("api_key", snap_key, String::new()),
                    ("model", snap_model, "gpt-4".into()),
                ],
            )
            .unwrap();
            let root = read().unwrap().expect("config exists");
            assert_eq!(
                root["llm"]["api_key"].as_str(),
                Some("sk-real"),
                "a field this form never touched must not be written back blank"
            );
            assert_eq!(
                root["llm"]["model"].as_str(),
                Some("gpt-4"),
                "the edit the user DID make must still land"
            );
            assert_eq!(kept, vec!["api_key".to_string()], "and it is reported");
        });
    }
}
