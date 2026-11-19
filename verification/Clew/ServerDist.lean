import Clew.Text

/-!
# Which server binary may be deployed — `clew_core::server_dist`

Connecting to a remote, Clew downloads the `clew-server` built for that
machine and runs it there over SSH. A release build embeds the SHA-256 of
every server binary the release built (`CLEW_SERVER_DIGESTS`), and a binary
is deployed only if its bytes hash to the digest embedded for exactly its
asset name. Without an embedded list (a development build) the `.sha256`
sidecar next to the download is all there is.

The remote's `uname` names the platform, and it is untrusted: `slug` turns
it into the token that is joined into a cache path and a download URL.

This file models `slug`, `asset_name`, `parse_digest_list`,
`embedded_digest`, `parse_sha256` and the accept/refuse decision
`server_binary` and `cached_if_valid` make, and proves:

* a slug is a non-empty run of `[A-Za-z0-9_-]` — whatever the remote said,
  and whatever Unicode lower-casing does — so it can neither climb out of
  the cache directory nor change the URL it is put into (the Rust check,
  `clew_rules::server_dist::slug_ok`, translated to Lean by Charon and
  Aeneas, is proved in `verification/extracted/Equal.lean` to decide what
  the check here decides, on the UTF-8 bytes of any string);
* an asset name names one platform and one protocol: two different pairs
  never share a name, so never share a pinned digest;
* with a list embedded, a binary is deployed only if it hashes to the digest
  pinned for its own asset, the sidecar plays no part, and an asset the
  list does not name is never deployed;
* every pinned digest is 64 lower-case hex digits under a non-empty name,
  and the shape the release writes its list in parses to exactly its entries.

The list and the sidecar are read by `clew_rules::server_dist::pinned_digest`
and `sidecar_digest`, proved in `verification/extracted/Equal.lean` to read
what `lookup (parseDigestList list) asset` and `parseSha256` here read, on
the UTF-8 bytes of any text.
-/

namespace Clew.ServerDist

open Clew.Text

/-! ## The model -/

/-- A slug character: `is_ascii_alphanumeric`, `-` or `_`. -/
def slugChar (c : Char) : Bool := isAsciiAlphanumeric c || c == '-' || c == '_'

/-- `slug`, for a lower-casing `lower` of the whole string
(`str::to_lowercase`, which looks at a character's neighbours for a final
`Σ`; it is a parameter because what is proved holds whatever it is). -/
def slug (lower : List Char → List Char) (platform : List Char) : Option (List Char) :=
  let s := (lower (trim platform)).map (fun c => if c == ' ' then '-' else c)
  if s.isEmpty || !s.all slugChar then none else some s

/-- `asset_name`: `clew-server-<slug>-p<protocol>`. -/
def assetName (slug : List Char) (protocol : Nat) : List Char :=
  "clew-server-".toList ++ slug ++ "-p".toList ++ decimal protocol

/-- `is_sha256_hex`: 64 hex digits. -/
def isSha256Hex (t : List Char) : Bool := t.length == 64 && t.all isAsciiHexdigit

/-- `str::to_ascii_lowercase`. -/
def lowerHex (t : List Char) : List Char := t.map toAsciiLowercase

/-- One entry of the list: `<asset>=<digest>`, or `sha256sum` output —
`<digest> <asset>` or `<digest> *<asset>`. Anything else is nothing. -/
def parseEntry (entry : List Char) : Option (List Char × List Char) :=
  match splitOnce '=' entry with
  | some (name, digest) =>
    let name := trim name
    let digest := trim digest
    if isSha256Hex digest && !name.isEmpty then some (name, lowerHex digest) else none
  | none =>
    match splitWhitespace entry with
    | [digest, name] =>
      let name := name.dropWhile (· == '*')
      if isSha256Hex digest && !name.isEmpty then some (name, lowerHex digest) else none
    | _ => none

/-- A separator between entries: a newline, `;` or `,`. -/
def entrySep (c : Char) : Bool := c == '\n' || c == ';' || c == ','

/-- `parse_digest_list`, as the `(asset, digest)` pairs it inserts, in order. -/
def parseDigestList (text : List Char) : List (List Char × List Char) :=
  ((split entrySep text).map trim).filterMap parseEntry

