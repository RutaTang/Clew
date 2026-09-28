//! Client-side transport to the clew-server.
//!
//! The client is a pure renderer: it never touches the backend logic directly,
//! it speaks `clew-protocol` to a clew-server process. This module is the
//! transport — it starts the server and frames messages over its stdio. The
//! server is either a local child process or, for an SSH target, the remote
//! clew-server this module deploys and then runs over `ssh`; from here on both
//! are just a process whose stdin takes requests and whose stdout carries
//! events.
//!
//! What a remote connection goes through, and what can stop it:
//!
//! 1. **Probe** — is this exact build (version, protocol, protocol
//!    fingerprint) already deployed? One `ssh` call, which also sweeps
//!    deployments nobody has used for two weeks.
//! 2. **Deploy**, if not: the remote's `uname`, then the matching binary as
//!    VERIFIED BYTES from `clew_core::server_dist::server_binary` (checked
//!    against the digests this build embeds — see [`deploy_bytes`]), streamed
//!    into a uniquely named temp file that is renamed into place.
//! 3. **Start** — run it behind a per-connection marker line. A login shell
//!    can print anything before the command runs (a Debian `~/.bashrc`, a
//!    motd); everything before the marker is skipped, so that output no
//!    longer reads as a corrupt protocol stream and ends the connection.
//! 4. **Handshake** — the first valid frame must arrive within
//!    [`FIRST_FRAME_TIMEOUT`], or the connection is failed rather than left
//!    "Connecting…" forever.
//!
//! Every remote command is a fixed `sh -c '…'` script (so the remote login
//! shell only has to run `sh`) built from constants and validated tokens;
//! nothing the user typed is ever interpolated into it. `ssh` runs with `-T`,
//! with its stdin closed except where the binary is streamed, with an overall
//! timeout per step, and with its stderr captured (bounded), because that is
//! where every useful error lives: a failure reaches the app as
//! `ServerMsg::Unavailable { reason }` naming what went wrong — an unknown
//! or CHANGED host key (with the fingerprints to check, and the file and line
//! of the key on record), refused credentials, an unreachable host, a failed
//! deploy — instead of one generic "could not reach the host". Only exit
//! status 255 is ssh's own failure; anything else is the remote command's,
//! whatever its stderr says (a remote `mkdir` saying "Permission denied" is a
//! failed deploy, not a refused login).
//!
//! Before the first `ssh` of a connection, `ssh -G` resolves what the user's
//! ssh configuration makes of the target (`connect::SshSettings`): the
//! known_hosts files it names (clew's own is appended to THAT list), and the
//! endpoint and name an unknown key has to be scanned at and recorded under.
//!
//! A transport that dies is restarted by the app bumping `conn_gen`; restarts
//! back off exponentially and stop after [`MAX_ATTEMPTS`] consecutive failures
//! with a reason the app shows, instead of looping every 1.5 s for as long as
//! the window stays open. A transport that answered and then died within
//! [`HEALTHY_UPTIME`] counts as a failure too — a server that crashes right
//! after its handshake is not a healthy one — and only one that stayed up
//! that long starts the count afresh. Teardown closes the server's stdin first
//! and gives it [`TEARDOWN_GRACE`] to exit (and reap what it spawned) before
//! killing it.
//!
//! Password and one-time-code hosts: clew has no terminal of its own. When it
//! was started from one, ssh prompts there (once — saved connections set
//! `NumberOfPasswordPrompts=1`); from the Dock it cannot prompt at all, and the
//! failure says to use key-based login (ssh-agent or an identity file). One
//! login serves the whole connection: every `ssh` call shares a control master
//! ([`Launcher::control_args`]).

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clew_protocol::{ClientMessage, ServerMessage};
use iced::futures::{SinkExt, Stream};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};

use crate::Message;
use crate::ServerMsg;
use crate::connect::ConnTarget;

/// Name of the backend binary, looked up next to the running clew executable
/// (both live in the same `target/<profile>/` dir) and then on `PATH`.
const SERVER_BIN: &str = "clew-server";

/// How many requests may wait for the writer to take them to the server's
/// stdin. A healthy server drains the queue as fast as it is filled; a full
/// one means the server (or the link to it) has stopped reading, and the
/// client refuses further requests rather than buffering without bound — the
/// UI thread with an error (`ServerLink::send`), background tasks by waiting
/// for room.
pub const REQUEST_QUEUE: usize = 1024;

/// The client's end of the request channel (bounded: see [`REQUEST_QUEUE`]).
pub type RequestTx = tokio::sync::mpsc::Sender<ClientMessage>;

/// The subscription that spawns the clew-server and streams its events to the
/// client. On start it hands the client a request sender via
/// `ServerMsg::Connected`, then pumps the server's stdout until it exits —
/// and reports the exit as `ServerMsg::Disconnected`.
///
/// Keyed on [`ConnKey`]: connecting to a different host (or back to local)
/// changes the subscription identity, so iced drops the old transport —
/// tearing down its server — and runs a fresh one for the new target. That
/// single seam is how an in-app "Connect" switches between local and remote.
/// The key's `seq` is the client's `conn_gen`, bumped when a transport dies: a
/// finished stream never restarts on its own (iced only reacts to identity
/// changes), so the bump *is* the reconnect.
pub fn subscription(key: ConnKey) -> iced::Subscription<Message> {
    iced::Subscription::run_with(key, stream)
}

/// The identity of one transport instance. `seq` is the window's `conn_gen`,
/// bumped on every reconnect AND every target switch, so it alone names a
/// transport instance — every message the stream emits carries it, and a
/// handler can recognize (and drop) a late message from a dead or replaced
/// transport by comparing the number. `respawn` marks an instance replacing
/// one that died: only that kind of restart backs off (and eventually gives
/// up); a user-initiated switch connects immediately.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ConnKey {
    pub target: ConnTarget,
    pub seq: u64,
    pub respawn: bool,
}

// -- Timeouts ----------------------------------------------------------------

/// `ssh -G`: reads configuration only, never connects (a `Match exec` in the
/// user's configuration is the one thing that can make it slow).
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(10);
/// The first `ssh` call of a connection: it may be the one that logs in, and
/// from a terminal that login can be a prompt a person answers.
const PROBE_TIMEOUT: Duration = Duration::from_secs(90);
/// Any later short remote command.
const STEP_TIMEOUT: Duration = Duration::from_secs(30);
/// Fetching the server binary for the remote's platform: the download's own
/// deadline (`server_dist::DOWNLOAD_DEADLINE`, which ends a slow download with
/// its own message), plus room for the rest of that call — the checksum
/// sidecar a developer build fetches (a minute at most), hashing and caching.
/// It used to be SHORTER than the download deadline, so this outer limit
/// fired first and the download it abandoned ran on in the blocking pool.
const DOWNLOAD_TIMEOUT: Duration =
    clew_core::server_dist::DOWNLOAD_DEADLINE.saturating_add(Duration::from_secs(2 * 60));
/// Streaming the binary to the remote.
const DEPLOY_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// From running the remote command to its start marker.
const START_TIMEOUT: Duration = Duration::from_secs(60);
/// From `ServerMsg::Connected` to the server's first frame (the Hello reply).
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(30);
/// How long teardown waits for the server to exit on EOF before killing it.
const TEARDOWN_GRACE: Duration = Duration::from_secs(2);

// -- Output caps -------------------------------------------------------------

/// Stdout a short remote command may produce (a `--version` line, `uname`).
const MAX_COMMAND_STDOUT: usize = 64 * 1024;
/// Stderr kept from a short remote command.
const MAX_COMMAND_STDERR: usize = 16 * 1024;
/// Output a login shell may print before the start marker.
const MAX_PRE_MARKER_BYTES: usize = 256 * 1024;
/// One stderr line kept from a running server (longer lines are cut).
const MAX_STDERR_LINE: u64 = 4096;
/// Stderr lines of a running server kept for failure messages.
const STDERR_TAIL_LINES: usize = 20;

// -- Reconnect backoff -------------------------------------------------------

/// Delay before the first automatic restart after a failure; doubles per
/// consecutive failure up to [`BACKOFF_CAP`].
const BACKOFF_BASE: Duration = Duration::from_millis(1500);
const BACKOFF_CAP: Duration = Duration::from_secs(60);
/// Consecutive failed attempts after which automatic restarts stop.
pub(crate) const MAX_ATTEMPTS: u32 = 8;
/// How long a transport must have stayed up after its first frame before its
/// end no longer counts as a failure. Resetting the count on the first frame
/// (what this replaced) let a server that answered the Hello and then crashed
/// — or sent one malformed frame — restart every 1.5 s forever, tearing down
/// the window's connection state each time.
const HEALTHY_UPTIME: Duration = Duration::from_secs(60);

/// Why a transport could not come up (or went down before it ever answered).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Failure {
    pub(crate) kind: FailureKind,
    /// A sentence for the user, naming the host and what to do about it.
    pub(crate) reason: String,
    /// For an unknown host key: the key the host presents, fingerprinted, so
    /// the Connect modal can offer to trust it (`ServerMsg::HostKeyUnknown`).
    /// `None` when the host could not be scanned (it is behind a jump host or
    /// proxy command, or the ssh configuration could not be read). Boxed:
    /// every other failure carries none, and `Failure` travels in every
    /// `Result` of the connect path.
    pub(crate) host_key: Option<Box<crate::connect::ScannedHostKey>>,
    /// For a CHANGED host key whose key on record is one trusted in clew (it
    /// is in clew's own known_hosts): the name to hand
    /// `connect::forget_host_key`, the Connect modal's "Forget the old key"
    /// action (`ServerMsg::HostKeyChanged`). `None` for a key on record in
    /// the user's own files — clew never edits those.
    pub(crate) forget_host: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FailureKind {
    /// The host presented a different key than the one on record.
    HostKeyChanged,
    /// The host's key is not on record (strict checking refuses it).
    HostKeyUnknown,
    /// The host refused the credentials.
    Auth,
    /// DNS, routing, refused or timed-out TCP.
    Unreachable,
    /// A step exceeded its timeout.
    Timeout,
    /// Getting the right server binary onto the host failed.
    Deploy,
    /// The deployed server speaks another protocol build.
    Protocol,
    /// No local server binary to run.
    NoServer,
    /// The server started but ended before it ever answered.
    Exited,
    Other,
}

impl Failure {
    fn new(kind: FailureKind, reason: impl Into<String>) -> Failure {
        Failure {
            kind,
            reason: reason.into(),
            host_key: None,
            forget_host: None,
        }
    }

    /// The message that reports this failure to the app: a trust prompt for
    /// an unknown host key that was scanned, the offer to forget a changed
    /// key clew itself recorded, else `ServerMsg::Unavailable`.
    fn into_message(self, conn: u64) -> Message {
        match (self.host_key, self.forget_host) {
            (Some(key), _) => Message::Server(ServerMsg::HostKeyUnknown {
                conn,
                reason: self.reason,
                key: *key,
            }),
            (None, Some(forget_host)) => Message::Server(ServerMsg::HostKeyChanged {
                conn,
                reason: self.reason,
                forget_host,
            }),
            (None, None) => Message::Server(ServerMsg::Unavailable {
                conn,
                reason: self.reason,
            }),
        }
    }

    /// Whether trying the same thing again later can succeed. A changed or
    /// unknown host key, refused credentials, a checksum or protocol mismatch
    /// and a missing binary will fail identically on every attempt — retrying
    /// those only repeats the error (and, for a key, keeps knocking on a door
    /// that may not be the host's).
    pub(crate) fn retryable(&self) -> bool {
        matches!(
            self.kind,
            FailureKind::Unreachable
                | FailureKind::Timeout
                | FailureKind::Exited
                | FailureKind::Other
        )
    }
}

/// Consecutive failures per target, with the last reason, shared by the
/// restarts of one target: a transport that dies is replaced by a NEW stream
/// instance (the app re-keys the subscription), so the count has to outlive
/// the instance. Reset by a transport that stayed up for [`HEALTHY_UPTIME`]
/// and by any attempt the user starts.
static FAILURES: Mutex<Option<HashMap<ConnTarget, (u32, String)>>> = Mutex::new(None);

fn failures(target: &ConnTarget) -> (u32, String) {
    let map = FAILURES.lock().unwrap_or_else(|e| e.into_inner());
    map.as_ref()
        .and_then(|m| m.get(target).cloned())
        .unwrap_or_default()
}

fn record_failure(target: &ConnTarget, failure: &Failure) {
    let mut map = FAILURES.lock().unwrap_or_else(|e| e.into_inner());
    let entry = map
        .get_or_insert_with(HashMap::new)
        .entry(target.clone())
        .or_default();
    entry.0 = entry.0.saturating_add(1);
    entry.1 = failure.reason.clone();
}

fn reset_failures(target: &ConnTarget) {
    let mut map = FAILURES.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(m) = map.as_mut() {
        m.remove(target);
    }
}

/// The wait before an automatic restart after `failures` consecutive
/// failures: `base` (1.5 s — [`BACKOFF_BASE`]), doubling, capped at a minute.
fn backoff_delay(base: Duration, failures: u32) -> Duration {
    let factor = 2u32.saturating_pow(failures.min(16));
    base.saturating_mul(factor).min(BACKOFF_CAP)
}

/// Skip what a remote login shell printed before clew-server started, up to
/// and including the `marker` line. Bounded in bytes; the first few lines are
/// kept for the error message should the marker never come.
async fn skip_to_marker<R>(reader: &mut R, marker: &str) -> Result<(), String>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let mut seen = 0usize;
    let mut noise: Vec<String> = Vec::new();
    loop {
        let mut buf = Vec::new();
        let n = reader
            .take(MAX_PRE_MARKER_BYTES as u64 + 1)
            .read_until(b'\n', &mut buf)
            .await
            .map_err(|e| e.to_string())?;
        if n == 0 {
            let shown = if noise.is_empty() {
                String::new()
            } else {
                format!(" (it printed: {})", noise.join(" | "))
            };
            return Err(format!(
                "the connection closed before clew-server started{shown}"
            ));
        }
        let text = String::from_utf8_lossy(&buf);
        let line = text.trim_end_matches(['\n', '\r']);
        if line == marker {
            return Ok(());
        }
        seen += n;
        if seen > MAX_PRE_MARKER_BYTES {
            return Err(format!(
                "the remote shell printed more than {} KiB before clew-server started",
                MAX_PRE_MARKER_BYTES / 1024
            ));
        }
        if noise.len() < 3 && !line.trim().is_empty() {
            noise.push(line.chars().take(200).collect());
        }
    }
}

/// Locate the clew-server binary: prefer a sibling of the running executable
/// (the workspace builds both into the same directory), else an absolute
/// match on `PATH`.
///
/// The fallback used to be the bare name, which the OS resolves against the
/// inherited `PATH` — including a relative entry like `.`, against the cwd
/// clew happened to be launched from. A repository's own `clew-server` would
/// then be spawned and handed the AI configuration on handshake. The same
/// absolute-only lookup the LSP store uses is applied here; when nothing
/// resolves, the caller reports the server as unavailable rather than
/// spawning something ambiguous.
fn server_bin_path() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let candidate = dir.join(SERVER_BIN);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    clew_core::lsp::store::find_on_path(SERVER_BIN)
}

