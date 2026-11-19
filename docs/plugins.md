# Plugins

**English** · [简体中文](plugins.zh-CN.md)

A plugin adds to Clew what you need and Clew does not have: a command that lists your project's TODOs by who wrote them, a graph of which services publish to which topics, a check your team cares about. You don't need to write one yourself. Ask your agent in Ask, an ACP agent in the Ask panel, or an agent such as Claude Code connected to Clew over MCP (see [MCP and ACP](mcp-acp.md)). It writes the plugin, Clew builds it, and you approve it once.

A plugin is one file: a WebAssembly component written in Rust. It runs inside Clew at close to native speed, in a sandbox that holds only what you allowed. It cannot open a file outside your project, start a program, reach the network or see your keys, because Clew gives it no way to.

## Making one

Ask, in your own words:

> Make me a plugin that lists the TODO and FIXME comments in this project, grouped by who last changed the line.

The agent reads Clew's plugin guide, writes the plugin and builds it. While it works it can try the plugin on your project, and it fixes what the compiler says until the build is clean. Then Clew asks you once:

> **Install “TODOs by author”?**
> Lists the TODO and FIXME comments, grouped by who last changed each line.
> **Adds** a command: TODOs by author
> **Can** read this project's files · read the project's git history
> *Don't install* · *Install*

Once installed, the command is in the command palette. Like any other command, you can run it by voice or the agent can run it for you. Its result opens as a page over the code, drawn by Clew in your theme. Every row opens the line it is about, by mouse, keyboard or "open the third one".

To change a plugin, ask for the change. The agent edits it, and Clew asks again before the new version runs. If the new version can do more than the last one, the card says so.

## What a plugin can add

| | Where it shows | |
|---|---|---|
| **Commands** | The command palette, voice and the agent's `run_command`. A command marked `where = "code"` also appears in the code's right-click menu and is told the file, the line and the selection. | now |
| **Views** | What a command shows: headings, Markdown, lists that open code, trees, tables, facts, code, notices. Clew draws them, so they follow the theme, the language, the keyboard ring, jump badges and voice. | now |
| **Project graphs** | Beside Clew's own graphs, in the Project graphs tool. | next |
| **Checks** | Findings in the code (a mark in the gutter, a note in the margin) and in Weak Spots. | later |
| **Ask tools** | A tool your Ask agent may call. | later |
| **Settings** | The plugin's own options, on its page in Settings → Plugins. | later |

## What a plugin can do

| Permission | What it means |
|---|---|
| *(none)* | Work on what it is given: the file, line and selection a command was run on. It may keep up to 1 MiB of its own data between runs and write to its log. |
| **Read this project's files** | List the project's files, read them (text, up to 4 MiB each), and use Clew's outline and search. Only the open project's files, never anything outside it. |
| **Read the project's history** | Read git's log and blame. Nothing that changes the repository. |

A plugin never gets the network, other files, programs, your environment, your keys or your AI provider. A later version of Clew may offer some of these as permissions of their own, each asked for by name.

Clew does not take a plugin's word for its permissions. It reads them from the plugin's code: the functions the code imports. A permission you have not granted is never linked in, so the code has no way to call it.

## Installing, updating, removing

- **Settings → Plugins** lists your plugins. From there you can turn one off, remove it, see what it may do, and read its log. **Install from a file…** installs a plugin someone sent you, and it asks the same question an agent's plugin does.
- Clew asks again whenever a plugin's file changes. Nothing changes a plugin's file after you approve it: Clew runs its own copy of the file you approved, and checks that copy each time it loads it.
- Plugins belong to you, not to a project. They work in every project you open.

---

## Design

This section is for whoever works on Clew. The user-facing behaviour above follows from the decisions below.

### Shape

```
 crates/clew-plugin           the plugin system, UI-free
   wit/plugin.wit             the contract (the only source of truth)
   manifest, inspect          plugin.toml; a file's manifest and imports
   runtime  (feature)         wasmtime: engine, compile cache, limits, calls
   store                      approvals (plugins.toml), the approved copies
   build                      plugin source → component, with cargo
   requests                   install / try requests from `clew-server mcp`
 crates/clew-plugin-sdk       what a plugin is written against (guest side)
 crates/clew-rules/plugin.rs  which imports need which permission (proved)
 src/app/plugins.rs           the GUI: registry, palette, pages, consent
```

