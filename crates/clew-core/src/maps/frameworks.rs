//! What frameworks wire together, read from how they are declared:
//!
//! - **dependency injection** — Spring beans (`@Component`, `@Service`, …,
//!   `@Bean` methods) and what each is given (constructor parameters,
//!   `@Autowired`/`@Inject` fields), .NET's registrations
//!   (`services.AddScoped<IFoo, Foo>()`) and the constructors that take
//!   them, NestJS and Angular `@Injectable`s and their constructors;
//! - **ORM relations** — the entity graph: JPA's `@OneToMany` and friends,
//!   EF Core's navigation properties (of the `DbSet<>` entities), Eloquent's
//!   `hasMany`/`belongsTo`, ActiveRecord's `has_many`/`belongs_to`, Django's
//!   `ForeignKey`/`OneToOneField`/`ManyToManyField`, SQLAlchemy's
//!   `relationship`, TypeORM's `@ManyToOne` and friends;
//! - **WordPress hooks** — each hook, the files that fire it (`do_action`,
//!   `apply_filters`) and the callbacks hooked on (`add_action`,
//!   `add_filter`).

use std::collections::{BTreeMap, BTreeSet};

use clew_protocol::ProjectMap;
use tree_sitter::Node;

use super::{Corpus, MapBuilder, Source, all_of, line, text, unquote};
use crate::highlight::Lang;

pub fn maps(corpus: &Corpus) -> Vec<ProjectMap> {
    let mut out = Vec::new();
    out.extend(injection(corpus));
    out.extend(orm(corpus));
    out.extend(wordpress(corpus));
    out
}

/// The type a declared type is about: a collection's or wrapper's element
/// (`List<Order>` → `Order`), a namespace or array dropped.
fn element_type(written: &str) -> String {
    let t = written.trim().trim_end_matches("[]").trim_end_matches('?');
    if let (Some(open), Some(close)) = (t.find('<'), t.rfind('>'))
        && open < close
    {
        let inner = &t[open + 1..close];
        let last = inner.rsplit(',').next().unwrap_or(inner);
        return element_type(last);
    }
    t.rsplit(['.', ':', '\\'])
        .next()
        .unwrap_or(t)
        .trim()
        .to_string()
}

fn class_name(class: Node<'_>, src: &str) -> String {
    class
        .child_by_field_name("name")
        .map(|n| text(n, src).to_string())
        .unwrap_or_default()
}

// -- dependency injection ------------------------------------------------------------

const SPRING_BEANS: &[&str] = &[
    "Component",
    "Service",
    "Repository",
    "Controller",
    "RestController",
    "Configuration",
];

fn injection(corpus: &Corpus) -> Option<ProjectMap> {
    let mut map = MapBuilder::new(
        "di",
        "Dependency injection",
        "each component the container makes; an arrow to each dependency it is given (labelled with how), from an interface to its implementation, and from a configuration to the beans it provides",
    );
    // .NET: what each service type resolves to.
    let mut registered: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for source in corpus.in_langs(&[Lang::CSharp]) {
        let Some(tree) = source.tree() else {
            continue;
        };
        let src = source.text.as_str();
        for call in all_of(tree.root_node(), "invocation_expression") {
            let Some(name) = call
                .child_by_field_name("function")
                .filter(|f| f.kind() == "member_access_expression")
                .and_then(|f| f.child_by_field_name("name"))
                .filter(|n| n.kind() == "generic_name")
            else {
                continue;
            };
            let method = name.named_child(0).map(|n| text(n, src)).unwrap_or("");
            if !matches!(
                method,
                "AddScoped"
                    | "AddSingleton"
                    | "AddTransient"
                    | "AddHostedService"
                    | "TryAddScoped"
                    | "TryAddSingleton"
                    | "TryAddTransient"
            ) {
                continue;
            }
            let Some(list) = name.named_child(1) else {
                continue;
            };
            let mut cursor = list.walk();
            let types: Vec<String> = list
                .named_children(&mut cursor)
                .map(|t| element_type(text(t, src)))
                .collect();
            let (service, implementation) = match types.as_slice() {
                [one] => (one.clone(), one.clone()),
                [service, implementation, ..] => (service.clone(), implementation.clone()),
                [] => continue,
            };
            let lifetime = method
                .trim_start_matches("TryAdd")
                .trim_start_matches("Add")
                .to_ascii_lowercase();
            let s = map.node("service", &service, &service);
            if service != implementation {
                let i = map.node("component", &implementation, &implementation);
                map.edge(s, i, &format!("resolves to ({lifetime})"));
            } else {
                map.add_detail(s, &format!("{lifetime} at {}:{}", source.rel, line(call)));
            }
            map.place(s, &source.rel, line(call));
            registered.entry(service).or_default().push(implementation);
        }
    }
    for source in &corpus.files {
        match source.lang {
            Some(Lang::Java) => spring(&mut map, source),
            Some(Lang::CSharp) => dotnet_constructors(&mut map, source, &registered),
            Some(Lang::TypeScript | Lang::Tsx) => injectables(&mut map, source),
            _ => {}
        }
    }
    // Where each component is declared.
    let defs = corpus.defs();
    for at in 0..usize::MAX {
        let Some(node) = map.get(at) else {
            break;
        };
        if node.file.is_empty()
            && let Some(class) = defs.class(&node.label.clone())
        {
            let (rel, line) = (class.rel.clone(), class.line);
            map.place(Some(at), &rel, line);
        }
    }
    map.finish()
}

