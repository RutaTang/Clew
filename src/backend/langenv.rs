//! Auto-detected per-project language environment, merged into the LSP
//! `initializationOptions` at launch.
//!
//! Language servers need to know the project's environment to be accurate. The
//! needs cluster three ways (see the design notes):
//!   A. Toolchain the server can't find itself — Python venv, Java JDK, Zig
//!      compiler. clew locates it and passes it through. This module does (A).
//!   B. Build config that selects which code is active — Rust cargo
//!      features/target, Go build tags, C/C++ defines. These are user *choices*,
//!      not auto-detectable, so they come from `.clew/lsp.toml` init_options
//!      (and, later, a picker), and also drive the inactive-`cfg` reading aid.
//!   C. Compilation database / project file the server locates itself — clangd's
//!      `compile_commands.json`, tsserver's `tsconfig.json`. Nothing to do.
//!
//! Explicit `init_options` from `lsp.toml` always win over what we auto-detect
//! — which is exactly why they carry the user's approval before they get here
//! (see [`merge`]): a repository value that wins over a vetted one is not a
//! preference, it is an override of this module's trust boundary.
//!
//! (A) reaches INTO the project for a path the server will then EXECUTE, so it
//! sits on the same trust boundary as an `lsp.toml` `command` — see
//! [`runs_foreign_bytes`] for where that boundary is drawn and what it does
//! and does not cover. Every future (A) case (a JDK, a Zig compiler) picks an
//! executable out of the project the same way and must go through that same
//! gate; the Python venv is only the first one.
//!
//! Caveat: some servers (pyright) read these settings by *pulling*
//! `workspace/configuration` rather than from `initializationOptions`. Making
//! that path take effect needs the client to answer that pull — tracked
//! separately; this module already computes the settings either way.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

/// The auto-detected environment settings for `language`'s server in `root`, or
/// `None` when there is nothing to add.
pub fn detect(language: &str, _server: &str, root: &Path) -> Option<Value> {
    match language {
        "python" => {
            let py = find_python_interpreter(root)?;
            let path = py.to_string_lossy().to_string();
            // Both keys, so pyright (defaultInterpreterPath) and pylsp
            // (pythonPath) each pick it up.
            Some(json!({
                "python": {
                    "pythonPath": path,
                    "defaultInterpreterPath": path,
                }
            }))
        }
        _ => None,
    }
}

/// Merge auto-detected settings under the explicit `lsp.toml` `init_options`,
/// with explicit values winning on conflict.
///
/// `explicit` must already have passed the approval gate — `App::approved_
/// init_options` on the client, `clew_server::approved_init_options` on the
/// server. Nothing here can re-check it, and the merge is precisely where an
/// ungated value does its damage: a repository's `python.pythonPath` lands ON
/// TOP of the interpreter [`find_python_interpreter`] vetted, so it does not
/// merely bypass [`runs_foreign_bytes`], it overrules it.
pub fn merge(language: &str, server: &str, root: &Path, explicit: Option<Value>) -> Option<Value> {
    match (detect(language, server, root), explicit) {
        (Some(mut auto), Some(explicit)) => {
            deep_merge(&mut auto, explicit);
            Some(auto)
        }
        (auto, explicit) => auto.or(explicit),
    }
}

/// Byte cap for reading an interpreter to compare it against its base. A
/// CPython binary is single-digit megabytes; the cap is what stops a candidate
/// that the repository chose from being read without limit.
const MAX_INTERPRETER_BYTES: u64 = 64 * 1024 * 1024;

/// `pyvenv.cfg` is a handful of `key = value` lines, and it ships with the
/// repository, so its size is not trusted either.
const MAX_PYVENV_CFG_BYTES: u64 = 64 * 1024;

