//! The ACP agents the user configured, in the global `config.toml`:
//!
//! ```toml
//! [acp.agents.claude]
//! command = "npx"
//! args = ["-y", "@agentclientprotocol/claude-agent-acp"]
//! env = { ANTHROPIC_API_KEY = "…" }   # optional
//! share_mcp = true                    # also hand it the user's MCP servers
//! ```
//!
//! Every agent also gets clew's own code tools (`clew-server mcp`). The
//! [`PRESETS`] are the agents of the ACP registry that start through `npx`,
//! for the settings form to add in one step.

use crate::linerpc::Launch;

/// One configured agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentConfig {
    pub name: String,
    pub launch: Launch,
    /// Hand the agent the user's enabled MCP servers too.
    pub share_mcp: bool,
}

impl AgentConfig {
    /// A one-line description of how the agent starts.
    pub fn summary(&self) -> String {
        let mut line = self.launch.program.clone();
        for arg in &self.launch.args {
            line.push(' ');
            line.push_str(arg);
        }
        line
    }
}

/// A known agent the settings form offers to add.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preset {
    /// The name it is added under.
    pub name: &'static str,
    /// What to call it in the form.
    pub label: &'static str,
    pub program: &'static str,
    pub args: &'static [&'static str],
}

/// The agents of the ACP registry that start through `npx` (unpinned, so
/// each start runs the current release), and OpenCode's own binary.
pub const PRESETS: &[Preset] = &[
    Preset {
        name: "claude",
        label: "Claude Agent",
        program: "npx",
        args: &["-y", "@agentclientprotocol/claude-agent-acp"],
    },
    Preset {
        name: "codex",
        label: "Codex",
        program: "npx",
        args: &["-y", "@agentclientprotocol/codex-acp"],
    },
    Preset {
        name: "gemini",
        label: "Gemini CLI",
        program: "npx",
        args: &["-y", "@google/gemini-cli", "--acp"],
    },
    Preset {
        name: "copilot",
        label: "GitHub Copilot",
        program: "npx",
        args: &["-y", "@github/copilot-language-server", "--acp"],
    },
    Preset {
        name: "qwen",
        label: "Qwen Code",
        program: "npx",
        args: &["-y", "@qwen-code/qwen-code", "--acp"],
    },
    Preset {
        name: "opencode",
        label: "OpenCode",
        program: "opencode",
        args: &["acp"],
    },
];

impl Preset {
    pub fn config(&self) -> AgentConfig {
        AgentConfig {
            name: self.name.to_string(),
            launch: Launch {
                program: self.program.to_string(),
                args: self.args.iter().map(|a| a.to_string()).collect(),
                env: Vec::new(),
                cwd: None,
            },
            share_mcp: false,
        }
    }
}

/// The configured agents, and a message for each entry that could not be
/// read.
pub fn load() -> (Vec<AgentConfig>, Vec<String>) {
    let table = match crate::globalconfig::read() {
        Ok(Some(table)) => table,
        Ok(None) => return (Vec::new(), Vec::new()),
        Err(e) => return (Vec::new(), vec![e]),
    };
    match table
        .get("acp")
        .and_then(|a| a.get("agents"))
        .and_then(toml::Value::as_table)
    {
        Some(agents) => from_table(agents),
        None => (Vec::new(), Vec::new()),
    }
}

/// The agents of an `[acp.agents]` table.
pub fn from_table(agents: &toml::Table) -> (Vec<AgentConfig>, Vec<String>) {
    let mut out = Vec::new();
    let mut errors = Vec::new();
    for (name, entry) in agents {
        match agent_from_toml(name, entry) {
            Ok(agent) => out.push(agent),
            Err(why) => errors.push(format!("ACP agent `{name}`: {why}")),
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    (out, errors)
}

fn agent_from_toml(name: &str, entry: &toml::Value) -> Result<AgentConfig, String> {
    if !crate::mcp::config::valid_name(name) {
        return Err("a name is letters, digits, `-` and `_`".into());
    }
    let entry = entry.as_table().ok_or("not a table")?;
    let command = entry
        .get("command")
        .and_then(toml::Value::as_str)
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .ok_or("needs a `command`")?;
    let args = match entry.get("args") {
        None => Vec::new(),
        Some(toml::Value::Array(items)) => items
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .ok_or("`args` holds a non-string")
            })
            .collect::<Result<_, _>>()?,
        Some(_) => return Err("`args` is not a list of strings".into()),
    };
    let env = match entry.get("env") {
        None => Vec::new(),
        Some(toml::Value::Table(map)) => map
            .iter()
            .map(|(k, v)| {
                v.as_str()
                    .map(|v| (k.clone(), v.to_string()))
                    .ok_or(format!("`env.{k}` is not a string"))
            })
            .collect::<Result<_, _>>()?,
        Some(_) => return Err("`env` is not a table of strings".into()),
    };
    Ok(AgentConfig {
        name: name.to_string(),
        launch: Launch {
            program: command.to_string(),
            args,
            env,
            cwd: None,
        },
        share_mcp: entry
            .get("share_mcp")
            .and_then(toml::Value::as_bool)
            .unwrap_or(false),
    })
}

