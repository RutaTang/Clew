import Equal
import Clew.Plugin

/-!
# The plugin rules that run are the plugin model

`crates/clew-rules/src/plugin.rs` decides what a plugin is linked with and
what its things are named on disk. Each theorem here says that the function
Charon and Aeneas made of it (`ClewRules.lean`) always returns — no panic,
no overflow, no index out of bounds, every loop ends — and returns exactly
what the model in `../Clew/Plugin.lean` computes. So the theorems proved
there (a plugin is never linked with what it was not granted, a name is
never a path, no two keys share a file) hold of the code that runs.
-/

open Aeneas Aeneas.Std Result

set_option linter.unusedSimpArgs false

namespace Clew.Extracted.PluginRules

open clew_rules.plugin
open Shell (bs)
open Clew.Plugin

/-! ## Bytes -/

theorem bs_len (l : List Std.U8) : (bs l).length = l.length := by simp [bs]

theorem u8_bne (x y : Std.U8) : (x != y) = (x.val != y.val) := by
  rw [Bool.eq_iff_iff]; simp [bne_iff_ne, ne_eq, UScalar.eq_equiv]

theorem usize_big : 4294967295 ≤ Usize.max := by
  rcases Usize.bounds_eq with h | h <;> rw [h] <;> simp [U32.max_eq, U64.max_eq]

