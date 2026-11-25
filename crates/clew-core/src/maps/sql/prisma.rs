//! A Prisma schema (`schema.prisma`, or the `.prisma` files of a schema
//! folder) read as the database it makes: each `model` a table — its
//! scalar fields the columns, `@id`, `@unique`, `?` and `@default` as the
//! schema has them, `///` docs as comments, `@@id`, `@@unique` and
//! `@@index` as keys and indexes — each `@relation(fields:, references:)` a
//! foreign key, each `enum` an enum. A relation field (`posts Post[]`) is
//! no column: the key it names is.

use std::collections::BTreeSet;

use super::ddl::{Column, Constraint, ForeignKey, Index, Kind, QName};
use super::schema::{Enum, Schema, Table};
use crate::maps::Source;

/// Whether a file is a Prisma schema's.
pub(crate) fn is_prisma(source: &Source) -> bool {
    source.rel.ends_with(".prisma")
}

/// The schema the Prisma files `sources` make, and the folder they are in;
/// none when they make no model.
pub(crate) fn schema(sources: &[&Source]) -> Option<(String, Schema)> {
    let blocks: Vec<(Block, &Source)> = sources
        .iter()
        .flat_map(|s| blocks(&s.text).into_iter().map(move |b| (b, *s)))
        .collect();
    let models: BTreeSet<String> = blocks
        .iter()
        .filter(|(b, _)| b.keyword == "model" || b.keyword == "view")
        .map(|(b, _)| b.name.clone())
        .collect();
    if models.is_empty() {
        return None;
    }
    let mut schema = Schema::default();
    for (b, source) in &blocks {
        match b.keyword.as_str() {
            "model" | "view" => {
                let table = model(b, &source.rel, &models);
                schema.tables.insert(table.name.key(), table);
            }
            "enum" => {
                let name = QName::bare(&b.name);
                schema.enums.insert(
                    name.key(),
                    Enum {
                        name,
                        values: b
                            .lines
                            .iter()
                            .filter_map(|(_, l)| {
                                let w = l.split_whitespace().next()?;
                                (!w.starts_with('@')).then(|| w.to_string())
                            })
                            .collect(),
                        file: source.rel.clone(),
                        line: b.line,
                    },
                );
            }
            _ => {}
        }
    }
    let root = sources
        .first()
        .map(|s| {
            s.rel
                .rsplit_once('/')
                .map_or(String::new(), |(dir, _)| dir.to_string())
        })
        .unwrap_or_default();
    Some((root, schema))
}

/// A `keyword Name { … }` block: its lines (each with its 1-based number,
/// its `//` comment gone), and the `///` doc above it.
#[derive(Debug, Default)]
struct Block {
    keyword: String,
    name: String,
    line: usize,
    doc: Option<String>,
    /// Each line of its body: number, text, and the `///` doc above it.
    lines: Vec<(usize, String)>,
    docs: Vec<Option<String>>,
}

fn blocks(text: &str) -> Vec<Block> {
    let mut out = Vec::new();
    let mut open: Option<Block> = None;
    let mut doc: Vec<String> = Vec::new();
    for (n, raw) in text.lines().enumerate() {
        let trimmed = raw.trim();
        if let Some(d) = trimmed.strip_prefix("///") {
            doc.push(d.trim().to_string());
            continue;
        }
        let line = strip_comment(trimmed);
        let pending = (!doc.is_empty()).then(|| doc.join(" "));
        doc.clear();
        if line.is_empty() {
            continue;
        }
        match &mut open {
            Some(b) => {
                if line == "}" {
                    out.extend(open.take());
                } else {
                    b.lines.push((n + 1, line.to_string()));
                    b.docs.push(pending);
                }
            }
            None => {
                let mut words = line.split_whitespace();
                let (Some(keyword), Some(name)) = (words.next(), words.next()) else {
                    continue;
                };
                if line.ends_with('{') {
                    open = Some(Block {
                        keyword: keyword.to_string(),
                        name: name.trim_end_matches('{').to_string(),
                        line: n + 1,
                        doc: pending,
                        ..Default::default()
                    });
                }
            }
        }
    }
    out
}

/// `line` without its `//` comment (one in a string kept).
fn strip_comment(line: &str) -> &str {
    let mut quoted = false;
    let b = line.as_bytes();
    for i in 0..b.len() {
        match b[i] {
            b'"' => quoted = !quoted,
            b'/' if !quoted && b.get(i + 1) == Some(&b'/') => return line[..i].trim_end(),
            _ => {}
        }
    }
    line
}

