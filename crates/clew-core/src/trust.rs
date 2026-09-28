//! Workspace trust: which project roots the user has allowed clew to open, and
//! what they have allowed a project's own `lsp.toml` to do — the commands it
//! may run, and the `initialize` options it may send to a language server.
//!
//! Both records live in clew's **global data directory**, never inside the
//! project. A project's own files are attacker-controlled when the repository
//! is untrusted, so consent recorded there would let a repository grant itself
//! permission — the very thing consent exists to prevent.
//!
//! Roots are keyed by [`crate::derived::project_key`] — canonical locally, so
//! a symlinked or relative path to an already-trusted project resolves to the
//! same entry, and host-scoped remotely — the same key the derived-artifact
//! cache uses, so the two can never disagree about which project is which.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// On-disk shape of `<data_root>/trust.toml`.
///
/// `Clone` so a caller can hand a snapshot to a background thread (staging a
/// command hashes up to [`MAX_COMMAND_BYTES`], which does not belong on the
/// UI thread).
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Trust {
    /// Canonical project roots the user allowed clew to open.
    #[serde(default)]
    roots: Vec<String>,
    /// What the user allowed a project's own `lsp.toml` to do for a language,
    /// keyed by canonical project root: `root -> { language -> fingerprint }`.
    /// The fingerprint covers the command (when the config names one) together
    /// with its arguments, `init_options` and the server/version it resolves
    /// to, so a change to ANY of those invalidates the entry.
    ///
    /// One entry per language, and deliberately so: a config either names a
    /// command ([`lsp_fingerprint`]) or only options ([`lsp_options_fingerprint`]),
    /// and the two hashes are domain-separated, so a config that changes shape
    /// has to be approved again rather than inheriting the other answer.
    #[serde(default)]
    lsp: BTreeMap<String, BTreeMap<String, String>>,
}

fn trust_path() -> Option<PathBuf> {
    Some(crate::lsp::store::data_root()?.join("trust.toml"))
}

/// Byte cap for `trust.toml`: a list of paths and hex fingerprints. Anything
/// larger is not a file clew wrote.
const MAX_TRUST_BYTES: u64 = 16 * 1024 * 1024;

/// Serializes [`Trust::update`] within this process (multi-window).
static SAVE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The canonical form of `root`, used as its key. Falls back to the path as
/// given when it cannot be canonicalized (a root that no longer exists).
pub fn key_of(root: &Path) -> String {
    crate::derived::project_key(None, root)
}

/// The approval key for a project on `host` (`None` = this machine). A remote
/// root cannot be canonicalized here and — more importantly — the same
/// absolute path on two different hosts is two different projects, so the
/// host is part of the key: an approval granted for one machine's
/// `/srv/proj` must not silently cover another's.
fn scoped_key(host: Option<&str>, root: &Path) -> String {
    crate::derived::project_key(host, root)
}

impl Trust {
    /// The record as it is on disk, for READING. A missing file is the empty
    /// record; so is one that cannot be read or parsed — a reader has nothing
    /// better to fall back to, and an empty record grants nothing. Anything
    /// about to WRITE uses [`Trust::load_checked`] instead (see
    /// [`Trust::update`]).
    pub fn load() -> Trust {
        Trust::load_checked().unwrap_or_default()
    }

    /// The record as it is on disk: `Ok(empty)` when there is no file yet,
    /// `Err` when there is one that cannot be read or parsed.
    pub fn load_checked() -> Result<Trust, String> {
        let path = trust_path().ok_or("no data directory")?;
        match crate::statefile::read_capped_checked(&path, MAX_TRUST_BYTES) {
            Ok(None) => Ok(Trust::default()),
            Ok(Some(text)) => toml::from_str(&text)
                .map_err(|e| format!("{} is not valid TOML: {e}", path.display())),
            Err(e) => Err(format!("{} cannot be read: {e}", path.display())),
        }
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
    ///
    /// A `trust.toml` that exists but cannot be read or parsed is an ERROR,
    /// never an empty record: writing "empty + this change" over it would
    /// silently drop every root and approval it holds (the same rule
    /// `globalconfig` follows for `config.toml`). The bytes stay for the user
    /// to fix.
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
            crate::derived::ensure_private_dir(dir).map_err(|e| e.to_string())?;
        }
        // `trust.toml.lock`: the name every clew version has locked this
        // record under (see `statefile::lock_named`), so an older clew
        // running alongside is excluded too.
        let _exclusive = crate::statefile::lock_named(&path, "trust.toml.lock")
            .map_err(|e| format!("cannot lock {}: {e}", path.display()))?;
        let mut fresh = Trust::load_checked()?;
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

    /// Whether this exact fingerprint — a command line with its options, or
    /// options alone — was approved for `language` in `root` on `host`
    /// (`None` = this machine).
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

    /// Approve one language-server fingerprint (command line, or options
    /// alone) for this project on `host`.
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

