//! A database's maps, from its SQL and its migrations:
//!
//! - **schema** — each table with its columns (type, keys, nullability,
//!   defaults, identity), indexes, checks, row-level security and its
//!   policies, and triggers; each view and enum; an arrow per foreign key,
//!   joining its columns to the ones they reference — the migrations
//!   replayed in order, so a table dropped or renamed is gone and a column
//!   added later is there;
//! - **history** — each migration in order, and what it creates, changes
//!   and drops, so when a column arrived reads off the chain;
//! - **lineage** — what a view, a `CREATE TABLE … AS`, an `INSERT … SELECT`
//!   or a CTE reads from;
//! - **usage** — which source files query which tables, from the SQL in
//!   their strings.
//!
//! The SQL of PostgreSQL (`DO` blocks and `SET search_path` too), MySQL,
//! SQLite and SQL Server is read, and Rails' `db/schema.rb` and
//! `db/migrate/*.rb`. A project with several databases — folders of
//! migrations side by side — has a schema and a history for each (see
//! [`files`]). A down migration (`*.down.sql`, what follows a `-- +goose
//! Down` or `-- migrate:down` marker) undoes and is left out.

use std::collections::BTreeSet;

use clew_protocol::{MapField, ProjectMap};

use super::{Corpus, MapBuilder, Source};
use crate::highlight::Lang;

pub(crate) mod ddl;
mod django;
mod ecto;
mod files;
mod lex;
mod prisma;
mod rails;
mod schema;
mod sqlalchemy;
#[cfg(test)]
mod tests;

use ddl::{Action, Change, Kind, Op, QName};
pub(crate) use rails::pluralize;
use schema::{Changes, Schema, Table};

/// Each database's schema, as its files leave it: the one with the most
/// tables first.
pub(crate) fn schemas(corpus: &Corpus) -> Vec<(files::Database, Schema)> {
    let mut out: Vec<(files::Database, Schema)> = files::databases(corpus)
        .into_iter()
        .map(|db| {
            let mut schema = Schema::default();
            for changes in &db.changes {
                schema.apply(changes);
            }
            (db, schema)
        })
        .collect();
    // A Prisma schema: the database its models make. Prisma's own SQL
    // migrations beside it make the same tables, by their columns' names
    // alone: the models stand for them (and the migrations keep their
    // history).
    let prisma: Vec<&Source> = corpus
        .files
        .iter()
        .filter(|s| prisma::is_prisma(s) && !crate::weakspots::is_test_path(&s.rel))
        .collect();
    if let Some((root, schema)) = prisma::schema(&prisma) {
        let migrations = match root.is_empty() {
            true => "migrations".to_string(),
            false => format!("{root}/migrations"),
        };
        let theirs = |db: &files::Database| {
            db.root == migrations
                || (db.root.is_empty() && db.changes.iter().all(|c| c.rel.starts_with(&migrations)))
        };
        match out.iter_mut().find(|(db, _)| theirs(db)) {
            Some((_, s)) => *s = schema,
            None => out.push((
                files::Database {
                    root: prisma.first().map_or(root, |s| s.rel.clone()),
                    changes: Vec::new(),
                },
                schema,
            )),
        }
    }
    // Django's models: the database they make (Django's migrations are
    // Python, not SQL).
    let models: Vec<&Source> = corpus
        .files
        .iter()
        .filter(|s| django::is_models(s) && !crate::weakspots::is_test_path(&s.rel))
        .collect();
    if let Some(schema) = django::schema(&models) {
        out.push((
            files::Database {
                root: match models.as_slice() {
                    [one] => one.rel.clone(),
                    _ => "Django models".to_string(),
                },
                changes: Vec::new(),
            },
            schema,
        ));
    }
    // SQLAlchemy's models (Alembic's migrations are Python too).
    let declared: Vec<&Source> = corpus
        .files
        .iter()
        .filter(|s| sqlalchemy::is_models(s) && !crate::weakspots::is_test_path(&s.rel))
        .collect();
    if let Some(schema) = sqlalchemy::schema(&declared) {
        out.push((
            files::Database {
                root: match declared.as_slice() {
                    [one] => one.rel.clone(),
                    _ => "SQLAlchemy models".to_string(),
                },
                changes: Vec::new(),
            },
            schema,
        ));
    }
    // Ecto's schemas: the tables their modules map (Ecto's migrations
    // are Elixir, not SQL).
    let ecto: Vec<&Source> = corpus
        .files
        .iter()
        .filter(|s| ecto::is_schema(s) && !crate::weakspots::is_test_path(&s.rel))
        .collect();
    if let Some(schema) = ecto::schema(&ecto) {
        out.push((
            files::Database {
                root: match ecto.as_slice() {
                    [one] => one.rel.clone(),
                    _ => "Ecto schemas".to_string(),
                },
                changes: Vec::new(),
            },
            schema,
        ));
    }
    out.sort_by(|a, b| {
        (b.1.tables.len() + b.1.views.len())
            .cmp(&(a.1.tables.len() + a.1.views.len()))
            .then_with(|| files::natural(&a.0.root, &b.0.root))
    });
    out
}

