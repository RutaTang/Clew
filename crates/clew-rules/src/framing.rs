//! What each line a framed reader reads means. `read_frame`,
//! `read_json_line` and `read_line_capped` in `crates/clew-core/src/framing.rs`
//! read a line from the stream and hand it to [`frame_line`], [`json_line`]
//! or [`capped_line`], which decide everything about it; [`header`] is what
//! a header line says.
//!
//! Each loop is a function of its own: Aeneas translates a `return` from
//! inside a loop only when nothing follows the loop.
#![expect(
    clippy::manual_range_contains,
    reason = "Aeneas's library models no `u8::is_ascii_*` method nor `RangeInclusive::contains`"
)]

/// The largest body a frame may announce: 64 MiB.
pub const MAX_FRAME_BYTES: u64 = 64 * 1024 * 1024;

/// The longest header line, its end included: 8 KiB.
pub const MAX_HEADER_LINE_BYTES: usize = 8 * 1024;

/// The most header lines, blank separators included, one frame may have.
pub const MAX_HEADER_LINES: usize = 32;

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
pub(crate) fn is_token_byte(b: u8) -> bool {
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

/// What `read_frame` does with a line it read (at most
/// [`MAX_HEADER_LINE_BYTES`] of the stream, up to and including its first
/// `\n`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameLine {
    /// The stream ended between frames: there is no frame.
    End,
    /// The stream ended inside a frame.
    Truncated,
    /// A header line filled [`MAX_HEADER_LINE_BYTES`] without ending.
    TooLong,
    /// More than [`MAX_HEADER_LINES`] header lines.
    TooManyLines,
    /// A blank line ahead of the headers: read on.
    Separator,
    /// The blank line after the headers: the body comes next.
    Body,
    /// A header line, and what [`header`] makes of it.
    Header(Result<Option<u64>, HeaderError>),
}

/// The decision on one line `read_frame` read, `length` being the
/// `Content-Length` so far, `headers` the header lines and `lines` all the
/// lines read before it.
pub fn frame_line(length: Option<u64>, headers: usize, lines: usize, line: &[u8]) -> FrameLine {
    if line.is_empty() {
        if headers == 0 {
            return FrameLine::End;
        }
        return FrameLine::Truncated;
    }
    if line[line.len() - 1] != b'\n' {
        if line.len() >= MAX_HEADER_LINE_BYTES {
            return FrameLine::TooLong;
        }
        return FrameLine::Truncated;
    }
    if lines >= MAX_HEADER_LINES {
        return FrameLine::TooManyLines;
    }
    let mut end = line.len() - 1;
    if end > 0 && line[end - 1] == b'\r' {
        end -= 1;
    }
    if end == 0 {
        if headers == 0 {
            return FrameLine::Separator;
        }
        return FrameLine::Body;
    }
    FrameLine::Header(header(length, &line[0..end]))
}

/// Whether `line[..end]` is all ASCII white space.
fn all_ws(line: &[u8], end: usize) -> bool {
    let mut i = 0;
    while i < end {
        if !is_ascii_ws(line[i]) {
            return false;
        }
        i += 1;
    }
    true
}

/// What `read_json_line` does with a line it read (at most `cap + 2` bytes
/// of the stream, up to and including its first `\n`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonLine {
    /// The stream ended: there is no message.
    End,
    /// A line longer than the cap, this long.
    TooLarge(usize),
    /// A blank line, ended: read on.
    Blank,
    /// A message: the line's first this many bytes.
    Line(usize),
}

/// The decision on one line `read_json_line` read: its `\n` or `\r\n` end
/// taken off, longer than `cap` refused, a blank one skipped (or, unended,
/// the end of the stream), anything else a message.
pub fn json_line(line: &[u8], cap: usize) -> JsonLine {
    if line.is_empty() {
        return JsonLine::End;
    }
    let ended = line[line.len() - 1] == b'\n';
    let mut end = line.len();
    if ended {
        end -= 1;
        if end > 0 && line[end - 1] == b'\r' {
            end -= 1;
        }
    }
    if end > cap {
        return JsonLine::TooLarge(end);
    }
    if all_ws(line, end) {
        if ended {
            return JsonLine::Blank;
        }
        return JsonLine::End;
    }
    JsonLine::Line(end)
}

