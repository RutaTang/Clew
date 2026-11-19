import Clew.Text

/-!
# Message framing — `clew_core::framing`

Every message Clew exchanges with a language server, a debug adapter, an MCP
or ACP peer, and its own clew-server is a frame on a byte stream. The peer is
another program — chosen, for a language server or a debug adapter, by the
repository's own configuration — so every byte it writes is untrusted, and a
frame boundary misread once means every later frame is read from the wrong
offset.

This file models the readers byte for byte — `read_frame`
(`Content-Length` headers), `read_json_line` (MCP and ACP), and
`read_line_capped` (Clew's own protocol) — and the writers, and proves:

* **what Clew writes is read back exactly, and nothing past it is
  consumed**: so a stream of frames is read as that stream of messages,
  however many there are, and never falls out of step;
* a body is never longer than the cap, whatever the peer claims;
* every frame read consumes input: the reader always makes progress;
* the clew protocol's cap is on the line's content, the newline not
  counted: a line exactly at the cap is read, one byte more is refused (the
  two ends once disagreed by that byte, and the largest frame one end would
  send was the frame the other dropped the connection on).

What `read_frame` decides from one header line is `header`; the Rust
function it models (`clew_rules::framing::header`), translated to Lean by
Charon and Aeneas, is proved to be it in `verification/extracted/Equal.lean`.

Bytes are `Nat`s.
-/

namespace Clew.Framing

open Clew.Text

/-! ## Bytes -/

def NL : Nat := 10
def CR : Nat := 13

/-- `MAX_HEADER_LINE_BYTES`. -/
def maxHeaderLine : Nat := 8 * 1024
/-- `MAX_HEADER_LINES`. -/
def maxHeaderLines : Nat := 32
/-- `MAX_FRAME_BYTES`. -/
def maxFrame : Nat := 64 * 1024 * 1024

def isAsciiDigitB (b : Nat) : Bool := 48 ≤ b && b ≤ 57
def isAsciiAlnumB (b : Nat) : Bool :=
  isAsciiDigitB b || (65 ≤ b && b ≤ 90) || (97 ≤ b && b ≤ 122)

/-- `u8::is_ascii_whitespace`: space, tab, LF, FF, CR (not VT). -/
def isAsciiWs (b : Nat) : Bool := b == 32 || b == 9 || b == 10 || b == 12 || b == 13

/-- `is_token_byte`: an RFC 7230 token byte. -/
def isTokenByte (b : Nat) : Bool :=
  isAsciiAlnumB b || [33, 35, 36, 37, 38, 39, 42, 43, 45, 46, 94, 95, 96, 124, 126].contains b

/-- `u8::to_ascii_lowercase`. -/
def lowerB (b : Nat) : Nat := if 65 ≤ b && b ≤ 90 then b + 32 else b

/-- `<[u8]>::trim_ascii`. -/
def trimAscii (bs : List Nat) : List Nat :=
  ((bs.dropWhile isAsciiWs).reverse.dropWhile isAsciiWs).reverse

/-- `content-length`, as bytes. -/
def contentLengthName : List Nat := "content-length".toList.map Char.toNat

/-- The decimal digits of `n`, as bytes. -/
def digitsB (n : Nat) : List Nat := (decimal n).map Char.toNat

def valueB (bs : List Nat) : Nat := bs.foldl (fun acc b => acc * 10 + (b - 48)) 0

/-! ## `read_frame` -/

/-- `FrameError`'s cases that framing decides. -/
inductive Error where
  | truncated
  | headerLineTooLong
  | tooManyHeaderLines
  | malformedHeader
  | missingContentLength
  | invalidContentLength
  | conflictingContentLength
  | tooLarge
  deriving DecidableEq, Repr

/-- `parse_length`: ASCII digits only, at most 20 of them, and a value that
fits a `u64`. -/
def parseLength (v : List Nat) : Except Error Nat :=
  if v.isEmpty || v.length > 20 || !v.all isAsciiDigitB then .error .invalidContentLength
  else
    let n := valueB v
    if n < 2 ^ 64 then .ok n else .error .invalidContentLength

/-- The first index of byte `x`, if any (`position`). -/
def findByte (x : Nat) : List Nat → Option Nat
  | [] => none
  | b :: bs => if b == x then some 0 else (findByte x bs).map (· + 1)

/-- The first newline's index, if any. -/
def findNL (bs : List Nat) : Option Nat := findByte NL bs

/-- `take(cap).read_until(b'\n')`: the bytes up to and including the first
newline, but no more than `cap` of them; and what is left. -/
def readLine (cap : Nat) (s : List Nat) : List Nat × List Nat :=
  match findNL (s.take cap) with
  | some i => (s.take (i + 1), s.drop (i + 1))
  | none => (s.take cap, s.drop cap)

/-- What the header loop has seen: the `Content-Length` so far, the headers,
the lines. -/
structure Seen where
  length : Option Nat := none
  headers : Nat := 0
  lines : Nat := 0

/-- One non-blank header line (its line end removed): malformed, a
`Content-Length` to record, or any other header to pass over. -/
def header (seen : Seen) (line : List Nat) : Except Error Seen :=
  if !line.all (· < 128) then .error .malformedHeader
  else
    match findByte 58 line with
    | none => .error .malformedHeader
    | some colon =>
      let name := line.take colon
      let value := line.drop (colon + 1)
      if name.isEmpty || !name.all isTokenByte then .error .malformedHeader
      else if name.map lowerB == contentLengthName then
        match parseLength (trimAscii value) with
        | .error e => .error e
        | .ok len =>
          if seen.length.isSome && seen.length != some len then .error .conflictingContentLength
          else if len > maxFrame then .error .tooLarge
          else .ok { seen with length := some len, headers := seen.headers + 1 }
      else .ok { seen with headers := seen.headers + 1 }

/-- A line with its end taken off: the `\n`, then a `\r` before it. -/
def stripEnd (line : List Nat) : List Nat :=
  let body := line.dropLast
  if body.getLast? == some CR then body.dropLast else body

/-- What a header line that is not `\n`-ended is: too long if it filled the
cap, else the stream ended inside it. -/
def unended (line : List Nat) : Error :=
  if line.length ≥ maxHeaderLine then .headerLineTooLong else .truncated

/-- The body, once the headers are done. -/
def finish (seen : Seen) (rest : List Nat) : Except Error (Option (List Nat)) × List Nat :=
  match seen.length with
  | none => (.error .missingContentLength, rest)
  | some len =>
    if rest.length < len then (.error .truncated, rest.drop len)
    else (.ok (some (rest.take len)), rest.drop len)

/-- The header loop, then the body: the frame's body (`none` at a clean end
of stream), and the bytes after it. `fuel` bounds the loop by the input's
length, which each line shortens. -/
def readFrameAux : Nat → Seen → List Nat → Except Error (Option (List Nat)) × List Nat
  | 0, _, s => (.error .truncated, s)
  | fuel + 1, seen, s =>
    match readLine maxHeaderLine s with
    | (line, rest) =>
      if line.isEmpty then
        (if seen.headers == 0 then .ok none else .error .truncated, rest)
      else if line.getLast? != some NL then (.error (unended line), rest)
      else if seen.lines + 1 > maxHeaderLines then (.error .tooManyHeaderLines, rest)
      else
        let seen := { seen with lines := seen.lines + 1 }
        if (stripEnd line).isEmpty then
          if seen.headers == 0 then readFrameAux fuel seen rest else finish seen rest
        else
          match header seen (stripEnd line) with
          | .error e => (.error e, rest)
          | .ok seen => readFrameAux fuel seen rest

