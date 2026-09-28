//! Killing a process group: every process in it.
//!
//! What clew starts on the reader's behalf — language servers, debug
//! adapters, the installers that fetch them, git — runs as the leader of a
//! process group of its own (`process_group(0)`), so that stopping it stops
//! what it started in turn: a server's workers, an adapter's debuggee, the ssh
//! under a fetch. [`kill`] and [`kill_async`] are that stop, and every group
//! kill goes through them: [`crate::framing::reap`] and
//! [`crate::framing::try_wait_sweeping`] (the LSP and DAP clients and
//! clew-server's processes), git's deadline and output cap, and a toolchain
//! install's cancel or deadline.

use std::time::{Duration, Instant};

/// The longest [`kill`] goes on signalling a group that still has a live
/// member: one that SIGKILL does not end at once — a process in an
/// uninterruptible wait takes it only when the wait is over.
pub const KILL_BOUND: Duration = Duration::from_secs(2);

/// The pause between two rounds of [`kill`].
const ROUND_PAUSE: Duration = Duration::from_millis(1);

/// SIGKILL every process in the group `leader` leads — including a child one
/// of them is forking at that moment — and return once none of them can run
/// on, or after [`KILL_BOUND`].
///
/// `leader` is the pid of the caller's own child, spawned as the leader of a
/// group of its own, whose id is therefore that pid. The caller keeps it
/// UNREAPED until this returns (exited or not): an exited child stays a
/// zombie holding its pid, so the id cannot pass to another group while the
/// signals go out. Reaped, it could, and the next round would kill strangers.
///
/// One signal is not enough on macOS. A member forking as it is sent can
/// have its child join the group after the kernel went through it, and that
/// child runs on as an orphan: an install cancelled while it started helpers
/// left one behind about every other time. So the group is signalled again,
/// a millisecond apart, for as long as the kernel finds a live member to
/// deliver to — a member still dying counts, and so does one in the middle
/// of a fork — and no longer once it answers that none is left: ESRCH, or
/// EPERM when only zombies remain, the unreaped leader among them. Linux
/// needs one round (see [`signal`]).
///
/// Blocking, for as long as the members take to die: not at all for a group
/// with none left alive, and one pause for members that die at the signal (a
/// process with gigabytes mapped does, on macOS). [`kill_async`] does the
/// same without holding the thread.
pub fn kill(leader: u32) {
    let Some(group) = group_of(leader) else {
        return;
    };
    let deadline = Instant::now() + KILL_BOUND;
    while signal(group) && Instant::now() < deadline {
        std::thread::sleep(ROUND_PAUSE);
    }
}

/// [`kill`] for async code: between rounds it waits on the runtime's timer,
/// which leaves the thread to other tasks. Dropped part-way, it leaves the
/// group unfinished, so a caller that may be dropped there finishes with
/// [`kill`] as it goes — as `framing::reap` does.
pub async fn kill_async(leader: u32) {
    let Some(group) = group_of(leader) else {
        return;
    };
    let deadline = Instant::now() + KILL_BOUND;
    while signal(group) && Instant::now() < deadline {
        tokio::time::sleep(ROUND_PAUSE).await;
    }
}

/// The group a child whose pid is `leader` leads, as the kernel names it —
/// `None` for a pid no child of ours can have: 0 would name the caller's
/// OWN group, 1 is init's, and one past `pid_t` is no pid at all.
fn group_of(leader: u32) -> Option<libc::pid_t> {
    libc::pid_t::try_from(leader).ok().filter(|&pgid| pgid > 1)
}

