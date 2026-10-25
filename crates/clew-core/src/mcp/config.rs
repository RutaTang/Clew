//! The MCP servers the user configured, in the global `config.toml`:
//!
//! ```toml
//! [mcp.servers.github]
//! command = "npx"
//! args = ["-y", "@modelcontextprotocol/server-github"]
//! env = { GITHUB_PERSONAL_ACCESS_TOKEN = "…" }
//! approve = "ask"            # ask (default) | read_only | always
//! allow = ["search_code"]    # tools that never ask
//!
//! [mcp.servers.docs]
//! url = "https://example.com/mcp"
//! headers = { Authorization = "Bearer …" }
//! enabled = false
//! timeout_secs = 300         # one tool call, at most (default 120)
//! ```
//!
//! A server is a `command` (stdio) or a `url` (Streamable HTTP). The usual
//! JSON shape other MCP clients document — `{"mcpServers": {"name": {…}}}`
//! — is read too ([`from_json`]), so a server's README snippet can be pasted
//! as it is.
//!
//! The file is the user's own, and written user-only (it may hold tokens):
//! every write goes through [`crate::globalconfig::edit`].

use std::time::Duration;

use serde_json::Value;

use crate::linerpc::Launch;

/// The default time box of one tool call.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// The longest time box a server may be given.
const MAX_TIMEOUT_SECS: u64 = 3600;

/// Longest server name. The name is part of the tool names the model sees
/// ([`super::model_tool_name`]), which are short.
pub const MAX_NAME_LEN: usize = 32;

/// When a tool call needs the user's say-so.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Approve {
    /// Every call asks (tools in `allow` aside).
    #[default]
    Ask,
    /// Tools the server marks read-only run without asking; the rest ask.
    ReadOnly,
    /// No call asks: the user trusts this server with everything.
    Always,
}

impl Approve {
    pub fn as_str(self) -> &'static str {
        match self {
            Approve::Ask => "ask",
            Approve::ReadOnly => "read_only",
            Approve::Always => "always",
        }
    }

    pub fn parse(s: &str) -> Option<Approve> {
        match s {
            "ask" => Some(Approve::Ask),
            "read_only" | "readonly" | "read-only" => Some(Approve::ReadOnly),
            "always" | "never_ask" => Some(Approve::Always),
            _ => None,
        }
    }

    /// For the settings form.
    pub fn label(self) -> &'static str {
        match self {
            Approve::Ask => "Ask every time",
            Approve::ReadOnly => "Ask unless read-only",
            Approve::Always => "Never ask",
        }
    }

    pub const ALL: [Approve; 3] = [Approve::Ask, Approve::ReadOnly, Approve::Always];
}

impl std::fmt::Display for Approve {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// How the server is reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transport {
    /// A process this program starts, spoken to over its stdio.
    Stdio(Launch),
    /// A Streamable HTTP endpoint.
    Http {
        url: String,
        headers: Vec<(String, String)>,
    },
}

/// One configured server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    pub name: String,
    pub transport: Transport,
    pub enabled: bool,
    pub approve: Approve,
    /// Tools that run without asking, whatever `approve` says.
    pub allow: Vec<String>,
    /// The time box of one tool call.
    pub timeout: Duration,
}

impl ServerConfig {
    /// Whether a call of `tool` (read-only by the server's own account when
    /// `read_only`) needs the user's say-so.
    pub fn needs_approval(&self, tool: &str, read_only: bool) -> bool {
        if self.allow.iter().any(|t| t == tool) {
            return false;
        }
        match self.approve {
            Approve::Ask => true,
            Approve::ReadOnly => !read_only,
            Approve::Always => false,
        }
    }

    /// A one-line description of where the server is, for the settings list.
    pub fn summary(&self) -> String {
        match &self.transport {
            Transport::Stdio(launch) => {
                let mut line = launch.program.clone();
                for arg in &launch.args {
                    line.push(' ');
                    line.push_str(arg);
                }
                line
            }
            Transport::Http { url, .. } => url.clone(),
        }
    }
}

