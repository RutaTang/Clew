<div align="center">

<img src="assets/logo.png" width="96" alt="Clew" />

# Clew

**Read any codebase like the person who wrote it.**

[![Release](https://img.shields.io/github/v/release/RutaTang/Clew?style=flat-square&color=4078f2)](https://github.com/RutaTang/Clew/releases/latest)
[![Downloads](https://img.shields.io/github/downloads/RutaTang/Clew/total?style=flat-square&color=50a14f)](https://github.com/RutaTang/Clew/releases)
![macOS · Linux](https://img.shields.io/badge/macOS%20·%20Linux-000?style=flat-square)

**English** · [简体中文](README.zh-CN.md)

<img src="assets/readme/hero.png" width="900" alt="Clew showing a project's code, with Ask's answer beside it citing the lines it read" />

Your IDE is where you write code.<br>
Clew is an **IUE**, an *Integrated Understanding Environment*, where you understand it.

</div>

## ✨ Highlights

| | |
|---|---|
| 🗺️ **See the shape** | An architecture overview, and live 3D graphs of who calls and imports whom |
| 🧭 **Know where to start** | A reading guide, and step-by-step walkthroughs of any flow |
| 💬 **Ask anything** | An agent that knows the whole project and shows the lines it read |
| 🕰️ **Travel in time** | Step a function back through its git history, and ask why it changed |
| 🧪 **Run it in your head** | Step through a function on inputs you give it, without running it |
| 🎙️ **Just talk** | Ask aloud and hear the answer, with speech kept on your computer |

**Works with** any LLM · MCP and ACP · 15 languages, from Rust and Python to Haskell and Zig

## See it

**Ask about the code.** The answer cites the lines it came from; click one to go there.

<img src="assets/readme/ask.gif" width="800" alt="A question about declined cards answered from checkout.py, with file and line sources" />

**See how it fits together.** Spin the call graph, search it, and fly to a file.

<img src="assets/readme/graph.gif" width="800" alt="The 3D call graph spinning, searched for pricing, and the file opened" />

**Go back in time.** Step a function through its history, and ask what each change was for.

<img src="assets/readme/time-travel.gif" width="800" alt="Time Travel showing a tax bug in an older version of total, then the fix and its explanation" />

**Run it in your head.** Clew's model traces a function line by line, on inputs you describe.

<img src="assets/readme/fake-run.gif" width="800" alt="Fake run of discounts for a member's order, stepping line by line with its variables" />

**Never leave the keyboard.** **⌘J** puts a short code on every button; type it to press it.

<img src="assets/readme/jump-mode.gif" width="800" alt="Jump mode labelling each control with a code that presses it" />

## Get started

1. **Download** the [latest release](https://github.com/RutaTang/Clew/releases/latest): the `.dmg` for macOS (Apple silicon) or the `.tar.gz` for Linux (x86-64).
2. **Open a folder** with the code you want to read, and add your model's key in **Settings → Language model**.
3. **Take the tour**: **⌘⇧U** (Ctrl+Shift+U on Linux) walks you through Clew on a small sample project.

<sub>On Linux, Clew needs ALSA (`libasound2`) and an X11 or Wayland desktop. Unpack the archive anywhere and run `clew`, keeping `clew-server` and `clew-speak` beside it.</sub>

## Learn more

[Keyboard](docs/keyboard.md) · [Voice](docs/voice.md) · [MCP and ACP](docs/mcp-acp.md) · [Language support](docs/languages.md) · [Project graphs](docs/project-graphs.md)

## Contributing

Clew is young, and every issue, idea and pull request helps: [open one](https://github.com/RutaTang/Clew/issues).