/-- `read_frame`. -/
def readFrame (s : List Nat) : Except Error (Option (List Nat)) × List Nat :=
  readFrameAux (s.length + 1) {} s

/-- `Content-Length`, as Clew writes it. -/
def headerName : List Nat := "Content-Length".toList.map Char.toNat

/-- `encode` (and `encode_framed` for `Content-Length`):
`Content-Length: <n>\r\n\r\n`, then the body. -/
def encode (body : List Nat) : List Nat :=
  headerName ++ 58 :: 32 :: (digitsB body.length ++ CR :: NL :: CR :: NL :: body)

/-! ## Line framing -/

/-- `read_json_line`, its cap a parameter (`MAX_FRAME_BYTES`): blank lines
skipped, `\r\n` or `\n` ended, a last unended line still a line, an
over-cap line an error. -/
def readJsonLineAux (cap : Nat) : Nat → List Nat → Except Error (Option (List Nat)) × List Nat
  | 0, s => (.ok none, s)
  | fuel + 1, s =>
    let (line, rest) := readLine (cap + 2) s
    if line.isEmpty then (.ok none, rest)
    else
      let ended := line.getLast? == some NL
      let line := if ended then line.dropLast else line
      let line := if ended && line.getLast? == some CR then line.dropLast else line
      if line.length > cap then (.error .tooLarge, rest)
      else if line.all isAsciiWs then
        if ended then readJsonLineAux cap fuel rest else (.ok none, rest)
      else (.ok (some line), rest)

