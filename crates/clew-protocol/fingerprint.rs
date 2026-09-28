//! The protocol fingerprint: a hash of this crate's source as a TOKEN STREAM.
//!
//! Shared by `build.rs` (which embeds it as `SCHEMA_FINGERPRINT`) and by
//! `tests/fingerprint.rs` (which pins what it does and does not react to, and
//! which checkout it is taken of).
//!
//! Hashing the raw bytes, as this used to, made every comment a protocol
//! change: a doc edit gave the build a new fingerprint, the deployed remote
//! server no longer matched, and the next connect redeployed a binary whose
//! wire was byte-for-byte the same. So the source is tokenized first (a real
//! Rust lexer, `proc-macro2`), and what is hashed is a canonical spelling of
//! the tokens with every comment, every doc comment (which reaches the token
//! stream as a `#[doc = "…"]` attribute) and every `#[cfg(test)]` item left
//! out, and whitespace reduced to what separates two tokens. What remains is
//! the code: any change a compiler would see — a field, a type, a variant, a
//! serde attribute, a constant — still changes the fingerprint.
//!
//! FNV-1a, not a cryptographic hash: this guards against accidental drift
//! between two of our own builds, not against an attacker (who controls the
//! binary and thus the constant anyway).

use std::path::{Path, PathBuf};
use std::str::FromStr;

use proc_macro2::{Delimiter, Spacing, TokenStream, TokenTree};

/// The fingerprint of one checkout of this crate, and what it was taken over.
pub struct Fingerprint {
    /// The 16-hex-digit string the handshake carries.
    pub value: String,
    /// The `src` directory the files were listed from.
    pub src: PathBuf,
    /// Every file hashed, in the order hashed.
    pub files: Vec<PathBuf>,
}

/// The fingerprint of the crate whose manifest sits in `manifest_dir`: every
/// `.rs` file under its `src/`, in path order.
pub fn of_crate(manifest_dir: &Path) -> Result<Fingerprint, String> {
    let src = manifest_dir.join("src");
    let files = rust_files(&src)?;
    let mut canonical = String::new();
    for file in &files {
        let text =
            std::fs::read_to_string(file).map_err(|e| format!("read {}: {e}", file.display()))?;
        canonical.push_str(
            &canonicalize(&text).map_err(|e| format!("tokenize {}: {e}", file.display()))?,
        );
        canonical.push('\n');
    }
    Ok(Fingerprint {
        value: format!("{:016x}", fnv1a(canonical.as_bytes())),
        src,
        files,
    })
}

/// Everything `build.rs` tells cargo for the crate in `manifest_dir`, one
/// directive per line: a `rerun-if-changed` for `src/` itself (a directory
/// entry re-runs the script when a file is added to it or removed) and one for
/// EVERY file the fingerprint read, then the fingerprint.
///
/// `manifest_dir` has to be the checkout being built — what cargo hands the
/// script in `CARGO_MANIFEST_DIR` when it RUNS it (see `build.rs`).
pub fn build_directives(manifest_dir: &Path) -> Result<Vec<String>, String> {
    let fingerprint = of_crate(manifest_dir)?;
    let mut lines = Vec::with_capacity(fingerprint.files.len() + 2);
    for path in std::iter::once(&fingerprint.src).chain(&fingerprint.files) {
        lines.push(format!("cargo:rerun-if-changed={}", directive_path(path)?));
    }
    lines.push(format!(
        "cargo:rustc-env=CLEW_PROTOCOL_FINGERPRINT={}",
        fingerprint.value
    ));
    Ok(lines)
}

/// `path` as a directive value. A cargo directive is one line of UTF-8, so a
/// path that is not — or that holds a line break — cannot be named to cargo,
/// and watching something else in its place would let a change go unseen.
fn directive_path(path: &Path) -> Result<&str, String> {
    path.to_str()
        .filter(|p| !p.contains(['\n', '\r']))
        .ok_or_else(|| format!("{} cannot be named to cargo", path.display()))
}

/// Every `.rs` file under `dir`, recursively, sorted.
pub fn rust_files(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries =
            std::fs::read_dir(&dir).map_err(|e| format!("list {}: {e}", dir.display()))?;
        for entry in entries {
            let path = entry
                .map_err(|e| format!("list {}: {e}", dir.display()))?
                .path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                files.push(path);
            }
        }
    }
    files.sort();
    Ok(files)
}

