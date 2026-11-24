use clew_protocol::{MapField, MapNode};

use super::*;

fn map<'a>(maps: &'a [ProjectMap], id: &str) -> &'a ProjectMap {
    maps.iter().find(|m| m.id == id).unwrap_or_else(|| {
        panic!(
            "{id} in {:?}",
            maps.iter().map(|m| &m.id).collect::<Vec<_>>()
        )
    })
}

fn node<'a>(map: &'a ProjectMap, label: &str) -> (usize, &'a MapNode) {
    map.nodes
        .iter()
        .enumerate()
        .find(|(_, n)| n.label == label)
        .unwrap_or_else(|| {
            panic!(
                "{label} in {:?}",
                map.nodes.iter().map(|n| &n.label).collect::<Vec<_>>()
            )
        })
}

fn field<'a>(map: &'a ProjectMap, table: &str, column: &str) -> &'a MapField {
    let (_, n) = node(map, table);
    n.fields
        .iter()
        .find(|f| f.name == column)
        .unwrap_or_else(|| {
            panic!(
                "{table}.{column} in {:?}",
                n.fields.iter().map(|f| &f.name).collect::<Vec<_>>()
            )
        })
}

/// A table's columns, each `name type` with its marks.
fn columns(map: &ProjectMap, table: &str) -> Vec<String> {
    node(map, table)
        .1
        .fields
        .iter()
        .map(|f| {
            let mut s = format!("{} {}", f.name, f.ty).trim().to_string();
            for (on, mark) in [
                (f.key, "key"),
                (!f.nullable, "not null"),
                (f.unique, "unique"),
                (f.identity, "identity"),
                (f.generated, "generated"),
            ] {
                if on {
                    s.push_str(&format!(" [{mark}]"));
                }
            }
            if !f.default.is_empty() {
                s.push_str(&format!(" = {}", f.default));
            }
            s
        })
        .collect()
}

fn edges(map: &ProjectMap) -> Vec<(String, String, String)> {
    map.edges
        .iter()
        .map(|e| {
            (
                map.nodes[e.from].label.clone(),
                map.nodes[e.to].label.clone(),
                e.label.clone(),
            )
        })
        .collect()
}

/// The column pairs each foreign key joins: `table.col → table.col`.
fn joins(map: &ProjectMap) -> Vec<String> {
    map.edges
        .iter()
        .flat_map(|e| {
            e.fields.iter().map(move |&(a, b)| {
                format!(
                    "{}.{} → {}.{}",
                    map.nodes[e.from].label,
                    map.nodes[e.from].fields[a].name,
                    map.nodes[e.to].label,
                    map.nodes[e.to].fields[b].name
                )
            })
        })
        .collect()
}

const MIGRATIONS: &[(&str, &str)] = &[
    (
        "db/migrations/001_users.up.sql",
        "-- users\nCREATE TABLE IF NOT EXISTS \"users\" (\n  id SERIAL PRIMARY KEY,\n  name VARCHAR(80) NOT NULL,\n  legacy TEXT\n);\n",
    ),
    ("db/migrations/001_users.down.sql", "DROP TABLE users;\n"),
    (
        "db/migrations/002_orders.sql",
        "/* orders */ CREATE TABLE public.orders (\n  id bigint,\n  user_id int REFERENCES users(id),\n  total numeric(10, 2) DEFAULT 0,\n  PRIMARY KEY (id)\n);\nCREATE TABLE items (id int PRIMARY KEY, order_id int, sku text,\n  CONSTRAINT items_order FOREIGN KEY (order_id) REFERENCES orders (id));\n-- +goose Down\nDROP TABLE orders;\n",
    ),
    (
        "db/migrations/003_changes.sql",
        "ALTER TABLE users ADD COLUMN email text, DROP COLUMN legacy;\nALTER TABLE items DROP CONSTRAINT items_order;\nALTER TABLE items ADD CONSTRAINT items_order2 FOREIGN KEY (order_id) REFERENCES orders(id);\nALTER TABLE users RENAME COLUMN name TO full_name;\nCREATE TABLE tmp (x int); DROP TABLE IF EXISTS tmp;\nCREATE FUNCTION f() RETURNS int AS $$ SELECT 1; CREATE TABLE ghost (x int); $$ LANGUAGE sql;\n",
    ),
    (
        "db/views.sql",
        "CREATE OR REPLACE VIEW big_orders AS\n  WITH totals AS (SELECT o.user_id, sum(o.total) t FROM orders o GROUP BY 1)\n  SELECT u.full_name, t.t FROM users u JOIN totals t ON t.user_id = u.id;\nCREATE MATERIALIZED VIEW report AS SELECT * FROM big_orders, items i;\nINSERT INTO archive (id) SELECT id FROM orders WHERE total > 100;\nCREATE TABLE archive AS SELECT * FROM orders;\n",
    ),
    (
        "app/repo.py",
        "def users():\n    return db.query(\"SELECT * FROM users WHERE id = %s\", 1)\n\ndef save(o):\n    db.run('INSERT INTO orders (id) VALUES (1)')\n    db.run(\"UPDATE nothing_here SET x = 1\")\n",
    ),
];

