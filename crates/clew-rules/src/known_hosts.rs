//! Which host a recorded ssh key vouches for: `plain_host_field` in
//! `src/backend/connect.rs` calls this on the UTF-8 bytes of the host field
//! a key is about to be recorded under, and records nothing when it says no.
//!
//! That a field it accepts names exactly one host and port, with none of
//! known_hosts' pattern syntax, is proved in `verification/Clew/Ssh.lean`,
//! and the Lean Aeneas makes of this source is proved to be that model, for
//! the UTF-8 bytes of any string.
#![expect(
    clippy::manual_range_contains,
    reason = "Aeneas's library models no `u8::is_ascii_*` method nor `RangeInclusive::contains`"
)]

/// A byte a host name may hold: ASCII letters and digits, `.`, `-`, `_`,
/// and `:` (an IPv6 address on port 22).
fn is_name_byte(b: u8) -> bool {
    (b >= b'0' && b <= b'9')
        || (b >= b'A' && b <= b'Z')
        || (b >= b'a' && b <= b'z')
        || b == b'.'
        || b == b'-'
        || b == b'_'
        || b == b':'
}

/// Whether every byte of `field[from..to]` is a name byte.
fn all_name(field: &[u8], from: usize, to: usize) -> bool {
    let mut i = from;
    while i < to {
        if !is_name_byte(field[i]) {
            return false;
        }
        i += 1;
    }
    true
}

/// Whether `field[from..to]` is a host name: 1 to 255 name bytes.
fn name_ok(field: &[u8], from: usize, to: usize) -> bool {
    if to <= from || to - from > 255 {
        return false;
    }
    all_name(field, from, to)
}

/// Where the first `]:` at or after `from` starts, as `str::split_once`
/// finds it.
fn find_close(field: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i < field.len() {
        if field[i] == b']' && i + 1 < field.len() && field[i + 1] == b':' {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Whether the digits `field[from..]` are a port: at least one, all ASCII
/// digits, worth 1 to 65535.
fn digits_port(field: &[u8], from: usize) -> bool {
    if from >= field.len() {
        return false;
    }
    let mut value: u32 = 0;
    let mut i = from;
    while i < field.len() {
        let b = field[i];
        if b < b'0' || b > b'9' {
            return false;
        }
        value = value * 10 + (b - b'0') as u32;
        if value > 65535 {
            return false;
        }
        i += 1;
    }
    value != 0
}

/// Whether `field[from..]` is a port other than 0, as `str::parse::<u16>`
/// reads one: an optional `+`, then digits.
fn port_ok(field: &[u8], from: usize) -> bool {
    if from < field.len() && field[from] == b'+' {
        digits_port(field, from + 1)
    } else {
        digits_port(field, from)
    }
}

/// Whether `field` names exactly one host as a known_hosts host field —
/// `name` or `[name]:port` — with none of the pattern syntax (`*`, `?`, `!`,
/// `,`), no hashed `|1|…` form and no marker.
pub fn plain_host_field(field: &[u8]) -> bool {
    if !field.is_empty() && field[0] == b'[' {
        match find_close(field, 1) {
            Some(close) => name_ok(field, 1, close) && port_ok(field, close + 2),
            None => false,
        }
    } else {
        name_ok(field, 0, field.len())
    }
}