    /// Every fingerprint approved anywhere, on any host — what keeps a staged
    /// copy in `exec/` alive (see [`sweep_exec`]).
    fn all_fingerprints(&self) -> std::collections::HashSet<&str> {
        self.lsp
            .values()
            .flat_map(|m| m.values().map(String::as_str))
            .collect()
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
/// executable path, a hash of its **bytes**, the arguments, the server/version
/// it came from, and the `init_options` it would be handed. Any change —
/// including the repository swapping the approved script's body in a later
/// commit, re-pointing a symlink, or editing only the options — invalidates a
/// previous approval, so it must be confirmed again.
///
/// `init_options` belongs here because it is the same attacker-chosen input as
/// `command`: it ships in the repository's `lsp.toml`, reaches the server's
/// `initialize` verbatim, and several servers treat it as a place to name
/// programs they then run (rust-analyzer's `cargo.buildScripts.overrideCommand`,
/// typescript-language-server's `tsserver.path`/`plugins`). Approving a command
/// while leaving its options free would gate the door and not the window.
///
/// This is the fingerprint for a config that sets a `command`. A config that
/// sets `init_options` and NO command names no bytes to hash, and is
/// fingerprinted by [`lsp_options_fingerprint`] instead — the two are
/// domain-separated so neither approval can ever satisfy the other.
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
    init_options: Option<&serde_json::Value>,
) -> Result<String, String> {
    hash_command(root, command, args, server, version, init_options).map(|h| h.fingerprint)
}

/// Domain tag for [`lsp_options_fingerprint`]. It occupies the first
/// length-prefixed field, where [`hash_command`] puts the canonical command
/// path, so the two hashes are computed over disjoint inputs: an approval
/// recorded for a command-bearing config can never satisfy an options-only
/// one, nor the reverse. That matters because `trust.toml` keeps exactly ONE
/// fingerprint per (root, language) — without the separation, a config that
/// dropped its `command` and kept its options could inherit the answer the
/// user gave to a different question.
const OPTIONS_DOMAIN: &str = "clew.lsp.init-options.v1";

/// A stable fingerprint of the `init_options` a config asks clew to send in
/// `initialize`, for the case where the config names NO `command` — the
/// server binary is then clew's own store-installed (or toolchain) copy,
/// already consented to at install time, so there are no command bytes to
/// hash and the question put to the user is only "send these options?".
///
/// Options need approval in their own right: they ship in the repository's
/// `.clew/lsp.toml` and reach the language server verbatim, and several
/// servers treat them as a place to name programs they then run
/// (rust-analyzer's `cargo.buildScripts.overrideCommand` / `procMacro.server`,
/// typescript-language-server's `tsserver.path`, pyright's `python.pythonPath`
/// — the last of which also OVERRIDES the interpreter `langenv` vetted). Left
/// ungated, cloning a repository and opening one file is code execution.
///
/// `server`/`version`/`args` are hashed alongside because the same options
/// mean different things to different servers: a key that is inert for the
/// server the user approved may name an executable for the one a later commit
/// switches to.
///
/// The value must be identical everywhere it is computed — the client records
/// it in `trust.toml` and pushes it to clew-server, which re-derives it for
/// its own spawn paths — so it deliberately hashes no file: a byte-hash of a
/// large store binary is not something the GUI thread can afford on every
/// start, and the two sides would then disagree.
pub fn lsp_options_fingerprint(
    args: &[String],
    server: &str,
    version: &str,
    init_options: &serde_json::Value,
) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    // Canonical rendering, same reasoning as in `hash_command`: sorted keys,
    // and a reordered-but-equivalent file may ask again but a CHANGED one can
    // never pass.
    let options =
        serde_json::to_string(init_options).map_err(|e| format!("lsp.toml init_options: {e}"))?;
    let mut h = Sha256::new();
    // Length-prefix each field so no rearrangement of the parts collides.
    for part in [
        OPTIONS_DOMAIN,
        &args.join("\u{1e}"),
        server,
        version,
        &options,
    ] {
        h.update((part.len() as u64).to_le_bytes());
        h.update(part.as_bytes());
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Open the command and hash it, returning the open handle alongside.
///
/// The handle is the point: it is the ONE resolution of the path, so the copy
/// that is actually executed reads the same *file* the approval decision was
/// made about — re-pointing a symlink, swapping a parent directory, or
/// replacing the leaf afterwards cannot redirect it somewhere else.
///
/// What the handle does NOT pin is the file's contents: a rewrite in place
/// keeps the inode and changes every later read of this handle. So this digest
/// is a statement about a moment, not a promise, and [`stage_bytes`] re-hashes
/// the bytes it actually writes rather than trusting it.
fn hash_command(
    root: &Path,
    command: &Path,
    args: &[String],
    server: &str,
    version: &str,
    init_options: Option<&serde_json::Value>,
) -> Result<Hashed, String> {
    use sha2::{Digest, Sha256};
    // The options as one canonical string. `serde_json::Map` is a `BTreeMap`
    // here, so keys come out sorted and the same config always hashes the
    // same; were a feature flag ever to make it insertion-ordered, the worst
    // case is that a reordered-but-equivalent file asks again, never that a
    // CHANGED one passes. `None` hashes as the empty string, which no real
    // value can produce (even `""` serializes to the two-byte `""`), so
    // "no options" and "some options" are distinct inputs.
    let options = match init_options {
        None => String::new(),
        Some(v) => serde_json::to_string(v).map_err(|e| format!("lsp.toml init_options: {e}"))?,
    };
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
        &options,
    ] {
        h.update((part.len() as u64).to_le_bytes());
        h.update(part.as_bytes());
    }
    h.update(content);
    let fingerprint = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    Ok(Hashed {
        file: f,
        fingerprint,
        content: content_hex,
        canonical: real,
    })
}

/// One opened-and-hashed command (see [`hash_command`]).
struct Hashed {
    /// The handle the digest was taken from.
    file: std::fs::File,
    /// What the user approves.
    fingerprint: String,
    /// SHA-256 of the bytes, verified again on every copy and every reuse.
    content: String,
    /// The resolved target — the file actually opened.
    canonical: PathBuf,
}

/// What a repo-specified language-server command resolved to.
pub struct StagedCommand {
    /// The value the user approves and `trust.toml` records.
    pub fingerprint: String,
    /// The repository path — for the consent modal and error messages ONLY.
    /// Never spawn this.
    pub source: PathBuf,
    /// The only thing that may be executed: the approved file itself when it
    /// lives outside the project in a location only its owner can change,
    /// otherwise clew's private copy of the approved bytes (see
    /// [`CommandProbe::into_exec`]). `None` when `approved` said no — nothing
    /// is materialized for a command the user has not agreed to run.
    pub exec_path: Option<PathBuf>,
}

/// A repo-specified command, opened and hashed ONCE, waiting for the user's
/// answer. The first half of [`stage_lsp_command`], split out so a GUI can do
/// the expensive part off its UI thread and ask the question on it:
///
/// ```text
/// let probe = spawn_blocking(|| probe_lsp_command(..))?;   // hash (slow)
/// if trust.is_lsp_approved(.., probe.fingerprint()) {      // cheap
///     let exe = spawn_blocking(|| probe.into_exec())?;     // copy (slow)
/// }
/// ```
///
/// It holds the open handle the digest was taken from, so the file that is
/// finally staged is the one the user was asked about, whatever happens to
/// the path in between. `Send`, so it can cross into those tasks.
pub struct CommandProbe {
    file: std::fs::File,
    fingerprint: String,
    content: String,
    source: PathBuf,
    canonical: PathBuf,
    /// Whether the canonical target lies outside the (canonical) project root.
    outside_project: bool,
}

impl CommandProbe {
    /// The value to put in front of the user and to check approvals against.
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// The command as the repository names it (resolved against the root) —
    /// for the consent modal and messages only.
    pub fn source(&self) -> &Path {
        &self.source
    }

