//! Where clew reads code from: the local machine, or a remote host over SSH.
//!
//! The client is a pure renderer that speaks `clew-protocol` to a clew-server
//! process (see `server.rs`). A [`ConnTarget`] chooses which server that is — a
//! local child, or an SSH session to a remote — and is the *key* of the server
//! subscription, so changing it tears down one transport and brings up the
//! other with no other code path changing. That is what makes remote and local
//! indistinguishable to the rest of the app.

use std::path::{Path, PathBuf};

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
pub(crate) fn ssh_port(args: &[String]) -> Option<u16> {
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

/// The destination (`user@host` or `host`) of an `ssh` argument list: the
/// argument after `--` when there is one (every list clew builds has it),
/// else the last token that is not an option.
pub(crate) fn ssh_destination(args: &[String]) -> Option<&str> {
    match args.iter().position(|a| a == "--") {
        Some(i) => args.get(i + 1).map(String::as_str),
        None => args
            .iter()
            .rev()
            .find(|a| !a.starts_with('-'))
            .map(String::as_str),
    }
}

impl ConnTarget {
    /// The startup target: `CLEW_SSH` (raw `ssh` args) selects a remote for
    /// power users / tests; otherwise local. In-app connections replace this.
    ///
    /// The value is split on whitespace and handed to `ssh` as separate
    /// arguments — never to a shell. It is the user's own environment, so
    /// its options are theirs to choose (host-key policy included).
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

/// The port typed into the Connect form: surrounding whitespace ignored, empty
/// meaning the default. Anything else that is not 1–65535 is an error to show
/// — it used to become port 22 silently, connecting to a different service
/// (or machine) than the one the user named.
pub fn parse_port(text: &str) -> Result<u16, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(default_port());
    }
    match text.parse::<u16>() {
        Ok(port) if port != 0 => Ok(port),
        _ => Err(format!("“{text}” is not a port number (1–65535).")),
    }
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

    /// Refuse a connection whose fields could be read as something other than
    /// what they are. Every field reaches `ssh` as its own argv element, never
    /// a shell, and `ssh_args` puts `--` before the destination — so this is
    /// the second line: a clear message instead of a confusing ssh error, and
    /// no `-oProxyCommand=…` typed into a field is ever an option anywhere.
    pub fn validate(&self) -> Result<(), String> {
        let plain = |what: &str, v: &str| -> Result<(), String> {
            if v.is_empty() {
                return Err(format!("The {what} is required."));
            }
            if v.starts_with('-') {
                return Err(format!("The {what} cannot start with “-”."));
            }
            if v.chars()
                .any(|c| c.is_whitespace() || c.is_control() || c == '@')
            {
                return Err(format!(
                    "The {what} cannot contain spaces, control characters or “@”."
                ));
            }
            Ok(())
        };
        plain("host", self.host.trim())?;
        plain("user", self.user.trim())?;
        if self.port == 0 {
            return Err("The port must be between 1 and 65535.".into());
        }
        let identity = self.identity.trim();
        if identity.starts_with('-') || identity.chars().any(char::is_control) {
            return Err("The identity file path is not a usable path.".into());
        }
        Ok(())
    }

    /// The `ssh` CLI arguments for this host.
    ///
    /// Host keys are checked strictly: an unknown key is refused, not
    /// accepted blindly (`accept-new` trusted whatever answered the first
    /// connection, and every later check was then made against that). The
    /// transport names the refusal and shows the key's fingerprints so the
    /// user can verify and trust the host deliberately (see
    /// `crate::server`). A connect timeout fails fast instead of hanging the
    /// transport, and one password prompt at most is attempted — clew has no
    /// terminal of its own to ask in (see `crate::server` on password and
    /// OTP hosts).
    ///
    /// The keepalives bound how long a link can be dead without anyone
    /// noticing. Without them a laptop changing networks left `ssh` happily
    /// accepting frames into a pipe that went nowhere until TCP gave up
    /// MINUTES later, and everything written in that window was lost. Three
    /// missed 15s probes fail the session in ~45s instead.
    ///
    /// `--` ends ssh's options before the destination, so no field value can
    /// be parsed as one.
    pub fn ssh_args(&self) -> Vec<String> {
        let mut args: Vec<String> = [
            "-o",
            "ConnectTimeout=10",
            "-o",
            "StrictHostKeyChecking=yes",
            "-o",
            "ServerAliveInterval=15",
            "-o",
            "ServerAliveCountMax=3",
            "-o",
            "NumberOfPasswordPrompts=1",
        ]
        .map(String::from)
        .to_vec();
        if self.port != 22 {
            args.push("-p".into());
            args.push(self.port.to_string());
        }
        if !self.identity.trim().is_empty() {
            args.push("-i".into());
            args.push(self.identity.trim().to_string());
        }
        args.push("--".into());
        args.push(format!("{}@{}", self.user.trim(), self.host.trim()));
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

/// Largest `connections.toml` clew will read. A list of hosts is a few
/// kilobytes; past this the file is not one.
const MAX_STORE_BYTES: u64 = 1024 * 1024;

/// Saved connections for display. A missing file is an empty list; a file
/// that exists but cannot be read or parsed is ALSO shown as empty — but only
/// here: [`upsert`] and [`remove`] refuse to write over it (see
/// [`load_checked`]).
pub fn load() -> Vec<SavedConnection> {
    load_checked().unwrap_or_else(|e| {
        eprintln!("[clew] {e}");
        Vec::new()
    })
}

/// Saved connections, telling "no file yet" (an empty list) apart from "a
/// file clew cannot use" (an error).
///
/// The distinction is what keeps a hand edit with a typo, a merge conflict or
/// a newer clew's format from being destroyed: this used to read all of those
/// as "no connections", and the next save wrote that empty list plus the one
/// new host over the user's file. Opened without following a symlink and
/// read under a size cap, like every other state file.
pub fn load_checked() -> Result<Vec<SavedConnection>, String> {
    use std::io::Read;
    let Some(path) = store_path() else {
        return Ok(Vec::new());
    };
    match std::fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
        Ok(_) => {}
    }
    let file = clew_core::statefile::open_plain(&path)
        .ok_or_else(|| format!("{} is not a plain file", path.display()))?;
    let mut text = String::new();
    file.take(MAX_STORE_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    if text.len() as u64 > MAX_STORE_BYTES {
        return Err(format!(
            "{} is too large to be a connection list",
            path.display()
        ));
    }
    toml::from_str::<Store>(&text)
        .map(|s| s.connections)
        .map_err(|e| format!("{} could not be parsed: {e}", path.display()))
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
/// A lock that cannot be taken (no writable data directory, a filesystem
/// without `flock`) refuses the edit rather than running it unlocked, where
/// two processes could interleave between the read and the rename and lose
/// one entry — the policy of `statefile::lock`. The atomic write already
/// rules out a torn file.
fn edit(change: impl FnOnce(&mut Vec<SavedConnection>)) -> Result<Vec<SavedConnection>, String> {
    // Poisoning only means an earlier caller panicked; the list is re-read from
    // disk here regardless, so there is no corrupt state to inherit.
    let _serialized = SAVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let path = store_path().ok_or("no data directory")?;
    let _exclusive = clew_core::statefile::lock(&path).map_err(|e| {
        format!(
            "cannot lock {}: {e} — the connection list was not changed",
            path.display()
        )
    })?;
    // A file that exists but cannot be used is left exactly as it is: writing
    // "what we could read (nothing) + this change" over it would erase every
    // host in it.
    let mut merged = load_checked()
        .map_err(|e| format!("{e} — not overwriting it; fix or remove the file, then try again"))?;
    change(&mut merged);
    save(&merged)?;
    Ok(merged)
}

/// Add (or update) one connection, de-duplicated by `user@host` + port and
/// placed first (most-recent-first ordering). Returns the merged list. A
/// connection that fails [`SavedConnection::validate`] is not saved.
pub fn upsert(conn: SavedConnection) -> Result<Vec<SavedConnection>, String> {
    conn.validate()?;
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

// ---------------------------------------------------------------------------
// Host keys: clew's own known_hosts
// ---------------------------------------------------------------------------
//
// Connections check host keys strictly (`StrictHostKeyChecking=yes`), so an
// unknown host is refused rather than trusted on first sight. Trusting one is
// an explicit decision in the Connect modal: clew shows the key's fingerprint
// (`ssh-keyscan` output hashed by `ssh-keygen -lf`, both run by absolute
// path), and "Trust" records EXACTLY that key line — in clew's own
// known_hosts under its data directory, never in `~/.ssh/known_hosts`. Every
// `ssh` clew runs then consults that file after the user's own (see
// [`known_hosts_options`]).
//
// What "the user's own" files are, which endpoint to scan, and which name to
// record the key under all come from what ssh itself will do for the target —
// `ssh -G`, parsed into [`SshSettings`] — not from the arguments as typed: a
// `~/.ssh/config` entry can rename the host, move the port, set a
// `HostKeyAlias`, route through a jump host, or list other known_hosts files.

/// How `ssh` will connect for a target, as `ssh -G` resolves it from the
/// command line AND the user's ssh configuration (`Host` / `Match` blocks,
/// `HostName`, `Port`, …).
///
/// The literal arguments are not enough to reason about host keys. A
/// `Host prod` block with `HostName 10.0.0.5` and `Port 2200` makes ssh look
/// the key up as `[10.0.0.5]:2200`, a `HostKeyAlias` makes it look it up by
/// the alias, and a `ProxyJump` means the host is not reachable from here at
/// all. Scanning and recording by the typed name got all three wrong — a key
/// trusted in clew was never found, and the Trust prompt came back forever —
/// and naming OpenSSH's default known_hosts files in place of the user's
/// configured ones made a host known only in, say, `~/.ssh/known_hosts_work`
/// look unknown, turning a key that CHANGED there into a trust prompt instead
/// of a refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshSettings {
    /// The host ssh connects to (`HostName`, else the destination's host).
    pub hostname: String,
    /// The port it connects to.
    pub port: u16,
    /// `HostKeyAlias`: the name ssh records and looks the host key up by.
    pub host_key_alias: Option<String>,
    /// `ProxyJump` / `ProxyCommand`, as `(option, value)`, when the connection
    /// goes through one: the host is then not reachable from here directly.
    pub proxy: Option<(&'static str, String)>,
    /// `UserKnownHostsFile` as configured, when ssh's printout of it could be
    /// read back into exactly these paths (see [`split_known_hosts_list`]);
    /// `None` when it could not, and then clew leaves the setting alone.
    pub user_known_hosts: Option<Vec<String>>,
}

impl SshSettings {
    /// Parse `ssh -G` output (lower-case `key value` lines). `exists` says
    /// whether a path names an existing file; it is how a known_hosts path
    /// with a space in it is told apart from two paths.
    pub fn parse(text: &str, exists: impl Fn(&str) -> bool) -> Result<SshSettings, String> {
        let (mut hostname, mut port, mut alias, mut proxy, mut known) =
            (None, None, None, None, None);
        for line in text.lines() {
            let Some((key, value)) = line.trim_end_matches('\r').split_once(' ') else {
                continue;
            };
            let trimmed = value.trim();
            match key {
                "hostname" => hostname = Some(trimmed.to_string()),
                "port" => {
                    port = Some(
                        trimmed
                            .parse::<u16>()
                            .map_err(|_| format!("ssh reported port {trimmed:?}"))?,
                    );
                }
                "hostkeyalias" if !trimmed.is_empty() => alias = Some(trimmed.to_string()),
                "proxyjump" if !matches!(trimmed, "" | "none") => {
                    proxy = Some(("ProxyJump", trimmed.to_string()));
                }
                "proxycommand" if proxy.is_none() && !matches!(trimmed, "" | "none") => {
                    proxy = Some(("ProxyCommand", trimmed.to_string()));
                }
                "userknownhostsfile" => known = Some(split_known_hosts_list(value, &exists)),
                _ => {}
            }
        }
        let hostname = hostname
            .filter(|h| !h.is_empty() && !h.chars().any(|c| c.is_whitespace() || c.is_control()))
            .ok_or("ssh reported no usable host name")?;
        Ok(SshSettings {
            hostname,
            port: port.ok_or("ssh reported no port")?,
            host_key_alias: alias,
            proxy,
            user_known_hosts: known.flatten(),
        })
    }

    /// The name ssh looks this host's key up by (and would record it under):
    /// the `HostKeyAlias` if one is set, else the host — `[host]:port` off
    /// port 22 — exactly as `ssh-keyscan` writes it (lower case; ssh matches
    /// host names case-insensitively).
    pub fn lookup_name(&self) -> String {
        match &self.host_key_alias {
            Some(alias) => alias.to_ascii_lowercase(),
            None => known_hosts_pattern(&self.hostname.to_ascii_lowercase(), Some(self.port)),
        }
    }

    /// The host and port ssh connects to directly, or `None` when it goes
    /// through a jump host or proxy command — then what answers at that
    /// address from here is not necessarily what ssh will talk to.
    pub fn direct_endpoint(&self) -> Option<(&str, u16)> {
        self.proxy
            .is_none()
            .then_some((self.hostname.as_str(), self.port))
    }
}

/// Read `ssh -G`'s `userknownhostsfile` value back into paths.
///
/// ssh prints the list joined by single spaces, unquoted, so a path with a
/// space in it comes out as two words. Words are rejoined when together they
/// name an existing file (the longest such run wins) — a file that does not
/// exist holds no keys, so reading one of those as two words changes nothing
/// ssh would find. Every path must then be absolute and survive being handed
/// back to ssh unchanged ([`repassable_path`]). `None` when the printout
/// cannot be read back with certainty; `Some(empty)` for `none`.
fn split_known_hosts_list(value: &str, exists: &impl Fn(&str) -> bool) -> Option<Vec<String>> {
    if value.trim() == "none" {
        return Some(Vec::new());
    }
    let words: Vec<&str> = value.split(' ').collect();
    let mut paths = Vec::new();
    let mut i = 0;
    while i < words.len() {
        let end = (i + 2..=words.len())
            .rev()
            .find(|&j| exists(&words[i..j].join(" ")))
            .unwrap_or(i + 1);
        paths.push(words[i..end].join(" "));
        i = end;
    }
    paths.iter().all(|p| repassable_path(p)).then_some(paths)
}

/// Whether `path` can be written, double-quoted, into an ssh option and be
/// read back as the same path: absolute, and free of what ssh's config
/// parser treats specially — `"` and `\` (quoting), `$` (`${…}` expansion)
/// and control characters. `%` is fine: it is doubled on the way out.
fn repassable_path(path: &str) -> bool {
    path.starts_with('/')
        && !path
            .chars()
            .any(|c| c.is_control() || matches!(c, '"' | '\\' | '$'))
}

/// One known_hosts path as a token of an ssh option value: quoted, with `%`
/// doubled because ssh expands `%` tokens in these paths.
fn ssh_option_path(path: &str) -> String {
    format!("\"{}\"", path.replace('%', "%%"))
}

/// Host key types clew offers for trust, most preferred first, with the name
/// `ssh-keygen -l` prints for each. One key is shown and recorded — the
/// strongest the host presents — because `ssh-keyscan` fetches each type over
/// its own connection: trusting the others on the strength of the one the
/// reader checked would trust keys nobody looked at.
const HOST_KEY_TYPES: &[(&str, &str)] = &[
    ("ssh-ed25519", "ED25519"),
    ("ecdsa-sha2-nistp256", "ECDSA"),
    ("ecdsa-sha2-nistp384", "ECDSA"),
    ("ecdsa-sha2-nistp521", "ECDSA"),
    ("rsa-sha2-512", "RSA"),
    ("rsa-sha2-256", "RSA"),
    ("ssh-rsa", "RSA"),
];

/// Largest clew known_hosts file read or written; a few hundred hosts take a
/// fraction of this.
const MAX_KNOWN_HOSTS_BYTES: u64 = 1024 * 1024;

/// A host key offered for trust: one `ssh-keyscan` line and the fingerprint
/// `ssh-keygen -lf` computed for that very line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScannedHostKey {
    /// Which connection the key was scanned for: the known_hosts host field
    /// of its destination as given — `host`, or `[host]:port` off the default
    /// port ([`ssh_host_port`] + [`known_hosts_pattern`]) — which trusting
    /// checks against the target it trusts for.
    pub host: String,
    /// The exact line trusting records: `<name> <key type> <base64 key>`,
    /// where `<name>` is what ssh looks the key up by
    /// ([`SshSettings::lookup_name`]). The same as [`Self::host`] unless the
    /// ssh configuration maps the destination (`HostName`, `Port`,
    /// `HostKeyAlias`).
    pub line: String,
    /// The key type as `ssh-keygen -l` names it (`ED25519`, `ECDSA`, `RSA`).
    pub kind: String,
    /// `SHA256:…`, the form administrators and `ssh-keygen -l` print.
    pub fingerprint: String,
}

/// The known_hosts host field for `host` on `port`: plain on the default
/// port, `[host]:port` otherwise — what `ssh-keyscan` writes and `ssh` looks
/// a host up by.
pub fn known_hosts_pattern(host: &str, port: Option<u16>) -> String {
    match port {
        Some(port) if port != 22 => format!("[{host}]:{port}"),
        _ => host.to_string(),
    }
}

/// The host (without `user@`) and port an `ssh` argument list connects to.
/// `None` for a destination that cannot be one host name (empty, or starting
/// with `-`).
pub fn ssh_host_port(args: &[String]) -> Option<(String, Option<u16>)> {
    let dest = ssh_destination(args)?;
    let host = dest.rsplit_once('@').map_or(dest, |(_, h)| h);
    if host.is_empty() || host.starts_with('-') || host.chars().any(|c| c.is_whitespace()) {
        return None;
    }
    Some((host.to_string(), ssh_port(args)))
}

/// Whether `field` names exactly one host as a known_hosts host field —
/// `name` or `[name]:port` — with none of the pattern syntax (`*`, `?`, `!`,
/// `,`), no hashed `|1|…` form and no marker: a key recorded under it applies
/// to that one name and nothing else. (`:` is allowed in a bare name for an
/// IPv6 address on port 22.)
pub(crate) fn plain_host_field(field: &str) -> bool {
    let name_ok = |name: &str| {
        !name.is_empty()
            && name.len() <= 255
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':'))
    };
    match field.strip_prefix('[') {
        Some(rest) => rest
            .split_once("]:")
            .is_some_and(|(name, port)| name_ok(name) && port.parse::<u16>().is_ok_and(|p| p != 0)),
        None => name_ok(field),
    }
}

/// Split one `ssh-keyscan` line into `(key type, base64 key)` when it is a
/// plain key line for exactly `host_field`: no marker (`@cert-authority`,
/// `@revoked`), no hashed or wildcard host, no trailing fields, a key type
/// clew offers, and a base64 key. Anything else — a banner comment, another
/// host's line — is not something to record.
pub(crate) fn parse_key_line<'a>(line: &'a str, host_field: &str) -> Option<(&'a str, &'a str)> {
    if line.chars().any(char::is_control) {
        return None;
    }
    let mut fields = line.split(' ');
    let (host, kind, key) = (fields.next()?, fields.next()?, fields.next()?);
    if fields.next().is_some() || host != host_field {
        return None;
    }
    HOST_KEY_TYPES.iter().find(|(t, _)| *t == kind)?;
    let base64 = |c: char| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=');
    (16..=16 * 1024).contains(&key.len()).then_some(())?;
    key.chars().all(base64).then_some((kind, key))
}

