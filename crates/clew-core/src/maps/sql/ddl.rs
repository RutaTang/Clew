//! What each statement of a SQL file does to the schema: the tables,
//! columns, keys, indexes, enums, policies and triggers it creates, changes
//! and drops — written for PostgreSQL, MySQL, SQLite and SQL Server alike.

use super::lex::{Statement, Tok, Token};

// -- names ------------------------------------------------------------------------

/// A name as a schema holds it: `storage.objects`, or `users` in the
/// default schema (PostgreSQL's `public`, SQL Server's `dbo`, SQLite's
/// `main` — written or not, the same table).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub(crate) struct QName {
    pub schema: Option<String>,
    pub name: String,
}

impl QName {
    /// The name its parts make: the last is the name, the one before it the
    /// schema (a database or server before that is left out).
    pub fn from_parts(mut parts: Vec<String>) -> Option<QName> {
        let name = parts.pop()?;
        let schema = parts.pop().filter(|s| {
            !["public", "dbo", "main"]
                .iter()
                .any(|d| s.eq_ignore_ascii_case(d))
        });
        Some(QName { schema, name })
    }

    pub fn bare(name: &str) -> QName {
        QName {
            schema: None,
            name: name.to_string(),
        }
    }

    /// What it is known by, case aside.
    pub fn key(&self) -> String {
        match &self.schema {
            Some(s) => format!("{}.{}", s.to_lowercase(), self.name.to_lowercase()),
            None => self.name.to_lowercase(),
        }
    }

    /// As it is shown: `storage.objects`, `users`.
    pub fn label(&self) -> String {
        match &self.schema {
            Some(s) => format!("{s}.{}", self.name),
            None => self.name.clone(),
        }
    }

    /// The same name, read with `schema` as the one an unqualified name is
    /// made in (a `SET search_path`).
    pub fn in_schema(&self, schema: Option<&str>) -> QName {
        match (&self.schema, schema) {
            (None, Some(s)) => QName {
                schema: Some(s.to_string()),
                name: self.name.clone(),
            },
            _ => self.clone(),
        }
    }
}

// -- what statements say ---------------------------------------------------------

