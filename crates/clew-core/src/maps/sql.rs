//! A database's maps, from its SQL and its migrations:
//!
//! - **schema** — each table (its columns, types, keys) and view, an arrow
//!   per foreign key, the migrations replayed in order so a table dropped or
//!   renamed is gone and a column added later is there;
//! - **history** — each migration in order, and the tables it creates,
//!   changes and drops, so when a column arrived reads off the chain;
//! - **lineage** — what a view, a `CREATE TABLE … AS`, an `INSERT … SELECT`
//!   or a CTE reads from;
//! - **usage** — which source files query which tables, from the SQL in
//!   their strings.
//!
//! SQL files are read in path order (migrations are named to sort); a
//! `*.down.sql` file, and what follows a `-- +goose Down` or
//! `-- migrate:down` marker, undoes and is left out. Rails' `db/schema.rb`
//! and `db/migrate/*.rb` are read too (`create_table`, `add_column`,
//! `add_reference`, `add_foreign_key`, …).

use std::collections::{BTreeMap, BTreeSet};

use clew_protocol::ProjectMap;

use super::{Corpus, MapBuilder, Source};
use crate::highlight::Lang;

pub fn maps(corpus: &Corpus) -> Vec<ProjectMap> {
    let migrations = changes(corpus);
    let mut schema = Schema::default();
    for change in &migrations {
        schema.apply(change);
    }
    let mut out = Vec::new();
    out.extend(schema_map(&schema));
    out.extend(history_map(&migrations));
    out.extend(lineage_map(&migrations));
    out.extend(usage_map(corpus, &schema));
    out
}

// -- the schema -------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq)]
struct Column {
    name: String,
    ty: String,
    pk: bool,
}