/// Locate a project-local Python interpreter (a virtualenv), or `None` to let
/// the server fall back to its own discovery.
///
/// The path returned is handed to the language server as `python.pythonPath` /
/// `defaultInterpreterPath`, and the Python server clew provisions (pyright)
/// RUNS it to enumerate `sys.path`. The search looks inside the project, so
/// every candidate here is a file that ships with the repository — the same
/// class of input as an `lsp.toml` `command`, which may only run after the
/// user has approved its fingerprint. Nothing on this path can ask for that
/// approval (it runs on every start, for a venv the user never chose *as a
/// clew setting*), so the boundary is drawn at the bytes instead:
/// [`runs_foreign_bytes`] admits a candidate only when executing it cannot
/// execute code the repository supplied.
pub fn find_python_interpreter(root: &Path) -> Option<PathBuf> {
    const DIRS: [&str; 4] = [".venv", "venv", ".env", "env"];
    const EXES: [&str; 3] = ["bin/python", "bin/python3", "Scripts/python.exe"];
    // Containment is only decidable against the real root; without it there is
    // no way to tell a repository file from a system one, so detect nothing.
    let real_root = std::fs::canonicalize(root).ok()?;
    // Project-local virtualenvs, most-conventional first.
    for dir in DIRS {
        for exe in EXES {
            let cand = root.join(dir).join(exe);
            if cand.is_file() && runs_foreign_bytes(&real_root, &cand) {
                return Some(cand);
            }
        }
    }
    // An already-activated virtualenv in the environment. Held to the SAME
    // check: `VIRTUAL_ENV` routinely names the project's own `.venv`, and one
    // file must not get two different answers depending on which name reached
    // it.
    if let Some(venv) = std::env::var_os("VIRTUAL_ENV") {
        for exe in EXES {
            let cand = Path::new(&venv).join(exe);
            if cand.is_file() && runs_foreign_bytes(&real_root, &cand) {
                return Some(cand);
            }
        }
    }
    None
}

/// Would executing `cand` run bytes that came from outside `real_root` (which
/// must already be canonical)?
///
/// Two shapes pass:
///   * The real file resolves outside the project. `python -m venv` links
///     `bin/python` at the base interpreter, so the ordinary case lands here
///     and costs neither a prompt nor any accuracy.
///   * The real file is inside the project but is byte-identical to the base
///     interpreter named by the venv's own `pyvenv.cfg`, whose `home` must
///     itself resolve outside the project. A `--copies` venv, and every
///     Windows venv, is a real copy, so without this arm they would be refused
///     for being exactly what they are.
///
/// What this prevents: a repository shipping `.venv/bin/python` as its own
/// script or binary and having clew nominate it for execution. Repo-authored
/// content fails both arms — the second compares against a file the repo does
/// not control, so a planted `pyvenv.cfg` buys nothing.
///
/// What it does NOT prevent, stated plainly rather than implied away:
///   * A repo-planted symlink to an already-installed system executable. The
///     bytes are the system's and the argv is the language server's, so the
///     repository chooses only *which* installed program runs.
///   * The window between this check and the server's own `execve`. Unlike an
///     `lsp.toml` command, this value cannot be pinned by copying the approved
///     bytes aside: its whole purpose is to be a path *inside* the venv, which
///     is how the server finds `site-packages`. Winning that race needs a live
///     process in the project, which is the thing this check exists to stop.
///   * A real in-project interpreter with no usable `pyvenv.cfg` (`conda
///     create -p ./env`) is refused, not executed — the server falls back to
///     its own discovery. That costs accuracy, not safety.
fn runs_foreign_bytes(real_root: &Path, cand: &Path) -> bool {
    let Ok(real) = std::fs::canonicalize(cand) else {
        return false;
    };
    if !real.starts_with(real_root) {
        return true;
    }
    copies_the_base_interpreter(real_root, cand, &real)
}