fn spring(map: &mut MapBuilder, source: &Source) {
    let Some(tree) = source.tree() else {
        return;
    };
    let src = source.text.as_str();
    for class in all_of(tree.root_node(), "class_declaration") {
        let names: Vec<String> = java_annotations(class, src);
        let Some(stereotype) = names.iter().find(|n| SPRING_BEANS.contains(&n.as_str())) else {
            continue;
        };
        let name = class_name(class, src);
        let bean = map.node("component", &name, &name);
        map.place(bean, &source.rel, line(class));
        map.add_detail(bean, &format!("@{stereotype}"));
        // An interface it implements resolves to it.
        if let Some(interfaces) = class.child_by_field_name("interfaces") {
            for t in all_of(interfaces, "type_identifier") {
                let i = text(t, src);
                let service = map.node("service", i, i);
                map.edge(service, bean, "resolves to");
            }
        }
        let Some(body) = class.child_by_field_name("body") else {
            continue;
        };
        let mut cursor = body.walk();
        for member in body.named_children(&mut cursor) {
            match member.kind() {
                "constructor_declaration" => {
                    for p in all_of(member, "formal_parameter") {
                        if let Some(t) = p.child_by_field_name("type") {
                            let dep = element_type(text(t, src));
                            let to = map.node("component", &dep, &dep);
                            map.edge(bean, to, "constructor");
                        }
                    }
                }
                "field_declaration" => {
                    let annotations = java_annotations(member, src);
                    if annotations
                        .iter()
                        .any(|a| matches!(a.as_str(), "Autowired" | "Inject" | "Resource"))
                        && let Some(t) = member.child_by_field_name("type")
                    {
                        let dep = element_type(text(t, src));
                        let to = map.node("component", &dep, &dep);
                        map.edge(bean, to, "field");
                    }
                }
                "method_declaration" => {
                    if java_annotations(member, src).iter().any(|a| a == "Bean")
                        && let Some(t) = member.child_by_field_name("type")
                    {
                        let made = element_type(text(t, src));
                        let to = map.node("component", &made, &made);
                        map.place(to, &source.rel, line(member));
                        map.edge(bean, to, "provides (@Bean)");
                    }
                }
                _ => {}
            }
        }
    }
}

fn java_annotations(declaration: Node<'_>, src: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cursor = declaration.walk();
    for child in declaration.children(&mut cursor) {
        if child.kind() == "modifiers" {
            let mut c = child.walk();
            for a in child.children(&mut c) {
                if matches!(a.kind(), "annotation" | "marker_annotation")
                    && let Some(n) = a.child_by_field_name("name")
                {
                    out.push(super::last_name(text(n, src)));
                }
            }
        }
    }
    out
}

