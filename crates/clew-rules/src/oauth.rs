//! The decisions of signing in to an MCP server (`crates/clew-core/src/mcp/oauth.rs`):
//! where credentials may be sent, whether a stored token is still good, when
//! one is taken as it is rather than renewed, and which redirect answers the
//! sign-in.
//!
//! What each guarantees is proved in `verification/Clew/OAuth.lean`, and the
//! Lean Aeneas makes of this source is proved to be that model.

use crate::text::same_bytes;

/// Whether an endpoint of the sign-in may be sent credentials: it is https,
/// or plain http to this machine (a local test server).
pub fn endpoint_allowed(https: bool, loopback_http: bool) -> bool {
    https || loopback_http
}

/// Whether a token that expires at `expires_at` (seconds since the epoch;
/// `None` for one that does not) is still good at `now` with `margin`
/// seconds to spare: `now + margin`, saturating as `u64::saturating_add`
/// does, is before it.
pub fn fresh(expires_at: Option<u64>, now: u64, margin: u64) -> bool {
    match expires_at {
        None => true,
        Some(at) => {
            if now > u64::MAX - margin {
                // `now + margin` saturates at `u64::MAX`, which is before no
                // `u64`.
                return false;
            }
            now + margin < at
        }
    }
}

/// Whether the stored access token is used as it is rather than renewed: it
/// is not the one the caller found unusable, and it is fresh. A refresh
/// token is presented only otherwise.
pub fn take_stored(stored_is_refused: bool, fresh: bool) -> bool {
    !stored_is_refused && fresh
}

/// Whether a redirect to the sign-in's callback answers this sign-in: its
/// `state` is exactly the one sent.
pub fn answers(state_param: Option<&[u8]>, state: &[u8]) -> bool {
    match state_param {
        None => false,
        Some(got) => same_bytes(got, state),
    }
}

#[cfg(test)]
mod tests {
    use super::{answers, endpoint_allowed, fresh, take_stored};

    /// What `fresh` says is what the code it replaced said, `is_none_or`
    /// over `saturating_add`, on every edge.
    #[test]
    fn fresh_is_saturating_add_before_expiry() {
        let edges = [
            0,
            1,
            59,
            60,
            61,
            u64::MAX - 61,
            u64::MAX - 60,
            u64::MAX - 1,
            u64::MAX,
        ];
        for at in edges.iter().copied().map(Some).chain([None]) {
            for now in edges {
                for margin in [0, 1, 60, u64::MAX] {
                    let by_std = at.is_none_or(|at| now.saturating_add(margin) < at);
                    assert_eq!(fresh(at, now, margin), by_std, "{at:?} {now} {margin}");
                }
            }
        }
    }

    #[test]
    fn the_other_decisions() {
        assert!(endpoint_allowed(true, false) && endpoint_allowed(false, true));
        assert!(!endpoint_allowed(false, false));
        assert!(take_stored(false, true));
        assert!(!take_stored(true, true) && !take_stored(false, false));
        assert!(answers(Some(b"abc"), b"abc"));
        assert!(!answers(Some(b"abd"), b"abc"));
        assert!(!answers(Some(b"ab"), b"abc"));
        assert!(!answers(Some(b"abcd"), b"abc"));
        assert!(!answers(None, b"abc"));
        assert!(answers(Some(b""), b""));
    }
}
