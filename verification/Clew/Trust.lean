import Clew.Text

/-!
# Consent that cannot be mistaken for other consent — `clew_core::trust`

A repository's `.clew/lsp.toml` may name a language server to run and
options to send it; Clew runs neither without the user's approval, recorded
outside the project as a fingerprint: a SHA-256 over the command, its
arguments, the server, its version, the options and the command's bytes.
An approval must never be satisfied by anything but what was approved —
not by another argument list, another command, an options-only config, a
"leave to build" record, or the same path on another machine.

SHA-256 is taken to be collision resistant: two different inputs never hash
alike. Everything else is proved here, about the inputs:

* `args_part` is injective: no two argument lists read alike (`[]` and
  `[""]` once did);
* the length-prefixed fields every fingerprint hashes are injective, and
  with the 32-byte content digest after them still are;
* the three kinds of fingerprint — command, options-only, leave-to-build —
  hash disjoint inputs, so an approval of one is never an approval of
  another;
* the key an approval is filed under names one machine and one project:
  `user@host`, the port when it is not 22, and the root never run
  together. (They did, before this model: a host may hold `:`, so
  `u@2001:db8::1:2222` on port 22 and `u@2001:db8::1` on port 2222 shared one
  key. The host is now bracketed when it holds a `:`, and `%`, `/`, `[`,
  `]` and `@` are escaped.)

Byte strings are lists of `Nat`s, each a byte.
-/

namespace Clew.Trust

open Clew.Text

/-! ## Decimal numbers as bytes -/

/-- `n` in decimal, as bytes. -/
def digits (n : Nat) : List Nat := (decimal n).map Char.toNat

def isDigit (b : Nat) : Bool := 48 ≤ b && b ≤ 57

def value (bs : List Nat) : Nat := bs.foldl (fun acc b => acc * 10 + (b - 48)) 0

/-- A number read off the front: the run of digits, and what follows it. -/
def readNum (bs : List Nat) : Nat × List Nat :=
  (value (bs.takeWhile isDigit), bs.dropWhile isDigit)

theorem digits_isDigit (n : Nat) : ∀ b ∈ digits n, isDigit b = true := by
  intro b hb
  simp only [digits, List.mem_map] at hb
  obtain ⟨c, hc, rfl⟩ := hb
  have := decimal_digits n c hc
  simp only [isAsciiDigit, Bool.and_eq_true, decide_eq_true_eq] at this
  simp only [isDigit, Bool.and_eq_true, decide_eq_true_eq]
  exact ⟨this.1, this.2⟩

theorem value_digits (n : Nat) : value (digits n) = n := by
  have h : value (digits n) = digitsValue (decimal n) := by
    simp [value, digits, digitsValue, List.foldl_map]
  rw [h, digitsValue_decimal]

theorem readNum_digits (n : Nat) (rest : List Nat)
    (h : ∀ b, rest.head? = some b → isDigit b = false) : readNum (digits n ++ rest) = (n, rest) := by
  simp [readNum, takeWhile_append_all _ _ (digits_isDigit n) h,
    dropWhile_append_all _ _ (digits_isDigit n) h, value_digits]

/-! ## `args_part` -/

/-- `args_part`: the count, then each argument as `;<byte length>:<bytes>`. -/
def argsPart (args : List (List Nat)) : List Nat :=
  digits args.length ++ args.flatMap (fun a => 59 :: (digits a.length ++ 58 :: a))

/-- Reading `k` arguments back. -/
def decodeArgsN : Nat → List Nat → Option (List (List Nat))
  | 0, [] => some []
  | 0, _ :: _ => none
  | k + 1, 59 :: rest =>
    match readNum rest with
    | (len, 58 :: r) =>
      if len ≤ r.length then (decodeArgsN k (r.drop len)).map (r.take len :: ·) else none
    | _ => none
  | _ + 1, _ => none

