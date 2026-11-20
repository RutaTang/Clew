//! The Rust plugins are built with, and the one step that makes it ready.
//!
//! Clew builds with the user's own Rust when it can: rustup's default
//! toolchain, else `cargo` and `rustc` on PATH. When that is missing, too
//! old or without the [`TARGET`] standard library, the user is offered the
//! step that fixes it, and asked before it runs:
//!
//! - with rustup, adding the target to the default toolchain
//!   (`rustup target add wasm32-wasip2`): small, and what rustup is for;
//! - otherwise (no Rust, Rust from a package manager, a toolchain too old
//!   or no default one), Rust of Clew's own, installed by rustup's own
//!   installer into Clew's data folder (`<data>/plugins/rust`): nothing in
//!   the user's home, PATH or shell profile changes, and removing the folder
//!   removes it. The installer is downloaded from static.rust-lang.org and
//!   checked against the SHA-256 published beside it before it runs.
//!
//! The user's Rust is used first when it is ready, so a user who sets their
//! own up later builds with it.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::build::{BuildError, MIN_RUST, TARGET, Toolchain};

/// Where Clew keeps the Rust it installs: rustup's state in `rustup`,
/// cargo's (its programs and the crates it downloads) in `cargo`.
pub fn managed_dir(data: &Path) -> PathBuf {
    crate::store::plugins_dir(data).join("rust")
}

/// What makes plugin building ready, as the user is offered it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Setup {
    /// rustup is installed: add the target to its default toolchain.
    AddTarget { rustup: PathBuf },
    /// Install Rust of Clew's own, in its data folder.
    Install,
}

/// Why plugins cannot be built here yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Why {
    /// No Rust at all.
    NoRust,
    /// rustup without a default toolchain; what it said.
    NoDefault(String),
    /// Rust without the target plugins are built for, by its version.
    NoTarget(String),
    /// Rust older than [`MIN_RUST`], by its version.
    TooOld(String),
    /// Clew's own Rust does not work; why.
    OwnBroken(String),
    /// Something else kept Rust from saying what it is.
    Other(String),
}

impl Why {
    /// The reason, as a sentence for an agent.
    pub fn sentence(&self) -> String {
        let (major, minor) = MIN_RUST;
        match self {
            Why::NoRust => "Building plugins needs Rust, and it is not installed.".into(),
            Why::NoDefault(said) => {
                format!("rustup has no default Rust toolchain to build plugins with ({said}).")
            }
            Why::NoTarget(version) => format!(
                "Rust {version} is installed, but not its {TARGET} target, which plugins are built for."
            ),
            Why::TooOld(version) => format!(
                "Rust {version} is too old to build plugins; they need {major}.{minor} or later."
            ),
            Why::OwnBroken(why) => {
                format!("Clew's own Rust, installed for plugins, does not work: {why}")
            }
            Why::Other(why) => format!("Rust cannot build plugins here: {why}"),
        }
    }
}

/// Why plugins cannot be built here yet, and the step that fixes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Missing {
    pub why: Why,
    /// The step that fixes it; `None` where Clew cannot take it (a
    /// platform rustup has no installer for).
    pub setup: Option<Setup>,
    /// The user's Rust is rustup's: `rustup update` brings one too old up
    /// to date. A package manager's Rust has no rustup to say it of.
    pub rustup: bool,
}

impl Missing {
    /// The error a build gives, for an agent to relay: what is missing, and
    /// that the user sets it up in Clew.
    pub fn into_build_error(self) -> BuildError {
        let fix = match &self.setup {
            // Where to go, not a button's English name: the user may read
            // Clew in another language.
            Some(Setup::AddTarget { .. }) => Some(format!(
                "ask the user to open Clew's Settings → Plugins, where one button adds the WebAssembly target, or to run `rustup target add {TARGET}`; then build again."
            )),
            Some(Setup::Install) => Some(
                "ask the user to open Clew's Settings → Plugins, where one button installs Rust into Clew's own folder (changing nothing else on the computer); then build again.".into(),
            ),
            None => Some("ask the user to install Rust from https://rustup.rs; then build again.".into()),
        };
        BuildError::Toolchain {
            message: self.why.sentence(),
            fix,
        }
    }
}