/// The `--copies` arm: is the in-project `cand` a byte-for-byte copy of the
/// base interpreter its `pyvenv.cfg` points at, with that base outside the
/// project? Then executing it and executing the base are the same act.
fn copies_the_base_interpreter(real_root: &Path, cand: &Path, real_cand: &Path) -> bool {
    // `<venv>/bin/python` -> `<venv>`; `<venv>/Scripts/python.exe` is the same
    // shape.
    let Some(venv) = cand.parent().and_then(Path::parent) else {
        return false;
    };
    // Repository-controlled file: read it capped, and through the state-file
    // opener that refuses a symlink or a FIFO at the leaf.
    let Some(cfg) =
        clew_core::statefile::read_capped(&venv.join("pyvenv.cfg"), MAX_PYVENV_CFG_BYTES)
    else {
        return false;
    };
    let mut home = None;
    let mut version = None;
    for line in cfg.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "home" => home = Some(value.trim()),
            // Both spellings occur across CPython releases.
            "version" | "version_info" => version = Some(value.trim()),
            _ => {}
        }
    }
    let Some(home) = home else {
        return false;
    };
    let Ok(home) = std::fs::canonicalize(home) else {
        return false;
    };
    // A `home` pointing back into the project proves nothing: the repository
    // would be vouching for itself.
    if home.starts_with(real_root) {
        return false;
    }
    // `bin/python` is usually a copy of `pythonX.Y`, so the candidate's own
    // name is only one of the possibilities for the base's name.
    let mut names = Vec::new();
    if let Some(name) = cand.file_name().and_then(|n| n.to_str()) {
        names.push(name.to_string());
    }
    if let Some((major, minor)) = version.and_then(|v| {
        let mut parts = v.split('.');
        Some((parts.next()?, parts.next()?))
    }) {
        names.push(format!("python{major}.{minor}"));
    }
    names.push("python3".to_string());
    names.push("python".to_string());
    for name in names {
        let Ok(base) = std::fs::canonicalize(home.join(&name)) else {
            continue;
        };
        // The base must be outside too, or a symlink out of `home` back into
        // the project would launder the repository's own bytes.
        if base.starts_with(real_root) {
            continue;
        }
        if same_bytes(real_cand, &base) {
            return true;
        }
    }
    false
}

/// Do two files hold exactly the same bytes? Bounded on every axis a
/// repository controls: both are opened as plain files (never through a
/// symlink, never blocking on a FIFO), unequal or oversized lengths lose
/// before a byte is read, and the loop stops at [`MAX_INTERPRETER_BYTES`].
fn same_bytes(a: &Path, b: &Path) -> bool {
    let (Some(mut fa), Some(mut fb)) = (
        clew_core::statefile::open_plain(a),
        clew_core::statefile::open_plain(b),
    ) else {
        return false;
    };
    let (Ok(ma), Ok(mb)) = (fa.metadata(), fb.metadata()) else {
        return false;
    };
    // An empty "interpreter" matches every other empty file, which would make
    // a zero-byte repo file pass against a zero-byte anything.
    if ma.len() == 0 || ma.len() != mb.len() || ma.len() > MAX_INTERPRETER_BYTES {
        return false;
    }
    let mut buf_a = vec![0u8; 64 * 1024];
    let mut buf_b = vec![0u8; 64 * 1024];
    let mut total: u64 = 0;
    loop {
        let (Some(na), Some(nb)) = (fill(&mut fa, &mut buf_a), fill(&mut fb, &mut buf_b)) else {
            return false;
        };
        if na != nb || buf_a[..na] != buf_b[..nb] {
            return false;
        }
        if na == 0 {
            return true;
        }
        // Re-checked while reading: the size above races with a file that is
        // still growing, and an unbounded loop is the one failure mode this
        // must never have.
        total += na as u64;
        if total > MAX_INTERPRETER_BYTES {
            return false;
        }
    }
}

/// Read until `buf` is full or the file ends, so the two comparisons always
/// line up on the same offsets — a short read on one side must not be mistaken
/// for a difference.
fn fill(f: &mut std::fs::File, buf: &mut [u8]) -> Option<usize> {
    use std::io::Read;
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
    Some(n)
}