def readJsonLine (cap : Nat) (s : List Nat) : Except Error (Option (List Nat)) × List Nat :=
  readJsonLineAux cap (s.length + 1) s

/-- A continuation byte, `10xxxxxx`. -/
def cont (b : Nat) : Bool := 0x80 ≤ b && b ≤ 0xBF

/-- Whether bytes are UTF-8 as `String::from_utf8` accepts it: the
well-formed sequences of the Unicode standard's table 3-7 — no overlong
form, no surrogate, nothing past U+10FFFF, nothing cut off. -/
def utf8Valid : List Nat → Bool
  | [] => true
  | b :: r =>
    if b < 0x80 then utf8Valid r
    else if 0xC2 ≤ b && b ≤ 0xDF then
      match r with
      | c :: r => cont c && utf8Valid r
      | [] => false
    else if 0xE0 ≤ b && b ≤ 0xEF then
      match r with
      | c1 :: c2 :: r =>
        (if b == 0xE0 then 0xA0 ≤ c1 && c1 ≤ 0xBF
          else if b == 0xED then 0x80 ≤ c1 && c1 ≤ 0x9F
          else cont c1) && cont c2 && utf8Valid r
      | _ => false
    else if 0xF0 ≤ b && b ≤ 0xF4 then
      match r with
      | c1 :: c2 :: c3 :: r =>
        (if b == 0xF0 then 0x90 ≤ c1 && c1 ≤ 0xBF
          else if b == 0xF4 then 0x80 ≤ c1 && c1 ≤ 0x8F
          else cont c1) && cont c2 && cont c3 && utf8Valid r
      | _ => false
    else false

/-- `read_line_capped`: one line, its content at most `cap` bytes and
UTF-8. `none` at the end of the stream, past the cap, or for bytes that are
not text — the line consumed all the same. -/
def readLineCapped (cap : Nat) (s : List Nat) : Option (List Nat) × List Nat :=
  let (line, rest) := readLine (cap + 1) s
  let text := fun l : List Nat => if utf8Valid l then some l else none
  if line.isEmpty then (none, rest)
  else if line.getLast? == some NL then (text line.dropLast, rest)
  else if line.length > cap then (none, rest)
  else (text line, rest)

/-! ## Lemmas -/