/// The arguments of `@name(…)` in `attrs`, whole (nested brackets kept).
fn attribute<'a>(attrs: &'a str, name: &str) -> Option<&'a str> {
    let mut from = 0;
    while let Some(at) = attrs[from..].find(name) {
        let at = from + at;
        let after = &attrs[at + name.len()..];
        // `@id` is not `@idx`.
        if after.starts_with(|c: char| c.is_alphanumeric() || c == '_') {
            from = at + name.len();
            continue;
        }
        let Some(args) = after.strip_prefix('(') else {
            return Some("");
        };
        let mut depth = 1;
        let mut quoted = false;
        for (i, c) in args.char_indices() {
            match c {
                '"' => quoted = !quoted,
                '(' | '[' if !quoted => depth += 1,
                ')' | ']' if !quoted => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(&args[..i]);
                    }
                }
                _ => {}
            }
        }
        return Some(args);
    }
    None
}

/// The names in a `[a, b]` list, or in `fields: [a, b]` of `args`.
fn names(args: &str, key: Option<&str>) -> Vec<String> {
    let list = match key {
        Some(k) => {
            let Some(at) = args.find(&format!("{k}:")) else {
                return Vec::new();
            };
            &args[at + k.len() + 1..]
        }
        None => args,
    };
    let Some(open) = list.find('[') else {
        return Vec::new();
    };
    let close = list[open..].find(']').map_or(list.len(), |c| open + c);
    list[open + 1..close]
        .split(',')
        .map(|n| n.trim().split('(').next().unwrap_or("").trim().to_string())
        .filter(|n| !n.is_empty())
        .collect()
}

/// A named argument's value (`onDelete: Cascade` → `Cascade`).
fn value_of(args: &str, key: &str) -> Option<String> {
    let at = args.find(&format!("{key}:"))?;
    let rest = args[at + key.len() + 1..].trim_start();
    let end = rest.find([',', ')']).unwrap_or(rest.len());
    let v = rest[..end].trim().trim_matches('"');
    (!v.is_empty()).then(|| v.to_string())
}

/// Prisma's words for what a referenced row's change does (`SetNull`) as
/// SQL says them (`set null`).
fn action(word: &str) -> String {
    let mut out = String::new();
    for (i, c) in word.chars().enumerate() {
        if c.is_uppercase() && i > 0 {
            out.push(' ');
        }
        out.extend(c.to_lowercase());
    }
    out
}

