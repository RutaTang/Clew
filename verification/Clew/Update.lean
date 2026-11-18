import Clew.Text

/-!
# No downgrade — `clew_core::update`

Clew updates itself. Before an update is installed, `ensure_upgrade`
compares the version the downloaded bundle declares with the version that is
running, and refuses anything not strictly newer: an old build with a known
hole cannot be pushed back onto a machine as an "update".

This file models `Version::parse`, the order `#[derive(Ord)]` gives
`Version` (its fields in declaration order: major, minor, patch), and
`ensure_upgrade`, and proves:

* what `ensure_upgrade` lets through is strictly newer than what runs;
* the order is a strict total order, so successive updates only ever climb,
  and no version is ever installed twice, the running one included;
* the version Clew writes (`Display`, `x.y.z`) and the tag it publishes
  (`vx.y.z`) read back as that version.
-/

namespace Clew.Update

open Clew.Text

/-- `Version`. -/
structure Version where
  major : Nat
  minor : Nat
  patch : Nat
  deriving DecidableEq, Repr

/-- `a < b` as the derived `Ord` decides it: field by field, in order. -/
def Version.lt (a b : Version) : Bool :=
  a.major < b.major || (a.major == b.major &&
    (a.minor < b.minor || (a.minor == b.minor && a.patch < b.patch)))

/-- `Version::parse`: trimmed, any leading `v`s dropped, everything from the
first `-` or `+` ignored, then three `.`-separated `u64`s, each trimmed;
pieces past the third are ignored. -/
def parse (s : List Char) : Option Version :=
  let s := (trim s).dropWhile (· == 'v')
  let core := (split (fun c => c == '-' || c == '+') s).headD []
  match split (· == '.') core with
  | a :: b :: c :: _ => do
    let major ← parseU64 (trim a)
    let minor ← parseU64 (trim b)
    let patch ← parseU64 (trim c)
    pure ⟨major, minor, patch⟩
  | _ => none

/-- Why `ensure_upgrade` refused. -/
inductive Refusal where
  | unreadable
  | notNewer
  deriving DecidableEq, Repr

/-- `ensure_upgrade`: the candidate must parse and be strictly newer. -/
def ensureUpgrade (running : Version) (candidate : List Char) : Except Refusal Version :=
  match parse candidate with
  | none => .error .unreadable
  | some v => if running.lt v then .ok v else .error .notNewer

/-- `Display`: `major.minor.patch`. -/
def display (v : Version) : List Char :=
  decimal v.major ++ ('.' :: (decimal v.minor ++ ('.' :: decimal v.patch)))

/-! ## The order -/

theorem lt_irrefl (a : Version) : a.lt a = false := by
  simp [Version.lt]

theorem lt_trans {a b c : Version} (hab : a.lt b = true) (hbc : b.lt c = true) :
    a.lt c = true := by
  simp only [Version.lt, Bool.or_eq_true, Bool.and_eq_true, decide_eq_true_eq,
    beq_iff_eq] at *
  omega

theorem lt_asymm {a b : Version} (hab : a.lt b = true) : b.lt a = false := by
  simp only [Version.lt, Bool.or_eq_true, Bool.and_eq_true, decide_eq_true_eq,
    beq_iff_eq, Bool.or_eq_false_iff, Bool.and_eq_false_iff, decide_eq_false_iff_not,
    beq_eq_false_iff_ne, ne_eq] at *
  omega

theorem lt_total (a b : Version) (hne : a ≠ b) : a.lt b = true ∨ b.lt a = true := by
  have hfields : a.major ≠ b.major ∨ a.minor ≠ b.minor ∨ a.patch ≠ b.patch := by
    cases a; cases b; simp_all; omega
  simp only [Version.lt, Bool.or_eq_true, Bool.and_eq_true, decide_eq_true_eq, beq_iff_eq]
  omega

/-! ## What `ensure_upgrade` guarantees -/

/-- **No downgrade, no reinstall.** An update `ensure_upgrade` accepts is
strictly newer than the running version. -/
theorem accepted_is_newer (running : Version) (candidate : List Char) (v : Version)
    (h : ensureUpgrade running candidate = .ok v) : running.lt v = true := by
  unfold ensureUpgrade at h
  split at h
  · cases h
  · split at h
    · rename_i hlt; cases h; exact hlt
    · cases h

/-- What each of a run of updates installs, starting from `running`, when
every one is accepted; `none` as soon as one is refused. -/
def installAll : Version → List (List Char) → Option (List Version)
  | _, [] => some []
  | running, c :: cs =>
    match ensureUpgrade running c with
    | .ok v => (installAll v cs).map (v :: ·)
    | .error _ => none

