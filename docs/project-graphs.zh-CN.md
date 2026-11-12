# 项目图

[English](project-graphs.md) · **简体中文**

工具条上的 **项目图** 打开 Clew 从项目文件里读出的各种图。它像读代码的人一样静态地读：运行时按数据注册的路由、只由 ORM 建出来的表，都看不到。什么都没找到的图不会列出；远程项目的图在它所在的主机上读取。

- **SQL** —— 数据库结构（表、列、外键、视图），按顺序执行每个迁移后的结构（也包括 Rails 的迁移，down 迁移不计），每个视图和 `INSERT … SELECT` 追溯到它读取的表的数据血缘，以及引用每张表的代码。
- **路由** —— 每条路由对应的处理函数：Flask、FastAPI、Django、Express 及同类框架、NestJS、Next.js、Spring、ASP.NET、Laravel、Symfony、Rails，以及 Go 的 `net/http`、gin、echo 和 chi。
- **框架** —— 依赖注入（Spring、.NET、Nest 和 Angular）、ORM 关系（JPA、Entity Framework、Eloquent、ActiveRecord、Django、SQLAlchemy、TypeORM）和 WordPress 钩子。
- **Python** —— 类继承关系及每个类的方法解析顺序（MRO），pytest fixture 以及用到它们的地方，还有运行时才决定调用谁的地方（用算出来的名字调用 `getattr`、存放可调用对象的字典、由装饰器注册的函数）以及它们可能调用到的东西。
- **JavaScript 和 TypeScript** —— React 和 Vue 的组件树，以及工作区里的包和它们之间的依赖。
- **Go** —— 每个类型实现了哪些接口，每个函数启动了哪些 goroutine、它们共用哪些 channel，以及包依赖图，包括其中的环和分层。
- **Rust** —— 每个 trait 和它的实现（包括 derive），`unsafe` 代码，以及 crate 的公开 API。
- **C 和 C++** —— include 关系图和每个头文件的代价，C++ 的重写（override），以及模板和它们的实例化。
- **Erlang** —— 监督树（子进程规格、重启策略、启动它的应用），每个模块发送和接收的消息，以及每个 `gen_server` 调用对应处理它的子句。
- **Haskell** —— 类型类实例（包括 deriving），以及每个函数所在的 monad 栈。
- **Zig** —— `comptime` 泛型和它们的实例化，错误集合和每个函数返回的错误，try 与 catch，以及 `build.zig` 的构建图。
- **Ruby** —— 没有用 `def` 写出来的方法：mixin 和 concern、`define_method`、`attr_*`、`delegate`、`alias`、`class_eval`，以及通过 `send` 和 `method_missing` 的调用。
- **Dart 和 Flutter** —— 按代码写法得到的组件树（`State` 的 build 归它的 widget），以及 Riverpod、provider 和 Bloc 中的状态管理。
- **Lean 4** —— 定理之间的依赖，记号（notation）和它的含义，以及每条 `simp` 引理改写的是什么。

在代码里，右键菜单（以及光标处按 `ge`）会加上这门语言在这里能说的东西：Rust 的 **展开宏**（rust-analyzer 给出的展开）；C 和 C++ 的 **展开宏**（Clew 自己按此处生效的 `#define` 读出来的结果——文件自己的和它头文件里的，头文件按它的编译命令和项目的 `include/` 找到，`#if` 去掉的部分不算——并给出它来自哪条 `#define`）；Lean 的 **显示目标**（此处证明的目标，或某个项期望的类型）；以及 Haskell 的 **显示类型**（Haskell 语言服务器给出的这个名字声明的类型，以及它在此处使用时的类型，如果不同的话——例如传入一个 `Int` 的 `print` 是 `Int -> IO ()`）。
