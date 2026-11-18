import Lean.Data.Json
import Clew

/-!
# The conformance vectors

`lake exe vectors <dir>` evaluates each model on a fixed set of inputs —
every string up to a small length over an alphabet chosen to reach each
branch of the code, and the edge cases named below — and writes one JSON
file per model to `<dir>`. The files are committed under
`verification/vectors/`, and the Rust test suite runs the real functions on
the same inputs and requires the model's answers: the theorems in `Clew/`
are about the models, and the vectors are what ties each model to the code
it describes.

`verification/check.sh` regenerates the files and fails if they differ
from what is committed, so a model change cannot leave the vectors behind.
-/

open Lean

namespace Vectors

/-- Every list of length at most `n` over `alphabet`, shortest first. -/
def allUpTo {α : Type} (alphabet : List α) : Nat → List (List α)
  | 0 => [[]]
  | n + 1 =>
    let shorter := allUpTo alphabet n
    shorter ++ ((shorter.filter (·.length == n)).flatMap fun w => alphabet.map (w ++ [·]))

def str (s : List Char) : Json := Json.str (String.ofList s)

/-- Every `White_Space` code point, the code points either side of each run
of them, and two that look like spaces and are not (U+180E, which left the
property in Unicode 6.3, and the byte-order mark): where `Text.isWhitespace`
could part from `char::is_whitespace`, a vector sits. -/
def spaceEdges : List Char :=
  [0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x1F, 0x20, 0x21, 0x84, 0x85, 0x86, 0x9F, 0xA0,
    0xA1, 0x167F, 0x1680, 0x1681, 0x180E, 0x1FFF, 0x2000, 0x2001, 0x2002, 0x2003, 0x2004, 0x2005,
    0x2006, 0x2007, 0x2008, 0x2009, 0x200A, 0x200B, 0x2027, 0x2028, 0x2029, 0x202A, 0x202E,
    0x202F, 0x2030, 0x205E, 0x205F, 0x2060, 0x2FFF, 0x3000, 0x3001, 0xFEFF].map Char.ofNat

/-- A `(input, output)` pair. -/
def pair (a b : Json) : Json := Json.arr #[a, b]

def confine : Json :=
  let alphabet := ['a', '.', '/', ' ', ' ']
  let extra := ["../x", "a/..", "..a", ".../", "src/", "src/.", ".", "./a", "a/./b",
    " ", "a ", "\t", "\n", "\u0085", "​", " /a", "//", "a//b", "..", "a/../b"]
  let spaced : List (List Char) := spaceEdges.flatMap fun w =>
    [[w], [w, 'a'], ['a', w], [w, '/', 'a'], [w, '.', '.'], ['a', w, '/', '.', '.']]
  let inputs : List (List Char) := allUpTo alphabet 5 ++ extra.map String.toList ++ spaced
  let lexical := inputs.map fun s =>
    pair (str s) (Json.str <| match Clew.Confine.checkLexical s with
      | .ok () => "ok"
      | .error .empty => "empty"
      | .error .absolute => "absolute"
      | .error .traversal => "traversal")
  let folder := inputs.map fun s => pair (str s) (Json.bool (Clew.Confine.namesAFolder s))
  Json.mkObj [("check_lexical", Json.arr lexical.toArray),
    ("names_a_folder", Json.arr folder.toArray)]

def version (v : Clew.Update.Version) : Json :=
  Json.arr #[toJson v.major, toJson v.minor, toJson v.patch]

