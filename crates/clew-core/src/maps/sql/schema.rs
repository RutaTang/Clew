//! A database's schema as its migrations leave it: each change replayed in
//! order, the way the database itself would make it — a table created
//! `IF NOT EXISTS` once, a foreign key following the table it references
//! through a rename, a column's keys and indexes gone with it.

use std::collections::{BTreeMap, BTreeSet};

use super::ddl::{
    Action, Change, Column, CommentOn, Constraint, ForeignKey, Index, Kind, Op, Policy, QName,
    Trigger,
};

#[derive(Debug, Clone, Default)]
pub(crate) struct Table {
    pub name: QName,
    pub columns: Vec<Column>,
    /// Its primary key, unique constraints, foreign keys and checks.
    pub constraints: Vec<Constraint>,
    pub indexes: Vec<Index>,
    /// Whether row-level security is on.
    pub rls: bool,
    pub policies: Vec<Policy>,
    pub triggers: Vec<Trigger>,
    pub comment: Option<String>,
    /// What a `CREATE TABLE … AS` made it from.
    pub derived_from: Vec<QName>,
    pub file: String,
    pub line: usize,
}

impl Table {
    /// Its primary key's columns.
    pub fn primary_key(&self) -> &[String] {
        self.constraints
            .iter()
            .find_map(|c| match &c.kind {
                Kind::PrimaryKey(cols) => Some(cols.as_slice()),
                _ => None,
            })
            .unwrap_or(&[])
    }

    pub fn foreign_keys(&self) -> impl Iterator<Item = &ForeignKey> {
        self.constraints.iter().filter_map(|c| match &c.kind {
            Kind::ForeignKey(fk) => Some(fk),
            _ => None,
        })
    }

    /// Whether `column` alone is unique: a unique constraint or a unique
    /// index (one over every row) on it and nothing else.
    pub fn is_unique(&self, column: &str) -> bool {
        let alone = |cols: &[String]| cols.len() == 1 && cols[0].eq_ignore_ascii_case(column);
        self.constraints
            .iter()
            .any(|c| matches!(&c.kind, Kind::Unique(cols) if alone(cols)))
            || self
                .indexes
                .iter()
                .any(|i| i.unique && !i.partial && alone(&i.columns))
    }

    /// Whether an index (or a key, which has one) starts with `column`.
    pub fn is_indexed(&self, column: &str) -> bool {
        let leads = |cols: &[String]| cols.first().is_some_and(|c| c.eq_ignore_ascii_case(column));
        self.indexes.iter().any(|i| leads(&i.columns))
            || self.constraints.iter().any(|c| match &c.kind {
                Kind::PrimaryKey(cols) | Kind::Unique(cols) => leads(cols),
                _ => false,
            })
    }

    fn column_mut(&mut self, name: &str) -> Option<&mut Column> {
        self.columns
            .iter_mut()
            .find(|c| c.name.eq_ignore_ascii_case(name))
    }

    /// `constraints` added, each where it was written; a primary key's
    /// columns are not null.
    fn add_constraints(&mut self, constraints: Vec<Constraint>, file: &str, search: Option<&str>) {
        for mut c in constraints {
            c.file = file.to_string();
            if c.name.is_none() {
                c.name = default_name(&self.name.name, &c.kind);
            }
            match &mut c.kind {
                Kind::PrimaryKey(cols) => {
                    for col in cols.iter() {
                        if let Some(column) = self.column_mut(col) {
                            column.not_null = true;
                        }
                    }
                    self.constraints
                        .retain(|old| !matches!(old.kind, Kind::PrimaryKey(_)));
                }
                Kind::ForeignKey(fk) => {
                    fk.search = search.map(str::to_string);
                }
                Kind::Index(index) => {
                    let mut index = index.clone();
                    index.file = file.to_string();
                    self.indexes.push(index);
                    continue;
                }
                _ => {}
            }
            self.constraints.push(c);
        }
    }
}

/// The name PostgreSQL gives a constraint written without one
/// (`orders_pkey`, `orders_user_id_fkey`), which a later `DROP CONSTRAINT`
/// names it by.
fn default_name(table: &str, kind: &Kind) -> Option<String> {
    let cols = |c: &[String]| c.join("_");
    let table = table.to_lowercase();
    match kind {
        Kind::PrimaryKey(_) => Some(format!("{table}_pkey")),
        Kind::Unique(c) => Some(format!("{table}_{}_key", cols(c))),
        Kind::ForeignKey(fk) => Some(format!("{table}_{}_fkey", cols(&fk.columns))),
        Kind::Check(_) => Some(format!("{table}_check")),
        Kind::Index(_) => None,
    }
    .map(|n| n.to_lowercase())
}

