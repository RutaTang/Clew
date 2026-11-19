//! What a plugin may be linked with: `clew_plugin::inspect` reads the names
//! of the interfaces a plugin's component imports, and asks [`needs`] which
//! permissions they take and [`may_link`] whether those are granted.
//!
//! A plugin's permissions are not what it says of itself but what its code
//! imports: an interface it never calls is left out of the component, and
//! one it is not granted is not in the linker it is instantiated with, so
//! there is nothing to call. An import Clew does not know refuses the
//! plugin outright, rather than being let through as harmless.
//!
//! Permissions are bits of a `u8`: [`READ`] and [`GIT`]. [`UNKNOWN`] is
//! never granted.
//!
//! The names a plugin's things go by on disk are decided here too, as
//! bytes: a plugin's or command's id ([`id_ok`]), a request's nonce
//! ([`nonce_ok`]), a file of a plugin's source an agent may write
//! ([`source_kind`]), and the file a key of a plugin's own store is kept in
//! ([`key_name`]). Each is a single name of Clew's choosing under a folder
//! of Clew's own, or a path that stays under one.
//!
//! What each of these decides, and why that is safe — the permissions a
//! plugin is linked with are exactly what its imports need, no name climbs
//! out of its folder, no two keys share a file — is proved in
//! `verification/Clew/Plugin.lean`, and the Lean that Charon and Aeneas
//! make of this source is proved in `verification/extracted/Equal.lean` to
//! compute exactly what that model computes.
#![expect(
    clippy::manual_range_contains,
    reason = "Aeneas's library models no `RangeInclusive::contains`"
)]

use crate::text::{same_bytes, starts_with};

/// Read the project's files, outline and search (`clew:plugin/project`).
pub const READ: u8 = 1;
/// Read the project's git history (`clew:plugin/git`).
pub const GIT: u8 = 2;
/// An import Clew does not provide.
pub const UNKNOWN: u8 = 0x80;

/// The name `import` without its version: everything before the first `@`
/// (`clew:plugin/project@0.1.0` → `clew:plugin/project`).
fn unversioned_len(import: &[u8]) -> usize {
    let mut i = 0;
    while i < import.len() {
        if import[i] == b'@' {
            return i;
        }
        i += 1;
    }
    import.len()
}

/// Whether `name` is `prefix` followed by `/` and more: an interface of the
/// WASI package `prefix`.
fn in_package(name: &[u8], prefix: &[u8]) -> bool {
    starts_with(name, prefix) && name.len() > prefix.len() + 1 && name[prefix.len()] == b'/'
}

/// Whether `name` is an interface of a WASI package Clew links. Each is
/// linked with nothing behind it: no folder opened, no environment, no
/// address that may be reached; standard output and error go to the
/// plugin's log.
fn wasi_linked(name: &[u8]) -> bool {
    in_package(name, b"wasi:io")
        || in_package(name, b"wasi:cli")
        || in_package(name, b"wasi:clocks")
        || in_package(name, b"wasi:random")
        || in_package(name, b"wasi:filesystem")
        || in_package(name, b"wasi:sockets")
}

/// The permission the interface `import` (as a component names it,
/// `clew:plugin/git@0.1.0`) needs: 0 for none, [`READ`], [`GIT`], or
/// [`UNKNOWN`] for one Clew does not provide.
pub fn need_of(import: &[u8]) -> u8 {
    let len = unversioned_len(import);
    let name = &import[..len];
    if same_bytes(name, b"clew:plugin/project") {
        READ
    } else if same_bytes(name, b"clew:plugin/git") {
        GIT
    } else if same_bytes(name, b"clew:plugin/host")
        || same_bytes(name, b"clew:plugin/types")
        || wasi_linked(name)
    {
        0
    } else {
        UNKNOWN
    }
}

/// The permissions `all` together with what `import` needs ([`need_of`]):
/// a step of [`needs`].
pub fn with_need(all: u8, import: &[u8]) -> u8 {
    all | need_of(import)
}

