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

/// Serializes [`Trust::update`] within this process (multi-window).
static SAVE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// An exclusive advisory lock held for one read-modify-write of `trust.toml`,
/// released when dropped. Taken on a sibling `.lock` file rather than on
/// `trust.toml` itself, because the atomic write replaces that inode.
///
/// Best effort by design: if the lock cannot be taken, the update still runs.
/// Serialized-and-correct is the goal, but refusing to record consent because
/// a lock file is unavailable would be worse than the race it prevents.
#[cfg(unix)]
struct FileLock(#[allow(dead_code)] std::fs::File);

#[cfg(unix)]
impl FileLock {
    fn acquire(path: &Path) -> Option<Self> {
        use std::os::unix::io::AsRawFd;
        let f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path.with_extension("toml.lock"))
            .ok()?;
        // Blocking, exclusive; released when the handle closes.
        (unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } == 0).then_some(Self(f))
    }
}

#[cfg(not(unix))]
struct FileLock;

#[cfg(not(unix))]
impl FileLock {
    fn acquire(_path: &Path) -> Option<Self> {
        Some(Self)
    }
}

/// The canonical form of `root`, used as its key. Falls back to the path as
/// given when it cannot be canonicalized (a root that no longer exists).
pub fn key_of(root: &Path) -> String {
    root.canonicalize()
        .unwrap_or_else(|_| root.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

/// The approval key for a project on `host` (`None` = this machine). A remote
/// root cannot be canonicalized here and — more importantly — the same
/// absolute path on two different hosts is two different projects, so the
/// host is part of the key: an approval granted for one machine's
/// `/srv/proj` must not silently cover another's.
fn scoped_key(host: Option<&str>, root: &Path) -> String {
    match host {
        None => key_of(root),
        Some(h) => format!("ssh://{h}:{}", root.to_string_lossy()),
    }
}

impl Trust {
    pub fn load() -> Trust {
        trust_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Apply `change` to the record and persist it — re-reading what is on
    /// disk RIGHT NOW, under a lock, and writing back atomically. `self` is
    /// refreshed to match.
    ///
    /// This is the ONLY way to record consent, and the read has to happen
    /// inside the lock. A window loads `Trust` once and holds it for as long
    /// as it is open, so by the time the user approves something its copy can
    /// be hours old; writing that snapshot wholesale silently deleted every
    /// root and approval another window had recorded meanwhile. Re-reading
    /// here makes concurrent windows (and separate clew processes) additive
    /// instead of last-writer-wins.
    pub fn update<F>(&mut self, change: F) -> Result<(), String>
    where
        F: FnOnce(&mut Trust),
    {
        // Two clew windows share one process, so the in-process lock is what
        // fixes the common case; the file lock underneath extends it to two
        // clew processes. Poisoning is not a reason to refuse: it only means
        // an earlier caller panicked, and the data is re-read below anyway.
        let _serialized = SAVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let path = trust_path().ok_or("no data directory")?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let _exclusive = FileLock::acquire(&path);
        let mut fresh = Trust::load();
        change(&mut fresh);
        let text = toml::to_string(&fresh).map_err(|e| e.to_string())?;
        // Atomic (temp + rename): a torn write here loses every recorded
        // consent at once, and the user is asked to re-approve everything.
        crate::statefile::write_atomic(&path, text.as_bytes()).map_err(|e| e.to_string())?;
        *self = fresh;
        Ok(())
    }

    /// Whether the user has allowed clew to open this project on `host`
    /// (`None` = this machine). Host-scoped like the LSP approvals: trusting
    /// `/srv/proj` on one machine must not silently cover the same path on
    /// another.
    pub fn is_root_trusted(&self, host: Option<&str>, root: &Path) -> bool {
        self.roots.contains(&scoped_key(host, root))
    }

    /// Record consent for a project root on `host`.
    pub fn trust_root(&mut self, host: Option<&str>, root: &Path) {
        let key = scoped_key(host, root);
        if !self.roots.contains(&key) {
            self.roots.push(key);
        }
    }

    /// Forget a project root and every language-server approval under it.
    pub fn forget_root(&mut self, host: Option<&str>, root: &Path) {
        let key = scoped_key(host, root);
        self.roots.retain(|r| *r != key);
        self.lsp.remove(&key);
    }

    /// Every trusted root, for the settings list.
    pub fn roots(&self) -> &[String] {
        &self.roots
    }

    /// Whether this exact command line was approved for `language` in `root`
    /// on `host` (`None` = this machine).
    pub fn is_lsp_approved(
        &self,
        host: Option<&str>,
        root: &Path,
        language: &str,
        fingerprint: &str,
    ) -> bool {
        self.lsp
            .get(&scoped_key(host, root))
            .and_then(|m| m.get(language))
            .is_some_and(|h| h == fingerprint)
    }

    /// Approve one language-server command line for this project on `host`.
    pub fn approve_lsp(
        &mut self,
        host: Option<&str>,
        root: &Path,
        language: &str,
        fingerprint: &str,
    ) {
        self.lsp
            .entry(scoped_key(host, root))
            .or_default()
            .insert(language.to_string(), fingerprint.to_string());
    }

    /// Every `(language, fingerprint)` approval recorded for this project on
    /// `host` — pushed to the clew-server so its spawn paths (SpawnLsp, the
    /// agent's semantic tools) honor approvals the user granted in the client.
    pub fn lsp_approvals_for(&self, host: Option<&str>, root: &Path) -> Vec<(String, String)> {
        self.lsp
            .get(&scoped_key(host, root))
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

/// Byte cap for a fingerprinted command. Well above any real language server
/// (release builds are tens of MB, debug builds hundreds) and low enough that
/// hashing stays a moment, not a hang.
pub const MAX_COMMAND_BYTES: u64 = 1024 * 1024 * 1024;

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
    let (_, fingerprint, _) = hash_command(root, command, args, server, version)?;
    Ok(fingerprint)
}

/// Open the command and hash it, returning the open handle alongside.
///
/// The handle is the point: it is the ONE resolution of the path. Everything
/// downstream — the approval decision and, once approved, the copy that is
/// actually executed — works from these same bytes, so nothing the repository
/// does to the path afterwards can change what runs.
///
/// Returns `(handle, fingerprint, content digest)`. The fingerprint is what
/// the user approves; the content digest names the staged copy.
fn hash_command(
    root: &Path,
    command: &Path,
    args: &[String],
    server: &str,
    version: &str,
) -> Result<(std::fs::File, String, String), String> {
    use sha2::{Digest, Sha256};
    let abs = resolve_command(root, command);
    let real = abs
        .canonicalize()
        .map_err(|e| format!("{}: {e}", abs.display()))?;
    // The command path comes from the repository's own lsp.toml, so it is
    // attacker-chosen: the open itself must refuse a symlink and must not
    // block on a FIFO, and the type check runs on the resulting handle.
    let mut f = crate::statefile::open_plain(&real)
        .ok_or_else(|| format!("{}: not a readable regular file", real.display()))?;
    let len = f
        .metadata()
        .map_err(|e| format!("{}: {e}", real.display()))?
        .len();
    if len > MAX_COMMAND_BYTES {
        return Err(format!(
            "{}: larger than {} MB — refusing to fingerprint",
            real.display(),
            MAX_COMMAND_BYTES / (1024 * 1024)
        ));
    }
    // Stream the executable through the hash — language servers can be large.
    let mut content = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut total: u64 = 0;
    loop {
        use std::io::Read;
        match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                // Re-checked while reading: the size above races with a
                // concurrent swap, and an unbounded loop is the one failure
                // mode this function must never have.
                total += n as u64;
                if total > MAX_COMMAND_BYTES {
                    return Err(format!("{}: grew past the size cap", real.display()));
                }
                content.update(&buf[..n]);
            }
            Err(e) => return Err(e.to_string()),
        }
    }
    let content = content.finalize();
    let content_hex: String = content.iter().map(|b| format!("{b:02x}")).collect();

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
    let fingerprint = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    Ok((f, fingerprint, content_hex))
}

/// What a repo-specified language-server command resolved to.
pub struct StagedCommand {
    /// The value the user approves and `trust.toml` records.
    pub fingerprint: String,
    /// The repository path — for the consent modal and error messages ONLY.
    /// Never spawn this.
    pub source: PathBuf,
    /// clew's private copy of the approved bytes, and the only thing that may
    /// be executed. `None` when `approved` said no — nothing is materialized
    /// for a command the user has not agreed to run.
    pub exec_path: Option<PathBuf>,
}

/// Resolve a repo-specified command to something safe to execute.
///
/// Approving a fingerprint and then spawning by PATH is a check-to-exec race:
/// between the hash and the `execve`, the repository can replace the leaf,
/// re-point a symlink, or swap a parent directory — approving A and running B.
/// Hashing an open handle does not fix it either, because the spawn re-resolves
/// the name.
///
/// So the approved bytes are copied, from the same handle they were hashed
/// from, into a file clew owns and the repository cannot reach, and THAT is
/// what runs. The copy is content-addressed, so a command already staged costs
/// only the hash; and it happens strictly AFTER `approved` returns true, so an
/// unapproved binary is never written into clew's own directory.
pub fn stage_lsp_command(
    root: &Path,
    command: &Path,
    args: &[String],
    server: &str,
    version: &str,
    approved: impl FnOnce(&str) -> bool,
) -> Result<StagedCommand, String> {
    let (mut file, fingerprint, content) = hash_command(root, command, args, server, version)?;
    let source = resolve_command(root, command);
    if !approved(&fingerprint) {
        return Ok(StagedCommand {
            fingerprint,
            source,
            exec_path: None,
        });
    }
    let exec_path = stage_bytes(&mut file, &content)?;
    Ok(StagedCommand {
        fingerprint,
        source,
        exec_path: Some(exec_path),
    })
}

/// Copy `file` (rewound) to `<data_root>/exec/<content>` and make it
/// executable. Already-staged content is reused as is: the name IS the
/// digest, so an existing file of the right size is the same bytes.
fn stage_bytes(file: &mut std::fs::File, content: &str) -> Result<PathBuf, String> {
    use std::io::{Seek, SeekFrom};
    let dir = crate::lsp::store::data_root()
        .ok_or("no data directory")?
        .join("exec");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let dest = dir.join(content);
    if dest.is_file() {
        return Ok(dest);
    }
    file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    // Staged under a unique name and renamed into place, so a concurrent
    // stage of the same command can never observe a half-written executable.
    let tmp = dir.join(format!(".tmp-{}-{content}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let mut out = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
    let copied = std::io::copy(file, &mut out).map_err(|e| e.to_string());
    drop(out);
    if let Err(e) = copied {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Read+execute, owner only: nothing else needs to touch it, and it
        // must not be writable — the point is that these bytes cannot change.
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o500));
    }
    std::fs::rename(&tmp, &dest).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e.to_string()
    })?;
    Ok(dest)
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
            assert!(!t.is_root_trusted(None, &project));
            t.update(|t| t.trust_root(None, &project)).unwrap();

            // A different spelling of the same directory is the same entry.
            let back = Trust::load();
            assert!(back.is_root_trusted(None, &project));
            assert!(back.is_root_trusted(None, &project.join("sub").join("..")));
            assert_eq!(back.roots().len(), 1);

            // Trusting twice does not duplicate.
            let mut again = back;
            again.trust_root(None, &project);
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
            assert!(!t.is_lsp_approved(None, &project, "rust", &fp));
            t.update(|t| t.approve_lsp(None, &project, "rust", &fp))
                .unwrap();
            assert!(Trust::load().is_lsp_approved(None, &project, "rust", &fp));
            assert_eq!(
                Trust::load().lsp_approvals_for(None, &project),
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
            assert!(!Trust::load().is_lsp_approved(None, &project, "rust", &extra_arg));
            let other_ver =
                lsp_fingerprint(&project, &cmd, &[], "rust-analyzer", "2026-08-01").unwrap();
            assert!(!Trust::load().is_lsp_approved(None, &project, "rust", &other_ver));

            // The approved script's BODY being swapped (a later hostile
            // commit) also invalidates the approval — the fingerprint hashes
            // the executable's bytes, not just its path.
            std::fs::write(&cmd, "#!/bin/sh\nexec ./payload\n").unwrap();
            let swapped =
                lsp_fingerprint(&project, &cmd, &[], "rust-analyzer", "2026-07-13").unwrap();
            assert_ne!(swapped, fp, "content change must change the fingerprint");
            assert!(!Trust::load().is_lsp_approved(None, &project, "rust", &swapped));

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
            assert!(!Trust::load().is_lsp_approved(None, &elsewhere, "rust", &fp));
        });
    }

    #[test]
    fn forgetting_a_root_drops_its_lsp_approvals() {
        with_data_dir("clew-trust-forget", |dir| {
            let project = dir.join("proj");
            std::fs::create_dir_all(&project).unwrap();
            let mut t = Trust::load();
            t.update(|t| {
                t.trust_root(None, &project);
                t.approve_lsp(None, &project, "rust", "fp-1");
                t.forget_root(None, &project);
            })
            .unwrap();

            let back = Trust::load();
            assert!(!back.is_root_trusted(None, &project));
            assert!(!back.is_lsp_approved(None, &project, "rust", "fp-1"));
            assert!(back.lsp_approvals_for(None, &project).is_empty());
        });
    }

    /// Two windows each hold their own `Trust` from the moment they opened.
    /// Saving one of those snapshots wholesale deleted whatever the other had
    /// recorded since; every update has to merge into what is on disk now.
    #[test]
    fn a_second_window_does_not_erase_the_first_windows_consent() {
        with_data_dir("clew-trust-concurrent", |dir| {
            let a = dir.join("proj-a");
            let b = dir.join("proj-b");
            std::fs::create_dir_all(&a).unwrap();
            std::fs::create_dir_all(&b).unwrap();

            // Both windows load the same (empty) record and keep it.
            let mut window_1 = Trust::load();
            let mut window_2 = Trust::load();

            window_1.update(|t| t.trust_root(None, &a)).unwrap();
            window_1
                .update(|t| t.approve_lsp(None, &a, "rust", "fp-a"))
                .unwrap();

            // Window 2 acts on its stale snapshot afterwards.
            window_2.update(|t| t.trust_root(None, &b)).unwrap();

            let disk = Trust::load();
            assert!(disk.is_root_trusted(None, &a), "window 1's root survived");
            assert!(disk.is_root_trusted(None, &b), "window 2's root recorded");
            assert!(disk.is_lsp_approved(None, &a, "rust", "fp-a"));
            // The writer also sees what it did not know about.
            assert!(window_2.is_root_trusted(None, &a));
        });
    }

    /// An approval is scoped to the host it was granted for: the same
    /// absolute project path on another machine (or on this one) is a
    /// different project and must be asked about separately.
    #[test]
    fn lsp_approvals_are_scoped_by_host() {
        with_data_dir("clew-trust-host", |_dir| {
            let root = Path::new("/srv/proj");
            let mut t = Trust::load();
            t.approve_lsp(Some("dev@build-a"), root, "rust", "fp-1");

            assert!(t.is_lsp_approved(Some("dev@build-a"), root, "rust", "fp-1"));
            assert!(!t.is_lsp_approved(None, root, "rust", "fp-1"));
            assert!(!t.is_lsp_approved(Some("dev@build-b"), root, "rust", "fp-1"));
            assert!(t.lsp_approvals_for(None, root).is_empty());
            assert_eq!(
                t.lsp_approvals_for(Some("dev@build-a"), root),
                vec![("rust".to_string(), "fp-1".to_string())]
            );
        });
    }

    /// The command path comes from the repository's lsp.toml: fingerprinting
    /// must refuse anything that is not a plain file — `/dev/zero` would
    /// hash forever, a FIFO would block the open — instead of hanging the
    /// caller.
    #[test]
    #[cfg(unix)]
    fn fingerprint_refuses_non_regular_files() {
        let dir = std::env::temp_dir().join("clew-trust-nonreg");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let dev = lsp_fingerprint(&dir, Path::new("/dev/zero"), &[], "s", "1");
        assert!(dev.is_err(), "a character device must be refused");

        let fifo = dir.join("fifo");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .is_ok_and(|s| s.success()),
            "mkfifo failed"
        );
        let piped = lsp_fingerprint(&dir, &fifo, &[], "s", "1");
        assert!(piped.is_err(), "a FIFO must be refused before the open");
    }
}

