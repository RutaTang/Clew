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
    /// Opaque options passed through to the server's `initialize` request.
    pub init_options: Option<toml::Value>,
}

impl ProjectLspConfig {
    /// Load `<root>/.clew/lsp.toml`. Missing file → empty config (defaults).
    /// A malformed file is surfaced as an error rather than silently ignored.
    pub fn load(root: &Path) -> Result<Self, String> {
        let path = root.join(".clew").join("lsp.toml");
        match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).map_err(|e| format!("lsp.toml: {e}")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("lsp.toml: {e}")),
        }
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

        let command = over
            .and_then(|o| o.command.as_ref())
            .map(PathBuf::from);

        let init_options = over
            .and_then(|o| o.init_options.clone())
            .map(toml_to_json);

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

/// Convert a parsed TOML value into JSON for the LSP `initialize` payload.
fn toml_to_json(value: toml::Value) -> serde_json::Value {
    use serde_json::Value as J;
    use toml::Value as T;
    match value {
        T::String(s) => J::String(s),
        T::Integer(i) => J::Number(i.into()),
        T::Float(f) => serde_json::Number::from_f64(f).map(J::Number).unwrap_or(J::Null),
        T::Boolean(b) => J::Bool(b),
        T::Datetime(d) => J::String(d.to_string()),
        T::Array(a) => J::Array(a.into_iter().map(toml_to_json).collect()),
        T::Table(t) => J::Object(t.into_iter().map(|(k, v)| (k, toml_to_json(v))).collect()),
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
        let cfg = parse("[python]\ncommand = \"/usr/bin/pyright-langserver\"\nserver = \"pyright\"\n");
        let eff = cfg.resolve("python").unwrap();
        assert_eq!(eff.server_name, "pyright");
        assert_eq!(eff.command, Some(PathBuf::from("/usr/bin/pyright-langserver")));
    }

    #[test]
    fn missing_file_is_default_but_bad_toml_errors() {
        let dir = std::env::temp_dir().join("clew-lsp-config-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".clew")).unwrap();
        // No file yet → defaults.
        assert!(ProjectLspConfig::load(&dir).unwrap().resolve("rust").is_some());
        // Malformed → error.
        std::fs::write(dir.join(".clew/lsp.toml"), "this is not = = toml").unwrap();
        assert!(ProjectLspConfig::load(&dir).is_err());
    }
}