fn dotnet_constructors(
    map: &mut MapBuilder,
    source: &Source,
    registered: &BTreeMap<String, Vec<String>>,
) {
    let Some(tree) = source.tree() else {
        return;
    };
    let src = source.text.as_str();
    for class in all_of(tree.root_node(), "class_declaration") {
        let name = class_name(class, src);
        for constructor in all_of(class, "constructor_declaration") {
            let deps: Vec<String> = all_of(constructor, "parameter")
                .into_iter()
                .filter_map(|p| p.child_by_field_name("type"))
                .map(|t| element_type(text(t, src)))
                .filter(|t| registered.contains_key(t))
                .collect();
            if deps.is_empty() {
                continue;
            }
            let from = map.node("component", &name, &name);
            map.place(from, &source.rel, line(class));
            for dep in deps {
                let to = map.node("service", &dep, &dep);
                map.edge(from, to, "constructor");
            }
        }
    }
}

fn injectables(map: &mut MapBuilder, source: &Source) {
    let Some(tree) = source.tree() else {
        return;
    };
    let src = source.text.as_str();
    for class in all_of(tree.root_node(), "class_declaration") {
        let decorators: Vec<String> = class_decorators(class, src);
        let Some(kind) = decorators.iter().find(|d| {
            matches!(
                d.as_str(),
                "Injectable" | "Controller" | "Component" | "Module" | "Resolver"
            )
        }) else {
            continue;
        };
        let name = class_name(class, src);
        let from = map.node("component", &name, &name);
        map.place(from, &source.rel, line(class));
        map.add_detail(from, &format!("@{kind}"));
        for method in all_of(class, "method_definition") {
            if method
                .child_by_field_name("name")
                .is_none_or(|n| text(n, src) != "constructor")
            {
                continue;
            }
            for param in all_of(method, "required_parameter")
                .into_iter()
                .chain(all_of(method, "optional_parameter"))
            {
                let Some(t) = param.child_by_field_name("type") else {
                    continue;
                };
                let dep = element_type(text(t, src).trim_start_matches(':'));
                if dep.is_empty() || dep.chars().next().is_some_and(|c| c.is_lowercase()) {
                    continue;
                }
                let to = map.node("component", &dep, &dep);
                map.edge(from, to, "constructor");
            }
        }
    }
}

/// The names of the decorators on a TS class (an export's included).
fn class_decorators(class: Node<'_>, src: &str) -> Vec<String> {
    let mut nodes = Vec::new();
    let mut cursor = class.walk();
    nodes.extend(
        class
            .children(&mut cursor)
            .filter(|c| c.kind() == "decorator"),
    );
    if let Some(parent) = class.parent().filter(|p| p.kind() == "export_statement") {
        let mut cursor = parent.walk();
        nodes.extend(
            parent
                .children(&mut cursor)
                .filter(|c| c.kind() == "decorator"),
        );
    }
    nodes
        .into_iter()
        .filter_map(|d| {
            let inner = d.named_child(0)?;
            let name = match inner.kind() {
                "call_expression" => inner.child_by_field_name("function")?,
                _ => inner,
            };
            Some(text(name, src).to_string())
        })
        .collect()
}

// -- ORM relations ---------------------------------------------------------------------

/// Rails' class for an association's name (`order_items` → `OrderItem`).
pub(crate) fn classify(name: &str) -> String {
    let singular = singularize(name);
    singular
        .split('_')
        .map(|w| {
            let mut c = w.chars();
            c.next()
                .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                .unwrap_or_default()
        })
        .collect()
}

fn singularize(word: &str) -> String {
    let w = word.to_string();
    if let Some(stem) = w.strip_suffix("ies") {
        return format!("{stem}y");
    }
    for suffix in ["sses", "xes", "ches", "shes", "zes"] {
        if let Some(stem) = w.strip_suffix(suffix) {
            return format!("{stem}{}", &suffix[..suffix.len() - 2]);
        }
    }
    if w.ends_with('s') && !w.ends_with("ss") {
        return w[..w.len() - 1].to_string();
    }
    w
}

struct Orm {
    map: MapBuilder,
}

impl Orm {
    fn entity(&mut self, name: &str, rel: &str, line: usize, framework: &str) -> Option<usize> {
        let at = self.map.node("entity", name, name);
        self.map.place(at, rel, line);
        if self
            .map
            .get(at.unwrap_or(usize::MAX))
            .is_some_and(|n| n.detail.is_empty())
        {
            self.map.detail(at, framework);
        }
        at
    }

    fn relate(&mut self, from: Option<usize>, target: &str, relation: &str, field: &str) {
        let to = self.map.node("entity", target, target);
        let label = match field.is_empty() {
            true => relation.to_string(),
            false => format!("{field}: {relation}"),
        };
        self.map.edge(from, to, &label);
    }
}