theorem installAll_chain : ∀ (cs : List (List Char)) (running : Version) (vs : List Version),
    installAll running cs = some vs → (running :: vs).Pairwise (fun a b => a.lt b = true) := by
  intro cs
  induction cs with
  | nil => intro running vs h; simp [installAll] at h; subst h; simp
  | cons c cs ih =>
    intro running vs h
    unfold installAll at h
    split at h
    · rename_i v hv
      cases hrest : installAll v cs with
      | none => simp [hrest] at h
      | some ws =>
        simp only [hrest, Option.map_some] at h
        cases h
        have hchain := ih v ws hrest
        have hnew := accepted_is_newer running c v hv
        refine List.Pairwise.cons ?_ hchain
        intro w hw
        rcases List.mem_cons.mp hw with rfl | hw'
        · exact hnew
        · exact lt_trans hnew (List.rel_of_pairwise_cons hchain hw')
    · cases h

/-- **Updates only climb.** However many updates are installed in a row,
each is newer than every one before it and than the version that ran first —
so no version, the running one included, is ever installed twice. -/
theorem installs_never_repeat (running : Version) (cs : List (List Char)) (vs : List Version)
    (h : installAll running cs = some vs) : (running :: vs).Nodup := by
  have hc := installAll_chain cs running vs h
  refine List.Pairwise.imp ?_ hc
  intro a b hab heq
  subst heq
  simp [lt_irrefl] at hab

/-! ## What Clew writes reads back -/

theorem decimal_no_sep (n : Nat) (sep : Char → Bool) (hs : ∀ c, isAsciiDigit c = true → sep c = false) :
    ∀ c ∈ decimal n, sep c = false :=
  fun c hc => hs c (decimal_digits n c hc)

theorem trim_decimal (n : Nat) : trim (decimal n) = decimal n :=
  trim_of_no_whitespace _ (fun c hc => digit_not_whitespace c (decimal_digits n c hc))

theorem digit_not_dot (c : Char) (h : isAsciiDigit c = true) : (c == '.') = false := by
  simp only [isAsciiDigit, Bool.and_eq_true, decide_eq_true_eq] at h
  have : c ≠ '.' := by intro he; subst he; simp at h
  simpa using this

theorem digit_not_sign (c : Char) (h : isAsciiDigit c = true) : (c == '-' || c == '+') = false := by
  simp only [isAsciiDigit, Bool.and_eq_true, decide_eq_true_eq] at h
  have h1 : c ≠ '-' := by intro he; subst he; simp at h
  have h2 : c ≠ '+' := by intro he; subst he; simp at h
  simp [h1, h2]

theorem display_chars (v : Version) :
    ∀ c ∈ display v, isAsciiDigit c = true ∨ c = '.' := by
  intro c hc
  by_cases hdot : c = '.'
  · exact .inr hdot
  · left
    simp only [display, List.mem_append, List.mem_cons, hdot, false_or] at hc
    rcases hc with h | h | h
    · exact decimal_digits _ c h
    · exact decimal_digits _ c h
    · exact decimal_digits _ c h

/-- **What Clew displays, it reads back.** For every version whose fields
fit in 64 bits, `Version::parse` of its `Display` form is that version. -/
theorem parse_display (v : Version) (h1 : v.major < 2 ^ 64) (h2 : v.minor < 2 ^ 64)
    (h3 : v.patch < 2 ^ 64) : parse (display v) = some v := by
  have hnows : ∀ c ∈ display v, isWhitespace c = false := by
    intro c hc
    rcases display_chars v c hc with h | h
    · exact digit_not_whitespace c h
    · subst h; decide
  have htrim : trim (display v) = display v := trim_of_no_whitespace _ hnows
  have hnov : (display v).dropWhile (· == 'v') = display v := by
    apply dropWhile_of_head
    intro c hc
    rcases display_chars v c (List.mem_of_mem_head? hc) with h | h
    · simp only [isAsciiDigit, Bool.and_eq_true, decide_eq_true_eq] at h
      have : c ≠ 'v' := by intro he; subst he; simp at h
      simpa using this
    · subst h; decide
  have hcore : split (fun c => c == '-' || c == '+') (display v) = [display v] := by
    apply split_none
    intro c hc
    rcases display_chars v c hc with h | h
    · exact digit_not_sign c h
    · subst h; decide
  have hdots : split (· == '.') (display v) = [decimal v.major, decimal v.minor, decimal v.patch] := by
    unfold display
    rw [split_at_sep (· == '.') (decimal v.major) '.' _ (decimal_no_sep _ _ digit_not_dot) rfl,
      split_at_sep (· == '.') (decimal v.minor) '.' _ (decimal_no_sep _ _ digit_not_dot) rfl,
      split_none (· == '.') _ (decimal_no_sep _ _ digit_not_dot)]
  unfold parse
  simp only [htrim, hnov, hcore, List.headD_cons, hdots, trim_decimal,
    parseU64_decimal _ h1, parseU64_decimal _ h2, parseU64_decimal _ h3]
  rfl

/-- **The published tag reads back too**: `vx.y.z`, as each release is
tagged. -/
theorem parse_tag (v : Version) (h1 : v.major < 2 ^ 64) (h2 : v.minor < 2 ^ 64)
    (h3 : v.patch < 2 ^ 64) : parse ('v' :: display v) = some v := by
  have hd := parse_display v h1 h2 h3
  have hws : isWhitespace 'v' = false := by decide
  have hnows : ∀ c ∈ display v, isWhitespace c = false := by
    intro c hc
    rcases display_chars v c hc with h | h
    · exact digit_not_whitespace c h
    · subst h; decide
  have htrim : trim ('v' :: display v) = 'v' :: display v := by
    apply trim_of_no_whitespace
    intro c hc
    rcases List.mem_cons.mp hc with rfl | hc
    · exact hws
    · exact hnows c hc
  have hdrop : ('v' :: display v).dropWhile (· == 'v') = (display v).dropWhile (· == 'v') := by
    simp [List.dropWhile]
  unfold parse at hd ⊢
  simp only [htrim, hdrop]
  simpa [trim_of_no_whitespace _ hnows] using hd

end Clew.Update