/// Whether `name` can name a server: letters, digits, `-` and `_`.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// `name` made into one that [`valid_name`] accepts.
pub fn clean_name(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(MAX_NAME_LEN)
        .collect();
    if out.is_empty() {
        out.push_str("server");
    }
    out
}

/// Whether `name: value` can be sent as an HTTP header.
fn valid_header(name: &str, value: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
        && !value.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0)
}

/// The configured servers, and a message for each entry that could not be
/// read. A missing file or section is no servers.
pub fn load() -> (Vec<ServerConfig>, Vec<String>) {
    let table = match crate::globalconfig::read() {
        Ok(Some(table)) => table,
        Ok(None) => return (Vec::new(), Vec::new()),
        Err(e) => return (Vec::new(), vec![e]),
    };
    let servers = table
        .get("mcp")
        .and_then(|m| m.get("servers"))
        .and_then(toml::Value::as_table);
    match servers {
        Some(servers) => from_table(servers),
        None => (Vec::new(), Vec::new()),
    }
}

/// The servers of a `[mcp.servers]` table.
pub fn from_table(servers: &toml::Table) -> (Vec<ServerConfig>, Vec<String>) {
    let mut out = Vec::new();
    let mut errors = Vec::new();
    for (name, entry) in servers {
        match server_from_toml(name, entry) {
            Ok(server) => out.push(server),
            Err(why) => errors.push(format!("MCP server `{name}`: {why}")),
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    (out, errors)
}

fn server_from_toml(name: &str, entry: &toml::Value) -> Result<ServerConfig, String> {
    if !valid_name(name) {
        return Err(format!(
            "a server name is letters, digits, `-` and `_`, at most {MAX_NAME_LEN} of them"
        ));
    }
    let entry = entry.as_table().ok_or("not a table")?;
    let str_list = |key: &str| -> Result<Vec<String>, String> {
        match entry.get(key) {
            None => Ok(Vec::new()),
            Some(toml::Value::Array(items)) => items
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_owned)
                        .ok_or(format!("`{key}` holds something that is not a string"))
                })
                .collect(),
            Some(_) => Err(format!("`{key}` is not a list of strings")),
        }
    };
    let str_map = |key: &str| -> Result<Vec<(String, String)>, String> {
        match entry.get(key) {
            None => Ok(Vec::new()),
            Some(toml::Value::Table(map)) => map
                .iter()
                .map(|(k, v)| {
                    v.as_str()
                        .map(|v| (k.clone(), v.to_string()))
                        .ok_or(format!("`{key}.{k}` is not a string"))
                })
                .collect(),
            Some(_) => Err(format!("`{key}` is not a table of strings")),
        }
    };
    let command = entry.get("command").and_then(toml::Value::as_str);
    let url = entry.get("url").and_then(toml::Value::as_str);
    let transport = match (command, url) {
        (Some(command), None) => Transport::Stdio(Launch {
            program: command.trim().to_string(),
            args: str_list("args")?,
            env: str_map("env")?,
            cwd: entry
                .get("cwd")
                .and_then(toml::Value::as_str)
                .filter(|c| !c.trim().is_empty())
                .map(crate::linerpc::expand_home),
        }),
        (None, Some(url)) => {
            let headers = str_map("headers")?;
            if let Some((bad, _)) = headers.iter().find(|(k, v)| !valid_header(k, v)) {
                return Err(format!("header `{bad}` cannot be sent as it is"));
            }
            Transport::Http {
                url: url.trim().to_string(),
                headers,
            }
        }
        (Some(_), Some(_)) => return Err("has both a `command` and a `url`".into()),
        (None, None) => return Err("needs a `command` or a `url`".into()),
    };
    if let Transport::Stdio(launch) = &transport
        && launch.program.is_empty()
    {
        return Err("an empty `command`".into());
    }
    let approve = match entry.get("approve") {
        None => Approve::Ask,
        Some(v) => v
            .as_str()
            .and_then(Approve::parse)
            .ok_or("`approve` is one of \"ask\", \"read_only\", \"always\"")?,
    };
    let timeout = match entry.get("timeout_secs") {
        None => DEFAULT_TIMEOUT,
        Some(v) => {
            let secs = v
                .as_integer()
                .filter(|s| *s > 0)
                .ok_or("`timeout_secs` is a positive number of seconds")?;
            Duration::from_secs((secs as u64).min(MAX_TIMEOUT_SECS))
        }
    };
    Ok(ServerConfig {
        name: name.to_string(),
        transport,
        enabled: entry
            .get("enabled")
            .and_then(toml::Value::as_bool)
            .unwrap_or(true),
        approve,
        allow: str_list("allow")?,
        timeout,
    })
}

