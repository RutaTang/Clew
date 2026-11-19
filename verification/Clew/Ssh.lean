import Clew.Text
import Clew.Trust


/-!
# Shell words and ssh arguments — `src/backend/server.rs`, `src/backend/connect.rs`, `src/macos/install.rs`

What Clew hands to a shell or to `ssh` is text another program parses, and
the parse decides what runs. Here that is proved, over every input:

* **Quoting.** `shell_command` (a command Clew shows the reader to paste)
  and the macOS updater's `quote` (every path in its swap script) write
  words a POSIX shell reads back as exactly those words — whatever the
  words hold: spaces, quotes, `$`, newlines. And `sh -c '<script>'`, the
  form every command Clew runs on a remote takes, is read as `sh`, `-c` and
  the script, whenever the script holds no `'` (which `sh_c` refuses).
  The Rust itself (`clew_rules::shell`), translated to Lean by Charon and
  Aeneas, is proved in `verification/extracted/Equal.lean` to return what
  `shellCommand`, `shC` and `quote` return, on the UTF-8 bytes of any
  string.
* **ssh's arguments.** A saved connection's fields never reach `ssh` as an
  option, whatever they hold: the options ssh reads are exactly the ones
  Clew wrote, and the destination exactly `user@host`. And the user, host
  and port ssh is told are the ones its approvals are filed under — the
  label a `CLEW_SSH` with the same arguments gets is the label the saved
  connection gets.
* **known_hosts.** A host field Clew writes a key under names one host and
  port, read back, and holds none of the pattern syntax (`*`, `?`, `!`,
  `,`, a hashed `|1|` name) that would let the key vouch for others.

The shell is modelled as the POSIX rules for the words Clew writes: an
unquoted character that is not special, a single-quoted run, and a
backslash before a character. Anything else is outside the model — the
reader refuses it — and the tests run every vector through the real
`/bin/sh` as well, so the model of the shell is checked against one.
-/

namespace Clew.Ssh

open Clew.Text

/-! ## Quoting for a POSIX shell -/

/-- The characters `shell_command` leaves bare: none is special to a shell. -/
def bareChar (c : Char) : Bool :=
  isAsciiAlphanumeric c || c == '-' || c == '_' || c == '.' || c == '/' || c == '@' ||
    c == ':' || c == ',' || c == '+'

/-- A `'` inside single quotes: close, an escaped quote, open again. -/
def quoteChar (c : Char) : List Char := if c == '\'' then ['\'', '\\', '\'', '\''] else [c]

/-- `quote` (and `shell_command`'s quoted words): `'…'`, each `'` as `'\''`. -/
def quote (w : List Char) : List Char := '\'' :: (w.flatMap quoteChar ++ ['\''])

/-- One of `shell_command`'s words: bare when every character may be. -/
def shellWord (w : List Char) : List Char :=
  if !w.isEmpty && w.all bareChar then w else quote w

/-- `shell_command`: the words, one space between them. -/
def shellCommand : List (List Char) → List Char
  | [] => []
  | [w] => shellWord w
  | w :: ws => shellWord w ++ ' ' :: shellCommand ws

/-- `sh_c`: a script run under `sh`, refused when it holds a `'`. -/
def shC (script : List Char) : Option (List Char) :=
  if script.contains '\'' then none else some ("sh -c '".toList ++ script ++ ['\''])

/-- A character before a word read so far. -/
def cons1 (c : Char) (p : List Char × List Char) : List Char × List Char := (c :: p.1, p.2)

/-- One word as a POSIX shell reads it, in or out of single quotes, up to an
unquoted space: the word after quote removal, and what follows. `none` for
what the model does not cover — an unquoted special character, a quote left
open, a backslash at the end or before a newline. -/
def readWordAux : Bool → List Char → Option (List Char × List Char)
  | true, [] => none
  | true, c :: r => if c == '\'' then readWordAux false r else (readWordAux true r).map (cons1 c)
  | false, [] => some ([], [])
  | false, c :: r =>
    if c == ' ' then some ([], c :: r)
    else if c == '\'' then readWordAux true r
    else if c == '\\' then
      match r with
      | d :: r' => if d == '\n' then none else (readWordAux false r').map (cons1 d)
      | [] => none
    else if bareChar c then (readWordAux false r).map (cons1 c)
    else none

def readWord (s : List Char) : Option (List Char × List Char) := readWordAux false s

/-- The words of a command line, one space between them. -/
def readWordsAux : Nat → List Char → Option (List (List Char))
  | 0, _ => none
  | _ + 1, [] => some []
  | n + 1, s =>
    match readWord s with
    | some (w, []) => some [w]
    | some (w, ' ' :: r) => (readWordsAux n r).map (w :: ·)
    | _ => none

def readWords (s : List Char) : Option (List (List Char)) := readWordsAux (s.length + 1) s

/-- Whether the rest of a line ends a word: nothing, or a space. -/
def endsWord (rest : List Char) : Prop := rest = [] ∨ ∃ r, rest = ' ' :: r

theorem readWordAux_end (rest : List Char) (h : endsWord rest) :
    readWordAux false rest = some ([], rest) := by
  rcases h with rfl | ⟨r, rfl⟩
  · rfl
  · conv => lhs; rw [readWordAux.eq_def]
    simp

theorem bareChar_facts (c : Char) (h : bareChar c = true) :
    c ≠ ' ' ∧ c ≠ '\'' ∧ c ≠ '\\' := by
  refine ⟨?_, ?_, ?_⟩ <;> (intro he; subst he; revert h; decide)