#[derive(Debug, Clone, PartialEq)]
struct ForeignKey {
    columns: Vec<String>,
    table: String,
    references: Vec<String>,
    /// Its constraint's name, when it has one: `DROP CONSTRAINT` names it.
    constraint: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct Table {
    /// As written (the key is it lower-cased).
    name: String,
    columns: Vec<Column>,
    fks: Vec<ForeignKey>,
    file: String,
    line: usize,
}

#[derive(Debug, Clone, Default)]
struct View {
    name: String,
    sources: Vec<String>,
    file: String,
    line: usize,
}

#[derive(Debug, Default)]
struct Schema {
    tables: BTreeMap<String, Table>,
    views: BTreeMap<String, View>,
}

/// One change a file makes, where it makes it.
#[derive(Debug, Clone, PartialEq)]
enum Op {
    CreateTable {
        name: String,
        columns: Vec<Column>,
        fks: Vec<ForeignKey>,
    },
    AddColumn {
        table: String,
        column: Column,
        fk: Option<ForeignKey>,
    },
    DropColumn {
        table: String,
        column: String,
    },
    RenameColumn {
        table: String,
        from: String,
        to: String,
    },
    AddForeignKey {
        table: String,
        fk: ForeignKey,
    },
    DropConstraint {
        table: String,
        name: String,
    },
    RenameTable {
        from: String,
        to: String,
    },
    DropTable {
        name: String,
    },
    /// A view, or a table made from a query: what it reads.
    Derive {
        name: String,
        view: bool,
        sources: Vec<String>,
        /// CTEs on the way: `(name, what it reads)`.
        ctes: Vec<(String, Vec<String>)>,
    },
    /// `INSERT INTO table … SELECT … FROM sources`.
    InsertSelect {
        table: String,
        sources: Vec<String>,
        ctes: Vec<(String, Vec<String>)>,
    },
}

/// A file's changes, in order, each with its line.
#[derive(Debug, Clone)]
struct Changes {
    rel: String,
    ops: Vec<(Op, usize)>,
}

fn key(name: &str) -> String {
    name.rsplit('.').next().unwrap_or(name).to_ascii_lowercase()
}

impl Schema {
    fn apply(&mut self, changes: &Changes) {
        for (op, line) in &changes.ops {
            match op.clone() {
                Op::CreateTable { name, columns, fks } => {
                    self.views.remove(&key(&name));
                    self.tables.insert(
                        key(&name),
                        Table {
                            name,
                            columns,
                            fks,
                            file: changes.rel.clone(),
                            line: *line,
                        },
                    );
                }
                Op::AddColumn { table, column, fk } => {
                    if let Some(t) = self.tables.get_mut(&key(&table)) {
                        t.columns
                            .retain(|c| !c.name.eq_ignore_ascii_case(&column.name));
                        t.columns.push(column);
                        t.fks.extend(fk);
                    }
                }
                Op::DropColumn { table, column } => {
                    if let Some(t) = self.tables.get_mut(&key(&table)) {
                        t.columns.retain(|c| !c.name.eq_ignore_ascii_case(&column));
                        t.fks.retain(|fk| {
                            !fk.columns.iter().any(|c| c.eq_ignore_ascii_case(&column))
                        });
                    }
                }
                Op::RenameColumn { table, from, to } => {
                    if let Some(t) = self.tables.get_mut(&key(&table)) {
                        for c in &mut t.columns {
                            if c.name.eq_ignore_ascii_case(&from) {
                                c.name = to.clone();
                            }
                        }
                        for fk in &mut t.fks {
                            for c in &mut fk.columns {
                                if c.eq_ignore_ascii_case(&from) {
                                    *c = to.clone();
                                }
                            }
                        }
                    }
                }
                Op::AddForeignKey { table, fk } => {
                    if let Some(t) = self.tables.get_mut(&key(&table)) {
                        t.fks.push(fk);
                    }
                }
                Op::DropConstraint { table, name } => {
                    if let Some(t) = self.tables.get_mut(&key(&table)) {
                        t.fks.retain(|fk| {
                            !fk.constraint
                                .as_deref()
                                .is_some_and(|c| c.eq_ignore_ascii_case(&name))
                        });
                    }
                }
                Op::RenameTable { from, to } => {
                    if let Some(mut t) = self.tables.remove(&key(&from)) {
                        t.name = to.clone();
                        self.tables.insert(key(&to), t);
                        for t in self.tables.values_mut() {
                            for fk in &mut t.fks {
                                if key(&fk.table) == key(&from) {
                                    fk.table = to.clone();
                                }
                            }
                        }
                    }
                }
                Op::DropTable { name } => {
                    self.tables.remove(&key(&name));
                    self.views.remove(&key(&name));
                }
                Op::Derive {
                    name,
                    view,
                    sources,
                    ctes,
                } => {
                    if view {
                        self.views.insert(
                            key(&name),
                            View {
                                name,
                                sources: through_ctes(&sources, &ctes),
                                file: changes.rel.clone(),
                                line: *line,
                            },
                        );
                    } else {
                        self.tables.insert(
                            key(&name),
                            Table {
                                name,
                                file: changes.rel.clone(),
                                line: *line,
                                ..Default::default()
                            },
                        );
                    }
                }
                Op::InsertSelect { .. } => {}
            }
        }
    }
}

/// What a query reads at bottom: its sources, each CTE among them replaced
/// by what the CTE reads, in turn.
fn through_ctes(sources: &[String], ctes: &[(String, Vec<String>)]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen = BTreeSet::new();
    let mut queue: Vec<String> = sources.to_vec();
    while let Some(source) = queue.pop() {
        if !seen.insert(key(&source)) {
            continue;
        }
        match ctes.iter().find(|(name, _)| key(name) == key(&source)) {
            Some((_, reads)) => queue.extend(reads.iter().cloned()),
            None => out.push(source),
        }
    }
    out.reverse();
    out
}

fn schema_map(schema: &Schema) -> Option<ProjectMap> {
    if schema.tables.is_empty() && schema.views.is_empty() {
        return None;
    }
    let mut map = MapBuilder::new(
        "sql.schema",
        "Database schema",
        "a node per table and view, as the migrations leave them; an arrow is a foreign key (labelled by its columns), or what a view reads",
    );
    for (k, table) in &schema.tables {
        let at = map.node("table", k, &table.name);
        map.place(at, &table.file, table.line);
        let columns: Vec<String> =
            table
                .columns
                .iter()
                .map(|c| {
                    let mut line = format!("{} {}", c.name, c.ty).trim().to_string();
                    if c.pk {
                        line.push_str(" (primary key)");
                    }
                    if let Some(fk) = table.fks.iter().find(|fk| {
                        fk.columns.len() == 1 && fk.columns[0].eq_ignore_ascii_case(&c.name)
                    }) {
                        line.push_str(&format!(" → {}", fk.table));
                    }
                    line
                })
                .collect();
        map.detail(at, columns.join("\n"));
    }
    for (k, view) in &schema.views {
        let at = map.node("view", k, &view.name);
        map.place(at, &view.file, view.line);
        map.detail(at, format!("reads {}", view.sources.join(", ")));
    }
    for (k, table) in &schema.tables {
        for fk in &table.fks {
            let from = map.node("table", k, &table.name);
            let target = key(&fk.table);
            let to = match schema.views.contains_key(&target) {
                true => map.node("view", &target, &fk.table),
                false => map.node("table", &target, &fk.table),
            };
            map.edge(from, to, &fk.columns.join(", "));
        }
    }
    for (k, view) in &schema.views {
        let from = map.node("view", k, &view.name);
        for source in &view.sources {
            let target = key(source);
            let to = match schema.views.contains_key(&target) {
                true => map.node("view", &target, source),
                false => map.node("table", &target, source),
            };
            map.edge(from, to, "reads");
        }
    }
    map.finish()
}

fn history_map(migrations: &[Changes]) -> Option<ProjectMap> {
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
    let mut map = MapBuilder::new(
        "sql.history",
        "Schema history",
        "each migration in the order it runs, what it changes in its detail, and an arrow to each table it creates, alters or drops",
    );
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
            let (table, verb, said) = match op {
                Op::CreateTable { name, columns, .. } => (
                    name,
                    "creates",
                    format!(
                        "+ table {name} ({})",
                        columns
                            .iter()
                            .map(|c| c.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                ),
                Op::AddColumn { table, column, .. } => (
                    table,
                    "alters",
                    format!("+ column {table}.{} {}", column.name, column.ty)
                        .trim()
                        .to_string(),
                ),
                Op::DropColumn { table, column } => {
                    (table, "alters", format!("- column {table}.{column}"))
                }
                Op::RenameColumn { table, from, to } => {
                    (table, "alters", format!("~ column {table}.{from} → {to}"))
                }
                Op::AddForeignKey { table, fk } => (
                    table,
                    "alters",
                    format!(
                        "+ foreign key {table}({}) → {}",
                        fk.columns.join(", "),
                        fk.table
                    ),
                ),
                Op::DropConstraint { table, name } => {
                    (table, "alters", format!("- constraint {table}.{name}"))
                }
                Op::RenameTable { from, to } => (from, "renames", format!("~ table {from} → {to}")),
                Op::DropTable { name } => (name, "drops", format!("- table {name}")),
                Op::Derive { name, view, .. } => (
                    name,
                    "creates",
                    format!("+ {} {name}", if *view { "view" } else { "table" }),
                ),
                Op::InsertSelect { .. } => continue,
            };
            map.add_detail(step, &format!("{said} (line {line})"));
            let at = map.node("table", &key(table), table);
            map.edge(step, at, verb);
        }
    }
    map.finish()
}

fn lineage_map(migrations: &[Changes]) -> Option<ProjectMap> {
    let mut map = MapBuilder::new(
        "sql.lineage",
        "Data lineage",
        "an arrow from what is read to what is made of it: a table into the view, the CREATE TABLE … AS or the INSERT … SELECT that reads it, through the CTEs on the way",
    );
    for changes in migrations {
        for (op, line) in &changes.ops {
            let (target, kind, sources, ctes, label) = match op {
                Op::Derive {
                    name,
                    view,
                    sources,
                    ctes,
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
            let to = map.node(kind, &key(target), target);
            map.place(to, &changes.rel, *line);
            let cte_names: BTreeSet<String> = ctes.iter().map(|(n, _)| key(n)).collect();
            for (cte, reads) in ctes {
                let at = map.node("cte", &format!("{}:{}", key(target), key(cte)), cte);
                map.place(at, &changes.rel, *line);
                map.edge(at, to, "with");
                for source in reads {
                    let from = match cte_names.contains(&key(source)) {
                        true => {
                            map.node("cte", &format!("{}:{}", key(target), key(source)), source)
                        }
                        false => map.node("table", &key(source), source),
                    };
                    map.edge(from, at, "select");
                }
            }
            for source in sources {
                let from = match cte_names.contains(&key(source)) {
                    true => map.node("cte", &format!("{}:{}", key(target), key(source)), source),
                    false => map.node("table", &key(source), source),
                };
                map.edge(from, to, label);
            }
        }
    }
    map.finish()
}

/// Which source files query which known tables: SQL in a string of code —
/// a line with a quote and a statement's keyword — naming a table after
/// `FROM`, `JOIN`, `INTO` or `UPDATE`.
fn usage_map(corpus: &Corpus, schema: &Schema) -> Option<ProjectMap> {
    if schema.tables.is_empty() && schema.views.is_empty() {
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
                if !looks_like_sql(&literal) {
                    continue;
                }
                let tokens = tokenize(&literal);
                link_tables(&mut map, schema, source, n + 1, &tokens);
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

/// The tables `tokens` (one string's SQL) name after `FROM`, `JOIN`, `INTO`
/// or `UPDATE`, linked from `source`.
fn link_tables(
    map: &mut MapBuilder,
    schema: &Schema,
    source: &Source,
    line: usize,
    tokens: &[Token],
) {
    for (i, t) in tokens.iter().enumerate() {
        let verb = match t.word_upper().as_deref() {
            Some("FROM" | "JOIN") => "reads",
            Some("INTO" | "UPDATE") => "writes",
            _ => continue,
        };
        let Some((name, _)) = name_at(tokens, i + 1) else {
            continue;
        };
        let k = key(&name);
        let kind = if schema.tables.contains_key(&k) {
            "table"
        } else if schema.views.contains_key(&k) {
            "view"
        } else {
            continue;
        };
        let file = map.node("file", &source.rel, &source.rel);
        map.place(file, &source.rel, line);
        let label = schema
            .tables
            .get(&k)
            .map(|t| t.name.clone())
            .or_else(|| schema.views.get(&k).map(|v| v.name.clone()))
            .unwrap_or(name);
        let table = map.node(kind, &k, &label);
        if let Some(t) = schema.tables.get(&k) {
            map.place(table, &t.file, t.line);
        }
        map.edge(file, table, verb);
    }
}

fn looks_like_sql(line: &str) -> bool {
    let upper = line.to_ascii_uppercase();
    ["SELECT ", "INSERT ", "UPDATE ", "DELETE ", "FROM ", "JOIN "]
        .iter()
        .any(|k| upper.contains(k))
}

// -- reading the files -------------------------------------------------------------

/// Every file's changes, in the order they apply: SQL files and Rails'
/// schema and migrations, by path.
fn changes(corpus: &Corpus) -> Vec<Changes> {
    let mut out = Vec::new();
    for source in &corpus.files {
        if source.lang == Some(Lang::Sql) {
            let name = source.name().to_ascii_lowercase();
            if name.ends_with(".down.sql") || name.contains("_down.") || name == "down.sql" {
                continue;
            }
            let ops = sql_ops(&up_part(&source.text));
            if !ops.is_empty() {
                out.push(Changes {
                    rel: source.rel.clone(),
                    ops,
                });
            }
        } else if source.lang == Some(Lang::Ruby) && is_rails_schema(source) {
            let ops = rails_ops(&source.text);
            if !ops.is_empty() {
                out.push(Changes {
                    rel: source.rel.clone(),
                    ops,
                });
            }
        }
    }
    // Rails' schema.rb is the schema as it stands: the migrations, when it
    // is there, are history only — applied first, it after them.
    out.sort_by_key(|c| c.rel.ends_with("db/schema.rb"));
    out
}

fn is_rails_schema(source: &Source) -> bool {
    source.rel.ends_with("db/schema.rb") || source.rel.contains("db/migrate/")
}

/// A migration's "up" part: what precedes its down marker.
fn up_part(text: &str) -> String {
    let mut out = String::new();
    for line in text.lines() {
        let l = line.trim().to_ascii_lowercase();
        if l.starts_with("-- +goose down")
            || l.starts_with("-- migrate:down")
            || l.starts_with("-- +migrate down")
        {
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

// -- SQL ----------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Word(String),
    /// A quoted identifier (`"x"`, `` `x` ``, `[x]`).
    Ident(String),
    /// A string literal (or a dollar-quoted body).
    Str,
    Punct(char),
}

#[derive(Debug, Clone)]
struct Token {
    tok: Tok,
    /// Byte offset in the text.
    at: usize,
}

impl Token {
    fn word_upper(&self) -> Option<String> {
        match &self.tok {
            Tok::Word(w) => Some(w.to_ascii_uppercase()),
            _ => None,
        }
    }
    fn is_word(&self, w: &str) -> bool {
        matches!(&self.tok, Tok::Word(x) if x.eq_ignore_ascii_case(w))
    }
    fn is(&self, c: char) -> bool {
        self.tok == Tok::Punct(c)
    }
}

fn tokenize(text: &str) -> Vec<Token> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
        } else if c == b'-' && b.get(i + 1) == Some(&b'-') {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if c == b'/' && b.get(i + 1) == Some(&b'*') {
            i += 2;
            while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i += 2;
        } else if c == b'\'' {
            let start = i;
            i += 1;
            while i < b.len() {
                if b[i] == b'\'' {
                    if b.get(i + 1) == Some(&b'\'') {
                        i += 2;
                        continue;
                    }
                    break;
                }
                if b[i] == b'\\' {
                    i += 1;
                }
                i += 1;
            }
            i += 1;
            out.push(Token {
                tok: Tok::Str,
                at: start,
            });
        } else if c == b'"' || c == b'`' || c == b'[' {
            let close = match c {
                b'[' => b']',
                other => other,
            };
            let start = i;
            i += 1;
            let from = i;
            while i < b.len() && b[i] != close && b[i] != b'\n' {
                i += 1;
            }
            let name = text.get(from..i).unwrap_or("").to_string();
            i += 1;
            out.push(Token {
                tok: Tok::Ident(name),
                at: start,
            });
        } else if c == b'$' {
            // A dollar-quoted body: `$$ … $$` or `$tag$ … $tag$`.
            let start = i;
            let mut j = i + 1;
            while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                j += 1;
            }
            if b.get(j) == Some(&b'$') {
                let tag = &text[i..=j];
                match text[j + 1..].find(tag) {
                    Some(end) => i = j + 1 + end + tag.len(),
                    None => i = b.len(),
                }
                out.push(Token {
                    tok: Tok::Str,
                    at: start,
                });
            } else {
                i = j;
            }
        } else if c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80 {
            let start = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] >= 0x80) {
                i += 1;
            }
            out.push(Token {
                tok: Tok::Word(text[start..i].to_string()),
                at: start,
            });
        } else {
            out.push(Token {
                tok: Tok::Punct(c as char),
                at: i,
            });
            i += 1;
        }
    }
    out
}

/// A (possibly dotted) name at `i`: its text and the index after it.
fn name_at(tokens: &[Token], mut i: usize) -> Option<(String, usize)> {
    let mut parts = Vec::new();
    loop {
        let part = match &tokens.get(i)?.tok {
            Tok::Word(w) => w.clone(),
            Tok::Ident(w) => w.clone(),
            _ => return None,
        };
        parts.push(part);
        i += 1;
        if tokens.get(i).is_some_and(|t| t.is('.')) {
            i += 1;
            continue;
        }
        return Some((parts.join("."), i));
    }
}

/// The index just past the `(` … `)` group opening at `i`.
fn past_group(tokens: &[Token], i: usize) -> usize {
    let mut depth = 0;
    let mut j = i;
    while j < tokens.len() {
        if tokens[j].is('(') {
            depth += 1;
        } else if tokens[j].is(')') {
            depth -= 1;
            if depth == 0 {
                return j + 1;
            }
        }
        j += 1;
    }
    tokens.len()
}

/// Split `tokens` on top-level `,`.
fn split_commas(tokens: &[Token]) -> Vec<&[Token]> {
    let mut out = Vec::new();
    let mut depth = 0;
    let mut start = 0;
    for (i, t) in tokens.iter().enumerate() {
        if t.is('(') {
            depth += 1;
        } else if t.is(')') {
            depth -= 1;
        } else if t.is(',') && depth == 0 {
            out.push(&tokens[start..i]);
            start = i + 1;
        }
    }
    out.push(&tokens[start..]);
    out
}

/// The names in a parenthesised list at `i` (`(a, b)`), and the index past it.
fn name_list(tokens: &[Token], i: usize) -> (Vec<String>, usize) {
    if !tokens.get(i).is_some_and(|t| t.is('(')) {
        return (Vec::new(), i);
    }
    let end = past_group(tokens, i);
    let inner = &tokens[i + 1..end.saturating_sub(1).max(i + 1)];
    let names = split_commas(inner)
        .into_iter()
        .filter_map(|part| name_at(part, 0).map(|(n, _)| n))
        .collect();
    (names, end)
}

fn skip_words(tokens: &[Token], mut i: usize, words: &[&str]) -> usize {
    while tokens
        .get(i)
        .is_some_and(|t| words.iter().any(|w| t.is_word(w)))
    {
        i += 1;
    }
    i
}

/// The statements of a SQL text, each its tokens.
fn statements(text: &str) -> Vec<Vec<Token>> {
    let mut out = Vec::new();
    let mut current = Vec::new();
    for token in tokenize(text) {
        if token.is(';') {
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
        } else {
            current.push(token);
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

fn sql_ops(text: &str) -> Vec<(Op, usize)> {
    let mut out = Vec::new();
    for statement in statements(text) {
        let line = super::line_at(text, statement[0].at);
        for op in statement_ops(&statement) {
            out.push((op, line));
        }
    }
    out
}

const COLUMN_CONSTRAINTS: &[&str] = &[
    "NOT",
    "NULL",
    "DEFAULT",
    "PRIMARY",
    "REFERENCES",
    "UNIQUE",
    "CHECK",
    "CONSTRAINT",
    "GENERATED",
    "COLLATE",
    "AUTO_INCREMENT",
    "AUTOINCREMENT",
    "IDENTITY",
    "ON",
    "COMMENT",
    "AS",
];

fn statement_ops(t: &[Token]) -> Vec<Op> {
    let first = t.first().and_then(Token::word_upper).unwrap_or_default();
    match first.as_str() {
        "CREATE" => create(t),
        "ALTER" if t.get(1).is_some_and(|x| x.is_word("TABLE")) => alter_table(t),
        "DROP"
            if t.get(1)
                .is_some_and(|x| x.is_word("TABLE") || x.is_word("VIEW")) =>
        {
            let i = skip_words(t, 2, &["IF", "EXISTS"]);
            split_commas(&t[i.min(t.len())..])
                .into_iter()
                .filter_map(|part| name_at(part, 0).map(|(name, _)| Op::DropTable { name }))
                .collect()
        }
        "INSERT" => insert_select(t).into_iter().collect(),
        "WITH" => {
            // `WITH … INSERT INTO t SELECT …`: the CTEs, then the insert.
            let (ctes, i) = with_clause(t, 0);
            match t.get(i).is_some_and(|x| x.is_word("INSERT")) {
                true => insert_select(&t[i..])
                    .map(|op| match op {
                        Op::InsertSelect { table, sources, .. } => Op::InsertSelect {
                            table,
                            sources,
                            ctes,
                        },
                        other => other,
                    })
                    .into_iter()
                    .collect(),
                false => Vec::new(),
            }
        }
        _ => Vec::new(),
    }
}

fn create(t: &[Token]) -> Vec<Op> {
    let mut i = skip_words(
        t,
        1,
        &[
            "OR",
            "REPLACE",
            "TEMP",
            "TEMPORARY",
            "UNLOGGED",
            "GLOBAL",
            "LOCAL",
        ],
    );
    let materialized = t.get(i).is_some_and(|x| x.is_word("MATERIALIZED"));
    if materialized {
        i += 1;
    }
    let what = t.get(i).and_then(Token::word_upper).unwrap_or_default();
    i = skip_words(t, i + 1, &["IF", "NOT", "EXISTS"]);
    let Some((name, mut i)) = name_at(t, i) else {
        return Vec::new();
    };
    match what.as_str() {
        "VIEW" => {
            if t.get(i).is_some_and(|x| x.is('(')) {
                i = past_group(t, i);
            }
            let at = t
                .iter()
                .skip(i)
                .position(|x| x.is_word("AS"))
                .map(|p| p + i + 1);
            let Some(at) = at else {
                return Vec::new();
            };
            let (ctes, sources) = query_sources(&t[at..]);
            vec![Op::Derive {
                name,
                view: true,
                sources,
                ctes,
            }]
        }
        "TABLE" => {
            if t.get(i).is_some_and(|x| x.is('(')) {
                let end = past_group(t, i);
                let inner = &t[i + 1..end.saturating_sub(1).max(i + 1)];
                let (columns, fks) = table_elements(inner);
                return vec![Op::CreateTable { name, columns, fks }];
            }
            // `CREATE TABLE t AS SELECT …`
            if t.get(i).is_some_and(|x| x.is_word("AS")) {
                let (ctes, sources) = query_sources(&t[i + 1..]);
                return vec![Op::Derive {
                    name,
                    view: false,
                    sources,
                    ctes,
                }];
            }
            Vec::new()
        }
        _ => Vec::new(),
    }
}

fn table_elements(inner: &[Token]) -> (Vec<Column>, Vec<ForeignKey>) {
    let mut columns: Vec<Column> = Vec::new();
    let mut fks = Vec::new();
    for part in split_commas(inner) {
        if part.is_empty() {
            continue;
        }
        let mut j = 0;
        let mut constraint = None;
        if part[0].is_word("CONSTRAINT") {
            constraint = name_at(part, 1).map(|(n, _)| n);
            j = name_at(part, 1).map_or(2, |(_, k)| k);
        }
        let head = part.get(j).and_then(Token::word_upper).unwrap_or_default();
        match head.as_str() {
            "PRIMARY" => {
                let (names, _) = name_list(part, j + 2);
                for c in &mut columns {
                    if names.iter().any(|n| n.eq_ignore_ascii_case(&c.name)) {
                        c.pk = true;
                    }
                }
            }
            "FOREIGN" => {
                let (cols, k) = name_list(part, j + 2);
                if part.get(k).is_some_and(|x| x.is_word("REFERENCES"))
                    && let Some((table, k)) = name_at(part, k + 1)
                {
                    let (references, _) = name_list(part, k);
                    fks.push(ForeignKey {
                        columns: cols,
                        table,
                        references,
                        constraint,
                    });
                }
            }
            "UNIQUE" | "CHECK" | "KEY" | "INDEX" | "FULLTEXT" | "SPATIAL" | "LIKE" | "EXCLUDE"
            | "PERIOD" => {}
            _ => {
                if let Some((column, fk)) = column_def(&part[j..]) {
                    fks.extend(fk);
                    columns.push(column);
                }
            }
        }
    }
    (columns, fks)
}

/// A column definition: its name, type and inline keys.
fn column_def(part: &[Token]) -> Option<(Column, Option<ForeignKey>)> {
    let (name, mut i) = name_at(part, 0)?;
    let mut ty = Vec::new();
    while let Some(tok) = part.get(i) {
        if let Some(w) = tok.word_upper()
            && COLUMN_CONSTRAINTS.contains(&w.as_str())
        {
            break;
        }
        match &tok.tok {
            Tok::Word(w) => ty.push(w.clone()),
            Tok::Ident(w) => ty.push(w.clone()),
            Tok::Punct('(') => {
                let end = past_group(part, i);
                let inside: Vec<String> = part[i + 1..end.saturating_sub(1).max(i + 1)]
                    .iter()
                    .filter_map(|t| match &t.tok {
                        Tok::Word(w) => Some(w.clone()),
                        Tok::Punct(',') => Some(",".into()),
                        _ => None,
                    })
                    .collect();
                if let Some(last) = ty.last_mut() {
                    last.push_str(&format!("({})", inside.join("").replace(',', ", ")));
                }
                i = end;
                continue;
            }
            Tok::Punct('[') | Tok::Punct(']') => {}
            _ => {}
        }
        i += 1;
    }
    let rest = &part[i.min(part.len())..];
    let pk = rest
        .windows(2)
        .any(|w| w[0].is_word("PRIMARY") && w[1].is_word("KEY"));
    let fk = rest
        .iter()
        .position(|t| t.is_word("REFERENCES"))
        .and_then(|r| {
            let (table, k) = name_at(rest, r + 1)?;
            let (references, _) = name_list(rest, k);
            Some(ForeignKey {
                columns: vec![name.clone()],
                table,
                references,
                constraint: None,
            })
        });
    Some((
        Column {
            name,
            ty: ty.join(" ").to_ascii_lowercase(),
            pk,
        },
        fk,
    ))
}

fn alter_table(t: &[Token]) -> Vec<Op> {
    let i = skip_words(t, 2, &["IF", "EXISTS", "ONLY"]);
    let Some((table, i)) = name_at(t, i) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for action in split_commas(&t[i.min(t.len())..]) {
        let head = action
            .first()
            .and_then(Token::word_upper)
            .unwrap_or_default();
        match head.as_str() {
            "ADD" => {
                let mut j = 1;
                let mut constraint = None;
                if action.get(j).is_some_and(|x| x.is_word("CONSTRAINT")) {
                    constraint = name_at(action, j + 1).map(|(n, _)| n);
                    j = name_at(action, j + 1).map_or(j + 2, |(_, k)| k);
                }
                if action.get(j).is_some_and(|x| x.is_word("FOREIGN")) {
                    let (cols, k) = name_list(action, j + 2);
                    if action.get(k).is_some_and(|x| x.is_word("REFERENCES"))
                        && let Some((target, k)) = name_at(action, k + 1)
                    {
                        let (references, _) = name_list(action, k);
                        out.push(Op::AddForeignKey {
                            table: table.clone(),
                            fk: ForeignKey {
                                columns: cols,
                                table: target,
                                references,
                                constraint,
                            },
                        });
                    }
                    continue;
                }
                if action.get(j).is_some_and(|x| {
                    ["PRIMARY", "UNIQUE", "CHECK", "INDEX", "KEY", "EXCLUDE"]
                        .iter()
                        .any(|w| x.is_word(w))
                }) {
                    continue;
                }
                let j = skip_words(action, j, &["COLUMN", "IF", "NOT", "EXISTS"]);
                if let Some((column, fk)) = column_def(&action[j.min(action.len())..]) {
                    out.push(Op::AddColumn {
                        table: table.clone(),
                        column,
                        fk,
                    });
                }
            }
            "DROP" => {
                if action.get(1).is_some_and(|x| x.is_word("CONSTRAINT")) {
                    let j = skip_words(action, 2, &["IF", "EXISTS"]);
                    if let Some((name, _)) = name_at(action, j) {
                        out.push(Op::DropConstraint {
                            table: table.clone(),
                            name,
                        });
                    }
                    continue;
                }
                let j = skip_words(action, 1, &["COLUMN", "IF", "EXISTS"]);
                if let Some((column, _)) = name_at(action, j) {
                    out.push(Op::DropColumn {
                        table: table.clone(),
                        column,
                    });
                }
            }
            "RENAME" => {
                if action.get(1).is_some_and(|x| x.is_word("TO"))
                    && let Some((to, _)) = name_at(action, 2)
                {
                    out.push(Op::RenameTable {
                        from: table.clone(),
                        to,
                    });
                    continue;
                }
                let j = skip_words(action, 1, &["COLUMN"]);
                if let Some((from, k)) = name_at(action, j)
                    && action.get(k).is_some_and(|x| x.is_word("TO"))
                    && let Some((to, _)) = name_at(action, k + 1)
                {
                    out.push(Op::RenameColumn {
                        table: table.clone(),
                        from,
                        to,
                    });
                }
            }
            _ => {}
        }
    }
    out
}

fn insert_select(t: &[Token]) -> Option<Op> {
    let i = skip_words(t, 1, &["INTO", "OR", "REPLACE", "IGNORE"]);
    let (table, mut i) = name_at(t, i)?;
    if t.get(i).is_some_and(|x| x.is('(')) {
        i = past_group(t, i);
    }
    let rest = &t[i.min(t.len())..];
    let query = rest
        .iter()
        .position(|x| x.is_word("SELECT") || x.is_word("WITH"))?;
    let (ctes, sources) = query_sources(&rest[query..]);
    (!sources.is_empty() || !ctes.is_empty()).then_some(Op::InsertSelect {
        table,
        sources,
        ctes,
    })
}

/// A `WITH` clause at `i`: each CTE and what it reads, and the index past
/// the clause.
fn with_clause(t: &[Token], i: usize) -> (Vec<(String, Vec<String>)>, usize) {
    let mut ctes = Vec::new();
    if !t.get(i).is_some_and(|x| x.is_word("WITH")) {
        return (ctes, i);
    }
    let mut j = skip_words(t, i + 1, &["RECURSIVE"]);
    while let Some((name, mut k)) = name_at(t, j) {
        if t.get(k).is_some_and(|x| x.is('(')) {
            k = past_group(t, k);
        }
        if !t.get(k).is_some_and(|x| x.is_word("AS")) {
            break;
        }
        k = skip_words(t, k + 1, &["NOT", "MATERIALIZED"]);
        if !t.get(k).is_some_and(|x| x.is('(')) {
            break;
        }
        let end = past_group(t, k);
        let body = &t[k + 1..end.saturating_sub(1).max(k + 1)];
        ctes.push((name, from_tables(body)));
        j = end;
        if !t.get(j).is_some_and(|x| x.is(',')) {
            break;
        }
        j += 1;
    }
    (ctes, j)
}

/// What a query reads: its CTEs (each with what it reads) and the tables its
/// own `FROM`s and `JOIN`s name — subqueries' included.
fn query_sources(t: &[Token]) -> (Vec<(String, Vec<String>)>, Vec<String>) {
    let (ctes, i) = with_clause(t, 0);
    (ctes, from_tables(&t[i.min(t.len())..]))
}

const CLAUSE_WORDS: &[&str] = &[
    "WHERE",
    "GROUP",
    "ORDER",
    "HAVING",
    "LIMIT",
    "UNION",
    "EXCEPT",
    "INTERSECT",
    "JOIN",
    "INNER",
    "LEFT",
    "RIGHT",
    "FULL",
    "CROSS",
    "OUTER",
    "ON",
    "USING",
    "NATURAL",
    "WINDOW",
    "OFFSET",
    "FETCH",
    "FOR",
    "RETURNING",
    "LATERAL",
    "SELECT",
    "VALUES",
    "SET",
];

/// The tables named after every `FROM` (and each `,` in its list) and
/// `JOIN` in `t`, in order, each once.
fn from_tables(t: &[Token]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |name: String| {
        if !out.iter().any(|o| o.eq_ignore_ascii_case(&name)) {
            out.push(name);
        }
    };
    let mut i = 0;
    while i < t.len() {
        let is_from = t[i].is_word("FROM");
        if !(is_from || t[i].is_word("JOIN")) {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        loop {
            if t.get(j).is_some_and(|x| x.is('(')) {
                // A subquery: its own FROMs are read as the loop reaches them.
                break;
            }
            let Some((name, k)) = name_at(t, j) else {
                break;
            };
            if CLAUSE_WORDS.iter().any(|w| name.eq_ignore_ascii_case(w)) {
                break;
            }
            push(name);
            j = k;
            // An alias: `AS x`, or a bare word that is no clause's.
            if t.get(j).is_some_and(|x| x.is_word("AS")) {
                j += 2;
            } else if let Some(w) = t.get(j).and_then(Token::word_upper)
                && !CLAUSE_WORDS.contains(&w.as_str())
            {
                j += 1;
            }
            if is_from && t.get(j).is_some_and(|x| x.is(',')) {
                j += 1;
                continue;
            }
            break;
        }
        i += 1;
    }
    out
}

// -- Rails ----------------------------------------------------------------------------

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
            super::ident(sym).to_string()
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
    let at = args.find(&format!("{option}:"))?;
    let rest = args[at + option.len() + 1..].trim_start();
    let value = rest.trim_start_matches(':').trim_start_matches(['"', '\'']);
    let v = super::ident(value);
    (!v.is_empty()).then(|| v.to_string())
}

fn rails_ops(text: &str) -> Vec<(Op, usize)> {
    let mut out = Vec::new();
    let mut table: Option<(String, Vec<Column>, Vec<ForeignKey>, usize)> = None;
    for (n, raw) in text.lines().enumerate() {
        let line = raw.trim();
        let call = |name: &str| -> Option<&str> {
            let rest = line.strip_prefix(name)?;
            let rest = rest.strip_prefix('(').unwrap_or(rest);
            rest.starts_with([' ', ':', '"', '\''])
                .then(|| rest.trim_end_matches(')'))
        };
        if let Some(open) = &mut table {
            if line == "end" {
                let (name, columns, fks, at) = table.take().unwrap_or_default();
                out.push((Op::CreateTable { name, columns, fks }, at));
                continue;
            }
            let Some(rest) = line.strip_prefix("t.") else {
                continue;
            };
            let kind = super::ident(rest);
            let args = rest[kind.len()..].trim();
            match kind {
                "timestamps" => {
                    for c in ["created_at", "updated_at"] {
                        open.1.push(Column {
                            name: c.into(),
                            ty: "datetime".into(),
                            pk: false,
                        });
                    }
                }
                "references" | "belongs_to" => {
                    for name in ruby_args(args) {
                        open.1.push(Column {
                            name: format!("{name}_id"),
                            ty: "bigint".into(),
                            pk: false,
                        });
                        if args.contains("foreign_key: true") || args.contains("foreign_key:true") {
                            open.2.push(ForeignKey {
                                columns: vec![format!("{name}_id")],
                                table: pluralize(&name),
                                references: vec!["id".into()],
                                constraint: None,
                            });
                        }
                    }
                }
                "index" | "check_constraint" => {}
                _ => {
                    for name in ruby_args(args) {
                        open.1.push(Column {
                            name,
                            ty: kind.into(),
                            pk: false,
                        });
                    }
                }
            }
            continue;
        }
        if let Some(args) = call("create_table") {
            if let Some(name) = ruby_args(args).into_iter().next() {
                let mut columns = Vec::new();
                if !args.contains("id: false") {
                    columns.push(Column {
                        name: "id".into(),
                        ty: "bigint".into(),
                        pk: true,
                    });
                }
                if line.ends_with(" do |t|") || line.contains(" do |t|") {
                    table = Some((name, columns, Vec::new(), n + 1));
                } else {
                    out.push((
                        Op::CreateTable {
                            name,
                            columns,
                            fks: Vec::new(),
                        },
                        n + 1,
                    ));
                }
            }
        } else if let Some(args) = call("add_column") {
            if let [t, c, ty, ..] = ruby_args(args).as_slice() {
                out.push((
                    Op::AddColumn {
                        table: t.clone(),
                        column: Column {
                            name: c.clone(),
                            ty: ty.clone(),
                            pk: false,
                        },
                        fk: None,
                    },
                    n + 1,
                ));
            }
        } else if let Some(args) = call("remove_column") {
            if let [t, c, ..] = ruby_args(args).as_slice() {
                out.push((
                    Op::DropColumn {
                        table: t.clone(),
                        column: c.clone(),
                    },
                    n + 1,
                ));
            }
        } else if let Some(args) = call("rename_column") {
            if let [t, from, to, ..] = ruby_args(args).as_slice() {
                out.push((
                    Op::RenameColumn {
                        table: t.clone(),
                        from: from.clone(),
                        to: to.clone(),
                    },
                    n + 1,
                ));
            }
        } else if let Some(args) = call("add_reference").or_else(|| call("add_belongs_to")) {
            if let [t, r, ..] = ruby_args(args).as_slice() {
                let fk = (args.contains("foreign_key: true") || args.contains("foreign_key:true"))
                    .then(|| ForeignKey {
                        columns: vec![format!("{r}_id")],
                        table: pluralize(r),
                        references: vec!["id".into()],
                        constraint: None,
                    });
                out.push((
                    Op::AddColumn {
                        table: t.clone(),
                        column: Column {
                            name: format!("{r}_id"),
                            ty: "bigint".into(),
                            pk: false,
                        },
                        fk,
                    },
                    n + 1,
                ));
            }
        } else if let Some(args) = call("add_foreign_key") {
            if let [from, to, ..] = ruby_args(args).as_slice() {
                let singular = to.strip_suffix('s').unwrap_or(to);
                let column =
                    ruby_option(args, "column").unwrap_or_else(|| format!("{singular}_id"));
                out.push((
                    Op::AddForeignKey {
                        table: from.clone(),
                        fk: ForeignKey {
                            columns: vec![column],
                            table: to.clone(),
                            references: vec!["id".into()],
                            constraint: None,
                        },
                    },
                    n + 1,
                ));
            }
        } else if let Some(args) = call("drop_table") {
            if let Some(name) = ruby_args(args).into_iter().next() {
                out.push((Op::DropTable { name }, n + 1));
            }
        } else if let Some(args) = call("rename_table")
            && let [from, to, ..] = ruby_args(args).as_slice()
        {
            out.push((
                Op::RenameTable {
                    from: from.clone(),
                    to: to.clone(),
                },
                n + 1,
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map<'a>(maps: &'a [ProjectMap], id: &str) -> &'a ProjectMap {
        maps.iter().find(|m| m.id == id).unwrap_or_else(|| {
            panic!(
                "{id} in {:?}",
                maps.iter().map(|m| &m.id).collect::<Vec<_>>()
            )
        })
    }

    fn node<'a>(map: &'a ProjectMap, label: &str) -> (usize, &'a clew_protocol::MapNode) {
        map.nodes
            .iter()
            .enumerate()
            .find(|(_, n)| n.label == label)
            .unwrap_or_else(|| {
                panic!(
                    "{label} in {:?}",
                    map.nodes.iter().map(|n| &n.label).collect::<Vec<_>>()
                )
            })
    }

    fn edges(map: &ProjectMap) -> Vec<(String, String, String)> {
        map.edges
            .iter()
            .map(|e| {
                (
                    map.nodes[e.from].label.clone(),
                    map.nodes[e.to].label.clone(),
                    e.label.clone(),
                )
            })
            .collect()
    }

    const MIGRATIONS: &[(&str, &str)] = &[
        (
            "db/migrations/001_users.up.sql",
            "-- users\nCREATE TABLE IF NOT EXISTS \"users\" (\n  id SERIAL PRIMARY KEY,\n  name VARCHAR(80) NOT NULL,\n  legacy TEXT\n);\n",
        ),
        ("db/migrations/001_users.down.sql", "DROP TABLE users;\n"),
        (
            "db/migrations/002_orders.sql",
            "/* orders */ CREATE TABLE public.orders (\n  id bigint,\n  user_id int REFERENCES users(id),\n  total numeric(10, 2) DEFAULT 0,\n  PRIMARY KEY (id)\n);\nCREATE TABLE items (id int PRIMARY KEY, order_id int, sku text,\n  CONSTRAINT items_order FOREIGN KEY (order_id) REFERENCES orders (id));\n-- +goose Down\nDROP TABLE orders;\n",
        ),
        (
            "db/migrations/003_changes.sql",
            "ALTER TABLE users ADD COLUMN email text, DROP COLUMN legacy;\nALTER TABLE items DROP CONSTRAINT items_order;\nALTER TABLE items ADD CONSTRAINT items_order2 FOREIGN KEY (order_id) REFERENCES orders(id);\nALTER TABLE users RENAME COLUMN name TO full_name;\nCREATE TABLE tmp (x int); DROP TABLE IF EXISTS tmp;\nCREATE FUNCTION f() RETURNS int AS $$ SELECT 1; CREATE TABLE ghost (x int); $$ LANGUAGE sql;\n",
        ),
        (
            "db/views.sql",
            "CREATE OR REPLACE VIEW big_orders AS\n  WITH totals AS (SELECT o.user_id, sum(o.total) t FROM orders o GROUP BY 1)\n  SELECT u.full_name, t.t FROM users u JOIN totals t ON t.user_id = u.id;\nCREATE MATERIALIZED VIEW report AS SELECT * FROM big_orders, items i;\nINSERT INTO archive (id) SELECT id FROM orders WHERE total > 100;\nCREATE TABLE archive AS SELECT * FROM orders;\n",
        ),
        (
            "app/repo.py",
            "def users():\n    return db.query(\"SELECT * FROM users WHERE id = %s\", 1)\n\ndef save(o):\n    db.run('INSERT INTO orders (id) VALUES (1)')\n    db.run(\"UPDATE nothing_here SET x = 1\")\n",
        ),
    ];

    #[test]
    fn the_schema_is_the_migrations_replayed() {
        let maps = maps(&Corpus::of(MIGRATIONS));
        let schema = map(&maps, "sql.schema");
        assert!(schema.is_well_formed());
        let (_, users) = node(schema, "users");
        assert_eq!(users.file, "db/migrations/001_users.up.sql");
        assert_eq!(users.line, 2);
        assert_eq!(
            users.detail, "id serial (primary key)\nfull_name varchar(80)\nemail text",
            "renamed, added, the dropped column gone"
        );
        let (_, orders) = node(schema, "public.orders");
        assert_eq!(
            orders.detail,
            "id bigint (primary key)\nuser_id int → users\ntotal numeric(10, 2)"
        );
        assert!(
            schema
                .nodes
                .iter()
                .all(|n| n.label != "tmp" && n.label != "ghost"),
            "dropped, or in a function body"
        );
        let e = edges(schema);
        assert!(
            e.contains(&("public.orders".into(), "users".into(), "user_id".into())),
            "{e:?}"
        );
        assert!(
            e.contains(&("items".into(), "public.orders".into(), "order_id".into())),
            "the re-added key: {e:?}"
        );
        assert_eq!(
            e.iter().filter(|(f, _, _)| f == "items").count(),
            1,
            "the dropped constraint gone: {e:?}"
        );
        let (_, view) = node(schema, "big_orders");
        assert_eq!(view.kind, "view");
        assert!(
            e.contains(&("report".into(), "big_orders".into(), "reads".into())),
            "{e:?}"
        );
        assert!(
            e.contains(&("big_orders".into(), "users".into(), "reads".into())),
            "{e:?}"
        );
        assert!(
            !e.iter().any(|(_, to, _)| to == "totals"),
            "a CTE is no table: {e:?}"
        );
    }

    #[test]
    fn history_lists_each_migration_and_what_it_changes() {
        let maps = maps(&Corpus::of(MIGRATIONS));
        let history = map(&maps, "sql.history");
        let labels: Vec<&str> = history
            .nodes
            .iter()
            .filter(|n| n.kind == "migration")
            .map(|n| n.label.as_str())
            .collect();
        assert_eq!(
            labels,
            [
                "001_users.up.sql",
                "002_orders.sql",
                "003_changes.sql",
                "views.sql"
            ],
            "in order, the down migration left out"
        );
        let (_, third) = node(history, "003_changes.sql");
        assert!(
            third.detail.contains("+ column users.email text (line 1)"),
            "{}",
            third.detail
        );
        assert!(
            third.detail.contains("- column users.legacy"),
            "{}",
            third.detail
        );
        assert!(
            third.detail.contains("~ column users.name → full_name"),
            "{}",
            third.detail
        );
        let (_, second) = node(history, "002_orders.sql");
        assert!(
            !second.detail.contains("- table orders"),
            "the goose Down part left out: {}",
            second.detail
        );
        let e = edges(history);
        assert!(e.contains(&(
            "001_users.up.sql".into(),
            "002_orders.sql".into(),
            "then".into()
        )));
        assert!(e.contains(&("003_changes.sql".into(), "users".into(), "alters".into())));
        assert!(e.contains(&("003_changes.sql".into(), "tmp".into(), "drops".into())));
    }

    #[test]
    fn lineage_follows_views_inserts_and_ctes() {
        let maps = maps(&Corpus::of(MIGRATIONS));
        let lineage = map(&maps, "sql.lineage");
        let e = edges(lineage);
        for want in [
            ("orders", "totals", "select"),
            ("totals", "big_orders", "with"),
            ("users", "big_orders", "select"),
            ("big_orders", "report", "select"),
            ("items", "report", "select"),
            ("orders", "archive", "insert … select"),
            ("orders", "archive", "select"),
        ] {
            assert!(
                e.contains(&(want.0.into(), want.1.into(), want.2.into())),
                "{want:?} in {e:?}"
            );
        }
        assert_eq!(node(lineage, "totals").1.kind, "cte");
    }

    #[test]
    fn code_is_linked_to_the_tables_its_sql_names() {
        let maps = maps(&Corpus::of(MIGRATIONS));
        let usage = map(&maps, "sql.usage");
        let e = edges(usage);
        assert!(
            e.contains(&("app/repo.py".into(), "users".into(), "reads".into())),
            "{e:?}"
        );
        assert!(
            e.contains(&(
                "app/repo.py".into(),
                "public.orders".into(),
                "writes".into()
            )),
            "{e:?}"
        );
        assert!(
            !e.iter().any(|(_, t, _)| t == "nothing_here"),
            "only known tables: {e:?}"
        );
        let (_, file) = node(usage, "app/repo.py");
        assert_eq!(file.line, 2);
    }

    #[test]
    fn rails_schema_and_migrations_are_read() {
        let corpus = Corpus::of(&[
            (
                "db/migrate/20240101000000_create_users.rb",
                "class CreateUsers < ActiveRecord::Migration[7.1]\n  def change\n    create_table :users do |t|\n      t.string :name\n      t.timestamps\n    end\n  end\nend\n",
            ),
            (
                "db/migrate/20240201000000_create_posts.rb",
                "class CreatePosts < ActiveRecord::Migration[7.1]\n  def change\n    create_table :posts do |t|\n      t.references :user, foreign_key: true\n      t.text :body\n    end\n    add_column :users, :email, :string\n    add_reference :posts, :category, foreign_key: true\n    add_foreign_key :posts, :users, column: :editor_id\n  end\nend\n",
            ),
            (
                "app/models/user.rb",
                "class User < ApplicationRecord\nend\n",
            ),
        ]);
        let maps = maps(&corpus);
        let schema = map(&maps, "sql.schema");
        let (_, users) = node(schema, "users");
        assert_eq!(
            users.detail,
            "id bigint (primary key)\nname string\ncreated_at datetime\nupdated_at datetime\nemail string"
        );
        let e = edges(schema);
        assert!(
            e.contains(&("posts".into(), "users".into(), "user_id".into())),
            "{e:?}"
        );
        assert!(
            e.contains(&("posts".into(), "categories".into(), "category_id".into())),
            "{e:?}"
        );
        assert!(
            e.contains(&("posts".into(), "users".into(), "editor_id".into())),
            "{e:?}"
        );
        let history = map(&maps, "sql.history");
        assert_eq!(
            history
                .nodes
                .iter()
                .filter(|n| n.kind == "migration")
                .count(),
            2
        );
        assert_eq!(pluralize("category"), "categories");
        assert_eq!(pluralize("box"), "boxes");
        assert_eq!(pluralize("day"), "days");
    }

    #[test]
    fn sql_is_tokenized_past_strings_comments_and_bodies() {
        let tokens = tokenize("SELECT 'a;b' -- c;\n, \"Q\".x /* ; */ $f$ ; $f$ [id]");
        let kinds: Vec<&Tok> = tokens.iter().map(|t| &t.tok).collect();
        assert_eq!(
            kinds,
            [
                &Tok::Word("SELECT".into()),
                &Tok::Str,
                &Tok::Punct(','),
                &Tok::Ident("Q".into()),
                &Tok::Punct('.'),
                &Tok::Word("x".into()),
                &Tok::Str,
                &Tok::Ident("id".into()),
            ]
        );
        assert_eq!(statements("a; b;; c").len(), 3);
        assert!(
            maps(&Corpus::of(&[("q.sql", "SELECT 1;")])).is_empty(),
            "no schema, no maps"
        );
    }
}
