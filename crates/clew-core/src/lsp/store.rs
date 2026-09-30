//! The global server store and consent-gated provisioning.
//!
//! Server binaries are shared across projects, keyed by `(name, version)`:
//! `<data-dir>/clew/servers/<name>/<version>/<binary>`. Downloads are verified
//! against a pinned SHA-256 before anything is unpacked, and every install —
//! download or toolchain build — is STAGED: it lands in a private directory
//! beside its destination, and only a complete, checked result is swapped
//! into place. A failed or interrupted install leaves the previous one
//! exactly as it was.

use std::ffi::OsStr;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use super::archive;
use super::config::EffectiveServer;
use super::registry::{self, Download, Install, Installer, Platform, Provision, Sha256};

/// Root of clew's global data directory (`CLEW_DATA_DIR` overrides it).
pub fn data_root() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CLEW_DATA_DIR") {
        return Some(PathBuf::from(dir));
    }
    default_data_root()
}

/// Where the data directory is when `CLEW_DATA_DIR` does not say: under the
/// user's home, by the platform's convention. Separate from [`data_root`] so
/// a test can ask about the shipped default without unsetting the variable
/// for the whole process — every test beside it would then read and write the
/// developer's real data directory.
pub fn default_data_root() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    Some(match std::env::consts::OS {
        "macos" => home?.join("Library/Application Support/clew"),
        "windows" => PathBuf::from(std::env::var_os("APPDATA")?).join("clew"),
        _ => match std::env::var_os("XDG_DATA_HOME") {
            Some(x) => PathBuf::from(x).join("clew"),
            None => home?.join(".local/share/clew"),
        },
    })
}

/// Where language servers are installed.
fn servers_root() -> Option<PathBuf> {
    Some(data_root()?.join("servers"))
}

/// Whether `s` is safe to use as one directory name under the store. The name
/// and version can come from the project's `lsp.toml`, so a path separator,
/// `..`, or an absolute path would let a repository steer the store's install
/// path.
fn safe_component(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s != "."
        && s != ".."
        && !s.starts_with('.') // staging and set-aside directories
        && !s.contains(['/', '\\', '\0'])
        // Reject anything that isn't exactly one normal path component
        // (catches Windows drive prefixes and other oddities too).
        && std::path::Path::new(s)
            .components()
            .eq(std::iter::once(std::path::Component::Normal(s.as_ref())))
}

fn server_dir(name: &str, version: &str) -> Option<PathBuf> {
    if !safe_component(name) || !safe_component(version) {
        return None;
    }
    Some(servers_root()?.join(name).join(version))
}

/// Whether `path` lies inside `root`. Used before any destructive step, so a
/// path built from untrusted input can never escape — even via a symlinked
/// parent.
pub(crate) fn inside(root: &Path, path: &Path) -> bool {
    // Canonicalize as far as the path exists (the leaf usually doesn't yet),
    // so a symlinked ancestor can't redirect the write out of the root.
    let mut existing = path;
    while !existing.exists() {
        match existing.parent() {
            Some(p) => existing = p,
            None => return false,
        }
    }
    let (Ok(real), Ok(real_root)) = (existing.canonicalize(), root.canonicalize()) else {
        // No root directory yet: accept only a path that is lexically under it.
        return path.starts_with(root);
    };
    real.starts_with(&real_root) && path.starts_with(root)
}

/// Whether `path` lies inside the server store.
fn inside_store(path: &Path) -> bool {
    servers_root().is_some_and(|store| inside(&store, path))
}

/// A server present in the global store.
#[derive(Debug, Clone)]
pub struct InstalledServer {
    pub name: String,
    pub version: String,
    pub bytes: u64,
}

/// List every installed server in the global store, with its disk usage.
pub fn installed_servers() -> Vec<InstalledServer> {
    let Some(servers) = servers_root() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let Ok(names) = std::fs::read_dir(&servers) else {
        return out;
    };
    // Dot-entries are the store's own scratch (an install's staging, or the
    // previous install set aside during a swap), never an installed server.
    let visible = |e: &std::fs::DirEntry| !e.file_name().to_string_lossy().starts_with('.');
    for name_entry in names.flatten().filter(visible) {
        let name = name_entry.file_name().to_string_lossy().into_owned();
        let Ok(versions) = std::fs::read_dir(name_entry.path()) else {
            continue;
        };
        for ver_entry in versions.flatten().filter(visible) {
            if !ver_entry.path().is_dir() {
                continue;
            }
            out.push(InstalledServer {
                name: name.clone(),
                version: ver_entry.file_name().to_string_lossy().into_owned(),
                bytes: dir_size(&ver_entry.path()),
            });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name).then(a.version.cmp(&b.version)));
    out
}

/// Remove an installed server from the store. Returns whether anything existed.
pub fn remove(name: &str, version: &str) -> Result<bool, String> {
    let Some(dir) = server_dir(name, version) else {
        return Ok(false);
    };
    if !dir.exists() {
        return Ok(false);
    }
    if !inside_store(&dir) {
        return Err("refusing to remove a path outside the server store".into());
    }
    std::fs::remove_dir_all(&dir).map_err(|e| e.to_string())?;
    // Prune the now-empty parent name directory.
    if let Some(parent) = dir.parent() {
        let _ = std::fs::remove_dir(parent);
    }
    Ok(true)
}

fn dir_size(path: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_size(&e.path()),
            _ => e.metadata().map(|m| m.len()).unwrap_or(0),
        })
        .sum()
}

/// Outcome of locating the binary for an effective server config.
#[derive(Debug, Clone)]
pub enum Located {
    /// Ready to launch as-is: a store-installed binary (its install was the
    /// consent), or a toolchain's own binary found on PATH.
    Ready(PathBuf),
    /// The `command` the project's own `.clew/lsp.toml` names, exactly as
    /// written there. That file ships with the repository, so this may run
    /// only once the user has approved its fingerprint, and only as the
    /// approved bytes (`trust::stage_lsp_command` / the server's
    /// `lsp_command_allowed`) — never by spawning this path. A variant of its
    /// own so that no caller can take it for a store binary by accident.
    RepoCommand(PathBuf),
    /// Not installed yet; needs a consent-gated download of a verified binary.
    NeedsDownload {
        download: Download,
        dest_dir: PathBuf,
    },
    /// Not installed yet; needs a consent-gated toolchain install.
    NeedsInstall { install: Install, dest_dir: PathBuf },
    /// No server available for this platform/config.
    Unsupported(String),
}

impl Located {
    /// A digest of exactly what installing this resolution would do: the
    /// verified download (URL, digest, archive, binary) or the toolchain
    /// command (tool, version, every argument, how the destination is
    /// passed, binary) — and where it lands, which names the server and its
    /// version. `None` for a resolution that installs nothing.
    ///
    /// For binding a consent to what it was given for. The prompt is shown
    /// from one resolution, and the install runs from another, made when the
    /// user clicks: a repository's `.clew/lsp.toml` edited in between (a
    /// different `server` or `version`) would otherwise have the click install
    /// something the prompt never showed. The prompt carries this digest; the
    /// installer takes it back and runs only a resolution that still matches
    /// ([`Self::check_install_consent`]).
    pub fn install_digest(&self) -> Option<Sha256> {
        let mut id = Identity::new("clew/lsp-install/1");
        match self {
            Located::NeedsDownload { download, dest_dir } => {
                id.field("download");
                id.field(download.url);
                id.field(&download.sha256.to_hex());
                id.field(match download.archive {
                    registry::Archive::Gzip => "gzip",
                    registry::Archive::Zip => "zip",
                    registry::Archive::TarXz => "tar.xz",
                    registry::Archive::TarGz => "tar.gz",
                });
                id.field(download.binary);
                id.path(dest_dir);
            }
            Located::NeedsInstall { install, dest_dir } => {
                id.field("toolchain");
                id.field(install.tool);
                id.field(&install.version);
                let args = install.args();
                id.field(&args.len().to_string());
                for arg in &args {
                    id.field(arg);
                }
                id.field(match install.kind {
                    Installer::Go { .. } => "GOBIN",
                    Installer::Npm { .. } => "--prefix",
                    Installer::Cargo { .. } => "--root",
                });
                id.field(install.binary);
                id.path(dest_dir);
            }
            Located::Ready(_) | Located::RepoCommand(_) | Located::Unsupported(_) => return None,
        }
        Some(id.finish())
    }

