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
//! Explicit `init_options` from `lsp.toml` always win over what we auto-detect.
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
pub fn merge(language: &str, server: &str, root: &Path, explicit: Option<Value>) -> Option<Value> {
    match (detect(language, server, root), explicit) {
        (Some(mut auto), Some(explicit)) => {
            deep_merge(&mut auto, explicit);
            Some(auto)
        }
        (auto, explicit) => auto.or(explicit),
    }
}

/// Locate a project-local Python interpreter (a virtualenv), or `None` to let
/// the server fall back to its own discovery.
pub fn find_python_interpreter(root: &Path) -> Option<PathBuf> {
    const DIRS: [&str; 4] = [".venv", "venv", ".env", "env"];
    const EXES: [&str; 3] = ["bin/python", "bin/python3", "Scripts/python.exe"];
    // Project-local virtualenvs, most-conventional first.
    for dir in DIRS {
        for exe in EXES {
            let cand = root.join(dir).join(exe);
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    // An already-activated virtualenv in the environment.
    if let Some(venv) = std::env::var_os("VIRTUAL_ENV") {
        for exe in EXES {
            let cand = Path::new(&venv).join(exe);
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
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

    fn scratch(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("clew-langenv-{tag}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn finds_dot_venv_interpreter() {
        let root = scratch("venv");
        let bin = root.join(".venv/bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("python"), "").unwrap();
        let found = find_python_interpreter(&root).unwrap();
        assert!(found.ends_with(".venv/bin/python"), "{found:?}");
    }

    #[test]
    fn no_interpreter_no_settings() {
        let root = scratch("empty");
        assert!(detect("python", "pyright", &root).is_none());
    }

    #[test]
    fn detect_produces_python_path_keys() {
        let root = scratch("keys");
        let bin = root.join("venv/bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("python3"), "").unwrap();
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
