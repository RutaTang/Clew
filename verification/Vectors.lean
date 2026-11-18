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

/-- A `(input, output)` pair. -/
def pair (a b : Json) : Json := Json.arr #[a, b]

def confine : Json :=
  let alphabet := ['a', '.', '/', ' ', ' ']
  let extra := ["../x", "a/..", "..a", ".../", "src/", "src/.", ".", "./a", "a/./b",
    " ", "a ", "\t", "\n", "\u0085", "​", " /a", "//", "a//b", "..", "a/../b"]
  let inputs : List (List Char) := allUpTo alphabet 5 ++ extra.map String.toList
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
  let parses := (tiny ++ shaped ++ edge).map fun s =>
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
three whose lower case is not themselves in one character or not ASCII. -/
def rustLower (c : Char) : List Char :=
  if c == '\u212A' then ['k']                -- KELVIN SIGN
  else if c == '\u0130' then ['i', '\u0307']  -- I WITH DOT ABOVE
  else if c == '\u00C9' then ['\u00E9']       -- E WITH ACUTE
  else [Clew.Text.toAsciiLowercase c]

def optStr : Option (List Char) → Json
  | some s => str s
  | none => Json.null

def serverDist : Json :=
  let alphabet := ['L', 'x', ' ', '-', '/', '.', '_', '8']
  let platforms : List (List Char) := allUpTo alphabet 3 ++ ["Linux x86_64", "Darwin arm64",
    " linux aarch64 ", "../etc", "linux/x86", "Linux\tx86", "LINUX X86_64", "linux.x86",
    "li_nux-1", "Linux\u00A0x86", "\u212A", "\u0130", "\u00C9", "x86%2F", "a?b", "a#b"].map String.toList
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
  let texts : List (List Char) := (entries ++ pairs ++
    [String.join (good.map (· ++ "\n")), "", "\n\n;,"]).map String.toList
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
  let ports := [22, 2222, 1]
  let labels := users.flatMap fun u => hosts.flatMap fun h => ports.map fun p =>
    pair (Json.arr #[(match u with | some u => Json.str u | none => Json.null), Json.str h, toJson p])
      (str (Clew.Trust.scopeLabel (u.map String.toList) h.toList p))
  let roots := ["/srv/p", "/b:/c", "/"]
  let someLabels := [Clew.Trust.scopeLabel (some "u".toList) "h".toList 22,
    Clew.Trust.scopeLabel (some "u".toList) "2001:db8::1".toList 2222,
    Clew.Trust.scopeLabel none "a/b".toList 1]
  let keys := someLabels.flatMap fun l => roots.map fun r =>
    pair (Json.arr #[str l, Json.str r]) (str (Clew.Trust.approvalKey l r.toList))
  Json.mkObj [("args_part", Json.arr argsParts.toArray),
    ("length_prefixed", Json.arr prefixed.toArray), ("scope_label", Json.arr labels.toArray),
    ("project_key", Json.arr keys.toArray)]

/-- Every model's vectors, by file name. -/
def all : List (String × Json) :=
  [("confine.json", confine), ("update.json", update), ("server_dist.json", serverDist),
    ("trust.json", trust)]

end Vectors

def main (args : List String) : IO UInt32 := do
  let dir := args.headD "vectors"
  IO.FS.createDirAll dir
  for (name, json) in Vectors.all do
    IO.FS.writeFile (System.FilePath.mk dir / name) (json.pretty 100 ++ "\n")
  return 0
