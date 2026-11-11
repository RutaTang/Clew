<div align="center">

<img src="assets/logo.png" width="96" alt="Clew" />

# Clew

**像写代码的人一样读懂任何代码库。**

[![Release](https://img.shields.io/github/v/release/RutaTang/Clew?style=flat-square&color=4078f2)](https://github.com/RutaTang/Clew/releases/latest)
[![Downloads](https://img.shields.io/github/downloads/RutaTang/Clew/total?style=flat-square&color=50a14f)](https://github.com/RutaTang/Clew/releases)
![macOS · Linux](https://img.shields.io/badge/macOS%20·%20Linux-000?style=flat-square)

[English](README.md) · **简体中文**

<img src="assets/readme/hero.png" width="900" alt="Clew 打开一个项目的代码，旁边是 Ask 的回答，并注明它读过的代码行" />

IDE 是你写代码的地方。<br>
Clew 是一个 **IUE**，即*集成理解环境*（Integrated Understanding Environment），是你读懂代码的地方。

</div>

## ✨ 亮点

| | |
|---|---|
| 🗺️ **看清全貌** | 架构概览，以及谁调用谁、谁导入谁的实时 3D 图 |
| 🧭 **知道从哪读起** | 阅读指南，以及任意流程的分步导览 |
| 💬 **有问必答** | 一个读过整个项目的智能体，并给出它读过的代码行 |
| 🕰️ **回到过去** | 让一个函数沿着 git 历史一步步倒退，并问它为什么改 |
| 🧪 **在脑中运行** | 按你给的输入一步步走完一个函数，而不真正运行它 |
| 🎙️ **直接说话** | 开口提问、听到回答，语音都留在你的电脑上 |

**支持** 任意 LLM · MCP 与 ACP · 15 种语言，从 Rust、Python 到 Haskell、Zig

## 看看效果

**就代码提问。** 回答会注明出自哪几行代码，点一下就跳过去。

<img src="assets/readme/ask.gif" width="800" alt="一个关于信用卡被拒的问题，回答出自 checkout.py，并附有文件和行号" />

**看清各部分如何连在一起。** 旋转调用图、在图里搜索，然后飞到某个文件。

<img src="assets/readme/graph.gif" width="800" alt="旋转的 3D 调用图，搜索 pricing 后打开这个文件" />

**回到过去。** 让一个函数沿着历史一步步走，并问每次改动是为了什么。

<img src="assets/readme/time-travel.gif" width="800" alt="时间旅行先显示旧版 total 里的税费 bug，再显示修复和它的解释" />

**在脑中运行。** Clew 的模型按你描述的输入，一行一行地推演一个函数。

<img src="assets/readme/fake-run.gif" width="800" alt="对一位会员的订单模拟运行 discounts，逐行显示变量" />

**手不离键盘。** **⌘J** 会在每个按钮上标出一个短代码，输入它就等于按下按钮。

<img src="assets/readme/jump-mode.gif" width="800" alt="跳转模式给每个控件标上代码，输入代码即可按下它" />

## 开始使用

1. **下载** [最新版本](https://github.com/RutaTang/Clew/releases/latest)：macOS（Apple 芯片）下载 `.dmg`，Linux（x86-64）下载 `.tar.gz`。
2. **打开一个文件夹**，里面是你想读的代码；再在 **设置 → 语言模型** 中填入你的模型密钥。
3. **跟着教程走一遍**：按 **⌘⇧U**（Linux 上是 Ctrl+Shift+U），Clew 会在一个小示例项目上带你熟悉它。

<sub>在 Linux 上，Clew 需要 ALSA（`libasound2`）以及 X11 或 Wayland 桌面。把压缩包解压到任意位置后运行 `clew`，`clew-server` 和 `clew-speak` 要和它放在一起。</sub>

## 了解更多

[键盘操作](docs/keyboard.zh-CN.md) · [语音](docs/voice.zh-CN.md) · [MCP 与 ACP](docs/mcp-acp.zh-CN.md) · [语言支持](docs/languages.zh-CN.md) · [项目图](docs/project-graphs.zh-CN.md)

## 参与贡献

Clew 还很年轻，每一个 issue、想法和 pull request 都有帮助：[来提一个吧](https://github.com/RutaTang/Clew/issues)。
