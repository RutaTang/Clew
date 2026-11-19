//! Building a plugin from its source: `plugin.toml` and `src/**/*.rs`.
//!
//! Building runs the Rust compiler on code an agent wrote, so the build is
//! the part of plugins that runs anything natively, and it is held to that:
//!
//! - Clew writes `Cargo.toml` itself from `plugin.toml`: crates.io
//!   dependencies by version, nothing else. No `build.rs`, `[patch]`, path or
//!   git dependency, or profile setting can come from the source.
//! - Only `src/**/*.rs` (no symlinks, size-capped), `plugin.toml` and
//!   `Cargo.lock` are copied into a clean folder of Clew's to build in: a
//!   `.cargo/config.toml` or `rust-toolchain.toml` in the source never
//!   reaches cargo.
//! - cargo and rustc are run by absolute path, with the variables that
//!   steer a build (`RUSTFLAGS`, wrappers) taken out.
//! - Before building, `cargo metadata` (which resolves and downloads, and
//!   runs nothing) lists every package with a build script or that is a
//!   proc macro: the code a build runs besides the compiler. Each must be
//!   one of a few crates nearly every Rust program uses
//!   ([`BUILD_CODE_ALLOWED`]), or the build is refused, naming it.
//!
//! What comes back for an agent is short: the compiler's own messages (20 at
//! most, no colour, paths as in the source), or what the plugin is.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::inspect::{Inspected, embed, inspect};
use crate::manifest::{Manifest, permission_names};

/// The target plugins are built for.
pub const TARGET: &str = "wasm32-wasip2";

/// The oldest Rust that builds for [`TARGET`] as a supported target.
pub const MIN_RUST: (u32, u32) = (1, 82);

/// Crates whose build script or proc macro a plugin's build may run: the
/// ones nearly every Rust program depends on. Any other is refused.
pub const BUILD_CODE_ALLOWED: &[&str] = &[
    "anyhow",
    "autocfg",
    "libm",
    "memchr",
    "num-traits",
    "paste",
    "proc-macro2",
    "quote",
    "rustversion",
    "serde",
    "serde_derive",
    "serde_json",
    "syn",
    "thiserror",
    "thiserror-impl",
    "typenum",
    "version_check",
    "zerocopy",
    "zerocopy-derive",
];

/// The guest SDK, written next to every plugin built.
const SDK: [(&str, &str); 4] = [
    (
        "Cargo.toml",
        include_str!("../../clew-plugin-sdk/Cargo.toml"),
    ),
    (
        "src/lib.rs",
        include_str!("../../clew-plugin-sdk/src/lib.rs"),
    ),
    ("src/rt.rs", include_str!("../../clew-plugin-sdk/src/rt.rs")),
    (
        "src/bindings.rs",
        include_str!("../../clew-plugin-sdk/src/bindings.rs"),
    ),
];

/// The limits on a plugin's source.
const MAX_SOURCE_FILES: usize = 200;
const MAX_SOURCE_FILE_BYTES: u64 = 1024 * 1024;
const MAX_SOURCE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_SOURCE_DEPTH: usize = 8;

/// How long resolving dependencies, and the build, may take (a first build
/// downloads the crates it uses).
const METADATA_DEADLINE: Duration = Duration::from_secs(5 * 60);
const BUILD_DEADLINE: Duration = Duration::from_secs(10 * 60);

/// The most compiler messages handed back.
const MAX_MESSAGES: usize = 20;

/// The Rust a plugin is built with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toolchain {
    pub cargo: PathBuf,
    pub rustc: PathBuf,
    /// `rustc`'s version, as `1.95.0`.
    pub version: String,
    /// Where cargo keeps what it downloads, for Clew's own Rust
    /// (`toolchain::managed_dir`); the user's default otherwise.
    pub cargo_home: Option<PathBuf>,
}

/// Why a build gave no plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildError {
    /// No Rust that can build plugins: what is missing, and what fixes it
    /// if anything does (a sentence that follows "To fix it: ").
    Toolchain {
        message: String,
        fix: Option<String>,
    },
    /// `plugin.toml` or the source as a whole is not right.
    Source(String),
    /// A dependency would run code of its own while building.
    Policy(String),
    /// The compiler refused the code: its messages.
    Compile(Vec<String>),
    /// cargo could not run, or did not finish.
    Failed(String),
}

