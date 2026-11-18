/-!
# Text: the Rust `str` operations the models are built from

Each model works on a `List Char` — a Rust `&str` as its `chars()` — or on a
`List UInt8` where the Rust code works on bytes. The functions here are the
`str` methods the verified Rust code calls, written down with the semantics
the Rust standard library documents for them, so a model reads like the code
it models. Every one of them is exercised against the real Rust method by
the conformance vectors (see `Vectors.lean`), so a mistake here shows up
as a failing `cargo test`, not as a silently wrong proof.
-/

namespace Clew.Text

/-- `char::is_whitespace`: the Unicode `White_Space` property, all 25 code
points of it (Unicode 3.2 onward; the set has not changed since). -/
def isWhitespace (c : Char) : Bool :=
  let n := c.toNat
  (0x09 ≤ n && n ≤ 0x0D) || n == 0x20 || n == 0x85 || n == 0xA0 || n == 0x1680
    || (0x2000 ≤ n && n ≤ 0x200A) || n == 0x2028 || n == 0x2029 || n == 0x202F
    || n == 0x205F || n == 0x3000

/-- `str::trim_start`. -/
def trimStart (s : List Char) : List Char := s.dropWhile isWhitespace

/-- `str::trim_end`. -/
def trimEnd (s : List Char) : List Char := (s.reverse.dropWhile isWhitespace).reverse

/-- `str::trim`. -/
def trim (s : List Char) : List Char := trimEnd (trimStart s)

/-- `str::split(pattern)` for a character predicate: `n` separators give
`n + 1` pieces, empty ones included. -/
def split (sep : Char → Bool) : List Char → List (List Char)
  | [] => [[]]
  | c :: cs =>
    if sep c then [] :: split sep cs
    else match split sep cs with
      | [] => [[c]]
      | p :: ps => (c :: p) :: ps

/-- `str::split_whitespace`: the pieces between runs of whitespace, none
empty. -/
def splitWhitespace (s : List Char) : List (List Char) :=
  (split isWhitespace s).filter (fun p => !p.isEmpty)

/-- `str::split_once(c)`: the text before the first `c` and after it. -/
def splitOnce (c : Char) : List Char → Option (List Char × List Char)
  | [] => none
  | d :: ds =>
    if d == c then some ([], ds)
    else (splitOnce c ds).map (fun (a, b) => (d :: a, b))

/-- `char::is_ascii_digit`. -/
def isAsciiDigit (c : Char) : Bool := '0' ≤ c && c ≤ '9'

/-- `char::is_ascii_hexdigit`. -/
def isAsciiHexdigit (c : Char) : Bool :=
  isAsciiDigit c || ('a' ≤ c && c ≤ 'f') || ('A' ≤ c && c ≤ 'F')

/-- `char::is_ascii_alphanumeric`. -/
def isAsciiAlphanumeric (c : Char) : Bool :=
  isAsciiDigit c || ('a' ≤ c && c ≤ 'z') || ('A' ≤ c && c ≤ 'Z')

/-- `char::to_ascii_lowercase`. -/
def toAsciiLowercase (c : Char) : Char :=
  if 'A' ≤ c && c ≤ 'Z' then Char.ofNat (c.toNat + 32) else c

/-- The value of a run of ASCII digits, most significant first. -/
def digitsValue (ds : List Char) : Nat :=
  ds.foldl (fun acc d => acc * 10 + (d.toNat - '0'.toNat)) 0

/-- `u64::from_str`: an optional `+`, then one or more ASCII digits, and a
value that fits in 64 bits. -/
def parseU64 (s : List Char) : Option Nat :=
  let ds := match s with
    | '+' :: rest => rest
    | _ => s
  if ds.isEmpty || !ds.all isAsciiDigit then none
  else
    let v := digitsValue ds
    if v < 2 ^ 64 then some v else none

/-- The ASCII digit for `d < 10`. -/
def digitChar (d : Nat) : Char := Char.ofNat ('0'.toNat + d)

/-- A natural number in decimal, as `format!("{}")` writes it: most
significant digit first, no sign, no leading zero. -/
def decimal (n : Nat) : List Char :=
  if n < 10 then [digitChar n] else decimal (n / 10) ++ [digitChar (n % 10)]
termination_by n
decreasing_by omega

/-! ## Lemmas -/

theorem digitChar_isAsciiDigit (d : Nat) (h : d < 10) : isAsciiDigit (digitChar d) = true := by
  match d, h with
  | 0, _ | 1, _ | 2, _ | 3, _ | 4, _ | 5, _ | 6, _ | 7, _ | 8, _ | 9, _ => decide

theorem digitChar_value (d : Nat) (h : d < 10) : (digitChar d).toNat - '0'.toNat = d := by
  match d, h with
  | 0, _ | 1, _ | 2, _ | 3, _ | 4, _ | 5, _ | 6, _ | 7, _ | 8, _ | 9, _ => decide

theorem digitsValue_snoc (ds : List Char) (d : Char) :
    digitsValue (ds ++ [d]) = digitsValue ds * 10 + (d.toNat - '0'.toNat) := by
  simp [digitsValue, List.foldl_append]

theorem decimal_digits (n : Nat) : ∀ c ∈ decimal n, isAsciiDigit c = true := by
  induction n using Nat.strongRecOn with
  | _ n ih =>
    unfold decimal
    split
    · rename_i h; intro c hc; simp at hc; subst hc; exact digitChar_isAsciiDigit n h
    · rename_i h
      intro c hc
      simp only [List.mem_append, List.mem_singleton] at hc
      rcases hc with hc | hc
      · exact ih (n / 10) (by omega) c hc
      · subst hc; exact digitChar_isAsciiDigit _ (Nat.mod_lt _ (by omega))