#[derive(Debug, Clone, Default)]
pub(crate) struct View {
    pub name: QName,
    pub materialized: bool,
    pub columns: Vec<String>,
    /// What it reads, through its CTEs.
    pub sources: Vec<QName>,
    pub search: Option<String>,
    pub comment: Option<String>,
    pub file: String,
    pub line: usize,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Enum {
    pub name: QName,
    pub values: Vec<String>,
    pub file: String,
    pub line: usize,
}

/// A file's changes, in order, each with its line.
#[derive(Debug, Clone)]
pub(crate) struct Changes {
    pub rel: String,
    pub ops: Vec<(Op, usize)>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Schema {
    pub tables: BTreeMap<String, Table>,
    pub views: BTreeMap<String, View>,
    pub enums: BTreeMap<String, Enum>,
    /// The schemas made (`CREATE SCHEMA`) besides the default one.
    pub schemas: BTreeSet<String>,
}

/// What a name resolves to among `keys`: the name in the schema a
/// `search_path` named, then in the default schema, then the one object of
/// that name in any schema.
fn resolve<'a, T>(
    items: &'a BTreeMap<String, T>,
    name: &QName,
    search: Option<&str>,
    name_of: impl Fn(&T) -> &QName,
) -> Option<&'a str> {
    if name.schema.is_some() {
        return items.get_key_value(&name.key()).map(|(k, _)| k.as_str());
    }
    if let Some(s) = search
        && let Some((k, _)) = items.get_key_value(&name.in_schema(Some(s)).key())
    {
        return Some(k);
    }
    if let Some((k, _)) = items.get_key_value(&name.key()) {
        return Some(k);
    }
    let mut found = items
        .iter()
        .filter(|(_, item)| name_of(item).name.eq_ignore_ascii_case(&name.name));
    match (found.next(), found.next()) {
        (Some((k, _)), None) => Some(k),
        _ => None,
    }
}

impl Schema {
    pub fn table_key(&self, name: &QName, search: Option<&str>) -> Option<String> {
        resolve(&self.tables, name, search, |t| &t.name).map(str::to_string)
    }

    pub fn view_key(&self, name: &QName, search: Option<&str>) -> Option<String> {
        resolve(&self.views, name, search, |v| &v.name).map(str::to_string)
    }

    pub fn enum_key(&self, name: &QName, search: Option<&str>) -> Option<String> {
        resolve(&self.enums, name, search, |e| &e.name).map(str::to_string)
    }

    /// The table a foreign key references.
    pub fn target(&self, fk: &ForeignKey) -> Option<&Table> {
        let key = self.table_key(&fk.table, fk.search.as_deref())?;
        self.tables.get(&key)
    }

