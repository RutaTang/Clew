//! Rails' `db/schema.rb` and `db/migrate/*.rb`, read as the changes they
//! make: `create_table` and its `t.` columns, `add_column`,
//! `add_reference`, `add_foreign_key`, `add_index`, and the renames and
//! drops.

use super::ddl::{Action, Column, Constraint, ForeignKey, Index, Kind, Op, QName};
use crate::maps::Source;

pub(crate) fn is_rails_schema(source: &Source) -> bool {
    source.rel.ends_with("db/schema.rb") || source.rel.contains("db/migrate/")
}

/// The plural Rails gives a model's name for its table (`person` →
/// `people` aside: the regular rules).
pub(crate) fn pluralize(word: &str) -> String {
    let w = word.to_ascii_lowercase();
    if w.ends_with('s')
        || w.ends_with('x')
        || w.ends_with('z')
        || w.ends_with("ch")
        || w.ends_with("sh")
    {
        return format!("{w}es");
    }
    if let Some(stem) = w.strip_suffix('y')
        && !stem.ends_with(['a', 'e', 'i', 'o', 'u'])
    {
        return format!("{stem}ies");
    }
    format!("{w}s")
}

/// The symbol or string arguments of a Ruby call's argument text, in order
/// (`:users, "email", :string` → `users`, `email`, `string`), up to the
/// first `key: value` option.
fn ruby_args(args: &str) -> Vec<String> {
    let args = args.split(" do").next().unwrap_or(args);
    let mut out = Vec::new();
    for part in args.split(',') {
        let p = part.trim().trim_start_matches('(');
        let value = if let Some(sym) = p.strip_prefix(':') {
            crate::maps::ident(sym).to_string()
        } else if let Some(q) = p.strip_prefix(['"', '\'']) {
            q.split(['"', '\'']).next().unwrap_or("").to_string()
        } else {
            // A `key: value` option, or what is no literal: the end.
            break;
        };
        if value.is_empty() {
            break;
        }
        out.push(value);
    }
    out
}

/// A Ruby option's symbol or string value (`column: :buyer_id` → `buyer_id`).
fn ruby_option(args: &str, option: &str) -> Option<String> {
    let at = find_option(args, option)?;
    let rest = args[at + option.len() + 1..].trim_start();
    let value = rest.trim_start_matches(':').trim_start_matches(['"', '\'']);
    let v = crate::maps::ident(value);
    (!v.is_empty()).then(|| v.to_string())
}

/// Where `option:` is in `args`, as a whole word.
fn find_option(args: &str, option: &str) -> Option<usize> {
    let needle = format!("{option}:");
    let mut from = 0;
    while let Some(at) = args[from..].find(&needle) {
        let at = from + at;
        let before = args[..at].chars().next_back();
        if before.is_none_or(|c| !(c.is_alphanumeric() || c == '_')) {
            return Some(at);
        }
        from = at + needle.len();
    }
    None
}

