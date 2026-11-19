# 形式化验证

[English](verification.md) · **简体中文**

Clew 的一部分规则不只是经过测试，而是经过证明的。对每条规则，`verification/` 里都有一个 Lean 4 模型、关于这个模型的定理，以及把模型和 Rust 代码绑定在一起的测试用例。选中的规则是 Clew 的安全所依赖的规则，也是评审轮次最多才改对的规则：
- 项目可以指向哪些路径；
- 可以安装哪些更新；
- 哪个服务端程序可以部署到远程主机上；
- 一条已记录的许可涵盖什么；
- 消息在数据流上如何分帧；
- Clew 交给 shell 或 ssh 的内容；
- Clew 直接运行哪些程序，以及发送哪些请求头；
- 说出的话交给哪里处理；
- 调试时鼠标悬停能否运行程序的代码；
- Clew 登录 MCP 服务器时所做的判断。

其中十五项的定理直接落到 Rust 代码本身：项目可以指向哪些路径（按写法和解析之后）、哪些更新可以安装以及可以从哪里下载、Clew 直接运行哪些程序、向 MCP 服务器发送哪些请求头、说出的话交给哪里、分帧消息的每一行意味着什么、交给 shell 的命令如何加引号、已保存的连接交给 ssh 的参数、记录下的 ssh 主机密钥为哪台主机担保、哪些平台名可以选定服务端程序、服务端程序必须具有哪个摘要、一条许可的指纹所哈希的字节、远程项目的许可登记在哪个键下、悬停能否运行程序的代码，以及登录 MCP 服务器时的判断。Aeneas 把这些代码翻译成 Lean，并证明这份翻译等于模型。

## 证明了什么

