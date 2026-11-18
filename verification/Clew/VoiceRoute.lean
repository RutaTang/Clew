/-!
# Where spoken words go — `route` in `src/app/voice.rs`

Everything the reader says in voice mode goes one of a few ways: it turns
the microphone off, answers a command waiting for a yes, stops Clew
speaking, runs a command, is held for a question the agent asks, or is
asked of Ask. Which way is decided by `route`, from a handful of facts
read off the words and the app (`RouteFacts`), as a list of steps taken in
order until one acts — only the two `Try` steps can decline to.

These rules took the most review rounds of anything in voice mode, each
round finding a path the last had missed. Here they are theorems, over
every combination of facts:

* "Bye, Clew" is always heard first, whatever else is true;
* a word that stops Clew is never sent to Ask; when the question waiting
  offers an answer by that name, it is held as that answer;
* nor is it ever taken for a yes ("okay, stop" opens with one);
* a command waiting for a yes runs only on a yes;
* what is said to a question waiting is never sent to Ask;
* while an answer is being written, a stop word stops it before any command
  of its name is tried — unless a command waits for a yes, which the stop
  word declines, or the question waiting has an answer by that name;
* every route ends in exactly one step that acts, after at most one `Try`,
  and never takes a step whose subject is not there (a waiting command, a
  question).
-/

namespace Clew.VoiceRoute

/-- `QuestionWord`. -/
inductive QuestionWord where
  | yesOrNo
  | answer
  deriving DecidableEq, Repr

/-- `RouteFacts`. -/
structure Facts where
  bye : Bool
  stopListeningSaid : Bool
  empty : Bool
  confirmWaiting : Bool
  hush : Bool
  yesOrNo : Option Bool
  toQuestion : Option QuestionWord
  answerBeingWritten : Bool
  runCommands : Bool
  deriving DecidableEq, Repr

/-- `Step`. -/
inductive Step where
  | bye
  | sayHowToStop
  | nothing
  | runWaiting
  | declineWaiting
  | hush
  | tryCommand
  | tryCommandsBackOn
  | hold
  | ask
  deriving DecidableEq, Repr

/-- `route`. -/
def route (f : Facts) : List Step :=
  if f.bye then [.bye]
  else if f.stopListeningSaid then [.sayHowToStop]
  else if f.empty then [.nothing]
  else
    let answer := if f.hush then some false else f.yesOrNo
    if f.confirmWaiting && answer == some true then [.runWaiting]
    else if f.confirmWaiting && answer == some false then [.declineWaiting]
    else
      let hush := f.hush && f.toQuestion != some .answer
      if hush && f.answerBeingWritten then [.hush]
      else
        [if f.runCommands then .tryCommand else .tryCommandsBackOn,
          if hush then .hush else if f.toQuestion.isSome then .hold else .ask]

/-- Every combination of facts: 1152 of them. -/
def allFacts : List Facts :=
  let bools := [false, true]
  bools.flatMap fun bye => bools.flatMap fun stop => bools.flatMap fun empty =>
  bools.flatMap fun confirm => bools.flatMap fun hush =>
  [none, some false, some true].flatMap fun yn =>
  [none, some QuestionWord.yesOrNo, some QuestionWord.answer].flatMap fun to =>
  bools.flatMap fun writing => bools.map fun run =>
    (⟨bye, stop, empty, confirm, hush, yn, to, writing, run⟩ : Facts)

theorem allFacts_complete (f : Facts) : f ∈ allFacts := by
  obtain ⟨bye, stop, empty, confirm, hush, yn, to, writing, run⟩ := f
  simp only [allFacts, List.mem_flatMap, List.mem_map]
  refine ⟨bye, ?_, stop, ?_, empty, ?_, confirm, ?_, hush, ?_, yn, ?_, to, ?_, writing, ?_, run, ?_,
    rfl⟩
  · cases bye <;> simp
  · cases stop <;> simp
  · cases empty <;> simp
  · cases confirm <;> simp
  · cases hush <;> simp
  · rcases yn with _ | ⟨_ | _⟩ <;> simp
  · rcases to with _ | ⟨_ | _⟩ <;> simp
  · cases writing <;> simp
  · cases run <;> simp

