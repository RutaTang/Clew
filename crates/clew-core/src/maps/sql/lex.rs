//! SQL as tokens and statements, for every dialect a project's SQL is
//! written in: PostgreSQL's dollar-quoted bodies and `DO` blocks, MySQL's
//! backticks, `#` comments and `DELIMITER`, SQL Server's `[brackets]` and
//! `GO` batches, SQLite's mix of all of them.

/// What a token is.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Tok {
    /// A bare word: a keyword, a name, a number.
    Word(String),
    /// A quoted identifier (`"x"`, `` `x` ``, `[x]`), unquoted.
    Ident(String),
    /// A string literal's contents (`'it''s'` → `it's`).
    Str(String),
    /// A dollar-quoted body (`$$ … $$`, `$fn$ … $fn$`): its contents, and
    /// the byte offset they start at.
    Dollar {
        body: String,
        at: usize,
    },
    Punct(char),
}

#[derive(Debug, Clone)]
pub(super) struct Token {
    pub tok: Tok,
    /// Byte offset in the text.
    pub at: usize,
    /// Byte offset just past it.
    pub end: usize,
}

impl Token {
    pub fn word_upper(&self) -> Option<String> {
        match &self.tok {
            Tok::Word(w) => Some(w.to_ascii_uppercase()),
            _ => None,
        }
    }

    pub fn is_word(&self, w: &str) -> bool {
        matches!(&self.tok, Tok::Word(x) if x.eq_ignore_ascii_case(w))
    }

    pub fn is_any(&self, words: &[&str]) -> bool {
        words.iter().any(|w| self.is_word(w))
    }

    pub fn is(&self, c: char) -> bool {
        self.tok == Tok::Punct(c)
    }

    /// The name it is, bare or quoted.
    pub fn name(&self) -> Option<&str> {
        match &self.tok {
            Tok::Word(w) | Tok::Ident(w) => Some(w),
            _ => None,
        }
    }

    /// A string literal's contents.
    pub fn string(&self) -> Option<&str> {
        match &self.tok {
            Tok::Str(s) => Some(s),
            _ => None,
        }
    }
}

/// `text` as tokens, offsets counted from `base` (the place `text` has in
/// the file it was cut from).
pub(super) fn tokenize_at(text: &str, base: usize) -> Vec<Token> {
    let b = text.as_bytes();
    let mut out: Vec<Token> = Vec::new();
    let mut i = 0;
    let push = |out: &mut Vec<Token>, tok: Tok, at: usize, end: usize| {
        out.push(Token {
            tok,
            at: base + at,
            end: base + end,
        });
    };
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
        } else if c == b'-' && b.get(i + 1) == Some(&b'-') {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if c == b'#'
            && starts_line(b, i)
            && b.get(i + 1).is_none_or(|n| n.is_ascii_whitespace())
        {
            // MySQL's line comment, where a line starts with it.
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if c == b'/' && b.get(i + 1) == Some(&b'*') {
            // Block comments nest in PostgreSQL.
            let mut depth = 0;
            while i < b.len() {
                if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                    depth += 1;
                    i += 2;
                } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    i += 1;
                }
            }
        } else if c == b'\'' {
            let start = i;
            i += 1;
            let mut value = Vec::new();
            while i < b.len() {
                if b[i] == b'\'' {
                    if b.get(i + 1) == Some(&b'\'') {
                        value.push(b'\'');
                        i += 2;
                        continue;
                    }
                    break;
                }
                if b[i] == b'\\' && i + 1 < b.len() {
                    i += 1;
                }
                value.push(b[i]);
                i += 1;
            }
            i = (i + 1).min(b.len());
            push(
                &mut out,
                Tok::Str(String::from_utf8_lossy(&value).into_owned()),
                start,
                i,
            );
        } else if c == b'"' || c == b'`' || (c == b'[' && !follows_a_value(b, i)) {
            let close = match c {
                b'[' => b']',
                other => other,
            };
            let start = i;
            i += 1;
            let mut name = Vec::new();
            while i < b.len() && b[i] != b'\n' {
                if b[i] == close {
                    // A doubled quote is the quote itself.
                    if close != b']' && b.get(i + 1) == Some(&close) {
                        name.push(close);
                        i += 2;
                        continue;
                    }
                    break;
                }
                name.push(b[i]);
                i += 1;
            }
            i = (i + 1).min(b.len());
            push(
                &mut out,
                Tok::Ident(String::from_utf8_lossy(&name).into_owned()),
                start,
                i,
            );
        } else if c == b'$' {
            // A dollar-quoted body: `$$ … $$` or `$tag$ … $tag$`; a `$1`
            // parameter otherwise.
            let start = i;
            let mut j = i + 1;
            while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                j += 1;
            }
            let tag_ok = b.get(j) == Some(&b'$') && !b.get(i + 1).is_some_and(u8::is_ascii_digit);
            if tag_ok {
                let tag = &text[i..=j];
                let body_at = j + 1;
                let (body_end, after) = match text[body_at..].find(tag) {
                    Some(end) => (body_at + end, body_at + end + tag.len()),
                    None => (b.len(), b.len()),
                };
                i = after;
                push(
                    &mut out,
                    Tok::Dollar {
                        body: text[body_at..body_end].to_string(),
                        at: base + body_at,
                    },
                    start,
                    i,
                );
            } else {
                push(&mut out, Tok::Punct('$'), start, start + 1);
                i = start + 1;
            }
        } else if c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80 {
            let start = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] >= 0x80) {
                i += 1;
            }
            push(&mut out, Tok::Word(text[start..i].to_string()), start, i);
        } else {
            push(&mut out, Tok::Punct(c as char), i, i + 1);
            i += 1;
        }
    }
    out
}