theorem findByte_append (x : Nat) (xs ys : List Nat) (hx : x ∉ xs) :
    findByte x (xs ++ x :: ys) = some xs.length := by
  induction xs with
  | nil => simp [findByte]
  | cons y xs ih =>
    have hne : (y == x) = false := by
      have : y ≠ x := fun h => hx (h ▸ by simp)
      simpa using this
    simp [findByte, hne, ih (fun h => hx (by simp [h]))]

theorem findByte_none (x : Nat) (xs : List Nat) (hx : x ∉ xs) : findByte x xs = none := by
  induction xs with
  | nil => rfl
  | cons y xs ih =>
    have hne : (y == x) = false := by
      have : y ≠ x := fun h => hx (h ▸ by simp)
      simpa using this
    simp [findByte, hne, ih (fun h => hx (by simp [h]))]

theorem findNL_append (xs ys : List Nat) (hx : NL ∉ xs) :
    findNL (xs ++ NL :: ys) = some xs.length := findByte_append NL xs ys hx

theorem findNL_none (xs : List Nat) (hx : NL ∉ xs) : findNL xs = none := findByte_none NL xs hx

/-- A line that ends within the cap is read up to and including its
newline. -/
theorem readLine_append (cap : Nat) (xs ys : List Nat) (hx : NL ∉ xs) (hcap : xs.length < cap) :
    readLine cap (xs ++ NL :: ys) = (xs ++ [NL], ys) := by
  unfold readLine
  have htake : (xs ++ NL :: ys).take cap = xs ++ NL :: ys.take (cap - xs.length - 1) := by
    rw [List.take_append, List.take_of_length_le (by omega)]
    congr 1
    obtain ⟨k, hk⟩ : ∃ k, cap - xs.length = k + 1 := ⟨cap - xs.length - 1, by omega⟩
    rw [hk, List.take_succ_cons, Nat.add_sub_cancel]
  rw [htake, findNL_append _ _ hx]
  simp only [List.take_append, List.drop_append, List.take_of_length_le (Nat.le_succ _),
    List.drop_of_length_le (Nat.le_succ _), Nat.add_sub_cancel_left, List.nil_append]
  simp

theorem digitsB_length (n : Nat) : (digitsB n).length = (decimal n).length := by
  simp [digitsB]

theorem decimal_length_le (n k : Nat) (h : n < 10 ^ k) (hk : 0 < k) : (decimal n).length ≤ k := by
  induction k generalizing n with
  | zero => omega
  | succ k ih =>
    unfold decimal
    split
    · simp
    · rename_i hn
      simp only [List.length_append, List.length_singleton]
      have : n / 10 < 10 ^ k := by
        rw [Nat.pow_succ] at h; omega
      have hk' : 0 < k := by
        rcases k with _ | k
        · simp at this; omega
        · omega
      have := ih (n / 10) this hk'
      omega

theorem digitsB_digits (n : Nat) : ∀ b ∈ digitsB n, isAsciiDigitB b = true := by
  intro b hb
  simp only [digitsB, List.mem_map] at hb
  obtain ⟨c, hc, rfl⟩ := hb
  have := decimal_digits n c hc
  simp only [isAsciiDigit, Bool.and_eq_true, decide_eq_true_eq] at this
  simp only [isAsciiDigitB, Bool.and_eq_true, decide_eq_true_eq]
  exact ⟨this.1, this.2⟩

theorem valueB_digitsB (n : Nat) : valueB (digitsB n) = n := by
  have h : valueB (digitsB n) = digitsValue (decimal n) := by
    simp [valueB, digitsB, digitsValue, List.foldl_map]
  rw [h, digitsValue_decimal]

theorem digitsB_ne_nil (n : Nat) : digitsB n ≠ [] := by
  simp [digitsB, decimal_ne_nil]

theorem digit_facts (b : Nat) (h : isAsciiDigitB b = true) :
    b ≠ NL ∧ b ≠ CR ∧ b ≠ 58 ∧ b < 128 ∧ isAsciiWs b = false := by
  simp only [isAsciiDigitB, Bool.and_eq_true, decide_eq_true_eq] at h
  simp only [NL, CR, isAsciiWs, Bool.or_eq_false_iff, beq_eq_false_iff_ne]
  omega