/// A server as its `[mcp.servers.<name>]` table.
pub fn to_toml(server: &ServerConfig) -> toml::Table {
    let mut t = toml::Table::new();
    let strings = |items: &[String]| {
        toml::Value::Array(items.iter().cloned().map(toml::Value::String).collect())
    };
    let map = |pairs: &[(String, String)]| {
        toml::Value::Table(
            pairs
                .iter()
                .map(|(k, v)| (k.clone(), toml::Value::String(v.clone())))
                .collect(),
        )
    };
    match &server.transport {
        Transport::Stdio(launch) => {
            t.insert("command".into(), launch.program.clone().into());
            if !launch.args.is_empty() {
                t.insert("args".into(), strings(&launch.args));
            }
            if !launch.env.is_empty() {
                t.insert("env".into(), map(&launch.env));
            }
            if let Some(cwd) = &launch.cwd {
                t.insert("cwd".into(), cwd.to_string_lossy().into_owned().into());
            }
        }
        Transport::Http { url, headers } => {
            t.insert("url".into(), url.clone().into());
            if !headers.is_empty() {
                t.insert("headers".into(), map(headers));
            }
        }
    }
    if !server.enabled {
        t.insert("enabled".into(), false.into());
    }
    if server.approve != Approve::Ask {
        t.insert("approve".into(), server.approve.as_str().into());
    }
    if !server.allow.is_empty() {
        t.insert("allow".into(), strings(&server.allow));
    }
    if server.timeout != DEFAULT_TIMEOUT {
        t.insert(
            "timeout_secs".into(),
            toml::Value::Integer(server.timeout.as_secs() as i64),
        );
    }
    t
}

/// Apply `f` to the `[mcp.servers]` table, created when missing.
fn edit_servers(f: impl FnOnce(&mut toml::Table)) -> Result<(), String> {
    crate::globalconfig::edit(|root| {
        let mcp = root
            .entry("mcp")
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        if !mcp.is_table() {
            *mcp = toml::Value::Table(toml::Table::new());
        }
        let mcp = mcp.as_table_mut().expect("a table");
        let servers = mcp
            .entry("servers")
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        if !servers.is_table() {
            *servers = toml::Value::Table(toml::Table::new());
        }
        f(servers.as_table_mut().expect("a table"));
    })
}

/// Store `server` under its name, replacing an entry of that name.
pub fn save(server: &ServerConfig) -> Result<(), String> {
    if !valid_name(&server.name) {
        return Err(format!("`{}` cannot name a server", server.name));
    }
    let table = to_toml(server);
    edit_servers(|servers| {
        servers.insert(server.name.clone(), toml::Value::Table(table));
    })
}

/// Remove the server `name`.
pub fn remove(name: &str) -> Result<(), String> {
    edit_servers(|servers| {
        servers.remove(name);
    })
}

/// Change one key of server `name`'s entry, leaving the rest as written.
fn edit_entry(name: &str, f: impl FnOnce(&mut toml::Table)) -> Result<(), String> {
    let mut found = false;
    edit_servers(|servers| {
        if let Some(toml::Value::Table(entry)) = servers.get_mut(name) {
            found = true;
            f(entry);
        }
    })?;
    if found {
        Ok(())
    } else {
        Err(format!("no MCP server `{name}` in the configuration"))
    }
}

/// Turn server `name` on or off.
pub fn set_enabled(name: &str, enabled: bool) -> Result<(), String> {
    edit_entry(name, |entry| {
        if enabled {
            entry.remove("enabled");
        } else {
            entry.insert("enabled".into(), false.into());
        }
    })
}