    /// Every change of `changes` made, in order.
    pub fn apply(&mut self, changes: &Changes) {
        // A `SET search_path` holds to the end of its file.
        let mut search: Option<String> = None;
        for (op, line) in &changes.ops {
            let file = changes.rel.as_str();
            let line = *line;
            match op.clone() {
                Op::SearchPath(s) => search = s,
                Op::CreateSchema(s) => {
                    if !["public", "dbo", "main"]
                        .iter()
                        .any(|d| s.eq_ignore_ascii_case(d))
                    {
                        self.schemas.insert(s);
                    }
                }
                Op::DropSchema(names) => {
                    for s in names {
                        let gone = |q: &QName| {
                            q.schema
                                .as_deref()
                                .is_some_and(|x| x.eq_ignore_ascii_case(&s))
                        };
                        self.tables.retain(|_, t| !gone(&t.name));
                        self.views.retain(|_, v| !gone(&v.name));
                        self.enums.retain(|_, e| !gone(&e.name));
                        self.schemas.retain(|x| !x.eq_ignore_ascii_case(&s));
                    }
                }
                Op::RenameSchema { from, to } => self.rename_schema(&from, &to),
                Op::CreateTable {
                    name,
                    columns,
                    constraints,
                    copies,
                    comment,
                    if_not_exists,
                } => {
                    let name = name.in_schema(search.as_deref());
                    if if_not_exists && self.table_key(&name, None).is_some() {
                        continue;
                    }
                    let mut table = Table {
                        name: name.clone(),
                        comment,
                        file: file.to_string(),
                        line,
                        ..Default::default()
                    };
                    for copy in &copies {
                        if let Some(from) = self
                            .table_key(copy, search.as_deref())
                            .and_then(|k| self.tables.get(&k))
                        {
                            table.columns.extend(from.columns.iter().cloned());
                        }
                    }
                    for mut column in columns {
                        column.file = file.to_string();
                        table
                            .columns
                            .retain(|c| !c.name.eq_ignore_ascii_case(&column.name));
                        table.columns.push(column);
                    }
                    table.add_constraints(constraints, file, search.as_deref());
                    self.views.remove(&name.key());
                    self.tables.insert(name.key(), table);
                }
                Op::Derive {
                    name,
                    view,
                    materialized,
                    columns,
                    sources,
                    ctes,
                } => {
                    let name = name.in_schema(search.as_deref());
                    let sources = through_ctes(&sources, &ctes);
                    if view {
                        self.views.insert(
                            name.key(),
                            View {
                                name,
                                materialized,
                                columns,
                                sources,
                                search: search.clone(),
                                comment: None,
                                file: file.to_string(),
                                line,
                            },
                        );
                    } else {
                        self.tables.insert(
                            name.key(),
                            Table {
                                name,
                                derived_from: sources,
                                file: file.to_string(),
                                line,
                                ..Default::default()
                            },
                        );
                    }
                }
                Op::InsertSelect { .. } => {}
                Op::AlterTable { table, actions } => {
                    let Some(key) = self.table_key(&table, search.as_deref()) else {
                        continue;
                    };
                    for action in actions {
                        self.alter(&key, action, file, search.as_deref());
                    }
                }
                Op::DropTable(names) => {
                    for name in names {
                        if let Some(key) = self.table_key(&name, search.as_deref()) {
                            self.drop_table(&key);
                        }
                    }
                }
                Op::DropView(names) => {
                    for name in names {
                        if let Some(key) = self.view_key(&name, search.as_deref()) {
                            self.views.remove(&key);
                        }
                    }
                }
                Op::RenameView { from, to } => {
                    if let Some(key) = self.view_key(&from, search.as_deref())
                        && let Some(mut v) = self.views.remove(&key)
                    {
                        v.name.name = to;
                        self.views.insert(v.name.key(), v);
                    }
                }
                Op::CreateIndex { table, mut index } => {
                    if let Some(t) = self
                        .table_key(&table, search.as_deref())
                        .and_then(|k| self.tables.get_mut(&k))
                    {
                        index.file = file.to_string();
                        if let Some(name) = &index.name {
                            t.indexes.retain(|i| {
                                !i.name
                                    .as_deref()
                                    .is_some_and(|n| n.eq_ignore_ascii_case(name))
                            });
                        }
                        t.indexes.push(index);
                    }
                }
                Op::DropIndex { names, table } => {
                    let only = table.and_then(|t| self.table_key(&t, search.as_deref()));
                    for name in names {
                        for (key, t) in self.tables.iter_mut() {
                            if only.as_deref().is_some_and(|o| o != key) {
                                continue;
                            }
                            let before = t.indexes.len();
                            t.indexes.retain(|i| {
                                !i.name
                                    .as_deref()
                                    .is_some_and(|n| n.eq_ignore_ascii_case(&name.name))
                            });
                            if t.indexes.len() != before {
                                break;
                            }
                        }
                    }
                }
                Op::RenameIndex { from, to } => {
                    for t in self.tables.values_mut() {
                        if let Some(i) = t.indexes.iter_mut().find(|i| {
                            i.name
                                .as_deref()
                                .is_some_and(|n| n.eq_ignore_ascii_case(&from.name))
                        }) {
                            i.name = Some(to.clone());
                            break;
                        }
                    }
                }
                Op::CreateEnum { name, values } => {
                    let name = name.in_schema(search.as_deref());
                    self.enums.insert(
                        name.key(),
                        Enum {
                            name,
                            values,
                            file: file.to_string(),
                            line,
                        },
                    );
                }
                Op::AddEnumValue {
                    name,
                    value,
                    before,
                    after,
                } => {
                    if let Some(e) = self
                        .enum_key(&name, search.as_deref())
                        .and_then(|k| self.enums.get_mut(&k))
                        && !e.values.contains(&value)
                    {
                        let at = match (before, after) {
                            (Some(b), _) => e.values.iter().position(|v| *v == b),
                            (_, Some(a)) => e.values.iter().position(|v| *v == a).map(|p| p + 1),
                            _ => None,
                        };
                        match at {
                            Some(at) => e.values.insert(at, value),
                            None => e.values.push(value),
                        }
                    }
                }
                Op::RenameEnumValue { name, from, to } => {
                    if let Some(e) = self
                        .enum_key(&name, search.as_deref())
                        .and_then(|k| self.enums.get_mut(&k))
                    {
                        for v in &mut e.values {
                            if *v == from {
                                *v = to.clone();
                            }
                        }
                    }
                }
                Op::RenameType { from, to } => {
                    if let Some(key) = self.enum_key(&from, search.as_deref())
                        && let Some(mut e) = self.enums.remove(&key)
                    {
                        e.name.name = to;
                        self.enums.insert(e.name.key(), e);
                    }
                }
                Op::DropType(names) => {
                    for name in names {
                        if let Some(key) = self.enum_key(&name, search.as_deref()) {
                            self.enums.remove(&key);
                        }
                    }
                }
                Op::CreatePolicy { table, mut policy } => {
                    if let Some(t) = self
                        .table_key(&table, search.as_deref())
                        .and_then(|k| self.tables.get_mut(&k))
                    {
                        policy.file = file.to_string();
                        t.policies
                            .retain(|p| !p.name.eq_ignore_ascii_case(&policy.name));
                        t.policies.push(policy);
                    }
                }
                Op::DropPolicy { table, name } => {
                    if let Some(t) = self
                        .table_key(&table, search.as_deref())
                        .and_then(|k| self.tables.get_mut(&k))
                    {
                        t.policies.retain(|p| !p.name.eq_ignore_ascii_case(&name));
                    }
                }
                Op::RenamePolicy { table, from, to } => {
                    if let Some(t) = self
                        .table_key(&table, search.as_deref())
                        .and_then(|k| self.tables.get_mut(&k))
                    {
                        for p in &mut t.policies {
                            if p.name.eq_ignore_ascii_case(&from) {
                                p.name = to.clone();
                            }
                        }
                    }
                }
                Op::CreateTrigger { table, mut trigger } => {
                    if let Some(t) = self
                        .table_key(&table, search.as_deref())
                        .and_then(|k| self.tables.get_mut(&k))
                    {
                        trigger.file = file.to_string();
                        t.triggers
                            .retain(|x| !x.name.eq_ignore_ascii_case(&trigger.name));
                        t.triggers.push(trigger);
                    }
                }
                Op::DropTrigger { table, name } => {
                    let only = table.and_then(|t| self.table_key(&t, search.as_deref()));
                    for (key, t) in self.tables.iter_mut() {
                        if only.as_deref().is_none_or(|o| o == key) {
                            t.triggers.retain(|x| !x.name.eq_ignore_ascii_case(&name));
                        }
                    }
                }
                Op::Comment { on, text } => match on {
                    CommentOn::Table(name) => {
                        if let Some(key) = self.table_key(&name, search.as_deref()) {
                            if let Some(t) = self.tables.get_mut(&key) {
                                t.comment = text;
                            }
                        } else if let Some(key) = self.view_key(&name, search.as_deref())
                            && let Some(v) = self.views.get_mut(&key)
                        {
                            v.comment = text;
                        }
                    }
                    CommentOn::Column(name, column) => {
                        if let Some(c) = self
                            .table_key(&name, search.as_deref())
                            .and_then(|k| self.tables.get_mut(&k))
                            .and_then(|t| t.column_mut(&column))
                        {
                            c.comment = text;
                        }
                    }
                },
            }
        }
    }

