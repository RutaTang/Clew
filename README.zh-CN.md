<div align="center">

<img src="assets/logo.png" width="104" alt="Clew" />

# Clew

**一个 AI 驱动的「代码分析和阅读」工具**

[![CI](https://img.shields.io/github/actions/workflow/status/RutaTang/Clew/ci.yml?branch=main&style=flat-square&label=CI)](https://github.com/RutaTang/Clew/actions/workflows/ci.yml) &nbsp; [![Release](https://img.shields.io/github/v/release/RutaTang/Clew?style=flat-square&color=4078f2)](https://github.com/RutaTang/Clew/releases/latest) &nbsp; [![Downloads](https://img.shields.io/github/downloads/RutaTang/Clew/total?style=flat-square&color=50a14f)](https://github.com/RutaTang/Clew/releases) &nbsp; ![macOS](https://img.shields.io/badge/platform-macOS-000?logo=apple&logoColor=white&style=flat-square) &nbsp; ![Rust](https://img.shields.io/badge/Rust-000?logo=rust&logoColor=white&style=flat-square) &nbsp; [![Stars](https://img.shields.io/github/stars/RutaTang/Clew?style=flat-square&logo=github&color=e3b341)](https://github.com/RutaTang/Clew/stargazers) &nbsp; [![PRs welcome](https://img.shields.io/badge/PRs-welcome-4c8eda?style=flat-square)](https://github.com/RutaTang/Clew/pulls)

[English](README.md) &nbsp;|&nbsp; **简体中文**

随着 LLMs 的发展，AI 已经能在很大程度上写 Repo 级别的代码了。因为我们近乎都是用自然语言驱动 AI 编写代码，和程序语义不同，我们难以避免自然语言阐述代码时可能带来的语义模糊和歧义。因此，对于我们来说，阅读、理解和分析代码仍然是必要。Clew 就是为此设计的。

<img src="assets/screenshot.png" width="900" alt="Clew" />

</div>

## 简介

Clew 不是编辑器，而是专门为**阅读、理解和分析代码**而设计的工具。

无论是想通过阅读现有代码去**快速读懂并上手一个陌生项目**、学习它的结构与实现，还是想仔细阅读、理解、分析 **AI 写的代码**，确认它的程序语义和你的本意一致、并检查有没有 bug，Clew 都能帮到你。

不同于 Editor 或 IDE，Clew 内建了多种为「读代码」而生的能力。既有 import graph、call graph、reading trail、bookmarks、symbol-aware notes、codebase statistics、LSP、debugger 这些**非 AI 功能**，也有 Architecture Overview、Ask Clew、Explain、Semantic Search、Walkthroughs 这些 **AI 驱动功能**。

## 快速开始

从 [Releases](https://github.com/RutaTang/Clew/releases/latest) 下载最新的 Clew 版本。进入初始页面，点击 Open Folder，打开你要阅读的代码库。点击右上角的「⋯」按钮展开菜单，点击 Tutorial 按钮，Clew 会在你打开的项目上为你介绍如何使用 Clew。

![Clew 初始页面](assets/welcome.png)

## 功能总览

- 🗺️ **项目全貌**：从整体和宏观的视角看一个项目，能帮我们理解它总体上是什么、由怎样的架构组成、为什么这样设计。Clew 提供一个由 AI 驱动的 Architecture Overview 功能以及一个 Code Statistics 功能，帮你抓住主要架构，从总体上理解这个项目。
- 💬 **Ask Clew**：读代码时总会冒出各种具体问题，比如「Authentication 在哪做的?」「这里为什么要这样写?」等。Clew 提供一个懂你整个项目的 AI Agent，它会结合项目内容给你有依据的回答。
- 🕸️ **Call Graph 和 Import Graph**：想弄清模块之间谁依赖谁、函数之间谁调用谁，光读代码很难拼出全貌。Clew 把 import 关系和调用关系画成一张可旋转、自转、拖拽的实时 3D 力布局图，按语言和层级深度着色，用箭头标出依赖方向，成环处还会高亮，让代码的结构一眼可见。
- 🧭 **Walkthrough**：上手一个新项目，不管是想了解项目主要的走向和涉及的代码，还是想知道某个功能主要的走向和涉及的代码，以及从哪里入手，又该按照什么顺序读，往往并不是容易的。Clew 提供了由 AI 驱动的 Walkthrough 功能，能针对整个项目或某个具体问题，生成一条有序、锚定到代码的引导式讲解，带你一步步走过关键的文件和函数，理清阅读的主线。
- 🕰️ **Time Travel**：对于一段代码想不通为什么这么写时，往往需要回头看它以前长什么样、是什么时候、为什么变成现在这样。Clew 提供 Time Travel 功能，沿着时间轴拖动，就能把任意文件回退到它 git 历史里的任一版本。针对某个函数，它还能生成一段由 AI 驱动的 story，解释这个函数从历史一步步变成现在这样的原因。
- 🔌 **多模型支持**：Clew 的 AI 功能背后是一个可替换的语言模型，不绑定任何一家厂商。你可以在设置里填入自己的 LLM Provider 的 API key 和 endpoint。

## 语言支持

为了更好地做代码分析，Clew 会针对不同语言的特性，提供贴合它的支持。目前 Clew 提供的功能对 Rust、Python、TypeScript/JavaScript、Go 和 Dart 这 6 种语言支持良好，对其余语言目前也提供了一些基础支持，如语法高亮。未来会支持更多语言。

## 贡献

目前 Clew 项目仍然处于早期，我们欢迎和期盼各种建议，如功能需求、报告 bugs、PR 等。
