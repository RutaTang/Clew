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

/-- Every model's vectors, by file name. -/
def all : List (String × Json) :=
  [("confine.json", confine), ("update.json", update)]

end Vectors

def main (args : List String) : IO UInt32 := do
  let dir := args.headD "vectors"
  IO.FS.createDirAll dir
  for (name, json) in Vectors.all do
    IO.FS.writeFile (System.FilePath.mk dir / name) (json.pretty 100 ++ "\n")
  return 0