    /// Turn an APPROVED probe into the path to spawn. Only call this after
    /// the fingerprint was approved: it materializes the bytes.
    ///
    /// Two cases, decided by where the file really is:
    ///
    /// - **Outside the project, in a trusted location** — owned by the user or
    ///   root, and neither it nor any directory above it writable by anyone
    ///   else ([`trusted_location`]): the file itself is returned, by its
    ///   canonical path. The repository cannot touch it, so there is no
    ///   check-to-exec race to close, and running it in place is what lets
    ///   a real toolchain work at all: `rust-analyzer` found through
    ///   `~/.cargo/bin`, clangd locating its resource directory, a
    ///   `node_modules/.bin` wrapper doing `dirname "$0"`, `@loader_path`
    ///   dylibs — none of them survive being copied into `exec/`. The inode is
    ///   re-checked, so a file REPLACED since it was hashed is refused (a new
    ///   probe asks again).
    /// - **Inside the project** (or somewhere others can write) — the bytes
    ///   are copied, from the handle they were hashed from, into
    ///   `<data>/exec/<fingerprint>` (0500, in a 0700 directory) and THAT is
    ///   what runs: between the hash and the `execve` the repository could
    ///   replace the leaf, re-point a symlink or swap a parent directory, so
    ///   approving one file and running another. The copy is re-hashed as it
    ///   is written. Programs that locate siblings relative to their own path
    ///   do not work from there — point `lsp.toml` at an installed copy
    ///   outside the repository for those.
    pub fn into_exec(mut self) -> Result<PathBuf, String> {
        if self.outside_project && trusted_location(&self.canonical) {
            same_file(&self.file, &self.canonical).map_err(|e| {
                format!(
                    "{}: {e} — it changed after it was approved; open the file again to \
                     re-approve",
                    self.canonical.display()
                )
            })?;
            return Ok(self.canonical);
        }
        let exec = stage_bytes(
            &mut self.file,
            &self.content,
            &self.fingerprint,
            &self.source,
        )?;
        // Housekeeping rides on the (rare, already slow) stage.
        sweep_exec();
        Ok(exec)
    }
}

/// Open and hash a repo-specified command: the first, expensive half of
/// [`stage_lsp_command`]. Blocking (reads up to [`MAX_COMMAND_BYTES`]) — run
/// it off the UI thread. The fingerprint is exactly [`lsp_fingerprint`]'s.
pub fn probe_lsp_command(
    root: &Path,
    command: &Path,
    args: &[String],
    server: &str,
    version: &str,
    init_options: Option<&serde_json::Value>,
) -> Result<CommandProbe, String> {
    let hashed = hash_command(root, command, args, server, version, init_options)?;
    let source = resolve_command(root, command);
    // An unresolvable root counts as "inside": the copy is the safe answer
    // when containment cannot be decided.
    let outside_project = match root.canonicalize() {
        Ok(real_root) => !hashed.canonical.starts_with(&real_root),
        Err(_) => false,
    };
    Ok(CommandProbe {
        file: hashed.file,
        fingerprint: hashed.fingerprint,
        content: hashed.content,
        source,
        canonical: hashed.canonical,
        outside_project,
    })
}

/// Resolve a repo-specified command to something safe to execute, asking
/// `approved` about its fingerprint in between: [`probe_lsp_command`], then
/// (only if approved) [`CommandProbe::into_exec`].
///
/// Approving a fingerprint and then spawning by PATH is a check-to-exec race:
/// between the hash and the `execve`, the repository can replace the leaf,
/// re-point a symlink, or swap a parent directory — approving A and running B.
/// Hashing an open handle does not fix it either, because the spawn re-resolves
/// the name. See [`CommandProbe::into_exec`] for how each case is closed.
/// Nothing is written into clew's own directory before `approved` returns true.
///
/// `init_options` is not staged — nothing is copied for it — but it IS part of
/// the fingerprint `approved` sees, so a repository that edits only its options
/// loses the approval it had for the command (see [`lsp_fingerprint`]). The
/// caller must pass the very options it will send in `initialize`; passing a
/// different set would approve one thing and run another.
///
/// Blocking: hashing and copying are proportional to the command's size.
pub fn stage_lsp_command(
    root: &Path,
    command: &Path,
    args: &[String],
    server: &str,
    version: &str,
    init_options: Option<&serde_json::Value>,
    approved: impl FnOnce(&str) -> bool,
) -> Result<StagedCommand, String> {
    let probe = probe_lsp_command(root, command, args, server, version, init_options)?;
    let fingerprint = probe.fingerprint.clone();
    let source = probe.source.clone();
    if !approved(&fingerprint) {
        return Ok(StagedCommand {
            fingerprint,
            source,
            exec_path: None,
        });
    }
    let exec_path = probe.into_exec()?;
    Ok(StagedCommand {
        fingerprint,
        source,
        exec_path: Some(exec_path),
    })
}

/// Whether nobody but its owner — the user, or root — can change the file at
/// `path` or re-point any directory above it: the file and every ancestor are
/// owned by the current user or root, the file is writable by neither group
/// nor others, and each ancestor is too — unless it is sticky (like `/tmp`),
/// where others may add entries but cannot rename or replace ours.
///
/// `path` must be canonical (no symlink components), which is what makes a
/// walk over its ancestors a statement about the real directories.
#[cfg(unix)]
pub fn trusted_location(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let me = unsafe { libc::geteuid() };
    let owned = |m: &std::fs::Metadata| m.uid() == me || m.uid() == 0;
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if !meta.is_file() || !owned(&meta) || meta.mode() & 0o022 != 0 {
        return false;
    }
    let mut dir = path.parent();
    while let Some(d) = dir {
        let Ok(m) = std::fs::symlink_metadata(d) else {
            return false;
        };
        let sticky = m.mode() & 0o1000 != 0;
        if !m.is_dir() || !owned(&m) || (m.mode() & 0o022 != 0 && !sticky) {
            return false;
        }
        dir = d.parent();
    }
    true
}

/// Without POSIX ownership and modes there is no cheap way to prove a
/// location private, so every command is staged.
#[cfg(not(unix))]
pub fn trusted_location(_path: &Path) -> bool {
    false
}

/// Whether the handle and the path still name the same file (device + inode).
fn same_file(file: &std::fs::File, path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let held = file.metadata().map_err(|e| e.to_string())?;
        let now = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
        if (held.dev(), held.ino()) != (now.dev(), now.ino()) {
            return Err("the file at this path was replaced".into());
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (file, path);
        Ok(())
    }
}

/// The private directory staged commands live in (0700).
fn exec_dir() -> Result<PathBuf, String> {
    let root = crate::lsp::store::data_root().ok_or("no data directory")?;
    crate::derived::ensure_private_dir(&root).map_err(|e| e.to_string())?;
    let dir = root.join("exec");
    crate::derived::ensure_private_dir(&dir).map_err(|e| e.to_string())?;
    Ok(dir)
}

/// Copy `file` (rewound) to `<data_root>/exec/<fingerprint>` and make it
/// executable, verifying as it writes that the bytes actually landing there
/// hash to `content`. The handle pins the inode, not its contents, so without
/// that check a source rewritten in place between the hash and this copy would
/// be stored as bytes B under the name of an approval for A.
///
/// Named by the approval's FINGERPRINT, not by content: that is what makes an
/// entry attributable, so [`sweep_exec`] can tell which ones some approval
/// still refers to.
///
/// An existing entry is never trusted by its name. It is re-hashed (and its
/// mode and owner checked) before it is reused, and anything that does not
/// match is removed and staged again — a corrupted or planted file under an
/// approved name must not run as the approved command.
fn stage_bytes(
    file: &mut std::fs::File,
    content: &str,
    fingerprint: &str,
    source: &Path,
) -> Result<PathBuf, String> {
    use std::io::{Seek, SeekFrom};
    use std::sync::atomic::{AtomicU64, Ordering};
    let dir = exec_dir()?;
    let dest = dir.join(fingerprint);
    match verify_staged(&dest, content) {
        Ok(true) => {
            // Mark it used, so the sweep keeps what is actually running.
            if let Ok(f) = std::fs::File::open(&dest) {
                let _ = f.set_modified(std::time::SystemTime::now());
            }
            return Ok(dest);
        }
        Ok(false) => {
            std::fs::remove_file(&dest).map_err(|e| {
                format!(
                    "{}: a staged copy that does not match the approval could not be \
                     removed: {e}",
                    dest.display()
                )
            })?;
        }
        Err(()) => {} // nothing there yet
    }
    file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    // Staged under a unique name and renamed into place, so a concurrent
    // stage of the same command can never observe a half-written executable.
    // The name deliberately carries no digest: `.tmp-<pid>-<digest>` would
    // announce to every process running as the user which approved content is
    // being staged and exactly when the write window opened. A process-local
    // counter distinguishes concurrent stages; the pid distinguishes processes.
    static NEXT_TMP: AtomicU64 = AtomicU64::new(0);
    let tmp = dir.join(format!(
        ".tmp-{}-{}",
        std::process::id(),
        NEXT_TMP.fetch_add(1, Ordering::Relaxed)
    ));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Created without any write bit: the descriptor is still writable
        // (it created the file), but the name never is.
        opts.mode(0o500);
    }
    let mut out = opts.open(&tmp).map_err(|e| e.to_string())?;
    let result = copy_verified(file, &mut out, content, source)
        .and_then(|()| out.sync_all().map_err(|e| e.to_string()))
        .and_then(|()| make_read_exec_only(&out));
    drop(out);
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    std::fs::rename(&tmp, &dest).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e.to_string()
    })?;
    Ok(dest)
}