theorem digitsB_short (n : Nat) (h : n ≤ maxFrame) : (digitsB n).length ≤ 8 := by
  rw [digitsB_length]
  exact decimal_length_le n 8 (by unfold maxFrame at h; omega) (by omega)

theorem trimAscii_space_digits (n : Nat) : trimAscii (32 :: digitsB n) = digitsB n := by
  have hd := digitsB_digits n
  have hne := digitsB_ne_nil n
  unfold trimAscii
  have h1 : (32 :: digitsB n).dropWhile isAsciiWs = digitsB n := by
    rw [show (32 :: digitsB n).dropWhile isAsciiWs = (digitsB n).dropWhile isAsciiWs by
      simp [List.dropWhile, isAsciiWs]]
    cases hdn : digitsB n with
    | nil => exact absurd hdn hne
    | cons b bs =>
      have := (digit_facts b (hd b (by rw [hdn]; simp))).2.2.2.2
      simp [List.dropWhile, this]
  rw [h1]
  cases hr : (digitsB n).reverse with
  | nil => simp at hr; exact absurd hr hne
  | cons b bs =>
    have hb : b ∈ digitsB n := by
      have : b ∈ (digitsB n).reverse := by rw [hr]; simp
      simpa using this
    have := (digit_facts b (hd b hb)).2.2.2.2
    simp [List.dropWhile, this, ← hr]

theorem parseLength_digitsB (n : Nat) (h : n ≤ maxFrame) : parseLength (digitsB n) = .ok n := by
  have hd := digitsB_digits n
  have hall : (digitsB n).all isAsciiDigitB = true := List.all_eq_true.mpr hd
  have hshort := digitsB_short n h
  have hne : (digitsB n).isEmpty = false := by
    cases hdn : digitsB n with
    | nil => exact absurd hdn (digitsB_ne_nil n)
    | cons _ _ => rfl
  have hlt : n < 2 ^ 64 := by unfold maxFrame at h; omega
  unfold parseLength
  simp [hne, hall, valueB_digitsB, hlt, show ¬ (digitsB n).length > 20 by omega]

theorem headerName_facts : headerName.length = 14 ∧ 58 ∉ headerName ∧ NL ∉ headerName ∧
    headerName.all (· < 128) = true ∧ headerName.isEmpty = false ∧
    headerName.all isTokenByte = true ∧ headerName.map lowerB = contentLengthName := by
  decide

/-- The header Clew writes is read as its `Content-Length`. -/
theorem header_encoded (seen : Seen) (n : Nat) (hn : n ≤ maxFrame) (hs : seen.length = none) :
    header seen (headerName ++ 58 :: 32 :: digitsB n) =
      .ok { seen with length := some n, headers := seen.headers + 1 } := by
  obtain ⟨hlen, hcolon, -, hascii, hempty, htoken, hlower⟩ := headerName_facts
  have hd := digitsB_digits n
  have hall : (headerName ++ 58 :: 32 :: digitsB n).all (· < 128) = true := by
    simp only [List.all_append, List.all_cons, hascii, Bool.true_and]
    simp only [show (58 : Nat) < 128 by omega, show (32 : Nat) < 128 by omega,
      decide_true, Bool.true_and]
    exact List.all_eq_true.mpr (fun b hb => by simpa using (digit_facts b (hd b hb)).2.2.2.1)
  unfold header
  simp only [hall, Bool.not_true, Bool.false_eq_true, ite_false]
  rw [findByte_append 58 headerName _ hcolon, hlen]
  have htake : (headerName ++ 58 :: 32 :: digitsB n).take 14 = headerName :=
    List.take_left' hlen
  have hdrop : (headerName ++ 58 :: 32 :: digitsB n).drop (14 + 1) = 32 :: digitsB n := by
    rw [← hlen]; simp
  simp only [htake, hdrop, hempty, htoken, Bool.not_true, Bool.or_false, Bool.false_eq_true,
    ite_false, hlower, beq_self_eq_true, ite_true, trimAscii_space_digits,
    parseLength_digitsB n hn, hs, Option.isSome_none, Bool.false_and]
  have hle : ¬ n > maxFrame := by omega
  simp only [hle, ite_false]