**路径限制**：`crates/clew-core/src/confine.rs` 中的 `check_lexical`，模型在 `verification/Clew/Confine.lean`。
- 来自外部的路径（客户端的请求、模型写出的工具参数）只要通过检查，就绝不会走到项目根目录之外。无论对着哪个根目录解析，都成立；而且解析的每一步都成立，不只是最后一步。
- 检查拒绝的恰好是空路径、绝对路径和含 `..` 的路径，此外什么都不拒绝。
- 这对代码本身已证明（Aeneas）：`check_lexical` 首先询问的 `clew_rules::confine::check_lexical`，适用于任意字符串的 UTF-8 字节。它开头所做的 `str::trim`（`clew_rules::text::trim_bounds`，涵盖每个 Unicode `White_Space` 字符），以及判断路径按写法是否指向文件夹的 `names_a_folder`，也同样已证明。之后标准库 `Path` 的解读在 Unix 上不会再多拒绝什么；它是为 Windows 保留的，在那里 `\`、盘符或 UNC 前缀也会走出项目。
- `confine` 解析出的路径，只有当它就是解析后的根目录本身，或是根目录后接 `/` 再加更多内容时才被接受：绝不会是仅仅以根目录的写法开头的兄弟目录，比如 `/project` 旁边的 `/project-old`。这就是 `clew_rules::confine::inside`，对任意两条路径的字节，对代码本身已证明（Aeneas）；`confine` 只接受它和 `Path::starts_with` 都接受的路径。解析 `..`、`.` 和链接是操作系统的事（`canonicalize`）。Windows 的路径不是以 `/` 分隔的字节，在那里由 `Path::starts_with` 单独决定。

**更新只升不降**：`crates/clew-core/src/update.rs` 中的 `Version::parse` 和 `ensure_upgrade`，模型在 `Update.lean`。
- 只有比当前运行的版本严格更新的更新才会被安装。
- 版本之间的顺序是严格全序。所以连续的更新只会往上走，任何版本都不会被安装两次，当前运行的版本也不会。
- Clew 显示的版本号和它发布的 `vX.Y.Z` 标签，读回来都是那个版本。
- 这对代码本身已证明（Aeneas）：`Version::parse` 和 `ensure_upgrade` 所调用的 `clew_rules::version::{parse, is_newer, upgrade}`，适用于任意字符串的 UTF-8 字节。`upgrade` 就是整个判断，所以它放行的版本比正在运行的版本新，这对实际运行的代码成立。
- 更新只从发布所在的主机下载，主机名必须完全一致（而不是以它开头的更长名字），并且必须位于发布的下载目录之下。这个判断是 `clew_rules::download::release_asset`，对代码本身已证明（Aeneas）；它针对的是 `url` crate 读出的主机和路径，与下载时实际发送的一样，所有 `.` 和 `..` 段都已解析。

**哪个服务端程序可以部署**：`crates/clew-core/src/server_dist.rs` 中的 `slug`、`asset_name`、`parse_digest_list`、`embedded_digest`、`parse_sha256` 和 `verify`，模型在 `ServerDist.lean`。
- 远程主机报告的平台名会变成一个普通的标记：不含 `/`，不含 `.`，也不含任何在 URL 里有特殊含义的字符。无论远程主机报告什么，也无论 Unicode 小写转换把它变成什么，都成立。
- 两个不同的“平台 + 协议”组合永远不会得到同一个资源名，因此也不会共用同一个固定下来的摘要。
- 在正式发布的版本中，只有哈希值等于为它自己的资源名固定下来的摘要的程序才会被部署。下载旁边的 `.sha256` 文件不起作用。发布版本没有列出的资源，永远不会被部署。
- 发布流程用各个 `sha256sum` 行拼成的摘要清单，解析出来恰好就是清单里的各项。
- 摘要列表和旁挂文件由 `clew_rules::server_dist::{pinned_digest, sidecar_digest}` 读取，已对代码本身证明（Aeneas）：对任意文本的 UTF-8 字节，读到的就是模型读到的。

**许可只涵盖被许可的内容**：`args_part`、每个指纹所哈希的带长度前缀的字段，以及许可登记时用的键；代码在 `crates/clew-core/src/trust.rs`、`crates/clew-core/src/derived.rs` 中的 `project_key` 和 `src/backend/connect.rs` 中的 `scope_label`，模型在 `Trust.lean`。
- 不同的参数列表，读出来永远不同。
- 不同的字段列表，读出来也永远不同，后面跟着命令内容的摘要时同样如此。
- 对命令的许可、只对选项的许可、允许构建的许可，哈希的输入互不相交，所以一种许可永远不会被当成另一种。
- 远程许可的键恰好对应一个用户、一台主机、一个端口和一个项目，无论项目路径是什么；而且永远不会等于本地项目的键。

证明最后这一条时发现了两个漏洞：
- 主机名可以含 `:`（IPv6 地址）。所以端口 22 上的 `u@2001:db8::1:2222` 和端口 2222 上的 `u@2001:db8::1` 曾经得到同一个标签，对一台机器的许可也就涵盖了另一台。现在含 `:` 的主机名会加上方括号（`[2001:db8::1]:2222`），并且 `%`、`/`、`[`、`]` 和 `@` 都会转义。
- 远程主机上的项目路径可以是相对路径（设置了 `CLEW_SSH` 时运行 `clew proj`）。所以 `u@h` 上的 `2222:/srv/p` 和 `u@h:2222` 上的 `/srv/p` 曾经得到同一个键。现在相对路径前面会加上 `./`。

普通的 `user@host` 和绝对路径的键不变，所以已经给出的许可继续有效。

键里的用户、主机和端口，就是 ssh 实际连接时用的那些。使用 `CLEW_SSH` 时，它们按 OpenSSH 读取参数的方式读出：来自 `-l`、`-o User=`、`-p`、`-o Port=`、`user@host` 或 `ssh://user@host:port` 形式的目标，每一项都取最先给出的那个。端口保留写下时的样子，所以 `-p http` 永远不会被当成端口 22。不是 UTF-8 的路径在键里保留原来的字节，所以两个这样的路径永远不会被当成同一个项目。

**消息分帧**：`crates/clew-core/src/framing.rs` 中的 `read_frame`、`read_json_line`、`read_line_capped` 和 `encode`，在 `Framing.lean` 中逐字节建模。每个读取函数从数据流读出一行，交给 `crates/clew-rules/src/framing.rs` 中的 `frame_line`、`json_line` 或 `capped_line`，由它们决定这一行的一切（头部行写了什么由 `header` 判断）；剩下的只是一个 `match`，每种判断一个分支。
- Clew 写出的一帧，无论后面跟着什么，读回来恰好是它的正文，数据流停在下一帧的开头。所以一帧接一帧写出的任意多帧，读回来恰好就是那些消息，数据流永远不会错位。
- 数据流干净地结束，就是没有下一帧，而不是出错。
- 正文永远不会超过上限，无论帧头声明了多长。
- 每读一帧都会消耗输入，所以读取总能向前推进。
- Clew 自己协议的上限针对的是一行的内容，不算换行符。恰好达到上限的一行文本会被读取；更长的一行会被拒绝，绝不会只读一部分；不是 UTF-8 的一行也会被拒绝。

