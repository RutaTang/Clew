//! JPA's entities read as the tables they map, Java's and Kotlin's alike
//! (Hibernate, Spring Data): an `@Entity` class a table — `@Table(name =
//! …)`'s, else its name in snake case, as Spring Boot names it — its
//! fields the columns (`@Column(name = …)`'s name, `nullable = false` and
//! a primitive type not null, `unique = true` a unique column), its `@Id`
//! the primary key, a `@ManyToOne` or owning `@OneToOne` a column
//! (`@JoinColumn(name = …)`, else `<field>_id`) and a foreign key to the
//! entity it names. A `@MappedSuperclass`'s fields are its subclasses'.
//! A collection (`@OneToMany`, `@ManyToMany`), a `@Transient` field and a
//! static one are no column.

use std::collections::BTreeMap;

use super::ddl::{Column, Constraint, ForeignKey, Kind, QName};
use super::schema::{Schema, Table};
use crate::highlight::Lang;
use crate::maps::Source;

/// Whether a file may hold a JPA entity.
pub(crate) fn is_entity(source: &Source) -> bool {
    matches!(source.lang, Some(Lang::Java | Lang::Kotlin))
        && (source.text.contains("@Entity") || source.text.contains("@MappedSuperclass"))
        && source.text.contains("persistence")
}

/// A class as written: an entity, or a mapped superclass.
#[derive(Debug, Default)]
struct Class {
    name: String,
    /// `@Table(name = …)`, or `@Entity(name = …)`.
    table: Option<String>,
    entity: bool,
    parent: Option<String>,
    fields: Vec<Field>,
    rel: String,
    line: usize,
}

#[derive(Debug)]
struct Field {
    name: String,
    ty: String,
    annotations: Vec<String>,
    line: usize,
}

/// The schema the entities of `sources` map; none when they map no table.
pub(crate) fn schema(sources: &[&Source]) -> Option<Schema> {
    let classes: Vec<Class> = sources.iter().flat_map(|s| classes_in(s)).collect();
    let by_name: BTreeMap<&str, &Class> = classes.iter().map(|c| (c.name.as_str(), c)).collect();
    let table_of = |name: &str| -> String {
        match by_name.get(name) {
            Some(class) => table_name(class),
            None => super::snake_case(name),
        }
    };
    let mut schema = Schema::default();
    for class in classes.iter().filter(|c| c.entity) {
        // Its own fields, after those of the mapped superclasses above it.
        let mut fields: Vec<&Field> = Vec::new();
        let mut parents = Vec::new();
        let mut parent = class.parent.as_deref();
        while let Some(name) = parent {
            let Some(up) = by_name.get(name).filter(|c| !c.entity) else {
                break;
            };
            if parents.len() > 8 {
                break;
            }
            parents.push(*up);
            parent = up.parent.as_deref();
        }
        for up in parents.iter().rev() {
            fields.extend(&up.fields);
        }
        fields.extend(&class.fields);
        let table = table_of_class(class, &fields, &table_of);
        schema.tables.insert(table.name.key(), table);
    }
    (!schema.tables.is_empty()).then_some(schema)
}

fn table_name(class: &Class) -> String {
    class
        .table
        .clone()
        .unwrap_or_else(|| super::snake_case(&class.name))
}