    fn alter(&mut self, key: &str, action: Action, file: &str, search: Option<&str>) {
        match action {
            Action::RenameTo(to) => {
                let Some(t) = self.tables.get(key) else {
                    return;
                };
                let mut name = t.name.clone();
                name.name = to;
                self.move_table(key, name);
            }
            Action::SetSchema(s) => {
                let Some(t) = self.tables.get(key) else {
                    return;
                };
                let name = QName::from_parts(vec![s, t.name.name.clone()]).unwrap_or_default();
                self.move_table(key, name);
            }
            Action::RenameColumn { from, to } => {
                let Some(t) = self.tables.get_mut(key) else {
                    return;
                };
                rename_in(t, &from, &to);
                // The foreign keys that reference the column, from any table.
                for at in self.referencing(key) {
                    if let Some(fk) = self.fk_mut(&at) {
                        for r in &mut fk.references {
                            if r.eq_ignore_ascii_case(&from) {
                                *r = to.clone();
                            }
                        }
                    }
                }
            }
            other => {
                let Some(t) = self.tables.get_mut(key) else {
                    return;
                };
                alter_in(t, other, file, search);
            }
        }
    }

    /// The table at `key` known as `to` from now on, what references it
    /// following it.
    fn move_table(&mut self, key: &str, to: QName) {
        for at in self.referencing(key) {
            if let Some(fk) = self.fk_mut(&at) {
                fk.table = to.clone();
                fk.search = None;
            }
        }
        if let Some(mut t) = self.tables.remove(key) {
            t.name = to;
            self.tables.insert(t.name.key(), t);
        }
    }