    /// Whether this resolution may go ahead on a consent given to the prompt
    /// whose [`Self::install_digest`] was `consented` (hex, either case).
    ///
    /// `Ok` for exactly that install, and for [`Located::Ready`] — installed
    /// meanwhile (another window, a retry), so nothing will be installed at
    /// all. Refused, saying why: an install that changed since the prompt, a
    /// malformed digest, and a resolution that cannot be installed
    /// ([`Located::RepoCommand`], [`Located::Unsupported`]).
    pub fn check_install_consent(&self, consented: &str) -> Result<(), String> {
        if matches!(self, Located::Ready(_)) {
            return Ok(());
        }
        let Some(current) = self.install_digest() else {
            return Err("this server is not installed by clew — nothing to install".into());
        };
        match Sha256::parse(consented) {
            Some(consented) if consented == current => Ok(()),
            Some(_) => Err(
                "the install changed after it was approved (was .clew/lsp.toml edited?) — \
                 refusing it; review the new prompt and approve that instead"
                    .into(),
            ),
            None => Err("the install approval is malformed — refusing".into()),
        }
    }
}

/// An unambiguous encoding of a sequence of fields, digested: each field is
/// length-prefixed, so no two different sequences share an encoding.
struct Identity(Vec<u8>);

impl Identity {
    fn new(domain: &str) -> Self {
        let mut id = Identity(Vec::new());
        id.field(domain);
        id
    }

    fn bytes(&mut self, bytes: &[u8]) {
        self.0
            .extend_from_slice(&(bytes.len() as u64).to_le_bytes());
        self.0.extend_from_slice(bytes);
    }

    fn field(&mut self, text: &str) {
        self.bytes(text.as_bytes());
    }

    fn path(&mut self, path: &Path) {
        self.bytes(path.as_os_str().as_encoded_bytes());
    }

    fn finish(self) -> Sha256 {
        Sha256::of(&self.0)
    }
}

/// Decide how to obtain the binary for `server` without touching the network.
pub fn locate(server: &EffectiveServer) -> Located {
    // Escape hatch: a custom command bypasses the store — and goes through
    // the approval gate instead.
    if let Some(cmd) = &server.command {
        return Located::RepoCommand(cmd.clone());
    }
    // The version names the install directory and goes into an install's
    // argv. `lsp.toml` is checked as it is parsed; this holds for an
    // `EffectiveServer` from anywhere.
    if !registry::is_plain_version(&server.version) {
        return Located::Unsupported(format!(
            "{}: version {:?} is not a plain version — refusing to provision it",
            server.server_name,
            server.version.chars().take(64).collect::<String>()
        ));
    }
    let Some(platform) = Platform::current() else {
        return Located::Unsupported("unsupported platform".into());
    };
    let Some(spec) = registry::by_name(&server.server_name) else {
        return Located::Unsupported(format!("no managed server '{}'", server.server_name));
    };
    // The EFFECTIVE version (the project's `lsp.toml` override, else the
    // registry pin) drives provisioning, so the consent prompt, the install
    // command and the install directory all name the same version. A managed
    // download exists only at the pinned version — its digest is compiled in
    // for that release — so an override there resolves to nothing rather than
    // fetching something unverified.
    let Some(provision) = spec.provision(&server.version, platform) else {
        return Located::Unsupported(if server.version == spec.version {
            format!("{} is not available for this platform", server.server_name)
        } else {
            format!(
                "{} {} is not available: clew ships a verified {} only at {}. \
                 Remove the `version` from .clew/lsp.toml, or set an explicit `command`.",
                server.server_name, server.version, server.server_name, spec.version
            )
        });
    };
    let (binary, provision) = match provision {
        // A toolchain-bundled server (e.g. `dart language-server`) is run
        // from the toolchain binary on PATH — nothing to download or install.
        // As found, not canonicalized: toolchain managers (mise, volta,
        // rustup) put a shim on PATH that is a link to ONE multiplexer, which
        // decides what to run from the name it was started under.
        Provision::Toolchain { binary } => {
            return match find_on_path_unresolved(binary) {
                Some(p) => Located::Ready(p),
                None => Located::Unsupported(format!(
                    "'{binary}' not found on PATH — install the toolchain (e.g. the Dart/Flutter SDK)"
                )),
            };
        }
        Provision::Download(d) => (d.binary, Provision::Download(d)),
        Provision::Install(i) => (i.binary, Provision::Install(i)),
    };
    let Some(dest_dir) = server_dir(&server.server_name, &server.version) else {
        return Located::Unsupported("no data directory".into());
    };
    let path = dest_dir.join(binary);
    if path.is_file() {
        return Located::Ready(path);
    }
    match provision {
        Provision::Download(download) => Located::NeedsDownload { download, dest_dir },
        Provision::Install(install) => Located::NeedsInstall { install, dest_dir },
        Provision::Toolchain { .. } => unreachable!("returned above"),
    }
}

/// The first directory on `PATH` containing an executable named `binary`,
/// canonicalized to the real absolute file. Relative `PATH` entries (`.`,
/// `tools`) are skipped outright: they resolve against the process's cwd
/// *now*, while the spawn later runs with the project root as cwd — a
/// relative hit would mean approving one file and executing whatever the
/// repo places at that name. Canonicalizing pins the approved inode the
/// same way, independent of any later cwd.
pub fn find_on_path(binary: &str) -> Option<PathBuf> {
    find_in(&std::env::var_os("PATH")?, binary, true)
}

/// Like [`find_on_path`] (relative entries skipped, executables only), but
/// the hit is returned as found — `<dir>/<binary>`, symlinks unresolved.
///
/// For interpreters, and for any tool that may be a toolchain shim. A
/// virtualenv's `bin/python` is a symlink to the base interpreter, and Python
/// finds its environment from the path it was run by: resolving the link
/// silently swaps the venv (and its site-packages) for the bare base install.
/// Likewise rustup's `cargo`, volta's `npm` and mise's shims are links to one
/// multiplexer that dispatches on the name it was run under: resolved, `cargo
/// install …` becomes `rustup install …`.
pub fn find_on_path_unresolved(binary: &str) -> Option<PathBuf> {
    find_in(&std::env::var_os("PATH")?, binary, false)
}

/// [`find_on_path_unresolved`] over an explicit search path.
pub(crate) fn find_in_unresolved(path: &OsStr, binary: &str) -> Option<PathBuf> {
    find_in(path, binary, false)
}

fn find_in(path: &OsStr, binary: &str, canonical: bool) -> Option<PathBuf> {
    // A name with a separator is not a PATH lookup at all.
    if binary.is_empty() || binary.contains(['/', '\\']) {
        return None;
    }
    std::env::split_paths(path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(binary))
        .find_map(|p| {
            if canonical {
                std::fs::canonicalize(&p)
                    .ok()
                    .filter(|c| is_executable_file(c))
            } else {
                is_executable_file(&p).then_some(p)
            }
        })
}

/// A regular file (after following links) that the OS would execute.
fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.is_file() && meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        meta.is_file()
    }
}

/// Longest a toolchain install may run (`cargo install` compiles from
/// source, which takes minutes, not hours).
const TOOLCHAIN_TIMEOUT: Duration = Duration::from_secs(45 * 60);

/// Run a toolchain installer, placing the server in `dest_dir`. Blocking;
/// stopped — with everything it started — once `cancel` is set (checked
/// every few milliseconds while it runs).
///
/// `version` must be the version `install` was prepared (and consented) for;
/// anything else is refused rather than silently installed. The command is
/// [`install_command`] — the same argument list the consent prompt showed.
pub fn toolchain_install_cancellable(
    install: &Install,
    version: &str,
    dest_dir: &Path,
    cancel: &AtomicBool,
) -> Result<PathBuf, String> {
    let tool = toolchain_tool(install.tool, std::env::var_os("PATH").as_deref())?;
    toolchain_install_with(install, version, dest_dir, &tool, cancel)
}

/// The toolchain binary an install runs, looked up on `path` (the inherited
/// `PATH`) by absolute entries only. A bare name would be resolved by the OS
/// against every entry — `.` or another relative one included — so launching
/// clew from a repository could run that repository's own `cargo`/`npm`/`go`
/// the moment the user consents to an install.
///
/// Returned as found, NOT canonicalized: see [`find_on_path_unresolved`] —
/// with rustup, the canonical `cargo` is `rustup`, which reads `install` as a
/// request to install a toolchain.
fn toolchain_tool(tool: &str, path: Option<&OsStr>) -> Result<PathBuf, String> {
    path.and_then(|path| find_in_unresolved(path, tool))
        .ok_or_else(|| {
            format!(
                "'{tool}' is required to install this server but was not found on PATH; \
                 install it, or set a custom `command` in .clew/lsp.toml"
            )
        })
}