#[test]
fn the_schema_is_the_migrations_replayed() {
    let maps = maps(&Corpus::of(MIGRATIONS));
    let schema = map(&maps, "sql.schema");
    assert!(schema.is_well_formed());
    let (_, users) = node(schema, "users");
    assert_eq!(users.file, "db/migrations/001_users.up.sql");
    assert_eq!(users.line, 2);
    assert_eq!(
        columns(schema, "users"),
        [
            "id serial [key] [not null] [identity]",
            "full_name varchar(80) [not null]",
            "email text"
        ],
        "renamed, added, the dropped column gone"
    );
    assert_eq!(field(schema, "users", "id").line, 3);
    let email = field(schema, "users", "email");
    assert_eq!(
        (email.file.as_str(), email.line),
        ("db/migrations/003_changes.sql", 1),
        "a column added later is where it was added"
    );
    assert_eq!(
        columns(schema, "orders"),
        [
            "id bigint [key] [not null]",
            "user_id int",
            "total numeric(10, 2) = 0"
        ],
        "public.orders is orders: the default schema"
    );
    assert!(
        schema
            .nodes
            .iter()
            .all(|n| n.label != "tmp" && n.label != "ghost"),
        "dropped, or in a function body"
    );
    let e = edges(schema);
    assert!(
        e.contains(&("orders".into(), "users".into(), "user_id".into())),
        "{e:?}"
    );
    assert!(
        e.contains(&("items".into(), "orders".into(), "order_id".into())),
        "the re-added key: {e:?}"
    );
    assert_eq!(
        e.iter().filter(|(f, _, _)| f == "items").count(),
        1,
        "the dropped constraint gone: {e:?}"
    );
    let j = joins(schema);
    assert!(
        j.contains(&"orders.user_id → users.id".to_string()),
        "{j:?}"
    );
    assert!(
        j.contains(&"items.order_id → orders.id".to_string()),
        "{j:?}"
    );
    let (_, view) = node(schema, "big_orders");
    assert_eq!(view.kind, "view");
    assert!(
        e.contains(&("report".into(), "big_orders".into(), "reads".into())),
        "{e:?}"
    );
    assert!(
        e.contains(&("big_orders".into(), "users".into(), "reads".into())),
        "{e:?}"
    );
    assert!(
        !e.iter().any(|(_, to, _)| to == "totals"),
        "a CTE is no table: {e:?}"
    );
}