/// An OpenSSH tool by absolute path: the first ABSOLUTE `PATH` entry holding
/// it (so a Homebrew OpenSSH the user prefers is honored), else the system's
/// `/usr/bin` copy. Never a bare name: that would be resolved against every
/// `PATH` entry, relative ones included — the current directory, i.e. the
/// repository clew was launched from.
fn openssh_tool(name: &str) -> PathBuf {
    clew_core::lsp::store::find_on_path(name).unwrap_or_else(|| Path::new("/usr/bin").join(name))
}

/// What a transport runs and how it paces restarts: the real OpenSSH tools
/// and clew-server ([`Launcher::system`]), or fakes and short timings in the
/// tests — the whole connect/restart path runs hermetically there.
#[derive(Debug, Clone)]
struct Launcher {
    ssh: PathBuf,
    keyscan: PathBuf,
    keygen: PathBuf,
    /// The local clew-server; `None` when none was found.
    local_server: Option<PathBuf>,
    /// First restart delay ([`BACKOFF_BASE`]).
    backoff_base: Duration,
    /// Uptime after which a transport's end is not a failure
    /// ([`HEALTHY_UPTIME`]).
    healthy_after: Duration,
    /// Where the calls of a connection share one login
    /// ([`Launcher::control_args`]): the control sockets' directory — `None`
    /// for no sharing (the tests that must not create the real one).
    control_dir: Option<PathBuf>,
}

impl Launcher {
    fn system() -> Launcher {
        Launcher {
            ssh: openssh_tool("ssh"),
            keyscan: openssh_tool("ssh-keyscan"),
            keygen: openssh_tool("ssh-keygen"),
            local_server: server_bin_path(),
            backoff_base: BACKOFF_BASE,
            healthy_after: HEALTHY_UPTIME,
            control_dir: Some(control_dir(current_uid())),
        }
    }

    /// Options that make every `ssh` call of a connection share ONE login:
    /// the probe, the deploy, the start and every reconnect ride a control
    /// master instead of each authenticating (and, on a one-time-code host,
    /// each asking again). The socket lives in a directory only this user
    /// can reach ([`private_socket_dir`]); without one, clew connects
    /// without sharing.
    fn control_args(&self) -> Vec<String> {
        match &self.control_dir {
            Some(dir) => control_args_in(dir, current_uid()),
            None => Vec::new(),
        }
    }
}

fn current_uid() -> u32 {
    // SAFETY: getuid(2) cannot fail and touches no memory.
    unsafe { libc::getuid() }
}

/// The control sockets' directory for `uid`. Short on purpose: a Unix socket
/// path is limited to 104 bytes, and ssh appends 17 more to the socket's
/// name (`%C`, 40 hex digits) while it sets the socket up.
fn control_dir(uid: u32) -> PathBuf {
    PathBuf::from(format!("/tmp/clew-{uid}"))
}

/// [`Launcher::control_args`] with the socket directory `dir` (made private
/// to `uid`).
fn control_args_in(dir: &Path, uid: u32) -> Vec<String> {
    match private_socket_dir(dir, uid) {
        Some(dir) => vec![
            "-o".into(),
            "ControlMaster=auto".into(),
            "-o".into(),
            // `%C` is a hash of local host, remote host, port and user, so
            // every destination has its own master. The directory is short
            // on purpose: a Unix socket path is limited to 104 bytes and ssh
            // appends 17 more while it sets the socket up.
            format!("ControlPath={}/%C", dir.display()),
            "-o".into(),
            "ControlPersist=60".into(),
        ],
        None => Vec::new(),
    }
}

/// `dir` as a directory only `uid` can use — created 0700 if missing, and
/// refused (`None`) if something else is already there: a symlink, another
/// account's directory, or one with group/other access. A control socket in
/// a directory someone else controls would let them sit between clew and the
/// host.
fn private_socket_dir(dir: &Path, uid: u32) -> Option<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => return None,
    }
    let meta = std::fs::symlink_metadata(dir).ok()?;
    (meta.file_type().is_dir() && meta.uid() == uid && meta.mode() & 0o077 == 0)
        .then(|| dir.to_path_buf())
}

/// How to run a command on the remote host.
struct Ssh {
    /// The `ssh` binary.
    program: PathBuf,
    /// Options before the target's own: `-T`, the shared-login options and
    /// the known-hosts options.
    prefix: Vec<String>,
    /// The target's arguments, ending with the destination.
    target: Vec<String>,
    /// `user@host` (or the CLEW_SSH label) for messages.
    label: String,
    /// What `ssh -G` resolved for the target, or why it could not.
    settings: Result<crate::connect::SshSettings, String>,
    /// clew's own known_hosts, when these invocations consult it.
    clew_known_hosts: Option<PathBuf>,
}

impl Ssh {
    /// The invocation for a target: `-T`, the shared-login options
    /// (`control`, from [`Launcher::control_args`]), and clew's known_hosts
    /// appended to the files the user's own ssh configuration names for this
    /// host — resolved first with `ssh -G` (`connect::known_hosts_options`).
    async fn for_target(label: &str, args: &[String], program: &Path, control: Vec<String>) -> Ssh {
        let mut options = vec!["-T".to_string()];
        options.extend(control);
        // Resolved with everything the real calls pass EXCEPT the known-hosts
        // options: the point is to learn the user's own list.
        let settings = resolve_settings(program, &options, args, label).await;
        if let Err(e) = &settings {
            eprintln!("[clew] {label}: {e}");
        }
        Ssh::assemble(label, args, program, options, settings)
    }

    /// [`Ssh::for_target`] once `ssh -G` has answered: `options` (`-T` and
    /// the shared login) followed by the known-hosts options `settings` call
    /// for.
    fn assemble(
        label: &str,
        args: &[String],
        program: &Path,
        options: Vec<String>,
        settings: Result<crate::connect::SshSettings, String>,
    ) -> Ssh {
        let known = crate::connect::known_hosts_options(args, settings.as_ref().ok());
        let mut prefix = options;
        prefix.extend(known.args);
        Ssh {
            program: program.to_path_buf(),
            prefix,
            target: args.to_vec(),
            label: label.to_string(),
            settings,
            clew_known_hosts: known.clew_file,
        }
    }

    /// `ssh -T … <target> <remote>`, where `remote` is ONE argument: ssh joins
    /// its command arguments with spaces for the remote shell, so a script
    /// split across argv would be re-parsed there.
    fn command(&self, remote: &str) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new(&self.program);
        cmd.args(&self.prefix).args(&self.target).arg(remote);
        cmd
    }
}

/// `ssh -G <options> <target>`: what the user's ssh configuration makes of
/// the target (see `connect::SshSettings`). Never connects.
async fn resolve_settings(
    program: &Path,
    options: &[String],
    args: &[String],
    label: &str,
) -> Result<crate::connect::SshSettings, String> {
    let mut cmd = tokio::process::Command::new(program);
    cmd.arg("-G").args(options).args(args);
    let out = run_bounded(
        cmd,
        None,
        RESOLVE_TIMEOUT,
        "reading the ssh configuration",
        label,
    )
    .await
    .map_err(|f| f.reason)?;
    if !out.success {
        return Err(format!(
            "`ssh -G` could not read the ssh configuration: {}",
            last_line(&out.stderr)
        ));
    }
    crate::connect::SshSettings::parse(&out.stdout, |p| Path::new(p).exists())
        .map_err(|e| format!("`ssh -G` printed something clew cannot read: {e}"))
}

/// What a finished short command produced (bounded).
struct Captured {
    success: bool,
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

/// Read up to `cap` bytes, then keep draining (discarding) to EOF so the
/// process is never blocked on a full pipe.
async fn read_capped<R: tokio::io::AsyncRead + Unpin>(mut reader: R, cap: usize) -> Vec<u8> {
    let mut kept = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let room = cap.saturating_sub(kept.len());
                kept.extend_from_slice(&chunk[..n.min(room)]);
            }
        }
    }
    kept
}

/// Run `cmd` to completion under `limit`, feeding it `input` (or nothing: its
/// stdin is `/dev/null`, so the REMOTE command reads end-of-file instead of
/// whatever clew's own stdin is), capturing bounded stdout and stderr. The
/// process is killed if the limit passes. ssh's own password and passphrase
/// prompts do not read stdin — they open the controlling terminal — so when
/// clew was started from a terminal they still appear there (see the module
/// docs on password hosts).
async fn run_bounded(
    mut cmd: tokio::process::Command,
    input: Option<&[u8]>,
    limit: Duration,
    step: &str,
    label: &str,
) -> Result<Captured, Failure> {
    cmd.stdin(if input.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true);
    let mut child = cmd
        .spawn()
        .map_err(|e| Failure::new(FailureKind::Other, format!("could not run ssh: {e}")))?;
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let work = async move {
        let write = async move {
            if let (Some(mut w), Some(data)) = (stdin, input) {
                w.write_all(data).await?;
                w.shutdown().await?;
            }
            Ok::<(), std::io::Error>(())
        };
        let out = async move {
            match stdout {
                Some(r) => read_capped(r, MAX_COMMAND_STDOUT).await,
                None => Vec::new(),
            }
        };
        let err = async move {
            match stderr {
                Some(r) => read_capped(r, MAX_COMMAND_STDERR).await,
                None => Vec::new(),
            }
        };
        let (written, out, err) = tokio::join!(write, out, err);
        let status = child.wait().await;
        (written, out, err, status)
    };
    let (written, out, err, status) = tokio::time::timeout(limit, work).await.map_err(|_| {
        Failure::new(
            FailureKind::Timeout,
            format!(
                "{label}: {step} did not finish within {} s",
                limit.as_secs()
            ),
        )
    })?;
    let status =
        status.map_err(|e| Failure::new(FailureKind::Other, format!("{label}: {step}: {e}")))?;
    let captured = Captured {
        success: status.success() && written.is_ok(),
        code: status.code(),
        stdout: String::from_utf8_lossy(&out).into_owned(),
        stderr: String::from_utf8_lossy(&err).into_owned(),
    };
    if let (Err(e), true) = (written, status.success()) {
        return Err(Failure::new(
            FailureKind::Deploy,
            format!("{label}: {step}: the upload was cut short ({e})"),
        ));
    }
    Ok(captured)
}

/// The last non-empty line of some output, for a one-line message.
fn last_line(text: &str) -> &str {
    text.lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
}

/// Classify a failed `ssh` call. Exit status 255 is ssh's own failure — the
/// connection, the host key, the login — and only then does its stderr say
/// which. Anything else is the REMOTE COMMAND's failure, whatever that
/// command printed: a remote `mkdir` that says "Permission denied" is not a
/// refused login, and reading it as one sent people off to fix their keys.
fn ssh_failure(step: &str, ssh: &Ssh, out: &Captured) -> Failure {
    let label = ssh.label.as_str();
    let err = out.stderr.as_str();
    if out.code == Some(255) {
        if err.contains("REMOTE HOST IDENTIFICATION HAS CHANGED")
            || (err.contains("ost key for") && err.contains("has changed"))
        {
            return changed_key_failure(ssh, err);
        }
        if err.contains("host key is known for") || err.contains("Host key verification failed") {
            return Failure::new(
                FailureKind::HostKeyUnknown,
                format!(
                    "{label}'s host key is not known yet, and clew does not accept an unknown \
                     key blindly."
                ),
            );
        }
        if err.contains("Permission denied") || err.contains("Too many authentication failures") {
            return Failure::new(
                FailureKind::Auth,
                format!(
                    "{label} refused the login ({}). clew cannot type a password when it was \
                     not started from a terminal: use key-based login (a key in ssh-agent — \
                     `ssh-add` — or an identity file).",
                    last_line(err)
                ),
            );
        }
        return Failure::new(
            FailureKind::Unreachable,
            format!("Could not reach {label}: {}", last_line(err)),
        );
    }
    let detail = if last_line(err).is_empty() {
        format!(
            "exit status {}",
            out.code.map_or("?".into(), |c| c.to_string())
        )
    } else {
        last_line(err).to_string()
    };
    Failure::new(
        FailureKind::Other,
        format!("{label}: {step} failed: {detail}"),
    )
}

/// What ssh's changed-host-key warning says, as OpenSSH prints it:
///
/// ```text
/// The fingerprint for the ED25519 key sent by the remote host is
/// SHA256:eTblk8vx….
/// …
/// Offending ED25519 key in /Users/me/.ssh/known_hosts:12
/// Host key for [example.com]:2222 has changed and you have requested strict checking.
/// ```
#[derive(Debug, Default, PartialEq, Eq)]
struct ChangedKeyReport {
    /// The key the host presents now: `(kind, fingerprint)`.
    presented: Option<(String, String)>,
    /// Where the key on record is: every `(file, line)` ssh names.
    offending: Vec<(String, u64)>,
    /// The name ssh looked the key up by.
    host: Option<String>,
}

impl ChangedKeyReport {
    fn parse(stderr: &str) -> ChangedKeyReport {
        let mut report = ChangedKeyReport::default();
        let mut lines = stderr.lines().map(str::trim);
        while let Some(line) = lines.next() {
            if let Some(rest) = line.strip_prefix("The fingerprint for the ")
                && let Some(kind) = rest.strip_suffix(" key sent by the remote host is")
                && let Some(print) = lines.next()
            {
                let print = print.trim_end_matches('.');
                if !print.is_empty() && !print.contains(char::is_whitespace) {
                    report.presented = Some((kind.to_string(), print.to_string()));
                }
            } else if let Some(rest) = line.strip_prefix("Offending ")
                && let Some((_, at)) = rest.split_once(" in ")
                && let Some((file, number)) = at.rsplit_once(':')
                && let Ok(number) = number.parse()
            {
                report.offending.push((file.to_string(), number));
            } else if let Some(at) = line.find("ost key for ")
                && let Some((host, _)) =
                    line[at + "ost key for ".len()..].split_once(" has changed")
            {
                report.host = Some(host.to_string());
            }
        }
        report
    }
}

