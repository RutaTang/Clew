//! Safe read/write for clew's own state files.
//!
//! Project state lives under `<root>/.clew/`, which ships with the repository
//! — a hostile repo controls its initial content. Reads must therefore refuse
//! anything that is not a plain, reasonably-sized file: a symlink to
//! `/dev/zero` would hang the open, one to a huge file would OOM, and one to
//! a sensitive file would quietly pull its contents into state that clew
//! later re-saves. Writes must never follow a pre-planted symlink at any of
//! the three names they touch: the temp file, the destination, or the `.lock`
//! file beside them.
//!
//! Every read, write, delete and lock of a clew STATE FILE goes through this
//! module — the project's `.clew/` stores and clew's own records and caches in
//! the global data directory (`config.toml`, `trust.toml`, the derived
//! caches): the reads ([`read_checked`] and its [`read`] shorthand), the
//! atomic writes ([`write_atomic`]), the deletes ([`remove`]), the lock
//! ([`lock`]) and the JSON-array merge ([`merge_file`]) — so the rules live in
//! exactly one place. What is not a state file has its own handling, where it
//! lives: the directories themselves (`derived::ensure_private_dir`), staged
//! language-server commands (`trust`), downloaded builds and their sweeps
//! (`server_dist`, the updater), installed servers (`lsp::store`).
//!
//! **Missing is not refused.** A caller about to WRITE must be able to tell
//! "there is no file yet" (start from empty) from "there is a file I could not
//! read or did not understand" (leave it alone). Collapsing both into `None`
//! is how one bookmark toggle used to replace an unreadable store with a
//! one-entry file. [`read_checked`] keeps the two apart, and every writer in
//! this module refuses on the second.

use std::io::Write;
use std::path::Path;

/// Byte cap for one state file. Explanation caches on large projects reach
/// single-digit megabytes; this is far above any legitimate file and far
/// below what would hurt to read.
pub const MAX_STATE_BYTES: u64 = 64 * 1024 * 1024;

/// Why a state file that exists could not be read. "It does not exist" is not
/// one of these: [`read_checked`] reports that as `Ok(None)`.
#[derive(Debug)]
pub enum ReadError {
    /// A `.clew` directory on the path is a symlink or not a directory, so the
    /// read would land outside the project.
    UnsafeDirectory,
    /// The leaf is a symlink, FIFO, device or directory — not a plain file.
    NotPlainFile,
    /// The path resolves outside the root it must be read under, through a
    /// symlink on its way (see `fs_scan::read_confined_capped_checked`).
    Outside,
    /// Larger than the cap the caller set: `size` bytes, as the open handle
    /// reported it (a file that grew past the cap while it was read is at
    /// least what was read of it).
    TooLarge { cap: u64, size: u64 },
    /// Not valid UTF-8 (every state file is text).
    NotUtf8,
    /// Permission denied, I/O error, stale handle, …
    Io(std::io::Error),
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadError::UnsafeDirectory => {
                f.write_str("a state directory on its path is a symlink or not a directory")
            }
            ReadError::NotPlainFile => f.write_str("it is not a plain file"),
            ReadError::Outside => f.write_str("it resolves outside the project"),
            ReadError::TooLarge { cap, .. } => write!(f, "it is larger than {cap} bytes"),
            ReadError::NotUtf8 => f.write_str("it is not valid UTF-8"),
            ReadError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ReadError {}

impl From<ReadError> for std::io::Error {
    fn from(e: ReadError) -> Self {
        match e {
            ReadError::Io(e) => e,
            other => std::io::Error::other(other.to_string()),
        }
    }
}

/// Read a state file as text. `Ok(None)` when it does not exist; `Err` when it
/// exists but may not or could not be read (see [`ReadError`]). Capped at
/// [`MAX_STATE_BYTES`].
pub fn read_checked(path: &Path) -> Result<Option<String>, ReadError> {
    read_capped_checked(path, MAX_STATE_BYTES)
}

/// [`read_checked`] with an explicit cap.
///
/// The cap is enforced on the READ, not only on the size the handle reported:
/// a file can grow between the `fstat` and the read (an appending process, a
/// pipe-like file), and a size check alone would let it past. The `fstat`
/// stays as a cheap early rejection so an oversized file is refused without
/// reading it first.
pub fn read_capped_checked(path: &Path, max_bytes: u64) -> Result<Option<String>, ReadError> {
    use std::io::Read;
    if !repo_dirs_are_real(path) {
        return Err(ReadError::UnsafeDirectory);
    }
    let Some(f) = open_plain_checked(path)? else {
        return Ok(None);
    };
    let len = f.metadata().map_err(ReadError::Io)?.len();
    if len > max_bytes {
        return Err(ReadError::TooLarge {
            cap: max_bytes,
            size: len,
        });
    }
    let mut bytes = Vec::new();
    // `max_bytes + 1`: reading one byte past the cap is what distinguishes
    // "exactly at the limit" from "grew past it while we were reading".
    (&f).take(max_bytes + 1)
        .read_to_end(&mut bytes)
        .map_err(ReadError::Io)?;
    if bytes.len() as u64 > max_bytes {
        return Err(grew_past(&f, max_bytes, bytes.len()));
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| ReadError::NotUtf8)
}

/// The refusal for a file that passed the size check on `f` and then read
/// past `cap` (`read` bytes): it grew while it was read, so its size is what
/// the handle reports now, and never less than what was read.
pub(crate) fn grew_past(f: &std::fs::File, cap: u64, read: usize) -> ReadError {
    let read = read as u64;
    let size = f.metadata().map_or(read, |m| m.len().max(read));
    ReadError::TooLarge { cap, size }
}

/// Legacy shorthand for [`read_checked`]: `None` for a missing file AND for
/// one that was refused. Fine for a caller that only DISPLAYS what it reads;
/// a caller that will write the file back must use [`read_checked`] (or
/// [`merge_file`]), or an unreadable store turns into an empty one.
pub fn read(path: &Path) -> Option<String> {
    read_checked(path).ok().flatten()
}

/// [`read`] with an explicit cap, for files that should be far smaller
/// (configs, indexes) — and for any repository-controlled file that must be
/// read without trusting its size or type (see [`crate::imports`]). Same
/// caveat as [`read`]: missing and refused are both `None`.
pub fn read_capped(path: &Path, max_bytes: u64) -> Option<String> {
    read_capped_checked(path, max_bytes).ok().flatten()
}

/// Open `path` as a plain file, race-free: the leaf must not be a symlink
/// (`O_NOFOLLOW`), the open never blocks on a FIFO (`O_NONBLOCK`), and the
/// file-type check runs on the OPEN handle (fstat) — so nothing swapped in
/// between a check and the read can redirect or wedge it.
///
/// `Ok(None)` when nothing is there; `Err` for a leaf that is not a plain file
/// (or any other failure).
#[cfg(unix)]
pub fn open_plain_checked(path: &Path) -> Result<Option<std::fs::File>, ReadError> {
    use std::os::unix::fs::OpenOptionsExt;
    let f = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        // `O_NOFOLLOW` on a symlink: ELOOP. A socket cannot be opened at
        // all: EOPNOTSUPP on macOS, ENXIO on Linux — as a device with none
        // behind it. Said as the host's raw error, it read as a failure to
        // read a file.
        Err(e)
            if matches!(
                e.raw_os_error(),
                Some(libc::ELOOP | libc::EOPNOTSUPP | libc::ENXIO)
            ) =>
        {
            return Err(ReadError::NotPlainFile);
        }
        Err(e) => return Err(ReadError::Io(e)),
    };
    if !f.metadata().map_err(ReadError::Io)?.is_file() {
        return Err(ReadError::NotPlainFile);
    }
    Ok(Some(f))
}

/// Best effort without O_NOFOLLOW: pre-check, then open. The residual
/// check-to-open race exists only on non-unix hosts.
#[cfg(not(unix))]
pub fn open_plain_checked(path: &Path) -> Result<Option<std::fs::File>, ReadError> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(ReadError::Io(e)),
    };
    if !meta.is_file() {
        return Err(ReadError::NotPlainFile);
    }
    std::fs::File::open(path).map(Some).map_err(ReadError::Io)
}

/// [`open_plain_checked`] without the reason: `None` for missing and refused
/// alike.
pub fn open_plain(path: &Path) -> Option<std::fs::File> {
    open_plain_checked(path).ok().flatten()
}

/// Delete a state file (the empty-state save path). A missing file is fine;
/// everything else that stops the delete is an ERROR the caller must see — a
/// swallowed failure leaves stale state that quietly resurrects on the next
/// launch. Refuses (like every state operation) when a `.clew` ancestor is a
/// symlink or the target is not a plain file: with `.clew -> /outside`, the
/// fixed file names clew deletes would land on someone else's files.
/// (`remove_file` itself never follows a symlink at the leaf.)
pub fn remove(path: &Path) -> std::io::Result<()> {
    if !repo_dirs_are_real(path) {
        return Err(std::io::Error::other(
            "a state directory is a symlink — refusing to delete through it",
        ));
    }
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
        Ok(m) if m.is_file() => std::fs::remove_file(path),
        Ok(_) => Err(std::io::Error::other(
            "refusing to delete: not a plain file",
        )),
    }
}

/// Every directory from the path's `.clew` component down to its parent must
/// be a real directory, not a symlink: `.clew` ships with the repository, so
/// `.clew -> /outside` (or `.clew/cache -> …`) would redirect every state
/// read, write and delete outside the project. Ancestors ABOVE `.clew` are
/// not checked — a symlinked project root is the user's own, legitimate path
/// choice. Paths with no `.clew` component (the global data dir) have no
/// repo-controlled segment and pass.
///
/// Missing directories pass: the write path creates them (as real
/// directories) right after this check.
///
/// Residual, deliberately accepted: this walks the chain by NAME, so a
/// directory replaced with a symlink between this check and the operation
/// that follows would not be caught. Closing that needs the whole read/write
/// path rebuilt on `openat`/`renameat` against held directory handles.
///
/// The threat this module defends against is a repository's *committed*
/// contents — a `.clew` symlink that is already there when clew opens the
/// project — and against that, checking by name is exact. Winning the
/// remaining window instead requires an attacker already executing code on
/// the user's machine, concurrently, at which point clew's state files are
/// not the interesting target. Every leaf is separately safe regardless:
/// reads open with `O_NOFOLLOW` and type-check the handle, writes create their
/// temp file with `O_EXCL` and `rename` over the destination rather than
/// writing through whatever is there, and [`lock`]'s `.lock` file — the one
/// leaf a repository can plant a link at without also having to supply its
/// contents — opens with `O_NOFOLLOW` and type-checks its handle too.
pub(crate) fn repo_dirs_are_real(path: &Path) -> bool {
    use std::path::PathBuf;
    let comps: Vec<_> = path.components().collect();
    let Some(pos) = comps.iter().position(|c| c.as_os_str() == ".clew") else {
        return true;
    };
    let mut probe = PathBuf::new();
    for c in &comps[..pos] {
        probe.push(c);
    }
    // Probe each prefix from `.clew` (inclusive) up to the file's parent.
    for c in &comps[pos..comps.len().saturating_sub(1)] {
        probe.push(c);
        match std::fs::symlink_metadata(&probe) {
            Ok(m) if m.is_dir() => {}
            // Not there yet (fresh project): the rest is missing too, and
            // whoever creates it creates real directories.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return true,
            // A symlink, a file squatting on the name, or an unreadable
            // entry: refuse.
            _ => return false,
        }
    }
    true
}

/// What `<root>/.clew/.gitignore` holds when clew creates it.
///
/// `.clew/` sits inside the user's repository, and several of the files clew
/// keeps there are one reader's private session — the navigation trail, the
/// reading target, lock files, atomic-write temp files, the generated-tour
/// cache — which must not show up as untracked noise in `git status`, let
/// alone get committed by an `git add -A`. Bookmarks and notes, `lsp.toml` and
/// `launch.json` are deliberately NOT listed: they are meant to travel with
/// the project.
pub const CLEW_GITIGNORE: &str = "\
# Written by clew: per-reader state that does not belong in the repository.
# Bookmarks, notes, lsp.toml and launch.json stay trackable on purpose.
history.json
reading.toml
*.lock
*.tmp
cache/
";

