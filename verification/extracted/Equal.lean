import ClewRules
import Clew.VoiceRoute
import Clew.Guards
import Clew.Framing
import Clew.Ssh
import Clew.ServerDist
import Clew.Trust
import Clew.Confine
import Clew.Update

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

/-! ## What the proofs share -/

/-- A spec met is a value returned. -/
theorem ok_of_spec {α : Type} {m : Result α} {p : α → Prop} (h : WP.spec m p) :
    ∃ v, m = ok v ∧ p v := by
  cases m <;> simp_all

/-- A loop returns, with a result meeting `Q`, when every step from a state
meeting `inv` either stops with such a result or goes on to a state meeting
`inv`, smaller by `μ`, from which any result meeting `Q` also meets it here. -/
theorem loop_spec {α β : Type} (body : α → Result (ControlFlow α β)) (inv : α → Prop)
    (Q : α → β → Prop) (μ : α → Nat)
    (h : ∀ s, inv s → (∃ b, body s = ok (.done b) ∧ Q s b) ∨
      ∃ s', body s = ok (.cont s') ∧ inv s' ∧ μ s' < μ s ∧ ∀ b, Q s' b → Q s b) :
    ∀ s, inv s → ∃ b, loop body s = ok b ∧ Q s b := by
  intro s
  induction hm : μ s using Nat.strong_induction_on generalizing s with
  | _ n ih =>
    intro hs
    rw [loop]
    rcases h s hs with ⟨b, hb, hq⟩ | ⟨s', hc, hs', hlt, hq⟩
    · exact ⟨b, by rw [hb]; simp, hq⟩
    · obtain ⟨b, hl, hb⟩ := ih (μ s') (hm ▸ hlt) s' rfl hs'
      exact ⟨b, by rw [hc]; simpa using hl, hq b hb⟩

theorem index_ok {α : Type} (line : Slice α) (i : Std.Usize) (hi : i.val < line.val.length) :
    Slice.index_usize line i = ok line.val[i.val] := by
  simp [Slice.index_usize, hi]

theorem succ_ok {α : Type} (line : Slice α) (i : Std.Usize) (hi : i.val < line.val.length) :
    ∃ j : Std.Usize, i + 1#usize = ok j ∧ j.val = i.val + 1 := by
  have := Slice.length_ineq line
  exact ok_of_spec (UScalar.add_spec (x := i) (y := 1#usize) (by scalar_tac))


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

/-! ## One header line of a framed message — `clew_rules::framing::header`

`read_frame` reads a message's header lines and calls `header` on each:
whether the line is a header at all, whether it is `Content-Length`, and
what that length is. Proved here: for every line and every length read so
far, the Rust function returns, and returns what the model's `header` in
`../Clew/Framing.lean` returns. -/

namespace Framing

open clew_rules.framing
theorem ite_ok {α : Type} (c : Prop) [Decidable c] (a b : α) :
    (if c then ok a else ok b : Result α) = ok (if c then a else b) := by
  split <;> rfl

theorem u8_lt (b : Std.U8) : b.val < 256 := by have := b.hBounds; simpa using this

theorem is_token_byte_ok (b : Std.U8) : is_token_byte b = ok (Clew.Framing.isTokenByte b.val) := by
  unfold is_token_byte
  simp only [ge_iff_le, UScalar.le_equiv, UScalar.eq_equiv, U8.ofNat, UScalar.ofNat,
    UScalar.ofNatCore_val_eq, ite_ok, Result.ok.injEq]
  have hb := u8_lt b
  generalize b.val = v at hb ⊢
  revert v; decide +kernel

theorem is_ascii_ws_ok (b : Std.U8) : is_ascii_ws b = ok (Clew.Framing.isAsciiWs b.val) := by
  unfold is_ascii_ws
  simp only [UScalar.eq_equiv, U8.ofNat, UScalar.ofNat, UScalar.ofNatCore_val_eq, ite_ok,
    Result.ok.injEq]
  have hb := u8_lt b
  generalize b.val = v at hb ⊢
  revert v; decide +kernel

/-- What `all_ascii` decides: every byte below 128. -/
theorem all_ascii_ok (line : Slice Std.U8) :
    all_ascii line = ok (line.val.all fun b => b.val < 128) := by
  obtain ⟨b, hl, hq⟩ := loop_spec (all_ascii_loop.body line) (fun _ => True)
    (fun i b => b = (line.val.drop i.val).all fun b => b.val < 128)
    (fun i => line.val.length - i.val) (by
      intro i _
      by_cases hi : i.val < line.val.length
      · obtain ⟨j, hj, hjv⟩ := succ_ok line i hi
        rw [List.drop_eq_getElem_cons hi]
        by_cases hb : 128 ≤ line.val[i.val].val
        · left
          refine ⟨false, ?_, ?_⟩
          · simp [all_ascii_loop.body, hi, index_ok line i hi, hb]
          · simp only [List.all_cons, Bool.false_eq, Bool.and_eq_false_iff, decide_eq_false_iff_not]
            left; omega
        · right
          refine ⟨j, ?_, trivial, by omega, ?_⟩
          · simp [all_ascii_loop.body, hi, index_ok line i hi, hb, hj]
          · intro b hb'
            rw [hb', hjv, List.all_cons]
            simp only [show line.val[i.val].val < 128 by omega, decide_true, Bool.true_and]
      · left
        refine ⟨true, by simp [all_ascii_loop.body, hi], ?_⟩
        rw [List.drop_eq_nil_of_le (by omega)]; rfl) 0#usize trivial
  unfold all_ascii all_ascii_loop
  rw [hl, hq]; simp

/-- What `all_token` decides: `line[..e]` is all token bytes. -/
theorem all_token_ok (line : Slice Std.U8) (e : Std.Usize) (he : e.val ≤ line.val.length) :
    all_token line e = ok ((line.val.take e.val).all fun b => Clew.Framing.isTokenByte b.val) := by
  obtain ⟨b, hl, hq⟩ := loop_spec (all_token_loop.body line e) (fun _ => True)
    (fun i b => b = ((line.val.take e.val).drop i.val).all fun b => Clew.Framing.isTokenByte b.val)
    (fun i => e.val - i.val) (by
      intro i _
      by_cases hi : i.val < e.val
      · have hil : i.val < line.val.length := by omega
        obtain ⟨j, hj, hjv⟩ := succ_ok line i hil
        have hlen : i.val < (line.val.take e.val).length := by simp; omega
        rw [List.drop_eq_getElem_cons hlen, List.getElem_take]
        cases ht : Clew.Framing.isTokenByte line.val[i.val].val
        · left
          exact ⟨false, by simp [all_token_loop.body, hi, index_ok line i hil, is_token_byte_ok, ht],
            by simp [ht]⟩
        · right
          refine ⟨j, by simp [all_token_loop.body, hi, index_ok line i hil, is_token_byte_ok, ht, hj],
            trivial, by omega, ?_⟩
          intro b hb; rw [hb, hjv]; simp [ht]
      · left
        refine ⟨true, by simp [all_token_loop.body, hi], ?_⟩
        rw [List.drop_eq_nil_of_le (by simp; omega)]; rfl) 0#usize trivial
  unfold all_token all_token_loop
  rw [hl, hq]; simp

/-- What `all_digits` decides: `line[from..to]` is all ASCII digits. -/
theorem all_digits_ok (line : Slice Std.U8) (f t : Std.Usize) (ht : t.val ≤ line.val.length) :
    all_digits line f t =
      ok (((line.val.take t.val).drop f.val).all fun b => Clew.Framing.isAsciiDigitB b.val) := by
  obtain ⟨b, hl, hq⟩ := loop_spec (all_digits_loop.body line t) (fun _ => True)
    (fun i b => b = ((line.val.take t.val).drop i.val).all fun b => Clew.Framing.isAsciiDigitB b.val)
    (fun i => t.val - i.val) (by
      intro i _
      by_cases hi : i.val < t.val
      · have hil : i.val < line.val.length := by omega
        obtain ⟨j, hj, hjv⟩ := succ_ok line i hil
        have hlen : i.val < (line.val.take t.val).length := by simp; omega
        rw [List.drop_eq_getElem_cons hlen, List.getElem_take]
        cases hd : Clew.Framing.isAsciiDigitB line.val[i.val].val
        · left
          simp only [Clew.Framing.isAsciiDigitB, Bool.and_eq_false_iff, decide_eq_false_iff_not] at hd
          exact ⟨false, by simp [all_digits_loop.body, hi, index_ok line i hil, U8.ofNat, UScalar.ofNat, UScalar.ofNatCore_val_eq]; omega,
            by simp [Clew.Framing.isAsciiDigitB]; omega⟩
        · right
          simp only [Clew.Framing.isAsciiDigitB, Bool.and_eq_true, decide_eq_true_eq] at hd
          have hbody : all_digits_loop.body line t i = ok (.cont j) := by
            simp [all_digits_loop.body, hi, index_ok line i hil, hj, U8.ofNat, UScalar.ofNat, UScalar.ofNatCore_val_eq]
            simp only [show ¬ (line.val[i.val].val < 48) by omega,
              show ¬ (57 < line.val[i.val].val) by omega, ↓reduceIte]
          refine ⟨j, hbody, trivial, by omega, ?_⟩
          intro b hb; rw [hb, hjv]; simp [Clew.Framing.isAsciiDigitB]; omega
      · left
        refine ⟨true, by simp [all_digits_loop.body, hi], ?_⟩
        rw [List.drop_eq_nil_of_le (by simp; omega)]; rfl) f trivial
  unfold all_digits all_digits_loop
  rw [hl, hq]

/-- What `find_colon` finds: the length of the run before the first `:`. -/
theorem find_colon_ok (line : Slice Std.U8) :
    ∃ c, find_colon line = ok c ∧ c.val = (line.val.takeWhile fun b => b.val ≠ 58).length := by
  obtain ⟨c, hl, hq⟩ := loop_spec (find_colon_loop.body line) (fun _ => True)
    (fun i c => c.val = i.val + ((line.val.drop i.val).takeWhile fun b => b.val ≠ 58).length)
    (fun i => line.val.length - i.val) (by
      intro i _
      by_cases hi : i.val < line.val.length
      · obtain ⟨j, hj, hjv⟩ := succ_ok line i hi
        rw [List.drop_eq_getElem_cons hi, List.takeWhile_cons]
        by_cases hc : line.val[i.val].val = 58
        · left
          exact ⟨i, by simp [find_colon_loop.body, hi, index_ok line i hi, UScalar.eq_equiv, hc, U8.ofNat, UScalar.ofNat, UScalar.ofNatCore_val_eq],
            by simp [hc]⟩
        · right
          refine ⟨j, by simp [find_colon_loop.body, hi, index_ok line i hi, UScalar.eq_equiv, hc, hj, U8.ofNat, UScalar.ofNat, UScalar.ofNatCore_val_eq],
            trivial, by omega, ?_⟩
          intro c hcv; rw [hcv, hjv]; simp only [hc, ne_eq, not_false_eq_true, decide_true, ite_true, List.length_cons]; omega
      · left
        refine ⟨i, by simp [find_colon_loop.body, hi], ?_⟩
        rw [List.drop_eq_nil_of_le (by omega)]; simp) 0#usize trivial
  unfold find_colon find_colon_loop
  exact ⟨c, hl, by rw [hq]; simp⟩

/-- Where `skip_ws` stops: past the run of ASCII whitespace from `f`. -/
theorem skip_ws_ok (line : Slice Std.U8) (f : Std.Usize) :
    ∃ r, skip_ws line f = ok r ∧
      r.val = f.val + ((line.val.drop f.val).takeWhile fun b => Clew.Framing.isAsciiWs b.val).length := by
  obtain ⟨c, hl, hq⟩ := loop_spec (skip_ws_loop.body line) (fun _ => True)
    (fun i c => c.val = i.val + ((line.val.drop i.val).takeWhile fun b => Clew.Framing.isAsciiWs b.val).length)
    (fun i => line.val.length - i.val) (by
      intro i _
      by_cases hi : i.val < line.val.length
      · obtain ⟨j, hj, hjv⟩ := succ_ok line i hi
        rw [List.drop_eq_getElem_cons hi, List.takeWhile_cons]
        cases hw : Clew.Framing.isAsciiWs line.val[i.val].val
        · left
          exact ⟨i, by simp [skip_ws_loop.body, hi, index_ok line i hi, is_ascii_ws_ok, hw], by simp⟩
        · right
          refine ⟨j, by simp [skip_ws_loop.body, hi, index_ok line i hi, is_ascii_ws_ok, hw, hj],
            trivial, by omega, ?_⟩
          intro c hcv; rw [hcv, hjv]; simp only [ite_true, List.length_cons]; omega
      · left
        refine ⟨i, by simp [skip_ws_loop.body, hi], ?_⟩
        rw [List.drop_eq_nil_of_le (by omega)]; simp) f trivial
  unfold skip_ws skip_ws_loop
  exact ⟨c, hl, hq⟩

theorem lower_ok (b : Std.U8) : ∃ c : Std.U8, lower b = ok c ∧ c.val = Clew.Framing.lowerB b.val := by
  unfold lower
  simp only [ge_iff_le, UScalar.le_equiv, U8.ofNat, UScalar.ofNat, UScalar.ofNatCore_val_eq]
  by_cases h : 65 ≤ b.val ∧ b.val ≤ 90
  · obtain ⟨c, hc, hcv⟩ := ok_of_spec (UScalar.add_spec (x := b) (y := 32#u8) (by scalar_tac))
    refine ⟨c, ?_, ?_⟩
    · simp [h.1, h.2, hc]
    · simp [Clew.Framing.lowerB, h.1, h.2, hcv]
  · refine ⟨b, ?_, ?_⟩
    · by_cases h1 : 65 ≤ b.val <;> simp_all
    · simp only [Clew.Framing.lowerB, Bool.and_eq_true, decide_eq_true_eq]
      exact (ite_eq_right_iff.mpr (fun h' => absurd h' h)).symm

theorem content_length_vals :
    CONTENT_LENGTH.val.map (·.val) = Clew.Framing.contentLengthName := by
  unfold CONTENT_LENGTH
  rfl

theorem content_length_length : Clew.Framing.contentLengthName.length = 14 := by decide

/-- What `is_content_length` decides: `line[..e]`, lower-cased, is `content-length`. -/
theorem is_content_length_ok (line : Slice Std.U8) (e : Std.Usize) (he : e.val ≤ line.val.length) :
    is_content_length line e =
      ok (decide (((line.val.take e.val).map fun b => Clew.Framing.lowerB b.val) =
        Clew.Framing.contentLengthName)) := by
  unfold is_content_length
  by_cases h14 : e.val = 14
  · have hl : 14 ≤ line.val.length := h14 ▸ he
    have hcl : CONTENT_LENGTH.val.length = 14 := by simp
    obtain ⟨b, hb, hq⟩ := loop_spec (is_content_length_loop.body line) (fun _ => True)
      (fun i b => b = decide ((((line.val.take 14).drop i.val).map fun b => Clew.Framing.lowerB b.val) =
        Clew.Framing.contentLengthName.drop i.val))
      (fun i => 14 - i.val) (by
        intro i _
        by_cases hi : i.val < 14
        · have hil : i.val < line.val.length := by omega
          obtain ⟨j, hj, hjv⟩ := succ_ok line i hil
          obtain ⟨c, hc, hcv⟩ := lower_ok line.val[i.val]
          have hidx : Array.index_usize CONTENT_LENGTH i = ok CONTENT_LENGTH.val[i.val] := by
            obtain ⟨x, hx, hxv⟩ := ok_of_spec (Array.index_usize_spec CONTENT_LENGTH i (by
              simp [hcl]; omega))
            rw [hx, hxv]
          have hn : Clew.Framing.contentLengthName[i.val]'(by rw [content_length_length]; omega) =
              (CONTENT_LENGTH.val[i.val]'(by omega)).val := by
            simp only [← content_length_vals, List.getElem_map]
          have hlen : i.val < (line.val.take 14).length := by simp; omega
          rw [List.drop_eq_getElem_cons hlen, List.getElem_take,
            List.drop_eq_getElem_cons (l := Clew.Framing.contentLengthName) (by rw [content_length_length]; omega)]
          simp only [List.map_cons, List.cons.injEq]
          by_cases heq : c.val = (CONTENT_LENGTH.val[i.val]'(by omega)).val
          · right
            refine ⟨j, ?_, trivial, by omega, ?_⟩
            · simp [is_content_length_loop.body, hi, index_ok line i hil, hc, hidx, hj, UScalar.eq_equiv, heq]
            · intro b hb; rw [hb, hjv, ← hcv, heq, hn]; simp
          · left
            refine ⟨false, ?_, ?_⟩
            · simp [is_content_length_loop.body, hi, index_ok line i hil, hc, hidx, UScalar.eq_equiv, heq]
            · rw [← hcv, hn]; simp [heq]
        · left
          refine ⟨true, by simp [is_content_length_loop.body, hi], ?_⟩
          rw [List.drop_eq_nil_of_le (by simp; omega), List.drop_eq_nil_of_le (by
            rw [content_length_length]; omega)]; simp) 0#usize trivial
    have hne : ¬ (e != 14#usize) = true := by simp [UScalar.eq_equiv, h14]
    simp only [hne, Bool.false_eq_true, ↓reduceIte]
    unfold is_content_length_loop
    rw [hb, hq, h14]; simp
  · have hne : (e != 14#usize) = true := by simp [h14]
    simp only [hne, ↓reduceIte]
    congr 1
    symm
    simp only [decide_eq_false_iff_not]
    intro heq
    have := congrArg List.length heq
    simp [content_length_length] at this
    omega

/-- `line[f..k]`. -/
def seg (line : Slice Std.U8) (f k : Nat) : List Std.U8 := (line.val.drop f).take (k - f)

theorem seg_succ (line : Slice Std.U8) (f k : Nat) (hf : f ≤ k) (hk : k < line.val.length) :
    seg line f (k + 1) = seg line f k ++ [line.val[k]] := by
  unfold seg
  rw [show k + 1 - f = (k - f) + 1 by omega, List.take_add_one]
  congr
  rw [List.getElem?_drop, show f + (k - f) = k by omega, List.getElem?_eq_getElem hk]
  rfl

theorem seg_self (line : Slice Std.U8) (f : Nat) : seg line f f = [] := by simp [seg]

def wsU (b : Std.U8) : Bool := Clew.Framing.isAsciiWs b.val

/-- Where `trim_end` stops: `line[f..r]` is `line[f..t]` with its trailing
ASCII whitespace taken off. -/
theorem trim_end_ok (line : Slice Std.U8) (f t : Std.Usize) (hft : f.val ≤ t.val)
    (ht : t.val ≤ line.val.length) :
    ∃ r, trim_end line f t = ok r ∧ f.val ≤ r.val ∧ r.val ≤ t.val ∧
      seg line f.val r.val = ((seg line f.val t.val).reverse.dropWhile wsU).reverse := by
  obtain ⟨r, hl, hq⟩ := loop_spec (trim_end_loop.body line f)
    (fun i => f.val ≤ i.val ∧ i.val ≤ line.val.length)
    (fun i r => f.val ≤ r.val ∧ r.val ≤ i.val ∧
      seg line f.val r.val = ((seg line f.val i.val).reverse.dropWhile wsU).reverse)
    (fun i => i.val) (by
      intro i ⟨hfi, hil⟩
      by_cases hi : f.val < i.val
      · obtain ⟨k, hk, hkv⟩ := ok_of_spec (UScalar.sub_spec (x := i) (y := 1#usize) (by simp; omega))
        have hkl : k.val < line.val.length := by simp at hkv; omega
        have hseg : seg line f.val i.val = seg line f.val k.val ++ [line.val[k.val]] := by
          have := seg_succ line f.val k.val (by simp at hkv; omega) hkl
          rw [← this]; congr 1; simp at hkv; omega
        cases hw : wsU line.val[k.val]
        · left
          refine ⟨i, ?_, le_of_lt hi, le_refl _, ?_⟩
          · simp only [wsU] at hw
            simp [trim_end_loop.body, hi, hk, index_ok line k hkl, is_ascii_ws_ok, hw]
          · rw [hseg, List.reverse_append]
            simp [hw]
        · right
          refine ⟨k, ?_, ⟨by simp at hkv; omega, by omega⟩, by simp at hkv; omega, ?_⟩
          · simp only [wsU] at hw
            simp [trim_end_loop.body, hi, hk, index_ok line k hkl, is_ascii_ws_ok, hw]
          · intro r ⟨h1, h2, h3⟩
            refine ⟨h1, by simp at hkv; omega, ?_⟩
            rw [h3, hseg, List.reverse_append]
            simp [hw]
      · left
        have hfe : i.val = f.val := by omega
        refine ⟨i, by simp [trim_end_loop.body, hi], by omega, le_refl _, ?_⟩
        rw [hfe, seg_self]; rfl) t ⟨hft, ht⟩
  unfold trim_end trim_end_loop
  exact ⟨r, hl, hq⟩

/-- The decimal value of digits, from `a` on: `valueB`'s fold. -/
def fold (a : Nat) (xs : List Std.U8) : Nat := xs.foldl (fun acc b => acc * 10 + (b.val - 48)) a

theorem fold_ge (a : Nat) (xs : List Std.U8) : a ≤ fold a xs := by
  induction xs generalizing a with
  | nil => simp [fold]
  | cons x xs ih =>
    simp only [fold, List.foldl_cons]
    exact Nat.le_trans (by nlinarith) (ih _)

theorem u64_max : (UScalar.max UScalarTy.U64) = 18446744073709551615 := by
  simp [U64.max_eq]

theorem u64_MAX_val : core.num.U64.MAX.val = 18446744073709551615 := by
  simp [core.num.U64.MAX, U64.rMax]

/-- What `decimal` computes from `n` at `i`: the value of the digits
`line[i..t]` after `n`, or nothing when it passes `u64::MAX`. -/
theorem decimal_loop_ok (line : Slice Std.U8) (t : Std.Usize) (ht : t.val ≤ line.val.length)
    (n : Std.U64) (i : Std.Usize)
    (hd : ∀ b ∈ (line.val.take t.val).drop i.val, Clew.Framing.isAsciiDigitB b.val = true) :
    ∃ r, decimal_loop line t n i = ok r ∧
      match r with
      | none => 2 ^ 64 ≤ fold n.val ((line.val.take t.val).drop i.val)
      | some m => m.val = fold n.val ((line.val.take t.val).drop i.val) := by
  obtain ⟨r, hl, hq⟩ := loop_spec (fun s : Std.U64 × Std.Usize => decimal_loop.body line t s.1 s.2)
    (fun s => ∀ b ∈ (line.val.take t.val).drop s.2.val, Clew.Framing.isAsciiDigitB b.val = true)
    (fun s r => match r with
      | none => 2 ^ 64 ≤ fold s.1.val ((line.val.take t.val).drop s.2.val)
      | some m => m.val = fold s.1.val ((line.val.take t.val).drop s.2.val))
    (fun s => t.val - s.2.val) (by
      rintro ⟨n, i⟩ hdig
      simp only at hdig ⊢
      by_cases hi : i.val < t.val
      · have hil : i.val < line.val.length := by omega
        have hlen : i.val < (line.val.take t.val).length := by simp; omega
        have hrest := List.drop_eq_getElem_cons hlen
        rw [List.getElem_take] at hrest
        have hx := hdig line.val[i.val] (by rw [hrest]; simp)
        simp only [Clew.Framing.isAsciiDigitB, Bool.and_eq_true, decide_eq_true_eq] at hx
        set x := line.val[i.val] with hxdef
        obtain ⟨d8, hd8, hd8v⟩ := ok_of_spec (UScalar.sub_spec (x := x) (y := 48#u8) (by simp; omega))
        simp only [U8.ofNat, UScalar.ofNat, UScalar.ofNatCore_val_eq] at hd8v
        set d := UScalar.cast UScalarTy.U64 d8 with hddef
        have hdv : d.val = x.val - 48 := by rw [hddef, UScalar.cast_val_eq]; simp; omega
        obtain ⟨q1, hq1, hq1v⟩ := ok_of_spec (UScalar.sub_spec (x := core.num.U64.MAX) (y := d)
          (by rw [u64_MAX_val, hdv]; omega))
        obtain ⟨q, hq, hqv⟩ := UScalar.div_spec q1 (y := 10#u64) (by simp)
        have hfold : fold n.val ((line.val.take t.val).drop i.val) =
            fold (n.val * 10 + (x.val - 48)) ((line.val.take t.val).drop (i.val + 1)) := by
          rw [hrest]; rfl
        by_cases hover : q.val < n.val
        · left
          refine ⟨none, ?_, ?_⟩
          · simp [decimal_loop.body, hi, index_ok line i hil, ← hxdef, hd8, lift, ← hddef, hq1, hq,
              UScalar.lt_equiv, hover]
          · simp only
            rw [hfold]
            refine Nat.le_trans ?_ (fold_ge _ _)
            rw [hqv, hq1v.1, u64_MAX_val, hdv] at *
            simp at hover
            omega
        · right
          have hqv' : q.val = (18446744073709551615 - (x.val - 48)) / 10 := by
            rw [hqv, hq1v.1, u64_MAX_val, hdv]; simp
          obtain ⟨m1, hm1, hm1v⟩ := ok_of_spec (UScalar.mul_spec (x := n) (y := 10#u64)
            (by rw [u64_max]; simp; omega))
          obtain ⟨m2, hm2, hm2v⟩ := ok_of_spec (UScalar.add_spec (x := m1) (y := d)
            (by rw [u64_max, hm1v, hdv]; simp; omega))
          obtain ⟨j, hj, hjv⟩ := succ_ok line i hil
          refine ⟨(m2, j), ?_, ?_, by simp; omega, ?_⟩
          · simp [decimal_loop.body, hi, index_ok line i hil, ← hxdef, hd8, lift, ← hddef, hq1, hq,
              UScalar.lt_equiv, hover, hm1, hm2, hj]
          · intro b hb
            apply hdig
            simp only [hjv] at hb
            rw [hrest]
            exact List.mem_cons_of_mem _ hb
          · intro r hr
            have hm2v' : m2.val = n.val * 10 + (x.val - 48) := by rw [hm2v, hm1v, hdv]; simp
            rw [hfold, ← hm2v', ← hjv]
            exact hr
      · left
        refine ⟨some n, by simp [decimal_loop.body, hi], ?_⟩
        simp only
        rw [List.drop_eq_nil_of_le (by simp; omega)]
        rfl) (n, i) hd
  unfold decimal_loop
  exact ⟨r, hl, hq⟩

/-- What `parse_length` reads from `line[f..t]`: what the model's
`parseLength` reads from those bytes. -/
theorem parse_length_ok (line : Slice Std.U8) (f t : Std.Usize) (hft : f.val ≤ t.val)
    (ht : t.val ≤ line.val.length) :
    ∃ r, parse_length line f t = ok r ∧
      r.map (·.val) = (match Clew.Framing.parseLength ((seg line f.val t.val).map (·.val)) with
        | .ok n => some n
        | .error _ => none) := by
  have hseg : (line.val.take t.val).drop f.val = seg line f.val t.val := by
    simp [seg, List.drop_take]
  have hlen : (seg line f.val t.val).length = t.val - f.val := by
    simp [seg]; omega
  unfold parse_length
  simp only [Clew.Framing.parseLength, List.length_map, hlen, List.all_map]
  by_cases he : t.val ≤ f.val
  · have : seg line f.val t.val = [] := by
      apply List.eq_nil_of_length_eq_zero; omega
    refine ⟨none, by simp [UScalar.le_equiv, he], by simp [this]⟩
  · obtain ⟨k, hk, hkv⟩ := ok_of_spec (UScalar.sub_spec (x := t) (y := f) (by omega))
    have hne : seg line f.val t.val ≠ [] := by
      intro h; have := congrArg List.length h; simp [hlen] at this; omega
    by_cases h20 : 20 < t.val - f.val
    · refine ⟨none, by simp [UScalar.le_equiv, he, hk, hkv.1, h20], by simp [h20]⟩
    · rw [all_digits_ok line f t ht, hseg]
      by_cases hd : (seg line f.val t.val).all (fun b => Clew.Framing.isAsciiDigitB b.val) = true
      · obtain ⟨r, hr, hrq⟩ := decimal_loop_ok line t ht 0#u64 f (by
          rw [hseg]; intro b hb; exact List.all_eq_true.mp hd b hb)
        refine ⟨r, by simp [UScalar.le_equiv, he, hk, hkv.1, h20, hd, decimal, hr], ?_⟩
        have hv : Clew.Framing.valueB ((seg line f.val t.val).map (·.val)) =
            fold 0 (seg line f.val t.val) := by
          simp [Clew.Framing.valueB, fold, List.foldl_map]
        rw [hseg] at hrq
        simp only [h20, decide_false, hv]
        have hcond : ¬ (seg line f.val t.val = [] ∨
            ∃ x ∈ seg line f.val t.val, Clew.Framing.isAsciiDigitB x.val = false) := by
          rintro (h | ⟨x, hx, hx'⟩)
          · exact hne h
          · have := List.all_eq_true.mp hd x hx; simp_all
        cases r with
        | none => simp at hrq; simp [hcond, Nat.not_lt.mpr hrq]
        | some m => simp at hrq; have := m.hBounds; simp at this; simp [hcond, ← hrq, this]
      · have hex : ∃ x ∈ seg line f.val t.val, Clew.Framing.isAsciiDigitB x.val = false := by
          simpa using hd
        refine ⟨none, by simp [UScalar.le_equiv, he, hk, hd], by simp [h20, hex]⟩

theorem max_frame_ok : ∃ m, MAX_FRAME_BYTES = ok m ∧ m.val = 67108864 := by
  unfold MAX_FRAME_BYTES
  obtain ⟨a, ha, hav⟩ := ok_of_spec (UScalar.mul_spec (x := 64#u64) (y := 1024#u64)
    (by rw [u64_max]; simp))
  obtain ⟨b, hb, hbv⟩ := ok_of_spec (UScalar.mul_spec (x := a) (y := 1024#u64)
    (by rw [u64_max, hav]; simp))
  exact ⟨b, by simp [ha, hb], by rw [hbv, hav]; simp⟩

theorem findByte_eq (x : Nat) (xs : List Nat) :
    Clew.Framing.findByte x xs =
      if (xs.takeWhile (· ≠ x)).length < xs.length then some (xs.takeWhile (· ≠ x)).length else none := by
  induction xs with
  | nil => simp [Clew.Framing.findByte]
  | cons y ys ih =>
    by_cases h : y = x
    · simp [Clew.Framing.findByte, h]
    · simp only [Clew.Framing.findByte, beq_iff_eq, h, ↓reduceIte, ih, List.takeWhile_cons,
        ne_eq, not_false_eq_true, decide_true, List.length_cons]
      split <;> simp_all

theorem parseLength_error (v : List Nat) (e : Clew.Framing.Error)
    (h : Clew.Framing.parseLength v = .error e) : e = .invalidContentLength := by
  unfold Clew.Framing.parseLength at h
  dsimp only at h
  split at h
  · cases h; rfl
  · split at h
    · cases h
    · cases h; rfl

theorem dropWhile_eq_drop {α : Type} (p : α → Bool) (xs : List α) :
    xs.dropWhile p = xs.drop (xs.takeWhile p).length := by
  have h := List.takeWhile_append_dropWhile (p := p) (l := xs)
  have h2 : (xs.takeWhile p ++ xs.dropWhile p).drop (xs.takeWhile p).length = xs.dropWhile p :=
    List.drop_left
  rw [h] at h2
  exact h2.symm

/-- A line's bytes, as the model reads them. -/
def bytes (line : Slice Std.U8) : List Nat := line.val.map (·.val)

/-- What the Rust function returns, as the model's result for the same
`Seen`. -/
def toModel (seen : Clew.Framing.Seen) :
    core.result.Result (Option Std.U64) HeaderError → Except Clew.Framing.Error Clew.Framing.Seen
  | .Ok l => .ok { seen with length := l.map (·.val), headers := seen.headers + 1 }
  | .Err .Malformed => .error .malformedHeader
  | .Err .InvalidLength => .error .invalidContentLength
  | .Err .Conflicting => .error .conflictingContentLength
  | .Err (.TooLarge _) => .error .tooLarge

/-- **`header` is the model**: for every header line and every
`Content-Length` read before it (`seen.length`), the Rust function returns —
no index out of bounds, no overflow, every loop ends — and decides what the
model decides: the same error, or the same `Content-Length` with one more
header counted. -/
theorem header_is_the_model (length : Option Std.U64) (line : Slice Std.U8)
    (seen : Clew.Framing.Seen) (hs : seen.length = length.map (·.val)) :
    ∃ r, header length line = ok r ∧ toModel seen r = Clew.Framing.header seen (bytes line) := by
  unfold header Clew.Framing.header
  rw [all_ascii_ok]
  simp only [bytes, List.all_map]
  by_cases ha : (line.val.all fun b => b.val < 128) = true
  swap
  · refine ⟨.Err .Malformed, by simp [ha], ?_⟩
    simp only [Bool.not_eq_true] at ha
    simp [toModel, Function.comp_def, ha]
  obtain ⟨c, hc, hcv⟩ := find_colon_ok line
  rw [findByte_eq]
  simp only [List.takeWhile_map, List.length_map, Function.comp_def]
  rw [← hcv]
  simp only [ha, bind_ok, ↓reduceIte, hc, Bool.not_true, Bool.false_eq_true]
  have hcL : c.val ≤ line.val.length := by rw [hcv]; exact (List.takeWhile_prefix _).length_le
  by_cases hend : c.val = line.val.length
  · have heq : c = line.len := by rw [UScalar.eq_equiv]; simp [hend]
    refine ⟨.Err .Malformed, by simp [heq], by simp [toModel, hend]⟩
  have hlt : c.val < line.val.length := by omega
  have hne : ¬ (c = line.len) := by rw [UScalar.eq_equiv]; simp; omega
  simp only [hne, hlt, ↓reduceIte]
  by_cases h0 : c.val = 0
  · have heq : c = 0#usize := by rw [UScalar.eq_equiv]; simp [h0]
    refine ⟨.Err .Malformed, by simp [heq], by simp [toModel, h0]⟩
  have hne0 : ¬ (c = 0#usize) := by rw [UScalar.eq_equiv]; simp [h0]
  simp only [hne0, ↓reduceIte]
  simp only [all_token_ok line c (le_of_lt hlt), ← List.map_take, List.all_map]
  have htake_ne : (List.map (fun x => x.val) (List.take c.val line.val)).isEmpty = false := by
    rw [Bool.eq_false_iff]; intro h
    simp only [List.isEmpty_iff, List.map_eq_nil_iff, List.take_eq_nil_iff] at h
    rcases h with h | h
    · exact h0 h
    · simp [h] at hlt
  simp only [htake_ne, Bool.false_or, Function.comp_def]
  by_cases ht : ((line.val.take c.val).all fun b => Clew.Framing.isTokenByte b.val) = true
  swap
  · simp only [Bool.not_eq_true] at ht
    refine ⟨.Err .Malformed, by simp [ht], by simp [toModel, ht]⟩
  simp only [ht, Bool.not_true, Bool.false_eq_true, ↓reduceIte, bind_ok,
    is_content_length_ok line c (le_of_lt hlt), List.map_map, Function.comp_def]
  by_cases hcl : ((line.val.take c.val).map fun b => Clew.Framing.lowerB b.val) =
      Clew.Framing.contentLengthName
  swap
  · refine ⟨.Ok length, by simp only [hcl, decide_false, Bool.false_eq_true, ↓reduceIte], ?_⟩
    have : (((line.val.take c.val).map fun b => Clew.Framing.lowerB b.val) ==
        Clew.Framing.contentLengthName) = false := by
      simp only [beq_eq_false_iff_ne]; exact hcl
    simp only [this, Bool.false_eq_true, ↓reduceIte, toModel]
    cases seen; simp_all
  have hbeq : (((line.val.take c.val).map fun b => Clew.Framing.lowerB b.val) ==
      Clew.Framing.contentLengthName) = true := by simp [hcl]
  simp only [hcl, decide_true, ↓reduceIte, beq_self_eq_true]
  obtain ⟨c1, hc1, hc1v⟩ := succ_ok line c hlt
  obtain ⟨fr, hfr, hfrv⟩ := skip_ws_ok line c1
  have hfrL : fr.val ≤ line.val.length := by
    have : ((line.val.drop c1.val).takeWhile fun b => Clew.Framing.isAsciiWs b.val).length ≤
        (line.val.drop c1.val).length := (List.takeWhile_prefix _).length_le
    simp at this; omega
  have hlenv : (Slice.len line).val = line.val.length := by simp
  obtain ⟨tt, hto, h1, h2, h3⟩ := trim_end_ok line fr (Slice.len line) (by omega) (by omega)
  obtain ⟨o, ho, hov⟩ := parse_length_ok line fr tt h1 (by omega)
  -- The model's trimmed value is `line[fr..to]`.
  have hfrom : (line.val.drop (c.val + 1)).dropWhile wsU = seg line fr.val line.val.length := by
    rw [dropWhile_eq_drop, List.drop_drop]
    unfold seg
    rw [List.take_of_length_le (by simp)]
    congr 1
    rw [hfrv, hc1v]
    rfl
  have htrim : Clew.Framing.trimAscii ((line.val.map (·.val)).drop (c.val + 1)) =
      (seg line fr.val tt.val).map (·.val) := by
    unfold Clew.Framing.trimAscii
    rw [← List.map_drop, List.dropWhile_map]
    have e1 : (line.val.drop (c.val + 1)).dropWhile (Clew.Framing.isAsciiWs ∘ fun x => x.val) =
        (line.val.drop (c.val + 1)).dropWhile wsU := rfl
    rw [e1, hfrom, ← List.map_reverse, List.dropWhile_map, ← List.map_reverse]
    have e2 : ((seg line fr.val line.val.length).reverse.dropWhile
        (Clew.Framing.isAsciiWs ∘ fun x => x.val)) =
        (seg line fr.val line.val.length).reverse.dropWhile wsU := rfl
    rw [e2, h3, hlenv]
  rw [htrim]
  simp only [hc1, bind_ok, hfr, hto, ho]
  cases hP : Clew.Framing.parseLength ((seg line fr.val tt.val).map (·.val)) with
  | error e =>
    rw [hP] at hov
    cases o with
    | some n => simp at hov
    | none =>
      have := parseLength_error _ e hP
      refine ⟨.Err .InvalidLength, rfl, by simp [toModel, this]⟩
  | ok len =>
    rw [hP] at hov
    cases o with
    | none => simp at hov
    | some n =>
      simp only [Option.map_some, Option.some.injEq] at hov
      obtain ⟨m, hm, hmv⟩ := max_frame_ok
      have hmax : Clew.Framing.maxFrame = 67108864 := rfl
      cases length with
      | none =>
        by_cases hbig : m.val < n.val
        · refine ⟨.Err (.TooLarge n), by simp [hm, hbig], ?_⟩
          simp [toModel, hs, hmax, ← hov]; omega
        · refine ⟨.Ok (some n), by simp [hm, hbig], ?_⟩
          have : ¬ (len > Clew.Framing.maxFrame) := by rw [hmax, ← hov]; omega
          simp only [Option.map_none] at hs
          simp [toModel, hs, this, hov]
      | some prev =>
        simp only [Option.map_some] at hs
        by_cases hsame : prev.val = n.val
        · by_cases hbig : m.val < n.val
          · refine ⟨.Err (.TooLarge n), by simp [hm, hbig, UScalar.eq_equiv, hsame], ?_⟩
            simp [toModel, hs, hmax, ← hov, hsame]; omega
          · refine ⟨.Ok (some n), by simp [hm, hbig, UScalar.eq_equiv, hsame], ?_⟩
            have : ¬ (len > Clew.Framing.maxFrame) := by rw [hmax, ← hov]; omega
            simp [toModel, hs, this, hov, hsame]
        · refine ⟨.Err .Conflicting, by simp [hsame], ?_⟩
          simp [toModel, hs, ← hov, hsame]

end Framing

/-! ## An MCP server's request headers — `clew_rules::headers`

`check_headers` calls `header_name_ok` and `header_value_ok` on each header's
bytes. The model in `../Clew/Guards.lean` reads characters; Rust hands over
the UTF-8 bytes of a string. Proved here: on the UTF-8 bytes of any list of
characters, as Lean's own `List.utf8Encode` makes them, the Rust functions
return, and decide what `validHeaderName` and the model's value check decide
of those characters. -/

namespace Headers

open clew_rules.headers
/-- What `header_name_ok` decides: not empty, every byte a token byte. -/
theorem header_name_ok_spec (line : Slice Std.U8) :
    header_name_ok line =
      ok (!line.val.isEmpty && line.val.all fun b => Clew.Framing.isTokenByte b.val) := by
  unfold header_name_ok
  by_cases he : line.val = []
  · simp [core.slice.Slice.is_empty, he]
  obtain ⟨b, hl, hq⟩ := loop_spec (header_name_ok_loop.body line) (fun _ => True)
    (fun i b => b = (line.val.drop i.val).all fun b => Clew.Framing.isTokenByte b.val)
    (fun i => line.val.length - i.val) (by
      intro i _
      by_cases hi : i.val < line.val.length
      · obtain ⟨j, hj, hjv⟩ := succ_ok line i hi
        rw [List.drop_eq_getElem_cons hi]
        cases ht : Clew.Framing.isTokenByte line.val[i.val].val
        · left
          exact ⟨false, by simp [header_name_ok_loop.body, hi, index_ok line i hi,
            Framing.is_token_byte_ok, ht], by simp only [List.all_cons, ht, Bool.false_and]⟩
        · right
          refine ⟨j, by simp [header_name_ok_loop.body, hi, index_ok line i hi,
            Framing.is_token_byte_ok, ht, hj], trivial, by omega, ?_⟩
          intro b hb; rw [hb, hjv, List.all_cons, ht, Bool.true_and]
      · left
        refine ⟨true, by simp [header_name_ok_loop.body, hi], ?_⟩
        rw [List.drop_eq_nil_of_le (by omega)]; rfl) 0#usize trivial
  simp only [core.slice.Slice.is_empty, Slice.length, bind_ok]
  unfold header_name_ok_loop
  rw [hl, hq]
  simp [he]

/-- A byte that may not be in a header value: CR, LF or NUL. -/
def breakByte (b : Nat) : Bool := b == 13 || b == 10 || b == 0

/-- What `header_value_ok` decides: no CR, LF or NUL. -/
theorem header_value_ok_spec (line : Slice Std.U8) :
    header_value_ok line = ok (!line.val.any fun b => breakByte b.val) := by
  obtain ⟨b, hl, hq⟩ := loop_spec (header_value_ok_loop.body line) (fun _ => True)
    (fun i b => b = !(line.val.drop i.val).any fun b => breakByte b.val)
    (fun i => line.val.length - i.val) (by
      intro i _
      by_cases hi : i.val < line.val.length
      · obtain ⟨j, hj, hjv⟩ := succ_ok line i hi
        rw [List.drop_eq_getElem_cons hi]
        cases hb : breakByte line.val[i.val].val
        · right
          simp only [breakByte, Bool.or_eq_false_iff, beq_eq_false_iff_ne] at hb
          refine ⟨j, by simp [header_value_ok_loop.body, hi, index_ok line i hi, hj, UScalar.eq_equiv,
            U8.ofNat, UScalar.ofNat, UScalar.ofNatCore_val_eq, hb.1.1, hb.1.2, hb.2], trivial, by omega, ?_⟩
          intro b hb'
          rw [hb', hjv, List.any_cons]
          simp only [breakByte, hb.1.1, hb.1.2, hb.2, beq_false_of_ne, ne_eq, not_false_eq_true,
            Bool.or_false, Bool.false_or]
        · left
          simp only [breakByte, Bool.or_eq_true, beq_iff_eq] at hb
          refine ⟨false, ?_, by rcases hb with (h | h) | h <;> (rw [List.any_cons]; simp only [breakByte, h]; rfl)⟩
          rcases hb with (h | h) | h <;>
            simp [header_value_ok_loop.body, hi, index_ok line i hi, UScalar.eq_equiv, U8.ofNat, UScalar.ofNat, UScalar.ofNatCore_val_eq, h]
      · left
        refine ⟨true, by simp [header_value_ok_loop.body, hi], ?_⟩
        rw [List.drop_eq_nil_of_le (by omega)]; rfl) 0#usize trivial
  unfold header_value_ok header_value_ok_loop
  rw [hl, hq]; simp

/-- The UTF-8 bytes of a list of characters, as Lean's own `List.utf8Encode`
makes them (`(cs.flatMap String.utf8EncodeChar).toByteArray`): what Rust's
`str::as_bytes` gives for a string of those characters. -/
def utf8 (cs : List Char) : List Nat := (cs.flatMap String.utf8EncodeChar).map (·.toNat)

/-- A character below 128 is one byte: itself. -/
theorem utf8_ascii (c : Char) (h : c.toNat < 128) :
    (String.utf8EncodeChar c).map (·.toNat) = [c.toNat] := by
  have h' : c.val.toNat ≤ 0x7f := by unfold Char.toNat at h; omega
  unfold String.utf8EncodeChar
  simp only [h', ↓reduceIte, List.map_cons, List.map_nil, UInt8.toNat_ofNat', List.cons.injEq, and_true]
  unfold Char.toNat
  omega

/-- Any other character is one byte or more, each 128 or above. -/
theorem utf8_high (c : Char) (h : 128 ≤ c.toNat) :
    String.utf8EncodeChar c ≠ [] ∧ ∀ b ∈ String.utf8EncodeChar c, 128 ≤ b.toNat := by
  have h' : ¬ c.val.toNat ≤ 0x7f := by unfold Char.toNat at h; omega
  unfold String.utf8EncodeChar
  simp only [h', ↓reduceIte]
  split_ifs <;> simp only [ne_eq, reduceCtorEq, not_false_eq_true, List.mem_cons, List.not_mem_nil,
    or_false, forall_eq_or_imp, forall_eq, UInt8.toNat_ofNat', true_and] <;> omega

theorem utf8EncodeChar_ne_nil (c : Char) : String.utf8EncodeChar c ≠ [] := by
  by_cases h : c.toNat < 128
  · intro he; have := utf8_ascii c h; simp [he] at this
  · exact (utf8_high c (by omega)).1

theorem isTokenByte_lt (n : Nat) (h : Clew.Framing.isTokenByte n = true) : n < 128 := by
  simp only [Clew.Framing.isTokenByte, Clew.Framing.isAsciiAlnumB, Clew.Framing.isAsciiDigitB,
    Bool.or_eq_true, Bool.and_eq_true, decide_eq_true_eq, List.contains_iff_mem, List.mem_cons,
    List.not_mem_nil, or_false] at h
  omega

theorem tokenChar_lt (c : Char) (h : Clew.Guards.tokenChar c = true) : c.toNat < 128 := by
  simp only [Clew.Guards.tokenChar, Clew.Text.isAsciiAlphanumeric, Clew.Text.isAsciiDigit,
    Bool.or_eq_true, Bool.and_eq_true, decide_eq_true_eq] at h
  rcases h with h | h
  · have h0 : ('0' : Char).val.toNat = 48 := rfl
    have h9 : ('9' : Char).val.toNat = 57 := rfl
    have ha : ('a' : Char).val.toNat = 97 := rfl
    have hz : ('z' : Char).val.toNat = 122 := rfl
    have hA : ('A' : Char).val.toNat = 65 := rfl
    have hZ : ('Z' : Char).val.toNat = 90 := rfl
    simp only [Char.le_def, UInt32.le_iff_toNat_le, h0, h9, ha, hz, hA, hZ] at h
    unfold Char.toNat; omega
  · have hall : ∀ d ∈ "!#$%&'*+-.^_`|~".toList, d.toNat < 128 := by decide
    exact hall c (List.contains_iff_mem.mp h)

/-- A character is a header-name character exactly when its one byte is a
token byte. -/
theorem tokenChar_eq (c : Char) :
    Clew.Guards.tokenChar c = Clew.Framing.isTokenByte c.toNat := by
  by_cases h : c.toNat < 128
  · have hc : c = Char.ofNat c.toNat := (Char.ofNat_toNat c).symm
    rw [hc]
    generalize c.toNat = n at h
    revert n; decide +kernel
  · have h1 : Clew.Guards.tokenChar c = false := by
      cases ht : Clew.Guards.tokenChar c
      · rfl
      · exact absurd (tokenChar_lt c ht) h
    have h2 : Clew.Framing.isTokenByte c.toNat = false := by
      cases ht : Clew.Framing.isTokenByte c.toNat
      · rfl
      · exact absurd (isTokenByte_lt _ ht) h
    rw [h1, h2]

theorem char_all_token (c : Char) :
    ((String.utf8EncodeChar c).all fun b => Clew.Framing.isTokenByte b.toNat) = Clew.Guards.tokenChar c := by
  by_cases h : c.toNat < 128
  · have := utf8_ascii c h
    rw [show ((String.utf8EncodeChar c).all fun b => Clew.Framing.isTokenByte b.toNat) =
      ((String.utf8EncodeChar c).map (·.toNat)).all Clew.Framing.isTokenByte by
        simp [List.all_map, Function.comp_def], this, tokenChar_eq]
    simp
  · obtain ⟨hne, hhi⟩ := utf8_high c (by omega)
    have h1 : Clew.Guards.tokenChar c = false := by
      cases ht : Clew.Guards.tokenChar c
      · rfl
      · exact absurd (tokenChar_lt c ht) h
    rw [h1]
    cases hl : String.utf8EncodeChar c with
    | nil => exact absurd hl hne
    | cons b bs =>
      have hb := hhi b (by rw [hl]; simp)
      have : Clew.Framing.isTokenByte b.toNat = false := by
        cases ht : Clew.Framing.isTokenByte b.toNat
        · rfl
        · exact absurd (isTokenByte_lt _ ht) (by omega)
      simp [this]

theorem breakChar_eq (c : Char) : Clew.Guards.breakChar c = breakByte c.toNat := by
  simp only [Clew.Guards.breakChar, breakByte]
  have e : ∀ d : Char, (c == d) = (c.toNat == d.toNat) := by
    intro d
    rw [Bool.eq_iff_iff, beq_iff_eq, beq_iff_eq]
    exact ⟨fun h => h ▸ rfl, fun h => Char.ext (UInt32.toNat_inj.mp (show c.val.toNat = d.val.toNat from h))⟩
  rw [e, e, e]
  rfl

theorem char_any_break (c : Char) :
    ((String.utf8EncodeChar c).any fun b => breakByte b.toNat) = Clew.Guards.breakChar c := by
  by_cases h : c.toNat < 128
  · have := utf8_ascii c h
    rw [show ((String.utf8EncodeChar c).any fun b => breakByte b.toNat) =
      ((String.utf8EncodeChar c).map (·.toNat)).any breakByte by
        simp [List.any_map, Function.comp_def], this, breakChar_eq]
    simp
  · obtain ⟨_, hhi⟩ := utf8_high c (by omega)
    have h1 : Clew.Guards.breakChar c = false := by
      rw [breakChar_eq]; unfold breakByte
      simp only [Bool.or_eq_false_iff, beq_eq_false_iff_ne, ne_eq]; omega
    rw [h1]
    simp only [List.any_eq_false]
    intro b hb
    have := hhi b hb
    simp [breakByte]; omega

theorem utf8_nil (cs : List Char) : utf8 cs = [] ↔ cs = [] := by
  constructor
  · intro h
    cases cs with
    | nil => rfl
    | cons c cs => simp [utf8] at h
  · rintro rfl; rfl

/-- **`header_name_ok` is the model**: on the UTF-8 bytes of any name, it
returns, and says what `validHeaderName` says of the name's characters. -/
theorem header_name_ok_is_the_model (n : List Char) (line : Slice Std.U8)
    (h : line.val.map (·.val) = utf8 n) :
    header_name_ok line = ok (Clew.Guards.validHeaderName n) := by
  rw [header_name_ok_spec]
  congr 1
  have hall : (line.val.all fun b => Clew.Framing.isTokenByte b.val) = n.all Clew.Guards.tokenChar := by
    rw [show (line.val.all fun b => Clew.Framing.isTokenByte b.val) =
      (line.val.map (·.val)).all Clew.Framing.isTokenByte by simp [List.all_map, Function.comp_def], h]
    simp only [utf8, List.flatMap, List.map_flatten, List.all_flatten, List.map_map, List.all_map,
      Function.comp_def, char_all_token]
  have hemp : line.val.isEmpty = n.isEmpty := by
    have : line.val = [] ↔ n = [] := by
      rw [← utf8_nil, ← h]; simp
    cases hl : line.val with
    | nil => simp [hl] at this; simp [this]
    | cons _ _ => simp [hl] at this; cases n with
      | nil => exact absurd rfl this
      | cons _ _ => rfl
  simp [Clew.Guards.validHeaderName, hall, hemp]

/-- **`header_value_ok` is the model**: on the UTF-8 bytes of any value, it
returns, and refuses exactly the values that hold a CR, an LF or a NUL. -/
theorem header_value_ok_is_the_model (v : List Char) (line : Slice Std.U8)
    (h : line.val.map (·.val) = utf8 v) :
    header_value_ok line = ok (!v.any Clew.Guards.breakChar) := by
  rw [header_value_ok_spec]
  congr 2
  rw [show (line.val.any fun b => breakByte b.val) = (line.val.map (·.val)).any breakByte by
    simp [List.any_map, Function.comp_def], h]
  simp only [utf8, List.flatMap, List.map_flatten, List.any_flatten, List.map_map, List.any_map,
    Function.comp_def, char_any_break]

end Headers

/-! ## Commands handed to a shell — `clew_rules::shell`

`shell_command` quotes the words of a command Clew gives the user to run on a
remote host, `sh_c` wraps the scripts Clew runs there over ssh, and `quote`
quotes the paths the macOS installer hands its shell helper. The model in
`../Clew/Ssh.lean` reads characters, and proves that a POSIX shell reads the
quoted command back as exactly the words given (`readWords_shellCommand`,
`readWords_shC`); Rust hands over the UTF-8 bytes of a string. Proved here:
on the UTF-8 bytes of any words, the Rust functions return, and return the
UTF-8 bytes of what the model returns. The one condition is room for the
result in memory (a quoted word takes at most `4 × length + 2` bytes), which
no string Clew holds comes near. -/

namespace Shell

open clew_rules.shell

/-- What `is_bare_byte` lets stand unquoted: ASCII letters and digits and
`-_./@:,+`. -/
def bareB (n : Nat) : Bool :=
  (48 ≤ n && n ≤ 57) || (65 ≤ n && n ≤ 90) || (97 ≤ n && n ≤ 122) ||
    n == 45 || n == 95 || n == 46 || n == 47 || n == 64 || n == 58 || n == 44 || n == 43

/-- A byte inside single quotes: a `'` (39) closes, escapes and opens again. -/
def qB (n : Nat) : List Nat := if n = 39 then [39, 92, 39, 39] else [n]

theorem is_bare_byte_ok (b : Std.U8) : is_bare_byte b = ok (bareB b.val) := by
  unfold is_bare_byte
  simp only [ge_iff_le, UScalar.le_equiv, UScalar.eq_equiv, U8.ofNat, UScalar.ofNat,
    UScalar.ofNatCore_val_eq, Framing.ite_ok, Result.ok.injEq]
  have hb := Framing.u8_lt b
  generalize b.val = v at hb ⊢
  revert v; decide +kernel

theorem all_bare_ok (word : Slice Std.U8) :
    all_bare word = ok (word.val.all fun b => bareB b.val) := by
  obtain ⟨b, hl, hq⟩ := loop_spec (all_bare_loop.body word) (fun _ => True)
    (fun i b => b = (word.val.drop i.val).all fun b => bareB b.val)
    (fun i => word.val.length - i.val) (by
      intro i _
      by_cases hi : i.val < word.val.length
      · obtain ⟨j, hj, hjv⟩ := succ_ok word i hi
        rw [List.drop_eq_getElem_cons hi]
        cases ht : bareB word.val[i.val].val
        · left
          exact ⟨false, by simp [all_bare_loop.body, hi, index_ok word i hi, is_bare_byte_ok, ht],
            by simp only [List.all_cons, ht, Bool.false_and]⟩
        · right
          refine ⟨j, by simp [all_bare_loop.body, hi, index_ok word i hi, is_bare_byte_ok, ht, hj],
            trivial, by omega, ?_⟩
          intro b hb; rw [hb, hjv, List.all_cons, ht, Bool.true_and]
      · left
        refine ⟨true, by simp [all_bare_loop.body, hi], ?_⟩
        rw [List.drop_eq_nil_of_le (by omega)]; rfl) 0#usize trivial
  unfold all_bare all_bare_loop
  rw [hl, hq]; simp

theorem has_quote_ok (script : Slice Std.U8) :
    has_quote script = ok (script.val.any fun b => b.val == 39) := by
  obtain ⟨b, hl, hq⟩ := loop_spec (has_quote_loop.body script) (fun _ => True)
    (fun i b => b = (script.val.drop i.val).any fun b => b.val == 39)
    (fun i => script.val.length - i.val) (by
      intro i _
      by_cases hi : i.val < script.val.length
      · obtain ⟨j, hj, hjv⟩ := succ_ok script i hi
        rw [List.drop_eq_getElem_cons hi]
        by_cases hb : script.val[i.val].val = 39
        · left
          refine ⟨true, ?_, by rw [List.any_cons]; simp [hb]⟩
          simp [has_quote_loop.body, hi, index_ok script i hi, UScalar.eq_equiv, U8.ofNat,
            UScalar.ofNat, UScalar.ofNatCore_val_eq, hb]
        · right
          refine ⟨j, by simp [has_quote_loop.body, hi, index_ok script i hi, hj, UScalar.eq_equiv,
            U8.ofNat, UScalar.ofNat, UScalar.ofNatCore_val_eq, hb], trivial, by omega, ?_⟩
          intro b hb'
          rw [hb', hjv, List.any_cons]; simp [hb]
      · left
        refine ⟨false, by simp [has_quote_loop.body, hi], ?_⟩
        rw [List.drop_eq_nil_of_le (by omega)]; rfl) 0#usize trivial
  unfold has_quote has_quote_loop
  rw [hl, hq]; simp

theorem push_ok {α : Type} (v : alloc.vec.Vec α) (x : α) (h : v.val.length < Usize.max) :
    ∃ v', alloc.vec.Vec.push v x = ok v' ∧ v'.val = v.val ++ [x] :=
  ok_of_spec (alloc.vec.Vec.push_spec v x h)

theorem extend_ok (v : alloc.vec.Vec Std.U8) (s : Slice Std.U8)
    (h : v.val.length + s.val.length ≤ Usize.max) :
    ∃ v', alloc.vec.Vec.extend_from_slice core.clone.CloneU8 v s = ok v' ∧ v'.val = v.val ++ s.val := by
  unfold alloc.vec.Vec.extend_from_slice
  split
  · obtain ⟨l, hl, hv, _⟩ := ok_of_spec (List.clone_spec (clone := core.clone.CloneU8.clone)
      (l := s.val) (by intro x _; rfl))
    rw [hl, bind_tc_ok]; exact ⟨_, rfl, by simp [hv]⟩
  · exfalso; simp_all

theorem to_vec_ok (s : Slice Std.U8) :
    ∃ v, alloc.slice.Slice.to_vec core.clone.CloneU8 s = ok v ∧ v.val = s.val := by
  obtain ⟨v, hv, he⟩ := ok_of_spec (alloc.slice.Slice.to_vec_spec core.clone.CloneU8 s
    (by intro x _; rfl))
  exact ⟨v, hv, by rw [he]; rfl⟩


/-- The numbers of a list of bytes. -/
def bs (l : List Std.U8) : List Nat := l.map (·.val)

theorem flatMap_qB_length (ns : List Nat) : (ns.flatMap qB).length ≤ 4 * ns.length := by
  induction ns with
  | nil => simp
  | cons n ns ih =>
    rw [List.flatMap_cons, List.length_append, List.length_cons]
    have : (qB n).length ≤ 4 := by unfold qB; split <;> simp
    omega

theorem bs_take_succ (l : List Std.U8) (i : Nat) (h : i < l.length) :
    bs (l.take (i + 1)) = bs (l.take i) ++ [l[i].val] := by
  rw [List.take_add_one, List.getElem?_eq_getElem h]
  simp only [bs, Option.toList_some, List.map_append, List.map_cons, List.map_nil]

theorem quote_loop_ok (word : Slice Std.U8) (out : alloc.vec.Vec Std.U8)
    (hout : bs out.val = [39]) (room : 4 * word.val.length + 2 ≤ Usize.max) :
    ∃ r, quote_loop word out 0#usize = ok r ∧ bs r.val = 39 :: (bs word.val).flatMap qB := by
  obtain ⟨r, hl, hq⟩ := loop_spec (fun (o, i) => quote_loop.body word o i)
    (fun p => p.2.val ≤ word.val.length ∧
      bs p.1.val = 39 :: (bs (word.val.take p.2.val)).flatMap qB)
    (fun _ r => bs r.val = 39 :: (bs word.val).flatMap qB)
    (fun p => word.val.length - p.2.val) (by
      rintro ⟨o, i⟩ ⟨hle, ho⟩
      have hlen : o.val.length ≤ 1 + 4 * i.val := by
        have h1 := congrArg List.length ho
        have h2 := flatMap_qB_length (bs (word.val.take i.val))
        simp only [bs, List.length_map, List.length_cons, List.length_take] at h1 h2
        omega
      by_cases hi : i.val < word.val.length
      · obtain ⟨j, hj, hjv⟩ := succ_ok word i hi
        right
        by_cases hb : word.val[i.val].val = 39
        · have hb' : word.val[i.val] = 39#u8 := by
            rw [UScalar.eq_equiv]; simp [hb, U8.ofNat, UScalar.ofNat, UScalar.ofNatCore_val_eq]
          obtain ⟨o1, h1, e1⟩ := push_ok o 39#u8 (by omega)
          obtain ⟨o2, h2, e2⟩ := push_ok o1 92#u8 (by rw [e1]; simp; omega)
          obtain ⟨o3, h3, e3⟩ := push_ok o2 39#u8 (by rw [e2, e1]; simp; omega)
          obtain ⟨o4, h4, e4⟩ := push_ok o3 39#u8 (by rw [e3, e2, e1]; simp; omega)
          refine ⟨(o4, j), ?_, ⟨show j.val ≤ _ by omega, ?_⟩,
            show word.val.length - j.val < word.val.length - i.val by omega, fun _ h => h⟩
          · simp [quote_loop.body, hi, index_ok word i hi, hb', h1, h2, h3, h4, hj]
          · rw [hjv, bs_take_succ _ _ hi, List.flatMap_append, e4, e3, e2, e1]
            simp only [bs, List.map_append] at ho ⊢
            rw [ho]; simp [qB, hb]
        · obtain ⟨o1, h1, e1⟩ := push_ok o word.val[i.val] (by omega)
          refine ⟨(o1, j), ?_, ⟨show j.val ≤ _ by omega, ?_⟩,
            show word.val.length - j.val < word.val.length - i.val by omega, fun _ h => h⟩
          · simp [quote_loop.body, hi, index_ok word i hi, h1, hj, UScalar.eq_equiv, U8.ofNat,
              UScalar.ofNat, UScalar.ofNatCore_val_eq, hb]
          · rw [hjv, bs_take_succ _ _ hi, List.flatMap_append, e1]
            simp only [bs, List.map_append] at ho ⊢
            rw [ho]; simp [qB, hb]
      · left
        refine ⟨o, by simp [quote_loop.body, hi], ?_⟩
        simpa [List.take_of_length_le (show word.val.length ≤ i.val by omega)] using ho)
    (out, 0#usize) ⟨by simp, show bs out.val = _ by rw [hout]; simp [bs]⟩
  exact ⟨r, hl, hq⟩

theorem quote_ok (word : Slice Std.U8) (room : 4 * word.val.length + 2 ≤ Usize.max) :
    ∃ v, quote word = ok v ∧ bs v.val = 39 :: ((bs word.val).flatMap qB ++ [39]) := by
  obtain ⟨o, h0, e0⟩ := push_ok (alloc.vec.Vec.new Std.U8) 39#u8 (by simp; omega)
  obtain ⟨r, hr, er⟩ := quote_loop_ok word o (by rw [e0]; rfl) room
  have hlen : r.val.length ≤ 1 + 4 * word.val.length := by
    have h1 := congrArg List.length er
    have h2 := flatMap_qB_length (bs word.val)
    simp only [bs, List.length_map, List.length_cons] at h1 h2
    omega
  obtain ⟨v, hv, ev⟩ := push_ok r 39#u8 (by omega)
  refine ⟨v, by simp [quote, h0, hr, hv], ?_⟩
  rw [ev]
  simp only [bs, List.map_append] at er ⊢
  rw [er]; rfl

/-- `shell_word` on bytes: bare when not empty and every byte may be. -/
def wordB (ns : List Nat) : List Nat :=
  if !ns.isEmpty && ns.all bareB then ns else 39 :: (ns.flatMap qB ++ [39])

theorem wordB_length (ns : List Nat) : (wordB ns).length ≤ 4 * ns.length + 2 := by
  have := flatMap_qB_length ns
  unfold wordB; split
  · omega
  · simp only [List.length_cons, List.length_append, List.length_nil]; omega

theorem shell_word_ok (word : Slice Std.U8) (room : 4 * word.val.length + 2 ≤ Usize.max) :
    ∃ v, shell_word word = ok v ∧ bs v.val = wordB (bs word.val) := by
  obtain ⟨q, hq, eq⟩ := quote_ok word room
  unfold shell_word
  by_cases he : word.val = []
  · refine ⟨q, by simp [core.slice.Slice.is_empty, he, hq], ?_⟩
    rw [eq]; simp [wordB, he, bs]
  · cases hb : word.val.all fun b => bareB b.val
    · refine ⟨q, by simp [core.slice.Slice.is_empty, he, all_bare_ok, hb, hq], ?_⟩
      rw [eq]; simp [wordB, bs, List.all_map, Function.comp_def, hb]
    · obtain ⟨v, hv, ev⟩ := to_vec_ok word
      refine ⟨v, by simp [core.slice.Slice.is_empty, he, all_bare_ok, hb, hv], ?_⟩
      rw [ev]; simp [wordB, bs, List.all_map, Function.comp_def, hb, he]

theorem with_word_ok (out : alloc.vec.Vec Std.U8) (first : Bool) (s : Slice Std.U8)
    (h : out.val.length + 1 + s.val.length ≤ Usize.max) :
    ∃ v, with_word out first s = ok v ∧
      v.val = out.val ++ ((if first then [] else [32#u8]) ++ s.val) := by
  cases first
  · obtain ⟨o, ho, eo⟩ := push_ok out 32#u8 (by omega)
    obtain ⟨v, hv, ev⟩ := extend_ok o s (by rw [eo]; simp; omega)
    exact ⟨v, by simp [with_word, ho, hv], by rw [ev, eo]; simp⟩
  · obtain ⟨v, hv, ev⟩ := extend_ok out s (by omega)
    exact ⟨v, by simp [with_word, hv], by rw [ev]; simp⟩

/-- `shell_command` on bytes: the words, one space between them. -/
def joinB : List (List Nat) → List Nat
  | [] => []
  | [w] => w
  | w :: ws => w ++ 32 :: joinB ws

theorem joinB_snoc (xs : List (List Nat)) (y : List Nat) :
    joinB (xs ++ [y]) = if xs = [] then y else joinB xs ++ 32 :: y := by
  induction xs with
  | nil => rfl
  | cons x xs ih =>
    cases xs with
    | nil => rfl
    | cons x' xs' =>
      simp only [List.cons_append, joinB] at ih ⊢
      rw [ih]; simp

theorem deref_val {α : Type} (v : alloc.vec.Vec α) : (alloc.vec.Vec.deref v).val = v.val := by
  simp [alloc.vec.Vec.deref]

/-- The most bytes one word takes in the command: quoted, and a space. -/
def wordRoom (v : alloc.vec.Vec Std.U8) : Nat := 4 * v.val.length + 3

/-- One word of the command, as `shell_word` writes it. -/
def wordBytes (v : alloc.vec.Vec Std.U8) : List Nat := wordB (bs v.val)

theorem joinB_room (vs : List (alloc.vec.Vec Std.U8)) :
    (joinB (vs.map wordBytes)).length ≤ (vs.map wordRoom).sum := by
  induction vs with
  | nil => simp [joinB]
  | cons v vs ih =>
    have hw : (wordBytes v).length + 1 ≤ wordRoom v := by
      have := wordB_length (bs v.val)
      simp only [wordBytes, wordRoom, bs, List.length_map] at this ⊢; omega
    cases vs with
    | nil => simp [joinB]; omega
    | cons v' vs' =>
      simp only [List.map_cons, joinB, List.length_append, List.length_cons, List.sum_cons] at ih ⊢
      omega

theorem shell_command_ok (words : Slice (alloc.vec.Vec Std.U8))
    (room : (words.val.map wordRoom).sum ≤ Usize.max) :
    ∃ v, shell_command words = ok v ∧ bs v.val = joinB (words.val.map wordBytes) := by
  obtain ⟨r, hl, hq⟩ := loop_spec (fun (o, i) => shell_command_loop.body words o i)
    (fun p => p.2.val ≤ words.val.length ∧ bs p.1.val = joinB ((words.val.take p.2.val).map wordBytes))
    (fun _ r => bs r.val = joinB (words.val.map wordBytes))
    (fun p => words.val.length - p.2.val) (by
      rintro ⟨o, i⟩ ⟨hle, ho⟩
      dsimp only at hle ho
      by_cases hi : i.val < words.val.length
      · obtain ⟨j, hj, hjv⟩ := succ_ok words i hi
        right
        have htake : words.val.take (i.val + 1) = words.val.take i.val ++ [words.val[i.val]] := by
          rw [List.take_add_one, List.getElem?_eq_getElem hi]; rfl
        have hsum : ((words.val.take (i.val + 1)).map wordRoom).sum ≤ Usize.max := by
          have := congrArg List.sum (List.take_append_drop (i.val + 1) (words.val.map wordRoom))
          rw [List.sum_append, ← List.map_take] at this
          omega
        have hroom := joinB_room (words.val.take i.val)
        rw [← ho] at hroom
        have hF : ((words.val.take (i.val + 1)).map wordRoom).sum =
            ((words.val.take i.val).map wordRoom).sum + wordRoom words.val[i.val] := by
          rw [htake, List.map_append, List.sum_append]; simp
        have hFi : wordRoom words.val[i.val] = 4 * words.val[i.val].val.length + 3 := rfl
        simp only [bs, List.length_map] at hroom
        obtain ⟨w, hw, ew⟩ := shell_word_ok (alloc.vec.Vec.deref words.val[i.val])
          (by rw [deref_val]; omega)
        have hwl := congrArg List.length ew
        have hwb := wordB_length (bs (alloc.vec.Vec.deref words.val[i.val]).val)
        simp only [bs, List.length_map, deref_val] at hwl hwb
        obtain ⟨v, hv, ev⟩ := with_word_ok o (i = 0#usize) (alloc.vec.Vec.deref w)
          (by rw [deref_val]; omega)
        refine ⟨(v, j), ?_, ⟨show j.val ≤ _ by omega, ?_⟩,
          show words.val.length - j.val < words.val.length - i.val by omega, fun _ h => h⟩
        · simp [shell_command_loop.body, hi, index_ok words i hi, hw, hv, hj]
        · show bs v.val = _
          rw [hjv, htake, List.map_append, List.map_singleton, joinB_snoc, ev]
          have eG : bs (alloc.vec.Vec.deref w).val = wordBytes words.val[i.val] := by
            rw [deref_val, ew, deref_val]; rfl
          simp only [bs, List.map_append] at eG ho ⊢
          rw [eG, ho]
          by_cases h0 : i.val = 0
          · have hi0 : i = 0#usize := by rw [UScalar.eq_equiv]; simp [h0]
            simp [hi0, joinB]
          · have hi0 : ¬ i = 0#usize := by rw [UScalar.eq_equiv]; simp [h0]
            have hne : (words.val.take i.val).map wordBytes ≠ [] := by
              intro h; have := congrArg List.length h
              rw [List.length_map, List.length_take, List.length_nil] at this; omega
            simp only [hi0, hne, decide_false, Bool.false_eq_true, ↓reduceIte, List.map_cons,
              List.map_nil]
            rfl
      · left
        refine ⟨o, by simp [shell_command_loop.body, hi], ?_⟩
        simpa [List.take_of_length_le (show words.val.length ≤ i.val by omega)] using ho)
    (alloc.vec.Vec.new Std.U8, 0#usize) ⟨by simp, by simp [bs, joinB]⟩
  exact ⟨r, hl, hq⟩

/-- The bytes `sh_c` puts before the script: `sh -c '`. -/
def shCPrefix : List Nat := [115, 104, 32, 45, 99, 32, 39]

theorem sh_c_ok (script : Slice Std.U8) (room : script.val.length + 8 ≤ Usize.max) :
    ∃ o, sh_c script = ok o ∧ o.map (fun v => bs v.val) =
      if (bs script.val).any (· == 39) then none else some (shCPrefix ++ bs script.val ++ [39]) := by
  have hq := has_quote_ok script
  cases hb : script.val.any fun b => b.val == 39
  · obtain ⟨o1, h1, e1⟩ := push_ok (alloc.vec.Vec.new Std.U8) 115#u8 (by simp; omega)
    obtain ⟨o2, h2, e2⟩ := push_ok o1 104#u8 (by simp [e1]; omega)
    obtain ⟨o3, h3, e3⟩ := push_ok o2 32#u8 (by simp [e2, e1]; omega)
    obtain ⟨o4, h4, e4⟩ := push_ok o3 45#u8 (by simp [e3, e2, e1]; omega)
    obtain ⟨o5, h5, e5⟩ := push_ok o4 99#u8 (by simp [e4, e3, e2, e1]; omega)
    obtain ⟨o6, h6, e6⟩ := push_ok o5 32#u8 (by simp [e5, e4, e3, e2, e1]; omega)
    obtain ⟨o7, h7, e7⟩ := push_ok o6 39#u8 (by simp [e6, e5, e4, e3, e2, e1]; omega)
    obtain ⟨o8, h8, e8⟩ := extend_ok o7 script (by simp [e7, e6, e5, e4, e3, e2, e1]; omega)
    obtain ⟨o9, h9, e9⟩ := push_ok o8 39#u8 (by simp [e8, e7, e6, e5, e4, e3, e2, e1]; omega)
    refine ⟨some o9, by simp [sh_c, hq, hb, h1, h2, h3, h4, h5, h6, h7, h8, h9], ?_⟩
    have hb' : (bs script.val).any (· == 39) = false := by
      simpa [bs, List.any_map, Function.comp_def] using hb
    simp only [hb', Bool.false_eq_true, ↓reduceIte, Option.map_some, e9, e8, e7, e6, e5, e4, e3,
      e2, e1]
    simp [bs, shCPrefix]
  · refine ⟨none, by simp [sh_c, hq, hb], ?_⟩
    have hb' : (bs script.val).any (· == 39) = true := by
      simpa [bs, List.any_map, Function.comp_def] using hb
    simp [hb']

/-! ### From bytes to characters -/

open Headers (utf8 utf8_ascii utf8_high utf8_nil)

theorem char_beq (c d : Char) : (c == d) = (c.toNat == d.toNat) := by
  rw [Bool.eq_iff_iff, beq_iff_eq, beq_iff_eq]
  exact ⟨fun h => h ▸ rfl, fun h => Char.ext (UInt32.toNat_inj.mp (show c.val.toNat = d.val.toNat from h))⟩

/-- A character may stand bare exactly when its number is a bare byte. -/
theorem bareChar_eq (c : Char) : Clew.Ssh.bareChar c = bareB c.toNat := by
  simp only [Clew.Ssh.bareChar, Clew.Text.isAsciiAlphanumeric, Clew.Text.isAsciiDigit, char_beq]
  have h0 : ('0' : Char).val.toNat = 48 := rfl
  have h9 : ('9' : Char).val.toNat = 57 := rfl
  have ha : ('a' : Char).val.toNat = 97 := rfl
  have hz : ('z' : Char).val.toNat = 122 := rfl
  have hA : ('A' : Char).val.toNat = 65 := rfl
  have hZ : ('Z' : Char).val.toNat = 90 := rfl
  have e1 : ('-' : Char).val.toNat = 45 := rfl
  have e2 : ('_' : Char).val.toNat = 95 := rfl
  have e3 : ('.' : Char).val.toNat = 46 := rfl
  have e4 : ('/' : Char).val.toNat = 47 := rfl
  have e5 : ('@' : Char).val.toNat = 64 := rfl
  have e6 : (':' : Char).val.toNat = 58 := rfl
  have e7 : (',' : Char).val.toNat = 44 := rfl
  have e8 : ('+' : Char).val.toNat = 43 := rfl
  simp only [Char.le_def, UInt32.le_iff_toNat_le, bareB]
  unfold Char.toNat
  rw [h0, h9, ha, hz, hA, hZ, e1, e2, e3, e4, e5, e6, e7, e8, Bool.eq_iff_iff]
  simp only [Bool.or_eq_true, Bool.and_eq_true, decide_eq_true_eq, beq_iff_eq]
  omega

theorem bareB_high (n : Nat) (h : 128 ≤ n) : bareB n = false := by
  simp only [bareB, Bool.or_eq_false_iff, Bool.and_eq_false_iff, decide_eq_false_iff_not,
    beq_eq_false_iff_ne]
  omega

theorem char_all_bare (c : Char) :
    ((String.utf8EncodeChar c).map (·.toNat)).all bareB = Clew.Ssh.bareChar c := by
  rw [bareChar_eq]
  by_cases h : c.toNat < 128
  · rw [utf8_ascii c h]; simp
  · obtain ⟨hne, hhi⟩ := utf8_high c (by omega)
    rw [bareB_high _ (by omega)]
    cases hl : String.utf8EncodeChar c with
    | nil => exact absurd hl hne
    | cons b bs => simp [bareB_high b.toNat (hhi b (by rw [hl]; simp))]

theorem utf8_cons (c : Char) (cs : List Char) :
    utf8 (c :: cs) = (String.utf8EncodeChar c).map (·.toNat) ++ utf8 cs := by
  simp [utf8]

theorem utf8_append (a b : List Char) : utf8 (a ++ b) = utf8 a ++ utf8 b := by
  simp [utf8]

theorem utf8_all_bare (w : List Char) : (utf8 w).all bareB = w.all Clew.Ssh.bareChar := by
  induction w with
  | nil => rfl
  | cons c w ih => rw [utf8_cons, List.all_append, ih, char_all_bare, List.all_cons]

theorem utf8_isEmpty (w : List Char) : (utf8 w).isEmpty = w.isEmpty := by
  cases w with
  | nil => rfl
  | cons c w =>
    have : utf8 (c :: w) ≠ [] := fun h => by simpa using (utf8_nil _).mp h
    simp [this]

theorem flatMap_qB_id (ns : List Nat) (h : ∀ n ∈ ns, n ≠ 39) : ns.flatMap qB = ns := by
  induction ns with
  | nil => rfl
  | cons n ns ih =>
    rw [List.flatMap_cons, ih (fun m hm => h m (by simp [hm]))]
    simp [qB, h n (by simp)]

theorem utf8_quoteChar (c : Char) :
    utf8 (Clew.Ssh.quoteChar c) = ((String.utf8EncodeChar c).map (·.toNat)).flatMap qB := by
  by_cases hc : c = '\''
  · subst hc; decide
  · have hq : Clew.Ssh.quoteChar c = [c] := by simp [Clew.Ssh.quoteChar, hc]
    rw [hq, flatMap_qB_id]
    · simp [utf8]
    · by_cases h : c.toNat < 128
      · rw [utf8_ascii c h]
        intro n hn
        simp only [List.mem_cons, List.not_mem_nil, or_false] at hn
        subst hn
        intro h39
        exact hc (Char.ext (UInt32.toNat_inj.mp (show c.val.toNat = ('\'' : Char).val.toNat from h39)))
      · obtain ⟨_, hhi⟩ := utf8_high c (by omega)
        intro n hn
        simp only [List.mem_map] at hn
        obtain ⟨b, hb, rfl⟩ := hn
        have := hhi b hb; omega

theorem utf8_flatMap_quoteChar (w : List Char) :
    utf8 (w.flatMap Clew.Ssh.quoteChar) = (utf8 w).flatMap qB := by
  induction w with
  | nil => rfl
  | cons c w ih => rw [List.flatMap_cons, utf8_append, ih, utf8_quoteChar, utf8_cons, List.flatMap_append]

theorem utf8_quote (w : List Char) : utf8 (Clew.Ssh.quote w) = 39 :: ((utf8 w).flatMap qB ++ [39]) := by
  rw [Clew.Ssh.quote, utf8_cons, utf8_append, utf8_flatMap_quoteChar]
  rfl

theorem utf8_shellWord (w : List Char) : utf8 (Clew.Ssh.shellWord w) = wordB (utf8 w) := by
  rw [Clew.Ssh.shellWord, wordB, utf8_isEmpty, utf8_all_bare]
  split <;> simp [utf8_quote]

theorem utf8_shellCommand (ws : List (List Char)) :
    utf8 (Clew.Ssh.shellCommand ws) = joinB (ws.map fun w => utf8 (Clew.Ssh.shellWord w)) := by
  induction ws with
  | nil => rfl
  | cons w ws ih =>
    cases ws with
    | nil => simp [Clew.Ssh.shellCommand, joinB]
    | cons w' ws' =>
      rw [Clew.Ssh.shellCommand.eq_3 w (w' :: ws') (by simp), utf8_append, utf8_cons, ih]
      simp [joinB]
      rfl

theorem utf8_any_quote (s : List Char) : (utf8 s).any (· == 39) = s.contains '\'' := by
  rw [List.contains_eq_any_beq]
  induction s with
  | nil => rfl
  | cons c s ih =>
    rw [utf8_cons, List.any_append, ih, List.any_cons, char_beq]
    congr 1
    by_cases h : c.toNat < 128
    · rw [utf8_ascii c h]; simp [eq_comm]
    · obtain ⟨_, hhi⟩ := utf8_high c (by omega)
      have h1 : (('\'' : Char).toNat == c.toNat) = false := by
        rw [beq_eq_false_iff_ne]; intro he; rw [← he] at h; exact h (by decide)
      rw [h1, List.any_eq_false]
      intro n hn
      simp only [List.mem_map] at hn
      obtain ⟨b, hb, rfl⟩ := hn
      have := hhi b hb
      simp; omega

/-! ### The theorems -/

/-- **`quote` is the model**: on the UTF-8 bytes of any word short enough to
quote in memory, it returns, and returns the UTF-8 bytes of `Clew.Ssh.quote`. -/
theorem quote_is_the_model (w : List Char) (word : Slice Std.U8) (h : bs word.val = utf8 w)
    (room : 4 * word.val.length + 2 ≤ Usize.max) :
    ∃ v, quote word = ok v ∧ bs v.val = utf8 (Clew.Ssh.quote w) := by
  obtain ⟨v, hv, ev⟩ := quote_ok word room
  exact ⟨v, hv, by rw [ev, h, utf8_quote]⟩

theorem shell_word_is_the_model (w : List Char) (word : Slice Std.U8) (h : bs word.val = utf8 w)
    (room : 4 * word.val.length + 2 ≤ Usize.max) :
    ∃ v, shell_word word = ok v ∧ bs v.val = utf8 (Clew.Ssh.shellWord w) := by
  obtain ⟨v, hv, ev⟩ := shell_word_ok word room
  exact ⟨v, hv, by rw [ev, h, utf8_shellWord]⟩

/-- **`shell_command` is the model**: on the UTF-8 bytes of any words, it
returns, and returns the UTF-8 bytes of `Clew.Ssh.shellCommand`, so the
command reads back as the same words (`Clew.Ssh.readWords_shellCommand`). -/
theorem shell_command_is_the_model (ws : List (List Char)) (words : Slice (alloc.vec.Vec Std.U8))
    (h : words.val.map (fun v => bs v.val) = ws.map utf8)
    (room : (words.val.map fun v => 4 * v.val.length + 3).sum ≤ Usize.max) :
    ∃ v, shell_command words = ok v ∧ bs v.val = utf8 (Clew.Ssh.shellCommand ws) := by
  obtain ⟨v, hv, ev⟩ := shell_command_ok words room
  refine ⟨v, hv, ?_⟩
  rw [ev, utf8_shellCommand]
  have : words.val.map wordBytes = (words.val.map fun v => bs v.val).map wordB := by
    simp [wordBytes, Function.comp_def]
  rw [this, h]
  simp [Function.comp_def, utf8_shellWord]

/-- **`sh_c` is the model**: on the UTF-8 bytes of any script, it returns,
refuses exactly the scripts that hold a `'`, and otherwise returns the UTF-8
bytes of `Clew.Ssh.shC`. -/
theorem sh_c_is_the_model (s : List Char) (script : Slice Std.U8) (h : bs script.val = utf8 s)
    (room : script.val.length + 8 ≤ Usize.max) :
    ∃ o, sh_c script = ok o ∧ o.map (fun v => bs v.val) = (Clew.Ssh.shC s).map utf8 := by
  obtain ⟨o, ho, eo⟩ := sh_c_ok script room
  refine ⟨o, ho, ?_⟩
  rw [eo, h, utf8_any_quote, Clew.Ssh.shC]
  split
  · rfl
  · simp only [Option.map_some, utf8_append]
    rfl

end Shell

/-! ## Which host a recorded ssh key vouches for — `clew_rules::known_hosts`

Before Clew records a host key, `plain_host_field` checks the known_hosts
host field it is to be recorded under, and nothing is recorded when it says
no. The model in `../Clew/Ssh.lean` reads characters, and proves that a
field it accepts names exactly one host and port, with none of known_hosts'
pattern syntax (`plainHostField_literal`). Proved here: on the UTF-8 bytes
of any field, the Rust function returns, and decides what the model decides
of the field's characters. -/

namespace KnownHosts

open clew_rules.known_hosts
open Shell (bs)

/-- What `is_name_byte` lets a host name hold: ASCII letters and digits and
`.-_:`. -/
def nameB (n : Nat) : Bool :=
  (48 ≤ n && n ≤ 57) || (65 ≤ n && n ≤ 90) || (97 ≤ n && n ≤ 122) ||
    n == 46 || n == 45 || n == 95 || n == 58

theorem is_name_byte_ok (b : Std.U8) : is_name_byte b = ok (nameB b.val) := by
  unfold is_name_byte
  simp only [ge_iff_le, UScalar.le_equiv, UScalar.eq_equiv, U8.ofNat, UScalar.ofNat,
    UScalar.ofNatCore_val_eq, Framing.ite_ok, Result.ok.injEq]
  have hb := Framing.u8_lt b
  generalize b.val = v at hb ⊢
  revert v; decide +kernel

/-- The bytes `l[f..t]`. -/
def seg (l : List Std.U8) (f t : Nat) : List Std.U8 := (l.drop f).take (t - f)

theorem seg_cons (l : List Std.U8) (i t : Nat) (hi : i < t) (ht : t ≤ l.length) :
    seg l i t = l[i]'(by omega) :: seg l (i + 1) t := by
  simp only [seg]
  rw [List.drop_eq_getElem_cons (by omega), show t - i = (t - (i + 1)) + 1 by omega, List.take_succ_cons]

theorem all_name_ok (field : Slice Std.U8) (f t : Std.Usize) (ht : t.val ≤ field.val.length) :
    all_name field f t = ok ((seg field.val f.val t.val).all fun b => nameB b.val) := by
  obtain ⟨b, hl, hq⟩ := loop_spec (all_name_loop.body field t) (fun _ => True)
    (fun i b => b = (seg field.val i.val t.val).all fun b => nameB b.val)
    (fun i => t.val - i.val) (by
      intro i _
      by_cases hi : i.val < t.val
      · have hil : i.val < field.val.length := by omega
        obtain ⟨j, hj, hjv⟩ := succ_ok field i hil
        rw [seg_cons _ _ _ hi ht]
        cases hn : nameB field.val[i.val].val
        · left
          exact ⟨false, by simp [all_name_loop.body, hi, index_ok field i hil, is_name_byte_ok, hn],
            by simp [hn]⟩
        · right
          refine ⟨j, by simp [all_name_loop.body, hi, index_ok field i hil, is_name_byte_ok, hn, hj],
            trivial, by omega, ?_⟩
          intro b hb; rw [hb, hjv, List.all_cons, hn, Bool.true_and]
      · left
        refine ⟨true, by simp [all_name_loop.body, hi], ?_⟩
        simp [seg, show t.val - i.val = 0 by omega]) f trivial
  unfold all_name all_name_loop
  rw [hl, hq]

/-- A host name: 1 to 255 bytes, each a name byte. -/
def nameOkB (ns : List Nat) : Bool := !ns.isEmpty && ns.length ≤ 255 && ns.all nameB

theorem name_ok_ok (field : Slice Std.U8) (f t : Std.Usize) (ht : t.val ≤ field.val.length) :
    name_ok field f t = ok (nameOkB (bs (seg field.val f.val t.val))) := by
  unfold name_ok
  have hlen : (seg field.val f.val t.val).length = t.val - f.val := by simp [seg]; omega
  by_cases h1 : t.val ≤ f.val
  · have : seg field.val f.val t.val = [] := by simp [seg, show t.val - f.val = 0 by omega]
    simp [UScalar.le_equiv, h1, nameOkB, this, bs]
  · obtain ⟨d, hd, hdv⟩ := ok_of_spec (UScalar.sub_spec (x := t) (y := f) (by omega))
    have hne : seg field.val f.val t.val ≠ [] := by intro h; rw [h] at hlen; simp at hlen; omega
    by_cases h2 : 255 < t.val - f.val
    · simp [UScalar.le_equiv, h1, hd, UScalar.lt_equiv, hdv, h2, nameOkB, bs, hlen]
      omega
    · simp [UScalar.le_equiv, h1, hd, UScalar.lt_equiv, hdv, h2, nameOkB, bs, hlen, all_name_ok field f t ht,
        hne, List.all_map, Function.comp_def]
      omega

/-- `split_once("]:")` on bytes. -/
def splitB : List Nat → Option (List Nat × List Nat)
  | [] => none
  | c :: r =>
    match r with
    | d :: r' => if c == 93 && d == 58 then some ([], r') else
        (splitB r).map fun p => (c :: p.1, p.2)
    | [] => none

theorem splitB_skip (x : Nat) (r : List Nat) (h : x ≠ 93 ∨ r.head? ≠ some 58) :
    splitB (x :: r) = (splitB r).map fun p => (x :: p.1, p.2) := by
  cases r with
  | nil => rfl
  | cons d r' =>
    simp only [List.head?_cons, ne_eq, Option.some.injEq] at h
    have : ¬ (x == 93 && d == 58) = true := by
      simp only [Bool.and_eq_true, beq_iff_eq]; omega
    simp only [splitB, this, Bool.false_eq_true, ↓reduceIte]

theorem bs_drop (l : List Std.U8) (i : Nat) (h : i < l.length) :
    bs (l.drop i) = l[i].val :: bs (l.drop (i + 1)) := by
  rw [List.drop_eq_getElem_cons h]; rfl

theorem find_close_ok (field : Slice Std.U8) (f : Std.Usize) :
    ∃ o, find_close field f = ok o ∧
      match o with
      | none => splitB (bs (field.val.drop f.val)) = none
      | some c => f.val ≤ c.val ∧ c.val + 1 < field.val.length ∧
          splitB (bs (field.val.drop f.val)) =
            some (bs (seg field.val f.val c.val), bs (field.val.drop (c.val + 2))) := by
  obtain ⟨o, hl, hq⟩ := loop_spec (find_close_loop.body field) (fun _ => True)
    (fun (i : Std.Usize) (o : Option Std.Usize) => match o with
      | none => splitB (bs (field.val.drop i.val)) = none
      | some c => i.val ≤ c.val ∧ c.val + 1 < field.val.length ∧
          splitB (bs (field.val.drop i.val)) =
            some (bs (seg field.val i.val c.val), bs (field.val.drop (c.val + 2))))
    (fun i => field.val.length - i.val) (by
      intro i _
      by_cases hi : i.val < field.val.length
      · obtain ⟨j, hj, hjv⟩ := succ_ok field i hi
        have hcont : ∀ o : Option Std.Usize, (match o with
            | none => splitB (bs (field.val.drop j.val)) = none
            | some c => j.val ≤ c.val ∧ c.val + 1 < field.val.length ∧
                splitB (bs (field.val.drop j.val)) =
                  some (bs (seg field.val j.val c.val), bs (field.val.drop (c.val + 2)))) →
            ¬ (field.val[i.val].val = 93 ∧ (bs (field.val.drop (i.val + 1))).head? = some 58) →
            (match o with
            | none => splitB (bs (field.val.drop i.val)) = none
            | some c => i.val ≤ c.val ∧ c.val + 1 < field.val.length ∧
                splitB (bs (field.val.drop i.val)) =
                  some (bs (seg field.val i.val c.val), bs (field.val.drop (c.val + 2)))) := by
          intro o ho hn
          rw [bs_drop _ _ hi, splitB_skip _ _ (not_and_or.mp hn)]
          rw [hjv] at ho
          cases o with
          | none => simp only at ho ⊢; rw [ho]; rfl
          | some c =>
            obtain ⟨h1, h2, h3⟩ := ho
            refine ⟨by omega, h2, ?_⟩
            rw [h3, seg_cons field.val i.val c.val (by omega) (by omega)]; rfl
        by_cases hb : field.val[i.val].val = 93
        · have hb' : field.val[i.val] = 93#u8 := by
            rw [UScalar.eq_equiv]; simp [hb, U8.ofNat, UScalar.ofNat, UScalar.ofNatCore_val_eq]
          by_cases hn : i.val + 1 < field.val.length
          · by_cases hc : field.val[i.val + 1].val = 58
            · left
              have hc' : field.val[j.val]'(by omega) = 58#u8 := by
                rw [UScalar.eq_equiv]; simp [hjv, hc, U8.ofNat, UScalar.ofNat, UScalar.ofNatCore_val_eq]
              refine ⟨some i, ?_, ?_⟩
              · simp [find_close_loop.body, hi, index_ok field i hi, hb', hj, hjv ▸ hn,
                  index_ok field j (by omega), hc']
              · refine ⟨le_refl _, hn, ?_⟩
                rw [bs_drop _ _ hi, bs_drop _ _ hn, hb, hc]
                simp [splitB, seg, bs]
            · right
              have hc' : ¬ field.val[j.val]'(by omega) = 58#u8 := by
                rw [UScalar.eq_equiv]; simp [hjv, hc, U8.ofNat, UScalar.ofNat, UScalar.ofNatCore_val_eq]
              refine ⟨j, ?_, trivial, by omega, fun o ho => hcont o ho ?_⟩
              · simp [find_close_loop.body, hi, index_ok field i hi, hb', hj, hjv ▸ hn,
                  index_ok field j (by omega), hc']
              · rw [bs_drop _ _ hn]; simp [hc]
          · right
            refine ⟨j, ?_, trivial, by omega, fun o ho => hcont o ho ?_⟩
            · simp [find_close_loop.body, hi, index_ok field i hi, hb', hj, hjv, hn]
            · rw [List.drop_eq_nil_of_le (by omega)]; simp [bs]
        · right
          have hb' : ¬ field.val[i.val] = 93#u8 := by
            rw [UScalar.eq_equiv]; simp [hb, U8.ofNat, UScalar.ofNat, UScalar.ofNatCore_val_eq]
          refine ⟨j, ?_, trivial, by omega, fun o ho => hcont o ho (by omega)⟩
          simp [find_close_loop.body, hi, index_ok field i hi, hb', hj]
      · left
        refine ⟨none, by simp [find_close_loop.body, hi], ?_⟩
        simp only
        rw [List.drop_eq_nil_of_le (by omega)]; rfl) f trivial
  unfold find_close find_close_loop
  exact ⟨o, hl, hq⟩

/-- An ASCII digit. -/
def isDigitB (n : Nat) : Bool := 48 ≤ n && n ≤ 57

/-- The value of decimal digits after `a`, as `u16::from_str` adds them up. -/
def foldN (a : Nat) (ds : List Nat) : Nat := ds.foldl (fun acc d => acc * 10 + (d - 48)) a

theorem foldN_ge (a : Nat) (ds : List Nat) : a ≤ foldN a ds := by
  induction ds generalizing a with
  | nil => simp [foldN]
  | cons x xs ih =>
    simp only [foldN, List.foldl_cons]
    exact Nat.le_trans (by nlinarith) (ih _)

/-- What `digits_port` decides of its bytes: digits, at least one, worth
1 to 65535. -/
def digitsPortB (ds : List Nat) : Bool :=
  !ds.isEmpty && ds.all isDigitB && foldN 0 ds ≤ 65535 && foldN 0 ds != 0

theorem digits_port_loop_ok (field : Slice Std.U8) (v : Std.U32) (i : Std.Usize)
    (hv : v.val ≤ 65535) :
    digits_port_loop field v i = ok ((bs (field.val.drop i.val)).all isDigitB &&
      foldN v.val (bs (field.val.drop i.val)) ≤ 65535 &&
      foldN v.val (bs (field.val.drop i.val)) != 0) := by
  obtain ⟨r, hl, hq⟩ := loop_spec (fun (v, i) => digits_port_loop.body field v i)
    (fun s : Std.U32 × Std.Usize => s.1.val ≤ 65535)
    (fun s r => r = ((bs (field.val.drop s.2.val)).all isDigitB &&
      foldN s.1.val (bs (field.val.drop s.2.val)) ≤ 65535 &&
      foldN s.1.val (bs (field.val.drop s.2.val)) != 0))
    (fun s => field.val.length - s.2.val) (by
      rintro ⟨v, i⟩ hv
      dsimp only at hv ⊢
      by_cases hi : i.val < field.val.length
      · rw [bs_drop _ _ hi]
        set x := field.val[i.val] with hx
        by_cases hlo : x.val < 48
        · left
          refine ⟨false, ?_, ?_⟩
          · simp [digits_port_loop.body, hi, index_ok field i hi, ← hx, UScalar.lt_equiv, U8.ofNat,
              UScalar.ofNat, UScalar.ofNatCore_val_eq, hlo]
          · simp [isDigitB, show ¬ 48 ≤ x.val by omega]
        by_cases hhi : 57 < x.val
        · left
          refine ⟨false, ?_, ?_⟩
          · simp [digits_port_loop.body, hi, index_ok field i hi, ← hx, UScalar.lt_equiv, U8.ofNat,
              UScalar.ofNat, UScalar.ofNatCore_val_eq, hlo, hhi]
          · simp [isDigitB, show ¬ x.val ≤ 57 by omega]
        obtain ⟨d8, hd8, hd8v⟩ := ok_of_spec (UScalar.sub_spec (x := x) (y := 48#u8) (by simp; omega))
        simp only [U8.ofNat, UScalar.ofNat, UScalar.ofNatCore_val_eq] at hd8v
        set d := UScalar.cast UScalarTy.U32 d8 with hddef
        have hdv : d.val = x.val - 48 := by rw [hddef, UScalar.cast_val_eq]; simp; omega
        obtain ⟨m1, hm1, hm1v⟩ := ok_of_spec (UScalar.mul_spec (x := v) (y := 10#u32)
          (by simp [U32.max_eq]; omega))
        obtain ⟨m2, hm2, hm2v⟩ := ok_of_spec (UScalar.add_spec (x := m1) (y := d)
          (by simp [U32.max_eq] at hm1v ⊢; omega))
        have hm2v' : m2.val = v.val * 10 + (x.val - 48) := by
          simp at hm1v; rw [hm2v, hm1v, hdv]
        have hfold : foldN v.val (x.val :: bs (field.val.drop (i.val + 1))) =
            foldN m2.val (bs (field.val.drop (i.val + 1))) := by
          rw [hm2v']; rfl
        have hdig : isDigitB x.val = true := by simp [isDigitB]; omega
        by_cases hover : 65535 < m2.val
        · left
          refine ⟨false, ?_, ?_⟩
          · simp [digits_port_loop.body, hi, index_ok field i hi, ← hx, UScalar.lt_equiv, U8.ofNat,
              UScalar.ofNat, UScalar.ofNatCore_val_eq, hlo, hhi, hd8, lift, ← hddef, hm1, hm2, hover]
          · rw [hfold]
            have := foldN_ge m2.val (bs (field.val.drop (i.val + 1)))
            simp only [List.all_cons, hdig, Bool.true_and]
            simp; omega
        · right
          obtain ⟨j, hj, hjv⟩ := succ_ok field i hi
          refine ⟨(m2, j), ?_, show m2.val ≤ 65535 by omega,
            show field.val.length - j.val < field.val.length - i.val by omega, ?_⟩
          · simp [digits_port_loop.body, hi, index_ok field i hi, ← hx, UScalar.lt_equiv, U8.ofNat,
              UScalar.ofNat, UScalar.ofNatCore_val_eq, hlo, hhi, hd8, lift, ← hddef, hm1, hm2, hover, hj]
          · intro r hr
            dsimp only at hr
            rw [hr, hfold, hjv, List.all_cons, hdig, Bool.true_and]
      · left
        refine ⟨v != 0#u32, by simp [digits_port_loop.body, hi], ?_⟩
        rw [List.drop_eq_nil_of_le (by omega)]
        have hz : (v != 0#u32) = (v.val != 0) := by
          rw [Bool.eq_iff_iff]; simp [UScalar.eq_equiv]
        simp [bs, foldN, hv, hz]) (v, i) hv
  unfold digits_port_loop
  exact hl.trans (congrArg ok hq)

theorem digits_port_ok (field : Slice Std.U8) (f : Std.Usize) :
    digits_port field f = ok (digitsPortB (bs (field.val.drop f.val))) := by
  unfold digits_port
  by_cases hf : field.val.length ≤ f.val
  · simp [UScalar.le_equiv, hf, digitsPortB, bs]
  · have hne : (bs (field.val.drop f.val)).isEmpty = false := by
      rw [bs_drop _ _ (by omega)]; rfl
    simp [UScalar.le_equiv, hf, digits_port_loop_ok field 0#u32 f (by simp), digitsPortB, hne]

/-- What `port_ok` decides: an optional `+`, then what `digits_port` decides. -/
def portB (ns : List Nat) : Bool :=
  match ns with
  | c :: rest => if c = 43 then digitsPortB rest else digitsPortB ns
  | [] => digitsPortB []

theorem port_ok_ok (field : Slice Std.U8) (f : Std.Usize) :
    port_ok field f = ok (portB (bs (field.val.drop f.val))) := by
  unfold port_ok
  by_cases hf : f.val < field.val.length
  · rw [bs_drop _ _ hf]
    by_cases hp : field.val[f.val].val = 43
    · have hp' : field.val[f.val] = 43#u8 := by
        rw [UScalar.eq_equiv]; simp [hp, U8.ofNat, UScalar.ofNat, UScalar.ofNatCore_val_eq]
      obtain ⟨g, hg, hgv⟩ := succ_ok field f hf
      simp [hf, index_ok field f hf, hp', hg, digits_port_ok, hgv, portB]
    · have hp' : ¬ field.val[f.val] = 43#u8 := by
        rw [UScalar.eq_equiv]; simp [hp, U8.ofNat, UScalar.ofNat, UScalar.ofNatCore_val_eq]
      simp [hf, index_ok field f hf, hp', digits_port_ok, portB, hp, bs_drop _ _ hf]
  · simp [hf, digits_port_ok, List.drop_eq_nil_of_le (show field.val.length ≤ f.val by omega), portB, bs]

/-- `plain_host_field` on bytes. -/
def plainB (ns : List Nat) : Bool :=
  match ns with
  | c :: rest =>
    if c = 91 then
      match splitB rest with
      | some (n, p) => nameOkB n && portB p
      | none => false
    else nameOkB ns
  | [] => false

theorem plain_host_field_ok (field : Slice Std.U8) :
    plain_host_field field = ok (plainB (bs field.val)) := by
  unfold plain_host_field
  by_cases he : field.val = []
  · have h0 : (Slice.len field).val = 0 := by simp [he]
    simp [core.slice.Slice.is_empty, he, name_ok_ok field 0#usize (Slice.len field) (by simp),
      plainB, bs, nameOkB, seg]
  have hpos : 0 < field.val.length := List.length_pos_iff.mpr he
  have hall : seg field.val 0 field.val.length = field.val := by simp [seg]
  have hb : bs field.val = field.val[0].val :: bs (field.val.drop 1) := by
    rw [← bs_drop _ _ hpos]; rfl
  have hz : (0#usize).val = 0 := by simp
  by_cases h91 : field.val[0].val = 91
  · have h91' : field.val[0] = 91#u8 := by
      rw [UScalar.eq_equiv]; simp [h91, U8.ofNat, UScalar.ofNat, UScalar.ofNatCore_val_eq]
    obtain ⟨o, ho, hq⟩ := find_close_ok field 1#usize
    simp only [show (1#usize).val = 1 by simp] at hq
    have hidx : Slice.index_usize field 0#usize = ok field.val[0] := index_ok field 0#usize (by simp; omega)
    cases o with
    | none =>
      simp only [List.drop_one] at hq
      simp [core.slice.Slice.is_empty, he, hidx, h91', ho, hb, plainB, hq]
    | some c =>
      obtain ⟨h1, h2, h3⟩ := hq
      simp only [List.drop_one] at h3
      have := Slice.length_ineq field
      obtain ⟨c2, hc2, hc2v⟩ := ok_of_spec (UScalar.add_spec (x := c) (y := 2#usize)
        (by scalar_tac))
      simp only [show (2#usize).val = 2 by simp] at hc2v
      simp [core.slice.Slice.is_empty, he, hidx, h91', ho, hb, plainB, h3,
        name_ok_ok field 1#usize c (by omega), hc2, port_ok_ok, hc2v]
      split <;> simp_all
  · have h91' : ¬ field.val[0] = 91#u8 := by
      rw [UScalar.eq_equiv]; simp [h91, U8.ofNat, UScalar.ofNat, UScalar.ofNatCore_val_eq]
    have hidx : Slice.index_usize field 0#usize = ok field.val[0] := index_ok field 0#usize (by simp; omega)
    simp [core.slice.Slice.is_empty, he, hidx, h91', name_ok_ok field 0#usize (Slice.len field) (by simp),
      hall, plainB, hb, h91]

/-! ### From bytes to characters -/

open Headers (utf8 utf8_ascii utf8_high)
open Shell (utf8_cons utf8_append utf8_isEmpty)

theorem toNat_ne (c d : Char) (h : c ≠ d) : c.toNat ≠ d.toNat :=
  fun he => h (Char.ext (UInt32.toNat_inj.mp (show c.val.toNat = d.val.toNat from he)))

/-- No byte of a character is an ASCII byte the character is not. -/
theorem bytes_ne (c : Char) (k : Nat) (hk : k < 128) (hc : c.toNat ≠ k) :
    ∀ b ∈ (String.utf8EncodeChar c).map (·.toNat), b ≠ k := by
  by_cases h : c.toNat < 128
  · rw [utf8_ascii c h]; simpa using hc
  · obtain ⟨_, hhi⟩ := utf8_high c (by omega)
    intro b hb
    simp only [List.mem_map] at hb
    obtain ⟨x, hx, rfl⟩ := hb
    have := hhi x hx; omega

theorem utf8_cons_head (c : Char) (r : List Char) (k : Nat) (hk : k < 128) (hc : c.toNat ≠ k) :
    ∃ b tl, utf8 (c :: r) = b :: tl ∧ b ≠ k := by
  rw [utf8_cons]
  cases hl : (String.utf8EncodeChar c).map (·.toNat) with
  | nil => exact absurd (List.map_eq_nil_iff.mp hl) (Headers.utf8EncodeChar_ne_nil c)
  | cons b bs => exact ⟨b, bs ++ utf8 r, rfl, bytes_ne c k hk hc b (by rw [hl]; simp)⟩

/-- A byte test no byte of 128 or more passes, which agrees with a character
test on every character's number, agrees with it on UTF-8 bytes. -/
theorem utf8_all (P : Nat → Bool) (Q : Char → Bool) (hq : ∀ c, Q c = P c.toNat)
    (hp : ∀ n, 128 ≤ n → P n = false) (w : List Char) : (utf8 w).all P = w.all Q := by
  induction w with
  | nil => rfl
  | cons c w ih =>
    rw [utf8_cons, List.all_append, ih, List.all_cons, hq]
    congr 1
    by_cases h : c.toNat < 128
    · rw [utf8_ascii c h]; simp
    · obtain ⟨hne, hhi⟩ := utf8_high c (by omega)
      rw [hp _ (by omega)]
      cases hl : String.utf8EncodeChar c with
      | nil => exact absurd hl hne
      | cons b bs => simp [hp b.toNat (hhi b (by rw [hl]; simp))]

theorem utf8_of_ascii (w : List Char) (h : ∀ c ∈ w, c.toNat < 128) : utf8 w = w.map Char.toNat := by
  induction w with
  | nil => rfl
  | cons c w ih =>
    rw [utf8_cons, utf8_ascii c (h c (by simp)), ih (fun d hd => h d (by simp [hd]))]; rfl

theorem nameChar_eq (c : Char) : Clew.Ssh.nameChar c = nameB c.toNat := by
  simp only [Clew.Ssh.nameChar, Clew.Text.isAsciiAlphanumeric, Clew.Text.isAsciiDigit, Shell.char_beq]
  have h0 : ('0' : Char).val.toNat = 48 := rfl
  have h9 : ('9' : Char).val.toNat = 57 := rfl
  have ha : ('a' : Char).val.toNat = 97 := rfl
  have hz : ('z' : Char).val.toNat = 122 := rfl
  have hA : ('A' : Char).val.toNat = 65 := rfl
  have hZ : ('Z' : Char).val.toNat = 90 := rfl
  have e1 : ('.' : Char).val.toNat = 46 := rfl
  have e2 : ('-' : Char).val.toNat = 45 := rfl
  have e3 : ('_' : Char).val.toNat = 95 := rfl
  have e4 : (':' : Char).val.toNat = 58 := rfl
  simp only [Char.le_def, UInt32.le_iff_toNat_le, nameB]
  unfold Char.toNat
  rw [h0, h9, ha, hz, hA, hZ, e1, e2, e3, e4, Bool.eq_iff_iff]
  simp only [Bool.or_eq_true, Bool.and_eq_true, decide_eq_true_eq, beq_iff_eq]
  omega

theorem nameB_high (n : Nat) (h : 128 ≤ n) : nameB n = false := by
  simp only [nameB, Bool.or_eq_false_iff, Bool.and_eq_false_iff, decide_eq_false_iff_not,
    beq_eq_false_iff_ne]
  omega

theorem nameOkB_utf8 (n : List Char) : nameOkB (utf8 n) = Clew.Ssh.nameOk n := by
  unfold nameOkB Clew.Ssh.nameOk
  rw [utf8_isEmpty, utf8_all nameB Clew.Ssh.nameChar nameChar_eq nameB_high]
  cases hall : n.all Clew.Ssh.nameChar
  · simp
  · have hascii : ∀ c ∈ n, c.toNat < 128 := by
      intro c hc
      have := List.all_eq_true.mp hall c hc
      rw [nameChar_eq] at this
      by_contra h
      rw [nameB_high _ (by omega)] at this
      exact absurd this (by simp)
    rw [utf8_of_ascii n hascii, List.length_map]

theorem splitB_skip_list (xs ys : List Nat) (h : ∀ x ∈ xs, x ≠ 93) :
    splitB (xs ++ ys) = (splitB ys).map fun p => (xs ++ p.1, p.2) := by
  induction xs with
  | nil => simp only [List.nil_append]; cases splitB ys <;> rfl
  | cons x xs ih =>
    rw [List.cons_append, splitB_skip x _ (Or.inl (h x (by simp))),
      ih (fun y hy => h y (by simp [hy])), Option.map_map]
    rfl

theorem sBC_skip (c : Char) (r : List Char) (hc : c ≠ ']') :
    Clew.Ssh.splitBracketColon (c :: r) =
      (Clew.Ssh.splitBracketColon r).map fun p => (c :: p.1, p.2) := by
  cases r with
  | nil => rfl
  | cons d r' =>
    have : (c == ']' && d == ':') = false := by simp [hc]
    simp only [Clew.Ssh.splitBracketColon, this, Bool.false_eq_true, ↓reduceIte]

theorem splitB_utf8 (rest : List Char) :
    splitB (utf8 rest) =
      (Clew.Ssh.splitBracketColon rest).map fun p => (utf8 p.1, utf8 p.2) := by
  induction rest with
  | nil => rfl
  | cons c r ih =>
    by_cases hc : c = ']'
    · subst hc
      have e : utf8 (']' :: r) = 93 :: utf8 r := by rw [utf8_cons]; rfl
      cases r with
      | nil => rfl
      | cons d r' =>
        by_cases hd : d = ':'
        · subst hd
          have e2 : utf8 (':' :: r') = 58 :: utf8 r' := by rw [utf8_cons]; rfl
          rw [e, e2]
          simp [splitB, Clew.Ssh.splitBracketColon, utf8]
        · obtain ⟨b, tl, hbt, hb⟩ := utf8_cons_head d r' 58 (by omega) (toNat_ne d ':' hd)
          have hsk : splitB (93 :: utf8 (d :: r')) =
              (splitB (utf8 (d :: r'))).map fun p => (93 :: p.1, p.2) :=
            splitB_skip 93 _ (Or.inr (by rw [hbt]; simpa using hb))
          have hm : (']' == ']' && d == ':') = false := by simp [hd]
          have hs : Clew.Ssh.splitBracketColon (']' :: d :: r') =
              (Clew.Ssh.splitBracketColon (d :: r')).map fun p => (']' :: p.1, p.2) := by
            rw [Clew.Ssh.splitBracketColon.eq_2]
            simp only [hm, Bool.false_eq_true, ↓reduceIte]
          rw [e, hsk, ih, hs, Option.map_map, Option.map_map]
          congr 1
    · rw [utf8_cons, splitB_skip_list _ _ (bytes_ne c 93 (by omega) (toNat_ne c ']' hc)), ih,
        sBC_skip c r hc, Option.map_map, Option.map_map]
      congr 1
      funext p
      simp [utf8_cons]

/-- After its optional `+`: digits, at least one, worth 1 to 65535 — what
`plainHostField` asks of a port, through `parseU16`. -/
def digitsOk (ds : List Char) : Bool :=
  !ds.isEmpty && ds.all Clew.Text.isAsciiDigit && Clew.Text.digitsValue ds < 65536 &&
    Clew.Text.digitsValue ds != 0

/-- What `u64::from_str` reads digits from: after a leading `+`, if any. -/
def stripPlus (s : List Char) : List Char :=
  match s with
  | '+' :: rest => rest
  | _ => s

/-- `parseU64` after `stripPlus`. -/
def parseDigits (ds : List Char) : Option Nat :=
  if ds.isEmpty || !ds.all Clew.Text.isAsciiDigit then none
  else if Clew.Text.digitsValue ds < 2 ^ 64 then some (Clew.Text.digitsValue ds) else none

theorem parseU64_eq (p : List Char) : Clew.Text.parseU64 p = parseDigits (stripPlus p) := by
  rfl

theorem port_check_eq (p : List Char) :
    (match Clew.Ssh.parseU16 p with
      | some n => n != 0
      | none => false) = digitsOk (stripPlus p) := by
  unfold Clew.Ssh.parseU16
  rw [parseU64_eq]
  generalize stripPlus p = ds
  unfold parseDigits digitsOk
  by_cases h1 : ds.isEmpty <;> by_cases h2 : ds.all Clew.Text.isAsciiDigit <;> simp [h1, h2]
  by_cases h3 : Clew.Text.digitsValue ds < 65536
  · have h64 : Clew.Text.digitsValue ds < 18446744073709551616 := by omega
    simp [h3, h64]
  · by_cases h64 : Clew.Text.digitsValue ds < 18446744073709551616 <;> simp [h3, h64]

/-- `stripPlus` on bytes. -/
def stripB (ns : List Nat) : List Nat :=
  match ns with
  | c :: rest => if c = 43 then rest else ns
  | [] => []

theorem portB_eq (ns : List Nat) : portB ns = digitsPortB (stripB ns) := by
  cases ns with
  | nil => rfl
  | cons c r => simp only [portB, stripB]; split <;> rfl

theorem utf8_stripPlus (p : List Char) : utf8 (stripPlus p) = stripB (utf8 p) := by
  cases p with
  | nil => rfl
  | cons c r =>
    by_cases hc : c = '+'
    · subst hc
      have e : utf8 ('+' :: r) = 43 :: utf8 r := by rw [utf8_cons]; rfl
      rw [e]; rfl
    · obtain ⟨b, tl, hbt, hb⟩ := utf8_cons_head c r 43 (by omega) (toNat_ne c '+' hc)
      have hs : stripPlus (c :: r) = c :: r := by
        unfold stripPlus; split
        · rename_i h; exact absurd (List.cons.inj h).1 hc
        · rfl
      rw [hs, hbt]
      simp [stripB, hb]

theorem isAsciiDigit_eq (c : Char) : Clew.Text.isAsciiDigit c = isDigitB c.toNat := by
  simp only [Clew.Text.isAsciiDigit, isDigitB]
  have h0 : ('0' : Char).val.toNat = 48 := rfl
  have h9 : ('9' : Char).val.toNat = 57 := rfl
  simp only [Char.le_def, UInt32.le_iff_toNat_le]
  unfold Char.toNat
  rw [h0, h9]

theorem isDigitB_high (n : Nat) (h : 128 ≤ n) : isDigitB n = false := by
  simp only [isDigitB, Bool.and_eq_false_iff, decide_eq_false_iff_not]; omega

theorem digitsPortB_utf8 (ds : List Char) : digitsPortB (utf8 ds) = digitsOk ds := by
  unfold digitsPortB digitsOk
  rw [utf8_isEmpty, utf8_all isDigitB Clew.Text.isAsciiDigit isAsciiDigit_eq isDigitB_high]
  cases hall : ds.all Clew.Text.isAsciiDigit
  · simp
  · have hascii : ∀ c ∈ ds, c.toNat < 128 := by
      intro c hc
      have := List.all_eq_true.mp hall c hc
      rw [isAsciiDigit_eq] at this
      by_contra h
      rw [isDigitB_high _ (by omega)] at this
      exact absurd this (by simp)
    have hv : foldN 0 (utf8 ds) = Clew.Text.digitsValue ds := by
      rw [utf8_of_ascii ds hascii]
      unfold foldN Clew.Text.digitsValue
      rw [List.foldl_map]
      rfl
    rw [hv]
    simp only [Bool.and_true]
    congr 2

theorem plainB_utf8 (f : List Char) : plainB (utf8 f) = Clew.Ssh.plainHostField f := by
  cases f with
  | nil => rfl
  | cons c rest =>
    by_cases hc : c = '['
    · subst hc
      have e : utf8 ('[' :: rest) = 91 :: utf8 rest := by rw [utf8_cons]; rfl
      rw [e]
      simp only [plainB, ↓reduceIte, splitB_utf8]
      unfold Clew.Ssh.plainHostField
      simp only [beq_self_eq_true, ↓reduceIte]
      cases Clew.Ssh.splitBracketColon rest with
      | none => rfl
      | some q =>
        obtain ⟨n, p⟩ := q
        simp only [Option.map_some, nameOkB_utf8, portB_eq, ← utf8_stripPlus, digitsPortB_utf8]
        rw [← port_check_eq p]
        rfl
    · obtain ⟨b, tl, hbt, hb⟩ := utf8_cons_head c rest 91 (by omega) (toNat_ne c '[' hc)
      rw [hbt]
      simp only [plainB, hb, ↓reduceIte]
      rw [← hbt, nameOkB_utf8]
      unfold Clew.Ssh.plainHostField
      simp [hc]

/-- **`plain_host_field` is the model**: on the UTF-8 bytes of any host
field, it returns, and says what `Clew.Ssh.plainHostField` says of the
field's characters — so a field it accepts names one host and port, and
holds none of known_hosts' pattern syntax (`plainHostField_literal`). -/
theorem plain_host_field_is_the_model (f : List Char) (field : Slice Std.U8)
    (h : bs field.val = utf8 f) :
    plain_host_field field = ok (Clew.Ssh.plainHostField f) := by
  rw [plain_host_field_ok, h, plainB_utf8]

end KnownHosts

/-! ## Which server binary may be deployed — `clew_rules::server_dist`

`slug` turns the platform a remote's `uname` reports into the name of the
server binary to deploy there, and that name is joined into a cache path and
a download URL. It refuses any platform whose lower-cased name `slug_ok`
says no to. The model in `../Clew/ServerDist.lean` proves that a slug it
accepts is a plain token, with nothing that climbs out of a directory or
changes a URL (`slug_has_no_path_or_url_syntax`). Proved here: on the UTF-8
bytes of any name, the Rust check returns, and decides what the model's
check decides of the name's characters. -/

namespace ServerDist

open clew_rules.server_dist
open Shell (bs)
open Headers (utf8)

/-- What `is_slug_byte` lets a slug hold: ASCII letters and digits, `-`, `_`. -/
def slugB (n : Nat) : Bool :=
  (48 ≤ n && n ≤ 57) || (65 ≤ n && n ≤ 90) || (97 ≤ n && n ≤ 122) || n == 45 || n == 95

theorem is_slug_byte_ok (b : Std.U8) : is_slug_byte b = ok (slugB b.val) := by
  unfold is_slug_byte
  simp only [ge_iff_le, UScalar.le_equiv, UScalar.eq_equiv, U8.ofNat, UScalar.ofNat,
    UScalar.ofNatCore_val_eq, Framing.ite_ok, Result.ok.injEq]
  have hb := Framing.u8_lt b
  generalize b.val = v at hb ⊢
  revert v; decide +kernel

/-- What `slug_ok` decides: not empty, every byte a slug byte. -/
theorem slug_ok_spec (s : Slice Std.U8) :
    slug_ok s = ok (!s.val.isEmpty && s.val.all fun b => slugB b.val) := by
  unfold slug_ok
  by_cases he : s.val = []
  · simp [core.slice.Slice.is_empty, he]
  obtain ⟨b, hl, hq⟩ := loop_spec (slug_ok_loop.body s) (fun _ => True)
    (fun i b => b = (s.val.drop i.val).all fun b => slugB b.val)
    (fun i => s.val.length - i.val) (by
      intro i _
      by_cases hi : i.val < s.val.length
      · obtain ⟨j, hj, hjv⟩ := succ_ok s i hi
        rw [List.drop_eq_getElem_cons hi]
        cases ht : slugB s.val[i.val].val
        · left
          exact ⟨false, by simp [slug_ok_loop.body, hi, index_ok s i hi, is_slug_byte_ok, ht],
            by simp only [List.all_cons, ht, Bool.false_and]⟩
        · right
          refine ⟨j, by simp [slug_ok_loop.body, hi, index_ok s i hi, is_slug_byte_ok, ht, hj],
            trivial, by omega, ?_⟩
          intro b hb; rw [hb, hjv, List.all_cons, ht, Bool.true_and]
      · left
        refine ⟨true, by simp [slug_ok_loop.body, hi], ?_⟩
        rw [List.drop_eq_nil_of_le (by omega)]; rfl) 0#usize trivial
  simp only [core.slice.Slice.is_empty, Slice.length, bind_ok]
  unfold slug_ok_loop
  rw [hl, hq]
  simp [he]

theorem slugChar_eq (c : Char) : Clew.ServerDist.slugChar c = slugB c.toNat := by
  simp only [Clew.ServerDist.slugChar, Clew.Text.isAsciiAlphanumeric, Clew.Text.isAsciiDigit,
    Shell.char_beq]
  have h0 : ('0' : Char).val.toNat = 48 := rfl
  have h9 : ('9' : Char).val.toNat = 57 := rfl
  have ha : ('a' : Char).val.toNat = 97 := rfl
  have hz : ('z' : Char).val.toNat = 122 := rfl
  have hA : ('A' : Char).val.toNat = 65 := rfl
  have hZ : ('Z' : Char).val.toNat = 90 := rfl
  have e1 : ('-' : Char).val.toNat = 45 := rfl
  have e2 : ('_' : Char).val.toNat = 95 := rfl
  simp only [Char.le_def, UInt32.le_iff_toNat_le, slugB]
  unfold Char.toNat
  rw [h0, h9, ha, hz, hA, hZ, e1, e2, Bool.eq_iff_iff]
  simp only [Bool.or_eq_true, Bool.and_eq_true, decide_eq_true_eq, beq_iff_eq]
  omega

theorem slugB_high (n : Nat) (h : 128 ≤ n) : slugB n = false := by
  simp only [slugB, Bool.or_eq_false_iff, Bool.and_eq_false_iff, decide_eq_false_iff_not,
    beq_eq_false_iff_ne]
  omega

/-- **`slug_ok` is the model's check**: on the UTF-8 bytes of any name, it
returns, and accepts exactly the names `Clew.ServerDist.slug` accepts — not
empty, every character a `slugChar`. -/
theorem slug_ok_is_the_model (s : List Char) (bytes : Slice Std.U8) (h : bs bytes.val = utf8 s) :
    slug_ok bytes = ok (!s.isEmpty && s.all Clew.ServerDist.slugChar) := by
  rw [slug_ok_spec]
  have hall : (bytes.val.all fun b => slugB b.val) = (bs bytes.val).all slugB := by
    simp [bs, List.all_map, Function.comp_def]
  have hemp : bytes.val.isEmpty = (bs bytes.val).isEmpty := by
    cases bytes.val <;> rfl
  rw [hall, hemp, h, Shell.utf8_isEmpty,
    KnownHosts.utf8_all slugB Clew.ServerDist.slugChar slugChar_eq slugB_high]

end ServerDist

/-! ## What an approval covers — `clew_rules::fingerprint`

Every language-server fingerprint is a SHA-256 of `length_prefixed` parts,
one of them the `args_part` of the server's arguments. The model in
`../Clew/Trust.lean` proves those encodings injective — no rearrangement of
the parts, no argument holding the separators, reads the same — so the
three kinds of fingerprint hash disjoint inputs. Proved here: the Rust
functions return, and write exactly the model's encodings, for any input
that fits in memory. -/

namespace Fingerprint

open clew_rules.fingerprint
open Shell (bs push_ok extend_ok deref_val)

theorem digitChar_toNat (d : Nat) (h : d < 10) : (Clew.Text.digitChar d).toNat = 48 + d := by
  have : ∀ d < 10, (Clew.Text.digitChar d).toNat = 48 + d := by decide
  exact this d h

theorem digits_eq (n : Nat) : Clew.Trust.digits n =
    if n < 10 then [48 + n] else Clew.Trust.digits (n / 10) ++ [48 + n % 10] := by
  unfold Clew.Trust.digits
  rw [Clew.Text.decimal]
  split
  · simp [digitChar_toNat n ‹_›]
  · simp [digitChar_toNat (n % 10) (Nat.mod_lt _ (by omega))]

theorem digits_length_pos (n : Nat) : 1 ≤ (Clew.Trust.digits n).length := by
  rw [digits_eq]; split <;> simp

/-- `with_decimal` appends `n` in decimal: `Clew.Trust.digits n`. -/
theorem with_decimal_ok (n : Std.Usize) (out : alloc.vec.Vec Std.U8)
    (hroom : out.val.length + (Clew.Trust.digits n.val).length ≤ Usize.max) :
    ∃ v, with_decimal out n = ok v ∧ bs v.val = bs out.val ++ Clew.Trust.digits n.val := by
  induction hm : n.val using Nat.strong_induction_on generalizing n out with
  | _ m ih =>
  subst hm
  have hd := digits_eq n.val
  obtain ⟨r, hr, hrv⟩ := ok_of_spec (UScalar.rem_spec n (y := 10#usize) (by simp))
  simp only [show (10#usize).val = 10 by simp] at hrv
  set c := UScalar.cast UScalarTy.U8 r with hc
  have hcv : c.val = n.val % 10 := by
    rw [hc, UScalar.cast_val_eq, hrv]; simp; omega
  obtain ⟨d, hdd, hdv⟩ := ok_of_spec (UScalar.add_spec (x := 48#u8) (y := c)
    (by have := Nat.mod_lt n.val (show 10 > 0 by omega); simp [hcv, U8.max_eq]; omega))
  have hdv' : d.val = 48 + n.val % 10 := by rw [hdv, hcv]; simp
  by_cases h10 : 10 ≤ n.val
  · obtain ⟨q, hq, hqv⟩ := UScalar.div_spec n (y := 10#usize) (by simp)
    simp only [show (10#usize).val = 10 by simp] at hqv
    simp only [show ¬ n.val < 10 by omega, ↓reduceIte] at hd
    have hlen : (Clew.Trust.digits n.val).length = (Clew.Trust.digits (n.val / 10)).length + 1 := by
      rw [hd]; simp
    obtain ⟨v1, hv1, ev1⟩ := ih q.val (by omega) q out (by rw [hqv]; omega) rfl
    have hv1l : v1.val.length = out.val.length + (Clew.Trust.digits (n.val / 10)).length := by
      have := congrArg List.length ev1; simp [bs] at this; rw [hqv] at this; omega
    obtain ⟨v, hv, ev⟩ := push_ok v1 d (by omega)
    refine ⟨v, ?_, ?_⟩
    · rw [with_decimal]
      simp [UScalar.le_equiv, h10, hq, hv1, hr, lift, ← hc, hdd, hv]
    · rw [ev, hd]
      simp only [bs, List.map_append] at ev1 ⊢
      rw [ev1, hqv]; simp [hdv']
  · simp only [show n.val < 10 by omega, ↓reduceIte] at hd
    obtain ⟨v, hv, ev⟩ := push_ok out d (by rw [hd] at hroom; simp at hroom; omega)
    refine ⟨v, ?_, ?_⟩
    · rw [with_decimal]
      simp [UScalar.le_equiv, h10, hr, lift, ← hc, hdd, hv]
    · rw [ev, hd]
      simp only [bs, List.map_append]
      simp [hdv']; omega

theorem flatMap_take_succ {α β : Type} (f : α → List β) (l : List α) (i : Nat) (h : i < l.length) :
    (l.take (i + 1)).flatMap f = (l.take i).flatMap f ++ f l[i] := by
  rw [List.take_add_one, List.getElem?_eq_getElem h, Option.toList_some, List.flatMap_append]
  simp

theorem flatMap_take_length_le {α β : Type} (f : α → List β) (l : List α) (i : Nat) :
    ((l.take i).flatMap f).length ≤ (l.flatMap f).length := by
  conv => rhs; rw [← List.take_append_drop i l]
  rw [List.flatMap_append, List.length_append]; omega

/-- One argument as `args_part` writes it: `;<byte length>:<bytes>`. -/
def argPiece (a : List Nat) : List Nat := 59 :: (Clew.Trust.digits a.length ++ 58 :: a)

theorem argsPart_eq (args : List (List Nat)) :
    Clew.Trust.argsPart args = Clew.Trust.digits args.length ++ args.flatMap argPiece := rfl

theorem with_arg_ok (out : alloc.vec.Vec Std.U8) (arg : Slice Std.U8)
    (h : out.val.length + (argPiece (bs arg.val)).length ≤ Usize.max) :
    ∃ v, with_arg out arg = ok v ∧ bs v.val = bs out.val ++ argPiece (bs arg.val) := by
  simp only [argPiece, bs, List.length_cons, List.length_append, List.length_map] at h
  obtain ⟨o1, h1, e1⟩ := push_ok out 59#u8 (by omega)
  obtain ⟨o2, h2, e2⟩ := with_decimal_ok (Slice.len arg) o1 (by rw [e1]; simp; omega)
  have hl2 : o2.val.length = out.val.length + 1 + (Clew.Trust.digits arg.val.length).length := by
    have := congrArg List.length e2; simp [bs, e1] at this; omega
  obtain ⟨o3, h3, e3⟩ := push_ok o2 58#u8 (by omega)
  obtain ⟨v, hv, ev⟩ := extend_ok o3 arg (by rw [e3]; simp; omega)
  refine ⟨v, by simp [with_arg, h1, h2, h3, hv], ?_⟩
  rw [ev, e3]
  simp only [bs, List.map_append] at e2 ⊢
  rw [e2, e1]
  simp [argPiece]

/-- **`args_part` is the model**: for any arguments whose encoding fits in
memory, it returns, and writes exactly `Clew.Trust.argsPart` of their bytes,
which no other argument list writes (`argsPart_injective`). -/
theorem args_part_is_the_model (args : Slice (alloc.vec.Vec Std.U8))
    (room : (Clew.Trust.argsPart (args.val.map fun v => bs v.val)).length ≤ Usize.max) :
    ∃ v, args_part args = ok v ∧ bs v.val = Clew.Trust.argsPart (args.val.map fun v => bs v.val) := by
  rw [argsPart_eq] at room ⊢
  simp only [List.length_append, List.length_map] at room
  obtain ⟨o0, h0, e0⟩ := with_decimal_ok (Slice.len args) (alloc.vec.Vec.new Std.U8) (by simp; omega)
  set f : alloc.vec.Vec Std.U8 → List Nat := fun v => argPiece (bs v.val) with hf
  have hfl : ∀ l : List (alloc.vec.Vec Std.U8), (l.map fun v => bs v.val).flatMap argPiece = l.flatMap f := by
    intro l; simp [hf, List.flatMap_map]
  rw [hfl] at room ⊢
  obtain ⟨r, hl, hq⟩ := loop_spec (fun (o, i) => args_part_loop.body args o i)
    (fun p => p.2.val ≤ args.val.length ∧
      bs p.1.val = Clew.Trust.digits args.val.length ++ (args.val.take p.2.val).flatMap f)
    (fun _ r => bs r.val = Clew.Trust.digits args.val.length ++ args.val.flatMap f)
    (fun p => args.val.length - p.2.val) (by
      rintro ⟨o, i⟩ ⟨hle, ho⟩
      dsimp only at hle ho
      by_cases hi : i.val < args.val.length
      · obtain ⟨j, hj, hjv⟩ := succ_ok args i hi
        right
        have hpre := flatMap_take_length_le f args.val (i.val + 1)
        rw [flatMap_take_succ f args.val i.val hi, List.length_append] at hpre
        have hol : o.val.length = (Clew.Trust.digits args.val.length).length +
            ((args.val.take i.val).flatMap f).length := by
          have := congrArg List.length ho
          rw [List.length_append] at this; simp only [bs, List.length_map] at this; omega
        have hfi : (f args.val[i.val]).length = (argPiece (bs args.val[i.val].val)).length := rfl
        obtain ⟨v, hv, ev⟩ := with_arg_ok o (alloc.vec.Vec.deref args.val[i.val]) (by
          rw [deref_val]; omega)
        refine ⟨(v, j), ?_, ⟨show j.val ≤ _ by omega, ?_⟩,
          show args.val.length - j.val < args.val.length - i.val by omega, fun _ h => h⟩
        · simp [args_part_loop.body, hi, index_ok args i hi, hv, hj]
        · show bs v.val = _
          rw [ev, ho, hjv, flatMap_take_succ f args.val i.val hi, deref_val]
          simp [hf]
      · left
        refine ⟨o, by simp [args_part_loop.body, hi], ?_⟩
        simpa [List.take_of_length_le (show args.val.length ≤ i.val by omega)] using ho)
    (o0, 0#usize) ⟨by simp, show bs o0.val = _ by rw [e0]; simp [bs]⟩
  exact ⟨r, by simp [args_part, h0]; exact hl, by rw [hq, List.length_map]⟩

theorem leBytes_succ (k n : Nat) :
    Clew.Trust.leBytes (k + 1) n = Clew.Trust.leBytes k n ++ [n / 256 ^ k % 256] := by
  induction k generalizing n with
  | zero => simp [Clew.Trust.leBytes]
  | succ k ih =>
    rw [Clew.Trust.leBytes, ih (n / 256), Clew.Trust.leBytes, Nat.div_div_eq_div_mul, ← Nat.pow_succ']
    rfl

/-- `with_le64` appends the eight little-endian bytes of `n`. -/
theorem with_le64_ok (out : alloc.vec.Vec Std.U8) (n : Std.U64)
    (h : out.val.length + 8 ≤ Usize.max) :
    ∃ v, with_le64 out n = ok v ∧ bs v.val = bs out.val ++ Clew.Trust.leBytes 8 n.val := by
  obtain ⟨r, hl, hq⟩ := loop_spec (fun (o, rest, k) => with_le64_loop.body o rest k)
    (fun p : alloc.vec.Vec Std.U8 × Std.U64 × Std.Usize => p.2.2.val ≤ 8 ∧
      bs p.1.val = bs out.val ++ Clew.Trust.leBytes p.2.2.val n.val ∧
      p.2.1.val = n.val / 256 ^ p.2.2.val)
    (fun _ r => bs r.val = bs out.val ++ Clew.Trust.leBytes 8 n.val)
    (fun p => 8 - p.2.2.val) (by
      rintro ⟨o, rest, k⟩ ⟨hk, ho, hr⟩
      dsimp only at hk ho hr
      have hol : o.val.length = out.val.length + k.val := by
        have := congrArg List.length ho
        simp only [bs, List.length_append, List.length_map, Clew.Trust.leBytes_length] at this
        omega
      by_cases h8 : k.val < 8
      · right
        obtain ⟨x, hx, hxv⟩ := ok_of_spec (UScalar.rem_spec rest (y := 256#u64) (by simp))
        simp only [show (256#u64).val = 256 by simp] at hxv
        set c := UScalar.cast UScalarTy.U8 x with hc
        have hcv : c.val = rest.val % 256 := by rw [hc, UScalar.cast_val_eq, hxv]; simp
        obtain ⟨o1, h1, e1⟩ := push_ok o c (by omega)
        obtain ⟨q, hqd, hqv⟩ := UScalar.div_spec rest (y := 256#u64) (by simp)
        simp only [show (256#u64).val = 256 by simp] at hqv
        obtain ⟨k1, hk1, hk1v⟩ := ok_of_spec (UScalar.add_spec (x := k) (y := 1#usize) (by scalar_tac))
        simp only [show (1#usize).val = 1 by simp] at hk1v
        refine ⟨(o1, q, k1), ?_, ⟨show k1.val ≤ 8 by omega, ?_, ?_⟩,
          show 8 - k1.val < 8 - k.val by omega, fun _ h => h⟩
        · simp [with_le64_loop.body, UScalar.lt_equiv, h8, hx, lift, ← hc, h1, hqd, hk1]
        · show bs o1.val = _
          rw [e1, hk1v, leBytes_succ]
          simp only [bs, List.map_append] at ho ⊢
          rw [ho]; simp [hcv, hr]
        · show q.val = _
          rw [hqv, hr, hk1v, Nat.div_div_eq_div_mul, ← Nat.pow_succ]
      · left
        refine ⟨o, by simp [with_le64_loop.body, UScalar.lt_equiv, h8], ?_⟩
        rw [ho, show k.val = 8 by omega])
    (out, n, 0#usize) ⟨by simp, by simp [Clew.Trust.leBytes], by simp⟩
  exact ⟨r, by unfold with_le64; exact hl, hq⟩

theorem with_field_ok (out : alloc.vec.Vec Std.U8) (part : Slice Std.U8)
    (h : out.val.length + (Clew.Trust.field (bs part.val)).length ≤ Usize.max) :
    ∃ v, with_field out part = ok v ∧ bs v.val = bs out.val ++ Clew.Trust.field (bs part.val) := by
  simp only [Clew.Trust.field, List.length_append, Clew.Trust.leBytes_length, bs, List.length_map] at h
  set c := UScalar.cast UScalarTy.U64 (Slice.len part) with hc
  have hmax : Usize.max < 2 ^ 64 := by
    rcases Usize.bounds_eq with h | h <;> rw [h] <;> simp [U32.max_eq, U64.max_eq]
  have hlen : (Slice.len part).val < 2 ^ 64 := by
    have := Slice.length_ineq part
    have e : (Slice.len part).val = part.val.length := by simp
    omega
  have hcv : c.val = part.val.length := by rw [hc, UScalar.cast_val_eq]; simp; omega
  obtain ⟨o1, h1, e1⟩ := with_le64_ok out c (by omega)
  have hl1 : o1.val.length = out.val.length + 8 := by
    have := congrArg List.length e1
    simp only [bs, List.length_append, List.length_map, Clew.Trust.leBytes_length] at this; omega
  obtain ⟨v, hv, ev⟩ := extend_ok o1 part (by omega)
  refine ⟨v, by simp [with_field, lift, ← hc, h1, hv], ?_⟩
  rw [ev]
  simp only [bs, List.map_append] at e1 ⊢
  rw [e1, hcv]
  simp [Clew.Trust.field]

/-- **`length_prefixed` is the model**: for any parts whose encoding fits in
memory, it returns, and writes exactly `Clew.Trust.fields` of their bytes —
the input every fingerprint hashes, in which no rearrangement of the parts
reads the same (`fields_injective`). -/
theorem length_prefixed_is_the_model (parts : Slice (alloc.vec.Vec Std.U8))
    (room : (Clew.Trust.fields (parts.val.map fun v => bs v.val)).length ≤ Usize.max) :
    ∃ v, length_prefixed parts = ok v ∧
      bs v.val = Clew.Trust.fields (parts.val.map fun v => bs v.val) := by
  set f : alloc.vec.Vec Std.U8 → List Nat := fun v => Clew.Trust.field (bs v.val) with hf
  have hfl : ∀ l : List (alloc.vec.Vec Std.U8), Clew.Trust.fields (l.map fun v => bs v.val) = l.flatMap f := by
    intro l; simp [Clew.Trust.fields, hf, List.flatMap_map]
  rw [hfl] at room ⊢
  obtain ⟨r, hl, hq⟩ := loop_spec (fun (o, i) => length_prefixed_loop.body parts o i)
    (fun p => p.2.val ≤ parts.val.length ∧ bs p.1.val = (parts.val.take p.2.val).flatMap f)
    (fun _ r => bs r.val = parts.val.flatMap f)
    (fun p => parts.val.length - p.2.val) (by
      rintro ⟨o, i⟩ ⟨hle, ho⟩
      dsimp only at hle ho
      by_cases hi : i.val < parts.val.length
      · obtain ⟨j, hj, hjv⟩ := succ_ok parts i hi
        right
        have hpre := flatMap_take_length_le f parts.val (i.val + 1)
        rw [flatMap_take_succ f parts.val i.val hi, List.length_append] at hpre
        have hol : o.val.length = ((parts.val.take i.val).flatMap f).length := by
          have := congrArg List.length ho; simp only [bs, List.length_map] at this; omega
        have hfi : (f parts.val[i.val]).length = (Clew.Trust.field (bs parts.val[i.val].val)).length := rfl
        obtain ⟨v, hv, ev⟩ := with_field_ok o (alloc.vec.Vec.deref parts.val[i.val]) (by
          rw [deref_val]; omega)
        refine ⟨(v, j), ?_, ⟨show j.val ≤ _ by omega, ?_⟩,
          show parts.val.length - j.val < parts.val.length - i.val by omega, fun _ h => h⟩
        · simp [length_prefixed_loop.body, hi, index_ok parts i hi, hv, hj]
        · show bs v.val = _
          rw [ev, ho, hjv, flatMap_take_succ f parts.val i.val hi, deref_val]
      · left
        refine ⟨o, by simp [length_prefixed_loop.body, hi], ?_⟩
        simpa [List.take_of_length_le (show parts.val.length ≤ i.val by omega)] using ho)
    (alloc.vec.Vec.new Std.U8, 0#usize) ⟨by simp, by simp [bs]⟩
  exact ⟨r, by unfold length_prefixed; exact hl, hq⟩

end Fingerprint

/-! ## Which machine and project an approval is filed under — `clew_rules::approval`

A remote project's approvals are filed under `ssh://<label>:<root>`, the
label `user@host:port` with each part escaped. The model in
`../Clew/Trust.lean` proves two remotes and two roots share a key only when
they are the same (`approvalKey_injective`). Proved here: on the UTF-8 bytes
of any user, host, port and root, the Rust functions return, and write the
UTF-8 bytes of the model's label and key. -/

namespace Approval

open clew_rules.approval
open Shell (bs push_ok extend_ok)

/-- `with_escaped_byte` on bytes: `%`, `/`, `[`, `]`, `@` as `%XX`. -/
def escB (b : Nat) : List Nat :=
  if b = 37 then [37, 50, 53] else if b = 47 then [37, 50, 70]
  else if b = 91 then [37, 53, 66] else if b = 93 then [37, 53, 68]
  else if b = 64 then [37, 52, 48] else [b]

theorem escB_length (b : Nat) : (escB b).length ≤ 3 := by
  unfold escB; split_ifs <;> simp

theorem push3_ok (v : alloc.vec.Vec Std.U8) (x y z : Std.U8) (h : v.val.length + 3 ≤ Usize.max) :
    ∃ w, (do
      let a ← alloc.vec.Vec.push v x
      let b ← alloc.vec.Vec.push a y
      alloc.vec.Vec.push b z) = ok w ∧ bs w.val = bs v.val ++ [x.val, y.val, z.val] := by
  obtain ⟨a, ha, ea⟩ := push_ok v x (by omega)
  obtain ⟨b, hb, eb⟩ := push_ok a y (by rw [ea]; simp; omega)
  obtain ⟨c, hc, ec⟩ := push_ok b z (by rw [eb, ea]; simp; omega)
  exact ⟨c, by simp [ha, hb, hc], by rw [ec, eb, ea]; simp [bs]⟩

theorem with_escaped_byte_ok (out : alloc.vec.Vec Std.U8) (b : Std.U8)
    (h : out.val.length + 3 ≤ Usize.max) :
    ∃ v, with_escaped_byte out b = ok v ∧ bs v.val = bs out.val ++ escB b.val := by
  unfold with_escaped_byte
  by_cases h37 : b.val = 37
  · have e : b = 37#u8 := by rw [UScalar.eq_equiv]; simp [h37]
    obtain ⟨w, hw, ew⟩ := push3_ok out 37#u8 50#u8 53#u8 h
    exact ⟨w, by simp only [e, ↓reduceIte]; exact hw, by rw [ew]; simp [escB, h37]⟩
  by_cases h47 : b.val = 47
  · have e : b = 47#u8 := by rw [UScalar.eq_equiv]; simp [h47]
    obtain ⟨w, hw, ew⟩ := push3_ok out 37#u8 50#u8 70#u8 h
    exact ⟨w, by simp only [e]; simp; exact hw, by rw [ew]; simp [escB, h47]⟩
  by_cases h91 : b.val = 91
  · have e : b = 91#u8 := by rw [UScalar.eq_equiv]; simp [h91]
    obtain ⟨w, hw, ew⟩ := push3_ok out 37#u8 53#u8 66#u8 h
    exact ⟨w, by simp only [e]; simp; exact hw, by rw [ew]; simp [escB, h91]⟩
  by_cases h93 : b.val = 93
  · have e : b = 93#u8 := by rw [UScalar.eq_equiv]; simp [h93]
    obtain ⟨w, hw, ew⟩ := push3_ok out 37#u8 53#u8 68#u8 h
    exact ⟨w, by simp only [e]; simp; exact hw, by rw [ew]; simp [escB, h93]⟩
  by_cases h64 : b.val = 64
  · have e : b = 64#u8 := by rw [UScalar.eq_equiv]; simp [h64]
    obtain ⟨w, hw, ew⟩ := push3_ok out 37#u8 52#u8 48#u8 h
    exact ⟨w, by simp only [e]; simp; exact hw, by rw [ew]; simp [escB, h64]⟩
  obtain ⟨w, hw, ew⟩ := push_ok out b (by omega)
  refine ⟨w, ?_, by rw [ew]; simp [bs, escB, h37, h47, h91, h93, h64]⟩
  simp [U8.ofNat, UScalar.ofNat, UScalar.ofNatCore_val_eq, h37, h47, h91, h93, h64, hw]

theorem with_escaped_ok (out : alloc.vec.Vec Std.U8) (text : Slice Std.U8)
    (room : out.val.length + 3 * text.val.length ≤ Usize.max) :
    ∃ v, with_escaped out text = ok v ∧ bs v.val = bs out.val ++ (bs text.val).flatMap escB := by
  obtain ⟨r, hl, hq⟩ := loop_spec (fun (o, i) => with_escaped_loop.body text o i)
    (fun p => p.2.val ≤ text.val.length ∧
      bs p.1.val = bs out.val ++ (bs (text.val.take p.2.val)).flatMap escB)
    (fun _ r => bs r.val = bs out.val ++ (bs text.val).flatMap escB)
    (fun p => text.val.length - p.2.val) (by
      rintro ⟨o, i⟩ ⟨hle, ho⟩
      dsimp only at hle ho
      have hlen : o.val.length ≤ out.val.length + 3 * i.val := by
        have h1 := congrArg List.length ho
        have h2 : ((bs (text.val.take i.val)).flatMap escB).length ≤ 3 * (bs (text.val.take i.val)).length := by
          generalize bs (text.val.take i.val) = ns
          induction ns with
          | nil => simp
          | cons n ns ih =>
            rw [List.flatMap_cons, List.length_append, List.length_cons]
            have := escB_length n; omega
        simp only [bs, List.length_append, List.length_map, List.length_take] at h1 h2
        omega
      by_cases hi : i.val < text.val.length
      · obtain ⟨j, hj, hjv⟩ := succ_ok text i hi
        right
        obtain ⟨v, hv, ev⟩ := with_escaped_byte_ok o text.val[i.val] (by omega)
        refine ⟨(v, j), ?_, ⟨show j.val ≤ _ by omega, ?_⟩,
          show text.val.length - j.val < text.val.length - i.val by omega, fun _ h => h⟩
        · simp [with_escaped_loop.body, hi, index_ok text i hi, hv, hj]
        · show bs v.val = _
          rw [ev, ho, hjv, Shell.bs_take_succ _ _ hi, List.flatMap_append]
          simp
      · left
        refine ⟨o, by simp [with_escaped_loop.body, hi], ?_⟩
        simpa [List.take_of_length_le (show text.val.length ≤ i.val by omega)] using ho)
    (out, 0#usize) ⟨by simp, show bs out.val = _ by simp [bs]⟩
  exact ⟨r, by unfold with_escaped; exact hl, hq⟩

theorem has_colon_ok (text : Slice Std.U8) :
    has_colon text = ok ((bs text.val).any (· == 58)) := by
  obtain ⟨b, hl, hq⟩ := loop_spec (has_colon_loop.body text) (fun _ => True)
    (fun i b => b = (text.val.drop i.val).any fun b => b.val == 58)
    (fun i => text.val.length - i.val) (by
      intro i _
      by_cases hi : i.val < text.val.length
      · obtain ⟨j, hj, hjv⟩ := succ_ok text i hi
        rw [List.drop_eq_getElem_cons hi]
        by_cases hb : text.val[i.val].val = 58
        · left
          refine ⟨true, ?_, by rw [List.any_cons]; simp [hb]⟩
          simp [has_colon_loop.body, hi, index_ok text i hi, UScalar.eq_equiv, U8.ofNat,
            UScalar.ofNat, UScalar.ofNatCore_val_eq, hb]
        · right
          refine ⟨j, by simp [has_colon_loop.body, hi, index_ok text i hi, hj, UScalar.eq_equiv,
            U8.ofNat, UScalar.ofNat, UScalar.ofNatCore_val_eq, hb], trivial, by omega, ?_⟩
          intro b hb'
          rw [hb', hjv, List.any_cons]; simp [hb]
      · left
        refine ⟨false, by simp [has_colon_loop.body, hi], ?_⟩
        rw [List.drop_eq_nil_of_le (by omega)]; rfl) 0#usize trivial
  unfold has_colon has_colon_loop
  rw [hl, hq]; simp [bs, List.any_map, Function.comp_def]

theorem flatMap_escB_length (ns : List Nat) : (ns.flatMap escB).length ≤ 3 * ns.length := by
  induction ns with
  | nil => simp
  | cons n ns ih =>
    rw [List.flatMap_cons, List.length_append, List.length_cons]
    have := escB_length n; omega

theorem bs_length (l : List Std.U8) : (bs l).length = l.length := by simp [bs]

/-- What `with_user` writes: `user@`, escaped, or nothing. -/
def userB : Option (List Nat) → List Nat
  | none => []
  | some u => u.flatMap escB ++ [64]

/-- What `with_host` writes: the host escaped, bracketed when it holds a `:`. -/
def hostB (h : List Nat) : List Nat :=
  if h.any (· == 58) then 91 :: (h.flatMap escB ++ [93]) else h.flatMap escB

/-- What `with_port` writes: `:port`, escaped, or nothing. -/
def portB : Option (List Nat) → List Nat
  | none => []
  | some t => 58 :: t.flatMap escB

/-- The byte length of an optional part. -/
def olen (o : Option (Slice Std.U8)) : Nat := (o.map (·.val.length)).getD 0

theorem with_user_ok (label : alloc.vec.Vec Std.U8) (user : Option (Slice Std.U8))
    (h : label.val.length + 3 * olen user + 1 ≤ Usize.max) :
    ∃ v, with_user label user = ok v ∧ bs v.val = bs label.val ++ userB (user.map (bs ·.val)) := by
  cases user with
  | none => exact ⟨label, rfl, by simp [userB]⟩
  | some u =>
    simp only [olen, Option.map_some, Option.getD_some] at h
    obtain ⟨l1, h1, e1⟩ := with_escaped_ok label u (by omega)
    have hl1 : l1.val.length ≤ label.val.length + 3 * u.val.length := by
      have := congrArg List.length e1
      have := flatMap_escB_length (bs u.val)
      simp only [bs_length, List.length_append] at *; omega
    obtain ⟨v, hv, ev⟩ := push_ok l1 64#u8 (by omega)
    refine ⟨v, by simp [with_user, h1, hv], ?_⟩
    rw [ev]; simp only [bs, List.map_append] at e1 ⊢; rw [e1]; simp [userB]

theorem with_host_ok (label : alloc.vec.Vec Std.U8) (host : Slice Std.U8)
    (h : label.val.length + 3 * host.val.length + 2 ≤ Usize.max) :
    ∃ v, with_host label host = ok v ∧ bs v.val = bs label.val ++ hostB (bs host.val) := by
  have hesc := flatMap_escB_length (bs host.val)
  rw [bs_length] at hesc
  cases hc : (bs host.val).any (· == 58)
  · obtain ⟨v, hv, ev⟩ := with_escaped_ok label host (by omega)
    exact ⟨v, by simp [with_host, has_colon_ok, hc, hv], by rw [ev]; simp [hostB, hc]⟩
  · obtain ⟨l1, h1, e1⟩ := push_ok label 91#u8 (by omega)
    obtain ⟨l2, h2, e2⟩ := with_escaped_ok l1 host (by rw [e1]; simp; omega)
    have hl2 : l2.val.length ≤ label.val.length + 1 + 3 * host.val.length := by
      have := congrArg List.length e2
      simp only [bs_length, List.length_append, e1, List.length_singleton] at this; omega
    obtain ⟨v, hv, ev⟩ := push_ok l2 93#u8 (by omega)
    refine ⟨v, by simp [with_host, has_colon_ok, hc, h1, h2, hv], ?_⟩
    have hb : bs v.val = bs l2.val ++ [93] := by rw [ev]; simp [bs]
    have hb1 : bs l1.val = bs label.val ++ [91] := by rw [e1]; simp [bs]
    rw [hb, e2, hb1]
    simp only [hostB, hc, ↓reduceIte]
    simp

theorem with_port_ok (label : alloc.vec.Vec Std.U8) (port : Option (Slice Std.U8))
    (h : label.val.length + 3 * olen port + 1 ≤ Usize.max) :
    ∃ v, with_port label port = ok v ∧ bs v.val = bs label.val ++ portB (port.map (bs ·.val)) := by
  cases port with
  | none => exact ⟨label, rfl, by simp [portB]⟩
  | some t =>
    simp only [olen, Option.map_some, Option.getD_some] at h
    obtain ⟨l1, h1, e1⟩ := push_ok label 58#u8 (by omega)
    obtain ⟨v, hv, ev⟩ := with_escaped_ok l1 t (by rw [e1]; simp; omega)
    refine ⟨v, by simp [with_port, h1, hv], ?_⟩
    rw [ev, e1]; simp [portB, bs]

theorem scope_label_ok (user : Option (Slice Std.U8)) (host : Slice Std.U8) (port : Option (Slice Std.U8))
    (room : 3 * (olen user + host.val.length + olen port) + 4 ≤ Usize.max) :
    ∃ v, scope_label user host port = ok v ∧ bs v.val =
      userB (user.map (bs ·.val)) ++ (hostB (bs host.val) ++ portB (port.map (bs ·.val))) := by
  obtain ⟨l1, h1, e1⟩ := with_user_ok (alloc.vec.Vec.new Std.U8) user (by simp; omega)
  have h0 : (alloc.vec.Vec.new Std.U8).val.length = 0 := by simp
  have hl1 : l1.val.length ≤ 3 * olen user + 1 := by
    have hlen := congrArg List.length e1
    rw [bs_length, List.length_append, bs_length, h0, Nat.zero_add] at hlen
    cases user with
    | none =>
      have : olen none = 0 := rfl
      simp only [userB, Option.map_none, List.length_nil] at hlen; omega
    | some u =>
      have hesc := flatMap_escB_length (bs u.val)
      rw [bs_length] at hesc
      have : olen (some u) = u.val.length := rfl
      simp only [userB, Option.map_some, List.length_append, List.length_singleton] at hlen
      omega
  obtain ⟨l2, h2, e2⟩ := with_host_ok l1 host (by omega)
  have hl2 : l2.val.length ≤ 3 * olen user + 3 * host.val.length + 3 := by
    have hlen := congrArg List.length e2
    have hesc := flatMap_escB_length (bs host.val)
    rw [bs_length, List.length_append, bs_length] at hlen
    rw [bs_length] at hesc
    unfold hostB at hlen
    split at hlen
    · simp only [List.length_cons, List.length_append, List.length_nil] at hlen; omega
    · omega
  obtain ⟨v, hv, ev⟩ := with_port_ok l2 port (by omega)
  refine ⟨v, by simp [scope_label, h1, h2, hv], ?_⟩
  rw [ev, e2, e1]; simp [bs]

/-- What `approval_key` writes after `ssh://<label>:`: the root, after `./`
unless it starts with `/`. -/
def rootB (r : List Nat) : List Nat := if r.head? = some 47 then r else 46 :: 47 :: r

theorem approval_key_ok (label root : Slice Std.U8)
    (room : label.val.length + root.val.length + 9 ≤ Usize.max) :
    ∃ v, approval_key label root = ok v ∧
      bs v.val = [115, 115, 104, 58, 47, 47] ++ bs label.val ++ 58 :: rootB (bs root.val) := by
  obtain ⟨k1, h1, e1⟩ := push_ok (alloc.vec.Vec.new Std.U8) 115#u8 (by simp; omega)
  obtain ⟨k2, h2, e2⟩ := push_ok k1 115#u8 (by simp [e1]; omega)
  obtain ⟨k3, h3, e3⟩ := push_ok k2 104#u8 (by simp [e2, e1]; omega)
  obtain ⟨k4, h4, e4⟩ := push_ok k3 58#u8 (by simp [e3, e2, e1]; omega)
  obtain ⟨k5, h5, e5⟩ := push_ok k4 47#u8 (by simp [e4, e3, e2, e1]; omega)
  obtain ⟨k6, h6, e6⟩ := push_ok k5 47#u8 (by simp [e5, e4, e3, e2, e1]; omega)
  obtain ⟨k7, h7, e7⟩ := extend_ok k6 label (by simp [e6, e5, e4, e3, e2, e1]; omega)
  obtain ⟨k8, h8, e8⟩ := push_ok k7 58#u8 (by simp [e7, e6, e5, e4, e3, e2, e1]; omega)
  have hk8 : k8.val.length = label.val.length + 7 := by simp [e8, e7, e6, e5, e4, e3, e2, e1]
  have hpre : bs k8.val = [115, 115, 104, 58, 47, 47] ++ bs label.val ++ [58] := by
    simp [e8, e7, e6, e5, e4, e3, e2, e1, bs]
  have bs_append : ∀ a b : List Std.U8, bs (a ++ b) = bs a ++ bs b := by intro a b; simp [bs]
  by_cases he : root.val = []
  · obtain ⟨d1, hd1, ed1⟩ := push_ok k8 46#u8 (by omega)
    obtain ⟨d2, hd2, ed2⟩ := push_ok d1 47#u8 (by simp [ed1]; omega)
    obtain ⟨v, hv, ev⟩ := extend_ok d2 root (by simp [ed2, ed1]; omega)
    refine ⟨v, by simp [approval_key, h1, h2, h3, h4, h5, h6, h7, h8, core.slice.Slice.is_empty, he,
      hd1, hd2, hv], ?_⟩
    rw [ev, ed2, ed1, bs_append, bs_append, bs_append, hpre, he]
    simp [rootB, bs]
  · have hpos : 0 < root.val.length := List.length_pos_iff.mpr he
    have hidx := index_ok root 0#usize (by simp; omega)
    have hb : bs root.val = root.val[0].val :: bs (root.val.drop 1) := by
      rw [← KnownHosts.bs_drop _ _ hpos]; rfl
    by_cases h47 : root.val[0].val = 47
    · have h47' : root.val[0] = 47#u8 := by rw [UScalar.eq_equiv]; simp [h47]
      obtain ⟨v, hv, ev⟩ := extend_ok k8 root (by omega)
      refine ⟨v, by simp [approval_key, h1, h2, h3, h4, h5, h6, h7, h8, core.slice.Slice.is_empty, he,
        hidx, h47', hv], ?_⟩
      rw [ev, bs_append, hpre]
      have hr : rootB (bs root.val) = bs root.val := by simp [rootB, hb, h47]
      rw [hr]; simp
    · have h47' : ¬ root.val[0] = 47#u8 := by rw [UScalar.eq_equiv]; simp [h47]
      obtain ⟨d1, hd1, ed1⟩ := push_ok k8 46#u8 (by omega)
      obtain ⟨d2, hd2, ed2⟩ := push_ok d1 47#u8 (by simp [ed1]; omega)
      obtain ⟨v, hv, ev⟩ := extend_ok d2 root (by simp [ed2, ed1]; omega)
      refine ⟨v, by simp [approval_key, h1, h2, h3, h4, h5, h6, h7, h8, core.slice.Slice.is_empty, he,
        hidx, h47, hd1, hd2, hv], ?_⟩
      rw [ev, ed2, ed1, bs_append, bs_append, bs_append, hpre]
      have hr : rootB (bs root.val) = 46 :: 47 :: bs root.val := by simp [rootB, hb, h47]
      rw [hr]; simp [bs]

/-! ### From bytes to characters -/

open Headers (utf8 utf8_ascii utf8_high)
open Shell (utf8_cons utf8_append)

theorem flatMap_escB_id (ns : List Nat) (h : ∀ n ∈ ns, escB n = [n]) : ns.flatMap escB = ns := by
  induction ns with
  | nil => rfl
  | cons n ns ih =>
    rw [List.flatMap_cons, h n (by simp), ih (fun m hm => h m (by simp [hm]))]; rfl

theorem utf8_escChar (c : Char) :
    utf8 (Clew.Trust.escChar c) = ((String.utf8EncodeChar c).map (·.toNat)).flatMap escB := by
  by_cases h1 : c = '%'
  · subst h1; decide
  by_cases h2 : c = '/'
  · subst h2; decide
  by_cases h3 : c = '['
  · subst h3; decide
  by_cases h4 : c = ']'
  · subst h4; decide
  by_cases h5 : c = '@'
  · subst h5; decide
  have he : Clew.Trust.escChar c = [c] := by simp [Clew.Trust.escChar, h1, h2, h3, h4, h5]
  rw [he, flatMap_escB_id]
  · simp [utf8]
  · intro b hb
    have n1 := KnownHosts.bytes_ne c 37 (by omega) (KnownHosts.toNat_ne c '%' h1) b hb
    have n2 := KnownHosts.bytes_ne c 47 (by omega) (KnownHosts.toNat_ne c '/' h2) b hb
    have n3 := KnownHosts.bytes_ne c 91 (by omega) (KnownHosts.toNat_ne c '[' h3) b hb
    have n4 := KnownHosts.bytes_ne c 93 (by omega) (KnownHosts.toNat_ne c ']' h4) b hb
    have n5 := KnownHosts.bytes_ne c 64 (by omega) (KnownHosts.toNat_ne c '@' h5) b hb
    simp [escB, n1, n2, n3, n4, n5]

theorem utf8_esc (s : List Char) : utf8 (Clew.Trust.esc s) = (utf8 s).flatMap escB := by
  induction s with
  | nil => rfl
  | cons c s ih =>
    rw [Clew.Trust.esc, List.flatMap_cons, utf8_append, ← Clew.Trust.esc, ih, utf8_escChar, utf8_cons,
      List.flatMap_append]

theorem utf8_any_colon (s : List Char) : (utf8 s).any (· == 58) = s.contains ':' := by
  rw [List.contains_eq_any_beq]
  induction s with
  | nil => rfl
  | cons c s ih =>
    rw [utf8_cons, List.any_append, ih, List.any_cons, Shell.char_beq]
    congr 1
    by_cases h : c.toNat < 128
    · rw [utf8_ascii c h]; simp [eq_comm]
    · obtain ⟨_, hhi⟩ := utf8_high c (by omega)
      have h1 : ((':' : Char).toNat == c.toNat) = false := by
        rw [beq_eq_false_iff_ne]; intro he; rw [← he] at h; exact h (by decide)
      rw [h1, List.any_eq_false]
      intro n hn
      simp only [List.mem_map] at hn
      obtain ⟨b, hb, rfl⟩ := hn
      have := hhi b hb
      simp; omega

theorem utf8_hostPart (h : List Char) : utf8 (Clew.Trust.hostPart h) = hostB (utf8 h) := by
  unfold Clew.Trust.hostPart hostB
  rw [utf8_any_colon]
  split
  · rw [utf8_cons, utf8_append, utf8_esc]; rfl
  · rw [utf8_esc]

theorem utf8_scopeLabel (u : Option (List Char)) (h : List Char) (p : Option (List Char)) :
    utf8 (Clew.Trust.scopeLabel u h p) = userB (u.map utf8) ++ (hostB (utf8 h) ++ portB (p.map utf8)) := by
  unfold Clew.Trust.scopeLabel
  rw [utf8_append, utf8_append, utf8_hostPart]
  congr 1
  · cases u with
    | none => rfl
    | some u => simp only [userB, Option.map_some]; rw [utf8_append, utf8_esc]; rfl
  · congr 1
    cases p with
    | none => rfl
    | some t => simp only [Clew.Trust.portPart, portB, Option.map_some]; rw [utf8_cons, utf8_esc]; rfl

theorem utf8_rootPart (root : List Char) : utf8 (Clew.Trust.rootPart root) = rootB (utf8 root) := by
  unfold Clew.Trust.rootPart rootB
  cases root with
  | nil => rfl
  | cons c r =>
    by_cases hc : c = '/'
    · subst hc
      have e : utf8 ('/' :: r) = 47 :: utf8 r := by rw [utf8_cons]; rfl
      simp [e]
    · obtain ⟨b, tl, hbt, hb⟩ := KnownHosts.utf8_cons_head c r 47 (by omega) (KnownHosts.toNat_ne c '/' hc)
      have e : utf8 ('.' :: '/' :: c :: r) = 46 :: 47 :: utf8 (c :: r) := by
        rw [utf8_cons, utf8_cons]; rfl
      simp [hc, hbt, hb, e]

theorem utf8_approvalKey (label root : List Char) :
    utf8 (Clew.Trust.approvalKey label root) =
      [115, 115, 104, 58, 47, 47] ++ utf8 label ++ 58 :: rootB (utf8 root) := by
  unfold Clew.Trust.approvalKey
  rw [utf8_append, utf8_append, utf8_cons, utf8_rootPart]
  have e : utf8 "ssh://".toList = [115, 115, 104, 58, 47, 47] := by decide
  rw [e]; rfl

/-- **`scope_label` is the model**: on the UTF-8 bytes of any user, host and
port whose label fits in memory, it returns, and writes the UTF-8 bytes of
`Clew.Trust.scopeLabel`. -/
theorem scope_label_is_the_model (u : Option (List Char)) (h : List Char) (p : Option (List Char))
    (user : Option (Slice Std.U8)) (host : Slice Std.U8) (port : Option (Slice Std.U8))
    (hu : user.map (fun s => bs s.val) = u.map utf8) (hh : bs host.val = utf8 h)
    (hp : port.map (fun s => bs s.val) = p.map utf8)
    (room : 3 * (olen user + host.val.length + olen port) + 4 ≤ Usize.max) :
    ∃ v, scope_label user host port = ok v ∧ bs v.val = utf8 (Clew.Trust.scopeLabel u h p) := by
  obtain ⟨v, hv, ev⟩ := scope_label_ok user host port room
  refine ⟨v, hv, ?_⟩
  rw [ev, utf8_scopeLabel]
  have hu' : user.map (bs ·.val) = u.map utf8 := hu
  have hp' : port.map (bs ·.val) = p.map utf8 := hp
  rw [hu', hh, hp']

/-- **`approval_key` is the model**: on the UTF-8 bytes of any label and
root, it returns, and writes the UTF-8 bytes of `Clew.Trust.approvalKey` —
the key `approvalKey_injective` proves names one machine and one project. -/
theorem approval_key_is_the_model (label root : List Char) (l r : Slice Std.U8)
    (hl : bs l.val = utf8 label) (hr : bs r.val = utf8 root)
    (room : l.val.length + r.val.length + 9 ≤ Usize.max) :
    ∃ v, approval_key l r = ok v ∧ bs v.val = utf8 (Clew.Trust.approvalKey label root) := by
  obtain ⟨v, hv, ev⟩ := approval_key_ok l r room
  exact ⟨v, hv, by rw [ev, utf8_approvalKey, hl, hr]⟩

end Approval

/-! ## Where a string's words begin and end — `clew_rules::text`

`str::trim` on the UTF-8 bytes of a string, read a character at a time: a
character's first byte says how many bytes it takes, and its first three bytes
whether it is white space (`char::is_whitespace`: Unicode's `White_Space`).
Proved here: on the UTF-8 bytes of any string, `trim_bounds` returns, and the
bytes between the two places it finds are the UTF-8 bytes of `Clew.Text.trim`
of the string — the `trim` every model in `../Clew` reads with. On the way:
the byte test for white space agrees with `Clew.Text.isWhitespace` on every
character (`spaceB_enc`), and UTF-8 is a prefix code (`utf8_inj`). -/

namespace Text

open clew_rules.text
open Shell (bs)
open Headers (utf8)

/-- A character's UTF-8 bytes, as arithmetic on its number. -/
def encN (n : Nat) : List Nat :=
  if n ≤ 0x7f then [n]
  else if n ≤ 0x7ff then [n / 64 % 32 + 0xc0, n % 64 + 0x80]
  else if n ≤ 0xffff then [n / 4096 % 16 + 0xe0, n / 64 % 64 + 0x80, n % 64 + 0x80]
  else [n / 262144 % 8 + 0xf0, n / 4096 % 64 + 0x80, n / 64 % 64 + 0x80, n % 64 + 0x80]

theorem char_lt (c : Char) : c.toNat < 0x110000 := by
  have := c.valid
  unfold Char.toNat
  simp only [UInt32.isValidChar, Nat.isValidChar] at this
  omega

theorem enc_eq (c : Char) : (String.utf8EncodeChar c).map (·.toNat) = encN c.toNat := by
  have hc := char_lt c
  rw [show c.toNat = c.val.toNat from rfl] at hc ⊢
  unfold String.utf8EncodeChar encN
  generalize c.val.toNat = n at hc ⊢
  split_ifs <;> simp [*, UInt8.toNat_add, UInt8.toNat_ofNat] <;> omega

/-- `is_space_bytes`: a character starting with bytes `a`, `b`, `c` (256 for
none) is white space. -/
def spaceN (a b c : Nat) : Bool :=
  if a = 32 ∨ (9 ≤ a ∧ a ≤ 13) then true
  else if a = 0xC2 then decide (b = 0x85 ∨ b = 0xA0)
  else if a = 0xE1 then decide (b = 0x9A ∧ c = 0x80)
  else if a = 0xE2 then
    (if b = 0x80 then decide ((0x80 ≤ c ∧ c ≤ 0x8A) ∨ c = 0xA8 ∨ c = 0xA9 ∨ c = 0xAF)
     else decide (b = 0x81 ∧ c = 0x9F))
  else if a = 0xE3 then decide (b = 0x80 ∧ c = 0x80)
  else false

/-- `is_space_at` on the bytes from a position on. -/
def spaceB (l : List Nat) : Bool := spaceN (l.getD 0 256) (l.getD 1 256) (l.getD 2 256)

/-- `char_len` on a first byte. -/
def lenB (b : Nat) : Nat := if b < 0x80 then 1 else if b < 0xE0 then 2 else if b < 0xF0 then 3 else 4

theorem spaceB_enc (c : Char) (rest : List Nat) :
    spaceB ((String.utf8EncodeChar c).map (·.toNat) ++ rest) = Clew.Text.isWhitespace c := by
  rw [enc_eq]
  have hc := char_lt c
  unfold Clew.Text.isWhitespace spaceB
  generalize c.toNat = n at hc ⊢
  unfold encN
  split_ifs <;> simp only [List.cons_append, List.nil_append, List.getD_cons_succ, List.getD_cons_zero] <;>
    generalize rest.getD 0 256 = y <;> generalize rest.getD 1 256 = z <;> unfold spaceN <;> split_ifs <;>
    (rw [Bool.eq_iff_iff]
     simp only [Bool.or_eq_true, Bool.and_eq_true, decide_eq_true_eq, beq_iff_eq,
       Bool.false_eq_true, true_iff, false_iff]
     omega)

theorem lenB_enc (c : Char) :
    ∃ b r, (String.utf8EncodeChar c).map (·.toNat) = b :: r ∧ lenB b = r.length + 1 := by
  rw [enc_eq]
  have hc := char_lt c
  generalize c.toNat = n at hc ⊢
  unfold encN
  split_ifs <;> refine ⟨_, _, rfl, ?_⟩ <;> unfold lenB <;> split_ifs <;> simp <;> omega

theorem is_space_e2_ok (b c : Std.U16) :
    is_space_e2 b c = ok (if b.val = 0x80 then decide ((0x80 ≤ c.val ∧ c.val ≤ 0x8A) ∨ c.val = 0xA8 ∨
      c.val = 0xA9 ∨ c.val = 0xAF) else decide (b.val = 0x81 ∧ c.val = 0x9F)) := by
  unfold is_space_e2
  simp only [ge_iff_le, UScalar.le_equiv, UScalar.eq_equiv, U16.ofNat, UScalar.ofNat,
    UScalar.ofNatCore_val_eq]
  generalize b.val = y
  generalize c.val = z
  split_ifs <;> simp_all

theorem is_space_bytes_ok (a b c : Std.U16) :
    is_space_bytes a b c = ok (spaceN a.val b.val c.val) := by
  unfold is_space_bytes spaceN
  simp only [ge_iff_le, UScalar.le_equiv, UScalar.eq_equiv, U16.ofNat, UScalar.ofNat,
    UScalar.ofNatCore_val_eq, is_space_e2_ok]
  generalize a.val = x
  generalize b.val = y
  generalize c.val = z
  split_ifs <;> simp_all <;> omega

theorem byte_after_ok (s : Slice Std.U8) (i k : Std.Usize) :
    ∃ v, byte_after s i k = ok v ∧ v.val = (bs (s.val.drop i.val)).getD k.val 256 := by
  unfold byte_after
  by_cases hi : i.val < s.val.length
  · obtain ⟨d, hd, hdv⟩ := ok_of_spec (UScalar.sub_spec (x := Slice.len s) (y := i) (by simp; omega))
    simp only [Slice.len_val] at hdv
    by_cases hk : k.val < s.val.length - i.val
    · have := Slice.length_ineq s
      obtain ⟨j, hj, hjv⟩ := ok_of_spec (UScalar.add_spec (x := i) (y := k) (by scalar_tac))
      have hjl : j.val < s.val.length := by omega
      refine ⟨UScalar.cast UScalarTy.U16 (s.val[j.val]'hjl), ?_, ?_⟩
      · simp [UScalar.lt_equiv, hi, hd, hdv.1, hk, hj, index_ok s j hjl]
      · have e : (bs (s.val.drop i.val)).getD k.val 256 = (s.val[j.val]'hjl).val := by
          simp only [bs, List.getD_eq_getElem?_getD, List.getElem?_map, List.getElem?_drop]
          rw [show i.val + k.val = j.val by omega, List.getElem?_eq_getElem hjl]; rfl
        rw [e, UScalar.cast_val_eq]
        have := Framing.u8_lt (s.val[j.val]'hjl)
        simp; omega
    · refine ⟨256#u16, by simp [UScalar.lt_equiv, hi, hd, hdv.1, hk], ?_⟩
      simp only [bs, List.getD_eq_getElem?_getD, List.getElem?_map, List.getElem?_drop]
      rw [List.getElem?_eq_none (by omega)]; rfl
  · refine ⟨256#u16, by simp [UScalar.lt_equiv, hi], ?_⟩
    rw [List.drop_eq_nil_of_le (by omega)]; rfl

theorem is_space_at_ok (s : Slice Std.U8) (i : Std.Usize) :
    is_space_at s i = ok (spaceB (bs (s.val.drop i.val))) := by
  obtain ⟨a, ha, hav⟩ := byte_after_ok s i 0#usize
  obtain ⟨b, hb, hbv⟩ := byte_after_ok s i 1#usize
  obtain ⟨c, hc, hcv⟩ := byte_after_ok s i 2#usize
  simp only [is_space_at, ha, hb, hc, bind_ok, is_space_bytes_ok, spaceB, hav, hbv, hcv]
  simp

/-- A character's UTF-8 bytes, as numbers. -/
abbrev enc (c : Char) : List Nat := (String.utf8EncodeChar c).map (·.toNat)

theorem char_len_ok (b : Std.U8) : ∃ l, char_len b = ok l ∧ l.val = lenB b.val := by
  unfold char_len lenB
  simp only [UScalar.lt_equiv, U8.ofNat, UScalar.ofNat, UScalar.ofNatCore_val_eq]
  split_ifs <;> simp

theorem bs_drop_add (l : List Std.U8) (i k : Nat) :
    bs (l.drop (i + k)) = (bs (l.drop i)).drop k := by
  simp only [bs, List.map_drop, List.drop_drop]

theorem next_char_ok (s : Slice Std.U8) (i : Std.Usize) (c : Char) (rest : List Nat)
    (h : bs (s.val.drop i.val) = enc c ++ rest) :
    ∃ j, next_char s i = ok j ∧ j.val = i.val + (enc c).length ∧ bs (s.val.drop j.val) = rest := by
  obtain ⟨b, r, he, hl⟩ := lenB_enc c
  have hlen : s.val.length - i.val = (enc c).length + rest.length := by
    have := congrArg List.length h
    simpa [bs] using this
  have hel : (enc c).length = r.length + 1 := by rw [show enc c = b :: r from he]; rfl
  have hi : i.val < s.val.length := by omega
  have hb : (s.val[i.val]'hi).val = b := by
    have h' := h
    rw [KnownHosts.bs_drop _ _ hi, show enc c = b :: r from he] at h'
    simp only [List.cons_append, List.cons.injEq] at h'
    exact h'.1
  obtain ⟨l, hl1, hlv⟩ := char_len_ok (s.val[i.val]'hi)
  rw [hb, hl] at hlv
  obtain ⟨d, hd, hdv⟩ := ok_of_spec (UScalar.sub_spec (x := Slice.len s) (y := i) (by simp; omega))
  simp only [Slice.len_val] at hdv
  have hd' : d.val = s.val.length - i.val := by have := hdv.1; simp only [Slice.length] at this; omega
  have := Slice.length_ineq s
  obtain ⟨j, hj, hjv⟩ := ok_of_spec (UScalar.add_spec (x := i) (y := l) (by scalar_tac))
  refine ⟨j, ?_, by omega, ?_⟩
  · have hnot : ¬ (l > d) := by rw [gt_iff_lt, UScalar.lt_equiv]; omega
    simp only [next_char, index_ok s i hi, hl1, hd, bind_ok, hnot, ↓reduceIte, hj]
  · rw [show j.val = i.val + (enc c).length by omega, bs_drop_add, h, List.drop_left]

/-- Two characters whose bytes start two byte lists alike are the same, and
so are the rests. -/
theorem enc_append_inj (c d : Char) (xs ys : List Nat) (h : enc c ++ xs = enc d ++ ys) :
    c = d ∧ xs = ys := by
  obtain ⟨b, r, he, hl⟩ := lenB_enc c
  obtain ⟨b', r', he', hl'⟩ := lenB_enc d
  have hlen : (enc c).length = (enc d).length := by
    have h' := h
    rw [show enc c = b :: r from he, show enc d = b' :: r' from he'] at h' ⊢
    simp only [List.cons_append, List.cons.injEq] at h'
    rw [h'.1] at hl
    simp only [List.length_cons]; omega
  obtain ⟨h1, h2⟩ := List.append_inj h hlen
  exact ⟨String.utf8EncodeChar_inj
    ((List.map_inj_right (fun x y hxy => UInt8.toNat_inj.mp hxy)).mp h1), h2⟩

/-- UTF-8 is a prefix code: two lists of characters with the same bytes are
the same characters. -/
theorem utf8_inj (cs ds : List Char) (h : utf8 cs = utf8 ds) : cs = ds := by
  induction cs generalizing ds with
  | nil => exact ((Headers.utf8_nil ds).mp h.symm).symm
  | cons c cs ih =>
    cases ds with
    | nil => exact absurd ((Headers.utf8_nil _).mp h) (by simp)
    | cons d ds =>
      rw [Shell.utf8_cons, Shell.utf8_cons] at h
      obtain ⟨rfl, h2⟩ := enc_append_inj c d _ _ h
      rw [ih ds h2]

theorem trimEnd_snoc (xs : List Char) (c : Char) :
    Clew.Text.trimEnd (xs ++ [c]) =
      if Clew.Text.isWhitespace c then Clew.Text.trimEnd xs else xs ++ [c] := by
  unfold Clew.Text.trimEnd
  rw [List.reverse_append, List.reverse_singleton, List.singleton_append, List.dropWhile_cons]
  split <;> simp_all

theorem bs_drop_length (l : List Std.U8) (i : Nat) : (bs (l.drop i)).length = l.length - i := by
  simp [bs]

theorem enc_pos (c : Char) : 0 < (enc c).length := by
  simp [enc, Char.utf8Size_pos]

theorem utf8_singleton (c : Char) : utf8 [c] = enc c := by
  simp [utf8]

theorem bs_drop_end (l : List Std.U8) (i : Nat) (h : l.length ≤ i) : bs (l.drop i) = [] := by
  rw [List.drop_eq_nil_of_le h]; rfl

theorem skip_space_ok (s : Slice Std.U8) (f : Std.Usize) (ds : List Char)
    (hf : f.val ≤ s.val.length) (h : bs (s.val.drop f.val) = utf8 ds) :
    ∃ r, skip_space s f = ok r ∧ r.val ≤ s.val.length ∧
      bs (s.val.drop r.val) = utf8 (ds.dropWhile Clew.Text.isWhitespace) := by
  have key := loop_spec (fun i => skip_space_loop.body s i)
    (fun i => i.val ≤ s.val.length ∧ ∃ ds, bs (s.val.drop i.val) = utf8 ds)
    (fun i r => ∀ ds, bs (s.val.drop i.val) = utf8 ds →
      r.val ≤ s.val.length ∧ bs (s.val.drop r.val) = utf8 (ds.dropWhile Clew.Text.isWhitespace))
    (fun i => s.val.length - i.val)
    (by
      rintro i ⟨hi, ds, hds⟩
      by_cases hlt : i.val < s.val.length
      · cases ds with
        | nil =>
          exfalso
          have := bs_drop_length s.val i.val
          rw [hds, (Headers.utf8_nil []).mpr rfl] at this
          simp at this; omega
        | cons c ds' =>
          rw [Shell.utf8_cons] at hds
          have hsp : is_space_at s i = ok (Clew.Text.isWhitespace c) := by
            rw [is_space_at_ok, hds, spaceB_enc]
          have hl : (enc c).length + (utf8 ds').length = s.val.length - i.val := by
            have := bs_drop_length s.val i.val
            rw [hds, List.length_append] at this; exact this
          have hpos := enc_pos c
          obtain ⟨j, hj, hjv, hjd⟩ := next_char_ok s i c _ hds
          by_cases hw : Clew.Text.isWhitespace c = true
          · right
            refine ⟨j, ?_, ⟨by omega, ds', hjd⟩, by omega, ?_⟩
            · simp [skip_space_loop.body, UScalar.lt_equiv, hlt, hsp, hw, hj]
            · intro r hq ds0 hds0
              have e : ds0 = c :: ds' := utf8_inj _ _ (by rw [← hds0, hds, Shell.utf8_cons])
              subst e
              rw [List.dropWhile_cons_of_pos hw]
              exact hq ds' hjd
          · left
            refine ⟨i, by simp [skip_space_loop.body, UScalar.lt_equiv, hlt, hsp, hw], ?_⟩
            intro ds0 hds0
            have e : ds0 = c :: ds' := utf8_inj _ _ (by rw [← hds0, hds, Shell.utf8_cons])
            subst e
            rw [List.dropWhile_cons_of_neg hw]
            exact ⟨hi, hds0⟩
      · left
        refine ⟨i, by simp [skip_space_loop.body, UScalar.lt_equiv, hlt], ?_⟩
        intro ds0 hds0
        rw [bs_drop_end _ _ (by omega)] at hds0
        have e : ds0 = [] := (Headers.utf8_nil ds0).mp hds0.symm
        subst e
        exact ⟨hi, by rw [bs_drop_end _ _ (by omega)]; rfl⟩)
  obtain ⟨r, hr, hq⟩ := key f ⟨hf, ds, h⟩
  exact ⟨r, hr, hq ds h⟩

theorem end_of_text_ok (s : Slice Std.U8) (F : Std.Usize) (ds : List Char)
    (hf : F.val ≤ s.val.length) (h : bs (s.val.drop F.val) = utf8 ds) :
    ∃ r, end_of_text s F = ok r ∧ F.val ≤ r.val ∧ r.val ≤ s.val.length ∧
      (bs (s.val.drop F.val)).take (r.val - F.val) = utf8 (Clew.Text.trimEnd ds) := by
  have key := loop_spec (fun (p : Std.Usize × Std.Usize) => end_of_text_loop.body s p.1 p.2)
    (fun p => F.val ≤ p.2.val ∧ p.2.val ≤ p.1.val ∧ p.1.val ≤ s.val.length ∧
      ∃ pre post, (bs (s.val.drop F.val)).take (p.1.val - F.val) = utf8 pre ∧
        bs (s.val.drop p.1.val) = utf8 post ∧
        (bs (s.val.drop F.val)).take (p.2.val - F.val) = utf8 (Clew.Text.trimEnd pre))
    (fun p r => ∀ pre post, (bs (s.val.drop F.val)).take (p.1.val - F.val) = utf8 pre →
        bs (s.val.drop p.1.val) = utf8 post →
        (bs (s.val.drop F.val)).take (p.2.val - F.val) = utf8 (Clew.Text.trimEnd pre) →
        F.val ≤ r.val ∧ r.val ≤ s.val.length ∧
        (bs (s.val.drop F.val)).take (r.val - F.val) = utf8 (Clew.Text.trimEnd (pre ++ post)))
    (fun p => s.val.length - p.1.val)
    (by
      rintro ⟨i, e⟩ ⟨hFe, hei, hi, pre, post, hpre, hpost, htr⟩
      dsimp only at hFe hei hi hpre hpost htr ⊢
      by_cases hlt : i.val < s.val.length
      · cases post with
        | nil =>
          exfalso
          have := bs_drop_length s.val i.val
          rw [hpost, (Headers.utf8_nil []).mpr rfl] at this
          simp at this; omega
        | cons c post' =>
          rw [Shell.utf8_cons] at hpost
          have hsp : is_space_at s i = ok (Clew.Text.isWhitespace c) := by
            rw [is_space_at_ok, hpost, spaceB_enc]
          have hl : (enc c).length + (utf8 post').length = s.val.length - i.val := by
            have := bs_drop_length s.val i.val
            rw [hpost, List.length_append] at this; exact this
          have hpos := enc_pos c
          obtain ⟨j, hj, hjv, hjd⟩ := next_char_ok s i c _ hpost
          have htake : (bs (s.val.drop F.val)).take (j.val - F.val) = utf8 (pre ++ [c]) := by
            rw [show j.val - F.val = (i.val - F.val) + (enc c).length by omega, List.take_add, hpre,
              ← bs_drop_add, show F.val + (i.val - F.val) = i.val by omega, hpost, List.take_left,
              Shell.utf8_append, utf8_singleton]
          have same : ∀ pre0 post0,
              (bs (s.val.drop F.val)).take (i.val - F.val) = utf8 pre0 →
              bs (s.val.drop i.val) = utf8 post0 → pre0 = pre ∧ post0 = c :: post' := by
            intro pre0 post0 h1 h2
            exact ⟨utf8_inj _ _ (by rw [← h1, hpre]),
              utf8_inj _ _ (by rw [← h2, hpost, Shell.utf8_cons])⟩
          right
          by_cases hw : Clew.Text.isWhitespace c = true
          · refine ⟨(j, e), ?_, ⟨hFe, by dsimp only; omega, by dsimp only; omega, pre ++ [c], post', htake, hjd, ?_⟩,
              by dsimp only; omega, ?_⟩
            · simp [end_of_text_loop.body, UScalar.lt_equiv, hlt, hsp, hw, hj]
            · rw [trimEnd_snoc]; simp only [hw, ↓reduceIte]; exact htr
            · intro r hq pre0 post0 h1 h2 _
              obtain ⟨e1, e2⟩ := same pre0 post0 h1 h2
              rw [e1, e2]
              have := hq (pre ++ [c]) post' htake hjd
                (by rw [trimEnd_snoc]; simp only [hw, ↓reduceIte]; exact htr)
              simpa using this
          · refine ⟨(j, j), ?_, ⟨by dsimp only; omega, by dsimp only; omega, by dsimp only; omega, pre ++ [c], post',
              htake, hjd, ?_⟩, by dsimp only; omega, ?_⟩
            · simp [end_of_text_loop.body, UScalar.lt_equiv, hlt, hsp, hw, hj]
            · rw [trimEnd_snoc]; simp only [hw, Bool.false_eq_true, ↓reduceIte]; exact htake
            · intro r hq pre0 post0 h1 h2 _
              obtain ⟨e1, e2⟩ := same pre0 post0 h1 h2
              rw [e1, e2]
              have := hq (pre ++ [c]) post' htake hjd
                (by rw [trimEnd_snoc]; simp only [hw, Bool.false_eq_true, ↓reduceIte]; exact htake)
              simpa using this
      · left
        refine ⟨e, by simp [end_of_text_loop.body, UScalar.lt_equiv, hlt], ?_⟩
        intro pre0 post0 _ h2 h3
        rw [bs_drop_end _ _ (by omega)] at h2
        have e0 : post0 = [] := (Headers.utf8_nil post0).mp h2.symm
        subst e0
        exact ⟨hFe, by omega, by simpa using h3⟩)
  obtain ⟨r, hr, hq⟩ := key (F, F) ⟨le_refl _, le_refl _, hf, [], ds,
    by simp; rfl, h, by simp [Clew.Text.trimEnd]; rfl⟩
  obtain ⟨h1, h2, h3⟩ := hq [] ds (by simp; rfl) h (by simp [Clew.Text.trimEnd]; rfl)
  exact ⟨r, hr, h1, h2, by simpa using h3⟩

/-- **`trim_bounds` is the model**: on the UTF-8 bytes of any string, it
returns, and the bytes between the two places it finds are the UTF-8 bytes of
`Clew.Text.trim` of the string. -/
theorem trim_bounds_is_the_model (s : Slice Std.U8) (cs : List Char) (h : bs s.val = utf8 cs) :
    ∃ st en, trim_bounds s = ok (st, en) ∧ st.val ≤ en.val ∧ en.val ≤ s.val.length ∧
      ((bs s.val).drop st.val).take (en.val - st.val) = utf8 (Clew.Text.trim cs) := by
  obtain ⟨st, hst, hstl, hstd⟩ := skip_space_ok s 0#usize cs (by simp) (by simpa using h)
  obtain ⟨en, hen, h1, h2, h3⟩ := end_of_text_ok s st _ hstl hstd
  refine ⟨st, en, by simp [trim_bounds, hst, hen], h1, h2, ?_⟩
  rw [show (bs s.val).drop st.val = bs (s.val.drop st.val) by simp [bs, List.map_drop], h3]
  rfl

end Text

/-! ## Whether a path's spelling can lead out of the project — `clew_rules::confine`

`check_lexical` refuses a project-relative path that is empty (after
`str::trim`), starts with `/`, or has a `..` piece between its `/`s;
`names_a_folder` says whether its last piece is empty or `.`. The model in
`../Clew/Confine.lean` proves a path the check accepts, resolved one piece at
a time against any root, never stands outside it (`accepted_never_leaves`).
Proved here: on the UTF-8 bytes of any path, both functions return, and say
what the model says. `/` is the one character whose UTF-8 bytes hold a 47, so
the pieces of the bytes between 47s are the bytes of the pieces between `/`s
(`utf8_split`). -/

namespace Confine

open clew_rules.confine
open Shell (bs)
open Headers (utf8)
open Clew.Extracted.Text (enc)

/-- `str::split(sep)` on bytes. -/
def splitN (sep : Nat) : List Nat → List (List Nat)
  | [] => [[]]
  | b :: bs => if b = sep then [] :: splitN sep bs
    else match splitN sep bs with
      | [] => [[b]]
      | p :: ps => (b :: p) :: ps

theorem splitN_ne_nil (sep : Nat) (l : List Nat) : splitN sep l ≠ [] := by
  cases l with
  | nil => simp [splitN]
  | cons b bs => unfold splitN; split <;> [simp; (split <;> simp)]

theorem splitN_append_sep (sep : Nat) (xs ys : List Nat) (h : ∀ b ∈ xs, b ≠ sep) :
    splitN sep (xs ++ sep :: ys) = xs :: splitN sep ys := by
  induction xs with
  | nil => simp [splitN]
  | cons x xs ih =>
    have hx : x ≠ sep := h x (by simp)
    rw [List.cons_append, splitN, ih (fun b hb => h b (by simp [hb]))]
    simp [hx]

theorem splitN_none (sep : Nat) (xs : List Nat) (h : ∀ b ∈ xs, b ≠ sep) : splitN sep xs = [xs] := by
  induction xs with
  | nil => rfl
  | cons x xs ih =>
    have hx : x ≠ sep := h x (by simp)
    rw [splitN, ih (fun b hb => h b (by simp [hb]))]
    simp [hx]

theorem splitN_prepend (sep : Nat) (xs ys : List Nat) (h : ∀ b ∈ xs, b ≠ sep)
    (p : List Nat) (ps : List (List Nat)) (hs : splitN sep ys = p :: ps) :
    splitN sep (xs ++ ys) = (xs ++ p) :: ps := by
  induction xs with
  | nil => simpa using hs
  | cons x xs ih =>
    have hx : x ≠ sep := h x (by simp)
    rw [List.cons_append, splitN, ih (fun b hb => h b (by simp [hb]))]
    simp [hx]

theorem slash_eq (c : Char) : (c == '/') = (c.toNat == 47) := Shell.char_beq c '/'

/-- `'/'` is one byte, 47, and no other character's bytes hold a 47: the
pieces of a string's bytes between 47s are the bytes of its pieces between
`/`s. -/
theorem utf8_split (cs : List Char) :
    splitN 47 (utf8 cs) = (Clew.Text.split (· == '/') cs).map utf8 := by
  induction cs with
  | nil => rfl
  | cons c cs ih =>
    rw [Shell.utf8_cons]
    by_cases hc : c.toNat = 47
    · have e : c = '/' := Char.toNat_inj.mp hc
      subst e
      show splitN 47 (47 :: utf8 cs) = _
      rw [splitN, ih]
      simp [Clew.Text.split]; rfl
    · have hno := KnownHosts.bytes_ne c 47 (by omega) hc
      have hsep : (c == '/') = false := by rw [slash_eq]; simpa using hc
      cases hs : Clew.Text.split (· == '/') cs with
      | nil => exact absurd (by rw [ih, hs]; rfl) (splitN_ne_nil 47 (utf8 cs))
      | cons p ps =>
        rw [hs] at ih
        rw [splitN_prepend 47 _ _ hno (utf8 p) (ps.map utf8) ih]
        simp only [Clew.Text.split, hsep, Bool.false_eq_true, ↓reduceIte, hs, List.map_cons,
          Shell.utf8_cons]

theorem utf8_dd (seg : List Char) : (utf8 seg == [46, 46]) = (seg == ['.', '.']) := by
  rw [Bool.eq_iff_iff, beq_iff_eq, beq_iff_eq]
  have e : utf8 ['.', '.'] = [46, 46] := rfl
  rw [← e]
  exact ⟨Text.utf8_inj _ _, fun h => h ▸ rfl⟩

/-- The bytes of `rel` from `x` on. -/
abbrev D (rel : Slice Std.U8) (x : Nat) : List Nat := bs (rel.val.drop x)

/-- The bytes of `rel[x..y]`. -/
abbrev seg (rel : Slice Std.U8) (x y : Nat) : List Nat := (D rel x).take (y - x)

theorem D_split (rel : Slice Std.U8) (x y : Nat) (h : x ≤ y) : D rel x = seg rel x y ++ D rel y := by
  rw [show D rel y = (D rel x).drop (y - x) by
    rw [show y = x + (y - x) by omega, D, D, Text.bs_drop_add]; simp]
  exact (List.take_append_drop _ _).symm

theorem seg_snoc (rel : Slice Std.U8) (x i : Nat) (hx : x ≤ i) (hi : i < rel.val.length) :
    seg rel x (i + 1) = seg rel x i ++ [(rel.val[i]'hi).val] := by
  simp only [seg]
  rw [show i + 1 - x = (i - x) + 1 by omega, List.take_add, ← Text.bs_drop_add,
    show x + (i - x) = i by omega, KnownHosts.bs_drop _ _ hi]
  simp

theorem is_parent_ok (rel : Slice Std.U8) (x y : Std.Usize) (h1 : x.val ≤ y.val)
    (h2 : y.val ≤ rel.val.length) :
    is_parent rel x y = ok (seg rel x.val y.val == [46, 46]) := by
  obtain ⟨d, hd, hdv⟩ := ok_of_spec (UScalar.sub_spec (x := y) (y := x) h1)
  have hd' : d.val = y.val - x.val := hdv.1
  have hlen : (seg rel x.val y.val).length = y.val - x.val := by
    simp only [seg, D, List.length_take, Text.bs_drop_length]; omega
  unfold is_parent
  rw [hd, bind_ok]
  by_cases h2' : d.val = 2
  · have hx : x.val < rel.val.length := by omega
    have hx1 : x.val + 1 < rel.val.length := by omega
    have := Slice.length_ineq rel
    obtain ⟨x1, hx1e, hx1v⟩ := ok_of_spec (UScalar.add_spec (x := x) (y := 1#usize) (by scalar_tac))
    have hx1v' : x1.val = x.val + 1 := by simpa using hx1v
    have hseg : seg rel x.val y.val = [(rel.val[x.val]'hx).val, (rel.val[x.val + 1]'hx1).val] := by
      simp only [seg, D]
      rw [show y.val - x.val = 2 by omega, KnownHosts.bs_drop _ _ hx, KnownHosts.bs_drop _ _ hx1]
      simp
    have hne : ¬ (d != 2#usize) = true := by simp [bne_iff_ne, h2']
    simp only [hne, Bool.false_eq_true, ↓reduceIte, index_ok rel x hx, hx1e, bind_ok]
    rw [index_ok rel x1 (by omega), hseg]
    simp only [hx1v']
    by_cases ha : (rel.val[x.val]'hx).val = 46 <;> by_cases hb : (rel.val[x.val + 1]'hx1).val = 46 <;>
      simp [UScalar.eq_equiv, ha, hb]
  · have hne : (d != 2#usize) = true := by simp [bne_iff_ne, h2']
    simp only [hne, ↓reduceIte]
    congr 1
    symm
    rw [beq_eq_false_iff_ne]
    intro he; rw [he] at hlen; simp at hlen; omega

theorem D_end (rel : Slice Std.U8) (x : Nat) (h : rel.val.length ≤ x) : D rel x = [] :=
  Text.bs_drop_end _ _ h

theorem len_val (rel : Slice Std.U8) : (Slice.len rel).val = rel.val.length := by simp

/-- The bytes from `x` on, split at the 47 at `i` that ends the piece from `x`. -/
theorem splitN_at (rel : Slice Std.U8) (x i : Nat) (hx : x ≤ i) (hi : i < rel.val.length)
    (hno : ∀ b ∈ seg rel x i, b ≠ 47) (h47 : (rel.val[i]'hi).val = 47) :
    splitN 47 (D rel x) = seg rel x i :: splitN 47 (D rel (i + 1)) := by
  rw [D_split rel x i hx, D, KnownHosts.bs_drop _ _ hi, h47]
  exact splitN_append_sep 47 _ _ hno

theorem splitN_last (rel : Slice Std.U8) (x i : Nat) (hx : x ≤ i) (hi : rel.val.length ≤ i)
    (hno : ∀ b ∈ seg rel x i, b ≠ 47) : splitN 47 (D rel x) = [seg rel x i] := by
  rw [D_split rel x i hx, D_end rel i hi, List.append_nil]
  exact splitN_none 47 _ hno

theorem has_parent_ok (rel : Slice Std.U8) :
    has_parent rel = ok ((splitN 47 (D rel 0)).any (· == [46, 46])) := by
  have key := loop_spec (fun (p : Std.Usize × Std.Usize) => has_parent_loop.body rel p.1 p.2)
    (fun p => p.1.val ≤ p.2.val ∧ p.2.val ≤ rel.val.length ∧ ∀ b ∈ seg rel p.1.val p.2.val, b ≠ 47)
    (fun p r => r = (splitN 47 (D rel p.1.val)).any (· == [46, 46]))
    (fun p => rel.val.length - p.2.val)
    (by
      rintro ⟨st, i⟩ ⟨h1, h2, hno⟩
      dsimp only at h1 h2 hno ⊢
      by_cases hlt : i.val < rel.val.length
      · obtain ⟨j, hj, hjv⟩ := succ_ok rel i hlt
        by_cases h47 : (rel.val[i.val]'hlt).val = 47
        · have hsp := splitN_at rel st.val i.val h1 hlt hno h47
          have hb : (rel.val[i.val]'hlt) = 47#u8 := by
            rw [UScalar.eq_equiv]; simpa using h47
          by_cases hpar : (seg rel st.val i.val == [46, 46]) = true
          · left
            refine ⟨true, ?_, ?_⟩
            · simp [has_parent_loop.body, UScalar.lt_equiv, hlt, index_ok rel i hlt, hb,
                is_parent_ok rel st i h1 (by omega), hpar]
            · rw [hsp]; simp [hpar]
          · right
            refine ⟨(j, j), ?_, ⟨le_refl _, by dsimp only; omega, by simp [seg]⟩,
              by dsimp only; omega, ?_⟩
            · simp only [Bool.not_eq_true] at hpar
              simp [has_parent_loop.body, UScalar.lt_equiv, hlt, index_ok rel i hlt, hb,
                is_parent_ok rel st i h1 (by omega), hpar, hj]
            · intro r hq
              dsimp only at hq
              rw [hsp, List.any_cons, hq, show j.val = i.val + 1 from hjv]
              simp only [Bool.not_eq_true] at hpar
              simp [hpar]
        · have hb : ¬ (rel.val[i.val]'hlt) = 47#u8 := by
            rw [UScalar.eq_equiv]; simpa using h47
          right
          refine ⟨(st, j), ?_, ⟨by dsimp only; omega, by dsimp only; omega, ?_⟩,
            by dsimp only; omega, fun r hq => hq⟩
          · simp [has_parent_loop.body, UScalar.lt_equiv, hlt, index_ok rel i hlt, hb, hj]
          · dsimp only
            rw [hjv, seg_snoc rel st.val i.val h1 hlt]
            intro b hb'
            rcases List.mem_append.mp hb' with hb' | hb'
            · exact hno b hb'
            · simp at hb'; omega
      · left
        refine ⟨seg rel st.val rel.val.length == [46, 46], ?_, ?_⟩
        · have hL := len_val rel
          have := is_parent_ok rel st (Slice.len rel) (by omega) (by omega)
          rw [hL] at this
          simp [has_parent_loop.body, UScalar.lt_equiv, hlt, this]
        · rw [splitN_last rel st.val i.val h1 (by omega) hno,
            show i.val = rel.val.length by omega]
          simp)
  obtain ⟨r, hr, hq⟩ := key (0#usize, 0#usize) ⟨le_refl _, by simp, by simp [seg]⟩
  rw [has_parent, has_parent_loop, hr, hq]
  rfl

theorem last_piece_ok (rel : Slice Std.U8) :
    ∃ r, last_piece rel = ok r ∧ r.val ≤ rel.val.length ∧
      (splitN 47 (D rel 0)).getLast? = some (D rel r.val) := by
  have key := loop_spec (fun (p : Std.Usize × Std.Usize) => last_piece_loop.body rel p.1 p.2)
    (fun p => p.1.val ≤ p.2.val ∧ p.2.val ≤ rel.val.length ∧ ∀ b ∈ seg rel p.1.val p.2.val, b ≠ 47)
    (fun p r => r.val ≤ rel.val.length ∧
      (splitN 47 (D rel p.1.val)).getLast? = some (D rel r.val))
    (fun p => rel.val.length - p.2.val)
    (by
      rintro ⟨st, i⟩ ⟨h1, h2, hno⟩
      dsimp only at h1 h2 hno ⊢
      by_cases hlt : i.val < rel.val.length
      · obtain ⟨j, hj, hjv⟩ := succ_ok rel i hlt
        right
        by_cases h47 : (rel.val[i.val]'hlt).val = 47
        · have hsp := splitN_at rel st.val i.val h1 hlt hno h47
          have hb : (rel.val[i.val]'hlt) = 47#u8 := by
            rw [UScalar.eq_equiv]; simpa using h47
          refine ⟨(j, j), ?_, ⟨le_refl _, by dsimp only; omega, by simp [seg]⟩,
            by dsimp only; omega, ?_⟩
          · simp [last_piece_loop.body, UScalar.lt_equiv, hlt, index_ok rel i hlt, hb, hj]
          · rintro r ⟨hr1, hr2⟩
            dsimp only at hr2
            refine ⟨hr1, ?_⟩
            rw [hsp, show i.val + 1 = j.val by omega]
            cases hs : splitN 47 (D rel j.val) with
            | nil => exact absurd hs (splitN_ne_nil _ _)
            | cons p ps => rw [hs] at hr2; rw [List.getLast?_cons_cons, hr2]
        · have hb : ¬ (rel.val[i.val]'hlt) = 47#u8 := by
            rw [UScalar.eq_equiv]; simpa using h47
          refine ⟨(st, j), ?_, ⟨by dsimp only; omega, by dsimp only; omega, ?_⟩,
            by dsimp only; omega, fun r hq => hq⟩
          · simp [last_piece_loop.body, UScalar.lt_equiv, hlt, index_ok rel i hlt, hb, hj]
          · dsimp only
            rw [hjv, seg_snoc rel st.val i.val h1 hlt]
            intro b hb'
            rcases List.mem_append.mp hb' with hb' | hb'
            · exact hno b hb'
            · simp at hb'; omega
      · left
        refine ⟨st, by simp [last_piece_loop.body, UScalar.lt_equiv, hlt], by omega, ?_⟩
        rw [splitN_last rel st.val i.val h1 (by omega) hno, D_split rel st.val i.val h1,
          D_end rel i.val (by omega)]
        simp)
  obtain ⟨r, hr, hq⟩ := key (0#usize, 0#usize) ⟨le_refl _, by simp, by simp [seg]⟩
  exact ⟨r, by rw [last_piece, last_piece_loop, hr], hq⟩

/-- `names_a_folder` on bytes: the last piece between 47s is empty or `.`. -/
def folderB (l : List (List Nat)) : Bool :=
  match l.getLast? with
  | some s => decide (s = [] ∨ s = [46])
  | none => false

theorem names_a_folder_ok (rel : Slice Std.U8) :
    names_a_folder rel = ok (folderB (splitN 47 (D rel 0))) := by
  obtain ⟨r, hr, hrl, hlast⟩ := last_piece_ok rel
  have hlen : (D rel r.val).length = rel.val.length - r.val := Text.bs_drop_length _ _
  obtain ⟨d, hd, hdv⟩ := ok_of_spec (UScalar.sub_spec (x := Slice.len rel) (y := r) (by simp; omega))
  have hd' : d.val = rel.val.length - r.val := by have := hdv.1; simp only [len_val] at this; omega
  unfold names_a_folder folderB
  rw [hlast, hr, bind_ok]
  dsimp only
  rw [hd, bind_ok]
  by_cases h0 : d.val = 0
  · have he : D rel r.val = [] := List.eq_nil_of_length_eq_zero (by omega)
    have : d = 0#usize := by rw [UScalar.eq_equiv]; simpa using h0
    simp [this, he]
  · have h0' : ¬ d = 0#usize := by rw [UScalar.eq_equiv]; simpa using h0
    have hne : D rel r.val ≠ [] := fun h => by rw [h] at hlen; simp at hlen; omega
    by_cases h1 : d.val = 1
    · have h1' : d = 1#usize := by rw [UScalar.eq_equiv]; simpa using h1
      have hr' : r.val < rel.val.length := by omega
      have hone : D rel r.val = [(rel.val[r.val]'hr').val] := by
        rw [D, KnownHosts.bs_drop _ _ hr', Text.bs_drop_end _ (r.val + 1) (by omega)]
      simp only [h1', ↓reduceIte, index_ok rel r hr', hone]
      by_cases ha : (rel.val[r.val]'hr').val = 46 <;> simp [UScalar.eq_equiv, ha]
    · have h1' : ¬ d = 1#usize := by rw [UScalar.eq_equiv]; simpa using h1
      have hn1 : D rel r.val ≠ [46] := fun h => by rw [h] at hlen; simp at hlen; omega
      simp [h0', h1', hne, hn1]

/-- `check_lexical`'s answer, read as the model's. -/
def lexOf : Except Clew.Confine.Error Unit → Lexical
  | .ok () => .Inside
  | .error .empty => .Empty
  | .error .absolute => .Absolute
  | .error .traversal => .Traversal

theorem D_zero (rel : Slice Std.U8) : D rel 0 = bs rel.val := by simp [D]

/-- A character's first byte is 47 just when it is `/`. -/
theorem first_byte_slash (c : Char) (b : Nat) (r : List Nat) (h : enc c = b :: r) :
    b = 47 ↔ c = '/' := by
  constructor
  · intro hb
    by_contra hc
    have hn : c.toNat ≠ 47 := fun e => hc (Char.toNat_inj.mp e)
    exact KnownHosts.bytes_ne c 47 (by omega) hn b (by rw [show (String.utf8EncodeChar c).map _ = b :: r from h]; simp) hb
  · rintro rfl
    have e : enc '/' = [47] := rfl
    rw [e] at h
    simp at h; omega

theorem any_segments (cs : List Char) :
    (splitN 47 (utf8 cs)).any (· == [46, 46]) = (Clew.Confine.segments cs).any (· == ['.', '.']) := by
  rw [utf8_split, List.any_map]
  unfold Clew.Confine.segments
  congr 1
  funext seg
  exact utf8_dd seg

/-- **`check_lexical` is the model**: on the UTF-8 bytes of any path, it
returns, and says what `Clew.Confine.checkLexical` says — the check whose
every accepted path, resolved against any root, never stands outside it
(`accepted_never_leaves`). -/
theorem check_lexical_is_the_model (cs : List Char) (rel : Slice Std.U8) (h : bs rel.val = utf8 cs) :
    check_lexical rel = ok (lexOf (Clew.Confine.checkLexical cs)) := by
  obtain ⟨st, en, htb, h1, h2, h3⟩ := Text.trim_bounds_is_the_model rel cs h
  unfold check_lexical
  rw [htb, bind_ok]
  by_cases hse : st = en
  · have ht : Clew.Text.trim cs = [] := by
      apply (Headers.utf8_nil _).mp
      rw [← h3, hse]; simp
    simp [hse, Clew.Confine.checkLexical, ht, lexOf]
  · have hlt : st.val < en.val := by
      have : st.val ≠ en.val := fun e => hse (UScalar.eq_of_val_eq e)
      omega
    have htrim : (Clew.Text.trim cs).isEmpty = false := by
      cases ht : Clew.Text.trim cs with
      | nil =>
        rw [ht] at h3
        have := congrArg List.length h3
        simp only [List.length_take, List.length_drop, Shell.bs, List.length_map, Headers.utf8,
          List.length_nil, List.flatMap_nil, List.map_nil] at this
        omega
      | cons _ _ => rfl
    have hlen0 : 0 < rel.val.length := by omega
    cases cs with
    | nil =>
      have := congrArg List.length h
      simp only [Shell.bs, List.length_map, Headers.utf8, List.flatMap_nil, List.map_nil,
        List.length_nil] at this
      omega
    | cons c cs' =>
      obtain ⟨b, r, he, _⟩ := Text.lenB_enc c
      have hB : bs rel.val = b :: (r ++ utf8 cs') := by
        rw [h, Shell.utf8_cons, show (String.utf8EncodeChar c).map _ = b :: r from he]; rfl
      have h0 : (rel.val[0]'hlen0).val = b := by
        have := KnownHosts.bs_drop rel.val 0 hlen0
        rw [List.drop_zero, hB] at this
        simp only [List.cons.injEq] at this
        exact this.1.symm
      have hi0 : Slice.index_usize rel 0#usize = ok (rel.val[0]'hlen0) := by
        simp [Slice.index_usize, hlen0]
      have hm : Clew.Confine.checkLexical (c :: cs') =
          if c = '/' then .error .absolute
          else if (Clew.Confine.segments (c :: cs')).any (· == ['.', '.']) then .error .traversal
          else .ok () := by
        simp [Clew.Confine.checkLexical, htrim]
      have hne' : ¬ st.val = en.val := by omega
      simp only [hi0, bind_ok, hm]
      by_cases hc : c = '/'
      · have hb : rel.val[0]'hlen0 = 47#u8 := by
          rw [UScalar.eq_equiv, h0]; simpa using (first_byte_slash c b r he).mpr hc
        simp [hb, hc, lexOf, hne']
      · have hb : ¬ rel.val[0]'hlen0 = 47#u8 := by
          rw [UScalar.eq_equiv, h0]; simpa using mt (first_byte_slash c b r he).mp hc
        rw [has_parent_ok, D_zero, h, any_segments]
        by_cases hp : (Clew.Confine.segments (c :: cs')).any (· == ['.', '.']) = true
        · simp [hb, hc, hp, lexOf, hne']
        · simp only [Bool.not_eq_true] at hp
          simp [hb, hc, hp, lexOf, hne']

/-- **`names_a_folder` is the model**: on the UTF-8 bytes of any path, it
returns, and says what `Clew.Confine.namesAFolder` says. -/
theorem names_a_folder_is_the_model (cs : List Char) (rel : Slice Std.U8) (h : bs rel.val = utf8 cs) :
    names_a_folder rel = ok (Clew.Confine.namesAFolder cs) := by
  rw [names_a_folder_ok, D_zero, h, utf8_split]
  unfold folderB Clew.Confine.namesAFolder Clew.Confine.segments
  rw [List.getLast?_map]
  cases (Clew.Text.split (· == '/') cs).getLast? with
  | none => rfl
  | some seg =>
    have e0 : utf8 seg = [] ↔ seg = [] := Headers.utf8_nil seg
    have e1 : utf8 seg = [46] ↔ seg = ['.'] := by
      rw [show ([46] : List Nat) = utf8 ['.'] from rfl]
      exact ⟨Text.utf8_inj _ _, fun h => h ▸ rfl⟩
    simp only [Option.map_some, e0, e1]
    split <;> simp_all

end Confine

/-! ## Which updates may be installed — `clew_rules::version`

`parse` reads the version an update declares (trimmed, leading `v`s
dropped, a `-` or `+` suffix ignored, three `.`-separated numbers, each
trimmed), `is_newer` orders two versions field by field, and `upgrade` is
`ensure_upgrade`'s decision. The model in `../Clew/Update.lean` proves an
accepted update is strictly newer and that the order is strict and total, so
updates only climb. Proved here: on the UTF-8 bytes of any string, the three
return, and decide what the model decides (`upgrade_newer`: what `upgrade`
lets through is newer, of the code). On the way, a separator that is one
ASCII byte splits a string's bytes where it splits its characters
(`split_first`). -/

namespace Version

open clew_rules.version
open Shell (bs)
open Headers (utf8)
open Clew.Extracted.Text (enc)
open KnownHosts (isDigitB foldN foldN_ge stripB stripPlus utf8_stripPlus isAsciiDigit_eq isDigitB_high
  parseDigits parseU64_eq toNat_ne)

/-- The bytes `s[x..y]`. -/
abbrev sg (s : Slice Std.U8) (x y : Nat) : List Nat := ((bs s.val).drop x).take (y - x)

theorem sg_split (s : Slice Std.U8) (x y z : Nat) (hxy : x ≤ y) (hyz : y ≤ z) :
    sg s x z = sg s x y ++ sg s y z := by
  simp only [sg]
  rw [show z - x = (y - x) + (z - y) by omega, List.take_add, List.drop_drop]
  congr 3; omega

theorem sg_cons (s : Slice Std.U8) (y z : Nat) (hyz : y < z) (hz : z ≤ s.val.length) :
    sg s y z = (s.val[y]'(by omega)).val :: sg s (y + 1) z := by
  simp only [sg]
  have hy : y < (bs s.val).length := by simp [bs]; omega
  rw [List.drop_eq_getElem_cons hy, show z - y = (z - (y + 1)) + 1 by omega, List.take_succ_cons]
  simp [bs]

theorem sg_self (s : Slice Std.U8) (x : Nat) : sg s x x = [] := by simp [sg]

theorem sg_snoc (s : Slice Std.U8) (x i : Nat) (hx : x ≤ i) (hi : i < s.val.length) :
    sg s x (i + 1) = sg s x i ++ [(s.val[i]'hi).val] := by
  rw [sg_split s x i (i + 1) hx (by omega), sg_cons s i (i + 1) (by omega) (by omega), sg_self]

theorem sg_length (s : Slice Std.U8) (x y : Nat) (_hxy : x ≤ y) (hy : y ≤ s.val.length) :
    (sg s x y).length = y - x := by
  simp [sg, bs]; omega

theorem find_ok (s : Slice Std.U8) (f t : Std.Usize) (a b : Std.U8) (h1 : f.val ≤ t.val)
    (h2 : t.val ≤ s.val.length) :
    ∃ j, find s f t a b = ok j ∧ f.val ≤ j.val ∧ j.val ≤ t.val ∧
      (∀ x ∈ sg s f.val j.val, x ≠ a.val ∧ x ≠ b.val) ∧
      (j.val < t.val → ∃ k, sg s j.val t.val = k :: sg s (j.val + 1) t.val ∧ (k = a.val ∨ k = b.val)) := by
  obtain ⟨r, hr, hq⟩ := loop_spec (fun i => find_loop.body s t a b i)
    (fun i => f.val ≤ i.val ∧ i.val ≤ t.val ∧ ∀ x ∈ sg s f.val i.val, x ≠ a.val ∧ x ≠ b.val)
    (fun _ r => f.val ≤ r.val ∧ r.val ≤ t.val ∧ (∀ x ∈ sg s f.val r.val, x ≠ a.val ∧ x ≠ b.val) ∧
      (r.val < t.val → ∃ k, sg s r.val t.val = k :: sg s (r.val + 1) t.val ∧ (k = a.val ∨ k = b.val)))
    (fun i => t.val - i.val)
    (by
      rintro i ⟨hi1, hi2, hno⟩
      by_cases hlt : i.val < t.val
      · have hil : i.val < s.val.length := by omega
        have hc := sg_cons s i.val t.val hlt h2
        by_cases ha : s.val[i.val] = a
        · left
          refine ⟨i, by simp [find_loop.body, UScalar.lt_equiv, hlt, index_ok s i hil, ha],
            hi1, hi2, hno, fun _ => ⟨_, hc, Or.inl (by rw [ha])⟩⟩
        · by_cases hb : s.val[i.val] = b
          · left
            refine ⟨i, by simp [find_loop.body, UScalar.lt_equiv, hlt, index_ok s i hil, hb],
              hi1, hi2, hno, fun _ => ⟨_, hc, Or.inr (by rw [hb])⟩⟩
          · right
            obtain ⟨j, hj, hjv⟩ := succ_ok s i hil
            refine ⟨j, by simp [find_loop.body, UScalar.lt_equiv, hlt, index_ok s i hil, ha, hb, hj],
              ⟨by omega, by omega, ?_⟩, by omega, fun r hr => hr⟩
            rw [hjv, sg_snoc s f.val i.val hi1 hil]
            intro x hx
            rcases List.mem_append.mp hx with hx | hx
            · exact hno x hx
            · simp only [List.mem_singleton] at hx
              subst hx
              exact ⟨fun e => ha (UScalar.eq_of_val_eq e), fun e => hb (UScalar.eq_of_val_eq e)⟩
      · left
        have hit : i.val = t.val := by omega
        refine ⟨t, by simp [find_loop.body, UScalar.lt_equiv, hlt], h1, le_refl _,
          by rw [← hit]; exact hno, fun h => absurd h (by omega)⟩)
    f ⟨le_refl _, h1, by simp⟩
  exact ⟨r, by unfold find find_loop; exact hr, hq⟩

theorem skip_v_ok (s : Slice Std.U8) (f t : Std.Usize) (h1 : f.val ≤ t.val) (h2 : t.val ≤ s.val.length) :
    ∃ j, skip_v s f t = ok j ∧ f.val ≤ j.val ∧ j.val ≤ t.val ∧ (∀ x ∈ sg s f.val j.val, x = 118) ∧
      (j.val < t.val → ∃ k, sg s j.val t.val = k :: sg s (j.val + 1) t.val ∧ k ≠ 118) := by
  obtain ⟨r, hr, hq⟩ := loop_spec (fun i => skip_v_loop.body s t i)
    (fun i => f.val ≤ i.val ∧ i.val ≤ t.val ∧ ∀ x ∈ sg s f.val i.val, x = 118)
    (fun _ r => f.val ≤ r.val ∧ r.val ≤ t.val ∧ (∀ x ∈ sg s f.val r.val, x = 118) ∧
      (r.val < t.val → ∃ k, sg s r.val t.val = k :: sg s (r.val + 1) t.val ∧ k ≠ 118))
    (fun i => t.val - i.val)
    (by
      rintro i ⟨hi1, hi2, hno⟩
      by_cases hlt : i.val < t.val
      · have hil : i.val < s.val.length := by omega
        have hc := sg_cons s i.val t.val hlt h2
        by_cases hv : s.val[i.val] = 118#u8
        · right
          obtain ⟨j, hj, hjv⟩ := succ_ok s i hil
          refine ⟨j, by simp [skip_v_loop.body, UScalar.lt_equiv, hlt, index_ok s i hil, hv, hj],
            ⟨by omega, by omega, ?_⟩, by omega, fun r hr => hr⟩
          rw [hjv, sg_snoc s f.val i.val hi1 hil]
          intro x hx
          rcases List.mem_append.mp hx with hx | hx
          · exact hno x hx
          · simp only [List.mem_singleton] at hx; rw [hx, hv]; rfl
        · left
          refine ⟨i, by simp [skip_v_loop.body, UScalar.lt_equiv, hlt, index_ok s i hil, hv],
            hi1, hi2, hno, fun _ => ⟨_, hc, fun e => hv (UScalar.eq_of_val_eq (by rw [e]; rfl))⟩⟩
      · left
        refine ⟨i, by simp [skip_v_loop.body, UScalar.lt_equiv, hlt], hi1, hi2, hno,
          fun h => absurd h hlt⟩)
    f ⟨le_refl _, h1, by simp⟩
  exact ⟨r, by unfold skip_v skip_v_loop; exact hr, hq⟩

/-! ### Characters and their bytes -/

theorem mem_utf8 (c : Char) (cs : List Char) (hc : c ∈ cs) (x : Nat) (hx : x ∈ enc c) :
    x ∈ utf8 cs := by
  simp only [Headers.utf8, enc, List.mem_map, List.mem_flatMap] at hx ⊢
  obtain ⟨b, hb, rfl⟩ := hx
  exact ⟨b, ⟨c, hc, hb⟩, rfl⟩

/-- A separator test on characters and one on bytes agree: a separator is
one byte, which passes; no byte of any other character passes. -/
def Agree (Q : Char → Bool) (P : Nat → Bool) : Prop :=
  (∀ c, Q c = true → enc c = [c.toNat] ∧ P c.toNat = true) ∧
    (∀ c, Q c = false → ∀ x ∈ enc c, P x = false)

theorem split_none (Q : Char → Bool) (P : Nat → Bool) (hag : Agree Q P) (cs : List Char)
    (h : ∀ x ∈ utf8 cs, P x = false) : Clew.Text.split Q cs = [cs] := by
  apply Clew.Text.split_none
  intro c hc
  cases hq : Q c
  · rfl
  · exfalso
    obtain ⟨he, hp⟩ := hag.1 c hq
    have := h _ (mem_utf8 c cs hc c.toNat (by rw [he]; simp))
    rw [hp] at this; exact absurd this (by simp)

theorem split_first (Q : Char → Bool) (P : Nat → Bool) (hag : Agree Q P) :
    ∀ (cs : List Char) (pre : List Nat) (k : Nat) (rest : List Nat),
      utf8 cs = pre ++ k :: rest → (∀ x ∈ pre, P x = false) → P k = true →
      ∃ a zs, utf8 a = pre ∧ utf8 zs = rest ∧ Clew.Text.split Q cs = a :: Clew.Text.split Q zs := by
  intro cs
  induction cs with
  | nil => intro pre k rest h; simp [Headers.utf8] at h
  | cons c cs ih =>
    intro pre k rest h hpre hk
    rw [Shell.utf8_cons] at h
    cases hq : Q c
    · have hno := hag.2 c hq
      obtain ⟨pre', hpre', hcs⟩ : ∃ pre', pre = enc c ++ pre' ∧ utf8 cs = pre' ++ k :: rest := by
        rcases List.append_eq_append_iff.mp h with ⟨a', h1, h2⟩ | ⟨c', h1, h2⟩
        · exact ⟨a', h1, h2⟩
        · cases c' with
          | nil => exact ⟨[], by simp at h1; rw [← h1]; simp, by simpa using h2.symm⟩
          | cons y c'' =>
            simp only [List.cons_append, List.cons.injEq] at h2
            have : P k = false := hno k (by rw [show enc c = _ from h1, h2.1]; simp)
            rw [hk] at this; exact absurd this (by simp)
      obtain ⟨a, zs, ha, hz, hs⟩ := ih pre' k rest hcs (fun x hx => hpre x (by rw [hpre']; simp [hx])) hk
      refine ⟨c :: a, zs, by rw [Shell.utf8_cons, ha, hpre'], hz, ?_⟩
      simp only [Clew.Text.split, hq, Bool.false_eq_true, ↓reduceIte, hs]
    · obtain ⟨he, hp⟩ := hag.1 c hq
      rw [show (String.utf8EncodeChar c).map (·.toNat) = [c.toNat] from he] at h
      cases pre with
      | nil =>
        simp only [List.nil_append, List.singleton_append, List.cons.injEq] at h
        refine ⟨[], cs, rfl, h.2, ?_⟩
        simp [Clew.Text.split, hq]
      | cons p pre =>
        simp only [List.cons_append, List.cons.injEq] at h
        have := hpre p (by simp)
        rw [← h.1, hp] at this; exact absurd this (by simp)

theorem utf8_dropWhile_v : ∀ (cs : List Char) (pre rest : List Nat), utf8 cs = pre ++ rest →
    (∀ x ∈ pre, x = 118) → rest.head? ≠ some 118 → utf8 (cs.dropWhile (· == 'v')) = rest := by
  intro cs
  induction cs with
  | nil =>
    intro pre rest h _ _
    have : pre ++ rest = [] := h.symm
    simp at this; simp [this.2]; rfl
  | cons c cs ih =>
    intro pre rest h hpre hr
    by_cases hc : c = 'v'
    · subst hc
      rw [show utf8 ('v' :: cs) = 118 :: utf8 cs from by rw [Shell.utf8_cons]; rfl] at h
      cases pre with
      | nil => simp at h; rw [← h] at hr; simp at hr
      | cons p pre' =>
        simp only [List.cons_append, List.cons.injEq] at h
        rw [List.dropWhile_cons_of_pos (by simp)]
        exact ih pre' rest h.2 (fun x hx => hpre x (by simp [hx])) hr
    · obtain ⟨b, tl, hbt, hb⟩ := KnownHosts.utf8_cons_head c cs 118 (by omega) (toNat_ne c 'v' hc)
      cases pre with
      | nil =>
        rw [List.dropWhile_cons_of_neg (by simpa using hc)]
        simpa using h
      | cons p pre' =>
        rw [hbt] at h
        simp only [List.cons_append, List.cons.injEq] at h
        exact absurd (h.1 ▸ hpre p (by simp)) hb

theorem agree_dot : Agree (· == '.') (fun x => x == 46 || x == 46) := by
  constructor
  · intro c hc
    have : c = '.' := by simpa using hc
    subst this; exact ⟨rfl, rfl⟩
  · intro c hc x hx
    have hne : c ≠ '.' := by simpa using hc
    have := KnownHosts.bytes_ne c 46 (by omega) (toNat_ne c '.' hne) x hx
    simp [this]

theorem agree_sign : Agree (fun c => c == '-' || c == '+') (fun x => x == 45 || x == 43) := by
  constructor
  · intro c hc
    simp only [Bool.or_eq_true, beq_iff_eq] at hc
    rcases hc with rfl | rfl <;> exact ⟨rfl, rfl⟩
  · intro c hc x hx
    simp only [Bool.or_eq_false_iff, beq_eq_false_iff_ne] at hc
    have h1 := KnownHosts.bytes_ne c 45 (by omega) (toNat_ne c '-' hc.1) x hx
    have h2 := KnownHosts.bytes_ne c 43 (by omega) (toNat_ne c '+' hc.2) x hx
    simp [h1, h2]

/-! ### Numbers -/

/-- `digits_u64` on bytes. -/
def digitsB (ds : List Nat) : Option Nat :=
  if ds.isEmpty || !ds.all isDigitB then none
  else if foldN 0 ds < 2 ^ 64 then some (foldN 0 ds) else none

theorem digitsB_utf8 (ds : List Char) : digitsB (utf8 ds) = parseDigits ds := by
  unfold digitsB parseDigits
  rw [Shell.utf8_isEmpty, KnownHosts.utf8_all isDigitB Clew.Text.isAsciiDigit isAsciiDigit_eq isDigitB_high]
  cases hall : ds.all Clew.Text.isAsciiDigit
  · simp
  · have hascii : ∀ c ∈ ds, c.toNat < 128 := by
      intro c hc
      have := List.all_eq_true.mp hall c hc
      rw [isAsciiDigit_eq] at this
      by_contra h
      rw [isDigitB_high _ (by omega)] at this
      exact absurd this (by simp)
    have hv : foldN 0 (utf8 ds) = Clew.Text.digitsValue ds := by
      rw [KnownHosts.utf8_of_ascii ds hascii]
      unfold foldN Clew.Text.digitsValue
      rw [List.foldl_map]
      rfl
    simp [hv]

theorem parseU64_utf8 (t : List Char) : digitsB (stripB (utf8 t)) = Clew.Text.parseU64 t := by
  rw [parseU64_eq, ← utf8_stripPlus, digitsB_utf8]

/-- What `digits_u64`'s loop computes after `v` from the bytes it reads. -/
def digitsFrom (v : Nat) (ds : List Nat) : Option Nat :=
  if ds.all isDigitB then (if foldN v ds < 2 ^ 64 then some (foldN v ds) else none) else none

theorem digits_u64_loop_ok (s : Slice Std.U8) (t : Std.Usize) (ht : t.val ≤ s.val.length)
    (v : Std.U128) (i : Std.Usize) (hv : v.val < 2 ^ 64) (hi : i.val ≤ t.val) :
    ∃ r, digits_u64_loop s t v i = ok r ∧
      r.map (·.val) = digitsFrom v.val (sg s i.val t.val) := by
  obtain ⟨r, hl, hq⟩ := loop_spec (fun p : Std.U128 × Std.Usize => digits_u64_loop.body s t p.1 p.2)
    (fun p => p.1.val < 2 ^ 64 ∧ p.2.val ≤ t.val)
    (fun p r => r.map (·.val) = digitsFrom p.1.val (sg s p.2.val t.val))
    (fun p => t.val - p.2.val)
    (by
      rintro ⟨v, i⟩ ⟨hv, hi⟩
      dsimp only at hv hi ⊢
      by_cases hlt : i.val < t.val
      · have hil : i.val < s.val.length := by omega
        have hc := sg_cons s i.val t.val hlt ht
        set x := s.val[i.val] with hxdef
        by_cases hlo : x.val < 48
        · left
          refine ⟨none, by simp [digits_u64_loop.body, UScalar.lt_equiv, hlt, index_ok s i hil,
            ← hxdef, hlo], ?_⟩
          rw [hc]; simp [digitsFrom, isDigitB, hlo]
        · by_cases hhi : 57 < x.val
          · left
            refine ⟨none, by simp [digits_u64_loop.body, UScalar.lt_equiv, hlt, index_ok s i hil,
              ← hxdef, hlo, hhi], ?_⟩
            rw [hc]; simp [digitsFrom, isDigitB]; omega
          · obtain ⟨d8, hd8, hd8v⟩ := ok_of_spec (UScalar.sub_spec (x := x) (y := 48#u8) (by simp; omega))
            simp only [U8.ofNat, UScalar.ofNat, UScalar.ofNatCore_val_eq] at hd8v
            set d := UScalar.cast UScalarTy.U128 d8 with hddef
            have hdv : d.val = x.val - 48 := by rw [hddef, UScalar.cast_val_eq]; simp; omega
            obtain ⟨m1, hm1, hm1v⟩ := ok_of_spec (UScalar.mul_spec (x := v) (y := 10#u128)
              (by scalar_tac))
            obtain ⟨m2, hm2, hm2v⟩ := ok_of_spec (UScalar.add_spec (x := m1) (y := d)
              (by scalar_tac))
            have hm2v' : m2.val = v.val * 10 + (x.val - 48) := by rw [hm2v, hm1v, hdv]; simp
            have hstep : digitsFrom v.val (sg s i.val t.val) =
                digitsFrom (v.val * 10 + (x.val - 48)) (sg s (i.val + 1) t.val) := by
              rw [hc]
              have hd : isDigitB x.val = true := by simp [isDigitB]; omega
              simp only [digitsFrom, List.all_cons, hd, Bool.true_and]
              rfl
            by_cases hover : 18446744073709551615 < m2.val
            · left
              refine ⟨none, by simp [digits_u64_loop.body, UScalar.lt_equiv, hlt, index_ok s i hil,
                ← hxdef, hlo, hhi, hd8, lift, ← hddef, hm1, hm2, hover], ?_⟩
              rw [hstep]
              unfold digitsFrom
              have := foldN_ge (v.val * 10 + (x.val - 48)) (sg s (i.val + 1) t.val)
              split_ifs with h1 h2
              · omega
              · rfl
              · rfl
            · right
              obtain ⟨j, hj, hjv⟩ := succ_ok s i hil
              refine ⟨(m2, j), by simp [digits_u64_loop.body, UScalar.lt_equiv, hlt, index_ok s i hil,
                ← hxdef, hlo, hhi, hd8, lift, ← hddef, hm1, hm2, hover, hj],
                ⟨by dsimp only; omega, by dsimp only; omega⟩,
                by dsimp only; omega, ?_⟩
              intro r hr
              dsimp only at hr
              rw [hstep, ← hm2v', ← hjv]
              exact hr
      · left
        have hit : i.val = t.val := by omega
        refine ⟨some (UScalar.cast UScalarTy.U64 v), by simp [digits_u64_loop.body, UScalar.lt_equiv,
          hlt, lift], ?_⟩
        rw [hit, sg_self]
        have h' : v.val < 18446744073709551616 := hv
        simp [digitsFrom, foldN, UScalar.cast_val_eq, h', Nat.mod_eq_of_lt h'])
    (v, i) ⟨hv, hi⟩
  exact ⟨r, by unfold digits_u64_loop; exact hl, hq⟩

theorem digits_u64_ok (s : Slice Std.U8) (f t : Std.Usize) (h1 : f.val ≤ t.val)
    (h2 : t.val ≤ s.val.length) :
    ∃ r, digits_u64 s f t = ok r ∧ r.map (·.val) = digitsB (sg s f.val t.val) := by
  by_cases hft : t.val ≤ f.val
  · refine ⟨none, by simp [digits_u64, UScalar.le_equiv, hft], ?_⟩
    rw [show t.val = f.val by omega, sg_self]; rfl
  · obtain ⟨r, hr, hq⟩ := digits_u64_loop_ok s t h2 0#u128 f (by simp) h1
    refine ⟨r, by simp only [digits_u64, ge_iff_le, UScalar.le_equiv, hft, ↓reduceIte, hr], ?_⟩
    rw [hq]
    have hne : (sg s f.val t.val).isEmpty = false := by
      rw [sg_cons s f.val t.val (by omega) h2]; rfl
    unfold digitsFrom digitsB
    rw [hne]
    cases (sg s f.val t.val).all isDigitB <;> simp

theorem parse_u64_ok (s : Slice Std.U8) (f t : Std.Usize) (h1 : f.val ≤ t.val)
    (h2 : t.val ≤ s.val.length) :
    ∃ r, parse_u64 s f t = ok r ∧ r.map (·.val) = digitsB (stripB (sg s f.val t.val)) := by
  by_cases hft : f.val < t.val
  · have hfl : f.val < s.val.length := by omega
    have hc := sg_cons s f.val t.val hft h2
    by_cases hp : s.val[f.val] = 43#u8
    · obtain ⟨f1, hf1, hf1v⟩ := succ_ok s f hfl
      obtain ⟨r, hr, hq⟩ := digits_u64_ok s f1 t (by omega) h2
      refine ⟨r, by simp [parse_u64, UScalar.lt_equiv, hft, index_ok s f hfl, hp, hf1, hr], ?_⟩
      rw [hq, hc, hp, ← hf1v]; rfl
    · obtain ⟨r, hr, hq⟩ := digits_u64_ok s f t h1 h2
      refine ⟨r, by simp [parse_u64, UScalar.lt_equiv, hft, index_ok s f hfl, hp, hr], ?_⟩
      rw [hq, hc]
      have : (s.val[f.val]).val ≠ 43 := fun e => hp (UScalar.eq_of_val_eq (by rw [e]; rfl))
      simp [stripB, this]
  · obtain ⟨r, hr, hq⟩ := digits_u64_ok s f t h1 h2
    refine ⟨r, by simp [parse_u64, UScalar.lt_equiv, hft, hr], ?_⟩
    rw [hq, show t.val = f.val by omega, sg_self]; rfl

theorem parse_field_ok (s : Slice Std.U8) (f t : Std.Usize) (h1 : f.val ≤ t.val)
    (h2 : t.val ≤ s.val.length) (piece : List Char) (hp : sg s f.val t.val = utf8 piece) :
    ∃ r, parse_field s f t = ok r ∧
      r.map (·.val) = Clew.Text.parseU64 (Clew.Text.trim piece) := by
  have hlen : ((s.val.drop f.val).take (t.val - f.val)).length ≤ Usize.max := by
    have := Slice.length_ineq s; simp; omega
  set field : Slice Std.U8 := Slice.from ((s.val.drop f.val).take (t.val - f.val)) hlen with hfield
  have hidx : core.slice.index.Slice.index (core.slice.index.SliceIndexRangeUsizeSlice Std.U8) s
      { start := f, «end» := t } = ok field := by
    rw [Slice.index_SliceIndexRangeUsizeSliceInst]
    simp [core.slice.index.SliceIndexRangeUsizeSlice.index, Slice.length, h1, h2, hfield, List.slice]
  have hbs : bs field.val = utf8 piece := by
    rw [← hp, hfield, Slice.from_val]; simp [bs, List.map_take, List.map_drop]
  obtain ⟨st, en, htb, he1, he2, he3⟩ := Text.trim_bounds_is_the_model field piece hbs
  obtain ⟨r, hr, hq⟩ := parse_u64_ok field st en he1 he2
  refine ⟨r, by simp only [parse_field, hidx, bind_ok, htb]; exact hr, ?_⟩
  rw [hq, show sg field st.val en.val = utf8 (Clew.Text.trim piece) from he3, parseU64_utf8]

/-! ### The version -/

/-- A version the code holds, as the model's. -/
def toModel (v : clew_rules.version.Version) : Clew.Update.Version := ⟨v.major.val, v.minor.val, v.patch.val⟩

/-- **`is_newer` is the model's order**: for any two versions, it returns,
and says what `Clew.Update.Version.lt` says. -/
theorem is_newer_is_the_model (running v : clew_rules.version.Version) :
    is_newer running v = ok ((toModel running).lt (toModel v)) := by
  unfold is_newer toModel Clew.Update.Version.lt
  have e : ∀ x y : Std.U64, (x = y) ↔ x.val = y.val := fun x y => ⟨fun h => h ▸ rfl, UScalar.eq_of_val_eq⟩
  by_cases h1 : v.major.val = running.major.val
  · by_cases h2 : v.minor.val = running.minor.val
    · simp [bne_iff_ne, e, h1, h2, UScalar.lt_equiv]
    · have : ¬ running.minor.val = v.minor.val := fun h => h2 h.symm
      simp [bne_iff_ne, e, h1, h2, this, UScalar.lt_equiv]
  · have : ¬ running.major.val = v.major.val := fun h => h1 h.symm
    simp [bne_iff_ne, e, h1, this, UScalar.lt_equiv]

theorem no_sep (a b : Std.U8) (L : List Nat) (h : ∀ x ∈ L, x ≠ a.val ∧ x ≠ b.val) :
    ∀ x ∈ L, (x == a.val || x == b.val) = false := by
  intro x hx
  obtain ⟨h1, h2⟩ := h x hx
  simp [h1, h2]

/-- **`parse` is the model**: on the UTF-8 bytes of any string, it returns,
and reads the version `Clew.Update.parse` reads, or none when that reads none. -/
theorem parse_is_the_model (cs : List Char) (s : Slice Std.U8) (h : bs s.val = utf8 cs) :
    ∃ r, parse s = ok r ∧ r.map toModel = Clew.Update.parse cs := by
  unfold Clew.Update.parse
  dsimp only
  obtain ⟨st, en, htb, h1, h2, h3⟩ := Text.trim_bounds_is_the_model s cs h
  have h3' : sg s st.val en.val = utf8 (Clew.Text.trim cs) := h3
  obtain ⟨fr, hfr, hf1, hf2, hfv, hfk⟩ := skip_v_ok s st en h1 h2
  set s1 := (Clew.Text.trim cs).dropWhile (· == 'v') with hs1def
  have hs1 : sg s fr.val en.val = utf8 s1 := by
    symm
    apply utf8_dropWhile_v _ (sg s st.val fr.val) (sg s fr.val en.val)
    · rw [← sg_split s _ _ _ hf1 hf2]; exact h3'.symm
    · exact hfv
    · by_cases hlt : fr.val < en.val
      · obtain ⟨k, hk, hk2⟩ := hfk hlt
        rw [hk]; simpa using hk2
      · rw [show en.val = fr.val by omega, sg_self]; simp
  obtain ⟨tt, hto, ht1, ht2, htno, htk⟩ := find_ok s fr en 45#u8 43#u8 hf2 h2
  set core := (Clew.Text.split (fun c => c == '-' || c == '+') s1).headD [] with hcoredef
  have hcore : sg s fr.val tt.val = utf8 core := by
    by_cases hlt : tt.val < en.val
    · obtain ⟨k, hk, hk2⟩ := htk hlt
      obtain ⟨a, zs, ha, _, hsp⟩ := split_first _ _ agree_sign s1 (sg s fr.val tt.val) k
        (sg s (tt.val + 1) en.val) (by rw [← hs1, sg_split s _ _ _ ht1 ht2, hk])
        (by simpa using no_sep _ _ _ htno) (by simp at hk2 ⊢; omega)
      rw [hcoredef, hsp]; simpa using ha.symm
    · have hte : tt.val = en.val := by omega
      have hsp := split_none _ _ agree_sign s1 (by rw [← hs1, ← hte]; simpa using no_sep _ _ _ htno)
      rw [hcoredef, hsp]; simp [hte, hs1]
  have hto_le : tt.val ≤ s.val.length := by omega
  obtain ⟨d1, hd1, hd11, hd12, hd1no, hd1k⟩ := find_ok s fr tt 46#u8 46#u8 ht1 hto_le
  by_cases e1 : d1.val = tt.val
  · have hsp := split_none _ _ agree_dot core (by rw [← hcore, ← e1]; simpa using no_sep _ _ _ hd1no)
    have : d1 = tt := UScalar.eq_of_val_eq e1
    refine ⟨none, by simp [parse, htb, hfr, hto, hd1, this], ?_⟩
    rw [hsp]; rfl
  · have ne1 : ¬ d1 = tt := fun h => e1 (h ▸ rfl)
    obtain ⟨k1, hk1, hk1'⟩ := hd1k (by omega)
    obtain ⟨A, Z1, hA, hZ1, hspA⟩ := split_first _ _ agree_dot core (sg s fr.val d1.val) k1
      (sg s (d1.val + 1) tt.val) (by rw [← hcore, sg_split s _ _ _ hd11 hd12, hk1])
      (by simpa using no_sep _ _ _ hd1no) (by simp at hk1' ⊢; omega)
    obtain ⟨i, hi, hiv⟩ := succ_ok s d1 (by omega)
    obtain ⟨d2, hd2, hd21, hd22, hd2no, hd2k⟩ := find_ok s i tt 46#u8 46#u8 (by omega) hto_le
    by_cases e2 : d2.val = tt.val
    · have hspZ1 := split_none _ _ agree_dot Z1 (by
        rw [hZ1, show d1.val + 1 = i.val by omega, ← e2]; simpa using no_sep _ _ _ hd2no)
      have : d2 = tt := UScalar.eq_of_val_eq e2
      refine ⟨none, by simp [parse, htb, hfr, hto, hd1, ne1, hi, hd2, this], ?_⟩
      rw [hspA, hspZ1]; rfl
    · have ne2 : ¬ d2 = tt := fun h => e2 (h ▸ rfl)
      obtain ⟨k2, hk2, hk2'⟩ := hd2k (by omega)
      obtain ⟨B, Z2, hB, hZ2, hspZ1⟩ := split_first _ _ agree_dot Z1 (sg s i.val d2.val) k2
        (sg s (d2.val + 1) tt.val)
        (by rw [hZ1, show d1.val + 1 = i.val by omega, sg_split s _ _ _ hd21 hd22, hk2])
        (by simpa using no_sep _ _ _ hd2no) (by simp at hk2' ⊢; omega)
      obtain ⟨i1, hi1, hi1v⟩ := succ_ok s d2 (by omega)
      obtain ⟨d3, hd3, hd31, hd32, hd3no, hd3k⟩ := find_ok s i1 tt 46#u8 46#u8 (by omega) hto_le
      obtain ⟨C, rest, hspZ2, hC⟩ : ∃ C rest, Clew.Text.split (· == '.') Z2 = C :: rest ∧
          sg s i1.val d3.val = utf8 C := by
        by_cases e3 : d3.val = tt.val
        · refine ⟨Z2, [], split_none _ _ agree_dot Z2 ?_, ?_⟩
          · rw [hZ2, show d2.val + 1 = i1.val by omega, ← e3]; simpa using no_sep _ _ _ hd3no
          · rw [e3, hi1v, hZ2]
        · obtain ⟨k3, hk3, hk3'⟩ := hd3k (by omega)
          obtain ⟨C, Z3, hCv, _, hsp⟩ := split_first _ _ agree_dot Z2 (sg s i1.val d3.val) k3
            (sg s (d3.val + 1) tt.val)
            (by rw [hZ2, show d2.val + 1 = i1.val by omega, sg_split s _ _ _ hd31 hd32, hk3])
            (by simpa using no_sep _ _ _ hd3no) (by simp at hk3' ⊢; omega)
          exact ⟨C, _, hsp, hCv.symm⟩
      obtain ⟨o, ho, hov⟩ := parse_field_ok s fr d1 hd11 (by omega) A hA.symm
      obtain ⟨o1, ho1, ho1v⟩ := parse_field_ok s i d2 hd21 (by omega) B hB.symm
      obtain ⟨o2, ho2, ho2v⟩ := parse_field_ok s i1 d3 hd31 (by omega) C hC
      rw [hspA, hspZ1, hspZ2]
      dsimp only
      rw [← hov, ← ho1v, ← ho2v]
      cases o with
      | none =>
        exact ⟨none, by simp [parse, htb, hfr, hto, hd1, ne1, hi, hd2, ne2, hi1, hd3, ho], rfl⟩
      | some n =>
        cases o1 with
        | none =>
          exact ⟨none, by simp [parse, htb, hfr, hto, hd1, ne1, hi, hd2, ne2, hi1, hd3, ho, ho1], rfl⟩
        | some n1 =>
          cases o2 with
          | none =>
            exact ⟨none, by simp [parse, htb, hfr, hto, hd1, ne1, hi, hd2, ne2, hi1, hd3, ho, ho1,
              ho2], rfl⟩
          | some n2 =>
            exact ⟨some ⟨n, n1, n2⟩, by simp [parse, htb, hfr, hto, hd1, ne1, hi, hd2, ne2, hi1, hd3,
              ho, ho1, ho2], rfl⟩

/-- `upgrade`'s answer, read as the model's `ensureUpgrade`. -/
def upgradeOf : clew_rules.version.Upgrade → Except Clew.Update.Refusal Clew.Update.Version
  | .Unreadable => .error .unreadable
  | .NotNewer _ => .error .notNewer
  | .Newer v => .ok (toModel v)

/-- **`upgrade` is the model**: for the running version and the UTF-8 bytes
of any version text, it returns, and decides what `Clew.Update.ensureUpgrade`
decides. -/
theorem upgrade_is_the_model (running : clew_rules.version.Version) (cs : List Char)
    (s : Slice Std.U8) (h : bs s.val = utf8 cs) :
    ∃ u, upgrade running s = ok u ∧ upgradeOf u = Clew.Update.ensureUpgrade (toModel running) cs := by
  obtain ⟨r, hr, hq⟩ := parse_is_the_model cs s h
  unfold Clew.Update.ensureUpgrade
  rw [← hq]
  cases r with
  | none => exact ⟨.Unreadable, by simp [upgrade, hr], rfl⟩
  | some v =>
    by_cases hn : (toModel running).lt (toModel v) = true
    · exact ⟨.Newer v, by simp [upgrade, hr, is_newer_is_the_model, hn], by simp [upgradeOf, hn]⟩
    · exact ⟨.NotNewer v, by simp [upgrade, hr, is_newer_is_the_model, hn], by simp [upgradeOf, hn]⟩

/-- **No downgrade, of the code.** A version `upgrade` lets through is
strictly newer than the running one (`Clew.Update.accepted_is_newer`). -/
theorem upgrade_newer (running : clew_rules.version.Version) (cs : List Char) (s : Slice Std.U8)
    (h : bs s.val = utf8 cs) (v : clew_rules.version.Version)
    (hu : upgrade running s = ok (.Newer v)) : (toModel running).lt (toModel v) = true := by
  obtain ⟨u, hu', heq⟩ := upgrade_is_the_model running cs s h
  rw [hu] at hu'
  have hu2 : u = .Newer v := by simpa using hu'.symm
  subst hu2
  exact Clew.Update.accepted_is_newer _ cs _ heq.symm

end Version

/-! ## What Clew hands ssh — `clew_rules::ssh`

`ssh_args` writes a saved connection's argument list: Clew's fixed options,
`-p` unless the port is 22, `-i` unless there is no identity file, then `--`
and `user@host`. The model in `../Clew/Ssh.lean` proves ssh reads that list
as exactly Clew's options and the destination, whatever the fields hold
(`parseSshArgs_sshArgs`). Proved here: on the UTF-8 bytes of any user, host
and identity, and any port, `ssh_args` returns, and writes the model's list
(`ssh_reads_as_meant`). -/

namespace SshArgs

open clew_rules.ssh
open Shell (bs push_ok extend_ok)
open Headers (utf8)

theorem push_small {α : Type} (v : alloc.vec.Vec α) (x : α) (h : v.val.length + 1 ≤ 4294967295) :
    alloc.vec.Vec.push v x = ok (alloc.vec.Vec.from (v.val ++ [x]) (by simp; scalar_tac)) := by
  unfold alloc.vec.Vec.push
  dsimp only
  split
  · simp
  · rename_i hn; exfalso; apply hn; simp [U32.max_eq]; omega

/-- The fixed options' bytes. -/
def fixedB : List (List Nat) := [[45, 111], [67, 111, 110, 110, 101, 99, 116, 84, 105, 109, 101, 111, 117, 116, 61, 49, 48], [45, 111], [83, 116, 114, 105, 99, 116, 72, 111, 115, 116, 75, 101, 121, 67, 104, 101, 99, 107, 105, 110, 103, 61, 121, 101, 115], [45, 111], [83, 101, 114, 118, 101, 114, 65, 108, 105, 118, 101, 73, 110, 116, 101, 114, 118, 97, 108, 61, 49, 53], [45, 111], [83, 101, 114, 118, 101, 114, 65, 108, 105, 118, 101, 67, 111, 117, 110, 116, 77, 97, 120, 61, 51], [45, 111], [78, 117, 109, 98, 101, 114, 79, 102, 80, 97, 115, 115, 119, 111, 114, 100, 80, 114, 111, 109, 112, 116, 115, 61, 49]]

theorem utf8_fixed : Clew.Ssh.fixedOptions.map utf8 = fixedB := by decide

theorem word_ok (bytes : Slice Std.U8) : ∃ w, word bytes = ok w ∧ w.val = bytes.val := by
  obtain ⟨w, hw, ew⟩ := extend_ok (alloc.vec.Vec.new Std.U8) bytes (by simp)
  exact ⟨w, hw, by simpa using ew⟩

theorem digits_len (n k : Nat) (h : n < 10 ^ (k + 1)) : (Clew.Trust.digits n).length ≤ k + 1 := by
  induction k generalizing n with
  | zero => rw [Fingerprint.digits_eq]; split <;> simp_all
  | succ k ih =>
    rw [Fingerprint.digits_eq]
    split
    · simp
    · have := ih (n / 10) (by rw [pow_succ] at h; omega)
      simp; omega

set_option maxRecDepth 20000 in
theorem ssh_args_ok (user host identity : Slice Std.U8) (port : Std.U16)
    (room : user.val.length + host.val.length + 1 ≤ Usize.max) :
    ∃ r, ssh_args user host port identity = ok r ∧ r.val.map (fun w => bs w.val) =
      fixedB ++ ((if port.val = 22 then [] else [[45, 112], Clew.Trust.digits port.val]) ++
        ((if identity.val = [] then [] else [[45, 105], bs identity.val]) ++
          [[45, 45], bs user.val ++ 64 :: bs host.val])) := by
  obtain ⟨wu, hwu, ewu⟩ := word_ok user
  obtain ⟨wu2, hwu2, ewu2⟩ := push_ok wu 64#u8 (by rw [ewu]; omega)
  obtain ⟨dest, hdest, edest⟩ := extend_ok wu2 host (by rw [ewu2, ewu]; simp; omega)
  obtain ⟨wi, hwi, ewi⟩ := word_ok identity
  have hpv : (UScalar.cast UScalarTy.Usize port).val = port.val := by
    rw [UScalar.cast_val_eq]; have := port.hBounds; simp at this ⊢
    apply Nat.mod_eq_of_lt
    rcases System.Platform.numBits_eq with h | h <;> rw [h] <;> omega
  have hdl : (Clew.Trust.digits (UScalar.cast UScalarTy.Usize port).val).length ≤ 5 := by
    rw [hpv]; apply digits_len port.val 4; have := port.hBounds; simp at this ⊢; omega
  obtain ⟨d, hd, ed⟩ := Fingerprint.with_decimal_ok (UScalar.cast UScalarTy.Usize port)
    (alloc.vec.Vec.new Std.U8) (by simp; scalar_tac)
  have hie : core.slice.Slice.is_empty identity = ok (decide (identity.val = [])) := by
    simp [core.slice.Slice.is_empty]
  have hpv' : port.val = 22 ↔ port = 22#u16 := ⟨fun h => UScalar.eq_of_val_eq (by simpa using h), fun h => by simp [h]⟩
  have edig : bs d.val = Clew.Trust.digits port.val := by rw [ed, hpv]; simp [bs]
  by_cases hp : port = 22#u16
  · have hp' : port.val = 22 := hpv'.mpr hp
    by_cases hi : identity.val = []
    · simp [ssh_args, with_option, push_small, hp, hi, hwu, hwu2, hdest]
      simp only [edest, ewu2, ewu]; simp [bs, fixedB]
    · simp [ssh_args, with_option, push_small, hp, hi, hwu, hwu2, hdest, hwi]
      simp only [edest, ewu2, ewu, ewi]; simp [bs, fixedB]
  · have hp' : ¬ port.val = 22 := fun h => hp (hpv'.mp h)
    by_cases hi : identity.val = []
    · simp [ssh_args, with_option, push_small, hp, hi, hwu, hwu2, hdest, hd, lift]
      simp only [edest, ewu2, ewu]; simp [bs, fixedB, hp'] at edig ⊢; exact edig
    · simp [ssh_args, with_option, push_small, hp, hi, hwu, hwu2, hdest, hwi, hd, lift]
      simp only [edest, ewu2, ewu, ewi]; simp [bs, fixedB, hp'] at edig ⊢; exact edig

theorem utf8_decimal (n : Nat) : utf8 (Clew.Text.decimal n) = Clew.Trust.digits n := by
  rw [KnownHosts.utf8_of_ascii]
  · rfl
  · intro c hc
    have := Clew.Text.decimal_digits n c hc
    rw [KnownHosts.isAsciiDigit_eq] at this
    simp [KnownHosts.isDigitB] at this; omega

/-- **`ssh_args` is the model**: on the UTF-8 bytes of any user, host and
identity file, and any port, it returns, and writes the UTF-8 bytes of
`Clew.Ssh.sshArgs` — the list `parseSshArgs_sshArgs` proves ssh reads as
exactly Clew's options and `user@host`. -/
theorem ssh_args_is_the_model (u h i : List Char) (user host identity : Slice Std.U8) (port : Std.U16)
    (hu : bs user.val = utf8 u) (hh : bs host.val = utf8 h) (hi : bs identity.val = utf8 i)
    (room : user.val.length + host.val.length + 1 ≤ Usize.max) :
    ∃ r, ssh_args user host port identity = ok r ∧
      r.val.map (fun w => bs w.val) = (Clew.Ssh.sshArgs u h port.val i).map utf8 := by
  obtain ⟨r, hr, er⟩ := ssh_args_ok user host identity port room
  refine ⟨r, hr, ?_⟩
  rw [er]
  have hie : (identity.val = []) ↔ i.isEmpty = true := by
    rw [← Shell.utf8_isEmpty, ← hi]
    simp [bs]
  have e_dd : utf8 ['-', '-'] = [45, 45] := rfl
  have e_dp : utf8 ['-', 'p'] = [45, 112] := rfl
  have e_di : utf8 ['-', 'i'] = [45, 105] := rfl
  have e_at : utf8 ('@' :: h) = 64 :: utf8 h := by rw [Shell.utf8_cons]; rfl
  unfold Clew.Ssh.sshArgs
  simp only [List.map_append, utf8_fixed]
  congr 1
  by_cases hp : port.val = 22
  · by_cases hii : identity.val = []
    · have : i.isEmpty = true := hie.mp hii
      simp [hp, hii, this, hu, hh, Shell.utf8_append, e_dd, e_at]
    · have : i.isEmpty = false := by simpa using (mt hie.mpr hii)
      simp [hp, hii, this, hu, hh, hi, Shell.utf8_append, e_dd, e_di, e_at]
  · by_cases hii : identity.val = []
    · have : i.isEmpty = true := hie.mp hii
      simp [hp, hii, this, hu, hh, Shell.utf8_append, e_dd, e_dp, e_at, utf8_decimal]
    · have : i.isEmpty = false := by simpa using (mt hie.mpr hii)
      simp [hp, hii, this, hu, hh, hi, Shell.utf8_append, e_dd, e_dp, e_di, e_at, utf8_decimal]
/-- **ssh reads the code's list as meant**: the words `ssh_args` writes are
the UTF-8 bytes of a list ssh parses as exactly Clew's options, `-p` and
`-i`, with `user@host` as the destination (`Clew.Ssh.parseSshArgs_sshArgs`),
whatever the fields hold. -/
theorem ssh_reads_as_meant (u h i : List Char) (user host identity : Slice Std.U8) (port : Std.U16)
    (hu : bs user.val = utf8 u) (hh : bs host.val = utf8 h) (hi : bs identity.val = utf8 i)
    (room : user.val.length + host.val.length + 1 ≤ Usize.max) :
    ∃ r ws, ssh_args user host port identity = ok r ∧ r.val.map (fun w => bs w.val) = ws.map utf8 ∧
      Clew.Ssh.parseSshArgs ws = ⟨Clew.Ssh.intended port.val i, some (u ++ '@' :: h),
        (Clew.Ssh.intended port.val i).length⟩ := by
  obtain ⟨r, hr, er⟩ := ssh_args_is_the_model u h i user host identity port hu hh hi room
  exact ⟨r, _, hr, er, Clew.Ssh.parseSshArgs_sshArgs u h port.val i⟩

end SshArgs

end Clew.Extracted