fn toolchain_install_with(
    install: &Install,
    version: &str,
    dest_dir: &Path,
    tool: &Path,
    cancel: &AtomicBool,
) -> Result<PathBuf, String> {
    if version != install.version {
        return Err(format!(
            "the install was prepared for version {} but {version} was requested — refusing",
            install.version
        ));
    }
    // The path is derived from possibly project-supplied name/version.
    if !inside_store(dest_dir) {
        return Err("refusing to install outside the server store".into());
    }
    let parent = dest_dir
        .parent()
        .ok_or_else(|| "invalid destination".to_string())?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let staging = create_staging_dir(parent, dest_dir)?;
    let built = (|| {
        let output = run_bounded_cancellable(
            install_command(install, tool, &staging),
            TOOLCHAIN_TIMEOUT,
            cancel,
        )?;
        if !output.status.success() {
            let err = String::from_utf8_lossy(&output.stderr);
            return Err(format!(
                "{}: {}",
                install.describe,
                tail_lines(err.trim(), 12)
            ));
        }
        let binary = staging.join(install.binary);
        if !binary.is_file() {
            return Err(format!(
                "install completed but '{}' is missing",
                install.binary
            ));
        }
        make_executable(&binary)
    })();
    if let Err(e) = built {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e);
    }
    swap_into_place(&staging, dest_dir)?;
    Ok(dest_dir.join(install.binary))
}

/// The command a toolchain install runs: `tool`, then exactly
/// [`Install::args`] — what the consent prompt ([`Install::describe`])
/// shows — then only the destination, which is clew's own staging directory.
pub(crate) fn install_command(install: &Install, tool: &Path, dir: &Path) -> std::process::Command {
    let mut cmd = std::process::Command::new(tool);
    // Run from the staging directory, not from wherever clew was launched.
    // These tools read configuration from the working directory and its
    // ancestors (`.cargo/config.toml`, `.npmrc`), so inheriting a project's
    // cwd would let the project redirect the registry or inject build flags.
    cmd.current_dir(dir);
    cmd.args(install.args());
    match &install.kind {
        Installer::Go { .. } => {
            cmd.env("GOBIN", dir);
        }
        Installer::Npm { .. } => {
            cmd.arg("--prefix").arg(dir);
        }
        // `--root <dir>` installs the binary at <dir>/bin/<name>.
        Installer::Cargo { .. } => {
            cmd.arg("--root").arg(dir);
        }
    }
    cmd
}

/// The last `n` lines of `text`: where tools put the actual error.
fn tail_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// Most output kept from a bounded run, per stream (the tail is kept).
const OUTPUT_TAIL_BYTES: usize = 64 * 1024;

/// Run `cmd` to completion within `limit`: stdin closed, stdout/stderr
/// captured (their tails), and — past the deadline — killed together with
/// everything it started (it runs as its own process group). Blocking.
pub(crate) fn run_bounded(
    cmd: std::process::Command,
    limit: Duration,
) -> Result<std::process::Output, String> {
    run_bounded_cancellable(cmd, limit, &AtomicBool::new(false))
}

/// [`run_bounded`], also stopped (the same way) once `cancel` is set — and
/// never started when it already is.
pub(crate) fn run_bounded_cancellable(
    mut cmd: std::process::Command,
    limit: Duration,
    cancel: &AtomicBool,
) -> Result<std::process::Output, String> {
    use std::process::Stdio;
    let name = cmd.get_program().to_string_lossy().into_owned();
    if cancel.load(Ordering::Relaxed) {
        return Err(format!("{name} was cancelled before it started"));
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("could not run {name}: {e}"))?;
    let collect = |stream: Option<Box<dyn Read + Send>>| {
        let (tx, rx) = std::sync::mpsc::channel();
        if let Some(mut stream) = stream {
            std::thread::spawn(move || {
                let mut tail = Vec::new();
                let mut buf = [0u8; 8192];
                while let Ok(n) = stream.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    tail.extend_from_slice(&buf[..n]);
                    if tail.len() > 2 * OUTPUT_TAIL_BYTES {
                        tail.drain(..tail.len() - OUTPUT_TAIL_BYTES);
                    }
                }
                let _ = tx.send(tail);
            });
        }
        rx
    };
    let stdout = collect(
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
    );
    let stderr = collect(
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
    );
    let deadline = Instant::now() + limit;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                let cancelled = cancel.load(Ordering::Relaxed);
                if !cancelled && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(20));
                    continue;
                }
                #[cfg(unix)]
                crate::procgroup::kill(child.id());
                let _ = child.kill();
                let _ = child.wait();
                return Err(if cancelled {
                    format!("{name} was cancelled — stopped it")
                } else {
                    format!(
                        "{name} did not finish within {} s — stopped it",
                        limit.as_secs()
                    )
                });
            }
            Err(e) => return Err(format!("waiting for {name}: {e}")),
        }
    };
    // Bounded: a daemon the tool left behind may hold the pipe open forever.
    let take = |rx: std::sync::mpsc::Receiver<Vec<u8>>| {
        let mut tail = rx.recv_timeout(Duration::from_secs(2)).unwrap_or_default();
        if tail.len() > OUTPUT_TAIL_BYTES {
            tail.drain(..tail.len() - OUTPUT_TAIL_BYTES);
        }
        tail
    };
    Ok(std::process::Output {
        status,
        stdout: take(stdout),
        stderr: take(stderr),
    })
}

/// Byte cap on a server download. The largest server clew ships is well under
/// this; the cap exists so a redirected, hijacked, or simply wrong URL cannot
/// stream without bound into memory before the digest ever gets to reject it.
const MAX_DOWNLOAD_BYTES: u64 = 512 * 1024 * 1024;
/// An upper bound for the whole transfer, against a peer that trickles one
/// byte just inside every read timeout.
const DOWNLOAD_DEADLINE: Duration = Duration::from_secs(30 * 60);

/// Fetch, verify and install a server, abandoned (between chunks) once
/// `cancel` is set. Blocking; run off the UI thread. Returns the path to the
/// installed executable.
pub fn download_and_install_cancellable(
    download: &Download,
    dest_dir: &Path,
    cancel: &AtomicBool,
) -> Result<PathBuf, String> {
    let store = servers_root().ok_or_else(|| "no data directory".to_string())?;
    install_download(download, dest_dir, &store, cancel)
}

/// Fetch, verify and install `download` into `dest_dir`, which must lie in
/// `store`. Shared with debug-adapter provisioning, which keeps its own root.
pub(crate) fn install_download(
    download: &Download,
    dest_dir: &Path,
    store: &Path,
    cancel: &AtomicBool,
) -> Result<PathBuf, String> {
    // Refused before any network traffic, not just before the write.
    if !inside(store, dest_dir) {
        return Err("refusing to install outside the store".into());
    }
    let bytes = fetch(download.url, MAX_DOWNLOAD_BYTES, cancel)?;
    install_bytes(&bytes, download, dest_dir, store, archive::BUDGET)
}

/// Download the raw bytes of `url` over HTTPS, refusing a body over `cap`.
///
/// Through clew's one transport ([`crate::net`]): the trust and the proxy the
/// model calls use, HTTPS on every hop (a redirect cannot downgrade to plain
/// http — the digest would still refuse the bytes, but only after a network
/// attacker had chosen them), timeouts, the cap, a deadline, and `cancel`
/// between chunks. This used to be an agent of its own with the bundled roots
/// only and no proxy, so behind a proxy or a TLS-inspecting CA a language
/// server could not be installed while chat worked. Stricter than the shared
/// policy in one way: no loopback exception — a store URL is always HTTPS.
fn fetch(url: &str, cap: u64, cancel: &AtomicBool) -> Result<Vec<u8>, String> {
    if !url.starts_with("https://") {
        return Err("refusing non-HTTPS download URL".into());
    }
    crate::net::get_cancellable(
        url,
        &[],
        crate::net::Limits {
            max_bytes: cap,
            deadline: DOWNLOAD_DEADLINE,
        },
        cancel,
    )
    .map_err(|e| format!("download failed: {e}"))
}