/// The canonical spelling of one source file's tokens (see the module docs).
pub fn canonicalize(source: &str) -> Result<String, String> {
    let tokens = TokenStream::from_str(source).map_err(|e| e.to_string())?;
    let mut out = String::new();
    write_stream(tokens, &mut out);
    Ok(out)
}

fn write_stream(stream: TokenStream, out: &mut String) {
    let tokens: Vec<TokenTree> = stream.into_iter().collect();
    let mut i = 0;
    while i < tokens.len() {
        if let Some(attr) = attribute_at(&tokens[i..]) {
            match attr.name.as_str() {
                // Documentation, not code.
                "doc" => {
                    i += attr.len;
                    continue;
                }
                // Test-only code, compiled into neither binary.
                "cfg_test" => {
                    i += attr.len + item_len(&tokens[i + attr.len..]);
                    continue;
                }
                _ => {}
            }
        }
        write_token(&tokens[i], out);
        i += 1;
    }
}

fn write_token(token: &TokenTree, out: &mut String) {
    match token {
        TokenTree::Group(group) => {
            let (open, close) = match group.delimiter() {
                Delimiter::Parenthesis => ("(", ")"),
                Delimiter::Brace => ("{", "}"),
                Delimiter::Bracket => ("[", "]"),
                Delimiter::None => ("", ""),
            };
            out.push_str(open);
            out.push(' ');
            write_stream(group.stream(), out);
            out.push_str(close);
            out.push(' ');
        }
        TokenTree::Ident(ident) => {
            out.push_str(&ident.to_string());
            out.push(' ');
        }
        // A joint punct is half of a multi-character operator (`::`, `->`,
        // `>>`); keeping the pair together keeps the operators apart.
        TokenTree::Punct(punct) => {
            out.push(punct.as_char());
            if punct.spacing() == Spacing::Alone {
                out.push(' ');
            }
        }
        // The literal as written: a string's escapes, a number's suffix.
        TokenTree::Literal(literal) => {
            out.push_str(&literal.to_string());
            out.push(' ');
        }
    }
}

/// An attribute found at the start of a token slice.
struct Attribute {
    /// `doc` for `#[doc …]`, `cfg_test` for exactly `#[cfg(test)]`, otherwise
    /// the attribute's first identifier.
    name: String,
    /// How many tokens it spans: `#`, an optional `!`, and the bracket group.
    len: usize,
}

fn attribute_at(tokens: &[TokenTree]) -> Option<Attribute> {
    let TokenTree::Punct(hash) = tokens.first()? else {
        return None;
    };
    if hash.as_char() != '#' {
        return None;
    }
    let group_at = match tokens.get(1)? {
        TokenTree::Punct(bang) if bang.as_char() == '!' => 2,
        _ => 1,
    };
    let TokenTree::Group(group) = tokens.get(group_at)? else {
        return None;
    };
    if group.delimiter() != Delimiter::Bracket {
        return None;
    }
    let inner: Vec<TokenTree> = group.stream().into_iter().collect();
    let TokenTree::Ident(first) = inner.first()? else {
        return None;
    };
    let is_cfg_test = first == "cfg"
        && inner.len() == 2
        && matches!(&inner[1], TokenTree::Group(args)
            if args.delimiter() == Delimiter::Parenthesis
                && args.stream().to_string() == "test");
    Some(Attribute {
        name: if is_cfg_test {
            "cfg_test".to_string()
        } else {
            first.to_string()
        },
        len: group_at + 1,
    })
}

/// How many tokens the item at the start of `tokens` spans: up to and
/// including its body (the first brace group) or its terminating `;`,
/// whichever comes first — `mod tests { … }`, `fn f() { … }`, `use a::b;`.
/// Further attributes on the item are part of it.
fn item_len(tokens: &[TokenTree]) -> usize {
    for (i, token) in tokens.iter().enumerate() {
        match token {
            TokenTree::Group(group) if group.delimiter() == Delimiter::Brace => return i + 1,
            TokenTree::Punct(punct) if punct.as_char() == ';' => return i + 1,
            _ => {}
        }
    }
    tokens.len()
}

/// 64-bit FNV-1a.
pub fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}
