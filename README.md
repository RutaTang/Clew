# clew

*a reader for code* — 一个专注「读代码」的桌面工具，Rust + [iced](https://iced.rs) 实现。

clew 不是编辑器：没有光标、没有保存、没有插件。它优化的是读代码时最高频的三件事——**找到、跳转、回退**。

## v1 功能

| 功能 | 说明 | 快捷键 |
| --- | --- | --- |
| 文件树 | 遵守 `.gitignore`，自动隐藏 `target/`、`node_modules/` 等 | 点击展开 |
| 代码视图 | 只读、tree-sitter 语法高亮、虚拟滚动（10 万行文件不卡） | — |
| 模糊文件查找 | 输入几个字母跳到任意文件 | `⌘P` / `Ctrl+P` |
| 全项目搜索 | ripgrep 内核，字面量 smart-case 匹配，点击结果跳到行 | `⌘⇧F` / `Ctrl+Shift+F` |
| 导航历史 | 像浏览器一样后退/前进，跳转永远可撤销 | `⌥←` / `⌥→` |
| 符号大纲 | 当前文件的函数 / 结构体 / 类列表，点击跳转 | 工具栏 Outline 按钮 |

## 使用

```sh
cargo run --release -- <项目目录>    # 打开一个项目
cargo run --release -- <文件路径>    # 打开文件所在目录并直接定位该文件
cargo run --release                  # 打开后从欢迎页选择文件夹
```

## 支持的语言（语法高亮 + 大纲）

Rust、Python、JavaScript/JSX、TypeScript/TSX、Go、C、C++、Java（以上带大纲），
以及 JSON、YAML、TOML、Shell、HTML、CSS（仅高亮）。其他文件以纯文本显示。

## 架构速览

```
src/
├── main.rs      # 应用状态、消息循环（Elm 架构 update）
├── ui.rs        # 全部视图代码（iced widgets）
├── viewer.rs    # 代码视图状态 + 虚拟滚动窗口计算
├── highlight.rs # 语言注册表 + tree-sitter 高亮 → 按行着色 span
├── outline.rs   # tags query 提取符号大纲
├── fs_scan.rs   # 项目扫描（ignore crate，构建文件树 + 扁平列表）
├── search.rs    # 全项目搜索（grep-searcher / grep-regex）
├── finder.rs    # 模糊文件查找（nucleo-matcher）
├── history.rs   # 后退/前进历史栈
└── theme.rs     # One Dark 风格配色与控件样式
```

关键设计：

- **虚拟滚动**：滚动容器总高度恒为 `行数 × 行高`（上下两个 spacer 占位），
  每帧只物化可见窗口 ±12 行的 `rich_text`，文件大小不影响渲染开销。
- **全部阻塞操作下沉**：扫描、读文件、高亮、大纲、搜索都在
  `tokio spawn_blocking` 里跑，UI 线程永不等待。
- **高亮换行守卫**：后台高亮结果回来时校验行数与当前文件一致，
  防止快速切换文件时旧结果串台。

## 开发

```sh
cargo test     # 21 个单元/集成测试
cargo clippy   # 零警告
```