impl BuildError {
    /// The error as an agent reads it.
    pub fn report(&self) -> String {
        match self {
            BuildError::Toolchain { message, fix } => match fix {
                Some(fix) => format!("{message}\nTo fix it: {fix}"),
                None => message.clone(),
            },
            BuildError::Source(why) => format!("The plugin's source is not right: {why}"),
            BuildError::Policy(why) => why.clone(),
            BuildError::Compile(messages) => {
                format!("The plugin did not compile:\n\n{}", messages.join("\n"))
            }
            BuildError::Failed(why) => format!("The build did not finish: {why}"),
        }
    }
}

/// A plugin built: its file, as an install would read it.
#[derive(Debug, Clone)]
pub struct Built {
    pub bytes: Vec<u8>,
    pub inspected: Inspected,
    /// Where the file is.
    pub artifact: PathBuf,
    /// What the compiler warned of in the plugin's own code, and what does
    /// not add up (a permission listed that the code never uses).
    pub warnings: Vec<String>,
}

// -- the toolchain -------------------------------------------------------------

/// The toolchain whose programs are `cargo` and `rustc`, checked: recent
/// enough, and with the [`TARGET`] standard library (`rustup`: whether
/// rustup manages it, so the fix is a rustup command).
pub fn toolchain_from(
    cargo: PathBuf,
    rustc: PathBuf,
    rustup: bool,
) -> Result<Toolchain, BuildError> {
    let version = rustc_version(&rustc)?;
    let sysroot = output(Command::new(&rustc).args(["--print", "sysroot"]))
        .map_err(|e| toolchain(format!("could not ask rustc for its sysroot: {e}")))?;
    let std_lib = Path::new(sysroot.trim())
        .join("lib")
        .join("rustlib")
        .join(TARGET);
    if !std_lib.is_dir() {
        return Err(BuildError::Toolchain {
            message: format!(
                "Rust {version} is installed, but not its {TARGET} target, which plugins are built for."
            ),
            fix: rustup.then(|| format!("run `rustup target add {TARGET}`.")),
        });
    }
    Ok(Toolchain {
        cargo,
        rustc,
        version,
        cargo_home: None,
    })
}

fn toolchain(message: String) -> BuildError {
    BuildError::Toolchain { message, fix: None }
}

/// `rustc`'s version (`1.95.0`), or why it said none.
pub(crate) fn rustc_version_of(rustc: &Path) -> Result<String, String> {
    let out =
        output(Command::new(rustc).arg("-vV")).map_err(|e| format!("could not run rustc: {e}"))?;
    out.lines()
        .find_map(|l| l.strip_prefix("release: "))
        .map(|v| v.trim().to_string())
        .ok_or_else(|| "rustc did not say its version".into())
}

/// Whether Rust `version` is older than [`MIN_RUST`].
pub(crate) fn too_old(version: &str) -> bool {
    let mut parts = version
        .split(['.', '-'])
        .map(|p| p.parse::<u32>().unwrap_or(0));
    let (major, minor) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    (major, minor) < MIN_RUST
}

/// `rustc`'s version, refused if older than [`MIN_RUST`].
fn rustc_version(rustc: &Path) -> Result<String, BuildError> {
    let version = rustc_version_of(rustc).map_err(toolchain)?;
    if too_old(&version) {
        return Err(BuildError::Toolchain {
            message: format!(
                "Rust {version} is too old to build plugins; they need {}.{} or later.",
                MIN_RUST.0, MIN_RUST.1
            ),
            fix: Some("run `rustup update stable`.".into()),
        });
    }
    Ok(version)
}

