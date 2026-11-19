import Clew.Text
import Clew.Confine

/-!
# Plugins: what a plugin is linked with, and the names of its things

A plugin is a WebAssembly component the user installs. Two kinds of
decision keep it to what it was given, and both are made in
`crates/clew-rules/src/plugin.rs`, which Charon and Aeneas translate to
Lean (`extracted/ClewRules.lean`); `extracted/Equal.lean` proves that
translation computes exactly what this model computes.

**What it may call.** A plugin's permissions are what its code imports:
`clew:plugin/project` takes *read*, `clew:plugin/git` takes *git*, Clew's
own `host` and `types` and the WASI interfaces Clew links (with nothing
behind them) take none, and anything else is unknown. It is linked only
when nothing it imports is unknown and every permission its imports take
was granted (`mayLink`). Proved here: that is exactly the rule — a plugin
that imports `project` is never linked without *read*, one that imports
`git` never without *git*, and one that imports anything Clew does not
know is never linked at all, whatever was granted.

**The names of its things.** A plugin's id names its folders; a request's
nonce names its files; a plugin's source is written by an agent, file by
file; a plugin's own store keeps each key in a file. Proved here: an id
and a nonce are one plain name (no separator, not `.` or `..`), so joined
to a folder each is exactly one folder below it; a source path is
`plugin.toml` or a Rust file under `src/` that stays under the plugin's
folder, with no hidden piece; and a key's file is a plain name no other key
shares.

Bytes are `Nat`s here, as `bs` makes them of the Rust's `u8`s.
-/

namespace Clew.Plugin

open Clew.Text

/-- The bytes of an ASCII string. -/
def b (s : String) : List Nat := s.toList.map Char.toNat

/-! ## What a plugin may call -/

/-- Read the project's files, outline and search. -/
def READ : Nat := 1
/-- Read the project's git history. -/
def GIT : Nat := 2
/-- An import Clew does not provide: never granted. -/
def UNKNOWN : Nat := 128

/-- An import's name without its version: everything before the first `@`. -/
def unversioned (imp : List Nat) : List Nat := imp.takeWhile (· != 64)

/-- `name` is an interface of the package `pre`: `pre`, `/`, and more. -/
def inPackage (name pre : List Nat) : Bool :=
  pre.isPrefixOf name && decide (pre.length + 1 < name.length) && name[pre.length]? == some 47

/-- The WASI packages Clew links, each with nothing behind it. -/
def wasiPackages : List (List Nat) :=
  [b "wasi:io", b "wasi:cli", b "wasi:clocks", b "wasi:random", b "wasi:filesystem",
    b "wasi:sockets"]

/-- `name` is an interface of a WASI package Clew links. -/
def wasiLinked (name : List Nat) : Bool := wasiPackages.any (inPackage name)

/-- The permission the interface `name` (unversioned) takes. -/
def needOfName (name : List Nat) : Nat :=
  if name == b "clew:plugin/project" then READ
  else if name == b "clew:plugin/git" then GIT
  else if name == b "clew:plugin/host" || name == b "clew:plugin/types" || wasiLinked name then 0
  else UNKNOWN

/-- `need_of`: the permission an import takes. -/
def needOf (imp : List Nat) : Nat := needOfName (unversioned imp)

/-- `needs`: every permission the imports take together. -/
def needs (imps : List (List Nat)) : Nat := imps.foldl (fun all i => all ||| needOf i) 0

/-- `may_link`: nothing unknown is needed, and nothing needed is ungranted
(`!granted` of a `u8` is `255 - granted`). -/
def mayLink (needed granted : Nat) : Bool :=
  needed &&& UNKNOWN == 0 && needed &&& (255 - granted) == 0

/-- What `needOf` can say. -/
theorem needOf_cases (imp : List Nat) :
    needOf imp = 0 ∨ needOf imp = READ ∨ needOf imp = GIT ∨ needOf imp = UNKNOWN := by
  unfold needOf needOfName
  split
  · exact .inr (.inl rfl)
  · split
    · exact .inr (.inr (.inl rfl))
    · split
      · exact .inl rfl
      · exact .inr (.inr (.inr rfl))

/-- The values the permissions of any imports together can take. -/
def together : List Nat := [0, 1, 2, 3, 128, 129, 130, 131]