Plugins run in the `clew` GUI process, where every contribution is shown. The GUI already serves local and remote projects through the same paths. A plugin's `project.read` uses the same paths as the rest of the GUI: a local project is read directly, a remote one through `ReadSources`. The runtime lives behind a cargo feature, so `clew-server` can use the authoring half (build, inspect, requests) without linking wasmtime. A remote host never runs plugins in this version. Running them next to a remote project's files is a later phase, behind a protocol request of its own.

### The plugin file

A plugin is a single WebAssembly component (`.wasm`) that carries its manifest in a custom section named `clew:plugin`. The section is the validated `plugin.toml` that the build embeds. One file means one thing to approve, share and hash. Clew reads the manifest and the imports with `wasmparser`, without instantiating anything, so it can describe a plugin before anything runs.

```toml
[plugin]
id = "todo-authors"            # a-z, 0-9 and -, 3 to 40 characters
name = "TODOs by author"
version = "0.1.0"
description = "Lists TODO and FIXME comments, grouped by who last changed each line."
permissions = ["read", "git"]  # checked against the code's imports at build

[[command]]
id = "list"
title = "TODOs by author"
where = "palette"              # or "code": the right-click menu too
# input = "Which tag?"         # ask for text before running

[[graph]]                      # next phase
id = "topics"
title = "Event bus"

[dependencies]                 # crates.io, versions only
regex = "1"
```

### The API

`wit/plugin.wit` is the contract. It has three groups of interfaces:

- **Free:** `host`, with log, progress, and a store of 1 MiB per plugin.
- **Gated by a permission:** `project` (read) and `git` (git).
- **Exported by the plugin:** `commands` (run a command, get an `outcome`: `show(view)`, `open(location)`, `say(text)` or nothing) and `graphs`.

A view is a list of blocks rather than a tree, because WIT has no recursive types and a page reads top to bottom anyway. Tree rows carry a depth.

Plugins are written against `clew-plugin-sdk`. It holds bindings generated from the WIT by `wit-bindgen`, committed with the WIT's digest so a test catches them going stale. It also provides a `Plugin` trait whose methods all have defaults, an `export!` macro (`macro_rules!`, not a proc macro), and builders for views. A plugin that adds only commands implements only `run`. The SDK has no build script, no proc macro and no prebuilt object. It carries the small part of `wit-bindgen`'s runtime it needs, `cabi_realloc` included, as plain Rust. So building a plugin with no dependencies runs no code but the compiler's.

The package is versioned (`clew:plugin@0.1.0`). Later versions only add interfaces and functions. Clew links every version it knows, and looks up each export before calling it, so a plugin built against an older SDK keeps working.

### Permissions are read from the code

The permission rule is a pure function in `clew-rules` (`plugin::permission_of`, `plugin::may_link`), which proves its properties in Lean like Clew's other rules:

