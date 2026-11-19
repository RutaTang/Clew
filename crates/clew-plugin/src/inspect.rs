//! A plugin's file, read without running any of it: the manifest it carries,
//! the interfaces it imports and exports, and so what it may do.
//!
//! A plugin is one WebAssembly component. Its `plugin.toml` travels inside
//! it, in a custom section named [`SECTION`] that the build appends
//! ([`embed`]), so the one file is the one thing a user approves, shares and
//! Clew hashes.

use sha2::{Digest, Sha256};
use wasmparser::{Parser, Payload, Validator, WasmFeatures};

use crate::manifest::{Manifest, permission_names};

/// The custom section a plugin's manifest is in.
pub const SECTION: &str = "clew:plugin";

/// The largest plugin file Clew reads.
pub const MAX_PLUGIN_BYTES: usize = 32 * 1024 * 1024;

/// The version of `clew:plugin` this Clew provides: a plugin importing or
/// exporting another was built for another Clew.
pub const API_VERSION: &str = "0.1";

/// The interfaces a plugin exports, by what it adds.
pub const COMMANDS_EXPORT: &str = "clew:plugin/commands";
pub const GRAPHS_EXPORT: &str = "clew:plugin/graphs";

/// What a plugin's file says of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inspected {
    pub manifest: Manifest,
    /// The interfaces it imports, as the component names them
    /// (`clew:plugin/project@0.1.0`).
    pub imports: Vec<String>,
    pub exports: Vec<String>,
    /// The permissions its imports need (`clew_rules::plugin` bits): what
    /// it may do, whatever its manifest says.
    pub needs: u8,
    /// SHA-256 of the file.
    pub sha256: [u8; 32],
}

impl Inspected {
    /// The fingerprint an approval records ([`fingerprint`]).
    pub fn fingerprint(&self) -> String {
        fingerprint(&self.sha256)
    }

    /// The permissions it needs, by name.
    pub fn permissions(&self) -> Vec<String> {
        permission_names(self.needs)
    }
}

/// The fingerprint of a plugin file whose SHA-256 is `sha256`: SHA-256 of
/// the length-prefixed encoding (`clew_rules::fingerprint`) of a domain tag
/// and the file's digest. The manifest is in the file, so it covers the
/// code, the name and what the plugin adds alike.
pub fn fingerprint(sha256: &[u8; 32]) -> String {
    let encoded =
        clew_rules::fingerprint::length_prefixed(&[b"clew-plugin/1".to_vec(), sha256.to_vec()]);
    hex(&Sha256::digest(&encoded))
}

/// `bytes` as lowercase hex.
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// The component `component` with `manifest` in its [`SECTION`] section.
/// Refused if it carries one already: there is one manifest per plugin.
pub fn embed(component: &[u8], manifest: &Manifest) -> Result<Vec<u8>, String> {
    if !Parser::is_component(component) {
        return Err("the build did not make a WebAssembly component".into());
    }
    if top_level(component)?.manifests > 0 {
        return Err(format!("the component already has a {SECTION} section"));
    }
    let payload = manifest.to_toml();
    let mut body = Vec::new();
    leb128(&mut body, SECTION.len() as u64);
    body.extend_from_slice(SECTION.as_bytes());
    body.extend_from_slice(payload.as_bytes());
    let mut out = Vec::with_capacity(component.len() + body.len() + 6);
    out.extend_from_slice(component);
    out.push(0); // a custom section
    leb128(&mut out, body.len() as u64);
    out.extend_from_slice(&body);
    Ok(out)
}