theorem together_step (a x : Nat) (ha : a ∈ together)
    (hx : x = 0 ∨ x = READ ∨ x = GIT ∨ x = UNKNOWN) : a ||| x ∈ together := by
  simp only [together, List.mem_cons, List.not_mem_nil, or_false] at ha
  simp only [READ, GIT, UNKNOWN] at hx
  rcases ha with rfl | rfl | rfl | rfl | rfl | rfl | rfl | rfl <;>
    rcases hx with rfl | rfl | rfl | rfl <;> decide

/-- One bit of permissions folded together: set when it was set already or
when an import adds it. -/
theorem bit_step (a x k : Nat) (ha : a ∈ together)
    (hx : x = 0 ∨ x = READ ∨ x = GIT ∨ x = UNKNOWN) (hk : k = 1 ∨ k = 2 ∨ k = 128) :
    ((a ||| x) &&& k == k) = ((a &&& k == k) || x == k) := by
  simp only [together, List.mem_cons, List.not_mem_nil, or_false] at ha
  simp only [READ, GIT, UNKNOWN] at hx
  rcases ha with rfl | rfl | rfl | rfl | rfl | rfl | rfl | rfl <;>
    rcases hx with rfl | rfl | rfl | rfl <;> rcases hk with rfl | rfl | rfl <;> decide

theorem fold_together (imps : List (List Nat)) (a : Nat) (ha : a ∈ together) :
    imps.foldl (fun all i => all ||| needOf i) a ∈ together := by
  induction imps generalizing a with
  | nil => exact ha
  | cons i is ih => exact ih _ (together_step a _ ha (needOf_cases i))

theorem fold_bit (imps : List (List Nat)) (a k : Nat) (ha : a ∈ together)
    (hk : k = 1 ∨ k = 2 ∨ k = 128) :
    (imps.foldl (fun all i => all ||| needOf i) a &&& k == k) =
      ((a &&& k == k) || imps.any (fun i => needOf i == k)) := by
  induction imps generalizing a with
  | nil => simp
  | cons i is ih =>
    simp only [List.foldl_cons, List.any_cons]
    rw [ih _ (together_step a _ ha (needOf_cases i)), bit_step a _ k ha (needOf_cases i) hk,
      Bool.or_assoc]

theorem needs_together (imps : List (List Nat)) : needs imps ∈ together :=
  fold_together imps 0 (by decide)

/-- A permission is among those the imports take together exactly when one
of them takes it. -/
theorem needs_bit (imps : List (List Nat)) (k : Nat) (hk : k = 1 ∨ k = 2 ∨ k = 128) :
    (needs imps &&& k == k) = imps.any (fun i => needOf i == k) := by
  unfold needs
  rw [fold_bit imps 0 k (by decide) hk]
  rcases hk with rfl | rfl | rfl <;> simp

/-- `mayLink` on what imports can take together, read bit by bit. -/
theorem mayLink_bits (n g : Nat) (hn : n ∈ together) (hg : g < 256) :
    mayLink n g = (!(n &&& 128 == 128) && (!(n &&& 1 == 1) || g &&& 1 == 1) &&
      (!(n &&& 2 == 2) || g &&& 2 == 2)) := by
  simp only [together, List.mem_cons, List.not_mem_nil, or_false] at hn
  unfold mayLink UNKNOWN
  revert g
  rcases hn with rfl | rfl | rfl | rfl | rfl | rfl | rfl | rfl <;> decide +kernel