#[test]
fn history_lists_each_migration_and_what_it_changes() {
    let maps = maps(&Corpus::of(MIGRATIONS));
    let history = map(&maps, "sql.history");
    let labels: Vec<&str> = history
        .nodes
        .iter()
        .filter(|n| n.kind == "migration")
        .map(|n| n.label.as_str())
        .collect();
    assert_eq!(
        labels,
        [
            "001_users.up.sql",
            "002_orders.sql",
            "003_changes.sql",
            "views.sql"
        ],
        "in order, the down migration left out"
    );
    let (_, third) = node(history, "003_changes.sql");
    for want in [
        "+ column users.email text (line 1)",
        "- column users.legacy",
        "~ column users.name → full_name",
    ] {
        assert!(third.detail.contains(want), "{want} in {}", third.detail);
    }
    let (_, second) = node(history, "002_orders.sql");
    assert!(
        !second.detail.contains("- table orders"),
        "the goose Down part left out: {}",
        second.detail
    );
    let e = edges(history);
    assert!(e.contains(&(
        "001_users.up.sql".into(),
        "002_orders.sql".into(),
        "then".into()
    )));
    assert!(e.contains(&("003_changes.sql".into(), "users".into(), "alters".into())));
    assert!(e.contains(&("003_changes.sql".into(), "tmp".into(), "drops".into())));
}

#[test]
fn lineage_follows_views_inserts_and_ctes() {
    let maps = maps(&Corpus::of(MIGRATIONS));
    let lineage = map(&maps, "sql.lineage");
    let e = edges(lineage);
    for want in [
        ("orders", "totals", "select"),
        ("totals", "big_orders", "with"),
        ("users", "big_orders", "select"),
        ("big_orders", "report", "select"),
        ("items", "report", "select"),
        ("orders", "archive", "insert … select"),
        ("orders", "archive", "select"),
    ] {
        assert!(
            e.contains(&(want.0.into(), want.1.into(), want.2.into())),
            "{want:?} in {e:?}"
        );
    }
    assert_eq!(node(lineage, "totals").1.kind, "cte");
}

#[test]
fn code_is_linked_to_the_tables_its_sql_names() {
    let maps = maps(&Corpus::of(MIGRATIONS));
    let usage = map(&maps, "sql.usage");
    let e = edges(usage);
    assert!(
        e.contains(&("app/repo.py".into(), "users".into(), "reads".into())),
        "{e:?}"
    );
    assert!(
        e.contains(&("app/repo.py".into(), "orders".into(), "writes".into())),
        "{e:?}"
    );
    assert!(
        !e.iter().any(|(_, t, _)| t == "nothing_here"),
        "only known tables: {e:?}"
    );
    let (_, file) = node(usage, "app/repo.py");
    assert_eq!(file.line, 2);
    let (_, users) = node(usage, "users");
    assert_eq!(
        users.file, "db/migrations/001_users.up.sql",
        "it opens where it is defined"
    );
}

#[test]
fn rails_schema_and_migrations_are_read() {
    let corpus = Corpus::of(&[
        (
            "db/migrate/20240101000000_create_users.rb",
            "class CreateUsers < ActiveRecord::Migration[7.1]\n  def change\n    create_table :users do |t|\n      t.string :name, null: false, default: \"\"\n      t.timestamps\n    end\n    add_index :users, :name, unique: true\n  end\nend\n",
        ),
        (
            "db/migrate/20240201000000_create_posts.rb",
            "class CreatePosts < ActiveRecord::Migration[7.1]\n  def change\n    create_table :posts, id: :uuid do |t|\n      t.references :user, foreign_key: true\n      t.text :body\n      t.index [:user_id, :body]\n    end\n    add_column :users, :email, :string\n    add_reference :posts, :category, foreign_key: true\n    add_foreign_key :posts, :users, column: :editor_id\n  end\nend\n",
        ),
        (
            "app/models/user.rb",
            "class User < ApplicationRecord\nend\n",
        ),
    ]);
    let maps = maps(&corpus);
    let schema = map(&maps, "sql.schema");
    assert_eq!(
        columns(schema, "users"),
        [
            "id bigint [key] [not null] [identity]",
            "name string [not null] [unique] = \"\"",
            "created_at datetime [not null]",
            "updated_at datetime [not null]",
            "email string"
        ]
    );
    assert_eq!(field(schema, "posts", "id").ty, "uuid");
    let e = edges(schema);
    assert!(
        e.contains(&("posts".into(), "users".into(), "user_id".into())),
        "{e:?}"
    );
    assert!(
        e.contains(&("posts".into(), "categories".into(), "category_id".into())),
        "{e:?}"
    );
    assert!(
        e.contains(&("posts".into(), "users".into(), "editor_id".into())),
        "{e:?}"
    );
    assert!(field(schema, "posts", "user_id").indexed);
    let history = map(&maps, "sql.history");
    assert_eq!(
        history
            .nodes
            .iter()
            .filter(|n| n.kind == "migration")
            .count(),
        2
    );
    assert_eq!(pluralize("category"), "categories");
    assert_eq!(pluralize("box"), "boxes");
    assert_eq!(pluralize("day"), "days");
}

