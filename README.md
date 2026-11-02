<div align="center">

<img src="assets/logo.png" width="104" alt="Clew" />

# Clew

**An AI-powered tool for analyzing and reading code**

[![CI](https://img.shields.io/github/actions/workflow/status/RutaTang/Clew/ci.yml?branch=main&style=flat-square&label=CI)](https://github.com/RutaTang/Clew/actions/workflows/ci.yml) &nbsp; [![Release](https://img.shields.io/github/v/release/RutaTang/Clew?style=flat-square&color=4078f2)](https://github.com/RutaTang/Clew/releases/latest) &nbsp; [![Downloads](https://img.shields.io/github/downloads/RutaTang/Clew/total?style=flat-square&color=50a14f)](https://github.com/RutaTang/Clew/releases) &nbsp; ![macOS](https://img.shields.io/badge/platform-macOS-000?logo=apple&logoColor=white&style=flat-square) &nbsp; ![Rust](https://img.shields.io/badge/Rust-000?logo=rust&logoColor=white&style=flat-square) &nbsp; [![Stars](https://img.shields.io/github/stars/RutaTang/Clew?style=flat-square&logo=github&color=e3b341)](https://github.com/RutaTang/Clew/stargazers) &nbsp; [![PRs welcome](https://img.shields.io/badge/PRs-welcome-4c8eda?style=flat-square)](https://github.com/RutaTang/Clew/pulls)

**English** &nbsp;|&nbsp; [简体中文](README.zh-CN.md)

As LLMs advance, AI can already write code at the scale of a whole repository. But we drive that AI almost entirely in natural language, and natural language is not program semantics. It inevitably brings ambiguity and vagueness when it describes code. So reading, understanding, and analyzing code stays necessary for us. Clew is built for exactly that.

<img src="assets/screenshot.png" width="900" alt="Clew" />

</div>

## Introduction

Clew is not an editor. It is a tool built specifically for **reading, understanding, and analyzing code**.

Whether you want to **quickly get up to speed on an unfamiliar project** by reading its existing code and learning its structure and implementation, or to carefully read, understand, and analyze **AI-written code** to confirm its semantics match what you intended and check it for bugs, Clew has you covered.

Unlike an editor or IDE, Clew builds in a range of capabilities made for reading code. There are **non-AI features** like import graph, call graph, reading trail, bookmarks, symbol-aware notes, codebase statistics, LSP, and debugger, and there are **AI-powered features** like Architecture Overview, Ask Clew, Explain, Semantic Search, and Walkthroughs.

## Quick start

Download the latest Clew from [Releases](https://github.com/RutaTang/Clew/releases/latest). On the welcome screen, click **Open Folder** and open the codebase you want to read. Then click the **⋯** button in the top right to open the menu and choose **Tutorial**, and Clew will walk you through how to use it right inside the project you opened.

![Clew welcome screen](assets/welcome.png)

## Features

- 🗺️ **The big picture**: Looking at a project from a high level helps you grasp what it is overall, how it is structured, and why it is designed the way it is. Clew gives you an AI-powered **Architecture Overview** and a **Code Statistics** view, so you can catch the main architecture and understand the project as a whole.
- 💬 **Ask Clew**: Questions come up constantly while reading code, like "where is Authentication done?" or "why is this written this way?". Clew gives you an AI agent that knows your whole project and answers with grounding in its actual content.
- 🕸️ **Call Graph and Import Graph**: To see which modules depend on which, and which functions call which, plain reading rarely adds up to a full picture. Clew draws the import and call relationships as a live 3D force-directed map you can orbit, spin, and drag, colored by language and hierarchy depth, with arrows marking the direction of dependency and cycles highlighted, so the structure of the code is visible at a glance.
- 🧭 **Walkthrough**: Getting into a new project is rarely easy. Whether you want to understand the project's main flow and the code it involves, or a specific feature's flow and code, and figure out where to start and what order to read in, it takes work. Clew's AI-powered **Walkthrough** can, for the whole project or a specific question, generate an ordered, code-anchored guided tour that walks you step by step through the key files and functions and lays out the main thread of reading.
- 🕰️ **Time Travel**: When you can't figure out why a piece of code is written the way it is, you often need to look back at what it used to be, when it changed, and why it became what it is now. Clew's **Time Travel** lets you drag along a timeline to roll any file back to any version in its git history. For a specific function, it can also generate an AI-powered **story** that explains how that function evolved into its current shape.
- 🔌 **Bring your own model**: The language model behind Clew's AI features is swappable and not tied to any single vendor. You can enter your own LLM provider's API key and endpoint in the settings.
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

**Clew as an ACP agent.** `clew-server acp` serves Ask Clew's agent to editors that host ACP agents, answering with the model set in Clew's settings. The MCP servers the editor hands over join its tools, each call asking you first. For example, in Zed's `settings.json`:

```json
"agent_servers": {
  "Clew": { "command": "/Applications/Clew.app/Contents/MacOS/clew-server", "args": ["acp"] }
}
```

## Language support

To analyze code well, Clew tailors its support to each language's characteristics. Its features currently support Rust, Python, TypeScript/JavaScript, Go, and Dart well, and other languages get some basic support for now, such as syntax highlighting. More languages are coming.

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

The code the build then leaves out is dimmed as you read: Rust items behind a `cfg` of a feature the package builds without (or of another platform), Go files whose `//go:build` line or `_windows.go`-style name excludes them, C/C++ `#if`/`#ifdef` groups the preprocessor drops for the platform and the `-D`/`-U` flags, and Zig's comptime `if`/`switch` branches on `builtin.os.tag` or `builtin.cpu.arch` the compiler does not analyze for the platform. Only what is certain is dimmed — a feature another package may turn on, or a macro a header may define, is left alone.

- **SQL**: the outline lists a schema's tables (with their columns), views, functions, triggers and types, each with the `--` comment above it as its doc, and the type map draws the tables joined by their foreign keys — declared in the table or in an `ALTER TABLE` of the same file, as `pg_dump` writes them — and each view to what it selects from.
- **Lean 4**: the outline (definitions, theorems, instances, structures, classes, inductive types) with their `/-- … -/` docs and the file's `/-! … -/` module doc, the import map, structures that `extends` others in the type map, and the Lean language server (`lake serve`, from the Lean toolchain elan installs). Lean's syntax is extensible, so a command clew's grammar cannot read is outlined from its first line, and costs no other command its outline.

## Contributing

Clew is still early, and all kinds of input are welcome and hoped for, whether feature requests, bug reports, or pull requests.
