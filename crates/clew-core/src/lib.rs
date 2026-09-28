//! clew-core: backend logic shared by the clew GUI client and the headless
//! clew-server.
//!
//! Computation over the filesystem (and the few network fetches clew makes
//! on its own behalf, see [`net`]) with no GUI, so the same code runs in the
//! in-process server (linked into the client) and in the standalone
//! `clew-server` binary that runs remotely. Where a result crosses the
//! protocol unchanged (the file tree, blame, `StateMerge`), the wire type from
//! `clew-protocol` is used directly rather than converted.

// These docs are for clew's own developers and are built with
// `--document-private-items` (the doc gate in .github/workflows/ci.yml), so a
// public item's doc may link to the private helper that does the work: such a
// link resolves there. rustdoc still flags it in that mode, hence the allow.
#![allow(rustdoc::private_intra_doc_links)]

/// Serializes tests that mutate process-global environment variables
/// (`CLEW_DATA_DIR`, provider API keys). Cargo runs tests of one binary in
/// parallel threads, so two env-touching tests racing each other fail flakily.
/// Tolerates a panicking holder: its guard is released while it unwinds, so
/// one failed test does not fail the others.
///
/// Re-entrant: a thread that holds the lock may take it again, and it is
/// released when that thread's outermost guard drops. Test helpers lock for
/// themselves ([`testutil::DataDir`], [`testutil::EnvVars`], a suite's lazily
/// primed data directory), so a test that already holds the lock and then
/// reaches one of them must not wait on itself — with a plain mutex that was a
/// hang of the whole run, avoided only by convention.
///
/// Public (not `cfg(test)`) so the GUI crate's tests, which set the same
/// variables, share this one lock — a `cfg(test)` item would be a *different*
/// lock in each crate and serialize nothing across them.
pub fn env_lock() -> EnvLock {
    let me = std::thread::current().id();
    let mut owner = ENV_OWNER.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        match owner.0 {
            None => {
                *owner = (Some(me), 1);
                break;
            }
            Some(holder) if holder == me => {
                owner.1 += 1;
                break;
            }
            Some(_) => owner = ENV_FREE.wait(owner).unwrap_or_else(|e| e.into_inner()),
        }
    }
    EnvLock {
        _not_send: std::marker::PhantomData,
    }
}

/// Which thread holds [`env_lock`], and how many of its guards are alive.
static ENV_OWNER: std::sync::Mutex<(Option<std::thread::ThreadId>, usize)> =
    std::sync::Mutex::new((None, 0));
/// Signalled when [`env_lock`] is released.
static ENV_FREE: std::sync::Condvar = std::sync::Condvar::new();

/// A hold on [`env_lock`]; released when the last guard of the holding thread
/// drops. Not `Send`: it is released by the thread that took it.
#[must_use = "the environment is locked only while the guard lives"]
pub struct EnvLock {
    _not_send: std::marker::PhantomData<*const ()>,
}

impl Drop for EnvLock {
    fn drop(&mut self) {
        let mut owner = ENV_OWNER.lock().unwrap_or_else(|e| e.into_inner());
        owner.1 -= 1;
        if owner.1 == 0 {
            owner.0 = None;
            drop(owner);
            ENV_FREE.notify_one();
        }
    }
}

pub mod apidoc;
pub mod confine;
pub mod debugadapter;
pub mod derived;
pub mod docs;
pub mod embed;
pub mod explain;
pub mod framing;
pub mod fs_scan;
pub mod git;
pub mod globalconfig;
pub mod highlight;
pub mod imports;
pub mod inactive;
pub mod incremental;
pub mod llm;
pub mod lsp;
pub mod net;
pub mod notebook;
pub mod outline;
#[cfg(unix)]
pub mod procgroup;
pub mod projectcalls;
pub mod rustscope;
pub mod search;
pub mod server_dist;
pub mod statefile;
pub mod stats;
pub mod structure;
#[doc(hidden)]
pub mod testutil;
pub mod trust;
pub mod update;