/// Supabase's storage migrations, as they are written: qualified names,
/// `IF NOT EXISTS` everywhere, changes inside `DO` blocks, generated
/// columns, row-level security and its policies.
#[test]
fn supabase_style_migrations_are_read_whole() {
    let corpus = Corpus::of(&[
        (
            "migrations/tenant/0002-storage-schema.sql",
            r#"DO $$
BEGIN
    IF NOT EXISTS(SELECT schema_name FROM information_schema.schemata WHERE schema_name = 'storage') THEN
        CREATE SCHEMA storage;
    END IF;
END$$;

CREATE TABLE IF NOT EXISTS "storage"."migrations" (
  id integer PRIMARY KEY,
  name varchar(100) UNIQUE NOT NULL,
  hash varchar(40) NOT NULL, -- sha1 hex encoded hash
  executed_at timestamp DEFAULT current_timestamp
);

CREATE TABLE IF NOT EXISTS "storage"."buckets" (
    "id" text not NULL,
    "name" text NOT NULL,
    type storage.BucketType NOT NULL default 'STANDARD',
    "owner" uuid,
    "created_at" timestamptz DEFAULT now(),
    PRIMARY KEY ("id")
);
CREATE UNIQUE INDEX IF NOT EXISTS "bname" ON "storage"."buckets" USING BTREE ("name");

CREATE TABLE IF NOT EXISTS "storage"."objects" (
    "id" uuid NOT NULL DEFAULT gen_random_uuid(),
    "bucket_id" text,
    "name" text,
    "metadata" jsonb,
    CONSTRAINT "objects_bucketId_fkey" FOREIGN KEY ("bucket_id") REFERENCES "storage"."buckets"("id"),
    PRIMARY KEY ("id")
);
CREATE UNIQUE INDEX IF NOT EXISTS "bucketid_objname" ON "storage"."objects" USING BTREE ("bucket_id","name");
ALTER TABLE storage.objects ENABLE ROW LEVEL SECURITY;
CREATE POLICY "read own" ON storage.objects FOR SELECT TO authenticated USING (auth.uid() = owner);
COMMENT ON COLUMN storage.objects.metadata IS 'what the uploader sent';
"#,
        ),
        (
            "migrations/tenant/0003-pathtoken-column.sql",
            "alter table storage.objects add column if not exists path_tokens text[] generated always as (string_to_array(\"name\", '/')) stored;\n",
        ),
        (
            "migrations/tenant/00010-later.sql",
            r#"DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM information_schema.columns WHERE table_name = 'objects' AND column_name = 'owner_id') THEN
        ALTER TABLE storage.objects ADD COLUMN owner_id text;
    END IF;
END$$;
CREATE TABLE IF NOT EXISTS storage.buckets (id int);
ALTER TABLE storage.buckets ALTER COLUMN owner TYPE text, ALTER COLUMN created_at SET NOT NULL;
CREATE TRIGGER update_objects_updated_at BEFORE UPDATE ON storage.objects FOR EACH ROW EXECUTE FUNCTION storage.update_updated_at_column();
"#,
        ),
        (
            "migrations/tenant/0004-add-owner.sql",
            "ALTER TABLE storage.objects ADD COLUMN IF NOT EXISTS owner uuid;\n",
        ),
    ]);
    let maps = maps(&corpus);
    let schema = map(&maps, "sql.schema");
    assert_eq!(
        columns(schema, "storage.migrations"),
        [
            "id integer [key] [not null]",
            "name varchar(100) [not null] [unique]",
            "hash varchar(40) [not null]",
            "executed_at timestamp = current_timestamp"
        ]
    );
    assert_eq!(
        columns(schema, "storage.buckets"),
        [
            "id text [key] [not null]",
            "name text [not null] [unique]",
            "type storage.buckettype [not null] = 'STANDARD'",
            "owner text",
            "created_at timestamptz [not null] = now()"
        ],
        "IF NOT EXISTS left the table as it was; ALTER COLUMN changed it"
    );
    assert_eq!(
        columns(schema, "storage.objects"),
        [
            "id uuid [key] [not null] = gen_random_uuid()",
            "bucket_id text",
            "name text",
            "metadata jsonb",
            "path_tokens text[] [generated] = string_to_array(name, '/')",
            "owner uuid",
            "owner_id text"
        ],
        "in the order the numbered migrations add them (0004 before 00010)"
    );
    assert_eq!(
        field(schema, "storage.objects", "metadata").doc,
        "what the uploader sent"
    );
    assert!(field(schema, "storage.objects", "bucket_id").indexed);
    assert!(
        !field(schema, "storage.objects", "bucket_id").unique,
        "unique only with name"
    );
    assert_eq!(
        joins(schema),
        ["storage.objects.bucket_id → storage.buckets.id"]
    );
    let (_, objects) = node(schema, "storage.objects");
    for want in [
        "unique index bucketid_objname (bucket_id, name)",
        "row level security",
        "policy read own: select to authenticated using (auth.uid() = owner)",
        "trigger update_objects_updated_at: before update → storage.update_updated_at_column",
    ] {
        assert!(
            objects.detail.lines().any(|l| l == want),
            "{want} in\n{}",
            objects.detail
        );
    }
}

