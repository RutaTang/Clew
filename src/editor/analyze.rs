//! Cursor-derived reading aids computed from the display text: the identifier
//! under the cursor (for occurrence highlight) and bracket matching.

use crate::highlight::{self, HlLine};

/// Whether `c` belongs to an identifier: the one definition every reading aid
/// shares (word under the cursor, occurrences, `w`/`b` motions, the
/// go-to-definition underline), so they can never disagree about what a word is.
pub fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Identifier range `[start, end)` around column `col` of `chars`, or `None`
/// when `col` is not on an identifier character.
pub fn ident_range(chars: &[char], col: usize) -> Option<(usize, usize)> {
    if !chars.get(col).copied().is_some_and(is_ident_char) {
        return None;
    }
    let mut start = col;
    while start > 0 && is_ident_char(chars[start - 1]) {
        start -= 1;
    }
    let mut end = col;
    while end < chars.len() && is_ident_char(chars[end]) {
        end += 1;
    }
    Some((start, end))
}

/// Display characters of one line (spans concatenated, tabs expanded).
fn line_chars(lines: &[HlLine], line: usize) -> Vec<char> {
    lines
        .get(line)
        .map(|l| l.spans.iter().flat_map(|(t, _)| t.chars()).collect())
        .unwrap_or_default()
}

/// How far either side of a column the word lookups read. An identifier
/// longer than this (a minified bundle's mangled names, an inlined blob) is
/// cut at the window's edge: the lookups run on every view rebuild, and used
/// to materialize the caret's whole line — megabytes on a minified one.
pub const MAX_WORD_CHARS: usize = 256;

/// Display characters `[from, to)` of `line` (fewer where the line ends
/// sooner), without materializing the rest. An ASCII span — nearly all of
/// code — is measured and sliced by bytes, so reaching column two million of
/// a minified line costs one ASCII check, not two million decoded chars.
pub fn line_window(line: &HlLine, from: usize, to: usize) -> Vec<char> {
    let mut out = Vec::with_capacity(to.saturating_sub(from).min(4 * MAX_WORD_CHARS));
    let mut seen = 0usize;
    for (text, _) in &line.spans {
        if seen >= to {
            break;
        }
        if text.is_ascii() {
            let n = text.len();
            if seen + n > from {
                let (a, b) = (from.max(seen) - seen, (to - seen).min(n));
                out.extend(text[a..b].chars());
            }
            seen += n;
        } else {
            for ch in text.chars() {
                if seen >= to {
                    break;
                }
                if seen >= from {
                    out.push(ch);
                }
                seen += 1;
            }
        }
    }
    out
}

/// Display-column range `[start, end)` of the identifier under `(line, col)`,
/// or `None` when `col` is not on an identifier character. Reads at most
/// [`MAX_WORD_CHARS`] either side of `col`.
pub fn word_range_at(lines: &[HlLine], line: usize, col: usize) -> Option<(usize, usize)> {
    let from = col.saturating_sub(MAX_WORD_CHARS);
    let window = line_window(
        lines.get(line)?,
        from,
        col.saturating_add(MAX_WORD_CHARS + 1),
    );
    let (start, end) = ident_range(&window, col - from)?;
    Some((from + start, from + end))
}

/// Whether a span's style marks its text as string or comment content. A style
/// covers a whole span, so testing the span is exactly testing each of its
/// characters — which is what lets the scans skip a literal span wholesale.
fn is_literal(style: Option<u8>) -> bool {
    style.is_some_and(highlight::style_is_literal)
}

/// Resolve display column `col` on `line` to `(span index, byte offset inside
/// that span, the character there, that span's style)`. Walks the spans instead
/// of materializing the line, so it allocates nothing.
fn locate(lines: &[HlLine], line: usize, col: usize) -> Option<(usize, usize, char, Option<u8>)> {
    let spans = &lines.get(line)?.spans;
    let mut seen = 0usize;
    for (i, (text, style)) in spans.iter().enumerate() {
        for (byte, ch) in text.char_indices() {
            if seen == col {
                return Some((i, byte, ch, *style));
            }
            seen += 1;
        }
    }
    None
}

