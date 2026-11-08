//! Answers that do not end at the output limit.
//!
//! A provider stops writing at the `max_tokens` it was given. An answer cut
//! there used to end mid-word, with only a note to say so. Now it is carried
//! on: the model is shown what it wrote, as its own turn, and asked to write
//! the rest ([`CARRY_ON_PROMPT`]). The rest is joined on where the answer
//! stopped ([`Seam`]) and reaches the reader as one answer, streamed on as it
//! comes. After [`CARRY_ONS`] more requests, an answer still cut is left as
//! it is, and the caller's [`super::TRUNCATED_NOTE`] says so.

use super::{Completion, LlmError};

/// How many more requests an answer cut off at its output limit is given to
/// finish. Each goes on from where the last stopped, so the whole answer has
/// this many more times the room of the first; past them it stays cut.
pub const CARRY_ONS: usize = 4;

/// What has the model write the rest of an answer its output limit cut off.
/// It is sent after the cut answer, which is put to the model as its own last
/// turn.
pub const CARRY_ON_PROMPT: &str = "Your reply above was cut off by the output limit. \
     Continue it from exactly where it stopped: write only what comes next, starting \
     with the very next character, even in the middle of a word, a line, a list or a \
     code block. Do not repeat anything already written, and add no preface or comment.";

/// What one request of a carried-on answer asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Asking<'a> {
    /// The question itself, as first asked.
    Afresh,
    /// The rest of this answer, which the output limit cut off.
    CarryOn(&'a str),
}

