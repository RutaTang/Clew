//! Django's models read as the tables they make: each concrete model a
//! table (an abstract one's fields given to the models made from it), its
//! fields the columns — `null=True`, `unique`, `primary_key`, `db_index`,
//! `default`, `auto_now`, `help_text` as the schema has them — an `id` key
//! when no field is one, each `ForeignKey` and `OneToOneField` a column
//! `<name>_id` and a foreign key (indexed, as Django makes it), and `Meta`'s
//! `db_table`, `indexes`, `unique_together` and `UniqueConstraint`s.

use std::collections::{BTreeMap, BTreeSet};

use tree_sitter::Node;

use super::ddl::{Column, Constraint, ForeignKey, Index, Kind, QName};
use super::schema::{Schema, Table};
use crate::highlight::Lang;
use crate::maps::{Source, all_of, last_name, line, text};

/// Whether a file may hold Django models.
pub(crate) fn is_models(source: &Source) -> bool {
    source.lang == Some(Lang::Python)
        && (source.rel.ends_with("models.py") || source.rel.contains("/models/"))
        && source.text.contains("models")
}

/// A model class as written.
struct Class<'t> {
    name: String,
    bases: Vec<String>,
    node: Node<'t>,
    rel: String,
    src: &'t str,
}

/// The schema the Django models of `sources` make; none when they make no
/// table.
pub(crate) fn schema(sources: &[&Source]) -> Option<Schema> {
    let trees: Vec<(tree_sitter::Tree, &Source)> = sources
        .iter()
        .filter_map(|s| Some((s.tree()?, *s)))
        .collect();
    let mut classes: Vec<Class<'_>> = Vec::new();
    for (tree, source) in &trees {
        for class in all_of(tree.root_node(), "class_definition") {
            // Models are top-level classes (a model's `Meta` is not one).
            if class.parent().is_some_and(|p| p.kind() != "module") {
                continue;
            }
            let bases = class
                .child_by_field_name("superclasses")
                .map(|b| {
                    let mut out = Vec::new();
                    let mut c = b.walk();
                    for arg in b.named_children(&mut c) {
                        if arg.kind() != "keyword_argument" {
                            out.push(text(arg, &source.text).to_string());
                        }
                    }
                    out
                })
                .unwrap_or_default();
            let Some(name) = class.child_by_field_name("name") else {
                continue;
            };
            classes.push(Class {
                name: text(name, &source.text).to_string(),
                bases,
                node: class,
                rel: source.rel.clone(),
                src: &source.text,
            });
        }
    }
    // Models: made from `models.Model`, or from another model.
    let mut models: BTreeSet<String> = BTreeSet::new();
    loop {
        let before = models.len();
        for c in &classes {
            if c.bases
                .iter()
                .any(|b| b == "models.Model" || b == "Model" || models.contains(&last_name(b)))
            {
                models.insert(c.name.clone());
            }
        }
        if models.len() == before {
            break;
        }
    }
    let by_name: BTreeMap<&str, &Class<'_>> = classes
        .iter()
        .filter(|c| models.contains(&c.name))
        .map(|c| (c.name.as_str(), c))
        .collect();
    let mut schema = Schema::default();
    for c in by_name.values() {
        if meta(c, "abstract").as_deref() == Some("True") {
            continue;
        }
        let table = model_table(c, &by_name);
        schema.tables.insert(table.name.key(), table);
    }
    (!schema.tables.is_empty()).then_some(schema)
}

/// A value `Meta` gives `key`, as written.
fn meta(c: &Class<'_>, key: &str) -> Option<String> {
    let body = c.node.child_by_field_name("body")?;
    let mut cursor = body.walk();
    let meta = body.named_children(&mut cursor).find(|n| {
        n.kind() == "class_definition"
            && n.child_by_field_name("name")
                .is_some_and(|m| text(m, c.src) == "Meta")
    })?;
    let meta_body = meta.child_by_field_name("body")?;
    all_of(meta_body, "assignment").into_iter().find_map(|a| {
        let left = a.child_by_field_name("left")?;
        let right = a.child_by_field_name("right")?;
        (text(left, c.src) == key).then(|| text(right, c.src).to_string())
    })
}

/// The strings and names in a list's text (`["a", "b"]`, `("a", "b")`).
fn quoted(list: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = list;
    while let Some(at) = rest.find(['"', '\'']) {
        let quote = rest.as_bytes()[at] as char;
        let after = &rest[at + 1..];
        let Some(end) = after.find(quote) else {
            break;
        };
        out.push(after[..end].to_string());
        rest = &after[end + 1..];
    }
    out
}