/// What `pg_dump` writes: keys, defaults and foreign keys added after the
/// tables, sequences for `serial`, enum types, comments.
#[test]
fn a_pg_dump_is_read_as_the_schema_it_restores() {
    let dump = r#"
SET search_path = public, pg_catalog;
CREATE TYPE public.status AS ENUM (
    'draft',
    'live'
);
ALTER TYPE public.status ADD VALUE 'archived' AFTER 'live';
CREATE TABLE public.users (
    id integer NOT NULL,
    email character varying(255) NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);
COMMENT ON TABLE public.users IS 'People who sign in.';
CREATE SEQUENCE public.users_id_seq AS integer START WITH 1 INCREMENT BY 1;
ALTER SEQUENCE public.users_id_seq OWNED BY public.users.id;
CREATE TABLE public.posts (
    id bigint NOT NULL,
    author_id integer,
    state public.status DEFAULT 'draft'::public.status NOT NULL
);
ALTER TABLE ONLY public.users ALTER COLUMN id SET DEFAULT nextval('public.users_id_seq'::regclass);
ALTER TABLE ONLY public.users
    ADD CONSTRAINT users_pkey PRIMARY KEY (id);
ALTER TABLE ONLY public.posts
    ADD CONSTRAINT posts_pkey PRIMARY KEY (id);
ALTER TABLE ONLY public.users
    ADD CONSTRAINT users_email_key UNIQUE (email);
CREATE INDEX index_posts_on_author_id ON public.posts USING btree (author_id);
ALTER TABLE ONLY public.posts
    ADD CONSTRAINT fk_rails_1 FOREIGN KEY (author_id) REFERENCES public.users(id) ON DELETE CASCADE;
"#;
    let maps = maps(&Corpus::of(&[("db/structure.sql", dump)]));
    let schema = map(&maps, "sql.schema");
    assert_eq!(
        columns(schema, "users"),
        [
            "id integer [key] [not null] [identity] = nextval('public.users_id_seq'::regclass)",
            "email character varying(255) [not null] [unique]",
            "created_at timestamp with time zone [not null] = now()"
        ]
    );
    assert_eq!(node(schema, "users").1.detail, "People who sign in.");
    let (_, status) = node(schema, "status");
    assert_eq!(status.kind, "enum");
    assert_eq!(
        status
            .fields
            .iter()
            .map(|f| f.name.as_str())
            .collect::<Vec<_>>(),
        ["draft", "live", "archived"]
    );
    let e = edges(schema);
    assert!(
        e.contains(&(
            "posts".into(),
            "users".into(),
            "author_id (on delete cascade)".into()
        )),
        "{e:?}"
    );
    assert!(
        e.contains(&("posts".into(), "status".into(), "state".into())),
        "{e:?}"
    );
    assert!(field(schema, "posts", "author_id").indexed);
}