/// Verify the checksum, unpack into a staging directory, check the result,
/// and swap it in as `dest_dir`.
///
/// gzip artifacts are a single executable written as `dest_dir/<binary>`;
/// archives are extracted whole (some servers, e.g. clangd, need sibling
/// resource files), with the executable at the relative path `<binary>`.
fn install_bytes(
    bytes: &[u8],
    download: &Download,
    dest_dir: &Path,
    store: &Path,
    budget: archive::Budget,
) -> Result<PathBuf, String> {
    // The destination is derived from a name/version that can come from the
    // project's lsp.toml — never touch a path outside the store.
    if !inside(store, dest_dir) {
        return Err("refusing to install outside the store".into());
    }
    // Verify before we ever unpack or execute anything.
    let actual = Sha256::of(bytes);
    if actual != download.sha256 {
        return Err(format!(
            "checksum mismatch: expected {}, got {actual}",
            download.sha256
        ));
    }
    let parent = dest_dir
        .parent()
        .ok_or_else(|| "invalid destination".to_string())?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let staging = create_staging_dir(parent, dest_dir)?;
    let unpacked = archive::unpack(download.archive, bytes, &staging, download.binary, budget)
        .and_then(|()| {
            let exe = staging.join(download.binary);
            if !exe.is_file() {
                return Err(format!("'{}' not found in archive", download.binary));
            }
            make_executable(&exe)
        });
    if let Err(e) = unpacked {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e);
    }
    swap_into_place(&staging, dest_dir)?;
    Ok(dest_dir.join(download.binary))
}

/// Make the complete `staging` directory the install at `dest`. A previous
/// install is moved aside first rather than deleted — if the second rename
/// fails it is put back — and removed only once the new one is in place.
fn swap_into_place(staging: &Path, dest: &Path) -> Result<(), String> {
    let fail = |e: std::io::Error| {
        let _ = std::fs::remove_dir_all(staging);
        format!("install failed: {e}")
    };
    let parent = dest
        .parent()
        .ok_or_else(|| "invalid destination".to_string())?;
    let previous = match std::fs::symlink_metadata(dest) {
        Ok(_) => {
            let aside = unique_sibling(parent, dest, "old").map_err(fail)?;
            std::fs::rename(dest, &aside).map_err(fail)?;
            Some(aside)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(fail(e)),
    };
    if let Err(e) = std::fs::rename(staging, dest) {
        if let Some(previous) = &previous {
            let _ = std::fs::rename(previous, dest);
        }
        return Err(fail(e));
    }
    if let Some(previous) = previous {
        let _ = std::fs::remove_dir_all(previous);
    }
    Ok(())
}

/// Names for scratch entries beside `dest`: `.<kind>-<dest>-<pid>-<n>`. The
/// pid keeps processes apart, the counter keeps one process's installs apart.
fn scratch_name(parent: &Path, dest: &Path, kind: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let base = dest.file_name().and_then(|n| n.to_str()).unwrap_or("srv");
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    parent.join(format!(".{kind}-{base}-{}-{n}", std::process::id()))
}

/// A sibling path of `dest` that does not exist yet.
fn unique_sibling(parent: &Path, dest: &Path, kind: &str) -> std::io::Result<PathBuf> {
    for _ in 0..64 {
        let candidate = scratch_name(parent, dest, kind);
        if std::fs::symlink_metadata(&candidate).is_err() {
            return Ok(candidate);
        }
    }
    Err(std::io::Error::other("no free scratch name"))
}

/// Scratch entries older than this are leftovers of an install that died
/// mid-way (a crash, a kill); they are removed when the next install starts.
const STALE_SCRATCH: Duration = Duration::from_secs(24 * 60 * 60);

/// A private staging directory beside `dest_dir`, created exclusively.
///
/// The name used to be `.tmp-<server>-<pid>`, which is not unique within one
/// process: two installs of the same server (two windows, or a retry racing
/// its predecessor) staged into the SAME directory and unpacked over each
/// other, so the surviving rename could publish a mixture of both. `create_dir`
/// fails on an existing entry, so the winner of each name is unambiguous and
/// the loser simply takes the next one — the same exclusive-create idiom the
/// state-file writer uses.
fn create_staging_dir(parent: &Path, dest_dir: &Path) -> Result<PathBuf, String> {
    sweep_stale_scratch(parent);
    for _ in 0..64 {
        let tmp = scratch_name(parent, dest_dir, "tmp");
        match std::fs::create_dir(&tmp) {
            Ok(()) => return Ok(tmp),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.to_string()),
        }
    }
    Err("could not create a staging directory for the install".into())
}

