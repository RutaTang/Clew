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

    /// Every `(language, fingerprint)` approval recorded for this project —
    /// pushed to the clew-server so its spawn paths (SpawnLsp, the agent's
    /// semantic tools) honor approvals the user granted in the client.
    pub fn lsp_approvals_for(&self, root: &Path) -> Vec<(String, String)> {
        self.lsp
            .get(&key_of(root))
            .map(|m| m.iter().map(|(l, f)| (l.clone(), f.clone())).collect())
            .unwrap_or_default()
    }
}

/// The absolute form of a (possibly project-relative) lsp.toml `command`.
/// Both the fingerprint and the actual spawn MUST use this same resolution —
/// a bare name handed to the OS would be looked up on PATH instead, so the
/// approved file and the executed file could silently differ.
pub fn resolve_command(root: &Path, command: &Path) -> PathBuf {
    if command.is_absolute() {
        command.to_path_buf()
    } else {
        root.join(command)
    }
}

/// A stable fingerprint of what would actually be executed: the canonical
/// executable path, a hash of its **bytes**, the arguments, and the
/// server/version it came from. Any change — including the repository
/// swapping the approved script's body in a later commit, or re-pointing a
/// symlink — invalidates a previous approval, so it must be confirmed again.
///
/// `command` resolves against `root` when relative (matching how the spawn
/// resolves it via the working directory). Errors when the file can't be
/// read — an unreadable command can't be meaningfully approved or run.
///
/// SHA-256, not a general-purpose hash: this value decides whether a command
/// runs without asking, so it must be collision-resistant (an attacker picks
/// the input) and stable across toolchain versions (`DefaultHasher` is neither).
pub fn lsp_fingerprint(
    root: &Path,
    command: &Path,
    args: &[String],
    server: &str,
    version: &str,
) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    let abs = resolve_command(root, command);
    let real = abs
        .canonicalize()
        .map_err(|e| format!("{}: {e}", abs.display()))?;
    // Stream the executable through the hash — language servers can be large.
    let mut content = Sha256::new();
    let mut f = std::fs::File::open(&real).map_err(|e| format!("{}: {e}", real.display()))?;
    let mut buf = [0u8; 64 * 1024];
    loop {
        use std::io::Read;
        match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => content.update(&buf[..n]),
            Err(e) => return Err(e.to_string()),
        }
    }
    let content = content.finalize();

    let mut h = Sha256::new();
    // Length-prefix each field so no rearrangement of the parts collides.
    for part in [
        real.to_string_lossy().as_ref(),
        &args.join("\u{1e}"),
        server,
        version,
    ] {
        h.update((part.len() as u64).to_le_bytes());
        h.update(part.as_bytes());
    }
    h.update(content);
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
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
    fn lsp_approval_is_per_command_line_and_content() {
        with_data_dir("clew-trust-lsp", |dir| {
            let project = dir.join("proj");
            std::fs::create_dir_all(&project).unwrap();
            let cmd = project.join("run-lsp.sh");
            std::fs::write(&cmd, "#!/bin/sh\nexec rust-analyzer\n").unwrap();
            let fp = lsp_fingerprint(&project, &cmd, &[], "rust-analyzer", "2026-07-13").unwrap();

            let mut t = Trust::load();
            assert!(!t.is_lsp_approved(&project, "rust", &fp));
            t.approve_lsp(&project, "rust", &fp);
            t.save().unwrap();
            assert!(Trust::load().is_lsp_approved(&project, "rust", &fp));
            assert_eq!(
                Trust::load().lsp_approvals_for(&project),
                vec![("rust".to_string(), fp.clone())]
            );

            // A changed argument, server, or version is NOT approved: an
            // edited lsp.toml has to be confirmed again.
            let extra_arg = lsp_fingerprint(
                &project,
                &cmd,
                &["--x".into()],
                "rust-analyzer",
                "2026-07-13",
            )
            .unwrap();
            assert!(!Trust::load().is_lsp_approved(&project, "rust", &extra_arg));
            let other_ver =
                lsp_fingerprint(&project, &cmd, &[], "rust-analyzer", "2026-08-01").unwrap();
            assert!(!Trust::load().is_lsp_approved(&project, "rust", &other_ver));

            // The approved script's BODY being swapped (a later hostile
            // commit) also invalidates the approval — the fingerprint hashes
            // the executable's bytes, not just its path.
            std::fs::write(&cmd, "#!/bin/sh\nexec ./payload\n").unwrap();
            let swapped =
                lsp_fingerprint(&project, &cmd, &[], "rust-analyzer", "2026-07-13").unwrap();
            assert_ne!(swapped, fp, "content change must change the fingerprint");
            assert!(!Trust::load().is_lsp_approved(&project, "rust", &swapped));

            // A relative command resolves against the project root (matching
            // how the spawn resolves it) — same file, same fingerprint.
            let rel = lsp_fingerprint(
                &project,
                Path::new("run-lsp.sh"),
                &[],
                "rust-analyzer",
                "2026-07-13",
            )
            .unwrap();
            assert_eq!(rel, swapped);

            // A missing command can't be fingerprinted (and can't run).
            assert!(lsp_fingerprint(&project, Path::new("/nonexistent/x"), &[], "s", "1").is_err());

            // …and an approval does not leak to another project.
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
            let mut t = Trust::load();
            t.trust_root(&project);
            t.approve_lsp(&project, "rust", "fp-1");
            t.forget_root(&project);
            t.save().unwrap();

            let back = Trust::load();
            assert!(!back.is_root_trusted(&project));
            assert!(!back.is_lsp_approved(&project, "rust", "fp-1"));
            assert!(back.lsp_approvals_for(&project).is_empty());
        });
    }
}