/-- A statement about every combination of facts, checked on all of them. -/
theorem forall_facts (p : Facts → Prop) [DecidablePred p]
    (h : allFacts.all (fun f => decide (p f)) = true) : ∀ f, p f := by
  intro f
  have := List.all_eq_true.mp h f (allFacts_complete f)
  simpa using this

/-! ## The rules -/

/-- **"Bye, Clew" first.** -/
theorem bye_first : ∀ f : Facts, f.bye = true → route f = [.bye] :=
  forall_facts _ (by decide +kernel)

/-- **A stop word is never asked of Ask.** -/
theorem hush_never_asked : ∀ f : Facts, f.hush = true → .ask ∉ route f :=
  forall_facts _ (by decide +kernel)

/-- **A stop word the question waiting has an answer of is that answer**:
like any word said to the question, it is tried as a command, then held
for the question — nothing is stopped. -/
theorem hush_answers_the_question : ∀ f : Facts,
    f.bye = false → f.stopListeningSaid = false → f.empty = false → f.confirmWaiting = false →
      f.hush = true → f.toQuestion = some QuestionWord.answer →
        route f = [if f.runCommands then .tryCommand else .tryCommandsBackOn, .hold] :=
  forall_facts _ (by decide +kernel)

/-- **"Okay, stop" is not a yes**: a stop word never runs the command
waiting for one. -/
theorem hush_never_yes : ∀ f : Facts, f.hush = true → .runWaiting ∉ route f :=
  forall_facts _ (by decide +kernel)

/-- **Only a yes runs a waiting command**, and only when one waits and
nothing ahead of it (bye, the microphone's label, nothing at all) was
said. -/
theorem runs_only_on_yes : ∀ f : Facts, .runWaiting ∈ route f ↔
    (f.bye = false ∧ f.stopListeningSaid = false ∧ f.empty = false ∧ f.confirmWaiting = true ∧
      f.hush = false ∧ f.yesOrNo = some true) :=
  forall_facts _ (by decide +kernel)

/-- **What is said to a question waiting is never asked of Ask.** -/
theorem question_words_never_asked : ∀ f : Facts, f.toQuestion.isSome = true → .ask ∉ route f :=
  forall_facts _ (by decide +kernel)

/-- **An answer being written is stopped first**: with nothing ahead of it,
a stop word while an answer is written does that and nothing else. -/
theorem hush_stops_the_answer_first : ∀ f : Facts,
    f.bye = false → f.stopListeningSaid = false → f.empty = false → f.confirmWaiting = false →
      f.hush = true → f.toQuestion ≠ some .answer → f.answerBeingWritten = true →
        route f = [.hush] :=
  forall_facts _ (by decide +kernel)

/-- A step that always acts. -/
def acts : Step → Bool
  | .tryCommand | .tryCommandsBackOn => false
  | _ => true

/-- Whether a route is one step that acts, or a `Try` and then one. -/
def wellFormed : List Step → Bool
  | [s] => acts s
  | [t, s] => !acts t && acts s
  | _ => false

/-- **Every route ends in one step that acts**, after at most one `Try`. -/
theorem ends_in_one_act : ∀ f : Facts, wellFormed (route f) = true :=
  forall_facts _ (by decide +kernel)

/-- **No step without its subject**: a waiting command is answered only
when one waits, words are held only for a question waiting. -/
theorem steps_have_their_subject : ∀ f : Facts,
    ((.runWaiting ∈ route f ∨ .declineWaiting ∈ route f) → f.confirmWaiting = true) ∧
      (.hold ∈ route f → f.toQuestion.isSome = true) :=
  forall_facts _ (by decide +kernel)

end Clew.VoiceRoute