/// The `.clew` directory `path` lies under, if any (the first `.clew`
/// component, inclusive).
fn clew_dir_of(path: &Path) -> Option<std::path::PathBuf> {
    let mut dir = std::path::PathBuf::new();
    for c in path.components() {
        dir.push(c);
        if c.as_os_str() == ".clew" {
            return Some(dir);
        }
    }
    None
}

/// Create `<root>/.clew/.gitignore` ([`CLEW_GITIGNORE`]) the first time clew
/// writes into a `.clew/` that has none. Never touches an existing one — the
/// user's or the repository's own rules win — and never follows a symlink at
/// the name (`create_new` is `O_CREAT|O_EXCL`). Best effort: a missing ignore
/// file is noise in `git status`, not a reason to fail the user's save.
///
/// Callers must have checked [`repo_dirs_are_real`] first.
fn ensure_clew_gitignore(path: &Path) {
    let Some(dir) = clew_dir_of(path) else {
        return;
    };
    let ignore = dir.join(".gitignore");
    if std::fs::symlink_metadata(&ignore).is_ok() || !dir.is_dir() {
        return;
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&ignore)
    {
        let _ = f.write_all(CLEW_GITIGNORE.as_bytes());
    }
}

/// Atomic write: a uniquely-named `create_new` temp file beside the target,
/// flushed to stable storage, then renamed over it.
///
/// - `create_new` (O_CREAT|O_EXCL) refuses to open through anything already
///   at the temp path — including a dangling symlink a repository planted at
///   a predictable name.
/// - The data is synced before the rename (see `sync_file`): without it a
///   crash shortly after the rename can leave the NEW name pointing at a file
///   whose data never reached the disk — an empty or torn store, which a
///   reader would then refuse (or, before [`read_checked`], silently treat as
///   empty and overwrite).
/// - `rename` replaces a symlink at the destination rather than writing
///   through it; the directory is then synced (best effort) so the rename
///   itself is durable.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    write_atomic_mode(path, bytes, None)
}

/// [`write_atomic`] for files carrying secrets (API keys): the temp file is
/// CREATED readable only by the user (0600 on unix), so the secret never
/// exists on disk with wider permissions — not even between create and
/// rename. (The rename preserves the temp file's mode.)
pub fn write_atomic_secret(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    write_atomic_mode(path, bytes, Some(0o600))
}

fn write_atomic_mode(path: &Path, bytes: &[u8], mode: Option<u32>) -> std::io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("state path has no parent"))?;
    // Checked BEFORE create_dir_all: with `.clew -> /outside` already in the
    // repo, create_dir_all would follow the link and the whole write (temp
    // file included) would land outside the project.
    if !repo_dirs_are_real(path) {
        return Err(std::io::Error::other(
            "a state directory is a symlink — refusing to write through it",
        ));
    }
    std::fs::create_dir_all(dir)?;
    ensure_clew_gitignore(path);
    let base = path.file_name().unwrap_or_default().to_string_lossy();
    let pid = std::process::id();
    for attempt in 0..16u32 {
        let tmp = dir.join(format!(".{base}.{pid}.{attempt}.tmp"));
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        if let Some(mode) = mode {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(mode);
        }
        #[cfg(not(unix))]
        let _ = mode;
        match opts.open(&tmp) {
            Ok(mut f) => {
                let written = f.write_all(bytes).and_then(|_| sync_file(&f));
                drop(f);
                if let Err(e) = written {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(e);
                }
                if let Err(e) = std::fs::rename(&tmp, path) {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(e);
                }
                sync_dir(dir);
                return Ok(());
            }
            // Something (stale temp, planted link) occupies this name: next.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other(
        "could not create a temp file for the atomic write",
    ))
}

/// Flush a directory's entries (the rename that just happened) to stable
/// storage. Best effort: some filesystems refuse `fsync` on a directory, and
/// the data itself is already synced — what is at stake here is only whether
/// the rename survives a crash, and the old file is intact if it does not.
fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = sync_file(&d);
    }
    #[cfg(not(unix))]
    let _ = dir;
}

/// Push a file's data to the device before it is renamed into place.
///
/// Plain `fsync(2)` on Apple platforms, where `File::sync_all` is
/// `F_FULLFSYNC`: that also drains the drive's own write cache, costs tens of
/// milliseconds, and state files are written on the UI thread as often as
/// every navigation (the reading trail). `fsync` already orders the data
/// ahead of the rename against an OS crash — the torn-or-empty store this
/// exists to prevent; only a power cut racing the drive's cache is left,
/// where the old file is the likely survivor.
fn sync_file(f: &std::fs::File) -> std::io::Result<()> {
    #[cfg(target_vendor = "apple")]
    {
        use std::os::unix::io::AsRawFd;
        loop {
            if unsafe { libc::fsync(f.as_raw_fd()) } == 0 {
                return Ok(());
            }
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::EINTR) {
                return Err(err);
            }
        }
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        f.sync_all()
    }
}

// ------------------------------------------------------- JSON-array stores

/// The newest on-disk layout of the JSON-array stores (bookmarks, notes, the
/// walkthrough library) this build understands.
///
/// Two layouts exist, and both parse:
///
/// - a bare JSON array of entry objects — schema 1, and still what clew
///   writes, because every clew release that exists today reads exactly that
///   and these files can be committed and shared with teammates on older
///   versions;
/// - an envelope `{"schema_version": N, "entries": [...]}` — reserved for the
///   first change a bare array cannot express. A file whose `schema_version`
///   is newer than this constant was written by a newer clew: it is shown if
///   it can be, but NEVER rewritten, so a downgrade (or an old teammate)
///   cannot destroy what the newer version stored.
pub const ARRAY_STORE_SCHEMA: u64 = 1;

/// Why a store's current bytes cannot be merged into (or overwritten).
#[derive(Debug)]
pub enum StoreError {
    /// The file exists but could not be read safely.
    Refused(ReadError),
    /// The text is not a store this build understands (syntax error, wrong
    /// shape, an entry of the wrong type).
    Unparseable(String),
    /// Written by a newer clew, whose layout this build does not know.
    NewerSchema { found: u64, supported: u64 },
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Refused(e) => write!(f, "cannot be read: {e}"),
            StoreError::Unparseable(e) => write!(f, "cannot be parsed: {e}"),
            StoreError::NewerSchema { found, supported } => write!(
                f,
                "was written by a newer clew (schema {found}; this version understands {supported})"
            ),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<StoreError> for std::io::Error {
    /// Of kind `InvalidData` when the store's own content is what refuses the
    /// operation — it is not a plain file, too large, not text, not a store
    /// this build understands — so trying again changes nothing until the
    /// file does (see [`is_refusal`]).
    ///
    /// A store that could not be read because the READ failed
    /// ([`ReadError::Io`]: an I/O error, too many open files, a stale handle)
    /// is not refused by its content, and keeps the kind of its error (never
    /// `InvalidData`): the next attempt may well succeed. It used to be
    /// counted a refusal, and a remote edit that met a moment of `EMFILE` was
    /// dropped at its first try.
    fn from(e: StoreError) -> Self {
        let kind = match &e {
            StoreError::Refused(ReadError::Io(io)) => match io.kind() {
                std::io::ErrorKind::InvalidData => std::io::ErrorKind::Other,
                kind => kind,
            },
            _ => std::io::ErrorKind::InvalidData,
        };
        std::io::Error::new(
            kind,
            format!("{e} — left untouched rather than overwritten"),
        )
    }
}

/// Whether `e`, from a store operation here, is the store refusing it — its
/// content cannot be read safely or understood ([`StoreError`]) — rather than
/// the attempt failing (an I/O error, a lock that could not be taken): the
/// one is permanent until somebody changes the file, the other worth trying
/// again.
pub fn is_refusal(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::InvalidData
}

/// A parsed JSON-array store, remembering which layout it came in so a merge
/// writes it back in the same one.
#[derive(Debug, Clone, PartialEq)]
pub struct ArrayStore {
    /// The entry objects, in file order.
    pub entries: Vec<serde_json::Value>,
    /// `Some(n)` when the file was an envelope with `schema_version: n`;
    /// `None` for a bare array (schema 1).
    pub envelope: Option<u64>,
}

impl ArrayStore {
    /// Render in the layout the store was read in.
    pub fn to_text(&self) -> Option<String> {
        match self.envelope {
            None => serde_json::to_string_pretty(&self.entries).ok(),
            Some(version) => serde_json::to_string_pretty(&serde_json::json!({
                "schema_version": version,
                "entries": self.entries,
            }))
            .ok(),
        }
    }
}

/// Parse a JSON-array store in either layout (see [`ARRAY_STORE_SCHEMA`]).
/// Every entry must be a JSON object; anything else means the file is not
/// one of clew's stores, and it is refused rather than half-understood.
pub fn parse_array_store(text: &str) -> Result<ArrayStore, StoreError> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|e| StoreError::Unparseable(e.to_string()))?;
    let (entries, envelope) = match value {
        serde_json::Value::Array(entries) => (entries, None),
        serde_json::Value::Object(mut obj) => {
            let version = obj
                .get("schema_version")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| {
                    StoreError::Unparseable("an object without a numeric `schema_version`".into())
                })?;
            if version > ARRAY_STORE_SCHEMA {
                return Err(StoreError::NewerSchema {
                    found: version,
                    supported: ARRAY_STORE_SCHEMA,
                });
            }
            match obj.remove("entries") {
                Some(serde_json::Value::Array(entries)) => (entries, Some(version)),
                _ => {
                    return Err(StoreError::Unparseable(
                        "an envelope without an `entries` array".into(),
                    ));
                }
            }
        }
        _ => {
            return Err(StoreError::Unparseable(
                "neither an array nor a versioned envelope".into(),
            ));
        }
    };
    if let Some(i) = entries.iter().position(|e| !e.is_object()) {
        return Err(StoreError::Unparseable(format!(
            "entry {} is not an object",
            i + 1
        )));
    }
    Ok(ArrayStore { entries, envelope })
}

/// Read a JSON-array store for a read-modify-write: `Ok(None)` when there is
/// no file yet, `Err` when there is one that must not be rewritten.
pub fn load_array_store(path: &Path) -> Result<Option<ArrayStore>, StoreError> {
    match read_checked(path) {
        Ok(None) => Ok(None),
        Ok(Some(text)) => parse_array_store(&text).map(Some),
        Err(e) => Err(StoreError::Refused(e)),
    }
}

/// Apply one entry-level change to a state file that holds a JSON array of
/// objects, returning the file's new text (`None` = the store is empty and its
/// file should be deleted, which is what every such store means by an empty
/// list).
///
/// This is the merge half of [`clew_protocol::StateMerge`]: the same
/// read-modify-write the local stores do under [`lock`], expressed as data so
/// it can be carried over the wire and applied where the file is — the only
/// place two clients' writes are both visible.
///
/// `current` is the file's text, `None` when there is no file. Text that is
/// not a store this build understands — a hand edit with a typo, a git
/// conflict marker, a newer clew's layout — is an error: the caller must leave
/// the file as it is. (Treating it as empty is how a single toggle used to
/// replace a whole store with one entry.)
pub fn merge_entries_checked(
    current: Option<&str>,
    op: &clew_protocol::StateMerge,
) -> Result<Option<String>, StoreError> {
    let store = match current {
        None => ArrayStore {
            entries: Vec::new(),
            envelope: None,
        },
        Some(text) => parse_array_store(text)?,
    };
    Ok(apply_merge(store, op))
}

