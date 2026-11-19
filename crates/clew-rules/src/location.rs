//! Whether a program may run where it is.

/// What is read of a file, or of a directory above it: its owner, its mode
/// bits, and whether it is the right kind (a regular file, a directory).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Node {
    pub uid: u32,
    pub mode: u32,
    pub kind_ok: bool,
}

/// Owned by `me` or root, and the right kind.
fn owned(me: u32, n: &Node) -> bool {
    n.kind_ok && (n.uid == me || n.uid == 0)
}

/// Writable by neither group (`0o020`) nor others (`0o002`).
fn closed(n: &Node) -> bool {
    n.mode & 0o022 == 0
}

/// Sticky (`0o1000`): others may add entries but not rename or replace ours.
fn sticky(n: &Node) -> bool {
    n.mode & 0o1000 != 0
}

/// Whether nobody but `me` or root can change `file` or re-point a directory
/// above it (`dirs`, nearest first): the file and every directory owned by
/// `me` or root and the right kind, the file writable by neither group nor
/// others, and each directory too unless it is sticky.
///
/// That a yes leaves no other user able to write the file or replace an
/// entry on its path is proved in `verification/Clew/Guards.lean`, and the
/// Lean Aeneas makes of this source is proved to be that model.
pub fn trusted_by(me: u32, file: &Node, dirs: &[Node]) -> bool {
    if !owned(me, file) || !closed(file) {
        return false;
    }
    let mut i = 0;
    while i < dirs.len() {
        let d = &dirs[i];
        if !owned(me, d) || !(sticky(d) || closed(d)) {
            return false;
        }
        i += 1;
    }
    true
}
