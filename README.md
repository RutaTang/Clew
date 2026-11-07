<div align="center">

<img src="assets/logo.png" width="104" alt="Clew" />

# Clew

**An AI-powered tool for analyzing and reading code**

[![CI](https://img.shields.io/github/actions/workflow/status/RutaTang/Clew/ci.yml?branch=main&style=flat-square&label=CI)](https://github.com/RutaTang/Clew/actions/workflows/ci.yml) &nbsp; [![Release](https://img.shields.io/github/v/release/RutaTang/Clew?style=flat-square&color=4078f2)](https://github.com/RutaTang/Clew/releases/latest) &nbsp; [![Downloads](https://img.shields.io/github/downloads/RutaTang/Clew/total?style=flat-square&color=50a14f)](https://github.com/RutaTang/Clew/releases) &nbsp; ![Platforms](https://img.shields.io/badge/platform-macOS%20%7C%20Linux-000?style=flat-square) &nbsp; ![Rust](https://img.shields.io/badge/Rust-000?logo=rust&logoColor=white&style=flat-square) &nbsp; [![Stars](https://img.shields.io/github/stars/RutaTang/Clew?style=flat-square&logo=github&color=e3b341)](https://github.com/RutaTang/Clew/stargazers) &nbsp; [![PRs welcome](https://img.shields.io/badge/PRs-welcome-4c8eda?style=flat-square)](https://github.com/RutaTang/Clew/pulls)

**English** &nbsp;|&nbsp; [简体中文](README.zh-CN.md)

As LLMs advance, AI can already write code at the scale of a whole repository. But we drive that AI almost entirely in natural language, and natural language is not program semantics. It inevitably brings ambiguity and vagueness when it describes code. So reading, understanding, and analyzing code stays necessary for us. Clew is built for exactly that.

<img src="assets/screenshot.png" width="900" alt="Clew" />

</div>

## Introduction

Clew is not an editor. It is a tool built specifically for **reading, understanding, and analyzing code**.

Whether you want to **quickly get up to speed on an unfamiliar project** by reading its existing code and learning its structure and implementation, or to carefully read, understand, and analyze **AI-written code** to confirm its semantics match what you intended and check it for bugs, Clew has you covered.

Unlike an editor or IDE, Clew builds in a range of capabilities made for reading code. There are **non-AI features** like import graph, call graph, reading trail, bookmarks, symbol-aware notes, codebase statistics, LSP, and debugger, and there are **AI-powered features** like Architecture Overview, Ask Clew, Explain, Semantic Search, and Walkthroughs.

## Quick start

Download the latest Clew from [Releases](https://github.com/RutaTang/Clew/releases/latest): the `.dmg` for macOS (Apple silicon) or the `.tar.gz` for Linux (x86-64; unpack it anywhere and run `clew`, keeping `clew-server` beside it). On Linux it needs ALSA's library (`libasound2`, which every desktop has) and an X11 or Wayland desktop. On the welcome screen, click **Open Folder** and open the codebase you want to read. Then click the **⋯** button on the tool strip down the right edge and choose **Tutorial**, and Clew will walk you through how to use it right inside the project you opened.

Everything Clew does is also a keystroke away (on Linux and Windows, **⌘** is **Ctrl**): **⌘⇧P** opens the command palette, where you can search for any command — the graphs and maps, the sidebar's views and panels, what the right-click menu does at the cursor, the debugger's controls, the theme — and run it with Enter. Typing `>` in the file finder (⌘P) does the same, and **⌘/** lists the shortcuts. Or say it: **⌘⇧S** listens for a command or a question (see [Voice](#voice)).

![Clew welcome screen](assets/welcome.png)

## Features

- 🗺️ **The big picture**: Looking at a project from a high level helps you grasp what it is overall, how it is structured, and why it is designed the way it is. Clew gives you an AI-powered **Architecture Overview** and a **Code Statistics** view, so you can catch the main architecture and understand the project as a whole.
- 💬 **Ask Clew**: Questions come up constantly while reading code, like "where is Authentication done?" or "why is this written this way?". Clew gives you an AI agent that knows your whole project and answers with grounding in its actual content. Ask is the whole project's, so it lives in the window's bottom-right corner (**⌘⇧A**): a small pill until you open it, then a card there, out of the way of the code above and beside it, or, made large, a window in the middle; Esc steps it back to the pill. The field at the top of the window searches files, symbols and, after `>`, commands. The agent can also act in the window — open a file at a line, go to a symbol, run any command of the palette ("show me the call graph") — asking you first for a command that costs something.
- 🎙️ **Voice**: Press **⌘⇧S** (or the microphone on Ask) and talk: say a command ("open the call graph", "hide the minimap", "dark mode") and it runs; ask a question and it goes to Ask. Speech is recognized **on your computer** by [whisper.cpp](https://github.com/ggml-org/whisper.cpp) — nothing you say leaves it — with a model you pick and download once in **Settings → Voice** — Echo's own three (Small, Medium and Large: whisper's `small-q5_1`, `large-v3-turbo-q5_0` and `large-v3-q5_0`) or a small English-only one — each checked against its published checksum. See [Voice](#voice).
- 🕸️ **Call Graph and Import Graph**: To see which modules depend on which, and which functions call which, plain reading rarely adds up to a full picture. Clew draws the import and call relationships as a live 3D force-directed map you can orbit, spin, and drag, colored by language and hierarchy depth, with arrows marking the direction of dependency and cycles highlighted, so the structure of the code is visible at a glance.
- 🧱 **Project maps**: Much of how a project fits together is said in its languages' and frameworks' own terms, which no import or call graph shows: the database's tables, the web app's routes, what Spring injects where, an OTP supervision tree, the interfaces a Go type satisfies without saying so. Clew reads these from the files and draws each as a map, each node opening where it is defined (see [Project maps](#project-maps)).
- 🧭 **Walkthrough**: Getting into a new project is rarely easy. Whether you want to understand the project's main flow and the code it involves, or a specific feature's flow and code, and figure out where to start and what order to read in, it takes work. Clew's AI-powered **Walkthrough** can, for the whole project or a specific question, generate an ordered, code-anchored guided tour that walks you step by step through the key files and functions and lays out the main thread of reading.
- 🕰️ **Time Travel**: When you can't figure out why a piece of code is written the way it is, you often need to look back at what it used to be, when it changed, and why it became what it is now. Clew's **Time Travel** lets you drag along a timeline to roll any file back to any version in its git history. For a specific function, it can also generate an AI-powered **story** that explains how that function evolved into its current shape.
- 🔌 **Bring your own model**: The language model behind Clew's AI features is swappable and not tied to any single vendor. You can enter your own LLM provider's API key and endpoint in the settings.
- 🌐 **Your language**: Clew speaks English, French, Simplified Chinese, Korean and Japanese. It follows your system's language, or the one you pick in **Settings → Appearance → Language**, and its AI answers, explanations and walkthroughs come back in that language too. The common errors git, SSH and the system report are shown in it as well, and language servers and debuggers that translate their own messages are asked for it.
- 🧩 **MCP and ACP**: Ask Clew can use the tools of your own **MCP** servers (issue trackers, docs, databases), and can hand your question to a coding agent that speaks **ACP** (Claude Agent, Codex, Gemini CLI, GitHub Copilot, …). The other way round, Clew serves its code-intelligence tools over MCP to any agent, and its own agent over ACP to editors.

## MCP and ACP

**MCP servers in Ask Clew.** In **Settings → MCP servers**, add a server by its command (`npx -y @modelcontextprotocol/server-github`), its URL (`https://…/mcp`), or by pasting the `{"mcpServers": {…}}` snippet from its README. Clew speaks both the current stateless MCP revision (2026-07-28) and the earlier `initialize`-based ones (2025-11-25 back to 2024-11-05), over stdio and Streamable HTTP. A remote server that wants you to sign in (MCP's OAuth flow) gets a **Sign in** button that finishes in your browser; the tokens are kept on your machine and renewed as they expire. Each tool call asks you first (with the arguments shown), unless you allow that tool always, or set the server to trust read-only tools or every call. The servers are stored in `config.toml`:

```toml
[mcp.servers.github]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]
env = { GITHUB_PERSONAL_ACCESS_TOKEN = "…" }
approve = "ask"            # ask | read_only | always
allow = ["search_code"]    # tools that never ask
timeout_secs = 120

[mcp.servers.docs]
url = "https://example.com/mcp"
headers = { Authorization = "Bearer …" }
```

**ACP agents in the Ask panel.** In **Settings → ACP agents**, add an agent (presets for Claude Agent, Codex, Gemini CLI, GitHub Copilot, Qwen Code and OpenCode, or any command that starts an agent in ACP mode), then pick it in the Ask panel. The agent runs on your machine and works on the project, with Clew's code tools available to it (agents and MCP servers start from your home folder, or an entry's `cwd`, so a project's own `.npmrc` cannot change what `npx` runs); its plan, tool calls and answer stream into the panel, and when it asks for permission, you answer there. ACP agents work on local projects.

**Clew as an MCP server.** `clew-server mcp` serves the project it runs in (or `--root <dir>`) over stdio: exact and semantic search, reading and outlining files, language-server definitions, references and hover, git history and the current changes, and the project's cached explanations. For example, with Claude Code:

```sh
claude mcp add clew -- /Applications/Clew.app/Contents/MacOS/clew-server mcp
```

(On Linux and Windows, `clew-server` is the one beside `clew` in the folder you unpacked.)

**Clew as an ACP agent.** `clew-server acp` serves Ask Clew's agent to editors that host ACP agents, answering with the model set in Clew's settings. The MCP servers the editor hands over join its tools, each call asking you first. For example, in Zed's `settings.json`:

```json
"agent_servers": {
  "Clew": { "command": "/Applications/Clew.app/Contents/MacOS/clew-server", "args": ["acp"] }
}
```

## Voice

**Talking.** Press **⌘⇧S** (Ctrl+Shift+S on Linux and Windows; rebindable, and in the palette as **Talk**) or the microphone button, and say what you want. Listening stops on its own when you pause, or when you press the key again; **Esc** cancels. What you said is then either one of the window's commands — said the way you would say it, in any order, with or without "open" or "show"; a command that costs something (an Explain All pass, starting the program, opening another folder) asks before it runs — or a question, which goes to Ask and is asked at once (both can be turned off in **Settings → Voice**). Voice shows what it is doing in Ask's corner: the pill says it is listening, and after a command what it did, without opening the card; a question opens the card with its answer.

**On your machine.** The microphone is read through the system's own audio (Core Audio, WASAPI, ALSA — which PipeWire and PulseAudio serve), the audio is resampled to 16 kHz, cut to where there is speech, and transcribed by whisper.cpp on this computer, on the GPU through Metal on a Mac and on the CPU elsewhere. Nothing is sent anywhere; only a question you choose to ask goes to your language model, as typed ones do.

**Models.** Whisper.cpp's own models, the same ones [Echo](https://github.com/damien-schneider/Echo) uses, downloaded only when you click a model's **Download** (its size is on the button) and accepted only if they are, byte for byte, the published file: **Small** (`small-q5_1`, 190 MB, the default), **Medium** (`large-v3-turbo-q5_0`, 574 MB) and **Large** (`large-v3-q5_0`, 1081 MB), checked by their SHA-256 and exact size as Echo checks them, all of them multilingual; and Tiny and Base (English) for quick commands on slower machines, checked by the SHA-1 whisper.cpp lists. They are kept in Clew's data folder (`voice/`), and can be removed there. On macOS, the first time Clew listens, the system asks whether it may use the microphone.

Voice works the same on macOS and Linux, as does everything else in Clew's window: the whole app is built and tested on both in CI, and each release ships it for both.

## Language support

To analyze code well, Clew tailors its support to each language's characteristics. Its features fully support Rust, Python, TypeScript/JavaScript, Go, Dart, Java, C#, PHP, Ruby, Erlang, Haskell and Zig; C and C++, Lean 4 and SQL get most of them, and other languages get some basic support for now, such as syntax highlighting. More languages are coming.

**Settings → Languages in this project** shows, for the open project, each language's server and how it stands, and lets you set each language up — every setting shows what is in effect before what you can pick:

- **Python** — the interpreter the language server and debugger use, and who named it (your choice, the project's `.clew/lsp.toml`, a virtualenv found in the project). Pick another from the environments Clew finds (the project's own, Poetry, Pipenv, conda, pyenv, uv, or a `python3` on your `PATH`), or give a path.
- **Rust** — the Cargo features rust-analyzer loads the project with (every package's features are listed from its `Cargo.toml`; or all of them, with or without the defaults), and the target it checks for.
- **Go** — the build tags gopls and the debugger build with (the tags the project's `//go:build` lines use are listed), and the `GOOS`/`GOARCH` to read the project for.
- **C and C++** — the `compile_commands.json` clangd uses (the ones CMake, Meson and others leave in the project are listed), and flags for files no compilation database covers (`-DDEBUG -Iinclude -std=c++20`).
- **Lean and Dart** — the toolchain their server runs from: elan's toolchains (the one the project's `lean-toolchain` names first), Flutter SDKs and fvm's versions (the project's first).
- **TypeScript and JavaScript** — the TypeScript the server runs: Clew's own by default, never the project's unless you choose it (it is listed, with its version, as running the project's code), or a global install.
- **Zig** — the `zig` compiler zls works with: zvm's and zigup's versions (the one the project's `build.zig.zon` asks for first), or one you name.
- **Java** — the JDK the Eclipse JDT language server builds the project with: the JDKs on your machine (`JAVA_HOME`, `/usr/lib/jvm`, macOS's, SDKMAN!'s, IntelliJ's, asdf's, mise's and Homebrew's), the one of the Java version the build asks for (`.java-version`, `.sdkmanrc`, a Gradle toolchain, the `pom.xml`) first.
- **PHP** — the PHP version intelephense reads the code as (the lowest one `composer.json` allows first).
- **C#** — the solution csharp-ls opens: the `.sln` and `.slnx` files at the project's root and a folder down.
- **Ruby, Erlang and Haskell** — the Ruby LSP of one of your Rubies (rbenv's, chruby's, asdf's, mise's, RVM's; the project's `.ruby-version` first), and the ELP and Haskell language server (ghcup's, Stack's, Cabal's) to run, or the ones on your `PATH`.
- **Any language** — a server program of your own: another build of Clew's server, another server, or one for a language Clew has none for. A server that builds the project still asks before it does.

Everything is found by reading files — nothing is run — and, for a remote project, on its host. The choices are kept in Clew's own data for this project on this machine, never written into the project, win over its `lsp.toml`, and reach the editor's language servers, the debugger and Ask's code tools alike.

The code the build then leaves out is dimmed as you read: Rust items behind a `cfg` of a feature the package builds without (or of another platform), Go files whose `//go:build` line or `_windows.go`-style name excludes them, C/C++ `#if`/`#ifdef` groups the preprocessor drops for the platform and the `-D`/`-U` flags, Zig's comptime `if`/`switch` branches on `builtin.os.tag` or `builtin.cpu.arch` the compiler does not analyze for the platform, C#'s `#if` groups of the symbols the file `#define`s, Haskell's CPP `#if` groups on `<os>_HOST_OS` and `<arch>_HOST_ARCH` (in a file that turns CPP on), and Erlang's `-ifdef`/`-if` groups of the macros the file `-define`s. Where it depends on the platform, the platform read for is the one picked in the status bar. Only what is certain is dimmed — a feature another package may turn on, or a macro a header may define, is left alone.

- **SQL**: the outline lists a schema's tables (with their columns), views, functions, triggers and types, each with the `--` comment above it as its doc, and the type map draws the tables joined by their foreign keys — declared in the table or in an `ALTER TABLE` of the same file, as `pg_dump` writes them — and each view to what it selects from.
- **Java, C#, PHP, Ruby, Erlang, Haskell and Zig**: highlighting; the outline, each definition with its doc (Javadoc, C#'s `///` XML docs, PHPDoc, Ruby's `#` comments, EDoc, Haddock, Zig's `///`) and tests marked (JUnit, NUnit/xUnit/MSTest, PHPUnit, Minitest, EUnit, Zig's `test` blocks); the import map; the call graph; and a language server (Eclipse JDT LS, csharp-ls, intelephense, Ruby LSP, ELP, the Haskell language server, zls). The debugger runs Ruby programs under rdbg (the `debug` gem; the chosen Ruby's own), .NET assemblies under netcoredbg, Zig's under lldb-dap, as native code (a `.zig` file is built for the session through LLVM, `-fllvm`, whose debug information lldb reads; a binary from Zig's own code generator — its Debug default on x86-64 — debugs too, but lldb reads no variables from it, and the debug console says so and how to build one it can), and — through debug adapters of Clew's own — Java programs over JDWP, with nothing but the JDK chosen for the project (a `.java` file is compiled for the session; a build's program is named by its `mainClass` and `classPaths`; breakpoints are set as their classes load; locals, fields, arrays and strings are read; and since JDWP evaluates nothing itself, Clew works out Java expressions over it — names, fields, elements, literals, arithmetic, comparisons and logic, string concatenation, and calls such as `order.total()`, `Math.max(a, b)` or `items.get(0) * 2`, an overload chosen and values boxed or unboxed as Java would, the method run in the stopped thread alone, a breakpoint it meets passed; nothing is called for a hover, and nothing from a pause, where the VM runs no method), PHP programs under Xdebug 3, Erlang programs under OTP's interpreter (a module's `main/1`, or the entry the launch names, its application compiled with `debug_info` for the session) and Haskell programs under GHCi's debugger (the project's source directories from its `.cabal` or `package.yaml`; a launch may name `cabal repl` or `stack repl` instead); breakpoints, stepping, the stack, variables and evaluation work in each, as far as the language's own debugger goes: Erlang and Java pause a running program, and so does PHP under Xdebug 3.4 or later on Linux (through Xdebug's control socket), or under an Xdebug that takes commands while the program runs; GHCi steps out by stepping until the function returns, but cannot pause one. All of this works on a remote host too, where Clew's server serves an adapter that listens on a port of that host (rdbg, Go's delve) over its own connection, and debugs Node.js programs with an adapter of its own over Node's inspector — vscode-js-debug, which Clew runs on your machine, opens a connection per debugged process that no remote channel carries. It has breakpoints with conditions, stepping, a real pause (into the program's own code, not Node's), caught and uncaught exceptions, each scope's variables and V8's own evaluation (nothing with a side effect for a hover), and runs TypeScript as Node 22.6 and later does, its types stripped in place, so every line is where the source has it; it follows the main thread, not workers or child processes. A remote project's `composer.json` and Haskell package descriptions are read on its host, so its imports resolve as a local project's do. Imports resolve the way each language builds: Java's packages under the project's source roots (`src/main/java`), C#'s `using` to the namespace the project's files declare, PHP's classes by `composer.json`'s PSR-4 map and its `require` paths, Ruby's `require_relative` and `require` from `lib/`, Erlang's `-include`/`-include_lib` and `-behaviour`, Haskell's modules under the `.cabal`'s or `package.yaml`'s source directories, and Zig's `@import` of a path; what is not the project's (the JDK, NuGet, Composer packages, gems, OTP, Hackage, `std`) is listed by package.
- **Lean 4**: the outline (definitions, theorems, instances, structures, classes, inductive types) with their `/-- … -/` docs and the file's `/-! … -/` module doc, the import map, structures that `extends` others in the type map, and the Lean language server (`lake serve`, from the Lean toolchain elan installs). Lean's syntax is extensible, so a command clew's grammar cannot read is outlined from its first line, and costs no other command its outline.

## Project maps

The **Project maps** tool on the tool strip opens the maps Clew reads from the project's files, statically, as a reader would — a route registered at run time from data, or a table only an ORM creates, is not seen. A map that finds nothing is not listed, and for a remote project they are read on its host.

- **SQL** — the schema (tables, columns, foreign keys, views), the schema after each migration in order (Rails migrations too, a down migration left out), the lineage of each view and `INSERT … SELECT` back to the tables it reads, and the code that names each table.
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

## Contributing

Clew is still early, and all kinds of input are welcome and hoped for, whether feature requests, bug reports, or pull requests.
