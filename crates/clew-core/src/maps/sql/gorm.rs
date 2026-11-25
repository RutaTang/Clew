//! GORM's models read as the tables they map: a struct that embeds
//! `gorm.Model` or tags its fields `gorm:"…"` (in a file using GORM) a
//! table — its `TableName()`'s, else its name in snake case, pluralized,
//! as GORM names it — its exported fields the columns (`column:`'s name,
//! else the field's in snake case: `UserID` `user_id`), `ID` or
//! `primaryKey` the key, `not null`, `unique` and `default:` as tagged; a
//! field of another model's type with its `<Name>ID` beside it a foreign
//! key on that column. `gorm.Model` is `id`, `created_at`, `updated_at`
//! and `deleted_at`; a slice of models and `gorm:"-"` are no column.

use std::collections::BTreeMap;

use super::ddl::{Column, Constraint, ForeignKey, Kind, QName};
use super::schema::{Schema, Table};
use crate::highlight::Lang;
use crate::maps::Source;

/// Whether a file may hold GORM models.
pub(crate) fn is_models(source: &Source) -> bool {
    source.lang == Some(Lang::Go)
        && (source.text.contains("gorm.io/gorm") || source.text.contains("jinzhu/gorm"))
}

#[derive(Debug, Default)]
struct Model {
    name: String,
    fields: Vec<Field>,
    /// It embeds `gorm.Model`.
    base: bool,
    tagged: bool,
    rel: String,
    line: usize,
}

#[derive(Debug)]
struct Field {
    name: String,
    ty: String,
    tag: String,
    line: usize,
}

/// The schema the models of `sources` map.
pub(crate) fn schema(sources: &[&Source]) -> Option<Schema> {
    let mut models: Vec<Model> = Vec::new();
    let mut names: BTreeMap<String, String> = BTreeMap::new();
    for source in sources {
        models.extend(structs_in(source));
        names.extend(table_names(source));
    }
    models.retain(|m| m.base || m.tagged);
    let known: Vec<String> = models.iter().map(|m| m.name.clone()).collect();
    let table_of = |model: &str| -> String {
        names
            .get(model)
            .cloned()
            .unwrap_or_else(|| super::pluralize(&super::snake_case(model)))
    };
    let mut schema = Schema::default();
    for model in &models {
        let table = table_of_model(model, &known, &table_of);
        schema.tables.insert(table.name.key(), table);
    }
    (!schema.tables.is_empty()).then_some(schema)
}

fn table_of_model(model: &Model, known: &[String], table_of: &dyn Fn(&str) -> String) -> Table {
    let mut table = Table {
        name: QName::bare(&table_of(&model.name)),
        file: model.rel.clone(),
        line: model.line,
        ..Default::default()
    };
    let constraint = |kind: Kind, line: usize| Constraint {
        name: None,
        kind,
        file: model.rel.clone(),
        line,
    };
    let column = |name: &str, ty: &str, not_null: bool, line: usize| Column {
        name: name.to_string(),
        ty: ty.to_string(),
        not_null,
        file: model.rel.clone(),
        line,
        ..Default::default()
    };
    let mut key = Vec::new();
    if model.base {
        let mut id = column("id", "uint", true, model.line);
        id.identity = true;
        table.columns.push(id);
        key.push("id".to_string());
        for name in ["created_at", "updated_at", "deleted_at"] {
            table
                .columns
                .push(column(name, "time.Time", false, model.line));
        }
    }
    let field_names: Vec<&str> = model.fields.iter().map(|f| f.name.as_str()).collect();
    for field in &model.fields {
        let settings = tag_settings(&field.tag);
        if settings.contains_key("-") {
            continue;
        }
        let base = field.ty.trim_start_matches('*');
        if base.starts_with("[]") && !base.starts_with("[]byte") {
            continue;
        }
        // An association: its foreign key is a field of its own.
        if known.iter().any(|k| k == base) {
            let fk = settings
                .get("foreignkey")
                .cloned()
                .unwrap_or_else(|| format!("{}ID", field.name));
            if field_names.contains(&fk.as_str()) {
                table.constraints.push(constraint(
                    Kind::ForeignKey(ForeignKey {
                        columns: vec![super::snake_case(&fk)],
                        table: QName::bare(&table_of(base)),
                        references: Vec::new(),
                        on_delete: None,
                        on_update: None,
                        search: None,
                    }),
                    field.line,
                ));
            }
            continue;
        }
        let name = settings
            .get("column")
            .cloned()
            .unwrap_or_else(|| super::snake_case(&field.name));
        let primary = settings.contains_key("primarykey")
            || settings.contains_key("primary_key")
            || field.name == "ID";
        let not_null = primary || settings.contains_key("not null");
        let mut col = column(&name, &field.ty, not_null, field.line);
        col.default = settings.get("default").cloned();
        col.identity = primary
            && settings.get("autoincrement").is_none_or(|v| v != "false")
            && matches!(
                base,
                "uint" | "int" | "int64" | "uint64" | "int32" | "uint32"
            );
        table.columns.push(col);
        if primary {
            key.push(name.clone());
        }
        if settings.contains_key("unique") || settings.contains_key("uniqueindex") {
            table
                .constraints
                .push(constraint(Kind::Unique(vec![name]), field.line));
        }
    }
    if !key.is_empty() {
        table
            .constraints
            .insert(0, constraint(Kind::PrimaryKey(key), model.line));
    }
    table
}