/// Set the staged file's mode to 0500 through its handle and confirm it took:
/// an executable that is not executable fails later, far from the cause, and
/// one left writable is one whose approved bytes can change.
fn make_read_exec_only(out: &std::fs::File) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        out.set_permissions(std::fs::Permissions::from_mode(0o500))
            .map_err(|e| format!("cannot make the staged command executable: {e}"))?;
        let mode = out
            .metadata()
            .map_err(|e| e.to_string())?
            .permissions()
            .mode()
            & 0o777;
        if mode != 0o500 {
            return Err(format!(
                "the staged command has mode {mode:o}, not 0500 — refusing to run it"
            ));
        }
    }
    #[cfg(not(unix))]
    let _ = out;
    Ok(())
}

/// Whether the staged entry at `dest` holds exactly the approved bytes and
/// has the shape clew gave it: `Err(())` when there is none, `Ok(false)` when
/// there is one that must not be used.
fn verify_staged(dest: &Path, content: &str) -> Result<bool, ()> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut f = match crate::statefile::open_plain_checked(dest) {
        Ok(Some(f)) => f,
        Ok(None) => return Err(()),
        Err(_) => return Ok(false),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let Ok(meta) = f.metadata() else {
            return Ok(false);
        };
        if meta.mode() & 0o777 != 0o500 || meta.uid() != unsafe { libc::geteuid() } {
            return Ok(false);
        }
    }
    let mut h = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                total += n as u64;
                if total > MAX_COMMAND_BYTES {
                    return Ok(false);
                }
                h.update(&buf[..n]);
            }
            Err(_) => return Ok(false),
        }
    }
    Ok(hex(&h.finalize()) == content)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Stream `file` into `out`, hashing exactly the bytes written, and refuse