fn leb128(out: &mut Vec<u8>, mut n: u64) {
    loop {
        let byte = (n & 0x7f) as u8;
        n >>= 7;
        if n == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// What the top level of a component holds.
struct TopLevel {
    imports: Vec<String>,
    exports: Vec<String>,
    manifests: usize,
    manifest: Option<Vec<u8>>,
}

/// The top level of `bytes`: its imports and exports, and its manifest
/// sections. Nested modules and components are stepped over.
fn top_level(bytes: &[u8]) -> Result<TopLevel, String> {
    let mut top = TopLevel {
        imports: Vec::new(),
        exports: Vec::new(),
        manifests: 0,
        manifest: None,
    };
    let mut depth = 0usize;
    for payload in Parser::new(0).parse_all(bytes) {
        let payload = payload.map_err(|e| format!("the plugin's file is damaged: {e}"))?;
        match payload {
            Payload::ModuleSection { .. } | Payload::ComponentSection { .. } => depth += 1,
            Payload::End(_) => depth = depth.saturating_sub(1),
            Payload::ComponentImportSection(section) if depth == 0 => {
                for import in section {
                    let import =
                        import.map_err(|e| format!("the plugin's file is damaged: {e}"))?;
                    top.imports.push(import.name.name.to_string());
                }
            }
            Payload::ComponentExportSection(section) if depth == 0 => {
                for export in section {
                    let export =
                        export.map_err(|e| format!("the plugin's file is damaged: {e}"))?;
                    top.exports.push(export.name.name.to_string());
                }
            }
            Payload::CustomSection(custom) if depth == 0 && custom.name() == SECTION => {
                top.manifests += 1;
                top.manifest = Some(custom.data().to_vec());
            }
            _ => {}
        }
    }
    Ok(top)
}

/// The name `name` without its version, and the version (`""` if none).
fn split_version(name: &str) -> (&str, &str) {
    name.split_once('@').unwrap_or((name, ""))
}

/// Whether `version` of `clew:plugin` is one this Clew provides.
fn api_supported(version: &str) -> bool {
    version
        .strip_prefix(API_VERSION)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
}

/// Read `bytes` as a plugin: a valid component carrying one manifest, whose
/// imports are all ones Clew provides, whose code needs no permission its
/// manifest leaves out, and which exports what its manifest says it adds.
pub fn inspect(bytes: &[u8]) -> Result<Inspected, String> {
    if bytes.len() > MAX_PLUGIN_BYTES {
        return Err(format!(
            "the plugin's file is {} MB; Clew reads plugins of {} MB at most",
            bytes.len() / (1024 * 1024),
            MAX_PLUGIN_BYTES / (1024 * 1024)
        ));
    }
    if !Parser::is_component(bytes) {
        return Err(
            "this is not a plugin: a plugin is a WebAssembly component (built for wasm32-wasip2)"
                .into(),
        );
    }
    Validator::new_with_features(WasmFeatures::default())
        .validate_all(bytes)
        .map_err(|e| format!("the plugin's file is not valid WebAssembly: {e}"))?;
    let top = top_level(bytes)?;
    let manifest = match (top.manifests, top.manifest) {
        (1, Some(text)) => {
            let text = String::from_utf8(text)
                .map_err(|_| "the plugin's manifest is not UTF-8 text".to_string())?;
            Manifest::parse(&text)?
        }
        (0, _) => {
            return Err(
                "this WebAssembly component is not a Clew plugin: it carries no plugin.toml".into(),
            );
        }
        _ => return Err("the plugin's file carries more than one plugin.toml".into()),
    };

    for name in top.imports.iter().chain(&top.exports) {
        let (bare, version) = split_version(name);
        if bare.starts_with("clew:plugin/") && !api_supported(version) {
            return Err(format!(
                "the plugin was built for another version of Clew's plugin API ({name}); this Clew provides {API_VERSION}"
            ));
        }
    }
    let unknown: Vec<&str> = top
        .imports
        .iter()
        .filter(|i| clew_rules::plugin::need_of(i.as_bytes()) == clew_rules::plugin::UNKNOWN)
        .map(String::as_str)
        .collect();
    if !unknown.is_empty() {
        return Err(format!(
            "the plugin uses what Clew does not give plugins: {}",
            unknown.join(", ")
        ));
    }
    let names: Vec<&[u8]> = top.imports.iter().map(|i| i.as_bytes()).collect();
    let needs = clew_rules::plugin::needs(&names);
    let unstated = needs & !manifest.declared();
    if unstated != 0 {
        return Err(format!(
            "the plugin's code uses {} but its plugin.toml does not list {} in [plugin] permissions",
            describe(unstated),
            permission_names(unstated)
                .iter()
                .map(|p| format!("\"{p}\""))
                .collect::<Vec<_>>()
                .join(" and ")
        ));
    }
    let exports = |wanted: &str| top.exports.iter().any(|e| split_version(e).0 == wanted);
    if !manifest.commands.is_empty() && !exports(COMMANDS_EXPORT) {
        return Err(
            "the plugin lists commands but its code exports none (use clew_plugin_sdk::export!)"
                .into(),
        );
    }
    if !manifest.graphs.is_empty() && !exports(GRAPHS_EXPORT) {
        return Err(
            "the plugin lists graphs but its code exports none (use clew_plugin_sdk::export!)"
                .into(),
        );
    }
    Ok(Inspected {
        manifest,
        imports: top.imports,
        exports: top.exports,
        needs,
        sha256: Sha256::digest(bytes).into(),
    })
}

/// What the permissions `bits` let the code do, for a refusal.
fn describe(bits: u8) -> String {
    let mut what = Vec::new();
    if bits & clew_rules::plugin::READ != 0 {
        what.push("the project's files (clew_plugin_sdk::project)");
    }
    if bits & clew_rules::plugin::GIT != 0 {
        what.push("the project's history (clew_plugin_sdk::git)");
    }
    what.join(" and ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leb128_is_unsigned_little_endian_base_128() {
        let enc = |n| {
            let mut v = Vec::new();
            leb128(&mut v, n);
            v
        };
        assert_eq!(enc(0), [0]);
        assert_eq!(enc(127), [127]);
        assert_eq!(enc(128), [0x80, 1]);
        assert_eq!(enc(624_485), [0xe5, 0x8e, 0x26]);
    }

    #[test]
    fn the_fingerprint_is_the_tagged_digest() {
        let a = fingerprint(&[0; 32]);
        assert_eq!(a.len(), 64);
        assert_ne!(a, fingerprint(&[1; 32]));
        assert_ne!(a, hex(&[0; 32]));
    }

    #[test]
    fn only_api_0_1_is_spoken() {
        assert!(api_supported("0.1.0"));
        assert!(api_supported("0.1.7"));
        assert!(api_supported("0.1"));
        assert!(!api_supported("0.10.0"));
        assert!(!api_supported("0.2.0"));
        assert!(!api_supported(""));
    }

    #[test]
    fn what_is_not_a_component_is_refused() {
        assert!(
            inspect(b"\0asm\x01\0\0\0")
                .unwrap_err()
                .contains("not a plugin")
        );
        assert!(inspect(b"hello").unwrap_err().contains("not a plugin"));
        assert!(embed(b"hello", &crate::manifest::tests_support::manifest()).is_err());
    }
}
