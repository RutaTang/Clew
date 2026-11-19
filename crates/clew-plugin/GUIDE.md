# Writing a Clew plugin

A Clew plugin adds something to Clew the user asked for: a command whose result Clew shows as a page, or a graph Clew shows in Project Graphs. It is written in Rust against `clew_plugin_sdk`, built by Clew into a WebAssembly component, and runs inside Clew in a sandbox. It can only do what its code imports: read the open project's files, read its git history, and nothing else. It has no network, no other files, no processes and no environment.

## How to make one

1. `plugin_new` with an `id` (`todo-authors`: a-z, 0-9 and -), a `name` the user will see, and a one-line `description`. It writes `plugin.toml` and `src/lib.rs` from a template that builds as it is.
2. Write the code with `plugin_write` (`src/lib.rs`, more files under `src/`, and `plugin.toml`). `plugin_read` reads a file back; `plugin_files` lists them. Source files are `.rs` files under `src/`, named with ASCII letters, digits, `_`, `-` and `.`, in folders if you like (`src/views/list.rs`); a name or folder may not start with `.`. Nothing else is written or built.
3. `plugin_build`. It answers with the compiler's messages, or with what the plugin is: its permissions and commands. Fix and build again until it builds clean, warnings included.
4. `plugin_try` runs a command of the build on the open project (`command`), or builds one of its graphs (`graph`), and gives you what came of it, so you can check it does what was asked. Nothing is shown to the user. Clew must be open with the project.
5. `plugin_install` asks the user, in Clew, to install it. They see its name, description, what it adds (its commands and graphs) and its permissions. It answers whether they did, and where what it added now is.

To change an installed plugin, change its source, build and install again. The user is asked again.

## plugin.toml

```toml
[plugin]
id = "todo-authors"                # never changes once made
name = "TODOs by author"           # what the user sees; up to 60 characters
version = "0.1.0"                  # raise it when you change the plugin
description = "Lists the TODO comments in the project, by who last changed the line."
permissions = ["read", "git"]      # exactly what the code uses (below)

[[command]]
id = "list"                        # what `run` is called with
title = "TODOs by author"          # in the command palette; up to 60 characters
where = "palette"                  # or "code": also in the code's right-click menu
# input = "Which tag?"             # Clew asks the user for a line of text first

[[graph]]                          # optional: a graph in Project Graphs
id = "owners"                      # what `graph` is called with
title = "TODO owners"              # in Project Graphs' list; up to 60 characters
description = "Who owns the files with TODOs."   # one line on how to read it

[dependencies]                     # crates.io only, by version
regex = "1"
```

In the palette a command is listed as "plugin name: command title" ("TODOs by author" alone when both are the same). A command with `where = "code"` is also in the code's right-click menu, and is told the file, the line clicked and the selection.

- `permissions`: `"read"` if the code calls anything in `project`, `"git"` if it calls anything in `git`. The build checks the code against this list. A permission the code uses but the list leaves out fails the build. One the list has but the code never uses is a warning: take it out, or the user is asked for it for nothing.
- `translations`: the plugin's name and description, and a command's or a graph's title, in other languages. Write them in the language the user writes to you in, and add English if that is not English:
  ```toml
  [plugin.translations.zh]
  name = "按作者列出 TODO"
  description = "…"

  [[command]]
  id = "list"
  title = "TODOs by author"
  translations = { zh = "按作者列出 TODO" }

  [[graph]]
  id = "owners"
  title = "TODO owners"
  translations = { zh = "TODO 的负责人" }
  ```
- Do not add `clew-plugin-sdk` to `[dependencies]`; Clew adds it. There is no `Cargo.toml` to write. Clew writes it from this file.

## The code

```rust
use clew_plugin_sdk::{project, Invocation, Item, Outcome, Plugin, View};

struct Todos;

impl Plugin for Todos {
    fn run(command: &str, context: Invocation) -> Result<Outcome, String> {
        match command {
            "list" => {
                let mut items = Vec::new();
                for path in project::files() {
                    let Ok(text) = project::read(&path) else { continue };
                    for (n, line) in text.lines().enumerate() {
                        if let Some(at) = line.find("TODO") {
                            items.push(Item::new(line[at..].trim()).at(&path, n as u32 + 1));
                        }
                    }
                }
                Ok(Outcome::Show(View::new("TODOs").items(items)))
            }
            other => Err(format!("no command {other}")),
        }
    }
}

clew_plugin_sdk::export!(Todos);
```

`export!` exports your type. Call it once, in `src/lib.rs`. `Plugin` has two methods, both with defaults: `run(command, context)` for commands, and `graph(id)` for graphs.

`context: Invocation` says where the command was run:

| field | |
|---|---|
| `file: Option<String>` | the file shown, from the project root |
| `line: Option<u32>` | the caret's line, from 1 |
| `selection: Option<String>` | the selected text |
| `input: Option<String>` | what the user typed, for a command with `input` |

### What a command gives back: `Outcome`

| | |
|---|---|
| `Outcome::Show(View)` | a page over the code |
| `Outcome::Open(Location)` | open the code at a place |
| `Outcome::Say(String)` | one line in the status bar |
| `Outcome::Nothing` | nothing to show |
| `Err(String)` | it could not; the user sees your sentence |