/// unless they hash to `content`.
///
/// The hash has to be taken here, on the way out, rather than inherited from
/// the earlier read: bytes filed under an approval they do not match would be
/// executed as the approved command.
fn copy_verified(
    file: &mut std::fs::File,
    out: &mut std::fs::File,
    content: &str,
    source: &Path,
) -> Result<(), String> {
    use sha2::{Digest, Sha256};
    use std::io::{Read, Write};
    let mut h = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut total: u64 = 0;
    loop {
        let n = match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => return Err(format!("{}: {e}", source.display())),
        };
        // Re-checked while copying for the same reason `hash_command`
        // re-checks while reading: the file can grow under the handle, and an
        // unbounded write into clew's own data directory is the one failure
        // mode this must never have.
        total += n as u64;
        if total > MAX_COMMAND_BYTES {
            return Err(format!(
                "{}: grew past the size cap while being staged",
                source.display()
            ));
        }
        out.write_all(&buf[..n])
            .map_err(|e| format!("{}: {e}", source.display()))?;
        h.update(&buf[..n]);
    }
    if hex(&h.finalize()) != content {
        return Err(format!(
            "{}: changed while it was being staged — refusing to run it",
            source.display()
        ));
    }
    Ok(())
}

/// How long a staged command no approval refers to survives since it was
/// last used. Approvals pushed to a remote clew-server live only in that
/// server's memory, never in the host's `trust.toml`, so "unreferenced" alone
/// would re-copy their commands on every start; "unreferenced AND unused for
/// a month" keeps what is in use and still bounds what is not.
const EXEC_UNUSED_GRACE: std::time::Duration = std::time::Duration::from_secs(30 * 24 * 60 * 60);

/// How old a leftover temp file (a stage interrupted by a crash) must be
/// before it is removed — far past any real copy, so a live stage in another
/// process is never touched.
const EXEC_TMP_GRACE: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// Most `exec/` entries one sweep looks at.
const MAX_SWEPT_EXEC_ENTRIES: usize = 1024;

/// Bounded housekeeping for `<data>/exec/`: remove staged commands that no
/// approval in `trust.toml` refers to and that nothing has used for
/// [`EXEC_UNUSED_GRACE`] (this also retires entries from the old
/// content-addressed naming), and temp files a crashed stage left behind.
/// Runs after every stage; best effort — a failure here costs disk space,
/// never a start. A `trust.toml` that cannot be read disables the sweep
/// rather than making every entry look unreferenced.
pub fn sweep_exec() {
    let Ok(dir) = exec_dir() else {
        return;
    };
    let Ok(trust) = Trust::load_checked() else {
        return;
    };
    sweep_exec_in(
        &dir,
        &trust.all_fingerprints(),
        std::time::SystemTime::now(),
    );
}