/// Every permission the imports `imports` need together, [`UNKNOWN`] among
/// them if any is one Clew does not provide: [`with_need`] of each in turn,
/// from none.
pub fn needs(imports: &[&[u8]]) -> u8 {
    imports.iter().fold(0, |all, import| with_need(all, import))
}

/// Whether a plugin that needs `needed` ([`needs`]) may be linked when the
/// user granted `granted`: it needs nothing unknown, and nothing it needs is
/// ungranted.
pub fn may_link(needed: u8, granted: u8) -> bool {
    needed & UNKNOWN == 0 && needed & !granted == 0
}

/// The longest id, in bytes.
pub const ID_MAX: usize = 40;

/// `a`-`z`.
fn is_lower(b: u8) -> bool {
    b >= b'a' && b <= b'z'
}

/// `0`-`9`.
fn is_digit(b: u8) -> bool {
    b >= b'0' && b <= b'9'
}

/// A byte an id may hold: `a`-`z`, `0`-`9`, `-`.
fn is_id_byte(b: u8) -> bool {
    is_lower(b) || is_digit(b) || b == b'-'
}

/// Whether `id` is an id: `min` to [`ID_MAX`] bytes of `a`-`z`, `0`-`9` and
/// `-`, starting with a letter, not ending with `-`, no `--` in it
/// (`todo-authors`). A plugin's id names its folders (its source, its
/// build, its store), so an id is one plain name: no `/`, no `.`.
pub fn id_ok(id: &[u8], min: usize) -> bool {
    if id.is_empty() || id.len() < min || id.len() > ID_MAX {
        return false;
    }
    if !is_lower(id[0]) {
        return false;
    }
    id_bytes_ok(id)
}

/// Whether the byte `id[i]` may stand where it does in an id: an id byte,
/// and a `-` neither last nor followed by another.
fn id_fine_at(id: &[u8], i: usize) -> bool {
    if !is_id_byte(id[i]) {
        return false;
    }
    if id[i] != b'-' {
        return true;
    }
    if i + 1 == id.len() {
        return false;
    }
    id[i + 1] != b'-'
}

/// Whether every byte of `id` may stand where it does ([`id_fine_at`]).
fn id_bytes_ok(id: &[u8]) -> bool {
    let mut i = 0;
    while i < id.len() {
        if !id_fine_at(id, i) {
            return false;
        }
        i += 1;
    }
    true
}

/// How long a request's nonce is: 12 bytes, in hex.
pub const NONCE_LEN: usize = 24;

/// `0`-`9`, `a`-`f`.
fn is_lower_hex(b: u8) -> bool {
    is_digit(b) || (b >= b'a' && b <= b'f')
}

/// Whether `name` is a nonce, as `clew_plugin::requests` makes them:
/// [`NONCE_LEN`] lowercase hex digits. A request's files are named by it,
/// so a name read back from the folder is never a path.
pub fn nonce_ok(name: &[u8]) -> bool {
    if name.len() != NONCE_LEN {
        return false;
    }
    let mut i = 0;
    while i < name.len() {
        if !is_lower_hex(name[i]) {
            return false;
        }
        i += 1;
    }
    true
}

/// [`source_kind`]: not a file of a plugin's source.
pub const SOURCE_REFUSED: u8 = 0;
/// [`source_kind`]: the plugin's `plugin.toml`.
pub const SOURCE_MANIFEST: u8 = 1;
/// [`source_kind`]: a Rust file under `src/`.
pub const SOURCE_RUST: u8 = 2;

/// A byte a path in a plugin's source may hold: ASCII letters and digits,
/// `_`, `-`, `.` and `/`.
fn is_path_byte(b: u8) -> bool {
    is_lower(b)
        || is_digit(b)
        || (b >= b'A' && b <= b'Z')
        || b == b'_'
        || b == b'-'
        || b == b'.'
        || b == b'/'
}

