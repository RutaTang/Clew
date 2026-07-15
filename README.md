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
| 字号调整 | 放大/缩小/重置，保持当前阅读位置 | `⌘+` / `⌘-` / `⌘0` |

## 使用

```sh
cargo run --release -- <项目目录>    # 打开一个项目
cargo run --release -- <文件路径>    # 打开文件所在目录并直接定位该文件
cargo run --release                  # 打开后从欢迎页选择文件夹
```

所有持久化数据都只存放在项目内 `<root>/.clew/`（书签为 `.clew/bookmarks.json`，首次打书签时才创建，删光后自动清理）。项目目录只读时保存会失败并在状态栏提示，不会写到项目之外的任何位置。建议把 `.clew/` 加进全局 gitignore，或提交它来与队友共享阅读标记。

## 支持的语言

语法高亮 + 大纲/符号索引：Rust、Python、JavaScript/JSX、TypeScript/TSX、Go、C、C++、Java。
仅语法高亮：JSON、YAML、TOML、Shell、HTML、CSS。其他文件以纯文本显示。

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
cargo test     # 35 个单元/集成测试
cargo clippy   # 零警告
```
