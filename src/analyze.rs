//! Cursor-derived reading aids computed from the display text: the identifier
//! under the cursor (for occurrence highlight) and bracket matching.

use crate::highlight::HlLine;

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Display characters of one line (spans concatenated, tabs expanded).
fn line_chars(lines: &[HlLine], line: usize) -> Vec<char> {
    lines
        .get(line)
        .map(|l| l.spans.iter().flat_map(|(t, _)| t.chars()).collect())
        .unwrap_or_default()
}

/// The identifier under `(line, col)`, if any, as its text.
pub fn word_at(lines: &[HlLine], line: usize, col: usize) -> Option<String> {
    let chars = line_chars(lines, line);
    if col >= chars.len() || !is_word(chars[col]) {
        return None;
    }
    let mut start = col;
    while start > 0 && is_word(chars[start - 1]) {
        start -= 1;
    }
    let mut end = col;
    while end < chars.len() && is_word(chars[end]) {
        end += 1;
    }
    Some(chars[start..end].iter().collect())
}

/// Whole-word occurrences of `word` across `lines`, as (line, col0, col1) in
/// display columns. Capped to keep highlighting cheap on huge files.
pub fn occurrences(word: &str, lines: &[HlLine], cap: usize) -> Vec<(usize, usize, usize)> {
    let needle: Vec<char> = word.chars().collect();
    if needle.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (li, line) in lines.iter().enumerate() {
        let chars = line_chars(lines, li);
        let _ = line;
        let mut i = 0;
        while i + needle.len() <= chars.len() {
            let is_match = chars[i..i + needle.len()] == needle[..]
                && (i == 0 || !is_word(chars[i - 1]))
                && (i + needle.len() == chars.len() || !is_word(chars[i + needle.len()]));
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
pub fn matching_bracket(
    lines: &[HlLine],
    line: usize,
    col: usize,
) -> Option<(usize, usize)> {
    let ch = *line_chars(lines, line).get(col)?;
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
    // Walk from the bracket outward, one character at a time.
    let mut l = line;
    let mut c = col;
    loop {
        let chars = line_chars(lines, l);
        let cur = chars.get(c).copied();
        if let Some(cur) = cur {
            if cur == open {
                depth += 1;
            } else if cur == close {
                depth -= 1;
            }
            if depth == 0 {
                return Some((l, c));
            }
        }
        // Advance.
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
            c = line_chars(lines, l).len().saturating_sub(1);
            if line_chars(lines, l).is_empty() {
                continue;
            }
        } else {
            return None;
        }
    }
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

/// Enclosing block-opener lines for sticky scroll: given the first visible
/// line, the lines above it whose indentation is progressively smaller, from
/// outermost to innermost (indentation-based, language-agnostic).
pub fn sticky_headers(lines: &[HlLine], first_visible: usize, max: usize) -> Vec<usize> {
    if first_visible == 0 {
        return Vec::new();
    }
    // Indentation of the content the viewport starts on.
    let base = (first_visible..lines.len())
        .find_map(|i| indent(&line_chars(lines, i)).map(|n| (i, n)));
    let Some((_, mut min_indent)) = base else {
        return Vec::new();
    };
    if min_indent == 0 {
        return Vec::new();
    }
    let mut headers = Vec::new();
    for i in (0..first_visible).rev() {
        if let Some(ind) = indent(&line_chars(lines, i))
            && ind < min_indent
        {
            headers.push(i);
            min_indent = ind;
            if headers.len() >= max || min_indent == 0 {
                break;
            }
        }
    }
    headers.reverse();
    headers
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

    #[test]
    fn occurrences_are_whole_word() {
        let lines = plain_lines("count counter count\n");
        // "count" matches twice, not inside "counter".
        assert_eq!(occurrences("count", &lines, 100), vec![(0, 0, 5), (0, 14, 19)]);
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
        let lines = plain_lines(src);
        // Viewport starting on line 3 (indent 8) is inside bar (line 1, indent
        // 4) inside impl (line 0, indent 0).
        assert_eq!(sticky_headers(&lines, 3, 5), vec![0, 1]);
        // Top of file: nothing pinned.
        assert_eq!(sticky_headers(&lines, 0, 5), Vec::<usize>::new());
        // A line at indent 0 has no enclosing header.
        assert_eq!(sticky_headers(&lines, 5, 5), Vec::<usize>::new());
    }
}
