//! The project's database schema where its code names it: a table or a
//! column under the pointer peeks as the schema has it — a table's columns
//! with their types and marks, a column's type, default and what it
//! references — and Go to Definition goes to where it is made, with no
//! language server for SQL. In a SQL file every name is looked up; in other
//! code, a name on a line of SQL (`"SELECT … FROM users"`).
//!
//! The schema is the project's schema maps (`clew_core::maps::sql`), read
//! once a SQL file is opened.

use std::path::PathBuf;

use clew_protocol::{MapField, ProjectMap};

use crate::app::prelude::*;
use crate::*;

/// What a word names: a table (or view, or enum) of a schema map, and one
/// of its columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SchemaHit {
    pub(crate) map: usize,
    pub(crate) node: usize,
    pub(crate) field: Option<usize>,
}

/// The identifier at char `col` of `line`, with the names dotted before and
/// after it (`storage.objects.id` at `objects`: `storage`, `objects`, `id`),
/// quotes and brackets aside.
pub(crate) fn dotted_at(
    line: &str,
    col: usize,
) -> Option<(Option<String>, String, Option<String>)> {
    let chars: Vec<char> = line.chars().collect();
    let ident = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
    let at = col.min(chars.len().saturating_sub(1));
    if !chars.get(at).is_some_and(|c| ident(*c)) {
        return None;
    }
    let mut start = at;
    while start > 0 && ident(chars[start - 1]) {
        start -= 1;
    }
    let mut end = at + 1;
    while end < chars.len() && ident(chars[end]) {
        end += 1;
    }
    let word: String = chars[start..end].iter().collect();
    let quote = |c: char| matches!(c, '"' | '`' | '[' | ']');
    // Back past a quote, a dot and a quote to the name before.
    let before = {
        let mut i = start;
        if i > 0 && quote(chars[i - 1]) {
            i -= 1;
        }
        if i > 0 && chars[i - 1] == '.' {
            i -= 1;
            if i > 0 && quote(chars[i - 1]) {
                i -= 1;
            }
            let e = i;
            while i > 0 && ident(chars[i - 1]) {
                i -= 1;
            }
            (i < e).then(|| chars[i..e].iter().collect::<String>())
        } else {
            None
        }
    };
    let after = {
        let mut i = end;
        if i < chars.len() && quote(chars[i]) {
            i += 1;
        }
        if i < chars.len() && chars[i] == '.' {
            i += 1;
            if i < chars.len() && quote(chars[i]) {
                i += 1;
            }
            let s = i;
            while i < chars.len() && ident(chars[i]) {
                i += 1;
            }
            (s < i).then(|| chars[s..i].iter().collect::<String>())
        } else {
            None
        }
    };
    Some((before, word, after))
}

/// Whether a line of code reads as SQL: a statement's words on it.
pub(crate) fn reads_as_sql(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    [
        "select ",
        "from ",
        "join ",
        "insert into",
        "update ",
        "delete from",
        "references ",
        "table ",
        "into ",
    ]
    .iter()
    .any(|k| lower.contains(k))
}

/// Whether `word` is a word of SQL itself, which no alias is.
fn is_keyword(word: &str) -> bool {
    [
        "on",
        "join",
        "inner",
        "left",
        "right",
        "full",
        "cross",
        "outer",
        "where",
        "group",
        "order",
        "having",
        "limit",
        "union",
        "using",
        "set",
        "values",
        "returning",
        "natural",
        "lateral",
        "select",
        "from",
        "and",
        "or",
        "as",
        "with",
        "add",
        "column",
        "drop",
        "alter",
        "to",
        "references",
        "for",
        "into",
        "if",
        "not",
        "exists",
        "only",
        "default",
        "primary",
        "key",
        "foreign",
        "constraint",
        "unique",
        "check",
        "rename",
        "owner",
        "enable",
        "disable",
    ]
    .iter()
    .any(|k| k.eq_ignore_ascii_case(word))
}

/// `storage.objects` → (`storage`, `objects`).
fn split_label(label: &str) -> (Option<&str>, &str) {
    match label.rsplit_once('.') {
        Some((s, n)) if !s.is_empty() && !n.is_empty() => (Some(s), n),
        _ => (None, label),
    }
}

