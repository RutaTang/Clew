//! Which server binary may be deployed. `slug` in
//! `crates/clew-core/src/server_dist.rs` calls `slug_ok` on the UTF-8 bytes of
//! the platform name a remote's `uname` reported, lower-cased, and refuses
//! the platform when it says no; `embedded_digest` and `parse_sha256` there
//! read the digest a binary must have with `pinned_digest` (the list built
//! into a release) and `sidecar_digest` (the `.sha256` file next to a
//! download).
//!
//! That a slug `slug_ok` accepts is a plain token, which can neither climb
//! out of the server cache (`../`) nor change the download URL (`/`, `?`,
//! `#`), and that with a list built in only the digest it pins for the asset
//! itself is ever accepted, is proved in `verification/Clew/ServerDist.lean`;
//! the Lean Aeneas makes of this source is proved to be those readings, for
//! the UTF-8 bytes of any string.
#![expect(
    clippy::manual_range_contains,
    reason = "Aeneas's library models no `u8::is_ascii_*` method nor `RangeInclusive::contains`"
)]

use crate::text::{skip_space, skip_word, trim_bounds};

/// A byte a slug may hold: ASCII letters and digits, `-` and `_`.
fn is_slug_byte(b: u8) -> bool {
    (b >= b'0' && b <= b'9')
        || (b >= b'A' && b <= b'Z')
        || (b >= b'a' && b <= b'z')
        || b == b'-'
        || b == b'_'
}

/// Whether `s` is a slug: not empty, and every byte a slug byte.
pub fn slug_ok(s: &[u8]) -> bool {
    if s.is_empty() {
        return false;
    }
    let mut i = 0;
    while i < s.len() {
        if !is_slug_byte(s[i]) {
            return false;
        }
        i += 1;
    }
    true
}

/// An ASCII hex digit.
fn is_hex(b: u8) -> bool {
    if b >= b'0' && b <= b'9' {
        return true;
    }
    if b >= b'a' && b <= b'f' {
        return true;
    }
    b >= b'A' && b <= b'F'
}

/// Whether `t` is a SHA-256 in hex: 64 ASCII hex digits.
fn is_hex64(t: &[u8]) -> bool {
    if t.len() != 64 {
        return false;
    }
    let mut i = 0;
    while i < t.len() {
        if !is_hex(t[i]) {
            return false;
        }
        i += 1;
    }
    true
}

/// `u8::to_ascii_lowercase`.
fn lower_byte(b: u8) -> u8 {
    if b >= b'A' && b <= b'Z' { b + 32 } else { b }
}

/// `t` with its ASCII letters lower-cased.
fn lower(t: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < t.len() {
        out.push(lower_byte(t[i]));
        i += 1;
    }
    out
}

/// Whether `a` and `b` are the same bytes.
fn same(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// Where the first `c` in `t` is, or `t.len()`.
fn find_byte(t: &[u8], c: u8) -> usize {
    let mut i = 0;
    while i < t.len() {
        if t[i] == c {
            return i;
        }
        i += 1;
    }
    t.len()
}

/// Where `t[from..]` goes on past the `*`s it starts with.
fn skip_stars(t: &[u8], from: usize) -> usize {
    let mut i = from;
    while i < t.len() && t[i] == b'*' {
        i += 1;
    }
    i
}

/// The name and digest of a trimmed entry, as `(name from, name to, digest
/// from, digest to)` in it: `<name>=<digest>`, each part trimmed, or
/// `sha256sum` output, `<digest> <name>` or `<digest> *<name>`. The digest
/// is 64 hex digits and the name not empty, or there is nothing.
fn entry_fields(t: &[u8]) -> Option<(usize, usize, usize, usize)> {
    let eq = find_byte(t, b'=');
    if eq < t.len() {
        let (name_from, name_to) = trim_bounds(&t[0..eq]);
        let (from, to) = trim_bounds(&t[eq + 1..t.len()]);
        let (digest_from, digest_to) = (eq + 1 + from, eq + 1 + to);
        if is_hex64(&t[digest_from..digest_to]) && name_from < name_to {
            return Some((name_from, name_to, digest_from, digest_to));
        }
        return None;
    }
    let digest_to = skip_word(t, 0);
    let second = skip_space(t, digest_to);
    let second_to = skip_word(t, second);
    if second == second_to || skip_space(t, second_to) != t.len() {
        return None;
    }
    let name_from = skip_stars(t, second);
    if is_hex64(&t[0..digest_to]) && name_from < second_to {
        return Some((name_from, second_to, 0, digest_to));
    }
    None
}

/// `found`, or the lower-cased digest of `entry` when it names `asset`.
fn with_entry(found: Option<Vec<u8>>, entry: &[u8], asset: &[u8]) -> Option<Vec<u8>> {
    let (start, end) = trim_bounds(entry);
    let t = &entry[start..end];
    match entry_fields(t) {
        Some((name_from, name_to, digest_from, digest_to)) => {
            if same(&t[name_from..name_to], asset) {
                Some(lower(&t[digest_from..digest_to]))
            } else {
                found
            }
        }
        None => found,
    }
}

/// A separator between entries of the list: a newline, `;` or `,`.
fn is_entry_sep(b: u8) -> bool {
    if b == b'\n' {
        return true;
    }
    b == b';' || b == b','
}

/// The digest the list built into a release pins for `asset`, lower-cased:
/// that of the last entry naming it, or none. Entries are separated by
/// newlines, `;` or `,`; anything that is not an entry is no entry.
pub fn pinned_digest(list: &[u8], asset: &[u8]) -> Option<Vec<u8>> {
    let mut found = None;
    let mut start = 0;
    let mut i = 0;
    while i < list.len() {
        if is_entry_sep(list[i]) {
            found = with_entry(found, &list[start..i], asset);
            start = i + 1;
        }
        i += 1;
    }
    with_entry(found, &list[start..list.len()], asset)
}

/// The digest a `.sha256` file holds, lower-cased: its first word that is
/// 64 hex digits, so both a bare digest and `sha256sum` output read.
pub fn sidecar_digest(file: &[u8]) -> Option<Vec<u8>> {
    let mut i = skip_space(file, 0);
    while i < file.len() {
        let end = skip_word(file, i);
        if is_hex64(&file[i..end]) {
            return Some(lower(&file[i..end]));
        }
        i = skip_space(file, end);
    }
    None
}
