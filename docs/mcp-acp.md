# MCP and ACP

**English** · [简体中文](mcp-acp.zh-CN.md)

**MCP servers in Ask Clew.** In **Settings → MCP servers**, add a server by its command (`npx -y @modelcontextprotocol/server-github`), its URL (`https://…/mcp`), or by pasting the `{"mcpServers": {…}}` snippet from its README. Clew speaks both the current stateless MCP revision (2026-07-28) and the earlier `initialize`-based ones (2025-11-25 back to 2024-11-05), over stdio and Streamable HTTP. A remote server that wants you to sign in (MCP's OAuth flow) gets a **Sign in** button that finishes in your browser; the tokens are kept on your machine and renewed as they expire. Each tool call asks you first (with the arguments shown), unless you allow that tool always, or set the server to trust read-only tools or every call. The servers are stored in `config.toml`:

```toml
[mcp.servers.github]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]
env = { GITHUB_PERSONAL_ACCESS_TOKEN = "…" }
approve = "ask"            # ask | read_only | always
allow = ["search_code"]    # tools that never ask
timeout_secs = 120

[mcp.servers.docs]
url = "https://example.com/mcp"
headers = { Authorization = "Bearer …" }
```

**ACP agents in the Ask panel.** In **Settings → ACP agents**, add an agent (presets for Claude Agent, Codex, Gemini CLI, GitHub Copilot, Qwen Code and OpenCode, or any command that starts an agent in ACP mode), then pick it in the Ask panel. The agent runs on your machine and works on the project, with Clew's code tools available to it (agents and MCP servers start from your home folder, or an entry's `cwd`, so a project's own `.npmrc` cannot change what `npx` runs); its plan, tool calls and answer stream into the panel, and when it asks for permission, you answer there. ACP agents work on local projects.

**Clew as an MCP server.** `clew-server mcp` serves the project it runs in (or `--root <dir>`) over stdio: exact and semantic search, reading and outlining files, language-server definitions, references and hover, git history and the current changes, and the project's cached explanations. For example, with Claude Code:

```sh
claude mcp add clew -- /Applications/Clew.app/Contents/MacOS/clew-server mcp
```

(On Linux and Windows, `clew-server` is the one beside `clew` in the folder you unpacked.)

**Clew as an ACP agent.** `clew-server acp` serves Ask Clew's agent to editors that host ACP agents, answering with the model set in Clew's settings. The MCP servers the editor hands over join its tools, each call asking you first. For example, in Zed's `settings.json`:

```json
"agent_servers": {
  "Clew": { "command": "/Applications/Clew.app/Contents/MacOS/clew-server", "args": ["acp"] }
}
```