theorem decimal_ne_nil (n : Nat) : decimal n ≠ [] := by
  unfold decimal; split <;> simp

theorem digitsValue_decimal (n : Nat) : digitsValue (decimal n) = n := by
  induction n using Nat.strongRecOn with
  | _ n ih =>
    unfold decimal
    split
    · rename_i h
      have hv := digitChar_value n h
      simp only [digitsValue, List.foldl_cons, List.foldl_nil, Nat.zero_mul, Nat.zero_add]
      exact hv
    · rename_i h
      rw [digitsValue_snoc, ih (n / 10) (by omega), digitChar_value _ (Nat.mod_lt _ (by omega))]
      omega

/-- `split` of text that holds no separator is that text. -/
theorem split_none (sep : Char → Bool) (xs : List Char) (h : ∀ c ∈ xs, sep c = false) :
    split sep xs = [xs] := by
  induction xs with
  | nil => rfl
  | cons c cs ih =>
    have hc : sep c = false := h c (by simp)
    have hcs : ∀ d ∈ cs, sep d = false := fun d hd => h d (by simp [hd])
    simp [split, hc, ih hcs]

/-- `split` across one separator: the text before it is the first piece. -/
theorem split_at_sep (sep : Char → Bool) (xs : List Char) (d : Char) (ys : List Char)
    (h : ∀ c ∈ xs, sep c = false) (hd : sep d = true) :
    split sep (xs ++ d :: ys) = xs :: split sep ys := by
  induction xs with
  | nil => simp [split, hd]
  | cons c cs ih =>
    have hc : sep c = false := h c (by simp)
    have hcs : ∀ e ∈ cs, sep e = false := fun e he => h e (by simp [he])
    simp [split, hc, ih hcs]

theorem dropWhile_of_head (p : Char → Bool) (xs : List Char)
    (h : ∀ c, xs.head? = some c → p c = false) : xs.dropWhile p = xs := by
  cases xs with
  | nil => rfl
  | cons c cs => simp [List.dropWhile, h c rfl]

/-- Text with no whitespace in it is its own `trim`. -/
theorem trim_of_no_whitespace (xs : List Char) (h : ∀ c ∈ xs, isWhitespace c = false) :
    trim xs = xs := by
  have hs : trimStart xs = xs := by
    apply dropWhile_of_head
    intro c hc
    exact h c (List.mem_of_mem_head? hc)
  have hr : xs.reverse.dropWhile isWhitespace = xs.reverse := by
    apply dropWhile_of_head
    intro c hc
    exact h c (by simpa using List.mem_of_mem_head? hc)
  simp [trim, trimEnd, hs, hr]

theorem digit_not_whitespace (c : Char) (h : isAsciiDigit c = true) : isWhitespace c = false := by
  simp only [isAsciiDigit, Bool.and_eq_true, decide_eq_true_eq] at h
  have h1 : 48 ≤ c.toNat := h.1
  have h2 : c.toNat ≤ 57 := h.2
  simp only [isWhitespace, Bool.or_eq_false_iff, Bool.and_eq_false_iff, beq_eq_false_iff_ne,
    ne_eq, decide_eq_false_iff_not, Nat.not_le]
  omega

/-- The decimal digits of `n < 2⁶⁴` read back as `n` by `u64::from_str`. -/
theorem parseU64_decimal (n : Nat) (h : n < 2 ^ 64) : parseU64 (decimal n) = some n := by
  have hd := decimal_digits n
  have hne := decimal_ne_nil n
  have hplus : ∀ rest, decimal n ≠ '+' :: rest := by
    intro rest he
    have := hd '+' (by rw [he]; simp)
    revert this; decide
  unfold parseU64
  split
  · rename_i rest he; exact absurd he (hplus rest)
  have hall : (decimal n).all isAsciiDigit = true := by
    rw [List.all_eq_true]; exact hd
  have hemp : (decimal n).isEmpty = false := by
    cases he : decimal n with
    | nil => exact absurd he hne
    | cons _ _ => rfl
  simp [hall, hemp, digitsValue_decimal, h]

/-! ## Lists -/

theorem takeWhile_append_all {α : Type} {p : α → Bool} (xs ys : List α)
    (hx : ∀ c ∈ xs, p c = true) (hy : ∀ c, ys.head? = some c → p c = false) :
    (xs ++ ys).takeWhile p = xs := by
  induction xs with
  | nil =>
    cases ys with
    | nil => rfl
    | cons y ys => simp [hy y rfl]
  | cons x xs ih =>
    have : p x = true := hx x (by simp)
    simp [this, ih (fun c hc => hx c (by simp [hc]))]

theorem dropWhile_append_all {α : Type} {p : α → Bool} (xs ys : List α)
    (hx : ∀ c ∈ xs, p c = true) (hy : ∀ c, ys.head? = some c → p c = false) :
    (xs ++ ys).dropWhile p = ys := by
  induction xs with
  | nil =>
    cases ys with
    | nil => rfl
    | cons y ys => simp [hy y rfl]
  | cons x xs ih =>
    have : p x = true := hx x (by simp)
    simp [this, ih (fun c hc => hx c (by simp [hc]))]

end Clew.Text