theorem rw_true_close (r : List Char) : readWordAux true ('\'' :: r) = readWordAux false r := by
  conv => lhs; rw [readWordAux.eq_def]
  simp

theorem rw_true_char (c : Char) (r : List Char) (hc : c ≠ '\'') :
    readWordAux true (c :: r) = (readWordAux true r).map (cons1 c) := by
  conv => lhs; rw [readWordAux.eq_def]
  simp [hc]

theorem rw_false_open (r : List Char) : readWordAux false ('\'' :: r) = readWordAux true r := by
  conv => lhs; rw [readWordAux.eq_def]
  simp

theorem rw_false_escape (d : Char) (r : List Char) (hd : d ≠ '\n') :
    readWordAux false ('\\' :: d :: r) = (readWordAux false r).map (cons1 d) := by
  conv => lhs; rw [readWordAux.eq_def]
  simp [hd]

theorem rw_false_bare (c : Char) (r : List Char) (hc : bareChar c = true) :
    readWordAux false (c :: r) = (readWordAux false r).map (cons1 c) := by
  obtain ⟨h1, h2, h3⟩ := bareChar_facts c hc
  conv => lhs; rw [readWordAux.eq_def]
  simp [h1, h2, h3, hc]

theorem map_cons1 (c : Char) (w : List Char) (o : Option (List Char × List Char)) :
    (o.map fun p => (w ++ p.1, p.2)).map (cons1 c) = o.map fun p => (c :: w ++ p.1, p.2) := by
  cases o <;> rfl

/-- Inside quotes, `quoteChar`'s text reads back as the text itself. -/
theorem readWordAux_quoted (w rest : List Char) :
    readWordAux true (w.flatMap quoteChar ++ '\'' :: rest) =
      (readWordAux false rest).map fun p => (w ++ p.1, p.2) := by
  induction w with
  | nil =>
    rw [List.flatMap_nil, List.nil_append, rw_true_close]
    cases readWordAux false rest <;> rfl
  | cons c w ih =>
    by_cases hc : c = '\''
    · subst hc
      have e : (('\'' :: w).flatMap quoteChar ++ '\'' :: rest) =
          '\'' :: '\\' :: '\'' :: '\'' :: (w.flatMap quoteChar ++ '\'' :: rest) := by
        simp [quoteChar]
      rw [e, rw_true_close, rw_false_escape _ _ (by decide), rw_false_open, ih, map_cons1]
    · have e : ((c :: w).flatMap quoteChar ++ '\'' :: rest) =
          c :: (w.flatMap quoteChar ++ '\'' :: rest) := by
        simp [quoteChar, hc]
      rw [e, rw_true_char c _ hc, ih, map_cons1]

/-- Bare characters read back as themselves. -/
theorem readWordAux_bare (w rest : List Char) (hw : w.all bareChar = true) :
    readWordAux false (w ++ rest) = (readWordAux false rest).map fun p => (w ++ p.1, p.2) := by
  induction w with
  | nil => cases h : readWordAux false rest <;> simp [h]
  | cons c w ih =>
    simp only [List.all_cons, Bool.and_eq_true] at hw
    rw [List.cons_append, rw_false_bare c _ hw.1, ih hw.2, map_cons1]

/-- **A quoted word is read back as the word**, whatever it holds. -/
theorem readWord_quote (w rest : List Char) (h : endsWord rest) :
    readWord (quote w ++ rest) = some (w, rest) := by
  unfold readWord quote
  rw [List.cons_append, rw_false_open, List.append_assoc, List.singleton_append,
    readWordAux_quoted, readWordAux_end rest h]
  simp

theorem readWord_shellWord (w rest : List Char) (h : endsWord rest) :
    readWord (shellWord w ++ rest) = some (w, rest) := by
  unfold shellWord
  split
  · rename_i hb
    simp only [Bool.and_eq_true] at hb
    unfold readWord
    rw [readWordAux_bare w rest hb.2, readWordAux_end rest h]
    simp
  · exact readWord_quote w rest h