**Clew 交给 shell 或 ssh 的内容**：`src/backend/server.rs` 中的 `shell_command` 和 `sh_c`、`src/macos/install.rs` 中的 `quote`，以及 `src/backend/connect.rs` 中的 `ssh_args`、`ssh_endpoint`、`known_hosts_pattern` 和 `plain_host_field`，模型在 `Ssh.lean`。
- Clew 为 shell 写出的命令（显示给你粘贴的命令，以及 macOS 更新程序替换脚本里的每个路径），POSIX shell 读出来恰好就是本来的那些词。无论词里有什么都成立：空格、引号、`$`、反引号、换行。
- Clew 在远程主机上运行的命令 `sh -c '<脚本>'`，读出来是 `sh`、`-c` 和脚本本身。
- 已保存连接的任何字段，无论内容是什么，都绝不会成为 ssh 的选项。ssh 读到的选项恰好是 Clew 写下的那些，目标恰好是 `user@host`。所以填成 `-oProxyCommand=…` 的主机名只是主机名，不是命令。这份列表由 `clew_rules::ssh::ssh_args` 写出，已对代码本身证明（Aeneas）：对任意字段的 UTF-8 字节，它写出的就是模型的列表。
- 一台机器只有一个标签，无论是通过已保存的连接，还是通过参数相同的 `CLEW_SSH` 连接（只要用户名本身不是 `ssh://` 地址）。所以从任一途径给出的许可都登记在同一个名字下。
- Clew 记录主机密钥时用的 known_hosts 字段，恰好对应一台主机和一个端口。它不含任何会让一个密钥替其他主机担保的模式语法（`*`、`?`、`!`、`,`、哈希形式的 `|1|` 名字）。

**两道关卡**：`crates/clew-rules/src/location.rs` 中的 `trusted_by`（由 `crates/clew-core/src/trust.rs` 中的 `trusted_location` 调用）和 `crates/clew-core/src/mcp/config.rs` 中的 `check_headers`（它用 `crates/clew-rules/src/headers.rs` 检查每个请求头的字节），模型在 `Guards.lean`。
- 项目指定的语言服务器，从 Clew 自己保存的、已获许可的字节副本运行，除非它所在的位置只有你或 root 能修改。`trusted_by` 允许它原地运行时，其他用户既不能写它，也不能替换通往它的路径上的任何目录项。这基于 Unix 权限规则，并把每个用户都当作可能属于该文件的组。
- 这样的命令如何运行由 `clew_rules::location::launch` 决定，`stage_lsp_command` 和 `CommandProbe::into_exec` 都询问它，对代码本身已证明（Aeneas）：指纹未获许可时恰好被拒绝；只有在已获许可、位于项目之外、且 `trusted_by` 判定为私有时才原地运行。其他已获许可的命令，运行的是 Clew 保存的已获许可字节的副本。
- `check_headers` 接受的 MCP 服务器请求头，从请求中读回时恰好就是这些请求头，每个一行，名字不变。无论服务器多么宽松地分行（在 CR、LF 或 NUL 处），都成立。任何值都不能另起一个请求头。

**悬停时求值**：`src/dap/proto.rs` 中的 `hover_eval_allowed`（`clew_rules::dap`），模型在 `Guards.lean`。
- 在暂停的程序里悬停一个名字会对它求值，这可能运行程序自己的代码；只有在调试适配器承诺悬停求值没有副作用，或者你选择允许时，才会这样做。对代码本身已证明（Aeneas）。

**登录 MCP 服务器**：`crates/clew-core/src/mcp/oauth.rs` 中的判断（`clew_rules::oauth`），模型在 `OAuth.lean`，每一条都对代码本身已证明（Aeneas）。
- 凭据只发给 https 端点，或通过明文 http 发给本机。
- 已保存的令牌只在剩余时间多于余量时才被使用。当前时间加余量会像 `u64::saturating_add` 那样饱和，绝不会回绕成一个小数字，让过期的令牌看起来还新鲜。
- 刷新令牌只在必须时才出示：已保存的令牌正是被发现不可用的那个，或者它已不再新鲜。会轮换刷新令牌的服务器，在同一个刷新令牌被出示两次时，可能吊销整个登录。
- 只有对这次登录的回答才能结束它：重定向只有带着与发出时完全相同的 `state` 才被接受。其他网页或程序连到回调端口，既不能完成这次登录，也不能让它失败。

