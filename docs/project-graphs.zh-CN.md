# 项目图

[English](project-graphs.md) · **简体中文**

工具条上的 **项目图** 打开 Clew 从项目文件里读出的各种图。它像读代码的人一样静态地读：运行时按数据注册的路由、只由 ORM 建出来的表，都看不到。什么都没找到的图不会列出；远程项目的图在它所在的主机上读取。

- **SQL** —— 以表格形式绘制的数据库结构：每张表一张卡片，列出各列的类型和标记（主键、标识列、唯一、可否为空 —— 图例说明含义），并从每个外键的列连线到它引用的列；视图和枚举也各有卡片。指向一张表会点亮它的连线，指向一列会显示它的类型、默认值、注释和引用；单击选中一张表，再次单击打开它。结构由按顺序重放的迁移得到（编号 `0002` 在 `00010` 之前，down 迁移不计），支持 PostgreSQL、MySQL、SQLite 和 SQL Server 的写法 —— 包括 `DO` 块、`ALTER COLUMN`、重命名和 `SET SCHEMA` —— 也可读取 Prisma schema、Django 模型、SQLAlchemy 模型或 Rails 的 `db/`；并列的多个迁移文件夹各自是一个数据库。此外：每个迁移后的结构，每个视图和 `INSERT … SELECT` 追溯到它读取的表的数据血缘，以及引用每张表的代码。
- **路由** —— 每条路由对应的处理函数：Flask、FastAPI、Django、Spring、ASP.NET、Laravel、Symfony、Rails，以及 Go 的 `net/http`、gin、echo 和 chi；JavaScript 这边有 Express、Fastify、Koa、Hono、Elysia、hapi、`Bun.serve` 和 NestJS（用 `app.use('/api', router)` 挂载的路由器带上它的前缀，挂载写在别的文件里也一样），tRPC 的过程（`post.byId`），React Router、Vue Router 和 TanStack Router 的页面（嵌套路径会拼接起来；写成常量的路径，比如 `paths.app.users.path`，会到声明它的地方查出来），以及 Next.js、Nuxt、SvelteKit、Remix 和 Astro 按文件组织的页面和接口（`[id]` 读作 `:id`）。
- **框架** —— 依赖注入（Spring、.NET、Nest 和 Angular）、ORM 关系（JPA、Entity Framework、Eloquent、ActiveRecord、Django、SQLAlchemy、TypeORM）和 WordPress 钩子。
- **Python** —— 类继承关系及每个类的方法解析顺序（MRO），pytest fixture 以及用到它们的地方，还有运行时才决定调用谁的地方（用算出来的名字调用 `getattr`、存放可调用对象的字典、由装饰器注册的函数）以及它们可能调用到的东西。
- **JavaScript 和 TypeScript** —— React、Vue、Svelte 和 Astro 的组件树：每个组件（包括用 `memo`、`forwardRef` 包起来的）连同它声明的 props，并有箭头指向它渲染的每个项目内组件，箭头上标着它传入的 props。组件按所在文件区分 —— Next.js 里每个 `Page` 都是各自独立的，标签上带着能把它们区分开的那几层文件夹（`Page · posts/[id]`）—— 标签里的名字按它的 import 找到对应组件，支持相对路径、tsconfig 的 `paths` 和 `baseUrl`、`@/` 和 `$lib/`，以及工作区里的包；从项目以外的包导入的不算项目的组件，没有任何 import 的名字（比如 Nuxt 自动导入的 `BaseButton`）取最近的同名组件。**状态**：每个 store —— Pinia、Redux Toolkit 的 slice 以及把它们合起来的 store、Zustand、Vuex 及其模块、Svelte store、Jotai atom、nanostores、MobX、NgRx，以及被其他文件用到的 context —— 列出它的状态、getter 和 action，并从每个用到它的组件、hook 或模块画一条箭头过来，标着用到了什么（`cart.items`、`useSelector((s) => s.cart.open)`、`dispatch('cart/add')`）。还有工作区里的包和它们之间的依赖。测试、stories、打包产物、压缩过的和生成的文件都不参与这些图。
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
