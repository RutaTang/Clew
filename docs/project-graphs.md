# Project graphs

**English** · [简体中文](project-graphs.zh-CN.md)

The **Project graphs** tool on the tool strip opens the graphs Clew reads from the project's files, statically, as a reader would — a route registered at run time from data, or a table only an ORM creates, is not seen. A graph that finds nothing is not listed, and for a remote project they are read on its host.

- **SQL** — the schema, drawn as tables: a card per table with its columns, their types and marks (primary key, identity, unique, nullable or not — a legend says which), and a line from each foreign key's columns to the ones they reference; views and enums too. Pointing at a table lights its lines, and at a column says its type, default, comment and what it references; a click picks a table, the next opens it. The schema is the migrations replayed in order (numbered `0002` before `00010`, a down migration left out), as PostgreSQL, MySQL, SQLite and SQL Server write them — `DO` blocks, `ALTER COLUMN`, renames and `SET SCHEMA` followed — or read from a Prisma schema, Django's models, SQLAlchemy's models or Rails' `db/`; folders of migrations side by side are databases of their own. Also: the schema after each migration, the lineage of each view and `INSERT … SELECT` back to the tables it reads, and the code that names each table.
- **Routes** — each route to its handler: Flask, FastAPI, Django, Express and its kin, NestJS, Next.js, Spring, ASP.NET, Laravel, Symfony, Rails and Go's `net/http`, gin, echo and chi.
- **Frameworks** — dependency injection (Spring, .NET, Nest and Angular), ORM relations (JPA, Entity Framework, Eloquent, ActiveRecord, Django, SQLAlchemy, TypeORM) and WordPress hooks.
- **Python** — the class hierarchy with each class's method resolution order, pytest fixtures and what uses them, and where what is called is picked at run time (`getattr` with a computed name, dicts of callables, functions a decorator registers) and what it can reach.
- **JavaScript and TypeScript** — the React and Vue component tree, and the workspace's packages and their dependencies.
- **Go** — the interfaces each type satisfies, the goroutines each function starts and the channels they share, and the package graph with its cycles and layers.
- **Rust** — each trait and its implementations (derives too), the `unsafe` code, and the crate's public API.
- **C and C++** — the include graph and what each header costs, C++ overrides, and templates with their instantiations.
- **Erlang** — the supervision tree (child specs, restart strategy, the application that starts it), the messages each module sends and takes, and each `gen_server` call to the clause that answers it.
- **Haskell** — type class instances (deriving too), and the monad stack each function runs in.
- **Zig** — `comptime` generics and their instantiations, error sets and what each function returns, tries and catches, and the `build.zig` graph.
- **Ruby** — the methods no `def` names: mixins and concerns, `define_method`, `attr_*`, `delegate`, `alias`, `class_eval`, calls by `send` and `method_missing`.
- **Dart and Flutter** — the widget tree as written (a `State`'s build is its widget's), and state management across Riverpod, provider and Bloc.
- **Lean 4** — theorem dependencies, notations and what they mean, and what each `simp` lemma rewrites.

In the code, the right-click menu (and `ge` at the cursor) adds what the file's language can say there: **Expand Macro** for Rust (rust-analyzer's expansion) and for C and C++ (Clew's own reading of the `#define`s in effect — the file's and its headers', found by its compile command and the project's `include/`, the groups `#if`s drop left out — with the `#define` it came from), **Show Goal** for Lean (the proof's goals there, or a term's expected type) and **Show Type** for Haskell (the name's declared type from the Haskell language server, and its type at that use when it is another — `Int -> IO ()` for `print` given an `Int`).