fn is_default_schema(s: &str) -> bool {
    ["public", "dbo", "main"]
        .iter()
        .any(|d| s.eq_ignore_ascii_case(d))
}

impl App {
    /// The project's schema maps, each with its place among the maps.
    fn schema_maps(&self) -> impl Iterator<Item = (usize, &ProjectMap)> {
        self.proj
            .maps
            .iter()
            .enumerate()
            .filter(|(_, m)| m.id == "sql.schema" || m.id.starts_with("sql.schema:"))
    }

    /// The table `name` names (`qualifier` its schema, when written): a
    /// table, view or enum some file makes.
    pub(crate) fn schema_table(
        &self,
        qualifier: Option<&str>,
        name: &str,
    ) -> Option<(usize, usize)> {
        let mut loose = None;
        for (m, map) in self.schema_maps() {
            for (i, n) in map.nodes.iter().enumerate() {
                if !matches!(n.kind.as_str(), "table" | "view" | "enum") || n.file.is_empty() {
                    continue;
                }
                let (schema, bare) = split_label(&n.label);
                if !bare.eq_ignore_ascii_case(name) {
                    continue;
                }
                let fits = match (qualifier, schema) {
                    (Some(q), Some(s)) => q.eq_ignore_ascii_case(s),
                    (Some(q), None) => is_default_schema(q),
                    (None, _) => true,
                };
                if fits && (qualifier.is_some() || schema.is_none()) {
                    return Some((m, i));
                }
                if fits {
                    loose.get_or_insert((m, i));
                }
            }
        }
        loose
    }

    /// What the word at `(line, col)` of `pane` names in the schema.
    pub(crate) fn schema_hit(&self, pane: usize, line: usize, col: usize) -> Option<SchemaHit> {
        let v = self.proj.panes.get(pane)?.as_ref()?;
        let sql = v.lang_key == Some("sql");
        let text = v.source_line(line)?;
        if !sql && !reads_as_sql(text) {
            return None;
        }
        let (before, word, after) = dotted_at(text, col)?;
        // `schema.table`, or `table.column` at the table.
        if let Some((map, node)) = self.schema_table(before.as_deref(), &word)
            && (before.is_some() || after.is_none() || self.has_column(map, node, after.as_deref()))
        {
            return Some(SchemaHit {
                map,
                node,
                field: None,
            });
        }
        let named = self.statement_tables(pane, line);
        // `table.column` or `alias.column`, at the column.
        if let Some(q) = before.as_deref() {
            let table = named
                .iter()
                .find(|(_, alias)| alias.as_deref().is_some_and(|a| a.eq_ignore_ascii_case(q)))
                .map(|(t, _)| *t)
                .or_else(|| self.schema_table(None, q));
            return table.and_then(|(map, node)| {
                self.column_of(map, node, &word).map(|f| SchemaHit {
                    map,
                    node,
                    field: Some(f),
                })
            });
        }
        // A column of a table the statement names.
        for &((map, node), _) in &named {
            if let Some(f) = self.column_of(map, node, &word) {
                return Some(SchemaHit {
                    map,
                    node,
                    field: Some(f),
                });
            }
        }
        // In SQL, a column only one table has.
        if sql {
            let mut only = None;
            for (m, map) in self.schema_maps() {
                for (i, n) in map.nodes.iter().enumerate() {
                    if n.kind != "table" {
                        continue;
                    }
                    if let Some(f) = n
                        .fields
                        .iter()
                        .position(|f| f.name.eq_ignore_ascii_case(&word))
                    {
                        if only.is_some() {
                            return None;
                        }
                        only = Some(SchemaHit {
                            map: m,
                            node: i,
                            field: Some(f),
                        });
                    }
                }
            }
            return only;
        }
        None
    }