fn table_of_class(class: &Class, fields: &[&Field], table_of: &dyn Fn(&str) -> String) -> Table {
    let mut table = Table {
        name: QName::bare(&table_name(class)),
        file: class.rel.clone(),
        line: class.line,
        ..Default::default()
    };
    let constraint = |kind: Kind, line: usize| Constraint {
        name: None,
        kind,
        file: class.rel.clone(),
        line,
    };
    let mut key = Vec::new();
    for field in fields {
        let has = |a: &str| field.annotations.iter().any(|x| annotation_is(x, a));
        let option = |a: &str, key: &str| {
            field
                .annotations
                .iter()
                .find(|x| annotation_is(x, a))
                .and_then(|x| argument(x, key))
        };
        if has("Transient")
            || has("OneToMany")
            || has("ManyToMany")
            || has("ElementCollection")
            || has("Embedded")
            || is_collection(&field.ty)
        {
            continue;
        }
        let one_to_one_owned = has("OneToOne") && option("OneToOne", "mappedBy").is_none();
        if has("ManyToOne") || one_to_one_owned {
            let name = option("JoinColumn", "name")
                .unwrap_or_else(|| format!("{}_id", super::snake_case(&field.name)));
            let not_null = option("JoinColumn", "nullable").as_deref() == Some("false")
                || option("ManyToOne", "optional").as_deref() == Some("false");
            table
                .columns
                .push(column(class, &name, "id", not_null, field.line));
            table.constraints.push(constraint(
                Kind::ForeignKey(ForeignKey {
                    columns: vec![name],
                    table: QName::bare(&table_of(base_type(&field.ty))),
                    references: Vec::new(),
                    on_delete: None,
                    on_update: None,
                    search: None,
                }),
                field.line,
            ));
            continue;
        }
        let name = option("Column", "name").unwrap_or_else(|| super::snake_case(&field.name));
        let ty = base_type(&field.ty).to_string();
        let id = has("Id");
        let not_null = id
            || is_primitive(&field.ty)
            || option("Column", "nullable").as_deref() == Some("false")
            || has("NotNull")
            || has("NonNull");
        let mut col = column(class, &name, &ty, not_null, field.line);
        col.identity = has("GeneratedValue");
        table.columns.push(col);
        if id {
            key.push(name.clone());
        }
        if option("Column", "unique").as_deref() == Some("true") {
            table
                .constraints
                .push(constraint(Kind::Unique(vec![name]), field.line));
        }
    }
    if !key.is_empty() {
        table
            .constraints
            .insert(0, constraint(Kind::PrimaryKey(key), class.line));
    }
    table
}

fn column(class: &Class, name: &str, ty: &str, not_null: bool, line: usize) -> Column {
    Column {
        name: name.to_string(),
        ty: ty.to_ascii_lowercase(),
        not_null,
        file: class.rel.clone(),
        line,
        ..Default::default()
    }
}