/// The Rust to build plugins with: the user's when it is ready, else
/// Clew's own when it is installed and ready. When neither is, why — the
/// user's Rust's problem, as that is what they know of — and the step
/// that fixes it.
pub fn find(data: Option<&Path>) -> Result<Toolchain, Missing> {
    let own = match system() {
        Ok(toolchain) => return Ok(toolchain),
        Err(missing) => missing,
    };
    let Some(data) = data else {
        return Err(own);
    };
    match managed(data) {
        Some(Ok(toolchain)) => Ok(toolchain),
        // Clew's own is there but broken (interrupted, removed in part):
        // installed again.
        Some(Err(why)) => Err(Missing {
            why: Why::OwnBroken(why.sentence()),
            setup: installable().then_some(Setup::Install),
            rustup: false,
        }),
        None => Err(own),
    }
}

/// Whether Clew's own Rust is installed, ready or not.
pub fn managed_installed(data: &Path) -> bool {
    managed_dir(data).exists()
}

/// The user's own Rust, checked; why not, and how to fix it.
fn system() -> Result<Toolchain, Missing> {
    let install = || installable().then_some(Setup::Install);
    let Some(rustup) = clew_core::lsp::store::find_on_path_unresolved("rustup") else {
        let cargo = clew_core::lsp::store::find_on_path_unresolved("cargo");
        let rustc = clew_core::lsp::store::find_on_path_unresolved("rustc");
        let (Some(cargo), Some(rustc)) = (cargo, rustc) else {
            return Err(Missing {
                why: Why::NoRust,
                setup: install(),
                rustup: false,
            });
        };
        // Rust without rustup (a package manager's): its target cannot be
        // added, so Clew's own is offered.
        return checked(cargo, rustc, None).map_err(|why| Missing {
            why,
            setup: install(),
            rustup: false,
        });
    };
    let which = |tool| rustup_which(&rustup, tool, &[]);
    let (cargo, rustc) = match (which("cargo"), which("rustc")) {
        (Ok(cargo), Ok(rustc)) => (cargo, rustc),
        (Err(why), _) | (_, Err(why)) => {
            return Err(Missing {
                why: Why::NoDefault(why),
                setup: install(),
                rustup: true,
            });
        }
    };
    match checked(cargo, rustc, None) {
        Ok(toolchain) => Ok(toolchain),
        Err(why @ Why::NoTarget(_)) => Err(Missing {
            why,
            setup: Some(Setup::AddTarget { rustup }),
            rustup: true,
        }),
        // Too old: updating the user's default toolchain is theirs to
        // decide; Clew's own leaves it as it is.
        Err(why) => Err(Missing {
            why,
            setup: install(),
            rustup: true,
        }),
    }
}

/// Clew's own Rust, if it was installed: ready, or why not.
fn managed(data: &Path) -> Option<Result<Toolchain, Why>> {
    let dir = managed_dir(data);
    if !dir.exists() {
        return None;
    }
    let rustup = dir.join("cargo").join("bin").join(exe("rustup"));
    if !rustup.is_file() {
        return Some(Err(Why::Other("its rustup is missing".into())));
    }
    let env = managed_env(&dir);
    let found = rustup_which(&rustup, "cargo", &env)
        .and_then(|cargo| Ok((cargo, rustup_which(&rustup, "rustc", &env)?)))
        .map_err(Why::Other)
        .and_then(|(cargo, rustc)| checked(cargo, rustc, Some(dir.join("cargo"))));
    Some(found)
}

/// The variables Clew's own rustup runs with.
fn managed_env(dir: &Path) -> Vec<(&'static str, PathBuf)> {
    vec![
        ("RUSTUP_HOME", dir.join("rustup")),
        ("CARGO_HOME", dir.join("cargo")),
    ]
}

/// `cargo` and `rustc` checked as a toolchain plugins can be built with;
/// why not.
fn checked(cargo: PathBuf, rustc: PathBuf, cargo_home: Option<PathBuf>) -> Result<Toolchain, Why> {
    let version = crate::build::rustc_version_of(&rustc).map_err(Why::Other)?;
    if crate::build::too_old(&version) {
        return Err(Why::TooOld(version));
    }
    // The fix for a missing target is offered by the caller, not said here.
    crate::build::toolchain_from(cargo, rustc, false)
        .map(|toolchain| Toolchain {
            cargo_home,
            ..toolchain
        })
        .map_err(|e| match e {
            BuildError::Toolchain { message, .. } if message.contains(TARGET) => {
                Why::NoTarget(version)
            }
            BuildError::Toolchain { message, .. } => Why::Other(message),
            other => Why::Other(other.report()),
        })
}

