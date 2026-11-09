//! Stepping through a Lean proof: where each tactic of a declaration's `by`
//! block ends — the places the Lean server is asked for the goals after it
//! (`$/lean/plainGoal`) — and what changed from one step's goals to the
//! next. Plain text, no tree-sitter: a tactic block is laid out by its
//! indentation, which Lean itself reads it by.

/// One step of a proof: a tactic, from its first line to its last, and
/// where the goals after it are asked for. The first step of every proof is
/// its start, just after `by`: the goals the proof sets out to close.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProofStep {
    /// 0-based first line of the tactic.
    pub first: usize,
    /// 0-based last line of the tactic (a bracket left open, a `:=` at a
    /// line's end, carries it on to the next line).
    pub last: usize,
    /// Byte column on `last` where the tactic's text ends, before a
    /// trailing comment: where the goals after it are read.
    pub end: usize,
    /// What the tactic says, its lines joined, trimmed (`by` for the
    /// start).
    pub text: String,
}

/// The steps of the proof of the declaration on 0-based lines `decl` to
/// `decl_end`: its start, then each tactic of the first `by` block in it.
/// Empty when the declaration has no `by` (a term proof, a definition).
pub fn proof_steps(source: &str, decl: usize, decl_end: usize) -> Vec<ProofStep> {
    let lines: Vec<&str> = source.lines().collect();
    let decl_end = decl_end.min(lines.len().saturating_sub(1));
    let Some((by_line, after_by)) = (decl..=decl_end).find_map(|i| {
        let code = code_of(lines.get(i)?);
        by_at(code).map(|at| (i, at))
    }) else {
        return Vec::new();
    };
    let decl_indent = lines.get(decl).map_or(0, |l| indent(l));
    let mut steps = vec![ProofStep {
        first: by_line,
        last: by_line,
        end: after_by,
        text: "by".to_string(),
    }];
    // A tactic on the `by` line itself: `by simp [foo]`.
    let rest = code_of(lines[by_line])[after_by..].trim();
    if !rest.is_empty() {
        steps.push(ProofStep {
            first: by_line,
            last: by_line,
            end: code_of(lines[by_line]).trim_end().len(),
            text: rest.to_string(),
        });
    }
    let mut open = steps
        .last()
        .filter(|s| s.first == by_line && s.text != "by")
        .is_some_and(|s| carries_on(&s.text));
    for (i, line) in lines
        .iter()
        .enumerate()
        .take(decl_end + 1)
        .skip(by_line + 1)
    {
        let code = code_of(line);
        if code.trim().is_empty() {
            continue;
        }
        // Dedented to the declaration's own column: the proof is over.
        if indent(line) <= decl_indent {
            break;
        }
        let end = code.trim_end().len();
        if open && let Some(step) = steps.last_mut().filter(|s| s.text != "by") {
            step.last = i;
            step.end = end;
            step.text.push(' ');
            step.text.push_str(code.trim());
            open = carries_on(&step.text);
            continue;
        }
        let text = code.trim().to_string();
        open = carries_on(&text);
        steps.push(ProofStep {
            first: i,
            last: i,
            end,
            text,
        });
    }
    steps
}

/// A line without its trailing `--` comment (one outside a string).
fn code_of(line: &str) -> &str {
    let mut in_string = false;
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => in_string = !in_string,
            b'\\' if in_string => i += 1,
            b'-' if !in_string && bytes.get(i + 1) == Some(&b'-') => return &line[..i],
            _ => {}
        }
        i += 1;
    }
    line
}

/// Byte column just after a `by` keyword in `code`, the last one on the
/// line (`theorem t : p := by`, `have h : q := by`).
fn by_at(code: &str) -> Option<usize> {
    let word = |c: char| c.is_alphanumeric() || c == '_' || c == '\'' || c == '.';
    let mut found = None;
    for (at, _) in code.match_indices("by") {
        let before = code[..at].chars().next_back();
        let after = code[at + 2..].chars().next();
        if !before.is_some_and(word) && !after.is_some_and(word) {
            found = Some(at + 2);
        }
    }
    found
}

/// The width of a line's leading whitespace.
fn indent(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// Whether a tactic's text goes on past its line: a bracket left open, or
/// a line that ends where a term must follow.
fn carries_on(text: &str) -> bool {
    let mut depth: i32 = 0;
    for c in text.chars() {
        match c {
            '(' | '[' | '{' | '⟨' => depth += 1,
            ')' | ']' | '}' | '⟩' => depth -= 1,
            _ => {}
        }
    }
    let t = text.trim_end();
    depth > 0
        || [":=", ",", "<|", "$", "←", "→", "∧", "∨", "↔", "=", "+", "*"]
            .iter()
            .any(|end| t.ends_with(end))
}

/// One goal as the Lean server writes it: its case name, its hypotheses
/// (`a b : T`), and what is to be shown (after `⊢`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Goal {
    pub case: Option<String>,
    /// Each hypothesis line: its names and its type.
    pub hyps: Vec<(Vec<String>, String)>,
    pub target: String,
}

