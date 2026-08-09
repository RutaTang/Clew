//! Where clew reads code from: the local machine, or a remote host over SSH.
//!
//! The client is a pure renderer that speaks `clew-protocol` to a clew-server
//! process (see `server.rs`). A [`ConnTarget`] chooses which server that is — a
//! local child, or an SSH session to a remote — and is the *key* of the server
//! subscription, so changing it tears down one transport and brings up the
//! other with no other code path changing. That is what makes remote and local
//! indistinguishable to the rest of the app.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Which server the client talks to. Used as the subscription identity, so it is
/// hashable: equal targets keep the same transport, a changed target restarts it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ConnTarget {
    /// A clew-server child on this machine.
    Local,
    /// A clew-server reached over SSH. `label` is `user@host` for display; `args`
    /// are the `ssh` CLI arguments (host plus `-p`/`-i`/`-o` flags) that open it.
    Ssh { label: String, args: Vec<String> },
}

/// The port a raw `ssh` argument list selects, if it says so: `-p 2222`,
/// `-p2222`, or `-o Port=2222` / `-oPort=2222` (case-insensitive, as ssh
/// treats option names). `None` means the default.
fn ssh_port(args: &[String]) -> Option<u16> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if let Some(rest) = a.strip_prefix("-p") {
            let value = if rest.is_empty() {
                it.next()?.as_str()
            } else {
                rest
            };
            return value.parse().ok();
        }
        if let Some(rest) = a.strip_prefix("-o") {
            let opt = if rest.is_empty() {
                it.next()?.as_str()
            } else {
                rest
            };
            if let Some((k, v)) = opt.split_once('=')
                && k.trim().eq_ignore_ascii_case("port")
            {
                return v.trim().parse().ok();
            }
        }
    }
    None
}

impl ConnTarget {
    /// The startup target: `CLEW_SSH` (raw `ssh` args) selects a remote for
    /// power users / tests; otherwise local. In-app connections replace this.
    pub fn from_env() -> Self {
        match std::env::var("CLEW_SSH") {
            Ok(ssh) if !ssh.trim().is_empty() => {
                let args: Vec<String> = ssh.split_whitespace().map(str::to_string).collect();
                // The host (or user@host) is the last non-flag token; fall back
                // to the whole string.
                let host = args
                    .iter()
                    .rev()
                    .find(|a| !a.starts_with('-'))
                    .cloned()
                    .unwrap_or_else(|| ssh.trim().to_string());
                // The label is also the approval-scoping identity, so a
                // non-default port belongs in it exactly as it does for an
                // in-app connection: `host:2222` can be a different machine
                // (or container) than `host:22`, and trust granted for one
                // must not silently cover the other.
                let label = match ssh_port(&args) {
                    Some(port) if port != 22 => format!("{host}:{port}"),
                    _ => host,
                };
                ConnTarget::Ssh { label, args }
            }
            _ => ConnTarget::Local,
        }
    }

    pub fn is_remote(&self) -> bool {
        matches!(self, ConnTarget::Ssh { .. })
    }

    /// The host identity approvals are scoped to: `None` locally, the SSH
    /// `user@host` label for a remote. The same absolute project path on two
    /// hosts is two different projects, so trust records must not collide.
    pub fn approval_host(&self) -> Option<&str> {
        match self {
            ConnTarget::Local => None,
            ConnTarget::Ssh { label, .. } => Some(label),
        }
    }

    /// Short label for the status-bar indicator.
    pub fn label(&self) -> String {
        match self {
            ConnTarget::Local => "Local".to_string(),
            ConnTarget::Ssh { label, .. } => label.clone(),
        }
    }
}

/// A remembered SSH host, editable in the Connect modal and persisted to
/// `connections.toml`. `name` is an optional friendly label; the rest map to
/// `ssh` flags.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedConnection {
    #[serde(default)]
    pub name: String,
    pub host: String,
    pub user: String,
    #[serde(default = "default_port")]
    pub port: u16,
    /// Path to a private key (`ssh -i`); empty means the agent / default keys.
    #[serde(default)]
    pub identity: String,
    /// Per-host opt-in: send the AI API keys to this host's clew-server so
    /// AI features run remotely. Off by default — keys never leave this
    /// machine unless the user granted it for exactly this host.
    #[serde(default)]
    pub send_ai_keys: bool,
}

fn default_port() -> u16 {
    22
}

impl SavedConnection {
    /// `user@host`, the canonical identity for display and de-duplication.
    pub fn user_host(&self) -> String {
        format!("{}@{}", self.user, self.host)
    }

    /// What the row shows: the friendly name if set, else `user@host`.
    pub fn label(&self) -> String {
        if self.name.trim().is_empty() {
            self.user_host()
        } else {
            self.name.clone()
        }
    }

    /// The `ssh` CLI arguments for this host. `accept-new` avoids a blocking
    /// host-key prompt on first connect (there is no TTY), and a connect timeout
    /// fails fast instead of hanging the transport.
    ///
    /// The keepalives bound how long a link can be dead without anyone
    /// noticing. Without them a laptop changing networks left `ssh` happily
    /// accepting frames into a pipe that went nowhere until TCP gave up
    /// MINUTES later, and everything written in that window was lost. Three
    /// missed 15s probes fail the session in ~45s instead.
    pub fn ssh_args(&self) -> Vec<String> {
        let mut args = vec![
            "-o".into(),
            "ConnectTimeout=10".into(),
            "-o".into(),
            "StrictHostKeyChecking=accept-new".into(),
            "-o".into(),
            "ServerAliveInterval=15".into(),
            "-o".into(),
            "ServerAliveCountMax=3".into(),
        ];
        if self.port != 22 {
            args.push("-p".into());
            args.push(self.port.to_string());
        }
        if !self.identity.trim().is_empty() {
            args.push("-i".into());
            args.push(self.identity.trim().to_string());
        }
        args.push(self.user_host());
        args
    }