/// A command for the user to run, with each argument quoted for a POSIX shell.
fn shell_command(words: &[&str]) -> String {
    words
        .iter()
        .map(|w| {
            if !w.is_empty()
                && w.chars().all(|c| {
                    c.is_ascii_alphanumeric()
                        || matches!(c, '-' | '_' | '.' | '/' | '@' | ':' | ',' | '+')
                })
            {
                w.to_string()
            } else {
                format!("'{}'", w.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The refusal of a host whose key CHANGED, with what ssh said about it: the
/// key presented now, where the key on record is (file and line), and how to
/// retire that one if the change is expected — for a key on record in clew's
/// own known_hosts, the forget action ([`Failure::forget_host`]) and the
/// `ssh-keygen -f` command that does the same (a bare `ssh-keygen -R <host>`,
/// what this used to say, only edits `~/.ssh/known_hosts`); for one in the
/// user's own files, the command naming that file.
fn changed_key_failure(ssh: &Ssh, stderr: &str) -> Failure {
    let label = ssh.label.as_str();
    let report = ChangedKeyReport::parse(stderr);
    // What ssh looks the host up by: resolved, else what ssh said, else the
    // destination as given.
    let name = ssh
        .settings
        .as_ref()
        .ok()
        .map(crate::connect::SshSettings::lookup_name)
        .or(report.host)
        .or_else(|| {
            crate::connect::ssh_host_port(&ssh.target)
                .map(|(host, port)| crate::connect::known_hosts_pattern(&host, port))
        })
        .unwrap_or_else(|| label.to_string());
    let mut reason = format!(
        "{label}'s host key has CHANGED since the last connection. That is what a \
         man-in-the-middle looks like (or the host was reinstalled), so clew will not connect."
    );
    if let Some((kind, print)) = &report.presented {
        reason.push_str(&format!(" It now presents {kind} {print}."));
    }
    let mut forget_host = None;
    for (file, line) in &report.offending {
        let remove = shell_command(&["ssh-keygen", "-f", file, "-R", &name]);
        if ssh.clew_known_hosts.as_deref() == Some(Path::new(file)) {
            reason.push_str(&format!(
                " The key on record ({file}:{line}) is one you trusted in clew. If you know why \
                 it changed, forget it — `{remove}` — then connect again and check the new \
                 key's fingerprint before trusting it."
            ));
            forget_host = Some(name.clone());
        } else {
            reason.push_str(&format!(
                " The key on record is in {file}:{line}. If you know why it changed, remove \
                 it — `{remove}` — and connect again."
            ));
        }
    }
    if report.offending.is_empty() {
        let remove = shell_command(&["ssh-keygen", "-R", &name]);
        reason.push_str(&format!(
            " If you know why it changed, remove the old key — `{remove}` — and connect again."
        ));
    }
    Failure {
        forget_host,
        ..Failure::new(FailureKind::HostKeyChanged, reason)
    }
}

/// The key `ssh` will be shown by the host of `settings`, for the user to
/// check before trusting it: `ssh-keyscan` fetches the keys at the endpoint
/// ssh ACTUALLY connects to (its `HostName` and `Port` after the user's
/// configuration), the most preferred type is taken, and `ssh-keygen -lf`
/// fingerprints exactly the line trusting will record — under the name ssh
/// looks the key up by (`HostKeyAlias`, or the resolved `[host]:port`). The
/// key is offered for the connection whose destination's host field is
/// `for_host`. `None` for a host behind a jump host or proxy command (what
/// answers at its address from here need not be what ssh talks to), a name
/// that is not one literal host, or a scan that finds no usable key.
async fn scan_host_key_with(
    settings: &crate::connect::SshSettings,
    for_host: &str,
    keyscan: &Path,
    keygen: &Path,
) -> Option<crate::connect::ScannedHostKey> {
    use crate::connect::{
        fingerprint_of, known_hosts_pattern, plain_host_field, preferred_key_lines,
    };
    let (host, port) = settings.direct_endpoint()?;
    let recorded_as = settings.lookup_name();
    if !plain_host_field(&recorded_as) {
        return None;
    }
    let host = host.to_ascii_lowercase();
    // ssh-keyscan prints the name it scanned, lower-cased, with the port.
    let scanned_as = known_hosts_pattern(&host, Some(port));
    let mut scan = tokio::process::Command::new(keyscan);
    scan.arg("-T").arg("5");
    if port != 22 {
        scan.arg("-p").arg(port.to_string());
    }
    scan.arg("--").arg(&host);
    let keys = run_bounded(scan, None, Duration::from_secs(15), "key scan", &host)
        .await
        .ok()?;
    // Fingerprint ONE line at a time, and exactly the line trusting records:
    // the fingerprint shown is then provably that line's, not a neighbour's.
    for scanned in preferred_key_lines(&keys.stdout, &scanned_as) {
        let mut fields = scanned.split(' ').skip(1);
        let (key_type, key) = (fields.next()?, fields.next()?);
        let line = format!("{recorded_as} {key_type} {key}");
        let mut fp = tokio::process::Command::new(keygen);
        fp.args(["-l", "-f", "-"]);
        let input = format!("{line}\n");
        let Ok(prints) = run_bounded(
            fp,
            Some(input.as_bytes()),
            Duration::from_secs(10),
            "fingerprint",
            &host,
        )
        .await
        else {
            continue;
        };
        if let Some((kind, fingerprint)) = fingerprint_of(&prints.stdout, key_type) {
            // Trusting records it under this name only because it was
            // offered under it, for this connection.
            crate::connect::offer_name(for_host, &recorded_as);
            return Some(crate::connect::ScannedHostKey {
                host: for_host.to_string(),
                line,
                kind,
                fingerprint,
            });
        }
    }
    None
}

/// The message for an unknown host key, and the key to offer for trust when
/// the host could be scanned. With a key, the Connect modal shows its
/// fingerprint beside Trust / Cancel; without one, the message says why and
/// how to trust the host from a terminal instead.
async fn describe_unknown_host(
    ssh: &Ssh,
    keyscan: &Path,
    keygen: &Path,
) -> (String, Option<crate::connect::ScannedHostKey>) {
    use crate::connect::{known_hosts_pattern, ssh_destination, ssh_host_port, ssh_port};
    let label = ssh.label.as_str();
    let refused = format!(
        "{label}'s host key is not known yet, and clew does not accept an unknown key blindly."
    );
    let check = "Check it against a fingerprint you trust (from the host's administrator, or \
                 `ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub` on the host)";
    // Offered only where a trusted key would be read back — clew's file is
    // among the ones these calls consult — and for a connection that names
    // one host (the app trusts for exactly that one).
    let for_host = ssh_host_port(&ssh.target).map(|(host, port)| known_hosts_pattern(&host, port));
    let why_not = match (&ssh.settings, &ssh.clew_known_hosts, &for_host) {
        (Ok(settings), Some(_), Some(for_host)) => match &settings.proxy {
            Some((option, value)) => format!(
                "{label} is reached through {option} {value}, so clew cannot fetch the key it \
                 presents to show you here."
            ),
            None => match scan_host_key_with(settings, for_host, keyscan, keygen).await {
                Some(key) => {
                    let reason = format!(
                        "{refused} It presents {} {}. {check} before trusting it.",
                        key.kind, key.fingerprint
                    );
                    return (reason, Some(key));
                }
                None => format!(
                    "clew could not fetch the key {label} presents ({}:{}).",
                    settings.hostname, settings.port
                ),
            },
        },
        (Err(e), _, _) => format!("({e})"),
        _ => String::new(),
    };
    let dest = ssh_destination(&ssh.target).unwrap_or(label);
    let port = ssh_port(&ssh.target)
        .filter(|p| *p != 22)
        .map(|p| p.to_string());
    // A command to paste into a shell, so quoted like the changed-key one
    // (`shell_command`): a destination with a space, a quote or a `$` must
    // reach ssh as the one word it is, and `--` keeps it from being read as
    // an option — exactly how clew passes it itself.
    let mut words = vec!["ssh"];
    if let Some(port) = &port {
        words.extend(["-p", port.as_str()]);
    }
    words.extend(["--", dest]);
    let command = shell_command(&words);
    let why_not = if why_not.is_empty() {
        why_not
    } else {
        format!(" {why_not}")
    };
    let reason = format!(
        "{refused}{why_not} {check}, then trust it by connecting once in Terminal — \
         `{command}` — and answering yes. Then connect again here."
    );
    (reason, None)
}

/// The client's own release version. The remote server is deployed and cached
/// keyed by it plus the protocol version and fingerprint
/// (`~/.clew/server/<version>-p<protocol>-<fingerprint>/clew-server`), so a
/// client update carries its matching — possibly patched — server to the
/// remote, each build at its own path (like VS Code's per-commit remote
/// server). The protocol component matters when the protocol changes within
/// one release version (development builds): without it, a cached binary
/// speaking the OLD protocol passed the `--version` probe and was silently
/// reused, and every frame it sent failed to deserialize.
const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The clew-server binary for `platform`, verified and in memory, ready to
/// stream: exactly the bytes that were checked are the bytes sent.
///
/// `clew_core::server_dist::server_binary` downloads (or reuses the cache) and
/// verifies against the digests this build embeds, handing back the BYTES —
/// re-reading a verified file by path, what the deployer used to do, left a
/// window in which the file could change after its check (the pattern commit
/// 728a040 removed for LSP commands).
///
/// Those digests come from `CLEW_SERVER_DIGESTS`, which the release workflow
/// sets while building the client, once the server jobs have built: the
/// release's `.sha256` sidecars concatenated — one `sha256sum` line per server
/// asset, `<64 lowercase hex>  clew-server-<slug>-p<protocol>` (for example
/// `3b0c…9f1e  clew-server-linux-x86_64-p13` at protocol 13). The parser,
/// `server_dist::parse_digest_list`, also accepts `;` or `,` between entries,
/// a `*` before the name, and `<asset>=<hex>`. A build without it (a developer
/// build) falls back to the sidecar published beside each asset, which only
/// guards against a corrupted transfer.
///
/// Blocking; run off the async threads.
fn deploy_bytes(platform: &str) -> Result<Vec<u8>, String> {
    let verified = clew_core::server_dist::server_binary(
        platform,
        CLIENT_VERSION,
        clew_protocol::PROTOCOL_VERSION,
    )?;
    upload_bytes(verified)
}

/// The check nearest to the upload: the bytes about to be streamed still hash
/// to the digest they were verified against. Owned bytes cannot change in
/// between; what this catches is a verifier that ever hands back a different
/// buffer than the one it checked.
fn upload_bytes(verified: clew_core::server_dist::VerifiedServer) -> Result<Vec<u8>, String> {
    use clew_core::lsp::registry::Sha256;
    let actual = Sha256::of(&verified.bytes);
    if Sha256::parse(&verified.sha256) != Some(actual) {
        return Err(format!(
            "the clew-server bytes do not match their verified digest (verified {}, now {actual}); \
             not deploying them",
            verified.sha256
        ));
    }
    Ok(verified.bytes)
}

/// Where this build's server lives on the remote, as validated tokens.
struct RemoteLayout {
    /// `<version>-p<protocol>-<fingerprint>`.
    dir: String,
    /// The exact first line `clew-server --version` must print.
    probe_line: String,
}

/// The remote layout for this build. Every token goes into a shell script, so
/// each is checked to be a plain `[A-Za-z0-9._-]` token (they are compile-time
/// constants; this guards the build, not user input).
fn remote_layout() -> Result<RemoteLayout, Failure> {
    let dir = format!(
        "{CLIENT_VERSION}-p{}-{}",
        clew_protocol::PROTOCOL_VERSION,
        clew_protocol::SCHEMA_FINGERPRINT
    );
    if dir.is_empty()
        || !dir
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
    {
        return Err(Failure::new(
            FailureKind::Other,
            format!("this build's server directory name {dir:?} is not a plain token"),
        ));
    }
    Ok(RemoteLayout {
        dir,
        // The very line the server's `--version` prints (one definition, in
        // the protocol crate, so the probe cannot drift from the binary).
        probe_line: clew_protocol::version_line(),
    })
}

/// A remote command that runs `script` under `sh`, whatever the user's login
/// shell is. `script` must not contain a single quote; every caller builds it
/// from constants and validated tokens, and this refuses rather than escapes.
fn sh_c(script: &str) -> Result<String, Failure> {
    if script.contains('\'') {
        return Err(Failure::new(
            FailureKind::Other,
            "internal error: a remote script contains a single quote",
        ));
    }
    Ok(format!("sh -c '{script}'"))
}

/// Probe for this build's server, and sweep the other builds' directories
/// nobody has used for two weeks. The probe touches the current directory, so
/// a build in use is never old enough to be swept — by this client or by
/// another machine's clew connecting to the same account.
fn probe_script(layout: &RemoteLayout) -> String {
    let dir = &layout.dir;
    format!(
        "R=\"$HOME/.clew/server\"; D=\"$R/{dir}\"; S=\"$D/clew-server\"; \
         if [ -x \"$S\" ]; then touch \"$D\" 2>/dev/null; \"$S\" --version 2>/dev/null; fi; \
         find \"$R\" -mindepth 1 -maxdepth 1 -type d -name \"*-p*\" ! -name \"{dir}\" \
         -mtime +14 -exec rm -rf {{}} + 2>/dev/null; \
         find \"$D\" -maxdepth 1 -name \"clew-server.*.tmp\" -mtime +1 -exec rm -f {{}} + 2>/dev/null; \
         exit 0"
    )
}

/// Receive the binary on stdin into a temp name unique to this upload (two
/// windows deploying at once no longer write one `clew-server.tmp`), then
/// rename it into place — a half-written binary is never run.
fn install_script(layout: &RemoteLayout, token: &str) -> String {
    let dir = &layout.dir;
    format!(
        "umask 077; D=\"$HOME/.clew/server/{dir}\"; T=\"$D/clew-server.{token}.tmp\"; \
         mkdir -p \"$D\" || exit 1; \
         if cat > \"$T\" && chmod 700 \"$T\" && mv -f \"$T\" \"$D/clew-server\"; then exit 0; fi; \
         rm -f \"$T\"; exit 1"
    )
}

/// Print `marker`, then become clew-server. Everything the login shell
/// printed before the marker is not protocol.
///
/// `--remote`: the launcher KNOWS this server runs over SSH, so it says so
/// rather than leaving the server to guess from `SSH_CONNECTION` (see
/// `clew_server::SpawnPolicy`) — the same reason [`start_local`] passes
/// `--local`.
fn start_script(layout: &RemoteLayout, marker: &str) -> String {
    let dir = &layout.dir;
    format!(
        "S=\"$HOME/.clew/server/{dir}/clew-server\"; \
         [ -x \"$S\" ] || {{ echo \"clew-server is not installed at $S\" >&2; exit 127; }}; \
         printf \"%s\\n\" \"{marker}\"; exec \"$S\" --remote"
    )
}

/// The re-probe after a deploy, with the server's own errors kept.
fn verify_script(layout: &RemoteLayout) -> String {
    format!(
        "\"$HOME/.clew/server/{}/clew-server\" --version 2>&1",
        layout.dir
    )
}

/// A token for temp names and markers: this process's id plus 64 bits from a
/// per-process random hasher. Unique, not secret — it only has to never
/// collide with another upload or appear in a login shell's output by chance.
fn unique_token() -> String {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(N.fetch_add(1, Ordering::Relaxed));
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default(),
    );
    format!("{}-{:016x}", std::process::id(), h.finish())
}

/// Bootstrap the remote: if this exact clew-server build isn't deployed, detect
/// the platform, obtain the verified binary (`obtain`, given `uname -sm`), and
/// stream it over SSH. This is the "no server yet" step: plain shell commands,
/// before any protocol.
async fn bootstrap_remote<F>(ssh: &Ssh, layout: &RemoteLayout, obtain: F) -> Result<(), Failure>
where
    F: FnOnce(String) -> Result<Vec<u8>, String> + Send + 'static,
{
    let label = ssh.label.as_str();
    let probe = run_bounded(
        ssh.command(&sh_c(&probe_script(layout))?),
        None,
        PROBE_TIMEOUT,
        "probing the host",
        label,
    )
    .await?;
    if !probe.success {
        return Err(ssh_failure("probing the host", ssh, &probe));
    }
    if probe.stdout.lines().any(|l| l.trim() == layout.probe_line) {
        return Ok(());
    }

    // Not there: detect the remote platform and get this version's binary for
    // it — downloaded from the release host and cached, fully automatically.
    let uname = run_bounded(
        ssh.command("uname -sm"),
        None,
        STEP_TIMEOUT,
        "detecting the platform",
        label,
    )
    .await?;
    if !uname.success {
        return Err(ssh_failure("detecting the platform", ssh, &uname));
    }
    let platform = uname.stdout.trim().to_string();
    let bytes = tokio::time::timeout(
        DOWNLOAD_TIMEOUT,
        tokio::task::spawn_blocking(move || obtain(platform)),
    )
    .await
    .map_err(|_| {
        Failure::new(
            FailureKind::Timeout,
            format!(
                "fetching clew-server for {label} did not finish within {} minutes",
                DOWNLOAD_TIMEOUT.as_secs() / 60
            ),
        )
    })?
    .map_err(|e| Failure::new(FailureKind::Deploy, format!("fetching clew-server: {e}")))?
    .map_err(|e| {
        Failure::new(
            FailureKind::Deploy,
            format!("No clew-server for {label}: {e}"),
        )
    })?;

    let installed = run_bounded(
        ssh.command(&sh_c(&install_script(layout, &unique_token()))?),
        Some(&bytes),
        DEPLOY_TIMEOUT,
        "installing clew-server",
        label,
    )
    .await?;
    if !installed.success {
        let failure = ssh_failure("installing clew-server", ssh, &installed);
        return Err(match failure.kind {
            FailureKind::Other => Failure::new(
                FailureKind::Deploy,
                format!(
                    "Could not install clew-server on {label} (is the home directory \
                     writable, with space to spare?): {}",
                    last_line(&installed.stderr)
                ),
            ),
            _ => failure,
        });
    }

    // Re-probe what was just deployed. If it can't run (wrong arch, damaged
    // transfer, a home directory mounted noexec) or speaks another protocol
    // build, fail HERE with a message — not later, as a handshake that
    // mysteriously never completes. A release download whose protocol sources
    // differ from this client's fails too: the handshake would refuse it
    // anyway, so name the problem now.
    let check = run_bounded(
        ssh.command(&sh_c(&verify_script(layout))?),
        None,
        STEP_TIMEOUT,
        "checking the deployed clew-server",
        label,
    )
    .await?;
    if check.stdout.lines().any(|l| l.trim() == layout.probe_line) {
        return Ok(());
    }
    if check.code == Some(255) {
        return Err(ssh_failure(
            "checking the deployed clew-server",
            ssh,
            &check,
        ));
    }
    Err(Failure::new(
        FailureKind::Protocol,
        format!(
            "the clew-server deployed to {label} did not pass the protocol check (wanted \
             '{}', got: {}){}",
            layout.probe_line,
            last_line(&check.stdout),
            if check.success {
                ""
            } else {
                " — is the home directory mounted noexec?"
            }
        ),
    ))
}

/// The tail of a running server's stderr, kept for failure messages while
/// every line is also forwarded to clew's own stderr.
#[derive(Clone, Default)]
struct StderrTail(Arc<Mutex<VecDeque<String>>>);

impl StderrTail {
    /// Drain `stderr` on a task until EOF. Returns the tail and the task, which
    /// can be awaited briefly so a message includes the last lines written.
    fn spawn<R>(stderr: R, tag: &'static str) -> (StderrTail, tokio::task::JoinHandle<()>)
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
    {
        let tail = StderrTail::default();
        let sink = tail.clone();
        let task = tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            loop {
                let mut buf = Vec::new();
                match (&mut reader)
                    .take(MAX_STDERR_LINE)
                    .read_until(b'\n', &mut buf)
                    .await
                {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let line = String::from_utf8_lossy(&buf).trim_end().to_string();
                        if line.is_empty() {
                            continue;
                        }
                        eprintln!("[{tag}] {line}");
                        sink.push(line);
                    }
                }
            }
        });
        (tail, task)
    }

    fn push(&self, line: String) {
        let mut q = self.0.lock().unwrap_or_else(|e| e.into_inner());
        q.push_back(line);
        while q.len() > STDERR_TAIL_LINES {
            q.pop_front();
        }
    }

    /// The last `n` lines, joined for a message.
    fn last(&self, n: usize) -> String {
        let q = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let skip = q.len().saturating_sub(n);
        q.iter().skip(skip).cloned().collect::<Vec<_>>().join(" | ")
    }
}

/// The server processes spawned and not yet reaped — what a quit waits for
/// ([`all_servers_reaped`]) so that the teardown below gets its grace period.
///
/// The grace wait runs as a task on the app's runtime, and a quit that exits
/// right after closing the servers' stdin shuts that runtime down: the task is
/// dropped with it, and `kill_on_drop` SIGKILLs every server a moment after
/// its EOF — before it could reap its language servers, which is the whole
/// point of the grace. The quit path therefore waits here (bounded by its own
/// deadline) before exiting.
pub(crate) struct Reaper {
    live: Mutex<usize>,
    changed: tokio::sync::Notify,
}

impl Reaper {
    pub(crate) fn new() -> Arc<Reaper> {
        Arc::new(Reaper {
            live: Mutex::new(0),
            changed: tokio::sync::Notify::new(),
        })
    }

    /// Count one more live server; it counts until the guard is dropped.
    fn track(self: &Arc<Reaper>) -> LiveServer {
        *self.live.lock().unwrap_or_else(|e| e.into_inner()) += 1;
        LiveServer(self.clone())
    }

    /// Resolves once no server this reaper tracks is alive.
    pub(crate) async fn all_reaped(&self) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            // Registered before the check, so a reap between the check and
            // the wait is not missed.
            changed.as_mut().enable();
            if *self.live.lock().unwrap_or_else(|e| e.into_inner()) == 0 {
                return;
            }
            changed.await;
        }
    }
}