    /// The tables the statement around `line` names, each with the alias
    /// written after it (`FROM posts p`, `JOIN users AS u`).
    fn statement_tables(&self, pane: usize, line: usize) -> Vec<((usize, usize), Option<String>)> {
        let Some(v) = self.proj.panes.get(pane).and_then(Option::as_ref) else {
            return Vec::new();
        };
        let first = (line.saturating_sub(40)..line)
            .rev()
            .find(|&l| {
                v.source_line(l)
                    .is_some_and(|t| t.trim_end().ends_with(';'))
            })
            .map_or(line.saturating_sub(40), |l| l + 1);
        let mut named: Vec<((usize, usize), Option<String>)> = Vec::new();
        for l in first..=(line + 40) {
            let Some(t) = v.source_line(l) else {
                break;
            };
            let chars: Vec<char> = t.chars().collect();
            let ident = |c: char| c.is_alphanumeric() || c == '_';
            let mut k = 0usize;
            while k < chars.len() {
                if !ident(chars[k]) || (k > 0 && ident(chars[k - 1])) {
                    k += 1;
                    continue;
                }
                let Some((q, w, after)) = dotted_at(t, k) else {
                    k += 1;
                    continue;
                };
                let end = k + w.chars().count();
                if after.is_none()
                    && let Some(found) = self.schema_table(q.as_deref(), &w)
                {
                    // The word after it, past `AS`, unless a keyword.
                    let rest: String = chars[end.min(chars.len())..].iter().collect();
                    let mut words = rest
                        .split(|c: char| !ident(c))
                        .filter(|w| !w.is_empty())
                        .take(2);
                    let mut alias = words.next().map(str::to_string);
                    if alias
                        .as_deref()
                        .is_some_and(|a| a.eq_ignore_ascii_case("as"))
                    {
                        alias = words.next().map(str::to_string);
                    }
                    let follows = rest.trim_start().chars().next().is_some_and(ident);
                    let alias = alias.filter(|a| follows && !is_keyword(a));
                    if !named.iter().any(|(t, a)| *t == found && *a == alias) {
                        named.push((found, alias));
                    }
                }
                k = end.max(k + 1);
            }
            if l > line && t.contains(';') {
                break;
            }
        }
        named
    }

    fn column_of(&self, map: usize, node: usize, name: &str) -> Option<usize> {
        self.proj
            .maps
            .get(map)?
            .nodes
            .get(node)?
            .fields
            .iter()
            .position(|f| f.name.eq_ignore_ascii_case(name))
    }

    fn has_column(&self, map: usize, node: usize, name: Option<&str>) -> bool {
        name.is_some_and(|n| self.column_of(map, node, n).is_some())
    }

    /// Where what `hit` names is made: its file and 1-based line.
    pub(crate) fn schema_place(&self, hit: SchemaHit) -> Option<(PathBuf, usize)> {
        let root = self.proj.project.as_ref()?.root.clone();
        let n = self.proj.maps.get(hit.map)?.nodes.get(hit.node)?;
        let (file, line) = match hit.field.and_then(|f| n.fields.get(f)) {
            Some(f) if f.line > 0 => (if f.file.is_empty() { &n.file } else { &f.file }, f.line),
            _ => (&n.file, n.line),
        };
        (!file.is_empty()).then(|| (root.join(file), line.max(1)))
    }

