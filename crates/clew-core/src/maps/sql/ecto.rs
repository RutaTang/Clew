//! Ecto's schemas read as the tables they map: each `schema "users" do`
//! block a table, its `field`s the columns (`default:` kept), an `id` key
//! unless `@primary_key` says otherwise, each `belongs_to` a column
//! `<name>_id` (or its `foreign_key:`) and a foreign key to the schema it
//! names, `timestamps()` its `inserted_at` and `updated_at`, an
//! `embeds_one` or `embeds_many` a column of its own (a map). `has_many`,
//! `has_one` and `many_to_many` are the other table's, or a join table's:
//! no column here.

use std::collections::BTreeMap;

use super::ddl::{Column, Constraint, ForeignKey, Kind, QName};
use super::schema::{Schema, Table};
use crate::highlight::Lang;
use crate::maps::Source;

/// Whether a file may hold an Ecto schema.
pub(crate) fn is_schema(source: &Source) -> bool {
    source.lang == Some(Lang::Elixir)
        && source.text.contains("Ecto.Schema")
        && source.text.contains("schema \"")
}

/// One schema as written: its module, its table, and its lines.
struct Written<'s> {
    module: String,
    table: String,
    rel: String,
    line: usize,
    body: Vec<(usize, &'s str)>,
    /// `@primary_key false`, or `{:uuid, :binary_id, …}`'s name and type.
    primary: Option<Option<(String, String)>>,
    doc: Option<String>,
}

/// The schema the Ecto schemas of `sources` map; none when they map no
/// table.
pub(crate) fn schema(sources: &[&Source]) -> Option<Schema> {
    let mut written: Vec<Written<'_>> = Vec::new();
    for source in sources {
        written.extend(schemas_in(source));
    }
    // A module's table, for the `belongs_to` that names it.
    let tables: BTreeMap<String, String> = written
        .iter()
        .map(|w| (w.module.clone(), w.table.clone()))
        .collect();
    let table_of = |module: &str| -> String {
        tables
            .get(module)
            .or_else(|| {
                // `Org` for `MyApp.Org`, aliased in the file.
                tables
                    .iter()
                    .find(|(m, _)| m.rsplit('.').next() == Some(module))
                    .map(|(_, t)| t)
            })
            .cloned()
            .unwrap_or_else(|| {
                let last = module.rsplit('.').next().unwrap_or(module);
                super::pluralize(&snake(last))
            })
    };
    let mut schema = Schema::default();
    for w in &written {
        let table = table_of_schema(w, &table_of);
        schema.tables.insert(table.name.key(), table);
    }
    (!schema.tables.is_empty()).then_some(schema)
}

/// The schemas one file defines.
fn schemas_in(source: &Source) -> Vec<Written<'_>> {
    let lines: Vec<&str> = source.text.lines().collect();
    let mut out = Vec::new();
    let mut module = String::new();
    let mut primary: Option<Option<(String, String)>> = None;
    let mut doc: Option<String> = None;
    let mut i = 0;
    while i < lines.len() {
        let t = lines[i].trim();
        if let Some(rest) = t.strip_prefix("defmodule ") {
            module = rest.trim_end_matches(" do").trim().to_string();
            primary = None;
            doc = None;
        } else if let Some(rest) = t.strip_prefix("@primary_key ") {
            primary = Some(primary_key(rest));
        } else if let Some(rest) = t.strip_prefix("@moduledoc ") {
            doc = rest
                .trim()
                .trim_matches('"')
                .lines()
                .next()
                .filter(|l| !l.is_empty() && *l != "false")
                .map(str::to_string)
                .or_else(|| {
                    // A heredoc: its first line.
                    lines
                        .get(i + 1)
                        .map(|l| l.trim().to_string())
                        .filter(|l| !l.is_empty() && !l.starts_with("\"\"\""))
                });
        } else if let Some(rest) = t.strip_prefix("schema ").filter(|_| t.ends_with(" do")) {
            let Some(table) = quoted(rest) else {
                i += 1;
                continue;
            };
            let own = indent(lines[i]);
            let start = i;
            let mut body = Vec::new();
            i += 1;
            while i < lines.len() {
                let line = lines[i];
                if !line.trim().is_empty() && indent(line) <= own {
                    break;
                }
                body.push((i + 1, line.trim()));
                i += 1;
            }
            out.push(Written {
                module: module.clone(),
                table,
                rel: source.rel.clone(),
                line: start + 1,
                body,
                primary: primary.clone(),
                doc: doc.clone(),
            });
        }
        i += 1;
    }
    out
}