fn apply_merge(mut store: ArrayStore, op: &clew_protocol::StateMerge) -> Option<String> {
    use clew_protocol::StateEdit;
    let list = &mut store.entries;
    let at = list.iter().position(|e| op.matches(e));

    match (&op.edit, at) {
        (StateEdit::Remove, Some(i)) => {
            list.remove(i);
        }
        (StateEdit::Remove, None) => {}
        (StateEdit::Upsert(entry), Some(i)) => list[i] = entry.clone(),
        (StateEdit::Upsert(entry), None) => list.push(entry.clone()),
        // Toggle resolves against the file, not against the caller's copy of
        // it: whether the bookmark is there is exactly what a stale snapshot
        // gets wrong.
        (StateEdit::Toggle(_), Some(i)) => {
            list.remove(i);
        }
        (StateEdit::Toggle(entry), None) => list.push(entry.clone()),
        (
            StateEdit::Patch {
                fields,
                insert,
                empty_when,
            },
            at,
        ) => {
            let i = match (at, insert) {
                (Some(i), _) => i,
                // Nothing to patch and no seed: the entry the caller meant is
                // gone (another client deleted it). Dropping the patch is
                // right — resurrecting it from a stale copy is not.
                (None, None) => return finish(store, op.delete_when_empty),
                (None, Some(seed)) => {
                    list.push(seed.clone());
                    list.len() - 1
                }
            };
            if let Some(obj) = list[i].as_object_mut() {
                for (k, v) in fields {
                    obj.insert(k.clone(), v.clone());
                }
            }
            // An entry whose every "carries information" field is blank is
            // dropped — how a reading note with neither the understood flag
            // nor any text says it no longer exists. The store names the
            // fields; the rule is not baked in here.
            if !empty_when.is_empty() && empty_when.iter().all(|k| is_blank(&list[i], k)) {
                list.remove(i);
            }
        }
    }
    finish(store, op.delete_when_empty)
}

fn finish(store: ArrayStore, delete_when_empty: bool) -> Option<String> {
    if store.entries.is_empty() && delete_when_empty {
        return None;
    }
    store.to_text()
}

/// Whether `entry.key` carries no information: absent, null, false, or an
/// empty/whitespace-only string.
fn is_blank(entry: &serde_json::Value, key: &str) -> bool {
    match entry.get(key) {
        None | Some(serde_json::Value::Null) => true,
        Some(serde_json::Value::Bool(b)) => !b,
        Some(serde_json::Value::String(s)) => s.trim().is_empty(),
        Some(_) => false,
    }
}

/// How a [`clew_protocol::StateEdit::Patch`] treats whether its entry is
/// there — which is what decides how it orders against the other edits of
/// that entry ([`supersedes`], [`commutes`]).
enum PatchShape<'a> {
    /// It never creates the entry and never drops it (`insert: None`, no
    /// `empty_when`): it sets its fields on an entry that is there, and
    /// leaves alone one that is not — a bookmark's note.
    Keeps(&'a serde_json::Map<String, serde_json::Value>),
    /// It sets fields of an entry that is there exactly when one of the
    /// fields that carry information (`empty_when`) is not blank: seeded,
    /// when absent, from an entry that holds the key and leaves every other
    /// such field blank, and dropped once all of them are — a reading note.
    /// As far as those fields and the key go, such an entry is just their
    /// values, all blank when it is absent.
    Fields {
        fields: &'a serde_json::Map<String, serde_json::Value>,
        information: &'a [String],
    },
    /// Any other patch: ordered against every edit of its entry. A patch
    /// that sets a field of the key, or seeds an entry that does not hold
    /// it, is one: it leaves an entry at another key, which no later edit of
    /// this one overwrites.
    Other,
}

/// The shape of `merge`'s patch; `None` when it is not a patch.
fn patch_shape(merge: &clew_protocol::StateMerge) -> Option<PatchShape<'_>> {
    let clew_protocol::StateEdit::Patch {
        fields,
        insert,
        empty_when,
    } = &merge.edit
    else {
        return None;
    };
    if merge.key_fields.iter().any(|k| fields.contains_key(k)) {
        return Some(PatchShape::Other);
    }
    Some(match insert {
        None if empty_when.is_empty() => PatchShape::Keeps(fields),
        Some(seed)
            if !empty_when.is_empty()
                && merge.matches(seed)
                && empty_when
                    .iter()
                    .all(|k| fields.contains_key(k) || is_blank(seed, k)) =>
        {
            PatchShape::Fields {
                fields,
                information: empty_when,
            }
        }
        _ => PatchShape::Other,
    })
}

/// Whether `a` and `b` address the same entry of a store.
fn same_entry(a: &clew_protocol::StateMerge, b: &clew_protocol::StateMerge) -> bool {
    a.key_fields == b.key_fields && a.key == b.key
}

/// Whether every field `earlier` sets, `later` sets too.
fn sets_all_of(
    later: &serde_json::Map<String, serde_json::Value>,
    earlier: &serde_json::Map<String, serde_json::Value>,
) -> bool {
    earlier.keys().all(|k| later.contains_key(k))
}

/// Whether `a` and `b` set no field in common.
fn disjoint(
    a: &serde_json::Map<String, serde_json::Value>,
    b: &serde_json::Map<String, serde_json::Value>,
) -> bool {
    a.keys().all(|k| !b.contains_key(k))
}

/// Whether two stores' `empty_when` name the same fields.
fn same_fields(a: &[String], b: &[String]) -> bool {
    a.iter().all(|k| b.contains(k)) && b.iter().all(|k| a.contains(k))
}

/// Whether `merge` changes the entry its key addresses and nothing else:
/// whatever it leaves is at that key, where a later edit of the key finds
/// it. An upsert or a toggle whose entry does not hold the key, or a patch
/// of neither shape, may leave one elsewhere.
fn stays_on_its_key(merge: &clew_protocol::StateMerge) -> bool {
    use clew_protocol::StateEdit;
    match &merge.edit {
        StateEdit::Remove => true,
        StateEdit::Upsert(entry) | StateEdit::Toggle(entry) => merge.matches(entry),
        StateEdit::Patch { .. } => matches!(
            patch_shape(merge),
            Some(PatchShape::Keeps(_) | PatchShape::Fields { .. })
        ),
    }
}

/// Whether `later`, applied after `earlier` — two edits of the same entry of
/// one store — leaves the store as `later` alone would, whatever the store
/// holds: `earlier` may then be left out, once `later` is sure to be applied
/// after anything `earlier` could have done. `false` for edits of different
/// entries.
///
/// - A removal removes the entry whatever an earlier edit that stays on its
///   key ([`stays_on_its_key`]) made of it.
/// - An upsert replaces the entry where it stands, or appends it: whatever
///   an earlier upsert or a patch that never creates or drops it did. Not
///   after an edit that may remove the entry — a removal, a toggle — which
///   would have the upsert append it, where it had stood elsewhere: the
///   walkthrough library is shown in the order of its file.
/// - A toggle goes by whether the entry is there, which a patch that never
///   creates or drops it does not change.
/// - A patch that sets every field the earlier one sets, of the same shape
///   ([`PatchShape`]), leaves nothing of it. For the shape that creates and
///   drops its entry, "the same" is on the key and on every field that
///   carries information, not on where the entry stands: the earlier one
///   may have dropped the entry for the later one to seed again, at the end
///   of the file and with the store's other fields as the seed has them.
///
/// Anything else is not known to, and is not taken to: the edits the app
/// leaves out on this say are only ever redundant. Said of a store that
/// holds each entry once, as every clew store writes it: where a hand edit
/// or a merge of the file left one twice, an edit addresses the first, and
/// what leaving one out leaves may differ in how many copies stay.
pub fn supersedes(later: &clew_protocol::StateMerge, earlier: &clew_protocol::StateMerge) -> bool {
    use clew_protocol::StateEdit;
    if !same_entry(later, earlier) {
        return false;
    }
    match &later.edit {
        StateEdit::Remove => stays_on_its_key(earlier),
        StateEdit::Upsert(_) => match &earlier.edit {
            StateEdit::Upsert(entry) => earlier.matches(entry),
            _ => matches!(patch_shape(earlier), Some(PatchShape::Keeps(_))),
        },
        StateEdit::Toggle(_) => matches!(patch_shape(earlier), Some(PatchShape::Keeps(_))),
        StateEdit::Patch { .. } => match (patch_shape(later), patch_shape(earlier)) {
            (Some(PatchShape::Keeps(later)), Some(PatchShape::Keeps(earlier))) => {
                sets_all_of(later, earlier)
            }
            (
                Some(PatchShape::Fields {
                    fields: later,
                    information: kept_by,
                }),
                Some(PatchShape::Fields {
                    fields: earlier,
                    information,
                }),
            ) => same_fields(kept_by, information) && sets_all_of(later, earlier),
            _ => false,
        },
    }
}

/// Whether `a` and `b` — two edits of the same entry of one store — leave it
/// the same whichever is applied first: each may land before the other.
/// `false` for edits of different entries, whose order no rule looks at.
///
/// - Two removals, and two upserts or two toggles of the same entry value.
/// - A removal and a patch that never creates the entry: either way, it is
///   gone.
/// - Two patches of the same shape ([`PatchShape`]) that set no field in
///   common. For the shape that creates and drops its entry, "the same" is
///   on the key and on every field that carries information, not on where
///   the entry stands, as in [`supersedes`].
///
/// Anything else is not known to, and is not taken to: two such edits land
/// in the order they were made. Said, as [`supersedes`] is, of a store that
/// holds each entry once.
pub fn commutes(a: &clew_protocol::StateMerge, b: &clew_protocol::StateMerge) -> bool {
    use clew_protocol::StateEdit;
    if !same_entry(a, b) {
        return false;
    }
    match (&a.edit, &b.edit) {
        (StateEdit::Remove, StateEdit::Remove) => true,
        (StateEdit::Upsert(x), StateEdit::Upsert(y))
        | (StateEdit::Toggle(x), StateEdit::Toggle(y)) => x == y,
        (StateEdit::Remove, StateEdit::Patch { .. }) => {
            matches!(patch_shape(b), Some(PatchShape::Keeps(_)))
        }
        (StateEdit::Patch { .. }, StateEdit::Remove) => {
            matches!(patch_shape(a), Some(PatchShape::Keeps(_)))
        }
        (StateEdit::Patch { .. }, StateEdit::Patch { .. }) => {
            match (patch_shape(a), patch_shape(b)) {
                (Some(PatchShape::Keeps(a)), Some(PatchShape::Keeps(b))) => disjoint(a, b),
                (
                    Some(PatchShape::Fields {
                        fields: a,
                        information: kept_by_a,
                    }),
                    Some(PatchShape::Fields {
                        fields: b,
                        information: kept_by_b,
                    }),
                ) => same_fields(kept_by_a, kept_by_b) && disjoint(a, b),
                _ => false,
            }
        }
        _ => false,
    }
}

/// What [`merge_file`] did with one edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Merged {
    /// The file's text after it (`None` = the store is empty and its file
    /// was deleted).
    pub text: Option<String>,
    /// False when the edit had been applied before — a replay of an edit
    /// whose reply was lost — and nothing was changed now.
    pub applied: bool,
}

/// The whole read-modify-write of one [`clew_protocol::StateMerge`] against
/// the file at `path`, applied at most once per `edit_id`: lock, read, merge,
/// write (or delete) — refusing, and leaving the file byte-for-byte as it
/// was, when its current content was refused or cannot be understood.
///
/// **At most once.** The sender of a remote edit cannot tell a request that
/// never arrived from one whose reply was lost with the transport, so it
/// sends the same edit again, under the same id, over the next one — which
/// may reach another server process, so the ids applied are kept on disk
/// (`EditLedger`), per store, in the project's `.clew/cache/edits/` (see
/// `edit_ledger_path`). A repeated id is answered with the file as it is,
/// and changes nothing: a toggle replayed after it landed would undo it.
///
/// The ledger is written AHEAD of the store: the edit is announced as pending
/// with the digest the store will have once it lands, then the store is
/// written, then the edit is confirmed. A crash between those writes leaves
/// the pending entry, and the next writer of the store settles it under the
/// same lock by comparing the store's digest with the one announced — landed,
/// it is confirmed; not, it is dropped, and a replay applies it. So a crash
/// can neither apply an edit twice nor lose one that a replay carries.
///
/// That holds for every writer that takes the store's [`lock`] and settles
/// first: this function, and — through [`settle_pending_edit`] — the app's
/// own edits of a store it has open locally, and the server's wholesale
/// `WriteState`. What remains is a change made behind clew's back between a
/// crash and the replay — a `git checkout` or `git pull` of a committed
/// store, a hand edit, a clew-server from before the ledger — which moves the
/// digest without settling: the pending edit then reads as not landed, and
/// its replay applies it again (for a toggle, undoing it). The window runs
/// from the crash until the client reconnects and replays.
///
/// This is the one implementation of a remote edit; a writer that replaces a
/// mergeable store wholesale settles with [`settle_pending_edit`] first. The
/// error names the reason in words a status line can show.
pub fn merge_file(
    path: &Path,
    op: &clew_protocol::StateMerge,
    edit_id: &str,
) -> Result<Merged, std::io::Error> {
    merge_file_until(path, op, edit_id, Interrupt::Never)
}