/// The key lines in `ssh-keyscan` output for `host_field`, most preferred type
/// first (see [`HOST_KEY_TYPES`]), without duplicates.
pub(crate) fn preferred_key_lines(keyscan_stdout: &str, host_field: &str) -> Vec<String> {
    let mut found: Vec<(usize, String)> = keyscan_stdout
        .lines()
        .map(str::trim_end)
        .filter_map(|line| {
            let (kind, _) = parse_key_line(line, host_field)?;
            let rank = HOST_KEY_TYPES.iter().position(|(t, _)| *t == kind)?;
            Some((rank, line.to_string()))
        })
        .collect();
    found.sort();
    found.dedup_by(|a, b| a.1 == b.1);
    found.into_iter().map(|(_, line)| line).collect()
}

/// The `(kind, fingerprint)` in `ssh-keygen -l -f -` output for ONE key line
/// of type `key_type` — `256 SHA256:… host (ED25519)`. `None` unless there is
/// exactly one such line and its kind is the one that key type has, so a
/// fingerprint can never be shown for a different key than the one recorded.
pub(crate) fn fingerprint_of(keygen_stdout: &str, key_type: &str) -> Option<(String, String)> {
    let expected = HOST_KEY_TYPES.iter().find(|(t, _)| *t == key_type)?.1;
    let mut lines = keygen_stdout.lines().filter(|l| !l.trim().is_empty());
    let line = lines.next()?;
    if lines.next().is_some() {
        return None;
    }
    let mut fields = line.split_whitespace();
    let _bits: u32 = fields.next()?.parse().ok()?;
    let hash = fields.next().filter(|h| {
        h.strip_prefix("SHA256:").is_some_and(|b| {
            !b.is_empty()
                && b.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '='))
        })
    })?;
    let kind = line.rsplit_once('(')?.1.strip_suffix(')')?.trim();
    (kind == expected).then(|| (kind.to_string(), hash.to_string()))
}