theorem lit_toml :
    bs [112#u8, 108#u8, 117#u8, 103#u8, 105#u8, 110#u8, 46#u8, 116#u8, 111#u8, 109#u8, 108#u8] =
      [112, 108, 117, 103, 105, 110, 46, 116, 111, 109, 108] := rfl

theorem lit_src : bs [115#u8, 114#u8, 99#u8, 47#u8] = [115, 114, 99, 47] := rfl

theorem is_lower_ok (x : Std.U8) : is_lower x = ok (lowerB x.val) := by
  unfold is_lower lowerB
  simp only [ge_iff_le, UScalar.le_equiv, U8.ofNat, UScalar.ofNat,
    UScalar.ofNatCore_val_eq, Framing.ite_ok, Result.ok.injEq]
  have hb := Framing.u8_lt x
  generalize x.val = v at hb ⊢
  revert v; decide +kernel

theorem is_digit_ok (x : Std.U8) : is_digit x = ok (digitB x.val) := by
  unfold is_digit digitB
  simp only [ge_iff_le, UScalar.le_equiv, U8.ofNat, UScalar.ofNat,
    UScalar.ofNatCore_val_eq, Framing.ite_ok, Result.ok.injEq]
  have hb := Framing.u8_lt x
  generalize x.val = v at hb ⊢
  revert v; decide +kernel

theorem is_id_byte_ok (x : Std.U8) : is_id_byte x = ok (idByte x.val) := by
  unfold is_id_byte idByte
  rw [is_lower_ok, bind_ok, is_digit_ok]
  simp only [bind_ok, UScalar.eq_equiv, U8.ofNat, UScalar.ofNat, UScalar.ofNatCore_val_eq]
  cases lowerB x.val <;> cases digitB x.val <;> by_cases h : x.val = 45 <;> simp [h]

theorem is_lower_hex_ok (x : Std.U8) : is_lower_hex x = ok (lowerHex x.val) := by
  unfold is_lower_hex lowerHex
  rw [is_digit_ok, bind_ok]
  cases h : digitB x.val
  · simp only [Bool.false_eq_true, ↓reduceIte, ge_iff_le, UScalar.le_equiv, U8.ofNat,
      UScalar.ofNat, UScalar.ofNatCore_val_eq, Framing.ite_ok, Result.ok.injEq, Bool.false_or]
    have hb := Framing.u8_lt x
    generalize x.val = v at hb ⊢
    revert v; decide +kernel
  · simp

theorem is_path_byte_ok (x : Std.U8) : is_path_byte x = ok (pathByte x.val) := by
  unfold is_path_byte pathByte
  rw [is_lower_ok, bind_ok]
  cases h1 : lowerB x.val
  · simp only [Bool.false_eq_true, ↓reduceIte, Bool.false_or]
    rw [is_digit_ok, bind_ok]
    cases h2 : digitB x.val
    · simp only [Bool.false_eq_true, ↓reduceIte, Bool.false_or, ge_iff_le, UScalar.le_equiv,
        UScalar.eq_equiv, U8.ofNat, UScalar.ofNat, UScalar.ofNatCore_val_eq, Framing.ite_ok,
        Result.ok.injEq]
      have hb := Framing.u8_lt x
      generalize x.val = v at hb ⊢
      revert v; decide +kernel
    · simp
  · simp

/-! ## What a plugin may call -/

theorem not_val (g : Std.U8) : (~~~g).val = 255 - g.val := by
  have e1 : (~~~g).val = (~~~g.bv).toNat := rfl
  have e2 : g.bv.toNat = g.val := rfl
  rw [e1, BitVec.toNat_not, e2]

/-- What `may_link` decides. -/
theorem may_link_ok (n g : Std.U8) : may_link n g = ok (mayLink n.val g.val) := by
  have hu : (clew_rules.plugin.UNKNOWN : Std.U8).val = 128 := by
    unfold clew_rules.plugin.UNKNOWN; rfl
  unfold may_link mayLink Clew.Plugin.UNKNOWN
  simp only [lift, bind_ok, UScalar.eq_equiv, UScalar.val_and, hu, not_val]
  by_cases h : n.val &&& 128 = 0
  · by_cases h2 : n.val &&& (255 - g.val) = 0 <;> simp [h, h2]
  · simp [h]

/-- What `with_need` adds. -/
theorem with_need_ok (all : Std.U8) (imp : Slice Std.U8) (n : Std.U8)
    (h : need_of imp = ok n) : ∃ v, with_need all imp = ok v ∧ v.val = all.val ||| n.val := by
  unfold with_need
  rw [h, bind_ok]
  exact ⟨_, rfl, UScalar.val_or _ _⟩

/-! ## Nonces -/

theorem nonce_ok_loop_ok (s : Slice Std.U8) :
    nonce_ok_loop s 0#usize = ok ((bs s.val).all lowerHex) := by
  obtain ⟨b, hl, hq⟩ := loop_spec (nonce_ok_loop.body s) (fun _ => True)
    (fun i b => b = (s.val.drop i.val).all fun x => lowerHex x.val)
    (fun i => s.val.length - i.val) (by
      intro i _
      by_cases hi : i.val < s.val.length
      · obtain ⟨j, hj, hjv⟩ := succ_ok s i hi
        rw [List.drop_eq_getElem_cons hi]
        cases ht : lowerHex s.val[i.val].val
        · left
          exact ⟨false, by simp [nonce_ok_loop.body, hi, index_ok s i hi, is_lower_hex_ok, ht],
            by simp only [List.all_cons, ht, Bool.false_and]⟩
        · right
          refine ⟨j, by simp [nonce_ok_loop.body, hi, index_ok s i hi, is_lower_hex_ok, ht, hj],
            trivial, by omega, ?_⟩
          intro b hb; rw [hb, hjv, List.all_cons, ht, Bool.true_and]
      · left
        refine ⟨true, by simp [nonce_ok_loop.body, hi], ?_⟩
        rw [List.drop_eq_nil_of_le (by omega)]; rfl) 0#usize trivial
  unfold nonce_ok_loop
  rw [hl, hq]
  simp [bs, List.all_map, Function.comp_def]

/-- What `nonce_ok` decides. -/
theorem nonce_ok_ok (s : Slice Std.U8) : nonce_ok s = ok (nonceOk (bs s.val)) := by
  have hn : (clew_rules.plugin.NONCE_LEN : Std.Usize).val = 24 := by unfold clew_rules.plugin.NONCE_LEN; rfl
  unfold nonce_ok nonceOk
  by_cases hl : s.val.length = 24
  · have : (Slice.len s != clew_rules.plugin.NONCE_LEN) = false := by
      simp [bne_iff_ne, UScalar.eq_equiv, hn, hl]
    simp only [this, Bool.false_eq_true, ↓reduceIte, nonce_ok_loop_ok]
    simp [bs, hl]
  · have : (Slice.len s != clew_rules.plugin.NONCE_LEN) = true := by
      simp [bne_iff_ne, UScalar.eq_equiv, hn, hl]
    simp only [this, ↓reduceIte]
    simp [bs, hl]

/-! ## Loops that check each place in turn

`id_bytes_ok` and `path_ok` each walk the bytes, asking a check of each
place (`id_fine_at`, `fine_at`) that reads the byte there and the one after
it. Such a loop decides that every suffix of the bytes passes the check. -/

/-- Every suffix of `l` meets `F`. -/
def allSuffixes (F : List Nat → Bool) : List Nat → Bool
  | [] => true
  | a :: t => F (a :: t) && allSuffixes F t

theorem bs_drop' (l : List Std.U8) (i : Nat) : (bs l).drop i = bs (l.drop i) := by
  simp only [bs, List.map_drop]

theorem bs_drop_cons (l : List Std.U8) (i : Nat) (h : i < l.length) :
    (bs l).drop i = l[i].val :: (bs l).drop (i + 1) := by
  rw [bs_drop', bs_drop', KnownHosts.bs_drop l i h]

/-- A loop that, at each place below the end, goes on when `F` holds of the
bytes from there and stops with `false` when not, and stops with `true` at
the end, returns whether every suffix meets `F`. -/
theorem all_fine_loop (s : Slice Std.U8) (body : Std.Usize → Result (ControlFlow Std.Usize Bool))
    (F : List Nat → Bool)
    (hstep : ∀ i : Std.Usize, i.val < s.val.length → ∀ j : Std.Usize, i + 1#usize = ok j →
      body i = if F ((bs s.val).drop i.val) then ok (.cont j) else ok (.done false))
    (hend : ∀ i : Std.Usize, ¬ i.val < s.val.length → body i = ok (.done true)) :
    loop body 0#usize = ok (allSuffixes F (bs s.val)) := by
  obtain ⟨b, hl, hq⟩ := loop_spec body (fun _ => True)
    (fun i b => b = allSuffixes F ((bs s.val).drop i.val))
    (fun i => s.val.length - i.val) (by
      intro i _
      by_cases hi : i.val < s.val.length
      · obtain ⟨j, hj, hjv⟩ := succ_ok s i hi
        have hd := bs_drop_cons s.val i.val hi
        rw [hstep i hi j hj]
        cases hF : F ((bs s.val).drop i.val)
        · left
          refine ⟨false, rfl, ?_⟩
          rw [hd, allSuffixes, ← hd, hF, Bool.false_and]
        · right
          refine ⟨j, rfl, trivial, by omega, ?_⟩
          intro b hb
          rw [hb, hjv, hd, allSuffixes, ← hd, hF, Bool.true_and]
      · left
        refine ⟨true, hend i hi, ?_⟩
        rw [List.drop_eq_nil_of_le (by simp [bs]; omega)]; rfl) 0#usize trivial
  rw [hl, hq]; rfl

/-! ## Ids -/

/-- What `id_fine_at` checks of the bytes from a place on. -/
def idFine : List Nat → Bool
  | [] => true
  | a :: rest => idByte a && (a != 45 || match rest with
    | [] => false
    | c :: _ => c != 45)

theorem idFine_all : ∀ l, allSuffixes idFine l = (l.all idByte && dashesOk l)
  | [] => rfl
  | [a] => by
    simp only [allSuffixes, idFine, List.all_cons, List.all_nil, dashesOk, Bool.or_false,
      Bool.and_true]
  | a :: c :: t => by
    rw [allSuffixes, idFine_all (c :: t)]
    simp only [idFine, List.all_cons, dashesOk]
    generalize idByte a = p, idByte c = q, t.all idByte = r, dashesOk (c :: t) = d
    by_cases ha : a = 45 <;> by_cases hc : c = 45 <;>
      cases p <;> cases q <;> cases r <;> cases d <;> simp [ha, hc, bne]

theorem id_fine_at_ok (s : Slice Std.U8) (i : Std.Usize) (hi : i.val < s.val.length) :
    id_fine_at s i = ok (idFine ((bs s.val).drop i.val)) := by
  rw [bs_drop_cons s.val i.val hi]
  unfold id_fine_at
  rw [index_ok s i hi, bind_ok, is_id_byte_ok, bind_ok]
  simp only [idFine]
  cases hp : idByte s.val[i.val].val
  · simp
  · by_cases h45 : s.val[i.val].val = 45
    · have hne : (s.val[i.val] != 45#u8) = false := by
        simp [bne_iff_ne, UScalar.eq_equiv, h45]
      obtain ⟨j, hj, hjv⟩ := succ_ok s i hi
      simp only [hne, Bool.false_eq_true, ↓reduceIte, hj, bind_ok, h45, bne_self_eq_false,
        Bool.false_or, Bool.true_and]
      by_cases hend : i.val + 1 = s.val.length
      · have hjn : j = Slice.len s := UScalar.eq_of_val_eq (by simp [hjv, hend])
        rw [List.drop_eq_nil_of_le (by simp [bs]; omega)]
        simp [hjn]
      · have hj' : j.val < s.val.length := by omega
        have hjne : ¬ j = Slice.len s := fun e => hend (by rw [← hjv, e]; simp)
        have hix : Slice.index_usize s j = ok s.val[i.val + 1] := by
          rw [index_ok s j hj']; simp [hjv]
        rw [bs_drop_cons s.val (i.val + 1) (by omega)]
        simp only [hjne, ↓reduceIte, hix, bind_ok, u8_bne]
        simp
    · have hne : (s.val[i.val] != 45#u8) = true := by
        simp [bne_iff_ne, UScalar.eq_equiv, h45]
      simp [hne, h45]

theorem id_bytes_ok_ok (s : Slice Std.U8) :
    id_bytes_ok s = ok ((bs s.val).all idByte && dashesOk (bs s.val)) := by
  rw [← idFine_all]
  unfold id_bytes_ok id_bytes_ok_loop
  apply all_fine_loop s _ idFine
  · intro i hi j hj
    have hlt : i < Slice.len s := by simp [UScalar.lt_equiv, hi]
    simp only [id_bytes_ok_loop.body, hlt, ↓reduceIte, id_fine_at_ok s i hi, bind_ok, hj]
  · intro i hi
    have hlt : ¬ i < Slice.len s := by simp [UScalar.lt_equiv]; omega
    simp only [id_bytes_ok_loop.body, hlt, ↓reduceIte]

/-- What `id_ok` decides. -/
theorem id_ok_ok (s : Slice Std.U8) (min : Std.Usize) :
    id_ok s min = ok (idOk (bs s.val) min.val) := by
  have hm : (clew_rules.plugin.ID_MAX : Std.Usize).val = 40 := by
    unfold clew_rules.plugin.ID_MAX; rfl
  unfold id_ok idOk
  by_cases he : s.val = []
  · simp [core.slice.Slice.is_empty, he, bs]
  obtain ⟨c, t, hct⟩ := List.exists_cons_of_ne_nil he
  have h0 : (0 : Nat) < s.val.length := by rw [hct]; simp
  have hb : bs s.val = c.val :: bs t := by rw [hct]; rfl
  have hlen : s.val.length = t.length + 1 := by rw [hct]; simp
  have hidx : Slice.index_usize s 0#usize = ok c := by
    rw [index_ok s 0#usize h0]; simp [hct]
  simp only [core.slice.Slice.is_empty, Slice.length, he, decide_false, Bool.false_eq_true,
    ↓reduceIte, hb, UScalar.lt_equiv, Slice.len_val, hlen, hm, gt_iff_lt, hidx, bind_ok,
    is_lower_ok, id_bytes_ok_ok]
  by_cases hmin : t.length + 1 < min.val
  · simp [bs_len, hmin, show ¬ min.val ≤ t.length + 1 by omega]
  by_cases hmax : 40 < t.length + 1
  · simp [bs_len, hmin, hmax, show ¬ t.length < 40 by omega]
  cases hc : lowerB c.val <;>
    simp [bs_len, hmin, hmax, hc, show min.val ≤ t.length + 1 by omega, show t.length < 40 by omega,
      Bool.and_assoc]

/-! ## A plugin's source -/

/-- What `fine_at` checks of the bytes from a place on. -/
def pathFine : List Nat → Bool
  | [] => true
  | a :: rest => pathByte a && (a != 47 || match rest with
    | [] => false
    | c :: _ => c != 47 && c != 46)

theorem pathFine_all : ∀ l, allSuffixes pathFine l = (l.all pathByte && slashesOk l)
  | [] => rfl
  | [a] => by
    simp only [allSuffixes, pathFine, List.all_cons, List.all_nil, slashesOk, Bool.or_false,
      Bool.and_true]
  | a :: c :: t => by
    rw [allSuffixes, pathFine_all (c :: t)]
    simp only [pathFine, List.all_cons, slashesOk]
    generalize pathByte a = p, pathByte c = q, t.all pathByte = r, slashesOk (c :: t) = d
    by_cases ha : a = 47 <;> by_cases hc : c = 47 <;> by_cases hd : c = 46 <;>
      cases p <;> cases q <;> cases r <;> cases d <;> simp [ha, hc, hd, bne]

theorem fine_at_ok (s : Slice Std.U8) (i : Std.Usize) (hi : i.val < s.val.length) :
    fine_at s i = ok (pathFine ((bs s.val).drop i.val)) := by
  rw [bs_drop_cons s.val i.val hi]
  unfold fine_at
  rw [index_ok s i hi, bind_ok, is_path_byte_ok, bind_ok]
  simp only [pathFine]
  cases hp : pathByte s.val[i.val].val
  · simp
  · by_cases h47 : s.val[i.val].val = 47
    · have hne : (s.val[i.val] != 47#u8) = false := by
        simp [bne_iff_ne, UScalar.eq_equiv, h47]
      obtain ⟨j, hj, hjv⟩ := succ_ok s i hi
      simp only [hne, Bool.false_eq_true, ↓reduceIte, hj, bind_ok, h47, bne_self_eq_false,
        Bool.false_or, Bool.true_and]
      by_cases hend : i.val + 1 = s.val.length
      · have hjn : j = Slice.len s := UScalar.eq_of_val_eq (by simp [hjv, hend])
        rw [List.drop_eq_nil_of_le (by simp [bs]; omega)]
        simp [hjn]
      · have hj' : j.val < s.val.length := by omega
        have hjne : ¬ j = Slice.len s := fun e => hend (by rw [← hjv, e]; simp)
        have hix : Slice.index_usize s j = ok s.val[i.val + 1] := by
          rw [index_ok s j hj']; simp [hjv]
        rw [bs_drop_cons s.val (i.val + 1) (by omega)]
        simp only [hjne, ↓reduceIte, hix, bind_ok]
        simp only [u8_bne]
        by_cases h2 : s.val[i.val + 1].val = 47 <;> simp [h2]
    · have hne : (s.val[i.val] != 47#u8) = true := by
        simp [bne_iff_ne, UScalar.eq_equiv, h47]
      simp [hne, h47]

theorem path_ok_ok (s : Slice Std.U8) :
    path_ok s = ok ((bs s.val).all pathByte && slashesOk (bs s.val)) := by
  rw [← pathFine_all]
  unfold path_ok path_ok_loop
  apply all_fine_loop s _ pathFine
  · intro i hi j hj
    have hlt : i < Slice.len s := by simp [UScalar.lt_equiv, hi]
    simp only [path_ok_loop.body, hlt, ↓reduceIte, fine_at_ok s i hi, bind_ok, hj]
  · intro i hi
    have hlt : ¬ i < Slice.len s := by simp [UScalar.lt_equiv]; omega
    simp only [path_ok_loop.body, hlt, ↓reduceIte]

theorem b_plugin_toml : b "plugin.toml" = [112, 108, 117, 103, 105, 110, 46, 116, 111, 109, 108] := by
  decide
theorem b_src : b "src/" = [115, 114, 99, 47] := by decide

theorem last_three (l : List Nat) (h : 3 ≤ l.length) (k : Nat) (hk : 1 ≤ k ∧ k ≤ 3)
    (x : Nat) : (l[l.length - k]? == some x) = (l[l.length - k]! == x) := by
  have : l.length - k < l.length := by omega
  simp [List.getElem?_eq_getElem this, getElem!_pos l (l.length - k) this]

/-- What `source_kind` decides. -/
theorem source_kind_ok (s : Slice Std.U8) :
    ∃ v, source_kind s = ok v ∧ v.val = sourceKind (bs s.val) := by
  have h0 : (clew_rules.plugin.SOURCE_REFUSED : Std.U8).val = 0 := by unfold clew_rules.plugin.SOURCE_REFUSED; rfl
  have h1 : (clew_rules.plugin.SOURCE_MANIFEST : Std.U8).val = 1 := by unfold clew_rules.plugin.SOURCE_MANIFEST; rfl
  have h2 : (clew_rules.plugin.SOURCE_RUST : Std.U8).val = 2 := by unfold clew_rules.plugin.SOURCE_RUST; rfl
  unfold source_kind sourceKind
  simp only [lift, bind_ok, Bytes.same_bytes_ok, Array.val_to_slice, Array.make_val, lit_toml,
    b_plugin_toml, b_src]
  by_cases hm : bs s.val = [112, 108, 117, 103, 105, 110, 46, 116, 111, 109, 108]
  · exact ⟨clew_rules.plugin.SOURCE_MANIFEST, by simp [hm], by simp [hm, h1]⟩
  simp only [hm, decide_false, Bool.false_eq_true, ↓reduceIte, beq_iff_eq]
  by_cases hshort : s.val.length < 8
  · refine ⟨clew_rules.plugin.SOURCE_REFUSED, by simp [UScalar.lt_equiv, hshort], ?_⟩
    simp [h0, bs, show ¬ 8 ≤ s.val.length by omega]
  have hlt8 : ¬ Slice.len s < 8#usize := by simp [UScalar.lt_equiv]; omega
  simp only [hlt8, ↓reduceIte, Bytes.starts_with_ok, Array.val_to_slice, Array.make_val, lit_src]
  have hblen : (bs s.val).length = s.val.length := by simp [bs]
  cases hp : ([115, 114, 99, 47] : List Nat).isPrefixOf (bs s.val)
  · refine ⟨clew_rules.plugin.SOURCE_REFUSED, by simp, ?_⟩
    simp [h0, hp]
  simp only [bind_ok, ↓reduceIte]
  have hn := Slice.length_ineq s
  have hge8 : 8 ≤ s.val.length := Nat.le_of_not_lt hshort
  obtain ⟨k3, hk3, hk3v⟩ := ok_of_spec (UScalar.sub_spec (x := Slice.len s) (y := 3#usize)
    (by simp; omega))
  obtain ⟨k2, hk2, hk2v⟩ := ok_of_spec (UScalar.sub_spec (x := Slice.len s) (y := 2#usize)
    (by simp; omega))
  obtain ⟨k1, hk1, hk1v⟩ := ok_of_spec (UScalar.sub_spec (x := Slice.len s) (y := 1#usize)
    (by simp; omega))
  have e3 : k3.val = s.val.length - 3 := by
    have := hk3v.1; simp [Slice.length] at this; exact this
  have e2 : k2.val = s.val.length - 2 := by
    have := hk2v.1; simp [Slice.length] at this; exact this
  have e1 : k1.val = s.val.length - 1 := by
    have := hk1v.1; simp [Slice.length] at this; exact this
  have i3 : Slice.index_usize s k3 = ok s.val[s.val.length - 3] := by
    rw [index_ok s k3 (by rw [e3]; omega)]; simp [e3]
  have i2 : Slice.index_usize s k2 = ok s.val[s.val.length - 2] := by
    rw [index_ok s k2 (by rw [e2]; omega)]; simp [e2]
  have i1 : Slice.index_usize s k1 = ok s.val[s.val.length - 1] := by
    rw [index_ok s k1 (by rw [e1]; omega)]; simp [e1]
  have g3 : (bs s.val)[(bs s.val).length - 3]? = some s.val[s.val.length - 3].val := by
    rw [bs_len]
    simp only [bs, List.getElem?_map,
      List.getElem?_eq_getElem (show s.val.length - 3 < s.val.length by omega), Option.map_some]
  have g2 : (bs s.val)[(bs s.val).length - 2]? = some s.val[s.val.length - 2].val := by
    rw [bs_len]
    simp only [bs, List.getElem?_map,
      List.getElem?_eq_getElem (show s.val.length - 2 < s.val.length by omega), Option.map_some]
  have g1 : (bs s.val)[(bs s.val).length - 1]? = some s.val[s.val.length - 1].val := by
    rw [bs_len]
    simp only [bs, List.getElem?_map,
      List.getElem?_eq_getElem (show s.val.length - 1 < s.val.length by omega), Option.map_some]
  simp only [hk3, hk2, hk1, i3, i2, i1, bind_ok, g3, g2, g1, Option.some.injEq, beq_iff_eq,
    bne_iff_ne, ne_eq, UScalar.eq_equiv, path_ok_ok, show 8 ≤ (bs s.val).length by omega,
    decide_true, Bool.true_and, hp]
  by_cases e3 : s.val[s.val.length - 3].val = 46
  · by_cases e2 : s.val[s.val.length - 2].val = 114
    · by_cases e1 : s.val[s.val.length - 1].val = 115
      · cases hq : ((bs s.val).all pathByte && slashesOk (bs s.val))
        · exact ⟨clew_rules.plugin.SOURCE_REFUSED, by simp [e3, e2, e1, hq], by simp [h0, e3, e2, e1, hq]⟩
        · exact ⟨clew_rules.plugin.SOURCE_RUST, by simp [e3, e2, e1, hq], by simp [h2, e3, e2, e1, hq]⟩
      · exact ⟨clew_rules.plugin.SOURCE_REFUSED, by simp [e3, e2, e1], by simp [h0, e3, e2, e1]⟩
    · exact ⟨clew_rules.plugin.SOURCE_REFUSED, by simp [e3, e2], by simp [h0, e3, e2]⟩
  · exact ⟨clew_rules.plugin.SOURCE_REFUSED, by simp [e3], by simp [h0, e3]⟩

/-! ## The files of a plugin's store -/

theorem hex_digit_ok (n : Std.U8) (h : n.val < 16) :
    ∃ v, hex_digit n = ok v ∧ v.val = hexd n.val := by
  unfold hex_digit hexd
  by_cases h10 : n.val < 10
  · have hl : n < 10#u8 := by simp [UScalar.lt_equiv, h10]
    obtain ⟨v, hv, ev⟩ := ok_of_spec (UScalar.add_spec (x := 48#u8) (y := n)
      (by simp [U8.max_eq]; omega))
    exact ⟨v, by simp [hl, hv], by simp [ev, h10]⟩
  · have hl : ¬ n < 10#u8 := by simp [UScalar.lt_equiv]; omega
    obtain ⟨d, hd, ed⟩ := ok_of_spec (UScalar.sub_spec (x := n) (y := 10#u8) (by simp; omega))
    obtain ⟨v, hv, ev⟩ := ok_of_spec (UScalar.add_spec (x := 97#u8) (y := d)
      (by simp [ed.1, U8.max_eq]; omega))
    exact ⟨v, by simp [hl, hd, hv], by simp [ev, ed.1, h10]⟩

theorem flatMap_hexPair_length (l : List Nat) : (l.flatMap hexPair).length = 2 * l.length := by
  induction l with
  | nil => rfl
  | cons x xs ih => simp [List.flatMap_cons, hexPair, ih]; omega

theorem key_name_loop_ok (key : Slice Std.U8) (hk : key.val.length ≤ 128) :
    ∃ v, key_name_loop key (alloc.vec.Vec.new Std.U8) 0#usize = ok v ∧
      bs v.val = (bs key.val).flatMap hexPair := by
  obtain ⟨r, hl, hq⟩ := loop_spec (fun (o, i) => key_name_loop.body key o i)
    (fun p => p.2.val ≤ key.val.length ∧
      bs p.1.val = (bs (key.val.take p.2.val)).flatMap hexPair)
    (fun _ r => bs r.val = (bs key.val).flatMap hexPair)
    (fun p => key.val.length - p.2.val) (by
      rintro ⟨o, i⟩ ⟨hle, ho⟩
      dsimp only at hle ho
      have hlen : o.val.length = 2 * i.val := by
        have := congrArg List.length ho
        rw [flatMap_hexPair_length] at this
        simpa [bs, List.length_take, Nat.min_eq_left hle] using this
      have hu := usize_big
      by_cases hi : i.val < key.val.length
      · obtain ⟨j, hj, hjv⟩ := succ_ok key i hi
        have hx := Framing.u8_lt key.val[i.val]
        obtain ⟨q, hq, eq⟩ := UScalar.div_spec key.val[i.val] (y := 16#u8) (by simp)
        obtain ⟨d1, hd1, ed1⟩ := hex_digit_ok q (by rw [eq]; simp; omega)
        obtain ⟨o1, ho1, eo1⟩ := Shell.push_ok o d1 (by simp [hlen]; omega)
        obtain ⟨m, hm, em⟩ := ok_of_spec (UScalar.rem_spec key.val[i.val] (y := 16#u8) (by simp))
        obtain ⟨d2, hd2, ed2⟩ := hex_digit_ok m (by rw [em]; simp; omega)
        obtain ⟨o2, ho2, eo2⟩ := Shell.push_ok o1 d2 (by rw [eo1]; simp [hlen]; omega)
        right
        refine ⟨(o2, j), ?_, ⟨show j.val ≤ key.val.length by omega, ?_⟩,
          show key.val.length - j.val < key.val.length - i.val by omega, fun _ h => h⟩
        · simp [key_name_loop.body, hi, index_ok key i hi, hq, hd1, ho1, hm, hd2, ho2, hj]
        · show bs o2.val = _
          rw [eo2, eo1, hjv, Shell.bs_take_succ _ _ hi, List.flatMap_append]
          simp only [bs, List.map_append, List.map_cons, List.map_nil] at ho ⊢
          rw [ho, ed1, ed2, eq, em]
          simp [hexPair]
      · left
        refine ⟨o, by simp [key_name_loop.body, hi], ?_⟩
        simpa [List.take_of_length_le (show key.val.length ≤ i.val by omega)] using ho)
    (alloc.vec.Vec.new Std.U8, 0#usize) ⟨by simp, by simp [bs]⟩
  exact ⟨r, by unfold key_name_loop; exact hl, hq⟩

/-- What `key_name` writes. -/
theorem key_name_ok (key : Slice Std.U8) :
    ∃ v, key_name key = ok v ∧ bs v.val = keyName (bs key.val) := by
  have hkm : (clew_rules.plugin.KEY_MAX : Std.Usize).val = 128 := by unfold clew_rules.plugin.KEY_MAX; rfl
  unfold key_name keyName
  by_cases he : key.val = []
  · exact ⟨alloc.vec.Vec.new Std.U8, by simp [core.slice.Slice.is_empty, he], by simp [he, bs]⟩
  by_cases hbig : 128 < key.val.length
  · exact ⟨alloc.vec.Vec.new Std.U8,
      by simp [core.slice.Slice.is_empty, he, UScalar.lt_equiv, hkm, hbig],
      by simp [bs, hbig]⟩
  have hu := usize_big
  obtain ⟨v, hv, ev⟩ := key_name_loop_ok key (by omega)
  have hvl : v.val.length = 2 * key.val.length := by
    have := congrArg List.length ev
    rw [flatMap_hexPair_length] at this
    simpa [bs] using this
  obtain ⟨o1, h1, e1⟩ := Shell.push_ok v 46#u8 (by rw [hvl]; omega)
  obtain ⟨o2, h2, e2⟩ := Shell.push_ok o1 98#u8 (by rw [e1]; simp [hvl]; omega)
  obtain ⟨o3, h3, e3⟩ := Shell.push_ok o2 105#u8 (by rw [e2, e1]; simp [hvl]; omega)
  obtain ⟨o4, h4, e4⟩ := Shell.push_ok o3 110#u8 (by rw [e3, e2, e1]; simp [hvl]; omega)
  refine ⟨o4, ?_, ?_⟩
  · simp [core.slice.Slice.is_empty, he, UScalar.lt_equiv, hkm, hbig, hv, h1, h2, h3, h4]
  · rw [e4, e3, e2, e1]
    have hne : (bs key.val).isEmpty = false := by simp [bs, he]
    have hb' : decide (128 < (bs key.val).length) = false := by simp [bs_len, hbig]
    have hbin : b ".bin" = [46, 98, 105, 110] := by decide
    rw [hne, hb', hbin]
    simp only [Bool.or_false, Bool.false_eq_true, ↓reduceIte]
    simp only [bs, List.map_append, List.map_cons, List.map_nil] at ev ⊢
    rw [ev]
    simp

/-! ## What an import needs -/

theorem lit_project : bs [99#u8, 108#u8, 101#u8, 119#u8, 58#u8, 112#u8, 108#u8, 117#u8, 103#u8, 105#u8, 110#u8, 47#u8, 112#u8, 114#u8, 111#u8, 106#u8, 101#u8, 99#u8, 116#u8] = b "clew:plugin/project" := by
  rw [show b "clew:plugin/project" = [99, 108, 101, 119, 58, 112, 108, 117, 103, 105, 110, 47, 112, 114, 111, 106, 101, 99, 116] by decide]; rfl

theorem lit_git : bs [99#u8, 108#u8, 101#u8, 119#u8, 58#u8, 112#u8, 108#u8, 117#u8, 103#u8, 105#u8, 110#u8, 47#u8, 103#u8, 105#u8, 116#u8] = b "clew:plugin/git" := by
  rw [show b "clew:plugin/git" = [99, 108, 101, 119, 58, 112, 108, 117, 103, 105, 110, 47, 103, 105, 116] by decide]; rfl

theorem lit_host : bs [99#u8, 108#u8, 101#u8, 119#u8, 58#u8, 112#u8, 108#u8, 117#u8, 103#u8, 105#u8, 110#u8, 47#u8, 104#u8, 111#u8, 115#u8, 116#u8] = b "clew:plugin/host" := by
  rw [show b "clew:plugin/host" = [99, 108, 101, 119, 58, 112, 108, 117, 103, 105, 110, 47, 104, 111, 115, 116] by decide]; rfl

theorem lit_types : bs [99#u8, 108#u8, 101#u8, 119#u8, 58#u8, 112#u8, 108#u8, 117#u8, 103#u8, 105#u8, 110#u8, 47#u8, 116#u8, 121#u8, 112#u8, 101#u8, 115#u8] = b "clew:plugin/types" := by
  rw [show b "clew:plugin/types" = [99, 108, 101, 119, 58, 112, 108, 117, 103, 105, 110, 47, 116, 121, 112, 101, 115] by decide]; rfl

theorem lit_io : bs [119#u8, 97#u8, 115#u8, 105#u8, 58#u8, 105#u8, 111#u8] = b "wasi:io" := by
  rw [show b "wasi:io" = [119, 97, 115, 105, 58, 105, 111] by decide]; rfl

theorem lit_cli : bs [119#u8, 97#u8, 115#u8, 105#u8, 58#u8, 99#u8, 108#u8, 105#u8] = b "wasi:cli" := by
  rw [show b "wasi:cli" = [119, 97, 115, 105, 58, 99, 108, 105] by decide]; rfl

theorem lit_clocks : bs [119#u8, 97#u8, 115#u8, 105#u8, 58#u8, 99#u8, 108#u8, 111#u8, 99#u8, 107#u8, 115#u8] = b "wasi:clocks" := by
  rw [show b "wasi:clocks" = [119, 97, 115, 105, 58, 99, 108, 111, 99, 107, 115] by decide]; rfl

theorem lit_random : bs [119#u8, 97#u8, 115#u8, 105#u8, 58#u8, 114#u8, 97#u8, 110#u8, 100#u8, 111#u8, 109#u8] = b "wasi:random" := by
  rw [show b "wasi:random" = [119, 97, 115, 105, 58, 114, 97, 110, 100, 111, 109] by decide]; rfl

theorem lit_filesystem : bs [119#u8, 97#u8, 115#u8, 105#u8, 58#u8, 102#u8, 105#u8, 108#u8, 101#u8, 115#u8, 121#u8, 115#u8, 116#u8, 101#u8, 109#u8] = b "wasi:filesystem" := by
  rw [show b "wasi:filesystem" = [119, 97, 115, 105, 58, 102, 105, 108, 101, 115, 121, 115, 116, 101, 109] by decide]; rfl

theorem lit_sockets : bs [119#u8, 97#u8, 115#u8, 105#u8, 58#u8, 115#u8, 111#u8, 99#u8, 107#u8, 101#u8, 116#u8, 115#u8] = b "wasi:sockets" := by
  rw [show b "wasi:sockets" = [119, 97, 115, 105, 58, 115, 111, 99, 107, 101, 116, 115] by decide]; rfl

theorem unversioned_len_ok (s : Slice Std.U8) :
    ∃ r : Std.Usize, unversioned_len s = ok r ∧ r.val ≤ s.val.length ∧
      bs (s.val.take r.val) = unversioned (bs s.val) := by
  obtain ⟨r, hl, hle, hq⟩ := loop_spec (unversioned_len_loop.body s)
    (fun i => i.val ≤ s.val.length)
    (fun i r => r.val ≤ s.val.length ∧
      bs (s.val.take r.val) = bs (s.val.take i.val) ++ ((bs s.val).drop i.val).takeWhile (· != 64))
    (fun i => s.val.length - i.val) (by
      intro i hle
      by_cases hi : i.val < s.val.length
      · have hd := bs_drop_cons s.val i.val hi
        by_cases h64 : s.val[i.val].val = 64
        · left
          refine ⟨i, ?_, hle, ?_⟩
          · simp [unversioned_len_loop.body, hi, index_ok s i hi, UScalar.eq_equiv, h64]
          · rw [hd]; simp [h64]
        · obtain ⟨j, hj, hjv⟩ := succ_ok s i hi
          right
          refine ⟨j, ?_, by omega, by omega, ?_⟩
          · simp [unversioned_len_loop.body, hi, index_ok s i hi, UScalar.eq_equiv, h64, hj]
          · rintro r ⟨hr, hq⟩
            refine ⟨hr, ?_⟩
            rw [hq, hjv, Shell.bs_take_succ _ _ hi, hd, List.takeWhile_cons_of_pos (by simpa using h64)]
            simp
      · left
        have hieq : i.val = s.val.length := by omega
        refine ⟨Slice.len s, by simp [unversioned_len_loop.body, hi], by simp, ?_⟩
        rw [List.drop_eq_nil_of_le (by simp [bs]; omega)]
        simp [hieq]) 0#usize (by simp)
  refine ⟨r, by unfold unversioned_len unversioned_len_loop; exact hl, hle, ?_⟩
  rw [hq]; simp [unversioned, bs]

theorem in_package_ok (nm pre : Slice Std.U8) (hp : pre.val.length < 1000) :
    in_package nm pre = ok (inPackage (bs nm.val) (bs pre.val)) := by
  have hu := usize_big
  unfold in_package inPackage
  rw [Bytes.starts_with_ok, bind_ok]
  cases hpre : (bs pre.val).isPrefixOf (bs nm.val)
  · simp
  · obtain ⟨j, hj, hjv⟩ := ok_of_spec (UScalar.add_spec (x := Slice.len pre) (y := 1#usize)
      (by simp [UScalar.max_USize_eq]; omega))
    simp only [↓reduceIte, hj, bind_ok, Bool.true_and, bs_len]
    by_cases hlong : pre.val.length + 1 < nm.val.length
    · have hgt : Slice.len nm > j := by
        simp [UScalar.lt_equiv, hjv]; omega
      have hix : Slice.index_usize nm (Slice.len pre) = ok nm.val[pre.val.length] := by
        rw [index_ok nm (Slice.len pre) (by simp [Slice.length]; omega)]; simp [Slice.length]
      have hget : (bs nm.val)[pre.val.length]? = some nm.val[pre.val.length].val := by
        simp only [bs, List.getElem?_map,
          List.getElem?_eq_getElem (show pre.val.length < nm.val.length by omega),
          Option.map_some]
      simp only [hgt, ↓reduceIte, hix, bind_ok, hget, hlong, decide_true, Bool.true_and]
      by_cases hx : nm.val[pre.val.length].val = 47 <;> simp [UScalar.eq_equiv, hx]
    · have hgt : ¬ Slice.len nm > j := by
        simp [UScalar.lt_equiv, hjv]; omega
      simp [hgt, hlong]

theorem wasi_linked_ok (nm : Slice Std.U8) :
    wasi_linked nm = ok (wasiLinked (bs nm.val)) := by
  unfold wasi_linked wasiLinked wasiPackages
  simp only [lift, bind_ok]
  rw [in_package_ok _ _ (by simp), in_package_ok _ _ (by simp), in_package_ok _ _ (by simp),
    in_package_ok _ _ (by simp), in_package_ok _ _ (by simp), in_package_ok _ _ (by simp)]
  simp only [Array.val_to_slice, Array.make_val, lit_io, lit_cli, lit_clocks, lit_random,
    lit_filesystem, lit_sockets, bind_ok, List.any_cons, List.any_nil, Bool.or_false]
  generalize inPackage (bs nm.val) (b "wasi:io") = x1, inPackage (bs nm.val) (b "wasi:cli") = x2,
    inPackage (bs nm.val) (b "wasi:clocks") = x3, inPackage (bs nm.val) (b "wasi:random") = x4,
    inPackage (bs nm.val) (b "wasi:filesystem") = x5,
    inPackage (bs nm.val) (b "wasi:sockets") = x6
  cases x1 <;> cases x2 <;> cases x3 <;> cases x4 <;> cases x5 <;> cases x6 <;> rfl

/-- What `need_of` decides. -/
theorem need_of_ok (s : Slice Std.U8) :
    ∃ v, need_of s = ok v ∧ v.val = needOf (bs s.val) := by
  have hR : (clew_rules.plugin.READ : Std.U8).val = 1 := by unfold clew_rules.plugin.READ; rfl
  have hG : (clew_rules.plugin.GIT : Std.U8).val = 2 := by unfold clew_rules.plugin.GIT; rfl
  have hU : (clew_rules.plugin.UNKNOWN : Std.U8).val = 128 := by
    unfold clew_rules.plugin.UNKNOWN; rfl
  obtain ⟨r, hr, hrle, hrv⟩ := unversioned_len_ok s
  obtain ⟨nm, hnm, hnmv, _⟩ := ok_of_spec
    (core.slice.index.SliceIndexRangeToUsizeSlice.index.step_spec ⟨r⟩ s (by simpa using hrle))
  have hname : bs nm.val = unversioned (bs s.val) := by
    rw [hnmv, ← hrv]; simp [List.slice]
  unfold need_of
  simp only [hr, bind_ok, Slice.index_SliceIndexRangeToUsizeSliceInst, hnm, lift,
    Bytes.same_bytes_ok, Array.val_to_slice, Array.make_val, lit_project, lit_git, lit_host,
    lit_types, hname, wasi_linked_ok]
  unfold needOf needOfName
  have ep : b "clew:plugin/project" = [99, 108, 101, 119, 58, 112, 108, 117, 103, 105, 110, 47,
    112, 114, 111, 106, 101, 99, 116] := by decide
  have eg : b "clew:plugin/git" = [99, 108, 101, 119, 58, 112, 108, 117, 103, 105, 110, 47,
    103, 105, 116] := by decide
  have eh : b "clew:plugin/host" = [99, 108, 101, 119, 58, 112, 108, 117, 103, 105, 110, 47,
    104, 111, 115, 116] := by decide
  have et : b "clew:plugin/types" = [99, 108, 101, 119, 58, 112, 108, 117, 103, 105, 110, 47,
    116, 121, 112, 101, 115] := by decide
  rw [ep, eg, eh, et]
  rw [ep, eg, eh, et] at *
  generalize unversioned (bs s.val) = n
  by_cases h1 : n = [99, 108, 101, 119, 58, 112, 108, 117, 103, 105, 110, 47,
    112, 114, 111, 106, 101, 99, 116]
  · exact ⟨clew_rules.plugin.READ, by simp [h1], by simp [h1, hR, Clew.Plugin.READ]⟩
  by_cases h2 : n = [99, 108, 101, 119, 58, 112, 108, 117, 103, 105, 110, 47, 103, 105, 116]
  · exact ⟨clew_rules.plugin.GIT, by simp [h2], by simp [h2, hG, Clew.Plugin.GIT]⟩
  by_cases h3 : n = [99, 108, 101, 119, 58, 112, 108, 117, 103, 105, 110, 47,
    104, 111, 115, 116]
  · exact ⟨0#u8, by simp [h3], by simp [h3]⟩
  by_cases h4 : n = [99, 108, 101, 119, 58, 112, 108, 117, 103, 105, 110, 47,
    116, 121, 112, 101, 115]
  · exact ⟨0#u8, by simp [h4], by simp [h4]⟩
  cases h5 : wasiLinked n
  · exact ⟨clew_rules.plugin.UNKNOWN, by simp [h1, h2, h3, h4, h5],
      by simp [h1, h2, h3, h4, h5, hU, Clew.Plugin.UNKNOWN]⟩
  · exact ⟨0#u8, by simp [h1, h2, h3, h4, h5], by simp [h1, h2, h3, h4, h5]⟩

/-! ## The code that runs is the model, and what that means

Each function the app calls returns, and returns what the model computes;
composed with the model's theorems (`../Clew/Plugin.lean`), what those
theorems promise holds of the code. -/

theorem need_of_is_the_model (s : Slice Std.U8) :
    ∃ v, need_of s = ok v ∧ v.val = needOf (bs s.val) := need_of_ok s

theorem may_link_is_the_model (n g : Std.U8) : may_link n g = ok (mayLink n.val g.val) :=
  may_link_ok n g

theorem id_ok_is_the_model (s : Slice Std.U8) (min : Std.Usize) :
    id_ok s min = ok (idOk (bs s.val) min.val) := id_ok_ok s min

theorem nonce_ok_is_the_model (s : Slice Std.U8) : nonce_ok s = ok (nonceOk (bs s.val)) :=
  nonce_ok_ok s

theorem source_kind_is_the_model (s : Slice Std.U8) :
    ∃ v, source_kind s = ok v ∧ v.val = sourceKind (bs s.val) := source_kind_ok s

theorem key_name_is_the_model (key : Slice Std.U8) :
    ∃ v, key_name key = ok v ∧ bs v.val = keyName (bs key.val) := key_name_ok key

/-- `needs` in the crate: `with_need` of each import in turn, from none. -/
def foldNeeds : List (Slice Std.U8) → Std.U8 → Result Std.U8
  | [], a => ok a
  | i :: is, a => do
    let a' ← with_need a i
    foldNeeds is a'

theorem foldNeeds_ok (imps : List (Slice Std.U8)) (a : Std.U8) :
    ∃ v, foldNeeds imps a = ok v ∧
      v.val = (imps.map (fun i => bs i.val)).foldl (fun all i => all ||| needOf i) a.val := by
  induction imps generalizing a with
  | nil => exact ⟨a, rfl, rfl⟩
  | cons i is ih =>
    obtain ⟨n, hn, hnv⟩ := need_of_ok i
    obtain ⟨a', ha', ha'v⟩ := with_need_ok a i n hn
    obtain ⟨v, hv, hvv⟩ := ih a'
    refine ⟨v, by simp [foldNeeds, ha', hv], ?_⟩
    rw [hvv, ha'v, hnv]; rfl

/-- **The code links a plugin only with what it was granted.** Whatever a
component imports and whatever was granted: when the crate folds what the
imports need (`with_need` from none) and `may_link` says yes, every import
is one Clew provides, one that reads the project was granted *read*, and one
that reads its history was granted *git*. -/
theorem code_links_only_what_was_granted (imps : List (Slice Std.U8)) (g n : Std.U8)
    (hn : foldNeeds imps 0#u8 = ok n) (hl : may_link n g = ok true) :
    ∀ i ∈ imps, needOf (bs i.val) ≠ Clew.Plugin.UNKNOWN ∧
      (needOf (bs i.val) = Clew.Plugin.READ → g.val &&& Clew.Plugin.READ = Clew.Plugin.READ) ∧
      (needOf (bs i.val) = Clew.Plugin.GIT → g.val &&& Clew.Plugin.GIT = Clew.Plugin.GIT) := by
  obtain ⟨v, hv, hvv⟩ := foldNeeds_ok imps 0#u8
  rw [hn] at hv; simp only [Result.ok.injEq] at hv; subst hv
  rw [may_link_ok] at hl
  have hlink : mayLink (needs (imps.map (fun i => bs i.val))) g.val = true := by
    have e : needs (imps.map (fun i => bs i.val)) = n.val := by rw [hvv]; rfl
    rw [e]; simpa using hl
  intro i hi
  exact (link_iff _ g.val (Framing.u8_lt g)).mp hlink (bs i.val) (List.mem_map_of_mem hi)

/-- **The code's ids are plain names.** -/
theorem code_id_is_plain (s : Slice Std.U8) (min : Std.Usize) (h : id_ok s min = ok true) :
    plainName (bs s.val) := by
  rw [id_ok_ok] at h
  exact id_plain _ _ (by simpa using h)

/-- **The code's nonces are plain names.** -/
theorem code_nonce_is_plain (s : Slice Std.U8) (h : nonce_ok s = ok true) :
    plainName (bs s.val) := by
  rw [nonce_ok_ok] at h
  exact nonce_plain _ (by simpa using h)

/-- **What the code lets an agent write stays in the plugin's folder.** -/
theorem code_source_stays_inside (s : Slice Std.U8) (v : Std.U8) (h : source_kind s = ok v)
    (h2 : v.val = 2) (root : List (List Char)) :
    root <+: Confine.resolve root (Confine.segments (chars (bs s.val))) := by
  obtain ⟨w, hw, hwv⟩ := source_kind_ok s
  rw [h] at hw; simp only [Result.ok.injEq] at hw; subst hw
  exact source_stays_inside _ (by rw [← hwv, h2]) root

/-- **The code's store files are plain names, none shared by two keys.** -/
theorem code_key_files (k1 k2 : Slice Std.U8) (v1 v2 : alloc.vec.Vec Std.U8)
    (h1 : key_name k1 = ok v1) (h2 : key_name k2 = ok v2) (hne : v1.val ≠ []) :
    plainName (bs v1.val) ∧ (bs v1.val = bs v2.val → bs k1.val = bs k2.val) := by
  obtain ⟨w1, hw1, e1⟩ := key_name_ok k1
  obtain ⟨w2, hw2, e2⟩ := key_name_ok k2
  rw [h1] at hw1; simp only [Result.ok.injEq] at hw1; subst hw1
  rw [h2] at hw2; simp only [Result.ok.injEq] at hw2; subst hw2
  have hn : keyName (bs k1.val) ≠ [] := by
    rw [← e1]; simpa [bs] using hne
  refine ⟨by rw [e1]; exact keyName_plain _ hn, fun he => ?_⟩
  rw [e1, e2] at he
  exact keyName_injective _ _ (fun x hx => by
      obtain ⟨y, _, rfl⟩ := List.mem_map.mp hx; exact Framing.u8_lt y)
    (fun x hx => by obtain ⟨y, _, rfl⟩ := List.mem_map.mp hx; exact Framing.u8_lt y) hn he

end Clew.Extracted.PluginRules