pub(super) fn tokenize(text: &str) -> Vec<Token> {
    tokenize_at(text, 0)
}

/// Whether only blanks precede `i` on its line.
fn starts_line(b: &[u8], i: usize) -> bool {
    b[..i]
        .iter()
        .rev()
        .take_while(|c| **c != b'\n')
        .all(|c| *c == b' ' || *c == b'\t' || *c == b'\r')
}

/// Whether the `[` at `i` follows a value right away (`text[]`,
/// `arr[1]`): an array's bracket, not SQL Server's quoted name.
fn follows_a_value(b: &[u8], i: usize) -> bool {
    i > 0 && (b[i - 1].is_ascii_alphanumeric() || matches!(b[i - 1], b'_' | b']' | b')' | b'"'))
}

/// A statement: its tokens (a `DO` block's DDL is one of its own, read as
/// run: a migration's `IF NOT EXISTS … THEN` guards a change it means to
/// make).
#[derive(Debug, Clone)]
pub(super) struct Statement {
    pub tokens: Vec<Token>,
}

/// The statements of a SQL file, in order: split on `;` (or the delimiter
/// a MySQL `DELIMITER` line sets) and on SQL Server's `GO` lines; the DDL a
/// PostgreSQL `DO` block runs is read as statements of its own.
pub(super) fn statements(text: &str) -> Vec<Statement> {
    let mut out = Vec::new();
    for (region, base, delimiter) in delimiter_regions(text) {
        match delimiter.as_str() {
            ";" => split(&tokenize_at(region, base), text, &mut out),
            // Between MySQL's `DELIMITER //` lines a statement (a
            // procedure, a trigger) holds `;`s of its own.
            other => {
                let mut at = 0;
                for piece in region.split(other) {
                    let tokens = tokenize_at(piece, base + at);
                    if !tokens.is_empty() {
                        out.push(Statement { tokens });
                    }
                    at += piece.len() + other.len();
                }
            }
        }
    }
    out
}

/// `text` cut where MySQL's `DELIMITER` lines change the statement
/// delimiter: each region, its offset, and its delimiter (the lines
/// themselves left out).
fn delimiter_regions(text: &str) -> Vec<(&str, usize, String)> {
    let mut out = Vec::new();
    let mut delimiter = ";".to_string();
    let mut start = 0;
    let mut at = 0;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim();
        let set = trimmed
            .get(..10)
            .filter(|head| head.eq_ignore_ascii_case("DELIMITER "))
            .map(|_| trimmed[10..].trim());
        if let Some(next) = set.filter(|d| !d.is_empty()) {
            out.push((&text[start..at], start, delimiter.clone()));
            delimiter = next.to_string();
            start = at + line.len();
        }
        at += line.len();
    }
    out.push((&text[start..], start, delimiter));
    out
}

/// Split `tokens` into statements on `;` and `GO` lines, reading into `DO`
/// blocks.
fn split(tokens: &[Token], text: &str, out: &mut Vec<Statement>) {
    let mut current: Vec<Token> = Vec::new();
    let flush = |current: &mut Vec<Token>, out: &mut Vec<Statement>| {
        if current.is_empty() {
            return;
        }
        let tokens = std::mem::take(current);
        if tokens[0].is_word("DO") {
            out.extend(do_block(&tokens, text));
        } else {
            out.push(Statement { tokens });
        }
    };
    let mut i = 0;
    while i < tokens.len() {
        let token = &tokens[i];
        if token.is(';') {
            flush(&mut current, out);
        } else if token.is_word("GO") && alone_on_its_line(text, token) {
            flush(&mut current, out);
            // `GO 2`: the count is the batch's, not the next statement's.
            if tokens.get(i + 1).is_some_and(|n| {
                matches!(&n.tok, Tok::Word(w) if w.chars().all(|c| c.is_ascii_digit()))
                    && !text[token.end.min(text.len())..n.at.min(text.len())].contains('\n')
            }) {
                i += 1;
            }
        } else {
            current.push(token.clone());
        }
        i += 1;
    }
    flush(&mut current, out);
}