#[test]
fn a_mysql_dump_is_read() {
    let dump = r#"
/*!40101 SET NAMES utf8mb4 */;
DROP TABLE IF EXISTS `accounts`;
CREATE TABLE `accounts` (
  `id` int unsigned NOT NULL AUTO_INCREMENT,
  `email` varchar(191) COLLATE utf8mb4_unicode_ci NOT NULL COMMENT 'sign-in address',
  `plan` enum('free','pro') NOT NULL DEFAULT 'free',
  `updated_at` timestamp NULL DEFAULT NULL ON UPDATE CURRENT_TIMESTAMP,
  `key` varchar(64) DEFAULT NULL,
  PRIMARY KEY (`id`),
  UNIQUE KEY `accounts_email_unique` (`email`),
  KEY `accounts_plan_index` (`plan`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COMMENT='Paying customers';
CREATE TABLE `invoices` (
  `id` bigint NOT NULL AUTO_INCREMENT,
  `account_id` int unsigned NOT NULL,
  PRIMARY KEY (`id`),
  CONSTRAINT `invoices_account_id_foreign` FOREIGN KEY (`account_id`) REFERENCES `accounts` (`id`) ON DELETE CASCADE
) ENGINE=InnoDB;
DELIMITER ;;
CREATE TRIGGER `stamp` BEFORE INSERT ON `invoices` FOR EACH ROW BEGIN SET NEW.id = 1; END ;;
DELIMITER ;
ALTER TABLE `invoices` ADD COLUMN `total` decimal(10,2) NOT NULL DEFAULT '0.00' AFTER `account_id`;
ALTER TABLE `accounts` MODIFY `plan` varchar(16) NOT NULL, CHANGE `key` `api_key` varchar(64) NULL;
"#;
    let maps = maps(&Corpus::of(&[("dump.sql", dump)]));
    let schema = map(&maps, "sql.schema");
    assert_eq!(
        columns(schema, "accounts"),
        [
            "id int unsigned [key] [not null] [identity]",
            "email varchar(191) [not null] [unique]",
            "plan varchar(16) [not null]",
            "updated_at timestamp = NULL",
            "api_key varchar(64)"
        ]
    );
    assert_eq!(field(schema, "accounts", "email").doc, "sign-in address");
    assert!(
        field(schema, "accounts", "plan").indexed,
        "the index follows the column through MODIFY"
    );
    assert_eq!(
        node(schema, "accounts").1.detail.lines().next(),
        Some("Paying customers")
    );
    assert_eq!(
        columns(schema, "invoices"),
        [
            "id bigint [key] [not null] [identity]",
            "account_id int unsigned [not null]",
            "total decimal(10, 2) [not null] = '0.00'"
        ]
    );
    assert_eq!(joins(schema), ["invoices.account_id → accounts.id"]);
    assert!(
        node(schema, "invoices")
            .1
            .detail
            .contains("trigger stamp: before insert")
    );
}

#[test]
fn sqlite_and_sql_server_are_read() {
    let sqlite = "CREATE TABLE notes (id INTEGER PRIMARY KEY AUTOINCREMENT, body TEXT NOT NULL, tag_id INTEGER REFERENCES tags);\nCREATE TABLE tags (id INTEGER PRIMARY KEY, name TEXT UNIQUE);\nALTER TABLE notes RENAME TO memos;\n";
    let built = maps(&Corpus::of(&[("app.sql", sqlite)]));
    let schema = map(&built, "sql.schema");
    assert_eq!(
        columns(schema, "memos"),
        [
            "id integer [key] [not null] [identity]",
            "body text [not null]",
            "tag_id integer"
        ]
    );
    assert_eq!(
        joins(schema),
        ["memos.tag_id → tags.id"],
        "a reference with no columns is to the primary key"
    );

    let tsql = "CREATE TABLE [dbo].[Customers] (\n  [Id] INT IDENTITY(1,1) NOT NULL,\n  [Name] NVARCHAR(MAX) NULL,\n  CONSTRAINT [PK_Customers] PRIMARY KEY CLUSTERED ([Id] ASC)\n)\nGO\nCREATE TABLE [sales].[Orders] ([Id] INT NOT NULL PRIMARY KEY, [CustomerId] INT NOT NULL)\nGO\nALTER TABLE [sales].[Orders] ADD CONSTRAINT [FK_Orders_Customers] FOREIGN KEY ([CustomerId]) REFERENCES [dbo].[Customers] ([Id])\nGO\n";
    let built = maps(&Corpus::of(&[("db/schema.sql", tsql)]));
    let schema = map(&built, "sql.schema");
    assert_eq!(
        columns(schema, "Customers"),
        ["Id int [key] [not null] [identity]", "Name nvarchar(max)"]
    );
    assert_eq!(joins(schema), ["sales.Orders.CustomerId → Customers.Id"]);
}

#[test]
fn a_column_dropped_takes_its_keys_and_indexes_and_renames_are_followed() {
    let corpus = Corpus::of(&[
        (
            "m/001.sql",
            "CREATE TABLE a (id int PRIMARY KEY, code text);\nCREATE TABLE b (id int PRIMARY KEY, a_id int REFERENCES a(id), a_code text);\nCREATE INDEX b_a ON b (a_id);\n",
        ),
        (
            "m/002.sql",
            "ALTER TABLE a RENAME TO alpha;\nALTER TABLE alpha RENAME COLUMN id TO alpha_id;\nALTER TABLE b DROP COLUMN a_code;\nCREATE SCHEMA app; ALTER TABLE b SET SCHEMA app;\n",
        ),
        ("m/003.sql", "ALTER TABLE app.b DROP COLUMN a_id;\n"),
    ]);
    let maps = maps(&corpus);
    let schema = map(&maps, "sql.schema");
    assert_eq!(columns(schema, "app.b"), ["id int [key] [not null]"]);
    assert!(
        schema.edges.is_empty(),
        "the key went with its column: {:?}",
        edges(schema)
    );
    assert!(
        !node(schema, "app.b").1.detail.contains("b_a"),
        "the index too"
    );
    assert_eq!(columns(schema, "alpha")[0], "alpha_id int [key] [not null]");
}

/// A constraint written without a name has the one PostgreSQL gives it,
/// so a later migration can drop it by that name.
#[test]
fn an_unnamed_constraint_is_dropped_by_the_name_postgres_gives_it() {
    let corpus = Corpus::of(&[
        (
            "m/0019.sql",
            "CREATE TABLE ns (id uuid PRIMARY KEY);\nCREATE TABLE t (id uuid PRIMARY KEY, namespace_id uuid NOT NULL references ns(id) ON DELETE CASCADE, code text UNIQUE);",
        ),
        (
            "m/0022.sql",
            "ALTER TABLE t DROP CONSTRAINT IF EXISTS t_namespace_id_fkey;\nALTER TABLE t ADD CONSTRAINT t_namespace_id_fkey FOREIGN KEY (namespace_id) REFERENCES ns (id);\nALTER TABLE t DROP CONSTRAINT t_code_key;",
        ),
    ]);
    let built = maps(&corpus);
    let schema = map(&built, "sql.schema");
    assert_eq!(
        edges(schema),
        [("t".into(), "ns".into(), "namespace_id".into())]
    );
    assert!(!field(schema, "t", "code").unique);
}

#[test]
fn several_databases_have_a_schema_each() {
    let corpus = Corpus::of(&[
        (
            "migrations/tenant/0001.sql",
            "CREATE TABLE storage.objects (id uuid, b text);\nCREATE TABLE storage.buckets (id text);",
        ),
        (
            "migrations/tenant/0002.sql",
            "ALTER TABLE storage.objects ADD COLUMN name text;",
        ),
        (
            "migrations/multitenant/0001.sql",
            "CREATE TABLE tenants (id text PRIMARY KEY);",
        ),
        (
            "migrations/multitenant/0002.sql",
            "ALTER TABLE tenants ADD COLUMN jwks jsonb;",
        ),
    ]);
    let maps = maps(&corpus);
    let ids: Vec<&str> = maps.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "sql.schema:migrations/tenant",
            "sql.schema:migrations/multitenant",
            "sql.history:migrations/tenant",
            "sql.history:migrations/multitenant",
        ],
        "the one with the most tables first"
    );
    assert_eq!(maps[0].title, "Database schema · migrations/tenant");
    assert!(maps[1].nodes.iter().all(|n| n.label == "tenants"));
}