/// One live server of a [`Reaper`]: dropped once the process is reaped.
struct LiveServer(Arc<Reaper>);

impl Drop for LiveServer {
    fn drop(&mut self) {
        *self.0.live.lock().unwrap_or_else(|e| e.into_inner()) -= 1;
        self.0.changed.notify_waiters();
    }
}

/// The process-wide [`Reaper`] every transport's server is tracked by.
fn servers() -> &'static Arc<Reaper> {
    static SERVERS: std::sync::OnceLock<Arc<Reaper>> = std::sync::OnceLock::new();
    SERVERS.get_or_init(Reaper::new)
}

/// Resolves once every clew-server this process spawned has exited and been
/// reaped — the step a quit takes between releasing its transports and
/// exiting (see [`Reaper`]).
pub(crate) async fn all_servers_reaped() {
    servers().all_reaped().await;
}

/// The server process, torn down gracefully when the transport ends or is
/// dropped: its stdin is closed first (the writer is told to stop and drops
/// it), so the server sees EOF and can stop what it spawned; only if it has
/// not exited within its grace ([`TEARDOWN_GRACE`]) is it killed. A bare
/// SIGKILL — what `kill_on_drop` alone did — gave it no chance to reap its
/// language servers and processes, which were left running. `kill_on_drop`
/// stays set as the last resort for a runtime that is shutting down.
struct ServerChild {
    child: Option<Child>,
    stop_writer: Option<tokio::sync::oneshot::Sender<()>>,
    grace: Duration,
    /// Counts the process as live until it is reaped (see [`Reaper`]).
    live: Option<LiveServer>,
}

impl Drop for ServerChild {
    fn drop(&mut self) {
        drop(self.stop_writer.take());
        let live = self.live.take();
        let Some(mut child) = self.child.take() else {
            return;
        };
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                let grace = self.grace;
                runtime.spawn(async move {
                    if tokio::time::timeout(grace, child.wait()).await.is_err() {
                        let _ = child.kill().await;
                    }
                    drop(live);
                });
            }
            Err(_) => {
                let _ = child.start_kill();
                drop(live);
            }
        }
    }
}

/// A started server: its process, its stdio, and its stderr tail.
struct Transport {
    child: ServerChild,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    stderr: StderrTail,
    stderr_task: tokio::task::JoinHandle<()>,
}

impl Transport {
    /// Spawn `cmd` with piped stdio and start draining its stderr.
    fn spawn(cmd: tokio::process::Command, tag: &'static str) -> Result<Transport, String> {
        Transport::spawn_with(cmd, tag, servers(), TEARDOWN_GRACE)
    }

    /// [`Transport::spawn`], tracked by `reaper` and torn down with `grace`.
    fn spawn_with(
        mut cmd: tokio::process::Command,
        tag: &'static str,
        reaper: &Arc<Reaper>,
        grace: Duration,
    ) -> Result<Transport, String> {
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| e.to_string())?;
        let stdin = child.stdin.take().ok_or("no stdin pipe")?;
        let stdout = child.stdout.take().ok_or("no stdout pipe")?;
        let stderr = child.stderr.take().ok_or("no stderr pipe")?;
        let (tail, task) = StderrTail::spawn(stderr, tag);
        Ok(Transport {
            child: ServerChild {
                child: Some(child),
                stop_writer: None,
                grace,
                live: Some(reaper.track()),
            },
            stdin,
            stdout: BufReader::new(stdout),
            stderr: tail,
            stderr_task: task,
        })
    }

    /// The server's last stderr lines, after giving the drain a moment to
    /// catch up with a process that just exited.
    async fn stderr_tail(&mut self, n: usize) -> String {
        let _ = tokio::time::timeout(Duration::from_millis(500), &mut self.stderr_task).await;
        self.stderr.last(n)
    }
}

/// Start the local clew-server child (`server`: from [`server_bin_path`]).
///
/// `--local`, explicitly: left to guess from its environment, a server
/// started by a clew that was itself launched from an SSH session inherits
/// `SSH_CONNECTION`, classifies itself as REMOTE and refuses the
/// `SpawnProcess` requests local language servers and debug adapters need.
fn start_local(server: Option<&Path>) -> Result<Transport, Failure> {
    let Some(path) = server else {
        return Err(Failure::new(
            FailureKind::NoServer,
            "No clew-server binary was found beside clew or on PATH; working without it.",
        ));
    };
    let mut cmd = tokio::process::Command::new(path);
    cmd.arg("--local");
    Transport::spawn(cmd, "clew-server").map_err(|e| {
        Failure::new(
            FailureKind::NoServer,
            format!("Could not start {}: {e}", path.display()),
        )
    })
}

/// Bootstrap, deploy if needed, and start the remote clew-server, returning
/// once it is past its start marker (its stdout is protocol from there on).
async fn start_remote(ssh: &Ssh) -> Result<Transport, Failure> {
    let layout = remote_layout()?;
    bootstrap_remote(ssh, &layout, |platform| deploy_bytes(&platform)).await?;
    let marker = format!("CLEW-READY-{}", unique_token());
    let mut transport = Transport::spawn(
        ssh.command(&sh_c(&start_script(&layout, &marker))?),
        "clew-server (remote)",
    )
    .map_err(|e| Failure::new(FailureKind::Other, format!("could not run ssh: {e}")))?;
    match tokio::time::timeout(
        START_TIMEOUT,
        skip_to_marker(&mut transport.stdout, &marker),
    )
    .await
    {
        Ok(Ok(())) => Ok(transport),
        Ok(Err(e)) => {
            let tail = transport.stderr_tail(3).await;
            let detail = if tail.is_empty() {
                e
            } else {
                format!("{e}: {tail}")
            };
            Err(Failure::new(
                FailureKind::Exited,
                format!("clew-server on {} did not start: {detail}", ssh.label),
            ))
        }
        Err(_) => Err(Failure::new(
            FailureKind::Timeout,
            format!(
                "clew-server on {} did not start within {} s",
                ssh.label,
                START_TIMEOUT.as_secs()
            ),
        )),
    }
}

/// The transport's writer: client requests to the server's stdin, one NDJSON
/// line each. It ends — shutting stdin, which is the server's EOF — when the
/// client drops its sender, a write fails, or `stop` fires (the transport is
/// torn down). Stopped, it first writes what was already queued: the
/// requests a closing window sent last — its bookmark and note edits, the
/// stops of its streams — reach the server ahead of the EOF. Picking the stop
/// over a queued request, as a plain `select!` between them may, dropped
/// them.
async fn write_requests(
    mut rx: tokio::sync::mpsc::Receiver<ClientMessage>,
    mut stop: tokio::sync::oneshot::Receiver<()>,
    mut stdin: impl tokio::io::AsyncWrite + Unpin,
) {
    let mut stopped = false;
    loop {
        let msg = if stopped {
            rx.try_recv().ok()
        } else {
            tokio::select! {
                msg = rx.recv() => msg,
                _ = &mut stop => {
                    stopped = true;
                    rx.try_recv().ok()
                }
            }
        };
        let Some(msg) = msg else { break };
        let Ok(mut json) = serde_json::to_string(&msg) else {
            continue;
        };
        json.push('\n');
        if stdin.write_all(json.as_bytes()).await.is_err() {
            break;
        }
        if stdin.flush().await.is_err() {
            break;
        }
    }
    let _ = stdin.shutdown().await;
}

/// Start the transport for `target`, turning an unknown-host-key refusal into
/// the full message with the host's fingerprints.
async fn start(target: &ConnTarget, launcher: &Launcher) -> Result<Transport, Failure> {
    match target {
        ConnTarget::Local => start_local(launcher.local_server.as_deref()),
        ConnTarget::Ssh { label, args } => {
            let ssh = Ssh::for_target(label, args, &launcher.ssh, launcher.control_args()).await;
            match start_remote(&ssh).await {
                Err(f) if f.kind == FailureKind::HostKeyUnknown => {
                    let (reason, host_key) =
                        describe_unknown_host(&ssh, &launcher.keyscan, &launcher.keygen).await;
                    Err(Failure {
                        reason,
                        host_key: host_key.map(Box::new),
                        ..f
                    })
                }
                other => other,
            }
        }
    }
}

/// Plain `fn(&ConnKey)` (no captures) as `Subscription::run_with` requires;
/// the key arrives by reference and is cloned into the async body. `use<>`
/// opts the returned stream out of capturing the input lifetime (it doesn't
/// borrow — the clone is owned), so the type matches `fn(&D) -> S`.
fn stream(key: &ConnKey) -> impl Stream<Item = Message> + use<> {
    run_transport(key.clone(), Launcher::system())
}

