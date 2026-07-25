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

impl ConnTarget {
    /// The startup target: `CLEW_SSH` (raw `ssh` args) selects a remote for
    /// power users / tests; otherwise local. In-app connections replace this.
    pub fn from_env() -> Self {
        match std::env::var("CLEW_SSH") {
            Ok(ssh) if !ssh.trim().is_empty() => {
                let args: Vec<String> = ssh.split_whitespace().map(str::to_string).collect();
                // The host (or user@host) is the last non-flag token; fall back
                // to the whole string.
                let label = args
                    .iter()
                    .rev()
                    .find(|a| !a.starts_with('-'))
                    .cloned()
                    .unwrap_or_else(|| ssh.trim().to_string());
                ConnTarget::Ssh { label, args }
            }
            _ => ConnTarget::Local,
        }
    }

    pub fn is_remote(&self) -> bool {
        matches!(self, ConnTarget::Ssh { .. })
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
    pub fn ssh_args(&self) -> Vec<String> {
        let mut args = vec![
            "-o".into(),
            "ConnectTimeout=10".into(),
            "-o".into(),
            "StrictHostKeyChecking=accept-new".into(),
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
        ConnTarget::Ssh {
            label: self.user_host(),
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
        }
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