/// The program rustup runs as `tool` for its default toolchain.
fn rustup_which(rustup: &Path, tool: &str, env: &[(&str, PathBuf)]) -> Result<PathBuf, String> {
    let mut cmd = Command::new(rustup);
    cmd.args(["which", tool])
        .current_dir(clew_core::linerpc::neutral_dir())
        .env_remove("RUSTUP_TOOLCHAIN")
        .stdin(std::process::Stdio::null());
    for (name, value) in env {
        cmd.env(name, value);
    }
    let out = cmd.output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        let why = String::from_utf8_lossy(&out.stderr);
        return Err(why
            .lines()
            .next()
            .unwrap_or("rustup failed")
            .trim()
            .to_string());
    }
    let path = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    if path.is_absolute() && path.is_file() {
        Ok(path)
    } else {
        Err(format!("rustup named no {tool}"))
    }
}

/// `name` as a program is named on this platform.
fn exe(name: &str) -> String {
    format!("{name}{}", std::env::consts::EXE_SUFFIX)
}

/// The platform rustup's installer is published for, as its triple; `None`
/// where there is none Clew can run.
pub fn host_triple() -> Option<&'static str> {
    Some(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("windows", "x86_64") => "x86_64-pc-windows-msvc",
        ("windows", "aarch64") => "aarch64-pc-windows-msvc",
        _ => return None,
    })
}

/// Whether Clew can install Rust of its own here.
fn installable() -> bool {
    host_triple().is_some()
}

/// Where rustup's installer for `triple` is published.
pub fn installer_url(triple: &str) -> String {
    format!(
        "https://static.rust-lang.org/rustup/dist/{triple}/{}",
        exe("rustup-init")
    )
}

/// The SHA-256 a `.sha256` file published beside a download says, in
/// lower-case hex: its first word (`<hex> *./rustup-init`).
pub fn published_sha256(text: &str) -> Option<String> {
    let hex = text.split_whitespace().next()?.to_ascii_lowercase();
    (hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit())).then_some(hex)
}

/// What a setup step is doing, as it goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Downloading rustup's installer.
    Downloading,
    /// rustup is installing Rust (it downloads about 250 MB).
    Installing,
    /// rustup is adding the target.
    AddingTarget,
}

/// How long each step may take.
const DOWNLOAD_DEADLINE: Duration = Duration::from_secs(10 * 60);
const INSTALL_DEADLINE: Duration = Duration::from_secs(45 * 60);
const TARGET_DEADLINE: Duration = Duration::from_secs(20 * 60);

/// The largest installer accepted.
const INSTALLER_MAX: u64 = 64 * 1024 * 1024;

/// Why setting Rust up did not work.
#[derive(Debug, Clone)]
pub enum SetUpError {
    /// The step ran, and plugins still cannot be built: why, for the
    /// reader's words (`Why`), not its English sentence.
    Missing(Missing),
    /// The step failed: rustup's own words, or one of [`NO_INSTALLER`],
    /// [`NO_CHECKSUM`], [`NOT_PUBLISHED`].
    Failed(String),
    /// Another Clew window is setting Rust up or removing Clew's own
    /// ([`hold`]): nothing was done.
    Elsewhere,
}

impl SetUpError {
    /// In English, for whoever reads it outside Clew's window.
    pub fn sentence(&self) -> String {
        match self {
            SetUpError::Missing(missing) => missing.why.sentence(),
            SetUpError::Failed(why) => why.clone(),
            SetUpError::Elsewhere => ELSEWHERE.into(),
        }
    }
}

/// Said when another Clew window is setting Rust up or removing it.
pub const ELSEWHERE: &str = "another Clew window is setting up Rust or removing it";