/// A `gorm:"…"` tag's settings, by their names lower-cased: `column:id`,
/// `not null`, `primaryKey`.
fn tag_settings(tag: &str) -> BTreeMap<String, String> {
    let Some(at) = tag.find("gorm:\"") else {
        return BTreeMap::new();
    };
    let rest = &tag[at + "gorm:\"".len()..];
    let body = &rest[..rest.find('"').unwrap_or(rest.len())];
    body.split(';')
        .filter(|s| !s.trim().is_empty())
        .map(|s| match s.split_once(':') {
            Some((k, v)) => (k.trim().to_ascii_lowercase(), v.trim().to_string()),
            None => (s.trim().to_ascii_lowercase(), String::new()),
        })
        .collect()
}

/// The structs of one file, with their fields.
fn structs_in(source: &Source) -> Vec<Model> {
    let mut out = Vec::new();
    let mut current: Option<Model> = None;
    for (n, raw) in source.text.lines().enumerate() {
        let t = raw.trim();
        if let Some(rest) = t.strip_prefix("type ")
            && t.ends_with("struct {")
        {
            let name: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            current = Some(Model {
                name,
                rel: source.rel.clone(),
                line: n + 1,
                ..Default::default()
            });
            continue;
        }
        let Some(model) = current.as_mut() else {
            continue;
        };
        if t == "}" {
            out.extend(current.take());
            continue;
        }
        let code = t.split("//").next().unwrap_or("").trim();
        if code == "gorm.Model" {
            model.base = true;
            continue;
        }
        let (decl, tag) = match code.find('`') {
            Some(at) => (code[..at].trim(), &code[at..]),
            None => (code, ""),
        };
        let words: Vec<&str> = decl.split_whitespace().collect();
        let [name, ty] = words.as_slice() else {
            continue;
        };
        if !name.chars().next().is_some_and(char::is_uppercase) {
            continue;
        }
        if tag.contains("gorm:\"") {
            model.tagged = true;
        }
        model.fields.push(Field {
            name: name.to_string(),
            ty: ty.to_string(),
            tag: tag.to_string(),
            line: n + 1,
        });
    }
    out
}

/// The tables `TableName()` methods name: `func (Order) TableName() string
/// { return "purchases" }`, on one line or over three.
fn table_names(source: &Source) -> Vec<(String, String)> {
    let lines: Vec<&str> = source.text.lines().collect();
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let t = line.trim();
        let Some(rest) = t.strip_prefix("func (") else {
            continue;
        };
        let Some((receiver, after)) = rest.split_once(')') else {
            continue;
        };
        if !after.trim_start().starts_with("TableName()") {
            continue;
        }
        let model = receiver
            .split_whitespace()
            .last()
            .unwrap_or("")
            .trim_start_matches('*')
            .to_string();
        let body = lines[i..lines.len().min(i + 4)].join(" ");
        if let Some(at) = body.find("return \"") {
            let rest = &body[at + "return \"".len()..];
            if let Some(end) = rest.find('"') {
                out.push((model, rest[..end].to_string()));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::maps::Corpus;

    /// Models as GORM makes their tables: pluralized snake-case names or
    /// `TableName()`'s, `gorm.Model`'s columns, tags' names and
    /// constraints, a belongs-to's foreign key, no column for a has-many.
    #[test]
    fn gorm_models_are_tables() {
        let corpus = Corpus::of(&[(
            "internal/models/models.go",
            "package models\n\nimport \"gorm.io/gorm\"\n\ntype UserProfile struct {\n\tgorm.Model\n\tEmail  string `gorm:\"uniqueIndex;not null\"`\n\tName   string\n\tOrders []Order\n}\n\ntype Order struct {\n\tID            uint   `gorm:\"primaryKey\"`\n\tTotal         int64  `gorm:\"column:total_cents;default:0\"`\n\tUserProfileID uint\n\tUserProfile   UserProfile\n\tNote          string `gorm:\"-\"`\n}\n\nfunc (Order) TableName() string {\n\treturn \"purchases\"\n}\n\ntype helper struct {\n\tx int\n}\n",
        )]);
        let sources: Vec<&Source> = corpus.files.iter().filter(|s| is_models(s)).collect();
        let schema = schema(&sources).unwrap();
        let table = |name: &str| {
            schema
                .tables
                .values()
                .find(|t| t.name.name == name)
                .unwrap_or_else(|| panic!("{name}: {:?}", schema.tables.keys()))
        };
        let users = table("user_profiles");
        let cols: Vec<(&str, bool)> = users
            .columns
            .iter()
            .map(|c| (c.name.as_str(), c.not_null))
            .collect();
        assert_eq!(
            cols,
            [
                ("id", true),
                ("created_at", false),
                ("updated_at", false),
                ("deleted_at", false),
                ("email", true),
                ("name", false),
            ]
        );
        assert_eq!(users.primary_key(), ["id"]);
        let orders = table("purchases");
        let cols: Vec<&str> = orders.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(cols, ["id", "total_cents", "user_profile_id"]);
        assert_eq!(orders.columns[1].default.as_deref(), Some("0"));
        assert!(
            orders
                .foreign_keys()
                .any(|fk| fk.table.name == "user_profiles" && fk.columns == ["user_profile_id"])
        );
        assert_eq!(schema.tables.len(), 2, "the untagged helper is no model");
    }

    #[test]
    fn names_are_snake_cased_as_gorm_names_them() {
        for (name, snake) in [
            ("UserID", "user_id"),
            ("OrderItem", "order_item"),
            ("HTTPServer", "http_server"),
            ("ID", "id"),
            ("createdAt", "created_at"),
        ] {
            assert_eq!(super::super::snake_case(name), snake, "{name}");
        }
    }
}
