import Clew.Text
import Clew.Trust

/-!
# Two guards — `trusted_by` in `crates/clew-core/src/trust.rs`, `check_headers` in `crates/clew-core/src/mcp/config.rs`

**A program Clew runs where it is.** A language server a project names is
run from a copy Clew makes of the approved bytes — unless the program sits
where nobody but you or root can change it, when it may run in place.
`trusted_by` decides that, from what `trusted_location` reads of the file
and of each directory above it: owner, mode bits, kind. Proved here: when
it says yes, no other user can write the file or replace any directory
entry on its path — under the Unix permission rules, taking every user as
possibly in the file's group, since that is not known. (An ACL is not mode
bits: the code's own comment says what it does with one.)

**An MCP server's headers.** A header the user configures is sent as a
line of the request. `check_headers` refuses a name that is not a header
name and a value that holds a line break or a NUL. Proved here: the
headers it accepts are read back — splitting the request's header block at
every CR, LF or NUL, as the most lenient server might — as exactly those
headers, one line each, under their own names. No value can start a header
of its own.
-/

namespace Clew.Guards

open Clew.Text

/-! ## A program Clew runs where it is -/

/-- What `trusted_location` reads of the file or of a directory above it:
its owner, its mode bits, and whether it is the right kind (a regular file,
a directory). -/
structure Node where
  uid : Nat
  mode : Nat
  kindOk : Bool
  deriving DecidableEq, Repr

/-- The group write bit, `0o020`. -/
def groupWrite (m : Nat) : Bool := m / 16 % 2 == 1

/-- The others' write bit, `0o002`. -/
def otherWrite (m : Nat) : Bool := m / 2 % 2 == 1

/-- The sticky bit, `0o1000`. -/
def sticky (m : Nat) : Bool := m / 512 % 2 == 1

def owned (me : Nat) (n : Node) : Bool := n.uid == me || n.uid == 0

/-- `trusted_by`: the file and every directory above it are the right kind
and owned by you or root; the file is writable by neither group nor
others, and so is each directory unless it is sticky. -/
def trustedBy (me : Nat) (file : Node) (dirs : List Node) : Bool :=
  file.kindOk && owned me file && !groupWrite file.mode && !otherWrite file.mode &&
    dirs.all fun d =>
      d.kindOk && owned me d && (sticky d.mode || (!groupWrite d.mode && !otherWrite d.mode))

/-- Whether user `u` may write a node, by its mode bits: as its owner, or
through the group or others' bits. An upper bound — which groups `u` is in
is not known, so the group bit counts for everyone. -/
def mayWrite (u : Nat) (n : Node) : Prop :=
  n.uid = u ∨ groupWrite n.mode = true ∨ otherWrite n.mode = true

/-- Whether user `u` may rename or remove someone else's entry in directory
`d`: write to it, and it is not sticky. -/
def mayReplaceIn (u : Nat) (d : Node) : Prop := mayWrite u d ∧ sticky d.mode = false