theorem encoded_line_free (n : Nat) :
    NL ∉ (headerName ++ 58 :: 32 :: digitsB n) ++ [CR] := by
  intro h
  simp only [List.mem_append, List.mem_cons, List.not_mem_nil, or_false] at h
  rcases h with (h | h | h | h) | h
  · exact headerName_facts.2.2.1 h
  · simp [NL] at h
  · simp [NL] at h
  · exact (digit_facts _ (digitsB_digits n _ h)).1 rfl
  · simp [NL, CR] at h

/-- **What Clew writes, it reads back — and nothing more.** A frame
`encode` wrote, whatever follows it on the stream, is read as exactly its
body, and the stream is left at the next frame. -/
theorem readFrame_encode (body rest : List Nat) (hb : body.length ≤ maxFrame) :
    readFrame (encode body ++ rest) = (.ok (some body), rest) := by
  have hshort := digitsB_short body.length hb
  have hP : ∀ seen : Seen, seen.length = none →
      header seen (headerName ++ 58 :: 32 :: digitsB body.length) =
        .ok { seen with length := some body.length, headers := seen.headers + 1 } :=
    fun seen hs => header_encoded seen body.length hb hs
  have hfree := encoded_line_free body.length
  have hlen1 : ((headerName ++ 58 :: 32 :: digitsB body.length) ++ [CR]).length < maxHeaderLine := by
    simp [headerName_facts.1, maxHeaderLine]; omega
  have hne : (headerName ++ 58 :: 32 :: digitsB body.length).isEmpty = false := by
    cases h : headerName ++ 58 :: 32 :: digitsB body.length with
    | nil => simp at h
    | cons _ _ => rfl
  have hsplit : encode body ++ rest =
      ((headerName ++ 58 :: 32 :: digitsB body.length) ++ [CR]) ++ NL ::
        ([CR] ++ NL :: (body ++ rest)) := by
    simp [encode]
  obtain ⟨k, hk⟩ : ∃ k, (encode body ++ rest).length + 1 = k + 2 :=
    ⟨(encode body ++ rest).length - 1, by simp [encode]; omega⟩
  unfold readFrame
  rw [hk, hsplit]
  generalize headerName ++ 58 :: 32 :: digitsB body.length = P at hP hfree hlen1 hne
  have hline1 := readLine_append maxHeaderLine _ ([CR] ++ NL :: (body ++ rest)) hfree hlen1
  have hline2 := readLine_append maxHeaderLine [CR] (body ++ rest) (by simp [NL, CR])
    (by simp [maxHeaderLine])
  -- The header line.
  have hstrip1 : stripEnd (P ++ [CR] ++ [NL]) = P := by simp [stripEnd, CR]
  have hne' : P ≠ [] := by intro h; simp [h] at hne
  unfold readFrameAux
  rw [hline1]
  simp only [List.isEmpty_iff, List.append_eq_nil_iff, List.cons_ne_nil, and_false,
    ite_false, List.getLast?_concat, bne_self_eq_false, Bool.false_eq_true, hstrip1, hne',
    show ¬ (0 + 1 > maxHeaderLines) by simp [maxHeaderLines]]
  rw [hP _ rfl]
  -- The blank line, then the body.
  unfold readFrameAux
  rw [hline2]
  simp [maxHeaderLines, stripEnd, finish, CR, NL,
    show ¬ (body.length + rest.length < body.length) by omega]