/// Display column of the character at byte offset `byte` inside `spans[idx]`.
/// Only called for a found match, so the per-span char count it pays is once
/// per bracket-match query, not once per character scanned.
fn column_of(spans: &[(String, Option<u8>)], idx: usize, byte: usize) -> usize {
    let before: usize = spans[..idx].iter().map(|(t, _)| t.chars().count()).sum();
    before + spans[idx].0[..byte].chars().count()
}

/// The identifier under `(line, col)`, if any, as its text (see
/// [`word_range_at`]).
pub fn word_at(lines: &[HlLine], line: usize, col: usize) -> Option<String> {
    let l = lines.get(line)?;
    let (start, end) = word_range_at(lines, line, col)?;
    Some(line_window(l, start, end).into_iter().collect())
}

/// Whole-word occurrences of `word` across `lines`, as (line, col0, col1) in
/// display columns. `cap` bounds how many matches are returned, not how much is
/// scanned: every line is still visited, so the per-line cost has to stay low.
/// The occurrence highlight asks for it on every view rebuild, through
/// [`crate::viewer::Viewer::occurrences`], which memoizes the answer per word
/// — so a scan runs when the word under the caret changes, over the whole
/// file.
pub fn occurrences(word: &str, lines: &[HlLine], cap: usize) -> Vec<(usize, usize, usize)> {
    let needle: Vec<char> = word.chars().collect();
    let Some(&first) = needle.first() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    // Buffers reused across lines. Materializing a line used to cost two fresh
    // Vecs per line of the file per frame; now only lines that can hold a match
    // are materialized, into these.
    let mut chars: Vec<char> = Vec::new();
    let mut styles: Vec<Option<u8>> = Vec::new();
    for (li, line) in lines.iter().enumerate() {
        // A match begins with `first`, and every character of the line belongs
        // to exactly one span, so a line whose spans never contain `first`
        // cannot match. Necessary, not sufficient — the real test still runs
        // below on the lines that survive.
        if !line.spans.iter().any(|(t, _)| t.contains(first)) {
            continue;
        }
        chars.clear();
        styles.clear();
        for (text, style) in &line.spans {
            chars.extend(text.chars());
            styles.resize(chars.len(), *style);
        }
        let mut i = 0;
        while i + needle.len() <= chars.len() {
            let in_literal = is_literal(styles.get(i).copied().flatten());
            let is_match = !in_literal
                && chars[i..i + needle.len()] == needle[..]
                && (i == 0 || !is_ident_char(chars[i - 1]))
                && (i + needle.len() == chars.len() || !is_ident_char(chars[i + needle.len()]));
            if is_match {
                out.push((li, i, i + needle.len()));
                if out.len() >= cap {
                    return out;
                }
                i += needle.len();
            } else {
                i += 1;
            }
        }
    }
    out
}