/// Rust set up or removed by one Clew window at a time, all windows
/// sharing Clew's data folder: held while one is (an exclusive lock on a
/// file beside [`managed_dir`], which removing that folder leaves).
/// Released when dropped, or when the window's process ends.
pub struct Held(Option<std::fs::File>);

impl Drop for Held {
    fn drop(&mut self) {
        if let Some(file) = &self.0 {
            let _ = file.unlock();
        }
    }
}

fn lock_path(data: &Path) -> PathBuf {
    crate::store::plugins_dir(data).join("rust.lock")
}

/// Hold Rust's setup for this window: `None` while another holds it.
/// Where locks cannot be had (a folder that cannot be written, a file
/// system without locks) it is held without one, as Clew's other shared
/// files are, and the step says what fails.
pub fn hold(data: &Path) -> Option<Held> {
    let path = lock_path(data);
    let _ = clew_core::derived::ensure_private_dir(&crate::store::plugins_dir(data));
    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
    else {
        return Some(Held(None));
    };
    // `busy` takes the lock for an instant to see whether it is free: a
    // window holding it is told from that by trying a few times over a
    // fifth of a second.
    for _ in 0..10 {
        match file.try_lock() {
            Ok(()) => return Some(Held(Some(file))),
            Err(std::fs::TryLockError::WouldBlock) => {
                std::thread::sleep(std::time::Duration::from_millis(20))
            }
            Err(std::fs::TryLockError::Error(_)) => return Some(Held(None)),
        }
    }
    None
}

/// Whether another Clew window is setting Rust up or removing it now.
pub fn busy(data: &Path) -> bool {
    let Ok(file) = std::fs::File::open(lock_path(data)) else {
        return false;
    };
    match file.try_lock() {
        Ok(()) => {
            let _ = file.unlock();
            false
        }
        Err(std::fs::TryLockError::WouldBlock) => true,
        Err(std::fs::TryLockError::Error(_)) => false,
    }
}

/// Wait while another Clew window sets Rust up or removes it.
pub fn wait_while_busy(data: &Path) {
    let Ok(file) = std::fs::File::open(lock_path(data)) else {
        return;
    };
    if file.lock().is_ok() {
        let _ = file.unlock();
    }
}

/// Why Clew's own Rust could not be installed, in words of Clew's own
/// (translated where shown).
pub const NO_INSTALLER: &str = "rustup publishes no installer for this computer";
pub const NO_CHECKSUM: &str = "the installer's published SHA-256 could not be read";
pub const NOT_PUBLISHED: &str =
    "the installer downloaded is not the one rust-lang.org published; nothing was run";

/// Take the step `setup`, telling `stage` how it goes, until it is done or
/// `cancel` is set; then check that plugins can be built. Refused while
/// another Clew window sets Rust up or removes it ([`hold`]).
pub fn set_up(
    data: &Path,
    setup: &Setup,
    cancel: &AtomicBool,
    stage: &mut dyn FnMut(Stage),
) -> Result<Toolchain, SetUpError> {
    let Some(_held) = hold(data) else {
        return Err(SetUpError::Elsewhere);
    };
    // Set up meanwhile — by another window, or outside Clew: nothing to do.
    // An install run again for nothing was 700 MB downloaded, and, cancelled
    // or failed, it removed the Rust just installed.
    if let Ok(toolchain) = find(Some(data)) {
        return Ok(toolchain);
    }
    match setup {
        Setup::AddTarget { rustup } => {
            stage(Stage::AddingTarget);
            let mut cmd = Command::new(rustup);
            cmd.args(["target", "add", TARGET])
                .current_dir(clew_core::linerpc::neutral_dir())
                .env_remove("RUSTUP_TOOLCHAIN");
            run(cmd, TARGET_DEADLINE, cancel).map_err(SetUpError::Failed)?;
        }
        Setup::Install => install(data, cancel, stage).map_err(SetUpError::Failed)?,
    }
    find(Some(data)).map_err(SetUpError::Missing)
}

/// Install Rust of Clew's own, with the target, into [`managed_dir`].
fn install(data: &Path, cancel: &AtomicBool, stage: &mut dyn FnMut(Stage)) -> Result<(), String> {
    let triple = host_triple().ok_or(NO_INSTALLER)?;
    install_from(data, &installer_url(triple), cancel, stage)
}

