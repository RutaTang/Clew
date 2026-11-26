//! Haskell's Persistent models read as the tables they define — a
//! `persistLowerCase` (or `persistUpperCase`) quasi-quote in the code, or a
//! Yesod app's `config/models` (`*.persistentmodels`): each entity a table
//! (`BlogPost` is `blog_post` lowercased, or its `sql=` name), each field a
//! column (`Maybe` nullable, `sql=`, `default=`), a field of another
//! entity's key (`authorId PersonId`) a foreign key, `Unique…` a unique
//! constraint, `Primary` the key — else the `id` Persistent adds — and
//! `Foreign` a foreign key of its own.

use super::ddl::{Column, Constraint, ForeignKey, Kind, QName};
use super::schema::{Schema, Table};
use crate::highlight::Lang;
use crate::maps::Source;

/// Whether `rel` is a Yesod app's models file.
pub(crate) fn is_models_file(rel: &str) -> bool {
    let rel = rel.replace('\\', "/");
    rel == "config/models" || rel.ends_with("/config/models") || rel.ends_with(".persistentmodels")
}

/// Whether a file may define Persistent models.
pub(crate) fn has_models(source: &Source) -> bool {
    match source.lang {
        Some(Lang::Haskell) => {
            source.text.contains("[persistLowerCase|") || source.text.contains("[persistUpperCase|")
        }
        None => is_models_file(&source.rel),
        _ => false,
    }
}

/// Each block of model definitions in a file: its text, the line it starts
/// at, and whether names are lowercased (snake case) for the database.
fn blocks(source: &Source) -> Vec<(&str, usize, bool)> {
    let text = source.text.as_str();
    if source.lang.is_none() {
        return vec![(text, 1, true)];
    }
    let mut out = Vec::new();
    for (open, lower) in [("[persistLowerCase|", true), ("[persistUpperCase|", false)] {
        let mut from = 0;
        while let Some(at) = text[from..].find(open) {
            let start = from + at + open.len();
            let end = text[start..].find("|]").map_or(text.len(), |e| start + e);
            out.push((
                &text[start..end],
                super::super::line_at(text, start) + 1,
                lower,
            ));
            from = end;
        }
    }
    out
}

struct Entity {
    name: String,
    table: String,
    rel: String,
    line: usize,
    lower: bool,
    /// Its indented lines, each with its line number.
    lines: Vec<(usize, String)>,
}

fn entities(sources: &[&Source]) -> Vec<Entity> {
    let mut out: Vec<Entity> = Vec::new();
    for source in sources {
        for (body, first, lower) in blocks(source) {
            for (n, raw) in body.lines().enumerate() {
                let line = first + n;
                let code = raw.split("--").next().unwrap_or("");
                if code.trim().is_empty() {
                    continue;
                }
                if !code.starts_with(char::is_whitespace) {
                    // `BlogPost sql=posts json`.
                    let mut words = code.split_whitespace();
                    let Some(name) = words.next().filter(|w| w.starts_with(char::is_uppercase))
                    else {
                        continue;
                    };
                    let table = words
                        .find_map(|w| w.strip_prefix("sql="))
                        .map(str::to_string)
                        .unwrap_or_else(|| match lower {
                            true => super::snake_case(name),
                            false => name.to_string(),
                        });
                    out.push(Entity {
                        name: name.to_string(),
                        table,
                        rel: source.rel.clone(),
                        line,
                        lower,
                        lines: Vec::new(),
                    });
                } else if let Some(entity) = out.last_mut()
                    && entity.rel == source.rel
                {
                    entity.lines.push((line, code.trim().to_string()));
                }
            }
        }
    }
    out
}