/// clew's own known_hosts: `<data dir>/known_hosts`. `None` without a data
/// directory, or when the path could not be handed to `ssh` intact (see
/// [`ssh_config_safe`]) — then no host can be trusted from inside clew.
pub fn known_hosts_path() -> Option<PathBuf> {
    let path = clew_core::lsp::store::data_root()?.join("known_hosts");
    ssh_config_safe(&path).then_some(path)
}

/// Whether `path` survives being written, double-quoted, into an `ssh -o`
/// option: ssh's config parser takes `"` and `\` as quoting, expands `%` and
/// `${…}` in known-hosts paths, and `~` at the start. Spaces are fine inside
/// the quotes ("Application Support").
fn ssh_config_safe(path: &Path) -> bool {
    path.is_absolute()
        && path.to_str().is_some_and(|p| {
            !p.chars()
                .any(|c| c.is_control() || matches!(c, '"' | '\\' | '%' | '$'))
        })
}

/// Whether an `ssh` argument list sets option `name` itself (`-o Name=…`,
/// `-oName=…`, `-o "Name …"`; names are case-insensitive).
fn sets_option(args: &[String], name: &str) -> bool {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--" {
            break;
        }
        let Some(rest) = a.strip_prefix("-o") else {
            continue;
        };
        let opt = if rest.is_empty() {
            match it.next() {
                Some(next) => next.as_str(),
                None => break,
            }
        } else {
            rest
        };
        let key = opt
            .split(|c: char| c == '=' || c.is_whitespace())
            .next()
            .unwrap_or("");
        if key.eq_ignore_ascii_case(name) {
            return true;
        }
    }
    false
}

