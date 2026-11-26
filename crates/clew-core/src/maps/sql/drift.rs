//! Dart's Drift (formerly moor) tables read as the tables they define: each
//! `class TodoItems extends Table` a table — `todo_items`, or what its
//! `tableName` getter says — its column getters the columns (`IntColumn get
//! createdAt => dateTime()()`: the getter's name in snake case, or
//! `.named('…')`), `.nullable()`, `.withDefault(…)`, `.unique()`, a
//! `.references(Category, #id)` a foreign key; its key what `primaryKey`
//! lists, else an `autoIncrement()` column. Drift's columns are not null
//! unless they say so.

use super::ddl::{Column, Constraint, ForeignKey, Kind, QName};
use super::schema::{Schema, Table};
use crate::highlight::Lang;
use crate::maps::Source;

/// Whether a file may define Drift tables.
pub(crate) fn is_tables(source: &Source) -> bool {
    source.lang == Some(Lang::Dart)
        && (source.text.contains("package:drift/") || source.text.contains("package:moor/"))
        && source.text.contains("extends Table")
}

/// A column getter's type: `IntColumn` is `integer`.
fn sql_type(column: &str) -> &'static str {
    match column {
        "IntColumn" => "integer",
        "Int64Column" => "bigint",
        "TextColumn" => "text",
        "BoolColumn" => "boolean",
        "DateTimeColumn" => "datetime",
        "RealColumn" => "real",
        "BlobColumn" => "blob",
        _ => "custom",
    }
}

/// The text inside the `{…}` that opens at `open`.
fn block(text: &str, open: usize) -> &str {
    let mut depth = 0i32;
    for (i, c) in text[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
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

/// The first quoted string in `text` (either quote).
fn quoted(text: &str) -> Option<String> {
    let start = text.find(['\'', '"'])?;
    let quote = text[start..].chars().next()?;
    let rest = &text[start + 1..];
    let end = rest.find(quote)?;
    Some(rest[..end].to_string())
}

/// What a call in a getter's chain says: `.named('body')` is `body`.
fn call_arg<'t>(text: &'t str, call: &str) -> Option<&'t str> {
    let at = text.find(call)? + call.len();
    let inner = &text[at..];
    // To the call's own closing parenthesis.
    let mut depth = 1i32;
    for (i, c) in inner.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&inner[..i]);
                }
            }
            _ => {}
        }
    }
    None
}

struct Written {
    class: String,
    table: String,
    rel: String,
    line: usize,
    /// Each column: its getter, the column type, and its chain, at a line.
    columns: Vec<(String, String, String, usize)>,
    primary: Option<Vec<String>>,
}

fn tables_in(source: &Source) -> Vec<Written> {
    let text = source.text.as_str();
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(at) = text[from..].find("class ") {
        let at = from + at;
        from = at + "class ".len();
        let Some(open) = text[from..].find('{').map(|o| from + o) else {
            break;
        };
        let header = &text[from..open];
        let mut words = header.split_whitespace();
        let Some(class) = words.next().map(str::to_string) else {
            continue;
        };
        if !(words.next() == Some("extends") && words.next() == Some("Table")) {
            continue;
        }
        let body = block(text, open);
        let body_start = open + 1;
        let mut table = super::snake_case(&class);
        let mut columns = Vec::new();
        let mut primary = None;
        // Each `;`-ended statement, with the line it starts at.
        let mut offset = body_start;
        for statement in body.split_inclusive(';') {
            let start = offset + (statement.len() - statement.trim_start().len());
            offset += statement.len();
            let s = statement.trim();
            let line = super::super::line_at(text, start) + 1;
            let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
            if flat.contains("get tableName") {
                if let Some(name) = quoted(&flat) {
                    table = name;
                }
                continue;
            }
            if flat.contains("get primaryKey") {
                let inner = flat
                    .split_once('{')
                    .map_or("", |(_, r)| r.split('}').next().unwrap_or(""));
                primary = Some(inner.split(',').map(|n| n.trim().to_string()).collect());
                continue;
            }
            // `IntColumn get id => integer().autoIncrement()();`
            let words: Vec<&str> = flat.split_whitespace().collect();
            let Some(get) = words.iter().position(|w| *w == "get") else {
                continue;
            };
            let (Some(ty), Some(name)) = (words.get(get.wrapping_sub(1)), words.get(get + 1))
            else {
                continue;
            };
            if !ty.ends_with("Column") {
                continue;
            }
            let chain = flat.split_once("=>").map_or("", |(_, r)| r.trim());
            columns.push((name.to_string(), ty.to_string(), chain.to_string(), line));
        }
        out.push(Written {
            class,
            table,
            rel: source.rel.clone(),
            line: super::super::line_at(text, at) + 1,
            columns,
            primary,
        });
    }
    out
}

