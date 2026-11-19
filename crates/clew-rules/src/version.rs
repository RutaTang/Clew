//! Which version an update declares, and whether it is newer than the one
//! running: `Version::parse` and `ensure_upgrade` in
//! `crates/clew-core/src/update.rs` call these on the UTF-8 bytes of the
//! version text, and install nothing `is_newer` refuses.
//!
//! That what `ensure_upgrade` lets through is strictly newer, that the order
//! is a strict total order, and that the version Clew writes reads back as
//! itself is proved in `verification/Clew/Update.lean`, and the Lean Aeneas
//! makes of this source is proved to be that model, for the UTF-8 bytes of
//! any string.

#![expect(
    clippy::manual_range_contains,
    reason = "Aeneas's library models no `RangeInclusive::contains`"
)]

use crate::text::trim_bounds;

/// A version `major.minor.patch`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

/// Whether `v` is newer than `running`: a later major, or the same major and
/// a later minor, or both the same and a later patch.
pub fn is_newer(running: Version, v: Version) -> bool {
    if v.major != running.major {
        return v.major > running.major;
    }
    if v.minor != running.minor {
        return v.minor > running.minor;
    }
    v.patch > running.patch
}

/// Where the first `a` or `b` in `s[from..to]` is, or `to` when there is
/// none.
fn find(s: &[u8], from: usize, to: usize, a: u8, b: u8) -> usize {
    let mut i = from;
    while i < to {
        if s[i] == a || s[i] == b {
            return i;
        }
        i += 1;
    }
    to
}

/// Where `s[from..to]` goes on past the `v`s it starts with.
fn skip_v(s: &[u8], from: usize, to: usize) -> usize {
    let mut i = from;
    while i < to && s[i] == b'v' {
        i += 1;
    }
    i
}

/// The value of the ASCII digits `s[from..to]`: at least one, all digits,
/// worth less than 2^64.
fn digits_u64(s: &[u8], from: usize, to: usize) -> Option<u64> {
    if from >= to {
        return None;
    }
    let mut value: u128 = 0;
    let mut i = from;
    while i < to {
        let b = s[i];
        if b < b'0' || b > b'9' {
            return None;
        }
        value = value * 10 + (b - b'0') as u128;
        if value > 0xFFFF_FFFF_FFFF_FFFF {
            return None;
        }
        i += 1;
    }
    Some(value as u64)
}

/// `u64::from_str` of `s[from..to]`: an optional `+`, then the digits.
fn parse_u64(s: &[u8], from: usize, to: usize) -> Option<u64> {
    if from < to && s[from] == b'+' {
        digits_u64(s, from + 1, to)
    } else {
        digits_u64(s, from, to)
    }
}

/// `u64::from_str` of `s[from..to]` with its white space trimmed.
fn parse_field(s: &[u8], from: usize, to: usize) -> Option<u64> {
    let field = &s[from..to];
    let (start, end) = trim_bounds(field);
    parse_u64(field, start, end)
}

/// `1.2.3` or `v1.2.3`: trimmed, the leading `v`s dropped, everything from
/// the first `-` or `+` ignored (a pre-release or build suffix), then three
/// `.`-separated numbers, each trimmed; pieces past the third are ignored.
#[expect(
    clippy::question_mark,
    reason = "each `match` is proved as Charon and Aeneas translate it; `?` goes through `Try`"
)]
pub fn parse(s: &[u8]) -> Option<Version> {
    let (start, end) = trim_bounds(s);
    let from = skip_v(s, start, end);
    let to = find(s, from, end, b'-', b'+');
    let dot1 = find(s, from, to, b'.', b'.');
    if dot1 == to {
        return None;
    }
    let dot2 = find(s, dot1 + 1, to, b'.', b'.');
    if dot2 == to {
        return None;
    }
    let dot3 = find(s, dot2 + 1, to, b'.', b'.');
    let major = match parse_field(s, from, dot1) {
        Some(n) => n,
        None => return None,
    };
    let minor = match parse_field(s, dot1 + 1, dot2) {
        Some(n) => n,
        None => return None,
    };
    let patch = match parse_field(s, dot2 + 1, dot3) {
        Some(n) => n,
        None => return None,
    };
    Some(Version {
        major,
        minor,
        patch,
    })
}