/// Where [`merge_file_until`] stops short, as a failed write or a crash
/// would — so tests can reproduce both at the real write points.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Interrupt {
    /// Run to the end: [`merge_file`].
    Never,
    /// The store write fails after the edit was announced.
    StoreWriteFails,
    /// The process dies after the store write, before the edit is confirmed.
    AfterStoreWrite,
}

/// [`merge_file`], stopping at `interrupt`.
pub(crate) fn merge_file_until(
    path: &Path,
    op: &clew_protocol::StateMerge,
    edit_id: &str,
    interrupt: Interrupt,
) -> Result<Merged, std::io::Error> {
    if !clew_protocol::valid_edit_id(edit_id) {
        return Err(std::io::Error::other(format!(
            "{edit_id:?} is not an edit id"
        )));
    }
    let ledger_path = edit_ledger_path(path)?;
    let _exclusive = lock(path)?;
    let current = read_checked(path).map_err(StoreError::Refused)?;
    let mut ledger = EditLedger::load(&ledger_path)?;
    let settled = ledger.settle(&content_digest(current.as_deref()));
    if ledger.applied.iter().any(|id| id == edit_id) {
        // What the edit's first reply would have carried, as far as it is
        // still the truth: the store as it is now — held to the same
        // standard a merge holds it to.
        if let Some(text) = &current {
            parse_array_store(text)?;
        }
        if settled {
            ledger.save(&ledger_path)?;
        }
        return Ok(Merged {
            text: current,
            applied: false,
        });
    }
    let merged = merge_entries_checked(current.as_deref(), op)?;
    if merged == current {
        // Nothing to write (removing what is not there): the edit is done.
        ledger.record(edit_id);
        ledger.save(&ledger_path)?;
        return Ok(Merged {
            text: merged,
            applied: true,
        });
    }
    ledger.pending = Some(PendingEdit {
        id: edit_id.to_string(),
        after: content_digest(merged.as_deref()),
    });
    ledger.save(&ledger_path)?;
    if interrupt == Interrupt::StoreWriteFails {
        return Err(std::io::Error::other("the store write failed"));
    }
    match &merged {
        Some(text) => write_atomic(path, text.as_bytes())?,
        None => remove(path)?,
    }
    if interrupt == Interrupt::AfterStoreWrite {
        return Err(std::io::Error::other("crashed before confirming the edit"));
    }
    // Best effort: a pending entry whose write landed is confirmed by the
    // next writer's settling anyway (see above).
    ledger.settle(&content_digest(merged.as_deref()));
    let _ = ledger.save(&ledger_path);
    Ok(Merged {
        text: merged,
        applied: true,
    })
}

/// For a writer about to replace the store at `path` wholesale, not through
/// [`merge_file`] — the app editing a project it has open locally, the
/// server's `WriteState` — while it holds the store's [`lock`]: settle the
/// remote edit a crash may have left pending against the store AS IT IS,
/// before it is replaced. Settled later, against the replacement, a pending
/// edit that had landed reads as not landed and is dropped, and its replay
/// applies it a second time — a toggle undoing itself.
///
/// Reads nothing, and writes nothing, when the store has no ledger (a store
/// no remote edit ever reached), or nothing pending in it. An error means
/// the caller must not write either: the pending edit could not be settled.
pub fn settle_pending_edit(path: &Path) -> std::io::Result<()> {
    let Ok(ledger_path) = edit_ledger_path(path) else {
        // Not a project store: no remote edit has a ledger for it.
        return Ok(());
    };
    match std::fs::symlink_metadata(&ledger_path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        _ => {}
    }
    let mut ledger = EditLedger::load(&ledger_path)?;
    if ledger.pending.is_none() {
        return Ok(());
    }
    let current = read_checked(path).map_err(StoreError::Refused)?;
    if ledger.settle(&content_digest(current.as_deref())) {
        ledger.save(&ledger_path)?;
    }
    Ok(())
}

/// How many applied edit ids an `EditLedger` keeps per store. A replay
/// comes from the one client whose transport died, right after it
/// reconnects: far fewer edits than this land in between.
pub const EDIT_LEDGER_CAP: usize = 1024;

/// The edits applied to one store (see [`merge_file`]): the newest
/// [`EDIT_LEDGER_CAP`] ids, oldest first, and at most one edit announced but
/// not yet known to have landed.
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct EditLedger {
    applied: Vec<String>,
    pending: Option<PendingEdit>,
}

/// An edit announced before its store was written: its id, and the digest
/// ([`content_digest`]) the store has once the write landed.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct PendingEdit {
    id: String,
    after: String,
}

impl EditLedger {
    /// The ledger at `path`; empty when there is none, or when it is not one
    /// this build can read (it is clew's own record, in the ignored cache:
    /// losing it costs only the de-duplication of replays already recorded).
    /// A file at the name that is not a plain file is refused, like every
    /// other state path.
    fn load(path: &Path) -> std::io::Result<EditLedger> {
        match read_checked(path) {
            Ok(None) => Ok(EditLedger::default()),
            Ok(Some(text)) => Ok(serde_json::from_str(&text).unwrap_or_default()),
            Err(e) => Err(StoreError::Refused(e).into()),
        }
    }

    fn save(&self, path: &Path) -> std::io::Result<()> {
        let text = serde_json::to_string(self).map_err(std::io::Error::other)?;
        write_atomic(path, text.as_bytes())
    }

    /// Settle the pending edit against the store's digest now: landed, it is
    /// recorded as applied; not, it is dropped. Returns whether the ledger
    /// changed.
    fn settle(&mut self, now: &str) -> bool {
        let Some(pending) = self.pending.take() else {
            return false;
        };
        if pending.after == now {
            self.record(&pending.id);
        }
        true
    }

    /// Record `id` as applied, forgetting the oldest beyond the cap.
    fn record(&mut self, id: &str) {
        self.applied.push(id.to_string());
        let over = self.applied.len().saturating_sub(EDIT_LEDGER_CAP);
        self.applied.drain(..over);
    }
}

/// A store's content as an [`EditLedger`] compares it: the SHA-256 of its
/// bytes, or `absent` when there is no file.
fn content_digest(text: Option<&str>) -> String {
    use sha2::{Digest, Sha256};
    match text {
        None => "absent".into(),
        Some(text) => Sha256::digest(text.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
    }
}

/// Where the [`EditLedger`] of the store at `path` lives: in the project's
/// `.clew/cache/edits/`, named after the store's path inside `.clew/` (`/`
/// and `%` escaped) — the cache, because the record is this machine's and
/// `cache/` is kept out of the repository, unlike the stores themselves.
/// Only a store inside a `.clew/` directory has one.
fn edit_ledger_path(path: &Path) -> std::io::Result<std::path::PathBuf> {
    let not_a_store =
        || std::io::Error::other(format!("{} is not a project state file", path.display()));
    let clew = clew_dir_of(path).ok_or_else(not_a_store)?;
    let rel = path.strip_prefix(&clew).map_err(|_| not_a_store())?;
    let name: String = rel
        .to_str()
        .ok_or_else(not_a_store)?
        .replace('%', "%25")
        .replace('/', "%2F");
    if name.is_empty() {
        return Err(not_a_store());
    }
    Ok(clew.join("cache").join("edits").join(name))
}

// ------------------------------------------------------------------ locking

/// An exclusive advisory lock on one state file, held across a
/// read-modify-write and released when dropped.
///
/// Taken on a sibling `.lock` file rather than on the state file itself,
/// because [`write_atomic`] replaces that inode: a lock held on the old one
/// would guard a file that no longer exists at the name.
pub struct FileLock {
    /// `None` when the filesystem cannot lock at all (see [`lock`]).
    #[cfg(unix)]
    #[allow(dead_code)]
    file: Option<std::fs::File>,
}

impl FileLock {
    /// Whether an OS lock is actually held (false only on a filesystem
    /// without `flock` support, or on a non-unix host).
    pub fn is_held(&self) -> bool {
        #[cfg(unix)]
        {
            self.file.is_some()
        }
        #[cfg(not(unix))]
        {
            false
        }
    }
}

/// Take [`FileLock`] for `path`, blocking until it is free.
///
/// Every store that merges does load → change → `write_atomic`: the project
/// stores (`bookmarks`, `notes`, the walkthrough library), `trust.toml`,
/// `config.toml`, `connections.toml` and the derived caches. An in-process
/// `Mutex` serializes that across a clew process's windows, but nothing
/// spanned two clew PROCESSES — two launches of the app, or a release build
/// beside a dev one — so both could read the same list, each apply its own
/// change, and the later `rename` win. The file itself is never torn (the
/// write is atomic); one entry just disappears, unreported.
///
/// **One failure policy for every caller:**
///
/// - The lock is held → `Ok`, and [`FileLock::is_held`] is true.
/// - The FILESYSTEM cannot lock at all (`ENOLCK`, `EOPNOTSUPP`/`ENOTSUP` —
///   some network mounts, a non-unix host) → still `Ok`, unlocked: refusing
///   every save on such a mount would be worse than the race, which stays
///   exactly as wide as it always was there.
/// - Anything else → `Err`, and the caller must not write: the lock file
///   could not be created (then neither can the write's temp file beside it),
///   or something that is not a plain file squats on the lock's name — which
///   in a `.clew/` that ships with the repository is a planted link or FIFO,
///   i.e. a hostile or broken checkout that clew should not be writing into.
///
/// The lock file is created beside the state file, dot-prefixed like the
/// atomic write's temp files (`.bookmarks.json.lock`); inside a project that
/// is `.clew/`, where [`CLEW_GITIGNORE`] keeps it out of `git status`. Its
/// name is predictable to whoever wrote the repository, so the open refuses to
/// follow a symlink at it; otherwise the "new file" would be created wherever
/// a committed link pointed.
pub fn lock(path: &Path) -> std::io::Result<FileLock> {
    let name = path
        .file_name()
        .ok_or_else(|| std::io::Error::other("state path has no file name"))?
        .to_string_lossy();
    lock_named(path, &format!(".{name}.lock"))
}

/// [`lock`] on an explicitly named lock file beside `path`, same policy.
///
/// For the stores whose lock file is older than [`lock`]'s `.<name>.lock`
/// convention: `config.toml` and `trust.toml` have always been locked on
/// `config.toml.lock` / `trust.toml.lock`. A store must keep ONE lock name
/// across clew versions — an older clew still running (a second app
/// instance, a dev build) locks the old name, and a newer one locking a
/// different file would not exclude it at all.
pub fn lock_named(path: &Path, lock_name: &str) -> std::io::Result<FileLock> {
    // The same refusal every other state operation makes: with `.clew` (or
    // `.clew/cache`) shipped as a symlink, creating the lock file would land
    // outside the project.
    if !repo_dirs_are_real(path) {
        return Err(std::io::Error::other(
            "a state directory is a symlink — refusing to lock through it",
        ));
    }
    if lock_name.is_empty() || lock_name.contains(['/', '\\']) || lock_name == ".." {
        return Err(std::io::Error::other(format!(
            "{lock_name:?} is not a lock file name"
        )));
    }
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("state path has no parent"))?;
    std::fs::create_dir_all(dir)?;
    ensure_clew_gitignore(path);
    lock_file(&dir.join(lock_name))
}

