# MCP 与 ACP

[English](mcp-acp.md) · **简体中文**

**在 Ask Clew 中使用 MCP 服务器。** 在 **设置 → MCP 服务器** 中，可以用命令（`npx -y @modelcontextprotocol/server-github`）、URL（`https://…/mcp`）添加服务器，也可以直接粘贴它 README 里的 `{"mcpServers": {…}}` 片段。Clew 同时支持当前无状态的 MCP 版本（2026-07-28）和更早的、以 `initialize` 开始的版本（2025-11-25 一直到 2024-11-05），传输方式支持 stdio 和 Streamable HTTP。需要你登录的远程服务器（MCP 的 OAuth 流程）会出现一个 **登录** 按钮，在浏览器里完成登录；令牌保存在你的电脑上，过期时自动续期。每次调用工具前都会先问你（并显示参数），除非你对这个工具选择了总是允许，或者把这个服务器设为信任只读工具、或信任所有调用。服务器保存在 `config.toml` 中：

```toml
[mcp.servers.github]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]
env = { GITHUB_PERSONAL_ACCESS_TOKEN = "…" }
approve = "ask"            # ask | read_only | always
allow = ["search_code"]    # 不需要询问的工具
timeout_secs = 120

[mcp.servers.docs]
url = "https://example.com/mcp"
headers = { Authorization = "Bearer …" }
```

**在 Ask 面板中使用 ACP 智能体。** 在 **设置 → ACP 智能体** 中添加一个智能体（内置 Claude Agent、Codex、Gemini CLI、GitHub Copilot、Qwen Code 和 OpenCode 的预设，也可以是任何以 ACP 模式启动智能体的命令），然后在 Ask 面板里选中它。智能体在你的电脑上运行，处理当前项目，并可以使用 Clew 的代码工具（智能体和 MCP 服务器从你的主目录、或条目里的 `cwd` 启动，所以项目自己的 `.npmrc` 改变不了 `npx` 运行的东西）；它的计划、工具调用和回答都会实时出现在面板里，它请求权限时，你在面板里回答。ACP 智能体用于本地项目。

**把 Clew 作为 MCP 服务器。** `clew-server mcp` 通过 stdio 为它所在的项目（或 `--root <dir>` 指定的目录）提供工具：精确搜索和语义搜索、读取文件和文件大纲、语言服务器给出的定义、引用和悬停信息、git 历史和当前改动，以及项目已缓存的注解。例如在 Claude Code 中：

```sh
claude mcp add clew -- /Applications/Clew.app/Contents/MacOS/clew-server mcp
```

（Linux 和 Windows 上，`clew-server` 就是解压目录里 `clew` 旁边的那个。）

**把 Clew 作为 ACP 智能体。** `clew-server acp` 把 Ask Clew 的智能体提供给支持 ACP 智能体的编辑器，用 Clew 设置里配置的模型回答。编辑器交给它的 MCP 服务器会加入它的工具，每次调用都先问你。例如在 Zed 的 `settings.json` 中：

```json
"agent_servers": {
  "Clew": { "command": "/Applications/Clew.app/Contents/MacOS/clew-server", "args": ["acp"] }
}
```