fn sweep_exec_in(
    dir: &Path,
    referenced: &std::collections::HashSet<&str>,
    now: std::time::SystemTime,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten().take(MAX_SWEPT_EXEC_ENTRIES) {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let age = meta
            .modified()
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .unwrap_or_default();
        let remove = if name.starts_with(".tmp-") {
            age > EXEC_TMP_GRACE
        } else {
            !referenced.contains(name) && age > EXEC_UNUSED_GRACE
        };
        if remove && meta.is_file() {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run `f` with `CLEW_DATA_DIR` at a fresh directory, restored afterwards
    /// (a failing test included) — see [`crate::testutil::DataDir`].
    pub(super) fn with_data_dir<T>(name: &str, f: impl FnOnce(&Path) -> T) -> T {
        let data = crate::testutil::DataDir::new(name);
        f(data.path())
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
            let fp =
                lsp_fingerprint(&project, &cmd, &[], "rust-analyzer", "2026-07-13", None).unwrap();

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
                None,
            )
            .unwrap();
            assert!(!Trust::load().is_lsp_approved(None, &project, "rust", &extra_arg));
            let other_ver =
                lsp_fingerprint(&project, &cmd, &[], "rust-analyzer", "2026-08-01", None).unwrap();
            assert!(!Trust::load().is_lsp_approved(None, &project, "rust", &other_ver));

            // The approved script's BODY being swapped (a later hostile
            // commit) also invalidates the approval — the fingerprint hashes
            // the executable's bytes, not just its path.
            std::fs::write(&cmd, "#!/bin/sh\nexec ./payload\n").unwrap();
            let swapped =
                lsp_fingerprint(&project, &cmd, &[], "rust-analyzer", "2026-07-13", None).unwrap();
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
                None,
            )
            .unwrap();
            assert_eq!(rel, swapped);

            // A missing command can't be fingerprinted (and can't run).
            assert!(
                lsp_fingerprint(&project, Path::new("/nonexistent/x"), &[], "s", "1", None)
                    .is_err()
            );

            // …and an approval does not leak to another project.
            let elsewhere = dir.join("other");
            std::fs::create_dir_all(&elsewhere).unwrap();
            assert!(!Trust::load().is_lsp_approved(None, &elsewhere, "rust", &fp));
        });
    }

    /// `init_options` ships in the same repo-owned `lsp.toml` as `command`,
    /// reaches the server's `initialize` verbatim, and for several servers
    /// names programs the server then runs (rust-analyzer's
    /// `cargo.buildScripts.overrideCommand`, tsserver's `plugins`). It is
    /// therefore part of what the user approves: a later commit that edits
    /// only the options must lose the approval, exactly as one that edits the
    /// command's bytes does.
    #[test]
    fn init_options_are_part_of_the_approval() {
        with_data_dir("clew-trust-init-options", |dir| {
            let project = dir.join("proj");
            std::fs::create_dir_all(&project).unwrap();
            let cmd = project.join("run-lsp.sh");
            std::fs::write(&cmd, "#!/bin/sh\nexec rust-analyzer\n").unwrap();
            let fingerprint = |opts: Option<&serde_json::Value>| {
                lsp_fingerprint(&project, &cmd, &[], "rust-analyzer", "2026-07-13", opts).unwrap()
            };

            let benign = serde_json::json!({"rust-analyzer.check.command": "clippy"});
            let approved = fingerprint(Some(&benign));
            let mut t = Trust::load();
            t.update(|t| t.approve_lsp(None, &project, "rust", &approved))
                .unwrap();

            // An unchanged config keeps its approval: nothing here re-prompts
            // for a file the user already said yes to.
            let unchanged = serde_json::json!({"rust-analyzer.check.command": "clippy"});
            assert_eq!(fingerprint(Some(&unchanged)), approved);
            assert!(Trust::load().is_lsp_approved(None, &project, "rust", &approved));

            // A later commit slips an execution-bearing key in beside it. The
            // command is byte-for-byte the approved one, so only the options
            // can invalidate this — and they must.
            let hostile = serde_json::json!({
                "rust-analyzer.check.command": "clippy",
                "rust-analyzer.cargo.buildScripts.overrideCommand": ["sh", "-c", "payload"],
            });
            let hostile_fp = fingerprint(Some(&hostile));
            assert_ne!(
                hostile_fp, approved,
                "edited init_options must change the fingerprint"
            );
            assert!(!Trust::load().is_lsp_approved(None, &project, "rust", &hostile_fp));

            // Removing the block is a change too, and "no options" must not
            // collide with an explicit null — the fields are length-prefixed
            // precisely so no two different configs hash alike.
            let no_options = fingerprint(None);
            assert_ne!(no_options, approved);
            assert!(!Trust::load().is_lsp_approved(None, &project, "rust", &no_options));
            assert_ne!(fingerprint(Some(&serde_json::Value::Null)), no_options);

            // The order the keys were written in must not matter: re-prompting
            // for an equivalent file trains the user to click the modal away.
            // If this ever fails, `serde_json`'s map became insertion-ordered.
            let a = serde_json::json!({"one": 1, "two": 2});
            let b = serde_json::json!({"two": 2, "one": 1});
            assert_eq!(fingerprint(Some(&a)), fingerprint(Some(&b)));
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

    /// `trust.toml` is locked under the name every clew version has used
    /// (`trust.toml.lock`), so an update waits for an OLDER clew holding it
    /// instead of racing it.
    #[test]
    #[cfg(unix)]
    fn an_update_waits_for_an_older_clew_holding_the_legacy_lock() {
        use std::sync::atomic::{AtomicBool, Ordering};
        with_data_dir("trust-legacy-lock", |dir| {
            let record = dir.join("trust.toml");
            let older = crate::statefile::lock_named(&record, "trust.toml.lock").unwrap();
            assert!(older.is_held());
            let done = AtomicBool::new(false);
            std::thread::scope(|s| {
                let waiter = s.spawn(|| {
                    Trust::load()
                        .update(|t| t.trust_root(None, Path::new("/p")))
                        .unwrap();
                    done.store(true, Ordering::SeqCst);
                });
                std::thread::sleep(std::time::Duration::from_millis(80));
                assert!(!done.load(Ordering::SeqCst), "the update must wait");
                drop(older);
                waiter.join().unwrap();
            });
            assert!(Trust::load().is_root_trusted(None, Path::new("/p")));
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
        let dir = crate::testutil::TempDir::new("trust-nonreg");

        let dev = lsp_fingerprint(&dir, Path::new("/dev/zero"), &[], "s", "1", None);
        assert!(dev.is_err(), "a character device must be refused");

        let fifo = dir.join("fifo");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .is_ok_and(|s| s.success()),
            "mkfifo failed"
        );
        let piped = lsp_fingerprint(&dir, &fifo, &[], "s", "1", None);
        assert!(piped.is_err(), "a FIFO must be refused before the open");
    }
}

#[cfg(test)]
mod staging_tests {
    use super::*;

    use super::tests::with_data_dir;

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
                stage_lsp_command(&root, Path::new("server.sh"), &[], "s", "1", None, |_| {
                    approve
                })
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
                lsp_fingerprint(&root, Path::new("server.sh"), &[], "s", "1", None).unwrap()
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

    /// The window the open handle does not close: it pins the inode, not the
    /// bytes, and `approved` runs inside that window. A repository that
    /// rewrites the file in place right there would otherwise get bytes B
    /// stored under the name hash(A) — and `exec/<digest>` is trusted by name
    /// on every later start, so that entry would run as the approved command
    /// forever. Staging has to refuse, and leave nothing behind.
    #[test]
    fn a_source_rewritten_between_the_hash_and_the_copy_is_refused() {
        with_data_dir("clew-trust-staging-race", |dir| {
            let root = dir.join("proj");
            std::fs::create_dir_all(&root).unwrap();
            let cmd = root.join("server.sh");
            std::fs::write(&cmd, b"#!/bin/sh\necho approved\n").unwrap();
            let honest =
                lsp_fingerprint(&root, Path::new("server.sh"), &[], "s", "1", None).unwrap();

            // The closure IS the race: it runs after the bytes were hashed and
            // before they are copied, and it truncates the same inode the
            // staging handle is holding open.
            let raced =
                stage_lsp_command(&root, Path::new("server.sh"), &[], "s", "1", None, |fp| {
                    assert_eq!(fp, honest, "the user is shown the pre-swap fingerprint");
                    std::fs::write(&cmd, b"#!/bin/sh\necho pwned\n").unwrap();
                    true
                });
            assert!(
                raced.is_err(),
                "bytes that do not hash to the approved digest must not be staged"
            );

            // Nothing poisoned survives: no entry under the approved digest,
            // and no half-written temp file for a later stage to trip over.
            let leftovers: Vec<PathBuf> = std::fs::read_dir(dir.join("exec"))
                .map(|d| d.filter_map(|e| e.ok()).map(|e| e.path()).collect())
                .unwrap_or_default();
            assert!(leftovers.is_empty(), "left behind: {leftovers:?}");

            // …and staging still works once the file stops moving under it.
            std::fs::write(&cmd, b"#!/bin/sh\necho approved\n").unwrap();
            let ok =
                stage_lsp_command(&root, Path::new("server.sh"), &[], "s", "1", None, |_| true)
                    .expect("stages");
            assert_eq!(
                std::fs::read(ok.exec_path.expect("approved commands are staged")).unwrap(),
                b"#!/bin/sh\necho approved\n"
            );
        });
    }

    /// The options-only fingerprint is a separate question from the command
    /// one, and every input a repository controls has to move it. The failure
    /// this guards is an approval that starts covering something it never
    /// covered: `trust.toml` holds one fingerprint per (root, language), so a
    /// config that swaps `command` for `init_options` (or edits the options a
    /// commit later) must not ride on the answer already on record.
    #[test]
    fn an_options_fingerprint_moves_with_every_repo_controlled_input() {
        let opts = serde_json::json!({"rust-analyzer.check.command": "clippy"});
        let base = lsp_options_fingerprint(&[], "rust-analyzer", "1", &opts).unwrap();

        // Editing only the options invalidates it.
        let edited = serde_json::json!({
            "rust-analyzer.cargo.buildScripts.overrideCommand": ["/bin/sh", "-c", "id"]
        });
        assert_ne!(
            base,
            lsp_options_fingerprint(&[], "rust-analyzer", "1", &edited).unwrap()
        );
        // …as does switching the server or its version under the same options:
        // a key that is inert for one server can name a program for another.
        assert_ne!(
            base,
            lsp_options_fingerprint(&[], "other-server", "1", &opts).unwrap()
        );
        assert_ne!(
            base,
            lsp_options_fingerprint(&[], "rust-analyzer", "2", &opts).unwrap()
        );
        assert_ne!(
            base,
            lsp_options_fingerprint(&["--x".into()], "rust-analyzer", "1", &opts).unwrap()
        );
        // Same inputs, same value — the client records it and the server
        // re-derives it, so the two must never disagree.
        assert_eq!(
            base,
            lsp_options_fingerprint(&[], "rust-analyzer", "1", &opts).unwrap()
        );

        // Domain separation: approving a command whose fingerprint covers
        // these very options must not satisfy the options-only check.
        with_data_dir("clew-trust-options-domain", |dir| {
            let root = dir.join("proj");
            std::fs::create_dir_all(&root).unwrap();
            let cmd = root.join("server.sh");
            std::fs::write(&cmd, b"#!/bin/sh\n").unwrap();
            let with_cmd = lsp_fingerprint(
                &root,
                Path::new("server.sh"),
                &[],
                "rust-analyzer",
                "1",
                Some(&opts),
            )
            .unwrap();
            assert_ne!(with_cmd, base);
        });
    }

    /// Refusing, not resetting: a `trust.toml` clew cannot parse holds every
    /// root and approval the user ever gave. Recording one more consent must
    /// not write "empty + this change" over it.
    #[test]
    fn an_unparseable_trust_file_is_never_overwritten() {
        with_data_dir("clew-trust-malformed", |dir| {
            let path = dir.join("trust.toml");
            let broken = "roots = [\"/a\"\n[lsp\n";
            std::fs::write(&path, broken).unwrap();

            let mut t = Trust::load();
            assert!(t.roots().is_empty(), "a reader falls back to nothing");
            assert!(Trust::load_checked().is_err());
            let err = t
                .update(|t| t.trust_root(None, Path::new("/b")))
                .expect_err("an update over an unparseable record must refuse");
            assert!(err.contains("not valid TOML"), "{err}");
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                broken,
                "the user's bytes survive for them to fix"
            );
        });
    }

    /// The GUI hashes off its UI thread and asks the question on it, so the
    /// probe has to be able to cross threads — and approving it afterwards
    /// must yield exactly what the one-shot path yields.
    #[test]
    fn a_probe_can_be_hashed_on_one_thread_and_approved_on_another() {
        fn assert_send<T: Send>() {}
        assert_send::<CommandProbe>();
        with_data_dir("clew-trust-probe", |dir| {
            let root = dir.join("proj");
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(root.join("server.sh"), b"#!/bin/sh\necho approved\n").unwrap();
            let r = root.clone();
            let probe = std::thread::spawn(move || {
                probe_lsp_command(&r, Path::new("server.sh"), &[], "s", "1", None)
            })
            .join()
            .unwrap()
            .expect("probes");
            assert_eq!(
                probe.fingerprint(),
                lsp_fingerprint(&root, Path::new("server.sh"), &[], "s", "1", None).unwrap()
            );
            assert_eq!(probe.source(), root.join("server.sh"));
            let exec = std::thread::spawn(move || probe.into_exec())
                .join()
                .unwrap()
                .expect("stages");
            assert_eq!(std::fs::read(exec).unwrap(), b"#!/bin/sh\necho approved\n");
        });
    }

    /// A command that lives OUTSIDE the repository, somewhere only its owner
    /// can change, runs where it is: a copy in `exec/` breaks every program
    /// that finds its siblings through its own path (wrappers doing
    /// `dirname "$0"`, clangd's resource dir, `@loader_path` dylibs). Once
    /// anyone else can write the location, it is copied again — and a file
    /// replaced after it was hashed is refused rather than run.
    #[test]
    #[cfg(unix)]
    fn an_out_of_project_command_in_a_private_location_runs_in_place() {
        use std::os::unix::fs::PermissionsExt;
        with_data_dir("clew-trust-in-place", |dir| {
            let root = dir.join("proj");
            let tools = dir.join("tools");
            std::fs::create_dir_all(&root).unwrap();
            std::fs::create_dir_all(tools.join("lib")).unwrap();
            let wrapper = tools.join("ls-wrapper.sh");
            std::fs::write(
                &wrapper,
                "#!/bin/sh\nexec \"$(dirname \"$0\")/lib/real\" \"$@\"\n",
            )
            .unwrap();
            std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
            let canonical = wrapper.canonicalize().unwrap();
            assert!(
                trusted_location(&canonical),
                "precondition: the temp dir chain is private"
            );

            let stage = || {
                stage_lsp_command(&root, &wrapper, &[], "s", "1", None, |_| true)
                    .expect("stages")
                    .exec_path
                    .expect("approved")
            };
            assert_eq!(stage(), canonical, "run where it lives");
            assert!(
                !dir.join("exec").exists(),
                "nothing of it was copied into clew's directory"
            );

            // Group/other-writable directory: anyone could swap the file
            // between the hash and the exec, so the approved bytes are copied.
            std::fs::set_permissions(&tools, std::fs::Permissions::from_mode(0o777)).unwrap();
            let copied = stage();
            std::fs::set_permissions(&tools, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert!(copied.starts_with(dir.join("exec")), "{}", copied.display());

            // Replaced after it was hashed: the approval was for other bytes.
            let probe = probe_lsp_command(&root, &wrapper, &[], "s", "1", None).unwrap();
            let replacement = tools.join("next.sh");
            std::fs::write(&replacement, "#!/bin/sh\necho other\n").unwrap();
            std::fs::rename(&replacement, &wrapper).unwrap();
            let err = probe.into_exec().expect_err("a replaced file must not run");
            assert!(err.contains("re-approve"), "{err}");
        });
    }

    /// `exec/<fingerprint>` is never trusted by its name: bytes changed under
    /// an approved name are re-staged from the approved source, not run.
    #[test]
    #[cfg(unix)]
    fn a_staged_copy_is_reverified_not_trusted_by_its_name() {
        use std::os::unix::fs::PermissionsExt;
        with_data_dir("clew-trust-reverify", |dir| {
            let root = dir.join("proj");
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(root.join("server.sh"), b"#!/bin/sh\necho approved\n").unwrap();
            let stage = || {
                stage_lsp_command(&root, Path::new("server.sh"), &[], "s", "1", None, |_| true)
                    .expect("stages")
                    .exec_path
                    .expect("approved")
            };
            let first = stage();
            assert!(first.starts_with(dir.join("exec")));

            std::fs::set_permissions(&first, std::fs::Permissions::from_mode(0o700)).unwrap();
            std::fs::write(&first, b"#!/bin/sh\necho pwned\n").unwrap();
            std::fs::set_permissions(&first, std::fs::Permissions::from_mode(0o500)).unwrap();

            let again = stage();
            assert_eq!(again, first, "the same approval, the same name");
            assert_eq!(
                std::fs::read(&again).unwrap(),
                b"#!/bin/sh\necho approved\n",
                "the tampered copy was replaced with the approved bytes"
            );
            let mode = std::fs::metadata(&again).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o500);
        });
    }

    /// `exec/` stays bounded: an entry no approval refers to goes once nothing
    /// has used it for a month, and so does a crashed stage's temp file; what
    /// is referenced or recently used stays.
    #[test]
    fn the_exec_sweep_keeps_what_is_referenced_or_recent() {
        let d = crate::testutil::TempDir::new("trust-exec-sweep");
        let now = std::time::SystemTime::now();
        let old = now - std::time::Duration::from_secs(40 * 24 * 60 * 60);
        let hour_ago = now - std::time::Duration::from_secs(2 * 60 * 60);
        let make = |name: &str, mtime: std::time::SystemTime| {
            let p = d.join(name);
            std::fs::write(&p, b"x").unwrap();
            std::fs::File::options()
                .write(true)
                .open(&p)
                .unwrap()
                .set_modified(mtime)
                .unwrap();
        };
        make("referenced-but-old", old);
        make("unreferenced-but-recent", now);
        make("unreferenced-and-old", old);
        make(".tmp-1-0", hour_ago);
        make(".tmp-2-0", now);

        let referenced: std::collections::HashSet<&str> = ["referenced-but-old"].into();
        sweep_exec_in(&d, &referenced, now);

        for kept in ["referenced-but-old", "unreferenced-but-recent", ".tmp-2-0"] {
            assert!(d.join(kept).exists(), "{kept} must survive the sweep");
        }
        for gone in ["unreferenced-and-old", ".tmp-1-0"] {
            assert!(!d.join(gone).exists(), "{gone} must be swept");
        }
    }
}
