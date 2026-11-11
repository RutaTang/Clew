# 语言支持

[English](languages.md) · **简体中文**

为了更好地做代码分析，Clew 会针对不同语言的特性，提供贴合它的支持。目前 Clew 提供的功能完整支持 Rust、Python、TypeScript/JavaScript、Go、Dart、Java、C#、PHP、Ruby、Erlang、Haskell 和 Zig；C 和 C++、Lean 4 以及 SQL 支持其中的大部分功能；对其余语言目前也提供了一些基础支持，如语法高亮。未来会支持更多语言。

**设置 → 语言服务器** 会针对当前打开的项目，显示每种语言的语言服务器及其状态，并让你为每种语言做设置——每项设置都先显示当前生效的内容，再列出可选项：

- **Python** —— 语言服务器和调试器所使用的解释器，以及由谁指定（你的选择、项目的 `.clew/lsp.toml`、在项目中找到的虚拟环境）。可以从 Clew 找到的环境中另选一个（项目自身的、Poetry、Pipenv、conda、pyenv、uv，或 `PATH` 上的 `python3`），也可以直接填写路径。
- **Rust** —— rust-analyzer 加载项目时启用的 Cargo features（从各 package 的 `Cargo.toml` 中列出全部 features；也可以全部启用，可选是否带默认 features），以及检查所针对的 target。
- **Go** —— gopls 和调试器构建时使用的 build tags（列出项目中 `//go:build` 行用到的 tags），以及按哪个 `GOOS`/`GOARCH` 阅读项目。
- **C 和 C++** —— clangd 使用的 `compile_commands.json`（列出 CMake、Meson 等在项目中生成的那些），以及编译数据库未覆盖的文件所用的编译参数（如 `-DDEBUG -Iinclude -std=c++20`）。
- **Lean 和 Dart** —— 语言服务器所用的工具链：elan 安装的各个工具链（项目 `lean-toolchain` 指定的排在最前）、Flutter SDK 以及 fvm 管理的各个版本（项目指定的排在最前）。
- **TypeScript 和 JavaScript** —— 语言服务器所运行的 TypeScript：默认使用 Clew 自带的版本，除非你主动选择，否则绝不会使用项目自带的版本（列表中会标出它的版本，并注明它会运行项目自身的代码），也可以选择全局安装的版本。
- **Zig** —— zls 所使用的 `zig` 编译器：zvm 和 zigup 管理的各个版本（项目 `build.zig.zon` 要求的版本排在最前），或你指定的编译器。
- **Java** —— Eclipse JDT 语言服务器构建项目所用的 JDK：本机上的各个 JDK（`JAVA_HOME`、`/usr/lib/jvm`、macOS、SDKMAN!、IntelliJ、asdf、mise 和 Homebrew 安装的），构建要求的 Java 版本（`.java-version`、`.sdkmanrc`、Gradle 工具链、`pom.xml`）对应的排在最前。
- **PHP** —— intelephense 按哪个 PHP 版本解读代码（`composer.json` 允许的最低版本排在最前）。
- **C#** —— csharp-ls 打开的解决方案：项目根目录及下一级目录中的 `.sln` 和 `.slnx` 文件。
- **Ruby、Erlang 和 Haskell** —— 选用哪个 Ruby 的 Ruby LSP（rbenv、chruby、asdf、mise、RVM 安装的；项目 `.ruby-version` 指定的排在最前），以及要运行的 ELP 和 Haskell 语言服务器（ghcup、Stack、Cabal 安装的），或 `PATH` 上的。
- **任意语言** —— 使用你自己的语言服务器程序：另一个版本的 Clew 自带服务器、另一种服务器，或为 Clew 尚无服务器的语言指定一个。会构建项目的服务器在构建前仍会先征得你的同意。

以上内容都只通过读取文件得到——不会运行任何程序；远程项目则在其所在主机上查找。这些选择保存在 Clew 自己的数据目录中，只对本机上的这个项目生效，绝不会写入项目，优先于项目的 `lsp.toml`，并同样作用于编辑器的语言服务器、调试器以及 Ask 的代码工具。

某种语言的服务器还没有安装时，Clew 会先问你，再下载它自己的那一份。如果这个服务器程序已经在你的 `PATH` 上——rustup 装的 rust-analyzer、你自己装的 clangd 或 pyright——提问里也会给出它；能回答版本号的会先问它的版本，这样版本管理器为一个并未安装的工具留下的垫片（shim）就不会被提供（TypeScript 的从不提供：Clew 自带的服务器固定了它所运行的 TypeScript，而你自己的服务器会从项目里取 TypeScript）。点 **使用已有的**，它就成为这门语言的服务器，和在这里选中它一样，什么都不会下载。

构建因此不会编译的代码会在阅读时变暗：Rust 中依赖未启用 feature（或其他平台）的 `cfg` 项、`//go:build` 行或 `_windows.go` 这类文件名所排除的 Go 文件，按平台和 `-D`/`-U` 参数会被预处理器丢弃的 C/C++ `#if`/`#ifdef` 分支，Zig 中基于 `builtin.os.tag` 或 `builtin.cpu.arch` 的 comptime `if`/`switch` 分支里编译器不会为该平台分析的部分，C# 中依据文件自身 `#define` 的符号判定不成立的 `#if` 分支，Haskell 中（启用了 CPP 的文件）基于 `<os>_HOST_OS` 和 `<arch>_HOST_ARCH` 的 CPP `#if` 分支，以及 Erlang 中依据文件自身 `-define` 的宏判定不成立的 `-ifdef`/`-if` 分支。与平台有关的判断，以状态栏中选定的平台为准。只有确定不会编译的代码才会变暗——可能由其他 package 启用的 feature、可能由头文件定义的宏，都不会被处理。

