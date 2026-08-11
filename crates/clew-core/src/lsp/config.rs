//! Per-project LSP configuration (`<root>/.clew/lsp.toml`) and resolution of
//! the effective server to use for a language.
//!
//! Precedence: the built-in [`registry`](super::registry) defaults, overridden
//! by the project's `lsp.toml`. The file is committable so a team shares one
//! reproducible LSP setup.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::registry;

/// Parsed `.clew/lsp.toml`: a table of per-language overrides.
///
/// ```toml
/// [rust]
/// server = "rust-analyzer"
/// version = "2026-07-13"
/// enabled = true
/// command = "/usr/local/bin/rust-analyzer"  # escape hatch, bypasses the store
///
/// [rust.init_options]
/// "rust-analyzer.check.command" = "clippy"
/// ```
#[derive(Debug, Default, Deserialize)]
#[serde(transparent)]
pub struct ProjectLspConfig {
    languages: HashMap<String, LangOverride>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LangOverride {
    pub server: Option<String>,
    pub version: Option<String>,
    pub enabled: Option<bool>,
    /// Custom executable path; when set clew runs it directly (no store).
    pub command: Option<String>,
    /// Options passed through to the server's `initialize` request. Opaque to
    /// clew, but NOT harmless: this file ships with the repository, and
    /// servers read these as a place to name programs they then run
    /// (rust-analyzer's `cargo.buildScripts.overrideCommand`, pyright's
    /// `python.pythonPath`). Every path that sends them gates them on the
    /// user's approval first — `App::approved_init_options` in the client,
    /// `clew_server::approved_init_options` on the server. Resolving them
    /// here is not consent to send them.
    pub init_options: Option<toml::Value>,
}

/// Byte cap for `lsp.toml`. A real one is a few hundred bytes of per-language
/// overrides; anything near this is not a config someone wrote by hand.
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

impl ProjectLspConfig {
    /// Load `<root>/.clew/lsp.toml`. Missing file → empty config (defaults).
    /// A malformed file is surfaced as an error rather than silently ignored.
    /// The file ships with the repository: refuse a symlink or an outsized
    /// file outright (a config is a few hundred bytes; a link to `/dev/zero`
    /// must not hang the load).
    pub fn load(root: &Path) -> Result<Self, String> {
        let path = root.join(".clew").join("lsp.toml");
        // The `.clew` directory chain must be real directories: with a
        // repo-shipped `.clew -> /outside`, this config — whose `command`
        // decides what gets executed — would be read from someone else's tree.
        // Checked here, ahead of the read, so this case cannot reach the
        // classification below, where a link pointing at nothing would look
        // like "no config" and silently resolve to defaults.
        if !crate::statefile::repo_dirs_are_real(&path) {
            return Err("lsp.toml: a .clew directory is a symlink — refusing to read it".into());
        }
        // Read through `statefile`, where the `.clew` rules live, rather than
        // spelling them again here: it opens the leaf ONCE with `O_NOFOLLOW |
        // O_NONBLOCK`, type-checks that open handle, and enforces the cap on
        // the read. The spelling this replaced — check the path, stat the
        // path, then `read_to_string` the path — resolved the name three
        // times, so a file swapped for a FIFO after the stat blocked the load
        // forever (this runs on the iced update thread and on the server's
        // blocking pool), and one that grew after the stat was read whole.
        let Some(text) = crate::statefile::read_capped(&path, MAX_CONFIG_BYTES) else {
            // Reached only when there is nothing to parse, so say WHICH
            // nothing. "Missing" is the ordinary case and means defaults;
            // every other reason is an error the user must see, because a
            // refused config that quietly became defaults would run a
            // different server than the project pins.
            return match std::fs::symlink_metadata(&path) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
                Err(e) => Err(format!("lsp.toml: {e}")),
                Ok(meta) if !meta.is_file() => Err("lsp.toml: not a regular file".into()),
                Ok(meta) if meta.len() > MAX_CONFIG_BYTES => {
                    Err("lsp.toml: unreasonably large".into())
                }
                Ok(_) => Err("lsp.toml: unreadable".into()),
            };
        };
        toml::from_str(&text).map_err(|e| format!("lsp.toml: {e}"))
    }

