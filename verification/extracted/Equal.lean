import ClewRules
import Clew.VoiceRoute
import Clew.Guards

/-!
# The code that runs is the model

`ClewRules.lean` is what Charon and Aeneas make of `crates/clew-rules`, the
crate the app calls for these decisions: the Rust source as Lean, in
Aeneas's `Result` monad, where a panic or an overflow is `fail`. Each
theorem here says that the extracted function always returns (`ok`: no
panic, no overflow, no out-of-bounds index, the loop ends) and returns
exactly what the model in `../Clew` computes. So every theorem proved of a
model holds of the code that runs.
-/

open Aeneas Aeneas.Std Result

namespace Clew.Extracted

/-! ## Where heard words go — `clew_rules::voice::route` -/

namespace Voice

open clew_rules.voice

def questionWord : QuestionWord → Clew.VoiceRoute.QuestionWord
  | .YesOrNo => .yesOrNo
  | .Answer => .answer

/-- The facts the Rust function is given, as the model's. -/
def facts (f : RouteFacts) : Clew.VoiceRoute.Facts where
  bye := f.bye
  stopListeningSaid := f.stop_listening_said
  empty := f.empty
  confirmWaiting := f.confirm_waiting
  hush := f.hush
  yesOrNo := f.yes_or_no
  toQuestion := f.to_question.map questionWord
  answerBeingWritten := f.answer_being_written
  runCommands := f.run_commands

def step : Step → Clew.VoiceRoute.Step
  | .Bye => .bye
  | .SayHowToStop => .sayHowToStop
  | .Nothing => .nothing
  | .RunWaiting => .runWaiting
  | .DeclineWaiting => .declineWaiting
  | .Hush => .hush
  | .TryCommand => .tryCommand
  | .TryCommandsBackOn => .tryCommandsBackOn
  | .Hold => .hold
  | .Ask => .ask

theorem only_ok (s : Step) :
    clew_rules.voice.only s = ok (alloc.vec.Vec.from [s] (by scalar_tac)) := by
  simp only [clew_rules.voice.only, alloc.vec.Vec.push]
  split
  · rfl
  · simp_all [U32.max_eq]

theorem push_ok (a b : Step) (h) :
    (alloc.vec.Vec.from [a] h).push b = ok (alloc.vec.Vec.from [a, b] (by scalar_tac)) := by
  simp only [alloc.vec.Vec.push]
  split
  · rfl
  · simp_all [U32.max_eq]