/// A goal from `$/lean/plainGoal`'s text: `case c` first when it has one,
/// a hypothesis a line (a type carried on to lines indented under it),
/// then `⊢ target`.
pub fn parse_goal(text: &str) -> Goal {
    let mut goal = Goal::default();
    let mut in_target = false;
    for line in text.lines() {
        if in_target {
            goal.target.push('\n');
            goal.target.push_str(line);
            continue;
        }
        if let Some(target) = line.strip_prefix("⊢") {
            goal.target = target.trim_start().to_string();
            in_target = true;
        } else if let Some(case) = line.strip_prefix("case ") {
            goal.case = Some(case.trim().to_string());
        } else if line.starts_with(char::is_whitespace) {
            if let Some((_, ty)) = goal.hyps.last_mut() {
                ty.push('\n');
                ty.push_str(line);
            }
        } else if let Some((names, ty)) = line.split_once(" : ") {
            goal.hyps.push((
                names.split_whitespace().map(str::to_string).collect(),
                ty.to_string(),
            ));
        }
    }
    goal
}

/// What a hypothesis of the goal after a tactic is, next to the goal
/// before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// Not there before.
    New,
    /// There before, with another type.
    Changed,
    /// As it was.
    Same,
}

/// Each hypothesis of `after`, next to `before` (the goal it came from):
/// new, changed or the same. With nothing before, each is the same — the
/// start of a proof is no change.
pub fn hyp_changes(before: Option<&Goal>, after: &Goal) -> Vec<Change> {
    after
        .hyps
        .iter()
        .map(|(names, ty)| {
            let Some(before) = before else {
                return Change::Same;
            };
            let had = before
                .hyps
                .iter()
                .find(|(b, _)| names.iter().any(|n| b.contains(n)));
            match had {
                None => Change::New,
                Some((_, was)) if was != ty => Change::Changed,
                Some(_) => Change::Same,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROOF: &str = "\
theorem add_comm' (a b : Nat) : a + b = b + a := by
  induction b with
  | zero => simp -- the base case
  | succ n ih =>
    rw [Nat.add_succ,
      ih]
    rfl

theorem next : True := trivial
";

    #[test]
    fn a_proof_steps_by_its_tactics() {
        let steps = proof_steps(PROOF, 0, 6);
        let shown: Vec<(usize, usize, &str)> = steps
            .iter()
            .map(|s| (s.first, s.last, s.text.as_str()))
            .collect();
        assert_eq!(
            shown,
            [
                (0, 0, "by"),
                (1, 1, "induction b with"),
                (2, 2, "| zero => simp"),
                (3, 3, "| succ n ih =>"),
                (4, 5, "rw [Nat.add_succ, ih]"),
                (6, 6, "rfl"),
            ]
        );
        // The start is read just after `by`; a step at its end, before
        // its comment.
        assert_eq!(steps[0].end, PROOF.lines().next().unwrap().len());
        assert_eq!(steps[2].end, "  | zero => simp".len());
        assert_eq!(steps[4].end, "      ih]".len());
    }

    #[test]
    fn a_term_proof_has_no_steps_and_a_one_line_proof_one() {
        assert!(proof_steps(PROOF, 8, 8).is_empty());
        let one = "example : 1 + 1 = 2 := by decide\n";
        let steps = proof_steps(one, 0, 0);
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[1].text, "decide");
        assert_eq!(steps[1].end, one.trim_end().len());
        // `by` inside a name, or a comment, is no `by`.
        assert!(proof_steps("def standby := 1 -- by hand\n", 0, 0).is_empty());
    }

    #[test]
    fn the_proof_ends_at_the_next_declaration() {
        let src = "theorem t : p := by\n  exact h\ntheorem u : q := by\n  exact g\n";
        let steps = proof_steps(src, 0, 3);
        assert_eq!(steps.len(), 2, "{steps:?}");
    }

    #[test]
    fn a_goal_reads_as_its_case_hypotheses_and_target() {
        let goal = parse_goal("case succ\nn : Nat\nih a : P n\n  ∧ Q n\n⊢ P (n + 1)");
        assert_eq!(goal.case.as_deref(), Some("succ"));
        assert_eq!(goal.hyps.len(), 2);
        assert_eq!(goal.hyps[1].0, ["ih", "a"]);
        assert_eq!(goal.hyps[1].1, "P n\n  ∧ Q n");
        assert_eq!(goal.target, "P (n + 1)");
    }

    #[test]
    fn a_step_marks_what_it_brought_and_changed() {
        let before = parse_goal("n : Nat\nh : n = 1\n⊢ n + 1 = 2");
        let after = parse_goal("n : Nat\nh : 1 = n\nk : Nat\n⊢ n + 1 = 2");
        assert_eq!(
            hyp_changes(Some(&before), &after),
            [Change::Same, Change::Changed, Change::New]
        );
        assert_eq!(hyp_changes(None, &after), [Change::Same; 3]);
    }
}