/// One request of an answer being carried on ([`carry_on`]): what it asks
/// for, within how many tokens, and where what the model writes goes.
pub type CarryRequest<'r> =
    dyn FnMut(Asking<'_>, u32, &mut dyn FnMut(&str)) -> Result<Completion, LlmError> + 'r;

/// `done`, carried on while it is cut off at its output limit: `request` is
/// asked, within `max_tokens`, for the rest of it, up to [`CARRY_ONS`] times,
/// and what it writes is joined on ([`Seam`]) and passed to `on_delta` as it
/// comes. A `request` passes what the model writes to the `on_delta` it is
/// given, as it was written (blank space and all), streamed or whole; one
/// that passes nothing is read from its result.
///
/// A request cut off before it wrote anything — a reasoning model that
/// thought its whole budget away — is asked [`Asking::Afresh`] with twice the
/// room, as there is nothing to go on from.
///
/// The [`Completion`] is the whole answer, `truncated` only while it is cut
/// still. A later request that fails leaves what was written, cut; a stop
/// (`cancelled`, or a request that says it stopped) is a stop.
pub fn carry_on(
    mut done: Completion,
    max_tokens: u32,
    on_delta: &mut dyn FnMut(&str),
    cancelled: &dyn Fn() -> bool,
    request: &mut CarryRequest<'_>,
) -> Result<Completion, LlmError> {
    let mut tokens = max_tokens;
    for _ in 0..CARRY_ONS {
        if !done.truncated {
            break;
        }
        if cancelled() {
            return Err(LlmError::Cancelled);
        }
        let so_far = done.text.clone();
        let afresh = so_far.trim().is_empty();
        let asking = if afresh {
            tokens = tokens.saturating_mul(2);
            Asking::Afresh
        } else {
            Asking::CarryOn(&so_far)
        };
        let mut seam = Seam::new(if afresh { "" } else { &so_far });
        let mut streamed = false;
        let mut added = String::new();
        let result = {
            let mut pass = |piece: &str| {
                streamed = true;
                let out = seam.push(piece);
                if !out.is_empty() {
                    on_delta(&out);
                    added.push_str(&out);
                }
            };
            request(asking, tokens, &mut pass)
        };
        match result {
            Ok(more) => {
                let mut rest = if streamed {
                    String::new()
                } else {
                    seam.push(&more.text)
                };
                rest.push_str(&seam.finish());
                if !rest.is_empty() {
                    on_delta(&rest);
                    added.push_str(&rest);
                }
                done.text.push_str(&added);
                done.truncated = more.truncated;
                done.stop_reason = more.stop_reason;
            }
            Err(LlmError::Cancelled) => return Err(LlmError::Cancelled),
            // What was written stands — the reader has seen it — cut.
            Err(_) => {
                done.text.push_str(&added);
                break;
            }
        }
    }
    if !done.truncated {
        // Whole, it ends as any answer does: without blank space.
        done.text.truncate(done.text.trim_end().len());
    }
    Ok(done)
}

/// The shortest repeat a [`Seam`] drops, in characters: shorter ones are as
/// likely to be what the answer says next.
const SEAM_MIN: usize = 3;

/// How much of the end of the answer so far, in bytes, a [`Seam`] looks for a
/// repeat of.
const SEAM_TAIL: usize = 2_000;

/// Where a carried-on answer joins what came before. Asked to go on from
/// where it stopped, a model now and then starts by writing the end of what
/// it wrote again: the cut word, or the cut line. That repeat is dropped.
///
/// A repeat is the whole END of the answer so far, from the start of a word
/// (or a space or mark), [`SEAM_MIN`] characters or more. The opening of the
/// continuation is held back while it could still become one, and let
/// through once it plainly is not one — or once the repeat is whole, without
/// it.
#[derive(Debug)]
pub struct Seam {
    /// The end of the answer so far.
    tail: String,
    /// The continuation's opening, held back.
    held: String,
    /// Where in `tail` a repeat could still start: each an end of `tail` that
    /// `held` is so far the start of.
    open: Vec<usize>,
    /// The longest end of `tail` that `held` starts with, in bytes.
    whole: usize,
    /// Decided: everything now passes as it comes.
    settled: bool,
}

impl Seam {
    /// The seam after `so_far`, the answer as it was cut.
    pub fn new(so_far: &str) -> Seam {
        let mut from = so_far.len().saturating_sub(SEAM_TAIL);
        while !so_far.is_char_boundary(from) {
            from += 1;
        }
        let tail = &so_far[from..];
        // The character before each place, to tell a word's start.
        let before = so_far[..from].chars().next_back();
        let chars: Vec<(usize, char)> = tail.char_indices().collect();
        let open = chars
            .iter()
            .enumerate()
            .filter(|&(n, &(_, c))| {
                let prior = if n == 0 { before } else { Some(chars[n - 1].1) };
                let word_start = !c.is_alphanumeric() || !prior.is_some_and(char::is_alphanumeric);
                word_start && chars.len() - n >= SEAM_MIN
            })
            .map(|(_, &(at, _))| at)
            .collect();
        Seam {
            tail: tail.to_string(),
            held: String::new(),
            open,
            whole: 0,
            settled: false,
        }
    }

    /// The next `piece` of the continuation; what of it, and of what was held
    /// back, may be passed on now.
    pub fn push(&mut self, piece: &str) -> String {
        if self.settled {
            return piece.to_string();
        }
        self.held.push_str(piece);
        let (tail, held) = (&self.tail, &self.held);
        let mut whole = self.whole;
        self.open.retain(|&at| {
            let end = &tail[at..];
            if held.len() >= end.len() {
                if held.starts_with(end) {
                    whole = whole.max(end.len());
                }
                false
            } else {
                end.starts_with(held.as_str())
            }
        });
        self.whole = whole;
        if self.open.is_empty() {
            self.settle()
        } else {
            String::new()
        }
    }

    /// The continuation ended: what is still held back.
    pub fn finish(&mut self) -> String {
        if self.settled {
            String::new()
        } else {
            self.settle()
        }
    }

    fn settle(&mut self) -> String {
        self.settled = true;
        let held = std::mem::take(&mut self.held);
        held[self.whole..].to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `continuation`, in `pieces` of a few bytes, through a seam after
    /// `so_far`: what the reader is passed.
    fn joined(so_far: &str, continuation: &str, piece: usize) -> String {
        let mut seam = Seam::new(so_far);
        let mut out = String::new();
        let chars: Vec<char> = continuation.chars().collect();
        for part in chars.chunks(piece) {
            out.push_str(&seam.push(&part.iter().collect::<String>()));
        }
        out.push_str(&seam.finish());
        out
    }

    /// What the model writes next passes whole, however it is cut up — a
    /// word cut in the middle carried on from its next letter.
    #[test]
    fn a_continuation_passes_as_it_is() {
        let so_far = "- lib.rs — `CacheIss";
        for piece in [1, 2, 5, 100] {
            assert_eq!(
                joined(so_far, "ue` keeps the cache.", piece),
                "ue` keeps the cache."
            );
            assert_eq!(joined(so_far, "", piece), "");
        }
        // Text that starts like a place in the answer, and goes another way.
        let so_far = "It reads the cache, then the index";
        assert_eq!(joined(so_far, " the cache is", 1), " the cache is");
    }

    /// A model that starts by writing the end of the answer again — the cut
    /// word, the cut line — has the repeat dropped.
    #[test]
    fn a_repeat_of_the_cut_end_is_dropped() {
        let so_far = "Files:\n- lib.rs — `CacheIss";
        for piece in [1, 3, 100] {
            assert_eq!(
                joined(so_far, "CacheIssue` holds it.", piece),
                "ue` holds it."
            );
            assert_eq!(
                joined(so_far, "\n- lib.rs — `CacheIssue` holds it.", piece),
                "ue` holds it."
            );
            assert_eq!(
                joined(so_far, "`CacheIssue` holds it.", piece),
                "ue` holds it."
            );
        }
        // Not in the middle of a word: the end of "pass" is not a repeat.
        assert_eq!(joined("tests pa", "ss.", 1), "ss.");
        // Nor a repeat shorter than SEAM_MIN.
        assert_eq!(joined("is a", " a good", 1), " a good");
        // Outside the ASCII range.
        assert_eq!(
            joined("キャッシュを読", "キャッシュを読み込む", 2),
            "み込む"
        );
    }

    /// A continuation that ends while it is still the start of the answer's
    /// end is passed on, not lost.
    #[test]
    fn what_is_held_back_is_passed_when_it_ends() {
        assert_eq!(joined("the cache and the cach", "the c", 1), "the c");
    }

    fn cut(text: &str) -> Completion {
        Completion {
            text: text.into(),
            truncated: true,
            stop_reason: Some("length".into()),
        }
    }

    fn whole(text: &str) -> Completion {
        Completion {
            text: text.into(),
            truncated: false,
            stop_reason: Some("stop".into()),
        }
    }

    /// An answer cut off is asked for its rest, from what it wrote, and the
    /// rest reaches the reader as it streams — one answer, not cut.
    #[test]
    fn a_cut_answer_is_carried_on() {
        let mut asked = Vec::new();
        let mut replies = vec![cut("the rest of it"), whole(", and the end.")].into_iter();
        let mut seen = String::new();
        let done = carry_on(
            cut("The start, "),
            100,
            &mut |d| seen.push_str(d),
            &|| false,
            &mut |asking, tokens, on_delta| {
                asked.push((format!("{asking:?}"), tokens));
                let reply = replies.next().unwrap();
                on_delta(&reply.text);
                Ok(reply)
            },
        )
        .unwrap();
        assert_eq!(done.text, "The start, the rest of it, and the end.");
        assert!(!done.truncated);
        assert_eq!(seen, "the rest of it, and the end.");
        assert_eq!(
            asked,
            [
                ("CarryOn(\"The start, \")".to_string(), 100),
                ("CarryOn(\"The start, the rest of it\")".to_string(), 100),
            ]
        );
    }

    /// A request that does not stream is read from its result, and joined
    /// the same way.
    #[test]
    fn a_whole_reply_is_joined_too() {
        let mut seen = String::new();
        let done = carry_on(
            cut("It returns `Resu"),
            100,
            &mut |d| seen.push_str(d),
            &|| false,
            &mut |_, _, _| Ok(whole("`Result<T>`.")),
        )
        .unwrap();
        assert_eq!(done.text, "It returns `Result<T>`.");
        assert_eq!(seen, "lt<T>`.");
    }

    /// Cut every time, it is carried on CARRY_ONS times, then left cut.
    #[test]
    fn an_answer_cut_every_time_is_left_cut() {
        let mut requests = 0;
        let done = carry_on(cut("a"), 100, &mut |_| {}, &|| false, &mut |_, _, _| {
            requests += 1;
            Ok(cut(" b"))
        })
        .unwrap();
        assert_eq!(requests, CARRY_ONS);
        assert!(done.truncated);
        assert_eq!(done.text, format!("a{}", " b".repeat(CARRY_ONS)));
    }

    /// Nothing written before the limit: asked afresh, with more room.
    #[test]
    fn a_reply_cut_before_it_began_is_asked_again_with_room() {
        let mut asked = Vec::new();
        let done = carry_on(
            cut(""),
            400,
            &mut |_| {},
            &|| false,
            &mut |asking, tokens, _| {
                asked.push((asking == Asking::Afresh, tokens));
                Ok(if tokens < 1600 {
                    cut("")
                } else {
                    whole("Done.")
                })
            },
        )
        .unwrap();
        assert_eq!(asked, [(true, 800), (true, 1600)]);
        assert_eq!(done.text, "Done.");
        assert!(!done.truncated);
    }

    /// A failed request leaves what was written, cut; a stop is a stop.
    #[test]
    fn a_failure_keeps_what_was_written_and_a_stop_stops() {
        let mut seen = String::new();
        let done = carry_on(
            cut("The start"),
            100,
            &mut |d| seen.push_str(d),
            &|| false,
            &mut |_, _, on_delta| {
                on_delta(", the middle");
                Err(LlmError::Protocol("dropped".into()))
            },
        )
        .unwrap();
        assert_eq!(done.text, "The start, the middle");
        assert_eq!(seen, ", the middle");
        assert!(done.truncated);

        let stopped = carry_on(cut("x"), 100, &mut |_| {}, &|| false, &mut |_, _, _| {
            Err(LlmError::Cancelled)
        });
        assert!(matches!(stopped, Err(LlmError::Cancelled)));
        let mut requests = 0;
        let stopped = carry_on(cut("x"), 100, &mut |_| {}, &|| true, &mut |_, _, _| {
            requests += 1;
            Ok(whole("y"))
        });
        assert!(matches!(stopped, Err(LlmError::Cancelled)));
        assert_eq!(requests, 0, "nothing asked once stopped");
    }

    /// A whole answer is not asked about again.
    #[test]
    fn a_whole_answer_is_left_alone() {
        let done = carry_on(
            whole("All of it."),
            100,
            &mut |_| {},
            &|| false,
            &mut |_, _, _| panic!("asked again"),
        )
        .unwrap();
        assert_eq!(done.text, "All of it.");
    }
}
