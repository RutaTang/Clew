/-!
# Signing in to an MCP server — `crates/clew-core/src/mcp/oauth.rs`

A remote MCP server may ask Clew to sign in with OAuth: Clew registers,
sends you to the server's sign-in page, waits for the redirect back on a
port of this machine, exchanges the code for tokens, and renews them. The
protocol is the network's; these are the decisions Clew makes in it
(`clew_rules::oauth`), each proved of the code itself in
`verification/extracted/Equal.lean`:

* where credentials may be sent;
* whether a stored token is still good, with a margin, without overflow;
* when a stored token is used as it is, and a refresh token presented;
* which redirect answers the sign-in.
-/

namespace Clew.OAuth

/-- The largest `u64`. -/
def u64Max : Nat := 2 ^ 64 - 1

/-- `endpoint_allowed`: https, or plain http to this machine. -/
def endpointAllowed (https loopbackHttp : Bool) : Bool := https || loopbackHttp

/-- `fresh`: a token without an expiry, or one that expires after
`now + margin` — the sum saturating at the largest `u64`, as
`u64::saturating_add` does. -/
def fresh (expiresAt : Option Nat) (now margin : Nat) : Bool :=
  match expiresAt with
  | none => true
  | some t => decide (min (now + margin) u64Max < t)

/-- `take_stored`: the stored access token is used as it is when it is not
the one the caller found unusable and it is fresh. -/
def takeStored (storedIsRefused isFresh : Bool) : Bool := !storedIsRefused && isFresh

/-- `answers`: the redirect's `state` is exactly the one sent. -/
def answers (stateParam : Option (List Nat)) (state : List Nat) : Bool :=
  stateParam == some state

/-- **Credentials go only over https, or to this machine.** -/
theorem credentials_only_over_https_or_to_this_machine (https loopbackHttp : Bool) :
    endpointAllowed https loopbackHttp = true ↔ https = true ∨ loopbackHttp = true := by
  simp [endpointAllowed]

/-- **A token is used only while more than the margin is left of it**:
for any expiry a `u64` holds, fresh means `now + margin` is before it, in
whole numbers — the saturating sum never wraps round to a small one. -/
theorem fresh_means_time_left (t now margin : Nat) (ht : t ≤ u64Max) :
    fresh (some t) now margin = true ↔ now + margin < t := by
  simp only [fresh, decide_eq_true_eq]
  constructor
  · intro h
    by_cases hle : now + margin ≤ u64Max
    · rwa [Nat.min_eq_left hle] at h
    · rw [Nat.min_eq_right (by omega)] at h; omega
  · intro h; exact Nat.lt_of_le_of_lt (Nat.min_le_left _ _) h

/-- **A refresh token is presented only when it has to be**: the stored
token is the one found unusable, or it is no longer fresh. Where a server
rotates refresh tokens, each is good for one exchange, and one presented
twice can get the whole sign-in revoked. -/
theorem refresh_only_when_needed (storedIsRefused isFresh : Bool) :
    takeStored storedIsRefused isFresh = false ↔ storedIsRefused = true ∨ isFresh = false := by
  cases storedIsRefused <;> cases isFresh <;> simp [takeStored]

/-- **Only the answer to this sign-in ends it**: a redirect is taken only
with exactly the `state` sent, so another page or program reaching the
callback port can neither finish the sign-in nor make it fail. -/
theorem answers_iff (stateParam : Option (List Nat)) (state : List Nat) :
    answers stateParam state = true ↔ stateParam = some state := by
  simp [answers]

end Clew.OAuth