- **SQL**：大纲列出 schema 中的表（及其列）、视图、函数、触发器和类型，并以其上方的 `--` 注释作为文档；类型图会按外键把表连接起来（无论外键写在表定义中，还是像 `pg_dump` 那样写在同一文件的 `ALTER TABLE` 里），并把每个视图连向它所查询的表。
- **Java、C#、PHP、Ruby、Erlang、Haskell 和 Zig**：语法高亮；大纲，每个定义附带其文档（Javadoc、C# 的 `///` XML 文档、PHPDoc、Ruby 的 `#` 注释、EDoc、Haddock、Zig 的 `///`），并标出测试（JUnit、NUnit/xUnit/MSTest、PHPUnit、Minitest、EUnit、Zig 的 `test` 块）；导入图；调用图；以及语言服务器（Eclipse JDT LS、csharp-ls、intelephense、Ruby LSP、ELP、Haskell 语言服务器、zls）。调试器用 rdbg（`debug` gem；所选 Ruby 自带的那个）运行 Ruby 程序，用 netcoredbg 运行 .NET 程序集，把 Zig 程序当作原生代码交给 lldb-dap（`.zig` 文件会在本次调试中经 LLVM 编译，即 `-fllvm`，lldb 能读取其调试信息；由 Zig 自带代码生成器——x86-64 上 Debug 构建的默认方式——生成的二进制同样可以调试，但 lldb 无法从中读取变量，调试控制台会说明这一点以及如何构建出它能读取的版本）；并通过 Clew 自带的调试适配器，经 JDWP 调试 Java 程序（只需项目选定的 JDK：`.java` 文件会在本次调试中编译；构建出的程序以 `mainClass` 和 `classPaths` 指定；断点在其类加载时设置；可读取局部变量、字段、数组与字符串；JDWP 本身不求值，Clew 便在其上自行计算 Java 表达式——名字、字段、元素、字面量、算术、比较与逻辑运算、字符串拼接，以及 `order.total()`、`Math.max(a, b)`、`items.get(0) * 2` 这样的调用，按 Java 的规则选择重载并装箱或拆箱，方法只在已停下的线程中运行，途中遇到的断点会被跳过；悬停时不调用任何方法，暂停时也不调用，因为此时虚拟机不运行方法），用 Xdebug 3 调试 PHP 程序、用 OTP 的解释器调试 Erlang 程序（模块的 `main/1`，或启动配置指定的入口；其应用会在本次调试中以 `debug_info` 编译）、用 GHCi 的调试器调试 Haskell 程序（源码目录取自项目的 `.cabal` 或 `package.yaml`；启动配置也可改用 `cabal repl` 或 `stack repl`）。断点、单步、调用栈、变量和求值在这些语言中都可使用，以各语言自身调试器的能力为限：Erlang 和 Java 可以暂停正在运行的程序；PHP 在 Linux 上使用 Xdebug 3.4 及以上版本时（通过 Xdebug 的控制套接字），或在程序运行时仍接受命令的 Xdebug 下，也可以；GHCi 通过逐步执行直到函数返回来实现单步跳出，但无法暂停程序。以上全部在远程主机上同样可用：在该主机端口上监听的调试适配器（rdbg、Go 的 delve）由 Clew 的服务端通过自己的连接转发；Node.js 程序则由服务端自带的适配器经 Node 的 inspector 调试——Clew 在本机使用的 vscode-js-debug 会为每个被调试进程单独建立连接，远程通道无法承载。它支持带条件的断点、单步、真正的暂停（停在程序自己的代码中，而非 Node 内部）、已捕获与未捕获的异常、各作用域的变量，以及由 V8 自身完成的求值（悬停时不允许任何副作用）；TypeScript 按 Node 22.6 及以上版本的方式运行，类型在原位被剥除，因此每一行都与源码对应；它跟随主线程，不跟随 worker 或子进程。远程项目的 `composer.json` 和 Haskell 包描述文件在其所在主机上读取，因此其导入的解析与本地项目一致。导入按各语言的构建方式解析：Java 的包在项目的源码根目录（`src/main/java`）下查找，C# 的 `using` 指向项目文件所声明的命名空间，PHP 的类按 `composer.json` 的 PSR-4 映射及其 `require` 路径解析，Ruby 的 `require_relative` 以及从 `lib/` 解析的 `require`，Erlang 的 `-include`/`-include_lib` 与 `-behaviour`，Haskell 的模块在 `.cabal` 或 `package.yaml` 的源码目录下查找，Zig 的 `@import` 按路径解析；不属于项目的（JDK、NuGet、Composer 包、gem、OTP、Hackage、`std`）按包列出。
- **Lean 4**：大纲（定义、定理、实例、结构体、类、归纳类型）及其 `/-- … -/` 文档与文件的 `/-! … -/` 模块文档、导入图、类型图中 `extends` 其他结构体的继承关系，以及 Lean 语言服务器（`lake serve`，来自 elan 安装的 Lean 工具链）。Lean 的语法可扩展，因此 clew 的语法解析器读不懂的命令会按其首行列入大纲，且不会影响其他命令的大纲。