    /// The table at `key` gone, and the foreign keys that referenced it
    /// (PostgreSQL's `CASCADE`; without it the drop is refused).
    fn drop_table(&mut self, key: &str) {
        let mut gone = self.referencing(key);
        // From the last, so the places of the rest stay put.
        gone.sort_by_key(|g| std::cmp::Reverse(g.1));
        for (k, i) in gone {
            if k != key
                && let Some(t) = self.tables.get_mut(&k)
                && i < t.constraints.len()
            {
                t.constraints.remove(i);
            }
        }
        self.tables.remove(key);
    }

    /// The foreign keys that reference the table at `key`: each one's
    /// table and its place among that table's constraints.
    fn referencing(&self, key: &str) -> Vec<(String, usize)> {
        let mut out = Vec::new();
        for (k, t) in &self.tables {
            for (i, c) in t.constraints.iter().enumerate() {
                if let Kind::ForeignKey(fk) = &c.kind
                    && self.table_key(&fk.table, fk.search.as_deref()).as_deref() == Some(key)
                {
                    out.push((k.clone(), i));
                }
            }
        }
        out
    }

    fn fk_mut(&mut self, (k, i): &(String, usize)) -> Option<&mut ForeignKey> {
        match &mut self.tables.get_mut(k)?.constraints.get_mut(*i)?.kind {
            Kind::ForeignKey(fk) => Some(fk),
            _ => None,
        }
    }

    fn rename_schema(&mut self, from: &str, to: &str) {
        let moved = |q: &mut QName| {
            if q.schema
                .as_deref()
                .is_some_and(|s| s.eq_ignore_ascii_case(from))
            {
                q.schema = Some(to.to_string());
            }
        };
        let tables = std::mem::take(&mut self.tables);
        for (_, mut t) in tables {
            moved(&mut t.name);
            for c in &mut t.constraints {
                if let Kind::ForeignKey(fk) = &mut c.kind {
                    moved(&mut fk.table);
                }
            }
            self.tables.insert(t.name.key(), t);
        }
        let views = std::mem::take(&mut self.views);
        for (_, mut v) in views {
            moved(&mut v.name);
            self.views.insert(v.name.key(), v);
        }
        let enums = std::mem::take(&mut self.enums);
        for (_, mut e) in enums {
            moved(&mut e.name);
            self.enums.insert(e.name.key(), e);
        }
        if self.schemas.remove(from) {
            self.schemas.insert(to.to_string());
        }
    }
}

/// The column `from` known as `to` in its table: its keys and indexes too.
fn rename_in(t: &mut Table, from: &str, to: &str) {
    let rename = |c: &mut String| {
        if c.eq_ignore_ascii_case(from) {
            *c = to.to_string();
        }
    };
    if let Some(c) = t.column_mut(from) {
        c.name = to.to_string();
    }
    for c in &mut t.constraints {
        match &mut c.kind {
            Kind::PrimaryKey(cols) | Kind::Unique(cols) => cols.iter_mut().for_each(rename),
            Kind::ForeignKey(fk) => fk.columns.iter_mut().for_each(rename),
            _ => {}
        }
    }
    for i in &mut t.indexes {
        i.columns.iter_mut().for_each(rename);
    }
}