/// Whether `token` is the only thing on its line (SQL Server's `GO`, which
/// may be followed by a count).
fn alone_on_its_line(text: &str, token: &Token) -> bool {
    let b = text.as_bytes();
    if token.at > b.len() || !starts_line(b, token.at) {
        return false;
    }
    let rest = text[token.end.min(text.len())..]
        .lines()
        .next()
        .unwrap_or("");
    rest.trim().chars().all(|c| c.is_ascii_digit())
}

/// The DDL a `DO` block runs: its body's statements, each from where the
/// DDL in it starts (past an `IF … THEN`, a `BEGIN`, an `ELSE`).
fn do_block(tokens: &[Token], text: &str) -> Vec<Statement> {
    let Some((body, at)) = tokens.iter().find_map(|t| match &t.tok {
        Tok::Dollar { body, at } => Some((body.as_str(), *at)),
        _ => None,
    }) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut inner = Vec::new();
    split(&tokenize_at(body, at), text, &mut inner);
    for statement in inner {
        let t = &statement.tokens;
        let start = (0..t.len()).find(|&i| {
            t[i].is_any(DDL_STARTS)
                && (i == 0
                    || t[i - 1].is_any(&["THEN", "ELSE", "BEGIN", "LOOP", "DECLARE"])
                    || t[i - 1].is(';'))
        });
        if let Some(start) = start {
            out.push(Statement {
                tokens: t[start..].to_vec(),
            });
        }
    }
    out
}

/// The words a statement that changes the schema starts with.
const DDL_STARTS: &[&str] = &["CREATE", "ALTER", "DROP", "COMMENT"];

#[cfg(test)]
mod tests {
    use super::*;

    fn words(s: &Statement) -> String {
        s.tokens
            .iter()
            .map(|t| match &t.tok {
                Tok::Word(w) | Tok::Ident(w) => w.clone(),
                Tok::Str(s) => format!("'{s}'"),
                Tok::Dollar { .. } => "$$".into(),
                Tok::Punct(c) => c.to_string(),
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn strings_comments_quotes_and_bodies_are_tokens() {
        let tokens = tokenize(
            "SELECT 'a;b''c' -- c;\n, \"Q\"\"x\".x /* /* ; */ */ $f$ ; $f$ [id] text[] $1",
        );
        let kinds: Vec<&Tok> = tokens.iter().map(|t| &t.tok).collect();
        assert_eq!(
            kinds,
            [
                &Tok::Word("SELECT".into()),
                &Tok::Str("a;b'c".into()),
                &Tok::Punct(','),
                &Tok::Ident("Q\"x".into()),
                &Tok::Punct('.'),
                &Tok::Word("x".into()),
                &Tok::Dollar {
                    body: " ; ".into(),
                    at: 50,
                },
                &Tok::Ident("id".into()),
                &Tok::Word("text".into()),
                &Tok::Punct('['),
                &Tok::Punct(']'),
                &Tok::Punct('$'),
                &Tok::Word("1".into()),
            ]
        );
    }

    #[test]
    fn statements_split_on_semicolons_go_and_mysql_delimiters() {
        assert_eq!(statements("a; b;; c").len(), 3);
        let tsql =
            "CREATE TABLE [dbo].[A] (x int)\nGO\nCREATE TABLE B (y int)\nGO 2\nSELECT go FROM t";
        let s: Vec<String> = statements(tsql).iter().map(words).collect();
        assert_eq!(
            s,
            [
                "CREATE TABLE dbo . A ( x int )",
                "CREATE TABLE B ( y int )",
                "SELECT go FROM t"
            ]
        );
        let mysql = "CREATE TABLE a (x int);\nDELIMITER //\nCREATE PROCEDURE p() BEGIN SELECT 1; SELECT 2; END //\nDELIMITER ;\nCREATE TABLE b (y int);\n# a comment\nDROP TABLE a;";
        let s: Vec<String> = statements(mysql).iter().map(words).collect();
        assert_eq!(
            s,
            [
                "CREATE TABLE a ( x int )",
                "CREATE PROCEDURE p ( ) BEGIN SELECT 1 ; SELECT 2 ; END",
                "CREATE TABLE b ( y int )",
                "DROP TABLE a"
            ]
        );
    }

    #[test]
    fn a_do_blocks_ddl_is_read_with_its_place() {
        let text = "DO $$\nBEGIN\n  IF NOT EXISTS (SELECT 1 FROM x) THEN\n    ALTER TABLE storage.objects ADD COLUMN owner_id text;\n  END IF;\n  EXECUTE 'x';\nEND$$;\nCREATE TABLE t (a int);";
        let s = statements(text);
        let said: Vec<String> = s.iter().map(words).collect();
        assert_eq!(
            said,
            [
                "ALTER TABLE storage . objects ADD COLUMN owner_id text",
                "CREATE TABLE t ( a int )"
            ]
        );
        assert_eq!(super::super::super::line_at(text, s[0].tokens[0].at), 4);
    }
}
