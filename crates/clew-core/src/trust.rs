//! Workspace trust: which project roots the user has allowed clew to open, and
//! which repo-specified language-server commands they have allowed it to run.
//!
//! Both records live in clew's **global data directory**, never inside the
//! project. A project's own files are attacker-controlled when the repository
//! is untrusted, so consent recorded there would let a repository grant itself
//! permission — the very thing consent exists to prevent.
//!
//! Roots are keyed by their canonical path, so a symlinked or relative path to
//! an already-trusted project resolves to the same entry.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// On-disk shape of `<data_root>/trust.toml`.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Trust {
    /// Canonical project roots the user allowed clew to open.
    #[serde(default)]
    roots: Vec<String>,
    /// Approved language-server command lines, keyed by canonical project root:
    /// `root -> { language -> command hash }`. A change to the command, its
    /// arguments, or the server/version it resolves to invalidates the entry.
    #[serde(default)]
    lsp: BTreeMap<String, BTreeMap<String, String>>,
}

fn trust_path() -> Option<PathBuf> {
    Some(crate::lsp::store::data_root()?.join("trust.toml"))
}

/// The canonical form of `root`, used as its key. Falls back to the path as
/// given when it cannot be canonicalized (a root that no longer exists).
pub fn key_of(root: &Path) -> String {
    root.canonicalize()
        .unwrap_or_else(|_| root.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

impl Trust {
    pub fn load() -> Trust {
        trust_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<(), String> {
        let path = trust_path().ok_or("no data directory")?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let text = toml::to_string(self).map_err(|e| e.to_string())?;
        std::fs::write(&path, text).map_err(|e| e.to_string())
    }

    /// Whether the user has allowed clew to open this project.
    pub fn is_root_trusted(&self, root: &Path) -> bool {
        let key = key_of(root);
        self.roots.contains(&key)
    }

    /// Record consent for a project root.
    pub fn trust_root(&mut self, root: &Path) {
        let key = key_of(root);
        if !self.roots.contains(&key) {
            self.roots.push(key);
        }
    }

    /// Forget a project root and every language-server approval under it.
    pub fn forget_root(&mut self, root: &Path) {
        let key = key_of(root);
        self.roots.retain(|r| *r != key);
        self.lsp.remove(&key);
    }

    /// Every trusted root, for the settings list.
    pub fn roots(&self) -> &[String] {
        &self.roots
    }

    /// Whether this exact command line was approved for `language` in `root`.
    pub fn is_lsp_approved(&self, root: &Path, language: &str, fingerprint: &str) -> bool {
        self.lsp
            .get(&key_of(root))
            .and_then(|m| m.get(language))
            .is_some_and(|h| h == fingerprint)
    }

    /// Approve one language-server command line for this project.
    pub fn approve_lsp(&mut self, root: &Path, language: &str, fingerprint: &str) {
        self.lsp
            .entry(key_of(root))
            .or_default()
            .insert(language.to_string(), fingerprint.to_string());
    }
}

/// A stable fingerprint of what would actually be executed. Any change to the
/// binary, its arguments, or the server/version it came from invalidates a
/// previous approval, so an edited `lsp.toml` must be approved again.
///
/// SHA-256, not a general-purpose hash: this value decides whether a command
/// runs without asking, so it must be collision-resistant (an attacker picks
/// the input) and stable across toolchain versions (`DefaultHasher` is neither).
pub fn lsp_fingerprint(command: &Path, args: &[String], server: &str, version: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    // Length-prefix each field so no rearrangement of the parts collides.
    for part in [
        command.to_string_lossy().as_ref(),
        &args.join("\u{1e}"),
        server,
        version,
    ] {
        h.update((part.len() as u64).to_le_bytes());
        h.update(part.as_bytes());
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_data_dir<T>(name: &str, f: impl FnOnce(&Path) -> T) -> T {
        let _env = crate::env_lock();
        let dir = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // SAFETY: env mutation serialized by env_lock.
        unsafe { std::env::set_var("CLEW_DATA_DIR", &dir) };
        let out = f(&dir);
        unsafe { std::env::remove_var("CLEW_DATA_DIR") };
        out
    }

    #[test]
    fn roots_round_trip_and_are_canonical() {
        with_data_dir("clew-trust-roots", |dir| {
            let project = dir.join("proj");
            std::fs::create_dir_all(project.join("sub")).unwrap();

            let mut t = Trust::load();
            assert!(!t.is_root_trusted(&project));
            t.trust_root(&project);
            t.save().unwrap();

            // A different spelling of the same directory is the same entry.
            let back = Trust::load();
            assert!(back.is_root_trusted(&project));
            assert!(back.is_root_trusted(&project.join("sub").join("..")));
            assert_eq!(back.roots().len(), 1);

            // Trusting twice does not duplicate.
            let mut again = back;
            again.trust_root(&project);
            assert_eq!(again.roots().len(), 1);
        });
    }

    #[test]
    fn lsp_approval_is_per_command_line() {
        with_data_dir("clew-trust-lsp", |dir| {
            let project = dir.join("proj");
            std::fs::create_dir_all(&project).unwrap();
            let cmd = PathBuf::from("/usr/local/bin/rust-analyzer");
            let fp = lsp_fingerprint(&cmd, &[], "rust-analyzer", "2026-07-13");

            let mut t = Trust::load();
            assert!(!t.is_lsp_approved(&project, "rust", &fp));
            t.approve_lsp(&project, "rust", &fp);
            t.save().unwrap();
            assert!(Trust::load().is_lsp_approved(&project, "rust", &fp));

            // A changed binary, argument, server, or version is NOT approved:
            // an edited lsp.toml has to be confirmed again.
            let other = lsp_fingerprint(
                &PathBuf::from("./payload"),
                &[],
                "rust-analyzer",
                "2026-07-13",
            );
            assert!(!Trust::load().is_lsp_approved(&project, "rust", &other));
            let extra_arg = lsp_fingerprint(&cmd, &["--x".into()], "rust-analyzer", "2026-07-13");
            assert!(!Trust::load().is_lsp_approved(&project, "rust", &extra_arg));
            let other_ver = lsp_fingerprint(&cmd, &[], "rust-analyzer", "2026-08-01");
            assert!(!Trust::load().is_lsp_approved(&project, "rust", &other_ver));
            // …and it does not leak to another project.
            let elsewhere = dir.join("other");
            std::fs::create_dir_all(&elsewhere).unwrap();
            assert!(!Trust::load().is_lsp_approved(&elsewhere, "rust", &fp));
        });
    }

    #[test]
    fn forgetting_a_root_drops_its_lsp_approvals() {
        with_data_dir("clew-trust-forget", |dir| {
            let project = dir.join("proj");
            std::fs::create_dir_all(&project).unwrap();
            let fp = lsp_fingerprint(&PathBuf::from("/bin/ra"), &[], "rust-analyzer", "1");
            let mut t = Trust::load();
            t.trust_root(&project);
            t.approve_lsp(&project, "rust", &fp);
            t.forget_root(&project);
            t.save().unwrap();

            let back = Trust::load();
            assert!(!back.is_root_trusted(&project));
            assert!(!back.is_lsp_approved(&project, "rust", &fp));
        });
    }
}