/// What `ensure_upgrade` makes of the version an update declares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Upgrade {
    /// It reads as no version.
    Unreadable,
    /// It reads as this version, which is not newer than the running one.
    NotNewer(Version),
    /// It reads as this version, which is newer: the one update installed.
    Newer(Version),
}

/// `ensure_upgrade`'s decision: install only a version `candidate` reads as
/// and that is newer than `running`.
pub fn upgrade(running: Version, candidate: &[u8]) -> Upgrade {
    match parse(candidate) {
        None => Upgrade::Unreadable,
        Some(v) => {
            if is_newer(running, v) {
                Upgrade::Newer(v)
            } else {
                Upgrade::NotNewer(v)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Version, is_newer, parse};

    fn v(major: u64, minor: u64, patch: u64) -> Option<Version> {
        Some(Version {
            major,
            minor,
            patch,
        })
    }

    #[test]
    fn parse_reads_what_str_parse_reads() {
        assert_eq!(parse(b"1.2.3"), v(1, 2, 3));
        assert_eq!(parse(b"vv1.2.3"), v(1, 2, 3));
        assert_eq!(parse(" \u{3000}v0.1.4\n".as_bytes()), v(0, 1, 4));
        assert_eq!(parse(b"1.2.3-rc1"), v(1, 2, 3));
        assert_eq!(parse(b"1.2.3+build.5"), v(1, 2, 3));
        assert_eq!(parse(b"1. 2 . 3.9"), v(1, 2, 3));
        assert_eq!(parse(b"18446744073709551615.0.0"), v(u64::MAX, 0, 0));
        for bad in [
            "",
            "1.2",
            "x.y.z",
            "nightly",
            "1..3",
            "1.2.",
            "18446744073709551616.0.0",
        ] {
            assert_eq!(parse(bad.as_bytes()), None, "{bad:?}");
        }
        assert_eq!(parse("1.2.٣".as_bytes()), None);
    }

    /// The reading `Version::parse` had before it moved here, on `str`.
    fn by_str(s: &str) -> Option<Version> {
        let s = s.trim().trim_start_matches('v');
        let core = s.split(['-', '+']).next().unwrap_or(s);
        let mut it = core.split('.');
        let major = it.next()?.trim().parse().ok()?;
        let minor = it.next()?.trim().parse().ok()?;
        let patch = it.next()?.trim().parse().ok()?;
        Some(Version {
            major,
            minor,
            patch,
        })
    }

    #[test]
    fn parse_is_the_str_reading_on_every_short_string() {
        let alphabet = ["1", "0", "v", ".", "-", "+", " ", "\u{3000}", "a", "٣"];
        let mut strings = vec![String::new()];
        for _ in 0..5 {
            let mut longer = Vec::new();
            for s in &strings {
                for a in alphabet {
                    longer.push(format!("{s}{a}"));
                }
            }
            strings = longer;
            for s in &strings {
                assert_eq!(parse(s.as_bytes()), by_str(s), "{s:?}");
            }
        }
    }

    #[test]
    fn is_newer_is_the_order_of_the_fields() {
        let a = Version {
            major: 1,
            minor: 2,
            patch: 3,
        };
        assert!(!is_newer(a, a));
        assert!(is_newer(a, Version { patch: 4, ..a }));
        assert!(is_newer(
            a,
            Version {
                minor: 3,
                patch: 0,
                ..a
            }
        ));
        assert!(!is_newer(
            a,
            Version {
                major: 0,
                minor: 9,
                patch: 9
            }
        ));
    }
}
