; clew patch: the highlight query of the grammar's repository
; (github.com/wvhulle/tree-sitter-lean, queries/highlights.scm), which the
; release does not ship. Its patterns for nodes this release does not have
; (doc_comment) are dropped, and its Helix capture names are clew's
; (`@namespace` → `@module`, `@keyword.function` → `@keyword`, …).
;

; ── Variables ────────────────────────────────────────────────

(identifier) @variable

; Capitalize → type by convention
((identifier) @type
 (#match? @type "^[A-Z]"))

; ── Modifiers ──────────────────────────────────────────────────

[
  "noncomputable"
  "partial"
  "protected"
  "private"
  "unsafe"
] @keyword

; ── Declarations ─────────────────────────────────────────────

(definition
  name: (identifier) @function)

(structure
  name: (identifier) @type)

(inductive
  name: (identifier) @type)

(class_inductive
  name: (identifier) @type)

(opaque
  name: (identifier) @function)

(constructor
  name: (identifier) @constructor)

(constant
  name: (identifier) @constant)

(axiom
  name: (identifier) @function)

(structure_field
  name: (identifier) @property)

; ── Attributes ──────────────────────────────────────────────────

(attributes
  "@" @punctuation.special)

(attribute_entry
  name: (identifier) @attribute)

(attribute_entry
  arg: (identifier) @attribute)

; ── Namespaces & imports ─────────────────────────────────────

(namespace
  name: (identifier) @module)

(open
  namespace: (identifier) @module)

(section
  name: (identifier) @module)

(import
  module: (_) @module)

; ── Explicit (@) ───────────────────────────────────────────────

(explicit
  "@" @operator)

; ── Patterns ─────────────────────────────────────────────────

(constructor_pattern
  constructor: (identifier) @constructor)

; ── Projections ──────────────────────────────────────────────

(projection
  name: (identifier) @property)

(projection
  name: (number) @property)

; ── Binders ──────────────────────────────────────────────────

(explicit_binder
  name: (identifier) @variable.parameter)

(implicit_binder
  name: (identifier) @variable.parameter)

(instance_binder
  name: (identifier) @variable.parameter)

(fun
  binders: (identifier) @variable.parameter)

(quantifier
  binders: (identifier) @variable.parameter)

(for_in
  var: (identifier) @variable.parameter)

; ── Let / have / mut / bind ──────────────────────────────────

(let
  name: (identifier) @variable)

(do_let
  pattern: (identifier) @variable)

(have
  name: (identifier) @variable)

(let_mut
  name: (identifier) @variable)

(let_bind
  name: (identifier) @variable)

(reassign
  name: (identifier) @variable)

; ── Field assignment ─────────────────────────────────────────

(field_assignment
  name: (identifier) @property)

; ── Application ─────────────────────────────────────────────

(application
  name: (identifier) @function)

; ── Tactics ──────────────────────────────────────────────────

(tactic_apply
  tactic: (identifier) @function.builtin)

(tactic_have
  name: (identifier) @variable)

(tactic_let
  pattern: (identifier) @variable)

(tactic_case
  "case" @keyword)

(tactic_case
  "next" @keyword)

(tactic_rewrite
  "rw" @function.builtin)

(tactic_rewrite
  "rewrite" @function.builtin)

(hash_command) @keyword

; ── Keywords ─────────────────────────────────────────────────

; Function/definition-introducing keywords
[
  "def"
  "theorem"
  "lemma"
  "abbrev"
  "instance"
  "example"
] @keyword

; Type/storage-defining keywords
[
  "structure"
  "inductive"
  "class"
  "constant"
  "axiom"
  "opaque"
] @keyword

[
  "deriving"
  "section"
  "namespace"
  "end"
  "attribute"
  "extends"
] @keyword

(prelude) @keyword
(module_header) @keyword

[
  "open"
  "import"
  "export"
  "scoped"
  "hiding"
  "meta"
  "all"
  "include"
  "omit"
] @keyword

[
  "public"
] @keyword

[
  "variable"
  "universe"
  "universes"
  "set_option"
  "initialize"
  "builtin_initialize"
] @keyword

[
  "let"
  "mut"
  "have"
  "where"
] @keyword

[
  "fun"
  "λ"
] @keyword

[
  "forall"
  "∀"
  "exists"
  "∃"
] @keyword

[
  "if"
  "then"
  "else"
] @keyword

[
  "for"
  "in"
  "while"
  "do"
  "unless"
] @keyword

(do_break) @keyword
(do_continue) @keyword

[
  "match"
  "with"
] @keyword

[
  "by"
  "show"
  "calc"
  "obtain"
  "suffices"
] @keyword

; Metaprogramming keywords
[
  "notation"
  "macro_rules"
  "syntax"
  "macro"
  "elab"
  "prefix"
  "infix"
  "infixl"
  "infixr"
  "postfix"
] @keyword

"return" @keyword

; Debug
[
  "dbg_trace"
] @function.builtin

; Exception-like keywords
[
  "try"
  "catch"
] @keyword

; ── Interpolated strings ─────────────────────────────────────

(interpolated_string
  prefix: _ @string.special)

(interpolated_string
  (string) @string)

; ── Set-option / pragmas ─────────────────────────────────────

(set_option
  name: (identifier) @variable.builtin)

; ── As-patterns ──────────────────────────────────────────────

(as_pattern
  binder: (identifier) @variable
  "@" @operator)

; ── Initialize ───────────────────────────────────────────────

(initialize
  name: (identifier) @variable)

; ── Include / Omit ───────────────────────────────────────────

(include
  name: (identifier) @variable)

(omit
  name: (identifier) @variable)

; ── Syntax declarations & quotations ─────────────────────────

(syntax
  attr: (identifier) @keyword)

(syntax
  category: (identifier) @type)

(syntax_quotation
  "`" @punctuation.special
  "(" @punctuation.special
  ")" @punctuation.special)

(sorry) @keyword

; ── Operators ────────────────────────────────────────────────

[
  "!"  "¬"
  "+"  "-"  "*"  "/"  "%"
  "++"  "∘"
  "::"
  "×"  "∪"  "∩"
  "&&"  "∧"
  "||"  "∨"
  "="  "=="  "!="  "≠"  "∣"
  "<"  ">"  "<="  ">="  "≤"  "≥"
  "|>"  "<|"  "|>."  "$"
  "<|>"
  "<;>"  "<;"
  "->"  "→"
  "<-"  "←"
  ":="
  "=>"
  "↔"  "⊢"
  "^"
] @operator

; ── Literals ─────────────────────────────────────────────────

(number) @number
(float) @number
(string) @string
(char) @string
(hole) @variable.builtin
(synthetic_hole) @variable.builtin
[(true) (false)] @constant.builtin

; ── String internals ─────────────────────────────────────────

(escape_sequence) @escape

(interpolation
  "{" @punctuation.special
  "}" @punctuation.special)

(comment) @comment

; ── Punctuation ──────────────────────────────────────────────

["(" ")" "[" "]" "{" "}" "⟨" "⟩" "#["] @punctuation.bracket
["|" "," "." ":" ";" "//"] @punctuation.delimiter