/// Set server `name`'s approval policy.
pub fn set_approve(name: &str, approve: Approve) -> Result<(), String> {
    edit_entry(name, |entry| {
        if approve == Approve::Ask {
            entry.remove("approve");
        } else {
            entry.insert("approve".into(), approve.as_str().into());
        }
    })
}

/// Let server `name`'s `tool` run without asking from now on.
pub fn allow_tool(name: &str, tool: &str) -> Result<(), String> {
    edit_entry(name, |entry| {
        let list = entry
            .entry("allow")
            .or_insert_with(|| toml::Value::Array(Vec::new()));
        if !list.is_array() {
            *list = toml::Value::Array(Vec::new());
        }
        let list = list.as_array_mut().expect("an array");
        if !list.iter().any(|v| v.as_str() == Some(tool)) {
            list.push(toml::Value::String(tool.to_string()));
        }
    })
}

/// Servers from the JSON other clients document: `{"mcpServers": {…}}` (or
/// VS Code's `{"servers": {…}}`, or the bare map of names), each entry a
/// `command` with `args`/`env`, or a `url` with `headers`. Entries of the
/// deprecated `sse` type are refused, by name.
pub fn from_json(text: &str) -> Result<Vec<ServerConfig>, String> {
    let value: Value =
        serde_json::from_str(text.trim()).map_err(|e| format!("not valid JSON: {e}"))?;
    let map = value
        .get("mcpServers")
        .or_else(|| value.get("servers"))
        .or_else(|| value.get("mcp").and_then(|m| m.get("servers")))
        .unwrap_or(&value)
        .as_object()
        .ok_or("expected {\"mcpServers\": {\"name\": {…}}}")?;
    // A single server's own object (it has a command or a url) pasted bare.
    if map.contains_key("command") || map.contains_key("url") {
        return Err("give the server a name: {\"mcpServers\": {\"name\": {…}}}".into());
    }
    let mut out = Vec::new();
    for (name, entry) in map {
        out.push(server_from_json(name, entry).map_err(|why| format!("`{name}`: {why}"))?);
    }
    if out.is_empty() {
        return Err("no servers in it".into());
    }
    Ok(out)
}