/// One round: SIGKILL every member of `group` the kernel finds. Whether
/// another round is due.
fn signal(group: libc::pid_t) -> bool {
    // SAFETY: plain syscall. The caller keeps the leader unreaped, so the id
    // still names the group it created.
    let found = unsafe { libc::killpg(group, libc::SIGKILL) } == 0;
    // Linux makes a group signal and a fork exclude each other (both take
    // the tasklist lock, and a fork fails once a fatal signal is pending):
    // either the fork fails, or its child is in the group when the signal
    // goes out. So one round is complete there — and has to be the last,
    // since Linux counts a zombie as signalled, the unreaped leader included:
    // `found` would stay true until the bound.
    found && !cfg!(target_os = "linux")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::path::Path;
    use std::process::{Child, Command, Stdio};

    /// Start `script` under `/bin/sh`, `record` as its `$1`, as the leader of
    /// a process group of its own.
    fn group(script: &str, record: &Path) -> Child {
        Command::new("/bin/sh")
            .arg("-c")
            .arg(script)
            .arg("sh")
            .arg(record)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap()
    }

    /// The pid a script writes to `path` (`echo $$ > "$1"`), once it is all
    /// there: the file exists, empty, before the pid is in it. Only a wait
    /// for the script to get going, which nothing here times — a minute, for
    /// a machine busy building.
    fn recorded(path: &Path) -> libc::pid_t {
        for _ in 0..6000 {
            if let Some(pid) = std::fs::read_to_string(path)
                .ok()
                .as_deref()
                .and_then(|text| text.strip_suffix('\n'))
                .and_then(|pid| pid.trim().parse().ok())
            {
                return pid;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the script never recorded its pid in {}", path.display());
    }

    /// Whether group `pgid`, whose leader is reaped, is left with nothing
    /// alive within 10 s of the kill: what a kill missed is alive for 30 s.
    /// Waited for, not asked once, because the members' zombies are the
    /// system's to reap as orphans, and until then Linux still finds them.
    fn emptied(pgid: libc::pid_t) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        // SAFETY: signal 0 only probes; the group is this test's own.
        while unsafe { libc::killpg(pgid, 0) } == 0 {
            if Instant::now() >= deadline {
                // SAFETY: as above; the survivors are this test's helpers.
                unsafe { libc::killpg(pgid, libc::SIGKILL) };
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        true
    }

    /// A group killed while it forks is left with nothing alive. One SIGKILL
    /// was not enough on macOS: a child forked as it was sent could join the
    /// group after the kernel went through it, and ran on — here, in most
    /// attempts. Both ways of waiting between rounds are tried, each at
    /// several points of the forking.
    #[test]
    fn a_group_killed_while_it_forks_is_left_with_nothing_alive() {
        let dir = crate::testutil::TempDir::new("procgroup-forking");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        for attempt in 0..16u64 {
            let record = dir.join(format!("leader-{attempt}"));
            // Records itself, then starts helpers back to back: a bounded
            // number of them, whatever becomes of the kill.
            let mut leader = group(
                "echo $$ > \"$1\"; for _ in $(seq 300); do sleep 30 & done; wait",
                &record,
            );
            let pgid = recorded(&record);
            assert_eq!(u32::try_from(pgid).ok(), Some(leader.id()));
            std::thread::sleep(Duration::from_millis(2 + 3 * (attempt / 2)));
            if attempt % 2 == 0 {
                kill(leader.id());
            } else {
                runtime.block_on(kill_async(leader.id()));
            }
            leader.wait().unwrap();
            assert!(
                emptied(pgid),
                "a member forked as its group was killed outlived it (attempt {attempt})"
            );
        }
    }

    /// The kill ends as soon as nothing in the group is left alive, rather
    /// than running on to its bound: the leader, a zombie held unreaped all
    /// along, is nothing to go on signalling. (Linux, which counts zombies as
    /// signalled, kept a group of killed members going for the full bound.)
    #[test]
    fn a_kill_ends_once_nothing_in_the_group_is_alive() {
        let dir = crate::testutil::TempDir::new("procgroup-prompt");
        let record = dir.join("leader");
        // A helper started, then the leader recorded.
        let mut leader = group("sleep 30 & echo $$ > \"$1\"; wait", &record);
        let pgid = recorded(&record);
        let started = Instant::now();
        kill(leader.id());
        let took = started.elapsed();
        leader.wait().unwrap();
        assert!(emptied(pgid), "the helper outlived the kill");
        assert!(took < KILL_BOUND, "the kill ran to its bound ({took:?})");
    }
}
