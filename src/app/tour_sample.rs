//! Tea Shop: the small project the tour is given on, shipped inside Clew.
//!
//! A tour of whatever project was open showed what that project happened to
//! have, and a step about a database schema, a type with its kinds or a fix
//! in the history pointed at nothing in a project without one. Tea Shop has
//! each of them, small enough to take in at a glance: an order's price with
//! its discounts and tax, a schema, a web API, a `Discount` and its kinds,
//! and in its history the commit that fixed how tax was worked out.
//!
//! It is written to `<data>/tour/teashop` ([`place`]) with that history
//! when git is installed, and written again only when what is there is no
//! longer it: changed since (Ask can edit files), or an older Clew's sample.
//! What Clew derives from it (its explanations, its index) is kept in the
//! data directory keyed by content, so writing it again costs nothing twice.

use std::path::{Path, PathBuf};

use crate::tr;

/// Who wrote Tea Shop, in its history.
const AUTHOR: (&str, &str) = ("Mira Chen", "mira@teashop.example");

/// The sample's files, root-relative, embedded from `assets/tour/teashop`.
macro_rules! sample {
    ($($rel:literal),* $(,)?) => {
        &[$(($rel, include_str!(concat!("../../assets/tour/teashop/", $rel)))),*]
    };
}

/// Every file of Tea Shop, as it is now.
const FILES: &[(&str, &str)] = sample![
    "README.md",
    "schema.sql",
    "main.py",
    "teashop/__init__.py",
    "teashop/models.py",
    "teashop/catalog.py",
    "teashop/discounts.py",
    "teashop/pricing.py",
    "teashop/inventory.py",
    "teashop/payments.py",
    "teashop/receipts.py",
    "teashop/checkout.py",
    "teashop/api.py",
    "tests/test_pricing.py",
];

/// The file the tour's history steps are about.
pub(crate) const FIXED_FILE: &str = "teashop/pricing.py";

/// [`FIXED_FILE`] before the fix: tax on the whole subtotal, discounts
/// taken off after.
const PRICING_BEFORE: &str = include_str!("../../assets/tour/history/pricing.py.1");

/// One commit of Tea Shop's history: its message, its date, and the files
/// it adds or changes.
struct Commit {
    message: &'static str,
    date: &'static str,
    files: &'static [&'static str],
}

/// Tea Shop's history, oldest first. Every file of [`FILES`] is in it, and
/// the last commit to name a file leaves it as [`FILES`] has it — except
/// [`FIXED_FILE`], which reads [`PRICING_BEFORE`] until the commit that
/// fixes it.
const HISTORY: &[Commit] = &[
    Commit {
        message: "Start the shop: its teas and its orders",
        date: "2025-03-03T09:14:00+00:00",
        files: &[
            "README.md",
            "schema.sql",
            "teashop/__init__.py",
            "teashop/models.py",
            "teashop/catalog.py",
        ],
    },
    Commit {
        message: "Price orders, with the discounts they earn",
        date: "2025-03-05T15:40:00+00:00",
        files: &["teashop/discounts.py", FIXED_FILE],
    },
    Commit {
        message: "Check out: stock set aside, card charged, receipt sent",
        date: "2025-03-10T11:02:00+00:00",
        files: &[
            "teashop/inventory.py",
            "teashop/payments.py",
            "teashop/receipts.py",
            "teashop/checkout.py",
            "teashop/api.py",
            "main.py",
        ],
    },
    Commit {
        message: FIX_MESSAGE,
        date: "2025-03-18T16:25:00+00:00",
        files: &[FIXED_FILE, "tests/test_pricing.py"],
    },
];

/// The fix's message: what "Why is this here?" explains a line of
/// [`FIXED_FILE`]'s `total` from.
const FIX_MESSAGE: &str = "Tax the discounted amount, not the full subtotal

Tax was worked out on the subtotal before any discount, so a customer
with a 10% discount still paid tax on the money taken off. Take the
discounts off first, then tax what is left, and never go below zero.";

/// The commit that fixes [`FIXED_FILE`] (by its index in [`HISTORY`]).
const FIX: usize = 3;

/// `rel` as commit `at` has it.
fn content(at: usize, rel: &str) -> &'static str {
    if rel == FIXED_FILE && at < FIX {
        return PRICING_BEFORE;
    }
    FILES
        .iter()
        .find(|(name, _)| *name == rel)
        .map(|(_, text)| *text)
        .expect("every file in the history is a file of the sample")
}

/// The 1-based line of the sample's file `rel`, as it is now, that first
/// holds `has`: where a tour step puts the caret.
pub(crate) fn line_of(rel: &str, has: &str) -> Option<usize> {
    let (_, text) = FILES.iter().find(|(name, _)| *name == rel)?;
    text.lines()
        .position(|line| line.contains(has))
        .map(|i| i + 1)
}

/// What marks the sample as written: a hash of everything it is made of,
/// so a Clew whose sample differs writes it again.
fn stamp() -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::hash::DefaultHasher::new();
    FILES.hash(&mut hasher);
    PRICING_BEFORE.hash(&mut hasher);
    for commit in HISTORY {
        (commit.message, commit.date, commit.files).hash(&mut hasher);
    }
    format!("{:016x}", hasher.finish())
}

/// Tea Shop, written for the tour.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placed {
    /// Its root.
    pub root: PathBuf,
    /// Whether it has its history (git was there to write it): the tour's
    /// history steps are left out when it does not.
    pub history: bool,
}