/// A short command's standard output, or why it failed.
fn output(cmd: &mut Command) -> Result<String, String> {
    let out = cmd
        .stdin(Stdio::null())
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

// -- the source ----------------------------------------------------------------

/// `plugin.toml` in `source`, read and checked.
pub fn read_manifest(source: &Path) -> Result<Manifest, BuildError> {
    let path = source.join("plugin.toml");
    let text = match clew_core::statefile::read_capped_checked(
        &path,
        crate::manifest::MAX_MANIFEST_BYTES as u64,
    ) {
        Ok(Some(text)) => text,
        Ok(None) => return Err(BuildError::Source("there is no plugin.toml".into())),
        Err(e) => return Err(BuildError::Source(format!("plugin.toml: {e}"))),
    };
    Manifest::parse(&text).map_err(BuildError::Source)
}

/// The `Cargo.toml` Clew builds `manifest`'s plugin with.
pub fn cargo_toml(manifest: &Manifest) -> String {
    let mut package = toml::Table::new();
    package.insert("name".into(), manifest.plugin.id.clone().into());
    package.insert("version".into(), manifest.plugin.version.clone().into());
    package.insert("edition".into(), "2021".into());
    package.insert("publish".into(), false.into());
    let mut lib = toml::Table::new();
    lib.insert("path".into(), "src/lib.rs".into());
    lib.insert(
        "crate-type".into(),
        toml::Value::Array(vec!["cdylib".into()]),
    );
    let mut dependencies = toml::Table::new();
    let mut sdk = toml::Table::new();
    sdk.insert("path".into(), "../sdk/clew-plugin-sdk".into());
    dependencies.insert("clew-plugin-sdk".into(), sdk.into());
    for (name, version) in &manifest.dependencies {
        dependencies.insert(name.clone(), version.clone().into());
    }
    let mut release = toml::Table::new();
    release.insert("opt-level".into(), toml::Value::Integer(3));
    release.insert("debug".into(), false.into());
    release.insert("strip".into(), true.into());
    let mut profile = toml::Table::new();
    profile.insert("release".into(), release.into());
    let mut doc = toml::Table::new();
    doc.insert("package".into(), package.into());
    doc.insert("lib".into(), lib.into());
    doc.insert("dependencies".into(), dependencies.into());
    doc.insert("profile".into(), profile.into());
    // A workspace of its own: cargo looks no further up for one.
    doc.insert("workspace".into(), toml::Table::new().into());
    format!(
        "# Written by Clew from plugin.toml for each build; edit plugin.toml instead.\n{}",
        toml::to_string(&doc).unwrap_or_default()
    )
}

/// Copy `source/src/**/*.rs` into `crate_dir/src`, which is emptied first:
/// no symlinks, nothing but Rust files, within the limits.
fn stage_sources(source: &Path, crate_dir: &Path) -> Result<(), BuildError> {
    let from = source.join("src");
    let meta = std::fs::symlink_metadata(&from)
        .map_err(|_| BuildError::Source("there is no src folder".into()))?;
    if !meta.is_dir() {
        return Err(BuildError::Source("src is not a folder".into()));
    }
    let to = crate_dir.join("src");
    let _ = std::fs::remove_dir_all(&to);
    std::fs::create_dir_all(&to).map_err(|e| BuildError::Failed(e.to_string()))?;
    let mut files = 0usize;
    let mut bytes = 0u64;
    copy_rust(&from, &to, 0, &mut files, &mut bytes)?;
    if !to.join("lib.rs").is_file() {
        return Err(BuildError::Source("there is no src/lib.rs".into()));
    }
    Ok(())
}

fn copy_rust(
    from: &Path,
    to: &Path,
    depth: usize,
    files: &mut usize,
    bytes: &mut u64,
) -> Result<(), BuildError> {
    if depth > MAX_SOURCE_DEPTH {
        return Err(BuildError::Source(format!(
            "src is more than {MAX_SOURCE_DEPTH} folders deep"
        )));
    }
    let entries = std::fs::read_dir(from).map_err(|e| BuildError::Failed(e.to_string()))?;
    for entry in entries {
        let entry = entry.map_err(|e| BuildError::Failed(e.to_string()))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with('.') {
            continue;
        }
        let meta = entry
            .metadata()
            .map_err(|e| BuildError::Failed(e.to_string()))?;
        let file_type = entry
            .file_type()
            .map_err(|e| BuildError::Failed(e.to_string()))?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            let sub = to.join(name);
            std::fs::create_dir_all(&sub).map_err(|e| BuildError::Failed(e.to_string()))?;
            copy_rust(&entry.path(), &sub, depth + 1, files, bytes)?;
        } else if file_type.is_file() && name.ends_with(".rs") {
            if meta.len() > MAX_SOURCE_FILE_BYTES {
                return Err(BuildError::Source(format!(
                    "src/…/{name} is larger than {} KB",
                    MAX_SOURCE_FILE_BYTES / 1024
                )));
            }
            *files += 1;
            *bytes += meta.len();
            if *files > MAX_SOURCE_FILES || *bytes > MAX_SOURCE_BYTES {
                return Err(BuildError::Source(format!(
                    "src holds more than {MAX_SOURCE_FILES} files or {} MB",
                    MAX_SOURCE_BYTES / (1024 * 1024)
                )));
            }
            let text =
                std::fs::read(entry.path()).map_err(|e| BuildError::Failed(e.to_string()))?;
            std::fs::write(to.join(name), text).map_err(|e| BuildError::Failed(e.to_string()))?;
        }
    }
    Ok(())
}