/// If `(line, col)` sits on a bracket, the position of its matching bracket.
/// Brackets inside strings and comments are ignored on both ends of the scan,
/// so a `)` in a string literal never pairs with real code.
///
/// The scan walks spans and reads raw bytes. Brackets are ASCII, which never
/// appears inside a multi-byte UTF-8 character, so a byte pass sees exactly the
/// characters a char pass would, and a string/comment span is skipped whole
/// without looking at its text at all. Columns are computed only for the match.
///
/// Shape matters here: the highlight set is rebuilt on every view rebuild, and
/// although [`crate::viewer::Viewer::matching_bracket`] memoizes the answer per
/// caret, every caret move pays one scan. The earlier version rebuilt a
/// `Vec<char>` and a `Vec<Option<u8>>` of the current line for every character
/// it stepped over, i.e. O(characters scanned x line length) with two
/// allocations per character: tens of milliseconds with the caret on an
/// ordinary `impl` brace, and seconds at the 4 MB file cap. The cost is now
/// O(bytes between the pair) with a byte compare as the constant.
pub fn matching_bracket(lines: &[HlLine], line: usize, col: usize) -> Option<(usize, usize)> {
    let (span0, byte0, ch, style) = locate(lines, line, col)?;
    // A bracket that is itself inside a string or comment does not participate.
    if is_literal(style) {
        return None;
    }
    let (open, close, forward) = match ch {
        '(' => (b'(', b')', true),
        '[' => (b'[', b']', true),
        '{' => (b'{', b'}', true),
        ')' => (b'(', b')', false),
        ']' => (b'[', b']', false),
        '}' => (b'{', b'}', false),
        _ => return None,
    };

    // The starting bracket itself takes depth to +-1, so depth can only come
    // back to 0 on the bracket that closes the pair — no other character can
    // end the scan, which is why only bracket bytes need a depth check.
    let mut depth: i32 = 0;
    if forward {
        let (mut from_span, mut from_byte) = (span0, byte0);
        for (l, hl) in lines.iter().enumerate().skip(line) {
            for idx in from_span..hl.spans.len() {
                let (text, style) = &hl.spans[idx];
                if is_literal(*style) {
                    continue;
                }
                let from = if idx == from_span { from_byte } else { 0 };
                for (k, &b) in text.as_bytes()[from..].iter().enumerate() {
                    if b == open {
                        depth += 1;
                    } else if b == close {
                        depth -= 1;
                    } else {
                        continue;
                    }
                    if depth == 0 {
                        return Some((l, column_of(&hl.spans, idx, from + k)));
                    }
                }
            }
            (from_span, from_byte) = (0, 0);
        }
    } else {
        let mut on_start_line = true;
        for l in (0..=line).rev() {
            let spans = &lines[l].spans;
            // On the starting line the scan begins at the caret's span; every
            // earlier line is scanned from its last span back.
            let upper = if on_start_line {
                span0 + 1
            } else {
                spans.len()
            };
            for idx in (0..upper.min(spans.len())).rev() {
                let (text, style) = &spans[idx];
                if is_literal(*style) {
                    continue;
                }
                let to = if on_start_line && idx == span0 {
                    byte0 + 1 // the caret's bracket is ASCII, so this is a boundary
                } else {
                    text.len()
                };
                for (k, &b) in text.as_bytes()[..to].iter().enumerate().rev() {
                    if b == open {
                        depth += 1;
                    } else if b == close {
                        depth -= 1;
                    } else {
                        continue;
                    }
                    if depth == 0 {
                        return Some((l, column_of(spans, idx, k)));
                    }
                }
            }
            on_start_line = false;
        }
    }
    None
}

/// Leading-space indent of a line, or `None` if the line is blank.
fn indent(chars: &[char]) -> Option<usize> {
    let mut n = 0;
    for &c in chars {
        if c == ' ' {
            n += 1;
        } else {
            return Some(n);
        }
    }
    None
}

/// Enclosing block-opener lines for sticky scroll, read off the precomputed
/// fold ranges: the header of every fold whose body contains `first_visible`,
/// outermost first, capped to the innermost `max`.
///
/// This is O(folds) with no per-line work, so it stays cheap to recompute every
/// frame even when scrolling deep inside a huge function. (The earlier version
/// scanned every line upward to the enclosing top-level item, allocating a char
/// vector per line — thousands of allocations per frame in a 2000-line `match`,
/// which showed up as scroll jank.)
pub fn sticky_headers(folds: &[(usize, usize)], first_visible: usize, max: usize) -> Vec<usize> {
    if first_visible == 0 {
        return Vec::new();
    }
    // Folds nest, so an enclosing fold always opens on an earlier line than the
    // ones it contains: sorting header lines ascending orders them outermost to
    // innermost, and the innermost `max` are the ones worth pinning.
    let mut headers: Vec<usize> = folds
        .iter()
        .filter(|&&(header, end)| header < first_visible && first_visible <= end)
        .map(|&(header, _)| header)
        .collect();
    headers.sort_unstable();
    if headers.len() > max {
        headers = headers.split_off(headers.len() - max);
    }
    headers
}

