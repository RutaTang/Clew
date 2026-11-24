//! SQLAlchemy's declarative models (and Flask-SQLAlchemy's `db.Model`) read
//! as the tables they make: each class with a `__tablename__` (or made from
//! `db.Model`) a table, its `Column(…)`, `mapped_column(…)` and bare
//! `Mapped[…]` attributes the columns — type, `primary_key`, `nullable` or
//! `Optional[…]`, `unique`, `index`, `default`, `autoincrement`, `comment`
//! — each `ForeignKey("table.column")` a foreign key to the class whose
//! table it names, and `__table_args__`' `UniqueConstraint`s and `Index`es.

use std::collections::BTreeMap;

use tree_sitter::Node;

use super::ddl::{Column, Constraint, ForeignKey, Index, Kind, QName};
use super::schema::{Schema, Table};
use crate::highlight::Lang;
use crate::maps::{Source, all_of, last_name, line, text};

/// Whether a file may hold SQLAlchemy models.
pub(crate) fn is_models(source: &Source) -> bool {
    source.lang == Some(Lang::Python)
        && (source.text.contains("__tablename__") || source.text.contains("db.Model"))
        && (source.text.contains("Column(")
            || source.text.contains("mapped_column(")
            || source.text.contains("Mapped["))
}

/// `UserAccount` → `user_account`: Flask-SQLAlchemy's table for a class.
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

/// A model: its class, its table's name, its body.
struct Model<'t> {
    class: String,
    table: String,
    node: Node<'t>,
    rel: String,
    src: &'t str,
}

fn unquote(s: &str) -> &str {
    s.trim().trim_matches(['"', '\''])
}

/// A call's arguments: the positional ones (their text, and their node),
/// and the keyword ones' text.
#[allow(clippy::type_complexity)]
fn arguments<'t, 'a>(
    call: Node<'t>,
    src: &'a str,
) -> (Vec<(&'a str, Node<'t>)>, BTreeMap<&'a str, &'a str>) {
    let mut positional = Vec::new();
    let mut keyword = BTreeMap::new();
    let Some(args) = call.child_by_field_name("arguments") else {
        return (positional, keyword);
    };
    let mut cursor = args.walk();
    for a in args.named_children(&mut cursor) {
        match a.kind() {
            "keyword_argument" => {
                if let (Some(n), Some(v)) = (
                    a.child_by_field_name("name"),
                    a.child_by_field_name("value"),
                ) {
                    keyword.insert(text(n, src), text(v, src));
                }
            }
            "comment" => {}
            _ => positional.push((text(a, src), a)),
        }
    }
    (positional, keyword)
}

/// The schema the SQLAlchemy models of `sources` make; none when they make
/// no table.
pub(crate) fn schema(sources: &[&Source]) -> Option<Schema> {
    let trees: Vec<(tree_sitter::Tree, &Source)> = sources
        .iter()
        .filter_map(|s| Some((s.tree()?, *s)))
        .collect();
    let mut models: Vec<Model<'_>> = Vec::new();
    for (tree, source) in &trees {
        let src = source.text.as_str();
        for class in all_of(tree.root_node(), "class_definition") {
            let Some(name) = class.child_by_field_name("name") else {
                continue;
            };
            let bases = class
                .child_by_field_name("superclasses")
                .map_or("", |b| text(b, src));
            let Some(body) = class.child_by_field_name("body") else {
                continue;
            };
            let tablename = all_of(body, "assignment").into_iter().find_map(|a| {
                let left = a.child_by_field_name("left")?;
                let right = a.child_by_field_name("right")?;
                (text(left, src) == "__tablename__").then(|| unquote(text(right, src)).to_string())
            });
            let flask = bases.contains("db.Model");
            let table = match (tablename, flask) {
                (Some(t), _) => t,
                (None, true) => snake(text(name, src)),
                (None, false) => continue,
            };
            models.push(Model {
                class: text(name, src).to_string(),
                table,
                node: class,
                rel: source.rel.clone(),
                src,
            });
        }
    }
    if models.is_empty() {
        return None;
    }
    let class_of: BTreeMap<String, String> = models
        .iter()
        .map(|m| (m.table.to_lowercase(), m.class.clone()))
        .collect();
    let mut schema = Schema::default();
    for m in &models {
        let table = model_table(m, &class_of);
        schema.tables.insert(table.name.key(), table);
    }
    Some(schema)
}

/// The type an annotation gives (`Mapped[Optional[str]]` → `str`, may be
/// empty) and whether it may be none.
fn mapped(annotation: &str) -> Option<(String, bool)> {
    let inner = annotation
        .trim()
        .strip_prefix("Mapped[")?
        .strip_suffix(']')?
        .trim();
    let (inner, optional) = match inner
        .strip_prefix("Optional[")
        .and_then(|i| i.strip_suffix(']'))
    {
        Some(i) => (i, true),
        None => match inner.split_once('|') {
            Some((a, b)) if b.trim() == "None" => (a.trim(), true),
            _ => (inner, false),
        },
    };
    Some((inner.trim_matches(['"', '\'']).to_string(), optional))
}