fn orm(corpus: &Corpus) -> Option<ProjectMap> {
    let mut orm = Orm {
        map: MapBuilder::new(
            "orm",
            "Entity relations",
            "each model or entity the ORM maps; an arrow to each it relates to, labelled with the field and how (one-to-many, belongs to, …)",
        ),
    };
    // EF Core: the entities are what a DbContext has `DbSet<>`s of.
    let mut ef_entities: BTreeSet<String> = BTreeSet::new();
    for source in corpus.in_langs(&[Lang::CSharp]) {
        if let Some(tree) = source.tree() {
            for p in all_of(tree.root_node(), "property_declaration") {
                if let Some(t) = p.child_by_field_name("type")
                    && text(t, &source.text).starts_with("DbSet<")
                {
                    ef_entities.insert(element_type(text(t, &source.text)));
                }
            }
        }
    }
    for source in &corpus.files {
        match source.lang {
            Some(Lang::Java) => jpa(&mut orm, source),
            Some(Lang::CSharp) => ef(&mut orm, source, &ef_entities),
            Some(Lang::Php) => eloquent(&mut orm, source),
            Some(Lang::Ruby) => active_record(&mut orm, source),
            Some(Lang::Python) => django_sqlalchemy(&mut orm, source),
            Some(Lang::TypeScript | Lang::Tsx) => typeorm(&mut orm, source),
            _ => {}
        }
    }
    orm.map.finish()
}

fn jpa(orm: &mut Orm, source: &Source) {
    let Some(tree) = source.tree() else {
        return;
    };
    let src = source.text.as_str();
    for class in all_of(tree.root_node(), "class_declaration") {
        if !java_annotations(class, src).iter().any(|a| a == "Entity") {
            continue;
        }
        let name = class_name(class, src);
        let from = orm.entity(&name, &source.rel, line(class), "JPA");
        for field in all_of(class, "field_declaration") {
            let relation =
                java_annotations(field, src)
                    .into_iter()
                    .find_map(|a| match a.as_str() {
                        "OneToMany" => Some("one-to-many"),
                        "ManyToOne" => Some("many-to-one"),
                        "OneToOne" => Some("one-to-one"),
                        "ManyToMany" => Some("many-to-many"),
                        _ => None,
                    });
            let (Some(relation), Some(t)) = (relation, field.child_by_field_name("type")) else {
                continue;
            };
            let target = element_type(text(t, src));
            let field_name = field
                .child_by_field_name("declarator")
                .and_then(|d| d.child_by_field_name("name"))
                .map(|n| text(n, src))
                .unwrap_or("");
            orm.relate(from, &target, relation, field_name);
        }
    }
}

fn ef(orm: &mut Orm, source: &Source, entities: &BTreeSet<String>) {
    let Some(tree) = source.tree() else {
        return;
    };
    let src = source.text.as_str();
    for class in all_of(tree.root_node(), "class_declaration") {
        let name = class_name(class, src);
        if !entities.contains(&name) {
            continue;
        }
        let from = orm.entity(&name, &source.rel, line(class), "EF Core");
        for p in all_of(class, "property_declaration") {
            let (Some(t), Some(field)) =
                (p.child_by_field_name("type"), p.child_by_field_name("name"))
            else {
                continue;
            };
            let written = text(t, src);
            let target = element_type(written);
            if !entities.contains(&target) {
                continue;
            }
            let many = written.contains('<') || written.ends_with("[]");
            orm.relate(
                from,
                &target,
                if many { "has many" } else { "has one" },
                text(field, src),
            );
        }
    }
}

fn eloquent(orm: &mut Orm, source: &Source) {
    let Some(tree) = source.tree() else {
        return;
    };
    let src = source.text.as_str();
    for class in all_of(tree.root_node(), "class_declaration") {
        let base = all_of(class, "base_clause")
            .first()
            .map(|b| text(*b, src).to_string())
            .unwrap_or_default();
        if !base.contains("Model") && !base.contains("Authenticatable") {
            continue;
        }
        let name = class_name(class, src);
        let from = orm.entity(&name, &source.rel, line(class), "Eloquent");
        for method in all_of(class, "method_declaration") {
            let field = method
                .child_by_field_name("name")
                .map(|n| text(n, src))
                .unwrap_or("");
            for call in all_of(method, "member_call_expression") {
                let relation = call
                    .child_by_field_name("name")
                    .map(|n| text(n, src))
                    .unwrap_or("");
                if !matches!(
                    relation,
                    "hasMany"
                        | "hasOne"
                        | "belongsTo"
                        | "belongsToMany"
                        | "morphMany"
                        | "morphTo"
                        | "hasManyThrough"
                        | "hasOneThrough"
                        | "morphToMany"
                ) {
                    continue;
                }
                let Some(target) = all_of(call, "class_constant_access_expression")
                    .first()
                    .map(|c| element_type(text(*c, src).trim_end_matches("::class")))
                else {
                    continue;
                };
                orm.relate(from, &target, relation, field);
            }
        }
    }
}

