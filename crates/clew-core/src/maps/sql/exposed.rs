//! Kotlin's Exposed tables read as the tables they define: each `object
//! Users : Table("users")` (or an `IntIdTable`, `LongIdTable`, `UUIDTable`…
//! with its `id` key; a name not written is the object's, `Table` cut off its
//! end, as Exposed names it), its `val`s the columns — `varchar("name", 50)`'s
//! name and type, `.nullable()`, `.default(…)`, `.autoIncrement()`,
//! `.uniqueIndex()` — a `reference("author_id", Users)` (`optReference`
//! nullable, or `.references(Users.id)`) a foreign key, and `override val
//! primaryKey = PrimaryKey(id)` its key. Exposed's columns are not null
//! unless they say so.

use super::ddl::{Column, Constraint, ForeignKey, Kind, QName};
use super::schema::{Schema, Table};
use crate::highlight::Lang;
use crate::maps::Source;

/// Whether a file may define Exposed tables.
pub(crate) fn is_tables(source: &Source) -> bool {
    source.lang == Some(Lang::Kotlin)
        && source.text.contains("org.jetbrains.exposed")
        && source.text.contains("Table")
}

const BASES: &[(&str, Option<&str>)] = &[
    ("Table", None),
    ("IntIdTable", Some("integer")),
    ("LongIdTable", Some("bigint")),
    ("UIntIdTable", Some("integer")),
    ("ULongIdTable", Some("bigint")),
    ("UUIDTable", Some("uuid")),
    ("IdTable", Some("integer")),
];

/// One table as written.
struct Written {
    object: String,
    table: String,
    /// Its `id` key's type, for an `IdTable`.
    id: Option<(String, String)>,
    rel: String,
    line: usize,
    /// Each statement of its body, with its first line.
    body: Vec<(usize, String)>,
}

/// The text from `open` (a `{`) to its matching `}`, braces in strings aside.
fn block(text: &str, open: usize) -> &str {
    let mut depth = 0i32;
    let mut quote = false;
    for (i, c) in text[open..].char_indices() {
        match c {
            '"' => quote = !quote,
            '{' if !quote => depth += 1,
            '}' if !quote => {
                depth -= 1;
                if depth == 0 {
                    return &text[open + 1..open + i];
                }
            }
            _ => {}
        }
    }
    &text[open + 1..]
}

/// The first string literal in `text`.
fn quoted(text: &str) -> Option<String> {
    let start = text.find('"')? + 1;
    let end = text[start..].find('"')? + start;
    Some(text[start..end].to_string())
}

fn tables_in(source: &Source) -> Vec<Written> {
    let text = source.text.as_str();
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(at) = text[from..].find("object ") {
        let at = from + at;
        from = at + "object ".len();
        let header_end = text[from..].find('{').map(|e| from + e);
        let Some(header_end) = header_end else { break };
        let header = &text[from..header_end];
        let Some((name, base)) = header.split_once(':') else {
            continue;
        };
        let object = name.trim().to_string();
        if object.is_empty() || !object.chars().all(|c| c.is_alphanumeric() || c == '_') {
            continue;
        }
        let base = base.trim();
        let base_name = base
            .split(['(', '<', ' '])
            .next()
            .unwrap_or("")
            .rsplit('.')
            .next()
            .unwrap_or("");
        let Some(&(_, id_type)) = BASES.iter().find(|(b, _)| *b == base_name) else {
            continue;
        };
        let args = base.split_once('(').map_or("", |(_, rest)| {
            rest.rsplit_once(')').map_or(rest, |(a, _)| a)
        });
        let mut strings = args.split(',').filter_map(quoted);
        let table = strings.next().unwrap_or_else(|| {
            object
                .strip_suffix("Table")
                .filter(|s| !s.is_empty())
                .unwrap_or(&object)
                .to_string()
        });
        let id = id_type.map(|ty| {
            (
                strings.next().unwrap_or_else(|| "id".into()),
                ty.to_string(),
            )
        });
        let body_text = block(text, header_end);
        let body_start = header_end + 1;
        let mut body: Vec<(usize, String)> = Vec::new();
        let mut offset = body_start;
        for raw in body_text.split_inclusive('\n') {
            let line = super::super::line_at(text, offset) + 1;
            offset += raw.len();
            let t = raw.trim();
            match body.last_mut() {
                // A chained call on the next line (`.nullable()`) is the
                // statement's.
                Some((_, last)) if t.starts_with('.') => last.push_str(t),
                _ if t.starts_with("val ") || t.starts_with("override val ") => {
                    body.push((line, t.to_string()))
                }
                _ => {}
            }
        }
        out.push(Written {
            object,
            table,
            id,
            rel: source.rel.clone(),
            line: super::super::line_at(text, at) + 1,
            body,
        });
    }
    out
}