/// [`install`], with rustup's installer at `url` and its SHA-256 at
/// `url.sha256`.
fn install_from(
    data: &Path,
    url: &str,
    cancel: &AtomicBool,
    stage: &mut dyn FnMut(Stage),
) -> Result<(), String> {
    let dir = managed_dir(data);
    stage(Stage::Downloading);
    let limits = clew_core::net::Limits {
        max_bytes: 4096,
        deadline: Duration::from_secs(60),
    };
    let published = clew_core::net::get_cancellable(&format!("{url}.sha256"), &[], limits, cancel)?;
    let expected = published_sha256(&String::from_utf8_lossy(&published)).ok_or(NO_CHECKSUM)?;
    let installer = dir.join(exe("rustup-init"));
    let partial = dir.join(format!("{}.part", exe("rustup-init")));
    let result = (|| {
        // Made only now, and taken away with the rest on a failure: made
        // before the checksum's fetch, an install offline or cancelled at
        // once left an empty folder read as Clew's own Rust, broken, and
        // the real reason was lost.
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let mut file = std::fs::File::create(&partial).map_err(|e| e.to_string())?;
        clew_core::net::download_resuming(
            url,
            &[],
            clew_core::net::Limits {
                max_bytes: INSTALLER_MAX,
                deadline: DOWNLOAD_DEADLINE,
            },
            cancel,
            &mut file,
            |_, _| {},
        )?;
        drop(file);
        let bytes = std::fs::read(&partial).map_err(|e| e.to_string())?;
        let digest: [u8; 32] = Sha256::digest(&bytes).into();
        let got = crate::inspect::hex(&digest);
        if got != expected {
            return Err(NOT_PUBLISHED.to_string());
        }
        std::fs::rename(&partial, &installer).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&installer, std::fs::Permissions::from_mode(0o700))
                .map_err(|e| e.to_string())?;
        }
        stage(Stage::Installing);
        let mut cmd = Command::new(&installer);
        cmd.args([
            "-y",
            "--no-modify-path",
            "--profile",
            "minimal",
            "--default-toolchain",
            "stable",
            "--target",
            TARGET,
        ])
        .current_dir(&dir)
        // Another Rust on this computer is fine: this one is Clew's.
        .env("RUSTUP_INIT_SKIP_PATH_CHECK", "yes")
        .env_remove("RUSTUP_TOOLCHAIN")
        .env_remove("RUSTUP_DIST_SERVER")
        .env_remove("RUSTUP_UPDATE_ROOT");
        for (name, value) in managed_env(&dir) {
            cmd.env(name, value);
        }
        run(cmd, INSTALL_DEADLINE, cancel)
    })();
    let _ = std::fs::remove_file(&partial);
    let _ = std::fs::remove_file(&installer);
    // Cancelled or failed partway, what was installed is half a Rust:
    // left, it was reported as Clew's own, broken, in place of the offer
    // to install it.
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&dir);
    }
    result
}