fn active_record(orm: &mut Orm, source: &Source) {
    let mut class: Option<(String, Option<usize>)> = None;
    for (n, raw) in source.text.lines().enumerate() {
        let line = raw.trim();
        if let Some(rest) = line.strip_prefix("class ") {
            let (name, base) = rest
                .split_once('<')
                .map_or((rest, ""), |(a, b)| (a.trim(), b.trim()));
            class = match base.contains("ApplicationRecord") || base.contains("ActiveRecord::Base")
            {
                true => {
                    let name = name.rsplit("::").next().unwrap_or(name).to_string();
                    let at = orm.entity(&name, &source.rel, n + 1, "ActiveRecord");
                    Some((name, at))
                }
                false => None,
            };
            continue;
        }
        let Some((_, at)) = &class else {
            continue;
        };
        let at = *at;
        let word = super::ident(line);
        let relation = match word {
            "has_many" | "has_one" | "belongs_to" | "has_and_belongs_to_many" => word,
            _ => continue,
        };
        let rest = line[word.len()..].trim();
        let Some(name) = rest
            .strip_prefix(':')
            .map(super::ident)
            .filter(|n| !n.is_empty())
        else {
            continue;
        };
        let target = option_value(rest, "class_name").unwrap_or_else(|| classify(name));
        orm.relate(at, &target, &relation.replace('_', " "), name);
    }
}

/// A Ruby `key: "Value"` option's value.
fn option_value(s: &str, key: &str) -> Option<String> {
    let at = s.find(&format!("{key}:"))?;
    let rest = s[at + key.len() + 1..].trim();
    let quote = rest.chars().next().filter(|c| *c == '"' || *c == '\'')?;
    rest[1..]
        .split(quote)
        .next()
        .map(|v| v.rsplit("::").next().unwrap_or(v).to_string())
}

fn django_sqlalchemy(orm: &mut Orm, source: &Source) {
    let Some(tree) = source.tree() else {
        return;
    };
    let src = source.text.as_str();
    for class in all_of(tree.root_node(), "class_definition") {
        let bases = class
            .child_by_field_name("superclasses")
            .map(|b| text(b, src).to_string())
            .unwrap_or_default();
        let django = bases.contains("Model");
        let sqlalchemy = bases.contains("Base")
            || bases.contains("DeclarativeBase")
            || bases.contains("db.Model");
        if !(django || sqlalchemy) {
            continue;
        }
        let name = class_name(class, src);
        let Some(body) = class.child_by_field_name("body") else {
            continue;
        };
        let mut from = None;
        for assignment in all_of(body, "assignment") {
            let (Some(left), Some(right)) = (
                assignment.child_by_field_name("left"),
                assignment.child_by_field_name("right"),
            ) else {
                continue;
            };
            if right.kind() != "call" {
                continue;
            }
            let function = right
                .child_by_field_name("function")
                .map(|f| text(f, src))
                .unwrap_or("");
            let relation = match super::last_name(function).as_str() {
                "ForeignKey" => "many-to-one",
                "OneToOneField" => "one-to-one",
                "ManyToManyField" => "many-to-many",
                "relationship" => "relationship",
                _ => continue,
            };
            let Some(first) = right
                .child_by_field_name("arguments")
                .and_then(|a| a.named_child(0))
            else {
                continue;
            };
            let target = match unquote(text(first, src)) {
                Some(s) if s == "self" => name.clone(),
                Some(s) => s.rsplit('.').next().unwrap_or(&s).to_string(),
                None if first.kind() == "identifier" || first.kind() == "attribute" => {
                    super::last_name(text(first, src))
                }
                None => continue,
            };
            if from.is_none() {
                from = orm.entity(
                    &name,
                    &source.rel,
                    line(class),
                    if django { "Django" } else { "SQLAlchemy" },
                );
            }
            orm.relate(from, &target, relation, text(left, src));
        }
        if from.is_none() && django && bases.contains("models.Model") {
            orm.entity(&name, &source.rel, line(class), "Django");
        }
    }
}