/// One transport instance for `key`, started with `launcher`'s programs.
fn run_transport(key: ConnKey, launcher: Launcher) -> impl Stream<Item = Message> {
    let ConnKey {
        target,
        seq: conn,
        respawn,
    } = key;
    iced::stream::channel(
        256,
        move |mut output: iced::futures::channel::mpsc::Sender<Message>| async move {
            let mut transport = if respawn {
                // A restart after a death: back off, retry what can succeed
                // later, and stop — with the reason — after MAX_ATTEMPTS
                // consecutive failures instead of looping forever.
                loop {
                    let (count, last) = failures(&target);
                    if count >= MAX_ATTEMPTS {
                        let reason = format!(
                            "Stopped reconnecting to {} after {count} failed attempts. Last \
                             error: {last}",
                            target.label()
                        );
                        let _ = output
                            .send(Message::Server(ServerMsg::Unavailable { conn, reason }))
                            .await;
                        return;
                    }
                    tokio::time::sleep(backoff_delay(launcher.backoff_base, count)).await;
                    match start(&target, &launcher).await {
                        Ok(t) => break t,
                        Err(f) => {
                            eprintln!("[clew] transport: {}", f.reason);
                            record_failure(&target, &f);
                            if !f.retryable() {
                                let _ = output.send(f.into_message(conn)).await;
                                return;
                            }
                        }
                    }
                }
            } else {
                // The user asked for this connection: a fresh count, no
                // delay, and a failure is shown rather than retried.
                reset_failures(&target);
                match start(&target, &launcher).await {
                    Ok(t) => t,
                    Err(f) => {
                        eprintln!("[clew] transport: {}", f.reason);
                        record_failure(&target, &f);
                        let _ = output.send(f.into_message(conn)).await;
                        return;
                    }
                }
            };

            // Hand the client the request end; if the app is already gone, stop.
            let (tx, rx) = tokio::sync::mpsc::channel::<ClientMessage>(REQUEST_QUEUE);
            if output
                .send(Message::Server(ServerMsg::Connected { conn, tx }))
                .await
                .is_err()
            {
                return;
            }

            // Writer: client requests -> server stdin (see `write_requests`).
            let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
            transport.child.stop_writer = Some(stop_tx);
            tokio::spawn(write_requests(rx, stop_rx, transport.stdin));

            // Reader: server stdout -> `ServerMsg::Event` messages, one per NDJSON
            // line, each capped at the protocol frame limit — an over-cap
            // "line" (a broken or hostile server) ends the transport instead
            // of growing client memory without bound. Until the first frame
            // arrives the wait is bounded too: a server that never answers
            // the Hello is a failed connection, not "Connecting…" forever.
            let mut reader = transport.stdout;
            let mut client_gone = false;
            // When the first frame arrived: how long this transport was
            // healthy decides whether its end counts as a failure.
            let mut answered_at: Option<tokio::time::Instant> = None;
            let mut ended_because: Option<String> = None;
            let first_frame_deadline = tokio::time::Instant::now() + FIRST_FRAME_TIMEOUT;
            loop {
                let answered = answered_at.is_some();
                let next = if answered {
                    clew_core::framing::read_protocol_line(&mut reader).await
                } else {
                    match tokio::time::timeout_at(
                        first_frame_deadline,
                        clew_core::framing::read_protocol_line(&mut reader),
                    )
                    .await
                    {
                        Ok(next) => next,
                        Err(_) => {
                            ended_because = Some(format!(
                                "clew-server did not answer within {} s",
                                FIRST_FRAME_TIMEOUT.as_secs()
                            ));
                            break;
                        }
                    }
                };
                match next {
                    Some(line) if !line.is_empty() => {
                        match serde_json::from_str::<ServerMessage>(&line) {
                            Ok(msg) => {
                                // Answering is not yet health: the count is
                                // only reset once it has stayed up a while
                                // (below).
                                answered_at.get_or_insert_with(tokio::time::Instant::now);
                                if output
                                    .send(Message::Server(ServerMsg::Event { conn, msg }))
                                    .await
                                    .is_err()
                                {
                                    client_gone = true;
                                    break;
                                }
                            }
                            // Fail closed: an unparseable frame means the
                            // server's protocol build differs (or the stream
                            // is corrupt). Skipping it silently dropped an
                            // unknowable subset of events; ending the
                            // transport surfaces the mismatch immediately
                            // (the reconnect handshake names it).
                            Err(e) => {
                                eprintln!("[clew] unparseable server frame ({e}) — disconnecting");
                                ended_because =
                                    Some(format!("clew-server sent a malformed frame ({e})"));
                                break;
                            }
                        }
                    }
                    Some(_) => {}  // blank keep-alive line
                    None => break, // EOF, read error, or over-cap frame
                }
            }
            // A transport that stayed up for `healthy_after` was a working
            // connection: whatever ended it, the restart starts the count
            // afresh. One that never answered — or answered and then died
            // soon after (a crash right after the handshake, a malformed
            // frame) — counts against this target, so the restart that
            // follows backs off and eventually stops.
            if !client_gone {
                let uptime = answered_at.map(|at| at.elapsed());
                if uptime.is_some_and(|up| up >= launcher.healthy_after) {
                    reset_failures(&target);
                } else {
                    let tail = {
                        let _ = tokio::time::timeout(
                            Duration::from_millis(500),
                            &mut transport.stderr_task,
                        )
                        .await;
                        transport.stderr.last(3)
                    };
                    let why = ended_because.clone().unwrap_or_else(|| match uptime {
                        None => "clew-server exited before it answered".into(),
                        Some(up) => {
                            format!("clew-server exited {} s after it answered", up.as_secs())
                        }
                    });
                    let reason = if tail.is_empty() {
                        why
                    } else {
                        format!("{why}: {tail}")
                    };
                    eprintln!("[clew] transport: {reason}");
                    record_failure(&target, &Failure::new(FailureKind::Exited, reason));
                }
            }
            // The server died mid-session: tell the client, so it can clear
            // its in-flight bookkeeping and bump the generation to reconnect —
            // with the reason when this side ended it (a frame that did not
            // decode is otherwise indistinguishable from a crash).
            // (Skipped when the *client* went away — the app is closing.)
            if !client_gone {
                let _ = output
                    .send(Message::Server(ServerMsg::Disconnected {
                        conn,
                        reason: ended_because,
                    }))
                    .await;
            }
            // Held to here; dropping it closes stdin and reaps the server.
            drop(transport.child);
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// Each host-key failure reaches the app as the prompt it can offer: a
    /// scanned unknown key as the trust prompt, a changed key clew recorded
    /// itself as the offer to forget it (with the name to forget), anything
    /// else — a changed key on record in the user's own files included — as
    /// a plain failure.
    #[test]
    fn host_key_failures_reach_the_app_as_the_prompt_they_offer() {
        let changed = Failure {
            forget_host: Some("[example.com]:2222".into()),
            ..Failure::new(FailureKind::HostKeyChanged, "changed")
        };
        assert!(matches!(
            changed.into_message(7),
            Message::Server(ServerMsg::HostKeyChanged { conn: 7, reason, forget_host })
                if reason == "changed" && forget_host == "[example.com]:2222"
        ));
        let users_own = Failure::new(FailureKind::HostKeyChanged, "theirs");
        assert!(matches!(
            users_own.into_message(7),
            Message::Server(ServerMsg::Unavailable { conn: 7, reason }) if reason == "theirs"
        ));
        let unknown = Failure {
            host_key: Some(Box::new(crate::connect::ScannedHostKey {
                host: "[example.com]:2222".into(),
                line: "[example.com]:2222 ssh-ed25519 AAAA".into(),
                kind: "ED25519".into(),
                fingerprint: "SHA256:x".into(),
            })),
            ..Failure::new(FailureKind::HostKeyUnknown, "unknown")
        };
        assert!(matches!(
            unknown.into_message(7),
            Message::Server(ServerMsg::HostKeyUnknown { conn: 7, .. })
        ));
    }

    /// A fresh, empty directory for the calling test (emptied again when
    /// asked for twice), removed when the test ends.
    fn temp(name: &str) -> PathBuf {
        let dir = crate::app::tests::test_dir(&format!("server-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A fake `ssh`: records every invocation's argv (one argument per line,
    /// then a `--END--` line) and stdin, and answers by recognizing the remote
    /// command — it never executes it. A file `installed` in `dir` makes the
    /// probe find this build; `extra` is spliced in for failure scenarios.
    /// `ssh -G` prints `dir/g.out` (a plain `[example.com]:2222` setup by
    /// default), or fails like ssh when `dir/g.fail` exists.
    fn fake_ssh(dir: &Path, probe_line: &str, extra: &str) -> PathBuf {
        let script = format!(
            r#"#!/bin/bash
[ "$1" = --clew-warm-up ] && exit 0
D='{dir}'
for a in "$@"; do printf '%s\n' "$a" >> "$D/argv.log"; done
printf -- '--END--\n' >> "$D/argv.log"
if [ "$1" = "-G" ]; then
  if [ -e "$D/g.fail" ]; then cat "$D/g.fail" >&2; exit 255; fi
  if [ -e "$D/g.out" ]; then cat "$D/g.out"; exit 0; fi
  printf 'hostname example.com\nport 2222\nuserknownhostsfile /nonexistent-clew-test/known_hosts\n'
  exit 0
fi
CMD="${{@: -1}}"
{extra}
case "$CMD" in
  *"cat >"*) cat > "$D/deployed.bin"; touch "$D/installed"; exit 0 ;;
  "uname -sm") echo "Linux x86_64"; exit 0 ;;
  *CLEW-READY-*)
    M=$(printf '%s' "$CMD" | grep -o 'CLEW-READY-[0-9a-f-]*')
    echo "Welcome to Ubuntu"; echo "You have new mail."
    printf '%s\n' "$M"
    printf '%s\n' '{{"Notification":{{"sub":null,"event":{{"Status":{{"message":"hi"}}}}}}}}'
    exit 0 ;;
  *"--version"*) if [ -e "$D/installed" ]; then echo '{probe_line}'; fi; exit 0 ;;
esac
exit 3
"#,
            dir = dir.display(),
        );
        let path = dir.join("ssh");
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        warm_up(&path);
        path
    }

    /// Run a freshly written fake tool once, with `--clew-warm-up` (every
    /// fake exits at once on it, recording nothing). macOS checks a new
    /// executable on its first run, and under a loaded test run that check
    /// took up to eight seconds — enough to trip the production timeouts the
    /// fakes run under (a 10 s fingerprint, a 10 s `ssh -G`). Paid here, it is
    /// paid before anything is timed.
    fn warm_up(path: &Path) {
        let _ = std::process::Command::new(path)
            .arg("--clew-warm-up")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }

    /// An invocation of `program` for `root@example.com:2222`, built without
    /// running `ssh -G` (these tests are about the remote steps).
    fn fake(program: PathBuf) -> Ssh {
        Ssh {
            program,
            prefix: vec!["-T".into()],
            target: port_2222_args(),
            label: "root@example.com:2222".into(),
            settings: Err("not resolved in this test".into()),
            clew_known_hosts: None,
        }
    }

    /// Every recorded invocation's argv.
    fn calls(dir: &Path) -> Vec<Vec<String>> {
        let log = std::fs::read_to_string(dir.join("argv.log")).unwrap_or_default();
        log.split("--END--\n")
            .filter(|c| !c.is_empty())
            .map(|c| c.lines().map(str::to_string).collect())
            .collect()
    }

    fn layout() -> RemoteLayout {
        remote_layout().unwrap()
    }

    /// First contact: probe, detect, stream the verified bytes, re-probe. Every
    /// remote command is ONE argument after the destination, behind `-T` and
    /// `--`, and the upload goes to a temp name unique to this upload.
    #[tokio::test]
    async fn a_fresh_host_gets_the_verified_bytes_streamed_to_a_unique_temp_name() {
        let dir = temp("bootstrap");
        let l = layout();
        let ssh = fake(fake_ssh(&dir, &l.probe_line, ""));
        let payload = b"\x7fELF not really a server".to_vec();
        let sent = payload.clone();
        bootstrap_remote(&ssh, &l, move |platform| {
            assert_eq!(platform, "Linux x86_64");
            Ok(sent)
        })
        .await
        .unwrap();

        assert_eq!(
            std::fs::read(dir.join("deployed.bin")).unwrap(),
            payload,
            "exactly the obtained bytes must be streamed"
        );
        let calls = calls(&dir);
        assert_eq!(
            calls.len(),
            4,
            "probe, uname, install, re-probe: {calls:#?}"
        );
        for argv in &calls {
            assert_eq!(argv[0], "-T", "no pty, whatever the user's config says");
            let dd = argv
                .iter()
                .position(|a| a == "--")
                .expect("`--` before the host");
            assert_eq!(argv[dd + 1], "root@example.com");
            assert_eq!(argv.len(), dd + 3, "the remote command is one argument");
        }
        let install = &calls[2][calls[2].len() - 1];
        assert!(install.starts_with("sh -c '") && install.ends_with('\''));
        let token = format!("clew-server.{}-", std::process::id());
        assert!(install.contains(&token), "unique temp name: {install}");
        assert!(install.contains("mv -f"), "renamed into place: {install}");
        assert!(install.contains(&l.dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An already-deployed build costs one call, and nothing is uploaded.
    #[tokio::test]
    async fn a_deployed_build_is_reused_after_one_probe() {
        let dir = temp("reuse");
        let l = layout();
        let ssh = fake(fake_ssh(&dir, &l.probe_line, ""));
        std::fs::write(dir.join("installed"), "").unwrap();
        bootstrap_remote(&ssh, &l, |_| panic!("nothing should be fetched"))
            .await
            .unwrap();
        assert_eq!(calls(&dir).len(), 1);
        let probe = &calls(&dir)[0];
        let cmd = probe.last().unwrap();
        assert!(
            cmd.contains("--version") && cmd.contains("-mtime +14"),
            "{cmd}"
        );
        assert!(
            cmd.contains(&format!("! -name \"{}\"", l.dir)),
            "the build in use must be excluded from the sweep: {cmd}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ssh's own failures are named, not collapsed into "could not reach".
    #[tokio::test]
    async fn ssh_failures_are_classified_from_stderr() {
        for (stderr, kind) in [
            (
                "@@@@@@@@\n@    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @\n",
                FailureKind::HostKeyChanged,
            ),
            (
                "No ED25519 host key is known for example.com and you have requested strict checking.\nHost key verification failed.\n",
                FailureKind::HostKeyUnknown,
            ),
            (
                "root@example.com: Permission denied (publickey).\n",
                FailureKind::Auth,
            ),
            (
                "ssh: connect to host example.com port 2222: Connection refused\n",
                FailureKind::Unreachable,
            ),
        ] {
            let dir = temp("classify");
            let l = layout();
            let extra = format!("printf '%s' '{stderr}' >&2; exit 255");
            let ssh = fake(fake_ssh(&dir, &l.probe_line, &extra));
            let err = bootstrap_remote(&ssh, &l, |_| panic!("no fetch"))
                .await
                .unwrap_err();
            assert_eq!(err.kind, kind, "{stderr:?} → {}", err.reason);
            assert!(
                err.reason.contains("root@example.com:2222"),
                "{}",
                err.reason
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    fn captured(code: i32, stderr: &str) -> Captured {
        Captured {
            success: code == 0,
            code: Some(code),
            stdout: String::new(),
            stderr: stderr.into(),
        }
    }

    /// Only exit status 255 is ssh's own failure. The same words from the
    /// REMOTE command — a `mkdir` refused, a script that prints ssh-like
    /// text — are that command's failure, never a refused login or a host
    /// key problem.
    #[test]
    fn only_exit_255_is_read_as_ssh_failing() {
        let ssh = fake(PathBuf::from("/nonexistent/ssh"));
        for (stderr, as_ssh) in [
            (
                "root@example.com: Permission denied (publickey).",
                FailureKind::Auth,
            ),
            (
                "Received disconnect: Too many authentication failures",
                FailureKind::Auth,
            ),
            (
                "WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!",
                FailureKind::HostKeyChanged,
            ),
            ("Host key verification failed.", FailureKind::HostKeyUnknown),
        ] {
            assert_eq!(
                ssh_failure("probing the host", &ssh, &captured(255, stderr)).kind,
                as_ssh,
                "{stderr}"
            );
            let remote = ssh_failure("probing the host", &ssh, &captured(1, stderr));
            assert_eq!(
                remote.kind,
                FailureKind::Other,
                "{stderr} → {}",
                remote.reason
            );
            assert!(
                remote.reason.contains("probing the host failed"),
                "{}",
                remote.reason
            );
        }
    }

    /// The finding, end to end: the install step's remote `mkdir` refused
    /// ("Permission denied", exit 1) is a failed DEPLOY that names the
    /// directory problem — it used to read as a refused login.
    #[tokio::test]
    async fn a_remote_permission_denied_is_a_failed_deploy_not_a_refused_login() {
        let dir = temp("remote-denied");
        let l = layout();
        let extra = "case \"$CMD\" in *\"cat >\"*) \
                     echo \"mkdir: cannot create directory '/root/.clew': Permission denied\" >&2; \
                     exit 1 ;; esac";
        let ssh = fake(fake_ssh(&dir, &l.probe_line, extra));
        let err = bootstrap_remote(&ssh, &l, |_| Ok(b"\x7fELF".to_vec()))
            .await
            .unwrap_err();
        assert_eq!(err.kind, FailureKind::Deploy, "{}", err.reason);
        assert!(
            err.reason.contains("Could not install clew-server")
                && err.reason.contains("Permission denied"),
            "{}",
            err.reason
        );
        assert!(!err.reason.contains("refused the login"), "{}", err.reason);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A deployed binary that does not speak this build's protocol fails the
    /// connect with that named, instead of as a handshake that never ends.
    #[tokio::test]
    async fn a_deploy_that_fails_the_protocol_check_is_named() {
        let dir = temp("mismatch");
        let l = layout();
        let ssh = fake(fake_ssh(&dir, "clew-server protocol 1 fingerprint 0", ""));
        let err = bootstrap_remote(&ssh, &l, |_| Ok(b"x".to_vec()))
            .await
            .unwrap_err();
        assert_eq!(err.kind, FailureKind::Protocol, "{}", err.reason);
        assert!(!err.retryable());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A step that hangs is killed at its timeout rather than hanging the
    /// connection forever.
    #[tokio::test]
    async fn a_hanging_remote_command_times_out() {
        let dir = temp("hang");
        let l = layout();
        // `exec`, so the kill at the timeout ends the sleep too (no orphan).
        let ssh = fake(fake_ssh(&dir, &l.probe_line, "exec sleep 30"));
        let out = run_bounded(
            ssh.command("uname -sm"),
            None,
            Duration::from_millis(300),
            "detecting the platform",
            &ssh.label,
        )
        .await;
        match out {
            Err(f) => assert_eq!(f.kind, FailureKind::Timeout),
            Ok(_) => panic!("a hanging command must time out"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// What a login shell prints before the server starts (a motd, a
    /// `~/.bashrc` echo) is skipped up to the marker; the first line after it
    /// is protocol.
    #[tokio::test]
    async fn output_before_the_start_marker_is_not_protocol() {
        let dir = temp("marker");
        let l = layout();
        let ssh = fake(fake_ssh(&dir, &l.probe_line, ""));
        let marker = format!("CLEW-READY-{}", unique_token());
        let mut t = Transport::spawn(
            ssh.command(&sh_c(&start_script(&l, &marker)).unwrap()),
            "test",
        )
        .unwrap();
        skip_to_marker(&mut t.stdout, &marker).await.unwrap();
        let frame = clew_core::framing::read_protocol_line(&mut t.stdout)
            .await
            .unwrap();
        assert!(
            serde_json::from_str::<ServerMessage>(&frame).is_ok(),
            "{frame}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_marker_that_never_comes_is_an_error_not_a_hang() {
        let mut eof: &[u8] = b"motd line\nanother\n";
        let err = skip_to_marker(&mut eof, "CLEW-READY-x").await.unwrap_err();
        assert!(err.contains("closed before clew-server started"), "{err}");
        assert!(err.contains("motd line"), "what it printed is shown: {err}");
        let flood = vec![b'x'; MAX_PRE_MARKER_BYTES + 10];
        let mut flood: &[u8] = &flood;
        let err = skip_to_marker(&mut flood, "CLEW-READY-x")
            .await
            .unwrap_err();
        assert!(err.contains("more than"), "{err}");
    }

    /// Run one of the remote scripts for real, with `/bin/sh` and `home` as
    /// `$HOME` (set on the child only — the test process's environment is
    /// untouched).
    fn run_remote_script(script: &str, home: &Path, stdin: &[u8]) -> std::process::Output {
        use std::io::Write;
        let mut child = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(script)
            .env("HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(stdin).unwrap();
        child.wait_with_output().unwrap()
    }

    fn set_mtime_days_ago(path: &Path, days: u64) {
        let when = std::time::SystemTime::now() - Duration::from_secs(days * 24 * 60 * 60);
        std::fs::File::open(path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(when))
            .unwrap();
    }

    /// The probe finds this build, and its sweep removes only other builds'
    /// directories that nobody used for two weeks — never the current one,
    /// never a recently used one, never anything not named like a build.
    #[test]
    fn the_probe_script_sweeps_only_stale_builds() {
        let home = temp("probe-home");
        let l = layout();
        let root = home.join(".clew/server");
        let current = root.join(&l.dir);
        std::fs::create_dir_all(&current).unwrap();
        let server = current.join("clew-server");
        std::fs::write(&server, format!("#!/bin/sh\necho '{}'\n", l.probe_line)).unwrap();
        std::fs::set_permissions(&server, std::fs::Permissions::from_mode(0o700)).unwrap();
        set_mtime_days_ago(&current, 60);
        let stale = root.join("0.0.1-p1-deadbeef");
        let recent = root.join("0.0.2-p2-cafef00d");
        let unrelated = root.join("notes");
        for d in [&stale, &recent, &unrelated] {
            std::fs::create_dir_all(d).unwrap();
        }
        set_mtime_days_ago(&stale, 30);
        set_mtime_days_ago(&unrelated, 30);
        let old_tmp = current.join("clew-server.1-abc.tmp");
        std::fs::write(&old_tmp, b"partial").unwrap();
        set_mtime_days_ago(&old_tmp, 3);
        set_mtime_days_ago(&current, 60);

        let out = run_remote_script(&probe_script(&l), &home, b"");
        assert!(out.status.success(), "{out:?}");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.lines().any(|x| x.trim() == l.probe_line), "{stdout}");
        assert!(!stale.exists(), "an unused old build is swept");
        assert!(
            current.exists() && server.exists(),
            "the build in use stays"
        );
        assert!(recent.exists(), "a recently used build stays");
        assert!(unrelated.exists(), "only build-shaped names are swept");
        assert!(!old_tmp.exists(), "an abandoned upload is removed");
        let age = std::fs::metadata(&current).unwrap().modified().unwrap();
        assert!(
            age.elapsed().unwrap() < Duration::from_secs(3600),
            "the probe marks the build in use as used"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// The install script puts exactly the streamed bytes in place, private
    /// to the user, with no temp file left over — and cleans up after itself
    /// when the directory cannot be written.
    #[test]
    fn the_install_script_renames_a_complete_upload_into_place() {
        let home = temp("install-home");
        let l = layout();
        let out = run_remote_script(&install_script(&l, "7-feed"), &home, b"binary bytes");
        assert!(out.status.success(), "{out:?}");
        let dir = home.join(".clew/server").join(&l.dir);
        let installed = dir.join("clew-server");
        assert_eq!(std::fs::read(&installed).unwrap(), b"binary bytes");
        assert_eq!(
            std::fs::metadata(&installed).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let leftovers = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(leftovers, 0);

        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        let out = run_remote_script(&install_script(&l, "8-feed"), &home, b"other");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            !out.status.success(),
            "an unwritable directory must fail the install"
        );
        assert_eq!(std::fs::read(&installed).unwrap(), b"binary bytes");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// The start script prints its marker and then IS the server; without an
    /// installed server it says so on stderr instead of printing the marker.
    #[test]
    fn the_start_script_prints_the_marker_then_execs_the_server() {
        let home = temp("start-home");
        let l = layout();
        let missing = run_remote_script(&start_script(&l, "CLEW-READY-1"), &home, b"");
        assert!(!missing.status.success());
        assert!(!String::from_utf8_lossy(&missing.stdout).contains("CLEW-READY-1"));
        assert!(String::from_utf8_lossy(&missing.stderr).contains("not installed"));

        let dir = home.join(".clew/server").join(&l.dir);
        std::fs::create_dir_all(&dir).unwrap();
        let server = dir.join("clew-server");
        std::fs::write(&server, "#!/bin/sh\necho \"frame $*\"\n").unwrap();
        std::fs::set_permissions(&server, std::fs::Permissions::from_mode(0o700)).unwrap();
        let out = run_remote_script(&start_script(&l, "CLEW-READY-1"), &home, b"");
        assert!(out.status.success(), "{out:?}");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "CLEW-READY-1\nframe --remote\n",
            "the server is told it runs over SSH, not left to guess"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// The local server is told it is local. Left to its environment, one
    /// started by a clew that was itself launched from an SSH session saw
    /// `SSH_CONNECTION`, took itself for remote and refused the process
    /// spawns local language servers and debug adapters need.
    #[tokio::test]
    async fn the_local_server_is_told_it_is_local() {
        let dir = temp("local-flag");
        let server = script(&dir, "clew-server", "echo \"$*\"\n");
        let mut t = start_local(Some(&server)).unwrap_or_else(|f| panic!("{}", f.reason));
        assert_eq!(
            clew_core::framing::read_protocol_line(&mut t.stdout)
                .await
                .as_deref(),
            Some("--local")
        );
        let none = start_local(None).err().expect("no server, no transport");
        assert_eq!(none.kind, FailureKind::NoServer);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remote_scripts_are_single_quote_free_and_name_only_this_build() {
        let l = layout();
        for script in [
            probe_script(&l),
            install_script(&l, &unique_token()),
            start_script(&l, "CLEW-READY-1"),
            verify_script(&l),
        ] {
            assert!(sh_c(&script).is_ok(), "{script}");
            assert!(script.contains(&l.dir));
        }
        assert!(sh_c("echo 'x'").is_err());
        assert_ne!(unique_token(), unique_token());
    }

    /// Restarts back off exponentially to a cap, and the count survives the
    /// stream instance (it is per target) until a healthy transport or a user
    /// action resets it.
    #[test]
    fn restarts_back_off_and_stop() {
        assert_eq!(backoff_delay(BACKOFF_BASE, 0), Duration::from_millis(1500));
        assert_eq!(backoff_delay(BACKOFF_BASE, 1), Duration::from_secs(3));
        assert_eq!(backoff_delay(BACKOFF_BASE, 3), Duration::from_secs(12));
        assert_eq!(backoff_delay(BACKOFF_BASE, 9), BACKOFF_CAP);
        assert_eq!(backoff_delay(BACKOFF_BASE, u32::MAX), BACKOFF_CAP);

        let target = ConnTarget::Ssh {
            label: format!("backoff-test-{}", unique_token()),
            args: vec!["h".into()],
        };
        assert_eq!(failures(&target).0, 0);
        for i in 1..=MAX_ATTEMPTS {
            record_failure(
                &target,
                &Failure::new(FailureKind::Unreachable, format!("try {i}")),
            );
        }
        assert_eq!(
            failures(&target),
            (MAX_ATTEMPTS, format!("try {MAX_ATTEMPTS}"))
        );
        reset_failures(&target);
        assert_eq!(failures(&target).0, 0);
    }

    #[test]
    fn only_transient_failures_are_retried() {
        for kind in [
            FailureKind::HostKeyChanged,
            FailureKind::HostKeyUnknown,
            FailureKind::Auth,
            FailureKind::Deploy,
            FailureKind::Protocol,
            FailureKind::NoServer,
        ] {
            assert!(!Failure::new(kind, "").retryable(), "{kind:?}");
        }
        for kind in [
            FailureKind::Unreachable,
            FailureKind::Timeout,
            FailureKind::Exited,
        ] {
            assert!(Failure::new(kind, "").retryable(), "{kind:?}");
        }
    }

    /// Only bytes that hash to the digest they were verified against are
    /// uploaded (either case of hex is the same digest).
    #[test]
    fn only_bytes_matching_their_verified_digest_are_uploaded() {
        use clew_core::lsp::registry::Sha256;
        use clew_core::server_dist::VerifiedServer;
        let bytes = b"\x7fELF server".to_vec();
        let digest = Sha256::of(&bytes).to_hex();
        let ok = upload_bytes(VerifiedServer {
            bytes: bytes.clone(),
            sha256: digest.to_ascii_uppercase(),
        });
        assert_eq!(ok.unwrap(), bytes);
        let swapped = upload_bytes(VerifiedServer {
            bytes: b"something else".to_vec(),
            sha256: digest,
        });
        assert!(swapped.unwrap_err().contains("do not match"));
        let malformed = upload_bytes(VerifiedServer {
            bytes,
            sha256: "not a digest".into(),
        });
        assert!(malformed.is_err());
    }

    /// An executable script at `dir/name` (a fake OpenSSH tool).
    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\n[ \"$1\" = --clew-warm-up ] && exit 0\nD='{}'\n{body}",
                dir.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        warm_up(&path);
        path
    }

    const ED25519_KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIEd25519KeyForTestsOnly0000";
    const RSA_KEY: &str = "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABgQCtestonly00";

    /// A fake `ssh-keyscan` that records its argv and prints a banner, the
    /// keys of `scanned_as` (RSA first, then ED25519) and another host's key.
    fn fake_keyscan(dir: &Path, scanned_as: &str) -> PathBuf {
        script(
            dir,
            "ssh-keyscan",
            &format!(
                "printf '%s\\n' \"$@\" > \"$D/keyscan.argv\"\n\
                 echo '# {scanned_as} SSH-2.0-OpenSSH_9.6'\n\
                 echo '{scanned_as} {RSA_KEY}'\n\
                 echo '{scanned_as} {ED25519_KEY}'\n\
                 echo '[other.example]:2222 ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOtherHostKey0000'\n"
            ),
        )
    }

    /// A fake `ssh-keygen -lf -` that records what it was fed and prints the
    /// fingerprint line `answer` gives for that input's key type.
    fn fake_keygen(dir: &Path, ed25519_answer: &str) -> PathBuf {
        script(
            dir,
            "ssh-keygen",
            &format!(
                "IN=$(cat)\n\
                 printf '%s\\n--\\n' \"$IN\" >> \"$D/keygen.stdin\"\n\
                 case \"$IN\" in\n\
                   *ssh-ed25519*) echo '{ed25519_answer}' ;;\n\
                   *ssh-rsa*) echo '3072 SHA256:RsaPrintForTests [example.com]:2222 (RSA)' ;;\n\
                 esac\n"
            ),
        )
    }

    fn port_2222_args() -> Vec<String> {
        crate::connect::SavedConnection {
            name: String::new(),
            host: "example.com".into(),
            user: "root".into(),
            port: 2222,
            identity: String::new(),
            send_ai_keys: false,
        }
        .ssh_args()
    }

    /// Resolved settings for a direct connection to `hostname:port`.
    fn direct(hostname: &str, port: u16) -> crate::connect::SshSettings {
        crate::connect::SshSettings {
            hostname: hostname.into(),
            port,
            host_key_alias: None,
            proxy: None,
            user_known_hosts: Some(Vec::new()),
        }
    }

    /// Run `f` with `CLEW_DATA_DIR` at `data`, restored afterwards. The
    /// variable is process-wide, hence the shared lock; `f` is kept short and
    /// synchronous (no process is spawned under it), so the variable is moved
    /// for as little time as possible.
    fn in_data_dir<T>(data: &Path, f: impl FnOnce() -> T) -> T {
        let _env = crate::app::tests::data_dir_override(data);
        f()
    }

    /// A private data directory whose path has a space in it, like macOS's
    /// "Application Support".
    fn data_dir(base: &Path) -> PathBuf {
        let data = base.join("data dir");
        std::fs::create_dir_all(&data).unwrap();
        data
    }

    /// D2-4 / I6: the key offered for trust is the host's strongest one, and
    /// the fingerprint shown is `ssh-keygen -lf`'s for EXACTLY that line — it
    /// is fed nothing else — so the Trust click records the very key whose
    /// fingerprint the reader checked. The scan targets the connection's own
    /// host and port, behind `--`.
    #[tokio::test]
    async fn the_key_offered_for_trust_is_fingerprinted_on_its_own() {
        let dir = temp("keyscan");
        let keyscan = fake_keyscan(&dir, "[example.com]:2222");
        let keygen = fake_keygen(
            &dir,
            "256 SHA256:Ed25519PrintForTests [example.com]:2222 (ED25519)",
        );
        let settings = direct("example.com", 2222);
        let key = scan_host_key_with(&settings, "[example.com]:2222", &keyscan, &keygen)
            .await
            .expect("the host's key is offered");
        let line = format!("[example.com]:2222 {ED25519_KEY}");
        assert_eq!(key.host, "[example.com]:2222");
        assert_eq!(key.line, line, "the strongest key, not the first printed");
        assert_eq!(key.kind, "ED25519");
        assert_eq!(key.fingerprint, "SHA256:Ed25519PrintForTests");
        let fed = std::fs::read_to_string(dir.join("keygen.stdin")).unwrap();
        assert_eq!(fed, format!("{line}\n--\n"), "one line, one run");
        let argv = std::fs::read_to_string(dir.join("keyscan.argv")).unwrap();
        assert_eq!(
            argv.lines().collect::<Vec<_>>(),
            ["-T", "5", "-p", "2222", "--", "example.com"]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A fingerprint that does not belong to the key it was asked about (the
    /// wrong type, extra lines) is not shown for it: the next candidate is
    /// tried, and with none left nothing is offered.
    #[tokio::test]
    async fn a_fingerprint_for_another_key_type_is_never_offered() {
        let dir = temp("keyscan-mismatch");
        let keyscan = fake_keyscan(&dir, "[example.com]:2222");
        let keygen = fake_keygen(&dir, "3072 SHA256:NotEd25519 [example.com]:2222 (RSA)");
        let settings = direct("example.com", 2222);
        let key = scan_host_key_with(&settings, "[example.com]:2222", &keyscan, &keygen)
            .await
            .expect("the RSA key is still offered, fingerprinted as itself");
        assert_eq!(key.line, format!("[example.com]:2222 {RSA_KEY}"));
        assert_eq!(key.kind, "RSA");
        assert_eq!(key.fingerprint, "SHA256:RsaPrintForTests");

        let silent = script(&dir, "ssh-keygen-silent", "cat > /dev/null\n");
        assert_eq!(
            scan_host_key_with(&settings, "[example.com]:2222", &keyscan, &silent).await,
            None
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// I6 (b), the Trust loop: a host `~/.ssh/config` maps elsewhere is
    /// scanned where ssh actually connects (`HostName`, `Port`) and its key is
    /// recorded under the name ssh looks it up by — the resolved
    /// `[host]:port`, or the `HostKeyAlias` — for the connection it was
    /// scanned for. Recording it under the typed name, what this replaced, was
    /// never found by ssh, so the same prompt came back after every Trust.
    #[tokio::test]
    async fn a_mapped_host_is_scanned_where_ssh_connects_and_recorded_as_ssh_looks_it_up() {
        let dir = temp("mapped-scan");
        let keyscan = fake_keyscan(&dir, "[real.example.com]:2200");
        let keygen = fake_keygen(&dir, "256 SHA256:MappedPrint [x]:1 (ED25519)");
        let mapped = direct("Real.Example.com", 2200);
        let key = scan_host_key_with(&mapped, "prod", &keyscan, &keygen)
            .await
            .expect("offered");
        let argv = std::fs::read_to_string(dir.join("keyscan.argv")).unwrap();
        assert_eq!(
            argv.lines().collect::<Vec<_>>(),
            ["-T", "5", "-p", "2200", "--", "real.example.com"],
            "the endpoint ssh connects to"
        );
        assert_eq!(key.host, "prod", "offered for the connection as typed");
        assert_eq!(key.line, format!("[real.example.com]:2200 {ED25519_KEY}"));
        assert_eq!(
            key.line.split(' ').next(),
            Some(mapped.lookup_name().as_str()),
            "recorded under exactly the name ssh looks up"
        );
        // What the app does with a Trust click, for the target `prod`.
        let data = data_dir(&dir);
        let recorded = in_data_dir(&data, || crate::connect::trust_host_key(&key, "prod"));
        let file = recorded.unwrap();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            format!("{}\n", key.line)
        );

        // A HostKeyAlias is what ssh looks up instead of any host name, and
        // the fingerprinted line is the recorded one.
        let aliased = crate::connect::SshSettings {
            host_key_alias: Some("ProdKey".into()),
            ..mapped
        };
        let key = scan_host_key_with(&aliased, "prod", &keyscan, &keygen)
            .await
            .expect("offered");
        assert_eq!(key.line, format!("prodkey {ED25519_KEY}"));
        let fed = std::fs::read_to_string(dir.join("keygen.stdin")).unwrap();
        assert!(fed.contains(&format!("prodkey {ED25519_KEY}\n")), "{fed}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Behind a jump host (or a proxy command) what answers at the host's
    /// address from here need not be what ssh talks to, so nothing is scanned
    /// — the refusal says why and how to trust the host from a terminal.
    #[tokio::test]
    async fn a_host_behind_a_jump_host_is_never_scanned() {
        let dir = temp("jump");
        let keyscan = fake_keyscan(&dir, "[example.com]:2222");
        let keygen = fake_keygen(&dir, "256 SHA256:X [x]:1 (ED25519)");
        let mut ssh = fake(PathBuf::from("/nonexistent/ssh"));
        ssh.settings = Ok(crate::connect::SshSettings {
            proxy: Some(("ProxyJump", "admin@bastion".into())),
            ..direct("example.com", 2222)
        });
        ssh.clew_known_hosts = Some(dir.join("known_hosts"));
        let (reason, key) = describe_unknown_host(&ssh, &keyscan, &keygen).await;
        assert_eq!(key, None);
        assert!(!dir.join("keyscan.argv").exists(), "nothing was scanned");
        assert!(
            reason.contains("ProxyJump admin@bastion")
                && reason.contains("`ssh -p 2222 -- root@example.com`"),
            "{reason}"
        );
        // A14: the suggested command is quoted for the shell it is pasted
        // into, like the changed-key one.
        let plain = ssh.target.clone();
        ssh.target = vec!["--".into(), "o'brien@host $(x)".into()];
        let (reason, _) = describe_unknown_host(&ssh, &keyscan, &keygen).await;
        assert!(
            reason.contains(r#"`ssh -- 'o'\''brien@host $(x)'`"#),
            "{reason}"
        );
        ssh.target = plain;

        // Nor where clew's own known_hosts is not consulted (a trusted key
        // would never be read back), with the reason when the configuration
        // could not be read.
        ssh.settings = Err("`ssh -G` could not read the ssh configuration: bad line".into());
        ssh.clew_known_hosts = None;
        let (reason, key) = describe_unknown_host(&ssh, &keyscan, &keygen).await;
        assert_eq!(key, None);
        assert!(!dir.join("keyscan.argv").exists());
        assert!(reason.contains("bad line"), "{reason}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// I6 (a): every ssh clew runs for a target consults the known_hosts
    /// files the USER's configuration names for it — as `ssh -G` resolves
    /// them, not OpenSSH's defaults — with clew's appended, and asks ssh to
    /// write none of them. `ssh -G` sees everything the real calls pass
    /// except those options.
    #[tokio::test]
    async fn every_ssh_call_appends_clews_known_hosts_to_the_users_own() {
        let dir = temp("kh-list");
        let data = data_dir(&dir);
        std::fs::write(
            dir.join("g.out"),
            "hostname example.com\nport 2222\nuserknownhostsfile \
             /Users/me/.ssh/known_hosts_work /Users/me/.ssh/known_hosts\n",
        )
        .unwrap();
        let program = fake_ssh(&dir, "", "");
        let (label, args) = ("root@example.com:2222", port_2222_args());
        let options = vec!["-T".to_string()];
        let settings = resolve_settings(&program, &options, &args, label).await;
        let resolve = &calls(&dir)[0];
        assert_eq!(resolve[..2], ["-G", "-T"]);
        assert!(!resolve.iter().any(|a| a.contains("KnownHostsFile")));
        assert_eq!(resolve[resolve.len() - 2..], ["--", "root@example.com"]);

        let ssh = in_data_dir(&data, || {
            Ssh::assemble(label, &args, &program, options.clone(), settings)
        });
        let clew = data.join("known_hosts");
        assert_eq!(ssh.clew_known_hosts.as_deref(), Some(clew.as_path()));
        assert!(
            ssh.prefix.contains(&format!(
                "UserKnownHostsFile=\"/Users/me/.ssh/known_hosts_work\" \
                 \"/Users/me/.ssh/known_hosts\" \"{}\"",
                clew.display()
            )),
            "{:?}",
            ssh.prefix
        );
        assert!(ssh.prefix.iter().any(|a| a == "UpdateHostKeys=no"));
        assert!(ssh.prefix.iter().any(|a| a == "CheckHostIP=no"));
        // The target's own strict checking is untouched.
        assert!(
            ssh.target
                .windows(2)
                .any(|w| w == ["-o", "StrictHostKeyChecking=yes"])
        );

        // An unreadable configuration: the user's setting stays in effect
        // untouched, and nothing can be trusted from inside clew.
        std::fs::write(
            dir.join("g.fail"),
            "/Users/me/.ssh/config line 3: Bad option\n",
        )
        .unwrap();
        let ssh = Ssh::for_target(label, &args, &program, Vec::new()).await;
        assert_eq!(ssh.clew_known_hosts, None);
        assert!(!ssh.prefix.iter().any(|a| a.contains("KnownHostsFile")));
        assert!(ssh.prefix.iter().any(|a| a == "CheckHostIP=no"));
        assert!(
            ssh.settings.as_ref().unwrap_err().contains("Bad option"),
            "{:?}",
            ssh.settings
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ssh's changed-key warning as OpenSSH 10.2 prints it (captured against
    /// a real sshd), naming `offending` as the key on record.
    fn changed_key_stderr(offending: &str) -> String {
        format!(
            "@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\n\
             @    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @\n\
             @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\n\
             IT IS POSSIBLE THAT SOMEONE IS DOING SOMETHING NASTY!\n\
             Someone could be eavesdropping on you right now (man-in-the-middle attack)!\n\
             It is also possible that a host key has just been changed.\n\
             The fingerprint for the ED25519 key sent by the remote host is\n\
             SHA256:NewKeyPrintForTests.\n\
             Please contact your system administrator.\n\
             Add correct host key in /Users/me/.ssh/known_hosts to get rid of this message.\n\
             Offending ED25519 key in {offending}\n\
             Host key for [example.com]:2222 has changed and you have requested strict checking.\n\
             Host key verification failed.\n"
        )
    }

    #[test]
    fn the_changed_key_warning_is_read_back() {
        let report = ChangedKeyReport::parse(&changed_key_stderr(
            "/Users/me/Library/Application Support/clew/known_hosts:12",
        ));
        assert_eq!(
            report,
            ChangedKeyReport {
                presented: Some(("ED25519".into(), "SHA256:NewKeyPrintForTests".into())),
                offending: vec![(
                    "/Users/me/Library/Application Support/clew/known_hosts".into(),
                    12
                )],
                host: Some("[example.com]:2222".into()),
            }
        );
        assert_eq!(ChangedKeyReport::parse(""), ChangedKeyReport::default());
    }

    /// I6 (c): a key that CHANGED against one trusted in clew is refused with
    /// what ssh said — the key presented now, and the file and line of the
    /// key on record — and the way out is the forget action (and the
    /// `ssh-keygen -f <clew's file>` command that does the same; the bare
    /// `ssh-keygen -R <host>` this used to suggest never touches clew's
    /// file). Forgetting removes that host's key and nothing else; nothing is
    /// trusted by it.
    #[tokio::test]
    async fn a_key_changed_against_one_trusted_in_clew_can_be_forgotten() {
        let dir = temp("changed-clew");
        let data = data_dir(&dir);
        let clew = data.join("known_hosts");
        std::fs::write(
            &clew,
            format!("[example.com]:2222 {ED25519_KEY}\nother.example {ED25519_KEY}\n"),
        )
        .unwrap();
        std::fs::write(
            dir.join("stderr.txt"),
            changed_key_stderr(&format!("{}:1", clew.display())),
        )
        .unwrap();
        let mut ssh = fake(fake_ssh(&dir, "", "cat \"$D/stderr.txt\" >&2; exit 255"));
        ssh.settings = Ok(direct("example.com", 2222));
        ssh.clew_known_hosts = Some(clew.clone());
        let err = bootstrap_remote(&ssh, &layout(), |_| panic!("no fetch"))
            .await
            .unwrap_err();
        assert_eq!(err.kind, FailureKind::HostKeyChanged);
        assert!(!err.retryable());
        for part in [
            "It now presents ED25519 SHA256:NewKeyPrintForTests.".to_string(),
            format!("({}:1) is one you trusted in clew", clew.display()),
            format!(
                "`ssh-keygen -f '{}' -R '[example.com]:2222'`",
                clew.display()
            ),
        ] {
            assert!(
                err.reason.contains(&part),
                "{part:?} missing: {}",
                err.reason
            );
        }
        assert_eq!(err.forget_host.as_deref(), Some("[example.com]:2222"));

        // The Forget action.
        let host = err.forget_host.unwrap();
        let forgotten = in_data_dir(&data, || crate::connect::forget_host_key(&host));
        assert_eq!(forgotten, Ok(1));
        assert_eq!(
            std::fs::read_to_string(&clew).unwrap(),
            format!("other.example {ED25519_KEY}\n")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A key on record in the USER's own known_hosts is theirs to retire:
    /// clew never offers to edit that file, and the command it names points
    /// at the file ssh reported.
    #[tokio::test]
    async fn a_key_changed_against_the_users_own_file_is_left_to_them() {
        let dir = temp("changed-user");
        std::fs::write(
            dir.join("stderr.txt"),
            changed_key_stderr("/Users/me/.ssh/known_hosts:7"),
        )
        .unwrap();
        let mut ssh = fake(fake_ssh(&dir, "", "cat \"$D/stderr.txt\" >&2; exit 255"));
        ssh.settings = Ok(direct("example.com", 2222));
        ssh.clew_known_hosts = Some(dir.join("clew known_hosts"));
        let err = bootstrap_remote(&ssh, &layout(), |_| panic!("no fetch"))
            .await
            .unwrap_err();
        assert_eq!(err.kind, FailureKind::HostKeyChanged);
        assert_eq!(err.forget_host, None);
        assert!(
            err.reason.contains("/Users/me/.ssh/known_hosts:7")
                && err
                    .reason
                    .contains("`ssh-keygen -f /Users/me/.ssh/known_hosts -R '[example.com]:2222'`"),
            "{}",
            err.reason
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Everything a restart needs, pointed at fakes: `ssh` answers the
    /// probe (the build is installed), and its start command prints the
    /// marker and ONE frame — the Hello's stand-in — and then exits, a server
    /// that answers and immediately dies. Its `ssh -G` fails, so the path
    /// never consults a data directory (these tests move no environment).
    fn dying_launcher(dir: &Path, healthy_after: Duration) -> Launcher {
        let program = fake_ssh(dir, &layout().probe_line, "");
        std::fs::write(dir.join("installed"), "").unwrap();
        std::fs::write(dir.join("g.fail"), "not in this test\n").unwrap();
        Launcher {
            ssh: program,
            keyscan: PathBuf::from("/nonexistent/ssh-keyscan"),
            keygen: PathBuf::from("/nonexistent/ssh-keygen"),
            local_server: None,
            backoff_base: Duration::from_millis(1),
            healthy_after,
            control_dir: None,
        }
    }

    /// [`dying_launcher`], whose server's first frame (after the marker) is a
    /// notification padded with JSON whitespace to exactly `content` bytes,
    /// followed by an ordinary one — and then it exits.
    fn launcher_with_a_frame_of(dir: &Path, content: usize) -> Launcher {
        let start = r#"
case "$CMD" in
  *CLEW-READY-*)
    printf '%s\n' "$(printf '%s' "$CMD" | grep -o 'CLEW-READY-[0-9a-f-]*')"
    J='{"Notification":{"sub":null,"event":{"Status":{"message":"big"}}}}'
    printf '%s' "$J"
    printf '%*s' $(( CONTENT - ${#J} )) ''
    printf '\n%s\n' '{"Notification":{"sub":null,"event":{"Status":{"message":"after"}}}}'
    exit 0 ;;
esac"#
            .replace("CONTENT", &content.to_string());
        let mut launcher = dying_launcher(dir, Duration::from_secs(60));
        launcher.ssh = fake_ssh(dir, &layout().probe_line, &start);
        launcher
    }

    /// The transport reads the server's frames with the one protocol reader
    /// (`framing::read_protocol_line`): a frame of exactly the protocol's cap
    /// — the largest a server sends — becomes an event, and so does the
    /// frame after it; one byte more ends the transport, with nothing after
    /// it read. A reader of the transport's own that counted the newline
    /// against the cap (what both ends did before they shared one) drops the
    /// first; one without the cap delivers the second.
    #[tokio::test]
    async fn the_transport_reads_frames_up_to_the_cap_and_no_further() {
        let cap = clew_protocol::MAX_FRAME_BYTES;
        for (content, want) in [(cap, 2), (cap + 1, 0)] {
            let dir = temp("frame-cap");
            let launcher = launcher_with_a_frame_of(&dir, content);
            let target = ConnTarget::Ssh {
                label: format!("frame-cap-{}", unique_token()),
                args: port_2222_args(),
            };
            let messages = instance(&target, 1, false, &launcher).await;
            let events = messages
                .iter()
                .filter(|m| matches!(m, Message::Server(ServerMsg::Event { .. })))
                .count();
            assert_eq!(
                events, want,
                "a first frame of {content} bytes (cap {cap}): {messages:?}"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// Every message one transport instance produces, to its end.
    async fn instance(
        target: &ConnTarget,
        seq: u64,
        respawn: bool,
        launcher: &Launcher,
    ) -> Vec<Message> {
        use iced::futures::StreamExt;
        let key = ConnKey {
            target: target.clone(),
            seq,
            respawn,
        };
        tokio::time::timeout(
            Duration::from_secs(30),
            run_transport(key, launcher.clone()).collect::<Vec<_>>(),
        )
        .await
        .expect("a transport instance ends")
    }

    /// D2-2: a server that answers the Hello and then dies is a failure, not
    /// a healthy connection. The count used to reset on that first frame, so
    /// such a server was restarted every 1.5 s forever — each cycle tearing
    /// down the window's connection state. Now each early death counts, the
    /// restarts back off, and after MAX_ATTEMPTS the stream stops with the
    /// reason. (The app's side is simulated: every `ServerMsg::Disconnected`
    /// is answered with the next instance, `respawn` set, as
    /// `on_server_disconnected` does.)
    #[tokio::test]
    async fn a_server_that_answers_and_dies_is_given_up_on_after_max_attempts() {
        let dir = temp("dying");
        let launcher = dying_launcher(&dir, Duration::from_secs(60));
        let target = ConnTarget::Ssh {
            label: format!("dying-{}", unique_token()),
            args: port_2222_args(),
        };
        let mut seq = 1;
        let mut respawn = false;
        let mut sessions = 0;
        let reason = loop {
            let messages = instance(&target, seq, respawn, &launcher).await;
            match messages.as_slice() {
                [
                    Message::Server(ServerMsg::Connected { conn: c1, .. }),
                    Message::Server(ServerMsg::Event { conn: c2, .. }),
                    Message::Server(ServerMsg::Disconnected { conn: c3, .. }),
                ] => {
                    assert_eq!((*c1, *c2, *c3), (seq, seq, seq));
                    sessions += 1;
                    assert_eq!(failures(&target).0, sessions, "each early death counts");
                }
                [Message::Server(ServerMsg::Unavailable { conn, reason })] => {
                    assert_eq!(*conn, seq);
                    break reason.clone();
                }
                other => panic!("instance {seq}: {other:?}"),
            }
            assert!(sessions <= MAX_ATTEMPTS, "restarted forever");
            seq += 1;
            respawn = true;
        };
        assert_eq!(sessions, MAX_ATTEMPTS);
        assert!(
            reason.contains(&format!("Stopped reconnecting to {}", target.label()))
                && reason.contains(&format!("after {MAX_ATTEMPTS} failed attempts"))
                && reason.contains("after it answered"),
            "{reason}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The other half: a transport that stayed up past `healthy_after` was a
    /// working connection, so its end starts the count afresh — a laptop that
    /// changed networks after an hour gets the short first delay again.
    #[tokio::test]
    async fn a_transport_that_stayed_up_starts_the_count_afresh() {
        let dir = temp("healthy");
        let target = ConnTarget::Ssh {
            label: format!("healthy-{}", unique_token()),
            args: port_2222_args(),
        };
        for _ in 0..3 {
            record_failure(&target, &Failure::new(FailureKind::Unreachable, "earlier"));
        }
        let healthy = dying_launcher(&dir, Duration::ZERO);
        let messages = instance(&target, 7, true, &healthy).await;
        assert!(
            matches!(
                messages.last(),
                Some(Message::Server(ServerMsg::Disconnected { conn: 7, .. }))
            ),
            "{messages:?}"
        );
        assert_eq!(failures(&target).0, 0, "healthy long enough: reset");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// D2-17: a server is torn down gracefully. Its stdin is closed first, so
    /// it sees EOF and exits on its own — reaped then, long before any kill;
    /// one that ignores EOF is killed only once its grace has run out. Either
    /// way it counts as live until it is reaped, which is what a quit waits
    /// for (`all_servers_reaped`) before exiting the runtime the grace runs on.
    #[tokio::test]
    async fn a_server_gets_eof_then_its_grace_then_a_kill() {
        let dir = temp("teardown");
        let reaper = Reaper::new();
        let live = |reaper: &Reaper| *reaper.live.lock().unwrap();
        let sh = |script: String| {
            let mut cmd = tokio::process::Command::new("/bin/sh");
            cmd.arg("-c").arg(script);
            cmd
        };

        // Exits on EOF, leaving a mark: reaped without waiting out a grace
        // it would otherwise sit through.
        let mark = dir.join("saw-eof");
        let polite = sh(format!("read _; : > '{}'", mark.display()));
        let transport =
            Transport::spawn_with(polite, "test", &reaper, Duration::from_secs(600)).unwrap();
        assert_eq!(live(&reaper), 1);
        drop(transport);
        tokio::time::timeout(Duration::from_secs(30), reaper.all_reaped())
            .await
            .expect("a server that exits on EOF is reaped without a kill");
        assert!(mark.exists(), "the server never saw EOF");
        assert_eq!(live(&reaper), 0);

        // Ignores EOF: still alive inside its grace, killed after it.
        let grace = Duration::from_millis(400);
        let stubborn = sh("exec sleep 60".into());
        let transport = Transport::spawn_with(stubborn, "test", &reaper, grace).unwrap();
        let started = tokio::time::Instant::now();
        drop(transport);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(live(&reaper), 1, "killed before its grace ran out");
        tokio::time::timeout(Duration::from_secs(30), reaper.all_reaped())
            .await
            .expect("a server that ignores EOF is killed after its grace");
        assert!(started.elapsed() >= grace, "{:?}", started.elapsed());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// D2-17: every `ssh` call of a connection rides one control master —
    /// one login for the probe, the deploy, the start and every reconnect —
    /// through a socket in a directory only this user can reach, and never
    /// through one somebody else could.
    #[test]
    fn every_ssh_call_of_a_connection_shares_one_login() {
        use std::os::unix::fs::MetadataExt;
        let base = temp("ctl-args");
        let uid = std::fs::metadata(&base).unwrap().uid();
        let dir = base.join("s");
        let args = control_args_in(&dir, uid);
        let path = format!("ControlPath={}/%C", dir.display());
        assert_eq!(
            args,
            [
                "-o",
                "ControlMaster=auto",
                "-o",
                path.as_str(),
                "-o",
                "ControlPersist=60"
            ]
        );
        // They go before the destination, in every call the transport makes.
        let ssh = Ssh::assemble(
            "u@h",
            &["--".into(), "u@h".into()],
            Path::new("/usr/bin/ssh"),
            [vec!["-T".to_string()], args.clone()].concat(),
            Err("not resolved".into()),
        );
        let cmd = ssh.command("true");
        let argv: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let at = |word: &str| argv.iter().position(|a| a == word).unwrap();
        assert!(at("ControlMaster=auto") < at("--"), "{argv:?}");
        // A directory others can reach: no sharing at all.
        let open = base.join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(control_args_in(&open, uid).is_empty());
        // The socket path production uses — `%C` is 40 hex digits, and ssh
        // adds 17 bytes while it sets the socket up — fits a Unix socket
        // address (104), for any uid.
        let longest = control_dir(u32::MAX);
        assert!(
            longest.as_os_str().len() + "/".len() + 40 + 17 <= 104,
            "{longest:?}"
        );
        assert_eq!(Launcher::system().control_dir, Some(control_dir(uid)));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The launcher threads the shared login into EVERY ssh call of a real
    /// start — the settings query, the probe, the start itself — before the
    /// destination.
    #[tokio::test]
    async fn a_start_shares_one_login_across_its_ssh_calls() {
        use std::os::unix::fs::MetadataExt;
        let dir = temp("ctl-start");
        let uid = std::fs::metadata(&dir).unwrap().uid();
        let launcher = Launcher {
            control_dir: Some(dir.join("sockets")),
            ..dying_launcher(&dir, Duration::from_secs(60))
        };
        let target = ConnTarget::Ssh {
            label: format!("ctl-{}", unique_token()),
            args: port_2222_args(),
        };
        let _ = instance(&target, 1, false, &launcher).await;
        let log = std::fs::read_to_string(dir.join("argv.log")).unwrap();
        let calls: Vec<Vec<&str>> = log
            .split("--END--\n")
            .filter(|call| !call.trim().is_empty())
            .map(|call| call.lines().collect())
            .collect();
        assert!(calls.len() >= 2, "{calls:?}");
        let path = format!("ControlPath={}/%C", dir.join("sockets").display());
        for call in &calls {
            let at = |word: &str| call.iter().position(|a| *a == word);
            let destination = at("--").expect("every call names its destination after --");
            assert!(
                at("ControlMaster=auto").is_some_and(|i| i < destination)
                    && at(path.as_str()).is_some_and(|i| i < destination),
                "a call without the shared login: {call:?}"
            );
        }
        assert_eq!(
            std::fs::metadata(dir.join("sockets")).unwrap().uid(),
            uid,
            "the socket directory is this user's"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A transport torn down with requests still queued writes them before
    /// the EOF — a closing window's last bookmark edits and stream stops
    /// included — instead of letting the stop win the race against them.
    #[tokio::test]
    async fn a_stopped_writer_sends_what_was_queued_first() {
        use tokio::io::AsyncReadExt;
        let (tx, rx) = tokio::sync::mpsc::channel::<ClientMessage>(64);
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        for id in 1..=20 {
            tx.try_send(ClientMessage {
                id,
                request: clew_protocol::Request::Cancel { id: 1000 + id },
            })
            .unwrap();
        }
        // Torn down before the writer ever ran: the stop and every request
        // are ready at once.
        stop_tx.send(()).unwrap();
        let (client_end, mut server_end) = tokio::io::duplex(1 << 20);
        write_requests(rx, stop_rx, client_end).await;
        let mut written = String::new();
        server_end.read_to_string(&mut written).await.unwrap();
        let ids: Vec<u64> = written
            .lines()
            .map(|line| serde_json::from_str::<ClientMessage>(line).unwrap().id)
            .collect();
        assert_eq!(ids, (1..=20).collect::<Vec<u64>>(), "{written}");
        drop(tx);
    }

    /// The control socket directory must be this user's alone.
    #[test]
    fn the_control_socket_directory_must_be_private() {
        use std::os::unix::fs::MetadataExt;
        let base = temp("ctl");
        let uid = std::fs::metadata(&base).unwrap().uid();
        let fresh = base.join("fresh");
        let made = private_socket_dir(&fresh, uid).expect("created");
        assert_eq!(std::fs::metadata(&made).unwrap().mode() & 0o777, 0o700);
        let open = base.join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            private_socket_dir(&open, uid).is_none(),
            "group/other access"
        );
        let link = base.join("link");
        std::os::unix::fs::symlink(&fresh, &link).unwrap();
        assert!(private_socket_dir(&link, uid).is_none(), "a symlink");
        assert!(
            private_socket_dir(&fresh, uid + 1).is_none(),
            "another owner"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// OpenSSH tools always resolve to an absolute path: the first ABSOLUTE
    /// `PATH` entry holding the tool (`find_on_path`, whose own test covers
    /// skipping relative entries), else the system copy — never a bare name
    /// the OS would look up through a relative entry. (`PATH` is deliberately
    /// not modified here: other tests in this binary spawn `git` by name.)
    #[test]
    fn openssh_tools_resolve_to_absolute_paths_only() {
        assert!(openssh_tool("ssh").is_absolute());
        assert_eq!(
            openssh_tool("clew-no-such-openssh-tool"),
            PathBuf::from("/usr/bin/clew-no-such-openssh-tool"),
            "an unresolvable tool falls back to the system path, not a bare name"
        );
    }
}