/// The known-hosts options for every `ssh` clew runs for one target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownHostsOptions {
    /// The `-o …` arguments, in order.
    pub args: Vec<String>,
    /// clew's own known_hosts when these options make ssh consult it — the
    /// condition for offering to trust a host from inside clew (a key recorded
    /// in a file ssh does not read would bring the same prompt back forever).
    pub clew_file: Option<PathBuf>,
}

/// The `-o` options every `ssh` clew runs for `args` carries, so that a key
/// trusted in clew ([`trust_host_key`]) is honored and nothing else changes:
///
/// - `UserKnownHostsFile` is the user's OWN list — whatever their ssh
///   configuration resolves to for this host (`settings`, from `ssh -G`) —
///   with clew's file appended. A host the user already trusts stays trusted,
///   a key that CHANGED against any of their files is still refused as
///   changed, and clew's file only adds hosts. ssh never writes to it: with
///   strict checking it adds no new host, and
/// - `UpdateHostKeys=no` / `CheckHostIP=no` keep it from writing any
///   known_hosts file on clew's behalf (key rotation, IP entries).
///
/// Without `settings`, or when the user's list could not be read back
/// unambiguously, `UserKnownHostsFile` is not passed at all — the user's
/// configuration stays exactly in effect, and no host can be trusted from
/// inside clew for this target (`clew_file` is `None`). An option the
/// target's own arguments already set (a `CLEW_SSH` target is the user's raw
/// ssh arguments) is left to them too: ssh takes the first value it sees, and
/// these come first.
pub fn known_hosts_options(args: &[String], settings: Option<&SshSettings>) -> KnownHostsOptions {
    let mut out = Vec::new();
    let mut clew_file = None;
    if !sets_option(args, "UserKnownHostsFile")
        && let Some(user) = settings.and_then(|s| s.user_known_hosts.as_ref())
        && let Some(path) = known_hosts_path()
    {
        let mut list: Vec<String> = user.iter().map(|p| ssh_option_path(p)).collect();
        if !user.iter().any(|p| Path::new(p) == path) {
            list.push(ssh_option_path(&path.to_string_lossy()));
        }
        out.push("-o".to_string());
        out.push(format!("UserKnownHostsFile={}", list.join(" ")));
        clew_file = Some(path);
    }
    for (name, value) in [("UpdateHostKeys", "no"), ("CheckHostIP", "no")] {
        if !sets_option(args, name) {
            out.push("-o".to_string());
            out.push(format!("{name}={value}"));
        }
    }
    KnownHostsOptions {
        args: out,
        clew_file,
    }
}

/// Serializes the read-modify-write of clew's known_hosts across this
/// process's windows (the file lock below covers other processes).
static KNOWN_HOSTS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The names the transport offered a key under for a connection whose
/// destination the ssh configuration maps elsewhere: `(host field of the
/// destination as given, name ssh looks it up by)`. [`trust_host_key`]
/// records a key only under the connection's own host field or a name
/// offered for it here — so however a key line naming some OTHER host
/// reached the app, it is never recorded for this one.
static OFFERED_NAMES: std::sync::Mutex<Vec<(String, String)>> = std::sync::Mutex::new(Vec::new());

/// Most mapped names remembered; the oldest go first (a prompt that old has
/// long been replaced).
const MAX_OFFERED_NAMES: usize = 256;

/// Note that a key for the connection to `for_host` is being offered under
/// `recorded_as` (see [`OFFERED_NAMES`]). Nothing to note when they agree.
pub(crate) fn offer_name(for_host: &str, recorded_as: &str) {
    if for_host == recorded_as {
        return;
    }
    let mut names = OFFERED_NAMES.lock().unwrap_or_else(|e| e.into_inner());
    let pair = (for_host.to_string(), recorded_as.to_string());
    names.retain(|p| *p != pair);
    names.push(pair);
    let excess = names.len().saturating_sub(MAX_OFFERED_NAMES);
    names.drain(..excess);
}

/// Whether a key for the connection to `for_host` may be recorded under
/// `recorded_as`: its own host field, or a name the transport offered for it.
fn may_record_as(for_host: &str, recorded_as: &str) -> bool {
    for_host == recorded_as
        || OFFERED_NAMES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|(f, r)| f == for_host && r == recorded_as)
}

/// Record `key` in clew's known_hosts for the connection whose destination's
/// host field is `host_field` — the target being retried — and return the
/// file's path.
///
/// Exactly the line that was shown is written, after checking it again: the
/// key must have been scanned for `host_field`'s connection, and its line
/// must still be a plain key line of a type clew offers, recorded under ONE
/// literal host name ([`plain_host_field`]) — the name ssh looks the host up
/// by: `host_field` itself, or the name the transport resolved it to from
/// the ssh configuration and offered the key under ([`offer_name`]). A line
/// naming any other host is refused. Appending is idempotent, atomic (temp +
/// rename) and done under the store's lock; an existing file that cannot be
/// read is never overwritten. `~/.ssh/known_hosts` is never touched.
pub fn trust_host_key(key: &ScannedHostKey, host_field: &str) -> Result<PathBuf, String> {
    let recorded_as = key.line.split(' ').next().unwrap_or_default();
    if key.host != host_field
        || !plain_host_field(recorded_as)
        || !may_record_as(host_field, recorded_as)
        || parse_key_line(&key.line, recorded_as).is_none()
    {
        return Err(format!(
            "the key offered is not a key line for {host_field}; nothing was recorded"
        ));
    }
    edit_known_hosts(|current| {
        if current.lines().any(|l| l.trim_end() == key.line) {
            return None;
        }
        let mut next = current.to_string();
        if !next.is_empty() && !next.ends_with('\n') {
            next.push('\n');
        }
        next.push_str(&key.line);
        next.push('\n');
        Some(next)
    })
}

