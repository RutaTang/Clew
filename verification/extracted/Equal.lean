import ClewRules
import Clew.VoiceRoute
import Clew.Guards
import Clew.Framing
import Clew.Ssh

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

end Clew.Extracted