/// A call's keyword argument, as written.
fn keyword<'a>(call: Node<'_>, src: &'a str, name: &str) -> Option<&'a str> {
    let args = call.child_by_field_name("arguments")?;
    let mut cursor = args.walk();
    let found = args.named_children(&mut cursor).find_map(|a| {
        (a.kind() == "keyword_argument"
            && a.child_by_field_name("name")
                .is_some_and(|n| text(n, src) == name))
        .then(|| a.child_by_field_name("value"))
        .flatten()
    });
    found.map(|v| text(v, src))
}

/// A call's first positional argument, as written.
fn first_argument<'a>(call: Node<'_>, src: &'a str) -> Option<&'a str> {
    let args = call.child_by_field_name("arguments")?;
    let mut cursor = args.walk();
    let first = args
        .named_children(&mut cursor)
        .find(|a| a.kind() != "keyword_argument" && a.kind() != "comment");
    first.map(|f| text(f, src))
}

/// A model a relation names: `Author`, `"Author"`, `"books.Author"`,
/// `"self"`.
fn target(written: &str, own: &str) -> String {
    let t = written.trim_matches(['"', '\'']);
    match t {
        "self" => own.to_string(),
        _ => last_name(t),
    }
}

/// What a referenced row's deletion does, as SQL says it.
fn on_delete(written: &str) -> String {
    match last_name(written).as_str() {
        "CASCADE" => "cascade".into(),
        "SET_NULL" => "set null".into(),
        "SET_DEFAULT" => "set default".into(),
        "PROTECT" | "RESTRICT" => "restrict".into(),
        "DO_NOTHING" => "no action".into(),
        other => other.to_lowercase(),
    }
}

/// A model's fields, its bases' (an abstract model's, a parent's) first.
fn fields<'a>(
    c: &'a Class<'a>,
    by_name: &BTreeMap<&str, &'a Class<'a>>,
    seen: &mut BTreeSet<String>,
) -> Vec<(&'a Class<'a>, Node<'a>)> {
    let mut out = Vec::new();
    if !seen.insert(c.name.clone()) {
        return out;
    }
    for base in &c.bases {
        if let Some(parent) = by_name.get(last_name(base).as_str())
            && meta(parent, "abstract").as_deref() == Some("True")
        {
            out.extend(fields(parent, by_name, seen));
        }
    }
    let Some(body) = c.node.child_by_field_name("body") else {
        return out;
    };
    let mut cursor = body.walk();
    for stmt in body.named_children(&mut cursor) {
        if stmt.kind() != "expression_statement" {
            continue;
        }
        let Some(assignment) = stmt.named_child(0).filter(|a| a.kind() == "assignment") else {
            continue;
        };
        let Some(right) = assignment
            .child_by_field_name("right")
            .filter(|r| r.kind() == "call")
        else {
            continue;
        };
        let function = right
            .child_by_field_name("function")
            .map_or("", |f| text(f, c.src));
        let class = last_name(function);
        if class.ends_with("Field") || class == "ForeignKey" {
            out.push((c, assignment));
        }
    }
    out
}