/-- The digest the list gives `asset`: the last one inserted (a `HashMap`
keeps the last insert). -/
def lookup (list : List (List Char × List Char)) (asset : List Char) : Option (List Char) :=
  (list.reverse.find? (·.1 == asset)).map (·.2)

/-- `embedded_digest`: no list — `ok none`, fall back to the sidecar; a list
that pins `asset` — its digest; a list that does not — refused. -/
def embeddedDigest (embedded : Option (List Char)) (asset : List Char) :
    Except Unit (Option (List Char)) :=
  match embedded with
  | none => .ok none
  | some list =>
    match lookup (parseDigestList list) asset with
    | some d => .ok (some d)
    | none => .error ()

/-- `parse_sha256`: the first whitespace-separated token that, lower-cased,
is 64 hex digits. -/
def parseSha256 (text : List Char) : Option (List Char) :=
  ((splitWhitespace text).map lowerHex).find? isSha256Hex

/-- What `server_binary` and `cached_if_valid` decide for a binary whose
SHA-256 is `actual` (as `hex_sha256` writes it: lower-case): the digest it
must have is the embedded one if there is a list, else the sidecar's; it is
deployed only if it has it (`verify`). -/
def accepts (embedded : Option (List Char)) (asset : List Char) (sidecar : Option (List Char))
    (actual : List Char) : Bool :=
  match embeddedDigest embedded asset with
  | .error () => false
  | .ok (some d) => actual == lowerHex d
  | .ok none =>
    match sidecar.bind parseSha256 with
    | some d => actual == lowerHex d
    | none => false

/-! ## The slug -/

/-- What passes `slug`'s final check. -/
theorem slug_check {t s : List Char}
    (h : (if (t.isEmpty || !t.all slugChar) = true then none else some t) = some s) :
    s ≠ [] ∧ ∀ c ∈ s, slugChar c = true := by
  cases he : t.isEmpty <;> cases ha : t.all slugChar <;> simp [he, ha] at h
  subst h
  refine ⟨?_, List.all_eq_true.mp ha⟩
  cases t with
  | nil => simp at he
  | cons _ _ => simp

/-- **A slug is a plain token.** Whatever the remote reports and however
characters lower-case, a slug is non-empty and made only of ASCII letters,
digits, `-` and `_`. -/
theorem slug_is_a_token (lower : List Char → List Char) (platform s : List Char)
    (h : slug lower platform = some s) : s ≠ [] ∧ ∀ c ∈ s, slugChar c = true := by
  unfold slug at h
  dsimp only at h
  exact slug_check h

/-- So a slug holds no `/` (no path separator, no URL path), no `.` (never
`.` or `..`), and nothing a URL reads as syntax (`?`, `#`, `%`, `:`, `@`). -/
theorem slug_has_no_path_or_url_syntax (lower : List Char → List Char) (platform s : List Char)
    (h : slug lower platform = some s) :
    ∀ c ∈ s, c ≠ '/' ∧ c ≠ '.' ∧ c ≠ '?' ∧ c ≠ '#' ∧ c ≠ '%' ∧ c ≠ ':' ∧ c ≠ '@' := by
  intro c hc
  have hs := (slug_is_a_token lower platform s h).2 c hc
  simp only [slugChar, isAsciiAlphanumeric, isAsciiDigit, Bool.or_eq_true, Bool.and_eq_true,
    decide_eq_true_eq, beq_iff_eq] at hs
  refine ⟨?_, ?_, ?_, ?_, ?_, ?_, ?_⟩ <;> (intro he; subst he; simp at hs)

/-! ## Asset names -/

theorem decimal_injective {m n : Nat} (h : decimal m = decimal n) : m = n := by
  have := congrArg digitsValue h
  simpa [digitsValue_decimal] using this