/// A column function's SQL type: `varchar("name", 50)` is `varchar(50)`.
fn sql_type(function: &str, args: &str) -> String {
    let numbers: Vec<&str> = args
        .split(',')
        .skip(1)
        .map(str::trim)
        .filter(|a| a.chars().all(|c| c.is_ascii_digit()) && !a.is_empty())
        .collect();
    let base = match function {
        "integer" => "integer",
        "long" => "bigint",
        "short" => "smallint",
        "byte" => "tinyint",
        "varchar" => "varchar",
        "char" => "char",
        "text" | "mediumText" | "largeText" => "text",
        "bool" => "boolean",
        "decimal" => "decimal",
        "double" => "double",
        "float" => "float",
        "uuid" => "uuid",
        "datetime" => "datetime",
        "date" => "date",
        "timestamp" | "timestampWithTimeZone" => "timestamp",
        "binary" => "binary",
        "blob" => "blob",
        "json" | "jsonb" => function,
        "enumeration" | "enumerationByName" | "customEnumeration" => "enum",
        other => other,
    };
    match numbers.as_slice() {
        [] => base.to_string(),
        n => format!("{base}({})", n.join(",")),
    }
}

pub(crate) fn schema(sources: &[&Source]) -> Option<Schema> {
    let written: Vec<Written> = sources.iter().flat_map(|s| tables_in(s)).collect();
    let table_of = |object: &str| -> String {
        written
            .iter()
            .find(|w| w.object == object)
            .map_or_else(|| object.to_string(), |w| w.table.clone())
    };
    let mut schema = Schema::default();
    for w in &written {
        let mut table = Table {
            name: QName::bare(&w.table),
            file: w.rel.clone(),
            line: w.line,
            ..Default::default()
        };
        let constraint = |kind: Kind, line: usize| Constraint {
            name: None,
            kind,
            file: w.rel.clone(),
            line,
        };
        // Each `val`'s column name, for `PrimaryKey(id)`.
        let mut column_of: Vec<(String, String)> = Vec::new();
        if let Some((id, ty)) = &w.id {
            table.columns.push(Column {
                name: id.clone(),
                ty: ty.clone(),
                not_null: true,
                identity: ty != "uuid",
                file: w.rel.clone(),
                line: w.line,
                ..Default::default()
            });
            table
                .constraints
                .push(constraint(Kind::PrimaryKey(vec![id.clone()]), w.line));
            column_of.push(("id".into(), id.clone()));
        }
        let mut primary: Option<(Vec<String>, usize)> = None;
        for (line, statement) in &w.body {
            let Some((lhs, rhs)) = statement.split_once('=') else {
                continue;
            };
            let rhs = rhs.trim();
            if lhs.trim().ends_with("primaryKey") {
                let inner = rhs
                    .split_once('(')
                    .map_or("", |(_, r)| r.split(')').next().unwrap_or(""));
                let names = inner
                    .split(',')
                    .map(|n| n.trim().to_string())
                    .filter(|n| !n.is_empty() && !n.starts_with("name"))
                    .collect();
                primary = Some((names, *line));
                continue;
            }
            let val = lhs.trim().trim_start_matches("override").trim();
            let Some(val) = val
                .strip_prefix("val ")
                .map(|v| v.split(':').next().unwrap_or(v).trim())
            else {
                continue;
            };
            let Some((function, rest)) = rhs.split_once('(') else {
                continue;
            };
            let function = function.trim();
            let args = rest.split(')').next().unwrap_or("");
            let Some(name) = quoted(args) else { continue };
            let referenced = matches!(function, "reference" | "optReference");
            let ty = if referenced {
                "reference".to_string()
            } else {
                sql_type(function, args)
            };
            let default = rhs.find(".default(").map(|at| {
                let inner = &rhs[at + ".default(".len()..];
                inner.split(')').next().unwrap_or("").trim().to_string()
            });
            table.columns.push(Column {
                name: name.clone(),
                ty,
                not_null: !(rhs.contains(".nullable()") || function == "optReference"),
                default,
                identity: rhs.contains(".autoIncrement("),
                file: w.rel.clone(),
                line: *line,
                ..Default::default()
            });
            column_of.push((val.to_string(), name.clone()));
            if rhs.contains(".uniqueIndex(") {
                table
                    .constraints
                    .push(constraint(Kind::Unique(vec![name.clone()]), *line));
            }
            // `reference("author_id", Users)`, `.references(Users.id)`.
            let target = if referenced {
                args.split(',').nth(1).map(|t| t.trim().to_string())
            } else {
                rhs.find(".references(").map(|at| {
                    let inner = &rhs[at + ".references(".len()..];
                    let inner = inner.split([')', ',']).next().unwrap_or("").trim();
                    inner.split('.').next().unwrap_or(inner).to_string()
                })
            };
            if let Some(target) = target.filter(|t| !t.is_empty()) {
                let target = target.split('.').next().unwrap_or(&target).to_string();
                table.constraints.push(constraint(
                    Kind::ForeignKey(ForeignKey {
                        columns: vec![name],
                        table: QName::bare(&table_of(&target)),
                        references: Vec::new(),
                        on_delete: None,
                        on_update: None,
                        search: None,
                    }),
                    *line,
                ));
            }
        }
        if let Some((vals, line)) = primary {
            let columns: Vec<String> = vals
                .iter()
                .filter_map(|v| {
                    column_of
                        .iter()
                        .find(|(val, _)| val == v)
                        .map(|(_, c)| c.clone())
                })
                .collect();
            if !columns.is_empty() {
                table
                    .constraints
                    .retain(|c| !matches!(c.kind, Kind::PrimaryKey(_)));
                table
                    .constraints
                    .push(constraint(Kind::PrimaryKey(columns), line));
            }
        }
        schema.tables.insert(table.name.key(), table);
    }
    (!schema.tables.is_empty()).then_some(schema)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::maps::Corpus;

    #[test]
    fn exposed_tables_are_the_tables_they_define() {
        let corpus = Corpus::of(&[(
            "src/main/kotlin/shop/Tables.kt",
            "package shop\nimport org.jetbrains.exposed.sql.Table\nimport org.jetbrains.exposed.dao.id.IntIdTable\n\nobject Users : IntIdTable(\"users\") {\n    val name = varchar(\"name\", 50)\n    val email = varchar(\"email\", 255).uniqueIndex()\n    val age = integer(\"age\")\n        .nullable()\n}\n\nobject PostTable : Table() {\n    val id = integer(\"id\").autoIncrement()\n    val title = text(\"title\").default(\"untitled\")\n    val author = reference(\"author_id\", Users)\n    val editor = optReference(\"editor_id\", Users)\n    override val primaryKey = PrimaryKey(id)\n}\n",
        )]);
        let sources: Vec<&Source> = corpus.files.iter().filter(|s| is_tables(s)).collect();
        let schema = schema(&sources).unwrap();
        let users = &schema.tables["users"];
        let cols: Vec<(&str, &str, bool)> = users
            .columns
            .iter()
            .map(|c| (c.name.as_str(), c.ty.as_str(), c.not_null))
            .collect();
        assert_eq!(
            cols,
            [
                ("id", "integer", true),
                ("name", "varchar(50)", true),
                ("email", "varchar(255)", true),
                ("age", "integer", false)
            ]
        );
        assert_eq!(users.primary_key(), ["id"]);
        let posts = &schema.tables["post"];
        assert_eq!(posts.primary_key(), ["id"]);
        let title = posts.columns.iter().find(|c| c.name == "title").unwrap();
        assert_eq!(title.default.as_deref(), Some("\"untitled\""));
        let fks: Vec<(String, String)> = posts
            .constraints
            .iter()
            .filter_map(|c| match &c.kind {
                Kind::ForeignKey(fk) => Some((fk.columns[0].clone(), fk.table.label())),
                _ => None,
            })
            .collect();
        assert_eq!(
            fks,
            [
                ("author_id".into(), "users".into()),
                ("editor_id".into(), "users".into())
            ]
        );
        let editor = posts
            .columns
            .iter()
            .find(|c| c.name == "editor_id")
            .unwrap();
        assert!(!editor.not_null);
    }
}