/// The classes of one file, with their fields: a field is a declaration at
/// a class body's own depth (Java's `Type name;`, Kotlin's `val`/`var`),
/// or a Kotlin primary constructor's `val`/`var` parameter.
fn classes_in(source: &Source) -> Vec<Class> {
    let kotlin = source.lang == Some(Lang::Kotlin);
    let mut out: Vec<Class> = Vec::new();
    // The class each brace depth's body is, and whether it is mapped.
    let mut bodies: Vec<(usize, Option<usize>)> = Vec::new();
    let mut depth = 0usize;
    let mut pending: Vec<String> = Vec::new();
    let mut in_constructor: Option<usize> = None;
    for (n, raw) in source.text.lines().enumerate() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        // Annotations alone on their line wait for what they annotate.
        let (annotations, rest) = leading_annotations(line);
        pending.extend(annotations);
        let rest = rest.trim();
        if let Some(name) = class_name(rest) {
            let mapped = pending
                .iter()
                .any(|a| annotation_is(a, "Entity") || annotation_is(a, "MappedSuperclass"));
            let entity = pending.iter().any(|a| annotation_is(a, "Entity"));
            let table = pending
                .iter()
                .find(|a| annotation_is(a, "Table"))
                .and_then(|a| argument(a, "name"))
                .or_else(|| {
                    pending
                        .iter()
                        .find(|a| annotation_is(a, "Entity"))
                        .and_then(|a| argument(a, "name"))
                });
            let index = mapped.then(|| {
                out.push(Class {
                    name: name.clone(),
                    table,
                    entity,
                    parent: parent_of(rest, kotlin),
                    rel: source.rel.clone(),
                    line: n + 1,
                    ..Default::default()
                });
                out.len() - 1
            });
            pending.clear();
            // A Kotlin primary constructor's parameters are its fields.
            if kotlin && let Some(index) = index {
                let after = &rest[rest.find(&name).unwrap_or(0) + name.len()..];
                if let Some(open) = after.find('(') {
                    let params = &after[open + 1..];
                    let closed = params.find(')');
                    push_kotlin_params(
                        &mut out[index],
                        &params[..closed.unwrap_or(params.len())],
                        n + 1,
                    );
                    if closed.is_none() {
                        in_constructor = Some(index);
                    }
                }
            }
            let opens = rest.matches('{').count();
            let closes = rest.matches('}').count();
            if opens > closes {
                bodies.push((depth + 1, index));
            }
            depth = (depth + opens).saturating_sub(closes);
            continue;
        }
        if let Some(index) = in_constructor {
            let closed = rest.find(')');
            let params = &rest[..closed.unwrap_or(rest.len())];
            // The annotations of this parameter, on its line or before it:
            // kept for it while a line holds none.
            let (inline, params) = leading_annotations(params.trim());
            if let Some(mut field) = kotlin_property(params, n + 1) {
                let mut annotations = std::mem::take(&mut pending);
                annotations.extend(inline);
                field.annotations = annotations;
                out[index].fields.push(field);
            } else {
                pending.extend(inline);
            }
            if let Some(at) = closed {
                in_constructor = None;
                let after = &rest[at..];
                let opens = after.matches('{').count();
                if opens > after.matches('}').count() {
                    bodies.push((depth + 1, Some(index)));
                    depth += 1;
                }
            }
            continue;
        }
        let at_body = bodies
            .last()
            .filter(|(d, _)| *d == depth)
            .and_then(|(_, index)| *index);
        if let Some(index) = at_body {
            let field = match kotlin {
                true => kotlin_property(rest, n + 1),
                false => java_field(rest, n + 1),
            };
            if let Some(mut field) = field {
                field.annotations = std::mem::take(&mut pending);
                out[index].fields.push(field);
            }
        }
        if !rest.is_empty() && !rest.starts_with('@') {
            pending.clear();
        }
        let opens = rest.matches('{').count();
        let closes = rest.matches('}').count();
        depth = (depth + opens).saturating_sub(closes);
        while bodies.last().is_some_and(|(d, _)| *d > depth) {
            bodies.pop();
        }
    }
    out
}

/// A line's leading annotations (`@Id @GeneratedValue(…)`), and the rest.
fn leading_annotations(line: &str) -> (Vec<String>, &str) {
    let mut out = Vec::new();
    let mut rest = line;
    while let Some(after) = rest.strip_prefix('@') {
        let name_len = after
            .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '.' || c == ':'))
            .unwrap_or(after.len());
        let mut end = 1 + name_len;
        if rest[end..].starts_with('(') {
            let mut level = 0;
            for (i, c) in rest[end..].char_indices() {
                match c {
                    '(' => level += 1,
                    ')' => {
                        level -= 1;
                        if level == 0 {
                            end += i + 1;
                            break;
                        }
                    }
                    _ => {}
                }
            }
        }
        out.push(rest[..end.min(rest.len())].to_string());
        rest = rest[end.min(rest.len())..].trim_start();
    }
    (out, rest)
}

/// Whether annotation `text` is `@name` (`@jakarta.persistence.Id` too).
fn annotation_is(text: &str, name: &str) -> bool {
    let head = text.trim_start_matches('@').split('(').next().unwrap_or("");
    head.rsplit(['.', ':']).next() == Some(name)
}

/// An annotation's argument `key` (`name = "orders"`): its value, unquoted.
fn argument(annotation: &str, key: &str) -> Option<String> {
    let inner = annotation.split_once('(')?.1.trim_end_matches(')');
    for part in inner.split(',') {
        let (k, v) = part.split_once('=')?;
        if k.trim() == key {
            return Some(v.trim().trim_matches('"').to_string());
        }
    }
    None
}