fn model(b: &Block, rel: &str, models: &BTreeSet<String>) -> Table {
    let mut table = Table {
        name: QName::bare(&b.name),
        comment: b.doc.clone(),
        file: rel.to_string(),
        line: b.line,
        ..Default::default()
    };
    let constraint = |kind: Kind, line: usize| Constraint {
        name: None,
        kind,
        file: rel.to_string(),
        line,
    };
    for (k, (line, text)) in b.lines.iter().enumerate() {
        if let Some(rest) = text.strip_prefix("@@") {
            let word: String = rest.chars().take_while(|c| c.is_alphanumeric()).collect();
            let args = attribute(text, &format!("@@{word}")).unwrap_or("");
            match word.as_str() {
                "id" => table
                    .constraints
                    .push(constraint(Kind::PrimaryKey(names(args, None)), *line)),
                "unique" => table
                    .constraints
                    .push(constraint(Kind::Unique(names(args, None)), *line)),
                "index" => table.indexes.push(Index {
                    name: value_of(args, "map").or_else(|| value_of(args, "name")),
                    columns: names(args, None),
                    unique: false,
                    partial: false,
                    method: value_of(args, "type")
                        .map(|t| t.to_lowercase())
                        .filter(|t| t != "btree"),
                    concurrent: false,
                    file: rel.to_string(),
                    line: *line,
                }),
                "map" => {
                    let db = args.trim().trim_matches('"');
                    let note = format!("table {db}");
                    table.comment = Some(match table.comment.take() {
                        Some(c) => format!("{c} ({note})"),
                        None => note,
                    });
                }
                _ => {}
            }
            continue;
        }
        let mut words = text.split_whitespace();
        let (Some(name), Some(ty)) = (words.next(), words.next()) else {
            continue;
        };
        let attrs = text[text.find(ty).map_or(text.len(), |i| i + ty.len())..].trim();
        let bare = ty.trim_end_matches(['?', '!']).trim_end_matches("[]");
        if models.contains(bare) {
            // A relation: its key, if this side holds it.
            if let Some(args) = attribute(attrs, "@relation") {
                let columns = names(args, Some("fields"));
                if !columns.is_empty() {
                    table.constraints.push(constraint(
                        Kind::ForeignKey(ForeignKey {
                            columns,
                            table: QName::bare(bare),
                            references: names(args, Some("references")),
                            on_delete: value_of(args, "onDelete").map(|a| action(&a)),
                            on_update: value_of(args, "onUpdate").map(|a| action(&a)),
                            search: None,
                        }),
                        *line,
                    ));
                }
            }
            continue;
        }
        let default = attribute(attrs, "@default").map(str::to_string);
        let identity = default
            .as_deref()
            .is_some_and(|d| d.starts_with("autoincrement(") || d.starts_with("sequence("));
        let mut column = Column {
            name: name.to_string(),
            // The database's own type when the field names one: `String
            // @db.VarChar(255)` is a varchar(255).
            ty: match attrs.find("@db.") {
                Some(at) => attrs[at + 4..]
                    .split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .to_lowercase(),
                None => ty.to_string(),
            },
            not_null: !ty.ends_with('?'),
            default: default.filter(|_| !identity),
            identity,
            generated: None,
            comment: b.docs.get(k).cloned().flatten(),
            file: rel.to_string(),
            line: *line,
        };
        if attrs.contains("@updatedAt") && column.default.is_none() {
            column.default = Some("@updatedAt".into());
        }
        if attribute(attrs, "@id").is_some() {
            table
                .constraints
                .push(constraint(Kind::PrimaryKey(vec![name.to_string()]), *line));
        }
        if attribute(attrs, "@unique").is_some() {
            table
                .constraints
                .push(constraint(Kind::Unique(vec![name.to_string()]), *line));
        }
        table.columns.push(column);
    }
    table
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_prisma_schema_is_its_tables() {
        let text = r#"datasource db {
  provider = "postgresql"
}

/// People who sign in.
model User {
  id        Int      @id @default(autoincrement())
  /// Where we write to them.
  email     String   @unique
  name      String?  // what they go by
  role      Role     @default(USER)
  posts     Post[]
  createdAt DateTime @default(now()) @map("created_at")
  updatedAt DateTime @updatedAt
  @@map("users")
}

model Post {
  id       String @id @default(uuid()) @db.Uuid
  title    String @db.VarChar(255)
  author   User   @relation(fields: [authorId], references: [id], onDelete: SetNull)
  authorId Int?
  @@index([authorId])
  @@unique([title, authorId])
}

enum Role {
  USER
  ADMIN // the ones who can
}
"#;
        let source = Source::new("prisma/schema.prisma", None, text.to_string());
        let (root, schema) = schema(&[&source]).expect("a schema");
        assert_eq!(root, "prisma");
        let user = &schema.tables["user"];
        assert_eq!(
            user.comment.as_deref(),
            Some("People who sign in. (table users)")
        );
        let cols: Vec<(&str, &str, bool)> = user
            .columns
            .iter()
            .map(|c| (c.name.as_str(), c.ty.as_str(), c.not_null))
            .collect();
        assert_eq!(
            cols,
            [
                ("id", "Int", true),
                ("email", "String", true),
                ("name", "String?", false),
                ("role", "Role", true),
                ("createdAt", "DateTime", true),
                ("updatedAt", "DateTime", true),
            ],
            "posts is a relation, no column"
        );
        assert!(user.columns[0].identity);
        assert_eq!(user.primary_key(), ["id"]);
        assert!(user.is_unique("email"));
        assert_eq!(
            user.columns[1].comment.as_deref(),
            Some("Where we write to them.")
        );
        assert_eq!(user.columns[3].default.as_deref(), Some("USER"));
        assert_eq!(user.columns[5].default.as_deref(), Some("@updatedAt"));
        let post = &schema.tables["post"];
        assert_eq!(post.columns[0].ty, "uuid");
        assert_eq!(post.columns[1].ty, "varchar(255)");
        let fk = post.foreign_keys().next().expect("a key");
        assert_eq!(
            (
                fk.columns.as_slice(),
                fk.table.name.as_str(),
                fk.references.as_slice()
            ),
            (
                &["authorId".to_string()][..],
                "User",
                &["id".to_string()][..]
            )
        );
        assert_eq!(fk.on_delete.as_deref(), Some("set null"));
        assert!(post.is_indexed("authorId"));
        assert_eq!(schema.enums["role"].values, ["USER", "ADMIN"]);
    }
}
