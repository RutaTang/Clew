//! Which of a project's SQL files make which database, and the order they
//! run in.
//!
//! A folder of migrations (two or more files named to sort — `0001_x.sql`,
//! `V2__x.sql`, `20240101_x/up.sql`) runs in the order of its names, read as
//! numbers (`0002` before `00010`). A project with one such folder, or none,
//! has one database: every file is in it. A project with several — a
//! tenant's database and the service's own, side by side — has one database
//! per folder, and a file of its own (a test's fixture, a dump) joins the
//! one whose tables it names or shares. Within a database, a file that
//! defines tables another needs runs first.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};

use super::ddl::{Action, Kind, Op, QName};
use super::schema::Changes;
use crate::highlight::Lang;
use crate::maps::{Corpus, Source};

/// One database: its files' changes, in the order they run.
#[derive(Debug, Clone)]
pub(crate) struct Database {
    /// The folder its migrations are in (or its one file); empty when the
    /// project has one database.
    pub root: String,
    pub changes: Vec<Changes>,
}

/// The project's databases.
pub(crate) fn databases(corpus: &Corpus) -> Vec<Database> {
    let files = read(corpus);
    if files.is_empty() {
        return Vec::new();
    }
    // Each file's unit: its migration folder, or itself.
    let mut ordered_in: HashMap<String, usize> = HashMap::new();
    for (changes, _) in &files {
        if let Some(dir) = migration_folder(&changes.rel) {
            *ordered_in.entry(dir).or_default() += 1;
        }
    }
    let mut units: BTreeMap<String, Unit> = BTreeMap::new();
    for (changes, rails) in files {
        let (key, set) = match (rails, migration_folder(&changes.rel)) {
            (true, _) => ("rails:db".to_string(), true),
            (false, Some(dir)) if ordered_in.get(&dir).copied().unwrap_or(0) >= 2 => (dir, true),
            _ => (changes.rel.clone(), false),
        };
        let unit = units.entry(key.clone()).or_insert_with(|| Unit {
            root: match key.strip_prefix("rails:") {
                Some(_) => "db".into(),
                None => key.clone(),
            },
            set,
            ..Default::default()
        });
        unit.changes.push(changes);
    }
    let mut units: Vec<Unit> = units.into_values().collect();
    for unit in &mut units {
        unit.changes.sort_by(|a, b| run_order(&a.rel, &b.rel));
        unit.read_names();
    }
    // A test's SQL (fixtures, data a test loads) is kept out of the
    // databases the rest of the project defines: its tables are a database
    // of their own, and what it changes of another's tables is the test's.
    let (mut tests, main): (Vec<usize>, Vec<usize>) = (0..units.len()).partition(|&u| {
        units[u]
            .changes
            .iter()
            .all(|c| crate::weakspots::is_test_path(&c.rel))
    });
    if !main.is_empty() {
        tests.retain(|&u| !units[u].defines.is_empty());
    }
    let mut groups = group(&units, &main);
    groups.extend(group(&units, &tests));
    let single = groups.len() == 1;
    groups
        .into_iter()
        .map(|members| {
            let root = match single {
                true => String::new(),
                false => members
                    .iter()
                    .find(|&&u| units[u].set)
                    .or(members.first())
                    .map(|&u| units[u].root.clone())
                    .unwrap_or_default(),
            };
            let changes = in_dependency_order(&units, &members)
                .into_iter()
                .flat_map(|u| units[u].changes.iter().cloned())
                .collect();
            Database { root, changes }
        })
        .collect()
}

/// Every file's changes, and whether it is Rails'.
fn read(corpus: &Corpus) -> Vec<(Changes, bool)> {
    let mut out = Vec::new();
    for source in &corpus.files {
        let (ops, rails) = if source.lang == Some(Lang::Sql) {
            if undoes(source) {
                continue;
            }
            (super::ddl::ops(&up_part(&source.text)), false)
        } else if source.lang == Some(Lang::Ruby) && super::rails::is_rails_schema(source) {
            (super::rails::ops(&source.text), true)
        } else {
            continue;
        };
        if !ops.is_empty() {
            out.push((
                Changes {
                    rel: source.rel.clone(),
                    ops,
                },
                rails,
            ));
        }
    }
    out
}