/// Write `contents` to `path` unless it holds them already: cargo then sees
/// the SDK unchanged, and rebuilds nothing of it.
fn write_if_changed(path: &Path, contents: &str) -> std::io::Result<()> {
    if std::fs::read(path).is_ok_and(|now| now == contents.as_bytes()) {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, contents)
}

// -- running cargo -------------------------------------------------------------

/// The variables taken out of cargo's environment: what would steer a build
/// away from what Clew wrote (flags, wrappers, another target folder or
/// toolchain).
const STEERING: &[&str] = &[
    "RUSTFLAGS",
    "RUSTDOCFLAGS",
    "CARGO_ENCODED_RUSTFLAGS",
    "CARGO_ENCODED_RUSTDOCFLAGS",
    "RUSTC_WRAPPER",
    "RUSTC_WORKSPACE_WRAPPER",
    "RUSTC_BOOTSTRAP",
    "RUSTUP_TOOLCHAIN",
    "CARGO_TARGET_DIR",
    "CARGO_INCREMENTAL",
    "CARGO_BUILD_TARGET",
    "CARGO_BUILD_RUSTFLAGS",
    "CARGO_BUILD_RUSTC",
    "CARGO_BUILD_RUSTC_WRAPPER",
    "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER",
    "CARGO_BUILD_TARGET_DIR",
];

/// cargo, set up to build in `dir` with `toolchain`, into `target`.
fn cargo(toolchain: &Toolchain, dir: &Path, target: &Path) -> Command {
    let mut cmd = clew_core::spawn::command(&toolchain.cargo);
    cmd.current_dir(dir);
    for name in STEERING {
        cmd.env_remove(name);
    }
    for (name, _) in std::env::vars_os() {
        let name = name.to_string_lossy();
        if name.starts_with("CARGO_PROFILE_") || name.starts_with("CARGO_TARGET_") {
            cmd.env_remove(&*name);
        }
    }
    cmd.env("RUSTC", &toolchain.rustc)
        .env("CARGO_TARGET_DIR", target)
        .env("CARGO_TERM_COLOR", "never")
        .env("CARGO_TERM_PROGRESS_WHEN", "never");
    if let Some(home) = &toolchain.cargo_home {
        cmd.env("CARGO_HOME", home);
    }
    cmd
}

/// What a finished run of a program left.
pub(crate) struct Ran {
    pub(crate) success: bool,
    pub(crate) stdout: String,
    /// The end of its standard error.
    pub(crate) stderr: String,
}

/// The most of cargo's standard output kept, and of its error.
const STDOUT_CAP: usize = 32 * 1024 * 1024;
const STDERR_TAIL: usize = 64 * 1024;