/-- **One name, one platform and protocol.** Two different
`(slug, protocol)` pairs never share an asset name — so never share the
digest the release pins under it. -/
theorem assetName_injective {s₁ s₂ : List Char} {p₁ p₂ : Nat}
    (h : assetName s₁ p₁ = assetName s₂ p₂) : s₁ = s₂ ∧ p₁ = p₂ := by
  -- Read backwards: the digits up to the `p` are the protocol, and what is
  -- left, past `-p`, is the slug behind the fixed prefix.
  have hrev := congrArg List.reverse h
  simp only [assetName, List.reverse_append, List.append_assoc] at hrev
  have hd : ∀ n, ∀ c ∈ (decimal n).reverse, isAsciiDigit c = true :=
    fun n c hc => decimal_digits n c (List.mem_reverse.mp hc)
  have hstop : ∀ (rest : List Char) c,
      ("-p".toList.reverse ++ rest).head? = some c → isAsciiDigit c = false := by
    intro rest c hc
    simp at hc
    subst hc
    decide
  have htake := congrArg (List.takeWhile isAsciiDigit) hrev
  rw [takeWhile_append_all _ _ (hd p₁) (hstop _),
    takeWhile_append_all _ _ (hd p₂) (hstop _)] at htake
  have hp : p₁ = p₂ := decimal_injective (List.reverse_inj.mp htake)
  subst hp
  refine ⟨?_, rfl⟩
  have hdrop := congrArg (List.dropWhile isAsciiDigit) hrev
  rw [dropWhile_append_all _ _ (hd p₁) (hstop _),
    dropWhile_append_all _ _ (hd p₁) (hstop _)] at hdrop
  simp only [List.append_cancel_left_eq, List.append_cancel_right_eq] at hdrop
  exact List.reverse_inj.mp hdrop

/-! ## The pinned digest decides -/

/-- **Pinned means pinned.** With a list embedded, a binary is deployed
only if it hashes to the digest the list gives its own asset. -/
theorem pinned_only (list asset : List Char) (sidecar : Option (List Char)) (actual : List Char)
    (h : accepts (some list) asset sidecar actual = true) :
    ∃ d, lookup (parseDigestList list) asset = some d ∧ actual = lowerHex d := by
  unfold accepts embeddedDigest at h
  cases hl : lookup (parseDigestList list) asset with
  | none => simp [hl] at h
  | some d => simp [hl] at h; exact ⟨d, rfl, h⟩

/-- **The sidecar has no say in a release build.** With a list embedded,
the decision is the same whatever sidecar is offered. -/
theorem sidecar_ignored (list asset : List Char) (s₁ s₂ : Option (List Char))
    (actual : List Char) : accepts (some list) asset s₁ actual = accepts (some list) asset s₂ actual := by
  cases hl : lookup (parseDigestList list) asset <;> simp [accepts, embeddedDigest, hl]

/-- **Unlisted means refused.** An asset the embedded list does not name
is never deployed, whatever its bytes and whatever its sidecar says. -/
theorem unlisted_refused (list asset : List Char) (sidecar : Option (List Char))
    (actual : List Char) (h : lookup (parseDigestList list) asset = none) :
    accepts (some list) asset sidecar actual = false := by
  simp [accepts, embeddedDigest, h]

/-! ## What the list holds -/

theorem lowerHex_length (t : List Char) : (lowerHex t).length = t.length := by
  simp [lowerHex]

theorem toAsciiLowercase_hex (c : Char) (h : isAsciiHexdigit c = true) :
    isAsciiHexdigit (toAsciiLowercase c) = true := by
  unfold toAsciiLowercase
  split
  · rename_i hup
    simp only [Bool.and_eq_true, decide_eq_true_eq] at hup
    have h1 : 65 ≤ c.toNat := hup.1
    simp only [isAsciiHexdigit, isAsciiDigit, Bool.or_eq_true, Bool.and_eq_true,
      decide_eq_true_eq] at h
    -- An upper-case hex digit is one of `A` to `F`.
    have hAF : c.toNat ≤ 70 := by
      rcases h with (⟨_, b⟩ | ⟨a, _⟩) | ⟨_, b⟩
      · have : c.toNat ≤ 57 := b; omega
      · have : 97 ≤ c.toNat := a
        have : c.toNat ≤ 90 := hup.2
        omega
      · exact b
    have hc : c = Char.ofNat c.toNat := (Char.ofNat_toNat c).symm
    have hk : c.toNat = 65 ∨ c.toNat = 66 ∨ c.toNat = 67 ∨ c.toNat = 68 ∨ c.toNat = 69 ∨
        c.toNat = 70 := by omega
    rcases hk with hk | hk | hk | hk | hk | hk <;> (rw [hc, hk]; decide)
  · exact h