/// Whether a SQL file undoes a migration: a down migration, Flyway's undo.
fn undoes(source: &Source) -> bool {
    let name = source.name().to_ascii_lowercase();
    name.ends_with(".down.sql")
        || name.contains("_down.")
        || name == "down.sql"
        || name == "downgrade.sql"
        || (name.starts_with('u')
            && name[1..].starts_with(|c: char| c.is_ascii_digit())
            && name.contains("__"))
}

/// A migration's "up" part: what precedes its down marker.
pub(crate) fn up_part(text: &str) -> String {
    let mut out = String::new();
    for line in text.lines() {
        let l = line.trim().to_ascii_lowercase();
        if l.starts_with("-- +goose down")
            || l.starts_with("-- migrate:down")
            || l.starts_with("-- +migrate down")
            || l.starts_with("--//@undo")
        {
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// The folder of migrations a file is one of, when its name (or its own
/// folder's, `20240101_init/up.sql`) is one that sorts: starts with a
/// number, or Flyway's `V1__`/`R__`.
fn migration_folder(rel: &str) -> Option<String> {
    let mut parts: Vec<&str> = rel.split('/').collect();
    let name = parts.pop()?;
    if ordered_name(name) {
        return Some(parts.join("/"));
    }
    let folder = parts.pop()?;
    let inner = matches!(
        name.to_ascii_lowercase().as_str(),
        "up.sql" | "migration.sql" | "upgrade.sql" | "forward.sql"
    ) || name.ends_with(".up.sql");
    (inner && ordered_name(folder)).then(|| parts.join("/"))
}

fn ordered_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.first().is_some_and(u8::is_ascii_digit)
        || (matches!(bytes.first(), Some(b'V' | b'v'))
            && bytes.get(1).is_some_and(u8::is_ascii_digit))
        || name.starts_with("R__")
}

/// The order files run in: their paths compared part by part, numbers as
/// numbers (`0002_x` before `00010_x`); Flyway's repeatable `R__` files
/// after its versioned ones; Rails' `schema.rb` last.
pub(crate) fn run_order(a: &str, b: &str) -> Ordering {
    let late = |p: &str| {
        let name = p.rsplit('/').next().unwrap_or(p);
        (p.ends_with("db/schema.rb"), name.starts_with("R__"))
    };
    late(a).cmp(&late(b)).then_with(|| natural(a, b))
}

/// `a` and `b` compared as people sort names with numbers in them.
pub(crate) fn natural(a: &str, b: &str) -> Ordering {
    let chunks = |s: &str| -> Vec<(bool, String)> {
        let mut out: Vec<(bool, String)> = Vec::new();
        for c in s.chars() {
            let digit = c.is_ascii_digit();
            match out.last_mut() {
                Some((d, run)) if *d == digit => run.push(c),
                _ => out.push((digit, c.to_string())),
            }
        }
        out
    };
    let (ca, cb) = (chunks(a), chunks(b));
    for (x, y) in ca.iter().zip(&cb) {
        let order = match (x.0, y.0) {
            (true, true) => {
                let (tx, ty) = (x.1.trim_start_matches('0'), y.1.trim_start_matches('0'));
                tx.len().cmp(&ty.len()).then_with(|| tx.cmp(ty))
            }
            _ => x.1.to_lowercase().cmp(&y.1.to_lowercase()),
        };
        if order != Ordering::Equal {
            return order;
        }
    }
    ca.len().cmp(&cb.len()).then_with(|| a.cmp(b))
}

/// Files that run together: a folder of migrations, or one file.
#[derive(Debug, Default)]
struct Unit {
    root: String,
    /// Whether it is a folder of migrations (or Rails').
    set: bool,
    changes: Vec<Changes>,
    /// The bare names (lower-cased) of the tables, views and types it makes.
    defines: HashSet<String>,
    /// The bare names of those it uses and does not make.
    refers: HashSet<String>,
}

impl Unit {
    fn read_names(&mut self) {
        let mut defines = HashSet::new();
        let mut uses = HashSet::new();
        for changes in &self.changes {
            for (op, _) in &changes.ops {
                for q in op_defines(op) {
                    defines.insert(q.name.to_lowercase());
                }
                for q in op_uses(op) {
                    uses.insert(q.name.to_lowercase());
                }
            }
        }
        self.refers = uses.difference(&defines).cloned().collect();
        self.defines = defines;
    }

    /// Whether it is a dump of a whole schema (run after the migrations it
    /// sums up).
    fn is_dump(&self) -> bool {
        !self.set
            && self.changes.iter().all(|c| {
                let name = c
                    .rel
                    .rsplit('/')
                    .next()
                    .unwrap_or(&c.rel)
                    .to_ascii_lowercase();
                name.starts_with("schema") || name.starts_with("structure") || name.contains("dump")
            })
    }
}

/// What an op makes.
fn op_defines(op: &Op) -> Vec<&QName> {
    match op {
        Op::CreateTable { name, .. } | Op::Derive { name, .. } | Op::CreateEnum { name, .. } => {
            vec![name]
        }
        _ => Vec::new(),
    }
}

/// What an op names that some other statement must have made.
fn op_uses(op: &Op) -> Vec<&QName> {
    fn fks(constraints: &[super::ddl::Constraint]) -> Vec<&QName> {
        constraints
            .iter()
            .filter_map(|c| match &c.kind {
                Kind::ForeignKey(fk) => Some(&fk.table),
                _ => None,
            })
            .collect()
    }
    match op {
        Op::CreateTable {
            constraints,
            copies,
            ..
        } => {
            let mut out = fks(constraints);
            out.extend(copies);
            out
        }
        Op::Derive { sources, .. } => sources.iter().collect(),
        Op::InsertSelect { table, sources, .. } => {
            let mut out = vec![table];
            out.extend(sources);
            out
        }
        Op::AlterTable { table, actions } => {
            let mut out = vec![table];
            for a in actions {
                match a {
                    Action::AddColumn { constraints, .. }
                    | Action::Redefine { constraints, .. } => out.extend(fks(constraints)),
                    Action::AddConstraint(c) => out.extend(fks(std::slice::from_ref(c))),
                    _ => {}
                }
            }
            out
        }
        Op::CreateIndex { table, .. }
        | Op::CreatePolicy { table, .. }
        | Op::CreateTrigger { table, .. } => vec![table],
        _ => Vec::new(),
    }
}

/// The databases `members` of `units` make: one, unless several folders of
/// migrations are among them — then each folder's, and each file of its own
/// in the one it shares the most names with (alone, if none).
fn group(units: &[Unit], members: &[usize]) -> Vec<Vec<usize>> {
    if members.is_empty() {
        return Vec::new();
    }
    if members.iter().filter(|&&u| units[u].set).count() <= 1 {
        return vec![members.to_vec()];
    }
    // Folders of migrations are one database when one uses a table only
    // the other makes.
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for &i in members.iter().filter(|&&u| units[u].set) {
        let unit = &units[i];
        let joined: Vec<usize> = (0..groups.len())
            .filter(|&g| {
                groups[g].iter().any(|&m| {
                    unit.refers.intersection(&units[m].defines).next().is_some()
                        || units[m].refers.intersection(&unit.defines).next().is_some()
                })
            })
            .collect();
        let mut merged = vec![i];
        for &g in joined.iter().rev() {
            merged.extend(groups.remove(g));
        }
        merged.sort_unstable();
        groups.push(merged);
    }
    groups.sort();
    for &i in members.iter().filter(|&&u| !units[u].set) {
        let unit = &units[i];
        let shared = |g: &Vec<usize>| -> usize {
            g.iter()
                .map(|&m| {
                    let other = &units[m];
                    unit.refers.intersection(&other.defines).count()
                        + other.refers.intersection(&unit.defines).count()
                        + unit.defines.intersection(&other.defines).count()
                })
                .sum()
        };
        let best = groups
            .iter()
            .enumerate()
            .map(|(g, members)| (shared(members), g))
            .filter(|(n, _)| *n > 0)
            .max_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
        match best {
            Some((_, g)) => groups[g].push(i),
            None => groups.push(vec![i]),
        }
    }
    groups
}

/// `members` in the order they run: one that makes what another uses
/// first; past that, folders of migrations before files of their own,
/// dumps last, then by path.
fn in_dependency_order(units: &[Unit], members: &[usize]) -> Vec<usize> {
    let rank = |u: usize| (units[u].is_dump(), !units[u].set);
    let mut pending: Vec<usize> = members.to_vec();
    pending.sort_by(|&a, &b| {
        rank(a)
            .cmp(&rank(b))
            .then_with(|| natural(&units[a].root, &units[b].root))
    });
    let needs = |a: usize, b: usize| -> bool {
        // `a` uses what `b` makes, and `b` does not use what `a` makes.
        units[a]
            .refers
            .intersection(&units[b].defines)
            .next()
            .is_some()
            && units[b]
                .refers
                .intersection(&units[a].defines)
                .next()
                .is_none()
    };
    let mut out = Vec::new();
    while !pending.is_empty() {
        let next = pending
            .iter()
            .position(|&a| !pending.iter().any(|&b| b != a && needs(a, b)))
            .unwrap_or(0);
        out.push(pending.remove(next));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_with_numbers_sort_as_numbers() {
        let mut names = vec![
            "m/00010-search.sql",
            "m/0002-schema.sql",
            "m/0001-initial.sql",
            "m/V10__x.sql",
            "m/V2__x.sql",
            "m/R__views.sql",
        ];
        names.sort_by(|a, b| run_order(a, b));
        assert_eq!(
            names,
            [
                "m/0001-initial.sql",
                "m/0002-schema.sql",
                "m/00010-search.sql",
                "m/V2__x.sql",
                "m/V10__x.sql",
                "m/R__views.sql"
            ]
        );
    }

    #[test]
    fn a_folder_of_migrations_is_known_by_its_names() {
        assert_eq!(
            migration_folder("db/migrations/0001_init.sql").as_deref(),
            Some("db/migrations")
        );
        assert_eq!(
            migration_folder("prisma/migrations/20240101_init/migration.sql").as_deref(),
            Some("prisma/migrations")
        );
        assert_eq!(
            migration_folder("migrations/2024-01-01-000000_users/up.sql").as_deref(),
            Some("migrations")
        );
        assert_eq!(migration_folder("db/schema.sql"), None);
        assert_eq!(migration_folder("db/tables/users.sql"), None);
    }

    fn roots_and_files(corpus: &Corpus) -> Vec<(String, Vec<String>)> {
        databases(corpus)
            .into_iter()
            .map(|d| (d.root, d.changes.into_iter().map(|c| c.rel).collect()))
            .collect()
    }

    #[test]
    fn one_folder_of_migrations_is_one_database_with_every_file() {
        let corpus = Corpus::of(&[
            (
                "db/migrations/0002_posts.sql",
                "CREATE TABLE posts (id int, user_id int REFERENCES users(id));",
            ),
            (
                "db/migrations/0001_users.sql",
                "CREATE TABLE users (id int PRIMARY KEY);",
            ),
            (
                "db/views/report.sql",
                "CREATE VIEW report AS SELECT * FROM posts;",
            ),
            ("db/tables/audit.sql", "CREATE TABLE audit (id int);"),
        ]);
        assert_eq!(
            roots_and_files(&corpus),
            [(
                String::new(),
                vec![
                    "db/migrations/0001_users.sql".to_string(),
                    "db/migrations/0002_posts.sql".into(),
                    "db/tables/audit.sql".into(),
                    "db/views/report.sql".into(),
                ]
            )]
        );
    }

    #[test]
    fn folders_of_migrations_side_by_side_are_databases_of_their_own() {
        let corpus = Corpus::of(&[
            (
                "migrations/tenant/0001-init.sql",
                "CREATE TABLE storage.objects (id uuid, owner uuid REFERENCES auth.users (id));",
            ),
            (
                "migrations/tenant/0002-more.sql",
                "ALTER TABLE storage.objects ADD COLUMN name text;",
            ),
            (
                "migrations/multitenant/0001-init.sql",
                "CREATE TABLE tenants (id text PRIMARY KEY);",
            ),
            (
                "migrations/multitenant/0002-more.sql",
                "ALTER TABLE tenants ADD COLUMN jwks jsonb;",
            ),
            (
                "db/auth/auth-schema.sql",
                "CREATE SCHEMA auth; CREATE TABLE auth.users (id uuid PRIMARY KEY);",
            ),
            (
                "src/test/db/01-auth-schema.sql",
                "CREATE SCHEMA auth; CREATE TABLE auth.users (id uuid PRIMARY KEY);",
            ),
            ("tools/scratch.sql", "CREATE TABLE scratch (x int);"),
        ]);
        assert_eq!(
            roots_and_files(&corpus),
            [
                (
                    "migrations/multitenant".to_string(),
                    vec![
                        "migrations/multitenant/0001-init.sql".to_string(),
                        "migrations/multitenant/0002-more.sql".into()
                    ]
                ),
                (
                    "migrations/tenant".to_string(),
                    vec![
                        "db/auth/auth-schema.sql".to_string(),
                        "migrations/tenant/0001-init.sql".into(),
                        "migrations/tenant/0002-more.sql".into()
                    ]
                ),
                (
                    "tools/scratch.sql".to_string(),
                    vec!["tools/scratch.sql".to_string()]
                ),
                (
                    "src/test/db/01-auth-schema.sql".to_string(),
                    vec!["src/test/db/01-auth-schema.sql".to_string()]
                ),
            ],
            "a file of its own runs first in the database that uses it; a test's fixture is a database of its own"
        );
    }

    #[test]
    fn a_tests_sql_is_kept_out_of_the_projects_databases() {
        let corpus = Corpus::of(&[
            (
                "db/migrations/0001_users.sql",
                "CREATE TABLE users (id int PRIMARY KEY);",
            ),
            (
                "db/migrations/0002_more.sql",
                "ALTER TABLE users ADD COLUMN name text;",
            ),
            (
                "test/fixtures/policies.sql",
                "CREATE POLICY p ON users USING (true);",
            ),
            ("test/db/01-auth.sql", "CREATE TABLE auth.users (id uuid);"),
        ]);
        assert_eq!(
            roots_and_files(&corpus),
            [
                (
                    "db/migrations".to_string(),
                    vec![
                        "db/migrations/0001_users.sql".to_string(),
                        "db/migrations/0002_more.sql".into()
                    ]
                ),
                (
                    "test/db/01-auth.sql".to_string(),
                    vec!["test/db/01-auth.sql".to_string()]
                ),
            ],
            "the test's tables a database of their own; its policy on the project's table left out"
        );
    }

    #[test]
    fn down_and_undo_files_are_left_out() {
        let corpus = Corpus::of(&[
            ("m/001_a.up.sql", "CREATE TABLE a (x int);"),
            ("m/001_a.down.sql", "DROP TABLE a;"),
            ("m/U1__a.sql", "DROP TABLE a;"),
            ("m/20240101_b/down.sql", "DROP TABLE a;"),
        ]);
        let files: Vec<String> = roots_and_files(&corpus)
            .into_iter()
            .flat_map(|(_, f)| f)
            .collect();
        assert_eq!(files, ["m/001_a.up.sql"]);
    }
}