/// Run `cmd` to its end within `limit`, or until `cancel` is set: then it
/// is stopped with all it started (its own process group).
pub(crate) fn run(
    mut cmd: Command,
    limit: Duration,
    cancel: &AtomicBool,
) -> Result<Ran, BuildError> {
    use std::io::Read;
    if cancel.load(Ordering::Relaxed) {
        return Err(BuildError::Failed("cancelled".into()));
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let program = Path::new(cmd.get_program())
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut child = cmd
        .spawn()
        .map_err(|e| BuildError::Failed(format!("could not run {program}: {e}")))?;
    #[cfg(windows)]
    let _job = clew_core::procgroup::adopt_scoped(&child);
    let read = |stream: Option<Box<dyn Read + Send>>, keep_tail: bool, cap: usize| {
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        if let Some(mut stream) = stream {
            std::thread::spawn(move || {
                let mut kept = Vec::new();
                let mut buf = [0u8; 16 * 1024];
                while let Ok(n) = stream.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    if keep_tail {
                        kept.extend_from_slice(&buf[..n]);
                        if kept.len() > 2 * cap {
                            kept.drain(..kept.len() - cap);
                        }
                    } else if kept.len() < cap {
                        let room = cap - kept.len();
                        kept.extend_from_slice(&buf[..n.min(room)]);
                    }
                }
                let _ = tx.send(kept);
            });
        }
        rx
    };
    let stdout = read(
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
        false,
        STDOUT_CAP,
    );
    let stderr = read(
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
        true,
        STDERR_TAIL,
    );
    let deadline = Instant::now() + limit;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                let cancelled = cancel.load(Ordering::Relaxed);
                if !cancelled && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(25));
                    continue;
                }
                clew_core::procgroup::kill(child.id());
                let _ = child.kill();
                let _ = child.wait();
                return Err(BuildError::Failed(if cancelled {
                    "cancelled".into()
                } else {
                    format!("it took longer than {} minutes", limit.as_secs() / 60)
                }));
            }
            Err(e) => return Err(BuildError::Failed(e.to_string())),
        }
    };
    let text = |rx: std::sync::mpsc::Receiver<Vec<u8>>| {
        String::from_utf8_lossy(&rx.recv().unwrap_or_default()).into_owned()
    };
    Ok(Ran {
        success: status.success(),
        stdout: text(stdout),
        stderr: text(stderr),
    })
}

/// The last `n` lines of cargo's error output that say something.
fn last_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter(|l| {
            let t = l.trim_start();
            !(t.starts_with("Compiling ")
                || t.starts_with("Downloaded ")
                || t.starts_with("Downloading ")
                || t.starts_with("Updating ")
                || t.starts_with("Locking ")
                || t.starts_with("Adding "))
        })
        .collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// Refuse a build whose dependencies run code of their own while building,
/// beyond [`BUILD_CODE_ALLOWED`], as `cargo metadata` lists them.
fn check_dependencies(metadata: &str, plugin: &str) -> Result<(), BuildError> {
    let metadata: serde_json::Value = serde_json::from_str(metadata).map_err(|e| {
        BuildError::Failed(format!("cargo metadata said what Clew cannot read: {e}"))
    })?;
    let mut refused = Vec::new();
    for package in metadata["packages"].as_array().into_iter().flatten() {
        let name = package["name"].as_str().unwrap_or_default();
        if name == plugin || name == "clew-plugin-sdk" {
            continue;
        }
        let runs_code = package["targets"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|t| {
                t["kind"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|k| matches!(k.as_str(), Some("custom-build") | Some("proc-macro")))
            });
        if runs_code && !BUILD_CODE_ALLOWED.contains(&name) {
            let version = package["version"].as_str().unwrap_or_default();
            refused.push(format!("{name} {version}"));
        }
    }
    if refused.is_empty() {
        Ok(())
    } else {
        refused.sort();
        refused.dedup();
        Err(BuildError::Policy(format!(
            "The plugin was not built: {} would run code of its own while building (a build script or a proc macro). A plugin may only use crates that do not, apart from {}. Use another crate, or write what you need yourself.",
            refused.join(", "),
            BUILD_CODE_ALLOWED.join(", ")
        )))
    }
}