/// Forget every key clew recorded for `host_field` in its own known_hosts,
/// returning how many lines went.
///
/// This is the way out of a CHANGED-key refusal whose key on record is one
/// trusted in clew (the transport names it: `Failure::forget_host`). After
/// forgetting, the next connection sees an unknown key and the Connect modal
/// shows the NEW key's fingerprint to be checked before it is trusted —
/// nothing is ever trusted by this call itself. Only lines whose host field
/// is exactly `host_field` go (compared case-insensitively, as ssh matches
/// names); those are the only kind [`trust_host_key`] writes. The same locks
/// and atomic write as trusting; a file that cannot be read is left untouched,
/// and `~/.ssh/known_hosts` is never touched.
pub fn forget_host_key(host_field: &str) -> Result<usize, String> {
    if !plain_host_field(host_field) {
        return Err(format!("{host_field:?} is not a single host name"));
    }
    let mut removed = 0;
    edit_known_hosts(|current| {
        let kept: Vec<&str> = current
            .lines()
            .filter(|line| {
                let field = line.split(' ').next().unwrap_or_default();
                let forget = field.eq_ignore_ascii_case(host_field);
                removed += usize::from(forget);
                !forget
            })
            .collect();
        (removed > 0).then(|| {
            let mut next = kept.join("\n");
            if !next.is_empty() {
                next.push('\n');
            }
            next
        })
    })?;
    Ok(removed)
}