/// Run `cmd` to its end within `limit`, or until `cancel` is set; why it
/// failed, from the end of what it said.
fn run(cmd: Command, limit: Duration, cancel: &AtomicBool) -> Result<(), String> {
    let ran = crate::build::run(cmd, limit, cancel).map_err(|e| match e {
        BuildError::Failed(why) => why,
        other => other.report(),
    })?;
    if ran.success {
        return Ok(());
    }
    let said = ran
        .stderr
        .lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .unwrap_or("it failed")
        .to_string();
    Err(said.strip_prefix("error: ").unwrap_or(&said).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A loopback HTTP server answering `GET <path>` with the body `files`
    /// gives for it, until it is dropped with the test. Its base URL.
    fn serve(files: Vec<(&'static str, Vec<u8>)>) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { break };
                let asked = clew_core::testutil::read_http(&mut conn);
                let body = files
                    .iter()
                    .find(|(path, _)| *path == asked.path())
                    .map(|(_, body)| body.clone());
                let head = match &body {
                    Some(body) => format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    ),
                    None => {
                        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .into()
                    }
                };
                use std::io::Write;
                let _ = conn.write_all(head.as_bytes());
                let _ = conn.write_all(&body.unwrap_or_default());
            }
        });
        base
    }

    /// Install from a fake rustup installer, `script`, served with the
    /// SHA-256 `published` (its own when `None`): what came of it, the
    /// stages it went through, and the data folder.
    #[cfg(unix)]
    fn fake_install(
        tag: &str,
        script: &str,
        published: Option<&str>,
    ) -> (Result<(), String>, Vec<Stage>, clew_core::testutil::TempDir) {
        let data = clew_core::testutil::TempDir::new(tag);
        let installer = script.as_bytes().to_vec();
        let digest: [u8; 32] = Sha256::digest(&installer).into();
        let sha = published
            .map(str::to_string)
            .unwrap_or_else(|| crate::inspect::hex(&digest));
        let base = serve(vec![
            ("/rustup-init", installer),
            (
                "/rustup-init.sha256",
                format!("{sha} *./rustup-init\n").into_bytes(),
            ),
        ]);
        let mut stages = Vec::new();
        let done = install_from(
            data.path(),
            &format!("{base}/rustup-init"),
            &AtomicBool::new(false),
            &mut |s| stages.push(s),
        );
        (done, stages, data)
    }

    /// The installer is run as published: checked, with rustup's own
    /// arguments for a minimal Rust with the target, in Clew's folder and
    /// nowhere else, and not kept.
    #[cfg(unix)]
    #[test]
    fn rust_is_installed_into_clews_own_folder() {
        let script = "#!/bin/sh\n\
            echo \"$@\" > \"$RUSTUP_HOME.args\"\n\
            echo \"$RUSTUP_HOME $CARGO_HOME $RUSTUP_INIT_SKIP_PATH_CHECK\" > \"$RUSTUP_HOME.env\"\n";
        let (done, stages, data) = fake_install("own-rust", script, None);
        done.unwrap();
        assert_eq!(stages, [Stage::Downloading, Stage::Installing]);
        let dir = managed_dir(data.path());
        let args = std::fs::read_to_string(dir.join("rustup.args")).unwrap();
        assert_eq!(
            args.trim(),
            "-y --no-modify-path --profile minimal --default-toolchain stable --target wasm32-wasip2"
        );
        let env = std::fs::read_to_string(dir.join("rustup.env")).unwrap();
        assert_eq!(
            env.trim(),
            format!(
                "{} {} yes",
                dir.join("rustup").display(),
                dir.join("cargo").display()
            )
        );
        assert!(
            !dir.join("rustup-init").exists(),
            "the installer is not kept"
        );
        assert!(!dir.join("rustup-init.part").exists());
    }

    /// An install that fails before anything is downloaded — offline, the
    /// checksum not served — leaves no folder behind: an empty one was read
    /// as Clew's own Rust, broken, and its real reason was lost.
    #[cfg(unix)]
    #[test]
    fn an_install_failing_at_its_checksum_leaves_nothing() {
        let data = clew_core::testutil::TempDir::new("own-rust-offline");
        let base = serve(Vec::new());
        let done = install_from(
            data.path(),
            &format!("{base}/rustup-init"),
            &AtomicBool::new(false),
            &mut |_| {},
        );
        assert!(done.is_err());
        assert!(
            !managed_dir(data.path()).exists(),
            "no folder made for a Rust never installed"
        );
        assert!(!managed_installed(data.path()));
    }

    /// An installer that is not the one published is never run.
    #[cfg(unix)]
    #[test]
    fn an_installer_not_as_published_is_not_run() {
        let script = "#!/bin/sh\ntouch \"$RUSTUP_HOME.ran\"\n";
        let (done, stages, data) = fake_install("own-rust-forged", script, Some(&"0".repeat(64)));
        let why = done.unwrap_err();
        assert!(why.contains("not the one rust-lang.org published"), "{why}");
        assert_eq!(stages, [Stage::Downloading]);
        let dir = managed_dir(data.path());
        assert!(!dir.join("rustup.ran").exists(), "it ran");
        assert!(!dir.join("rustup-init").exists() && !dir.join("rustup-init.part").exists());
    }

    /// An installer that fails says why, in its own last words.
    #[cfg(unix)]
    #[test]
    fn a_failed_install_says_why() {
        // It wrote part of a Rust before failing: none of it is left, to be
        // taken for Clew's own, broken.
        let script = "#!/bin/sh\nmkdir -p \"$RUSTUP_HOME/toolchains\"\necho 'info: downloading' >&2\necho 'error: no space left on device' >&2\nexit 1\n";
        let (done, _, data) = fake_install("own-rust-fails", script, None);
        assert_eq!(done.unwrap_err(), "no space left on device");
        assert!(
            !managed_dir(data.path()).exists(),
            "the half-installed Rust is removed"
        );
    }

    #[test]
    fn the_published_digest_is_its_first_word() {
        let hex = "b1c8cb14e30889e7e2999e6b69e62d9e9b9ffc0f4df8180564d306d9680e6c55";
        assert_eq!(
            published_sha256(&format!("{hex} *./rustup-init\n")).as_deref(),
            Some(hex)
        );
        assert_eq!(
            published_sha256(&format!("{}  rustup-init", hex.to_uppercase())).as_deref(),
            Some(hex)
        );
        assert_eq!(published_sha256("not a digest"), None);
        assert_eq!(published_sha256(""), None);
        assert_eq!(published_sha256(&hex[1..]), None);
    }

    #[test]
    fn the_installer_is_rustups_own_for_this_platform() {
        assert_eq!(
            installer_url("x86_64-unknown-linux-gnu"),
            format!(
                "https://static.rust-lang.org/rustup/dist/x86_64-unknown-linux-gnu/rustup-init{}",
                std::env::consts::EXE_SUFFIX
            )
        );
        if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            assert_eq!(host_triple(), Some("x86_64-unknown-linux-gnu"));
        }
    }

    #[test]
    fn clews_own_rust_is_in_its_plugins_folder() {
        let data = Path::new("/data");
        assert_eq!(managed_dir(data), Path::new("/data/plugins/rust"));
        assert!(managed(data).is_none(), "none installed there");
    }

    #[test]
    fn an_agent_is_told_the_user_sets_it_up_in_clew() {
        let missing = Missing {
            why: Why::NoRust,
            setup: Some(Setup::Install),
            rustup: false,
        };
        let said = missing.into_build_error().report();
        assert!(said.contains("Settings → Plugins"), "{said}");
        assert!(
            said.contains("installs Rust into Clew's own folder"),
            "{said}"
        );
        // No English button name to look for: the user may read Clew in
        // another language.
        assert!(!said.contains("click"), "{said}");
        let missing = Missing {
            why: Why::NoTarget("1.90.0".into()),
            setup: Some(Setup::AddTarget {
                rustup: "/bin/rustup".into(),
            }),
            rustup: true,
        };
        let said = missing.into_build_error().report();
        assert!(said.contains("rustup target add wasm32-wasip2"), "{said}");
    }

    /// One window at a time sets Rust up or removes it: another sees it
    /// held, is refused, and waits until it is let go.
    #[test]
    fn rust_is_set_up_by_one_window_at_a_time() {
        let data = clew_core::testutil::TempDir::new("rust-held");
        assert!(!busy(data.path()), "nothing held yet");
        let held = hold(data.path()).expect("free");
        assert!(busy(data.path()));
        assert!(hold(data.path()).is_none(), "held by the first");
        let cancel = AtomicBool::new(false);
        let refused = set_up(data.path(), &Setup::Install, &cancel, &mut |_| {
            panic!("no step runs while another window holds it")
        });
        assert!(matches!(refused, Err(SetUpError::Elsewhere)), "{refused:?}");
        let let_go = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            drop(held);
        });
        let started = std::time::Instant::now();
        wait_while_busy(data.path());
        assert!(started.elapsed() >= Duration::from_millis(150), "it waited");
        let_go.join().unwrap();
        assert!(!busy(data.path()));
        assert!(hold(data.path()).is_some(), "free again");

        // Held for an instant — as `busy` holds it to look — is not held
        // by another window: the window that asks waits that instant out.
        let probe = std::fs::OpenOptions::new()
            .write(true)
            .open(lock_path(data.path()))
            .unwrap();
        probe.lock().unwrap();
        let let_go = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            probe.unlock().unwrap();
        });
        assert!(
            hold(data.path()).is_some(),
            "an instant's hold is not a window's"
        );
        let_go.join().unwrap();
    }
}