/// The class a line declares, by name.
fn class_name(line: &str) -> Option<String> {
    let words: Vec<&str> = line.split_whitespace().collect();
    let at = words.iter().position(|w| *w == "class")?;
    let name: String = words
        .get(at + 1)?
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    (!name.is_empty()).then_some(name)
}

/// The class a declaration extends: Java's `extends Base`, Kotlin's `:
/// Base(…)` after its constructor.
fn parent_of(line: &str, kotlin: bool) -> Option<String> {
    let after = match kotlin {
        true => {
            let close = line.rfind(')').unwrap_or(0);
            line[close..].split_once(':')?.1
        }
        false => line.split_once(" extends ")?.1,
    };
    let name: String = after
        .trim()
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '.')
        .collect();
    let name = name.rsplit('.').next().unwrap_or(&name).to_string();
    (!name.is_empty()).then_some(name)
}

/// A Java field (`private BigDecimal total = BigDecimal.ZERO;`).
fn java_field(line: &str, at: usize) -> Option<Field> {
    let decl = line.strip_suffix(';')?;
    let decl = decl.split('=').next()?.trim();
    if decl.contains('(') {
        return None;
    }
    let words: Vec<&str> = decl.split_whitespace().collect();
    if words.contains(&"static") {
        return None;
    }
    let words: Vec<&str> = words
        .into_iter()
        .filter(|w| {
            !matches!(
                *w,
                "private" | "protected" | "public" | "final" | "transient"
            )
        })
        .collect();
    let (name, ty) = words.split_last()?;
    if ty.is_empty() {
        return None;
    }
    Some(Field {
        name: name.to_string(),
        ty: ty.join(" "),
        annotations: Vec::new(),
        line: at,
    })
}

/// A Kotlin property (`var name: String? = null`).
fn kotlin_property(line: &str, at: usize) -> Option<Field> {
    let words: Vec<&str> = line.split_whitespace().collect();
    let at_word = words.iter().position(|w| *w == "val" || *w == "var")?;
    if words[..at_word]
        .iter()
        .any(|w| *w == "fun" || *w == "companion")
    {
        return None;
    }
    let rest = words[at_word + 1..].join(" ");
    let (name, ty) = rest.split_once(':')?;
    let ty = ty.split('=').next()?.trim().trim_end_matches(',');
    Some(Field {
        name: name.trim().to_string(),
        ty: ty.to_string(),
        annotations: Vec::new(),
        line: at,
    })
}

/// A Kotlin primary constructor's properties on one line.
fn push_kotlin_params(class: &mut Class, params: &str, at: usize) {
    for part in split_params(params) {
        let (annotations, rest) = leading_annotations(part.trim());
        if let Some(mut field) = kotlin_property(rest, at) {
            field.annotations = annotations;
            class.fields.push(field);
        }
    }
}

