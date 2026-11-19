//! What one header line of a framed message says (`read_frame` in
//! `crates/clew-core/src/framing.rs` reads the lines and calls [`header`]).
//!
//! Each loop is a function of its own: Aeneas translates a `return` from
//! inside a loop only when nothing follows the loop.
#![expect(
    clippy::manual_range_contains,
    reason = "Aeneas's library models no `u8::is_ascii_*` method nor `RangeInclusive::contains`"
)]

/// The largest body a frame may announce: 64 MiB.
pub const MAX_FRAME_BYTES: u64 = 64 * 1024 * 1024;

/// Why a header line ends the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderError {
    /// Not `Name: value` in ASCII with a token for a name.
    Malformed,
    /// A `Content-Length` that is not a plain decimal count fitting a `u64`.
    InvalidLength,
    /// A `Content-Length` that disagrees with an earlier one.
    Conflicting,
    /// A `Content-Length` over [`MAX_FRAME_BYTES`].
    TooLarge(u64),
}

/// An RFC 7230 `token` byte — what a header name is made of. No whitespace,
/// so `Content-Length :` is malformed rather than guessed at.
fn is_token_byte(b: u8) -> bool {
    (b >= b'0' && b <= b'9')
        || (b >= b'A' && b <= b'Z')
        || (b >= b'a' && b <= b'z')
        || b == b'!'
        || b == b'#'
        || b == b'$'
        || b == b'%'
        || b == b'&'
        || b == b'\''
        || b == b'*'
        || b == b'+'
        || b == b'-'
        || b == b'.'
        || b == b'^'
        || b == b'_'
        || b == b'`'
        || b == b'|'
        || b == b'~'
}

/// `u8::is_ascii_whitespace`: space, tab, LF, FF, CR (not VT).
fn is_ascii_ws(b: u8) -> bool {
    b == b' ' || b == b'\t' || b == b'\n' || b == 0x0c || b == b'\r'
}

/// `u8::to_ascii_lowercase`.
fn lower(b: u8) -> u8 {
    if b >= b'A' && b <= b'Z' { b + 32 } else { b }
}

const CONTENT_LENGTH: [u8; 14] = *b"content-length";

/// Whether `line[..end]` is `content-length` in any case.
fn is_content_length(line: &[u8], end: usize) -> bool {
    if end != 14 {
        return false;
    }
    let mut i = 0;
    while i < 14 {
        if lower(line[i]) != CONTENT_LENGTH[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// Whether every byte of `line` is ASCII.
fn all_ascii(line: &[u8]) -> bool {
    let mut i = 0;
    while i < line.len() {
        if line[i] >= 128 {
            return false;
        }
        i += 1;
    }
    true
}

/// The index of the first `:` in `line`, or its length if there is none.
fn find_colon(line: &[u8]) -> usize {
    let mut i = 0;
    while i < line.len() && line[i] != b':' {
        i += 1;
    }
    i
}

/// Whether `line[..end]` is all token bytes.
fn all_token(line: &[u8], end: usize) -> bool {
    let mut i = 0;
    while i < end {
        if !is_token_byte(line[i]) {
            return false;
        }
        i += 1;
    }
    true
}

/// The first index from `from` on that is not ASCII whitespace.
fn skip_ws(line: &[u8], from: usize) -> usize {
    let mut i = from;
    while i < line.len() && is_ascii_ws(line[i]) {
        i += 1;
    }
    i
}

/// The end of `line[from..to]` once trailing ASCII whitespace is taken off.
fn trim_end(line: &[u8], from: usize, to: usize) -> usize {
    let mut i = to;
    while i > from && is_ascii_ws(line[i - 1]) {
        i -= 1;
    }
    i
}

/// Whether `line[from..to]` is all ASCII digits.
fn all_digits(line: &[u8], from: usize, to: usize) -> bool {
    let mut i = from;
    while i < to {
        if line[i] < b'0' || line[i] > b'9' {
            return false;
        }
        i += 1;
    }
    true
}

/// The decimal value of the digits `line[from..to]`, if it fits a `u64`.
fn decimal(line: &[u8], from: usize, to: usize) -> Option<u64> {
    let mut n: u64 = 0;
    let mut i = from;
    while i < to {
        let digit = (line[i] - b'0') as u64;
        if n > (u64::MAX - digit) / 10 {
            return None;
        }
        n = n * 10 + digit;
        i += 1;
    }
    Some(n)
}

/// `line[from..to]` read as a `Content-Length`: ASCII digits only — not the
/// `+` `str::parse` takes — at most 20 of them, and a value that fits a
/// `u64`.
fn parse_length(line: &[u8], from: usize, to: usize) -> Option<u64> {
    if from >= to || to - from > 20 || !all_digits(line, from, to) {
        return None;
    }
    decimal(line, from, to)
}

/// What one header line says (its line end removed, and not blank), given
/// the `Content-Length` read so far: the `Content-Length` after it, or why
/// the stream cannot be read on.
///
/// Header names are matched case-insensitively, and headers other than
/// `Content-Length` are passed over. What `read_frame` makes of a stream
/// with this is proved in `verification/Clew/Framing.lean` — every message
/// `encode_framed` writes is read back as that message, and whatever is
/// read is bounded — and the Lean Aeneas makes of this source is proved to
/// be that model's `header`.
pub fn header(length: Option<u64>, line: &[u8]) -> Result<Option<u64>, HeaderError> {
    if !all_ascii(line) {
        return Err(HeaderError::Malformed);
    }
    let colon = find_colon(line);
    if colon == line.len() || colon == 0 || !all_token(line, colon) {
        return Err(HeaderError::Malformed);
    }
    if !is_content_length(line, colon) {
        return Ok(length);
    }
    let from = skip_ws(line, colon + 1);
    let to = trim_end(line, from, line.len());
    let len = match parse_length(line, from, to) {
        Some(n) => n,
        None => return Err(HeaderError::InvalidLength),
    };
    if let Some(prev) = length
        && prev != len
    {
        return Err(HeaderError::Conflicting);
    }
    if len > MAX_FRAME_BYTES {
        return Err(HeaderError::TooLarge(len));
    }
    Ok(Some(len))
}
