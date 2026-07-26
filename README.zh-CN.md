<div align="center">

<img src="assets/logo.png" width="104" alt="clew" />

# clew

**一个专注「读代码」的工具。**

[![CI](https://github.com/RutaTang/Clew/actions/workflows/ci.yml/badge.svg)](https://github.com/RutaTang/Clew/actions/workflows/ci.yml) &nbsp; ![macOS](https://img.shields.io/badge/platform-macOS-000?logo=apple&logoColor=white&style=flat-square) &nbsp; ![Rust](https://img.shields.io/badge/Rust-000?logo=rust&logoColor=white&style=flat-square) &nbsp; [![Stars](https://img.shields.io/github/stars/RutaTang/Clew?style=flat-square&logo=github&color=e3b341)](https://github.com/RutaTang/Clew/stargazers)

[English](README.md) &nbsp;|&nbsp; **简体中文**

你读代码的时间,远多于写代码。clew 就是为这一半而生。它是一个快速、只读的桌面应用,帮你在陌生的代码库里找到路,并真正读懂它。

<img src="assets/screenshot.png" width="900" alt="clew" />

</div>

clew 不是编辑器。没有闪烁的光标,没有保存,没有插件。它只做一件事,就是帮你**读**。正因为专注,它把编辑器当作附属功能才补上的东西做成了本分。一次永远可撤销的跳转。一张「谁 import 了谁」的 3D 地图。一个会结合被调用上下文来解释函数的 AI。一个在你改不坏的代码里滑动的 Vim 风格光标。

用 Rust + [iced](https://iced.rs) 实现,面向 macOS。

## 亮点

- 🧭 **跟得上思路的移动。** 模糊跳转到任意文件或符号、全项目 ripgrep 搜索、浏览器式的前进/后退让每次跳转都可撤销、左右分屏对照、Vim 块状光标。
- 🔍 **精确导航,开箱即用。** 跨语言的跳转定义 / 引用 / 实现 / 类型,加上 hover、诊断、inlay hints。clew 自己下载并**版本锁定**语言服务器,同一份配置在任何机器上结果一致。
- 🧠 **是理解,不只是浏览。** 对任意文件或函数一键 **Explain**、自动生成的架构 **Overview**、语义 **Find**(「我们在哪里校验 token?」)、懂你项目的 **Ask** 对话,以及原生的 **Docs** API 视图。数学公式和 Mermaid 图内联渲染。
- 🕸️ **看清代码的形状。** import 图和调用图,是一张可旋转、自转、拖拽的**实时 3D 力布局地图**,按语言和层级深度着色。
- 🐞 **读正在运行的程序。** 内置调试器(DAP),支持断点、调用栈、变量、单步。
- 🎨 **七套主题,明暗皆备。** One Dark/Light、Gruvbox Soft、Paper、Cyberpunk。跟随系统外观,或分别钉住你的明色和暗色主题。实时切换。
- 🌐 **在哪都能读代码。** 通过 SSH 打开远程项目,无头后端跑在远端主机上,流式传回你的 Mac。
- 📖 **读过的都记得。** 书签、带「已读懂」进度的笔记、导航轨迹、引导式 walkthrough,全部只存在项目的 `.clew/` 里,不写到别处。

## 快速开始

```sh
# 构建并打开一个项目
cargo run --release -- /path/to/your/project

# ……或一个文件(打开其所在目录并直接定位到它)
cargo run --release -- /path/to/file.rs

# ……或直接启动,从欢迎页选文件夹
cargo run --release
```

**首次打开一个项目时**,clew 会征求你的同意,在项目里创建 `.clew/` 目录。这个目录**本身就是授权凭证**。clew 记住的一切,包括书签、笔记、LSP 配置,都只存在这里,别无他处。建议把 `.clew/` 加进全局 gitignore,或提交它以与队友共享阅读轨迹。

第一次用?应用内导览(**⋯ 菜单 → Tutorial**)会带你把每个功能实时走一遍。

## 快捷键

| 操作 | 按键 | 操作 | 按键 |
| --- | --- | --- | --- |
| 跳转文件 | `⌘P` | 跳转定义 | `⌘` 点击 · `gd` |
| 跳转符号 | `⌘T` | 引用 / 实现 / 类型 | `gr` · `gi` · `gy` |
| 全项目搜索 | `⌘⇧F` | 移动光标 | `h j k l` · `w b` · `0 $` · `gg G` |
| 跳转行号 | `⌘L` | 分屏 | `⌘\` |
| 后退 / 前进 | `⌥←` · `⌥→` | 标记当前行 | `⌘D` |
| 新窗口 | `⌘N` | 放大 / 缩小 / 重置 | `⌘+` · `⌘-` · `⌘0` |

## 语言支持

**精确 LSP 导航**(clew 托管服务器):Rust、C/C++、Zig、Go、Python、TypeScript/JS、JSON/HTML/CSS、TOML。
**语法高亮和符号大纲**:Rust、Python、JS/JSX、TS/TSX、Go、C、C++、Java。
其余一律以纯文本打开,读代码从不中断。

clew 自带语言服务器,**版本锁定、与系统隔离**。有独立二进制的走**下载 + SHA-256 校验**,只随工具链分发的用你已装的 `go`/`npm` 装到 clew 自己的隔离目录,并跨项目共享。每项目配置放在 `<root>/.clew/lsp.toml`,可提交给团队复用同一套环境。管理面板(工具栏 → **Servers**)展示每个服务器的状态、磁盘占用、实时日志与索引进度,并支持一键下载 / 删除 / 重启。缺服务器时优雅降级回 `⌘T` 符号搜索。

## AI 功能(可选)

**Explain**、**Overview**、**Ask**、**Find**、**Docs** 需要在 **Settings** 里配置一个语言模型。可以是任意 OpenAI 兼容或 Anthropic 端点,语义搜索另配一个 embeddings 端点。其余功能无需 key 即可使用。

## 架构

clew 分成客户端和服务端两部分。

- **`clew`** 是你运行的 GUI。
- **`clew-server`** 是无头后端,承担重活(扫描、索引、LSP/DAP、git),可本地或经 SSH 远程运行。
- **`clew-core`** 和 **`clew-protocol`** 是共享引擎,以及二者之间的通信协议。

几个用心的地方:

- **虚拟滚动**,10 万行文件也是恒定渲染开销。
- 所有阻塞操作(扫描、高亮、索引、搜索)都在 UI 线程之外,永不卡顿。
- 响应式布局,代码列永远优先。

## 开发

```sh
cargo test                          # 单元 + 集成测试
cargo clippy                        # 零警告
cargo test --release -- --ignored   # 会启动真实 rust-analyzer 的端到端测试
```

推一个 `v*` tag 即可构建签名并公证的 `.app` 和 `.dmg`。见 [`.github/workflows/release.yml`](.github/workflows/release.yml)。

## 状态

个人项目,目前仅 macOS,更新很快。难免有粗糙之处。欢迎提 issue 和想法。