def decodeArgs (bs : List Nat) : Option (List (List Nat)) :=
  match readNum bs with
  | (n, rest) => decodeArgsN n rest

theorem decodeArgsN_flatMap (args : List (List Nat)) :
    decodeArgsN args.length (args.flatMap (fun a => 59 :: (digits a.length ++ 58 :: a))) =
      some args := by
  induction args with
  | nil => rfl
  | cons a as ih =>
    simp only [List.length_cons, List.flatMap_cons, List.cons_append, List.append_assoc,
      decodeArgsN]
    rw [readNum_digits a.length _ (by intro b hb; simp at hb; subst hb; decide)]
    simp [ih]

/-- **Arguments are read back exactly as they were given.** -/
theorem decodeArgs_argsPart (args : List (List Nat)) : decodeArgs (argsPart args) = some args := by
  unfold decodeArgs argsPart
  have hrest : ∀ b, (args.flatMap (fun a => 59 :: (digits a.length ++ 58 :: a))).head? = some b →
      isDigit b = false := by
    intro b hb
    cases args with
    | nil => simp at hb
    | cons a as => simp at hb; subst hb; decide
  rw [readNum_digits _ _ hrest]
  exact decodeArgsN_flatMap args

/-- **No two argument lists read alike**: an approval given to one argument
list is never an approval of another. -/
theorem argsPart_injective {a b : List (List Nat)} (h : argsPart a = argsPart b) : a = b := by
  have := decodeArgs_argsPart a
  rw [h, decodeArgs_argsPart b] at this
  exact (Option.some.inj this).symm

/-! ## Length-prefixed fields -/

/-- `k` little-endian bytes of `n` (`to_le_bytes`, for `k = 8`). -/
def leBytes : Nat → Nat → List Nat
  | 0, _ => []
  | k + 1, n => n % 256 :: leBytes k (n / 256)

def fromLE : List Nat → Nat
  | [] => 0
  | b :: bs => b + 256 * fromLE bs

theorem leBytes_length (k n : Nat) : (leBytes k n).length = k := by
  induction k generalizing n with
  | zero => rfl
  | succ k ih => simp [leBytes, ih]

theorem fromLE_leBytes (k n : Nat) : fromLE (leBytes k n) = n % 256 ^ k := by
  induction k generalizing n with
  | zero => simp [leBytes, fromLE, Nat.mod_one]
  | succ k ih =>
    simp only [leBytes, fromLE, ih, Nat.pow_succ, Nat.mul_comm (256 ^ k) 256]
    rw [Nat.mod_mul]

/-- One field as every fingerprint hashes it: its length as 8 little-endian
bytes, then its bytes. -/
def field (p : List Nat) : List Nat := leBytes 8 p.length ++ p

def fields (ps : List (List Nat)) : List Nat := ps.flatMap field

def readField (bs : List Nat) : Option (List Nat × List Nat) :=
  let n := fromLE (bs.take 8)
  let rest := bs.drop 8
  if 8 ≤ bs.length ∧ n ≤ rest.length then some (rest.take n, rest.drop n) else none

def readFields : Nat → List Nat → Option (List (List Nat) × List Nat)
  | 0, bs => some ([], bs)
  | k + 1, bs =>
    match readField bs with
    | some (p, rest) => (readFields k rest).map (fun (ps, r) => (p :: ps, r))
    | none => none

theorem readField_field (p rest : List Nat) (h : p.length < 2 ^ 64) :
    readField (field p ++ rest) = some (p, rest) := by
  have hl := leBytes_length 8 p.length
  have hv : fromLE (leBytes 8 p.length) = p.length := by
    rw [fromLE_leBytes]; exact Nat.mod_eq_of_lt (by simpa using h)
  unfold readField field
  simp only [List.append_assoc]
  rw [List.take_left' hl, List.drop_left' hl, hv]
  simp [hl]

