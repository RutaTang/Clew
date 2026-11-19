//! Which server binary may be deployed: `slug` in
//! `crates/clew-core/src/server_dist.rs` calls this on the UTF-8 bytes of the
//! platform name a remote's `uname` reported, lower-cased, and refuses the
//! platform when it says no.
//!
//! That a slug it accepts is a plain token, which can neither climb out of
//! the server cache (`../`) nor change the download URL (`/`, `?`, `#`), is
//! proved in `verification/Clew/ServerDist.lean`, and the Lean Aeneas makes
//! of this source is proved to be that check, for the UTF-8 bytes of any
//! string.
#![expect(
    clippy::manual_range_contains,
    reason = "Aeneas's library models no `u8::is_ascii_*` method nor `RangeInclusive::contains`"
)]

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