fn typeorm(orm: &mut Orm, source: &Source) {
    let Some(tree) = source.tree() else {
        return;
    };
    let src = source.text.as_str();
    for class in all_of(tree.root_node(), "class_declaration") {
        if !class_decorators(class, src).iter().any(|d| d == "Entity") {
            continue;
        }
        let name = class_name(class, src);
        let from = orm.entity(&name, &source.rel, line(class), "TypeORM");
        for field in all_of(class, "public_field_definition") {
            let field_name = field
                .child_by_field_name("name")
                .map(|n| text(n, src))
                .unwrap_or("");
            let mut cursor = field.walk();
            for decorator in field
                .children(&mut cursor)
                .filter(|c| c.kind() == "decorator")
            {
                let Some(call) = decorator
                    .named_child(0)
                    .filter(|c| c.kind() == "call_expression")
                else {
                    continue;
                };
                let relation = call
                    .child_by_field_name("function")
                    .map(|f| text(f, src))
                    .unwrap_or("");
                let relation = match relation {
                    "OneToMany" => "one-to-many",
                    "ManyToOne" => "many-to-one",
                    "OneToOne" => "one-to-one",
                    "ManyToMany" => "many-to-many",
                    _ => continue,
                };
                // `() => User`
                let Some(target) = all_of(call, "arrow_function")
                    .first()
                    .and_then(|f| f.child_by_field_name("body"))
                    .map(|b| text(b, src).to_string())
                else {
                    continue;
                };
                orm.relate(from, &element_type(&target), relation, field_name);
            }
        }
    }
}

// -- WordPress hooks ------------------------------------------------------------------------