/// The decision on the line `read_line_capped` read (at most `cap + 1`
/// bytes of the stream, up to and including its first `\n`): where its text
/// ends — its `\n` taken off — or none at the end of the stream and for a
/// line over `cap` with no end in sight.
pub fn capped_line(buf: &[u8], cap: usize) -> Option<usize> {
    if buf.is_empty() {
        return None;
    }
    if buf[buf.len() - 1] == b'\n' {
        return Some(buf.len() - 1);
    }
    if buf.len() > cap {
        return None;
    }
    Some(buf.len())
}

#[cfg(test)]
mod tests {
    use super::{
        FrameLine, HeaderError, JsonLine, MAX_HEADER_LINE_BYTES, MAX_HEADER_LINES, capped_line,
        frame_line, json_line,
    };

    #[test]
    fn frame_line_decides_each_line() {
        assert_eq!(frame_line(None, 0, 0, b""), FrameLine::End);
        assert_eq!(frame_line(None, 1, 1, b""), FrameLine::Truncated);
        assert_eq!(
            frame_line(None, 0, 0, b"Content-Length: 1"),
            FrameLine::Truncated
        );
        let long = vec![b'a'; MAX_HEADER_LINE_BYTES];
        assert_eq!(frame_line(None, 0, 0, &long), FrameLine::TooLong);
        assert_eq!(frame_line(None, 0, 0, &long[1..]), FrameLine::Truncated);
        assert_eq!(
            frame_line(None, 0, MAX_HEADER_LINES, b"\n"),
            FrameLine::TooManyLines
        );
        assert_eq!(
            frame_line(None, 0, MAX_HEADER_LINES - 1, b"\r\n"),
            FrameLine::Separator
        );
        assert_eq!(frame_line(None, 0, 3, b"\n"), FrameLine::Separator);
        assert_eq!(frame_line(Some(2), 1, 1, b"\r\n"), FrameLine::Body);
        assert_eq!(
            frame_line(None, 0, 0, b"Content-Length: 12\r\n"),
            FrameLine::Header(Ok(Some(12)))
        );
        assert_eq!(
            frame_line(Some(3), 1, 1, b"Content-Type: x\n"),
            FrameLine::Header(Ok(Some(3)))
        );
        assert_eq!(
            frame_line(None, 0, 0, b"\r\r\n"),
            FrameLine::Header(Err(HeaderError::Malformed))
        );
    }

    #[test]
    fn json_line_decides_each_line() {
        assert_eq!(json_line(b"", 8), JsonLine::End);
        assert_eq!(json_line(b" \t\r\n", 8), JsonLine::Blank);
        assert_eq!(json_line(b"\n", 8), JsonLine::Blank);
        assert_eq!(json_line(b"  ", 8), JsonLine::End);
        assert_eq!(json_line(b"{}\r\n", 8), JsonLine::Line(2));
        assert_eq!(json_line(b"{}\n", 8), JsonLine::Line(2));
        assert_eq!(json_line(b"{}", 8), JsonLine::Line(2));
        assert_eq!(json_line(b"12345678\r\n", 8), JsonLine::Line(8));
        assert_eq!(json_line(b"123456789\n", 8), JsonLine::TooLarge(9));
        assert_eq!(json_line(b"1234567890", 8), JsonLine::TooLarge(10));
    }

    #[test]
    fn blank_is_std_ascii_white_space() {
        for b in 0..=255u8 {
            assert_eq!(super::all_ws(&[b], 1), b.is_ascii_whitespace(), "{b}");
        }
    }

    #[test]
    fn capped_line_finds_where_the_text_ends() {
        assert_eq!(capped_line(b"", 4), None);
        assert_eq!(capped_line(b"\n", 4), Some(0));
        assert_eq!(capped_line(b"abc\n", 4), Some(3));
        assert_eq!(capped_line(b"abcd\n", 4), Some(4));
        assert_eq!(capped_line(b"abcd", 4), Some(4));
        assert_eq!(capped_line(b"abcde", 4), None);
        assert_eq!(capped_line(b"a\r\n", 4), Some(2));
    }
}
