//! `str::trim` over the UTF-8 bytes of a string: the other rules here read
//! text as bytes, and this is where they find where its words begin and end.
//!
//! White space is `char::is_whitespace`: Unicode's `White_Space`. Every such
//! character but the ASCII ones takes two or three bytes in UTF-8, so the
//! bytes are read a character at a time: a character's first byte says how
//! many bytes it takes. That these functions, on the UTF-8 bytes of any
//! string, find exactly what `str::trim` finds is proved in
//! `verification/extracted/Equal.lean`, against `Text.trim` in
//! `verification/Clew/Text.lean`.
#![expect(
    clippy::manual_range_contains,
    reason = "Aeneas's library models no `RangeInclusive::contains`"
)]

/// How many bytes the character whose UTF-8 encoding starts with `b` takes.
fn char_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b < 0xE0 {
        2
    } else if b < 0xF0 {
        3
    } else {
        4
    }
}

/// Where the character at `s[i]` ends: `i` plus its length, never past the
/// end of `s` (which a whole character, as in a `str`, never is).
fn next_char(s: &[u8], i: usize) -> usize {
    let len = char_len(s[i]);
    if len > s.len() - i { s.len() } else { i + len }
}

/// The byte `k` places after `s[i]`, or 256 — no byte — past the end of `s`.
fn byte_after(s: &[u8], i: usize, k: usize) -> u16 {
    if i < s.len() && k < s.len() - i {
        s[i + k] as u16
    } else {
        256
    }
}

/// Whether a character whose encoding starts with `E2 b c` is white space:
/// U+2000 to U+200A, U+2028, U+2029, U+202F or U+205F.
fn is_space_e2(b: u16, c: u16) -> bool {
    if b == 0x80 {
        (c >= 0x80 && c <= 0x8A) || c == 0xA8 || c == 0xA9 || c == 0xAF
    } else {
        b == 0x81 && c == 0x9F
    }
}

/// Whether a character whose encoding starts with the bytes `a`, `b`, `c`
/// (256 for none) is white space: TAB to CR, space, and the two- and
/// three-byte encodings of U+0085, U+00A0, U+1680, U+2000 to U+200A, U+2028,
/// U+2029, U+202F, U+205F and U+3000.
fn is_space_bytes(a: u16, b: u16, c: u16) -> bool {
    if a == 0x20 || (a >= 0x09 && a <= 0x0D) {
        true
    } else if a == 0xC2 {
        b == 0x85 || b == 0xA0
    } else if a == 0xE1 {
        b == 0x9A && c == 0x80
    } else if a == 0xE2 {
        is_space_e2(b, c)
    } else if a == 0xE3 {
        b == 0x80 && c == 0x80
    } else {
        false
    }
}

/// Whether the character at `s[i]` is white space.
fn is_space_at(s: &[u8], i: usize) -> bool {
    is_space_bytes(
        byte_after(s, i, 0),
        byte_after(s, i, 1),
        byte_after(s, i, 2),
    )
}

/// Where the text from `i` on starts once its leading white space is
/// skipped (`s.len()` when it is all white space).
pub(crate) fn skip_space(s: &[u8], from: usize) -> usize {
    let mut i = from;
    while i < s.len() {
        if !is_space_at(s, i) {
            return i;
        }
        i = next_char(s, i);
    }
    i
}

/// Where the word that starts at `from` ends: at its first white space, or
/// at `s.len()`.
pub(crate) fn skip_word(s: &[u8], from: usize) -> usize {
    let mut i = from;
    while i < s.len() {
        if is_space_at(s, i) {
            return i;
        }
        i = next_char(s, i);
    }
    i
}

/// Where the text from `from` on ends once its trailing white space is cut:
/// just past its last character that is not white space (`from` when there
/// is none).
fn end_of_text(s: &[u8], from: usize) -> usize {
    let mut i = from;
    let mut end = from;
    while i < s.len() {
        let next = next_char(s, i);
        if !is_space_at(s, i) {
            end = next;
        }
        i = next;
    }
    end
}

/// Where `str::trim` of the text `s` holds starts and ends: `&s[start..end]`
/// is it.
pub fn trim_bounds(s: &[u8]) -> (usize, usize) {
    let start = skip_space(s, 0);
    (start, end_of_text(s, start))
}

#[cfg(test)]
mod tests {
    use super::trim_bounds;

    /// What `trim_bounds` finds is where `str::trim` finds it, for every
    /// character alone, between two others, and run together with spaces.
    #[test]
    fn trim_bounds_is_str_trim() {
        let check = |s: &str| {
            let (start, end) = trim_bounds(s.as_bytes());
            assert_eq!(&s[start..end], s.trim(), "{s:?}");
        };
        for c in (0..=0x10FFFF).filter_map(char::from_u32) {
            check(&c.to_string());
            check(&format!("a{c}b"));
            check(&format!("{c} x{c}{c}"));
            check(&format!("\u{3000}{c}\u{2003}"));
        }
        for s in ["", " ", "\u{85}\u{a0}", "\t\n x \u{205f}", "é\u{2028}é"] {
            check(s);
        }
    }
}
