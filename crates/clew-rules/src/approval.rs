//! Which machine and project an approval is filed under: `scope_label` in
//! `src/backend/connect.rs` and `project_key` in
//! `crates/clew-core/src/derived.rs` build a remote project's key with these,
//! on the UTF-8 bytes of their strings.
//!
//! That two remotes — user, host and port — and two roots share a key only
//! when they are the same is proved in `verification/Clew/Trust.lean`
//! (`approvalKey_injective`), and the Lean Aeneas makes of this source is
//! proved to write exactly the keys that model writes, for the UTF-8 bytes
//! of any strings.

/// `out` with `b` after it, escaped: `%`, `/`, `[`, `]` and `@` as `%XX`,
/// so none of them, which the key's own syntax uses, appears unescaped.
fn with_escaped_byte(mut out: Vec<u8>, b: u8) -> Vec<u8> {
    if b == b'%' {
        out.push(b'%');
        out.push(b'2');
        out.push(b'5');
    } else if b == b'/' {
        out.push(b'%');
        out.push(b'2');
        out.push(b'F');
    } else if b == b'[' {
        out.push(b'%');
        out.push(b'5');
        out.push(b'B');
    } else if b == b']' {
        out.push(b'%');
        out.push(b'5');
        out.push(b'D');
    } else if b == b'@' {
        out.push(b'%');
        out.push(b'4');
        out.push(b'0');
    } else {
        out.push(b);
    }
    out
}

/// `out` with `text` after it, each byte escaped.
fn with_escaped(mut out: Vec<u8>, text: &[u8]) -> Vec<u8> {
    let mut i = 0;
    while i < text.len() {
        out = with_escaped_byte(out, text[i]);
        i += 1;
    }
    out
}

/// Whether `text` holds a `:`.
fn has_colon(text: &[u8]) -> bool {
    let mut i = 0;
    while i < text.len() {
        if text[i] == b':' {
            return true;
        }
        i += 1;
    }
    false
}

/// `label` with `user@` after it, the user escaped; nothing when ssh is
/// given no user.
fn with_user(mut label: Vec<u8>, user: Option<&[u8]>) -> Vec<u8> {
    if let Some(user) = user {
        label = with_escaped(label, user);
        label.push(b'@');
    }
    label
}

/// `label` with the host after it, escaped, and bracketed when it holds a
/// `:` (an IPv6 address), so a port after it can never be read as part of it.
fn with_host(mut label: Vec<u8>, host: &[u8]) -> Vec<u8> {
    if has_colon(host) {
        label.push(b'[');
        label = with_escaped(label, host);
        label.push(b']');
    } else {
        label = with_escaped(label, host);
    }
    label
}

/// `label` with `:port` after it, the port escaped; nothing when the port
/// is ssh's own.
fn with_port(mut label: Vec<u8>, port: Option<&[u8]>) -> Vec<u8> {
    if let Some(port) = port {
        label.push(b':');
        label = with_escaped(label, port);
    }
    label
}

/// `user@host:port`, each part escaped: the user left out when ssh is given
/// none, the host bracketed when it holds a `:`, and the port left out when
/// it is ssh's own.
pub fn scope_label(user: Option<&[u8]>, host: &[u8], port: Option<&[u8]>) -> Vec<u8> {
    with_port(with_host(with_user(Vec::new(), user), host), port)
}

/// A remote project's key: `ssh://<label>:<root>`, a relative root written
/// after `./` so it never starts where the label could go on.
#[expect(
    clippy::vec_init_then_push,
    reason = "Aeneas translates `Vec::new` and `push`; `vec![]` is a box it does not"
)]
pub fn approval_key(label: &[u8], root: &[u8]) -> Vec<u8> {
    let mut key = Vec::new();
    key.push(b's');
    key.push(b's');
    key.push(b'h');
    key.push(b':');
    key.push(b'/');
    key.push(b'/');
    key.extend_from_slice(label);
    key.push(b':');
    if root.is_empty() || root[0] != b'/' {
        key.push(b'.');
        key.push(b'/');
    }
    key.extend_from_slice(root);
    key
}
