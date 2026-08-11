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

/// Persist the connection list wholesale, creating the data directory if
/// needed. Correct only when the caller's list IS the whole truth (a fresh
/// read it has not shared with anyone); mutations must go through [`upsert`]
/// or [`remove`] instead. Written atomically (temp + rename), so a second clew
/// process can never observe a half-written file.
pub fn save(connections: &[SavedConnection]) -> Result<(), String> {
    let path = store_path().ok_or("no data directory")?;
    let store = Store {
        connections: connections.to_vec(),
    };
    let text = toml::to_string_pretty(&store).map_err(|e| e.to_string())?;
    clew_core::statefile::write_atomic(&path, text.as_bytes()).map_err(|e| e.to_string())
}

/// Serializes the read-modify-write below across this process's windows.
///
/// Every window holds its own `saved_connections` snapshot, taken when the
/// window opened, so two windows editing connections are two writers with
/// hours-old copies of the list.
static SAVE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Apply one change to what is on disk RIGHT NOW and persist it, returning the
/// merged list.
///
/// The re-read is the whole point. `save` writes a window's in-memory Vec
/// wholesale, so the last window to save erased every entry the other window
/// had added or deleted since it loaded — and because the losing window kept
/// rendering its own stale copy, nothing looked wrong until the next launch.
/// Callers must adopt the returned list for exactly that reason.
///
/// Two clew PROCESSES are covered too, by the file lock this is wrapped in.
/// That matters more here than for the per-project stores: `connections.toml`
/// is GLOBAL, so the two writers do not even have to be on the same project.
///
/// Residual, accepted: that lock is best effort — with no writable data
/// directory, or on a filesystem without `flock`, this runs unlocked and two
/// processes can still interleave between the read and the rename, losing one
/// entry. Never a torn file, which the atomic write rules out.
fn edit(change: impl FnOnce(&mut Vec<SavedConnection>)) -> Result<Vec<SavedConnection>, String> {
    // Poisoning only means an earlier caller panicked; the list is re-read from
    // disk here regardless, so there is no corrupt state to inherit.
    let _serialized = SAVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _exclusive = store_path().and_then(|p| clew_core::statefile::lock_exclusive(&p));
    let mut merged = load();
    change(&mut merged);
    save(&merged)?;
    Ok(merged)
}

/// Add (or update) one connection, de-duplicated by `user@host` + port and
/// placed first (most-recent-first ordering). Returns the merged list.
pub fn upsert(conn: SavedConnection) -> Result<Vec<SavedConnection>, String> {
    edit(|list| {
        list.retain(|c| !(c.user_host() == conn.user_host() && c.port == conn.port));
        list.insert(0, conn);
    })
}

/// Drop the connection with this `user@host` + port. Returns the merged list.
///
/// Identity, not index: the caller's index points into its own snapshot, which
/// another window may have reordered on disk since.
pub fn remove(user_host: &str, port: u16) -> Result<Vec<SavedConnection>, String> {
    edit(|list| list.retain(|c| !(c.user_host() == user_host && c.port == port)))
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

    /// Point the store at a fresh temp directory. `CLEW_DATA_DIR` is
    /// process-global, so the shared env lock keeps concurrent tests apart.
    fn with_data_dir<T>(name: &str, f: impl FnOnce() -> T) -> T {
        let _env = clew_core::env_lock();
        let dir = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let prev = std::env::var_os("CLEW_DATA_DIR");
        // SAFETY: env mutation serialized by env_lock.
        unsafe { std::env::set_var("CLEW_DATA_DIR", &dir) };
        let out = f();
        match prev {
            Some(p) => unsafe { std::env::set_var("CLEW_DATA_DIR", p) },
            None => unsafe { std::env::remove_var("CLEW_DATA_DIR") },
        }
        out
    }

    fn host(host: &str) -> SavedConnection {
        SavedConnection {
            name: String::new(),
            host: host.into(),
            user: "root".into(),
            port: 22,
            identity: String::new(),
            send_ai_keys: false,
        }
    }

    fn hosts(list: &[SavedConnection]) -> Vec<&str> {
        list.iter().map(|c| c.host.as_str()).collect()
    }

    #[test]
    fn upsert_round_trips_and_de_duplicates_by_user_host_and_port() {
        with_data_dir("clew-connect-roundtrip", || {
            let mut keyed = host("a.example.com");
            keyed.identity = "/keys/id_ed25519".into();
            upsert(keyed).unwrap();
            let stored = load();
            assert_eq!(stored.len(), 1);
            assert_eq!(stored[0].identity, "/keys/id_ed25519");

            // Same user@host, same port: replaced in place, not duplicated…
            let mut renamed = host("a.example.com");
            renamed.name = "prod".into();
            assert_eq!(upsert(renamed).unwrap().len(), 1);
            assert_eq!(load()[0].label(), "prod");

            // …but a different port is a different machine, so it is its own row.
            let mut other_port = host("a.example.com");
            other_port.port = 2222;
            assert_eq!(upsert(other_port).unwrap().len(), 2);
        });
    }

    /// Every window loads `saved_connections` once and `save` wrote that
    /// snapshot wholesale, so the last window to save deleted the host the
    /// other window had just added — and kept rendering its own stale list, so
    /// nothing looked wrong until relaunch. The add path must merge against
    /// the file.
    #[test]
    fn an_add_from_a_stale_window_keeps_what_another_window_added() {
        with_data_dir("clew-connect-add-merge", || {
            upsert(host("a.example.com")).unwrap();
            // Window 1 opens here and holds this list for the rest of the test.
            let stale = load();
            assert_eq!(hosts(&stale), ["a.example.com"]);

            // Window 2 saves host B while window 1 sits idle.
            upsert(host("b.example.com")).unwrap();
            assert!(
                !stale.iter().any(|c| c.host == "b.example.com"),
                "window 1's snapshot is genuinely stale now"
            );

            // Window 1 adds host C. B must survive, and window 1's returned
            // list must be what is on disk, not its snapshot plus C.
            let merged = upsert(host("c.example.com")).unwrap();
            assert_eq!(
                hosts(&merged),
                ["c.example.com", "b.example.com", "a.example.com"],
                "most-recent-first, with nothing lost"
            );
            assert_eq!(merged, load());
        });
    }

    /// The symmetric loss: a deletion made in one window was resurrected by
    /// the other window's later save, because that save republished a list
    /// still containing the deleted host.
    #[test]
    fn a_removal_from_a_stale_window_does_not_resurrect_another_windows_deletion() {
        with_data_dir("clew-connect-remove-merge", || {
            for h in ["a.example.com", "b.example.com", "c.example.com"] {
                upsert(host(h)).unwrap();
            }
            // Window 1's snapshot: all three.
            let stale = load();
            assert_eq!(stale.len(), 3);

            // Window 2 deletes B.
            remove("root@b.example.com", 22).unwrap();

            // Window 1 deletes A from its stale list. B stays deleted.
            let merged = remove(&stale[2].user_host(), stale[2].port).unwrap();
            assert_eq!(hosts(&merged), ["c.example.com"]);
            assert_eq!(merged, load());
        });
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