#[test]
fn nothing_to_draw_draws_nothing() {
    assert!(
        maps(&Corpus::of(&[("q.sql", "SELECT 1;")])).is_empty(),
        "no schema, no maps"
    );
    assert!(maps(&Corpus::of(&[("q.sql", "CREATE TABLE (")])).is_empty());
}

/// A real project's SQL (`CLEW_TEST_SQL_PROJECT=path`) read as Clew reads it:
/// each database's tables with their columns and keys, printed for a reader
/// to check against the project, and every map well formed.
#[test]
#[ignore = "reads a project's SQL (CLEW_TEST_SQL_PROJECT=path); run explicitly"]
fn live_a_projects_sql_reads_whole() {
    let root = std::path::PathBuf::from(
        std::env::var("CLEW_TEST_SQL_PROJECT").expect("CLEW_TEST_SQL_PROJECT names a folder"),
    );
    let scanned = crate::fs_scan::scan(root.clone());
    let started = std::time::Instant::now();
    let corpus = Corpus::read(&scanned.files);
    let built = maps(&corpus);
    let took = started.elapsed();
    for m in &built {
        assert!(m.is_well_formed(), "{}", m.id);
        println!(
            "== {} ({}): {} nodes, {} arrows",
            m.title,
            m.id,
            m.nodes.len(),
            m.edges.len()
        );
        if !m.id.starts_with("sql.schema") {
            continue;
        }
        for n in &m.nodes {
            println!("  {} {} ({}:{})", n.kind, n.label, n.file, n.line);
            for f in &n.fields {
                let mut marks = Vec::new();
                for (on, mark) in [
                    (f.key, "key"),
                    (!f.nullable, "not null"),
                    (f.unique, "unique"),
                    (f.identity, "identity"),
                    (f.generated, "generated"),
                    (f.indexed, "indexed"),
                ] {
                    if on {
                        marks.push(mark);
                    }
                }
                println!(
                    "      {} {} [{}]{}",
                    f.name,
                    f.ty,
                    marks.join(", "),
                    if f.default.is_empty() {
                        String::new()
                    } else {
                        format!(" = {}", f.default)
                    }
                );
            }
            for line in n.detail.lines() {
                println!("    · {line}");
            }
        }
        for j in joins(m) {
            println!("  FK {j}");
        }
    }
    println!("read in {took:?}");
    assert!(took < std::time::Duration::from_secs(20), "slow: {took:?}");
}
