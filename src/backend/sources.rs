//! A remote project's files as its host has them, read over `ReadSources`:
//! every rel asked for, over as many replies as it takes, and what the host
//! said of each — read, not there, too large, no plain text file, there and
//! not readable — or that it is not known.

use std::collections::{HashSet, VecDeque};

use clew_protocol::{Event, Refusal, Rel};

/// Most rels one `ReadSources` asks for: it bounds the request, and the work
/// one reply does. What a reply carries is bounded in bytes on the host,
/// which names the rels it had no room for (see [`read`]).
const BATCH: usize = 128;

/// What a host said of the rels asked of it ([`read`]): each rel in one
/// list, in the order it was settled.
#[derive(Debug, Default)]
pub struct HostSources {
    /// The rels read, each with its text.
    pub files: Vec<(Rel, String)>,
    /// The rels not on the host: gone.
    pub missing: Vec<Rel>,
    /// The rels too large to read, each with its size in bytes. Asked for
    /// again, the host says the same until the file changes.
    pub too_large: Vec<(Rel, u64)>,
    /// The rels that are there and are no plain text files of the project —
    /// not UTF-8, a link, a FIFO or a device, or reached through a link out
    /// of it — each with what the host found it to be. Asked for again, the
    /// host says the same until they change.
    pub refused: Vec<(Rel, Refusal)>,
    /// The rels that are there and that the host could not read — this
    /// user may not, or the read failed — each with the host's why. Never
    /// gone: what a caller holds for one stands.
    pub unreadable: Vec<(Rel, String)>,
    /// The rels not known: not answered for, each with why its round trip
    /// failed — the connection, an error for an answer, a reply of another
    /// kind — or `None` where a reply came and did not settle it (it settled
    /// none of its batch, or the host said nothing of it). Never gone.
    pub unread: Vec<(Rel, Option<String>)>,
}