/// What `cargo build --message-format=json` said: the plugin's file, the
/// errors, and the warnings in the plugin's own code.
struct Messages {
    artifact: Option<PathBuf>,
    errors: Vec<String>,
    warnings: Vec<String>,
}

fn read_messages(stdout: &str, plugin: &str) -> Messages {
    let lib = plugin.replace('-', "_");
    let mut messages = Messages {
        artifact: None,
        errors: Vec::new(),
        warnings: Vec::new(),
    };
    for line in stdout.lines() {
        let Ok(message) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match message["reason"].as_str() {
            Some("compiler-artifact") if message["target"]["name"] == lib.as_str() => {
                messages.artifact = message["filenames"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|f| f.as_str())
                    .find(|f| f.ends_with(".wasm"))
                    .map(PathBuf::from);
            }
            Some("compiler-message") => {
                let rendered = message["message"]["rendered"].as_str().unwrap_or_default();
                let rendered = rendered.trim_end().to_string();
                if rendered.is_empty() {
                    continue;
                }
                let own = message["target"]["name"] == lib.as_str();
                match message["message"]["level"].as_str() {
                    Some("error") | Some("error: internal compiler error")
                        if messages.errors.len() < MAX_MESSAGES
                            && !rendered.starts_with("error: aborting due to") =>
                    {
                        messages.errors.push(capped(&rendered, 8 * 1024));
                    }
                    Some("warning")
                        if own
                            && messages.warnings.len() < MAX_MESSAGES
                            && !rendered.starts_with("warning: unused")
                            && !rendered.contains("warning emitted")
                            && !rendered.contains("warnings emitted") =>
                    {
                        messages.warnings.push(capped(&rendered, 4 * 1024));
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    messages
}

fn capped(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n…", &text[..end])
}

// -- the build ------------------------------------------------------------------

/// Build the plugin whose source is `source` with `toolchain`, in `work`
/// (Clew's own build folder: `<data>/plugins/build`). The result is checked
/// as an install would check it: a plugin whose code needs a permission its
/// `plugin.toml` does not list is refused here, with which one.
pub fn build(
    source: &Path,
    work: &Path,
    toolchain: &Toolchain,
    cancel: &AtomicBool,
) -> Result<Built, BuildError> {
    let manifest = read_manifest(source)?;
    let id = manifest.plugin.id.clone();
    let crate_dir = work.join(&id);
    std::fs::create_dir_all(&crate_dir).map_err(|e| BuildError::Failed(e.to_string()))?;
    let _exclusive = clew_core::statefile::lock_named(&crate_dir.join("Cargo.toml"), ".build.lock")
        .map_err(|e| BuildError::Failed(format!("another build of {id} is running ({e})")))?;

    for (name, contents) in SDK {
        write_if_changed(&work.join("sdk/clew-plugin-sdk").join(name), contents)
            .map_err(|e| BuildError::Failed(e.to_string()))?;
    }
    stage_sources(source, &crate_dir)?;
    write_if_changed(&crate_dir.join("Cargo.toml"), &cargo_toml(&manifest))
        .map_err(|e| BuildError::Failed(e.to_string()))?;
    let lock = source.join("Cargo.lock");
    match std::fs::read(&lock) {
        Ok(bytes) if bytes.len() <= 1024 * 1024 && !lock.is_symlink() => {
            std::fs::write(crate_dir.join("Cargo.lock"), bytes)
                .map_err(|e| BuildError::Failed(e.to_string()))?;
        }
        _ => {
            let _ = std::fs::remove_file(crate_dir.join("Cargo.lock"));
        }
    }

    let target = work.join("target");
    let mut metadata = cargo(toolchain, &crate_dir, &target);
    metadata.args([
        "metadata",
        "--format-version",
        "1",
        "--filter-platform",
        TARGET,
    ]);
    let ran = run(metadata, METADATA_DEADLINE, cancel)?;
    if !ran.success {
        return Err(BuildError::Source(format!(
            "its dependencies could not be resolved:\n{}",
            last_lines(&ran.stderr, 20)
        )));
    }
    check_dependencies(&ran.stdout, &id)?;

    let mut build = cargo(toolchain, &crate_dir, &target);
    build.args([
        "build",
        "--release",
        "--target",
        TARGET,
        "--message-format=json",
    ]);
    let ran = run(build, BUILD_DEADLINE, cancel)?;
    let messages = read_messages(&ran.stdout, &id);
    if !ran.success {
        if !messages.errors.is_empty() {
            return Err(BuildError::Compile(messages.errors));
        }
        return Err(BuildError::Failed(last_lines(&ran.stderr, 30)));
    }
    let wasm = messages
        .artifact
        .ok_or_else(|| BuildError::Failed("cargo built no WebAssembly file".into()))?;
    let component = std::fs::read(&wasm).map_err(|e| BuildError::Failed(e.to_string()))?;
    let bytes = embed(&component, &manifest).map_err(BuildError::Failed)?;
    let inspected = inspect(&bytes).map_err(BuildError::Source)?;

    let mut warnings = messages.warnings;
    let unused = manifest.declared() & !inspected.needs;
    if unused != 0 {
        warnings.push(format!(
            "plugin.toml lists {} in [plugin] permissions, but the code never uses {}: take {} out, so the user is not asked for it",
            quoted(&permission_names(unused)),
            if unused.count_ones() == 1 { "it" } else { "them" },
            if unused.count_ones() == 1 { "it" } else { "them" },
        ));
    }
    let artifact = crate_dir.join(format!("{id}.wasm"));
    std::fs::write(&artifact, &bytes).map_err(|e| BuildError::Failed(e.to_string()))?;
    if let Ok(lock) = std::fs::read(crate_dir.join("Cargo.lock")) {
        let _ = std::fs::write(source.join("Cargo.lock"), lock);
    }
    Ok(Built {
        bytes,
        inspected,
        artifact,
        warnings,
    })
}

fn quoted(names: &[String]) -> String {
    names
        .iter()
        .map(|n| format!("\"{n}\""))
        .collect::<Vec<_>>()
        .join(" and ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::tests_support;

    #[test]
    fn cargo_toml_holds_only_what_clew_writes() {
        let mut m = tests_support::manifest();
        m.dependencies.insert("regex".into(), "1".into());
        m.dependencies.insert("weird".into(), "1\"\n[patch]".into());
        let text = cargo_toml(&m);
        let doc: toml::Table = toml::from_str(&text).unwrap();
        assert_eq!(doc["package"]["name"].as_str(), Some("sample"));
        assert_eq!(doc["package"]["edition"].as_str(), Some("2021"));
        assert_eq!(doc["dependencies"]["regex"].as_str(), Some("1"));
        assert_eq!(
            doc["dependencies"]["weird"].as_str(),
            Some("1\"\n[patch]"),
            "a value is a string, never TOML of its own"
        );
        assert_eq!(
            doc["dependencies"]["clew-plugin-sdk"]["path"].as_str(),
            Some("../sdk/clew-plugin-sdk")
        );
        assert!(doc.contains_key("workspace"));
        assert!(!doc.contains_key("patch"));
        assert!(doc["package"].get("build").is_none());
    }

    #[test]
    fn only_rust_files_under_src_are_staged() {
        let root = std::env::temp_dir().join(format!("clew-plugin-stage-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let source = root.join("source");
        std::fs::create_dir_all(source.join("src/nested")).unwrap();
        std::fs::create_dir_all(source.join(".cargo")).unwrap();
        std::fs::write(source.join("src/lib.rs"), "// lib").unwrap();
        std::fs::write(source.join("src/nested/more.rs"), "// more").unwrap();
        std::fs::write(source.join("src/notes.txt"), "no").unwrap();
        std::fs::write(source.join("build.rs"), "fn main() {}").unwrap();
        std::fs::write(
            source.join(".cargo/config.toml"),
            "[build]\nrustc-wrapper = \"x\"",
        )
        .unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc/passwd", source.join("src/passwd.rs")).unwrap();
        let crate_dir = root.join("work/sample");
        std::fs::create_dir_all(crate_dir.join("src")).unwrap();
        std::fs::write(crate_dir.join("src/stale.rs"), "// gone").unwrap();
        stage_sources(&source, &crate_dir).unwrap();
        assert!(crate_dir.join("src/lib.rs").is_file());
        assert!(crate_dir.join("src/nested/more.rs").is_file());
        assert!(!crate_dir.join("src/notes.txt").exists());
        assert!(!crate_dir.join("src/stale.rs").exists());
        assert!(!crate_dir.join("src/passwd.rs").exists());
        assert!(!crate_dir.join("build.rs").exists());
        assert!(!crate_dir.join(".cargo").exists());

        std::fs::remove_file(source.join("src/lib.rs")).unwrap();
        assert!(matches!(
            stage_sources(&source, &crate_dir),
            Err(BuildError::Source(why)) if why.contains("src/lib.rs")
        ));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn dependencies_that_run_code_are_refused_unless_well_known() {
        let metadata = |name: &str, kind: &str| {
            format!(
                r#"{{"packages":[
                    {{"name":"sample","version":"0.1.0","targets":[{{"kind":["cdylib"]}}]}},
                    {{"name":"serde_derive","version":"1.0.0","targets":[{{"kind":["proc-macro"]}}]}},
                    {{"name":"{name}","version":"2.0.0","targets":[{{"kind":["{kind}"]}},{{"kind":["lib"]}}]}}
                ]}}"#
            )
        };
        assert!(check_dependencies(&metadata("regex", "lib"), "sample").is_ok());
        assert!(check_dependencies(&metadata("serde", "custom-build"), "sample").is_ok());
        let refused = check_dependencies(&metadata("evil", "custom-build"), "sample").unwrap_err();
        assert!(matches!(&refused, BuildError::Policy(why) if why.contains("evil 2.0.0")));
        assert!(check_dependencies(&metadata("magic", "proc-macro"), "sample").is_err());
    }

    #[test]
    fn compiler_messages_are_read_from_cargos_json() {
        let stdout = [
            r#"{"reason":"compiler-message","target":{"name":"regex"},"message":{"level":"warning","rendered":"warning: in a dependency"}}"#,
            r#"{"reason":"compiler-message","target":{"name":"todo_authors"},"message":{"level":"warning","rendered":"warning: this is the plugin's own\n --> src/lib.rs:3:5\n"}}"#,
            r#"{"reason":"compiler-message","target":{"name":"todo_authors"},"message":{"level":"error","rendered":"error[E0425]: cannot find value `x`\n --> src/lib.rs:9:1\n"}}"#,
            r#"{"reason":"compiler-message","target":{"name":"todo_authors"},"message":{"level":"error","rendered":"error: aborting due to 1 previous error"}}"#,
            r#"{"reason":"compiler-artifact","target":{"name":"todo_authors"},"filenames":["/w/target/wasm32-wasip2/release/todo_authors.wasm"]}"#,
            "not json",
        ]
        .join("\n");
        let m = read_messages(&stdout, "todo-authors");
        assert_eq!(m.errors.len(), 1);
        assert!(m.errors[0].contains("E0425"));
        assert_eq!(m.warnings.len(), 1);
        assert!(m.warnings[0].contains("plugin's own"));
        assert_eq!(
            m.artifact.unwrap(),
            PathBuf::from("/w/target/wasm32-wasip2/release/todo_authors.wasm")
        );
    }

    #[test]
    fn the_sdk_written_beside_a_plugin_is_this_one() {
        let names: Vec<&str> = SDK.iter().map(|(n, _)| *n).collect();
        assert_eq!(
            names,
            ["Cargo.toml", "src/lib.rs", "src/rt.rs", "src/bindings.rs"]
        );
        assert!(SDK[0].1.contains("name = \"clew-plugin-sdk\""));
    }

    #[test]
    fn a_report_says_what_to_do() {
        let e = BuildError::Toolchain {
            message: "no target".into(),
            fix: Some("rustup target add wasm32-wasip2".into()),
        };
        assert!(
            e.report()
                .contains("To fix it: rustup target add wasm32-wasip2")
        );
        let e = BuildError::Compile(vec!["error: one".into(), "error: two".into()]);
        assert!(e.report().contains("error: one\nerror: two"));
    }
}