/-! ## The stream as a whole -/

/-- **A clean end of stream is a clean end**: nothing to read is no frame,
not an error. -/
theorem readFrame_nil : readFrame [] = (.ok none, []) := rfl

/-- Frames read one after another until the stream ends cleanly. -/
def readAll : Nat → List Nat → Option (List (List Nat))
  | 0, _ => none
  | fuel + 1, s =>
    match readFrame s with
    | (.ok none, _) => some []
    | (.ok (some b), rest) => (readAll fuel rest).map (b :: ·)
    | (.error _, _) => none

/-- **The stream stays in step.** However many frames Clew writes, one after
another, they are read back as exactly those messages, in order. -/
theorem readAll_encode (bodies : List (List Nat)) (h : ∀ b ∈ bodies, b.length ≤ maxFrame) :
    readAll (bodies.length + 1) (bodies.flatMap encode) = some bodies := by
  induction bodies with
  | nil => simp [readAll, readFrame_nil]
  | cons b bs ih =>
    show readAll (bs.length + 1 + 1) (encode b ++ bs.flatMap encode) = some (b :: bs)
    rw [readAll.eq_2, readFrame_encode b _ (h b (by simp))]
    simp only
    rw [ih (fun b' hb' => h b' (by simp [hb']))]
    rfl

theorem readLine_split (cap : Nat) (s : List Nat) :
    (readLine cap s).1 ++ (readLine cap s).2 = s := by
  unfold readLine
  split <;> simp


/-- A `Content-Length` once recorded is within the cap. -/
def bounded (seen : Seen) : Prop := ∀ l, seen.length = some l → l ≤ maxFrame