/// One read-modify-write of clew's known_hosts, under both locks (this
/// process's windows, and other processes): `change` gets the current text
/// (empty when there is no file yet) and returns the text to write — atomically
/// — or `None` to leave the file as it is. Returns the file's path.
fn edit_known_hosts(change: impl FnOnce(&str) -> Option<String>) -> Result<PathBuf, String> {
    let path = known_hosts_path()
        .ok_or("clew has no data directory it can hand to ssh, so it cannot remember host keys")?;
    let _serialized = KNOWN_HOSTS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _exclusive = clew_core::statefile::lock(&path)
        .map_err(|e| format!("cannot lock {}: {e}", path.display()))?;
    let current = clew_core::statefile::read_capped_checked(&path, MAX_KNOWN_HOSTS_BYTES)
        .map_err(|e| {
            format!(
                "{} could not be read ({e}) — left untouched",
                path.display()
            )
        })?
        .unwrap_or_default();
    if let Some(next) = change(&current) {
        clew_core::statefile::write_atomic(&path, next.as_bytes())
            .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    }
    Ok(path)
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

    /// The destination is always behind `--`, so even a value that slipped
    /// past validation could never be parsed by ssh as an option.
    #[test]
    fn the_destination_is_never_an_option() {
        let mut c = conn(22, "");
        c.host = "-oProxyCommand=touch /tmp/pwned".into();
        let a = c.ssh_args();
        let dd = a
            .iter()
            .position(|s| s == "--")
            .expect("an end-of-options marker");
        assert_eq!(dd, a.len() - 2, "`--` sits right before the destination");
        assert_eq!(ssh_destination(&a), Some(a[a.len() - 1].as_str()));
        // And validation names the problem before ssh ever runs.
        assert!(c.validate().unwrap_err().contains("cannot start with"));
    }

    /// Unknown host keys are refused rather than accepted blindly.
    #[test]
    fn host_keys_are_checked_strictly() {
        let a = conn(22, "").ssh_args();
        assert!(
            a.windows(2)
                .any(|w| w == ["-o", "StrictHostKeyChecking=yes"])
        );
        assert!(!a.iter().any(|s| s.contains("accept-new")));
    }

    /// D2-17: a saved connection asks for a password at most once — clew has
    /// no terminal of its own, so a second prompt could only hang — fails fast
    /// on an unreachable host, and notices a dead link within ~45 s.
    #[test]
    fn a_saved_connection_prompts_once_and_fails_fast() {
        let a = conn(22, "").ssh_args();
        for option in [
            "NumberOfPasswordPrompts=1",
            "ConnectTimeout=10",
            "ServerAliveInterval=15",
            "ServerAliveCountMax=3",
        ] {
            assert!(
                a.windows(2).any(|w| w == ["-o", option]),
                "{option} missing from {a:?}"
            );
        }
    }

    #[test]
    fn validation_refuses_fields_ssh_could_misread() {
        assert!(conn(22, "").validate().is_ok());
        let with = |f: &dyn Fn(&mut SavedConnection)| {
            let mut c = conn(22, "/keys/id");
            f(&mut c);
            c.validate()
        };
        assert!(with(&|c| c.user = "-lroot".into()).is_err());
        assert!(with(&|c| c.user = "ro ot".into()).is_err());
        assert!(with(&|c| c.host = "a.example.com b".into()).is_err());
        assert!(with(&|c| c.host = "user@example.com".into()).is_err());
        assert!(with(&|c| c.host = String::new()).is_err());
        assert!(with(&|c| c.port = 0).is_err());
        assert!(with(&|c| c.identity = "-F/etc/passwd".into()).is_err());
    }

    /// A port with stray whitespace used to fail to parse and silently become
    /// 22 — a connection to a different service than the one typed.
    #[test]
    fn ports_are_trimmed_and_garbage_is_an_error() {
        assert_eq!(parse_port(" 2222 "), Ok(2222));
        assert_eq!(parse_port(""), Ok(22));
        assert_eq!(parse_port("  "), Ok(22));
        assert!(parse_port("22a").is_err());
        assert!(parse_port("0").is_err());
        assert!(parse_port("70000").is_err());
        assert!(parse_port("-22").is_err());
    }

    /// A connections.toml clew cannot parse (a typo, a merge conflict, a newer
    /// format) is not a list to overwrite: saving a host used to replace the
    /// whole file with that one host.
    #[test]
    fn an_unparseable_store_is_never_overwritten() {
        with_data_dir("clew-connect-unparseable", || {
            let path = store_path().unwrap();
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let garbage = "[[connection]]\nhost = \"a.example.com\"\n<<<<<<< HEAD\n";
            std::fs::write(&path, garbage).unwrap();

            assert!(load_checked().is_err());
            assert!(load().is_empty(), "display still degrades to empty");
            let err = upsert(host("b.example.com")).unwrap_err();
            assert!(err.contains("not overwriting"), "{err}");
            assert!(remove("root@a.example.com", 22).is_err());
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                garbage,
                "the user's file must be left exactly as it was"
            );
        });
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

    /// Point the store at a fresh directory of this test's own for the
    /// duration of `f`, under the env lock; the variable is restored to the
    /// suite's default and the directory removed however `f` ends — a failing
    /// assertion included.
    fn with_data_dir<T>(name: &str, f: impl FnOnce() -> T) -> T {
        let dir = crate::app::tests::test_dir(name);
        std::fs::create_dir_all(&dir).unwrap();
        let _env = crate::app::tests::data_dir_override(&dir);
        f()
    }

    /// A store whose lock cannot be taken is not edited unlocked: the edit is
    /// refused and the file left as it was. Running it without the lock (as
    /// this edit once did when locking failed) let a second clew process
    /// interleave between the read and the rename and lose an entry.
    #[test]
    fn an_edit_that_cannot_lock_the_store_changes_nothing() {
        use std::os::unix::fs::PermissionsExt;
        with_data_dir("clew-connect-unlockable", || {
            let path = store_path().unwrap();
            let dir = path.parent().unwrap().to_path_buf();
            save(&[host("a.example.com")]).unwrap();
            let before = std::fs::read_to_string(&path).unwrap();
            // No new file can be made in the data directory, so neither can
            // the lock file beside the store.
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
            let privileged = std::fs::File::create(dir.join("probe")).is_ok();
            let result = upsert(host("b.example.com"));
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
            if privileged {
                // Permissions do not bind this user (root): nothing to show.
                return;
            }
            let err = result.unwrap_err();
            assert!(err.contains("cannot lock"), "{err}");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        });
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
mod known_hosts_tests {
    use super::*;

    const ED25519: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIEd25519KeyForTestsOnly0000";
    const RSA: &str = "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABgQCtestonly00";

    fn key(host: &str, rest: &str) -> ScannedHostKey {
        ScannedHostKey {
            host: host.into(),
            line: format!("{host} {rest}"),
            kind: "ED25519".into(),
            fingerprint: "SHA256:Ed25519PrintForTests".into(),
        }
    }

    /// Run `f` with `CLEW_DATA_DIR` at a fresh directory of this test's own
    /// (with a space in its path, like macOS's "Application Support"),
    /// restored afterwards however `f` ends; the directory goes with the test.
    fn isolated<T>(name: &str, f: impl FnOnce(&Path) -> T) -> T {
        let data = crate::app::tests::test_dir(&format!("kh-{name}")).join("data dir");
        std::fs::create_dir_all(&data).unwrap();
        let _env = crate::app::tests::data_dir_override(&data);
        f(&data)
    }

    #[test]
    fn the_host_field_is_what_ssh_looks_the_host_up_by() {
        assert_eq!(known_hosts_pattern("example.com", None), "example.com");
        assert_eq!(known_hosts_pattern("example.com", Some(22)), "example.com");
        assert_eq!(
            known_hosts_pattern("example.com", Some(2222)),
            "[example.com]:2222"
        );
        let mut c = SavedConnection {
            name: String::new(),
            host: "example.com".into(),
            user: "root".into(),
            port: 2222,
            identity: String::new(),
            send_ai_keys: false,
        };
        assert_eq!(
            ssh_host_port(&c.ssh_args()),
            Some(("example.com".into(), Some(2222)))
        );
        c.port = 22;
        assert_eq!(
            ssh_host_port(&c.ssh_args()),
            Some(("example.com".into(), None))
        );
    }

    /// Only a plain key line for exactly the scanned host is ever offered or
    /// recorded: markers, comments, other or wildcard hosts, extra fields,
    /// unknown key types and non-base64 keys are all refused.
    #[test]
    fn only_a_plain_key_line_for_the_host_is_accepted() {
        let host = "[example.com]:2222";
        let good = format!("{host} {ED25519}");
        assert_eq!(
            parse_key_line(&good, host),
            Some(("ssh-ed25519", ED25519.split(' ').nth(1).unwrap()))
        );
        for bad in [
            format!("@cert-authority {host} {ED25519}"),
            format!("@revoked {host} {ED25519}"),
            format!("# {host} SSH-2.0-OpenSSH_9.6"),
            format!("[other.example]:2222 {ED25519}"),
            format!("* {ED25519}"),
            format!("example.com {ED25519}"),
            format!("{host} {ED25519} trailing-comment"),
            format!("{host} ssh-dss AAAAB3NzaC1kc3MAAACBAtestonly00"),
            format!("{host} ssh-ed25519 not*base64*at*all*here"),
            format!("{host} ssh-ed25519 AAAA"),
            format!("{host} {ED25519}\n{host} {RSA}"),
            format!("{host}\t{ED25519}"),
        ] {
            assert_eq!(parse_key_line(&bad, host), None, "{bad:?}");
        }
    }

    #[test]
    fn the_strongest_key_is_preferred_and_duplicates_are_dropped() {
        let host = "example.com";
        let out = format!(
            "# example.com:22 SSH-2.0-OpenSSH_9.6\n{host} {RSA}\n{host} {ED25519}\n\
             {host} {ED25519}\nother.example {ED25519}\n"
        );
        assert_eq!(
            preferred_key_lines(&out, host),
            [format!("{host} {ED25519}"), format!("{host} {RSA}")]
        );
        assert!(preferred_key_lines("", host).is_empty());
    }

    /// The format `ssh-keygen -l -f -` prints for one known_hosts line (as
    /// captured from OpenSSH 10.2): the fingerprint is taken only when the
    /// kind matches the key type asked about and it is the only line.
    #[test]
    fn a_fingerprint_is_read_only_for_the_key_it_belongs_to() {
        let out =
            "256 SHA256:wSQgQgtZNZ3Rs1moOyS5Juo2z8Rxy9LNnsnySSo8Iak [example.com]:2222 (ED25519)\n";
        assert_eq!(
            fingerprint_of(out, "ssh-ed25519"),
            Some((
                "ED25519".into(),
                "SHA256:wSQgQgtZNZ3Rs1moOyS5Juo2z8Rxy9LNnsnySSo8Iak".into()
            ))
        );
        assert_eq!(fingerprint_of(out, "ssh-rsa"), None, "another key type");
        assert_eq!(
            fingerprint_of(
                &format!("{out}3072 SHA256:abc example.com (RSA)\n"),
                "ssh-ed25519"
            ),
            None,
            "more than one key: which one would be shown?"
        );
        assert_eq!(
            fingerprint_of("(stdin) is not a public key file.\n", "ssh-ed25519"),
            None
        );
        assert_eq!(
            fingerprint_of("256 MD5:aa:bb example.com (ED25519)\n", "ssh-ed25519"),
            None
        );
        assert_eq!(
            fingerprint_of("3072 SHA256:Rsa example.com (RSA)\n", "rsa-sha2-512"),
            Some(("RSA".into(), "SHA256:Rsa".into()))
        );
    }

    /// `ssh -G` output for a host `~/.ssh/config` maps elsewhere, as
    /// OpenSSH 10.2 prints it (trimmed to the lines that matter plus two that
    /// do not).
    const G_MAPPED: &str = "user me\nhostname real.example.com\nport 2200\n\
        hostkeyalias KeyAlias\nuserknownhostsfile /Users/me/.ssh/known_hosts_work /Users/me/.ssh/known_hosts\n\
        stricthostkeychecking true\nproxyjump jump@bastion:2201\n";

    fn settings(known: &[&str]) -> SshSettings {
        SshSettings {
            hostname: "example.com".into(),
            port: 22,
            host_key_alias: None,
            proxy: None,
            user_known_hosts: Some(known.iter().map(|p| p.to_string()).collect()),
        }
    }

    /// What `ssh -G` resolved is read back field by field: where ssh
    /// connects, the name it looks the key up by, whether a jump host is in
    /// the way, and the user's own known_hosts files.
    #[test]
    fn ssh_g_output_is_read_back_into_the_settings_ssh_will_use() {
        let s = SshSettings::parse(G_MAPPED, |_| false).unwrap();
        assert_eq!(s.hostname, "real.example.com");
        assert_eq!(s.port, 2200);
        assert_eq!(s.lookup_name(), "keyalias", "an alias is what ssh looks up");
        assert_eq!(s.proxy, Some(("ProxyJump", "jump@bastion:2201".into())));
        assert_eq!(s.direct_endpoint(), None, "a jump host is in the way");
        assert_eq!(
            s.user_known_hosts.as_deref(),
            Some(
                &[
                    "/Users/me/.ssh/known_hosts_work".to_string(),
                    "/Users/me/.ssh/known_hosts".into()
                ][..]
            )
        );

        let plain = SshSettings::parse(
            "hostname Example.COM\nport 2222\nproxycommand none\nuserknownhostsfile none\n",
            |_| false,
        )
        .unwrap();
        assert_eq!(plain.lookup_name(), "[example.com]:2222");
        assert_eq!(plain.direct_endpoint(), Some(("Example.COM", 2222)));
        assert_eq!(
            plain.user_known_hosts,
            Some(Vec::new()),
            "`none` is no file"
        );
        let default_port = SshSettings::parse("hostname h\nport 22\n", |_| false).unwrap();
        assert_eq!(default_port.lookup_name(), "h");
        assert_eq!(default_port.user_known_hosts, None);

        for broken in ["port 22\n", "hostname h\n", "hostname h\nport 99999\n", ""] {
            assert!(SshSettings::parse(broken, |_| false).is_err(), "{broken:?}");
        }
    }

    /// ssh prints the known_hosts list unquoted, so a path with a space in it
    /// arrives as two words. An existing file is found whole; what cannot be
    /// read back for certain (or handed back to ssh unchanged) is not guessed.
    #[test]
    fn a_known_hosts_path_with_a_space_is_read_back_whole() {
        let exists = |p: &str| p == "/Users/me/Library/Application Support/kh";
        assert_eq!(
            split_known_hosts_list(
                "/Users/me/.ssh/known_hosts /Users/me/Library/Application Support/kh /x",
                &exists
            ),
            Some(vec![
                "/Users/me/.ssh/known_hosts".to_string(),
                "/Users/me/Library/Application Support/kh".into(),
                "/x".into(),
            ])
        );
        // Two words that name no file: the second is not a path by itself.
        assert_eq!(
            split_known_hosts_list("/tmp/with space/kh", &|_| false),
            None
        );
        for unsafe_path in ["/a\"b", "/a\\b", "/${HOME}/kh", "relative/kh", "/a\u{7}b"] {
            assert_eq!(
                split_known_hosts_list(unsafe_path, &|_| false),
                None,
                "{unsafe_path:?}"
            );
        }
        assert_eq!(
            split_known_hosts_list("/100%/kh", &|_| false),
            Some(vec!["/100%/kh".into()]),
            "`%` is handed back doubled"
        );
    }

    /// Every ssh clew runs consults the user's OWN known_hosts files — the
    /// ones their configuration names for this host, not OpenSSH's defaults —
    /// with clew's appended (all quoted: the data directory has a space in it
    /// on macOS, and `%` doubled), and asks ssh to write none of them.
    #[test]
    fn clews_known_hosts_is_appended_to_the_users_own_list() {
        let checked = isolated("options", |data| {
            let args = vec!["--".to_string(), "root@example.com".to_string()];
            let user = settings(&["/Users/me/.ssh/known_hosts_work", "/Users/me/100%/kh"]);
            let opts = known_hosts_options(&args, Some(&user));
            let clew = data.join("known_hosts");
            assert_eq!(
                opts.args,
                [
                    "-o".to_string(),
                    format!(
                        "UserKnownHostsFile=\"/Users/me/.ssh/known_hosts_work\" \
                         \"/Users/me/100%%/kh\" \"{}\"",
                        clew.display()
                    ),
                    "-o".into(),
                    "UpdateHostKeys=no".into(),
                    "-o".into(),
                    "CheckHostIP=no".into(),
                ]
            );
            assert_eq!(opts.clew_file.as_deref(), Some(clew.as_path()));

            // Already in the user's list: not named twice.
            let listed = settings(&[clew.to_str().unwrap()]);
            let opts = known_hosts_options(&args, Some(&listed));
            assert_eq!(opts.args[1].matches("known_hosts\"").count(), 1);
            true
        });
        assert!(checked);
    }

    /// Without a readable configuration the user's known_hosts setting is
    /// left exactly in effect — never replaced by a guess — and no host can
    /// be trusted from inside clew (a key recorded where ssh does not look
    /// would bring the prompt back forever). A target that sets any of these
    /// options itself (a `CLEW_SSH` target is the user's own arguments) keeps
    /// its own value.
    #[test]
    fn the_users_known_hosts_setting_is_never_replaced_by_a_guess() {
        let checked = isolated("options-fallback", |_| {
            let args = vec!["--".to_string(), "root@example.com".to_string()];
            let fallback = [
                "-o".to_string(),
                "UpdateHostKeys=no".into(),
                "-o".into(),
                "CheckHostIP=no".into(),
            ];
            for resolved in [
                None,
                Some(&SshSettings {
                    user_known_hosts: None,
                    ..settings(&[])
                }),
            ] {
                let opts = known_hosts_options(&args, resolved);
                assert_eq!(opts.args, fallback);
                assert_eq!(opts.clew_file, None);
            }
            let own: Vec<String> = [
                "-o",
                "UserKnownHostsFile=/etc/mine",
                "-oupdatehostkeys=yes",
                "root@example.com",
            ]
            .map(String::from)
            .to_vec();
            let opts = known_hosts_options(&own, Some(&settings(&["/etc/mine"])));
            assert_eq!(opts.args, ["-o", "CheckHostIP=no"]);
            assert_eq!(opts.clew_file, None);
            true
        });
        assert!(checked);
    }

    /// A data directory whose path ssh's config parser would rewrite (`%`
    /// tokens, `${…}`, quotes) is never handed to it: no key can be trusted
    /// from inside clew then, rather than one landing in a different file.
    #[test]
    fn an_unsafe_data_dir_path_is_never_passed_to_ssh() {
        for bad in [
            "/tmp/a%hb",
            "/tmp/${HOME}",
            "/tmp/q\"uote",
            "/tmp/back\\slash",
            "relative",
        ] {
            assert!(!ssh_config_safe(Path::new(bad)), "{bad}");
        }
        assert!(ssh_config_safe(Path::new(
            "/Users/me/Library/Application Support/clew/known_hosts"
        )));
    }

    /// Trusting writes EXACTLY the key line shown, into clew's own
    /// known_hosts under its data directory — once, keeping what is already
    /// there — and nothing else anywhere (so never `~/.ssh/known_hosts`). A
    /// key for another host, or a line that is not a key line, is refused
    /// without writing anything.
    #[test]
    fn trusting_records_exactly_the_key_shown_in_clews_own_file() {
        let host = "[example.com]:2222";
        let (file, written) = isolated("trust", |data| {
            let path = data.join("known_hosts");
            std::fs::write(
                &path,
                "other.example ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOtherHostKey0000",
            )
            .unwrap();
            let k = key(host, ED25519);
            assert_eq!(trust_host_key(&k, host).unwrap(), path);
            assert_eq!(trust_host_key(&k, host).unwrap(), path, "idempotent");

            assert!(trust_host_key(&k, "example.com").is_err(), "another host");
            let mut forged = k.clone();
            forged.line = format!("@cert-authority * {ED25519}");
            assert!(trust_host_key(&forged, host).is_err(), "not a key line");
            let mut wrong_host = key("[evil.example]:2222", ED25519);
            wrong_host.host = host.into();
            assert!(
                trust_host_key(&wrong_host, host).is_err(),
                "line for another host"
            );
            // What exists in the data directory now: the file and its lock.
            let mut written: Vec<String> = std::fs::read_dir(data)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            written.sort();
            (std::fs::read_to_string(&path).unwrap(), written)
        });
        assert_eq!(
            file,
            format!(
                "other.example ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOtherHostKey0000\n{host} {ED25519}\n"
            )
        );
        assert_eq!(written, [".known_hosts.lock", "known_hosts"]);
    }

    /// A known_hosts clew cannot read (here a symlink planted at its name) is
    /// never overwritten: the trust fails and the link is left as it was.
    #[test]
    fn an_unreadable_known_hosts_is_never_overwritten() {
        let (result, still_link) = isolated("unreadable", |data| {
            let target = data.join("elsewhere");
            std::fs::write(&target, "keep me\n").unwrap();
            std::os::unix::fs::symlink(&target, data.join("known_hosts")).unwrap();
            let result = trust_host_key(&key("example.com", ED25519), "example.com");
            let meta = std::fs::symlink_metadata(data.join("known_hosts")).unwrap();
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep me\n");
            (result, meta.file_type().is_symlink())
        });
        assert!(result.is_err());
        assert!(still_link);
    }

    /// The Trust-loop fix, at the recording end: a host `~/.ssh/config` maps
    /// (`HostName`/`Port`, or a `HostKeyAlias`) is recorded under the name
    /// ssh looks it up by — the one the transport offered the key under for
    /// that connection. Any other name is refused, and so is anything that
    /// is not exactly one host: never a pattern that would vouch for others.
    #[test]
    fn a_mapped_host_is_recorded_under_the_name_ssh_looks_up() {
        let file = isolated("trust-mapped", |data| {
            let mut mapped = key("prod-trust-test", ED25519);
            mapped.line = format!("[10.0.0.5]:2200 {ED25519}");
            assert!(
                trust_host_key(&mapped, "prod-trust-test").is_err(),
                "a name the transport never offered for this connection"
            );
            offer_name("prod-trust-test", "[10.0.0.5]:2200");
            trust_host_key(&mapped, "prod-trust-test").unwrap();
            let mut aliased = key("[alias-trust-test]:2222", ED25519);
            aliased.line = format!("keyalias {ED25519}");
            offer_name("[alias-trust-test]:2222", "keyalias");
            trust_host_key(&aliased, "[alias-trust-test]:2222").unwrap();

            assert!(
                trust_host_key(&mapped, "other").is_err(),
                "another connection"
            );
            for pattern in [
                "*.example.com",
                "a.example.com,b.example.com",
                "!evil",
                "|1|abc=",
            ] {
                offer_name("prod-trust-test", pattern);
                let mut wide = key("prod-trust-test", ED25519);
                wide.line = format!("{pattern} {ED25519}");
                assert!(
                    trust_host_key(&wide, "prod-trust-test").is_err(),
                    "{pattern}"
                );
            }
            std::fs::read_to_string(data.join("known_hosts")).unwrap()
        });
        assert_eq!(
            file,
            format!("[10.0.0.5]:2200 {ED25519}\nkeyalias {ED25519}\n")
        );
    }

    #[test]
    fn only_a_single_literal_host_is_a_plain_host_field() {
        for good in [
            "example.com",
            "[example.com]:2222",
            "10.0.0.5",
            "::1",
            "[::1]:2200",
            "a_b-c",
        ] {
            assert!(plain_host_field(good), "{good}");
        }
        for bad in [
            "",
            "*.example.com",
            "host?",
            "!host",
            "a,b",
            "|1|salt|hash",
            "@revoked",
            "[example.com]",
            "[example.com]:0",
            "[example.com]:x",
            "exa mple",
        ] {
            assert!(!plain_host_field(bad), "{bad:?}");
        }
    }

    /// Forgetting removes every key clew recorded for exactly that host (as
    /// ssh matches names: case-insensitively) and nothing else; with nothing
    /// recorded it changes nothing, and it refuses a pattern outright.
    #[test]
    fn forgetting_a_host_removes_only_its_own_lines() {
        let (removed, file, again, missing) = isolated("forget", |data| {
            let path = data.join("known_hosts");
            std::fs::write(
                &path,
                format!(
                    "# kept comment\n[example.com]:2222 {ED25519}\nother.example {ED25519}\n\
                     [Example.COM]:2222 {RSA}\n[example.com]:22222 {ED25519}\n"
                ),
            )
            .unwrap();
            let removed = forget_host_key("[example.com]:2222").unwrap();
            let file = std::fs::read_to_string(&path).unwrap();
            let again = forget_host_key("[example.com]:2222").unwrap();
            assert!(forget_host_key("*.example.com").is_err());
            std::fs::remove_file(&path).unwrap();
            let missing = forget_host_key("example.com").unwrap();
            (removed, file, again, missing)
        });
        assert_eq!(removed, 2);
        assert_eq!(
            file,
            format!("# kept comment\nother.example {ED25519}\n[example.com]:22222 {ED25519}\n")
        );
        assert_eq!((again, missing), (0, 0));
    }

    /// Like trusting, forgetting never rewrites a known_hosts it cannot read.
    #[test]
    fn forgetting_never_overwrites_an_unreadable_known_hosts() {
        let (result, kept) = isolated("forget-unreadable", |data| {
            let target = data.join("elsewhere");
            std::fs::write(&target, format!("example.com {ED25519}\n")).unwrap();
            std::os::unix::fs::symlink(&target, data.join("known_hosts")).unwrap();
            let result = forget_host_key("example.com");
            (result, std::fs::read_to_string(&target).unwrap())
        });
        assert!(result.is_err());
        assert_eq!(kept, format!("example.com {ED25519}\n"));
    }
}

#[cfg(test)]
mod env_target_tests {
    use super::*;

    /// The REAL `ConnTarget::from_env`, with `CLEW_SSH` set to `ssh` for the
    /// duration of the call. (This used to re-implement the parsing in the
    /// test, so it asserted on a copy and could pass while `from_env`
    /// regressed.) The variable is process-global, hence the lock, and it is
    /// restored afterwards.
    fn from(ssh: &str) -> ConnTarget {
        let _env = crate::app::tests::EnvVars::new().set("CLEW_SSH", ssh);
        ConnTarget::from_env()
    }

    #[test]
    fn clew_ssh_selects_a_remote_and_blank_means_local() {
        match from("-p 2222 -i /keys/id root@example.com") {
            ConnTarget::Ssh { label, args } => {
                assert_eq!(label, "root@example.com:2222");
                assert_eq!(
                    args,
                    ["-p", "2222", "-i", "/keys/id", "root@example.com"],
                    "split on whitespace into separate arguments, never a shell string"
                );
            }
            other => panic!("expected an SSH target, got {other:?}"),
        }
        assert_eq!(from("   "), ConnTarget::Local);
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