/// Best-effort removal of stale `.tmp-*` / `.old-*` entries in `parent`.
fn sweep_stale_scratch(parent: &Path) {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !(name.starts_with(".tmp-") || name.starts_with(".old-")) {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > STALE_SCRATCH);
        if stale {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| e.to_string())
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<(), String> {
    Ok(())
}

/// Test support shared by this module and `debugadapter`.
#[cfg(test)]
pub(crate) mod testing {
    use std::path::{Path, PathBuf};

    /// Point the data dir at a fresh temp directory (with a `servers/` store)
    /// for the duration of `f`, holding the env lock (CLEW_DATA_DIR is
    /// process-global), and restore whatever was set before — a panic in `f`
    /// included.
    pub(crate) fn with_store<T>(tag: &str, f: impl FnOnce(&Path) -> T) -> T {
        let data = crate::testutil::DataDir::new(&format!("store-{tag}"));
        std::fs::create_dir_all(data.join("servers")).unwrap();
        f(&data)
    }

    /// Write an executable shell script.
    #[cfg(unix)]
    pub(crate) fn script(path: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.to_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use super::testing::with_store;
    use super::*;
    use crate::lsp::registry::Archive;
    use std::io::Write;

    fn gzip(data: &[u8]) -> Vec<u8> {
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(data).unwrap();
        enc.finish().unwrap()
    }

    fn download(bytes: &[u8], archive: Archive, binary: &'static str) -> Download {
        Download {
            url: "https://example.invalid/artifact",
            sha256: Sha256::of(bytes),
            archive,
            binary,
        }
    }

    fn store(dir: &Path) -> PathBuf {
        dir.join("servers")
    }

    #[test]
    fn install_verifies_checksum_and_unpacks() {
        with_store("ok", |dir| {
            let payload = b"#!/bin/sh\necho fake-server\n";
            let gz = gzip(payload);
            let dl = download(&gz, Archive::Gzip, "rust-analyzer");
            let dest = dir.join("servers/rust-analyzer/2026-07-13");
            let installed = install_bytes(&gz, &dl, &dest, &store(dir), archive::BUDGET).unwrap();
            assert_eq!(installed, dest.join("rust-analyzer"));
            assert_eq!(std::fs::read(&installed).unwrap(), payload);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(&installed).unwrap().permissions().mode();
                assert_eq!(mode & 0o111, 0o111, "must be executable");
            }
            // Staging is gone; only the install remains.
            let leftovers: Vec<_> = std::fs::read_dir(dest.parent().unwrap())
                .unwrap()
                .flatten()
                .map(|e| e.file_name())
                .collect();
            assert_eq!(leftovers, vec![std::ffi::OsString::from("2026-07-13")]);
        });
    }

    #[test]
    fn install_refuses_a_destination_outside_the_store() {
        with_store("outside", |dir| {
            let gz = gzip(b"payload");
            let dl = download(&gz, Archive::Gzip, "rust-analyzer");
            // A path steered out of the store (e.g. by a `version` from the
            // project's lsp.toml) must be refused before anything is touched.
            let victim = dir.join("precious");
            std::fs::create_dir_all(&victim).unwrap();
            std::fs::write(victim.join("keep.txt"), b"keep").unwrap();
            let err = install_bytes(&gz, &dl, &victim, &store(dir), archive::BUDGET).unwrap_err();
            assert!(err.contains("outside the store"), "{err}");
            assert!(victim.join("keep.txt").is_file(), "must not be touched");
            // Refused before the network, too.
            let err =
                install_download(&dl, &victim, &store(dir), &AtomicBool::new(false)).unwrap_err();
            assert!(err.contains("outside the store"), "{err}");
        });
    }

    #[test]
    fn install_rejects_bad_checksum_and_keeps_the_previous_install() {
        with_store("bad", |dir| {
            let dest = dir.join("servers/s/v");
            std::fs::create_dir_all(&dest).unwrap();
            std::fs::write(dest.join("rust-analyzer"), b"old").unwrap();
            let gz = gzip(b"payload");
            let mut dl = download(&gz, Archive::Gzip, "rust-analyzer");
            dl.sha256 = Sha256::of(b"something else");
            let err = install_bytes(&gz, &dl, &dest, &store(dir), archive::BUDGET).unwrap_err();
            assert!(err.contains("checksum mismatch"), "{err}");
            assert_eq!(std::fs::read(dest.join("rust-analyzer")).unwrap(), b"old");

            // A verified archive that lacks the binary fails the same way:
            // after unpacking, before the swap.
            let gz = gzip(b"payload");
            let dl = download(&gz, Archive::Gzip, "rust-analyzer");
            let tar = {
                let mut out = Vec::new();
                let mut builder = tar::Builder::new(&mut out);
                let mut header = tar::Header::new_gnu();
                header.set_path("README").unwrap();
                header.set_size(2);
                header.set_cksum();
                builder.append(&header, &b"hi"[..]).unwrap();
                builder.finish().unwrap();
                drop(builder);
                out
            };
            let mut xz = Vec::new();
            lzma_rs::xz_compress(&mut std::io::Cursor::new(&tar), &mut xz).unwrap();
            let dl_missing = download(&xz, Archive::TarXz, "zls");
            let err =
                install_bytes(&xz, &dl_missing, &dest, &store(dir), archive::BUDGET).unwrap_err();
            assert!(err.contains("not found in archive"), "{err}");
            assert_eq!(std::fs::read(dest.join("rust-analyzer")).unwrap(), b"old");

            // And a good one replaces it whole.
            let installed = install_bytes(&gz, &dl, &dest, &store(dir), archive::BUDGET).unwrap();
            assert_eq!(std::fs::read(installed).unwrap(), b"payload");
            let parent: Vec<_> = std::fs::read_dir(dest.parent().unwrap())
                .unwrap()
                .flatten()
                .map(|e| e.file_name())
                .collect();
            assert_eq!(
                parent.len(),
                1,
                "no staging or set-aside dirs left: {parent:?}"
            );
        });
    }

    #[test]
    fn install_zip_extracts_tree_and_finds_nested_binary() {
        // Build a zip with a nested binary and a sibling resource file.
        let mut buf = Vec::new();
        {
            let mut zw = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let opts: zip::write::FileOptions<()> = zip::write::FileOptions::default();
            zw.start_file("clangd_1.0/bin/clangd", opts).unwrap();
            zw.write_all(b"#!/bin/sh\necho clangd\n").unwrap();
            zw.start_file("clangd_1.0/lib/clang/headers.h", opts)
                .unwrap();
            zw.write_all(b"// header").unwrap();
            zw.finish().unwrap();
        }
        let dl = download(&buf, Archive::Zip, "clangd_1.0/bin/clangd");
        with_store("zip", |dir| {
            let dest = dir.join("servers/clangd/1.0");
            let installed = install_bytes(&buf, &dl, &dest, &store(dir), archive::BUDGET).unwrap();
            assert_eq!(installed, dest.join("clangd_1.0/bin/clangd"));
            assert!(installed.is_file());
            // The sibling resource tree came along.
            assert!(dest.join("clangd_1.0/lib/clang/headers.h").is_file());
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(&installed).unwrap().permissions().mode();
                assert_eq!(mode & 0o111, 0o111);
            }
        });
    }

    #[test]
    fn install_tar_xz_extracts_binary() {
        let mut tar = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar);
            let payload = b"#!/bin/sh\necho zls\n";
            let mut header = tar::Header::new_gnu();
            header.set_path("zls").unwrap();
            header.set_size(payload.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder.append(&header, &payload[..]).unwrap();
            builder.finish().unwrap();
        }
        let mut xz = Vec::new();
        lzma_rs::xz_compress(&mut std::io::Cursor::new(&tar), &mut xz).unwrap();
        let dl = download(&xz, Archive::TarXz, "zls");
        with_store("tarxz", |dir| {
            let dest = dir.join("servers/zls/0.16.0");
            let installed = install_bytes(&xz, &dl, &dest, &store(dir), archive::BUDGET).unwrap();
            assert_eq!(installed, dest.join("zls"));
            assert!(installed.is_file());
        });
    }

    /// F12: a zip entry that tries to leave the destination fails the whole
    /// install. It used to be skipped, installing the rest as if nothing
    /// happened.
    #[test]
    fn a_traversing_zip_entry_fails_the_install() {
        let mut buf = Vec::new();
        {
            let mut zw = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let opts: zip::write::FileOptions<()> = zip::write::FileOptions::default();
            zw.start_file("bin/tool", opts).unwrap();
            zw.write_all(b"#!/bin/sh\n").unwrap();
            zw.start_file("../../escape", opts).unwrap();
            zw.write_all(b"x").unwrap();
            zw.finish().unwrap();
        }
        let dl = download(&buf, Archive::Zip, "bin/tool");
        with_store("zip-escape", |dir| {
            let dest = dir.join("servers/t/1");
            let err = install_bytes(&buf, &dl, &dest, &store(dir), archive::BUDGET).unwrap_err();
            assert!(err.contains("not a plain relative path"), "{err}");
            assert!(!dest.exists(), "nothing is installed");
            assert!(!dir.join("servers/escape").exists());
            assert!(!dir.join("escape").exists());
        });
    }

    /// F10: plain HTTP is refused before any connection is made, and so is a
    /// download already cancelled. (The cap, the deadline and cancellation
    /// between chunks are the shared transport's, tested there:
    /// `net::tests::a_copy_is_capped_cancellable_and_bounded_in_time`.)
    #[test]
    fn downloads_are_https_only_and_cancellable() {
        let never = AtomicBool::new(false);
        let cap = 256 * 1024;
        let err = fetch("http://127.0.0.1:9/artifact", cap, &never).unwrap_err();
        assert!(err.contains("non-HTTPS"), "{err}");
        // Cancelled before it started: no connection is even attempted.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("https://{}/artifact", listener.local_addr().unwrap());
        let err = fetch(&url, cap, &AtomicBool::new(true)).unwrap_err();
        assert!(err.contains("cancelled"), "{err}");
        assert!(
            matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock),
            "a cancelled download connected"
        );
    }

    /// F15: a repository's own command is its own variant, so nothing can
    /// mistake it for a store binary and spawn it without the approval gate.
    #[test]
    fn a_repo_command_is_located_as_such() {
        let server = EffectiveServer {
            language: "rust".into(),
            server_name: "rust-analyzer".into(),
            version: "2026-07-13".into(),
            args: Vec::new(),
            command: Some(PathBuf::from("tools/ra")),
            init_options: None,
        };
        assert!(matches!(
            locate(&server),
            Located::RepoCommand(p) if p == Path::new("tools/ra")
        ));
    }

    /// F2, at the store level: every argument the toolchain install spawns —
    /// all but the store destination — appears in the consent prompt, for
    /// every installer in the registry and at pinned and unpinned versions.
    /// The prompt and the argv used to be built separately; for a pinned
    /// typescript-language-server the spawn asked npm for `typescript@5@1.2.3`.
    #[test]
    fn the_install_argv_is_what_the_consent_prompt_describes() {
        let tool_dir = Path::new("/opt/toolchain/bin");
        let dest = Path::new("/store/servers/x/y");
        let mut checked = 0;
        for spec in registry::all() {
            for version in ["latest", "1.2.3", "v0.99.0"] {
                let Some(Provision::Install(install)) = spec.provision(version, Platform::MacArm64)
                else {
                    continue;
                };
                let tool = tool_dir.join(install.tool);
                let cmd = install_command(&install, &tool, dest);
                assert_eq!(cmd.get_program(), tool.as_os_str());
                assert_eq!(cmd.get_current_dir(), Some(dest));
                let described: Vec<&str> = install.describe.split_whitespace().collect();
                assert_eq!(described.first(), Some(&install.tool));
                let mut args = cmd.get_args().map(|a| a.to_str().unwrap()).peekable();
                let mut passed = Vec::new();
                while let Some(arg) = args.next() {
                    // The destination is clew's own staging directory — the
                    // one thing the prompt cannot and need not name.
                    if arg == "--prefix" || arg == "--root" {
                        assert_eq!(args.next(), Some(dest.to_str().unwrap()));
                        continue;
                    }
                    assert!(
                        described.contains(&arg),
                        "{} {version}: `{arg}` runs but the prompt says {:?}",
                        spec.name,
                        install.describe
                    );
                    passed.push(arg.to_string());
                }
                assert!(
                    !passed.iter().any(|a| a.matches('@').count() > 1),
                    "double pin in {passed:?}"
                );
                checked += 1;
            }
        }
        assert!(
            checked >= 18,
            "the registry's installers were not all walked"
        );
    }

    /// F2, spelled out: the exact command each installer spawns, compared
    /// with literals rather than with the registry's own rendering of it —
    /// which is what the prompt shows, so comparing the two proves nothing
    /// about either.
    #[test]
    fn the_spawned_install_commands_are_exactly_these() {
        let dest = Path::new("/store/servers/x/y");
        let d = dest.to_str().unwrap();
        let cases: &[(&str, &str, &str, &[&str])] = &[
            (
                "gopls",
                "latest",
                "go install golang.org/x/tools/gopls@latest",
                &["install", "golang.org/x/tools/gopls@latest"],
            ),
            (
                "gopls",
                "v0.99.0",
                "go install golang.org/x/tools/gopls@v0.99.0",
                &["install", "golang.org/x/tools/gopls@v0.99.0"],
            ),
            (
                "pyright",
                "latest",
                "npm install pyright",
                &["install", "pyright", "--prefix", d],
            ),
            (
                "pyright",
                "1.2.3",
                "npm install pyright@1.2.3",
                &["install", "pyright@1.2.3", "--prefix", d],
            ),
            (
                "typescript-language-server",
                "1.2.3",
                "npm install typescript-language-server@1.2.3 typescript@5",
                &[
                    "install",
                    "typescript-language-server@1.2.3",
                    "typescript@5",
                    "--prefix",
                    d,
                ],
            ),
            (
                "vscode-css-language-server",
                "latest",
                "npm install vscode-langservers-extracted",
                &["install", "vscode-langservers-extracted", "--prefix", d],
            ),
            (
                "taplo",
                "latest",
                "cargo install taplo-cli --features lsp",
                &["install", "taplo-cli", "--features", "lsp", "--root", d],
            ),
            (
                "taplo",
                "0.9.3",
                "cargo install taplo-cli --version 0.9.3 --features lsp",
                &[
                    "install",
                    "taplo-cli",
                    "--version",
                    "0.9.3",
                    "--features",
                    "lsp",
                    "--root",
                    d,
                ],
            ),
        ];
        for (server, version, prompt, argv) in cases {
            let Some(Provision::Install(install)) = registry::by_name(server)
                .unwrap()
                .provision(version, Platform::MacArm64)
            else {
                panic!("{server} is a toolchain install");
            };
            assert_eq!(install.describe, *prompt, "{server} {version}");
            let tool = Path::new("/opt/toolchain/bin").join(install.tool);
            let cmd = install_command(&install, &tool, dest);
            let spawned: Vec<&str> = cmd.get_args().map(|a| a.to_str().unwrap()).collect();
            assert_eq!(spawned, *argv, "{server} {version}");
            // Go takes its destination from the environment instead.
            let gobin = cmd
                .get_envs()
                .find(|(key, _)| *key == "GOBIN")
                .and_then(|(_, value)| value);
            assert_eq!(
                gobin,
                (install.tool == "go").then_some(dest.as_os_str()),
                "{server}"
            );
        }
    }

    /// F10: the toolchain is run under the name it was found by. rustup's
    /// `cargo` (volta's `npm`, mise's shims) is a link to one multiplexer
    /// that dispatches on that name; canonicalized, `cargo install …` became
    /// `rustup install …`.
    #[test]
    #[cfg(unix)]
    fn a_toolchain_shim_is_run_under_its_own_name() {
        let dir = crate::testutil::TempDir::new("store-shim");
        let multiplexer = super::testing::script(
            &dir.join("real/multiplexer"),
            "echo \"$(basename \"$0\") $*\" > invoked.txt",
        );
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::os::unix::fs::symlink(&multiplexer, bin.join("cargo")).unwrap();
        let search = std::env::join_paths([bin.as_path()]).unwrap();

        let tool = toolchain_tool("cargo", Some(&search)).unwrap();
        assert_eq!(tool, bin.join("cargo"), "the shim, not what it links to");
        let Some(Provision::Install(install)) = registry::by_name("taplo")
            .unwrap()
            .provision("latest", Platform::MacArm64)
        else {
            panic!("taplo is a cargo install");
        };
        let work = dir.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let status = install_command(&install, &tool, &work).status().unwrap();
        assert!(status.success());
        let invoked = std::fs::read_to_string(work.join("invoked.txt")).unwrap();
        assert!(invoked.starts_with("cargo install taplo-cli"), "{invoked}");

        let err = toolchain_tool("cargo", None).unwrap_err();
        assert!(err.contains("not found on PATH"), "{err}");
    }

    /// F10: a toolchain install stops — with everything it started — when
    /// it is cancelled (a restart, a project switch), and the previous
    /// install stays as it was. It used to run on for up to 45 minutes.
    #[test]
    #[cfg(unix)]
    fn a_cancelled_toolchain_install_stops_and_keeps_the_previous_one() {
        with_store("tc-cancel", |dir| {
            let dest = dir.join("servers/pyright/latest");
            let old_bin = dest.join("node_modules/.bin/pyright-langserver");
            std::fs::create_dir_all(old_bin.parent().unwrap()).unwrap();
            std::fs::write(&old_bin, b"old").unwrap();
            // Starts a helper of its own, records it, and never finishes.
            // The record is made the way `echo $! > file` makes it, the file
            // first and the pid after, with a pause in between: the cancel
            // waits for the pid itself. Sent once the file merely existed, it
            // could stop the script before the pid was in it — one run in a
            // few hundred, and every time with the pause.
            let pidfile = dir.join("helper.pid");
            let tool = super::testing::script(
                &dir.join("tools/npm"),
                &format!(
                    "sleep 60 & helper=$!; : > '{pid}'; sleep 0.2; echo $helper > '{pid}'; wait",
                    pid = pidfile.display()
                ),
            );
            let install = pyright_install("latest");
            let cancel = std::sync::Arc::new(AtomicBool::new(false));
            let flip = {
                let cancel = cancel.clone();
                let pidfile = pidfile.clone();
                std::thread::spawn(move || {
                    let helper = recorded_pid(&pidfile);
                    cancel.store(true, Ordering::Relaxed);
                    (Instant::now(), helper)
                })
            };
            let err =
                toolchain_install_with(&install, "latest", &dest, &tool, &cancel).unwrap_err();
            let returned = Instant::now();
            // Timed from the cancel, not from the spawn: the first run of a
            // fresh script can wait on the OS's check of the new file.
            let (cancelled, helper) = flip.join().unwrap();
            let helper = helper.expect("the installer never recorded its helper");
            assert!(err.contains("cancelled"), "{err}");
            assert!(returned.saturating_duration_since(cancelled) < Duration::from_secs(10));
            assert_eq!(std::fs::read(&old_bin).unwrap(), b"old");
            let mut gone = false;
            for _ in 0..200 {
                // SAFETY: signal 0 only probes for existence.
                if unsafe { libc::kill(helper, 0) } != 0 {
                    gone = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(gone, "the installer's helper outlived the cancel");
            let siblings: Vec<_> = std::fs::read_dir(dest.parent().unwrap())
                .unwrap()
                .flatten()
                .map(|e| e.file_name())
                .collect();
            assert_eq!(siblings.len(), 1, "no staging left behind: {siblings:?}");

            // Already cancelled: nothing is started at all.
            let err = run_bounded_cancellable(
                std::process::Command::new(&tool),
                Duration::from_secs(10),
                &AtomicBool::new(true),
            )
            .unwrap_err();
            assert!(err.contains("before it started"), "{err}");
        });
    }

    /// The pid a test script writes to `path` (`echo $pid > path`), once it
    /// is all there — the file exists, empty, before the pid is written —
    /// or `None` if it never is within a minute.
    ///
    /// Only a wait for the script to get going, which nothing here times:
    /// the first run of a fresh script waits on the OS's check of the new
    /// file, and on a machine busy building it took over five seconds (the
    /// bound this had), failing tests whose timing assertions all start
    /// from the cancel.
    #[cfg(unix)]
    fn recorded_pid(path: &Path) -> Option<libc::pid_t> {
        for _ in 0..6000 {
            let recorded = std::fs::read_to_string(path).ok();
            if let Some(pid) = recorded
                .as_deref()
                .and_then(|text| text.strip_suffix('\n'))
                .and_then(|pid| pid.trim().parse().ok())
            {
                return Some(pid);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        None
    }

    /// A cancel that lands while the installer is starting helpers stops
    /// those too. One SIGKILL to the group did not: a child forked as it was
    /// sent could join the group after the kernel went through it, and ran
    /// on as an orphan — here, most of the times the cancel came.
    #[test]
    #[cfg(unix)]
    fn a_cancel_while_the_installer_forks_stops_every_helper() {
        let dir = crate::testutil::TempDir::new("store-cancel-fork");
        let group = dir.join("group.pid");
        // Records its group, then starts helpers back to back — a bounded
        // number of them, whatever becomes of the cancel.
        let tool = super::testing::script(
            &dir.join("forker"),
            &format!(
                "echo $$ > '{}'; for _ in $(seq 300); do sleep 30 & done; wait",
                group.display()
            ),
        );
        for attempt in 0..8u64 {
            let _ = std::fs::remove_file(&group);
            let cancel = std::sync::Arc::new(AtomicBool::new(false));
            let flip = {
                let cancel = cancel.clone();
                let group = group.clone();
                std::thread::spawn(move || {
                    let pgid = recorded_pid(&group);
                    // At a different point of the forking each time.
                    std::thread::sleep(Duration::from_millis(5 + 4 * attempt));
                    cancel.store(true, Ordering::Relaxed);
                    pgid
                })
            };
            let err = run_bounded_cancellable(
                std::process::Command::new(&tool),
                Duration::from_secs(20),
                &cancel,
            )
            .unwrap_err();
            let pgid = flip
                .join()
                .unwrap()
                .expect("the installer never recorded its group");
            assert!(err.contains("cancelled"), "{err}");
            // What the kill missed would still be alive in the group, which
            // signal 0 finds. Killed members linger as zombies until init reaps
            // them, and Linux counts a zombie as signalled: an init that reaps
            // slowly (a container without one) needs a moment, so the probe
            // waits it out. A real survivor is a `sleep 30`, well past that.
            let deadline = Instant::now() + Duration::from_secs(10);
            // SAFETY: signal 0 only probes; the group is this test's own.
            let mut survived = unsafe { libc::killpg(pgid, 0) } == 0;
            while survived && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
                // SAFETY: as above.
                survived = unsafe { libc::killpg(pgid, 0) } == 0;
            }
            if survived {
                // SAFETY: as above; the survivors are this test's helpers.
                unsafe { libc::killpg(pgid, libc::SIGKILL) };
            }
            assert!(
                !survived,
                "a helper forked as the install was cancelled outlived it (attempt {attempt})"
            );
        }
    }

    /// F3: a consent is bound to exactly what the prompt showed. Resolving,
    /// then editing `lsp.toml` (a different version) while the prompt is up,
    /// then installing: the second resolution no longer matches the digest
    /// the prompt carried, and is refused.
    #[test]
    fn an_install_consent_is_bound_to_what_was_shown() {
        with_store("consent", |dir| {
            let root = dir.join("project");
            std::fs::create_dir_all(root.join(".clew")).unwrap();
            let resolve = |toml: &str| {
                std::fs::write(root.join(".clew/lsp.toml"), toml).unwrap();
                let config = super::super::config::ProjectLspConfig::load(&root).unwrap();
                locate(&config.resolve("python").unwrap())
            };
            let shown = resolve("[python]\nversion = \"1.1.0\"\n");
            let consent = shown.install_digest().expect("an install").to_hex();
            // Unchanged: the same resolution matches, in either case.
            let same = resolve("[python]\nversion = \"1.1.0\"\n");
            assert_eq!(same.check_install_consent(&consent), Ok(()));
            assert_eq!(same.check_install_consent(&consent.to_uppercase()), Ok(()));
            // Edited while the prompt was up.
            let edited = resolve("[python]\nversion = \"1.2.0\"\n");
            let err = edited.check_install_consent(&consent).unwrap_err();
            assert!(err.contains("changed after it was approved"), "{err}");
            // A different server is a different install too.
            let other = resolve("[python]\nserver = \"vscode-json-language-server\"\n");
            assert!(other.check_install_consent(&consent).is_err());
            // Garbage, and a resolution with nothing to install, are refused.
            assert!(
                same.check_install_consent("not a digest")
                    .unwrap_err()
                    .contains("malformed")
            );
            let command = resolve("[python]\ncommand = \"/opt/pyright\"\n");
            assert_eq!(command.install_digest(), None);
            assert!(command.check_install_consent(&consent).is_err());
            // Installed meanwhile: nothing will be installed, so nothing
            // unapproved can be.
            let ready = Located::Ready(PathBuf::from("/store/pyright"));
            assert_eq!(ready.install_digest(), None);
            assert_eq!(ready.check_install_consent(&consent), Ok(()));
        });
    }

    /// Each part of a download's identity moves the digest.
    #[test]
    fn the_install_digest_covers_the_whole_download() {
        let base = Download {
            url: "https://example.invalid/a.gz",
            sha256: Sha256::of(b"a"),
            archive: registry::Archive::Gzip,
            binary: "srv",
        };
        let located = |download: Download, dest: &str| Located::NeedsDownload {
            download,
            dest_dir: PathBuf::from(dest),
        };
        let digest = |l: Located| l.install_digest().unwrap();
        let reference = digest(located(base.clone(), "/s/srv/1"));
        assert_eq!(reference, digest(located(base.clone(), "/s/srv/1")));
        for changed in [
            located(
                Download {
                    url: "https://example.invalid/b.gz",
                    ..base.clone()
                },
                "/s/srv/1",
            ),
            located(
                Download {
                    sha256: Sha256::of(b"b"),
                    ..base.clone()
                },
                "/s/srv/1",
            ),
            located(
                Download {
                    archive: registry::Archive::TarGz,
                    ..base.clone()
                },
                "/s/srv/1",
            ),
            located(
                Download {
                    binary: "other",
                    ..base.clone()
                },
                "/s/srv/1",
            ),
            located(base.clone(), "/s/srv/2"),
        ] {
            assert_ne!(digest(changed), reference);
        }
    }

    /// F2: a version that is not plain is never provisioned, from any
    /// caller — `lsp.toml` refuses it at parse time, and `locate` holds for
    /// an `EffectiveServer` built any other way.
    #[test]
    fn a_version_that_is_not_plain_is_never_provisioned() {
        let server = EffectiveServer {
            language: "python".into(),
            server_name: "pyright".into(),
            version: "npm:evil-pkg".into(),
            args: Vec::new(),
            command: None,
            init_options: None,
        };
        match locate(&server) {
            Located::Unsupported(message) => {
                assert!(message.contains("not a plain version"), "{message}")
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    fn pyright_install(version: &str) -> Install {
        match registry::by_name("pyright")
            .unwrap()
            .provision(version, Platform::MacArm64)
        {
            Some(Provision::Install(i)) => i,
            other => panic!("pyright installs via npm, got {other:?}"),
        }
    }

    /// F2: a failed install used to `remove_dir_all` the working previous
    /// install before the new one had even started.
    #[test]
    #[cfg(unix)]
    fn a_failed_toolchain_install_keeps_the_previous_one() {
        with_store("tc-fail", |dir| {
            let dest = dir.join("servers/pyright/latest");
            let old_bin = dest.join("node_modules/.bin/pyright-langserver");
            std::fs::create_dir_all(old_bin.parent().unwrap()).unwrap();
            std::fs::write(&old_bin, b"old").unwrap();
            let tool = super::testing::script(
                &dir.join("tools/npm"),
                "echo 'npm ERR! network' >&2; exit 1",
            );
            let install = pyright_install("latest");
            let err =
                toolchain_install_with(&install, "latest", &dest, &tool, &AtomicBool::new(false))
                    .unwrap_err();
            assert!(err.contains("npm ERR! network"), "{err}");
            assert!(err.starts_with(&install.describe), "{err}");
            assert_eq!(std::fs::read(&old_bin).unwrap(), b"old");
            let siblings: Vec<_> = std::fs::read_dir(dest.parent().unwrap())
                .unwrap()
                .flatten()
                .map(|e| e.file_name())
                .collect();
            assert_eq!(siblings.len(), 1, "no staging left behind: {siblings:?}");
        });
    }

    #[test]
    #[cfg(unix)]
    fn a_successful_toolchain_install_replaces_the_previous_one() {
        with_store("tc-ok", |dir| {
            let dest = dir.join("servers/pyright/latest");
            std::fs::create_dir_all(&dest).unwrap();
            std::fs::write(dest.join("stale-file"), b"old").unwrap();
            // Installs into its cwd, which must be the staging directory, and
            // records the arguments it was given.
            let tool = super::testing::script(
                &dir.join("tools/npm"),
                "echo \"$@\" > args.txt\n\
                 mkdir -p node_modules/.bin\n\
                 printf '#!/bin/sh\\n' > node_modules/.bin/pyright-langserver",
            );
            let install = pyright_install("latest");
            let bin =
                toolchain_install_with(&install, "latest", &dest, &tool, &AtomicBool::new(false))
                    .unwrap();
            assert_eq!(bin, dest.join("node_modules/.bin/pyright-langserver"));
            assert!(bin.is_file());
            assert!(
                !dest.join("stale-file").exists(),
                "the old tree is replaced"
            );
            let args = std::fs::read_to_string(dest.join("args.txt")).unwrap();
            assert!(args.starts_with("install pyright --prefix "), "{args}");
        });
    }

    #[test]
    fn a_toolchain_install_refuses_a_version_it_was_not_prepared_for() {
        with_store("tc-version", |dir| {
            let install = pyright_install("1.1.0");
            let dest = dir.join("servers/pyright/1.2.0");
            let err = toolchain_install_with(
                &install,
                "1.2.0",
                &dest,
                Path::new("/bin/false"),
                &AtomicBool::new(false),
            )
            .unwrap_err();
            assert!(err.contains("prepared for version 1.1.0"), "{err}");
            assert!(!dest.exists());
        });
    }

    #[test]
    #[cfg(unix)]
    fn a_bounded_run_is_stopped_at_its_deadline() {
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.args(["-c", "echo out; echo err >&2; exit 3"]);
        let out = run_bounded(cmd, Duration::from_secs(10)).unwrap();
        assert_eq!(out.status.code(), Some(3));
        assert_eq!(out.stdout, b"out\n");
        assert_eq!(out.stderr, b"err\n");

        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.args(["-c", "sleep 30"]);
        let started = Instant::now();
        let err = run_bounded(cmd, Duration::from_millis(200)).unwrap_err();
        assert!(err.contains("did not finish"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    /// A relative `PATH` entry must never produce a hit, even when it names a
    /// directory (relative to the current cwd) that really contains the
    /// binary: the lookup happens in Clew's cwd but the spawn later runs in
    /// the project root, so a relative path would be re-resolved against a
    /// repo-controlled directory — check A, execute B. Absolute hits come
    /// back canonicalized (symlinks resolved), pinning the approved file —
    /// except through `find_on_path_unresolved`, for interpreters.
    ///
    /// Exercised through `find_in` with an explicit search path: rewriting
    /// the process-wide PATH raced every other test that spawns a tool.
    #[test]
    #[cfg(unix)]
    fn path_lookup_skips_relative_entries_and_non_executables() {
        let dir = crate::testutil::TempDir::new("store-pathfind");
        let real = dir.join("real");
        let linked = dir.join("linked");
        let plain = dir.join("plain");
        std::fs::create_dir_all(&linked).unwrap();
        std::fs::create_dir_all(&plain).unwrap();
        super::testing::script(&real.join("clew-fake-tool"), "");
        std::os::unix::fs::symlink(real.join("clew-fake-tool"), linked.join("clew-fake-tool"))
            .unwrap();
        // Present but not executable: the OS would not run it either.
        std::fs::write(plain.join("clew-fake-tool"), b"#!/bin/sh\n").unwrap();

        // A relative entry that DOES resolve, against the current cwd, to a
        // directory holding the binary — the poisoned case — spelled as a
        // `../..` walk from the cwd so no fixture lands in the source tree.
        let cwd = std::env::current_dir().unwrap();
        let mut relative = PathBuf::new();
        for _ in cwd.components().skip(1) {
            relative.push("..");
        }
        relative.push(real.strip_prefix("/").unwrap());
        assert!(
            relative.join("clew-fake-tool").is_file(),
            "fixture resolves"
        );

        let search =
            std::env::join_paths([relative.as_path(), plain.as_path(), linked.as_path()]).unwrap();
        let found = find_in(&search, "clew-fake-tool", true).expect("the absolute entry");
        assert_eq!(
            found,
            std::fs::canonicalize(real.join("clew-fake-tool")).unwrap()
        );
        let as_found = find_in(&search, "clew-fake-tool", false).unwrap();
        assert_eq!(
            as_found,
            linked.join("clew-fake-tool"),
            "link left unresolved"
        );
        assert_eq!(find_in(&search, "../real/clew-fake-tool", true), None);
    }

    #[test]
    fn store_paths_reject_traversal_from_project_config() {
        with_store("traversal", |dir| {
            // A version (or name) from the project's lsp.toml must not steer
            // the install path.
            assert!(server_dir("rust-analyzer", "../../../../etc").is_none());
            assert!(server_dir("rust-analyzer", "/tmp/anywhere").is_none());
            assert!(server_dir("../evil", "1").is_none());
            assert!(server_dir("rust-analyzer", "").is_none());
            assert!(server_dir("rust-analyzer", "..").is_none());
            assert!(server_dir("a/b", "1").is_none());
            // Nor collide with the store's own scratch names.
            assert!(server_dir("rust-analyzer", ".tmp-x").is_none());
            // A normal name/version still resolves under the store.
            let ok = server_dir("rust-analyzer", "2026-07-13").expect("normal path");
            assert!(ok.starts_with(dir.join("servers")));

            // …and the containment guard rejects anything outside the store
            // even if a path is constructed some other way.
            assert!(!inside_store(&dir.join("elsewhere")));
            assert!(!inside_store(std::path::Path::new("/tmp")));
            assert!(inside_store(&ok));

            // `remove` refuses such a path rather than deleting it.
            let victim = dir.join("precious");
            std::fs::create_dir_all(&victim).unwrap();
            assert!(remove("rust-analyzer", "../precious").is_ok_and(|removed| !removed));
            assert!(
                victim.exists(),
                "traversal must not delete outside the store"
            );
        });
    }

    #[test]
    fn scratch_directories_are_not_installed_servers() {
        with_store("listing", |dir| {
            std::fs::create_dir_all(dir.join("servers/zls/0.16.0")).unwrap();
            std::fs::write(dir.join("servers/zls/0.16.0/zls"), b"x").unwrap();
            std::fs::create_dir_all(dir.join("servers/zls/.tmp-0.16.0-1-0")).unwrap();
            std::fs::create_dir_all(dir.join("servers/zls/.old-0.16.0-1-1")).unwrap();
            let listed: Vec<_> = installed_servers()
                .into_iter()
                .map(|s| format!("{} {}", s.name, s.version))
                .collect();
            assert_eq!(listed, ["zls 0.16.0"]);
        });
    }

    #[test]
    fn data_root_and_server_dir_honor_override() {
        with_store("override", |dir| {
            assert_eq!(data_root().as_deref(), Some(dir));
            assert_eq!(
                server_dir("rust-analyzer", "2026-07-13"),
                Some(dir.join("servers/rust-analyzer/2026-07-13"))
            );
        });
    }

    /// Two installs of the same server in one process must stage into
    /// different directories. The old name carried only the PID, so a second
    /// install (another window, or a retry racing its predecessor) unpacked
    /// into the SAME directory and the surviving rename could publish a
    /// mixture of both.
    #[test]
    fn concurrent_staging_directories_never_collide() {
        let parent = crate::testutil::TempDir::new("staging-collide");
        let dest = parent.join("rust-analyzer");

        let a = create_staging_dir(&parent, &dest).expect("first staging dir");
        let b = create_staging_dir(&parent, &dest).expect("second staging dir");
        assert_ne!(a, b, "a concurrent install must get its own directory");
        assert!(a.is_dir() && b.is_dir());
        assert_eq!(a.parent(), Some(parent.path()));
        assert_eq!(b.parent(), Some(parent.path()));
    }
}

/// Real installs against real toolchains and the network. Ignored by default;
/// run explicitly to verify an installer end to end.
#[cfg(test)]
mod live_install_tests {
    use super::testing::with_store;
    use super::*;

    fn install_and_check(server: &str, expect_bin_ends: &str) {
        let spec = registry::by_name(server).unwrap();
        let Some(Provision::Install(install)) =
            spec.provision(spec.version, Platform::current().unwrap())
        else {
            panic!("{server} should be a toolchain install");
        };
        with_store(&format!("live-{server}"), |_| {
            let dest = server_dir(server, spec.version).unwrap();
            let bin = toolchain_install_cancellable(
                &install,
                spec.version,
                &dest,
                &AtomicBool::new(false),
            )
            .expect("install");
            assert!(bin.is_file(), "expected bin at {bin:?}");
            assert!(bin.ends_with(expect_bin_ends), "{bin:?}");
            eprintln!("{server}: installed {bin:?}");
        });
    }

    #[test]
    #[ignore = "runs a real `npm install` over the network; run explicitly"]
    fn npm_install_typescript_language_server() {
        install_and_check(
            "typescript-language-server",
            "node_modules/.bin/typescript-language-server",
        );
    }

    #[test]
    #[ignore = "runs a real `npm install` over the network; run explicitly"]
    fn config_language_servers_install() {
        install_and_check(
            "vscode-json-language-server",
            "node_modules/.bin/vscode-json-language-server",
        );
    }

    #[test]
    #[ignore = "runs a real `cargo install` (compiles taplo); run explicitly"]
    fn cargo_install_taplo_has_lsp() {
        install_and_check("taplo", "bin/taplo");
    }
}