def update : Json :=
  -- Short strings over the characters that steer `Version::parse`, every one.
  let tiny : List (List Char) := allUpTo ['1', '.', 'v', '-', '+', ' '] 4
  -- Shaped ones: a prefix, three parts (in range, empty, signed, padded,
  -- one past `u64::MAX`), a suffix.
  let prefixes := ["", "v", "vv", " v"]
  let parts := ["0", "1", "+2", " 3 ", "", "18446744073709551616"]
  let suffixes := ["", "-rc1", "+build.5", ".4", "-"]
  let shaped : List (List Char) := prefixes.flatMap fun p => parts.flatMap fun a =>
    parts.flatMap fun b => parts.flatMap fun c => suffixes.map fun x =>
      (p ++ a ++ "." ++ b ++ "." ++ c ++ x).toList
  let edge : List (List Char) := ["18446744073709551615.0.0", "1.2", "1.2.3.4", "x1.2.3",
    "V1.2.3", "\u00A01.2.3\u2003", "1 .2.3", "1.2.3 -beta"].map String.toList
  let spaced : List (List Char) := spaceEdges.flatMap fun w =>
    [w :: "1.2.3".toList, "1.2.3".toList ++ [w], "1.".toList ++ w :: "2.3".toList,
      'v' :: w :: "1.2.3".toList]
  let parses := (tiny ++ shaped ++ edge ++ spaced).map fun s =>
    pair (str s) (match Clew.Update.parse s with
      | some v => version v
      | none => Json.null)
  let small : List Clew.Update.Version := (List.range 3).flatMap fun a =>
    (List.range 3).flatMap fun b => (List.range 3).map fun c => ⟨a, b, c⟩
  let order := small.flatMap fun a => small.map fun b =>
    pair (Json.arr #[version a, version b]) (Json.bool (a.lt b))
  let running : List Clew.Update.Version := [⟨0, 2, 3⟩, ⟨0, 2, 4⟩, ⟨1, 0, 0⟩, ⟨0, 0, 0⟩]
  let candidates := ["0.2.3", "0.2.4", "v0.2.5", "0.3.0", "0.1.99", "1.0.0", "0.2.3-rc1",
    "0.2.4+1", "garbage", "", "0.2", "18446744073709551615.0.0", "0.10.0", "v1.0.0-rc1",
    "0.0.0", "0.0.1"]
  let upgrades := running.flatMap fun r => candidates.map fun c =>
    pair (Json.arr #[version r, Json.str c])
      (match Clew.Update.ensureUpgrade r c.toList with
        | .ok v => version v
        | .error .unreadable => Json.str "unreadable"
        | .error .notNewer => Json.str "not_newer")
  Json.mkObj [("parse", Json.arr parses.toArray), ("lt", Json.arr order.toArray),
    ("ensure_upgrade", Json.arr upgrades.toArray)]

/-- `char::to_lowercase` on the characters the slug vectors use: ASCII, and
some whose lower case is not themselves in one character or not ASCII. -/
def rustLowerChar (c : Char) : List Char :=
  if c == '\u212A' then ['k']                -- KELVIN SIGN
  else if c == '\u0130' then ['i', '\u0307']  -- I WITH DOT ABOVE
  else if c == '\u00C9' then ['\u00E9']       -- E WITH ACUTE
  else if c == '\u0391' then ['\u03B1']       -- ALPHA
  else [Clew.Text.toAsciiLowercase c]

/-- `str::to_lowercase` on those strings: each character's lower case, but a
`Σ` after a letter and before none is the final `ς`. -/
def rustLower (s : List Char) : List Char :=
  let letter := fun (c : Char) => c.isAlpha || c == '\u0391' || c == '\u03A3'
  let rec go : List Char → List Char → List Char
    | _, [] => []
    | before, c :: after =>
      (if c == '\u03A3' then
        (if before.head?.any letter && !(after.head?.any letter) then ['\u03C2'] else ['\u03C3'])
      else rustLowerChar c) ++ go (c :: before) after
  go [] s

def optStr : Option (List Char) → Json
  | some s => str s
  | none => Json.null

def serverDist : Json :=
  let alphabet := ['L', 'x', ' ', '-', '/', '.', '_', '8']
  let platforms : List (List Char) := allUpTo alphabet 3 ++ ["Linux x86_64", "Darwin arm64",
    " linux aarch64 ", "../etc", "linux/x86", "Linux\tx86", "LINUX X86_64", "linux.x86",
    "li_nux-1", "Linux\u00A0x86", "\u212A", "\u0130", "\u00C9", "x86%2F", "a?b", "a#b", "\u0391\u03A3", "\u03A3",
    "\u0391\u03A3\u0391", "x\u03A3 y"].map String.toList
  let platforms := platforms ++ spaceEdges.flatMap fun w =>
    ["Linux".toList ++ w :: "x86".toList, w :: "Linux".toList]
  let slugs := platforms.map fun p => pair (str p) (optStr (Clew.ServerDist.slug rustLower p))
  let assets := [("linux-x86_64", 27), ("a", 0), ("a-p1", 2), ("x", 1234567)].map fun (sl, k) =>
    pair (Json.arr #[Json.str sl, toJson k]) (str (Clew.ServerDist.assetName sl.toList k))
  let d1 := "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
  let digests := [d1, d1.toUpper, "abc123", "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ag"]
  let names := ["clew-server-linux-x86_64-p27", "*clew-server-x", "a b", ""]
  let shapes : List (String → String → String) := [
    fun d n => d ++ "  " ++ n, fun d n => n ++ "=" ++ d, fun d n => " " ++ n ++ " = " ++ d ++ " ",
    fun d n => d ++ " " ++ n ++ " extra", fun d _ => d]
  let entries := shapes.flatMap fun f => names.flatMap fun n => digests.map fun d => f d n
  let good := [d1 ++ "  clew-server-linux-x86_64-p27", "clew-server-linux-aarch64-p27=" ++ d1.toUpper,
    d1 ++ " *clew-server-macos", "garbage", "", "a=b=" ++ d1,
    "clew-server-linux-x86_64-p27=" ++ String.ofList (List.replicate 64 'a')]
  let seps := ["\n", ";", ",", "\r\n"]
  let pairs := good.flatMap fun a => good.flatMap fun b => seps.map fun sep => a ++ sep ++ b
  let spacedEntries : List (List Char) := spaceEdges.flatMap fun w =>
    [d1.toList ++ w :: "clew-server-x".toList, "clew-server-x=".toList ++ d1.toList ++ [w],
      w :: "clew-server-x=".toList ++ d1.toList]
  let texts : List (List Char) := (entries ++ pairs ++
    [String.join (good.map (· ++ "\n")), "", "\n\n;,"]).map String.toList ++ spacedEntries
  -- The map `parse_digest_list` returns: each name's last digest.
  let asMap (l : List (List Char × List Char)) : List (List Char × List Char) :=
    l.foldl (fun acc e => (acc.filter (·.1 != e.1)) ++ [e]) []
  let lists := texts.map fun t =>
    pair (str t) (Json.arr ((asMap (Clew.ServerDist.parseDigestList t)).map
      (fun (n, d) => Json.arr #[str n, str d])).toArray)
  let assetNames := ["clew-server-linux-x86_64-p27", "clew-server-linux-aarch64-p27",
    "clew-server-macos", "clew-server-x", "missing"]
  let embeddedLists : List (Option String) :=
    [none, some "", some (String.join (good.map (· ++ "\n"))), some (good.head! ++ ";" ++ good[1]!)]
  let embedded := embeddedLists.flatMap fun l => assetNames.map fun a =>
    pair (Json.arr #[(match l with | some t => Json.str t | none => Json.null), Json.str a])
      (match Clew.ServerDist.embeddedDigest (l.map String.toList) a.toList with
        | .ok none => Json.str "none"
        | .ok (some d) => str d
        | .error () => Json.str "refused")
  let sidecars : List (List Char) := (digests.map (· ++ "  clew-server-linux-x86_64-p27\n") ++
    digests ++ ["", "  ", "abc " ++ d1, d1.toUpper ++ " x", "x y z"]).map String.toList
  let sidecars := sidecars ++ spaceEdges.flatMap fun w =>
    [d1.toList ++ w :: "x".toList, w :: d1.toList, d1.toList ++ [w]]
  let sums := sidecars.map fun t => pair (str t) (optStr (Clew.ServerDist.parseSha256 t))
  -- `verify` of the bytes `abc`, whose SHA-256 is `d1`, against each expected digest.
  let verifies := (digests ++ ["", d1 ++ " "]).map fun e =>
    pair (Json.str e) (Json.bool (d1.toList == Clew.ServerDist.lowerHex e.toList))
  Json.mkObj [("slug", Json.arr slugs.toArray), ("asset_name", Json.arr assets.toArray),
    ("parse_digest_list", Json.arr lists.toArray), ("embedded_digest", Json.arr embedded.toArray),
    ("parse_sha256", Json.arr sums.toArray), ("verify_abc", Json.arr verifies.toArray)]

/-- A string's UTF-8 bytes, as the trust model's byte lists. -/
def utf8 (s : String) : List Nat := s.toUTF8.toList.map (·.toNat)

def bytesJson (bs : List Nat) : Json := Json.arr (bs.map toJson).toArray

def trust : Json :=
  let words := ["", "a", ";", ":", "1"]
  let argLists : List (List String) := [[]] ++ words.map ([·]) ++
    (words.flatMap fun a => words.map fun b => [a, b]) ++
    [["a;1:b"], ["é"], ["12345678901"], ["-p", "2222", "x y"]]
  let argsParts := argLists.map fun args =>
    pair (Json.arr (args.map Json.str).toArray) (bytesJson (Clew.Trust.argsPart (args.map utf8)))
  let partLists : List (List String) := [[], [""], ["", ""], ["a"], ["é"],
    ["clew.lsp.build.v1", "rust-analyzer"], ["/usr/bin/x", "1;0:", "s", "1.0", ""],
    ["clew.lsp.init-options.v1", "0", "pyright", "1.1", "{\"a\":1}"]]
  let prefixed := partLists.map fun parts =>
    pair (Json.arr (parts.map Json.str).toArray) (bytesJson (Clew.Trust.fields (parts.map utf8)))
  let users : List (Option String) := [none, some "u", some "a:b", some "x/y", some "p%q",
    some "a@b", some "[r]"]
  let hosts := ["h", "example.com", "::1", "2001:db8::1:2222", "2001:db8::1", "a/b", "h%25", "[x]",
    "a@b", ""]
  let ports : List (Option String) := [none, some "2222", some "1", some "http", some "a]b/c",
    some ""]
  let opt : Option String → Json := fun | some t => Json.str t | none => Json.null
  let labels := users.flatMap fun u => hosts.flatMap fun h => ports.map fun p =>
    pair (Json.arr #[opt u, Json.str h, opt p])
      (str (Clew.Trust.scopeLabel (u.map String.toList) h.toList (p.map String.toList)))
  let roots := ["/srv/p", "/b:/c", "/", "p", "2222:/srv/p", "./p", ".", "", "a/b"]
  let someLabels := [Clew.Trust.scopeLabel (some "u".toList) "h".toList none,
    Clew.Trust.scopeLabel (some "u".toList) "h".toList (some "2222".toList),
    Clew.Trust.scopeLabel (some "u".toList) "2001:db8::1".toList (some "2222".toList),
    Clew.Trust.scopeLabel none "a/b".toList (some "1".toList)]
  let keys := someLabels.flatMap fun l => roots.map fun r =>
    pair (Json.arr #[str l, Json.str r]) (str (Clew.Trust.approvalKey l r.toList))
  Json.mkObj [("args_part", Json.arr argsParts.toArray),
    ("length_prefixed", Json.arr prefixed.toArray), ("scope_label", Json.arr labels.toArray),
    ("project_key", Json.arr keys.toArray)]

def frameError : Clew.Framing.Error → String
  | .truncated => "truncated"
  | .headerLineTooLong => "header_line_too_long"
  | .tooManyHeaderLines => "too_many_header_lines"
  | .malformedHeader => "malformed_header"
  | .missingContentLength => "missing_content_length"
  | .invalidContentLength => "invalid_content_length"
  | .conflictingContentLength => "conflicting_content_length"
  | .tooLarge => "too_large"

def ascii (bs : List Nat) : Json := Json.str (String.ofList (bs.map Char.ofNat))

def framing : Json :=
  -- Every sequence of up to four of these pieces: headers, values, line
  -- ends and junk in every order.
  let tokens := ["Content-Length: ", "1", "\r\n", "\n", "x", ":"]
  let seqs := (allUpTo tokens 4).map fun ts => utf8 (String.join ts)
  let crafted := ["", "\r\n", "content-length: 2\r\n\r\n{}", "Content-Length:2\n\n{}",
    "Content-Type: x\r\nContent-Length: 2\r\n\r\n{}",
    "Content-Length: 2\r\nContent-Length: 2\r\n\r\n{}",
    "Content-Length: 2\r\nContent-Length: 3\r\n\r\n{}", "Content-Length: +2\r\n\r\n{}",
    "Content-Length: 2 \r\n\r\n{}", "Content-Length : 2\r\n\r\n{}",
    "Content-Length: 123456789012345678901\r\n\r\n", "Content-Length: 18446744073709551616\r\n\r\n",
    "Content-Length: 67108865\r\n\r\n", "Content-Length: 67108864\r\n\r\n",
    "X\r\n\r\n", "Content-Length: 5\r\n\r\nab", "Content-Length: 2\r\n",
    "Cont\u00E9nt-Length: 2\r\n\r\n{}", "Content-Length: \t2\t\r\n\r\n{}",
    "Content-Length: \u000B2\r\n\r\n{}", "\r\n\r\nContent-Length: 2\r\n\r\n{}",
    "Content-Length: 0\r\n\r\n", "Content-Length: 2\r\n\r\n{}Content-Length: 1\r\n\r\n1",
    "\nContent-Length: 1\n\n1\n"]
  let blanks := String.join (List.replicate 33 "\r\n") ++ "Content-Length: 1\r\n\r\n1"
  let longLine := List.replicate 8200 97
  let longHeader := utf8 "X-Pad: " ++ List.replicate 8180 97 ++ utf8 "\r\nContent-Length: 2\r\n\r\n{}"
  let bodies := ["{}", "[1,2]", "{\"a\":\"\u00E9\"}", "null", "\"\""].map utf8
  let encoded := bodies.map Clew.Framing.encode ++
    [bodies.flatMap Clew.Framing.encode, Clew.Framing.encode (bodies.head!) ++ utf8 "garbage"]
  let inputs := seqs ++ (crafted ++ [blanks]).map utf8 ++ [longLine, longHeader] ++ encoded
  let frames := inputs.map fun bs =>
    pair (bytesJson bs) (match Clew.Framing.readFrame bs with
      | (.ok none, rest) => Json.arr #["eof", toJson rest.length]
      | (.ok (some b), rest) => Json.arr #["ok", bytesJson b, toJson rest.length]
      | (.error e, _) => Json.arr #["err", Json.str (frameError e)])
  let encodes := bodies.map fun b => pair (bytesJson b) (bytesJson (Clew.Framing.encode b))
  let lineInputs : List (List Nat) := (allUpTo ['a', ' ', '\r', '\n'] 4).map (·.map Char.toNat)
  let caps := [0, 1, 2, 3]
  let jsonLines := caps.flatMap fun cap => lineInputs.map fun bs =>
    pair (Json.arr #[toJson cap, ascii bs]) (match Clew.Framing.readJsonLine cap bs with
      | (.ok none, rest) => Json.arr #["none", toJson rest.length]
      | (.ok (some l), rest) => Json.arr #["ok", ascii l, toJson rest.length]
      | (.error _, _) => Json.arr #["too_large"])
  let capped := caps.flatMap fun cap => lineInputs.map fun bs =>
    pair (Json.arr #[toJson cap, ascii bs]) (match Clew.Framing.readLineCapped cap bs with
      | (some l, rest) => Json.arr #[ascii l, toJson rest.length]
      | (none, rest) => Json.arr #[Json.null, toJson rest.length])
  -- UTF-8: every one- and two-byte sequence over the bytes at the edges of
  -- the table, then the three- and four-byte leads with every edge of a
  -- continuation byte after them, as whole lines and cut off by the cap.
  let edges := [0x00, 0x41, 0x7F, 0x80, 0x8F, 0x90, 0x9F, 0xA0, 0xBF, 0xC0, 0xC1, 0xC2, 0xDF,
    0xE0, 0xE1, 0xEC, 0xED, 0xEE, 0xEF, 0xF0, 0xF1, 0xF3, 0xF4, 0xF5, 0xFF]
  let conts := [0x41, 0x7F, 0x80, 0x8F, 0x90, 0x9F, 0xA0, 0xBF, 0xC0]
  let short := [[]] ++ edges.map ([·]) ++ edges.flatMap fun a => edges.map fun b => [a, b]
  let three := [0xE0, 0xE1, 0xED, 0xEF, 0xC2].flatMap fun a => conts.flatMap fun b =>
    conts.map fun c => [a, b, c]
  let four := [0xF0, 0xF1, 0xF4, 0xF5].flatMap fun a => conts.flatMap fun b =>
    [0x80, 0xBF, 0x41].flatMap fun c => [0x80, 0xBF, 0x41].map fun d => [a, b, c, d]
  let texts := short ++ three ++ four
  let utf8Lines := texts.flatMap fun t => [(8, t ++ [10]), (8, t), (2, t ++ [10])]
  let cappedBytes := utf8Lines.map fun (cap, bs) =>
    pair (Json.arr #[toJson cap, bytesJson bs]) (match Clew.Framing.readLineCapped cap bs with
      | (some l, rest) => Json.arr #[bytesJson l, toJson rest.length]
      | (none, rest) => Json.arr #[Json.null, toJson rest.length])
  Json.mkObj [("read_frame", Json.arr frames.toArray), ("encode", Json.arr encodes.toArray),
    ("read_json_line", Json.arr jsonLines.toArray), ("read_line_capped", Json.arr capped.toArray),
    ("read_line_capped_bytes", Json.arr cappedBytes.toArray)]

def stepName : Clew.VoiceRoute.Step → String
  | .bye => "bye"
  | .sayHowToStop => "say_how_to_stop"
  | .nothing => "nothing"
  | .runWaiting => "run_waiting"
  | .declineWaiting => "decline_waiting"
  | .hush => "hush"
  | .tryCommand => "try_command"
  | .tryCommandsBackOn => "try_commands_back_on"
  | .hold => "hold"
  | .ask => "ask"

/-- Every combination of facts — so the Rust `route` is checked to be the
model everywhere, and the theorems hold of it outright. -/
def voiceRoute : Json :=
  let facts := Clew.VoiceRoute.allFacts.map fun f =>
    let question : Json := match f.toQuestion with
      | none => Json.null
      | some .yesOrNo => Json.str "yes_or_no"
      | some .answer => Json.str "answer"
    pair (Json.mkObj [("bye", Json.bool f.bye), ("stop_listening_said", Json.bool f.stopListeningSaid),
        ("empty", Json.bool f.empty), ("confirm_waiting", Json.bool f.confirmWaiting),
        ("hush", Json.bool f.hush), ("yes_or_no", match f.yesOrNo with
          | some b => Json.bool b
          | none => Json.null),
        ("to_question", question), ("answer_being_written", Json.bool f.answerBeingWritten),
        ("run_commands", Json.bool f.runCommands)])
      (Json.arr ((Clew.VoiceRoute.route f).map (Json.str ∘ stepName)).toArray)
  Json.mkObj [("route", Json.arr facts.toArray)]

/-- Every model's vectors, by file name. -/
def optJson : Option (List Char) → Json
  | some s => str s
  | none => Json.null

def ssh : Json :=
  -- Words a shell must read back: empty, spaces, quotes, `$`, a backslash,
  -- a newline, a glob, a tilde, non-ASCII, a leading dash.
  let words : List String := ["", "a", "a b", "'", "a'b", "''", "$HOME", "\\", "\n", "*", "~",
    "é", "-p", "a\"b", "#", "x;y", "`id`", "a@b:c"]
  let lists := (allUpTo words 2).map (·.map String.toList)
  let commands := lists.map fun ws =>
    pair (Json.arr (ws.map str).toArray) (str (Clew.Ssh.shellCommand ws))
  let quotes := words.map fun w => pair (Json.str w) (str (Clew.Ssh.quote w.toList))
  let scripts : List String := ["true", "echo hi", "a'b", "", "x\ny", "printf \"%s\\n\" x", "'"]
  let shcs := scripts.map fun t => pair (Json.str t) (optJson (Clew.Ssh.shC t.toList))
  -- ssh's arguments: every list of up to three of these words, and shaped ones.
  let tokens : List String := ["-l", "a", "-p", "22", "2222", "-o", "User=b", "Port=7", "u@h",
    "ssh://c@h:3", "--", "-v", "-lx", "-p9", "h", "-oUser=d"]
  let argLists : List (List String) := allUpTo tokens 3 ++
    [["ssh://h"], ["ssh://r@h:2222/"], ["ssh://r;x=1@h"], ["ssh://a%40b+c@h"], ["ssh://r@h."],
      ["ssh://r@[h]:2222"], ["ssh://r@h:2222/srv"], ["ssh://@h"], ["ssh://r@"], ["ssh://r@a..b"],
      ["ssh://r@[2001:db8::1]"], ["ssh://r@h:"], ["ssh://a%zz@h"], ["ssh://a%00@h"],
      ["ssh://r@[h]x"], ["ssh://a%c3%a9@h"], ["ssh://a%ff@h"], ["-p", "http", "h"],
      ["-p", "02222", "u@h"], ["a@b@h"], ["-l", "a", "-o", "User=c", "h"],
      ["-o", "port = 5", "h"], ["-o", "PORT=6", "h"], ["h", "uptime", "-p", "9"]]
  let endpoints := argLists.map fun args =>
    let a := args.map String.toList
    pair (Json.arr (args.map Json.str).toArray) (match Clew.Ssh.sshEndpoint a with
      | some (u, h, p) => Json.arr #[optJson u, str h, optJson p,
          str (Clew.Trust.scopeLabel u h (Clew.Ssh.labelPort p))]
      | none => Json.null)
  -- Saved connections, hostile fields among them.
  let users := ["root", "a b", "ssh:/x", "-oProxyCommand=x", "é", "a%b"]
  let hosts := ["example.com", "2001:db8::1", "-oProxyCommand=touch x", "h.", "[x]"]
  let ports := [22, 2222, 1, 65535]
  let identities := ["", "/keys/id", "-x", "a b"]
  let saved := users.flatMap fun u => hosts.flatMap fun h => ports.flatMap fun p =>
    identities.map fun i =>
      let args := Clew.Ssh.sshArgs u.toList h.toList p i.toList
      pair (Json.arr #[Json.str u, Json.str h, toJson p, Json.str i])
        (Json.arr #[Json.arr (args.map str).toArray,
          str (Clew.Ssh.savedLabel u.toList h.toList p),
          match Clew.Ssh.sshEndpoint args with
          | some (eu, eh, ep) => Json.arr #[optJson eu, str eh, optJson ep]
          | none => Json.null])
  -- known_hosts: the field written for a host and port, and fields judged.
  let khHosts := ["example.com", "2001:db8::1", "a_b-c.d", "h"]
  let khPorts : List (Option Nat) := [none, some 22, some 2222, some 1, some 65535]
  let patterns := khHosts.flatMap fun h => khPorts.map fun p =>
    pair (Json.arr #[Json.str h, (match p with | some n => toJson n | none => Json.null)])
      (str (Clew.Ssh.knownHostsPattern h.toList p))
  let fields : List String := ["example.com", "[example.com]:2222", "[h]:0", "[h]:65536", "[h]:+22",
    "*.example.com", "a,b", "!h", "|1|abc", "@revoked", "h h", "", "[]:22", "[h]", "[h]:22:1",
    "2001:db8::1", "[2001:db8::1]:2222", "h?", String.ofList (List.replicate 256 'a'),
    String.ofList (List.replicate 255 'a'), "[a]:b]:22", "é"]
  let plains := fields.map fun f => pair (Json.str f) (Json.bool (Clew.Ssh.plainHostField f.toList))
  Json.mkObj [("shell_command", Json.arr commands.toArray), ("quote", Json.arr quotes.toArray),
    ("sh_c", Json.arr shcs.toArray), ("ssh_endpoint", Json.arr endpoints.toArray),
    ("saved_connection", Json.arr saved.toArray),
    ("known_hosts_pattern", Json.arr patterns.toArray),
    ("plain_host_field", Json.arr plains.toArray)]

def all : List (String × Json) :=
  [("confine.json", confine), ("update.json", update), ("server_dist.json", serverDist),
    ("trust.json", trust), ("framing.json", framing), ("voice_route.json", voiceRoute), ("ssh.json", ssh)]

end Vectors

def main (args : List String) : IO UInt32 := do
  let dir := args.headD "vectors"
  IO.FS.createDirAll dir
  for (name, json) in Vectors.all do
    IO.FS.writeFile (System.FilePath.mk dir / name) (json.pretty 100 ++ "\n")
  return 0