/-- **Fields read back exactly**, whatever follows them. -/
theorem readFields_fields (ps : List (List Nat)) (rest : List Nat)
    (h : ∀ p ∈ ps, p.length < 2 ^ 64) : readFields ps.length (fields ps ++ rest) = some (ps, rest) := by
  induction ps with
  | nil => rfl
  | cons p ps ih =>
    simp only [fields, List.flatMap_cons, List.append_assoc, List.length_cons, readFields]
    rw [readField_field p _ (h p (by simp))]
    simp only
    rw [show ps.flatMap field ++ rest = fields ps ++ rest from rfl,
      ih (fun q hq => h q (by simp [hq]))]
    rfl

/-- **The fields and what follows them are told apart**: equal hash
inputs with as many fields mean equal fields and an equal tail. -/
theorem fields_injective {ps qs : List (List Nat)} {r r' : List Nat} (hlen : ps.length = qs.length)
    (hp : ∀ p ∈ ps, p.length < 2 ^ 64) (hq : ∀ q ∈ qs, q.length < 2 ^ 64)
    (h : fields ps ++ r = fields qs ++ r') : ps = qs ∧ r = r' := by
  have a := readFields_fields ps r hp
  have b := readFields_fields qs r' hq
  rw [h, hlen, b] at a
  simp only [Option.some.injEq, Prod.mk.injEq] at a
  exact ⟨a.1.symm, a.2.symm⟩

/-- The first field decides: different first fields, different inputs —
whatever the rest of either is. -/
theorem first_field_decides {p q : List Nat} {ps qs : List (List Nat)} {r r' : List Nat}
    (hp : p.length < 2 ^ 64) (hq : q.length < 2 ^ 64)
    (h : fields (p :: ps) ++ r = fields (q :: qs) ++ r') : p = q := by
  have a := readField_field p (fields ps ++ r) hp
  have b := readField_field q (fields qs ++ r') hq
  have h' : field p ++ (fields ps ++ r) = field q ++ (fields qs ++ r') := by
    simpa [fields, List.append_assoc] using h
  rw [h', b] at a
  simp only [Option.some.injEq, Prod.mk.injEq] at a
  exact a.1.symm

/-! ## The three kinds of fingerprint -/

def bytesOf (s : String) : List Nat := s.toList.map Char.toNat

/-- `OPTIONS_DOMAIN`. -/
def optionsDomain : List Nat := bytesOf "clew.lsp.init-options.v1"

/-- `BUILD_DOMAIN`. -/
def buildDomain : List Nat := bytesOf "clew.lsp.build.v1"

/-- What a command fingerprint hashes: the canonical command path, the
arguments, the server, its version, the options, then the 32-byte SHA-256
of the command's bytes. -/
def commandInput (path args server version options content : List Nat) : List Nat :=
  fields [path, args, server, version, options] ++ content

/-- What an options-only fingerprint hashes. -/
def optionsInput (args server version options : List Nat) : List Nat :=
  fields [optionsDomain, args, server, version, options]

/-- What a leave-to-build fingerprint hashes. -/
def buildInput (server : List Nat) : List Nat := fields [buildDomain, server]

/-- A canonical path is absolute: its first byte is `/`. -/
def absolute (path : List Nat) : Prop := path.head? = some 47

theorem absolute_ne_domain (path d : List Nat) (hp : absolute path) (hd : d.head? = some 99) :
    path ≠ d := by
  intro he; subst he; unfold absolute at hp; rw [hp] at hd; cases hd

theorem optionsDomain_head : optionsDomain.head? = some 99 := by decide
theorem buildDomain_head : buildDomain.head? = some 99 := by decide
theorem optionsDomain_length : optionsDomain.length < 2 ^ 64 := by decide
theorem buildDomain_length : buildDomain.length < 2 ^ 64 := by decide

/-- **A command approval is never an options-only approval.** -/
theorem command_ne_options (path args server version options content : List Nat)
    (args' server' version' options' : List Nat) (hpath : absolute path)
    (hlen : path.length < 2 ^ 64) :
    commandInput path args server version options content ≠
      optionsInput args' server' version' options' := by
  intro h
  have := first_field_decides (r' := []) hlen optionsDomain_length
    (by simpa [commandInput, optionsInput] using h)
  exact absolute_ne_domain path optionsDomain hpath optionsDomain_head this

/-- **A command approval is never leave to build.** -/
theorem command_ne_build (path args server version options content server' : List Nat)
    (hpath : absolute path) (hlen : path.length < 2 ^ 64) :
    commandInput path args server version options content ≠ buildInput server' := by
  intro h
  have := first_field_decides (r' := []) hlen buildDomain_length
    (by simpa [commandInput, buildInput] using h)
  exact absolute_ne_domain path buildDomain hpath buildDomain_head this

/-- **An options-only approval is never leave to build.** -/
theorem options_ne_build (args server version options server' : List Nat) :
    optionsInput args server version options ≠ buildInput server' := by
  intro h
  have := first_field_decides (r := []) (r' := []) optionsDomain_length buildDomain_length
    (by simpa [optionsInput, buildInput] using h)
  revert this; decide

/-- **A command approval covers that command and nothing else**: the same
hash input means the same path, arguments, server, version, options and
command bytes. -/
theorem commandInput_injective {path args server version options content : List Nat}
    {path' args' server' version' options' content' : List Nat}
    (hb : ∀ p ∈ [path, args, server, version, options], p.length < 2 ^ 64)
    (hb' : ∀ p ∈ [path', args', server', version', options'], p.length < 2 ^ 64)
    (h : commandInput path args server version options content =
      commandInput path' args' server' version' options' content') :
    path = path' ∧ args = args' ∧ server = server' ∧ version = version' ∧ options = options' ∧
      content = content' := by
  have := fields_injective (ps := [path, args, server, version, options])
    (qs := [path', args', server', version', options']) rfl hb hb' h
  obtain ⟨hps, h6⟩ := this
  simp only [List.cons.injEq, and_true] at hps
  obtain ⟨h1, h2, h3, h4, h5⟩ := hps
  exact ⟨h1, h2, h3, h4, h5, h6⟩

/-- **An options-only approval covers those options and nothing else.** -/
theorem optionsInput_injective {args server version options args' server' version' options' : List Nat}
    (hb : ∀ p ∈ [args, server, version, options], p.length < 2 ^ 64)
    (hb' : ∀ p ∈ [args', server', version', options'], p.length < 2 ^ 64)
    (h : optionsInput args server version options = optionsInput args' server' version' options') :
    args = args' ∧ server = server' ∧ version = version' ∧ options = options' := by
  have hall : ∀ p ∈ [optionsDomain, args, server, version, options], p.length < 2 ^ 64 := by
    intro p hp
    simp only [List.mem_cons] at hp
    rcases hp with rfl | hp
    · exact optionsDomain_length
    · exact hb p (by simpa using hp)
  have hall' : ∀ p ∈ [optionsDomain, args', server', version', options'], p.length < 2 ^ 64 := by
    intro p hp
    simp only [List.mem_cons] at hp
    rcases hp with rfl | hp
    · exact optionsDomain_length
    · exact hb' p (by simpa using hp)
  have h' : fields [optionsDomain, args, server, version, options] ++ [] =
      fields [optionsDomain, args', server', version', options'] ++ [] := by
    rw [List.append_nil, List.append_nil]; exact h
  have := fields_injective (ps := [optionsDomain, args, server, version, options])
    (qs := [optionsDomain, args', server', version', options']) rfl hall hall' h'
  obtain ⟨hps, -⟩ := this
  simp only [List.cons.injEq, true_and, and_true] at hps
  exact hps

/-! ## Approvals are scoped to one machine and one project -/

/-- The characters escaped in the parts of a remote's approval scope. -/
def escChar (c : Char) : List Char :=
  if c == '%' then ['%', '2', '5']
  else if c == '/' then ['%', '2', 'F']
  else if c == '[' then ['%', '5', 'B']
  else if c == ']' then ['%', '5', 'D']
  else if c == '@' then ['%', '4', '0']
  else [c]

def esc (s : List Char) : List Char := s.flatMap escChar

/-- The escape a `%` starts, read back. -/
def unescPair (a b : Char) : Option Char :=
  if a == '2' && b == '5' then some '%'
  else if a == '2' && b == 'F' then some '/'
  else if a == '5' && b == 'B' then some '['
  else if a == '5' && b == 'D' then some ']'
  else if a == '4' && b == '0' then some '@'
  else none

def unesc : List Char → Option (List Char)
  | [] => some []
  | c :: r =>
    if c == '%' then
      match r with
      | a :: b :: rest => (unescPair a b).bind fun d => (unesc rest).map (d :: ·)
      | _ => none
    else (unesc r).map (c :: ·)

theorem unesc_esc (s : List Char) : unesc (esc s) = some s := by
  induction s with
  | nil => rfl
  | cons c s ih =>
    simp only [esc, List.flatMap_cons] at ih ⊢
    by_cases h1 : c = '%'
    · subst h1
      rw [show escChar '%' = ['%', '2', '5'] by decide]
      simp [unesc, unescPair, ih]
    by_cases h2 : c = '/'
    · subst h2
      rw [show escChar '/' = ['%', '2', 'F'] by decide]
      simp [unesc, unescPair, ih]
    by_cases h3 : c = '['
    · subst h3
      rw [show escChar '[' = ['%', '5', 'B'] by decide]
      simp [unesc, unescPair, ih]
    by_cases h4 : c = ']'
    · subst h4
      rw [show escChar ']' = ['%', '5', 'D'] by decide]
      simp [unesc, unescPair, ih]
    by_cases h5 : c = '@'
    · subst h5
      rw [show escChar '@' = ['%', '4', '0'] by decide]
      simp [unesc, unescPair, ih]
    rw [show escChar c = [c] by simp [escChar, h1, h2, h3, h4, h5], List.singleton_append,
      unesc.eq_def]
    simp [h1, ih]

theorem lit_free (x : Char) (s : List Char) (h1 : x ≠ '/') (h2 : x ≠ '[') (h3 : x ≠ ']')
    (h4 : x ≠ '@') (h5 : x ≠ ':') :
    x ≠ '/' ∧ x ≠ '[' ∧ x ≠ ']' ∧ x ≠ '@' ∧ (x = ':' → ':' ∈ s) :=
  ⟨h1, h2, h3, h4, fun h => absurd h h5⟩

/-- What escaping leaves out: no `/`, `[`, `]` or `@` at all, and a `:` only
where the text had one. -/
theorem esc_free (s : List Char) : ∀ c ∈ esc s, c ≠ '/' ∧ c ≠ '[' ∧ c ≠ ']' ∧ c ≠ '@' ∧
    (c = ':' → ':' ∈ s) := by
  intro c hc
  simp only [esc, List.mem_flatMap] at hc
  obtain ⟨d, hd, hc⟩ := hc
  unfold escChar at hc
  by_cases h1 : d = '%'
  · subst h1; simp at hc
    rcases hc with rfl | rfl | rfl <;>
      exact lit_free _ _ (by decide) (by decide) (by decide) (by decide) (by decide)
  by_cases h2 : d = '/'
  · subst h2; simp at hc
    rcases hc with rfl | rfl | rfl <;>
      exact lit_free _ _ (by decide) (by decide) (by decide) (by decide) (by decide)
  by_cases h3 : d = '['
  · subst h3; simp at hc
    rcases hc with rfl | rfl | rfl <;>
      exact lit_free _ _ (by decide) (by decide) (by decide) (by decide) (by decide)
  by_cases h4 : d = ']'
  · subst h4; simp at hc
    rcases hc with rfl | rfl | rfl <;>
      exact lit_free _ _ (by decide) (by decide) (by decide) (by decide) (by decide)
  by_cases h5 : d = '@'
  · subst h5; simp at hc
    rcases hc with rfl | rfl | rfl <;>
      exact lit_free _ _ (by decide) (by decide) (by decide) (by decide) (by decide)
  simp [h1, h2, h3, h4, h5] at hc
  subst hc
  exact ⟨h2, h3, h4, h5, fun he => he ▸ hd⟩

/-- `scope_label`'s host: bracketed when it holds a `:` (an IPv6 address),
so a port after it can never be read as part of it. -/
def hostPart (host : List Char) : List Char :=
  if host.contains ':' then '[' :: (esc host ++ [']']) else esc host

def portPart (port : Nat) : List Char := if port == 22 then [] else ':' :: decimal port

/-- `scope_label`: `user@host`, the user left out when ssh is given none,
`:port` when the port is not 22. -/
def scopeLabel (user : Option (List Char)) (host : List Char) (port : Nat) : List Char :=
  (match user with
    | some u => esc u ++ ['@']
    | none => []) ++ (hostPart host ++ portPart port)

/-- `project_key` for a remote: `ssh://<label>:<root>`. -/
def approvalKey (label root : List Char) : List Char := "ssh://".toList ++ label ++ ':' :: root

/-- The port read back. -/
def readPort : List Char → Option Nat
  | [] => some 22
  | ':' :: ds => if !ds.isEmpty && ds.all isAsciiDigit then some (digitsValue ds) else none
  | _ => none

/-- A host part read back. -/
def readHost (r : List Char) : Option (List Char × Nat) :=
  match r with
  | '[' :: rest =>
    match splitOnce ']' rest with
    | some (e, after) => do
      let h ← unesc e
      let p ← readPort after
      pure (h, p)
    | none => none
  | _ =>
    match splitOnce ':' r with
    | some (e, ds) => do
      let h ← unesc e
      let p ← readPort (':' :: ds)
      pure (h, p)
    | none => do
      let h ← unesc r
      pure (h, 22)

/-- A label read back: the user (if there is an `@`), the host, the port. -/
def readLabel (l : List Char) : Option (Option (List Char) × List Char × Nat) :=
  match splitOnce '@' l with
  | some (eu, r) => do
    let u ← unesc eu
    let (h, p) ← readHost r
    pure (some u, h, p)
  | none => do
    let (h, p) ← readHost l
    pure (none, h, p)

/-- A key read back: the label is everything up to the `:` before the
first `/`, which starts the root. -/
def readKey (k : List Char) : Option (List Char × List Char) :=
  let body := k.drop 6
  let front := body.takeWhile (· != '/')
  let root := body.dropWhile (· != '/')
  match front.reverse with
  | ':' :: l => some (l.reverse, root)
  | _ => none

theorem splitOnce_append {c : Char} (xs ys : List Char) (hx : c ∉ xs) :
    splitOnce c (xs ++ c :: ys) = some (xs, ys) := by
  induction xs with
  | nil => simp [splitOnce]
  | cons x xs ih =>
    have hne : (x == c) = false := by
      have : x ≠ c := fun he => hx (he ▸ by simp)
      simpa using this
    simp [splitOnce, hne, ih (fun h => hx (by simp [h]))]

theorem splitOnce_absent {c : Char} (xs : List Char) (hx : c ∉ xs) : splitOnce c xs = none := by
  induction xs with
  | nil => rfl
  | cons x xs ih =>
    have hne : (x == c) = false := by
      have : x ≠ c := fun he => hx (he ▸ by simp)
      simpa using this
    simp [splitOnce, hne, ih (fun h => hx (by simp [h]))]

theorem readPort_portPart (port : Nat) : readPort (portPart port) = some port := by
  unfold portPart
  by_cases h : port = 22
  · subst h; rfl
  · have hd : (decimal port).all isAsciiDigit = true := List.all_eq_true.mpr (decimal_digits port)
    have hne : (decimal port).isEmpty = false := by
      cases he : decimal port with
      | nil => exact absurd he (decimal_ne_nil port)
      | cons _ _ => rfl
    simp [h, readPort, hd, hne, digitsValue_decimal]

theorem portPart_free (port : Nat) : ∀ c ∈ portPart port, c ≠ '/' ∧ c ≠ '@' ∧ c ≠ ']' := by
  intro c hc
  unfold portPart at hc
  split at hc
  · simp at hc
  · simp only [List.mem_cons] at hc
    rcases hc with rfl | hc
    · decide
    · have := decimal_digits port c hc
      simp only [isAsciiDigit, Bool.and_eq_true, decide_eq_true_eq] at this
      refine ⟨?_, ?_, ?_⟩ <;> (intro he; subst he; simp at this)

theorem portPart_head (port : Nat) : ∀ c, (portPart port).head? = some c → c = ':' := by
  intro c hc
  unfold portPart at hc
  split at hc
  · simp at hc
  · simp at hc; exact hc.symm

theorem readHost_hostPart (host : List Char) (port : Nat) :
    readHost (hostPart host ++ portPart port) = some (host, port) := by
  have hfree := esc_free host
  unfold hostPart
  by_cases hc : host.contains ':' = true
  · simp only [hc, ite_true, List.cons_append, List.append_assoc]
    unfold readHost
    have hnb : ']' ∉ esc host := fun h => (hfree ']' h).2.2.1 rfl
    simp only [splitOnce_append _ _ hnb]
    simp [unesc_esc, readPort_portPart]
  · have hcf : host.contains ':' = false := by simpa using hc
    have hnc : ':' ∉ host := by simpa using hcf
    simp only [hcf, Bool.false_eq_true, ite_false]
    have hhead : ∀ c, (esc host ++ portPart port).head? = some c → c ≠ '[' := by
      intro c hc'
      cases he : esc host with
      | nil =>
        rw [he] at hc'
        have := portPart_head port c (by simpa using hc')
        subst this; decide
      | cons e es =>
        rw [he] at hc'
        simp at hc'
        subst hc'
        exact (hfree e (by rw [he]; simp)).2.1
    have hnoc : ':' ∉ esc host := fun h => hnc ((hfree ':' h).2.2.2.2 rfl)
    unfold readHost
    split
    · rename_i rest heq
      exact absurd rfl (hhead '[' (by rw [heq]; rfl))
    · unfold portPart
      by_cases hp : port = 22
      · subst hp
        simp only [beq_self_eq_true, ite_true, List.append_nil]
        rw [splitOnce_absent _ hnoc]
        simp [unesc_esc]
      · have hp' : (port == 22) = false := by simpa using hp
        simp only [hp', Bool.false_eq_true, ite_false]
        rw [splitOnce_append _ _ hnoc]
        have := readPort_portPart port
        unfold portPart at this
        simp only [hp', Bool.false_eq_true, ite_false] at this
        simp [unesc_esc, this]

/-- **A label names one user, host and port.** -/
theorem readLabel_scopeLabel (user : Option (List Char)) (host : List Char) (port : Nat) :
    readLabel (scopeLabel user host port) = some (user, host, port) := by
  have hnoat : '@' ∉ hostPart host ++ portPart port := by
    intro h
    simp only [List.mem_append] at h
    rcases h with h | h
    · unfold hostPart at h
      split at h
      · simp only [List.mem_cons, List.mem_append, List.not_mem_nil, or_false] at h
        rcases h with h | h | h
        · exact absurd h (by decide)
        · exact (esc_free host '@' h).2.2.2.1 rfl
        · exact absurd h (by decide)
      · exact (esc_free host '@' h).2.2.2.1 rfl
    · exact (portPart_free port '@' h).2.1 rfl
  cases user with
  | none =>
    unfold readLabel scopeLabel
    simp only [List.nil_append]
    rw [splitOnce_absent _ hnoat, readHost_hostPart]
    rfl
  | some u =>
    have hnu : '@' ∉ esc u := fun h => (esc_free u '@' h).2.2.2.1 rfl
    unfold readLabel scopeLabel
    simp only [List.append_assoc, List.singleton_append]
    rw [splitOnce_append _ _ hnu]
    simp [unesc_esc, readHost_hostPart]

theorem scopeLabel_free (user : Option (List Char)) (host : List Char) (port : Nat) :
    ∀ c ∈ scopeLabel user host port, c ≠ '/' := by
  intro c hc
  simp only [scopeLabel, List.mem_append] at hc
  rcases hc with hc | hc | hc
  · cases user with
    | none => simp at hc
    | some u =>
      simp only [List.mem_append, List.mem_singleton] at hc
      rcases hc with hc | rfl
      · exact (esc_free u c hc).1
      · decide
  · unfold hostPart at hc
    split at hc
    · simp only [List.mem_cons, List.mem_append, List.not_mem_nil, or_false] at hc
      rcases hc with rfl | hc | rfl
      · decide
      · exact (esc_free host c hc).1
      · decide
    · exact (esc_free host c hc).1
  · exact (portPart_free port c hc).1

/-- **The key splits back into its label and root**, for an absolute root. -/
theorem readKey_approvalKey (label root : List Char) (hl : ∀ c ∈ label, c ≠ '/')
    (hr : root.head? = some '/') : readKey (approvalKey label root) = some (label, root) := by
  unfold readKey approvalKey
  dsimp only
  have hdrop : ("ssh://".toList ++ label ++ ':' :: root).drop 6 = label ++ ':' :: root := by
    simp
  rw [hdrop]
  have hfront : ∀ c ∈ label ++ [':'], (c != '/') = true := by
    intro c hc
    simp only [List.mem_append, List.mem_singleton] at hc
    rcases hc with hc | rfl
    · simpa using hl c hc
    · decide
  have hstop : ∀ c, root.head? = some c → (c != '/') = false := by
    intro c hc; rw [hr] at hc; cases hc; decide
  have e : label ++ ':' :: root = (label ++ [':']) ++ root := by simp
  rw [e, takeWhile_append_all _ _ hfront hstop, dropWhile_append_all _ _ hfront hstop]
  simp

/-- **One key, one machine and one project.** Two remotes — user, host and
port — and two absolute roots share an approval key only if they are the
same: an approval granted for one machine's project never covers another's. -/
theorem approvalKey_injective {u u' : Option (List Char)} {h h' r r' : List Char} {p p' : Nat}
    (hr : r.head? = some '/') (hr' : r'.head? = some '/')
    (heq : approvalKey (scopeLabel u h p) r = approvalKey (scopeLabel u' h' p') r') :
    u = u' ∧ h = h' ∧ p = p' ∧ r = r' := by
  have k := readKey_approvalKey _ r (scopeLabel_free u h p) hr
  have k' := readKey_approvalKey _ r' (scopeLabel_free u' h' p') hr'
  rw [heq, k'] at k
  simp only [Option.some.injEq, Prod.mk.injEq] at k
  obtain ⟨hl, hroot⟩ := k
  have l := readLabel_scopeLabel u h p
  have l' := readLabel_scopeLabel u' h' p'
  rw [← hl, l'] at l
  simp only [Option.some.injEq, Prod.mk.injEq] at l
  exact ⟨l.1.symm, l.2.1.symm, l.2.2.symm, hroot.symm⟩

/-- **A remote approval is never a local one**: a local project's key is its
canonical (absolute) path, a remote's starts `ssh://`. -/
theorem local_ne_remote (path label root : List Char) (hp : path.head? = some '/') :
    path ≠ approvalKey label root := by
  intro h
  rw [h] at hp
  simp [approvalKey] at hp

end Clew.Trust