/// Recursively merge `over` into `base`; `over`'s values win on conflict.
fn deep_merge(base: &mut Value, over: Value) {
    match (base, over) {
        (Value::Object(b), Value::Object(o)) => {
            for (k, v) in o {
                match b.get_mut(&k) {
                    Some(existing) => deep_merge(existing, v),
                    None => {
                        b.insert(k, v);
                    }
                }
            }
        }
        (b, o) => *b = o,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bytes standing in for a CPython binary. Never empty: an empty file
    /// would compare equal to every other empty file.
    const BASE_BYTES: &[u8] = b"\x7fELF not really python, but a distinctive body";

    fn scratch(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("clew-langenv-{tag}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    /// A "system Python" outside any project root, returning its `bin`
    /// directory (what `pyvenv.cfg`'s `home` names).
    fn system_python(tag: &str, bytes: &[u8]) -> PathBuf {
        let bin = scratch(tag).join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("python3.12"), bytes).unwrap();
        bin
    }

    /// What the detector picks *inside* `root`. An ambient `VIRTUAL_ENV` on
    /// the developer's machine is a real hit but not the one under test, so
    /// it is filtered out rather than allowed to decide these assertions.
    fn project_local(root: &Path) -> Option<PathBuf> {
        find_python_interpreter(root).filter(|p| p.starts_with(root))
    }

    fn venv_bin(root: &Path, dir: &str) -> PathBuf {
        let bin = root.join(dir).join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        bin
    }

    /// The ordinary case: `python -m venv` links `bin/python` at the base
    /// interpreter, so the real bytes live outside the project and the
    /// interpreter is detected with no prompt and no loss of accuracy.
    #[test]
    #[cfg(unix)]
    fn finds_symlinked_venv_interpreter() {
        let root = scratch("venv");
        let base = system_python("venv-base", BASE_BYTES);
        let bin = venv_bin(&root, ".venv");
        std::os::unix::fs::symlink(base.join("python3.12"), bin.join("python")).unwrap();
        let found = project_local(&root).unwrap();
        assert!(found.ends_with(".venv/bin/python"), "{found:?}");
    }

    /// …and the same claim against a venv that `python -m venv` really built,
    /// so "a normal project still resolves, with no new prompt" does not rest
    /// on a hand-made fixture. Skipped where python3 is unavailable or refuses
    /// to build one (some builds cannot without symlinks).
    #[test]
    fn finds_a_real_python_venv() {
        let root = scratch("realvenv");
        let built = std::process::Command::new("python3")
            .args(["-m", "venv", ".venv"])
            .current_dir(&root)
            .output();
        if !matches!(&built, Ok(o) if o.status.success()) {
            return;
        }
        let found = project_local(&root).expect("a real venv must still resolve");
        assert!(found.starts_with(root.join(".venv")), "{found:?}");
    }

    /// R11-07: a repository can ship `.venv/bin/python` as its own file, and
    /// pyright RUNS the interpreter it is handed. A real in-project file that
    /// is nobody's copy is refused, so nothing nominates it for execution.
    #[test]
    fn rejects_repo_shipped_interpreter() {
        let root = scratch("hostile");
        let bin = venv_bin(&root, ".venv");
        std::fs::write(bin.join("python"), "#!/bin/sh\ntouch /tmp/pwned\n").unwrap();
        assert_eq!(project_local(&root), None);
        assert!(detect("python", "pyright", &root).is_none());
    }

    /// A `--copies` venv (and every Windows venv) holds a real copy of the
    /// base interpreter. It must still resolve, or the check would refuse a
    /// legitimate environment for being what it is.
    #[test]
    fn accepts_copies_venv_matching_its_base() {
        let root = scratch("copies");
        let base = system_python("copies-base", BASE_BYTES);
        let bin = venv_bin(&root, ".venv");
        std::fs::write(bin.join("python"), BASE_BYTES).unwrap();
        std::fs::write(
            root.join(".venv/pyvenv.cfg"),
            format!("home = {}\nversion = 3.12.1\n", base.display()),
        )
        .unwrap();
        let found = project_local(&root).unwrap();
        assert!(found.ends_with(".venv/bin/python"), "{found:?}");
    }

    /// The copy arm compares BYTES, so a `pyvenv.cfg` that points at a real
    /// system Python does not vouch for a payload sitting next to it.
    #[test]
    fn rejects_copies_venv_that_differs_from_its_base() {
        let root = scratch("forged");
        let base = system_python("forged-base", BASE_BYTES);
        let bin = venv_bin(&root, ".venv");
        std::fs::write(bin.join("python"), b"#!/bin/sh\ntouch /tmp/pwned\n").unwrap();
        std::fs::write(
            root.join(".venv/pyvenv.cfg"),
            format!("home = {}\nversion = 3.12.1\n", base.display()),
        )
        .unwrap();
        assert_eq!(project_local(&root), None);
    }

    /// …and a `home` pointing back into the project is the repository
    /// vouching for itself, byte-equal or not.
    #[test]
    fn rejects_pyvenv_cfg_home_inside_the_project() {
        let root = scratch("selfhome");
        let fake_base = root.join("tools");
        std::fs::create_dir_all(&fake_base).unwrap();
        std::fs::write(fake_base.join("python3.12"), BASE_BYTES).unwrap();
        let bin = venv_bin(&root, ".venv");
        std::fs::write(bin.join("python"), BASE_BYTES).unwrap();
        std::fs::write(
            root.join(".venv/pyvenv.cfg"),
            format!("home = {}\nversion = 3.12.1\n", fake_base.display()),
        )
        .unwrap();
        assert_eq!(project_local(&root), None);
    }

    /// Zero bytes equal zero bytes: without the length guard an empty repo
    /// file would "match" any empty file in a plausible `home`.
    #[test]
    fn rejects_empty_interpreter_matching_empty_base() {
        let root = scratch("empties");
        let base = system_python("empties-base", b"");
        let bin = venv_bin(&root, ".venv");
        std::fs::write(bin.join("python"), b"").unwrap();
        std::fs::write(
            root.join(".venv/pyvenv.cfg"),
            format!("home = {}\nversion = 3.12.1\n", base.display()),
        )
        .unwrap();
        assert_eq!(project_local(&root), None);
    }

    #[test]
    fn no_interpreter_no_settings() {
        let root = scratch("empty");
        assert_eq!(project_local(&root), None);
        // With no activated venv in the environment either, there is nothing
        // for `detect` to add at all.
        if std::env::var_os("VIRTUAL_ENV").is_none() {
            assert!(detect("python", "pyright", &root).is_none());
        }
    }

    #[test]
    #[cfg(unix)]
    fn detect_produces_python_path_keys() {
        let root = scratch("keys");
        let base = system_python("keys-base", BASE_BYTES);
        let bin = venv_bin(&root, "venv");
        std::os::unix::fs::symlink(base.join("python3.12"), bin.join("python3")).unwrap();
        let opts = detect("python", "pyright", &root).unwrap();
        assert!(
            opts["python"]["pythonPath"]
                .as_str()
                .unwrap()
                .ends_with("venv/bin/python3")
        );
        assert!(opts["python"]["defaultInterpreterPath"].is_string());
    }

    #[test]
    fn explicit_init_options_win_over_auto() {
        let auto = json!({ "python": { "pythonPath": "/auto", "analysis": { "level": "basic" } } });
        let mut merged = auto;
        deep_merge(
            &mut merged,
            json!({ "python": { "pythonPath": "/explicit" } }),
        );
        // Explicit overrides the conflicting key…
        assert_eq!(merged["python"]["pythonPath"], "/explicit");
        // …while non-conflicting auto keys survive.
        assert_eq!(merged["python"]["analysis"]["level"], "basic");
    }
}
