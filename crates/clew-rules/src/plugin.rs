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

/// Every permission the imports `imports` need together, [`UNKNOWN`] among
/// them if any is one Clew does not provide.
pub fn needs(imports: &[&[u8]]) -> u8 {
    let mut all = 0;
    let mut i = 0;
    while i < imports.len() {
        all |= need_of(imports[i]);
        i += 1;
    }
    all
}

/// Whether a plugin that needs `needed` ([`needs`]) may be linked when the
/// user granted `granted`: it needs nothing unknown, and nothing it needs is
/// ungranted.
pub fn may_link(needed: u8, granted: u8) -> bool {
    needed & UNKNOWN == 0 && needed & !granted == 0
}

#[cfg(test)]
mod tests {
    use super::{GIT, READ, UNKNOWN, may_link, need_of, needs};

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
