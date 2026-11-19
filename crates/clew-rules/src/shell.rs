//! What Clew hands to a POSIX shell: `shell_command` and `sh_c` in
//! `src/backend/server.rs` and `quote` in `src/macos/install.rs` call these
//! on the UTF-8 bytes of their strings.
//!
//! That a POSIX shell reads what these write back as exactly the words given
//! is proved in `verification/Clew/Ssh.lean`, and the Lean Aeneas makes of
//! this source is proved to be that model, for the UTF-8 bytes of any
//! strings.
#![expect(
    clippy::manual_range_contains,
    reason = "Aeneas's library models no `u8::is_ascii_*` method nor `RangeInclusive::contains`"
)]

/// A byte a word may hold unquoted.
fn is_bare_byte(b: u8) -> bool {
    (b >= b'0' && b <= b'9')
        || (b >= b'A' && b <= b'Z')
        || (b >= b'a' && b <= b'z')
        || b == b'-'
        || b == b'_'
        || b == b'.'
        || b == b'/'
        || b == b'@'
        || b == b':'
        || b == b','
        || b == b'+'
}

/// Whether every byte of `word` may stand unquoted.
fn all_bare(word: &[u8]) -> bool {
    let mut i = 0;
    while i < word.len() {
        if !is_bare_byte(word[i]) {
            return false;
        }
        i += 1;
    }
    true
}

/// `'word'`, each `'` in it written `'\''`.
pub fn quote(word: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(b'\'');
    let mut i = 0;
    while i < word.len() {
        if word[i] == b'\'' {
            out.push(b'\'');
            out.push(b'\\');
            out.push(b'\'');
            out.push(b'\'');
        } else {
            out.push(word[i]);
        }
        i += 1;
    }
    out.push(b'\'');
    out
}

/// One word of a command: as it is when every byte may stand bare, quoted
/// otherwise.
pub fn shell_word(word: &[u8]) -> Vec<u8> {
    if !word.is_empty() && all_bare(word) {
        word.to_vec()
    } else {
        quote(word)
    }
}

/// `out` with `word` after it, a space between them unless `out` is empty
/// of words (`first`).
fn with_word(mut out: Vec<u8>, first: bool, word: &[u8]) -> Vec<u8> {
    if !first {
        out.push(b' ');
    }
    out.extend_from_slice(word);
    out
}

/// The words, each a [`shell_word`], one space between them. (The words are
/// owned: Aeneas translates no borrow of borrows, `&[&[u8]]`.)
pub fn shell_command(words: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < words.len() {
        out = with_word(out, i == 0, &shell_word(&words[i]));
        i += 1;
    }
    out
}

/// Whether `script` holds a `'`.
fn has_quote(script: &[u8]) -> bool {
    let mut i = 0;
    while i < script.len() {
        if script[i] == b'\'' {
            return true;
        }
        i += 1;
    }
    false
}

/// `sh -c '<script>'`, run under `sh` whatever the login shell is; nothing
/// when `script` holds a `'`, which is refused rather than escaped.
#[expect(
    clippy::vec_init_then_push,
    reason = "Aeneas translates `Vec::new` and `push`; `vec![]` is a box it does not"
)]
pub fn sh_c(script: &[u8]) -> Option<Vec<u8>> {
    if has_quote(script) {
        return None;
    }
    let mut out = Vec::new();
    out.push(b's');
    out.push(b'h');
    out.push(b' ');
    out.push(b'-');
    out.push(b'c');
    out.push(b' ');
    out.push(b'\'');
    out.extend_from_slice(script);
    out.push(b'\'');
    Some(out)
}