/// Parameters split at their top-level commas.
fn split_params(params: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut level, mut start) = (0i32, 0);
    for (i, c) in params.char_indices() {
        match c {
            '(' | '<' | '[' => level += 1,
            ')' | '>' | ']' => level -= 1,
            ',' if level == 0 => {
                out.push(&params[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&params[start..]);
    out
}

fn strip_comment(line: &str) -> &str {
    match line.find("//") {
        Some(at) if !line[..at].contains('"') => &line[..at],
        _ => line,
    }
}

/// A type without its nullability and package (`com.acme.User?`: `User`).
fn base_type(ty: &str) -> &str {
    let ty = ty.trim().trim_end_matches('?');
    ty.rsplit('.').next().unwrap_or(ty)
}

fn is_collection(ty: &str) -> bool {
    let head = base_type(ty).split('<').next().unwrap_or("");
    matches!(
        head,
        "List"
            | "Set"
            | "Collection"
            | "Map"
            | "MutableList"
            | "MutableSet"
            | "MutableMap"
            | "ArrayList"
            | "HashSet"
            | "LinkedHashSet"
            | "SortedSet"
    ) || ty.ends_with("[]") && !ty.starts_with("byte")
}

fn is_primitive(ty: &str) -> bool {
    matches!(
        ty.trim(),
        "int" | "long" | "short" | "byte" | "boolean" | "double" | "float" | "char"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::maps::Corpus;

    /// Java's and Kotlin's entities as tables: `@Table`'s name or the
    /// class's in snake case, `@Column`'s names and constraints, the `@Id`
    /// key, a `@ManyToOne`'s column and foreign key, a mapped superclass's
    /// fields; collections and transient fields are no column.
    #[test]
    fn jpa_entities_are_tables() {
        let corpus = Corpus::of(&[
            (
                "src/main/java/com/acme/BaseEntity.java",
                "package com.acme;\nimport jakarta.persistence.*;\n\n@MappedSuperclass\npublic abstract class BaseEntity {\n    @Id\n    @GeneratedValue(strategy = GenerationType.IDENTITY)\n    private Long id;\n}\n",
            ),
            (
                "src/main/java/com/acme/Order.java",
                "package com.acme;\nimport jakarta.persistence.*;\nimport java.util.List;\n\n@Entity\n@Table(name = \"orders\")\npublic class Order extends BaseEntity {\n    @Column(name = \"order_number\", nullable = false, unique = true)\n    private String number;\n    private int quantity;\n    @ManyToOne\n    @JoinColumn(name = \"customer_id\")\n    private Customer customer;\n    @OneToMany(mappedBy = \"order\")\n    private List<OrderLine> lines;\n    @Transient\n    private String note;\n    private static final long serialVersionUID = 1L;\n\n    public String getNumber() {\n        String local = number;\n        return local;\n    }\n}\n",
            ),
            (
                "src/main/kotlin/com/acme/Customer.kt",
                "package com.acme\n\nimport jakarta.persistence.*\n\n@Entity\nclass Customer(\n    @Id @GeneratedValue\n    val id: Long? = null,\n    @Column(nullable = false)\n    var fullName: String = \"\",\n    @ManyToOne var region: Region? = null,\n) {\n    var notes: String? = null\n    fun greet(): String {\n        val hello = \"hi\"\n        return hello\n    }\n}\n",
            ),
        ]);
        let sources: Vec<&Source> = corpus.files.iter().filter(|s| is_entity(s)).collect();
        let schema = schema(&sources).unwrap();
        let table = |name: &str| {
            schema
                .tables
                .values()
                .find(|t| t.name.name == name)
                .unwrap_or_else(|| panic!("{name}: {:?}", schema.tables.keys()))
        };
        let orders = table("orders");
        let cols: Vec<(&str, bool)> = orders
            .columns
            .iter()
            .map(|c| (c.name.as_str(), c.not_null))
            .collect();
        assert_eq!(
            cols,
            [
                ("id", true),
                ("order_number", true),
                ("quantity", true),
                ("customer_id", false)
            ]
        );
        assert_eq!(orders.primary_key(), ["id"]);
        assert!(orders.columns[0].identity);
        assert!(
            orders
                .foreign_keys()
                .any(|fk| fk.table.name == "customer" && fk.columns == ["customer_id"])
        );
        assert!(orders.constraints.iter().any(
            |c| matches!(&c.kind, Kind::Unique(cols) if cols == &["order_number".to_string()])
        ));
        let customer = table("customer");
        let cols: Vec<&str> = customer.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(cols, ["id", "full_name", "region_id", "notes"]);
        assert!(customer.columns[1].not_null);
        assert_eq!(customer.primary_key(), ["id"]);
        assert!(customer.foreign_keys().any(|fk| fk.table.name == "region"));
    }
}