#[cfg(unix)]
fn lock_file(lock_path: &Path) -> std::io::Result<FileLock> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;
    // The lock file's name is fully determined by the state file's, and it
    // lives in `.clew/`, which ships with the repository — so a clone can
    // carry a symlink already sitting at exactly this name. Without
    // `O_NOFOLLOW` this open follows it and CREATES a file wherever it points,
    // which is precisely the unconsented write outside the project that the
    // rest of this module exists to prevent. `O_NONBLOCK` keeps a planted FIFO
    // from wedging the open (nothing ever opens the other end), and the type
    // check runs on the OPEN handle, so it describes what was actually locked.
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(lock_path)
        .map_err(|e| {
            if e.raw_os_error() == Some(libc::ELOOP) {
                std::io::Error::other("the lock file's name is a symlink — refusing it")
            } else {
                e
            }
        })?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other(
            "the lock file's name is taken by something that is not a plain file",
        ));
    }
    // Blocking, exclusive; released when the handle closes. The critical
    // section is one writer's read-modify-write of one small store: a read
    // and an atomic replace — for a remote edit, also its ledger's (two reads
    // and up to three fsynced replaces, see `merge_file`).
    loop {
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
            return Ok(FileLock { file: Some(file) });
        }
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(code)
                if code == libc::ENOLCK || code == libc::EOPNOTSUPP || code == libc::ENOTSUP =>
            {
                return Ok(FileLock { file: None });
            }
            _ => return Err(err),
        }
    }
}

/// No advisory locking here: the cross-process race stays open on non-unix
/// hosts, exactly as it was. The in-process `Mutex` each store holds is still
/// what covers the common (two windows, one process) case.
#[cfg(not(unix))]
fn lock_file(_lock_path: &Path) -> std::io::Result<FileLock> {
    Ok(FileLock {})
}

// -------------------------------------------------------------- path checks

/// Whether `rel` — a root-relative path from a persisted state file — is safe
/// to join onto the project root: relative, and made only of normal
/// components (no `..`, no leading `/`). State files ship with the repo, so a
/// stored path is attacker data until proven boring.
pub fn safe_rel(rel: &str) -> bool {
    let p = Path::new(rel);
    !rel.is_empty()
        && !p.is_absolute()
        && p.components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
}