/-- **`route` is the model**: on every combination of facts (1152) the Rust
function returns, and returns the model's steps. -/
theorem route_is_the_model (f : RouteFacts) :
    ∃ steps, route f = ok steps ∧ steps.val.map step = Clew.VoiceRoute.route (facts f) := by
  obtain ⟨bye, stop, empty, confirm, hush, yn, tq, writing, run⟩ := f
  cases bye <;> cases stop <;> cases empty <;> cases confirm <;> cases hush <;>
    rcases yn with _ | ⟨_ | _⟩ <;> rcases tq with _ | ⟨_ | _⟩ <;> cases writing <;> cases run <;>
    (simp only [route, only_ok, push_ok, Bool.false_eq_true, ↓reduceIte, bind_ok]
     try simp only [Result.ok.injEq, exists_eq_left', alloc.vec.Vec.from_val]
     try simp
     try decide)

end Voice

/-! ## A program Clew runs where it is — `clew_rules::location::trusted_by` -/

namespace Location

open clew_rules.location

/-- What the Rust function reads of a node, as the model's. -/
def node (n : Node) : Clew.Guards.Node := ⟨n.uid.val, n.mode.val, n.kind_ok⟩

theorem land_eq_zero_iff (m k : Nat) :
    m &&& k = 0 ↔ ∀ i, k.testBit i = true → m.testBit i = false := by
  constructor
  · intro h i hk
    have := congrArg (·.testBit i) h
    simp only [Nat.testBit_and, hk, Bool.and_true, Nat.zero_testBit] at this
    exact this
  · intro h
    apply Nat.eq_of_testBit_eq
    intro i
    rw [Nat.testBit_and, Nat.zero_testBit]
    cases hk : k.testBit i
    · simp
    · simp [h i hk]

theorem testBit_eq (m i : Nat) : m.testBit i = (m / 2 ^ i % 2 == 1) := by
  rw [Nat.testBit_eq_decide_div_mod_eq]; rfl

/-- `mode & 0o022 == 0`: neither the group's nor others' write bit. -/
theorem closed_bits (m : Nat) : m &&& 18 = 0 ↔ (m / 16 % 2 = 0 ∧ m / 2 % 2 = 0) := by
  rw [land_eq_zero_iff]
  constructor
  · intro h
    have h1 := h 1 (by decide)
    have h4 := h 4 (by decide)
    rw [testBit_eq] at h1 h4
    simp at h1 h4
    omega
  · intro ⟨h4, h1⟩ i hk
    by_cases hi : 5 ≤ i
    · rw [Nat.testBit_lt_two_pow (Nat.lt_of_lt_of_le (by decide : 18 < 2 ^ 5)
        (Nat.pow_le_pow_right (by decide) hi))] at hk
      cases hk
    · have : i = 0 ∨ i = 1 ∨ i = 2 ∨ i = 3 ∨ i = 4 := by omega
      rcases this with rfl | rfl | rfl | rfl | rfl
      all_goals first | exact absurd hk (by decide) | (rw [testBit_eq]; simp; omega)

/-- `mode & 0o1000 != 0`: the sticky bit. -/
theorem sticky_bit (m : Nat) : m &&& 512 = 0 ↔ m / 512 % 2 = 0 := by
  rw [land_eq_zero_iff]
  constructor
  · intro h; have h9 := h 9 (by decide); rw [testBit_eq] at h9; simp at h9; omega
  · intro h i hk
    by_cases hi : i = 9
    · subst hi; rw [testBit_eq]; simp; omega
    · rw [show (512 : Nat) = 2 ^ 9 by rfl, Nat.testBit_two_pow] at hk
      simp at hk; omega

/-- A spec met is a value returned. -/
theorem ok_of_spec {α : Type} {m : Result α} {p : α → Prop} (h : WP.spec m p) :
    ∃ v, m = ok v ∧ p v := by
  cases m <;> simp_all

theorem owned_ok (me : Std.U32) (n : Node) :
    owned me n = ok (n.kind_ok && Clew.Guards.owned me.val (node n)) := by
  obtain ⟨uid, mode, k⟩ := n
  simp only [owned, node, Clew.Guards.owned, UScalar.eq_equiv]
  cases k <;> simp
  by_cases h : uid.val = me.val
  · simp [h]
  · simp only [h, beq_false_of_ne h, Bool.false_or]
    cases hz : decide (uid.val = 0) <;> simp_all

theorem closed_ok (n : Node) :
    closed n = ok (!Clew.Guards.groupWrite n.mode.val && !Clew.Guards.otherWrite n.mode.val) := by
  simp only [closed, lift, UScalar.eq_equiv, Clew.Guards.groupWrite, Clew.Guards.otherWrite]
  have := closed_bits n.mode.val
  by_cases h : n.mode.val &&& 18 = 0 <;> simp_all

theorem sticky_ok (n : Node) : sticky n = ok (Clew.Guards.sticky n.mode.val) := by
  simp only [sticky, lift, Clew.Guards.sticky]
  have := sticky_bit n.mode.val
  by_cases h : n.mode.val &&& 512 = 0 <;> simp_all [UScalar.eq_equiv]

/-- The model's test of one directory above the file. -/
def dirOk (me : Nat) (d : Clew.Guards.Node) : Bool :=
  d.kindOk && Clew.Guards.owned me d &&
    (Clew.Guards.sticky d.mode || (!Clew.Guards.groupWrite d.mode && !Clew.Guards.otherWrite d.mode))

theorem body_end (me : Std.U32) (dirs : Slice Node) (i : Std.Usize)
    (hi : ¬ i.val < dirs.val.length) :
    trusted_by_loop.body me dirs i = ok (.done true) := by
  simp [trusted_by_loop.body, hi]

theorem body_stop (me : Std.U32) (dirs : Slice Node) (i : Std.Usize)
    (hi : i.val < dirs.val.length) (hd : dirOk me.val (node dirs.val[i.val]) = false) :
    trusted_by_loop.body me dirs i = ok (.done false) := by
  have hget : Slice.index_usize dirs i = ok dirs.val[i.val] := by simp [Slice.index_usize, hi]
  simp only [dirOk, node, Clew.Guards.owned, Bool.and_eq_false_iff, Bool.or_eq_false_iff] at hd
  simp only [trusted_by_loop.body, Slice.len, UScalar.lt_equiv, hget, bind_ok, owned_ok, sticky_ok,
    closed_ok, node, Clew.Guards.owned]
  rw [ite_eq_left_of_eq_true _ _ (by simpa using hi)]
  split_ifs <;> simp_all

theorem body_next (me : Std.U32) (dirs : Slice Node) (i : Std.Usize)
    (hi : i.val < dirs.val.length) (hd : dirOk me.val (node dirs.val[i.val]) = true) :
    ∃ j : Std.Usize, j.val = i.val + 1 ∧ trusted_by_loop.body me dirs i = ok (.cont j) := by
  have hget : Slice.index_usize dirs i = ok dirs.val[i.val] := by simp [Slice.index_usize, hi]
  obtain ⟨j, hj, hjv⟩ : ∃ j : Std.Usize, i + 1#usize = ok j ∧ j.val = i.val + 1 := by
    have := Slice.length_ineq dirs
    exact ok_of_spec (UScalar.add_spec (x := i) (y := 1#usize) (by scalar_tac))
  refine ⟨j, hjv, ?_⟩
  simp only [dirOk, node, Clew.Guards.owned, Bool.and_eq_true, Bool.or_eq_true] at hd
  simp only [trusted_by_loop.body, Slice.len, UScalar.lt_equiv, hget, bind_ok, owned_ok, sticky_ok,
    closed_ok, node, Clew.Guards.owned]
  rw [ite_eq_left_of_eq_true _ _ (by simpa using hi)]
  split_ifs <;> simp_all

/-- The loop over the directories, from the `i`-th on, is the model's test
of every one of them. -/
theorem trusted_by_loop_ok (me : Std.U32) (dirs : Slice Node) (i : Std.Usize) :
    trusted_by_loop me dirs i = ok ((dirs.val.drop i.val).all fun d => dirOk me.val (node d)) := by
  unfold trusted_by_loop
  rw [loop]
  by_cases hi : i.val < dirs.val.length
  · rw [List.drop_eq_getElem_cons hi, List.all_cons]
    cases hd : dirOk me.val (node dirs.val[i.val])
    · simp [body_stop me dirs i hi hd]
    · obtain ⟨j, hj, hb⟩ := body_next me dirs i hi hd
      have ih := trusted_by_loop_ok me dirs j
      unfold trusted_by_loop at ih
      simp only [hb, bind_ok, Bool.true_and]
      rw [ih, hj]
  · have : dirs.val.drop i.val = [] := List.drop_eq_nil_of_le (by omega)
    simp [body_end me dirs i hi, this]
termination_by dirs.val.length - i.val
decreasing_by omega

/-- **`trusted_by` is the model**: for every file and every list of
directories the Rust function returns — no index out of bounds, no
overflow, the loop ends — and says what the model says. -/
theorem trusted_by_is_the_model (me : Std.U32) (file : Node) (dirs : Slice Node) :
    trusted_by me file dirs = ok (Clew.Guards.trustedBy me.val (node file) (dirs.val.map node)) := by
  have hl := trusted_by_loop_ok me dirs 0#usize
  have h0 : (0#usize).val = 0 := by simp
  rw [h0, List.drop_zero] at hl
  simp only [trusted_by, owned_ok, closed_ok, bind_ok, Clew.Guards.trustedBy, List.all_map]
  cases hk : file.kind_ok <;> cases ho : Clew.Guards.owned me.val (node file) <;>
    cases hg : Clew.Guards.groupWrite file.mode.val <;> cases hw : Clew.Guards.otherWrite file.mode.val <;>
    simp_all [node, dirOk, Function.comp_def]

end Location

end Clew.Extracted