    /// Resolve the effective server for `language`, applying overrides on top
    /// of the built-in default. Returns `None` when no server applies (unknown
    /// language with no override, or explicitly disabled).
    pub fn resolve(&self, language: &str) -> Option<EffectiveServer> {
        let over = self.languages.get(language);

        if let Some(o) = over
            && o.enabled == Some(false)
        {
            return None;
        }

        // Which server: explicit override, else the language default.
        let server_name = over
            .and_then(|o| o.server.clone())
            .or_else(|| registry::default_for_language(language).map(|s| s.name.to_string()))?;

        let spec = registry::by_name(&server_name);
        let version = over
            .and_then(|o| o.version.clone())
            .or_else(|| spec.as_ref().map(|s| s.version.to_string()))
            .unwrap_or_default();
        let args = spec
            .as_ref()
            .map(|s| s.args.iter().map(|a| a.to_string()).collect())
            .unwrap_or_default();

        let command = over.and_then(|o| o.command.as_ref()).map(PathBuf::from);

        let init_options = over.and_then(|o| o.init_options.clone()).map(toml_to_json);

        Some(EffectiveServer {
            language: language.to_string(),
            server_name,
            version,
            args,
            command,
            init_options,
        })
    }
}

/// The concrete server to launch for a language after resolving overrides.
#[derive(Debug, Clone, PartialEq)]
pub struct EffectiveServer {
    pub language: String,
    pub server_name: String,
    pub version: String,
    pub args: Vec<String>,
    /// When set, run this binary directly and skip the managed store.
    pub command: Option<PathBuf>,
    pub init_options: Option<serde_json::Value>,
}

/// How deeply `init_options` may nest. The file is the repository's own
/// `.clew/lsp.toml`, so its shape is attacker-chosen; past this depth the
/// value is replaced with null rather than recursed into.
const MAX_INIT_OPTIONS_DEPTH: usize = 64;

/// Convert a parsed TOML value into JSON for the LSP `initialize` payload.
fn toml_to_json(value: toml::Value) -> serde_json::Value {
    toml_to_json_at(value, 0)
}