**说出的话交给哪里**：`crates/clew-rules/src/voice.rs` 中的 `route`（由应用调用），模型在 `VoiceRoute.lean`。
- “Bye, Clew”永远最先处理。
- 让 Clew 停下来的词绝不会被发给 Ask。如果正在等待的问题提供了同名的回答，这个词就是那个回答。
- 停止词也绝不会被当成“是”：“okay, stop”是以一个“是”开头的。
- 等待确认的命令，只有在说“是”时才会执行。
- 对正在等待的问题说的话，绝不会被发给 Ask。
- 回答正在生成时，停止词会先让它停下，然后才会尝试任何命令。例外有两种：有命令在等待确认，这时停止词会拒绝这条命令；正在等待的问题提供了同名的回答。
- 每一条路由都恰好以一个会执行的步骤结束；而且绝不会采取一个对象不存在的步骤，比如在没有命令等待时回答等待的命令，或在没有问题时暂存回答。

## 证明如何落实到代码

定理是关于模型的。把每个模型和代码联系起来的是：

- **模型读起来就像代码。** 它按同样的顺序走同样的步骤。它依赖的每个 Rust 标准库函数，细到 `char::is_whitespace` 的含义，都在 `Text.lean` 里写明了。这些函数通过用到它们的函数来测试：每个 `White_Space` 码位及其相邻码位都在测试输入里。
- **测试用例。** `lake exe vectors` 在一组固定的输入上运行每个模型，把结果写进 `verification/vectors/*.json`。这些输入包括：在一个能走到每个分支的字母表上的所有短输入，以及每个文件列出的边界情况。`cargo test` 用同样的输入运行真正的 Rust 函数，要求得到同样的结果。只要代码和模型有一处不一致，测试就会失败。
- **shell 的模型对照真正的 shell 检查。** 每个 shell 测试用例也会交给 `/bin/sh` 运行，它读出来的必须是同样的词。
- **有十五条规则，被翻译和证明的就是代码本身。** 路径检查（`check_lexical`、`names_a_folder`、它开头的 `str::trim`，即 `trim_bounds`，以及对解析后路径的检查 `inside`）、更新检查（`parse`、`is_newer`、`upgrade`，以及下载可以从哪里来 `release_asset`）、ssh 参数列表（`ssh_args`）、服务端摘要列表和旁挂文件（`pinned_digest`、`sidecar_digest`）、`route`、`trusted_by` 和 `launch`、分帧读取函数对每一行的判断（`frame_line`、`json_line`、`capped_line`，以及 `header`）、MCP 请求头检查、shell 引号（`shell_command`、`sh_c`、`quote`）、known_hosts 检查（`plain_host_field`）、平台 slug 检查（`slug_ok`）、指纹编码（`length_prefixed`、`args_part`）、远程许可键（`scope_label`、`approval_key`）、悬停规则（`hover_eval_allowed`）和 MCP 登录的判断（`endpoint_allowed`、`fresh`、`take_stored`、`answers`）放在 `crates/clew-rules` 中。Charon 和 Aeneas 把这份 Rust 源码翻译成 Lean（`verification/extracted/ClewRules.lean`），`Equal.lean` 证明这份翻译对每一个输入都等于模型：它总会返回，不会 panic、不会溢出、不会越界索引，每个循环都一定结束；返回的也恰好是模型算出的结果。所以关于这些模型的每条定理，都对实际运行的代码成立。对分帧而言，读取函数读到的每一行上的判断，按读取函数的 `match` 那样执行，被证明恰好是模型循环的一轮；从数据流读出这一行是 tokio 的事。见[实际运行的代码](#实际运行的代码)。
- 其他规则的测试用例是很大的输入样本，而不是全部输入。

`verification/check.sh` 检查三件事：
- 所有证明都能通过；
- 每个证明只依赖 Lean 的三条标准公理，没有别的（`Audit.lean`）：没有 `sorry`，也没有 `Lean.ofReduceBool`，后者意味着信任没有经过内核检查的编译代码；
- 已提交的测试用例与模型现在算出的结果完全一致。

CI 在 `verification` 任务中运行它。Lean 工具链固定在 `verification/lean-toolchain` 中，CI 会核对发布包的 SHA-256。

`verification/extracted/check.sh` 检查翻译出来的代码：
- `ClewRules.lean` 恰好就是 Charon 和 Aeneas 现在从 `crates/clew-rules` 得到的结果，所以改了 Rust 却没有重新证明，检查就会失败；
- `Equal.lean` 中的所有证明都能通过，警告视为错误；
- 这些证明只依赖 Lean 的三条标准公理，没有别的。

CI 在 `extracted` 任务中运行它，所用的 Charon 和 Aeneas 来自一个固定的发布版本，CI 会核对它的 SHA-256。

## 证明依赖什么

- Lean 的内核和它的三条标准公理。
- SHA-256 抗碰撞。关于许可和服务端摘要的定理说的是被哈希的字节；不同的字节得到不同的哈希值，这是 SHA-256 本身的保证。
- 操作系统如何处理路径。符号链接和规范化没有建模。`confine` 在运行时会对照规范化的根目录，再检查一次解析后的路径，而这一检查已证明（`inside`）：被信任的是 `canonicalize` 解析了每个 `..`、`.` 和链接。
- 测试用例。除了对代码本身证明的十五条规则，它们是输入的样本，而不是全部。
- 对这十五者：Charon 和 Aeneas 如实翻译了 Rust，rustc 也按 Rust 的语义编译了它。这些工具和编译器一样，是被信任的。对字符串上的规则（路径、版本、摘要列表、请求头、shell 引号、ssh 参数、known_hosts、slug、许可键），还依赖 Rust 的 `str` 存放的是其字符的 UTF-8 编码，这是 Rust 保证的。对写出文本的规则（shell 引号、ssh 参数、指纹编码、许可标签和键），还依赖写出的结果放得进内存：加引号的词里每个字节至多占四个字节，每个词再多占三个；转义的标签部分每个字节至多占三个；编码、键或 ssh 目标则是它自己的长度。Clew 持有的任何字符串都远远到不了这个量。对分帧，还依赖用 `take(n)` 限长、再用 `read_until(b'\n')` 读取，返回的正是模型的 `readLine n` 返回的内容，以及 `String::from_utf8` 接受的恰好是模型的 `utf8Valid`，这由测试用例检查。对解析后的路径、更新下载和登录，还依赖每个判断所针对的输入：操作系统解析出的规范路径，以及 `url` crate 从 URL 读出的协议、主机和路径。
- POSIX shell。不是 POSIX 的登录 shell（fish、csh）不在模型范围内；Clew 在远程运行的脚本避开了这些 shell 读法不同的写法，并有测试检查这一点。
- ssh 自己的配置做了什么。键里的用户、主机和端口，是 ssh 命令行上的那些。`~/.ssh/config`、`-F` 或 `-o HostName=` 把它们映射成什么，不会被读取：`Host` 别名按它的名字识别。

## 每一个决定，以及如何检查

下面是 Clew 决定谁可以做什么、信任什么的地方。能写成规则并建模的，都经过证明；否则表里写明它依赖什么。

| 决定 | 代码 | 如何检查 |
|---|---|---|
| 请求或工具可以指向哪些路径 | `confine::check_lexical` | 对代码本身已证明（Aeneas），即 `clew_rules::confine::check_lexical`，适用于任意字符串的 UTF-8 字节。Windows 自己的路径形式（`\`、盘符、UNC）由其后标准库 `Path` 的解读拒绝，有测试 |
| 解析后的路径仍在项目之内 | `confine::confine` | 在 Unix 上对代码本身已证明（Aeneas）：`clew_rules::confine::inside` 只接受规范的根目录或其下的路径，并且 `Path::starts_with` 也必须接受。解析链接（`canonicalize`）是操作系统的事 |
| 更新是否比正在运行的版本新 | `update::ensure_upgrade` | 对代码本身已证明（Aeneas）：即整个判断 `clew_rules::version::upgrade`，适用于任意字符串的 UTF-8 字节 |
| 更新可以从哪里下载 | `updater::validate_dmg_url` | 对代码本身已证明（Aeneas）：`clew_rules::download::release_asset`，针对 `url` crate 从 URL 读出的主机和路径 |
| 下载的更新是否真实（macOS） | `macos::install::verify` | 依赖 Clew 调用的 Apple 代码签名 |
| 哪个服务端程序可以部署 | `server_dist` | 已证明。每个平台名在选定程序之前都要通过的检查 `clew_rules::server_dist::slug_ok`，以及固定摘要列表和旁挂文件的读取 `pinned_digest`、`sidecar_digest`，对代码本身已证明（Aeneas），适用于任意字符串的 UTF-8 字节。把程序的 SHA-256 与读到的摘要相比较只是一次字符串比较，有测试 |
| 一条许可涵盖什么 | `trust` 指纹 | 已证明。每个指纹所哈希的字节 `clew_rules::fingerprint::{length_prefixed, args_part}` 对代码本身已证明（Aeneas）。SHA-256 不碰撞是 SHA-256 自己的承诺 |
| 许可登记在哪台机器、哪个项目下 | `derived::project_key`、`scope_label`、`ssh_endpoint` | 已证明。标签和远程键 `clew_rules::approval::{scope_label, approval_key}` 对代码本身已证明（Aeneas），适用于任意字符串的 UTF-8 字节 |
| 项目的语言服务器能否运行 | `lsp_gate::lsp_command_allowed`、`trust::stage_lsp_command` | 已证明。它如何运行，即 `clew_rules::location::launch`，对代码本身已证明（Aeneas）：指纹未获许可就拒绝；只有在已获许可、位于项目之外、且 `trusted_by` 判定为私有时才原地运行；否则运行 Clew 的副本。指纹也已证明。副本保存的正是被哈希的那些字节，这属于文件系统，有测试 |
| 某个文件夹能否存放 Clew 不经暂存就运行的程序 | `clew_rules::location::trusted_by` | 对代码本身已证明（Aeneas）。读取它所依据的所有者和权限属于文件系统 |
| 消息如何分帧 | `framing` | 已证明。每个读取函数读到的每一行上的判断 `clew_rules::framing::{frame_line, json_line, capped_line}` 及 `header`，对代码本身已证明（Aeneas）恰好是模型循环的一轮。从数据流读出一行是 tokio 的 `read_until`，已测试 |
| Clew 交给 shell 的内容 | `clew_rules::shell`（`shell_command`、`sh_c`、`quote`） | 对代码本身已证明（Aeneas），适用于任意字符串的 UTF-8 字节，并对照 `/bin/sh` 检查 |
| Clew 交给 ssh 的内容 | `SavedConnection::ssh_args` | 对代码本身已证明（Aeneas）：即 `clew_rules::ssh::ssh_args`，适用于任意字段的 UTF-8 字节 |
| 记录下的主机密钥为哪台主机担保 | `known_hosts_pattern`、`clew_rules::known_hosts::plain_host_field` | 已证明。每个字段在记录密钥之前都要通过的检查 `plain_host_field` 对代码本身已证明（Aeneas），适用于任意字符串的 UTF-8 字节 |
| 说出的话交给哪里 | `clew_rules::voice::route` | 对代码本身已证明（Aeneas） |
| 鼠标悬停时能否运行被调试程序的代码 | `dap::hover_eval_allowed` | 对代码本身已证明（Aeneas）：`clew_rules::dap::hover_eval_allowed` |
| MCP 服务器的请求头 | `mcp::config::check_headers` | 对代码本身已证明（Aeneas），适用于任意字符串的 UTF-8 字节 |
| 登录 MCP 服务器 | `mcp` OAuth | 已证明。它的判断 `clew_rules::oauth::{endpoint_allowed, fresh, take_stored, answers}` 对代码本身已证明（Aeneas）。交换本身是网络的事 |

## 实际运行的代码

大多数模型是照着代码逐步写成的，再由测试用例把两者联系起来。路径检查、更新检查、ssh 参数列表、摘要读取、`route`、`trusted_by` 和 `launch`、分帧读取函数对每一行的判断、MCP 请求头检查、shell 引号、known_hosts 检查、slug 检查、指纹编码、许可键、悬停规则和 MCP 登录的判断则没有副本：定理所针对的 Lean，就是实际运行的 Rust。

- **`crates/clew-rules`** 存放这十五个决定。它没有任何依赖，只有纯代码，而且只用 Aeneas 能翻译的那部分 Rust：`match`、`while`、递归、切片和可选的切片、`Vec`、带溢出检查的整数运算和位掩码。应用和 `clew-core` 都调用它。
- **`verification/extracted/extract.sh`** 在这个 crate 上运行 Charon 和 Aeneas，写出 `ClewRules.lean`。这个文件不手工修改。
- **`Equal.lean`** 证明翻译出的函数等于 `verification/Clew` 中的模型，具体如[证明如何落实到代码](#证明如何落实到代码)所述。
- **独立的项目。** Aeneas 的 Lean 库需要 Mathlib，所以这是一个单独的 Lake 项目；`verification/` 中的证明依然只需要 Lean 本身。第一次构建会编译 Aeneas 和它用到的约 1500 个 Mathlib 模块；CI 从 Mathlib 的构建缓存获取。

改动 `crates/clew-rules` 会被两道检查拦下：翻译结果不再与 `ClewRules.lean` 一致；重新生成后，如果新代码算出的结果与模型不同，证明就会失败。把 `trusted_by` 的掩码从 `0o022` 放宽到 `0o002`，两道检查都会失败；让恰好等于上限的消息通过，让 shell 词里的空格不加引号，让 known_hosts 主机名里出现 `*`，让含 `..` 的路径通过路径检查，让 `trim_bounds` 不再把 U+3000 当作空白，让 `is_newer` 放行与正在运行的版本相同的版本，去掉 ssh 目标前的 `--`，让摘要列表不再在 `,` 处切分，放过第 33 行头部，在 JSON 行末尾留下 `\r`，拒绝恰好等于上限的一行，把兄弟目录 `/pq` 当作 `/p` 之下的路径，在令牌的最后一秒还使用它，按前缀匹配登录的 `state`，接受仅仅以发布主机名开头的主机，或在语言服务器只是位于项目之外或只是私有（而不是两者兼备）时就原地运行它，也一样。

其他规则仍然是由测试用例检查的模型，因为它们的代码用到了 Aeneas 不能翻译的东西。Aeneas 把 `str` 读作字节，不对逐字符遍历、字符串格式化、传给迭代器的闭包、`char` 的分类、SHA-256 或异步 I/O 建模。分帧读取函数剩下的部分是 I/O：从数据流读出一行。一条规则一旦能不用这些写出来，就可以移进 `clew-rules`，上面十五条就是这样移进去的。

## 哪些没有验证，为什么

- **绘制**：布局、渲染、动画。它们应当产生的是像素，而不是证明可以针对的命题。
- **语言判断**：在哪种语言里哪些词算“停”、Whisper 写出了什么、一段声音是不是唤醒词。这些来自模型和统计，不是有精确表述的规则。语音模式里经过验证的，是词句分好类之后交给哪里处理。
- **并发和外部世界**：任务调度、超时、子进程、网络、文件系统。
- **语言模型给出的回答。**

## 运行

```sh
cd verification
lake build                  # 证明
./check.sh                  # 证明、它们依赖的公理，以及测试用例是否最新
lake exe vectors vectors    # 修改模型后重新生成测试用例
```

[elan](https://github.com/leanprover/elan) 会安装 `lean-toolchain` 指定的 Lean 版本。这个项目除了 Lean 本身不需要别的依赖。

```sh
cd verification/extracted
lake exe cache get          # 从构建缓存获取 Mathlib（或者让第一次构建编译它）
./check.sh                  # 翻译是否最新、证明、它们依赖的公理
./extract.sh --write        # 修改 crates/clew-rules 后重新生成 ClewRules.lean
```

`extract.sh` 需要 `PATH` 上有 `lakefile.lean` 指定的 Aeneas 发布版本中的 `charon` 和 `aeneas`，以及该版本 `rust-toolchain` 文件指定的、带 `rustc-dev` 的 Rust nightly。

## 添加一条规则

1. 在 `verification/Clew/<领域>.lean` 中逐步为函数建模，并证明它应当保证的性质。
2. 在 `Vectors.lean` 中加入它的测试用例，运行 `lake exe vectors vectors`，并提交生成的 JSON 文件。
3. 在 Rust 代码里加一个测试，用 `clew_core::testutil::lean_cases` 让真正的函数在这些用例上运行。
4. 把新的定理列进 `Audit.lean`，然后运行 `./check.sh`。

如果这个函数能用 Aeneas 可以翻译的那部分 Rust 写出来，就把它放进 `crates/clew-rules`，而不是写测试：把它加进 `extract.sh` 的 `--start-from`，运行 `./extract.sh --write`，再在 `Equal.lean` 中证明翻译等于模型。
