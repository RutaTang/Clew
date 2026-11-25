//! Entity Framework Core's model read as the tables it maps: each
//! `DbSet<Order> Orders` of a `DbContext` a table — named `Orders`, as
//! EF names it, unless the entity's `[Table("…")]` or the context's
//! `.ToTable("…")` says otherwise — its entity class's properties the
//! columns (`[Column("…")]`'s name), `Id` or `<Class>Id` (or `[Key]`) the
//! primary key, a property of another entity's type a foreign key — on
//! its `<Name>Id` property, or the shadow column EF adds for it. A value
//! type, `[Required]` or `required` is not null; `T?` and a reference type
//! otherwise may be. Collections and `[NotMapped]` are no column; a base
//! class's properties are its subclasses'.

use std::collections::BTreeMap;

use super::ddl::{Column, Constraint, ForeignKey, Kind, QName};
use super::schema::{Schema, Table};
use crate::highlight::Lang;
use crate::maps::Source;

/// Whether a file declares a `DbContext`.
pub(crate) fn is_context(source: &Source) -> bool {
    source.lang == Some(Lang::CSharp)
        && source.text.contains("DbContext")
        && source.text.contains("DbSet<")
}

#[derive(Debug, Default)]
struct Class {
    name: String,
    table: Option<String>,
    parent: Option<String>,
    props: Vec<Prop>,
    rel: String,
    line: usize,
}

#[derive(Debug)]
struct Prop {
    name: String,
    ty: String,
    attributes: Vec<String>,
    required: bool,
    line: usize,
}

/// The schema the contexts among `contexts` map, their entities read from
/// `sources` (the project's C# files).
pub(crate) fn schema(contexts: &[&Source], sources: &[&Source]) -> Option<Schema> {
    // Each entity type, and the table its `DbSet` (or `ToTable`) names.
    let mut sets: Vec<(String, String)> = Vec::new();
    let mut to_table: BTreeMap<String, String> = BTreeMap::new();
    for context in contexts {
        for line in context.text.lines() {
            let t = line.trim();
            if let Some((entity, name)) = db_set(t)
                && !sets.iter().any(|(e, _)| *e == entity)
            {
                sets.push((entity, name));
            }
            if let Some(at) = t.find("Entity<") {
                let rest = &t[at + "Entity<".len()..];
                let entity: String = rest.chars().take_while(|c| *c != '>').collect();
                if let Some(table) = t
                    .find("ToTable(\"")
                    .map(|i| &t[i + "ToTable(\"".len()..])
                    .and_then(|r| r.split('"').next())
                {
                    to_table.insert(entity, table.to_string());
                }
            }
        }
    }
    if sets.is_empty() {
        return None;
    }
    let classes: Vec<Class> = sources.iter().flat_map(|s| classes_in(s)).collect();
    let by_name: BTreeMap<&str, &Class> = classes.iter().map(|c| (c.name.as_str(), c)).collect();
    let table_of = |entity: &str| -> Option<String> {
        let set = sets.iter().find(|(e, _)| e == entity)?;
        Some(
            to_table
                .get(entity)
                .cloned()
                .or_else(|| by_name.get(entity).and_then(|c| c.table.clone()))
                .unwrap_or_else(|| set.1.clone()),
        )
    };
    let mut schema = Schema::default();
    for (entity, _) in &sets {
        let Some(class) = by_name.get(entity.as_str()) else {
            continue;
        };
        let mut props: Vec<&Prop> = Vec::new();
        let mut chain = Vec::new();
        let mut parent = class.parent.as_deref();
        while let Some(name) = parent {
            let Some(up) = by_name.get(name) else {
                break;
            };
            if chain.len() > 8 {
                break;
            }
            chain.push(*up);
            parent = up.parent.as_deref();
        }
        for up in chain.iter().rev() {
            props.extend(&up.props);
        }
        props.extend(&class.props);
        let Some(name) = table_of(entity) else {
            continue;
        };
        let table = table_of_class(class, &name, &props, &table_of);
        schema.tables.insert(table.name.key(), table);
    }
    (!schema.tables.is_empty()).then_some(schema)
}