/// What a database is called beside others: its folder of migrations, or
/// the folder its files share (its file, when it has one).
fn name_of(db: &files::Database) -> String {
    if !db.root.is_empty() {
        return db.root.clone();
    }
    let dirs: Vec<&str> = db
        .changes
        .iter()
        .map(|c| c.rel.rsplit_once('/').map_or("", |(d, _)| d))
        .collect();
    match db.changes.as_slice() {
        [one] => one.rel.clone(),
        _ => {
            let first = dirs.first().copied().unwrap_or("");
            let mut common: Vec<&str> = first.split('/').collect();
            for d in &dirs[1..] {
                let parts: Vec<&str> = d.split('/').collect();
                let same = common
                    .iter()
                    .zip(&parts)
                    .take_while(|(a, b)| a == b)
                    .count();
                common.truncate(same);
            }
            match common.join("/") {
                c if c.is_empty() => "SQL".to_string(),
                c => c,
            }
        }
    }
}

pub fn maps(corpus: &Corpus) -> Vec<ProjectMap> {
    let databases = schemas(corpus);
    let shown: Vec<&(files::Database, Schema)> = databases
        .iter()
        .filter(|(_, s)| !s.tables.is_empty() || !s.views.is_empty())
        .collect();
    // With several databases, each map says whose it is.
    let several = shown.len() > 1;
    let whose = |db: &files::Database| several.then(|| name_of(db));
    let mut out = Vec::new();
    for (db, schema) in &shown {
        out.extend(schema_map(schema, whose(db).as_deref()));
    }
    for (db, _) in &shown {
        out.extend(history_map(&db.changes, whose(db).as_deref()));
    }
    let all: Vec<&Changes> = databases.iter().flat_map(|(db, _)| &db.changes).collect();
    out.extend(lineage_map(&all));
    out.extend(usage_map(
        corpus,
        &shown.iter().map(|(_, s)| s).collect::<Vec<_>>(),
    ));
    out
}

// -- the schema -------------------------------------------------------------------

/// A table's columns as a map's fields.
fn table_fields(table: &Table) -> Vec<MapField> {
    let pk = table.primary_key();
    table
        .columns
        .iter()
        .map(|c| {
            let key = pk.iter().any(|k| k.eq_ignore_ascii_case(&c.name));
            MapField {
                name: c.name.clone(),
                ty: c.ty.clone(),
                key,
                nullable: !c.not_null && !key,
                unique: !(key && pk.len() == 1) && table.is_unique(&c.name),
                identity: c.identity,
                generated: c.generated.is_some(),
                indexed: table.is_indexed(&c.name),
                default: c
                    .generated
                    .clone()
                    .or(c.default.clone())
                    .unwrap_or_default(),
                doc: c.comment.clone().unwrap_or_default(),
                file: match c.file == table.file {
                    true => String::new(),
                    false => c.file.clone(),
                },
                line: c.line,
            }
        })
        .collect()
}