pub(crate) fn schema(sources: &[&Source]) -> Option<Schema> {
    let written: Vec<Written> = sources.iter().flat_map(|s| tables_in(s)).collect();
    let table_of = |class: &str| -> String {
        written
            .iter()
            .find(|w| w.class == class)
            .map_or_else(|| super::snake_case(class), |w| w.table.clone())
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
        let mut names: Vec<(String, String)> = Vec::new();
        let mut auto: Option<(String, usize)> = None;
        for (getter, ty, chain, line) in &w.columns {
            let name = call_arg(chain, ".named(")
                .and_then(quoted)
                .unwrap_or_else(|| super::snake_case(getter));
            let identity = chain.contains(".autoIncrement(");
            table.columns.push(Column {
                name: name.clone(),
                ty: sql_type(ty).to_string(),
                not_null: !chain.contains(".nullable("),
                default: call_arg(chain, ".withDefault(").map(|d| d.trim().to_string()),
                identity,
                file: w.rel.clone(),
                line: *line,
                ..Default::default()
            });
            names.push((getter.clone(), name.clone()));
            if identity {
                auto = Some((name.clone(), *line));
            }
            if chain.contains(".unique(") {
                table
                    .constraints
                    .push(constraint(Kind::Unique(vec![name.clone()]), *line));
            }
            // `.references(Category, #id)`.
            if let Some(args) = call_arg(chain, ".references(") {
                let mut parts = args.split(',').map(str::trim);
                let target = parts.next().unwrap_or("").to_string();
                let column = parts
                    .next()
                    .map(|c| super::snake_case(c.trim_start_matches('#')));
                if !target.is_empty() {
                    table.constraints.push(constraint(
                        Kind::ForeignKey(ForeignKey {
                            columns: vec![name],
                            table: QName::bare(&table_of(&target)),
                            references: column.into_iter().collect(),
                            on_delete: None,
                            on_update: None,
                            search: None,
                        }),
                        *line,
                    ));
                }
            }
        }
        let key = match &w.primary {
            Some(getters) => Some((
                getters
                    .iter()
                    .filter_map(|g| names.iter().find(|(n, _)| n == g).map(|(_, c)| c.clone()))
                    .collect::<Vec<_>>(),
                w.line,
            )),
            None => auto.map(|(name, line)| (vec![name], line)),
        };
        if let Some((columns, line)) = key.filter(|(c, _)| !c.is_empty()) {
            table
                .constraints
                .push(constraint(Kind::PrimaryKey(columns), line));
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
    fn drift_tables_are_the_tables_they_define() {
        let corpus = Corpus::of(&[(
            "lib/db.dart",
            "import 'package:drift/drift.dart';\n\nclass TodoCategories extends Table {\n  IntColumn get id => integer().autoIncrement()();\n  TextColumn get name => text().unique()();\n}\n\nclass TodoItems extends Table {\n  IntColumn get id => integer().autoIncrement()();\n  TextColumn get title => text().withLength(min: 6, max: 32)();\n  TextColumn get content => text().named('body')();\n  IntColumn get category =>\n      integer().nullable().references(TodoCategories, #id)();\n  DateTimeColumn get createdAt => dateTime().withDefault(currentDateAndTime)();\n}\n\nclass Tags extends Table {\n  TextColumn get slug => text()();\n  @override\n  String get tableName => 'tag_list';\n  @override\n  Set<Column> get primaryKey => {slug};\n}\n",
        )]);
        let sources: Vec<&Source> = corpus.files.iter().filter(|s| is_tables(s)).collect();
        let schema = schema(&sources).unwrap();
        let items = &schema.tables["todo_items"];
        let cols: Vec<(&str, &str, bool)> = items
            .columns
            .iter()
            .map(|c| (c.name.as_str(), c.ty.as_str(), c.not_null))
            .collect();
        assert_eq!(
            cols,
            [
                ("id", "integer", true),
                ("title", "text", true),
                ("body", "text", true),
                ("category", "integer", false),
                ("created_at", "datetime", true)
            ]
        );
        assert_eq!(items.primary_key(), ["id"]);
        let fk = items
            .constraints
            .iter()
            .find_map(|c| match &c.kind {
                Kind::ForeignKey(fk) => Some(fk.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            (fk.columns[0].as_str(), fk.table.label().as_str()),
            ("category", "todo_categories")
        );
        assert_eq!(fk.references, ["id"]);
        let tags = &schema.tables["tag_list"];
        assert_eq!(tags.primary_key(), ["slug"]);
        let created = items
            .columns
            .iter()
            .find(|c| c.name == "created_at")
            .unwrap();
        assert_eq!(created.default.as_deref(), Some("currentDateAndTime"));
    }
}