### Pages: `View`

Clew draws the page in its own style. Rows open the code they point at: by click, by keyboard, or by voice ("open the third one").

```rust
View::new("Title")
    .heading("A heading")
    .text("A paragraph of **Markdown**.")
    .notice(Level::Warning, "Something to notice")
    .items(vec![Item::new("Title").detail("second line").at("src/a.rs", 12).tag("3").level(Level::Error)])
    .tree(vec![TreeRow::new(0, "parent"), TreeRow::new(1, "child").at("src/b.rs", 4)])
    .table(Table::new(["File", "Lines"]).row(Row::new(["src/a.rs", "120"]).at("src/a.rs", 0)))
    .facts([("Files", "120"), ("Lines", "9 000")])
    .code(Some("rust"), "fn main() {}")
```

A `line` of 0 opens a file at its top. Paths are from the project root, with `/`, as `project::files()` gives them. Clew shows 10 000 rows at most across a page, so for big results show the top ones and say how many there are.

### What a plugin may ask of Clew

`host` needs no permission:

- `host::info(msg)`, `host::warn(msg)`, `host::error(msg)`: the plugin's log, which the user can read in Settings → Plugins. `println!` and `eprintln!` go there too.
- `host::progress(done, total)`: how far a command has come, shown as a percentage in the status bar while it runs.
- `host::load(key) -> Option<Vec<u8>>`, `host::save(key, &bytes) -> Result<(), String>`: a store of the plugin's own, 1 MB in all, kept between runs.

`project` needs `"read"`:

- `project::info() -> ProjectInfo { name, files, partial, remote }`
- `project::files() -> Vec<String>`: every file of the project (what `.gitignore` leaves out is not listed).
- `project::read(path) -> Result<String, String>`: a file's text, up to 4 MB. Binary files are refused.
- `project::outline(path) -> Result<Vec<Symbol>, String>`: the file's definitions, as `Symbol { name, kind, line, end_line }`.
- `project::search(&Query) -> Result<Vec<Hit>, String>`: matching lines across the project (2000 at most). Use `Query::text("TODO")` or `Query::regex(r"fn \w+")`, with `.case_sensitive()`, `.whole_word()`, `.include("src/**/*.rs")` and `.exclude("tests/**")`. A hit is `Hit { path, line, text }`. Searching is much faster than reading every file yourself, so search first and read only what you need.

`git` needs `"git"`:

- `git::log(Some(path) or None, limit) -> Result<Vec<Commit>, String>`, newest first. A commit is `Commit { id, author, time, subject }`, with `time` in seconds since 1970.
- `git::blame(path) -> Result<Vec<BlameLine>, String>`: who last changed each line, as `BlameLine { line, author, time, commit }`.

### Graphs

A `[[graph]]` in `plugin.toml` and `fn graph(id: &str) -> Result<Graph, String>` add a graph to Project Graphs, beside Clew's own. Clew builds it when the user opens it there, and again once the project's files have changed. Try it with `plugin_try` and `graph`.

```rust
fn graph(id: &str) -> Result<Graph, String> {
    match id {
        "owners" => {
            let mut g = Graph::new();
            let a = g.node_at("service", "billing", "src/billing.rs", 1);
            let b = g.node("topic", "invoice.created");
            g.edge(a, b, "publishes");
            Ok(g)
        }
        other => Err(format!("no graph {other}")),
    }
}
```

- `node(kind, label)` and `node_at(kind, label, path, line)` add a node and return its number; a node with a place opens the code when the user picks it. `kind` groups nodes: Project Graphs lists them by kind, under its name.
- `edge(from, to, label)` adds an arrow between two nodes by their numbers.
- A graph shows 4 000 nodes and 20 000 arrows at most; an arrow to a node that is not there is left out.

## Limits

- A command has 30 seconds and 256 MB of memory. A graph has 60 seconds and 512 MB.
- A page shows 10 000 rows and 4 MB of text at most; past that it is cut, and says so. A run's log keeps 1000 lines.
- A crash or a panic stops only that run. The user, and you in `plugin_try`, see the panic message and where it happened.
- No network, no files outside the project, no processes, no environment variables. `std::fs`, `std::net` and `std::process` fail; use `project` and `git`.
- Nothing is read from the computer while building either: `include_str!`, `include_bytes!`, `include!`, `env!`, `option_env!` and `#[path]` are refused in the plugin's code. Put text the plugin needs in the code itself, and read the project with `project`.
- Dependencies come from crates.io, by version. A crate that runs its own code while building (a build script or a proc macro) is refused, apart from serde, serde_json, thiserror, anyhow, memchr, libm, num-traits, libc, and the few others serde, serde_json and url need, which the build names. Pure-Rust crates such as `regex`, `url`, `chrono` and `toml` are fine. `paste` is not.

## Writing a good plugin

- Do what was asked and no more. The user reads every permission on the install card.
- Give the user something to act on. Rows should point at the code (`.at(path, line)`), titles should say what each row is, and the page should open with a line saying what it found ("12 TODOs in 7 files").
- An empty result is a result: say "No TODOs in this project", not nothing.
- Errors are sentences for the user ("The project has no git history"), not debug dumps.
- Large projects: report `host::progress` while you go, skip files that are not text, and do not read a file twice.
