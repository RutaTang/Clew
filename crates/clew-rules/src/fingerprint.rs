//! What an approval covers: the bytes a language server's fingerprint
//! hashes. `crates/clew-core/src/trust.rs` builds every fingerprint's input
//! with these and hashes it with SHA-256.
//!
//! That no two different inputs encode alike — no rearrangement of the
//! parts, no argument holding the separators, reads the same — is proved in
//! `verification/Clew/Trust.lean`, and the Lean Aeneas makes of this source
//! is proved to write exactly those encodings.

/// `out` with `n` in decimal after it: most significant digit first, no
/// leading zero, as `format!("{n}")` writes it.
pub(crate) fn with_decimal(mut out: Vec<u8>, n: usize) -> Vec<u8> {
    if n >= 10 {
        out = with_decimal(out, n / 10);
    }
    out.push(b'0' + (n % 10) as u8);
    out
}

/// `out` with `;<byte length>:<bytes>` of `arg` after it.
fn with_arg(mut out: Vec<u8>, arg: &[u8]) -> Vec<u8> {
    out.push(b';');
    out = with_decimal(out, arg.len());
    out.push(b':');
    out.extend_from_slice(arg);
    out
}

/// The arguments as one fingerprint part: their count, then each as
/// `;<byte length>:<bytes>`.
pub fn args_part(args: &[Vec<u8>]) -> Vec<u8> {
    let mut out = with_decimal(Vec::new(), args.len());
    let mut i = 0;
    while i < args.len() {
        out = with_arg(out, &args[i]);
        i += 1;
    }
    out
}

/// `out` with the eight little-endian bytes of `n` after it.
fn with_le64(mut out: Vec<u8>, n: u64) -> Vec<u8> {
    let mut rest = n;
    let mut k: usize = 0;
    while k < 8 {
        out.push((rest % 256) as u8);
        rest /= 256;
        k += 1;
    }
    out
}

/// `out` with `part` after it, its length first as eight little-endian bytes.
fn with_field(out: Vec<u8>, part: &[u8]) -> Vec<u8> {
    let mut out = with_le64(out, part.len() as u64);
    out.extend_from_slice(part);
    out
}

/// The parts as the bytes a fingerprint hashes: each part's length as eight
/// little-endian bytes, then its bytes.
pub fn length_prefixed(parts: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < parts.len() {
        out = with_field(out, &parts[i]);
        i += 1;
    }
    out
}