fn server_from_json(name: &str, entry: &Value) -> Result<ServerConfig, String> {
    let entry = entry.as_object().ok_or("not an object")?;
    let strings = |key: &str| -> Vec<String> {
        entry
            .get(key)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    };
    let pairs = |key: &str| -> Vec<(String, String)> {
        entry
            .get(key)
            .and_then(Value::as_object)
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
                    .collect()
            })
            .unwrap_or_default()
    };
    let kind = entry
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if kind == "sse" {
        return Err("the deprecated SSE transport is not supported — use the server's Streamable HTTP URL or its command".into());
    }
    let transport = if let Some(command) = entry.get("command").and_then(Value::as_str) {
        Transport::Stdio(Launch {
            program: command.trim().to_string(),
            args: strings("args"),
            env: pairs("env"),
            cwd: entry
                .get("cwd")
                .and_then(Value::as_str)
                .map(crate::linerpc::expand_home),
        })
    } else if let Some(url) = entry
        .get("url")
        .or_else(|| entry.get("serverUrl"))
        .and_then(Value::as_str)
    {
        let headers = pairs("headers");
        if let Some((bad, _)) = headers.iter().find(|(k, v)| !valid_header(k, v)) {
            return Err(format!("header `{bad}` cannot be sent as it is"));
        }
        Transport::Http {
            url: url.trim().to_string(),
            headers,
        }
    } else {
        return Err("needs a `command` or a `url`".into());
    };
    Ok(ServerConfig {
        name: clean_name(name),
        transport,
        enabled: !entry
            .get("disabled")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        approve: Approve::Ask,
        allow: Vec::new(),
        timeout: DEFAULT_TIMEOUT,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(text: &str) -> toml::Table {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn servers_read_from_toml_with_defaults() {
        let t = table(
            r#"
            [gh]
            command = "npx"
            args = ["-y", "server-github"]
            env = { TOKEN = "t" }
            allow = ["search"]

            [remote]
            url = "https://example.com/mcp"
            headers = { Authorization = "Bearer x" }
            approve = "read_only"
            enabled = false
            timeout_secs = 99999
            "#,
        );
        let (servers, errors) = from_table(&t);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(servers.len(), 2);
        let gh = &servers[0];
        assert_eq!(gh.name, "gh");
        assert!(gh.enabled);
        assert_eq!(gh.approve, Approve::Ask);
        assert_eq!(gh.timeout, DEFAULT_TIMEOUT);
        let Transport::Stdio(launch) = &gh.transport else {
            panic!("stdio")
        };
        assert_eq!(launch.args, ["-y", "server-github"]);
        assert_eq!(launch.env, [("TOKEN".to_string(), "t".to_string())]);
        assert!(!gh.needs_approval("search", false), "allowed tool");
        assert!(gh.needs_approval("delete", true));
        let remote = &servers[1];
        assert!(!remote.enabled);
        assert_eq!(remote.timeout, Duration::from_secs(MAX_TIMEOUT_SECS));
        assert!(!remote.needs_approval("read", true));
        assert!(remote.needs_approval("write", false));
        // Round trip.
        let back = server_from_toml("remote", &toml::Value::Table(to_toml(remote))).unwrap();
        assert_eq!(&back, remote);
    }

    #[test]
    fn bad_entries_are_named_and_skipped() {
        let t = table(
            r#"
            ["bad name"]
            command = "x"
            [both]
            command = "x"
            url = "https://x"
            [neither]
            args = []
            [policy]
            command = "x"
            approve = "sometimes"
            [header]
            url = "https://x"
            headers = { "Bad Header" = "v" }
            [ok]
            command = "x"
            "#,
        );
        let (servers, errors) = from_table(&t);
        assert_eq!(servers.len(), 1);
        assert_eq!(errors.len(), 5, "{errors:?}");
    }

    #[test]
    fn readme_json_snippets_are_read() {
        let servers = from_json(
            r#"{"mcpServers": {
                "file system": {"command": "npx", "args": ["-y", "@mcp/fs", "/tmp"]},
                "web": {"type": "http", "url": "https://x/mcp", "headers": {"X-Key": "k"}},
                "off": {"command": "y", "disabled": true}
            }}"#,
        )
        .unwrap();
        assert_eq!(servers.len(), 3);
        let named = |n: &str| servers.iter().find(|s| s.name == n).unwrap();
        assert!(matches!(
            &named("file_system").transport,
            Transport::Stdio(_)
        ));
        assert!(
            matches!(&named("web").transport, Transport::Http { headers, .. } if headers.len() == 1)
        );
        assert!(!named("off").enabled);
        assert!(
            from_json(r#"{"mcpServers": {"old": {"type": "sse", "url": "https://x/sse"}}}"#)
                .is_err()
        );
        assert!(from_json(r#"{"command": "x"}"#).is_err());
        assert!(from_json("nope").is_err());
    }

    #[test]
    fn names_are_cleaned_to_what_tool_names_can_carry() {
        assert!(valid_name("git-hub_2"));
        assert!(!valid_name("a b"));
        assert!(!valid_name(""));
        assert_eq!(clean_name("a b/c"), "a_b_c");
        assert_eq!(clean_name(""), "server");
        assert_eq!(clean_name(&"x".repeat(50)).len(), MAX_NAME_LEN);
    }

    #[test]
    fn the_config_file_is_edited_in_place() {
        let _data = crate::testutil::DataDir::new("mcp-config");
        let server = ServerConfig {
            name: "one".into(),
            transport: Transport::Stdio(Launch {
                program: "srv".into(),
                ..Launch::default()
            }),
            enabled: true,
            approve: Approve::Ask,
            allow: Vec::new(),
            timeout: DEFAULT_TIMEOUT,
        };
        save(&server).unwrap();
        set_approve("one", Approve::Always).unwrap();
        allow_tool("one", "t").unwrap();
        allow_tool("one", "t").unwrap();
        set_enabled("one", false).unwrap();
        let (servers, errors) = load();
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].approve, Approve::Always);
        assert_eq!(servers[0].allow, ["t"]);
        assert!(!servers[0].enabled);
        assert!(set_enabled("missing", true).is_err());
        remove("one").unwrap();
        assert!(load().0.is_empty());
    }
}