    /// The peek of what `hit` names, as Markdown.
    pub(crate) fn schema_peek(&self, hit: SchemaHit) -> Option<String> {
        let map = self.proj.maps.get(hit.map)?;
        let n = map.nodes.get(hit.node)?;
        let place = |file: &str, line: usize| format!("`{file}:{}`", line.max(1));
        match hit.field.and_then(|f| n.fields.get(f)) {
            Some(f) => {
                let mut out = vec![format!("**{}.{}** `{}`", n.label, f.name, f.ty)];
                out.push(marks(f).join(" · "));
                if !f.default.is_empty() {
                    out.push(match f.generated {
                        true => tr!(
                            "Generated from {expression}",
                            expression = format!("`{}`", f.default)
                        )
                        .to_string(),
                        false => {
                            tr!("Default {value}", value = format!("`{}`", f.default)).to_string()
                        }
                    });
                }
                let field = hit.field.unwrap_or(0);
                for (target, column) in references(map, hit.node, field) {
                    out.push(
                        tr!("References {target}", target = format!("{target}.{column}"))
                            .to_string(),
                    );
                }
                if !f.doc.is_empty() {
                    out.push(f.doc.clone());
                }
                let file = if f.file.is_empty() { &n.file } else { &f.file };
                out.push(place(file, if f.line > 0 { f.line } else { n.line }));
                Some(out.join("\n\n"))
            }
            None => {
                let kind = crate::i18n::lookup(&n.kind).to_string();
                let mut out = vec![format!(
                    "**{}** · {kind} · {}",
                    n.label,
                    place(&n.file, n.line)
                )];
                if !n.fields.is_empty() {
                    // Aligned in columns, a long name or type widening its
                    // own row alone: the peek is only so wide.
                    let width = n
                        .fields
                        .iter()
                        .map(|f| f.name.chars().count())
                        .max()
                        .unwrap_or(0)
                        .min(20);
                    let ty_width = n
                        .fields
                        .iter()
                        .map(|f| f.ty.chars().count())
                        .max()
                        .unwrap_or(0)
                        .min(14);
                    let rows: Vec<String> = n
                        .fields
                        .iter()
                        .enumerate()
                        .take(80)
                        .map(|(k, f)| {
                            let mut row = format!("{:width$}  {:ty_width$}", f.name, f.ty);
                            // What stands out: a column may be empty unless
                            // it says otherwise.
                            let nullable = tr!("nullable");
                            let m: Vec<&str> =
                                marks(f).into_iter().filter(|m| *m != nullable).collect();
                            if !m.is_empty() && n.kind != "enum" {
                                row.push_str("  ");
                                row.push_str(&m.join(" · "));
                            }
                            for (target, column) in references(map, hit.node, k) {
                                row.push_str(&format!("  → {target}.{column}"));
                            }
                            row.trim_end().to_string()
                        })
                        .collect();
                    let mut block = rows.join("\n");
                    if n.fields.len() > 80 {
                        block.push_str("\n…");
                    }
                    out.push(format!("```text\n{block}\n```"));
                }
                let detail: Vec<String> = n
                    .detail
                    .lines()
                    .take(12)
                    .map(|l| format!("- {}", crate::ui::map_text(l)))
                    .collect();
                if !detail.is_empty() {
                    out.push(detail.join("\n"));
                }
                Some(out.join("\n\n"))
            }
        }
    }
}

/// What the schema says of a column, in words.
fn marks(f: &MapField) -> Vec<&'static str> {
    let mut out = Vec::new();
    if f.key {
        out.push(tr!("primary key"));
    }
    if f.identity {
        out.push(tr!("identity"));
    }
    if f.unique {
        out.push(tr!("unique"));
    }
    if f.generated {
        out.push(tr!("generated"));
    }
    if !f.ty.is_empty() {
        out.push(match f.nullable {
            true => tr!("nullable"),
            false => tr!("not null"),
        });
    }
    if f.indexed && !f.key {
        out.push(tr!("indexed"));
    }
    out
}

/// What the column `field` of node `node` references: each table's label
/// and column.
fn references(map: &ProjectMap, node: usize, field: usize) -> Vec<(String, String)> {
    map.edges
        .iter()
        .filter(|e| e.from == node)
        .flat_map(|e| {
            e.fields
                .iter()
                .filter(move |&&(a, _)| a == field)
                .filter_map(move |&(_, b)| {
                    let target = map.nodes.get(e.to)?;
                    Some((target.label.clone(), target.fields.get(b)?.name.clone()))
                })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dotted_name_is_read_whole() {
        let line = r#"SELECT o.name FROM "storage"."objects" o JOIN storage.buckets b ON b.id = o.bucket_id"#;
        let at = |needle: &str| line.find(needle).unwrap();
        assert_eq!(
            dotted_at(line, at("objects")),
            Some((Some("storage".into()), "objects".into(), None))
        );
        assert_eq!(
            dotted_at(line, at("buckets")),
            Some((Some("storage".into()), "buckets".into(), None))
        );
        assert_eq!(
            dotted_at(line, at("bucket_id")),
            Some((Some("o".into()), "bucket_id".into(), None))
        );
        assert_eq!(
            dotted_at(line, at("storage.buckets")),
            Some((None, "storage".into(), Some("buckets".into())))
        );
        assert_eq!(dotted_at(line, at(" FROM")), None);
        assert!(reads_as_sql("db.query(\"SELECT * FROM users\")"));
        assert!(!reads_as_sql("let users = load();"));
    }
}