fn table_of_schema(w: &Written<'_>, table_of: &dyn Fn(&str) -> String) -> Table {
    let mut table = Table {
        name: QName::bare(&w.table),
        comment: w.doc.clone(),
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
    let column =
        |name: &str, ty: &str, line: usize, not_null: bool, default: Option<String>| Column {
            name: name.to_string(),
            ty: ty.to_string(),
            not_null,
            default,
            file: w.rel.clone(),
            line,
            ..Default::default()
        };
    // The key, as `@primary_key` names it — `id` by default.
    match &w.primary {
        Some(None) => {}
        Some(Some((name, ty))) => {
            table.columns.push(column(name, ty, w.line, true, None));
            table
                .constraints
                .push(constraint(Kind::PrimaryKey(vec![name.clone()]), w.line));
        }
        None => {
            table.columns.push(column("id", "id", w.line, true, None));
            table
                .constraints
                .push(constraint(Kind::PrimaryKey(vec!["id".into()]), w.line));
        }
    }
    for &(line, t) in &w.body {
        let (word, rest) = t.split_once(char::is_whitespace).unwrap_or((t, ""));
        let args: Vec<&str> = split_args(rest);
        let atom = |k: usize| {
            args.get(k)
                .and_then(|a| a.strip_prefix(':'))
                .map(|a| a.trim().to_string())
        };
        match word {
            "field" => {
                let Some(name) = atom(0) else { continue };
                let ty = args.get(1).map_or("string", |a| a.trim_start_matches(':'));
                let default = option(rest, "default:");
                table.columns.push(column(&name, ty, line, false, default));
            }
            "belongs_to" => {
                let (Some(name), Some(target)) = (atom(0), args.get(1)) else {
                    continue;
                };
                let key = option(rest, "foreign_key:")
                    .map(|k| k.trim_start_matches(':').to_string())
                    .unwrap_or_else(|| format!("{name}_id"));
                let ty = option(rest, "type:")
                    .map_or("id".into(), |t| t.trim_start_matches(':').to_string());
                table.columns.push(column(&key, &ty, line, false, None));
                table.constraints.push(constraint(
                    Kind::ForeignKey(ForeignKey {
                        columns: vec![key],
                        table: QName::bare(&table_of(target.trim())),
                        references: Vec::new(),
                        on_delete: None,
                        on_update: None,
                        search: None,
                    }),
                    line,
                ));
            }
            "embeds_one" | "embeds_many" => {
                if let Some(name) = atom(0) {
                    table.columns.push(column(&name, "map", line, false, None));
                }
            }
            _ if t.starts_with("timestamps") => {
                // `timestamps(type: :utc_datetime)`: its options in its
                // parentheses.
                let inner = t
                    .split_once('(')
                    .map_or("", |(_, r)| r.trim_end_matches(')'));
                let ty = option(inner, "type:").map_or("naive_datetime".into(), |t| {
                    t.trim_start_matches(':').to_string()
                });
                for name in ["inserted_at", "updated_at"] {
                    table.columns.push(column(name, &ty, line, true, None));
                }
            }
            _ => {}
        }
    }
    table
}

/// `@primary_key false`, or `{:uuid, :binary_id, autogenerate: true}`:
/// none, or the key's name and type.
fn primary_key(rest: &str) -> Option<(String, String)> {
    let rest = rest.trim();
    if rest.starts_with("false") {
        return None;
    }
    let inner = rest.trim_start_matches('{').trim_end_matches('}');
    let parts: Vec<&str> = inner.split(',').map(str::trim).collect();
    match parts.as_slice() {
        [name, ty, ..] => Some((
            name.trim_start_matches(':').to_string(),
            ty.trim_start_matches(':').to_string(),
        )),
        _ => Some(("id".into(), "id".into())),
    }
}

/// A call's arguments, split at its top-level commas.
fn split_args(rest: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut depth, mut start) = (0i32, 0);
    let mut in_string = false;
    for (i, c) in rest.char_indices() {
        match c {
            '"' => in_string = !in_string,
            '(' | '[' | '{' if !in_string => depth += 1,
            ')' | ']' | '}' if !in_string => depth -= 1,
            ',' if depth == 0 && !in_string => {
                out.push(rest[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    let last = rest[start..].trim();
    if !last.is_empty() {
        out.push(last);
    }
    out
}

/// A keyword option's value, as written (`default: 0`).
fn option(rest: &str, key: &str) -> Option<String> {
    split_args(rest)
        .into_iter()
        .find_map(|a| a.strip_prefix(key).map(|v| v.trim().to_string()))
}

fn quoted(text: &str) -> Option<String> {
    let start = text.find('"')? + 1;
    let len = text[start..].find('"')?;
    Some(text[start..start + len].to_string())
}

fn indent(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// `OrderItem` as `order_item`.
fn snake(name: &str) -> String {
    let mut out = String::new();
    for (i, c) in name.chars().enumerate() {
        if c.is_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.extend(c.to_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::maps::Corpus;

    /// Ecto schemas as tables: fields, the `id` key, `belongs_to`'s column
    /// and foreign key (to the other schema's table, an alias too),
    /// `timestamps()`, `@primary_key`'s own key, an embed's column.
    #[test]
    fn ecto_schemas_are_tables() {
        let corpus = Corpus::of(&[
            (
                "lib/shop/accounts/user.ex",
                "defmodule Shop.Accounts.User do\n  @moduledoc \"A customer.\"\n  use Ecto.Schema\n\n  schema \"users\" do\n    field :email, :string\n    field :age, :integer, default: 0\n    belongs_to :org, Shop.Orgs.Org\n    has_many :orders, Shop.Order\n    embeds_one :settings, Settings\n    timestamps(type: :utc_datetime)\n  end\nend\n",
            ),
            (
                "lib/shop/orgs/org.ex",
                "defmodule Shop.Orgs.Org do\n  use Ecto.Schema\n  @primary_key {:uuid, :binary_id, autogenerate: true}\n  schema \"organisations\" do\n    field :name, :string\n  end\nend\n",
            ),
        ]);
        let sources: Vec<&Source> = corpus.files.iter().filter(|s| is_schema(s)).collect();
        let schema = schema(&sources).unwrap();
        let users = schema
            .tables
            .values()
            .find(|t| t.name.name == "users")
            .unwrap();
        let cols: Vec<(&str, &str)> = users
            .columns
            .iter()
            .map(|c| (c.name.as_str(), c.ty.as_str()))
            .collect();
        assert_eq!(
            cols,
            [
                ("id", "id"),
                ("email", "string"),
                ("age", "integer"),
                ("org_id", "id"),
                ("settings", "map"),
                ("inserted_at", "utc_datetime"),
                ("updated_at", "utc_datetime"),
            ]
        );
        assert_eq!(users.columns[2].default.as_deref(), Some("0"));
        assert_eq!(users.comment.as_deref(), Some("A customer."));
        assert!(users.constraints.iter().any(|c| matches!(
            &c.kind,
            Kind::ForeignKey(fk) if fk.table.name == "organisations" && fk.columns == ["org_id"]
        )));
        let orgs = schema
            .tables
            .values()
            .find(|t| t.name.name == "organisations")
            .unwrap();
        assert_eq!(orgs.columns[0].name, "uuid");
        assert_eq!(orgs.columns[0].ty, "binary_id");
    }
}