fn wordpress(corpus: &Corpus) -> Option<ProjectMap> {
    let mut map = MapBuilder::new(
        "wp.hooks",
        "WordPress hooks",
        "each action and filter; an arrow from each file that fires it (do_action, apply_filters) to it, and from it to each callback hooked on (add_action, add_filter)",
    );
    for source in corpus.in_langs(&[Lang::Php]) {
        let Some(tree) = source.tree() else {
            continue;
        };
        let src = source.text.as_str();
        for call in all_of(tree.root_node(), "function_call_expression") {
            let Some(function) = call.child_by_field_name("function") else {
                continue;
            };
            let name = text(function, src);
            let fires = matches!(
                name,
                "do_action" | "apply_filters" | "do_action_ref_array" | "apply_filters_ref_array"
            );
            let hooks = matches!(name, "add_action" | "add_filter");
            if !(fires || hooks) {
                continue;
            }
            let args: Vec<Node> = call
                .child_by_field_name("arguments")
                .map(|a| {
                    let mut c = a.walk();
                    a.named_children(&mut c)
                        .filter_map(|x| {
                            x.named_child(x.named_child_count().saturating_sub(1) as u32)
                        })
                        .collect()
                })
                .unwrap_or_default();
            let Some(hook) = args.first().and_then(|a| unquote(text(*a, src))) else {
                continue;
            };
            let kind = if name.contains("filter") {
                "filter"
            } else {
                "action"
            };
            let h = map.node(kind, &hook, &hook);
            if fires {
                let file = map.node("file", &source.rel, &source.rel);
                map.place(file, &source.rel, line(call));
                map.place(h, &source.rel, line(call));
                map.edge(file, h, "fires");
                continue;
            }
            let callback = args.get(1);
            let (label, target) = match callback {
                Some(c) if c.kind() == "array_creation_expression" => {
                    let parts = all_of(*c, "array_element_initializer");
                    let method = parts
                        .get(1)
                        .and_then(|p| unquote(text(*p, src)))
                        .unwrap_or_default();
                    (method.clone(), Some(method))
                }
                Some(c) => match unquote(text(*c, src)) {
                    Some(f) => (f.clone(), Some(f)),
                    None => ("(closure)".to_string(), None),
                },
                None => continue,
            };
            let cb = match target.and_then(|t| corpus.defs().find(&t, None, &source.rel)) {
                Some(def) => {
                    let at = map.node("callback", &format!("{}:{}", def.rel, def.line), &label);
                    map.place(at, &def.rel.clone(), def.line);
                    at
                }
                None => {
                    let at = map.node(
                        "callback",
                        &format!("{}:{}", source.rel, line(call)),
                        &label,
                    );
                    map.place(at, &source.rel, line(call));
                    at
                }
            };
            map.edge(h, cb, "runs");
        }
    }
    map.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edges(map: &ProjectMap) -> Vec<(String, String, String)> {
        let mut out: Vec<_> = map
            .edges
            .iter()
            .map(|e| {
                (
                    map.nodes[e.from].label.clone(),
                    map.nodes[e.to].label.clone(),
                    e.label.clone(),
                )
            })
            .collect();
        out.sort();
        out
    }

    fn e(a: &str, b: &str, l: &str) -> (String, String, String) {
        (a.into(), b.into(), l.into())
    }

    fn map_of<'a>(maps: &'a [ProjectMap], id: &str) -> &'a ProjectMap {
        maps.iter()
            .find(|m| m.id == id)
            .unwrap_or_else(|| panic!("{id}"))
    }

    #[test]
    fn spring_dotnet_and_nest_injection() {
        let maps = maps(&Corpus::of(&[
            (
                "src/OrderService.java",
                "@Service\npublic class OrderService implements Orders {\n    @Autowired private PaymentClient payments;\n    public OrderService(OrderRepository repo, List<Notifier> notifiers) {}\n}\n",
            ),
            (
                "src/AppConfig.java",
                "@Configuration\nclass AppConfig {\n    @Bean\n    public PaymentClient paymentClient() { return null; }\n}\n",
            ),
            (
                "src/OrderRepository.java",
                "@Repository\ninterface OrderRepository {}\nclass Unmanaged { Unmanaged(Thing t) {} }\n",
            ),
            (
                "Program.cs",
                "services.AddScoped<IMailer, SmtpMailer>();\nservices.AddSingleton<Clock>();\n",
            ),
            (
                "Signup.cs",
                "public class Signup {\n    public Signup(IMailer mailer, Clock clock, string name) {}\n}\n",
            ),
            (
                "src/cats.service.ts",
                "@Injectable()\nexport class CatsService {\n  constructor(private readonly repo: CatsRepository, private readonly config: ConfigService) {}\n}\n",
            ),
        ]));
        let di = map_of(&maps, "di");
        assert!(di.is_well_formed());
        let got = edges(di);
        for want in [
            e("OrderService", "OrderRepository", "constructor"),
            e("OrderService", "Notifier", "constructor"),
            e("OrderService", "PaymentClient", "field"),
            e("Orders", "OrderService", "resolves to"),
            e("AppConfig", "PaymentClient", "provides (@Bean)"),
            e("IMailer", "SmtpMailer", "resolves to (scoped)"),
            e("Signup", "IMailer", "constructor"),
            e("Signup", "Clock", "constructor"),
            e("CatsService", "CatsRepository", "constructor"),
            e("CatsService", "ConfigService", "constructor"),
        ] {
            assert!(got.contains(&want), "{want:?} in {got:?}");
        }
        assert!(
            !got.iter().any(|(a, _, _)| a == "Unmanaged"),
            "not a bean: {got:?}"
        );
        assert!(
            !got.iter().any(|(_, b, _)| b == "string"),
            "unregistered: {got:?}"
        );
        let service = di.nodes.iter().find(|n| n.label == "OrderService").unwrap();
        assert_eq!(
            (service.file.as_str(), service.line),
            ("src/OrderService.java", 1)
        );
        assert_eq!(service.detail, "@Service");
        let paid = di
            .nodes
            .iter()
            .find(|n| n.label == "PaymentClient")
            .unwrap();
        assert_eq!(
            (paid.file.as_str(), paid.line),
            ("src/AppConfig.java", 3),
            "made by the @Bean"
        );
    }

    #[test]
    fn orm_relations_across_frameworks() {
        let maps = maps(&Corpus::of(&[
            (
                "src/Customer.java",
                "@Entity\npublic class Customer {\n    @OneToMany(mappedBy = \"customer\")\n    private List<Order> orders;\n    private String name;\n}\n",
            ),
            (
                "Data/ShopContext.cs",
                "class ShopContext : DbContext {\n    public DbSet<Invoice> Invoices { get; set; }\n    public DbSet<Line> Lines { get; set; }\n}\nclass Invoice {\n    public ICollection<Line> Lines { get; set; }\n    public string Note { get; set; }\n}\nclass Line {\n    public Invoice Invoice { get; set; }\n}\n",
            ),
            (
                "app/Models/Post.php",
                "<?php\nclass Post extends Model {\n    public function author() { return $this->belongsTo(User::class); }\n    public function comments() { return $this->hasMany(Comment::class); }\n}\n",
            ),
            (
                "app/models/order.rb",
                "class Order < ApplicationRecord\n  belongs_to :customer\n  has_many :order_items\n  has_one :receipt, class_name: \"Billing::Receipt\"\nend\n",
            ),
            (
                "library/models.py",
                "class Book(models.Model):\n    author = models.ForeignKey(Author, on_delete=models.CASCADE)\n    tags = models.ManyToManyField('Tag')\n    parent = models.ForeignKey('self', null=True)\n\nclass Author(models.Model):\n    name = models.CharField(max_length=80)\n",
            ),
            (
                "src/photo.ts",
                "@Entity()\nexport class Photo {\n  @ManyToOne(() => User, (user) => user.photos)\n  user: User;\n}\n",
            ),
        ]));
        let orm = map_of(&maps, "orm");
        let got = edges(orm);
        for want in [
            e("Customer", "Order", "orders: one-to-many"),
            e("Invoice", "Line", "Lines: has many"),
            e("Line", "Invoice", "Invoice: has one"),
            e("Post", "User", "author: belongsTo"),
            e("Post", "Comment", "comments: hasMany"),
            e("Order", "Customer", "customer: belongs to"),
            e("Order", "OrderItem", "order_items: has many"),
            e("Order", "Receipt", "receipt: has one"),
            e("Book", "Author", "author: many-to-one"),
            e("Book", "Tag", "tags: many-to-many"),
            e("Book", "Book", "parent: many-to-one"),
            e("Photo", "User", "user: many-to-one"),
        ] {
            assert!(got.contains(&want), "{want:?} in {got:?}");
        }
        assert!(
            orm.nodes
                .iter()
                .any(|n| n.label == "Author" && n.file == "library/models.py"),
            "a model with no relation is still one"
        );
        assert!(!got.iter().any(|(a, _, _)| a == "ShopContext"));
        assert_eq!(classify("order_items"), "OrderItem");
        assert_eq!(classify("categories"), "Category");
        assert_eq!(classify("boxes"), "Box");
        assert_eq!(element_type("Map<String, List<Order>>"), "Order");
        assert_eq!(
            element_type("System.Collections.Generic.ICollection<Line>"),
            "Line"
        );
    }

    #[test]
    fn wordpress_hooks() {
        let maps = maps(&Corpus::of(&[
            (
                "wp-content/plugins/shop/shop.php",
                "<?php\nadd_action('init', 'shop_register');\nadd_filter('the_content', [$this, 'decorate']);\nadd_action('shop_paid', function ($order) {});\nfunction shop_register() {}\n",
            ),
            (
                "wp-content/plugins/shop/checkout.php",
                "<?php\nclass Checkout {\n    public function decorate($c) { return $c; }\n    public function pay() { do_action('shop_paid', $this); $x = apply_filters('shop_total', 1); }\n}\n",
            ),
        ]));
        let hooks = map_of(&maps, "wp.hooks");
        let got = edges(hooks);
        for want in [
            e("init", "shop_register", "runs"),
            e("the_content", "decorate", "runs"),
            e("shop_paid", "(closure)", "runs"),
            e("wp-content/plugins/shop/checkout.php", "shop_paid", "fires"),
            e(
                "wp-content/plugins/shop/checkout.php",
                "shop_total",
                "fires",
            ),
        ] {
            assert!(got.contains(&want), "{want:?} in {got:?}");
        }
        let register = hooks
            .nodes
            .iter()
            .find(|n| n.label == "shop_register")
            .unwrap();
        assert_eq!(register.line, 5, "the callback at its definition");
        let total = hooks
            .nodes
            .iter()
            .find(|n| n.label == "shop_total")
            .unwrap();
        assert_eq!(total.kind, "filter");
    }
}