theorem header_bounded (seen seen' : Seen) (line : List Nat) (hb : bounded seen)
    (h : header seen line = .ok seen') : bounded seen' := by
  unfold header at h
  split at h
  · cases h
  · split at h
    · cases h
    · dsimp only at h
      split at h
      · cases h
      · split at h
        · split at h
          · cases h
          · split at h
            · cases h
            · split at h
              · cases h
              · rename_i len _ _ hle
                cases h
                intro l hl
                simp at hl
                omega
        · cases h
          exact hb

theorem finish_sound (seen : Seen) (rest body r : List Nat) (hb : bounded seen)
    (h : finish seen rest = (.ok (some body), r)) : body.length ≤ maxFrame ∧ r.length ≤ rest.length := by
  unfold finish at h
  split at h
  · cases h
  · rename_i len hlen
    split at h
    · cases h
    · simp only [Prod.mk.injEq, Except.ok.injEq, Option.some.injEq] at h
      obtain ⟨rfl, rfl⟩ := h
      have := hb len hlen
      exact ⟨by simp; omega, by simp⟩

/-- **Within the cap, and always moving.** A frame read is never longer than
`MAX_FRAME_BYTES`, whatever its header claimed, and reading it consumed
input: the reader cannot spin in place. -/
theorem readFrameAux_sound : ∀ (fuel : Nat) (seen : Seen) (s body rest : List Nat),
    bounded seen → readFrameAux fuel seen s = (.ok (some body), rest) →
      body.length ≤ maxFrame ∧ rest.length < s.length := by
  intro fuel
  induction fuel with
  | zero => intro seen s body rest _ h; simp [readFrameAux] at h
  | succ fuel ih =>
    intro seen s body rest hb h
    have hsplit := readLine_split maxHeaderLine s
    generalize hl : readLine maxHeaderLine s = r at h hsplit
    obtain ⟨line, rest1⟩ := r
    simp only at hsplit
    simp only [readFrameAux, hl] at h
    by_cases h1 : line.isEmpty = true
    · simp only [h1, ite_true] at h
      split at h <;> simp at h
    simp only [h1, Bool.false_eq_true, ite_false] at h
    have hne : line ≠ [] := by intro he; simp [he] at h1
    have hshorter : rest1.length < s.length := by
      rw [← hsplit, List.length_append]
      have := List.length_pos_iff.mpr hne
      omega
    have hbl : bounded { seen with lines := seen.lines + 1 } := fun l hl => hb l hl
    by_cases h2 : (line.getLast? != some NL) = true
    · simp [h2] at h
    simp only [h2, Bool.false_eq_true, ite_false] at h
    by_cases h3 : seen.lines + 1 > maxHeaderLines
    · simp [h3] at h
    simp only [h3, ite_false] at h
    by_cases h4 : (stripEnd line).isEmpty = true
    · simp only [h4, ite_true] at h
      by_cases h5 : (seen.headers == 0) = true
      · simp only [h5, ite_true] at h
        have := ih _ rest1 body rest hbl h
        exact ⟨this.1, by omega⟩
      · simp only [h5, Bool.false_eq_true, ite_false] at h
        have := finish_sound _ rest1 body rest hbl h
        exact ⟨this.1, by omega⟩
    · simp only [h4, Bool.false_eq_true, ite_false] at h
      split at h
      · simp at h
      · rename_i seen' hh
        have hb' := header_bounded _ seen' _ hbl hh
        have := ih seen' rest1 body rest hb' h
        exact ⟨this.1, by omega⟩

theorem readFrame_sound (s body rest : List Nat) (h : readFrame s = (.ok (some body), rest)) :
    body.length ≤ maxFrame ∧ rest.length < s.length :=
  readFrameAux_sound _ {} s body rest (fun _ h => by simp at h) h

/-! ## Lines -/

/-- **A JSON line Clew writes is read back exactly** — one message, no line
end in it, within the cap — and the stream is left at the next line. -/
theorem readJsonLine_line (cap : Nat) (msg rest : List Nat) (hnl : NL ∉ msg)
    (hcap : msg.length ≤ cap) (hcr : msg.getLast? ≠ some CR) (hws : msg.all isAsciiWs = false) :
    readJsonLine cap (msg ++ NL :: rest) = (.ok (some msg), rest) := by
  obtain ⟨k, hk⟩ : ∃ k, (msg ++ NL :: rest).length + 1 = k + 1 := ⟨_, rfl⟩
  unfold readJsonLine
  rw [hk]
  unfold readJsonLineAux
  rw [readLine_append (cap + 2) msg rest hnl (by omega)]
  have hcr' : (msg.getLast? == some CR) = false := by simpa using hcr
  simp [hcr', hws, show ¬ msg.length > cap by omega]

/-- **The clew protocol's cap is on content.** A line of text of at most
`cap` bytes, newline after it, is read; the stream is left after the
newline. -/
theorem readLineCapped_within (cap : Nat) (line rest : List Nat) (hnl : NL ∉ line)
    (hcap : line.length ≤ cap) (htext : utf8Valid line = true) :
    readLineCapped cap (line ++ NL :: rest) = (some line, rest) := by
  unfold readLineCapped
  rw [readLine_append (cap + 1) line rest hnl (by omega)]
  simp [htext]

/-- **And past the cap, nothing.** A line whose first `cap + 1` bytes hold
no newline is refused, not read in part. -/
theorem readLineCapped_over (cap : Nat) (s : List Nat) (hnl : NL ∉ s.take (cap + 1))
    (hlong : cap < s.length) : (readLineCapped cap s).1 = none := by
  unfold readLineCapped readLine
  rw [findNL_none _ hnl]
  have hlen : (s.take (cap + 1)).length = cap + 1 := by simp; omega
  have hne : s.take (cap + 1) ≠ [] := by intro h; simp [h] at hlen
  have hlast : (s.take (cap + 1)).getLast? ≠ some NL := by
    intro h
    exact hnl (List.mem_of_getLast? h)
  simp [hne, hlast, hlen]

end Clew.Framing