fn table_of_class(
    class: &Class,
    name: &str,
    props: &[&Prop],
    table_of: &dyn Fn(&str) -> Option<String>,
) -> Table {
    let mut table = Table {
        name: QName::bare(name),
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
    let has = |p: &Prop, a: &str| p.attributes.iter().any(|x| attribute_is(x, a));
    let names: Vec<&str> = props.iter().map(|p| p.name.as_str()).collect();
    let mut key = Vec::new();
    for prop in props {
        if has(prop, "NotMapped") || is_collection(&prop.ty) {
            continue;
        }
        let base = prop.ty.trim_end_matches('?');
        // A navigation: its foreign key, on `<Name>Id` (or the shadow
        // column EF adds when there is none).
        if let Some(target) = table_of(base) {
            let fk = prop
                .attributes
                .iter()
                .find(|a| attribute_is(a, "ForeignKey"))
                .and_then(|a| quoted(a))
                .unwrap_or_else(|| format!("{}Id", prop.name));
            if !names.contains(&fk.as_str()) {
                table
                    .columns
                    .push(column(class, &fk, "int", false, prop.line));
            }
            table.constraints.push(constraint(
                Kind::ForeignKey(ForeignKey {
                    columns: vec![fk],
                    table: QName::bare(&target),
                    references: Vec::new(),
                    on_delete: None,
                    on_update: None,
                    search: None,
                }),
                prop.line,
            ));
            continue;
        }
        let column_name = prop
            .attributes
            .iter()
            .find(|a| attribute_is(a, "Column"))
            .and_then(|a| quoted(a))
            .unwrap_or_else(|| prop.name.clone());
        let is_key =
            has(prop, "Key") || prop.name == "Id" || prop.name == format!("{}Id", class.name);
        let not_null = is_key
            || prop.required
            || has(prop, "Required")
            || (is_value_type(base) && !prop.ty.ends_with('?'));
        table
            .columns
            .push(column(class, &column_name, base, not_null, prop.line));
        if is_key {
            key.push(column_name);
        }
    }
    // A key named `Id` wins over `<Class>Id`, as EF finds it.
    if key.len() > 1 && key.iter().any(|k| k == "Id") {
        key.retain(|k| k == "Id");
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

/// `public DbSet<Order> Orders { get; set; }` (or `=> Set<Order>();`):
/// the entity and the set's name.
fn db_set(line: &str) -> Option<(String, String)> {
    let at = line.find("DbSet<")?;
    let rest = &line[at + "DbSet<".len()..];
    let close = rest.find('>')?;
    let entity = rest[..close].trim().to_string();
    let name: String = rest[close + 1..]
        .trim_start_matches('?')
        .trim_start()
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    (!name.is_empty() && !entity.is_empty()).then_some((entity, name))
}

/// The classes of one file, with their auto-properties.
fn classes_in(source: &Source) -> Vec<Class> {
    let mut out: Vec<Class> = Vec::new();
    let mut attributes: Vec<String> = Vec::new();
    for (n, raw) in source.text.lines().enumerate() {
        let mut t = raw.trim();
        if let Some(at) = t.find("//") {
            t = t[..at].trim();
        }
        // Attributes on their own line wait for what they mark.
        while t.starts_with('[') {
            let Some(end) = t.find(']') else {
                break;
            };
            attributes.push(t[..=end].to_string());
            t = t[end + 1..].trim();
        }
        if t.is_empty() {
            continue;
        }
        let words: Vec<&str> = t.split_whitespace().collect();
        if let Some(at) = words.iter().position(|w| *w == "class" || *w == "record") {
            let name: String = words
                .get(at + 1)
                .map(|w| {
                    w.chars()
                        .take_while(|c| c.is_alphanumeric() || *c == '_')
                        .collect()
                })
                .unwrap_or_default();
            let parent = t.split_once(':').map(|(_, r)| {
                r.trim()
                    .split([',', ' ', '{', '<'])
                    .next()
                    .unwrap_or("")
                    .to_string()
            });
            let table = attributes
                .iter()
                .find(|a| attribute_is(a, "Table"))
                .and_then(|a| quoted(a));
            out.push(Class {
                name,
                table,
                parent: parent.filter(|p| !p.is_empty()),
                rel: source.rel.clone(),
                line: n + 1,
                ..Default::default()
            });
            attributes.clear();
            continue;
        }
        if let (Some(class), Some(mut prop)) = (out.last_mut(), property(t, n + 1)) {
            prop.attributes = std::mem::take(&mut attributes);
            class.props.push(prop);
        }
        attributes.clear();
    }
    out
}

/// `public decimal? Total { get; set; }`: an auto-property, its name and
/// type; `required` said.
fn property(line: &str, at: usize) -> Option<Prop> {
    let brace = line.find('{')?;
    if !line[brace..].contains("get") {
        return None;
    }
    let head = &line[..brace];
    if head.contains('(') || head.contains("=>") {
        return None;
    }
    let words: Vec<&str> = head.split_whitespace().collect();
    if words
        .iter()
        .any(|w| matches!(*w, "static" | "const" | "abstract" | "override"))
    {
        return None;
    }
    let required = words.contains(&"required");
    let words: Vec<&str> = words
        .into_iter()
        .filter(|w| {
            !matches!(
                *w,
                "public" | "private" | "protected" | "internal" | "virtual" | "required" | "new"
            )
        })
        .collect();
    let (name, ty) = words.split_last()?;
    if ty.is_empty() {
        return None;
    }
    Some(Prop {
        name: name.to_string(),
        ty: ty.join(" "),
        attributes: Vec::new(),
        required,
        line: at,
    })
}

/// Whether `[text]` is the attribute `name` (`[Key]`, `[Column("x")]`).
fn attribute_is(text: &str, name: &str) -> bool {
    text.trim_matches(['[', ']']).split(',').any(|a| {
        let head = a.trim().split('(').next().unwrap_or("").trim();
        head == name || head == format!("{name}Attribute")
    })
}

fn quoted(text: &str) -> Option<String> {
    let start = text.find('"')? + 1;
    let len = text[start..].find('"')?;
    Some(text[start..start + len].to_string())
}

fn is_collection(ty: &str) -> bool {
    let head = ty.split('<').next().unwrap_or("");
    matches!(
        head,
        "ICollection"
            | "IList"
            | "List"
            | "IEnumerable"
            | "HashSet"
            | "ISet"
            | "IReadOnlyCollection"
            | "IReadOnlyList"
            | "Collection"
    ) || ty.ends_with("[]") && !ty.starts_with("byte")
}

fn is_value_type(ty: &str) -> bool {
    matches!(
        ty,
        "int"
            | "long"
            | "short"
            | "byte"
            | "bool"
            | "decimal"
            | "double"
            | "float"
            | "char"
            | "uint"
            | "ulong"
            | "ushort"
            | "sbyte"
            | "Guid"
            | "DateTime"
            | "DateTimeOffset"
            | "DateOnly"
            | "TimeOnly"
            | "TimeSpan"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::maps::Corpus;

    /// A context's sets as tables, by EF's conventions: the set's name,
    /// `Id` the key, a navigation's `<Name>Id` foreign key (a shadow one
    /// where the class has none), value types not null, collections no
    /// column, a base class's properties, `ToTable` and `[Column]` names.
    #[test]
    fn ef_core_sets_are_tables() {
        let corpus = Corpus::of(&[
            (
                "Data/ShopContext.cs",
                "using Microsoft.EntityFrameworkCore;\n\npublic class ShopContext : DbContext\n{\n    public DbSet<Customer> Customers { get; set; }\n    public DbSet<Order> Orders => Set<Order>();\n\n    protected override void OnModelCreating(ModelBuilder b)\n    {\n        b.Entity<Customer>().ToTable(\"clients\");\n    }\n}\n",
            ),
            (
                "Models/Entity.cs",
                "public abstract class Entity\n{\n    public int Id { get; set; }\n    public DateTime CreatedAt { get; set; }\n}\n",
            ),
            (
                "Models/Order.cs",
                "public class Order : Entity\n{\n    [Required]\n    [Column(\"order_number\")]\n    public string Number { get; set; } = \"\";\n    public decimal? Total { get; set; }\n    public int CustomerId { get; set; }\n    public Customer Customer { get; set; } = null!;\n    public List<Line> Lines { get; set; } = new();\n    [NotMapped]\n    public string Note { get; set; }\n}\n",
            ),
            (
                "Models/Customer.cs",
                "public class Customer\n{\n    public int CustomerId { get; set; }\n    public required string Name { get; set; }\n    public Order? LastOrder { get; set; }\n    public string Describe() => Name;\n}\n",
            ),
        ]);
        let contexts: Vec<&Source> = corpus.files.iter().filter(|s| is_context(s)).collect();
        let sources: Vec<&Source> = corpus.files.iter().collect();
        let schema = schema(&contexts, &sources).unwrap();
        let table = |name: &str| {
            schema
                .tables
                .values()
                .find(|t| t.name.name == name)
                .unwrap_or_else(|| panic!("{name}: {:?}", schema.tables.keys()))
        };
        let orders = table("Orders");
        let cols: Vec<(&str, bool)> = orders
            .columns
            .iter()
            .map(|c| (c.name.as_str(), c.not_null))
            .collect();
        assert_eq!(
            cols,
            [
                ("Id", true),
                ("CreatedAt", true),
                ("order_number", true),
                ("Total", false),
                ("CustomerId", true),
            ]
        );
        assert_eq!(orders.primary_key(), ["Id"]);
        assert!(
            orders
                .foreign_keys()
                .any(|fk| fk.table.name == "clients" && fk.columns == ["CustomerId"])
        );
        let clients = table("clients");
        let cols: Vec<&str> = clients.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(cols, ["CustomerId", "Name", "LastOrderId"]);
        assert_eq!(clients.primary_key(), ["CustomerId"]);
        assert!(clients.columns[1].not_null, "required");
        assert!(clients.foreign_keys().any(|fk| fk.table.name == "Orders"));
    }
}