/-- **The linking rule.** Imports are linked with what was granted (a `u8`)
exactly when none of them is unknown and every permission one of them
takes was granted. -/
theorem link_iff (imps : List (List Nat)) (g : Nat) (hg : g < 256) :
    mayLink (needs imps) g = true ↔
      ∀ i ∈ imps, needOf i ≠ UNKNOWN ∧ (needOf i = READ → g &&& READ = READ) ∧
        (needOf i = GIT → g &&& GIT = GIT) := by
  rw [mayLink_bits _ g (needs_together imps) hg, needs_bit imps 128 (by simp),
    needs_bit imps 1 (by simp), needs_bit imps 2 (by simp)]
  simp only [Bool.and_eq_true, Bool.not_eq_true', Bool.or_eq_true, beq_iff_eq]
  constructor
  · rintro ⟨⟨h128, h1⟩, h2⟩ i hi
    refine ⟨fun e => ?_, fun e => ?_, fun e => ?_⟩
    · have : imps.any (fun i => needOf i == 128) = true :=
        List.any_eq_true.mpr ⟨i, hi, by simp [e, UNKNOWN]⟩
      rw [this] at h128; cases h128
    · rcases h1 with h1 | h1
      · have : imps.any (fun i => needOf i == 1) = true :=
          List.any_eq_true.mpr ⟨i, hi, by simp [e, READ]⟩
        rw [this] at h1; cases h1
      · exact h1
    · rcases h2 with h2 | h2
      · have : imps.any (fun i => needOf i == 2) = true :=
          List.any_eq_true.mpr ⟨i, hi, by simp [e, GIT]⟩
        rw [this] at h2; cases h2
      · exact h2
  · intro h
    refine ⟨⟨?_, ?_⟩, ?_⟩
    · rw [List.any_eq_false]
      intro i hi e
      exact (h i hi).1 (by simpa [UNKNOWN] using e)
    · by_cases ex : imps.any (fun i => needOf i == 1) = true
      · obtain ⟨i, hi, e⟩ := List.any_eq_true.mp ex
        exact .inr ((h i hi).2.1 (by simpa [READ] using e))
      · exact .inl (by simpa using ex)
    · by_cases ex : imps.any (fun i => needOf i == 2) = true
      · obtain ⟨i, hi, e⟩ := List.any_eq_true.mp ex
        exact .inr ((h i hi).2.2 (by simpa [GIT] using e))
      · exact .inl (by simpa using ex)

/-- **Reading the project needs *read*.** A plugin that imports
`clew:plugin/project` is never linked unless *read* was granted. -/
theorem project_needs_read (imps : List (List Nat)) (i : List Nat) (hi : i ∈ imps)
    (hp : unversioned i = b "clew:plugin/project") (g : Nat) (hg : g < 256)
    (hl : mayLink (needs imps) g = true) : g &&& READ = READ :=
  ((link_iff imps g hg).mp hl i hi).2.1 (by unfold needOf; rw [hp]; rfl)

/-- **Reading the history needs *git*.** A plugin that imports
`clew:plugin/git` is never linked unless *git* was granted. -/
theorem git_needs_git (imps : List (List Nat)) (i : List Nat) (hi : i ∈ imps)
    (hp : unversioned i = b "clew:plugin/git") (g : Nat) (hg : g < 256)
    (hl : mayLink (needs imps) g = true) : g &&& GIT = GIT := by
  have hn : needOf i = GIT := by
    unfold needOf; rw [hp]; rfl
  exact ((link_iff imps g hg).mp hl i hi).2.2 hn

/-- **The unknown is never linked.** A plugin that imports anything Clew
does not provide is refused whatever was granted. -/
theorem unknown_never_links (imps : List (List Nat)) (i : List Nat) (hi : i ∈ imps)
    (hu : needOf i = UNKNOWN) (g : Nat) : mayLink (needs imps) g = false := by
  have h128 : (needs imps &&& 128 == 128) = true := by
    rw [needs_bit imps 128 (by simp)]
    exact List.any_eq_true.mpr ⟨i, hi, by simp [hu, UNKNOWN]⟩
  unfold mayLink UNKNOWN
  have : needs imps &&& 128 ≠ 0 := by
    intro e; rw [e] at h128; simp at h128
  simp [this]

/-- What is known: Clew's three interfaces and the WASI packages it links.
Every other name is unknown. -/
theorem known_iff (imp : List Nat) :
    needOf imp ≠ UNKNOWN ↔
      (unversioned imp = b "clew:plugin/project" ∨ unversioned imp = b "clew:plugin/git" ∨
        unversioned imp = b "clew:plugin/host" ∨ unversioned imp = b "clew:plugin/types" ∨
        wasiLinked (unversioned imp) = true) := by
  unfold needOf needOfName
  generalize unversioned imp = name
  by_cases h1 : name = b "clew:plugin/project"
  · rw [ite_eq_left_of_eq_true _ _ (eq_true (by simp [h1]))]
    exact ⟨fun _ => .inl h1, fun _ => by simp [READ, UNKNOWN]⟩
  by_cases h2 : name = b "clew:plugin/git"
  · rw [ite_eq_right_of_eq_false _ _ (eq_false (by simpa using h1)),
      ite_eq_left_of_eq_true _ _ (eq_true (by simp [h2]))]
    exact ⟨fun _ => .inr (.inl h2), fun _ => by simp [GIT, UNKNOWN]⟩
  rw [ite_eq_right_of_eq_false _ _ (eq_false (by simpa using h1)),
    ite_eq_right_of_eq_false _ _ (eq_false (by simpa using h2))]
  by_cases h3 : (name == b "clew:plugin/host" || name == b "clew:plugin/types" ||
      wasiLinked name) = true
  · rw [ite_eq_left_of_eq_true _ _ (eq_true h3)]
    simp only [Bool.or_eq_true, beq_iff_eq] at h3
    refine ⟨fun _ => ?_, fun _ => by simp [UNKNOWN]⟩
    rcases h3 with (h | h) | h
    · exact .inr (.inr (.inl h))
    · exact .inr (.inr (.inr (.inl h)))
    · exact .inr (.inr (.inr (.inr h)))
  · rw [ite_eq_right_of_eq_false _ _ (eq_false h3)]
    simp only [Bool.or_eq_true, beq_iff_eq, not_or] at h3
    refine ⟨fun h => absurd rfl h, ?_⟩
    rintro (h | h | h | h | h)
    · exact absurd h h1
    · exact absurd h h2
    · exact absurd h h3.1.1
    · exact absurd h h3.1.2
    · exact absurd h h3.2

/-! ## Plain names

A name joined to a folder is exactly one folder below it when it holds no
separator (`/`, or `\` and `:` on Windows), no NUL, and is not `.` or `..`. -/

/-- One plain name. -/
def plainName (n : List Nat) : Prop :=
  n ≠ [] ∧ n ≠ [46] ∧ n ≠ [46, 46] ∧ ∀ x ∈ n, x ≠ 47 ∧ x ≠ 92 ∧ x ≠ 58 ∧ x ≠ 0

/-- Bytes as the characters a path is read as. -/
def chars (n : List Nat) : List Char := n.map Char.ofNat

theorem ofNat_eq_iff (x : Nat) (c : Char) (hc : c.toNat ≠ 0) :
    Char.ofNat x = c ↔ x = c.toNat := by
  constructor
  · intro h
    by_cases hv : x.isValidChar
    · rw [← h]; simp [Char.ofNat, hv, Char.toNat, Char.ofNatAux]
    · have e : Char.ofNat x = '\x00' := by simp [Char.ofNat, hv]; rfl
      rw [e] at h; subst h; exact absurd (by decide) hc
  · rintro rfl; exact Char.ofNat_toNat c

theorem split_no_sep (sep : Char → Bool) (l : List Char) (h : ∀ c ∈ l, sep c = false) :
    split sep l = [l] := by
  induction l with
  | nil => rfl
  | cons c cs ih =>
    simp only [split, h c (by simp), Bool.false_eq_true, ↓reduceIte]
    rw [ih (fun d hd => h d (by simp [hd]))]

/-- **A plain name is one folder down.** Joined to any folder, it resolves to
that folder and the name, nothing else. -/
theorem plain_is_one_down (n : List Nat) (h : plainName n) (root : List (List Char)) :
    Confine.resolve root (Confine.segments (chars n)) = root ++ [chars n] := by
  obtain ⟨hne, hdot, hdd, hx⟩ := h
  have hs : Confine.segments (chars n) = [chars n] := by
    unfold Confine.segments
    apply split_no_sep
    intro c hc
    obtain ⟨x, hxn, rfl⟩ := List.mem_map.mp hc
    have : Char.ofNat x ≠ '/' := fun e => (hx x hxn).1 ((ofNat_eq_iff x '/' (by decide)).mp e)
    simpa using this
  have h1 : (chars n == []) = false := by simpa [chars] using hne
  have h2 : (chars n == ['.']) = false := by
    rw [beq_eq_false_iff_ne]; intro e
    rcases n with _ | ⟨x, _ | ⟨y, t⟩⟩
    · simp [chars] at e
    · simp only [chars, List.map_cons, List.map_nil, List.cons.injEq, and_true] at e
      exact hdot (by rw [(ofNat_eq_iff x '.' (by decide)).mp e]; rfl)
    · simp [chars] at e
  have h3 : (chars n == ['.', '.']) = false := by
    rw [beq_eq_false_iff_ne]; intro e
    rcases n with _ | ⟨x, _ | ⟨y, _ | ⟨z, t⟩⟩⟩
    · simp [chars] at e
    · simp [chars] at e
    · simp only [chars, List.map_cons, List.map_nil, List.cons.injEq, and_true] at e
      exact hdd (by rw [(ofNat_eq_iff x '.' (by decide)).mp e.1,
        (ofNat_eq_iff y '.' (by decide)).mp e.2]; rfl)
    · simp [chars] at e
  rw [hs]
  simp [Confine.resolve, h1, h2, h3]

/-! ## Ids -/

/-- `a`-`z`. -/
def lowerB (n : Nat) : Bool := 97 ≤ n && n ≤ 122
/-- `0`-`9`. -/
def digitB (n : Nat) : Bool := 48 ≤ n && n ≤ 57
/-- A byte an id may hold. -/
def idByte (n : Nat) : Bool := lowerB n || digitB n || n == 45

/-- No `-` last and no `--`. -/
def dashesOk : List Nat → Bool
  | [] => true
  | [a] => a != 45
  | a :: c :: t => !(a == 45 && c == 45) && dashesOk (c :: t)

/-- `id_ok`: `min` to 40 bytes of `a`-`z`, `0`-`9`, `-`, starting with a
letter, not ending with `-`, no `--`. -/
def idOk (id : List Nat) (min : Nat) : Bool :=
  match id with
  | [] => false
  | c :: _ => decide (min ≤ id.length) && decide (id.length ≤ 40) && lowerB c &&
      id.all idByte && dashesOk id

/-- **An id is a plain name.** -/
theorem id_plain (id : List Nat) (min : Nat) (h : idOk id min = true) : plainName id := by
  match id, h with
  | c :: t, h =>
    simp only [idOk, Bool.and_eq_true, decide_eq_true_eq, List.all_eq_true] at h
    obtain ⟨⟨⟨_, hc⟩, hall⟩, _⟩ := h
    have hb : ∀ x ∈ c :: t, x ≠ 47 ∧ x ≠ 92 ∧ x ≠ 58 ∧ x ≠ 0 ∧ x ≠ 46 := by
      intro x hx
      have := hall x hx
      simp only [idByte, lowerB, digitB, Bool.or_eq_true, Bool.and_eq_true,
        decide_eq_true_eq, beq_iff_eq] at this
      omega
    refine ⟨by simp, ?_, ?_, fun x hx => ⟨(hb x hx).1, (hb x hx).2.1, (hb x hx).2.2.1,
      (hb x hx).2.2.2.1⟩⟩
    · intro e; cases e; exact (hb 46 (by simp)).2.2.2.2 rfl
    · intro e; cases e; exact (hb 46 (by simp)).2.2.2.2 rfl

/-! ## Nonces -/

/-- `0`-`9`, `a`-`f`. -/
def lowerHex (n : Nat) : Bool := digitB n || (97 ≤ n && n ≤ 102)

/-- `nonce_ok`: 24 lowercase hex digits. -/
def nonceOk (n : List Nat) : Bool := n.length == 24 && n.all lowerHex

/-- **A nonce is a plain name.** -/
theorem nonce_plain (n : List Nat) (h : nonceOk n = true) : plainName n := by
  simp only [nonceOk, Bool.and_eq_true, beq_iff_eq, List.all_eq_true] at h
  obtain ⟨hl, hall⟩ := h
  refine ⟨fun e => by simp [e] at hl, fun e => by simp [e] at hl,
    fun e => by simp [e] at hl, fun x hx => ?_⟩
  have := hall x hx
  simp only [lowerHex, digitB, Bool.or_eq_true, Bool.and_eq_true, decide_eq_true_eq] at this
  omega

/-! ## A plugin's source -/

/-- A byte a source path may hold: ASCII letters and digits, `_`, `-`, `.`,
`/`. -/
def pathByte (n : Nat) : Bool :=
  lowerB n || digitB n || (65 ≤ n && n ≤ 90) || n == 95 || n == 45 || n == 46 || n == 47

/-- Each `/` is followed by a byte that is neither `/` nor `.`. -/
def slashesOk : List Nat → Bool
  | [] => true
  | [a] => a != 47
  | a :: c :: t => (a != 47 || (c != 47 && c != 46)) && slashesOk (c :: t)

/-- `source_kind`: 1 for `plugin.toml`, 2 for a Rust file under `src/`, 0
for anything else. -/
def sourceKind (rel : List Nat) : Nat :=
  if rel == b "plugin.toml" then 1
  else if decide (8 ≤ rel.length) && (b "src/").isPrefixOf rel &&
      rel[rel.length - 3]? == some 46 && rel[rel.length - 2]? == some 114 &&
      rel[rel.length - 1]? == some 115 && rel.all pathByte && slashesOk rel then 2
  else 0

/-- The manifest is `plugin.toml` and nothing else. -/
theorem manifest_iff (rel : List Nat) : sourceKind rel = 1 ↔ rel = b "plugin.toml" := by
  unfold sourceKind
  by_cases h : rel = b "plugin.toml"
  · simp [h]
  · simp only [beq_iff_eq, h, ↓reduceIte, iff_false]
    split <;> simp

/-- Each `/` followed by neither `/` nor `.`, over characters. -/
def sepsOk : List Char → Prop
  | [] => True
  | [a] => a ≠ '/'
  | a :: c :: t => (a = '/' → c ≠ '/' ∧ c ≠ '.') ∧ sepsOk (c :: t)

theorem sepsOk_tail (a : Char) (cs : List Char) (h : sepsOk (a :: cs)) : sepsOk cs := by
  match cs, h with
  | [], _ => trivial
  | _ :: _, h => exact h.2

theorem split_ne_nil (sep : Char → Bool) (l : List Char) : split sep l ≠ [] := by
  cases l with
  | nil => simp [split]
  | cons c cs =>
    unfold split
    split
    · simp
    · split <;> simp

/-- A piece after the first begins after a separator: when no separator is
followed by another or by `.`, none of those pieces is empty or starts with
`.`. -/
theorem split_rest (l : List Char) (h : sepsOk l) :
    ∀ s ∈ (split (· == '/') l).tail, s ≠ [] ∧ s.head? ≠ some '.' := by
  induction l with
  | nil => simp [split]
  | cons a cs ih =>
    by_cases ha : a = '/'
    · subst ha
      simp only [split, beq_self_eq_true, ↓reduceIte, List.tail_cons]
      cases cs with
      | nil => exact absurd rfl h
      | cons c t =>
        have h' : ('/' = '/' → c ≠ '/' ∧ c ≠ '.') ∧ sepsOk (c :: t) := h
        obtain ⟨hc1, hc2⟩ := h'.1 rfl
        have hcs : (c == '/') = false := by simpa using hc1
        have hhead : ∀ s0 ss, split (· == '/') (c :: t) = s0 :: ss → s0.head? = some c := by
          intro s0 ss e
          simp only [split, hcs, Bool.false_eq_true, ↓reduceIte] at e
          revert e
          cases split (· == '/') t with
          | nil => intro e; simp only [List.cons.injEq] at e; rw [← e.1]; rfl
          | cons p ps => intro e; simp only [List.cons.injEq] at e; rw [← e.1]; rfl
        obtain ⟨s0, ss, hss⟩ := List.exists_cons_of_ne_nil (split_ne_nil (· == '/') (c :: t))
        have hrest := ih h'.2
        rw [hss] at hrest ⊢
        intro s hs
        rcases List.mem_cons.mp hs with rfl | hs
        · have e := hhead s ss hss
          refine ⟨(fun n => by rw [n] at e; cases e), ?_⟩
          rw [e]; simpa using hc2
        · exact hrest s (by simpa using hs)
    · have hab : (a == '/') = false := by simpa using ha
      have hrest := ih (sepsOk_tail a cs h)
      simp only [split, hab, Bool.false_eq_true, ↓reduceIte]
      revert hrest
      cases split (· == '/') cs with
      | nil => intro _ s hs; simp at hs
      | cons p ps => intro hps s hs; exact hps s (by simpa using hs)

theorem slashesOk_sepsOk (rel : List Nat) (h : slashesOk rel = true) : sepsOk (chars rel) := by
  induction rel with
  | nil => trivial
  | cons a t ih =>
    have sl : ∀ x, Char.ofNat x = '/' ↔ x = 47 := fun x => ofNat_eq_iff x '/' (by decide)
    have dt : ∀ x, Char.ofNat x = '.' ↔ x = 46 := fun x => ofNat_eq_iff x '.' (by decide)
    match t, h, ih with
    | [], h, _ =>
      simp only [slashesOk, bne_iff_ne, ne_eq] at h
      simp only [chars, List.map_cons, List.map_nil, sepsOk]
      rw [ne_eq, sl]; exact h
    | c :: t, h, ih =>
      simp only [slashesOk, Bool.and_eq_true, Bool.or_eq_true, bne_iff_ne, ne_eq] at h
      simp only [chars, List.map_cons, sepsOk] at ih ⊢
      refine ⟨fun e => ?_, ih h.2⟩
      rw [sl] at e
      rcases h.1 with h1 | h1
      · exact absurd e h1
      · rw [ne_eq, sl, ne_eq, dt]; exact h1

/-- A path that does not start with white space is not empty once trimmed. -/
theorem trim_ne_nil (c : Char) (cs : List Char) (hc : isWhitespace c = false) :
    trim (c :: cs) ≠ [] := by
  simp only [trim, trimStart, trimEnd, List.dropWhile_cons, hc, Bool.false_eq_true, ↓reduceIte]
  intro h
  rw [List.reverse_eq_nil_iff] at h
  have hx : c ∈ (List.reverse (c :: cs)).dropWhile isWhitespace := by
    rw [List.reverse_cons]
    induction cs.reverse with
    | nil => simp [hc]
    | cons d ds ih =>
      simp only [List.cons_append, List.dropWhile_cons]
      split
      · exact ih
      · simp
  rw [h] at hx; simp at hx

/-- **A source path is a Rust file under `src/`, in the plugin's folder.**
What `source_kind` calls a Rust file starts with `src/` and ends with `.rs`,
every piece of it after `src` is a name that is not empty and does not start
with `.` (so no `.`, no `..`, no hidden file), and it passes `check_lexical`:
joined to the plugin's folder it never leaves it (`Confine.accepted_stays_inside`). -/
theorem source_rust (rel : List Nat) (h : sourceKind rel = 2) :
    (b "src/").isPrefixOf rel = true ∧ rel[rel.length - 3]? = some 46 ∧
      rel[rel.length - 2]? = some 114 ∧ rel[rel.length - 1]? = some 115 ∧
      Confine.segments (chars rel) =
        ['s', 'r', 'c'] :: (Confine.segments (chars rel)).tail ∧
      (∀ s ∈ (Confine.segments (chars rel)).tail, s ≠ [] ∧ s.head? ≠ some '.') ∧
      Confine.checkLexical (chars rel) = .ok () := by
  unfold sourceKind at h
  split at h
  · cases h
  split at h
  · rename_i hc
    simp only [Bool.and_eq_true, decide_eq_true_eq, beq_iff_eq] at hc
    obtain ⟨⟨⟨⟨⟨⟨_, hp⟩, h3⟩, h2⟩, h1⟩, _⟩, hsl⟩ := hc
    have hseps := slashesOk_sepsOk rel hsl
    -- `src/` begins the path: its first piece is `src`.
    obtain ⟨rest, hrest⟩ : ∃ rest, rel = 115 :: 114 :: 99 :: 47 :: rest := by
      match rel, hp with
      | 115 :: 114 :: 99 :: 47 :: rest, _ => exact ⟨rest, rfl⟩
    have hsplit : Confine.segments (chars rel) =
        ['s', 'r', 'c'] :: split (· == '/') (chars rest) := by
      rw [hrest]; rfl
    have htail : ∀ s ∈ (Confine.segments (chars rel)).tail, s ≠ [] ∧ s.head? ≠ some '.' :=
      split_rest (chars rel) hseps
    refine ⟨hp, h3, h2, h1, by rw [hsplit]; rfl, htail, ?_⟩
    rw [Confine.checkLexical_spec]
    refine ⟨?_, ?_, ?_⟩
    · rw [hrest]; exact trim_ne_nil _ _ (by decide)
    · rw [hrest]; simp only [chars, List.map_cons, List.head?_cons]; decide
    · intro s hs e
      rw [hsplit] at hs htail
      simp only [List.mem_cons] at hs
      rcases hs with rfl | hs
      · cases e
      · have := (htail s hs).2
        rw [e] at this; exact this rfl
  · cases h

/-- **Confinement of an agent's writes.** A Rust file `source_kind` accepts,
joined to the plugin's folder and resolved, stays under that folder. -/
theorem source_stays_inside (rel : List Nat) (h : sourceKind rel = 2)
    (root : List (List Char)) : root <+: Confine.resolve root (Confine.segments (chars rel)) :=
  Confine.accepted_stays_inside _ (source_rust rel h).2.2.2.2.2.2 root

/-! ## The files of a plugin's store -/

/-- The hex digit of `n` (below 16), lowercase. -/
def hexd (n : Nat) : Nat := if n < 10 then 48 + n else 97 + (n - 10)

/-- A byte as its two hex digits. -/
def hexPair (x : Nat) : List Nat := [hexd (x / 16), hexd (x % 16)]

/-- `key_name`: the key's bytes in hex, then `.bin`; empty for a key that is
empty or longer than 128 bytes. -/
def keyName (key : List Nat) : List Nat :=
  if key.isEmpty || decide (128 < key.length) then [] else key.flatMap hexPair ++ b ".bin"

theorem hexd_inj (m n : Nat) (hm : m < 16) (hn : n < 16) (h : hexd m = hexd n) : m = n := by
  unfold hexd at h
  split at h <;> split at h <;> omega

theorem hexPair_inj (x y : Nat) (hx : x < 256) (hy : y < 256) (h : hexPair x = hexPair y) :
    x = y := by
  simp only [hexPair, List.cons.injEq, and_true] at h
  have e1 := hexd_inj _ _ (by omega) (by omega) h.1
  have e2 := hexd_inj _ _ (by omega) (by omega) h.2
  omega

theorem flatMap_hexPair_inj (k1 k2 : List Nat) (h1 : ∀ x ∈ k1, x < 256)
    (h2 : ∀ x ∈ k2, x < 256) (h : k1.flatMap hexPair = k2.flatMap hexPair) : k1 = k2 := by
  induction k1 generalizing k2 with
  | nil =>
    cases k2 with
    | nil => rfl
    | cons y ys => simp [hexPair] at h
  | cons x xs ih =>
    cases k2 with
    | nil => simp [hexPair] at h
    | cons y ys =>
      simp only [List.flatMap_cons, hexPair, List.cons_append, List.nil_append,
        List.cons.injEq] at h
      have hxy : x = y := hexPair_inj x y (h1 x (by simp)) (h2 y (by simp))
        (by simp [hexPair, h.1, h.2.1])
      rw [hxy, ih ys (fun z hz => h1 z (by simp [hz])) (fun z hz => h2 z (by simp [hz])) h.2.2]

/-- **No two keys share a file.** Two keys of a plugin's store that have
files have the same file only when they are the same key. -/
theorem keyName_injective (k1 k2 : List Nat) (h1 : ∀ x ∈ k1, x < 256)
    (h2 : ∀ x ∈ k2, x < 256) (n1 : keyName k1 ≠ []) (h : keyName k1 = keyName k2) :
    k1 = k2 := by
  have n2 : keyName k2 ≠ [] := h ▸ n1
  unfold keyName at h n1 n2
  split at n1
  · exact absurd rfl n1
  rename_i c1
  split at n2
  · exact absurd rfl n2
  rename_i c2
  rw [ite_eq_right_of_eq_false _ _ (eq_false c1), ite_eq_right_of_eq_false _ _ (eq_false c2)] at h
  exact flatMap_hexPair_inj k1 k2 h1 h2 (List.append_cancel_right h)

theorem hexd_safe (n : Nat) : hexd n ≠ 47 ∧ hexd n ≠ 92 ∧ hexd n ≠ 58 ∧ hexd n ≠ 0 ∧
    hexd n ≠ 46 := by
  unfold hexd; split <;> omega

/-- **A key's file is a plain name.** -/
theorem keyName_plain (key : List Nat) (h : keyName key ≠ []) : plainName (keyName key) := by
  unfold keyName at h ⊢
  split at h
  · exact absurd rfl h
  rename_i hk
  rw [ite_eq_right_of_eq_false _ _ (eq_false hk)]
  have hne : key ≠ [] := by intro e; simp [e] at hk
  have hlen : 2 ≤ (key.flatMap hexPair).length := by
    obtain ⟨x, xs, rfl⟩ := List.exists_cons_of_ne_nil hne
    simp [hexPair]
  refine ⟨by simp [b], fun e => ?_, fun e => ?_, fun x hx => ?_⟩
  · have := congrArg List.length e; simp [b] at this
  · have := congrArg List.length e; simp [b] at this
  · simp only [List.mem_append, List.mem_flatMap] at hx
    rcases hx with ⟨y, _, hy⟩ | hx
    · simp only [hexPair, List.mem_cons, List.not_mem_nil, or_false] at hy
      rcases hy with rfl | rfl <;> exact ⟨(hexd_safe _).1, (hexd_safe _).2.1,
        (hexd_safe _).2.2.1, (hexd_safe _).2.2.2.1⟩
    · simp only [b] at hx
      revert x; decide

end Clew.Plugin