/-- **What runs in place, nobody else can change**: when `trusted_by`
says yes, no user but you and root may write the program or replace any
entry on the way to it. -/
theorem trusted_means_private (me : Nat) (file : Node) (dirs : List Node)
    (h : trustedBy me file dirs = true) (u : Nat) (hu : u ≠ me) (hr : u ≠ 0) :
    ¬ mayWrite u file ∧ ∀ d ∈ dirs, ¬ mayReplaceIn u d := by
  simp only [trustedBy, owned, Bool.and_eq_true, Bool.or_eq_true, beq_iff_eq, Bool.not_eq_true',
    List.all_eq_true] at h
  obtain ⟨⟨⟨⟨_, hown⟩, hg⟩, ho⟩, hdirs⟩ := h
  refine ⟨?_, ?_⟩
  · rintro (huid | hgw | how)
    · rcases hown with h1 | h1 <;> omega
    · rw [hg] at hgw; cases hgw
    · rw [ho] at how; cases how
  · intro d hd ⟨hw, hs⟩
    have := hdirs d hd
    obtain ⟨⟨_, downed⟩, hsticky⟩ := this
    rcases hsticky with hst | ⟨hg', ho'⟩
    · rw [hs] at hst; cases hst
    · rcases hw with huid | hgw | how
      · rcases downed with h1 | h1 <;> omega
      · rw [hg'] at hgw; cases hgw
      · rw [ho'] at how; cases how

/-! ## An MCP server's headers -/

/-- A character of a header name (`tchar`, RFC 9110). -/
def tokenChar (c : Char) : Bool :=
  isAsciiAlphanumeric c || "!#$%&'*+-.^_`|~".toList.contains c

/-- `valid_header_name`. -/
def validHeaderName (n : List Char) : Bool := !n.isEmpty && n.all tokenChar

/-- What a value may not hold: a line break, or a NUL. -/
def breakChar (c : Char) : Bool := c == '\r' || c == '\n' || c == '\x00'

/-- Why `check_headers` refuses. -/
inductive Refusal where
  | name
  | value
  deriving DecidableEq, Repr

/-- `check_headers`: the first header that cannot be sent, and why. -/
def checkHeaders : List (List Char × List Char) → Option Refusal
  | [] => none
  | (n, v) :: hs =>
    if !validHeaderName n then some .name
    else if v.any breakChar then some .value
    else checkHeaders hs

/-- The header block a request carries: `name: value` and CRLF, each. -/
def headerBlock : List (List Char × List Char) → List Char
  | [] => []
  | (n, v) :: hs => (n ++ ':' :: ' ' :: v) ++ '\r' :: '\n' :: headerBlock hs

/-- The most lenient reading of a header block: a line ends at any CR, LF
or NUL, empty lines are skipped, and a line's name runs to its first `:`. -/
def readHeaders (block : List Char) : List (List Char × List Char) :=
  ((split breakChar block).filter (!·.isEmpty)).map fun l =>
    match splitOnce ':' l with
    | some p => p
    | none => (l, [])

theorem tokenChar_facts (c : Char) (h : tokenChar c = true) : c ≠ ':' ∧ breakChar c = false := by
  simp only [tokenChar, isAsciiAlphanumeric, isAsciiDigit, Bool.or_eq_true, Bool.and_eq_true,
    decide_eq_true_eq] at h
  refine ⟨?_, ?_⟩
  · intro he; subst he; revert h; decide
  · simp only [breakChar, Bool.or_eq_false_iff, beq_eq_false_iff_ne]
    refine ⟨⟨?_, ?_⟩, ?_⟩ <;> (intro he; subst he; revert h; decide)

theorem readHeaders_cons (n v : List Char) (rest : List Char) (hn : validHeaderName n = true)
    (hv : v.any breakChar = false) :
    readHeaders ((n ++ ':' :: ' ' :: v) ++ '\r' :: '\n' :: rest) = (n, ' ' :: v) :: readHeaders rest := by
  have hall : n.all tokenChar = true := by
    simp only [validHeaderName, Bool.and_eq_true] at hn; exact hn.2
  have hne : n ≠ [] := by
    simp only [validHeaderName, Bool.and_eq_true, Bool.not_eq_true', List.isEmpty_eq_false_iff] at hn
    exact hn.1
  have hline : ∀ c ∈ n ++ ':' :: ' ' :: v, breakChar c = false := by
    intro c hc
    simp only [List.mem_append, List.mem_cons] at hc
    rcases hc with hc | rfl | rfl | hc
    · exact (tokenChar_facts c (List.all_eq_true.mp hall c hc)).2
    · decide
    · decide
    · have := List.any_eq_false.mp hv c hc; simpa using this
  have hcolon : ':' ∉ n := fun hm => (tokenChar_facts ':' (List.all_eq_true.mp hall ':' hm)).1 rfl
  unfold readHeaders
  rw [split_at_sep _ _ _ _ hline (by decide)]
  rw [show split breakChar ('\n' :: rest) = [] :: split breakChar rest by simp [split, breakChar]]
  have hlne : (n ++ ':' :: ' ' :: v).isEmpty = false := by
    cases n with
    | nil => exact absurd rfl hne
    | cons _ _ => rfl
  simp only [List.filter_cons, hlne, Bool.not_false, ite_true, List.isEmpty_nil, Bool.not_true,
    Bool.false_eq_true, ite_false, List.map_cons]
  rw [Clew.Trust.splitOnce_append _ _ hcolon]

/-- **No header starts another**: the headers `check_headers` accepts are
read back from the request as exactly those headers, one line each, under
their own names — however leniently the server splits lines. -/
theorem readHeaders_headerBlock (hs : List (List Char × List Char)) (h : checkHeaders hs = none) :
    readHeaders (headerBlock hs) = hs.map fun p => (p.1, ' ' :: p.2) := by
  induction hs with
  | nil => rfl
  | cons p hs ih =>
    obtain ⟨n, v⟩ := p
    unfold checkHeaders at h
    split at h
    · cases h
    · rename_i hn
      split at h
      · cases h
      · rename_i hv
        have hn' : validHeaderName n = true := by simpa using hn
        have hv' : v.any breakChar = false := by simpa using hv
        simp only [headerBlock, List.map_cons]
        rw [readHeaders_cons n v _ hn' hv', ih h]

end Clew.Guards
