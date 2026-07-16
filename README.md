# clew

*a reader for code* — 一个专注「读代码」的桌面工具，Rust + [iced](https://iced.rs) 实现。

clew 不是编辑器：没有光标、没有保存、没有插件。它优化的是读代码时最高频的三件事——**找到、跳转、回退**。

## 功能（v2）

| 功能 | 说明 | 快捷键 |
| --- | --- | --- |
| 文件树 | 遵守 `.gitignore`，自动隐藏 `target/`、`node_modules/` 等 | 点击展开 |
| 代码视图 | 只读、tree-sitter 语法高亮、虚拟滚动（10 万行文件不卡） | — |
| 模糊文件查找 | 输入几个字母跳到任意文件 | `⌘P` |
| 符号搜索 | 全项目函数/结构体/类的模糊搜索，跳到定义行 | `⌘T` |
| 全项目搜索 | ripgrep 内核，字面量 smart-case 匹配，点击结果跳到行 | `⌘⇧F` |
| 跳转行号 | 查找框输入 `:123` 回车 | `⌘L` |
| 导航历史 | 像浏览器一样后退/前进，跳转永远可撤销 | `⌥←` / `⌥→` |
| 分屏对照 | 左右两个窗格，各自独立滚动；开启时复制当前文件 | `⌘\` |
| 行选择与复制 | 点击/拖拽/Shift 点击选择行区间，复制原始文本 | `⌘C`，Esc 清除 |
| 书签 | 标记当前行，MARKS 面板管理，跟随项目持久化 | `⌘D` |
| 符号大纲 | 当前文件的函数/结构体列表，点击跳转 | 工具栏 Outline |
| 阅读光标 | Vim normal-mode 风格块光标，键盘移动（只读、不可编辑） | `h/j/k/l`·`w/b`·`0/$`·`gg/G`·方向键 |
| 跳转定义 | 经 LSP 精确跳到定义（clew 托管自己的语言服务器） | `⌘`点击 或 `gd` |
| 跳转引用/实现/类型 | LSP 查引用（进 Search 面板）、跳实现、跳类型定义 | `gr` / `gi` / `gy` |
| 字号调整 | 放大/缩小/重置，保持当前阅读位置 | `⌘+` / `⌘-` / `⌘0` |

## 使用

```sh
cargo run --release -- <项目目录>    # 打开一个项目
cargo run --release -- <文件路径>    # 打开文件所在目录并直接定位该文件
cargo run --release                  # 打开后从欢迎页选择文件夹
```

所有持久化数据都只存放在项目内 `<root>/.clew/`，不会写到项目之外的任何位置。

**首次打开一个项目时**，clew 会弹出应用内确认框，征求你同意在项目里创建 `.clew/` 目录；不同意就不打开该项目。`.clew/` 目录本身即是「已授权」的凭证——之后再打开同一项目不再询问。若项目目录不可写，创建失败会在状态栏提示且项目不打开。

书签存于 `.clew/bookmarks.json`（打第一个书签时创建，删光后清理该文件、保留 `.clew/` 目录）。建议把 `.clew/` 加进全局 gitignore，或提交它来与队友共享阅读标记。

## 跳转定义 / 语言服务器

`⌘`点击一个符号即可精确跳转到定义。clew **自带并托管自己的语言服务器**（版本锁定），与系统上装的 rust-analyzer 等完全隔离——同一份配置在任何机器上跑同一个 server。

- **二进制全局共享**：首次用到某语言时弹窗征求同意下载对应 server，下载会校验 SHA-256，之后所有项目复用（存于 `~/Library/Application Support/clew/servers/`）。
- **配置每项目独立**：`<root>/.clew/lsp.toml`，可提交与队友共享，覆盖内置默认：

```toml
[rust]
version = "2026-07-13"          # 版本 pin（缺省用 clew 内置的）
enabled = true                  # 可对本项目关掉 LSP
command = "/path/to/server"     # 逃生口：指向自定义/系统二进制，绕过托管

[rust.init_options]             # 透传给 server 的 initialize 选项
"rust-analyzer.check.command" = "clippy"
```

内置支持的语言与获取方式：

| 语言 | Server | 获取方式 |
| --- | --- | --- |
| Rust | rust-analyzer | 下载校验二进制 |
| C / C++ | clangd | 下载校验二进制（含 `lib/` 资源） |
| Zig | zls | 下载校验二进制（`.tar.xz`） |
| Go | gopls | `go install`（需 Go 工具链） |
| Python | pyright | `npm install`（需 Node） |
| TypeScript / JS | typescript-language-server | `npm install`（需 Node） |
| JSON / HTML / CSS | vscode-*-language-server | `npm install`（需 Node） |
| TOML | taplo | `npm install`（需 Node） |

有独立二进制的（Rust、C/C++、Zig）走**下载 + SHA-256 校验**（支持 gzip / zip / tar.xz）；只以工具链分发的（Go/Python/TS/JSON/HTML/CSS/TOML）用你已装的 `go`/`npm` 装到 clew 自己的隔离目录（不污染系统全局）。缺工具链时会提示安装或改用 `command` 逃生口。其他语言（如 Haskell，需匹配 GHC 版本较复杂）可用 `command` 指向已装的 server。没装 server 或不支持的语言会优雅降级回 `⌘T` 符号搜索，读代码不受影响。

以上语言都带 tree-sitter 语法高亮。

**管理面板**（工具栏 Servers 按钮）：查看每个语言的 server 状态、已装 server 的磁盘占用，一键 **下载 / 删除 / 重启**，并查看当前语言 server 的**实时日志**（stderr）和**索引进度**（`$/progress` 显示在状态栏）。

## 支持的语言

语法高亮 + 大纲/符号索引：Rust、Python、JavaScript/JSX、TypeScript/TSX、Go、C、C++、Java。
仅语法高亮：JSON、YAML、TOML、Shell、HTML、CSS、Zig。其他文件以纯文本显示。

## 架构速览

```
src/
├── main.rs      # 应用状态、消息循环（Elm 架构 update）、双窗格路由
├── ui.rs        # 全部视图代码（iced widgets）
├── viewer.rs    # 单窗格状态 + 虚拟滚动窗口计算 + 行选择
├── highlight.rs # 语言注册表 + tree-sitter 高亮 → 按行着色 span
├── outline.rs   # tags query 提取单文件符号
├── index.rs     # 全项目符号索引（后台构建）
├── fs_scan.rs   # 项目扫描（ignore crate，构建文件树 + 扁平列表）
├── search.rs    # 全项目搜索（grep-searcher / grep-regex）
├── finder.rs    # 模糊查找（文件/符号/:行号，nucleo-matcher）
├── history.rs   # 后退/前进历史栈
├── bookmarks.rs # 书签 + .clew/ 持久化
├── codeview.rs  # 自定义代码视图 Widget（虚拟化渲染 + 字符级命中测试）
├── lsp/         # 语言服务器：注册表 / 配置 / 全局 store / JSON-RPC 客户端
└── theme.rs     # One Dark 风格配色与控件样式
```

关键设计：

- **虚拟滚动**：滚动容器总高度恒为 `行数 × 行高`（上下两个 spacer 占位），
  每帧只物化可见窗口 ±12 行的 `rich_text`，文件大小不影响渲染开销。
- **全部阻塞操作下沉**：扫描、读文件、高亮、大纲、符号索引、搜索都在
  `tokio spawn_blocking` 里跑，UI 线程永不等待。
- **高亮换行守卫**：后台高亮结果回来时校验行数与当前文件一致，
  防止快速切换文件时旧结果串台；分屏同文件时通过 `Arc` 共享高亮结果。
- **响应式布局**：窗口窄于 950pt 自动隐藏大纲、收窄侧栏，代码区永远优先。

## 开发

```sh
cargo test     # 60 个单元/集成测试（另有 2 个 --ignored 的实网/实进程测试）
cargo clippy   # 零警告

# 需要真实 rust-analyzer 的端到端测试（会启动子进程）：
cargo test --release -- --ignored
```