fn model_table(c: &Class<'_>, by_name: &BTreeMap<&str, &Class<'_>>) -> Table {
    let src = c.src;
    let doc = c
        .node
        .child_by_field_name("body")
        .and_then(|b| b.named_child(0))
        .and_then(|s| {
            let string = s.named_child(0).filter(|n| n.kind() == "string")?;
            let t = text(string, src).trim_matches(['"', '\'']).trim();
            t.lines().next().map(str::to_string)
        });
    let mut table = Table {
        name: QName::bare(&c.name),
        comment: doc,
        file: c.rel.clone(),
        line: line(c.node),
        ..Default::default()
    };
    if let Some(db_table) = meta(c, "db_table") {
        let note = format!("table {}", db_table.trim_matches(['"', '\'']));
        table.comment = Some(match table.comment.take() {
            Some(d) => format!("{d} ({note})"),
            None => note,
        });
    }
    let constraint = |kind: Kind, rel: &str, line: usize| Constraint {
        name: None,
        kind,
        file: rel.to_string(),
        line,
    };
    // A parent model's table (multi-table inheritance): a key to it.
    for base in &c.bases {
        if let Some(parent) = by_name.get(last_name(base).as_str())
            && meta(parent, "abstract").as_deref() != Some("True")
        {
            let column = format!("{}_ptr_id", parent.name.to_lowercase());
            table.columns.push(Column {
                name: column.clone(),
                ty: "OneToOneField".into(),
                not_null: true,
                file: c.rel.clone(),
                line: line(c.node),
                ..Default::default()
            });
            table.constraints.push(constraint(
                Kind::PrimaryKey(vec![column.clone()]),
                &c.rel,
                line(c.node),
            ));
            table.constraints.push(constraint(
                Kind::ForeignKey(ForeignKey {
                    columns: vec![column],
                    table: QName::bare(&parent.name),
                    references: Vec::new(),
                    on_delete: Some("cascade".into()),
                    on_update: None,
                    search: None,
                }),
                &c.rel,
                line(c.node),
            ));
        }
    }
    let mut seen = BTreeSet::new();
    for (owner, assignment) in fields(c, by_name, &mut seen) {
        let src = owner.src;
        let (Some(left), Some(call)) = (
            assignment.child_by_field_name("left"),
            assignment.child_by_field_name("right"),
        ) else {
            continue;
        };
        let name = text(left, src).to_string();
        let class = last_name(
            call.child_by_field_name("function")
                .map_or("", |f| text(f, src)),
        );
        let at = line(assignment);
        let kw = |k: &str| keyword(call, src, k);
        let truthy = |k: &str| kw(k) == Some("True");
        if class == "ManyToManyField" {
            // A table of its own between the two: no column here.
            continue;
        }
        let relation = matches!(class.as_str(), "ForeignKey" | "OneToOneField");
        // A relation's column is `<name>_id`, as Django makes it.
        let column_name = match relation {
            true => format!("{name}_id"),
            false => name.clone(),
        };
        let size = |k: &str| kw(k).map(str::to_string);
        let ty = match (
            class.as_str(),
            size("max_length"),
            size("max_digits"),
            size("decimal_places"),
        ) {
            (_, Some(n), _, _) => format!("{class}({n})"),
            (_, _, Some(d), Some(p)) => format!("{class}({d}, {p})"),
            (_, _, _, _) => class.clone(),
        };
        let default = kw("default")
            .map(str::to_string)
            .or_else(|| (truthy("auto_now") || truthy("auto_now_add")).then(|| "now".to_string()));
        let column = Column {
            name: column_name.clone(),
            ty,
            not_null: !truthy("null"),
            default,
            identity: matches!(
                class.as_str(),
                "AutoField" | "BigAutoField" | "SmallAutoField"
            ),
            generated: None,
            comment: kw("help_text").map(|h| h.trim_matches(['"', '\'']).to_string()),
            file: owner.rel.clone(),
            line: at,
        };
        if truthy("primary_key") {
            table.constraints.push(constraint(
                Kind::PrimaryKey(vec![column_name.clone()]),
                &owner.rel,
                at,
            ));
        }
        if truthy("unique") || class == "OneToOneField" {
            table.constraints.push(constraint(
                Kind::Unique(vec![column_name.clone()]),
                &owner.rel,
                at,
            ));
        }
        if relation && let Some(t) = first_argument(call, src).or(kw("to")) {
            table.constraints.push(constraint(
                Kind::ForeignKey(ForeignKey {
                    columns: vec![column_name.clone()],
                    table: QName::bare(&target(t, &c.name)),
                    references: Vec::new(),
                    on_delete: kw("on_delete").map(on_delete),
                    on_update: None,
                    search: None,
                }),
                &owner.rel,
                at,
            ));
        }
        // Django indexes a foreign key's column unless told not to.
        if (relation && kw("db_index") != Some("False")) || truthy("db_index") {
            table.indexes.push(Index {
                name: None,
                columns: vec![column_name.clone()],
                unique: false,
                partial: false,
                method: None,
                concurrent: false,
                file: owner.rel.clone(),
                line: at,
            });
        }
        table.columns.push(column);
    }
    // No field the key: Django's own `id`.
    if table.primary_key().is_empty() {
        table.columns.insert(
            0,
            Column {
                name: "id".into(),
                ty: "AutoField".into(),
                not_null: true,
                identity: true,
                file: c.rel.clone(),
                line: line(c.node),
                ..Default::default()
            },
        );
        table.constraints.push(constraint(
            Kind::PrimaryKey(vec!["id".into()]),
            &c.rel,
            line(c.node),
        ));
    }
    // Meta's indexes and unique groups.
    if let Some(indexes) = meta(c, "indexes") {
        for part in indexes.split("Index(").skip(1) {
            let fields = part.split("fields").nth(1).map(quoted).unwrap_or_default();
            if !fields.is_empty() {
                table.indexes.push(Index {
                    name: part
                        .split("name")
                        .nth(1)
                        .and_then(|n| quoted(n).into_iter().next()),
                    columns: fields,
                    unique: false,
                    partial: part.contains("condition"),
                    method: None,
                    concurrent: false,
                    file: c.rel.clone(),
                    line: line(c.node),
                });
            }
        }
    }
    if let Some(together) = meta(c, "unique_together") {
        for group in together.split(['(', '[']).skip(1) {
            let cols = quoted(group.split([')', ']']).next().unwrap_or(""));
            if cols.len() > 1 {
                table
                    .constraints
                    .push(constraint(Kind::Unique(cols), &c.rel, line(c.node)));
            }
        }
    }
    if let Some(constraints) = meta(c, "constraints") {
        for part in constraints.split("UniqueConstraint(").skip(1) {
            let cols = part.split("fields").nth(1).map(quoted).unwrap_or_default();
            if !cols.is_empty() {
                table
                    .constraints
                    .push(constraint(Kind::Unique(cols), &c.rel, line(c.node)));
            }
        }
    }
    table
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn django_models_are_their_tables() {
        let text = r#"from django.db import models


class Stamped(models.Model):
    created = models.DateTimeField(auto_now_add=True)

    class Meta:
        abstract = True


class Author(Stamped):
    """Someone who writes."""

    name = models.CharField(max_length=100, help_text="As printed")
    email = models.EmailField(unique=True, null=True, blank=True)

    class Meta:
        db_table = "authors"
        indexes = [models.Index(fields=["name"], name="author_name")]


class Book(models.Model):
    isbn = models.CharField(primary_key=True, max_length=13)
    author = models.ForeignKey(
        Author,
        on_delete=models.CASCADE,
        related_name="books",
    )
    editor = models.ForeignKey("Author", on_delete=models.SET_NULL, null=True, related_name="+")
    price = models.DecimalField(max_digits=6, decimal_places=2, default=0)
    tags = models.ManyToManyField("Tag")
    objects = models.Manager()

    class Meta:
        unique_together = [("author", "price")]
"#;
        let source = Source::new("books/models.py", Some(Lang::Python), text.to_string());
        assert!(is_models(&source));
        let schema = schema(&[&source]).expect("a schema");
        assert!(!schema.tables.contains_key("stamped"), "abstract: no table");
        let author = &schema.tables["author"];
        let cols: Vec<(&str, &str, bool)> = author
            .columns
            .iter()
            .map(|c| (c.name.as_str(), c.ty.as_str(), c.not_null))
            .collect();
        assert_eq!(
            cols,
            [
                ("id", "AutoField", true),
                ("created", "DateTimeField", true),
                ("name", "CharField(100)", true),
                ("email", "EmailField", false),
            ]
        );
        assert_eq!(
            author.comment.as_deref(),
            Some("Someone who writes. (table authors)")
        );
        assert_eq!(author.columns[2].comment.as_deref(), Some("As printed"));
        assert_eq!(author.columns[1].default.as_deref(), Some("now"));
        assert!(author.is_unique("email"));
        assert!(author.is_indexed("name"));
        let book = &schema.tables["book"];
        let cols: Vec<&str> = book.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            cols,
            ["isbn", "author_id", "editor_id", "price"],
            "a key of its own; no m2m column, no manager"
        );
        assert_eq!(book.primary_key(), ["isbn"]);
        let keys: Vec<(String, String, Option<String>)> = book
            .foreign_keys()
            .map(|fk| {
                (
                    fk.columns[0].clone(),
                    fk.table.name.clone(),
                    fk.on_delete.clone(),
                )
            })
            .collect();
        assert_eq!(
            keys,
            [
                (
                    "author_id".to_string(),
                    "Author".to_string(),
                    Some("cascade".to_string())
                ),
                ("editor_id".into(), "Author".into(), Some("set null".into())),
            ]
        );
        assert!(book.is_indexed("author_id"), "Django indexes a foreign key");
        assert_eq!(book.columns[3].ty, "DecimalField(6, 2)");
        assert_eq!(book.columns[3].default.as_deref(), Some("0"));
    }
}