theorem shellWord_ne_nil (w : List Char) : shellWord w ≠ [] := by
  unfold shellWord quote
  split
  · rename_i hb
    simp only [Bool.and_eq_true, Bool.not_eq_true', List.isEmpty_eq_false_iff] at hb
    exact hb.1
  · simp

/-- Written words, one space between them. -/
def joinWords : List (List Char) → List Char
  | [] => []
  | [e] => e
  | e :: es => e ++ ' ' :: joinWords es

/-- A word written as `e`, which a shell reads back as `w` before any end of
a word. -/
def ReadsAs (e w : List Char) : Prop :=
  e ≠ [] ∧ ∀ rest, endsWord rest → readWord (e ++ rest) = some (w, rest)

theorem joinWords_length (es : List (List Char)) (h : ∀ e ∈ es, e ≠ []) :
    es.length ≤ (joinWords es).length := by
  induction es with
  | nil => simp [joinWords]
  | cons e es ih =>
    have he := List.length_pos_iff.mpr (h e (by simp))
    have ih := ih (fun x hx => h x (by simp [hx]))
    cases es with
    | nil => simp [joinWords]; omega
    | cons e' es' =>
      simp only [joinWords] at ih ⊢
      simp only [List.length_append, List.length_cons] at ih ⊢
      omega

theorem readWordsAux_join (ps : List (List Char × List Char)) (hp : ∀ p ∈ ps, ReadsAs p.1 p.2)
    (n : Nat) (hn : ps.length ≤ n) :
    readWordsAux (n + 1) (joinWords (ps.map Prod.fst)) = some (ps.map Prod.snd) := by
  induction ps generalizing n with
  | nil => simp [joinWords, readWordsAux]
  | cons p ps ih =>
    obtain ⟨hne, hread⟩ := hp p (by simp)
    cases ps with
    | nil =>
      have hw := hread [] (Or.inl rfl)
      simp only [List.append_nil] at hw
      simp only [List.map_cons, List.map_nil, joinWords]
      cases hs : p.1 with
      | nil => exact absurd hs hne
      | cons a b =>
        rw [hs] at hw
        simp [readWordsAux, hw]
    | cons p' ps' =>
      have hw := hread (' ' :: joinWords ((p' :: ps').map Prod.fst)) (Or.inr ⟨_, rfl⟩)
      obtain ⟨m, rfl⟩ : ∃ m, n = m + 1 := ⟨n - 1, by simp at hn; omega⟩
      have hrest := ih (fun x hx => hp x (by simp [hx])) m (by simp at hn ⊢; omega)
      simp only [List.map_cons, joinWords] at hrest hw ⊢
      cases hs : p.1 with
      | nil => exact absurd hs hne
      | cons a b =>
        rw [hs] at hw
        rw [List.cons_append, readWordsAux]
        · rw [← List.cons_append, hw]
          simp [hrest]
        · simp

theorem readWords_join (ps : List (List Char × List Char)) (hp : ∀ p ∈ ps, ReadsAs p.1 p.2) :
    readWords (joinWords (ps.map Prod.fst)) = some (ps.map Prod.snd) := by
  unfold readWords
  apply readWordsAux_join ps hp
  have := joinWords_length (ps.map Prod.fst) (fun e he => by
    simp only [List.mem_map] at he
    obtain ⟨p, hp', rfl⟩ := he
    exact (hp p hp').1)
  simpa using this

theorem shellWord_readsAs (w : List Char) : ReadsAs (shellWord w) w :=
  ⟨shellWord_ne_nil w, fun rest h => readWord_shellWord w rest h⟩

theorem shellCommand_join (ws : List (List Char)) :
    shellCommand ws = joinWords ((ws.map fun w => (shellWord w, w)).map Prod.fst) := by
  induction ws with
  | nil => rfl
  | cons w ws ih =>
    cases ws with
    | nil => rfl
    | cons w' ws' => simp only [shellCommand, List.map_cons, joinWords] at ih ⊢; rw [ih]

/-- **What `shell_command` writes, a shell reads as exactly those words**:
nothing in a word can end it, start another, or expand. -/
theorem readWords_shellCommand (ws : List (List Char)) : readWords (shellCommand ws) = some ws := by
  rw [shellCommand_join]
  have := readWords_join (ws.map fun w => (shellWord w, w)) (by
    intro p hp
    simp only [List.mem_map] at hp
    obtain ⟨w, -, rfl⟩ := hp
    exact shellWord_readsAs w)
  simpa [Function.comp_def] using this

/-- **A remote script runs as written**: `sh -c '<script>'` is read as `sh`,
`-c` and the script, for any script `sh_c` accepts. -/
theorem readWords_shC (script cmd : List Char) (h : shC script = some cmd) :
    readWords cmd = some ["sh".toList, "-c".toList, script] := by
  unfold shC at h
  split at h
  · cases h
  · rename_i hq
    cases h
    have hfm : script.flatMap quoteChar = script := by
      induction script with
      | nil => rfl
      | cons c s ih =>
        have hc : (c == '\'') = false := by
          have : c ≠ '\'' := fun he => hq (by simp [he])
          simpa using this
        simp only [List.flatMap_cons, quoteChar, hc, Bool.false_eq_true, ite_false,
          List.singleton_append]
        rw [ih (fun h' => hq (by simp at h' ⊢; exact Or.inr h'))]
    have hsh : ReadsAs "sh".toList "sh".toList := by
      have := shellWord_readsAs "sh".toList
      rwa [show shellWord "sh".toList = "sh".toList by decide] at this
    have hc : ReadsAs "-c".toList "-c".toList := by
      have := shellWord_readsAs "-c".toList
      rwa [show shellWord "-c".toList = "-c".toList by decide] at this
    have hs : ReadsAs (quote script) script :=
      ⟨by simp [quote], fun rest h => readWord_quote script rest h⟩
    have := readWords_join [("sh".toList, "sh".toList), ("-c".toList, "-c".toList),
      (quote script, script)] (by
        intro p hp
        simp only [List.mem_cons, List.not_mem_nil, or_false] at hp
        rcases hp with rfl | rfl | rfl
        · exact hsh
        · exact hc
        · exact hs)
    simp only [List.map_cons, List.map_nil, joinWords, quote, hfm] at this
    simpa using this

/-! ## ssh's arguments -/

/-- The `ssh` options that take a value (`SSH_VALUE_OPTIONS`). -/
def isValueLetter (c : Char) : Bool := "BbcDEeFIiJLlmOoPpQRSWw".toList.contains c

/-- The first letter of an option cluster that takes a value, and the rest
of the word after it. -/
def clusterValue : List Char → Option (Char × List Char)
  | [] => none
  | c :: r => if isValueLetter c then some (c, r) else clusterValue r

/-- The options at the front of a list of words, read as `parse_ssh_args`
reads them: the options, whether a `--` ended them, and the words left. -/
def readOpts : List (List Char) → List (Char × List Char) × Bool × List (List Char)
  | [] => ([], false, [])
  | w :: ws =>
    if w == ['-', '-'] then ([], true, ws)
    else
      match w with
      | c :: b =>
        if c == '-' && !b.isEmpty then
          match clusterValue b with
          | none => readOpts ws
          | some (l, []) =>
            match ws with
            | [] => ([], false, [])
            | v :: ws' => let r := readOpts ws'; ((l, v) :: r.1, r.2.1, r.2.2)
          | some (l, rest) => let r := readOpts ws; ((l, rest) :: r.1, r.2.1, r.2.2)
        else ([], false, w :: ws)
      | [] => ([], false, w :: ws)

/-- `SshArgs`. -/
structure Parsed where
  options : List (Char × List Char)
  destination : Option (List Char)
  before : Nat
  deriving DecidableEq, Repr

/-- `parse_ssh_args`: options, the destination, and options after it unless
a `--` came first. -/
def parseSshArgs (args : List (List Char)) : Parsed :=
  match readOpts args with
  | (o, _, []) => ⟨o, none, 0⟩
  | (o, true, d :: _) => ⟨o, some d, o.length⟩
  | (o, false, d :: rest) => ⟨o ++ (readOpts rest).1, some d, o.length⟩

/-- `config_value`: the value of `-o Key=value` when its key is `key`. -/
def configValue (option key : List Char) : Option (List Char) :=
  match splitOnce '=' option with
  | some (k, v) => if (trim k).map toAsciiLowercase == key then some (trim v) else none
  | none => none

def userOf (o : Char × List Char) : Option (List Char) :=
  if o.1 == 'l' then some o.2 else if o.1 == 'o' then configValue o.2 "user".toList else none

def portOf (o : Char × List Char) : Option (List Char) :=
  if o.1 == 'p' then some o.2 else if o.1 == 'o' then configValue o.2 "port".toList else none

/-- `rsplit_once`: split at the last `c`. -/
def rsplitOnce (c : Char) (s : List Char) : Option (List Char × List Char) :=
  (splitOnce c s.reverse).map fun p => (p.2.reverse, p.1.reverse)

def hexVal (b : Nat) : Option Nat :=
  if 48 ≤ b ∧ b ≤ 57 then some (b - 48)
  else if 97 ≤ b ∧ b ≤ 102 then some (b - 87)
  else if 65 ≤ b ∧ b ≤ 70 then some (b - 55)
  else none

/-- `url_decode` on bytes: `+` a space, `%XX` a byte other than NUL. -/
def urlDecodeBytes : List Nat → Option (List Nat)
  | [] => some []
  | b :: r =>
    if b == 43 then (urlDecodeBytes r).map (32 :: ·)
    else if b == 37 then
      match r with
      | x :: y :: r' =>
        match hexVal x, hexVal y with
        | some hi, some lo =>
          if 16 * hi + lo == 0 then none else (urlDecodeBytes r').map ((16 * hi + lo) :: ·)
        | _, _ => none
      | _ => none
    else (urlDecodeBytes r).map (b :: ·)

/-- `url_decode`: the user of an `ssh://` URI, decoded, and UTF-8 after. -/
def urlDecode (text : List Char) : Option (List Char) := do
  let bytes ← urlDecodeBytes ((String.ofList text).toUTF8.toList.map (·.toNat))
  let s ← String.fromUTF8? (ByteArray.mk (bytes.map UInt8.ofNat).toArray)
  pure s.toList

def domainChar (c : Char) : Bool := isAsciiAlphanumeric c || c == '.' || c == '-' || c == '_'

def hasDoubleDot : List Char → Bool
  | a :: b :: r => (a == '.' && b == '.') || hasDoubleDot (b :: r)
  | _ => false

/-- `valid_domain`: a URI's host as ssh takes it, one final dot dropped. -/
def validDomain (h : List Char) : Option (List Char) :=
  match h with
  | [] => none
  | c :: _ =>
    if (isAsciiAlphanumeric c || c == '_') && !hasDoubleDot h && h.all domainChar then
      some (if h.getLast? == some '.' then h.dropLast else h)
    else none

/-- An `ssh://` URI's user: up to the first `@`, its `;params` dropped,
decoded; `none` for an empty or undecodable one. -/
def uriUser (uri : List Char) : Option (Option (List Char) × List Char) :=
  match splitOnce '@' uri with
  | some (info, rest) =>
    let user := match splitOnce ';' info with
      | some (u, _) => u
      | none => info
    if user.isEmpty then none else (urlDecode user).map fun u => (some u, rest)
  | none => some (none, uri)

def hostDelim (c : Char) : Bool := c == ':' || c == '/'

/-- A URI's host, bracketed or up to a `:` or `/`, and what follows it. -/
def uriHost (rest : List Char) : Option (List Char × List Char) :=
  match rest with
  | c :: inner =>
    if c == '[' then splitOnce ']' inner
    else some (rest.takeWhile (fun d => !hostDelim d), rest.dropWhile (fun d => !hostDelim d))
  | [] => some ([], [])

/-- A URI's port after its host: none, `:port`, or a bare `/`. -/
def uriPort (after : List Char) : Option (Option (List Char)) :=
  match after with
  | [] => some none
  | c :: a =>
    if c == ':' then
      match splitOnce '/' a with
      | some (q, path) => if !path.isEmpty || q.isEmpty then none else some (some q)
      | none => if a.isEmpty then none else some (some a)
    else if c == '/' && a.isEmpty then some none
    else none

def readUri (uri : List Char) : Option (Option (List Char) × List Char × Option (List Char)) :=
  match uriUser uri with
  | none => none
  | some (user, rest) =>
    match uriHost rest with
    | none => none
    | some (hostRaw, after) =>
      match validDomain hostRaw, uriPort after with
      | some host, some port => some (user, host, port)
      | _, _ => none

/-- `read_destination`: user, host and port, `ssh://` URIs included. -/
def readDestination (dest : List Char) :
    Option (Option (List Char) × List Char × Option (List Char)) :=
  if "ssh://".toList.isPrefixOf dest then readUri (dest.drop 6)
  else
    match rsplitOnce '@' dest with
    | some (u, h) => some (some u, h, none)
    | none => some (none, dest, none)

/-- `ssh_endpoint`: the first user and port given, in the order the words
come, the destination's among them. -/
def sshEndpoint (args : List (List Char)) :
    Option (Option (List Char) × List Char × Option (List Char)) := do
  let p := parseSshArgs args
  let d ← p.destination
  let (du, host, dp) ← readDestination d
  let front := p.options.take p.before
  let back := p.options.drop p.before
  let user := match front.findSome? userOf with
    | some u => some u
    | none => match du with
      | some u => some u
      | none => back.findSome? userOf
  let port := match front.findSome? portOf with
    | some q => some q
    | none => match dp with
      | some q => some q
      | none => back.findSome? portOf
  pure (user, host, port)

/-- `str::parse::<u16>`. -/
def parseU16 (s : List Char) : Option Nat :=
  (parseU64 s).bind fun n => if n < 65536 then some n else none

/-- `label_port`: none for 22 or none given, a number as a number, anything
else as written. -/
def labelPort : Option (List Char) → Option (List Char)
  | none => none
  | some p =>
    match parseU16 p with
    | some n => if n == 22 then none else some (decimal n)
    | none => some p

/-- The label `CLEW_SSH` with these arguments is known by. -/
def clewSshLabel (args : List (List Char)) : Option (List Char) :=
  (sshEndpoint args).map fun e => Clew.Trust.scopeLabel e.1 e.2.1 (labelPort e.2.2)

/-- The options every saved connection passes. -/
def fixedOptions : List (List Char) :=
  [['-', 'o'], "ConnectTimeout=10".toList, ['-', 'o'], "StrictHostKeyChecking=yes".toList,
    ['-', 'o'], "ServerAliveInterval=15".toList, ['-', 'o'], "ServerAliveCountMax=3".toList,
    ['-', 'o'], "NumberOfPasswordPrompts=1".toList]

/-- `SavedConnection::ssh_args`. -/
def sshArgs (user host : List Char) (port : Nat) (identity : List Char) : List (List Char) :=
  fixedOptions ++ ((if port == 22 then [] else [['-', 'p'], decimal port]) ++
    ((if identity.isEmpty then [] else [['-', 'i'], identity]) ++
      [['-', '-'], user ++ '@' :: host]))

/-- The options a saved connection means ssh to read. -/
def intendedFixed : List (Char × List Char) :=
  [('o', "ConnectTimeout=10".toList), ('o', "StrictHostKeyChecking=yes".toList),
    ('o', "ServerAliveInterval=15".toList), ('o', "ServerAliveCountMax=3".toList),
    ('o', "NumberOfPasswordPrompts=1".toList)]

def intendedRest (port : Nat) (identity : List Char) : List (Char × List Char) :=
  (if port == 22 then [] else [('p', decimal port)]) ++
    (if identity.isEmpty then [] else [('i', identity)])

def intended (port : Nat) (identity : List Char) : List (Char × List Char) :=
  intendedFixed ++ intendedRest port identity

/-- A saved connection's label (`SavedConnection::target`). -/
def savedLabel (user host : List Char) (port : Nat) : List Char :=
  Clew.Trust.scopeLabel (some user) host (labelPort (some (decimal port)))

theorem readOpts_valued (l : Char) (v : List Char) (ws : List (List Char))
    (hl : isValueLetter l = true) :
    readOpts (['-', l] :: v :: ws) =
      ((l, v) :: (readOpts ws).1, (readOpts ws).2.1, (readOpts ws).2.2) := by
  have hl' : l ≠ '-' := by intro h; subst h; exact absurd hl (by decide)
  rw [readOpts.eq_def]
  simp [hl', clusterValue, hl]

theorem readOpts_dashdash (ws : List (List Char)) : readOpts (['-', '-'] :: ws) = ([], true, ws) := by
  rw [readOpts.eq_def]; simp

theorem readOpts_fixed (X : List (List Char)) :
    readOpts (fixedOptions ++ X) =
      (intendedFixed ++ (readOpts X).1, (readOpts X).2.1, (readOpts X).2.2) := by
  simp only [fixedOptions, List.cons_append, List.nil_append]
  repeat rw [readOpts_valued _ _ _ (by decide)]
  rfl

theorem readOpts_rest (user host : List Char) (port : Nat) (identity : List Char) :
    readOpts ((if port == 22 then [] else [['-', 'p'], decimal port]) ++
      ((if identity.isEmpty then [] else [['-', 'i'], identity]) ++
        [['-', '-'], user ++ '@' :: host])) =
      (intendedRest port identity, true, [user ++ '@' :: host]) := by
  unfold intendedRest
  by_cases hp : (port == 22) = true <;> by_cases hi : identity.isEmpty = true <;>
    simp only [hp, hi, ite_true, Bool.false_eq_true, ite_false, List.nil_append,
      List.cons_append] <;>
    (repeat rw [readOpts_valued _ _ _ (by decide)]) <;> rw [readOpts_dashdash] <;> rfl

/-- **No field of a saved connection is ever an option to ssh**: whatever
the user, host and identity hold, ssh reads exactly the options Clew meant
— its own, `-p` and `-i` — and the destination exactly `user@host`. -/
theorem parseSshArgs_sshArgs (user host : List Char) (port : Nat) (identity : List Char) :
    parseSshArgs (sshArgs user host port identity) =
      ⟨intended port identity, some (user ++ '@' :: host), (intended port identity).length⟩ := by
  unfold sshArgs parseSshArgs
  rw [readOpts_fixed, readOpts_rest]
  rfl

theorem isPrefixOf_at (p u h : List Char) (hp : '@' ∉ p) :
    p.isPrefixOf (u ++ '@' :: h) = p.isPrefixOf u := by
  induction p generalizing u with
  | nil => simp
  | cons a p ih =>
    have ha : a ≠ '@' := fun he => hp (he ▸ List.mem_cons_self)
    cases u with
    | nil => simp [List.isPrefixOf, ha]
    | cons b u =>
      simp only [List.cons_append, List.isPrefixOf_cons_cons]
      rw [ih u (fun h' => hp (List.mem_cons_of_mem _ h'))]

theorem rsplitOnce_at (u h : List Char) (hh : '@' ∉ h) : rsplitOnce '@' (u ++ '@' :: h) = some (u, h) := by
  unfold rsplitOnce
  have : (u ++ '@' :: h).reverse = h.reverse ++ '@' :: u.reverse := by simp
  rw [this, Clew.Trust.splitOnce_append _ _ (by simpa using hh)]
  simp

theorem parseU16_decimal (n : Nat) (h : n < 65536) : parseU16 (decimal n) = some n := by
  unfold parseU16
  rw [parseU64_decimal n (by omega)]
  simp [h]

theorem findSome_fixed_user : intendedFixed.findSome? userOf = none := by decide
theorem findSome_fixed_port : intendedFixed.findSome? portOf = none := by decide

theorem sshEndpoint_sshArgs (user host : List Char) (port : Nat) (identity : List Char)
    (hh : '@' ∉ host) (hu : "ssh://".toList.isPrefixOf user = false) :
    sshEndpoint (sshArgs user host port identity) =
      some (some user, host, if port == 22 then none else some (decimal port)) := by
  have hdest : readDestination (user ++ '@' :: host) = some (some user, host, none) := by
    unfold readDestination
    rw [isPrefixOf_at _ _ _ (by decide), hu]
    simp [rsplitOnce_at user host hh]
  have huser : (intended port identity).findSome? userOf = none := by
    unfold intended intendedRest
    rw [List.findSome?_append, findSome_fixed_user]
    by_cases hp : (port == 22) = true <;> by_cases hi : identity.isEmpty = true <;>
      simp [hp, hi, userOf]
  have hport : (intended port identity).findSome? portOf =
      if port == 22 then none else some (decimal port) := by
    unfold intended intendedRest
    rw [List.findSome?_append, findSome_fixed_port]
    by_cases hp : (port == 22) = true <;> by_cases hi : identity.isEmpty = true <;>
      simp [hp, hi, portOf]
  unfold sshEndpoint
  rw [parseSshArgs_sshArgs]
  simp [hdest, huser, hport]
  by_cases h22 : port = 22 <;> simp [h22]

/-- **A machine has one label, however it is reached**: the label a
`CLEW_SSH` with a saved connection's arguments gets is the label the saved
connection gets — so approvals given through either are filed under one
name. (A user whose name begins `ssh://` would turn the destination into a
URI; no other field matters.) -/
theorem clewSshLabel_sshArgs (user host : List Char) (port : Nat) (identity : List Char)
    (hh : '@' ∉ host) (hu : "ssh://".toList.isPrefixOf user = false) (hp : port < 65536) :
    clewSshLabel (sshArgs user host port identity) = some (savedLabel user host port) := by
  unfold clewSshLabel savedLabel
  rw [sshEndpoint_sshArgs user host port identity hh hu]
  simp only [Option.map_some, Option.some.injEq]
  congr 1
  unfold labelPort
  by_cases h22 : port = 22
  · subst h22; simp [parseU16_decimal 22 (by decide)]
  · have h22' : (port == 22) = false := by simpa using h22
    simp [h22', parseU16_decimal port hp]

/-! ## known_hosts -/

/-- `known_hosts_pattern`: the host on port 22, `[host]:port` otherwise. -/
def knownHostsPattern (host : List Char) (port : Option Nat) : List Char :=
  match port with
  | some p => if p != 22 then '[' :: (host ++ "]:".toList ++ decimal p) else host
  | none => host

def nameChar (c : Char) : Bool :=
  isAsciiAlphanumeric c || c == '.' || c == '-' || c == '_' || c == ':'

def nameOk (n : List Char) : Bool := !n.isEmpty && n.length ≤ 255 && n.all nameChar

/-- `split_once("]:")`. -/
def splitBracketColon : List Char → Option (List Char × List Char)
  | [] => none
  | c :: r =>
    match r with
    | d :: r' => if c == ']' && d == ':' then some ([], r') else
        (splitBracketColon r).map fun p => (c :: p.1, p.2)
    | [] => none

/-- `plain_host_field`. -/
def plainHostField (f : List Char) : Bool :=
  match f with
  | c :: rest =>
    if c == '[' then
      match splitBracketColon rest with
      | some (name, port) => nameOk name && (match parseU16 port with
          | some p => p != 0
          | none => false)
      | none => false
    else nameOk f
  | [] => false

/-- The pattern syntax of a known_hosts host field: a wildcard, a negation,
a list, a hashed name, a marker, or a space. -/
def patternChar (c : Char) : Bool :=
  c == '*' || c == '?' || c == '!' || c == ',' || c == '|' || c == '@' || isWhitespace c

theorem nameChar_not_pattern (c : Char) (h : nameChar c = true) : patternChar c = false := by
  have hw : isWhitespace c = false := by
    simp only [nameChar, isAsciiAlphanumeric, isAsciiDigit, Bool.or_eq_true, Bool.and_eq_true,
      decide_eq_true_eq, beq_iff_eq] at h
    have hr : 45 ≤ c.toNat ∧ c.toNat ≤ 122 := by
      rcases h with ((((((⟨h1, h2⟩ | ⟨h1, h2⟩) | ⟨h1, h2⟩) | h) | h) | h) | h)
      · have a : 48 ≤ c.toNat := h1; have b : c.toNat ≤ 57 := h2; omega
      · have a : 97 ≤ c.toNat := h1; have b : c.toNat ≤ 122 := h2; omega
      · have a : 65 ≤ c.toNat := h1; have b : c.toNat ≤ 90 := h2; omega
      all_goals (subst h; decide)
    simp only [isWhitespace, Bool.or_eq_false_iff, Bool.and_eq_false_iff, beq_eq_false_iff_ne,
      ne_eq, decide_eq_false_iff_not, Nat.not_le]
    omega
  simp only [patternChar, hw, Bool.or_false, Bool.or_eq_false_iff, beq_eq_false_iff_ne, ne_eq]
  simp only [nameChar, isAsciiAlphanumeric, isAsciiDigit, Bool.or_eq_true, Bool.and_eq_true,
    decide_eq_true_eq, beq_iff_eq] at h
  refine ⟨⟨⟨⟨⟨?_, ?_⟩, ?_⟩, ?_⟩, ?_⟩, ?_⟩ <;> (intro he; subst he; simp at h)

theorem splitBracketColon_some (s name port : List Char) (h : splitBracketColon s = some (name, port)) :
    s = name ++ ']' :: ':' :: port := by
  induction s generalizing name with
  | nil => simp [splitBracketColon] at h
  | cons c r ih =>
    cases r with
    | nil => simp [splitBracketColon] at h
    | cons d r' =>
      rw [splitBracketColon.eq_def] at h
      by_cases hcd : (c == ']' && d == ':') = true
      · simp only [hcd, ite_true, Option.some.injEq, Prod.mk.injEq] at h
        obtain ⟨rfl, rfl⟩ := h
        simp only [Bool.and_eq_true, beq_iff_eq] at hcd
        obtain ⟨rfl, rfl⟩ := hcd
        rfl
      · have hcd' : (c == ']' && d == ':') = false := by simpa using hcd
        simp only [hcd', Bool.false_eq_true, ite_false] at h
        cases hr : splitBracketColon (d :: r') with
        | none => rw [hr] at h; simp at h
        | some q =>
          rw [hr] at h
          simp only [Option.map_some, Option.some.injEq, Prod.mk.injEq] at h
          obtain ⟨rfl, rfl⟩ := h
          rw [ih q.1 hr]
          rfl

theorem parseU64_chars (s : List Char) (n : Nat) (h : parseU64 s = some n) :
    ∀ c ∈ s, isAsciiDigit c = true ∨ c = '+' := by
  have digits : ∀ ds : List Char, (if ds.isEmpty || !ds.all isAsciiDigit then none else
      (if digitsValue ds < 2 ^ 64 then some (digitsValue ds) else none)) = some n →
      ds.all isAsciiDigit = true := by
    intro ds hds
    cases ha : ds.all isAsciiDigit
    · simp [ha] at hds
    · rfl
  unfold parseU64 at h
  split at h
  · rename_i rest
    have := digits rest h
    intro c hc
    simp only [List.mem_cons] at hc
    rcases hc with rfl | hc
    · exact Or.inr rfl
    · exact Or.inl (List.all_eq_true.mp this c hc)
  · have := digits s h
    intro c hc
    exact Or.inl (List.all_eq_true.mp this c hc)

theorem parseU16_chars (s : List Char) (n : Nat) (h : parseU16 s = some n) :
    ∀ c ∈ s, isAsciiDigit c = true ∨ c = '+' := by
  unfold parseU16 at h
  cases hp : parseU64 s with
  | none => rw [hp] at h; simp at h
  | some m => exact parseU64_chars s m hp

theorem digit_not_pattern (c : Char) (h : isAsciiDigit c = true ∨ c = '+') : patternChar c = false := by
  rcases h with h | rfl
  · exact nameChar_not_pattern c (by simp [nameChar, isAsciiAlphanumeric, h])
  · decide

/-- **A plain field is a literal**: none of the syntax that would let one
recorded key vouch for other hosts — a wildcard, a negation, a list, a
hashed name, a marker — is in it. -/
theorem plainHostField_literal (f : List Char) (h : plainHostField f = true) :
    ∀ c ∈ f, patternChar c = false := by
  unfold plainHostField at h
  split at h
  · rename_i c rest
    by_cases hc : c = '['
    · subst hc
      simp only [beq_self_eq_true, ite_true] at h
      split at h
      · rename_i name port hsplit
        have hs := splitBracketColon_some rest name port hsplit
        simp only [Bool.and_eq_true] at h
        obtain ⟨hn, hp⟩ := h
        have hall : name.all nameChar = true := by
          simp only [nameOk, Bool.and_eq_true] at hn; exact hn.2
        split at hp
        · rename_i q hq
          intro x hx
          rw [hs] at hx
          simp only [List.mem_cons, List.mem_append] at hx
          rcases hx with rfl | hx | rfl | rfl | hx
          · decide
          · exact nameChar_not_pattern x (List.all_eq_true.mp hall x hx)
          · decide
          · decide
          · exact digit_not_pattern x (parseU16_chars port q hq x hx)
        · cases hp
      · cases h
    · have hc' : (c == '[') = false := by simpa using hc
      simp only [hc', Bool.false_eq_true, ite_false] at h
      have hall : (c :: rest).all nameChar = true := by
        simp only [nameOk, Bool.and_eq_true] at h; exact h.2
      intro x hx
      exact nameChar_not_pattern x (List.all_eq_true.mp hall x hx)
  · cases h

theorem splitBracketColon_append (xs ys : List Char) (hx : ']' ∉ xs) :
    splitBracketColon (xs ++ ']' :: ':' :: ys) = some (xs, ys) := by
  induction xs with
  | nil => simp [splitBracketColon]
  | cons x xs ih =>
    have hx' : x ≠ ']' := fun he => hx (he ▸ List.mem_cons_self)
    have ih := ih (fun h' => hx (List.mem_cons_of_mem _ h'))
    cases xs with
    | nil => simp [splitBracketColon]
    | cons y ys' =>
      simp only [List.cons_append] at ih ⊢
      rw [splitBracketColon]
      simp [hx', ih]

theorem nameOk_no_bracket (n : List Char) (h : nameOk n = true) : ']' ∉ n ∧ ∀ c, n.head? = some c → c ≠ '[' := by
  have hall : n.all nameChar = true := by simp only [nameOk, Bool.and_eq_true] at h; exact h.2
  refine ⟨fun hm => ?_, fun c hc he => ?_⟩
  · have := List.all_eq_true.mp hall ']' hm; revert this; decide
  · subst he
    have := List.all_eq_true.mp hall '[' (List.mem_of_mem_head? hc); revert this; decide

/-- **The field Clew writes a host's key under is plain** — for a host
name it accepts and any port. -/
theorem knownHostsPattern_plain (host : List Char) (p : Nat) (hn : nameOk host = true)
    (hp : 0 < p) (hp' : p < 65536) : plainHostField (knownHostsPattern host (some p)) = true := by
  obtain ⟨hnb, hhead⟩ := nameOk_no_bracket host hn
  unfold knownHostsPattern
  by_cases h22 : p = 22
  · subst h22
    simp only [bne_self_eq_false, Bool.false_eq_true, ite_false]
    cases hh : host with
    | nil => simp [hh, nameOk] at hn
    | cons c r =>
      have hc : (c == '[') = false := by simpa using hhead c (by simp [hh])
      unfold plainHostField
      simp only [hc, Bool.false_eq_true, ite_false]
      rw [← hh]; exact hn
  · have hne : (p != 22) = true := by simpa using h22
    simp only [hne, ite_true]
    unfold plainHostField
    simp only [beq_self_eq_true, ite_true, List.append_assoc]
    rw [show "]:".toList ++ decimal p = ']' :: ':' :: decimal p by rfl,
      splitBracketColon_append _ _ hnb]
    simp only [hn, Bool.true_and, parseU16_decimal p hp']
    simp
    omega

/-- **One field, one host and port**: two hosts Clew accepts, on two
ports, are written under the same field only if they are the same. -/
theorem knownHostsPattern_injective (h h' : List Char) (p p' : Nat) (hn : nameOk h = true)
    (hn' : nameOk h' = true) (hp : p < 65536) (hp' : p' < 65536)
    (heq : knownHostsPattern h (some p) = knownHostsPattern h' (some p')) : h = h' ∧ p = p' := by
  obtain ⟨hb, hh⟩ := nameOk_no_bracket h hn
  obtain ⟨hb', hh'⟩ := nameOk_no_bracket h' hn'
  unfold knownHostsPattern at heq
  by_cases h22 : p = 22 <;> by_cases h22' : p' = 22
  · subst h22; subst h22'; simp at heq; exact ⟨heq, rfl⟩
  · subst h22
    have : (p' != 22) = true := by simpa using h22'
    simp only [bne_self_eq_false, Bool.false_eq_true, ite_false, this, ite_true] at heq
    exact absurd rfl (hh '[' (by rw [heq]; rfl))
  · subst h22'
    have : (p != 22) = true := by simpa using h22
    simp only [bne_self_eq_false, Bool.false_eq_true, ite_false, this, ite_true] at heq
    exact absurd rfl (hh' '[' (by rw [← heq]; rfl))
  · have e1 : (p != 22) = true := by simpa using h22
    have e2 : (p' != 22) = true := by simpa using h22'
    simp only [e1, e2, ite_true, List.cons.injEq, true_and, List.append_assoc] at heq
    have s1 := splitBracketColon_append h (decimal p) hb
    have s2 := splitBracketColon_append h' (decimal p') hb'
    rw [show "]:".toList ++ decimal p = ']' :: ':' :: decimal p by rfl] at heq
    rw [show "]:".toList ++ decimal p' = ']' :: ':' :: decimal p' by rfl] at heq
    rw [heq, s2] at s1
    simp only [Option.some.injEq, Prod.mk.injEq] at s1
    obtain ⟨rfl, hd⟩ := s1
    refine ⟨rfl, ?_⟩
    have := parseU16_decimal p hp
    rw [← hd, parseU16_decimal p' hp'] at this
    simpa using this.symm

end Clew.Ssh