/// What the path `rel` is in a plugin's source, which is all an agent may
/// write: [`SOURCE_MANIFEST`] for `plugin.toml`, [`SOURCE_RUST`] for a Rust
/// file under `src/` (`src/views/list.rs`), [`SOURCE_REFUSED`] for anything
/// else. A Rust file's path is made of ASCII letters, digits, `_`, `-`, `.`
/// and `/`, with no empty piece and no piece that starts with `.` — so no
/// `..`, no hidden file, nothing absolute, nothing a Windows path would read
/// as a drive or a separator.
pub fn source_kind(rel: &[u8]) -> u8 {
    if same_bytes(rel, b"plugin.toml") {
        return SOURCE_MANIFEST;
    }
    let n = rel.len();
    if n < 8 || !starts_with(rel, b"src/") {
        return SOURCE_REFUSED;
    }
    if rel[n - 3] != b'.' || rel[n - 2] != b'r' || rel[n - 1] != b's' {
        return SOURCE_REFUSED;
    }
    if path_ok(rel) {
        SOURCE_RUST
    } else {
        SOURCE_REFUSED
    }
}

/// Whether the byte `rel[i]` may stand where it does in a path of a
/// plugin's source: a path byte, and a `/` neither last nor followed by
/// another `/` or a `.`.
fn fine_at(rel: &[u8], i: usize) -> bool {
    if !is_path_byte(rel[i]) {
        return false;
    }
    if rel[i] != b'/' {
        return true;
    }
    if i + 1 == rel.len() {
        return false;
    }
    rel[i + 1] != b'/' && rel[i + 1] != b'.'
}

/// Whether every byte of `rel` may stand where it does ([`fine_at`]).
fn path_ok(rel: &[u8]) -> bool {
    let mut i = 0;
    while i < rel.len() {
        if !fine_at(rel, i) {
            return false;
        }
        i += 1;
    }
    true
}

/// The longest key of a plugin's store, in bytes.
pub const KEY_MAX: usize = 128;

/// The hex digit of `n` (below 16), lowercase.
fn hex_digit(n: u8) -> u8 {
    if n < 10 { b'0' + n } else { b'a' + (n - 10) }
}

/// The name of the file the key `key` of a plugin's store is kept in: its
/// bytes in lowercase hex, then `.bin`. Empty for a key that is empty or
/// longer than [`KEY_MAX`]: there is no such file. Any key a plugin chooses
/// is a plain file name, and no two keys share one.
pub fn key_name(key: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    if key.is_empty() || key.len() > KEY_MAX {
        return out;
    }
    let mut i = 0;
    while i < key.len() {
        out.push(hex_digit(key[i] / 16));
        out.push(hex_digit(key[i] % 16));
        i += 1;
    }
    out.push(b'.');
    out.push(b'b');
    out.push(b'i');
    out.push(b'n');
    out
}

#[cfg(test)]
mod tests {
    use super::{
        GIT, READ, SOURCE_MANIFEST, SOURCE_REFUSED, SOURCE_RUST, UNKNOWN, id_ok, key_name,
        may_link, need_of, needs, nonce_ok, source_kind,
    };

    #[test]
    fn ids_are_plain_names() {
        for good in ["todo-authors", "abc", "a1-b2", "x0"] {
            assert!(id_ok(good.as_bytes(), 1), "{good}");
        }
        assert!(id_ok(&[b'a'; 40], 3));
        for bad in [
            "", "-ab", "ab-", "a--b", "Abc", "1ab", "a.b", "a/b", "..", "a b", "a_b", "é",
        ] {
            assert!(!id_ok(bad.as_bytes(), 1), "{bad:?}");
        }
        assert!(!id_ok(b"ab", 3), "shorter than asked");
        assert!(!id_ok(&[b'a'; 41], 1), "longer than 40");
    }

    #[test]
    fn nonces_are_lowercase_hex() {
        assert!(nonce_ok(b"0123456789abcdef01234567"));
        assert!(!nonce_ok(b"0123456789ABCDEF01234567"));
        assert!(!nonce_ok(b"0123456789abcdef0123456"));
        assert!(!nonce_ok(b"../../0123456789abcdef01"));
        assert!(!nonce_ok(b""));
    }