pub(crate) fn schema(sources: &[&Source]) -> Option<Schema> {
    let entities = entities(sources);
    let table_of = |entity: &str| -> Option<String> {
        entities
            .iter()
            .find(|e| e.name == entity)
            .map(|e| e.table.clone())
    };
    let mut schema = Schema::default();
    for e in &entities {
        let mut table = Table {
            name: QName::bare(&e.table),
            file: e.rel.clone(),
            line: e.line,
            ..Default::default()
        };
        let constraint = |kind: Kind, line: usize| Constraint {
            name: None,
            kind,
            file: e.rel.clone(),
            line,
        };
        let column_name = |field: &str| match e.lower {
            true => super::snake_case(field),
            false => field.to_string(),
        };
        let mut primary: Option<(Vec<String>, usize)> = None;
        let mut fields: Vec<(String, String)> = Vec::new();
        let mut body = Vec::new();
        for (line, code) in &e.lines {
            let words: Vec<&str> = code.split_whitespace().collect();
            let Some(&first) = words.first() else {
                continue;
            };
            match first {
                "deriving" => continue,
                "Primary" => {
                    primary = Some((words[1..].iter().map(|f| f.to_string()).collect(), *line));
                }
                // `Foreign Person fkperson personName personAge`.
                "Foreign" => {
                    if let (Some(target), Some(cols)) = (words.get(1), words.get(3..)) {
                        body.push((
                            *line,
                            Kind::ForeignKey(ForeignKey {
                                columns: cols.iter().map(|c| c.to_string()).collect(),
                                table: QName::bare(
                                    &table_of(target).unwrap_or_else(|| target.to_string()),
                                ),
                                references: Vec::new(),
                                on_delete: None,
                                on_update: None,
                                search: None,
                            }),
                        ));
                    }
                }
                // `UniqueName name`.
                u if u.starts_with("Unique") => {
                    body.push((
                        *line,
                        Kind::Unique(words[1..].iter().map(|f| f.to_string()).collect()),
                    ));
                }
                "Id" => {}
                f if f.starts_with(char::is_lowercase) => {
                    let Some(&ty) = words.get(1) else { continue };
                    let name = words
                        .iter()
                        .find_map(|w| w.strip_prefix("sql="))
                        .map_or_else(|| column_name(f), str::to_string);
                    let maybe = words.contains(&"Maybe") || ty == "Maybe";
                    let ty = if ty == "Maybe" {
                        words.get(2).copied().unwrap_or(ty)
                    } else {
                        ty
                    };
                    table.columns.push(Column {
                        name: name.clone(),
                        ty: ty.to_string(),
                        not_null: !maybe,
                        default: words
                            .iter()
                            .find_map(|w| w.strip_prefix("default="))
                            .map(str::to_string),
                        file: e.rel.clone(),
                        line: *line,
                        ..Default::default()
                    });
                    fields.push((f.to_string(), name.clone()));
                    // `authorId PersonId`: a key of another entity.
                    if let Some(target) = ty.strip_suffix("Id").and_then(&table_of) {
                        body.push((
                            *line,
                            Kind::ForeignKey(ForeignKey {
                                columns: vec![name],
                                table: QName::bare(&target),
                                references: Vec::new(),
                                on_delete: None,
                                on_update: None,
                                search: None,
                            }),
                        ));
                    }
                }
                _ => {}
            }
        }
        let field_column = |f: &String| {
            fields
                .iter()
                .find(|(field, _)| field == f)
                .map_or_else(|| column_name(f), |(_, c)| c.clone())
        };
        match primary {
            Some((key, line)) => table.constraints.push(constraint(
                Kind::PrimaryKey(key.iter().map(field_column).collect()),
                line,
            )),
            None => {
                table.columns.insert(
                    0,
                    Column {
                        name: "id".into(),
                        ty: "id".into(),
                        not_null: true,
                        identity: true,
                        file: e.rel.clone(),
                        line: e.line,
                        ..Default::default()
                    },
                );
                table
                    .constraints
                    .push(constraint(Kind::PrimaryKey(vec!["id".into()]), e.line));
            }
        }
        for (line, kind) in body {
            let kind = match kind {
                Kind::Unique(cols) => Kind::Unique(cols.iter().map(field_column).collect()),
                Kind::ForeignKey(mut fk)
                    if !fk
                        .columns
                        .iter()
                        .all(|c| table.columns.iter().any(|t| &t.name == c)) =>
                {
                    fk.columns = fk.columns.iter().map(field_column).collect();
                    Kind::ForeignKey(fk)
                }
                other => other,
            };
            table.constraints.push(constraint(kind, line));
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
    fn persistent_models_are_the_tables_they_define() {
        let corpus = Corpus::of(&[
            (
                "src/Model.hs",
                "share [mkPersist sqlSettings, mkMigrate \"migrateAll\"] [persistLowerCase|\nPerson\n    name String\n    age Int Maybe\n    UniqueName name\n    deriving Show\nBlogPost sql=posts\n    title String default='untitled'\n    authorId PersonId\n    deriving Show\n|]\n",
            ),
            (
                "config/models",
                "Tag\n    slug Text\n    Primary slug\n    -- a comment\n",
            ),
        ]);
        let sources: Vec<&Source> = corpus.files.iter().filter(|s| has_models(s)).collect();
        assert_eq!(sources.len(), 2);
        let schema = schema(&sources).unwrap();
        let person = &schema.tables["person"];
        let cols: Vec<(&str, &str, bool)> = person
            .columns
            .iter()
            .map(|c| (c.name.as_str(), c.ty.as_str(), c.not_null))
            .collect();
        assert_eq!(
            cols,
            [
                ("id", "id", true),
                ("name", "String", true),
                ("age", "Int", false)
            ]
        );
        assert_eq!(person.primary_key(), ["id"]);
        assert!(
            person
                .constraints
                .iter()
                .any(|c| c.kind == Kind::Unique(vec!["name".into()]))
        );
        let posts = &schema.tables["posts"];
        let fk = posts
            .constraints
            .iter()
            .find_map(|c| match &c.kind {
                Kind::ForeignKey(fk) => Some(fk.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            (fk.columns[0].as_str(), fk.table.label().as_str()),
            ("author_id", "person")
        );
        let title = posts.columns.iter().find(|c| c.name == "title").unwrap();
        assert_eq!(title.default.as_deref(), Some("'untitled'"));
        assert_eq!(schema.tables["tag"].primary_key(), ["slug"]);
    }
}