/-- **Every pinned digest is well formed**: 64 hex digits, lower-case,
under a non-empty name. -/
theorem parseEntry_wellformed (entry : List Char) (name digest : List Char)
    (h : parseEntry entry = some (name, digest)) :
    name ≠ [] ∧ digest.length = 64 ∧ ∀ c ∈ digest, isAsciiHexdigit c = true := by
  unfold parseEntry at h
  have finish : ∀ (n d : List Char), isSha256Hex d = true → n ≠ [] →
      (n, lowerHex d) = (name, digest) →
      name ≠ [] ∧ digest.length = 64 ∧ ∀ c ∈ digest, isAsciiHexdigit c = true := by
    intro n d hd hn he
    simp only [Prod.mk.injEq] at he
    obtain ⟨rfl, rfl⟩ := he
    simp only [isSha256Hex, Bool.and_eq_true, beq_iff_eq, List.all_eq_true] at hd
    refine ⟨hn, by simp [lowerHex_length, hd.1], ?_⟩
    intro c hc
    simp only [lowerHex, List.mem_map] at hc
    obtain ⟨c', hc', rfl⟩ := hc
    exact toAsciiLowercase_hex c' (hd.2 c' hc')
  split at h
  · dsimp only at h
    split at h
    · rename_i hok
      simp only [Bool.and_eq_true, Bool.not_eq_true', List.isEmpty_eq_false_iff] at hok
      exact finish _ _ hok.1 hok.2 (Option.some.inj h)
    · cases h
  · split at h
    · dsimp only at h
      split at h
      · rename_i hok
        simp only [Bool.and_eq_true, Bool.not_eq_true', List.isEmpty_eq_false_iff] at hok
        exact finish _ _ hok.1 hok.2 (Option.some.inj h)
      · cases h
    · cases h

theorem parseDigestList_wellformed (text : List Char) :
    ∀ p ∈ parseDigestList text,
      p.1 ≠ [] ∧ p.2.length = 64 ∧ ∀ c ∈ p.2, isAsciiHexdigit c = true := by
  intro p hp
  simp only [parseDigestList, List.mem_filterMap] at hp
  obtain ⟨e, _, he⟩ := hp
  exact parseEntry_wellformed e p.1 p.2 he

/-! ## The release's own list parses to exactly its entries -/

/-- One line as `sha256sum` writes it, the shape the release concatenates
into `CLEW_SERVER_DIGESTS`: the digest, two spaces, the name, a newline. -/
def sumLine (asset digest : List Char) : List Char := digest ++ (' ' :: ' ' :: (asset ++ ['\n']))

theorem sumLine_append (asset digest rest : List Char) :
    sumLine asset digest ++ rest = (digest ++ ' ' :: ' ' :: asset) ++ '\n' :: rest := by
  simp [sumLine]

/-- A lower-case hex digit, as `sha256sum` writes them. -/
def lowerHexDigit (c : Char) : Bool := isAsciiDigit c || ('a' ≤ c && c ≤ 'f')

/-- A name the list can carry: not empty, not starting with `*`, and free
of whitespace, of the entry separators and of `=` — every release asset
name (`clew-server-<slug>-p<protocol>`) is one. -/
def plainName (asset : List Char) : Prop :=
  asset ≠ [] ∧ asset.head? ≠ some '*' ∧
    ∀ c ∈ asset, isWhitespace c = false ∧ entrySep c = false ∧ c ≠ '='

theorem lowerHexDigit_facts (c : Char) (h : lowerHexDigit c = true) :
    isAsciiHexdigit c = true ∧ isWhitespace c = false ∧ entrySep c = false ∧ c ≠ '=' ∧
      toAsciiLowercase c = c := by
  simp only [lowerHexDigit, isAsciiDigit, Bool.or_eq_true, Bool.and_eq_true,
    decide_eq_true_eq] at h
  have hr : (48 ≤ c.toNat ∧ c.toNat ≤ 57) ∨ (97 ≤ c.toNat ∧ c.toNat ≤ 102) := by
    rcases h with ⟨a, b⟩ | ⟨a, b⟩
    · exact .inl ⟨a, b⟩
    · exact .inr ⟨a, b⟩
  refine ⟨?_, ?_, ?_, ?_, ?_⟩
  · simp only [isAsciiHexdigit, isAsciiDigit, Bool.or_eq_true, Bool.and_eq_true,
      decide_eq_true_eq]
    rcases hr with ⟨a, b⟩ | ⟨a, b⟩
    · exact .inl (.inl ⟨a, b⟩)
    · exact .inl (.inr ⟨a, b⟩)
  · simp only [isWhitespace, Bool.or_eq_false_iff, Bool.and_eq_false_iff, beq_eq_false_iff_ne,
      ne_eq, decide_eq_false_iff_not, Nat.not_le]
    omega
  · have h1 : c ≠ '\n' := by intro he; subst he; simp at hr
    have h2 : c ≠ ';' := by intro he; subst he; simp at hr
    have h3 : c ≠ ',' := by intro he; subst he; simp at hr
    simp [entrySep, h1, h2, h3]
  · intro he; subst he; simp at hr
  · unfold toAsciiLowercase
    have : ¬ ('A' ≤ c ∧ c ≤ 'Z') := by
      intro ⟨a, b⟩
      have a' : 65 ≤ c.toNat := a
      have b' : c.toNat ≤ 90 := b
      omega
    simp [this]

theorem splitOnce_none (c : Char) (xs : List Char) (h : ∀ d ∈ xs, d ≠ c) : splitOnce c xs = none := by
  induction xs with
  | nil => rfl
  | cons d ds ih =>
    have hd : (d == c) = false := by simpa using h d (by simp)
    simp [splitOnce, hd, ih (fun e he => h e (by simp [he]))]

theorem trim_of_ends (xs : List Char) (hh : ∀ c, xs.head? = some c → isWhitespace c = false)
    (hl : ∀ c, xs.reverse.head? = some c → isWhitespace c = false) : trim xs = xs := by
  have hs : trimStart xs = xs := dropWhile_of_head _ _ hh
  have hr : xs.reverse.dropWhile isWhitespace = xs.reverse := dropWhile_of_head _ _ hl
  simp [trim, trimEnd, hs, hr]

/-- One line's content (its newline aside) parses to its entry. -/
theorem parseEntry_sumLine (asset digest : List Char) (ha : plainName asset)
    (hlen : digest.length = 64) (hd : ∀ c ∈ digest, lowerHexDigit c = true) :
    parseEntry (trim (digest ++ ' ' :: ' ' :: asset)) = some (asset, digest) := by
  obtain ⟨hne, hstar, hchars⟩ := ha
  have hdig := fun c hc => lowerHexDigit_facts c (hd c hc)
  have hdne : digest ≠ [] := by intro he; simp [he] at hlen
  -- Trimming changes nothing: a hex digit first, a name character last.
  have htrim : trim (digest ++ ' ' :: ' ' :: asset) = digest ++ ' ' :: ' ' :: asset := by
    apply trim_of_ends
    · intro c hc
      cases digest with
      | nil => exact absurd rfl hdne
      | cons d ds => simp at hc; subst hc; exact (hdig d (by simp)).2.1
    · intro c hc
      have hmem : c ∈ asset := by
        cases hra : asset.reverse with
        | nil => simp at hra; exact absurd hra hne
        | cons r rs =>
          simp [hra] at hc
          subst hc
          have : r ∈ asset.reverse := by rw [hra]; simp
          simpa using this
      exact (hchars c hmem).1
  rw [htrim]
  unfold parseEntry
  have hnoeq : splitOnce '=' (digest ++ ' ' :: ' ' :: asset) = none := by
    apply splitOnce_none
    intro d hd'
    simp only [List.mem_append, List.mem_cons] at hd'
    rcases hd' with h | h | h | h
    · exact (hdig d h).2.2.2.1
    · subst h; decide
    · subst h; decide
    · exact (hchars d h).2.2
  have hws : splitWhitespace (digest ++ ' ' :: ' ' :: asset) = [digest, asset] := by
    unfold splitWhitespace
    rw [split_at_sep _ digest ' ' _ (fun c hc => (hdig c hc).2.1) (by decide)]
    rw [show split isWhitespace (' ' :: asset) = [] :: split isWhitespace asset by
      simp [split, show isWhitespace ' ' = true by decide]]
    rw [split_none _ asset (fun c hc => (hchars c hc).1)]
    have hde : digest.isEmpty = false := by
      cases digest with
      | nil => exact absurd rfl hdne
      | cons _ _ => rfl
    have hae : asset.isEmpty = false := by
      cases asset with
      | nil => exact absurd rfl hne
      | cons _ _ => rfl
    simp [List.filter, hde, hae]
  have hdrop : asset.dropWhile (· == '*') = asset := by
    apply dropWhile_of_head
    intro c hc
    have : c ≠ '*' := by intro he; subst he; exact hstar hc
    simpa using this
  have hhex : isSha256Hex digest = true := by
    simp only [isSha256Hex, Bool.and_eq_true, beq_iff_eq, List.all_eq_true]
    exact ⟨hlen, fun c hc => (hdig c hc).1⟩
  have hlower : lowerHex digest = digest := by
    unfold lowerHex
    conv => rhs; rw [← List.map_id digest]
    apply List.map_congr_left
    intro c hc
    exact (hdig c hc).2.2.2.2
  have hasset : asset.isEmpty = false := by
    cases asset with
    | nil => exact absurd rfl hne
    | cons _ _ => rfl
  simp [hnoeq, hws, hdrop, hhex, hlower, hasset]

/-- The pieces between the newlines of the release's list. -/
theorem split_sumLines (entries : List (List Char × List Char))
    (h : ∀ e ∈ entries, plainName e.1 ∧ e.2.length = 64 ∧ ∀ c ∈ e.2, lowerHexDigit c = true) :
    split entrySep (entries.flatMap fun e => sumLine e.1 e.2) =
      entries.map (fun e => e.2 ++ ' ' :: ' ' :: e.1) ++ [[]] := by
  induction entries with
  | nil => rfl
  | cons e es ih =>
    obtain ⟨⟨_, _, hchars⟩, _, hd⟩ := h e (by simp)
    have hrest := ih (fun e' he' => h e' (by simp [he']))
    have hnosep : ∀ c ∈ e.2 ++ ' ' :: ' ' :: e.1, entrySep c = false := by
      intro c hc
      simp only [List.mem_append, List.mem_cons] at hc
      rcases hc with h | h | h | h
      · exact (lowerHexDigit_facts c (hd c h)).2.2.1
      · subst h; decide
      · subst h; decide
      · exact (hchars c h).2.1
    rw [List.flatMap_cons, sumLine_append, split_at_sep _ _ _ _ hnosep (by decide), hrest]
    simp

/-- **The release's list reads as written.** The digest list the release
assembles — each server's `sha256sum` line, concatenated — parses to
exactly its entries, in order: every asset pinned to its own digest, and
nothing else. -/
theorem release_list_parses (entries : List (List Char × List Char))
    (h : ∀ e ∈ entries, plainName e.1 ∧ e.2.length = 64 ∧ ∀ c ∈ e.2, lowerHexDigit c = true) :
    parseDigestList (entries.flatMap fun e => sumLine e.1 e.2) = entries := by
  unfold parseDigestList
  rw [split_sumLines entries h]
  simp only [List.map_append, List.map_map, List.filterMap_append]
  have hlast : List.filterMap parseEntry (List.map trim [[]]) = [] := by decide
  rw [hlast, List.append_nil]
  induction entries with
  | nil => rfl
  | cons e es ih =>
    obtain ⟨ha, hlen, hd⟩ := h e (by simp)
    simp only [List.map_cons, List.filterMap_cons, Function.comp_apply,
      parseEntry_sumLine e.1 e.2 ha hlen hd]
    rw [ih (fun e' he' => h e' (by simp [he']))]

end Clew.ServerDist
