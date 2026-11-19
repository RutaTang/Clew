//! The argument list a saved connection hands ssh: `SavedConnection::ssh_args`
//! in `src/backend/connect.rs` calls this on the UTF-8 bytes of its trimmed
//! user, host and identity file.
//!
//! That ssh reads exactly the options Clew means from it — its own, `-p` and
//! `-i` — and `user@host` as the destination, whatever the fields hold, is
//! proved in `verification/Clew/Ssh.lean` (`parseSshArgs_sshArgs`), and the
//! Lean Aeneas makes of this source is proved to write that model's list,
//! for the UTF-8 bytes of any strings.

#![expect(
    clippy::vec_init_then_push,
    reason = "Aeneas translates `Vec::new` and `push`; `vec![]` is a box it does not"
)]

use crate::fingerprint::with_decimal;

/// A word of the given ASCII bytes, pushed one at a time: Charon translates
/// no constant byte strings, and Aeneas does not model `str::as_bytes`.
macro_rules! ascii {
    ($($b:literal)*) => {{
        let mut word: Vec<u8> = Vec::new();
        $(word.push($b);)*
        word
    }};
}

/// `args` with `-o <value>` after them.
fn with_option(mut args: Vec<Vec<u8>>, value: Vec<u8>) -> Vec<Vec<u8>> {
    args.push(ascii!(b'-' b'o'));
    args.push(value);
    args
}

/// `bytes` as a word of their own.
fn word(bytes: &[u8]) -> Vec<u8> {
    let mut w = Vec::new();
    w.extend_from_slice(bytes);
    w
}

/// ssh's arguments for `user@host` on `port` with the identity file
/// `identity` (none when empty): Clew's fixed options, `-p` unless the port
/// is 22, `-i` unless there is no identity, then `--` and the destination,
/// so no field can be read as an option.
pub fn ssh_args(user: &[u8], host: &[u8], port: u16, identity: &[u8]) -> Vec<Vec<u8>> {
    // ConnectTimeout=10
    let mut args = with_option(
        Vec::new(),
        ascii!(b'C' b'o' b'n' b'n' b'e' b'c' b't' b'T' b'i' b'm' b'e' b'o' b'u' b't' b'=' b'1' b'0'),
    );
    // StrictHostKeyChecking=yes
    args = with_option(
        args,
        ascii!(b'S' b't' b'r' b'i' b'c' b't' b'H' b'o' b's' b't' b'K' b'e' b'y' b'C' b'h' b'e' b'c' b'k' b'i' b'n' b'g' b'=' b'y' b'e' b's'),
    );
    // ServerAliveInterval=15
    args = with_option(
        args,
        ascii!(b'S' b'e' b'r' b'v' b'e' b'r' b'A' b'l' b'i' b'v' b'e' b'I' b'n' b't' b'e' b'r' b'v' b'a' b'l' b'=' b'1' b'5'),
    );
    // ServerAliveCountMax=3
    args = with_option(
        args,
        ascii!(b'S' b'e' b'r' b'v' b'e' b'r' b'A' b'l' b'i' b'v' b'e' b'C' b'o' b'u' b'n' b't' b'M' b'a' b'x' b'=' b'3'),
    );
    // NumberOfPasswordPrompts=1
    args = with_option(
        args,
        ascii!(b'N' b'u' b'm' b'b' b'e' b'r' b'O' b'f' b'P' b'a' b's' b's' b'w' b'o' b'r' b'd' b'P' b'r' b'o' b'm' b'p' b't' b's' b'=' b'1'),
    );
    if port != 22 {
        args.push(ascii!(b'-' b'p'));
        args.push(with_decimal(Vec::new(), port as usize));
    }
    if !identity.is_empty() {
        args.push(ascii!(b'-' b'i'));
        args.push(word(identity));
    }
    args.push(ascii!(b'-' b'-'));
    let mut destination = word(user);
    destination.push(b'@');
    destination.extend_from_slice(host);
    args.push(destination);
    args
}

#[cfg(test)]
mod tests {
    use super::ssh_args;

    #[test]
    fn the_fixed_options_are_spelled_right() {
        let args = ssh_args(b"u", b"h", 22, b"");
        let words: Vec<&str> = args
            .iter()
            .map(|w| std::str::from_utf8(w).unwrap())
            .collect();
        assert_eq!(
            words,
            [
                "-o",
                "ConnectTimeout=10",
                "-o",
                "StrictHostKeyChecking=yes",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=3",
                "-o",
                "NumberOfPasswordPrompts=1",
                "--",
                "u@h"
            ]
        );
    }

    #[test]
    fn the_destination_comes_last_after_a_double_dash() {
        let args = ssh_args(b"root", b"-oProxyCommand=x", 2222, b"/k");
        let words: Vec<&[u8]> = args.iter().map(Vec::as_slice).collect();
        assert_eq!(
            words[10..],
            [
                &b"-p"[..],
                b"2222",
                b"-i",
                b"/k",
                b"--",
                b"root@-oProxyCommand=x"
            ]
        );
        let plain = ssh_args(b"u", b"h", 22, b"");
        assert_eq!(plain.len(), 12);
        assert_eq!(plain[11], b"u@h");
    }
}
