English | [简体中文](README.zh-CN.md)

<div align="center">

# clew

**A reader for code.**

You spend more time reading code than writing it. clew is built for that half — a fast, read-only desktop app for finding your way around an unfamiliar codebase and actually understanding it.

![clew](assets/screenshot.png)

</div>

clew isn't an editor. There's no cursor blinking in your file, nothing to save, no plugins to install. It does one thing — help you *read* — and that focus buys a reader the things editors bolt on as afterthoughts: a jump that's always undoable, a 3D map of who imports whom, an AI that explains a function in the context of everything it calls, and a Vim-style cursor that glides through code you can't accidentally change.

Built in Rust with [iced](https://iced.rs), for macOS.

## Highlights

- 🧭 **Move at the speed of thought** — fuzzy jump to any file or symbol, project-wide ripgrep search, browser-style back/forward that makes every jump reversible, split panes for side-by-side reading, and a Vim block cursor.
- 🔍 **Precise navigation, batteries included** — go-to-definition, references, implementations and type across languages, plus hover, diagnostics and inlay hints. clew downloads and version-pins its *own* language servers, so the same config gives the same result on every machine.
- 🧠 **Understand, don't just browse** — one-click **Explain** for any file or function, an auto-generated architecture **Overview**, semantic **Find** ("where do we validate tokens?"), an **Ask** chat that knows your project, and a native **Docs** view of its API surface. Math and Mermaid diagrams render inline.
- 🕸️ **See the shape of the code** — import and call graphs as a live **3D force-directed map** you can orbit, spin and drag, colored by language and hierarchy depth.
- 🐞 **Read the running program** — a built-in debugger (DAP): breakpoints, call stack, variables, stepping.
- 🎨 **Seven themes, light & dark** — One Dark/Light, Gruvbox Soft, Paper, Cyberpunk. Follow the system appearance or pin your own light and dark picks; switches live.
- 🌐 **Read code anywhere** — open a project over SSH and the headless backend runs on the remote host, streaming to your Mac.
- 📖 **Reading, remembered** — bookmarks, notes with a per-file "understood" tracker, a navigation trail, and guided walkthroughs — all saved in the project's `.clew/`, never anywhere else.

## Quick start

```sh
# Build and open a project
cargo run --release -- /path/to/your/project

# ...or a single file (opens its folder and jumps straight to it)
cargo run --release -- /path/to/file.rs

# ...or just launch and pick a folder from the welcome screen
cargo run --release
```

The first time you open a project, clew asks permission to create a `.clew/` folder — that folder *is* the consent, and everything clew remembers (bookmarks, notes, LSP config) lives there and nowhere else. Add `.clew/` to your global gitignore, or commit it to share reading trails with your team.

New here? The in-app tour (**⋯ menu → Tutorial**) walks you through every feature, live.

## Keys

| Action | Keys | Action | Keys |
| --- | --- | --- | --- |
| Go to file | `⌘P` | Go to definition | `⌘`-click · `gd` |
| Go to symbol | `⌘T` | References / impls / types | `gr` · `gi` · `gy` |
| Search project | `⌘⇧F` | Move the cursor | `h j k l` · `w b` · `0 $` · `gg G` |
| Go to line | `⌘L` | Split editor | `⌘\` |
| Back / forward | `⌥←` · `⌥→` | Bookmark this line | `⌘D` |
| New window | `⌘N` | Zoom in / out / reset | `⌘+` · `⌘-` · `⌘0` |

## Language support

**Precise LSP navigation** (clew manages the servers): Rust, C/C++, Zig, Go, Python, TypeScript/JS, JSON/HTML/CSS, TOML.
**Syntax highlighting + symbol outline**: Rust, Python, JS/JSX, TS/TSX, Go, C, C++, Java.
Everything else opens as plain text — reading never breaks.

clew ships its own language servers, **version-pinned and isolated** from anything on your system. Binaries are downloaded with SHA-256 verification (or installed via your `go`/`npm` into clew's own directory) and shared across projects; per-project settings live in `<root>/.clew/lsp.toml`, which you can commit to give your team an identical setup. A management panel (toolbar → **Servers**) shows each server's status, disk use, live logs and indexing progress, with one-click download / delete / restart. Missing a server gracefully falls back to `⌘T` symbol search.

## AI features (optional)

**Explain**, **Overview**, **Ask**, **Find** and **Docs** use a language model you configure in **Settings** — any OpenAI-compatible or Anthropic endpoint, with a separate embeddings endpoint for semantic search. Everything else works without a key.

## How it's built

clew is a **client / server split**:

- **`clew`** — the GUI you run.
- **`clew-server`** — a headless backend that does the heavy lifting (scanning, indexing, LSP/DAP, git), locally or over SSH.
- **`clew-core` / `clew-protocol`** — the shared engine and the wire protocol between them.

A few things it's careful about: **virtual scrolling** so a 100k-line file renders in constant cost; every blocking operation (scan, highlight, index, search) off the UI thread so it never stutters; and a responsive layout that always keeps the code column first.

## Development

```sh
cargo test                          # unit + integration tests
cargo clippy                        # zero warnings
cargo test --release -- --ignored   # e2e tests that spawn a real rust-analyzer
```

Build a signed, notarized `.app` / `.dmg` by pushing a `v*` tag — see [`.github/workflows/release.yml`](.github/workflows/release.yml).

## Status

A personal project, macOS-only for now, and moving fast — expect some rough edges. Issues and ideas are welcome.