#[cfg(test)]
mod staging_tests {
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

    /// What runs must be the bytes the user approved, not whatever is at the
    /// path afterwards. Approving a fingerprint and then spawning the
    /// repository's path is a check-to-exec race the repository wins simply
    /// by rewriting the file.
    #[test]
    fn the_approved_bytes_are_what_gets_executed() {
        with_data_dir("clew-trust-staging", |dir| {
            let root = dir.join("proj");
            std::fs::create_dir_all(&root).unwrap();
            let cmd = root.join("server.sh");
            std::fs::write(&cmd, b"#!/bin/sh\necho approved\n").unwrap();

            let stage = |approve: bool| {
                stage_lsp_command(&root, Path::new("server.sh"), &[], "s", "1", |_| approve)
                    .expect("stages")
            };

            // Not approved: nothing of the repository's is materialized.
            let refused = stage(false);
            assert!(refused.exec_path.is_none());
            assert!(
                !dir.join("exec").exists(),
                "an unapproved command must not be copied into clew's own directory"
            );
            // The fingerprint is still the value the modal shows and records.
            assert_eq!(
                refused.fingerprint,
                lsp_fingerprint(&root, Path::new("server.sh"), &[], "s", "1").unwrap()
            );

            let approved = stage(true);
            let exec = approved.exec_path.expect("approved commands are staged");
            assert_eq!(
                std::fs::read(&exec).unwrap(),
                b"#!/bin/sh\necho approved\n",
                "the copy holds the bytes that were hashed"
            );
            assert!(exec.starts_with(dir), "and lives in clew's own directory");
            assert_ne!(exec, cmd);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(&exec).unwrap().permissions().mode();
                assert_eq!(mode & 0o777, 0o500, "owner read+execute only");
            }

            // The repository swaps the file after approval. The staged copy —
            // the thing that is spawned — is untouched, and a fresh stage
            // yields a different fingerprint, so the approval no longer holds.
            std::fs::write(&cmd, b"#!/bin/sh\necho pwned\n").unwrap();
            assert_eq!(
                std::fs::read(&exec).unwrap(),
                b"#!/bin/sh\necho approved\n",
                "swapping the source must not change what was approved to run"
            );
            let after = stage(false);
            assert_ne!(
                after.fingerprint, approved.fingerprint,
                "the swapped file must not pass the old approval"
            );
        });
    }
}
