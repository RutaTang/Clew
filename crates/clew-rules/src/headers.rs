//! An MCP server's request headers: what may be sent as a line of the
//! request (`check_headers` in `crates/clew-core/src/mcp/config.rs` calls
//! these on each header's bytes).

/// Whether `name` can be sent as a header's name: not empty, and every byte
/// a token byte (RFC 9110 `tchar`).
pub fn header_name_ok(name: &[u8]) -> bool {
    if name.is_empty() {
        return false;
    }
    let mut i = 0;
    while i < name.len() {
        if !crate::framing::is_token_byte(name[i]) {
            return false;
        }
        i += 1;
    }
    true
}

/// Whether `value` can be sent as a header's value: it holds no line break
/// (CR, LF) and no NUL, so it cannot start a header of its own.
///
/// That the headers these two accept are read back from the request as
/// exactly those headers is proved in `verification/Clew/Guards.lean`, and
/// the Lean Aeneas makes of this source is proved to be that model, for the
/// UTF-8 bytes of any string.
pub fn header_value_ok(value: &[u8]) -> bool {
    let mut i = 0;
    while i < value.len() {
        if value[i] == b'\r' || value[i] == b'\n' || value[i] == 0 {
            return false;
        }
        i += 1;
    }
    true
}