    pub fn target(&self) -> ConnTarget {
        // The label doubles as the approval-scoping host identity, so a
        // non-default port is part of it: `host:2222` can be a different
        // machine (or container) than `host:22`, and trust granted for one
        // must not cover the other.
        let label = if self.port == 22 {
            self.user_host()
        } else {
            format!("{}:{}", self.user_host(), self.port)
        };
        ConnTarget::Ssh {
            label,
            args: self.ssh_args(),
        }
    }
}

/// On-disk shape of `connections.toml`: `[[connection]]` tables.
#[derive(Default, Serialize, Deserialize)]
struct Store {
    #[serde(default, rename = "connection")]
    connections: Vec<SavedConnection>,
}

fn store_path() -> Option<PathBuf> {
    Some(clew_core::lsp::store::data_root()?.join("connections.toml"))
}

/// Load saved connections; an absent or unreadable file yields an empty list.
pub fn load() -> Vec<SavedConnection> {
    let Some(path) = store_path() else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    toml::from_str::<Store>(&text)
        .map(|s| s.connections)
        .unwrap_or_default()
}

/// Persist the connection list, creating the data directory if needed.
pub fn save(connections: &[SavedConnection]) -> Result<(), String> {
    let path = store_path().ok_or("no data directory")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let store = Store {
        connections: connections.to_vec(),
    };
    let text = toml::to_string_pretty(&store).map_err(|e| e.to_string())?;
    std::fs::write(&path, text).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn(port: u16, identity: &str) -> SavedConnection {
        SavedConnection {
            name: String::new(),
            host: "example.com".into(),
            user: "root".into(),
            port,
            identity: identity.into(),
            send_ai_keys: false,
        }
    }

    /// `connections.toml` written before the AI-key opt-in existed must load
    /// with the opt-in OFF: absence of consent is not consent.
    #[test]
    fn send_ai_keys_defaults_to_false_on_old_toml() {
        let store: Store =
            toml::from_str("[[connection]]\nhost = \"example.com\"\nuser = \"root\"\n").unwrap();
        assert!(!store.connections[0].send_ai_keys);
    }

    #[test]
    fn ssh_args_include_port_and_identity_when_set() {
        let a = conn(2222, "/keys/id_ed25519").ssh_args();
        assert!(a.windows(2).any(|w| w == ["-p", "2222"]));
        assert!(a.windows(2).any(|w| w == ["-i", "/keys/id_ed25519"]));
        assert_eq!(a.last().unwrap(), "root@example.com");
    }

    #[test]
    fn ssh_args_omit_default_port_and_empty_identity() {
        let a = conn(22, "  ").ssh_args();
        assert!(!a.iter().any(|s| s == "-p"));
        assert!(!a.iter().any(|s| s == "-i"));
        assert_eq!(a.last().unwrap(), "root@example.com");
    }

    #[test]
    fn target_is_remote_labelled_user_host() {
        let t = conn(22, "").target();
        assert!(t.is_remote());
        assert_eq!(t.label(), "root@example.com");
    }

    #[test]
    fn label_prefers_name_over_user_host() {
        let mut c = conn(22, "");
        assert_eq!(c.label(), "root@example.com");
        c.name = "prod".into();
        assert_eq!(c.label(), "prod");
    }

    #[test]
    fn local_target_is_not_remote() {
        assert!(!ConnTarget::Local.is_remote());
        assert_eq!(ConnTarget::Local.label(), "Local");
    }
}

#[cfg(test)]
mod env_target_tests {
    use super::*;

    fn from(ssh: &str) -> ConnTarget {
        let args: Vec<String> = ssh.split_whitespace().map(str::to_string).collect();
        let host = args
            .iter()
            .rev()
            .find(|a| !a.starts_with('-'))
            .cloned()
            .unwrap();
        match ssh_port(&args) {
            Some(port) if port != 22 => ConnTarget::Ssh {
                label: format!("{host}:{port}"),
                args,
            },
            _ => ConnTarget::Ssh { label: host, args },
        }
    }

    /// The label doubles as the approval-scoping host identity. A CLEW_SSH
    /// target used to drop the port from it, so trust granted for a container
    /// on `host:2222` silently covered `host:22` — a different machine.
    #[test]
    fn a_clew_ssh_target_scopes_trust_to_its_port() {
        assert_eq!(
            from("root@example.com").approval_host(),
            Some("root@example.com")
        );
        assert_eq!(
            from("-p 22 root@example.com").approval_host(),
            Some("root@example.com")
        );
        for spelling in [
            "-p 2222 root@example.com",
            "-p2222 root@example.com",
            "-o Port=2222 root@example.com",
            "-oPort=2222 root@example.com",
            "-o port=2222 root@example.com",
        ] {
            assert_eq!(
                from(spelling).approval_host(),
                Some("root@example.com:2222"),
                "{spelling}"
            );
        }
    }
}
