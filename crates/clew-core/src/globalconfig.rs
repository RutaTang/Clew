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

    /// Serialize the tests: `CLEW_DATA_DIR` is process-global.
    fn with_data_dir<T>(name: &str, f: impl FnOnce() -> T) -> T {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
}