/// A column as a statement defines it.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Column {
    pub name: String,
    /// Its type as written, lower-cased: `varchar(100)`, `text[]`.
    pub ty: String,
    pub not_null: bool,
    /// Its default, as written.
    pub default: Option<String>,
    /// A value the database makes: `serial`, an identity column,
    /// `AUTO_INCREMENT`, a default taken from a sequence.
    pub identity: bool,
    /// A generated column's expression.
    pub generated: Option<String>,
    pub comment: Option<String>,
    pub file: String,
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ForeignKey {
    pub columns: Vec<String>,
    pub table: QName,
    /// The columns it references; none written: the table's primary key.
    pub references: Vec<String>,
    pub on_delete: Option<String>,
    pub on_update: Option<String>,
    /// The schema an unqualified `table` was read in (a `SET search_path`).
    pub search: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Index {
    pub name: Option<String>,
    /// Its columns, or the expressions it indexes, as written.
    pub columns: Vec<String>,
    pub unique: bool,
    /// Whether it covers only the rows its `WHERE` picks.
    pub partial: bool,
    /// Its access method when not the default B-tree (`gin`, `fulltext`).
    pub method: Option<String>,
    /// Built `CONCURRENTLY`: without holding writes to its table.
    pub concurrent: bool,
    pub file: String,
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Kind {
    PrimaryKey(Vec<String>),
    Unique(Vec<String>),
    ForeignKey(ForeignKey),
    Check(String),
    /// A MySQL `KEY`/`INDEX` in a table's definition.
    Index(Index),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Constraint {
    pub name: Option<String>,
    pub kind: Kind,
    pub file: String,
    pub line: usize,
}

/// A row-level security policy.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Policy {
    pub name: String,
    /// `all`, `select`, `insert`, `update` or `delete`.
    pub command: String,
    pub roles: Vec<String>,
    pub using: Option<String>,
    pub check: Option<String>,
    pub restrictive: bool,
    pub file: String,
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Trigger {
    pub name: String,
    /// When it fires: `before insert or update`.
    pub timing: String,
    /// The function it runs, when it names one.
    pub function: Option<String>,
    pub file: String,
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CommentOn {
    Table(QName),
    Column(QName, String),
}

/// One change to the schema.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Op {
    CreateSchema(String),
    DropSchema(Vec<String>),
    RenameSchema {
        from: String,
        to: String,
    },
    /// The schema an unqualified name is made in from here on (`SET
    /// search_path`); none: the default one.
    SearchPath(Option<String>),
    CreateTable {
        name: QName,
        columns: Vec<Column>,
        constraints: Vec<Constraint>,
        /// Tables whose columns it takes (`LIKE`, `INHERITS`, `PARTITION OF`).
        copies: Vec<QName>,
        comment: Option<String>,
        /// `IF NOT EXISTS`: a table already there is left as it is.
        if_not_exists: bool,
    },
    /// A view, or a table made from a query: what it reads.
    Derive {
        name: QName,
        view: bool,
        materialized: bool,
        /// Its columns, where the statement names them.
        columns: Vec<String>,
        sources: Vec<QName>,
        /// CTEs on the way: `(name, what it reads)`.
        ctes: Vec<(String, Vec<QName>)>,
    },
    /// `INSERT INTO table … SELECT … FROM sources`.
    InsertSelect {
        table: QName,
        sources: Vec<QName>,
        ctes: Vec<(String, Vec<QName>)>,
    },
    AlterTable {
        table: QName,
        actions: Vec<Action>,
    },
    DropTable(Vec<QName>),
    DropView(Vec<QName>),
    RenameView {
        from: QName,
        to: String,
    },
    CreateIndex {
        table: QName,
        index: Index,
    },
    DropIndex {
        names: Vec<QName>,
        table: Option<QName>,
    },
    RenameIndex {
        from: QName,
        to: String,
    },
    CreateEnum {
        name: QName,
        values: Vec<String>,
    },
    AddEnumValue {
        name: QName,
        value: String,
        /// `BEFORE 'x'`: the value it goes in front of.
        before: Option<String>,
        after: Option<String>,
    },
    RenameEnumValue {
        name: QName,
        from: String,
        to: String,
    },
    RenameType {
        from: QName,
        to: String,
    },
    DropType(Vec<QName>),
    CreatePolicy {
        table: QName,
        policy: Policy,
    },
    DropPolicy {
        table: QName,
        name: String,
    },
    RenamePolicy {
        table: QName,
        from: String,
        to: String,
    },
    CreateTrigger {
        table: QName,
        trigger: Trigger,
    },
    DropTrigger {
        table: Option<QName>,
        name: String,
    },
    Comment {
        on: CommentOn,
        text: Option<String>,
    },
}

/// One action of an `ALTER TABLE`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Action {
    AddColumn {
        column: Column,
        constraints: Vec<Constraint>,
        /// `IF NOT EXISTS`: a column already there is left as it is.
        if_not_exists: bool,
    },
    DropColumn(String),
    AlterColumn {
        column: String,
        change: Change,
    },
    /// MySQL's `MODIFY` and `CHANGE`, SQL Server's `ALTER COLUMN x type`:
    /// the column defined again (`from` its old name).
    Redefine {
        from: String,
        column: Column,
        constraints: Vec<Constraint>,
    },
    RenameColumn {
        from: String,
        to: String,
    },
    RenameTo(String),
    SetSchema(String),
    AddConstraint(Constraint),
    DropConstraint(String),
    RenameConstraint {
        from: String,
        to: String,
    },
    DropPrimaryKey,
    /// MySQL's `DROP INDEX x` / `DROP KEY x`.
    DropIndex(String),
    RowLevelSecurity(bool),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Change {
    Type(String),
    NotNull(bool),
    Default(Option<String>),
    Identity(bool),
}

// -- reading -----------------------------------------------------------------------

/// Line numbers of a text, by byte offset.
pub(crate) struct Lines {
    starts: Vec<usize>,
}

impl Lines {
    pub fn new(text: &str) -> Lines {
        let mut starts = vec![0];
        starts.extend(
            text.bytes()
                .enumerate()
                .filter(|(_, b)| *b == b'\n')
                .map(|(i, _)| i + 1),
        );
        Lines { starts }
    }

    /// The 1-based line of byte `at`.
    pub fn line(&self, at: usize) -> usize {
        self.starts.partition_point(|s| *s <= at)
    }
}

/// Every change `text` makes, in order, each with its line.
pub(crate) fn ops(text: &str) -> Vec<(Op, usize)> {
    let lines = Lines::new(text);
    let mut out = Vec::new();
    for Statement { tokens } in super::lex::statements(text) {
        let Some(first) = tokens.first() else {
            continue;
        };
        let line = lines.line(first.at);
        let p = Parser {
            t: &tokens,
            lines: &lines,
        };
        for op in p.statement() {
            out.push((op, line));
        }
    }
    out
}

struct Parser<'a> {
    t: &'a [Token],
    lines: &'a Lines,
}

/// The words a column's type ends at: what follows is a constraint.
const TYPE_ENDS: &[&str] = &[
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
    "CHARSET",
    "KEY",
    "STORAGE",
    "COMPRESSION",
    "VISIBLE",
    "INVISIBLE",
    "SPARSE",
    "ROWGUIDCOL",
    "MASKED",
    "ENCODE",
    "USING",
    "FIRST",
    "AFTER",
];

/// The words a column's `DEFAULT` expression ends at.
const DEFAULT_ENDS: &[&str] = &[
    "NOT",
    "NULL",
    "PRIMARY",
    "REFERENCES",
    "UNIQUE",
    "CHECK",
    "CONSTRAINT",
    "GENERATED",
    "COLLATE",
    "AUTO_INCREMENT",
    "AUTOINCREMENT",
    "COMMENT",
    "ON",
    "FIRST",
    "AFTER",
];

/// What a `CREATE` makes, past its modifiers.
const OBJECTS: &[&str] = &[
    "TABLE",
    "VIEW",
    "INDEX",
    "TYPE",
    "POLICY",
    "TRIGGER",
    "SCHEMA",
    "FUNCTION",
    "PROCEDURE",
    "SEQUENCE",
    "EXTENSION",
    "DOMAIN",
    "EVENT",
    "ROLE",
    "USER",
    "DATABASE",
];

impl Parser<'_> {
    fn at(&self, i: usize) -> Option<&Token> {
        self.t.get(i)
    }

    fn is_word(&self, i: usize, w: &str) -> bool {
        self.at(i).is_some_and(|t| t.is_word(w))
    }

    fn is(&self, i: usize, c: char) -> bool {
        self.at(i).is_some_and(|t| t.is(c))
    }

    fn line(&self, i: usize) -> usize {
        self.at(i.min(self.t.len().saturating_sub(1)))
            .map_or(0, |t| self.lines.line(t.at))
    }

    fn skip(&self, mut i: usize, words: &[&str]) -> usize {
        while self.at(i).is_some_and(|t| t.is_any(words)) {
            i += 1;
        }
        i
    }

    fn statement(&self) -> Vec<Op> {
        let first = self.at(0).and_then(Token::word_upper).unwrap_or_default();
        match first.as_str() {
            "CREATE" => self.create(),
            "ALTER" => self.alter(),
            "DROP" => self.drop(),
            "COMMENT" => self.comment().into_iter().collect(),
            "SET" => self.set().into_iter().collect(),
            "INSERT" | "REPLACE" => self.insert_select(0, Vec::new()).into_iter().collect(),
            "WITH" => {
                // `WITH … INSERT INTO t SELECT …`: the CTEs, then the insert.
                let (ctes, i) = self.with_clause(0);
                match self.is_word(i, "INSERT") {
                    true => self.insert_select(i, ctes).into_iter().collect(),
                    false => Vec::new(),
                }
            }
            _ => Vec::new(),
        }
    }

    // -- names ----------------------------------------------------------------

    /// A (possibly dotted) name at `i`: its parts and the index after it.
    fn parts_at(&self, mut i: usize) -> Option<(Vec<String>, usize)> {
        let mut parts = Vec::new();
        loop {
            parts.push(self.at(i)?.name()?.to_string());
            i += 1;
            if self.is(i, '.') {
                i += 1;
                continue;
            }
            return Some((parts, i));
        }
    }

    fn qname_at(&self, i: usize) -> Option<(QName, usize)> {
        let (parts, i) = self.parts_at(i)?;
        Some((QName::from_parts(parts)?, i))
    }

    /// A single name at `i` (the last part of a dotted one).
    fn name_at(&self, i: usize) -> Option<(String, usize)> {
        let (mut parts, i) = self.parts_at(i)?;
        Some((parts.pop()?, i))
    }

    /// The index just past the `(` … `)` group opening at `i`.
    fn past_group(&self, i: usize) -> usize {
        past_group(self.t, i)
    }

    /// The names (or expressions) in a parenthesised list at `i`, and the
    /// index past it.
    fn list(&self, i: usize) -> (Vec<String>, usize) {
        if !self.is(i, '(') {
            return (Vec::new(), i);
        }
        let end = self.past_group(i);
        let inner = &self.t[i + 1..end.saturating_sub(1).max(i + 1)];
        let items = split_commas(inner)
            .into_iter()
            .filter(|part| !part.is_empty())
            .map(|part| match part {
                // `email`, `email DESC`, `email(20)`, `name text_pattern_ops`.
                [one, rest @ ..]
                    if one.name().is_some()
                        && rest.iter().all(|t| {
                            t.is_any(&["ASC", "DESC", "NULLS", "FIRST", "LAST"])
                                || matches!(t.tok, Tok::Word(_))
                                || t.is('(')
                                || t.is(')')
                        })
                        && !rest.first().is_some_and(|t| t.is('(')) =>
                {
                    one.name().unwrap_or_default().to_string()
                }
                [one, open, ..]
                    if one.name().is_some() && open.is('(') && is_prefix_length(part) =>
                {
                    one.name().unwrap_or_default().to_string()
                }
                _ => render(part, false),
            })
            .collect();
        (items, end)
    }

    // -- CREATE ---------------------------------------------------------------

    fn create(&self) -> Vec<Op> {
        // Past `OR REPLACE`, `TEMPORARY`, MySQL's `DEFINER = …` and the like,
        // to the kind of object it makes.
        let Some(what_at) = (1..self.t.len().min(24)).find(|&i| {
            self.at(i).is_some_and(|t| t.is_any(OBJECTS))
                && !(self.is_word(i, "INDEX") && self.is_word(i.saturating_sub(1), "USING"))
        }) else {
            return Vec::new();
        };
        let modifiers = &self.t[1..what_at];
        let has = |w: &str| modifiers.iter().any(|t| t.is_word(w));
        let what = self
            .at(what_at)
            .and_then(Token::word_upper)
            .unwrap_or_default();
        let i = what_at + 1;
        match what.as_str() {
            "TABLE" => self.create_table(i),
            "VIEW" => self.create_view(i, has("MATERIALIZED")),
            "INDEX" => self
                .create_index(i, has("UNIQUE"), modifiers)
                .into_iter()
                .collect(),
            "TYPE" => self.create_type(i).into_iter().collect(),
            "POLICY" => self.create_policy(i).into_iter().collect(),
            "TRIGGER" => self.create_trigger(i).into_iter().collect(),
            "SCHEMA" => {
                let i = self.skip(i, &["IF", "NOT", "EXISTS"]);
                let i = match self.is_word(i, "AUTHORIZATION") {
                    true => i + 1,
                    false => i,
                };
                self.name_at(i)
                    .map(|(name, _)| Op::CreateSchema(name))
                    .into_iter()
                    .collect()
            }
            _ => Vec::new(),
        }
    }

    fn create_table(&self, i: usize) -> Vec<Op> {
        let if_not_exists = self.is_word(i, "IF");
        let i = self.skip(i, &["IF", "NOT", "EXISTS"]);
        let Some((name, mut i)) = self.qname_at(i) else {
            return Vec::new();
        };
        let mut copies = Vec::new();
        // `PARTITION OF parent`: the parent's columns.
        if self.is_word(i, "PARTITION")
            && self.is_word(i + 1, "OF")
            && let Some((parent, k)) = self.qname_at(i + 2)
        {
            copies.push(parent);
            i = k;
        }
        let mut columns = Vec::new();
        let mut constraints = Vec::new();
        if self.is(i, '(') {
            let end = self.past_group(i);
            for part in split_commas(&self.t[i + 1..end.saturating_sub(1).max(i + 1)]) {
                self.table_element(part, &mut columns, &mut constraints, &mut copies);
            }
            i = end;
        } else if self.is_word(i, "AS") || self.is_word(i, "WITH") && !self.is(i + 1, '(') {
            // `CREATE TABLE t AS SELECT …`
            let (ctes, sources) = self.query_sources(self.skip(i, &["AS"]));
            return vec![Op::Derive {
                name,
                view: false,
                materialized: false,
                columns: Vec::new(),
                sources,
                ctes,
            }];
        } else if self.is_word(i, "LIKE")
            && let Some((other, _)) = self.qname_at(i + 1)
        {
            copies.push(other);
        }
        // Past the definition: `INHERITS (…)`, MySQL's `COMMENT = '…'`, a
        // query (`CREATE TABLE t (a, b) AS SELECT …`).
        let mut comment = None;
        while i < self.t.len() {
            if self.is_word(i, "INHERITS") {
                let (parents, k) = self.list(i + 1);
                copies.extend(parents.iter().map(|p| {
                    QName::from_parts(p.split('.').map(str::to_string).collect())
                        .unwrap_or_default()
                }));
                i = k;
                continue;
            }
            if self.is_word(i, "COMMENT") {
                let k = if self.is(i + 1, '=') { i + 2 } else { i + 1 };
                comment = self.at(k).and_then(Token::string).map(str::to_string);
            }
            i += 1;
        }
        vec![Op::CreateTable {
            name,
            columns,
            constraints,
            copies,
            comment,
            if_not_exists,
        }]
    }

    /// One element of a table's definition: a column, a constraint, a key
    /// or a `LIKE`.
    fn table_element(
        &self,
        part: &[Token],
        columns: &mut Vec<Column>,
        constraints: &mut Vec<Constraint>,
        copies: &mut Vec<QName>,
    ) {
        if part.is_empty() {
            return;
        }
        let p = Parser {
            t: part,
            lines: self.lines,
        };
        if p.is_word(0, "LIKE") {
            if let Some((other, _)) = p.qname_at(1) {
                copies.push(other);
            }
            return;
        }
        if let Some(c) = p.table_constraint(0) {
            constraints.push(c);
            return;
        }
        if let Some((column, inline)) = p.column_def(0) {
            columns.push(column);
            constraints.extend(inline);
        }
    }

    /// A table constraint at `i` (`CONSTRAINT x PRIMARY KEY (…)`, a MySQL
    /// `KEY idx (…)`).
    fn table_constraint(&self, mut i: usize) -> Option<Constraint> {
        let line = self.line(i);
        let mut name = None;
        if self.is_word(i, "CONSTRAINT") {
            let (n, k) = self.name_at(i + 1)?;
            name = Some(n);
            i = k;
        }
        let head = self.at(i).and_then(Token::word_upper)?;
        let kind = match head.as_str() {
            "PRIMARY" if self.is_word(i + 1, "KEY") => {
                let k = self.past_optional_name(self.skip_using(i + 2));
                Kind::PrimaryKey(self.list(self.skip_using(k)).0)
            }
            "UNIQUE" => {
                let named_at = self.skip(i + 1, &["KEY", "INDEX"]);
                let k = self.past_nulls_distinct(named_at);
                let k = self.past_optional_name(k);
                let (columns, _) = self.list(self.skip_using(k));
                if columns.is_empty() {
                    // `UNIQUE USING INDEX x`: what the index covers is not here.
                    return None;
                }
                if name.is_none()
                    && !self.is(named_at, '(')
                    && !self
                        .at(named_at)
                        .is_some_and(|t| t.is_any(&["NULLS", "USING"]))
                {
                    // MySQL's `UNIQUE KEY name (…)` names the index.
                    name = self.name_at(named_at).map(|(n, _)| n);
                }
                Kind::Unique(columns)
            }
            "FOREIGN" if self.is_word(i + 1, "KEY") => {
                let k = self.past_optional_name(i + 2);
                let (columns, k) = self.list(k);
                if !self.is_word(k, "REFERENCES") {
                    return None;
                }
                let (fk, _) = self.references(k + 1, columns)?;
                Kind::ForeignKey(fk)
            }
            "CHECK" if self.is(i + 1, '(') => {
                let end = self.past_group(i + 1);
                Kind::Check(render(
                    &self.t[(i + 2).min(end)..end.saturating_sub(1).max(i + 2)],
                    false,
                ))
            }
            "KEY" | "INDEX" | "FULLTEXT" | "SPATIAL" => {
                let method =
                    matches!(head.as_str(), "FULLTEXT" | "SPATIAL").then(|| head.to_lowercase());
                let k = self.skip(i + 1, &["KEY", "INDEX"]);
                let index_name = (!self.is(k, '('))
                    .then(|| self.name_at(k).map(|(n, _)| n))
                    .flatten();
                let k = self.past_optional_name(k);
                let k = self.skip_using(k);
                let (columns, _) = self.list(k);
                // `key varchar(255)`: a column called `key`, its type's length.
                if columns.is_empty()
                    || columns
                        .iter()
                        .all(|c| c.chars().all(|c| c.is_ascii_digit()))
                {
                    return None;
                }
                Kind::Index(Index {
                    name: index_name,
                    columns,
                    unique: false,
                    partial: false,
                    method,
                    concurrent: false,
                    file: String::new(),
                    line,
                })
            }
            _ => return None,
        };
        Some(Constraint {
            name,
            kind,
            file: String::new(),
            line,
        })
    }

    /// Past PostgreSQL's `NULLS [NOT] DISTINCT` at `i`.
    fn past_nulls_distinct(&self, i: usize) -> usize {
        match self.is_word(i, "NULLS") {
            true => self.skip(i + 1, &["NOT", "DISTINCT"]),
            false => i,
        }
    }

    /// Past a name at `i` unless a `(` is there.
    fn past_optional_name(&self, i: usize) -> usize {
        match self.is(i, '(') {
            true => i,
            false => self.name_at(i).map_or(i, |(_, k)| k),
        }
    }

    /// Past MySQL's `USING BTREE`.
    fn skip_using(&self, i: usize) -> usize {
        match self.is_word(i, "USING") {
            true => i + 2,
            false => i,
        }
    }

    /// `REFERENCES t (cols) [MATCH …] [ON DELETE …] [ON UPDATE …]` from `i`
    /// (past `REFERENCES`): the key, and the index after it.
    fn references(&self, i: usize, columns: Vec<String>) -> Option<(ForeignKey, usize)> {
        let (table, k) = self.qname_at(i)?;
        let (references, mut k) = self.list(k);
        let mut on_delete = None;
        let mut on_update = None;
        loop {
            if self.is_word(k, "MATCH") {
                k += 2;
            } else if self.is_word(k, "ON")
                && (self.is_word(k + 1, "DELETE") || self.is_word(k + 1, "UPDATE"))
            {
                let action_at = k + 2;
                let action = match self.at(action_at).and_then(Token::word_upper).as_deref() {
                    Some("SET") => {
                        let what = self
                            .at(action_at + 1)
                            .and_then(Token::word_upper)
                            .unwrap_or_default();
                        k = action_at + 2;
                        // `SET NULL (col)`: the columns it sets.
                        if self.is(k, '(') {
                            k = self.past_group(k);
                        }
                        format!("set {}", what.to_lowercase())
                    }
                    Some("NO") => {
                        k = action_at + 2;
                        "no action".to_string()
                    }
                    Some(word) => {
                        k = action_at + 1;
                        word.to_lowercase()
                    }
                    None => break,
                };
                match self.is_word(action_at - 1, "DELETE") {
                    true => on_delete = Some(action),
                    false => on_update = Some(action),
                }
            } else if self.at(k).is_some_and(|t| {
                t.is_any(&[
                    "DEFERRABLE",
                    "NOT",
                    "INITIALLY",
                    "DEFERRED",
                    "IMMEDIATE",
                    "ENFORCED",
                    "VALID",
                ])
            }) {
                k += 1;
            } else {
                break;
            }
        }
        Some((
            ForeignKey {
                columns,
                table,
                references,
                on_delete,
                on_update,
                search: None,
            },
            k,
        ))
    }

    /// A column definition at `i`: the column and the constraints written on
    /// it (its primary key, unique, foreign key, check).
    fn column_def(&self, i: usize) -> Option<(Column, Vec<Constraint>)> {
        let line = self.line(i);
        if self.at(i).is_some_and(|t| t.is_any(&["PERIOD", "EXCLUDE"])) {
            return None;
        }
        let (name, mut k) = self.name_at(i)?;
        let type_at = k;
        while let Some(t) = self.at(k) {
            // A word of TYPE_ENDS before a `.` is a schema's name
            // (`storage.BucketType`); PostgreSQL's `STORAGE` clause names a
            // storage mode.
            let keyword = t.is_any(TYPE_ENDS)
                && !self.is(k + 1, '.')
                && (!t.is_word("STORAGE")
                    || self.at(k + 1).is_some_and(|m| {
                        m.is_any(&["PLAIN", "EXTERNAL", "EXTENDED", "MAIN", "DEFAULT"])
                    }));
            if keyword {
                break;
            }
            // `CHARACTER SET x` after a type (but `character varying` is one).
            if t.is_word("CHARACTER") && self.is_word(k + 1, "SET") {
                break;
            }
            if t.is('(') {
                k = self.past_group(k);
                continue;
            }
            k += 1;
        }
        let ty = render(&self.t[type_at..k], true);
        let mut column = Column {
            name: name.clone(),
            identity: is_serial(&ty),
            not_null: is_serial(&ty),
            ty,
            line,
            ..Default::default()
        };
        let mut constraints = Vec::new();
        let mut constraint_name = None;
        while k < self.t.len() {
            let here = self.line(k);
            let word = self.at(k).and_then(Token::word_upper).unwrap_or_default();
            match word.as_str() {
                "CONSTRAINT" => {
                    constraint_name = self.name_at(k + 1).map(|(n, _)| n);
                    k += 2;
                    continue;
                }
                "NOT" if self.is_word(k + 1, "NULL") => {
                    column.not_null = true;
                    k += 2;
                }
                "NULL" => {
                    column.not_null = false;
                    k += 1;
                }
                "DEFAULT" => {
                    let end = self.expression_end(k + 1, DEFAULT_ENDS);
                    let end = match end == k + 1 && self.is_word(k + 1, "NULL") {
                        true => k + 2,
                        false => end,
                    };
                    let value = render(&self.t[k + 1..end], false);
                    if value.to_ascii_lowercase().starts_with("nextval(") {
                        column.identity = true;
                    }
                    // `DEFAULT NULL` says what no default says.
                    column.default = (!value.eq_ignore_ascii_case("null")).then_some(value);
                    k = end;
                }
                "PRIMARY" if self.is_word(k + 1, "KEY") => {
                    column.not_null = true;
                    constraints.push(Constraint {
                        name: constraint_name.take(),
                        kind: Kind::PrimaryKey(vec![name.clone()]),
                        file: String::new(),
                        line: here,
                    });
                    k += 2;
                }
                "UNIQUE" => {
                    constraints.push(Constraint {
                        name: constraint_name.take(),
                        kind: Kind::Unique(vec![name.clone()]),
                        file: String::new(),
                        line: here,
                    });
                    k = self.past_nulls_distinct(self.skip(k + 1, &["KEY"]));
                }
                "REFERENCES" => match self.references(k + 1, vec![name.clone()]) {
                    Some((fk, end)) => {
                        constraints.push(Constraint {
                            name: constraint_name.take(),
                            kind: Kind::ForeignKey(fk),
                            file: String::new(),
                            line: here,
                        });
                        k = end;
                    }
                    None => k += 1,
                },
                "CHECK" if self.is(k + 1, '(') => {
                    let end = self.past_group(k + 1);
                    constraints.push(Constraint {
                        name: constraint_name.take(),
                        kind: Kind::Check(render(
                            &self.t[k + 2..end.saturating_sub(1).max(k + 2)],
                            false,
                        )),
                        file: String::new(),
                        line: here,
                    });
                    k = end;
                }
                "GENERATED" => {
                    // `GENERATED {ALWAYS | BY DEFAULT} AS IDENTITY`, or
                    // `GENERATED ALWAYS AS (expr) STORED`.
                    let as_at = (k + 1..(k + 5).min(self.t.len())).find(|&j| self.is_word(j, "AS"));
                    match as_at {
                        Some(a) if self.is_word(a + 1, "IDENTITY") => {
                            column.identity = true;
                            column.not_null = true;
                            k = a + 2;
                            if self.is(k, '(') {
                                k = self.past_group(k);
                            }
                        }
                        Some(a) if self.is(a + 1, '(') => {
                            let end = self.past_group(a + 1);
                            column.generated = Some(render(
                                &self.t[a + 2..end.saturating_sub(1).max(a + 2)],
                                false,
                            ));
                            k = self.skip(end, &["STORED", "VIRTUAL", "PERSISTED"]);
                        }
                        _ => k += 1,
                    }
                }
                "AS" if self.is(k + 1, '(') => {
                    let end = self.past_group(k + 1);
                    column.generated = Some(render(
                        &self.t[k + 2..end.saturating_sub(1).max(k + 2)],
                        false,
                    ));
                    k = self.skip(end, &["STORED", "VIRTUAL", "PERSISTED"]);
                }
                "AUTO_INCREMENT" | "AUTOINCREMENT" => {
                    column.identity = true;
                    k += 1;
                }
                "IDENTITY" => {
                    column.identity = true;
                    column.not_null = true;
                    k += 1;
                    if self.is(k, '(') {
                        k = self.past_group(k);
                    }
                }
                "COMMENT" => {
                    column.comment = self.at(k + 1).and_then(Token::string).map(str::to_string);
                    k += 2;
                }
                "ON" if self.is_word(k + 1, "UPDATE") => {
                    // MySQL's `ON UPDATE CURRENT_TIMESTAMP`.
                    k = self.expression_end(k + 2, DEFAULT_ENDS);
                }
                "COLLATE" | "CHARSET" => k += 2,
                "CHARACTER" if self.is_word(k + 1, "SET") => k += 3,
                _ => k += 1,
            }
        }
        Some((column, constraints))
    }

    /// Where an expression from `i` ends: at a top-level word of `ends`, a
    /// `,` or the end.
    fn expression_end(&self, mut i: usize, ends: &[&str]) -> usize {
        let start = i;
        while let Some(t) = self.at(i) {
            if i > start && (t.is_any(ends) || t.is(',')) {
                break;
            }
            if t.is('(') {
                i = self.past_group(i);
                continue;
            }
            i += 1;
        }
        i
    }

    fn create_view(&self, i: usize, materialized: bool) -> Vec<Op> {
        let i = self.skip(i, &["IF", "NOT", "EXISTS"]);
        let Some((name, mut i)) = self.qname_at(i) else {
            return Vec::new();
        };
        let mut columns = Vec::new();
        if self.is(i, '(') {
            let (names, k) = self.list(i);
            columns = names;
            i = k;
        }
        let Some(at) = (i..self.t.len()).find(|&j| self.is_word(j, "AS")) else {
            return Vec::new();
        };
        let (ctes, sources) = self.query_sources(at + 1);
        vec![Op::Derive {
            name,
            view: true,
            materialized,
            columns,
            sources,
            ctes,
        }]
    }

    fn create_index(&self, i: usize, unique: bool, modifiers: &[Token]) -> Option<Op> {
        let line = self.line(0);
        let concurrent = self.is_word(i, "CONCURRENTLY");
        let mut i = self.skip(i, &["CONCURRENTLY", "IF", "NOT", "EXISTS"]);
        let mut name = None;
        if !self.is_word(i, "ON") {
            let (n, k) = self.name_at(i)?;
            name = Some(n);
            i = k;
        }
        i = self.skip_using(i);
        if !self.is_word(i, "ON") {
            return None;
        }
        let (table, mut i) = self.qname_at(self.skip(i + 1, &["ONLY"]))?;
        let mut method = modifiers
            .iter()
            .find(|t| t.is_any(&["FULLTEXT", "SPATIAL"]))
            .and_then(Token::word_upper)
            .map(|w| w.to_lowercase());
        if self.is_word(i, "USING") {
            method = self
                .at(i + 1)
                .and_then(Token::word_upper)
                .map(|w| w.to_lowercase())
                .filter(|m| m != "btree");
            i += 2;
        }
        let (columns, end) = self.list(i);
        let partial = (end..self.t.len()).any(|j| self.is_word(j, "WHERE"));
        Some(Op::CreateIndex {
            table,
            index: Index {
                name,
                columns,
                unique,
                partial,
                method,
                concurrent,
                file: String::new(),
                line,
            },
        })
    }

    fn create_type(&self, i: usize) -> Option<Op> {
        let (name, i) = self.qname_at(self.skip(i, &["IF", "NOT", "EXISTS"]))?;
        if !(self.is_word(i, "AS") && self.is_word(i + 1, "ENUM") && self.is(i + 2, '(')) {
            return None;
        }
        let end = self.past_group(i + 2);
        let values = self.t[i + 3..end.saturating_sub(1).max(i + 3)]
            .iter()
            .filter_map(Token::string)
            .map(str::to_string)
            .collect();
        Some(Op::CreateEnum { name, values })
    }

    fn create_policy(&self, i: usize) -> Option<Op> {
        let line = self.line(0);
        let (name, i) = self.name_at(i)?;
        if !self.is_word(i, "ON") {
            return None;
        }
        let (table, mut i) = self.qname_at(i + 1)?;
        let mut policy = Policy {
            name,
            command: "all".into(),
            roles: Vec::new(),
            using: None,
            check: None,
            restrictive: false,
            file: String::new(),
            line,
        };
        while i < self.t.len() {
            let word = self.at(i).and_then(Token::word_upper).unwrap_or_default();
            match word.as_str() {
                "AS" => {
                    policy.restrictive = self.is_word(i + 1, "RESTRICTIVE");
                    i += 2;
                }
                "FOR" => {
                    policy.command = self
                        .at(i + 1)
                        .and_then(Token::word_upper)
                        .unwrap_or_default()
                        .to_lowercase();
                    i += 2;
                }
                "TO" => {
                    i += 1;
                    while let Some(role) = self.at(i).and_then(Token::name) {
                        if self.at(i).is_some_and(|t| t.is_any(&["USING", "WITH"])) {
                            break;
                        }
                        policy.roles.push(role.to_string());
                        i += 1;
                        if !self.is(i, ',') {
                            break;
                        }
                        i += 1;
                    }
                }
                "USING" if self.is(i + 1, '(') => {
                    let end = self.past_group(i + 1);
                    policy.using = Some(render(
                        &self.t[i + 2..end.saturating_sub(1).max(i + 2)],
                        false,
                    ));
                    i = end;
                }
                "WITH" if self.is_word(i + 1, "CHECK") && self.is(i + 2, '(') => {
                    let end = self.past_group(i + 2);
                    policy.check = Some(render(
                        &self.t[i + 3..end.saturating_sub(1).max(i + 3)],
                        false,
                    ));
                    i = end;
                }
                _ => i += 1,
            }
        }
        Some(Op::CreatePolicy { table, policy })
    }

    fn create_trigger(&self, i: usize) -> Option<Op> {
        let line = self.line(0);
        let (name, i) = self.name_at(self.skip(i, &["IF", "NOT", "EXISTS"]))?;
        let on = (i..self.t.len()).find(|&j| self.is_word(j, "ON"))?;
        let timing = self.t[i..on]
            .iter()
            .filter_map(Token::name)
            .map(str::to_lowercase)
            .collect::<Vec<_>>()
            .join(" ");
        let (table, k) = self.qname_at(on + 1)?;
        let function = (k..self.t.len())
            .find(|&j| self.is_word(j, "EXECUTE"))
            .and_then(|j| self.qname_at(self.skip(j + 1, &["FUNCTION", "PROCEDURE"])))
            .map(|(f, _)| f.label());
        Some(Op::CreateTrigger {
            table,
            trigger: Trigger {
                name,
                timing,
                function,
                file: String::new(),
                line,
            },
        })
    }

    // -- ALTER ----------------------------------------------------------------

    fn alter(&self) -> Vec<Op> {
        let what = self.at(1).and_then(Token::word_upper).unwrap_or_default();
        match what.as_str() {
            "TABLE" => self.alter_table(),
            "TYPE" => self.alter_type().into_iter().collect(),
            "POLICY" => {
                let Some((from, i)) = self.name_at(2) else {
                    return Vec::new();
                };
                let Some((table, i)) = self
                    .is_word(i, "ON")
                    .then(|| self.qname_at(i + 1))
                    .flatten()
                else {
                    return Vec::new();
                };
                match self.is_word(i, "RENAME") && self.is_word(i + 1, "TO") {
                    true => self
                        .name_at(i + 2)
                        .map(|(to, _)| Op::RenamePolicy { table, from, to })
                        .into_iter()
                        .collect(),
                    false => Vec::new(),
                }
            }
            "SCHEMA" => match (self.name_at(2), self.is_word(3, "RENAME")) {
                (Some((from, 3)), true) if self.is_word(4, "TO") => self
                    .name_at(5)
                    .map(|(to, _)| Op::RenameSchema { from, to })
                    .into_iter()
                    .collect(),
                _ => Vec::new(),
            },
            "INDEX" => {
                let i = self.skip(2, &["IF", "EXISTS"]);
                let Some((from, i)) = self.qname_at(i) else {
                    return Vec::new();
                };
                match self.is_word(i, "RENAME") && self.is_word(i + 1, "TO") {
                    true => self
                        .name_at(i + 2)
                        .map(|(to, _)| Op::RenameIndex { from, to })
                        .into_iter()
                        .collect(),
                    false => Vec::new(),
                }
            }
            "VIEW" | "MATERIALIZED" => {
                let i = self.skip(1, &["MATERIALIZED", "VIEW", "IF", "EXISTS"]);
                let Some((from, i)) = self.qname_at(i) else {
                    return Vec::new();
                };
                match self.is_word(i, "RENAME") && self.is_word(i + 1, "TO") {
                    true => self
                        .name_at(i + 2)
                        .map(|(to, _)| Op::RenameView { from, to })
                        .into_iter()
                        .collect(),
                    false => Vec::new(),
                }
            }
            _ => Vec::new(),
        }
    }

    fn alter_table(&self) -> Vec<Op> {
        let i = self.skip(2, &["IF", "EXISTS", "ONLY"]);
        let Some((table, mut i)) = self.qname_at(i) else {
            return Vec::new();
        };
        if self.is(i, '*') {
            i += 1;
        }
        let mut actions = Vec::new();
        for part in split_commas(&self.t[i.min(self.t.len())..]) {
            let p = Parser {
                t: part,
                lines: self.lines,
            };
            actions.extend(p.alter_action());
        }
        vec![Op::AlterTable { table, actions }]
    }

    fn alter_action(&self) -> Vec<Action> {
        let head = self.at(0).and_then(Token::word_upper).unwrap_or_default();
        match head.as_str() {
            "ADD" => {
                // `ADD (a int, b int)`: several columns at once.
                if self.is(1, '(') && !self.is_word(2, "CONSTRAINT") {
                    let end = self.past_group(1);
                    let mut out = Vec::new();
                    for part in split_commas(&self.t[2..end.saturating_sub(1).max(2)]) {
                        let p = Parser {
                            t: part,
                            lines: self.lines,
                        };
                        if let Some((column, constraints)) = p.column_def(0) {
                            out.push(Action::AddColumn {
                                column,
                                constraints,
                                if_not_exists: false,
                            });
                        }
                    }
                    return out;
                }
                if !self.is_word(1, "COLUMN")
                    && let Some(c) = self.table_constraint(1)
                {
                    return vec![Action::AddConstraint(c)];
                }
                // SQL Server's `ADD [CONSTRAINT x] DEFAULT (expr) FOR column`.
                let default_at = match self.is_word(1, "CONSTRAINT") {
                    true => self.name_at(2).map_or(3, |(_, k)| k),
                    false => 1,
                };
                if self.is_word(default_at, "DEFAULT")
                    && let Some(for_at) =
                        (default_at + 1..self.t.len()).find(|&j| self.is_word(j, "FOR"))
                    && let Some((column, _)) = self.name_at(for_at + 1)
                {
                    let value = render(&self.t[default_at + 1..for_at], false);
                    return vec![Action::AlterColumn {
                        column,
                        change: Change::Default(Some(unwrap_parens(&value))),
                    }];
                }
                let column_at = self.skip(1, &["COLUMN"]);
                let if_not_exists = self.is_word(column_at, "IF");
                let j = self.skip(column_at, &["IF", "NOT", "EXISTS"]);
                self.column_def(j)
                    .map(|(column, constraints)| Action::AddColumn {
                        column,
                        constraints,
                        if_not_exists,
                    })
                    .into_iter()
                    .collect()
            }
            "DROP" => {
                let what = self.at(1).and_then(Token::word_upper).unwrap_or_default();
                match what.as_str() {
                    "CONSTRAINT" | "FOREIGN" | "CHECK" => {
                        let j = self.skip(2, &["KEY", "IF", "EXISTS"]);
                        self.name_at(j)
                            .map(|(name, _)| Action::DropConstraint(name))
                            .into_iter()
                            .collect()
                    }
                    "PRIMARY" => vec![Action::DropPrimaryKey],
                    "INDEX" | "KEY" => self
                        .name_at(self.skip(2, &["IF", "EXISTS"]))
                        .map(|(name, _)| Action::DropIndex(name))
                        .into_iter()
                        .collect(),
                    "TRIGGER" | "PERIOD" | "IDENTITY" | "DEFAULT" | "EXPRESSION" => Vec::new(),
                    _ => {
                        let j = self.skip(1, &["COLUMN", "IF", "EXISTS"]);
                        self.name_at(j)
                            .map(|(name, _)| Action::DropColumn(name))
                            .into_iter()
                            .collect()
                    }
                }
            }
            "ALTER" => self.alter_column().into_iter().collect(),
            "MODIFY" => {
                let j = self.skip(1, &["COLUMN"]);
                self.column_def(j)
                    .map(|(column, constraints)| Action::Redefine {
                        from: column.name.clone(),
                        column,
                        constraints,
                    })
                    .into_iter()
                    .collect()
            }
            "CHANGE" => {
                let j = self.skip(1, &["COLUMN"]);
                let Some((from, j)) = self.name_at(j) else {
                    return Vec::new();
                };
                self.column_def(j)
                    .map(|(column, constraints)| Action::Redefine {
                        from,
                        column,
                        constraints,
                    })
                    .into_iter()
                    .collect()
            }
            "RENAME" => {
                let what = self.at(1).and_then(Token::word_upper).unwrap_or_default();
                match what.as_str() {
                    "TO" | "AS" => self
                        .name_at(2)
                        .map(|(to, _)| Action::RenameTo(to))
                        .into_iter()
                        .collect(),
                    // A constraint's or (MySQL) an index's new name.
                    "CONSTRAINT" | "INDEX" | "KEY" => {
                        match (self.name_at(2), self.is_word(3, "TO")) {
                            (Some((from, 3)), true) => self
                                .name_at(4)
                                .map(|(to, _)| Action::RenameConstraint { from, to })
                                .into_iter()
                                .collect(),
                            _ => Vec::new(),
                        }
                    }
                    _ => {
                        let j = self.skip(1, &["COLUMN"]);
                        match self.name_at(j) {
                            Some((from, k)) if self.is_word(k, "TO") => self
                                .name_at(k + 1)
                                .map(|(to, _)| Action::RenameColumn { from, to })
                                .into_iter()
                                .collect(),
                            // SQLite's and MySQL's `RENAME new`.
                            Some((to, _)) if j == 1 => vec![Action::RenameTo(to)],
                            _ => Vec::new(),
                        }
                    }
                }
            }
            "SET" if self.is_word(1, "SCHEMA") => self
                .name_at(2)
                .map(|(s, _)| Action::SetSchema(s))
                .into_iter()
                .collect(),
            "ENABLE" | "DISABLE"
                if self.is_word(1, "ROW")
                    && self.is_word(2, "LEVEL")
                    && self.is_word(3, "SECURITY") =>
            {
                vec![Action::RowLevelSecurity(head == "ENABLE")]
            }
            _ => Vec::new(),
        }
    }

    /// `ALTER [COLUMN] x …`: what changes of the column.
    fn alter_column(&self) -> Option<Action> {
        let j = self.skip(1, &["COLUMN"]);
        let (column, k) = self.name_at(j)?;
        let word = self.at(k).and_then(Token::word_upper).unwrap_or_default();
        let change = match word.as_str() {
            "TYPE" => Change::Type(self.type_from(k + 1)),
            "SET" => {
                let what = self
                    .at(k + 1)
                    .and_then(Token::word_upper)
                    .unwrap_or_default();
                match what.as_str() {
                    "DATA" if self.is_word(k + 2, "TYPE") => Change::Type(self.type_from(k + 3)),
                    "NOT" => Change::NotNull(true),
                    "DEFAULT" => {
                        Change::Default(Some(render(&self.t[(k + 2).min(self.t.len())..], false)))
                    }
                    _ => return None,
                }
            }
            "DROP" => {
                let what = self
                    .at(k + 1)
                    .and_then(Token::word_upper)
                    .unwrap_or_default();
                match what.as_str() {
                    "NOT" => Change::NotNull(false),
                    "DEFAULT" => Change::Default(None),
                    "IDENTITY" => Change::Identity(false),
                    _ => return None,
                }
            }
            "ADD" if (k..self.t.len()).any(|i| self.is_word(i, "IDENTITY")) => {
                Change::Identity(true)
            }
            // SQL Server's `ALTER COLUMN x nvarchar(50) NOT NULL`.
            _ if self.at(k).is_some_and(|t| t.name().is_some()) => {
                let (column, constraints) = self.column_def(j)?;
                return Some(Action::Redefine {
                    from: column.name.clone(),
                    column,
                    constraints,
                });
            }
            _ => return None,
        };
        Some(Action::AlterColumn { column, change })
    }

    /// A type from `i`, to `USING`, `COLLATE` or the end.
    fn type_from(&self, i: usize) -> String {
        let end = (i..self.t.len())
            .find(|&j| self.at(j).is_some_and(|t| t.is_any(&["USING", "COLLATE"])))
            .unwrap_or(self.t.len());
        render(&self.t[i.min(end)..end], true)
    }

    fn alter_type(&self) -> Option<Op> {
        let (name, i) = self.qname_at(2)?;
        let word = self.at(i).and_then(Token::word_upper).unwrap_or_default();
        match word.as_str() {
            "ADD" if self.is_word(i + 1, "VALUE") => {
                let j = self.skip(i + 2, &["IF", "NOT", "EXISTS"]);
                let value = self.at(j)?.string()?.to_string();
                let place = |w: &str| {
                    self.is_word(j + 1, w)
                        .then(|| self.at(j + 2).and_then(Token::string).map(str::to_string))
                        .flatten()
                };
                Some(Op::AddEnumValue {
                    name,
                    value,
                    before: place("BEFORE"),
                    after: place("AFTER"),
                })
            }
            "RENAME" if self.is_word(i + 1, "VALUE") => {
                let from = self.at(i + 2)?.string()?.to_string();
                let to = self.at(i + 4)?.string()?.to_string();
                Some(Op::RenameEnumValue { name, from, to })
            }
            "RENAME" if self.is_word(i + 1, "TO") => {
                let (to, _) = self.name_at(i + 2)?;
                Some(Op::RenameType { from: name, to })
            }
            _ => None,
        }
    }

    // -- DROP, COMMENT, SET -----------------------------------------------------

    fn drop(&self) -> Vec<Op> {
        let what = self.at(1).and_then(Token::word_upper).unwrap_or_default();
        let names_from = |i: usize| -> Vec<QName> {
            let i = self.skip(i, &["CONCURRENTLY", "IF", "EXISTS"]);
            split_commas(&self.t[i.min(self.t.len())..])
                .into_iter()
                .filter_map(|part| {
                    Parser {
                        t: part,
                        lines: self.lines,
                    }
                    .qname_at(0)
                    .map(|(n, _)| n)
                })
                .collect()
        };
        match what.as_str() {
            "TABLE" => vec![Op::DropTable(names_from(2))],
            "VIEW" => vec![Op::DropView(names_from(2))],
            "MATERIALIZED" if self.is_word(2, "VIEW") => vec![Op::DropView(names_from(3))],
            "TYPE" => vec![Op::DropType(names_from(2))],
            "SCHEMA" => vec![Op::DropSchema(
                names_from(2).into_iter().map(|n| n.name).collect(),
            )],
            "INDEX" => {
                // MySQL's `DROP INDEX x ON t`.
                let on = (2..self.t.len()).find(|&j| self.is_word(j, "ON"));
                let table = on.and_then(|j| self.qname_at(j + 1)).map(|(t, _)| t);
                let names = match on {
                    Some(j) => Parser {
                        t: &self.t[..j],
                        lines: self.lines,
                    }
                    .qname_at(self.skip(2, &["IF", "EXISTS"]))
                    .map(|(n, _)| vec![n])
                    .unwrap_or_default(),
                    None => names_from(2),
                };
                vec![Op::DropIndex { names, table }]
            }
            "POLICY" => {
                let i = self.skip(2, &["IF", "EXISTS"]);
                match self.name_at(i) {
                    Some((name, k)) if self.is_word(k, "ON") => self
                        .qname_at(k + 1)
                        .map(|(table, _)| Op::DropPolicy { table, name })
                        .into_iter()
                        .collect(),
                    _ => Vec::new(),
                }
            }
            "TRIGGER" => {
                let i = self.skip(2, &["IF", "EXISTS"]);
                match self.name_at(i) {
                    Some((name, k)) => vec![Op::DropTrigger {
                        table: self
                            .is_word(k, "ON")
                            .then(|| self.qname_at(k + 1).map(|(t, _)| t))
                            .flatten(),
                        name,
                    }],
                    None => Vec::new(),
                }
            }
            _ => Vec::new(),
        }
    }

    fn comment(&self) -> Option<Op> {
        if !self.is_word(1, "ON") {
            return None;
        }
        let what = self.at(2).and_then(Token::word_upper)?;
        let (mut parts, i) = self.parts_at(3)?;
        if !self.is_word(i, "IS") {
            return None;
        }
        let text = self.at(i + 1).and_then(Token::string).map(str::to_string);
        let on = match what.as_str() {
            "TABLE" | "VIEW" => CommentOn::Table(QName::from_parts(parts)?),
            "MATERIALIZED" => return None,
            "COLUMN" => {
                let column = parts.pop()?;
                CommentOn::Column(QName::from_parts(parts)?, column)
            }
            _ => return None,
        };
        Some(Op::Comment { on, text })
    }

    /// `SET search_path TO storage, public`: the first schema named, unless
    /// it is the default one.
    fn set(&self) -> Option<Op> {
        let i = self.skip(1, &["SESSION", "LOCAL"]);
        if !self.is_word(i, "search_path") {
            return None;
        }
        let j = match self.is_word(i + 1, "TO") || self.is(i + 1, '=') {
            true => i + 2,
            false => i + 1,
        };
        let first = self
            .at(j)
            .and_then(|t| t.name().or(t.string()))
            .map(|s| s.split(',').next().unwrap_or(s).trim().to_string())?;
        let schema = QName::from_parts(vec![first, String::new()]).and_then(|q| q.schema);
        Some(Op::SearchPath(
            schema.filter(|s| s != "$user" && !s.is_empty()),
        ))
    }

    // -- queries ----------------------------------------------------------------

    fn insert_select(&self, i: usize, ctes: Vec<(String, Vec<QName>)>) -> Option<Op> {
        let i = self.skip(i + 1, &["INTO", "OR", "REPLACE", "IGNORE"]);
        let (table, mut i) = self.qname_at(i)?;
        if self.is(i, '(') {
            i = self.past_group(i);
        }
        let query =
            (i..self.t.len()).find(|&j| self.is_word(j, "SELECT") || self.is_word(j, "WITH"))?;
        let (more, sources) = self.query_sources(query);
        let mut ctes = ctes;
        ctes.extend(more);
        (!sources.is_empty() || !ctes.is_empty()).then_some(Op::InsertSelect {
            table,
            sources,
            ctes,
        })
    }

    /// A `WITH` clause at `i`: each CTE and what it reads, and the index past
    /// the clause.
    fn with_clause(&self, i: usize) -> (Vec<(String, Vec<QName>)>, usize) {
        let mut ctes = Vec::new();
        if !self.is_word(i, "WITH") {
            return (ctes, i);
        }
        let mut j = self.skip(i + 1, &["RECURSIVE"]);
        while let Some((name, mut k)) = self.name_at(j) {
            if self.is(k, '(') {
                k = self.past_group(k);
            }
            if !self.is_word(k, "AS") {
                break;
            }
            k = self.skip(k + 1, &["NOT", "MATERIALIZED"]);
            if !self.is(k, '(') {
                break;
            }
            let end = self.past_group(k);
            let body = &self.t[k + 1..end.saturating_sub(1).max(k + 1)];
            ctes.push((name, from_tables(body)));
            j = end;
            if !self.is(j, ',') {
                break;
            }
            j += 1;
        }
        (ctes, j)
    }

    /// What a query from `i` reads: its CTEs (each with what it reads) and
    /// the tables its own `FROM`s and `JOIN`s name — subqueries' included.
    fn query_sources(&self, i: usize) -> (Vec<(String, Vec<QName>)>, Vec<QName>) {
        let (ctes, j) = self.with_clause(i);
        (ctes, from_tables(&self.t[j.min(self.t.len())..]))
    }
}

/// `(getdate())` → `getdate()`: SQL Server writes a default in brackets.
fn unwrap_parens(value: &str) -> String {
    let mut v = value.trim();
    while let Some(inner) = v.strip_prefix('(').and_then(|r| r.strip_suffix(')')) {
        // Only one pair round the whole: `(a) + (b)` is no such pair.
        let mut depth = 0;
        let whole = inner.chars().all(|c| {
            match c {
                '(' => depth += 1,
                ')' => depth -= 1,
                _ => {}
            }
            depth >= 0
        });
        if !whole {
            break;
        }
        v = inner.trim();
    }
    v.to_string()
}

/// Whether `part` is a name with a prefix length (`email(20)`).
fn is_prefix_length(part: &[Token]) -> bool {
    matches!(part, [_, open, n, close, ..]
        if open.is('(') && close.is(')') && matches!(&n.tok, Tok::Word(w) if w.chars().all(|c| c.is_ascii_digit())))
}

fn is_serial(ty: &str) -> bool {
    matches!(
        ty,
        "serial" | "bigserial" | "smallserial" | "serial4" | "serial8" | "serial2"
    )
}

/// The index just past the `(` … `)` group opening at `i`.
pub(super) fn past_group(tokens: &[Token], i: usize) -> usize {
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
pub(super) fn split_commas(tokens: &[Token]) -> Vec<&[Token]> {
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

/// The longest an expression is shown.
const EXPRESSION_MAX: usize = 120;

/// Tokens as SQL text, spaced as written SQL is: `numeric(10, 2)`,
/// `'{}'::jsonb`, `now()`. A type's words are lower-cased; quoted names lose
/// their quotes.
pub(super) fn render(tokens: &[Token], is_type: bool) -> String {
    let mut out = String::new();
    let mut previous: Option<&Token> = None;
    for t in tokens {
        // `f(x)`, `a.b`, `x::text`, `text[]`, `(a, b)`.
        let glue = match previous {
            None => true,
            Some(p) => {
                t.is(')')
                    || t.is(']')
                    || t.is(',')
                    || t.is('.')
                    || t.is(':')
                    || p.is('(')
                    || p.is('[')
                    || p.is('.')
                    || (p.is(':') && out.ends_with("::"))
                    || (is_operator(p) && is_operator(t))
                    || (t.is('(') && calls(p))
                    || (t.is('[') && (p.name().is_some() || p.is(']') || p.is(')')))
            }
        };
        if !glue {
            out.push(' ');
        }
        match &t.tok {
            Tok::Word(w) if is_type => out.push_str(&w.to_lowercase()),
            Tok::Word(w) | Tok::Ident(w) => out.push_str(w),
            Tok::Str(s) => {
                out.push('\'');
                out.push_str(&s.replace('\'', "''"));
                out.push('\'');
            }
            Tok::Dollar { body, .. } => {
                out.push_str("$$");
                out.push_str(body);
                out.push_str("$$");
            }
            Tok::Punct(c) => out.push(*c),
        }
        previous = Some(t);
    }
    if out.chars().count() > EXPRESSION_MAX {
        out = out.chars().take(EXPRESSION_MAX - 1).collect::<String>() + "…";
    }
    out
}

/// Whether `t` is one character of an operator (`>=`, `<>`, `||`).
fn is_operator(t: &Token) -> bool {
    matches!(
        t.tok,
        Tok::Punct(
            '<' | '>' | '=' | '!' | '~' | '|' | '&' | '+' | '-' | '*' | '/' | '%' | '^' | '@' | '#'
        )
    )
}

/// Whether a `(` after `p` is a call's (`now()`, `numeric(10, 2)`), not a
/// group after a keyword (`IN (…)`, `AND (…)`).
fn calls(p: &Token) -> bool {
    match &p.tok {
        Tok::Word(w) => !matches!(
            w.to_ascii_uppercase().as_str(),
            "IN" | "AND"
                | "OR"
                | "NOT"
                | "EXISTS"
                | "ANY"
                | "ALL"
                | "SOME"
                | "AS"
                | "ON"
                | "WHEN"
                | "THEN"
                | "ELSE"
                | "USING"
                | "CHECK"
                | "IS"
                | "BETWEEN"
                | "LIKE"
                | "ILIKE"
                | "VALUES"
                | "SELECT"
                | "WHERE"
                | "FROM"
                | "CASE"
                | "RETURN"
        ),
        Tok::Ident(_) => true,
        Tok::Punct(c) => matches!(c, ']' | ')'),
        _ => false,
    }
}

/// The words after which a `FROM` list ends.
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
pub(super) fn from_tables(t: &[Token]) -> Vec<QName> {
    let p = Parser {
        t,
        lines: &Lines { starts: vec![0] },
    };
    let mut out: Vec<QName> = Vec::new();
    let mut push = |name: QName| {
        if !out.iter().any(|o| o.key() == name.key()) {
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
            if p.is(j, '(') {
                // A subquery: its own FROMs are read as the loop reaches them.
                break;
            }
            let Some((name, k)) = p.qname_at(j) else {
                break;
            };
            if CLAUSE_WORDS
                .iter()
                .any(|w| name.name.eq_ignore_ascii_case(w))
                || p.is(k, '(')
            {
                // A clause, or a function in FROM (`unnest(…)`).
                break;
            }
            push(name);
            j = k;
            // An alias: `AS x`, or a bare word that is no clause's.
            if p.is_word(j, "AS") {
                j += 2;
            } else if let Some(w) = t.get(j).and_then(Token::word_upper)
                && !CLAUSE_WORDS.contains(&w.as_str())
            {
                j += 1;
            }
            if is_from && p.is(j, ',') {
                j += 1;
                continue;
            }
            break;
        }
        i += 1;
    }
    out
}

/// The tables a string of SQL in code names after `FROM`, `JOIN`, `INTO` or
/// `UPDATE`: each, and whether it is written (`INTO`, `UPDATE`).
pub(super) fn named_tables(sql: &str) -> Vec<(QName, bool)> {
    let tokens = super::lex::tokenize(sql);
    let p = Parser {
        t: &tokens,
        lines: &Lines { starts: vec![0] },
    };
    let mut out = Vec::new();
    for (i, t) in tokens.iter().enumerate() {
        let writes = match t.word_upper().as_deref() {
            Some("FROM" | "JOIN") => false,
            Some("INTO" | "UPDATE") => true,
            _ => continue,
        };
        // `FROM unnest(…)` reads no table; `INTO t (cols)` writes one.
        if let Some((name, k)) = p.qname_at(i + 1)
            && (writes || !p.is(k, '('))
        {
            out.push((name, writes));
        }
    }
    out
}