/// What a table has besides its columns, a line each: its comment, a key
/// over several columns, indexes, checks, row-level security, policies,
/// triggers.
fn table_detail(table: &Table) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(c) = table.comment.as_deref().filter(|c| !c.trim().is_empty()) {
        out.push(c.trim().to_string());
    }
    if !table.derived_from.is_empty() {
        out.push(format!(
            "made from {}",
            table
                .derived_from
                .iter()
                .map(QName::label)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    for c in &table.constraints {
        match &c.kind {
            Kind::PrimaryKey(cols) if cols.len() > 1 => {
                out.push(format!("primary key ({})", cols.join(", ")));
            }
            Kind::Unique(cols) if cols.len() > 1 => {
                out.push(format!("unique ({})", cols.join(", ")));
            }
            Kind::Check(expr) => out.push(format!("check ({expr})")),
            _ => {}
        }
    }
    for i in &table.indexes {
        let mut line = format!(
            "{} {}({})",
            if i.unique { "unique index" } else { "index" },
            i.name
                .as_deref()
                .map(|n| format!("{n} "))
                .unwrap_or_default(),
            i.columns.join(", ")
        );
        if let Some(m) = &i.method {
            line.push_str(&format!(" using {m}"));
        }
        if i.partial {
            line.push_str(" where …");
        }
        out.push(line);
    }
    if table.rls {
        out.push("row level security".into());
    }
    for p in &table.policies {
        let mut line = format!("policy {}: {}", p.name, p.command);
        if !p.roles.is_empty() {
            line.push_str(&format!(" to {}", p.roles.join(", ")));
        }
        if let Some(using) = &p.using {
            line.push_str(&format!(" using ({using})"));
        }
        if let Some(check) = &p.check {
            line.push_str(&format!(" with check ({check})"));
        }
        out.push(line);
    }
    for t in &table.triggers {
        let mut line = format!("trigger {}: {}", t.name, t.timing);
        if let Some(f) = &t.function {
            line.push_str(&format!(" → {f}"));
        }
        out.push(line);
    }
    out
}

/// A map's id, for one of several databases (`whose`) or the one.
fn map_id(id: &str, whose: Option<&str>) -> String {
    match whose {
        Some(root) => format!("{id}:{root}"),
        None => id.to_string(),
    }
}

fn schema_map(schema: &Schema, whose: Option<&str>) -> Option<ProjectMap> {
    if schema.tables.is_empty() && schema.views.is_empty() {
        return None;
    }
    let id = map_id("sql.schema", whose);
    let mut map = MapBuilder::new(
        &id,
        "Database schema",
        "a card per table and view, as the migrations leave them, listing its columns, with a line from each foreign key's columns to the ones they reference",
    );
    if let Some(root) = whose {
        map.title_for(root);
    }
    for (k, table) in &schema.tables {
        let at = map.node("table", k, &table.name.label());
        map.place(at, &table.file, table.line);
        map.fields(at, table_fields(table));
        map.detail(at, table_detail(table).join("\n"));
    }
    for (k, view) in &schema.views {
        let at = map.node("view", k, &view.name.label());
        map.place(at, &view.file, view.line);
        map.fields(
            at,
            view.columns
                .iter()
                .map(|c| MapField {
                    name: c.clone(),
                    nullable: true,
                    ..Default::default()
                })
                .collect(),
        );
        let mut detail = Vec::new();
        if let Some(c) = view.comment.as_deref().filter(|c| !c.trim().is_empty()) {
            detail.push(c.trim().to_string());
        }
        if view.materialized {
            detail.push("materialized".into());
        }
        if !view.sources.is_empty() {
            detail.push(format!(
                "reads {}",
                view.sources
                    .iter()
                    .map(QName::label)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        map.detail(at, detail.join("\n"));
    }
    for (k, e) in &schema.enums {
        let at = map.node("enum", k, &e.name.label());
        map.place(at, &e.file, e.line);
        map.fields(
            at,
            e.values
                .iter()
                .map(|v| MapField {
                    name: v.clone(),
                    ..Default::default()
                })
                .collect(),
        );
    }
    // A foreign key: a line from its columns to the ones they reference.
    for (k, table) in &schema.tables {
        let from = map.node("table", k, &table.name.label());
        for fk in table.foreign_keys() {
            let target = schema.target(fk);
            let to = match target {
                Some(t) => map.node("table", &t.name.key(), &t.name.label()),
                None => {
                    // A table this database never makes (another's, an
                    // extension's): a card with its name alone.
                    let name = fk.table.in_schema(fk.search.as_deref());
                    map.node("table", &name.key(), &name.label())
                }
            };
            let referenced: Vec<String> = match (fk.references.is_empty(), target) {
                (true, Some(t)) => t.primary_key().to_vec(),
                _ => fk.references.clone(),
            };
            let pairs = fk
                .columns
                .iter()
                .zip(&referenced)
                .filter_map(|(c, r)| {
                    let a = table
                        .columns
                        .iter()
                        .position(|x| x.name.eq_ignore_ascii_case(c))?;
                    let b = target?
                        .columns
                        .iter()
                        .position(|x| x.name.eq_ignore_ascii_case(r))?;
                    Some((a, b))
                })
                .collect();
            let mut label = fk.columns.join(", ");
            if let Some(action) = &fk.on_delete {
                label.push_str(&format!(" (on delete {action})"));
            }
            map.edge_joining(from, to, &label, pairs);
        }
    }
    for (k, view) in &schema.views {
        let from = map.node("view", k, &view.name.label());
        for source in &view.sources {
            let to = if is_system(source) {
                // The database's own catalogs (`pg_class`): no table of
                // the project's.
                continue;
            } else if let Some(t) = schema.view_key(source, view.search.as_deref()) {
                map.node("view", &t, &schema.views[&t].name.label())
            } else if let Some(t) = schema.table_key(source, view.search.as_deref()) {
                map.node("table", &t, &schema.tables[&t].name.label())
            } else {
                map.node("table", &source.key(), &source.label())
            };
            map.edge(from, to, "reads");
        }
    }
    // A column of an enum's type: an arrow to the enum.
    for (k, table) in &schema.tables {
        for c in &table.columns {
            let ty = c.ty.trim_end_matches("[]");
            let name = QName::from_parts(ty.split('.').map(str::to_string).collect());
            if let Some(e) = name.and_then(|n| schema.enum_key(&n, None)) {
                let from = map.node("table", k, &table.name.label());
                let to = map.node("enum", &e, &schema.enums[&e].name.label());
                map.edge(from, to, &c.name);
            }
        }
    }
    map.finish()
}

/// Whether `name` is one of the database's own catalogs: PostgreSQL's
/// `pg_class` and `information_schema`, SQL Server's `sys`, MySQL's and
/// SQLite's.
fn is_system(name: &QName) -> bool {
    let lower = name.name.to_ascii_lowercase();
    let schema = name.schema.as_deref().map(str::to_ascii_lowercase);
    matches!(
        schema.as_deref(),
        Some("pg_catalog" | "information_schema" | "sys" | "mysql" | "performance_schema")
    ) || (schema.is_none() && (lower.starts_with("pg_") || lower.starts_with("sqlite_")))
}

// -- history ------------------------------------------------------------------------

/// What an op does, a line for each table it touches: the table, how it is
/// touched (`creates`, `alters`, `renames`, `drops`) and a line saying it.
fn describe(op: &Op) -> Vec<(QName, &'static str, String)> {
    let cols = |names: &[String]| names.join(", ");
    match op {
        Op::CreateTable { name, columns, .. } => vec![(
            name.clone(),
            "creates",
            format!(
                "+ table {} ({})",
                name.label(),
                columns
                    .iter()
                    .map(|c| c.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )],
        Op::Derive { name, view, .. } => vec![(
            name.clone(),
            "creates",
            format!(
                "+ {} {}",
                if *view { "view" } else { "table" },
                name.label()
            ),
        )],
        Op::AlterTable { table, actions } => {
            let t = table.label();
            actions
                .iter()
                .map(|a| {
                    let (verb, said) = match a {
                        Action::AddColumn { column, .. } => (
                            "alters",
                            format!("+ column {t}.{} {}", column.name, column.ty)
                                .trim()
                                .to_string(),
                        ),
                        Action::DropColumn(c) => ("alters", format!("- column {t}.{c}")),
                        Action::RenameColumn { from, to } => {
                            ("alters", format!("~ column {t}.{from} → {to}"))
                        }
                        Action::AlterColumn { column, change } => (
                            "alters",
                            match change {
                                Change::Type(ty) => format!("~ column {t}.{column} type {ty}"),
                                Change::NotNull(true) => format!("~ column {t}.{column} not null"),
                                Change::NotNull(false) => format!("~ column {t}.{column} null"),
                                Change::Default(Some(d)) => {
                                    format!("~ column {t}.{column} default {d}")
                                }
                                Change::Default(None) => {
                                    format!("~ column {t}.{column} no default")
                                }
                                Change::Identity(true) => format!("~ column {t}.{column} identity"),
                                Change::Identity(false) => {
                                    format!("~ column {t}.{column} no identity")
                                }
                            },
                        ),
                        Action::Redefine { from, column, .. } => (
                            "alters",
                            match from.eq_ignore_ascii_case(&column.name) {
                                true => format!("~ column {t}.{} {}", column.name, column.ty),
                                false => {
                                    format!("~ column {t}.{from} → {} {}", column.name, column.ty)
                                }
                            },
                        ),
                        Action::AddConstraint(c) => (
                            "alters",
                            match &c.kind {
                                Kind::PrimaryKey(k) => format!("+ primary key {t}({})", cols(k)),
                                Kind::Unique(k) => format!("+ unique {t}({})", cols(k)),
                                Kind::ForeignKey(fk) => format!(
                                    "+ foreign key {t}({}) → {}",
                                    cols(&fk.columns),
                                    fk.table.label()
                                ),
                                Kind::Check(e) => format!("+ check {t} ({e})"),
                                Kind::Index(i) => format!("+ index {t}({})", cols(&i.columns)),
                            },
                        ),
                        Action::DropConstraint(n) => ("alters", format!("- constraint {t}.{n}")),
                        Action::RenameConstraint { from, to } => {
                            ("alters", format!("~ constraint {t}.{from} → {to}"))
                        }
                        Action::DropPrimaryKey => ("alters", format!("- primary key {t}")),
                        Action::DropIndex(n) => ("alters", format!("- index {n}")),
                        Action::RenameTo(to) => ("renames", format!("~ table {t} → {to}")),
                        Action::SetSchema(s) => ("renames", format!("~ table {t} → schema {s}")),
                        Action::RowLevelSecurity(true) => {
                            ("alters", format!("+ row level security {t}"))
                        }
                        Action::RowLevelSecurity(false) => {
                            ("alters", format!("- row level security {t}"))
                        }
                    };
                    (table.clone(), verb, said)
                })
                .collect()
        }
        Op::DropTable(names) => names
            .iter()
            .map(|n| (n.clone(), "drops", format!("- table {}", n.label())))
            .collect(),
        Op::CreateIndex { table, index } => vec![(
            table.clone(),
            "alters",
            format!(
                "+ {} {}on {}({})",
                if index.unique {
                    "unique index"
                } else {
                    "index"
                },
                index
                    .name
                    .as_deref()
                    .map(|n| format!("{n} "))
                    .unwrap_or_default(),
                table.label(),
                cols(&index.columns)
            ),
        )],
        Op::CreatePolicy { table, policy } => vec![(
            table.clone(),
            "alters",
            format!("+ policy {} on {}", policy.name, table.label()),
        )],
        Op::DropPolicy { table, name } => vec![(
            table.clone(),
            "alters",
            format!("- policy {name} on {}", table.label()),
        )],
        Op::CreateTrigger { table, trigger } => vec![(
            table.clone(),
            "alters",
            format!("+ trigger {} on {}", trigger.name, table.label()),
        )],
        _ => Vec::new(),
    }
}

/// Lines for what an op does outside any one table (an enum, a schema, an
/// index dropped by name).
fn describe_other(op: &Op) -> Option<String> {
    match op {
        Op::CreateEnum { name, values } => {
            Some(format!("+ enum {} ({})", name.label(), values.join(", ")))
        }
        Op::AddEnumValue { name, value, .. } => Some(format!("+ value {}.{value}", name.label())),
        Op::RenameEnumValue { name, from, to } => {
            Some(format!("~ value {}.{from} → {to}", name.label()))
        }
        Op::DropType(names) => Some(format!(
            "- type {}",
            names
                .iter()
                .map(QName::label)
                .collect::<Vec<_>>()
                .join(", ")
        )),
        Op::CreateSchema(s) => Some(format!("+ schema {s}")),
        Op::DropSchema(s) => Some(format!("- schema {}", s.join(", "))),
        Op::DropIndex { names, .. } => Some(format!(
            "- index {}",
            names
                .iter()
                .map(QName::label)
                .collect::<Vec<_>>()
                .join(", ")
        )),
        Op::DropView(names) => Some(format!(
            "- view {}",
            names
                .iter()
                .map(QName::label)
                .collect::<Vec<_>>()
                .join(", ")
        )),
        _ => None,
    }
}

fn history_map(migrations: &[Changes], whose: Option<&str>) -> Option<ProjectMap> {
    let ddl: Vec<&Changes> = migrations
        .iter()
        .filter(|c| {
            c.ops
                .iter()
                .any(|(op, _)| !matches!(op, Op::InsertSelect { .. }))
        })
        .collect();
    if ddl.len() < 2 {
        return None;
    }
    let id = map_id("sql.history", whose);
    let mut map = MapBuilder::new(
        &id,
        "Schema history",
        "each migration in the order it runs, what it changes in its detail, and an arrow to each table it creates, alters or drops",
    );
    if let Some(root) = whose {
        map.title_for(root);
    }
    let mut previous = None;
    for changes in ddl {
        let step = map.node(
            "migration",
            &changes.rel,
            changes.rel.rsplit('/').next().unwrap_or(&changes.rel),
        );
        map.place(step, &changes.rel, 1);
        map.edge(previous, step, "then");
        previous = step;
        for (op, line) in &changes.ops {
            if let Some(said) = describe_other(op) {
                map.add_detail(step, &format!("{said} (line {line})"));
            }
            for (table, verb, said) in describe(op) {
                map.add_detail(step, &format!("{said} (line {line})"));
                let at = map.node("table", &table.key(), &table.label());
                map.edge(step, at, verb);
            }
        }
    }
    map.finish()
}

// -- lineage ------------------------------------------------------------------------

fn lineage_map(migrations: &[&Changes]) -> Option<ProjectMap> {
    let mut map = MapBuilder::new(
        "sql.lineage",
        "Data lineage",
        "an arrow from what is read to what is built from it, such as from a table to the view, CREATE TABLE … AS or INSERT … SELECT that reads it, through any CTEs in between",
    );
    for changes in migrations {
        for (op, line) in &changes.ops {
            let (target, kind, sources, ctes, label) = match op {
                Op::Derive {
                    name,
                    view,
                    sources,
                    ctes,
                    ..
                } => (
                    name,
                    if *view { "view" } else { "table" },
                    sources,
                    ctes,
                    "select",
                ),
                Op::InsertSelect {
                    table,
                    sources,
                    ctes,
                } => (table, "table", sources, ctes, "insert … select"),
                _ => continue,
            };
            let to = map.node(kind, &target.key(), &target.label());
            map.place(to, &changes.rel, *line);
            let cte_names: BTreeSet<String> = ctes.iter().map(|(n, _)| n.to_lowercase()).collect();
            let is_cte =
                |q: &QName| q.schema.is_none() && cte_names.contains(&q.name.to_lowercase());
            let cte_key = |name: &str| format!("{}:{}", target.key(), name.to_lowercase());
            for (cte, reads) in ctes {
                let at = map.node("cte", &cte_key(cte), cte);
                map.place(at, &changes.rel, *line);
                map.edge(at, to, "with");
                for source in reads {
                    let from = match is_cte(source) {
                        true => map.node("cte", &cte_key(&source.name), &source.name),
                        false => map.node("table", &source.key(), &source.label()),
                    };
                    map.edge(from, at, "select");
                }
            }
            for source in sources {
                let from = match is_cte(source) {
                    true => map.node("cte", &cte_key(&source.name), &source.name),
                    false => map.node("table", &source.key(), &source.label()),
                };
                map.edge(from, to, label);
            }
        }
    }
    map.finish()
}

// -- usage --------------------------------------------------------------------------

/// Which source files query which known tables: SQL in a string of code —
/// a line with a quote and a statement's keyword — naming a table after
/// `FROM`, `JOIN`, `INTO` or `UPDATE`.
fn usage_map(corpus: &Corpus, schemas: &[&Schema]) -> Option<ProjectMap> {
    if schemas.is_empty() {
        return None;
    }
    let mut map = MapBuilder::new(
        "sql.usage",
        "Tables in code",
        "an arrow from a source file to each table its SQL strings read or write",
    );
    for source in &corpus.files {
        if matches!(source.lang, Some(Lang::Sql) | None) {
            continue;
        }
        for (n, line) in source.text.lines().enumerate() {
            if !line.contains(['"', '\'', '`']) || !looks_like_sql(line) {
                continue;
            }
            for literal in string_contents(line) {
                if looks_like_sql(&literal) {
                    link_tables(&mut map, schemas, source, n + 1, &literal);
                }
            }
        }
    }
    map.finish()
}

/// The text inside each string literal on a line of code.
fn string_contents(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = line.char_indices().peekable();
    while let Some((_, c)) = chars.next() {
        if !matches!(c, '"' | '\'' | '`') {
            continue;
        }
        let mut inner = String::new();
        let mut closed = false;
        while let Some((_, d)) = chars.next() {
            if d == '\\' {
                if let Some((_, e)) = chars.next() {
                    inner.push(e);
                }
                continue;
            }
            if d == c {
                closed = true;
                break;
            }
            inner.push(d);
        }
        if closed || !inner.is_empty() {
            out.push(inner);
        }
    }
    out
}

/// The known tables one string's SQL names, linked from `source`.
fn link_tables(map: &mut MapBuilder, schemas: &[&Schema], source: &Source, line: usize, sql: &str) {
    for (name, writes) in ddl::named_tables(sql) {
        let found = schemas.iter().find_map(|s| {
            if let Some(k) = s.table_key(&name, None) {
                let t = &s.tables[&k];
                return Some(("table", k, t.name.label(), t.file.clone(), t.line));
            }
            let k = s.view_key(&name, None)?;
            let v = &s.views[&k];
            Some(("view", k, v.name.label(), v.file.clone(), v.line))
        });
        let Some((kind, key, label, file, at)) = found else {
            continue;
        };
        let from = map.node("file", &source.rel, &source.rel);
        map.place(from, &source.rel, line);
        let to = map.node(kind, &key, &label);
        map.place(to, &file, at);
        map.edge(from, to, if writes { "writes" } else { "reads" });
    }
}

fn looks_like_sql(line: &str) -> bool {
    let upper = line.to_ascii_uppercase();
    ["SELECT ", "INSERT ", "UPDATE ", "DELETE ", "FROM ", "JOIN "]
        .iter()
        .any(|k| upper.contains(k))
}

// -- findings -----------------------------------------------------------------------

/// What a database's schema, or one of its migrations, asks a reader to
/// look at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub rel: String,
    pub line: usize,
    /// The table it is about.
    pub table: String,
    pub issue: Issue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Issue {
    /// A table with columns and no primary key: no row of it can be named.
    NoPrimaryKey,
    /// A foreign key whose columns lead no index: a join on it, and a
    /// delete in the table it references, reads the whole table
    /// (PostgreSQL makes none for it).
    UnindexedForeignKey,
    /// Row-level security off on a table of a schema whose other tables
    /// have it on.
    RowLevelSecurityOff,
    /// A migration that drops a table or a column an earlier one made: its
    /// data is gone.
    DropsData,
    /// A migration that adds a `NOT NULL` column with no default to a table
    /// an earlier one made: it fails on a table with rows.
    NotNullWithoutDefault,
    /// A migration that holds writes to a table an earlier one made while
    /// it works: an index built without `CONCURRENTLY`, a column's type
    /// changed (PostgreSQL).
    LocksTable,
}

/// Whether a database's SQL is PostgreSQL's: its row-level security,
/// enums, types or `CONCURRENTLY`.
fn is_postgres(changes: &[Changes]) -> bool {
    const TYPES: &[&str] = &[
        "timestamptz",
        "jsonb",
        "bytea",
        "bigserial",
        "serial",
        "tsvector",
        "inet",
        "citext",
    ];
    changes.iter().flat_map(|c| &c.ops).any(|(op, _)| match op {
        Op::CreatePolicy { .. } | Op::CreateEnum { .. } | Op::SearchPath(_) => true,
        Op::CreateIndex { index, .. } => {
            index.concurrent
                || index
                    .method
                    .as_deref()
                    .is_some_and(|m| matches!(m, "gin" | "gist" | "brin"))
        }
        Op::CreateTable { columns, .. } => columns
            .iter()
            .any(|c| TYPES.contains(&c.ty.as_str()) || c.ty.ends_with("[]")),
        Op::AlterTable { actions, .. } => actions
            .iter()
            .any(|a| matches!(a, Action::RowLevelSecurity(_))),
        _ => false,
    })
}

/// What the project's databases ask a reader to look at: each schema as
/// its migrations leave it, and each migration that changes a table an
/// earlier one made. A test's SQL is left out.
pub fn findings(corpus: &Corpus) -> Vec<Finding> {
    let mut out = Vec::new();
    for db in files::databases(corpus) {
        let postgres = is_postgres(&db.changes);
        let mut schema = Schema::default();
        for changes in &db.changes {
            let rel = changes.rel.as_str();
            if !crate::weakspots::is_test_path(rel) {
                let before: BTreeSet<String> = schema.tables.keys().cloned().collect();
                let mut search: Option<String> = None;
                let mut note = |line: usize, table: &QName, issue: Issue| {
                    out.push(Finding {
                        rel: rel.to_string(),
                        line,
                        table: table.label(),
                        issue,
                    });
                };
                for (op, line) in &changes.ops {
                    let existing = |name: &QName, search: &Option<String>| {
                        schema
                            .table_key(name, search.as_deref())
                            .filter(|k| before.contains(k))
                    };
                    match op {
                        Op::SearchPath(s) => search = s.clone(),
                        Op::AlterTable { table, actions } if existing(table, &search).is_some() => {
                            for a in actions {
                                match a {
                                    Action::DropColumn(_) => note(*line, table, Issue::DropsData),
                                    Action::AddColumn { column, .. }
                                        if column.not_null
                                            && column.default.is_none()
                                            && !column.identity
                                            && column.generated.is_none() =>
                                    {
                                        note(
                                            column.line.max(*line),
                                            table,
                                            Issue::NotNullWithoutDefault,
                                        )
                                    }
                                    Action::AlterColumn {
                                        change: Change::Type(_),
                                        ..
                                    } if postgres => note(*line, table, Issue::LocksTable),
                                    _ => {}
                                }
                            }
                        }
                        Op::DropTable(names) => {
                            for name in names {
                                if existing(name, &search).is_some() {
                                    note(*line, name, Issue::DropsData);
                                }
                            }
                        }
                        Op::CreateIndex { table, index }
                            if postgres
                                && !index.concurrent
                                && existing(table, &search).is_some() =>
                        {
                            note(*line, table, Issue::LocksTable)
                        }
                        _ => {}
                    }
                }
            }
            schema.apply(changes);
        }
        // The schema as they leave it.
        let secured: BTreeSet<Option<String>> = schema
            .tables
            .values()
            .filter(|t| t.rls)
            .map(|t| t.name.schema.as_ref().map(|s| s.to_lowercase()))
            .collect();
        for t in schema.tables.values() {
            if crate::weakspots::is_test_path(&t.file) || t.columns.is_empty() {
                continue;
            }
            let at = |issue: Issue, rel: &str, line: usize| Finding {
                rel: rel.to_string(),
                line,
                table: t.name.label(),
                issue,
            };
            if t.primary_key().is_empty() && t.derived_from.is_empty() {
                out.push(at(Issue::NoPrimaryKey, &t.file, t.line));
            }
            if postgres {
                for c in &t.constraints {
                    if let Kind::ForeignKey(fk) = &c.kind
                        && fk.columns.first().is_some_and(|col| !t.is_indexed(col))
                    {
                        let rel = if c.file.is_empty() { &t.file } else { &c.file };
                        out.push(at(Issue::UnindexedForeignKey, rel, c.line.max(1)));
                    }
                }
            }
            if !t.rls && secured.contains(&t.name.schema.as_ref().map(|s| s.to_lowercase())) {
                out.push(at(Issue::RowLevelSecurityOff, &t.file, t.line));
            }
        }
    }
    out
}