fn toml_to_json_at(value: toml::Value, depth: usize) -> serde_json::Value {
    use serde_json::Value as J;
    use toml::Value as T;
    if depth > MAX_INIT_OPTIONS_DEPTH {
        return J::Null;
    }
    match value {
        T::String(s) => J::String(s),
        T::Integer(i) => J::Number(i.into()),
        T::Float(f) => serde_json::Number::from_f64(f)
            .map(J::Number)
            .unwrap_or(J::Null),
        T::Boolean(b) => J::Bool(b),
        T::Datetime(d) => J::String(d.to_string()),
        T::Array(a) => J::Array(
            a.into_iter()
                .map(|v| toml_to_json_at(v, depth + 1))
                .collect(),
        ),
        T::Table(t) => J::Object(
            t.into_iter()
                .map(|(k, v)| (k, toml_to_json_at(v, depth + 1)))
                .collect(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml: &str) -> ProjectLspConfig {
        toml::from_str(toml).unwrap()
    }

    #[test]
    fn empty_config_uses_registry_default() {
        let cfg = ProjectLspConfig::default();
        let eff = cfg.resolve("rust").unwrap();
        assert_eq!(eff.server_name, "rust-analyzer");
        assert_eq!(eff.version, "2026-07-13");
        assert!(eff.command.is_none());
        assert!(eff.init_options.is_none());
        assert!(cfg.resolve("cobol").is_none());
    }

    #[test]
    fn disabled_language_resolves_to_none() {
        let cfg = parse("[rust]\nenabled = false\n");
        assert!(cfg.resolve("rust").is_none());
    }

    #[test]
    fn version_override_and_init_options() {
        let cfg = parse(
            "[rust]\nversion = \"2099-01-01\"\n\n[rust.init_options]\n\"rust-analyzer.check.command\" = \"clippy\"\n",
        );
        let eff = cfg.resolve("rust").unwrap();
        assert_eq!(eff.version, "2099-01-01");
        let opts = eff.init_options.unwrap();
        assert_eq!(opts["rust-analyzer.check.command"], "clippy");
    }

    #[test]
    fn custom_command_escape_hatch() {
        let cfg = parse("[rust]\ncommand = \"/opt/ra\"\n");
        let eff = cfg.resolve("rust").unwrap();
        assert_eq!(eff.command, Some(PathBuf::from("/opt/ra")));
    }

    #[test]
    fn override_server_for_a_new_language() {
        // A language clew has no default for, pointed at a custom command.
        let cfg =
            parse("[python]\ncommand = \"/usr/bin/pyright-langserver\"\nserver = \"pyright\"\n");
        let eff = cfg.resolve("python").unwrap();
        assert_eq!(eff.server_name, "pyright");
        assert_eq!(
            eff.command,
            Some(PathBuf::from("/usr/bin/pyright-langserver"))
        );
    }

    #[test]
    fn missing_file_is_default_but_bad_toml_errors() {
        let dir = std::env::temp_dir().join("clew-lsp-config-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".clew")).unwrap();
        // No file yet → defaults.
        assert!(
            ProjectLspConfig::load(&dir)
                .unwrap()
                .resolve("rust")
                .is_some()
        );
        // Malformed → error.
        std::fs::write(dir.join(".clew/lsp.toml"), "this is not = = toml").unwrap();
        assert!(ProjectLspConfig::load(&dir).is_err());
    }

    /// `lsp.toml` is repository-controlled and decides what clew EXECUTES, so
    /// every way of not being a plain readable file must surface as an error.
    /// The one outcome that must never happen is a refusal turning into
    /// `Ok(default)`, which would silently run the registry's server in place
    /// of the one the project pinned — hence the assertions on `is_err`, not
    /// merely on "did not panic".
    #[test]
    #[cfg(unix)]
    fn a_repo_planted_lsp_toml_is_refused_rather_than_read_or_defaulted() {
        let base = std::env::temp_dir().join("clew-lsp-config-hostile");
        let _ = std::fs::remove_dir_all(&base);
        let make = |name: &str| {
            let d = base.join(name);
            std::fs::create_dir_all(d.join(".clew")).unwrap();
            d
        };

        // A symlink at the leaf, even to a perfectly valid config: the file is
        // opened with O_NOFOLLOW, so this is a refusal and not a read of
        // whatever the link names.
        let elsewhere = base.join("elsewhere.toml");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(&elsewhere, "[rust]\ncommand = \"/opt/evil\"\n").unwrap();
        let d = make("leaf-link");
        std::os::unix::fs::symlink(&elsewhere, d.join(".clew/lsp.toml")).unwrap();
        assert!(ProjectLspConfig::load(&d).is_err());

        // A symlinked `.clew` must be an error, NOT the missing-file default.
        // The directory check has to run before the read for that: the
        // classification below it stats through the link, where a name that
        // does not exist on the other side reads as "no config".
        let d = base.join("dir-link");
        std::fs::create_dir_all(&d).unwrap();
        let outside = base.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, d.join(".clew")).unwrap();
        assert!(
            ProjectLspConfig::load(&d).is_err(),
            "a linked .clew must refuse, not fall back to defaults"
        );

        // A FIFO must be refused PROMPTLY. `load` runs on the iced update
        // thread (src/app/services.rs) and on the server's blocking pool; an
        // open that waits for a writer freezes the window.
        let d = make("fifo");
        let fifo = d.join(".clew/lsp.toml");
        let c = std::ffi::CString::new(fifo.to_string_lossy().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(ProjectLspConfig::load(&d).is_err());
        });
        assert_eq!(
            rx.recv_timeout(std::time::Duration::from_secs(5)),
            Ok(true),
            "a FIFO at lsp.toml must not block the load"
        );

        // Oversized is refused; exactly at the cap still loads (the cap is
        // enforced on the read, so the boundary must not false-trip).
        let d = make("oversize");
        let filler = "# padding\n".repeat(2);
        let body = "[rust]\nversion = \"1\"\n";
        let mut at_cap = body.to_string();
        at_cap.push_str(&"#".repeat(MAX_CONFIG_BYTES as usize - body.len() - 1));
        at_cap.push('\n');
        assert_eq!(at_cap.len() as u64, MAX_CONFIG_BYTES);
        std::fs::write(d.join(".clew/lsp.toml"), &at_cap).unwrap();
        assert_eq!(
            ProjectLspConfig::load(&d)
                .unwrap()
                .resolve("rust")
                .unwrap()
                .version,
            "1"
        );
        std::fs::write(d.join(".clew/lsp.toml"), at_cap + &filler).unwrap();
        assert!(ProjectLspConfig::load(&d).is_err());
    }
}