    #[test]
    fn a_plugin_source_is_its_manifest_and_rust_under_src() {
        assert_eq!(source_kind(b"plugin.toml"), SOURCE_MANIFEST);
        for good in [
            "src/lib.rs",
            "src/views/list.rs",
            "src/a.rs",
            "src/x..rs",
            "src/A_b-1.rs",
        ] {
            assert_eq!(source_kind(good.as_bytes()), SOURCE_RUST, "{good}");
        }
        for bad in [
            "./plugin.toml",
            "plugin.toml ",
            "Cargo.toml",
            "src/lib.txt",
            "src/.rs",
            "src/.hidden.rs",
            "src/../x.rs",
            "src/a/../../x.rs",
            "src//a.rs",
            "src/a/.b.rs",
            "/src/a.rs",
            "src\\a.rs",
            "src/a b.rs",
            "src/c:.rs",
            "lib.rs",
            "src.rs",
            "srcx/a.rs",
            "src/",
        ] {
            assert_eq!(source_kind(bad.as_bytes()), SOURCE_REFUSED, "{bad:?}");
        }
    }

    #[test]
    fn a_key_is_its_hex_and_bin() {
        assert_eq!(key_name(b"a/b"), b"612f62.bin");
        assert_eq!(key_name(&[0, 255]), b"00ff.bin");
        assert!(key_name(b"").is_empty());
        assert!(key_name(&[b'k'; 129]).is_empty());
        assert_eq!(key_name(&[b'k'; 128]).len(), 260);
        assert_ne!(key_name(b"ab"), key_name(b"ba"));
    }

    #[test]
    fn each_import_needs_its_permission_or_none() {
        assert_eq!(need_of(b"clew:plugin/project@0.1.0"), READ);
        assert_eq!(need_of(b"clew:plugin/git@0.1.0"), GIT);
        assert_eq!(need_of(b"clew:plugin/host@0.1.0"), 0);
        assert_eq!(need_of(b"clew:plugin/types@0.1.0"), 0);
        assert_eq!(need_of(b"clew:plugin/git"), GIT);
        for wasi in [
            "wasi:io/streams@0.2.6",
            "wasi:cli/environment@0.2.6",
            "wasi:clocks/monotonic-clock@0.2.0",
            "wasi:random/random@0.2.6",
            "wasi:filesystem/preopens@0.2.6",
            "wasi:sockets/tcp@0.2.6",
        ] {
            assert_eq!(need_of(wasi.as_bytes()), 0, "{wasi}");
        }
    }

    #[test]
    fn what_clew_does_not_provide_is_unknown() {
        for other in [
            "wasi:http/outgoing-handler@0.2.6",
            "wasi:io",
            "wasi:io/",
            "wasi:iox/streams@0.2.6",
            "clew:plugin/projects@0.1.0",
            "clew:plugin/project-x@0.1.0",
            "clew:plugin@0.1.0",
            "clew:other/project@0.1.0",
            "",
            "@",
        ] {
            assert_eq!(need_of(other.as_bytes()), UNKNOWN, "{other:?}");
        }
    }

    #[test]
    fn a_plugin_links_only_with_all_it_needs_granted() {
        let reads: [&[u8]; 3] = [
            b"clew:plugin/types@0.1.0",
            b"clew:plugin/project@0.1.0",
            b"wasi:cli/stdout@0.2.6",
        ];
        assert_eq!(needs(&reads), READ);
        assert!(may_link(needs(&reads), READ));
        assert!(may_link(needs(&reads), READ | GIT));
        assert!(!may_link(needs(&reads), 0));
        assert!(!may_link(needs(&reads), GIT));

        let both: [&[u8]; 2] = [b"clew:plugin/git@0.1.0", b"clew:plugin/project@0.1.0"];
        assert_eq!(needs(&both), READ | GIT);
        assert!(!may_link(needs(&both), READ));
        assert!(may_link(needs(&both), READ | GIT));

        let none: [&[u8]; 1] = [b"clew:plugin/host@0.1.0"];
        assert!(may_link(needs(&none), 0));
        assert!(may_link(needs(&[]), 0));

        let unknown: [&[u8]; 2] = [b"clew:plugin/host@0.1.0", b"wasi:http/types@0.2.6"];
        assert!(!may_link(needs(&unknown), 0xFF));
    }
}