fn alter_in(t: &mut Table, action: Action, file: &str, search: Option<&str>) {
    match action {
        Action::AddColumn {
            mut column,
            constraints,
            if_not_exists,
        } => {
            column.file = file.to_string();
            match t
                .columns
                .iter()
                .position(|c| c.name.eq_ignore_ascii_case(&column.name))
            {
                Some(_) if if_not_exists => return,
                Some(at) => t.columns[at] = column,
                None => t.columns.push(column),
            }
            t.add_constraints(constraints, file, search);
        }
        Action::DropColumn(name) => {
            t.columns.retain(|c| !c.name.eq_ignore_ascii_case(&name));
            let has = |cols: &[String]| cols.iter().any(|c| c.eq_ignore_ascii_case(&name));
            t.constraints.retain(|c| match &c.kind {
                Kind::PrimaryKey(cols) | Kind::Unique(cols) => !has(cols),
                Kind::ForeignKey(fk) => !has(&fk.columns),
                _ => true,
            });
            t.indexes.retain(|i| !has(&i.columns));
        }
        Action::AlterColumn { column, change } => {
            let Some(c) = t.column_mut(&column) else {
                return;
            };
            match change {
                Change::Type(ty) => c.ty = ty,
                Change::NotNull(on) => c.not_null = on,
                Change::Default(value) => {
                    if value
                        .as_deref()
                        .is_some_and(|v| v.to_ascii_lowercase().starts_with("nextval("))
                    {
                        c.identity = true;
                    }
                    c.default = value;
                }
                Change::Identity(on) => {
                    c.identity = on;
                    if on {
                        c.not_null = true;
                    }
                }
            }
        }
        Action::Redefine {
            from,
            mut column,
            constraints,
        } => {
            column.file = file.to_string();
            let to = column.name.clone();
            match t
                .columns
                .iter()
                .position(|c| c.name.eq_ignore_ascii_case(&from))
            {
                Some(at) => {
                    // MySQL's MODIFY keeps what it does not say only for the
                    // comment's sake: a column defined again is that column.
                    if column.comment.is_none() {
                        column.comment = t.columns[at].comment.clone();
                    }
                    t.columns[at] = column;
                }
                None => t.columns.push(column),
            }
            if !from.eq_ignore_ascii_case(&to) {
                rename_in(t, &from, &to);
            }
            t.add_constraints(constraints, file, search);
        }
        Action::AddConstraint(c) => t.add_constraints(vec![c], file, search),
        Action::DropConstraint(name) => {
            let named =
                |n: &Option<String>| n.as_deref().is_some_and(|n| n.eq_ignore_ascii_case(&name));
            t.constraints.retain(|c| !named(&c.name));
            t.indexes.retain(|i| !named(&i.name));
        }
        Action::RenameConstraint { from, to } => {
            for c in &mut t.constraints {
                if c.name
                    .as_deref()
                    .is_some_and(|n| n.eq_ignore_ascii_case(&from))
                {
                    c.name = Some(to.clone());
                }
            }
            for i in &mut t.indexes {
                if i.name
                    .as_deref()
                    .is_some_and(|n| n.eq_ignore_ascii_case(&from))
                {
                    i.name = Some(to.clone());
                }
            }
        }
        Action::DropPrimaryKey => {
            t.constraints
                .retain(|c| !matches!(c.kind, Kind::PrimaryKey(_)));
        }
        Action::DropIndex(name) => {
            let named =
                |n: &Option<String>| n.as_deref().is_some_and(|n| n.eq_ignore_ascii_case(&name));
            t.indexes.retain(|i| !named(&i.name));
            t.constraints
                .retain(|c| !(named(&c.name) && matches!(c.kind, Kind::Unique(_))));
        }
        Action::RowLevelSecurity(on) => t.rls = on,
        Action::RenameTo(_) | Action::SetSchema(_) | Action::RenameColumn { .. } => {}
    }
}

/// What a query reads at bottom: its sources, each CTE among them replaced
/// by what the CTE reads, in turn.
pub(crate) fn through_ctes(sources: &[QName], ctes: &[(String, Vec<QName>)]) -> Vec<QName> {
    let mut out: Vec<QName> = Vec::new();
    let mut seen = BTreeSet::new();
    let mut queue: Vec<QName> = sources.iter().rev().cloned().collect();
    while let Some(source) = queue.pop() {
        if !seen.insert(source.key()) {
            continue;
        }
        let cte = (source.schema.is_none())
            .then(|| {
                ctes.iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case(&source.name))
            })
            .flatten();
        match cte {
            Some((_, reads)) => queue.extend(reads.iter().rev().cloned()),
            None => out.push(source),
        }
    }
    out
}