/// Whether an **absolute** path from a persisted state file really lies under
/// `root`. `starts_with` alone is lexical — `/root/../../etc/x` passes it —
/// so `..`/`.` components are rejected outright, and the resolved path is
/// re-checked: a repo-shipped `root/link -> /outside` makes `root/link/x`
/// pass every lexical test while reading someone else's file.
pub fn safe_abs_under(root: &Path, path: &Path) -> bool {
    use std::path::Component;
    let lexical = path.starts_with(root)
        && path.components().all(|c| {
            matches!(
                c,
                Component::Normal(_) | Component::RootDir | Component::Prefix(_)
            )
        });
    if !lexical {
        return false;
    }
    // Containment must survive symlink resolution. A path that cannot be
    // resolved (missing file, dangling link) passes — it cannot be read
    // either, and refusing it would break "file was deleted" flows.
    match (path.canonicalize(), root.canonicalize()) {
        (Ok(p), Ok(r)) => p.starts_with(&r),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn dir(name: &str) -> TempDir {
        TempDir::new(name)
    }

    /// The merge of a store this build understands.
    fn merged(current: Option<&str>, op: &clew_protocol::StateMerge) -> Option<String> {
        merge_entries_checked(current, op).expect("a store this build understands")
    }

    #[test]
    fn read_refuses_symlinks_and_oversize() {
        let d = dir("clew-statefile-read");
        std::fs::write(d.join("ok.json"), "{}").unwrap();
        assert_eq!(read(&d.join("ok.json")).as_deref(), Some("{}"));
        assert!(read(&d.join("missing.json")).is_none());
        // A symlink — even to a readable file — is refused: state files are
        // plain files, and a repo-shipped link is how the DoS/leak starts.
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(d.join("ok.json"), d.join("link.json")).unwrap();
            assert!(read(&d.join("link.json")).is_none());
            std::os::unix::fs::symlink("/dev/zero", d.join("zero.json")).unwrap();
            assert!(read(&d.join("zero.json")).is_none());
        }
        // Over the cap: refused without reading.
        std::fs::write(d.join("big.json"), "x").unwrap();
        assert!(read_capped(&d.join("big.json"), 0).is_none());
    }

    /// The distinction every writer depends on: a file that is not there may
    /// be created, a file that is there but could not be read must be left
    /// alone. Each refusal names its reason.
    #[test]
    fn read_checked_tells_missing_from_refused() {
        let d = dir("clew-statefile-read-checked");
        assert!(matches!(read_checked(&d.join("missing.json")), Ok(None)));
        std::fs::write(d.join("ok.json"), "[]").unwrap();
        assert_eq!(
            read_checked(&d.join("ok.json")).unwrap().as_deref(),
            Some("[]")
        );

        std::fs::write(d.join("big.json"), "0123456789").unwrap();
        assert!(matches!(
            read_capped_checked(&d.join("big.json"), 4),
            Err(ReadError::TooLarge { cap: 4, size: 10 })
        ));

        std::fs::write(d.join("latin1.json"), b"[\"caf\xe9\"]").unwrap();
        assert!(matches!(
            read_checked(&d.join("latin1.json")),
            Err(ReadError::NotUtf8)
        ));

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(d.join("ok.json"), d.join("link.json")).unwrap();
            assert!(matches!(
                read_checked(&d.join("link.json")),
                Err(ReadError::NotPlainFile)
            ));
            let fifo = d.join("fifo.json");
            let c = std::ffi::CString::new(fifo.to_string_lossy().as_bytes()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0);
            assert!(matches!(read_checked(&fifo), Err(ReadError::NotPlainFile)));
            // A socket cannot be opened at all, which is no failure to read
            // a file either. (A scratch path too long to bind one at has
            // nothing to check.)
            let sock = d.join("sock.json");
            if let Ok(_listener) = std::os::unix::net::UnixListener::bind(&sock) {
                assert!(matches!(read_checked(&sock), Err(ReadError::NotPlainFile)));
            }

            // A `.clew` that is a symlink refuses even a file that would be
            // "missing" on the other side of it.
            let root = d.join("proj");
            std::fs::create_dir_all(&root).unwrap();
            std::os::unix::fs::symlink(&d, root.join(".clew")).unwrap();
            assert!(matches!(
                read_checked(&root.join(".clew").join("nothing-here.json")),
                Err(ReadError::UnsafeDirectory)
            ));
        }
    }

    /// The cap binds the READ, not just the size the handle reported. A file
    /// that is within the cap at `fstat` time and grows past it before the
    /// read completes must still be refused — otherwise the size check is
    /// only advisory, and an appending writer defeats it.
    #[test]
    fn read_cap_survives_a_file_that_grows_after_the_size_check() {
        let d = dir("clew-statefile-grow");
        let path = d.join("grow.json");
        std::fs::write(&path, "x".repeat(64)).unwrap();
        // Well within the cap: read normally.
        assert_eq!(read_capped(&path, 128).map(|s| s.len()), Some(64));
        // At exactly the cap: still fine (the +1 probe must not false-trip).
        assert_eq!(read_capped(&path, 64).map(|s| s.len()), Some(64));
        // One byte over: refused.
        assert!(read_capped(&path, 63).is_none());
    }

    #[test]
    fn write_atomic_defeats_planted_links() {
        let d = dir("clew-statefile-write");
        let target = d.join("state.json");
        // A symlink planted AT the destination is replaced, not followed:
        // the outside file must remain untouched.
        let outside = d.join("outside.txt");
        std::fs::write(&outside, "precious").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, &target).unwrap();
        write_atomic(&target, b"{\"v\":1}").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "{\"v\":1}");
        assert_eq!(
            std::fs::read_to_string(&outside).unwrap(),
            "precious",
            "a planted destination symlink must not be written through"
        );
        #[cfg(unix)]
        assert!(
            std::fs::symlink_metadata(&target).unwrap().is_file(),
            "the destination is a plain file now"
        );
        // Overwrite works (temp names don't collide with the previous run),
        // and no temp file is left beside the store.
        write_atomic(&target, b"{\"v\":2}").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "{\"v\":2}");
        let leftovers: Vec<_> = std::fs::read_dir(&d)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");
    }

    /// The secret variant creates its file user-only from the first byte.
    #[test]
    #[cfg(unix)]
    fn write_atomic_secret_is_user_only() {
        use std::os::unix::fs::PermissionsExt;
        let d = dir("clew-statefile-secret");
        let path = d.join("config.toml");
        write_atomic_secret(&path, b"api_key = \"sk\"\n").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    /// The first write into a project's `.clew/` leaves an ignore file behind
    /// that keeps per-reader state out of `git status` — and never replaces
    /// one the user or the repository already has.
    #[test]
    fn the_first_clew_write_adds_a_gitignore_but_never_replaces_one() {
        let d = dir("clew-statefile-gitignore");
        let fresh = d.join("fresh");
        std::fs::create_dir_all(&fresh).unwrap();
        write_atomic(&fresh.join(".clew").join("history.json"), b"{}").unwrap();
        let ignore = std::fs::read_to_string(fresh.join(".clew/.gitignore")).unwrap();
        assert_eq!(ignore, CLEW_GITIGNORE);
        for pattern in ["history.json", "reading.toml", "*.lock", "cache/"] {
            assert!(
                ignore.lines().any(|l| l == pattern),
                "{pattern} missing from {ignore:?}"
            );
        }
        assert!(
            !ignore
                .lines()
                .any(|l| l == "bookmarks.json" || l == "notes.json"),
            "shareable stores stay trackable"
        );

        // A nested write (`.clew/cache/…`) puts it in `.clew`, not in `cache`.
        let nested = d.join("nested");
        write_atomic(&nested.join(".clew/cache/walkthroughs.json"), b"[]").unwrap();
        assert!(nested.join(".clew/.gitignore").is_file());
        assert!(!nested.join(".clew/cache/.gitignore").exists());

        // The repository's own rules win.
        let owned = d.join("owned");
        std::fs::create_dir_all(owned.join(".clew")).unwrap();
        std::fs::write(owned.join(".clew/.gitignore"), "# mine\n").unwrap();
        lock(&owned.join(".clew").join("bookmarks.json")).unwrap();
        write_atomic(&owned.join(".clew").join("bookmarks.json"), b"[]").unwrap();
        assert_eq!(
            std::fs::read_to_string(owned.join(".clew/.gitignore")).unwrap(),
            "# mine\n"
        );

        // Paths outside any `.clew` (the global data dir) get nothing.
        write_atomic(&d.join("data").join("trust.toml"), b"").unwrap();
        assert!(!d.join("data/.gitignore").exists());
    }

    #[test]
    fn safe_abs_under_rejects_lexical_tricks() {
        let root = Path::new("/proj");
        assert!(safe_abs_under(root, Path::new("/proj/src/a.rs")));
        assert!(!safe_abs_under(root, Path::new("/etc/passwd")));
        // Lexically "under" the root, actually outside it.
        assert!(!safe_abs_under(root, Path::new("/proj/../../etc/passwd")));
        assert!(!safe_abs_under(root, Path::new("/proj/./x/../../../etc")));
    }

    #[test]
    fn safe_rel_rejects_escapes() {
        assert!(safe_rel("src/lib.rs"));
        assert!(safe_rel("a/b/c.txt"));
        assert!(!safe_rel(""));
        assert!(!safe_rel("/etc/passwd"));
        assert!(!safe_rel("../outside.rs"));
        assert!(!safe_rel("a/../../outside.rs"));
        assert!(!safe_rel("./a.rs")); // CurDir is not a Normal component
    }

    /// `.clew` itself being a symlink must stop every state operation: read,
    /// write (including its temp file), and the empty-state delete would all
    /// land outside the project otherwise.
    #[test]
    #[cfg(unix)]
    fn a_symlinked_clew_dir_stops_reads_writes_and_deletes() {
        let d = dir("clew-statefile-linkdir");
        let root = d.join("proj");
        let outside = d.join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("notes.json"), "[1]").unwrap();
        std::os::unix::fs::symlink(&outside, root.join(".clew")).unwrap();

        let state = root.join(".clew").join("notes.json");
        assert!(
            read(&state).is_none(),
            "reading through a linked .clew must refuse"
        );
        assert!(
            write_atomic(&state, b"[2]").is_err(),
            "writing through a linked .clew must refuse"
        );
        assert!(
            remove(&state).is_err(),
            "deleting through a linked .clew must refuse LOUDLY"
        );
        assert_eq!(
            std::fs::read_to_string(outside.join("notes.json")).unwrap(),
            "[1]",
            "the outside file survives untouched"
        );
        assert!(
            !outside.join(".gitignore").exists(),
            "nothing is created through the link either"
        );

        // A nested link (`.clew/cache -> outside`) under a real .clew is
        // refused the same way.
        let root2 = d.join("proj2");
        std::fs::create_dir_all(root2.join(".clew")).unwrap();
        std::os::unix::fs::symlink(&outside, root2.join(".clew").join("cache")).unwrap();
        let nested = root2.join(".clew").join("cache").join("stats.json");
        assert!(write_atomic(&nested, b"{}").is_err());
        assert!(read(&nested).is_none());

        // A real .clew (even one that does not exist yet) keeps working.
        let root3 = d.join("proj3");
        std::fs::create_dir_all(&root3).unwrap();
        let fresh = root3.join(".clew").join("notes.json");
        write_atomic(&fresh, b"[3]").unwrap();
        assert_eq!(read(&fresh).as_deref(), Some("[3]"));
        remove(&fresh).unwrap();
        assert!(read(&fresh).is_none());
        // Deleting a file that is already gone is not an error.
        remove(&fresh).unwrap();
    }

    /// The lock has to be an OS-level one, not a process-local mutex: a second
    /// clew process is exactly the writer the in-process locks cannot see.
    ///
    /// `flock` is held per OPEN FILE DESCRIPTION, so two independent
    /// acquisitions contend the same way whether they come from two threads or
    /// two processes — which is what makes this testable in-process at all.
    /// Without the lock the second acquisition returns immediately and the
    /// read-modify-writes interleave.
    #[test]
    #[cfg(unix)]
    fn a_held_lock_blocks_the_next_acquisition() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let d = dir("clew-statefile-lock");
        let path = d.join(".clew").join("bookmarks.json");

        let held = lock(&path).expect("the lock is available");
        assert!(held.is_held());
        let entered = AtomicBool::new(false);
        std::thread::scope(|s| {
            let waiter = s.spawn(|| {
                let _second = lock(&path).expect("acquired once we let go");
                entered.store(true, Ordering::SeqCst);
            });
            std::thread::sleep(std::time::Duration::from_millis(80));
            assert!(
                !entered.load(Ordering::SeqCst),
                "a second holder must wait while the first is inside its \
                 read-modify-write"
            );
            drop(held);
            waiter.join().unwrap();
        });
        assert!(entered.load(Ordering::SeqCst));
        // The lock file lives beside the store, dot-prefixed like the atomic
        // write's temp files, and never replaces the store itself.
        assert!(d.join(".clew").join(".bookmarks.json.lock").is_file());
        assert!(!path.exists());
    }

    /// A named lock is the same lock under another file name: it contends
    /// like any other, and its name cannot leave the directory.
    #[test]
    #[cfg(unix)]
    fn a_named_lock_contends_and_stays_beside_its_file() {
        let d = dir("statefile-named-lock");
        let path = d.join("config.toml");
        let held = lock_named(&path, "config.toml.lock").unwrap();
        assert!(held.is_held());
        assert!(d.join("config.toml.lock").is_file());
        // A second holder of the same name has to wait: it is the same lock.
        let (tx, rx) = std::sync::mpsc::channel();
        let p = path.clone();
        let waiter = std::thread::spawn(move || {
            let _second = lock_named(&p, "config.toml.lock").unwrap();
            let _ = tx.send(());
        });
        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(80))
                .is_err()
        );
        drop(held);
        rx.recv_timeout(std::time::Duration::from_secs(10))
            .expect("acquired once released");
        waiter.join().unwrap();
        for bad in ["", "..", "../x.lock", "a/b.lock"] {
            assert!(lock_named(&path, bad).is_err(), "{bad:?}");
        }
    }

    /// A symlinked `.clew` stops the lock too: creating the lock file through
    /// it would put clew's file in someone else's directory.
    #[test]
    #[cfg(unix)]
    fn a_symlinked_clew_dir_stops_the_lock() {
        let d = dir("clew-statefile-lock-link");
        let root = d.join("proj");
        let outside = d.join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join(".clew")).unwrap();
        assert!(lock(&root.join(".clew").join("notes.json")).is_err());
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
    }

    /// The lock file is the third leaf a state write touches, and the only one
    /// whose name a repository can plant a link at without also supplying the
    /// file's contents: `.clew/` ships with the repo and `.bookmarks.json.lock`
    /// is fully determined by `bookmarks.json`. Following that link would
    /// create a file at the attacker's target — an unconsented write outside
    /// the project, from nothing but a clone and one bookmark keypress. Under
    /// the one lock policy that is an ERROR, not a silent unlocked write.
    #[test]
    #[cfg(unix)]
    fn a_symlinked_lock_file_is_refused_and_creates_nothing_outside() {
        let d = dir("clew-statefile-lock-leaf");
        let root = d.join("proj");
        let clew = root.join(".clew");
        std::fs::create_dir_all(&clew).unwrap();
        let outside = d.join("outside");
        std::fs::create_dir_all(&outside).unwrap();

        // Dangling on purpose: the damage is the CREATE, not an overwrite.
        let target = outside.join("planted");
        std::os::unix::fs::symlink(&target, clew.join(".bookmarks.json.lock")).unwrap();

        assert!(
            lock(&clew.join("bookmarks.json")).is_err(),
            "a symlink at the lock file's name must refuse the lock, not be \
             followed"
        );
        assert!(
            !target.exists(),
            "nothing may be created at the link's target"
        );
        assert_eq!(
            std::fs::read_dir(&outside).unwrap().count(),
            0,
            "the outside directory stays empty"
        );

        // A FIFO squatting the same name is refused too, and must not block
        // the open — the lock is taken on the iced update thread.
        let fifo_root = d.join("proj-fifo");
        let fifo_clew = fifo_root.join(".clew");
        std::fs::create_dir_all(&fifo_clew).unwrap();
        let fifo = fifo_clew.join(".notes.json.lock");
        let c = std::ffi::CString::new(fifo.to_string_lossy().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0);
        assert!(
            lock(&fifo_clew.join("notes.json")).is_err(),
            "a FIFO at the lock file's name must be refused, not locked"
        );

        // The ordinary case still works: a real lock file is created and held.
        let ok_root = d.join("proj-ok");
        std::fs::create_dir_all(ok_root.join(".clew")).unwrap();
        assert!(lock(&ok_root.join(".clew").join("bookmarks.json")).is_ok());
    }

    fn merge(edit: clew_protocol::StateEdit, line: i64) -> clew_protocol::StateMerge {
        clew_protocol::StateMerge {
            key_fields: vec!["rel".into(), "line".into()],
            key: vec!["a.rs".into(), line.into()],
            edit,
            delete_when_empty: true,
        }
    }

    /// The merge addresses ONE entry, so everything else in the file — every
    /// bookmark another client added since this one loaded it — survives.
    #[test]
    fn merge_touches_only_the_entry_it_addresses() {
        use clew_protocol::StateEdit;
        let theirs = r#"[{"rel":"z.rs","line":9,"preview":"theirs"}]"#;

        // Toggle resolves against the FILE: absent here, so it is an add.
        let added = merged(
            Some(theirs),
            &merge(
                StateEdit::Toggle(serde_json::json!({"rel":"a.rs","line":1,"preview":"mine"})),
                1,
            ),
        )
        .expect("not empty");
        let list: Vec<serde_json::Value> = serde_json::from_str(&added).unwrap();
        assert_eq!(list.len(), 2, "the other client's bookmark survives");
        assert_eq!(list[1]["preview"], "mine");

        // Present now, so the same toggle removes it — and only it.
        let removed = merged(
            Some(&added),
            &merge(
                StateEdit::Toggle(serde_json::json!({"rel":"a.rs","line":1,"preview":"mine"})),
                1,
            ),
        )
        .expect("not empty");
        let list: Vec<serde_json::Value> = serde_json::from_str(&removed).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["rel"], "z.rs");

        // Remove by identity is a no-op for an entry that is already gone.
        let same = merged(Some(&removed), &merge(StateEdit::Remove, 1)).expect("not empty");
        assert_eq!(
            serde_json::from_str::<Vec<serde_json::Value>>(&same)
                .unwrap()
                .len(),
            1
        );
    }

    /// Patch merges fields into the entry ON DISK, so a field another client
    /// wrote is not overwritten by a stale copy of the whole entry; and an
    /// entry left carrying nothing is dropped, the rule the store names.
    #[test]
    fn patch_keeps_the_fields_it_was_not_given() {
        use clew_protocol::StateEdit;
        let on_disk = r#"[{"rel":"a.rs","symbol":"f","understood":true,"text":"theirs"}]"#;
        let note_merge = |edit| clew_protocol::StateMerge {
            key_fields: vec!["rel".into(), "symbol".into()],
            key: vec!["a.rs".into(), "f".into()],
            edit,
            delete_when_empty: true,
        };
        let empty_when = vec!["understood".to_string(), "text".to_string()];

        let mut fields = serde_json::Map::new();
        fields.insert("understood".into(), false.into());
        let patched = merged(
            Some(on_disk),
            &note_merge(StateEdit::Patch {
                fields,
                insert: None,
                empty_when: empty_when.clone(),
            }),
        )
        .expect("the note still has text");
        let list: Vec<serde_json::Value> = serde_json::from_str(&patched).unwrap();
        assert_eq!(
            list[0]["text"], "theirs",
            "prose another client wrote must survive a flag change"
        );
        assert_eq!(list[0]["understood"], false);

        // Clearing the last field that carried information drops the entry,
        // and an emptied store asks to be deleted.
        let mut fields = serde_json::Map::new();
        fields.insert("text".into(), "  ".into());
        assert!(
            merged(
                Some(&patched),
                &note_merge(StateEdit::Patch {
                    fields,
                    insert: None,
                    empty_when,
                }),
            )
            .is_none()
        );
    }

    /// A patch with no seed must not resurrect an entry another client
    /// deleted: attaching a note to a bookmark that is gone is a no-op.
    #[test]
    fn patch_without_a_seed_does_not_recreate_a_deleted_entry() {
        let mut fields = serde_json::Map::new();
        fields.insert("note".into(), "typed prose".into());
        let out = merged(
            Some("[]"),
            &merge(
                clew_protocol::StateEdit::Patch {
                    fields,
                    insert: None,
                    empty_when: Vec::new(),
                },
                1,
            ),
        );
        assert!(out.is_none(), "nothing to patch, nothing written");
    }

    /// `supersedes` and `commutes` hold whatever the store holds, as the
    /// merge applies the edits: checked on every pair of a set of edits of
    /// one entry — every variant, each patch shape, patches of neither
    /// shape, and edits that leave an entry at another key — against the
    /// entry absent, and with each of its fields missing, null, blank or
    /// set, a field no edit knows of among them, alone in the store or
    /// between two other entries. What is compared is the whole store, so an
    /// entry left at another key, or moved to the end of the file, is seen.
    /// The app leaves an edit out, or lets one land before another, only on
    /// their word, so a word they give wrongly is an edit landed out of
    /// turn.
    #[test]
    fn edits_that_supersede_or_commute_do_so_whatever_the_store_holds() {
        use clew_protocol::{StateEdit, StateMerge};
        use serde_json::{Value, json};
        let at = |edit| StateMerge {
            key_fields: vec!["k".into()],
            key: vec![json!(1)],
            edit,
            delete_when_empty: true,
        };
        let fields = |pairs: &[(&str, Value)]| -> serde_json::Map<String, Value> {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect()
        };
        let information = vec!["x".to_string(), "y".to_string()];
        // A bookmark's note: never creates the entry, never drops it.
        let keeps = |set: &[(&str, Value)]| {
            at(StateEdit::Patch {
                fields: fields(set),
                insert: None,
                empty_when: Vec::new(),
            })
        };
        // A reading note's field: seeds the entry, and drops it once `x`
        // and `y` are both blank.
        let note = |set: &[(&str, Value)], seed: Value| {
            at(StateEdit::Patch {
                fields: fields(set),
                insert: Some(seed),
                empty_when: information.clone(),
            })
        };
        let text_a = note(&[("x", json!("a"))], json!({"k": 1, "x": "a", "y": ""}));
        let text_b = note(
            &[("x", json!("b"))],
            json!({"k": 1, "x": "b", "y": null, "z": "s"}),
        );
        let flag_on = note(&[("y", json!(true))], json!({"k": 1, "x": "", "y": true}));
        let upsert = at(StateEdit::Upsert(json!({"k": 1, "x": "a"})));
        let remove = at(StateEdit::Remove);
        let toggle = at(StateEdit::Toggle(json!({"k": 1, "x": "a"})));
        let note_a = keeps(&[("x", json!("a"))]);
        let moves_it = keeps(&[("k", json!(2)), ("x", json!("a"))]);
        let edits = vec![
            upsert.clone(),
            at(StateEdit::Upsert(json!({"k": 1, "x": "b", "z": "u"}))),
            remove.clone(),
            toggle.clone(),
            at(StateEdit::Toggle(json!({"k": 1, "y": "b"}))),
            note_a.clone(),
            keeps(&[("x", json!("b"))]),
            keeps(&[("x", json!(null)), ("y", json!("a"))]),
            keeps(&[("y", json!("b"))]),
            keeps(&[("z", json!("c"))]),
            text_a.clone(),
            text_b.clone(),
            note(&[("x", json!(""))], json!({"k": 1, "x": "", "y": false})),
            flag_on.clone(),
            note(&[("y", json!(false))], json!({"k": 1, "y": false})),
            note(
                &[("x", json!("a")), ("y", json!("a"))],
                json!({"k": 1, "x": "a", "y": "a"}),
            ),
            // Neither shape: a seed carrying information of its own, a seed
            // never dropped, a drop with no seed, other fields carrying
            // information.
            note(&[("x", json!("a"))], json!({"k": 1, "x": "a", "y": "b"})),
            at(StateEdit::Patch {
                fields: fields(&[("x", json!("a"))]),
                insert: Some(json!({"k": 1, "x": "a"})),
                empty_when: Vec::new(),
            }),
            at(StateEdit::Patch {
                fields: fields(&[("x", json!(""))]),
                insert: None,
                empty_when: information.clone(),
            }),
            at(StateEdit::Patch {
                fields: fields(&[("y", json!("b"))]),
                insert: Some(json!({"k": 1, "y": "b"})),
                empty_when: vec!["y".into()],
            }),
            at(StateEdit::Patch {
                fields: fields(&[("x", json!(""))]),
                insert: Some(json!({"k": 1, "x": ""})),
                empty_when: vec!["x".into()],
            }),
            // Edits that leave an entry at another key: patches that move
            // the entry, a seed without the key, and an upsert and a toggle
            // of entries that do not hold it.
            moves_it.clone(),
            keeps(&[("k", json!(3))]),
            note(&[("x", json!("a"))], json!({"x": "a", "y": ""})),
            at(StateEdit::Upsert(json!({"x": "a"}))),
            at(StateEdit::Toggle(json!({"k": 5, "x": "a"}))),
        ];
        // The entry absent, or with each of x, y and z missing, null, blank
        // or set.
        let values = [
            None,
            Some(json!(null)),
            Some(json!("")),
            Some(json!("a")),
            Some(json!("b")),
        ];
        let mut entries: Vec<Option<Value>> = vec![None];
        for x in &values {
            for y in &values {
                for z in &values {
                    let mut entry = serde_json::Map::new();
                    entry.insert("k".into(), json!(1));
                    for (name, value) in [("x", x), ("y", y), ("z", z)] {
                        if let Some(value) = value {
                            entry.insert(name.into(), value.clone());
                        }
                    }
                    entries.push(Some(Value::Object(entry)));
                }
            }
        }
        // Each alone, and between two other entries.
        let mut stores: Vec<Vec<Value>> = Vec::new();
        for entry in &entries {
            stores.push(entry.iter().cloned().collect());
            let mut between = vec![json!({"k": 0, "x": "p"})];
            between.extend(entry.iter().cloned());
            between.push(json!({"k": 2, "x": "q"}));
            stores.push(between);
        }
        // The store `op` leaves, applied to `store`.
        let apply = |store: &[Value], op: &StateMerge| -> Vec<Value> {
            let text = serde_json::to_string(store).unwrap();
            match merge_entries_checked(Some(&text), op).unwrap() {
                Some(merged) => serde_json::from_str(&merged).unwrap(),
                None => Vec::new(),
            }
        };
        // Whether two outcomes are the same, as the predicates promise: to
        // the byte — or, between two patches that create and drop their
        // entry, on every other entry, and on the fields of that one that
        // carry information for either, an entry whose every such field is
        // blank being no entry at all, wherever in the file it stands.
        let same = |a: &StateMerge, b: &StateMerge, one: &[Value], other: &[Value]| {
            let kept_by = |m: &StateMerge| match patch_shape(m) {
                Some(PatchShape::Fields { information, .. }) => Some(information.to_vec()),
                _ => None,
            };
            let (Some(mut information), Some(also)) = (kept_by(a), kept_by(b)) else {
                return one == other;
            };
            for field in also {
                if !information.contains(&field) {
                    information.push(field);
                }
            }
            let rest = |store: &[Value]| -> Vec<Value> {
                store.iter().filter(|e| !a.matches(e)).cloned().collect()
            };
            let seen = |store: &[Value]| {
                let entry = store.iter().find(|e| a.matches(e))?;
                let values: Vec<Option<Value>> = information
                    .iter()
                    .map(|k| (!is_blank(entry, k)).then(|| entry[k].clone()))
                    .collect();
                values.iter().any(Option::is_some).then_some(values)
            };
            rest(one) == rest(other) && seen(one) == seen(other)
        };
        for a in &edits {
            for b in &edits {
                assert_eq!(commutes(a, b), commutes(b, a), "{a:?} / {b:?}");
                let (superseding, commuting) = (supersedes(b, a), commutes(a, b));
                if !superseding && !commuting {
                    continue;
                }
                for store in &stores {
                    let both = apply(&apply(store, a), b);
                    if superseding {
                        let alone = apply(store, b);
                        assert!(
                            same(a, b, &both, &alone),
                            "{b:?} after {a:?} on {store:?}: {both:?}, alone {alone:?}"
                        );
                    }
                    if commuting {
                        let reversed = apply(&apply(store, b), a);
                        assert!(
                            same(a, b, &both, &reversed),
                            "{a:?} then {b:?} on {store:?}: {both:?}, reversed {reversed:?}"
                        );
                    }
                }
            }
        }
        // And they say so where the app needs them to. A note's text
        // edited twice: the second is all that counts.
        assert!(supersedes(&text_b, &text_a));
        assert!(!supersedes(&text_a, &flag_on) && !supersedes(&flag_on, &text_a));
        // Its text and its flag: either order.
        assert!(commutes(&text_a, &flag_on));
        // A bookmark toggled twice: either order.
        assert!(commutes(&toggle, &toggle));
        // Its note, then a toggle: the toggle alone counts — but a toggle,
        // then its note, is neither.
        assert!(supersedes(&toggle, &note_a));
        assert!(!supersedes(&note_a, &toggle) && !commutes(&toggle, &note_a));
        // A removal, then a note that seeds the entry again: neither.
        assert!(!supersedes(&text_a, &remove) && !commutes(&remove, &text_a));
        // A tour regenerated: the last upsert is all that counts. Removed
        // and regenerated, the removal decides where it stands: neither.
        assert!(supersedes(&upsert, &upsert));
        assert!(!supersedes(&upsert, &remove) && !supersedes(&upsert, &toggle));
        // A removal removes whatever stayed at its key, and nothing else.
        assert!(supersedes(&remove, &toggle) && supersedes(&remove, &text_a));
        assert!(!supersedes(&remove, &moves_it));
        // Edits of different entries: no word either way.
        let other = StateMerge {
            key: vec![json!(2)],
            ..at(StateEdit::Remove)
        };
        assert!(!supersedes(&other, &remove) && !commutes(&other, &remove));
    }

    /// The bug: an unparseable store (a typo, a git conflict marker) read as
    /// empty, so one toggle replaced it with a one-entry file. The merge
    /// refuses instead, and every writer leaves the bytes alone.
    #[test]
    fn an_unparseable_store_is_refused_not_replaced() {
        use clew_protocol::StateEdit;
        let conflicted = "<<<<<<< HEAD\n[{\"rel\":\"a.rs\",\"line\":1}]\n=======\n[]\n>>>>>>> x\n";
        let toggle = merge(
            StateEdit::Toggle(serde_json::json!({"rel":"a.rs","line":2})),
            2,
        );
        assert!(matches!(
            merge_entries_checked(Some(conflicted), &toggle),
            Err(StoreError::Unparseable(_))
        ));
        // Entries must be objects: a list of strings is not one of our stores.
        assert!(merge_entries_checked(Some(r#"["a.rs"]"#), &toggle).is_err());
        // Missing is empty, as always.
        assert!(merge_entries_checked(None, &toggle).unwrap().is_some());
    }

    /// A newer clew's layout is never rewritten by this one; the envelope
    /// this build knows is merged in place and kept in its own layout.
    #[test]
    fn a_newer_schema_is_left_alone_and_a_known_envelope_round_trips() {
        use clew_protocol::StateEdit;
        let toggle = merge(
            StateEdit::Toggle(serde_json::json!({"rel":"a.rs","line":1})),
            1,
        );
        let newer = r#"{"schema_version": 99, "entries": [], "something": "new"}"#;
        assert!(matches!(
            merge_entries_checked(Some(newer), &toggle),
            Err(StoreError::NewerSchema {
                found: 99,
                supported: ARRAY_STORE_SCHEMA
            })
        ));

        let known = r#"{"schema_version": 1, "entries": [{"rel":"z.rs","line":3}]}"#;
        let merged = merge_entries_checked(Some(known), &toggle)
            .unwrap()
            .unwrap();
        let back = parse_array_store(&merged).unwrap();
        assert_eq!(back.envelope, Some(1), "the envelope survives the merge");
        assert_eq!(back.entries.len(), 2);

        // A bare array stays a bare array — the layout every older clew reads.
        let bare = merge_entries_checked(Some("[]"), &toggle).unwrap().unwrap();
        assert!(bare.trim_start().starts_with('['), "{bare}");
    }

    /// The whole read-modify-write refuses before it writes, and leaves the
    /// file byte-for-byte as it was, whether the content was unreadable,
    /// unparseable or from the future.
    #[test]
    fn merge_file_leaves_a_store_it_cannot_understand_untouched() {
        use clew_protocol::StateEdit;
        let d = dir("clew-statefile-merge-file");
        let path = d.join(".clew").join("bookmarks.json");
        let toggle = merge(
            StateEdit::Toggle(serde_json::json!({"rel":"a.rs","line":1})),
            1,
        );

        // Missing: created.
        let created = merge_file(&path, &toggle, "e-1").unwrap().text;
        assert!(created.is_some());
        assert_eq!(read(&path), created);
        // Toggled away: the file is deleted (delete_when_empty).
        assert_eq!(merge_file(&path, &toggle, "e-2").unwrap().text, None);
        assert!(!path.exists());

        for (label, bytes) in [
            ("unparseable", b"{ not json".to_vec()),
            (
                "newer schema",
                br#"{"schema_version":7,"entries":[]}"#.to_vec(),
            ),
            ("not UTF-8", b"[\"\xff\"]".to_vec()),
        ] {
            std::fs::write(&path, &bytes).unwrap();
            let err = merge_file(&path, &toggle, "e-3").expect_err(label);
            assert!(
                err.to_string().contains("untouched"),
                "{label}: the reason says nothing was written: {err}"
            );
            assert_eq!(std::fs::read(&path).unwrap(), bytes, "{label}: bytes kept");
            assert!(is_refusal(&err), "{label}: the content refuses it: {err}");
        }
    }

    /// A store — or its edit ledger — that could not be read because the
    /// READ failed is not refused by its content: the error is not a
    /// refusal, so the server answers `Failed` and the client sends the edit
    /// again. Both reads mapped every failure to a refusal, and a remote edit
    /// that met a moment of `EMFILE` or `EIO` was dropped at its first try.
    /// (An unreadable file stands in for those here: the open fails the same
    /// way, with an I/O error.)
    #[cfg(unix)]
    #[test]
    fn a_store_whose_read_failed_is_not_a_refusal() {
        use clew_protocol::StateEdit;
        use std::os::unix::fs::PermissionsExt;
        let d = dir("clew-statefile-read-failed");
        let path = d.join(".clew").join("bookmarks.json");
        let toggle = merge(
            StateEdit::Toggle(serde_json::json!({"rel":"a.rs","line":1})),
            1,
        );
        let unreadable = |file: &Path, then: &dyn Fn()| {
            std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o000)).unwrap();
            then();
            std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o600)).unwrap();
        };
        merge_file(&path, &toggle, "r-1").unwrap();
        let before = std::fs::read(&path).unwrap();
        unreadable(&path, &|| {
            let err = merge_file(&path, &toggle, "r-2").expect_err("the store cannot be read");
            assert!(!is_refusal(&err), "a failed read of the store: {err}");
            assert!(err.to_string().contains("untouched"), "{err}");
        });
        assert_eq!(std::fs::read(&path).unwrap(), before, "nothing was written");
        let ledger = d.join(".clew/cache/edits/bookmarks.json");
        unreadable(&ledger, &|| {
            let err = merge_file(&path, &toggle, "r-3").expect_err("the ledger cannot be read");
            assert!(!is_refusal(&err), "a failed read of the ledger: {err}");
        });
        // Once the reads work again, the edit goes through.
        assert!(merge_file(&path, &toggle, "r-3").unwrap().applied);
    }

    /// fixR1 #13: each edit id is applied once. A replay — the same edit,
    /// sent again because the reply was lost with its transport — is
    /// answered with the store as it is, and a toggle is not undone by it.
    #[test]
    fn an_edit_id_is_applied_at_most_once() {
        use clew_protocol::StateEdit;
        let d = dir("clew-statefile-merge-once");
        let path = d.join(".clew").join("bookmarks.json");
        let toggle = merge(
            StateEdit::Toggle(serde_json::json!({"rel":"a.rs","line":1})),
            1,
        );
        let first = merge_file(&path, &toggle, "w-1").unwrap();
        assert!(first.applied && first.text.is_some());
        let replay = merge_file(&path, &toggle, "w-1").unwrap();
        assert_eq!(
            replay,
            Merged {
                text: first.text.clone(),
                applied: false
            }
        );
        assert_eq!(read(&path), first.text, "the replay changed the store");
        // A no-op edit is recorded too: removing what is not there, then the
        // entry appearing, then the removal replayed — it must not remove it.
        let remove_b = merge(StateEdit::Remove, 2);
        assert!(merge_file(&path, &remove_b, "w-2").unwrap().applied);
        let add_b = merge(
            StateEdit::Upsert(serde_json::json!({"rel":"a.rs","line":2})),
            2,
        );
        merge_file(&path, &add_b, "other-1").unwrap();
        assert!(!merge_file(&path, &remove_b, "w-2").unwrap().applied);
        assert!(read(&path).unwrap().contains("\"line\": 2"));
        // The record lives in the ignored cache and stays bounded.
        let ledger_path = d.join(".clew/cache/edits/bookmarks.json");
        let ledger: EditLedger =
            serde_json::from_str(&std::fs::read_to_string(&ledger_path).unwrap()).unwrap();
        assert_eq!(ledger.applied, ["w-1", "w-2", "other-1"]);
        assert_eq!(ledger.pending, None);
        let mut full = EditLedger::default();
        for i in 0..EDIT_LEDGER_CAP + 5 {
            full.record(&format!("id-{i}"));
        }
        assert_eq!(full.applied.len(), EDIT_LEDGER_CAP);
        assert_eq!(full.applied[0], "id-5", "the oldest go first");
        // Only a store inside `.clew/` has a ledger, and ids are checked.
        let stray = d.join("elsewhere.json");
        assert!(merge_file(&stray, &toggle, "x-1").is_err());
        assert!(merge_file(&path, &toggle, "bad id").is_err());
    }

    /// The ledger is written ahead of the store, and a crash between the two
    /// writes is settled by the next merge under the lock, from the store's
    /// digest: an edit whose write landed is not applied again by its
    /// replay; one whose write never happened is applied by it.
    #[test]
    fn a_crash_between_ledger_and_store_neither_repeats_nor_loses_an_edit() {
        use clew_protocol::StateEdit;
        let d = dir("clew-statefile-merge-crash");
        let path = d.join(".clew").join("bookmarks.json");
        let ledger_path = d.join(".clew/cache/edits/bookmarks.json");
        let toggle = merge(
            StateEdit::Toggle(serde_json::json!({"rel":"a.rs","line":1})),
            1,
        );
        let announce = |id: &str, after: Option<&str>| {
            let ledger = EditLedger {
                applied: Vec::new(),
                pending: Some(PendingEdit {
                    id: id.into(),
                    after: content_digest(after),
                }),
            };
            ledger.save(&ledger_path).unwrap();
        };

        // Crashed AFTER the store write: the store is what was announced.
        let landed = merge_entries_checked(None, &toggle).unwrap();
        write_atomic(&path, landed.as_deref().unwrap().as_bytes()).unwrap();
        announce("t-1", landed.as_deref());
        let replay = merge_file(&path, &toggle, "t-1").unwrap();
        assert!(!replay.applied, "an edit that landed was applied again");
        assert_eq!(read(&path), landed);

        // Crashed BEFORE the store write: the store is still what it was.
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_file(&ledger_path).unwrap();
        announce("t-2", landed.as_deref());
        let replay = merge_file(&path, &toggle, "t-2").unwrap();
        assert!(replay.applied, "an edit that never landed was lost");
        assert_eq!(read(&path), landed);
        let ledger: EditLedger =
            serde_json::from_str(&std::fs::read_to_string(&ledger_path).unwrap()).unwrap();
        assert_eq!(
            ledger.applied,
            ["t-2"],
            "only the edit that landed is recorded"
        );
    }

    /// The ledger on disk, as a test reads it.
    fn ledger_at(path: &Path) -> EditLedger {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    /// The write-ahead order, through `merge_file` itself: interrupted after
    /// its store write, the edit is on disk as pending with the digest the
    /// store now has — written BEFORE the store — so the replay is answered,
    /// not applied again (the toggle does not undo itself), and the replay
    /// records the settled edit.
    #[test]
    fn a_merge_crashed_after_its_store_write_is_not_applied_again() {
        use clew_protocol::StateEdit;
        let d = dir("clew-statefile-merge-ahead");
        let path = d.join(".clew").join("bookmarks.json");
        let ledger_path = d.join(".clew/cache/edits/bookmarks.json");
        let toggle = merge(
            StateEdit::Toggle(serde_json::json!({"rel":"a.rs","line":1})),
            1,
        );
        crate::testutil::merge_crashing_after_store_write(&path, &toggle, "w-1").unwrap_err();
        let landed = read(&path);
        assert!(landed.is_some(), "the store write happened");
        assert_eq!(
            ledger_at(&ledger_path).pending,
            Some(PendingEdit {
                id: "w-1".into(),
                after: content_digest(landed.as_deref()),
            }),
            "the edit was announced ahead of the store write"
        );

        let replay = merge_file(&path, &toggle, "w-1").unwrap();
        assert!(!replay.applied, "the replay applied the edit a second time");
        assert_eq!(read(&path), landed, "the bookmark is still there");
        let ledger = ledger_at(&ledger_path);
        assert_eq!(
            (ledger.pending, ledger.applied),
            (None, vec!["w-1".to_string()])
        );
    }

    /// A store write that fails after the edit was announced leaves the edit
    /// to its replay: the next merge finds the store unchanged, drops the
    /// announcement, and applies the edit.
    #[test]
    fn a_failed_store_write_leaves_the_edit_to_its_replay() {
        use clew_protocol::StateEdit;
        let d = dir("clew-statefile-merge-fail");
        let path = d.join(".clew").join("bookmarks.json");
        let toggle = merge(
            StateEdit::Toggle(serde_json::json!({"rel":"a.rs","line":1})),
            1,
        );
        merge_file_until(&path, &toggle, "w-2", Interrupt::StoreWriteFails).unwrap_err();
        assert_eq!(read(&path), None, "nothing was written");
        let replay = merge_file(&path, &toggle, "w-2").unwrap();
        assert!(replay.applied, "the edit was lost");
        assert_eq!(read(&path), merge_entries_checked(None, &toggle).unwrap());
    }

    /// A writer that replaces the store without `merge_file` — the app's own
    /// edit of a project open locally, the server's `WriteState` — settles a
    /// pending edit against the store before replacing it. Settled after,
    /// against the replacement, the edit read as not landed, and its replay
    /// toggled the bookmark back off.
    #[test]
    fn a_wholesale_write_settles_a_pending_edit_first() {
        use clew_protocol::StateEdit;
        let d = dir("clew-statefile-settle-local");
        let path = d.join(".clew").join("bookmarks.json");
        let toggle = merge(
            StateEdit::Toggle(serde_json::json!({"rel":"a.rs","line":1})),
            1,
        );
        crate::testutil::merge_crashing_after_store_write(&path, &toggle, "w-3").unwrap_err();
        // A local edit, the way the app's store makes one: lock, settle,
        // replace (here: one more bookmark).
        let both = r#"[{"rel":"a.rs","line":1},{"rel":"b.rs","line":2}]"#;
        {
            let _exclusive = lock(&path).unwrap();
            settle_pending_edit(&path).unwrap();
            write_atomic(&path, both.as_bytes()).unwrap();
        }
        let replay = merge_file(&path, &toggle, "w-3").unwrap();
        assert!(!replay.applied, "the replay undid the bookmark");
        assert_eq!(read(&path).as_deref(), Some(both));

        // A store no remote edit reached has no ledger: nothing is read or
        // written for it.
        let fresh = d.join("other").join(".clew").join("notes.json");
        settle_pending_edit(&fresh).unwrap();
        assert!(!d.join("other/.clew/cache").exists());
    }

    /// Lexical containment is not containment: a repo-shipped symlink inside
    /// the root reaches outside while every component looks normal.
    ///
    /// The project is spelled as clew spells one — resolved, as a project
    /// root is when it opens — whatever the temp dir's spelling: one with a
    /// `..` in it (`TMPDIR=/work/../tmp`) failed the lexical check for every
    /// path under it, the plain file included.
    #[test]
    #[cfg(unix)]
    fn safe_abs_under_rejects_symlink_escapes() {
        let scratch = dir("clew-statefile-abs-link");
        let d = scratch.canonicalize().unwrap();
        let root = d.join("proj");
        let outside = d.join("outside");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(root.join("src/a.rs"), "fn a() {}").unwrap();
        std::fs::write(outside.join("secret.txt"), "s3cr3t").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();

        assert!(safe_abs_under(&root, &root.join("src/a.rs")));
        assert!(
            !safe_abs_under(&root, &root.join("link").join("secret.txt")),
            "a symlink inside the root must not smuggle outside files in"
        );
        // A missing path stays (lexically) allowed: it cannot be read anyway.
        assert!(safe_abs_under(&root, &root.join("src/deleted.rs")));
    }
}