- Each import needs a permission or none: `clew:plugin/project` → read, `clew:plugin/git` → git; `clew:plugin/host` and `clew:plugin/types` → none. `wasi:*` needs none either: Clew links WASI with nothing behind it (no preopened folder, no environment, sockets refused, stdout and stderr into the plugin's log).
- Any other import refuses the plugin outright, so a plugin cannot ask for a capability Clew doesn't know.
- A plugin is linked only if every import it needs is either free or granted. The linker is built from the granted set, so an ungranted interface is missing, not stubbed.

The permissions the user approves are the ones derived from the imports. The manifest's `permissions` must match them. If they don't, the build says which one is missing, so the source the agent wrote always states what the plugin does.

`wit-component` leaves out an interface the code never calls. A plugin that never calls `git` does not import it, and so is never asked for it.

### Runtime

- **Engine:** one per process. Cranelift, component model, epoch interruption, and a 1 MiB wasm stack.
- **Compile cache:** compiled code is cached by `(sha256 of the file, Engine::precompile_compatibility_hash)` under `<data>/plugins/compiled/`, written atomically into Clew's private data folder. That folder is the trust boundary, since loading compiled code is code execution. Compiling happens in the background when a plugin is installed and when Clew starts. A later load is a deserialize: about 0.4 ms for a small plugin.
- **Calls are stateless:** each call gets a new `Store` and instance from a cached `InstancePre` (about 12 µs). Nothing leaks between calls, a crashed call takes nothing down with it, and calls run in parallel on blocking threads. State a plugin wants to keep goes through `host.save`.
- **Limits per call:** 256 MiB of memory (`StoreLimits`), and a deadline enforced by a 10 ms epoch ticker. Commands get 30 s, graphs 60 s. A trap, a deadline or an exhausted limit becomes a sentence the user can read: "The plugin stopped: it took longer than 30 s."
- **Output is capped and checked before it is drawn:** 10 000 rows per view, 4000 graph nodes, strings cut at 64 KiB, and locations confined to the project (`confine::check_lexical`).

Measured on the prototype (wasmtime 48, a 47 KB plugin): 28 ms to compile, 0.4 ms to load from the cache, 12 µs to instantiate, 0.12 µs per empty call, and 14 ms to pass 10 MB of text in and count its lines.

### Trust

The design follows the language-server approval Clew already has.

- **Fingerprint:** `sha256(length_prefixed(["clew-plugin/1", sha256(file)]))`, using the proved encoding in `clew_rules::fingerprint`. The file carries the manifest, so the fingerprint covers the code, the name and the declared contributions together.
- **Record:** `<data>/plugins.toml` holds, per plugin id, its fingerprint, the permissions approved, whether it is on, and a copy of its manifest, so the palette is ready at startup without reading any plugin. It is a file of its own rather than a table in `trust.toml`, because an older Clew rewriting `trust.toml` would drop a table it doesn't know. Writes use the same lock, `load_checked` and `write_atomic` as `Trust::update`.
- **The approved copy:** on approval, the bytes the user saw are copied to `<data>/plugins/installed/<fingerprint>.wasm` (mode 0400). Clew runs that copy only, and checks its digest each time it loads it. So a rebuild after approval cannot change what runs: a new build is a new fingerprint and a new question.
- **The question:** asked only by the GUI, through `Questions<T>`, with the 500 ms answer delay and the `may_allow` check the other consent cards use. The card names the plugin, its description, what it adds, what it may do, and where it came from: an agent in this chat, an agent over ACP, or a file you picked. On an update, what is new is marked. A headless path such as `clew-server mcp` never approves anything. It files a request (below), and the GUI asks.

### Building

A plugin's source is `plugin.toml` plus `src/*.rs`, under `<data>/plugins/source/<id>/`. Plugins are yours, not a project's, so their source lives outside the project.

- **Clew writes `Cargo.toml` itself**, from `plugin.toml`: crates.io dependencies by version only, `crate-type = ["cdylib"]` and a release profile. So there is no `build.rs`, no `[patch]`, no path or git dependency, and no profile setting a plugin could use to run code. The source is copied into a clean build folder. Only `.rs` files under `src/` are copied, along with `plugin.toml` and `Cargo.lock`, with no symlinks and size caps. A `.cargo/config.toml` or `rust-toolchain.toml` in the source has no effect.
- **The toolchain is called by absolute path:** `cargo` and `rustc` (`RUSTC`), found the way `install_command` finds tools, with a neutral working folder and `CARGO_TARGET_DIR` shared by all plugins so dependencies are built once. It needs Rust with the `wasm32-wasip2` target (Rust 1.82 or later). If the target is missing, the build says so in one line, with the command that adds it. Clew downloading a pinned toolchain itself, verified like voice models, is a later phase.
- **Dependency policy:** before building, `cargo metadata` (which resolves and downloads, but runs nothing) lists every package that has a build script or is a proc macro. Each must be on a short list of widely used crates (`serde`, `serde_derive`, `serde_json`, `proc-macro2`, `quote`, `syn`, …), or the build is refused with the crate's name and why. Rust's compiler runs no code it compiles; build scripts and proc macros do, so they are what the list guards.
- **What the agent sees:** a build runs with `--message-format=json`. Errors come back as the compiler's own rendered messages, without colour, at most 20 of them, file paths relative to the source. Success comes back as the derived permissions and contributions. The build then embeds the manifest section and inspects the component exactly as an install would. If the imports and the manifest disagree, the build fails.
- The build has a deadline (10 minutes for a first build, which downloads crates) and can be cancelled. Its process group is killed on either.

### Authoring tools

The same tools serve Clew's Ask agent and every ACP agent:

| Tool | |
|---|---|
| `plugin_guide` | The guide: the API, the manifest, the SDK, examples, and what is refused and why. |
| `plugin_new` | Make a plugin's source from a template: `plugin.toml` and `src/lib.rs`. |
| `plugin_files` | List a plugin's source files. |
| `plugin_read` | Read a file of a plugin's source. |
| `plugin_write` | Write a file of a plugin's source, confined to its folder (the Ask agent has no other way to write). |
| `plugin_build` | Build it. Returns compiler messages or what the plugin is. |
| `plugin_try` | Run one of its commands on the open project, with its declared permissions (read and git only) and nothing approved. The result goes back to the agent, not to the screen. That is safe without asking: the agent can already read the project with its own tools, and a plugin has no network to send anything anywhere. |
| `plugin_install` | Ask the user, in Clew, to install the build. Waits for the answer. |
| `plugin_status` | Whether a plugin is installed, at which version, and whether it is on. |

For the Ask agent these are client-side tools (like the window tools), run by the GUI. Its `plugin_install` shows the consent card in the conversation, in place of the approval card. For ACP agents and any other MCP client, `clew-server mcp` serves them from the same code. It builds itself, and files `plugin_try` and `plugin_install` as requests in `<data>/plugins/requests/`, naming the client as `initialize` named it. The GUI watches that folder; one window claims a request by renaming it, runs the try or asks the question, and writes the answer the tool is waiting for. A try is claimed only by a window with that project open on this machine, matched by its canonical path; a try waits 90 seconds, an install 15 minutes, and a request no window answered is withdrawn with a sentence saying why. The tools that write, build or ask for an install say they are not read-only (`readOnlyHint: false`), so a client asks before them; the code tools and the plugin tools that only read say they are.

### Views

A plugin's `view` becomes a native page over the code, like Weak Spots. Each block maps to Clew's own widgets:

- Rows are `ui::button` with `heard::said`, so the keyboard ring, jump badges and voice reach them with no extra work.
- Markdown goes through Clew's renderer, and code through its highlighter.
- Plugin text is shown as written: plugins carry their own languages (`title` plus `translations = { zh = "…" }` in `plugin.toml`). Clew's catalogs cover only Clew's own words around the page.

### Testing

- **Component fixtures:** built from source in `crates/clew-plugin/fixtures/` by `fixtures/build.sh` and checked in with their source's digest, so CI needs no wasm target and a test catches a fixture out of date. They cover a command, a view of every block, a loop that never ends, a plugin that allocates without limit, one importing an interface it was not granted, one importing an unknown interface, and one whose manifest misstates its permissions.
- **Tests:** the manifest and its section; permission derivation; refusals; deadlines and memory limits; output caps and confinement; the store (approve, update, turn off, remove, a tampered copy refused); the build's policy (a fake `cargo` on `PATH` proves the arguments and environment, and a real build runs where the target is installed); the GUI (the palette lists a plugin's commands, a page draws every block, and the consent card asks, waits and records).
- **Proofs:** the `may_link` rule in Lean, with the Rust source extracted by Aeneas like the rules in `docs/verification.md`.

### Phases

1. **Now:** the runtime, commands and views, the store and consent, Settings → Plugins, install from a file, the build, the authoring tools for Ask and ACP, and the guide.
2. **Next:** project graphs, checks, Ask tools, plugin settings, views a plugin updates in place (an action on a row calls the plugin again), and a toolchain Clew downloads itself.
3. **Later:** plugins next to a remote project's files (wasmtime in `clew-server`, behind a request of its own), plugins a project shares in `.clew/plugins/` (still approved by each user), and the network and the AI provider as permissions, each asked for by name and metered like Clew's own use.

### Costs

- **Size:** wasmtime with Cranelift adds about 15–20 MB to `clew`, and a few minutes to a release build.
- **Toolchain:** making plugins needs Rust with the WebAssembly target, about 250 MB. Running plugins needs nothing.
- **First build:** the first build of a plugin downloads the crates it uses. Later builds of any plugin reuse them.