/// An option's literal value as written (`default: ""` → `""`,
/// `default: -> { "now()" }` → `now()`).
fn ruby_literal(args: &str, option: &str) -> Option<String> {
    let at = find_option(args, option)?;
    let rest = args[at + option.len() + 1..].trim_start();
    if let Some(lambda) = rest.strip_prefix("->") {
        let inner = lambda.trim_start().strip_prefix('{')?.split('}').next()?;
        return Some(inner.trim().trim_matches(['"', '\'']).to_string());
    }
    let mut depth = 0;
    let end = rest
        .char_indices()
        .find(|&(_, c)| {
            match c {
                '[' | '{' | '(' => depth += 1,
                ']' | '}' | ')' => depth -= 1,
                _ => {}
            }
            (c == ',' && depth == 0) || depth < 0
        })
        .map_or(rest.len(), |(i, _)| i);
    let value = rest[..end].trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn not_null(args: &str) -> bool {
    ruby_literal(args, "null").as_deref() == Some("false")
}

fn column(name: &str, ty: &str, args: &str, line: usize) -> Column {
    Column {
        name: name.to_string(),
        ty: ty.to_string(),
        not_null: not_null(args),
        default: ruby_literal(args, "default"),
        identity: false,
        generated: None,
        comment: ruby_literal(args, "comment").map(|c| c.trim_matches(['"', '\'']).to_string()),
        file: String::new(),
        line,
    }
}

fn wants_foreign_key(args: &str) -> bool {
    ruby_literal(args, "foreign_key").is_some_and(|v| v != "false")
}

/// A reference's foreign key: to the table its `foreign_key: { to_table: }`
/// names, or the plural of its name.
fn reference_key(name: &str, args: &str, line: usize) -> Constraint {
    let table = ruby_option(args, "to_table").unwrap_or_else(|| pluralize(name));
    constraint(
        Kind::ForeignKey(ForeignKey {
            columns: vec![format!("{name}_id")],
            table: QName::bare(&table),
            references: vec!["id".into()],
            on_delete: ruby_option(args, "on_delete").map(|a| a.replace('_', " ")),
            on_update: None,
            search: None,
        }),
        line,
    )
}

fn constraint(kind: Kind, line: usize) -> Constraint {
    Constraint {
        name: None,
        kind,
        file: String::new(),
        line,
    }
}

/// The columns an index names: `[:a, :b]`, `["a"]`, `:a`.
fn index_columns(args: &str) -> Vec<String> {
    let args = args.trim_start();
    match args.strip_prefix('[') {
        Some(list) => list
            .split(']')
            .next()
            .unwrap_or("")
            .split(',')
            .map(|p| p.trim().trim_matches([':', '"', '\'']).to_string())
            .filter(|c| !c.is_empty())
            .collect(),
        None => ruby_args(args).into_iter().take(1).collect(),
    }
}

fn index(args: &str, line: usize) -> Index {
    Index {
        name: ruby_option(args, "name"),
        columns: index_columns(args),
        unique: ruby_literal(args, "unique").as_deref() == Some("true"),
        partial: find_option(args, "where").is_some(),
        method: ruby_option(args, "using").filter(|m| m != "btree"),
        concurrent: ruby_option(args, "algorithm").as_deref() == Some("concurrently"),
        file: String::new(),
        line,
    }
}

/// The table `create_table` is writing.
struct Open {
    name: String,
    columns: Vec<Column>,
    constraints: Vec<Constraint>,
    indexes: Vec<Index>,
    line: usize,
}

pub(crate) fn ops(text: &str) -> Vec<(Op, usize)> {
    let mut out = Vec::new();
    let mut table: Option<Open> = None;
    for (n, raw) in text.lines().enumerate() {
        let line = raw.trim();
        let at = n + 1;
        let call = |name: &str| -> Option<&str> {
            let rest = line.strip_prefix(name)?;
            let rest = rest.strip_prefix('(').unwrap_or(rest);
            rest.starts_with([' ', ':', '"', '\''])
                .then(|| rest.trim_end_matches(')'))
        };
        if let Some(open) = &mut table {
            if line == "end" {
                let Some(open) = table.take() else {
                    continue;
                };
                let name = QName::bare(&open.name);
                out.push((
                    Op::CreateTable {
                        name: name.clone(),
                        columns: open.columns,
                        constraints: open.constraints,
                        copies: Vec::new(),
                        comment: None,
                        if_not_exists: false,
                    },
                    open.line,
                ));
                for index in open.indexes {
                    let line = index.line;
                    out.push((
                        Op::CreateIndex {
                            table: name.clone(),
                            index,
                        },
                        line,
                    ));
                }
                continue;
            }
            let Some(rest) = line.strip_prefix("t.") else {
                continue;
            };
            let kind = crate::maps::ident(rest);
            let args = rest[kind.len()..].trim();
            match kind {
                "timestamps" => {
                    for c in ["created_at", "updated_at"] {
                        let mut col = column(c, "datetime", args, at);
                        col.not_null = !args.contains("null: true");
                        open.columns.push(col);
                    }
                }
                "references" | "belongs_to" => {
                    for name in ruby_args(args) {
                        let ty = ruby_option(args, "type").unwrap_or_else(|| "bigint".into());
                        open.columns
                            .push(column(&format!("{name}_id"), &ty, args, at));
                        if wants_foreign_key(args) {
                            open.constraints.push(reference_key(&name, args, at));
                        }
                    }
                }
                "index" => open.indexes.push(index(args, at)),
                "check_constraint" => {}
                _ => {
                    for name in ruby_args(args) {
                        open.columns.push(column(&name, kind, args, at));
                    }
                }
            }
            continue;
        }
        if let Some(args) = call("create_table") {
            if let Some(name) = ruby_args(args).into_iter().next() {
                let mut columns = Vec::new();
                let mut constraints = Vec::new();
                if ruby_literal(args, "id").as_deref() != Some("false") {
                    let pk = ruby_option(args, "primary_key").unwrap_or_else(|| "id".into());
                    let ty = ruby_option(args, "id").unwrap_or_else(|| "bigint".into());
                    columns.push(Column {
                        name: pk.clone(),
                        ty,
                        not_null: true,
                        identity: true,
                        line: at,
                        ..Default::default()
                    });
                    constraints.push(constraint(Kind::PrimaryKey(vec![pk]), at));
                }
                let open = Open {
                    name,
                    columns,
                    constraints,
                    indexes: Vec::new(),
                    line: at,
                };
                if line.contains(" do |t|") {
                    table = Some(open);
                } else {
                    out.push((
                        Op::CreateTable {
                            name: QName::bare(&open.name),
                            columns: open.columns,
                            constraints: open.constraints,
                            copies: Vec::new(),
                            comment: None,
                            if_not_exists: false,
                        },
                        at,
                    ));
                }
            }
        } else if let Some(args) = call("add_column") {
            if let [t, c, ty, ..] = ruby_args(args).as_slice() {
                out.push((
                    alter(
                        t,
                        Action::AddColumn {
                            column: column(c, ty, args, at),
                            constraints: Vec::new(),
                            if_not_exists: false,
                        },
                    ),
                    at,
                ));
            }
        } else if let Some(args) = call("remove_column") {
            if let [t, c, ..] = ruby_args(args).as_slice() {
                out.push((alter(t, Action::DropColumn(c.clone())), at));
            }
        } else if let Some(args) = call("rename_column") {
            if let [t, from, to, ..] = ruby_args(args).as_slice() {
                out.push((
                    alter(
                        t,
                        Action::RenameColumn {
                            from: from.clone(),
                            to: to.clone(),
                        },
                    ),
                    at,
                ));
            }
        } else if let Some(args) = call("add_reference").or_else(|| call("add_belongs_to")) {
            if let [t, r, ..] = ruby_args(args).as_slice() {
                let ty = ruby_option(args, "type").unwrap_or_else(|| "bigint".into());
                let constraints = match wants_foreign_key(args) {
                    true => vec![reference_key(r, args, at)],
                    false => Vec::new(),
                };
                out.push((
                    alter(
                        t,
                        Action::AddColumn {
                            column: column(&format!("{r}_id"), &ty, args, at),
                            constraints,
                            if_not_exists: false,
                        },
                    ),
                    at,
                ));
            }
        } else if let Some(args) = call("add_foreign_key") {
            if let [from, to, ..] = ruby_args(args).as_slice() {
                let singular = to.strip_suffix('s').unwrap_or(to);
                let col = ruby_option(args, "column").unwrap_or_else(|| format!("{singular}_id"));
                let mut key = reference_key(singular, args, at);
                if let Kind::ForeignKey(fk) = &mut key.kind {
                    fk.columns = vec![col];
                    fk.table = QName::bare(to);
                    fk.references =
                        vec![ruby_option(args, "primary_key").unwrap_or_else(|| "id".into())];
                }
                key.name = ruby_option(args, "name");
                out.push((alter(from, Action::AddConstraint(key)), at));
            }
        } else if let Some(args) = call("add_index") {
            if let Some(t) = ruby_args(args).into_iter().next() {
                let rest = args.split_once(',').map_or("", |(_, r)| r);
                out.push((
                    Op::CreateIndex {
                        table: QName::bare(&t),
                        index: index(rest, at),
                    },
                    at,
                ));
            }
        } else if let Some(args) = call("change_column_null") {
            if let [t, c, ..] = ruby_args(args).as_slice() {
                let on = !args.contains("true");
                out.push((
                    alter(
                        t,
                        Action::AlterColumn {
                            column: c.clone(),
                            change: super::ddl::Change::NotNull(on),
                        },
                    ),
                    at,
                ));
            }
        } else if let Some(args) = call("drop_table") {
            if let Some(name) = ruby_args(args).into_iter().next() {
                out.push((Op::DropTable(vec![QName::bare(&name)]), at));
            }
        } else if let Some(args) = call("rename_table")
            && let [from, to, ..] = ruby_args(args).as_slice()
        {
            out.push((alter(from, Action::RenameTo(to.clone())), at));
        }
    }
    out
}

fn alter(table: &str, action: Action) -> Op {
    Op::AlterTable {
        table: QName::bare(table),
        actions: vec![action],
    }
}