/// Write Tea Shop into `tour` (`<data>/tour`), unless it is already there as
/// it should be, and say where. Blocking (files, and git for the history).
///
/// Written whole, never patched: what is there is removed first, so a file
/// Ask added or a history half-written by a Clew that was closed does not
/// stay. Without git it is written without its history, and tried with it
/// again the next time.
pub(crate) fn place(tour: &Path) -> Result<Placed, String> {
    let root = tour.join("teashop");
    let marker = tour.join("teashop.stamp");
    let want = stamp();
    if std::fs::read_to_string(&marker).ok().as_deref() == Some(&*format!("{want} history"))
        && intact(&root)
    {
        return Ok(Placed {
            root,
            history: true,
        });
    }
    let fresh = |root: &Path| -> std::io::Result<()> {
        // The marker first: a sample half-replaced is never taken for one
        // whole.
        match std::fs::remove_file(&marker) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        match std::fs::remove_dir_all(root) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        std::fs::create_dir_all(root)
    };
    let error = |e: std::io::Error| tr!("Cannot write the sample project: {error}", error = e);
    fresh(&root).map_err(error)?;
    let commits: Vec<Vec<(&str, &str)>> = HISTORY
        .iter()
        .enumerate()
        .map(|(at, commit)| {
            commit
                .files
                .iter()
                .map(|rel| (*rel, content(at, rel)))
                .collect()
        })
        .collect();
    let history: Vec<clew_core::git::NewCommit<'_>> = HISTORY
        .iter()
        .zip(&commits)
        .map(|(commit, files)| clew_core::git::NewCommit {
            message: commit.message,
            date: commit.date,
            files,
        })
        .collect();
    let history = match clew_core::git::write_history(&root, AUTHOR, &history) {
        Ok(()) => true,
        Err(_) => {
            // No git (or one that would not commit): the files alone.
            fresh(&root).map_err(error)?;
            for (rel, text) in FILES {
                let path = root.join(rel);
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir).map_err(error)?;
                }
                std::fs::write(&path, text).map_err(error)?;
            }
            false
        }
    };
    let kind = if history { "history" } else { "files" };
    std::fs::write(&marker, format!("{want} {kind}")).map_err(error)?;
    Ok(Placed { root, history })
}

/// Whether every file of the sample at `root` is as Clew wrote it, and its
/// history is there.
fn intact(root: &Path) -> bool {
    root.join(".git").is_dir()
        && FILES
            .iter()
            .all(|(rel, text)| std::fs::read(root.join(rel)).is_ok_and(|b| b == text.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The history ends where the sample is: every file is in it, each as
    /// the sample has it after the last commit that names it, and only the
    /// fixed file reads otherwise before its fix.
    #[test]
    fn the_history_ends_at_the_sample() {
        let mut last: std::collections::HashMap<&str, &str> = Default::default();
        for (at, commit) in HISTORY.iter().enumerate() {
            for rel in commit.files {
                last.insert(rel, content(at, rel));
            }
        }
        assert_eq!(last.len(), FILES.len(), "every file, once");
        for (rel, text) in FILES {
            assert_eq!(last.get(rel), Some(text), "{rel}");
        }
        assert_ne!(content(1, FIXED_FILE), content(FIX, FIXED_FILE));
        assert!(HISTORY[FIX].files.contains(&FIXED_FILE));
        assert!(
            HISTORY.windows(2).all(|w| w[0].date < w[1].date),
            "dates in order"
        );
    }

    /// Placed once, it is reused as it is; changed, it is written again
    /// whole; and it has the history the tour shows — the fix last, the
    /// file before it as it was.
    #[test]
    fn the_sample_is_placed_whole_and_kept() {
        let scratch = clew_core::testutil::TempDir::new("tour-sample");
        let tour = scratch.path().join("tour");
        let placed = place(&tour).unwrap();
        assert_eq!(placed.root, tour.join("teashop"));
        assert!(placed.history, "git is there in the tests");
        assert!(intact(&placed.root));
        let root = &placed.root;
        let log = clew_core::git::file_history(root, FIXED_FILE, 10).unwrap();
        assert_eq!(log.len(), 2, "{log:?}");
        assert_eq!(
            log[0].subject,
            "Tax the discounted amount, not the full subtotal"
        );
        assert_eq!(
            clew_core::git::file_at(root, &log[1].sha, FIXED_FILE)
                .unwrap()
                .as_deref(),
            Some(PRICING_BEFORE)
        );

        // Kept: what Clew keeps beside it survives a second tour.
        std::fs::create_dir_all(root.join(".clew")).unwrap();
        std::fs::write(root.join(".clew/marks"), "kept").unwrap();
        assert_eq!(place(&tour).unwrap(), placed);
        assert!(root.join(".clew/marks").exists(), "reused, not rewritten");

        // Changed: written again, whole — nothing added stays.
        std::fs::write(root.join(FIXED_FILE), "changed").unwrap();
        std::fs::write(root.join("teashop/added.py"), "x = 1\n").unwrap();
        assert_eq!(place(&tour).unwrap(), placed);
        assert!(intact(root));
        assert!(!root.join("teashop/added.py").exists());

        // An older Clew's sample: written again too.
        std::fs::write(tour.join("teashop.stamp"), "0000000000000000 history").unwrap();
        std::fs::write(root.join("teashop/added.py"), "x = 1\n").unwrap();
        assert_eq!(place(&tour).unwrap(), placed);
        assert!(!root.join("teashop/added.py").exists());
    }
}