/// Ask the host for `rels` — distinct and project-relative — through `ask`,
/// one `ReadSources` round trip a call and at most [`BATCH`] rels a batch,
/// and say what it said of each ([`HostSources`]).
///
/// A reply carries at most so many bytes and names the rels it had no room
/// for (`deferred`): those are asked for again, first and in order, until
/// each is settled. The host settles the first rel of every batch, so each
/// reply gets further; a reply that settled none of its batch is not asked
/// again — a host that does not keep to that would be asked forever — and
/// what it deferred is unread. What a reply names that was not asked for is
/// not taken.
///
/// The first round trip that fails ends the reading: its batch and every
/// rel not asked for yet are unread, with why. Each caller fails on any
/// unread rel, and a host that stalled one round trip — each waits out the
/// request's whole time limit — was asked the rest of the way, one batch
/// after another, before the caller heard of it.
pub async fn read<F, Fut>(rels: Vec<Rel>, mut ask: F) -> HostSources
where
    F: FnMut(Vec<Rel>) -> Fut,
    Fut: std::future::Future<Output = Result<Event, String>>,
{
    let mut out = HostSources::default();
    let mut queue: VecDeque<Rel> = rels.into();
    while !queue.is_empty() {
        let batch: Vec<Rel> = queue.drain(..queue.len().min(BATCH)).collect();
        let failed = match ask(batch.clone()).await {
            Ok(Event::Sources {
                files,
                missing,
                too_large,
                refused,
                unreadable,
                deferred,
                ..
            }) => {
                // What the reply does not settle is unread.
                let mut open: HashSet<&str> = batch.iter().map(String::as_str).collect();
                let mut settle = |rel: &str| open.remove(rel);
                out.files
                    .extend(files.into_iter().filter(|(rel, _)| settle(rel)));
                out.missing
                    .extend(missing.into_iter().filter(|rel| settle(rel)));
                out.too_large
                    .extend(too_large.into_iter().filter(|(rel, _)| settle(rel)));
                out.refused
                    .extend(refused.into_iter().filter(|(rel, _)| settle(rel)));
                out.unreadable
                    .extend(unreadable.into_iter().filter(|(rel, _)| settle(rel)));
                let later: Vec<&str> = deferred
                    .iter()
                    .filter_map(|rel| open.take(rel.as_str()))
                    .collect();
                let again = if later.len() < batch.len() {
                    later
                } else {
                    open.extend(later);
                    Vec::new()
                };
                out.unread.extend(
                    batch
                        .iter()
                        .filter(|rel| open.contains(rel.as_str()))
                        .map(|rel| (rel.clone(), None)),
                );
                for rel in again.into_iter().rev() {
                    queue.push_front(rel.to_string());
                }
                continue;
            }
            Ok(_) => "unexpected reply to ReadSources".to_string(),
            Err(why) => why,
        };
        // The round trip failed: nothing more is asked of this host.
        out.unread.extend(
            batch
                .into_iter()
                .chain(queue.drain(..))
                .map(|rel| (rel, Some(failed.clone()))),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `Sources` reply naming `files` read (each with its own name for
    /// text) and `deferred`, and nothing else.
    fn sources(files: &[&str], deferred: &[&str]) -> Result<Event, String> {
        Ok(Event::Sources {
            root: "/p".into(),
            files: files
                .iter()
                .map(|r| (r.to_string(), r.to_string()))
                .collect(),
            missing: Vec::new(),
            too_large: Vec::new(),
            refused: Vec::new(),
            unreadable: Vec::new(),
            deferred: deferred.iter().map(|r| r.to_string()).collect(),
        })
    }

    fn rels(names: &[&str]) -> Vec<Rel> {
        names.iter().map(|r| r.to_string()).collect()
    }

    /// What a reply had no room for is asked for again, first and in
    /// order, until each is settled; a reply that settled none of its batch
    /// is not asked again, and what it deferred is unread.
    #[test]
    fn deferred_rels_are_asked_for_again_until_a_reply_settles_none() {
        let asked = std::cell::RefCell::new(Vec::new());
        // One file a reply, until only `d` is left, which it never settles.
        let ask = |batch: Vec<Rel>| {
            asked.borrow_mut().push(batch.clone());
            let reply = match batch.as_slice() {
                [only] if only == "d" => sources(&[], &["d"]),
                [first, rest @ ..] => {
                    let rest: Vec<&str> = rest.iter().map(String::as_str).collect();
                    sources(&[first.as_str()], &rest)
                }
                [] => unreachable!("an empty batch was asked for"),
            };
            std::future::ready(reply)
        };
        let host = iced::futures::executor::block_on(read(rels(&["a", "b", "c", "d"]), ask));
        assert_eq!(
            *asked.borrow(),
            [
                rels(&["a", "b", "c", "d"]),
                rels(&["b", "c", "d"]),
                rels(&["c", "d"]),
                rels(&["d"]),
            ]
        );
        let read: Vec<&str> = host.files.iter().map(|(rel, _)| rel.as_str()).collect();
        assert_eq!(read, ["a", "b", "c"]);
        assert_eq!(host.unread, [("d".to_string(), None)]);
    }

    /// A reply is taken only for what its batch asked, each rel once — what
    /// the host could not read included, with its error.
    #[test]
    fn only_what_was_asked_is_taken() {
        let ask = |batch: Vec<Rel>| {
            let mut files = batch.clone();
            files.push("not-asked".into());
            files.push(batch[0].clone());
            let files: Vec<&str> = files.iter().map(String::as_str).collect();
            let mut reply = sources(&files, &[]);
            if let Ok(Event::Sources { unreadable, .. }) = &mut reply {
                unreadable.push(("elsewhere".into(), "Permission denied".into()));
            }
            std::future::ready(reply)
        };
        let host = iced::futures::executor::block_on(read(rels(&["a", "b"]), ask));
        let taken: Vec<&str> = host.files.iter().map(|(rel, _)| rel.as_str()).collect();
        assert_eq!(taken, ["a", "b"]);
        assert!(host.unreadable.is_empty(), "{:?}", host.unreadable);

        let ask = |_: Vec<Rel>| {
            let mut reply = sources(&["a"], &[]);
            if let Ok(Event::Sources { unreadable, .. }) = &mut reply {
                unreadable.push(("secret".into(), "Permission denied".into()));
            }
            std::future::ready(reply)
        };
        let host = iced::futures::executor::block_on(read(rels(&["a", "secret"]), ask));
        assert_eq!(
            host.unreadable,
            [("secret".to_string(), "Permission denied".to_string())]
        );
        assert!(host.unread.is_empty(), "{:?}", host.unread);
    }

    /// The first round trip that fails ends the reading: its batch, and
    /// every rel not asked for yet, is unread with that failure — while a
    /// rel an earlier reply left unsettled is unread with none, not with
    /// another batch's failure. Each batch after a failed one used to be
    /// asked all the same, a stalled host waiting out the whole time limit
    /// for each before the caller heard of any.
    #[test]
    fn a_failed_round_trip_ends_the_reading_with_why_for_each_rel() {
        let names: Vec<String> = (0..2 * BATCH + 1).map(|i| format!("f{i:03}")).collect();
        let mut calls = 0;
        let ask = |batch: Vec<Rel>| {
            calls += 1;
            let reply = match calls {
                // The first reply settles all of its batch but one.
                1 => {
                    let files: Vec<&str> = batch[1..].iter().map(String::as_str).collect();
                    sources(&files, &[])
                }
                2 => Err("the connection dropped".to_string()),
                _ => sources(&[], &[]),
            };
            std::future::ready(reply)
        };
        let host = iced::futures::executor::block_on(read(names.clone(), ask));
        assert_eq!(calls, 2, "a batch was asked for after a failed one");
        let read: Vec<&str> = host.files.iter().map(|(rel, _)| rel.as_str()).collect();
        assert_eq!(read, names[1..BATCH]);
        let dropped = Some("the connection dropped".to_string());
        let mut unread = vec![(names[0].clone(), None)];
        unread.extend(
            names[BATCH..]
                .iter()
                .map(|rel| (rel.clone(), dropped.clone())),
        );
        assert_eq!(host.unread, unread);
    }
}