/// Indentation-based fold ranges as `(header, end)` pairs, where the fold hides
/// lines `header+1 ..= end`. A line is a fold header when the block of lines
/// after it is indented deeper (blank lines inside the block are tolerated).
/// Ranges nest naturally: an `impl` and a `fn` inside it each get their own.
/// Language-agnostic — it reads indentation, not syntax.
pub fn fold_ranges(lines: &[HlLine]) -> Vec<(usize, usize)> {
    let ind: Vec<Option<usize>> = (0..lines.len())
        .map(|i| indent(&line_chars(lines, i)))
        .collect();
    let mut out = Vec::new();
    for (i, di) in ind.iter().enumerate() {
        let Some(di) = *di else { continue };
        // Extend the block while following lines are blank or deeper-indented;
        // `end` tracks the last non-blank deeper line so trailing blanks are
        // excluded from the fold.
        let mut end = i;
        let mut j = i + 1;
        while j < lines.len() {
            match ind[j] {
                None => j += 1, // blank: tolerated, does not extend the block
                Some(dj) if dj > di => {
                    end = j;
                    j += 1;
                }
                Some(_) => break, // same or shallower indent ends the block
            }
        }
        if end > i {
            out.push((i, end));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::highlight::plain_lines;

    #[test]
    fn word_under_cursor() {
        let lines = plain_lines("let count = count + 1;\n");
        assert_eq!(word_at(&lines, 0, 4).as_deref(), Some("count")); // on 'c'
        assert_eq!(word_at(&lines, 0, 6).as_deref(), Some("count")); // mid-word
        assert_eq!(word_at(&lines, 0, 3), None); // space
    }

    /// The word under the caret is read from a window around it, not from
    /// the whole line: identical answers on ordinary lines, bounded work on a
    /// minified one — where the caret can sit two million columns in.
    #[test]
    fn word_lookups_read_a_window_not_the_line() {
        let lines = plain_lines("let 名前 = count_all(日本);\n");
        assert_eq!(word_at(&lines, 0, 4).as_deref(), Some("名前"));
        assert_eq!(word_range_at(&lines, 0, 13), Some((9, 18)));
        assert_eq!(word_at(&lines, 0, 20).as_deref(), Some("日本"));
        assert_eq!(word_at(&lines, 0, 8), None); // `=`
        assert_eq!(word_at(&lines, 9, 0), None); // no such line

        let huge = format!("{} tail_word", "ab ".repeat(700_000));
        let lines = plain_lines(&huge);
        let col = huge.len() - 3; // inside `tail_word`
        assert_eq!(word_at(&lines, 0, col).as_deref(), Some("tail_word"));
        // An identifier longer than the window is cut at its edges: what is
        // read is the window around the caret, not the line (a whole-line
        // read would return the identifier's full extent here).
        let long = plain_lines(&"x".repeat(5 * MAX_WORD_CHARS));
        let (s, e) = word_range_at(&long, 0, 2 * MAX_WORD_CHARS).unwrap();
        assert_eq!((s, e), (MAX_WORD_CHARS, 3 * MAX_WORD_CHARS + 1));
    }

    /// The window agrees with slicing the materialized line, across span
    /// boundaries and multi-byte text.
    #[test]
    fn line_window_matches_the_materialized_line() {
        use crate::highlight::highlight_lines;
        let src = "let s = \"日本語\"; // コメント x\n";
        let lines = highlight_lines(src, Some("rust"));
        let all: Vec<char> = line_chars(&lines, 0);
        for from in 0..all.len() + 2 {
            for to in from..all.len() + 3 {
                let want: Vec<char> = all.iter().copied().skip(from).take(to - from).collect();
                assert_eq!(line_window(&lines[0], from, to), want, "[{from}, {to})");
            }
        }
    }

    #[test]
    fn occurrences_are_whole_word() {
        let lines = plain_lines("count counter count\n");
        // "count" matches twice, not inside "counter".
        assert_eq!(
            occurrences("count", &lines, 100),
            vec![(0, 0, 5), (0, 14, 19)]
        );
    }

    #[test]
    fn brackets_match_across_lines() {
        let lines = plain_lines("fn f() {\n    g([1, 2]);\n}\n");
        // '{' at line 0 col 7 → '}' at line 2 col 0.
        assert_eq!(matching_bracket(&lines, 0, 7), Some((2, 0)));
        // line 1: "    g([1, 2]);" → '(' at 5, ')' at 12.
        assert_eq!(matching_bracket(&lines, 1, 5), Some((1, 12)));
        assert_eq!(matching_bracket(&lines, 1, 12), Some((1, 5)));
        // Not on a bracket.
        assert_eq!(matching_bracket(&lines, 0, 0), None);
    }

    #[test]
    fn sticky_headers_are_enclosing_openers() {
        let src = "impl Foo {\n    fn bar() {\n        let x = 1;\n        let y = 2;\n    }\n}\n";
        let folds = fold_ranges(&plain_lines(src));
        // Viewport starting on line 3 (indent 8) is inside bar (line 1, indent
        // 4) inside impl (line 0, indent 0).
        assert_eq!(sticky_headers(&folds, 3, 5), vec![0, 1]);
        // Top of file: nothing pinned.
        assert_eq!(sticky_headers(&folds, 0, 5), Vec::<usize>::new());
        // A line at indent 0 has no enclosing header.
        assert_eq!(sticky_headers(&folds, 5, 5), Vec::<usize>::new());
        // The innermost `max` win when nesting is deeper than the cap.
        assert_eq!(sticky_headers(&folds, 3, 1), vec![1]);
    }

    #[test]
    fn brackets_ignore_parens_in_strings() {
        use crate::highlight::highlight_lines;
        // `let a = f(")");` — the real '(' at col 9 must pair with the real ')'
        // at col 13, not the ')' at col 11 inside the string literal.
        let src = "let a = f(\")\");\n";
        let lines = highlight_lines(src, Some("rust"));
        assert_eq!(matching_bracket(&lines, 0, 9), Some((0, 13)));
        // The ')' inside the string does not pair with anything.
        assert_eq!(matching_bracket(&lines, 0, 11), None);
    }

    #[test]
    fn occurrences_skip_strings_and_comments() {
        use crate::highlight::highlight_lines;
        // `foo` appears as code, in a string, and in a comment; only the code
        // occurrence counts.
        let src = "let foo = 1;\nlet s = \"foo\";\n// foo\n";
        let lines = highlight_lines(src, Some("rust"));
        let occ = occurrences("foo", &lines, 100);
        assert_eq!(occ, vec![(0, 4, 7)]);
    }

    /// Reference implementation of bracket matching: the plain per-character
    /// walk over the materialized line, kept only to prove the span/byte scan
    /// answers identically on every position of a mixed buffer.
    fn naive_matching_bracket(lines: &[HlLine], line: usize, col: usize) -> Option<(usize, usize)> {
        let chars_of = |l: usize| -> Vec<char> {
            lines
                .get(l)
                .map(|x| x.spans.iter().flat_map(|(t, _)| t.chars()).collect())
                .unwrap_or_default()
        };
        let styles_of = |l: usize| -> Vec<Option<u8>> {
            lines
                .get(l)
                .map(|x| {
                    x.spans
                        .iter()
                        .flat_map(|(t, s)| t.chars().map(move |_| *s))
                        .collect()
                })
                .unwrap_or_default()
        };
        let literal_at = |l: usize, c: usize| -> bool {
            styles_of(l)
                .get(c)
                .copied()
                .flatten()
                .is_some_and(highlight::style_is_literal)
        };
        let ch = *chars_of(line).get(col)?;
        if literal_at(line, col) {
            return None;
        }
        let (open, close, forward) = match ch {
            '(' => ('(', ')', true),
            '[' => ('[', ']', true),
            '{' => ('{', '}', true),
            ')' => ('(', ')', false),
            ']' => ('[', ']', false),
            '}' => ('{', '}', false),
            _ => return None,
        };
        let mut depth: i32 = 0;
        let (mut l, mut c) = (line, col);
        loop {
            let chars = chars_of(l);
            let cur = chars.get(c).copied();
            if let Some(cur) = cur.filter(|_| !literal_at(l, c)) {
                if cur == open {
                    depth += 1;
                } else if cur == close {
                    depth -= 1;
                }
                if depth == 0 {
                    return Some((l, c));
                }
            }
            if forward {
                if c + 1 < chars.len() {
                    c += 1;
                } else if l + 1 < lines.len() {
                    l += 1;
                    c = 0;
                } else {
                    return None;
                }
            } else if c > 0 {
                c -= 1;
            } else if l > 0 {
                l -= 1;
                c = chars_of(l).len().saturating_sub(1);
                if chars_of(l).is_empty() {
                    continue;
                }
            } else {
                return None;
            }
        }
    }

    /// The span/byte scan must answer exactly what the per-character walk did,
    /// at every position of a buffer with nesting, unbalanced brackets, string
    /// and comment literals, multi-byte characters and empty lines.
    #[test]
    fn bracket_scan_matches_the_character_walk_everywhere() {
        use crate::highlight::highlight_lines;
        let src = "fn f(a: [u8; 2]) -> Result<(), E> {\n\
                   \n\
                       let s = \"（unbalanced ( and } inside\";\n\
                       // a comment with ) and {\n\
                       let 日本 = g([1, (2, 3)], h{});\n\
                       if (x) { y([z]) } else { w(()) }\n\
                   }\n\
                   let stray = (;\n";
        for lines in [highlight_lines(src, Some("rust")), plain_lines(src)] {
            for (li, l) in lines.iter().enumerate() {
                let cols: usize = l.spans.iter().map(|(t, _)| t.chars().count()).sum();
                for c in 0..cols + 2 {
                    assert_eq!(
                        matching_bracket(&lines, li, c),
                        naive_matching_bracket(&lines, li, c),
                        "line {li} col {c}"
                    );
                }
            }
        }
    }

    /// R4-16: `code_highlights` runs this inline while building the widget tree,
    /// so it re-runs on every view rebuild — a mouse move over the code area is
    /// enough. The per-character version rebuilt two Vecs of the whole current
    /// line for every character it stepped over, so matching an outer brace cost
    /// O(characters scanned x line length): 3.56 s for this buffer in a debug
    /// build, ~44 ms per frame on a real 131 KB source file. The span/byte scan
    /// does it in 3.5 ms. The bound is loose on purpose — it is guarding the
    /// O(n x m) shape, not a wall-clock target.
    #[test]
    fn bracket_scan_does_not_rescan_the_line_per_character() {
        let mut src = String::from("{\n");
        for i in 0..1000 {
            src.push_str(&format!("    x{i} = \"{}\";\n", "a".repeat(280)));
        }
        src.push_str("}\n");
        let lines = plain_lines(&src);
        let t = std::time::Instant::now();
        assert_eq!(matching_bracket(&lines, 0, 0), Some((1001, 0)));
        let took = t.elapsed();
        assert!(
            took < std::time::Duration::from_millis(500),
            "whole-file bracket scan took {took:?}"
        );
    }

    /// Same rebuild path, whole-file shape: `occurrences` visits every line of
    /// the file per frame (`cap` bounds matches returned, not lines scanned), so
    /// it must not materialize a line that cannot hold the word. Two fresh Vecs
    /// per line of this 2 MB buffer took 141 ms in a debug build; skipping the
    /// lines whose spans cannot contain the word's first character takes 0.4 ms.
    #[test]
    fn occurrences_skip_lines_that_cannot_match() {
        let mut src = String::new();
        for i in 0..5000 {
            src.push_str(&format!("    x{i} = {};\n", "a".repeat(380)));
        }
        let lines = plain_lines(&src);
        let t = std::time::Instant::now();
        assert!(occurrences("zzz_absent", &lines, 500).is_empty());
        let took = t.elapsed();
        assert!(
            took < std::time::Duration::from_millis(20),
            "whole-file occurrence scan took {took:?}"
        );
    }

    #[test]
    fn fold_ranges_are_indented_blocks() {
        let src = "impl Foo {\n    fn bar() {\n        let x = 1;\n        let y = 2;\n    }\n\n    fn baz() {\n        ok();\n    }\n}\n";
        let lines = plain_lines(src);
        // impl (line 0) folds through its last deeper line (line 8).
        // bar (line 1) folds lines 2..=3; baz (line 6) folds line 7.
        let folds = fold_ranges(&lines);
        assert!(folds.contains(&(0, 8)));
        assert!(folds.contains(&(1, 3)));
        assert!(folds.contains(&(6, 7)));
        // A single-line body with no deeper following line is not foldable.
        let flat = plain_lines("a\nb\nc\n");
        assert_eq!(fold_ranges(&flat), Vec::<(usize, usize)>::new());
    }
}
