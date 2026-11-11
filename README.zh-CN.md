<div align="center">

<img src="assets/logo.png" width="96" alt="Clew" />

# Clew

**像写代码的人一样读懂任何代码库。**

[![Release](https://img.shields.io/github/v/release/RutaTang/Clew?style=flat-square&color=4078f2)](https://github.com/RutaTang/Clew/releases/latest)
[![Downloads](https://img.shields.io/github/downloads/RutaTang/Clew/total?style=flat-square&color=50a14f)](https://github.com/RutaTang/Clew/releases)
![macOS · Linux](https://img.shields.io/badge/macOS%20·%20Linux-000?style=flat-square)

[English](README.md) · **简体中文**

<img src="assets/readme/hero.png" width="900" alt="Clew 打开一个函数的代码，旁边的页边栏是它的解释" />

IDE 是你写代码的地方。<br>
Clew 是一个 **IUE**，即*集成理解环境*（Integrated Understanding Environment），是你读懂代码的地方。

</div>

## 亮点

- **IDE 的基本功能，一样不少**

  IDE 提供的基本功能，Clew 也都有，例如语言服务器、tree-sitter、调试等等。

  ![IDE 的基本功能，一样不少](assets/readme/ide.gif)

- **全面理解代码**

  Clew 提供*架构概览、代码库统计、调用图、类型图、项目图、术语表*等功能，帮助你*全面理解任何代码库*。

  ![全面理解代码](assets/readme/comprehensive.gif)

- **深入理解代码细节**

  Clew 提供*导览、调用层级、语义搜索、调试、运行、模拟调试、模拟运行、代码解释、时间旅行*等功能，帮助你*全面理解任何代码库*。

  ![深入理解代码细节](assets/readme/detail.gif)

- **AI 随行**

  Clew 内置 AI 智能体，并支持 MCP 和 ACP。

  ![AI 随行](assets/readme/ai.gif)

- **语音模式**

  在语音模式下，你不用鼠标和键盘也能使用 Clew：你对 Clew 说话，其中的 AI 智能体会替你操作 Clew，并用语音回答你。

  ![语音模式](assets/readme/voice.gif)

- **跳转模式**

  在跳转模式下，只用键盘就能去往任何地方、使用任何功能。

  ![跳转模式](assets/readme/jump-mode.gif)

## 开始使用

1. **下载** [最新版本](https://github.com/RutaTang/Clew/releases/latest)：macOS（Apple 芯片）下载 `.dmg`，Linux（x86-64）下载 `.tar.gz`。
2. **打开一个文件夹**，里面是你想读的代码；再在 **设置 → 语言模型** 中填入你的模型密钥。
3. **跟着教程走一遍**：按 **⌘⇧U**（Linux 上是 Ctrl+Shift+U），Clew 会在一个小示例项目上带你熟悉它。

<sub>在 Linux 上，Clew 需要 ALSA（`libasound2`）以及 X11 或 Wayland 桌面。把压缩包解压到任意位置后运行 `clew`，`clew-server` 和 `clew-speak` 要和它放在一起。</sub>

## 了解更多

[键盘操作](docs/keyboard.zh-CN.md) · [语音](docs/voice.zh-CN.md) · [MCP 与 ACP](docs/mcp-acp.zh-CN.md) · [语言支持](docs/languages.zh-CN.md) · [项目图](docs/project-graphs.zh-CN.md)

## 参与贡献

Clew 还很年轻，每一个 issue、想法和 pull request 都有帮助：[来提一个吧](https://github.com/RutaTang/Clew/issues)。