/// An agent as its `[acp.agents.<name>]` table.
pub fn to_toml(agent: &AgentConfig) -> toml::Table {
    let mut t = toml::Table::new();
    t.insert("command".into(), agent.launch.program.clone().into());
    if !agent.launch.args.is_empty() {
        t.insert(
            "args".into(),
            toml::Value::Array(
                agent
                    .launch
                    .args
                    .iter()
                    .cloned()
                    .map(toml::Value::String)
                    .collect(),
            ),
        );
    }
    if !agent.launch.env.is_empty() {
        t.insert(
            "env".into(),
            toml::Value::Table(
                agent
                    .launch
                    .env
                    .iter()
                    .map(|(k, v)| (k.clone(), toml::Value::String(v.clone())))
                    .collect(),
            ),
        );
    }
    if agent.share_mcp {
        t.insert("share_mcp".into(), true.into());
    }
    t
}

fn edit_agents(f: impl FnOnce(&mut toml::Table)) -> Result<(), String> {
    crate::globalconfig::edit(|root| {
        let acp = root
            .entry("acp")
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        if !acp.is_table() {
            *acp = toml::Value::Table(toml::Table::new());
        }
        let agents = acp
            .as_table_mut()
            .expect("a table")
            .entry("agents")
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        if !agents.is_table() {
            *agents = toml::Value::Table(toml::Table::new());
        }
        f(agents.as_table_mut().expect("a table"));
    })
}

/// Store `agent` under its name, replacing one of that name.
pub fn save(agent: &AgentConfig) -> Result<(), String> {
    if !crate::mcp::config::valid_name(&agent.name) {
        return Err(format!("`{}` cannot name an agent", agent.name));
    }
    let table = to_toml(agent);
    edit_agents(|agents| {
        agents.insert(agent.name.clone(), toml::Value::Table(table));
    })
}

/// Remove the agent `name`.
pub fn remove(name: &str) -> Result<(), String> {
    edit_agents(|agents| {
        agents.remove(name);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agents_read_from_toml_and_round_trip() {
        let t: toml::Table = toml::from_str(
            r#"
            [claude]
            command = "npx"
            args = ["-y", "@agentclientprotocol/claude-agent-acp"]
            env = { K = "v" }
            share_mcp = true
            [bad]
            args = []
            "#,
        )
        .unwrap();
        let (agents, errors) = from_table(&t);
        assert_eq!(agents.len(), 1);
        assert_eq!(errors.len(), 1, "{errors:?}");
        let claude = &agents[0];
        assert!(claude.share_mcp);
        assert_eq!(
            claude.summary(),
            "npx -y @agentclientprotocol/claude-agent-acp"
        );
        let back = agent_from_toml("claude", &toml::Value::Table(to_toml(claude))).unwrap();
        assert_eq!(&back, claude);
    }

    #[test]
    fn presets_are_valid_agents() {
        for preset in PRESETS {
            let agent = preset.config();
            assert!(
                crate::mcp::config::valid_name(&agent.name),
                "{}",
                agent.name
            );
            let back = agent_from_toml(&agent.name, &toml::Value::Table(to_toml(&agent))).unwrap();
            assert_eq!(back, agent);
        }
    }

    #[test]
    fn the_config_file_is_edited_in_place() {
        let _data = crate::testutil::DataDir::new("acp-config");
        save(&PRESETS[0].config()).unwrap();
        save(&PRESETS[2].config()).unwrap();
        assert_eq!(load().0.len(), 2);
        remove("claude").unwrap();
        let (agents, _) = load();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].name, "gemini");
    }
}