fn model_table(m: &Model<'_>, class_of: &BTreeMap<String, String>) -> Table {
    let src = m.src;
    let mut table = Table {
        name: QName::bare(&m.class),
        comment: Some(format!("table {}", m.table)),
        file: m.rel.clone(),
        line: line(m.node),
        ..Default::default()
    };
    let constraint = |kind: Kind, line: usize| Constraint {
        name: None,
        kind,
        file: m.rel.clone(),
        line,
    };
    let Some(body) = m.node.child_by_field_name("body") else {
        return table;
    };
    let mut cursor = body.walk();
    for stmt in body.named_children(&mut cursor) {
        if stmt.kind() != "expression_statement" {
            continue;
        }
        let Some(a) = stmt.named_child(0).filter(|a| a.kind() == "assignment") else {
            continue;
        };
        let Some(left) = a.child_by_field_name("left") else {
            continue;
        };
        let name = text(left, src).to_string();
        let at = line(a);
        if name == "__table_args__" {
            let args = a.child_by_field_name("right").map_or("", |r| text(r, src));
            for part in args.split("UniqueConstraint(").skip(1) {
                let cols: Vec<String> = part
                    .split(')')
                    .next()
                    .unwrap_or("")
                    .split(',')
                    .filter(|p| !p.contains('='))
                    .map(|p| unquote(p).to_string())
                    .filter(|p| !p.is_empty())
                    .collect();
                if !cols.is_empty() {
                    table.constraints.push(constraint(Kind::Unique(cols), at));
                }
            }
            for part in args.split("Index(").skip(1) {
                let mut items = part.split(')').next().unwrap_or("").split(',').map(unquote);
                let index_name = items.next().map(str::to_string);
                let cols: Vec<String> = items
                    .filter(|p| !p.contains('=') && !p.is_empty())
                    .map(str::to_string)
                    .collect();
                if !cols.is_empty() {
                    table.indexes.push(Index {
                        name: index_name,
                        columns: cols,
                        unique: part.contains("unique=True"),
                        partial: false,
                        method: None,
                        concurrent: false,
                        file: m.rel.clone(),
                        line: at,
                    });
                }
            }
            continue;
        }
        if name.starts_with("__") {
            continue;
        }
        let annotation = a
            .child_by_field_name("type")
            .and_then(|t| mapped(text(t, src)));
        let call = a
            .child_by_field_name("right")
            .filter(|r| r.kind() == "call");
        let function = call
            .and_then(|c| c.child_by_field_name("function"))
            .map_or(String::new(), |f| last_name(text(f, src)));
        let is_column = matches!(function.as_str(), "Column" | "mapped_column");
        // `x: Mapped[int]` with nothing assigned is a column too; one
        // assigned anything else (a relationship) is not.
        let bare_annotation = annotation.is_some() && a.child_by_field_name("right").is_none();
        if !(is_column || bare_annotation) {
            continue;
        }
        let (positional, keyword) = match call.filter(|_| is_column) {
            Some(c) => arguments(c, src),
            None => (Vec::new(), BTreeMap::new()),
        };
        let truthy = |k: &str| keyword.get(k).copied() == Some("True");
        // `Column("db_name", String, …)`: the column's own name first.
        let mut positional = positional.into_iter().peekable();
        let column_name = match positional.peek() {
            Some((p, _)) if p.starts_with(['"', '\'']) => {
                let n = unquote(p).to_string();
                positional.next();
                n
            }
            _ => name.clone(),
        };
        let mut ty = None;
        let mut foreign = None;
        for (p, node) in positional {
            let head = node
                .child_by_field_name("function")
                .map_or(p.to_string(), |f| last_name(text(f, src)));
            if last_name(&head) == "ForeignKey" {
                if let Some(target) = node
                    .child_by_field_name("arguments")
                    .and_then(|args| args.named_child(0))
                {
                    foreign = Some((
                        unquote(text(target, src)).to_string(),
                        arguments(node, src)
                            .1
                            .get("ondelete")
                            .map(|v| unquote(v).to_lowercase()),
                    ));
                }
            } else if ty.is_none() {
                ty = Some(
                    last_name(p.split('(').next().unwrap_or(p))
                        + p.find('(').map_or("", |i| &p[i..]),
                );
            }
        }
        let (annotated, optional) = annotation.clone().unwrap_or_default();
        let ty = ty.unwrap_or(annotated);
        let primary = truthy("primary_key");
        let not_null = match keyword.get("nullable").copied() {
            Some("False") => true,
            Some("True") => false,
            _ => primary || (annotation.is_some() && !optional),
        };
        let column = Column {
            name: column_name.clone(),
            ty,
            not_null,
            default: keyword
                .get("default")
                .or(keyword.get("server_default"))
                .map(|d| d.to_string()),
            identity: primary && keyword.get("autoincrement").copied() != Some("False"),
            generated: None,
            comment: keyword
                .get("comment")
                .or(keyword.get("doc"))
                .map(|c| unquote(c).to_string()),
            file: m.rel.clone(),
            line: at,
        };
        if primary {
            // Several columns marked so are one key.
            let mut cols = table.primary_key().to_vec();
            cols.push(column_name.clone());
            table
                .constraints
                .retain(|c| !matches!(c.kind, Kind::PrimaryKey(_)));
            table
                .constraints
                .push(constraint(Kind::PrimaryKey(cols), at));
        }
        if truthy("unique") {
            table
                .constraints
                .push(constraint(Kind::Unique(vec![column_name.clone()]), at));
        }
        if truthy("index") {
            table.indexes.push(Index {
                name: None,
                columns: vec![column_name.clone()],
                unique: false,
                partial: false,
                method: None,
                concurrent: false,
                file: m.rel.clone(),
                line: at,
            });
        }
        if let Some((target, on_delete)) = foreign {
            // `"users.id"`: the table, then the column.
            let (target_table, target_column) =
                target.rsplit_once('.').unwrap_or((target.as_str(), ""));
            let target_table = target_table.rsplit('.').next().unwrap_or(target_table);
            let class = class_of
                .get(&target_table.to_lowercase())
                .cloned()
                .unwrap_or_else(|| target_table.to_string());
            table.constraints.push(constraint(
                Kind::ForeignKey(ForeignKey {
                    columns: vec![column_name.clone()],
                    table: QName::bare(&class),
                    references: match target_column {
                        "" => Vec::new(),
                        c => vec![c.to_string()],
                    },
                    on_delete,
                    on_update: None,
                    search: None,
                }),
                at,
            ));
        }
        table.columns.push(column);
    }
    table
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlalchemy_models_are_their_tables() {
        let text = r#"from sqlalchemy import Column, ForeignKey, Integer, String, UniqueConstraint
from sqlalchemy.orm import DeclarativeBase, Mapped, mapped_column, relationship


class Base(DeclarativeBase):
    pass


class User(Base):
    __tablename__ = "users"
    id = Column(Integer, primary_key=True)
    email = Column(String(120), unique=True, nullable=False)
    nickname = Column("nick", String(40))
    posts = relationship("Post", back_populates="author")


class Post(Base):
    __tablename__ = "posts"
    __table_args__ = (UniqueConstraint("title", "author_id"),)
    id: Mapped[int] = mapped_column(primary_key=True)
    title: Mapped[str] = mapped_column(String(200), index=True)
    author_id: Mapped[int] = mapped_column(ForeignKey("users.id", ondelete="CASCADE"))
    body: Mapped[Optional[str]]
    author: Mapped["User"] = relationship(back_populates="posts")
"#;
        let source = Source::new("app/models.py", Some(Lang::Python), text.to_string());
        assert!(is_models(&source));
        let schema = schema(&[&source]).expect("a schema");
        assert!(!schema.tables.contains_key("base"), "no table");
        let user = &schema.tables["user"];
        let cols: Vec<(&str, &str, bool)> = user
            .columns
            .iter()
            .map(|c| (c.name.as_str(), c.ty.as_str(), c.not_null))
            .collect();
        assert_eq!(
            cols,
            [
                ("id", "Integer", true),
                ("email", "String(120)", true),
                ("nick", "String(40)", false)
            ],
            "a relationship is no column"
        );
        assert!(user.columns[0].identity);
        assert!(user.is_unique("email"));
        let post = &schema.tables["post"];
        let cols: Vec<(&str, &str, bool)> = post
            .columns
            .iter()
            .map(|c| (c.name.as_str(), c.ty.as_str(), c.not_null))
            .collect();
        assert_eq!(
            cols,
            [
                ("id", "int", true),
                ("title", "String(200)", true),
                ("author_id", "int", true),
                ("body", "str", false),
            ]
        );
        let fk = post.foreign_keys().next().expect("a key");
        assert_eq!(
            (
                fk.table.name.as_str(),
                fk.references.as_slice(),
                fk.on_delete.as_deref()
            ),
            ("User", &["id".to_string()][..], Some("cascade"))
        );
        assert!(post.is_indexed("title"));
        assert!(
            post.constraints
                .iter()
                .any(|c| matches!(&c.kind, Kind::Unique(cols) if cols.len() == 2))
        );
    }

    #[test]
    fn flask_sqlalchemy_tables_are_named_for_their_classes() {
        let text = "class UserAccount(db.Model):\n    id = db.Column(db.Integer, primary_key=True)\n    team_id = db.Column(db.Integer, db.ForeignKey('team.id'))\n\nclass Team(db.Model):\n    id = db.Column(db.Integer, primary_key=True)\n";
        let source = Source::new("app.py", Some(Lang::Python), text.to_string());
        let schema = schema(&[&source]).expect("a schema");
        let account = &schema.tables["useraccount"];
        assert_eq!(account.comment.as_deref(), Some("table user_account"));
        let fk = account.foreign_keys().next().expect("a key");
        assert_eq!(fk.table.name, "Team");
        assert_eq!(account.columns[1].ty, "Integer");
    }
}
