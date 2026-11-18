import Clew.Text

/-!
# Path confinement — `clew_core::confine`

A project-relative path arrives from outside — a client request, a tool
argument a model wrote — and is resolved against the project root. Its
lexical half, `check_lexical`, refuses a path whose shape alone could leave
the root; `confine` then resolves symlinks and checks the result is still
under the canonical root. This file models the lexical half and proves what
it guarantees: **every path it accepts, resolved one piece at a time against
any root, stays under that root at every step.** The symlink half depends on
the filesystem and is outside what a model can settle; it is the second
line, re-checked on the canonical path.

The model is of Unix paths, which is where both ends of Clew run (the
server on Linux, the app on macOS and Linux).
-/

namespace Clew.Confine

open Clew.Text

/-- `ConfineError`'s lexical cases. -/
inductive Error where
  | empty
  | absolute
  | traversal
  deriving DecidableEq, Repr

/-- The pieces of a path between its `/` separators. `Path::components`
reads them the same way: an empty piece or a `.` past the first is nothing,
`..` is the parent, anything else a name. -/
def segments (rel : List Char) : List (List Char) := split (· == '/') rel

/-- `check_lexical`: empty (after trimming Unicode whitespace) first, then a
leading `/` (`Path::is_absolute` on Unix), then any `..` piece. -/
def checkLexical (rel : List Char) : Except Error Unit :=
  if (trim rel).isEmpty then .error .empty
  else if rel.head? == some '/' then .error .absolute
  else if (segments rel).any (· == ['.', '.']) then .error .traversal
  else .ok ()

/-- `names_a_folder`: the last piece is empty or `.` (`src/`, `src/.`, `.`). -/
def namesAFolder (rel : List Char) : Bool :=
  match (segments rel).getLast? with
  | some [] => true
  | some ['.'] => true
  | _ => false

/-- Lexical resolution of pieces onto a directory stack — what the OS does
to a path when no symlink is involved: an empty piece or `.` stays, `..`
goes up one, a name goes down into it. -/
def resolve : List (List Char) → List (List Char) → List (List Char)
  | stack, [] => stack
  | stack, s :: ss =>
    if s == [] || s == ['.'] then resolve stack ss
    else if s == ['.', '.'] then resolve stack.dropLast ss
    else resolve (stack ++ [s]) ss

/-- Resolving pieces that hold no `..` only ever goes down: whatever the
stack started with, it still starts with. -/
theorem resolve_keeps_prefix (base : List (List Char)) :
    ∀ (segs : List (List Char)) (stack : List (List Char)),
      (∀ s ∈ segs, s ≠ ['.', '.']) → base <+: stack → base <+: resolve stack segs := by
  intro segs
  induction segs with
  | nil => intro stack _ hp; simpa [resolve] using hp
  | cons s ss ih =>
    intro stack hno hp
    have hs : s ≠ ['.', '.'] := hno s (by simp)
    have hrest : ∀ t ∈ ss, t ≠ ['.', '.'] := fun t ht => hno t (by simp [ht])
    unfold resolve
    by_cases h1 : (s == [] || s == ['.']) = true
    · simp only [h1, ite_true]
      exact ih stack hrest hp
    · have h2 : (s == ['.', '.']) = false := by simpa using hs
      simp only [Bool.not_eq_true] at h1
      simp only [h1, h2, Bool.false_eq_true, ite_false]
      exact ih (stack ++ [s]) hrest (List.IsPrefix.trans hp (List.prefix_append stack [s]))

/-- What `check_lexical` accepts holds no `..` piece. -/
theorem accepted_has_no_parent (rel : List Char) (h : checkLexical rel = .ok ()) :
    ∀ s ∈ segments rel, s ≠ ['.', '.'] := by
  unfold checkLexical at h
  split at h
  · cases h
  · split at h
    · cases h
    · split at h
      · cases h
      · rename_i hany
        intro s hs heq
        apply hany
        rw [List.any_eq_true]
        exact ⟨s, hs, by simp [heq]⟩

/-- **Confinement.** A path `check_lexical` accepts, resolved against any
root, ends under that root. -/
theorem accepted_stays_inside (rel : List Char) (h : checkLexical rel = .ok ())
    (root : List (List Char)) : root <+: resolve root (segments rel) :=
  resolve_keeps_prefix root (segments rel) root (accepted_has_no_parent rel h)
    (List.prefix_refl root)

/-- **Confinement at every step.** Not only the end: each piece of an
accepted path, resolved in turn, leaves the walk under the root — there is
no moment at which it stands outside. -/
theorem accepted_never_leaves (rel : List Char) (h : checkLexical rel = .ok ())
    (root : List (List Char)) (k : Nat) :
    root <+: resolve root ((segments rel).take k) := by
  apply resolve_keeps_prefix root _ root _ (List.prefix_refl root)
  intro s hs
  exact accepted_has_no_parent rel h s (List.mem_of_mem_take hs)

/-- The refusals are exactly the documented ones, in their order: what is
empty is refused as empty, a leading `/` as absolute, a `..` as traversal —
and nothing else is refused. -/
theorem checkLexical_spec (rel : List Char) :
    checkLexical rel = .ok () ↔
      (trim rel ≠ [] ∧ rel.head? ≠ some '/' ∧ ∀ s ∈ segments rel, s ≠ ['.', '.']) := by
  constructor
  · intro h
    refine ⟨?_, ?_, accepted_has_no_parent rel h⟩
    · intro he; simp [checkLexical, he] at h
    · intro ha
      unfold checkLexical at h
      split at h
      · cases h
      · simp [ha] at h
  · rintro ⟨hne, hnr, hnp⟩
    unfold checkLexical
    have e1 : (trim rel).isEmpty = false := by
      cases ht : trim rel with
      | nil => exact absurd ht hne
      | cons _ _ => rfl
    have e2 : (rel.head? == some '/') = false := by simpa using hnr
    have e3 : (segments rel).any (· == ['.', '.']) = false := by
      rw [List.any_eq_false]
      intro s hs hb
      exact hnp s hs (by simpa using hb)
    simp [e1, e2, e3]

end Clew.Confine
