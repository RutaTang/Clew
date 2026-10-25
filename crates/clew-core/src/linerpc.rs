//! JSON-RPC 2.0 over newline-delimited JSON: the stdio transport MCP and ACP
//! share, on the [`crate::framing`] actor ([`Framing::Lines`]).
//!
//! A [`Session`] is one conversation with a peer — a process this program
//! launched ([`Session::spawn`]) or any pair of streams ([`Session::over`]).
//! Calls go out through it and are answered through the actor's table, as
//! for the LSP and DAP clients. What the peer sends of its own accord comes
//! out of [`Session::take_peer`] as [`PeerMsg`]s: its notifications, and the
//! requests it wants answered, which the consumer answers when it is ready
//! with [`Session::respond`] — a permission the user is asked for takes as
//! long as the user does. A request the consumer can answer on the spot
//! without looking (`ping`) is answered inside the actor by the session's
//! [`Quick`] handler, and never reaches the consumer.
//!
//! The peer is a program the USER configured — an MCP server, an ACP agent —
//! not one a repository chose, so it is launched as configured. What it
//! sends is still only data: every line is size-capped, a line that is not
//! JSON is skipped, and nothing it says is executed.
//!
//! The process runs in its own process group (on unix) and is killed with
//! that group when the session ends, after a short grace once its input has
//! been closed — the shutdown MCP's stdio transport specifies.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, BufReader};
use tokio::sync::{mpsc, watch};

use crate::framing::{
    self, CallError, EndReason, Failure, FrameError, Framing, Handled, Ids, Protocol,
};

/// JSON-RPC's own error codes.
pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;

/// How long a process whose input was closed may take to exit on its own
/// before its process group is killed.
pub const EXIT_GRACE: Duration = Duration::from_secs(2);

/// Most stderr lines kept for diagnostics (see [`Session::stderr_tail`]).
const STDERR_LINES: usize = 40;
/// Longest stderr line kept; the rest of a longer one is dropped.
const STDERR_LINE_BYTES: usize = 2048;

/// An error answer to a request — ours to the peer, or the peer's to us.
#[derive(Debug, Clone, PartialEq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> RpcError {
        RpcError {
            code,
            message: message.into(),
            data: None,
        }
    }

    /// The peer asked for something this side does not implement.
    pub fn method_not_found(method: &str) -> RpcError {
        RpcError::new(METHOD_NOT_FOUND, format!("method not found: {method}"))
    }

    /// The request's parameters are not what the method takes.
    pub fn invalid_params(message: impl Into<String>) -> RpcError {
        RpcError::new(INVALID_PARAMS, message)
    }

    /// The method failed for a reason of this side's.
    pub fn internal(message: impl Into<String>) -> RpcError {
        RpcError::new(INTERNAL_ERROR, message)
    }

    /// The JSON-RPC `error` object.
    pub fn to_json(&self) -> Value {
        let mut err = json!({ "code": self.code, "message": self.message });
        if let Some(data) = &self.data {
            err["data"] = data.clone();
        }
        err
    }
}

/// What the peer sent of its own accord.
#[derive(Debug, Clone, PartialEq)]
pub enum PeerMsg {
    /// A request the consumer answers with [`Session::respond`]. `id` is the
    /// peer's, echoed verbatim in the answer.
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    /// A notification: nothing to answer.
    Notification { method: String, params: Value },
}

/// Answers the peer's requests that need no consumer, inside the actor:
/// `Some(answer)` for a request it handles, `None` to pass it on as a
/// [`PeerMsg::Request`].
pub type Quick = Box<dyn FnMut(&str, &Value) -> Option<Result<Value, RpcError>> + Send>;

/// The message that tells the peer one of our requests is no longer wanted,
/// sent when its caller gave up on it (a time box, a dropped call).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelStyle {
    /// MCP's `notifications/cancelled` with `requestId`.
    Mcp,
    /// ACP's `$/cancel_request` with `requestId`.
    Acp,
}

/// A JSON-RPC answer to request `id`.
pub fn response(id: &Value, result: Result<Value, RpcError>) -> Value {
    match result {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err(err) => json!({ "jsonrpc": "2.0", "id": id, "error": err.to_json() }),
    }
}

/// A JSON-RPC request or notification. `params` is left out when null: the
/// spec lets it be omitted, but a present one must be an object or array.
pub fn envelope(id: Option<i64>, method: &str, params: Value) -> Value {
    let mut msg = json!({ "jsonrpc": "2.0", "method": method });
    if let Some(id) = id {
        msg["id"] = json!(id);
    }
    if !params.is_null() {
        msg["params"] = params;
    }
    msg
}

/// The text of a failed call, for a person.
pub fn describe(error: &CallError) -> String {
    match error {
        CallError::NotRunning => "it is not running".into(),
        CallError::Closed => "it closed the connection".into(),
        CallError::TimedOut => "it did not answer in time".into(),
        CallError::Failed(f) => f.message.clone(),
    }
}

/// The JSON-RPC half of a session's actor.
struct Wire {
    peer: mpsc::UnboundedSender<PeerMsg>,
    quick: Quick,
    cancel: Option<CancelStyle>,
    shared: Arc<Shared>,
}

impl Protocol for Wire {
    fn framing(&self) -> Framing {
        Framing::Lines
    }

    fn request(&mut self, id: i64, method: &str, params: Value) -> Value {
        envelope(Some(id), method, params)
    }

    fn notification(&mut self, _id: i64, method: &str, params: Value) -> Value {
        envelope(None, method, params)
    }

    fn inbound(&mut self, msg: Value, _ids: &Ids) -> Handled {
        let id = msg.get("id").filter(|v| !v.is_null()).cloned();
        let method = msg.get("method").and_then(Value::as_str).map(str::to_owned);
        match (id, method) {
            // An answer to one of our requests. Our ids are integers; any
            // other id answers nothing we sent.
            (Some(id), None) => {
                let Some(id) = id.as_i64() else {
                    return Handled::Nothing;
                };
                let result = match msg.get("error") {
                    Some(err) => Err(Failure {
                        code: err.get("code").and_then(Value::as_i64),
                        message: err
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("error")
                            .to_string(),
                        data: err.get("data").cloned(),
                    }),
                    None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                };
                Handled::Response { id, result }
            }
            (Some(id), Some(method)) => {
                let params = msg.get("params").cloned().unwrap_or(Value::Null);
                if let Some(answer) = (self.quick)(&method, &params) {
                    return Handled::Reply(response(&id, answer));
                }
                let request = PeerMsg::Request {
                    id: id.clone(),
                    method,
                    params,
                };
                match self.peer.send(request) {
                    Ok(()) => Handled::Nothing,
                    // Nobody is left to answer it: say so, rather than leave
                    // the peer waiting on an answer that cannot come.
                    Err(_) => Handled::Reply(response(
                        &id,
                        Err(RpcError::internal("the client is no longer listening")),
                    )),
                }
            }
            (None, Some(method)) => {
                let params = msg.get("params").cloned().unwrap_or(Value::Null);
                let _ = self.peer.send(PeerMsg::Notification { method, params });
                Handled::Nothing
            }
            (None, None) => Handled::Nothing,
        }
    }

    fn frame_error(&mut self, error: &FrameError) {
        self.shared.note(format!("[clew] {error}"));
    }

    fn cancel(&mut self, id: i64) -> Option<Value> {
        let method = match self.cancel? {
            CancelStyle::Mcp => "notifications/cancelled",
            CancelStyle::Acp => "$/cancel_request",
        };
        Some(envelope(
            None,
            method,
            json!({ "requestId": id, "reason": "the client stopped waiting" }),
        ))
    }

    fn ended(&mut self, reason: &EndReason) -> String {
        let text = match reason {
            EndReason::Stopped => "the session was closed".to_string(),
            EndReason::Closed => "it exited".to_string(),
            EndReason::Broken(why) => format!("the connection to it broke: {why}"),
            EndReason::ConsumerGone => "the session was closed".to_string(),
        };
        let _ = self.shared.ended.send(Some(text.clone()));
        text
    }
}

/// What the actor and the session share: the peer's last stderr lines and
/// how the session ended.
struct Shared {
    stderr: Mutex<VecDeque<String>>,
    ended: watch::Sender<Option<String>>,
}

impl Shared {
    fn note(&self, line: String) {
        let mut tail = self.stderr.lock().unwrap_or_else(PoisonError::into_inner);
        if tail.len() == STDERR_LINES {
            tail.pop_front();
        }
        tail.push_back(line);
    }
}

/// How to start a peer process.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Launch {
    /// The program: a path, or a name looked up on the user's `PATH`
    /// ([`resolve_program`]).
    pub program: String,
    pub args: Vec<String>,
    /// Set on top of the inherited environment.
    pub env: Vec<(String, String)>,
    /// The working directory; a neutral one ([`neutral_dir`]) when `None`.
    pub cwd: Option<PathBuf>,
}

/// How a session treats the peer's requests and our abandoned ones.
pub struct Options {
    pub quick: Quick,
    pub cancel: Option<CancelStyle>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            quick: Box::new(|_, _| None),
            cancel: None,
        }
    }
}

/// One JSON-RPC conversation over JSON lines. Dropping the session stops it
/// (and the process it launched).
pub struct Session {
    handle: framing::Handle,
    peer: Mutex<Option<mpsc::UnboundedReceiver<PeerMsg>>>,
    shared: Arc<Shared>,
    ended: watch::Receiver<Option<String>>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("alive", &self.alive())
            .finish()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.handle.stop();
    }
}

impl Session {
    /// Launch `launch` and talk to it over its stdin and stdout. Must run
    /// inside a tokio runtime.
    pub fn spawn(launch: &Launch, options: Options) -> Result<Session, String> {
        let program = resolve_program(&launch.program).ok_or_else(|| {
            format!(
                "`{}` was not found — give its full path, or install it",
                launch.program
            )
        })?;
        let mut command = tokio::process::Command::new(&program);
        command
            .args(&launch.args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        if let Some(path) = spawn_path() {
            command.env("PATH", path);
        }
        for (key, value) in &launch.env {
            command.env(key, value);
        }
        command.current_dir(launch.cwd.clone().unwrap_or_else(neutral_dir));
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command
            .spawn()
            .map_err(|e| format!("could not start {}: {e}", program.display()))?;
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let stderr = child.stderr.take().ok_or("no stderr")?;
        let session = Session::wire(stdout, stdin, options, Some(child));
        tokio::spawn(stderr_loop(stderr, session.shared.clone()));
        Ok(session)
    }

    /// Talk JSON lines over `reader` (the peer's output) and `writer` (its
    /// input). Must run inside a tokio runtime.
    pub fn over<R, W>(reader: R, writer: W, options: Options) -> Session
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        Session::wire(reader, writer, options, None)
    }

    fn wire<R, W>(
        reader: R,
        writer: W,
        options: Options,
        child: Option<tokio::process::Child>,
    ) -> Session
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (handle, mailbox) = framing::mailbox();
        let (peer_tx, peer_rx) = mpsc::unbounded_channel();
        let (ended_tx, ended_rx) = watch::channel(None);
        let shared = Arc::new(Shared {
            stderr: Mutex::new(VecDeque::new()),
            ended: ended_tx,
        });
        let incoming = framing::spawn_reader_framed(reader, Framing::Lines);
        let wire = Wire {
            peer: peer_tx,
            quick: options.quick,
            cancel: options.cancel,
            shared: shared.clone(),
        };
        tokio::spawn(async move {
            let ended = framing::run(writer, wire, mailbox, incoming).await;
            if let Some(child) = child {
                let grace = match ended.reason {
                    EndReason::Broken(_) => Duration::ZERO,
                    _ => EXIT_GRACE,
                };
                framing::reap(child, grace, cfg!(unix)).await;
            }
            ended.release();
        });
        Session {
            handle,
            peer: Mutex::new(Some(peer_rx)),
            shared,
            ended: ended_rx,
        }
    }

    /// The peer's own messages. There is one stream of them; the first call
    /// takes it.
    pub fn take_peer(&self) -> Option<mpsc::UnboundedReceiver<PeerMsg>> {
        self.peer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }

    /// Send a request and await its answer, for at most `timeout`. Dropping
    /// the future (or the time box running out) tells the peer it need not
    /// answer, where the session's [`CancelStyle`] says how.
    pub async fn call(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, CallError> {
        self.handle.call(method, params, timeout).await
    }

    /// [`Session::call`], queued now rather than at the first poll.
    pub fn call_now(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> impl Future<Output = Result<Value, CallError>> + Send + 'static {
        self.handle.call_now(method, params, timeout)
    }

    /// A handle to the session, for a task that answers the peer from
    /// elsewhere. The session still ends when it is dropped.
    pub fn handle(&self) -> framing::Handle {
        self.handle.clone()
    }

    /// Send a notification.
    pub fn notify(&self, method: &str, params: Value) {
        self.handle.notify(method, params);
    }

    /// Answer the peer's request `id`.
    pub fn respond(&self, id: &Value, result: Result<Value, RpcError>) {
        self.handle.send_raw(response(id, result));
    }

    /// Whether the session is still running.
    pub fn alive(&self) -> bool {
        self.handle.alive()
    }

    /// How the session ended, once it has.
    pub fn ended(&self) -> Option<String> {
        self.ended.borrow().clone()
    }

    /// A future that resolves, once the session has ended, to how it did —
    /// for a task that outlives its borrow of the session.
    pub fn on_end(&self) -> impl Future<Output = String> + Send + 'static {
        let mut ended = self.ended.clone();
        async move {
            match ended.wait_for(Option::is_some).await {
                Ok(text) => text.clone().unwrap_or_default(),
                Err(_) => "the session was closed".into(),
            }
        }
    }

    /// Wait until the session has ended, and say how.
    pub async fn wait_ended(&self) -> String {
        let mut ended = self.ended.clone();
        match ended.wait_for(Option::is_some).await {
            Ok(text) => text.clone().unwrap_or_default(),
            Err(_) => "the session was closed".into(),
        }
    }

    /// The peer's last lines of stderr (and transport notes), oldest first,
    /// for an error message that says more than "it exited".
    pub fn stderr_tail(&self) -> String {
        let tail = self
            .shared
            .stderr
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        tail.iter().cloned().collect::<Vec<_>>().join("\n")
    }

    /// End the session, and wait (bounded) until it has.
    pub async fn stop(&self) {
        self.handle
            .stop_and_wait(framing::STOP_LIMIT + EXIT_GRACE + Duration::from_secs(1))
            .await;
    }
}

/// Capture the peer's stderr into the session's tail, a capped line at a
/// time (stderr is not required to be UTF-8, or to have line ends at all).
async fn stderr_loop<R>(stderr: R, shared: Arc<Shared>)
where
    R: AsyncRead + Unpin,
{
    let mut reader = BufReader::new(stderr);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match (&mut reader)
            .take(STDERR_LINE_BYTES as u64)
            .read_until(b'\n', &mut buf)
            .await
        {
            Ok(0) | Err(_) => return,
            Ok(_) => {
                let line = String::from_utf8_lossy(&buf);
                let line = line.trim_end();
                if !line.is_empty() {
                    shared.note(line.to_string());
                }
            }
        }
    }
}

/// Where a configured program is: an absolute path as given (`~/`
/// expanded), or a bare name found in an ABSOLUTE directory of the user's
/// `PATH` ([`spawn_path`]). A relative path, or a relative `PATH` entry such
/// as `.`, would resolve against whatever directory this process happens to
/// run in — possibly a repository, which could then plant the program.
pub fn resolve_program(program: &str) -> Option<PathBuf> {
    let program = program.trim();
    if program.is_empty() {
        return None;
    }
    let expanded = expand_home(program);
    if expanded.is_absolute() {
        return Some(expanded);
    }
    if program.contains(['/', '\\']) {
        return None;
    }
    let path = spawn_path().or_else(|| std::env::var_os("PATH"))?;
    crate::lsp::store::find_in_unresolved(&path, program)
}

/// `~/rest` as a path under the home directory; anything else as it is.
pub fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return Path::new(&home).join(rest);
    }
    PathBuf::from(path)
}

/// Where a launched program runs unless its configuration names a folder:
/// the user's home, else the filesystem root — never wherever this process
/// happens to be, which is a project when clew was started from a terminal
/// in one. `npx` and `uvx`, which launch most MCP servers and ACP agents,
/// read configuration from the working directory and its ancestors
/// (`.npmrc`, `node_modules`, `uv.toml`), so a project as the working
/// directory could choose what they run. The project reaches an ACP agent
/// as the session's `cwd`.
pub fn neutral_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|h| h.is_absolute() && h.is_dir());
    home.unwrap_or_else(|| {
        if cfg!(unix) {
            PathBuf::from("/")
        } else {
            std::env::temp_dir()
        }
    })
}

/// `program` and `args` as a command that starts in `dir` itself, for a
/// launch whose spawner gives no working directory — the MCP servers handed
/// to an ACP agent, which the agent starts wherever it is (the project). On
/// Unix, `/bin/sh` changes to `dir` and then becomes the program; elsewhere
/// the command is left as it is.
pub fn launched_in(program: PathBuf, args: Vec<String>, dir: &Path) -> (PathBuf, Vec<String>) {
    if !cfg!(unix) {
        return (program, args);
    }
    let mut wrapped = vec![
        "-c".to_string(),
        "cd -- \"$1\" || exit 1; shift; exec \"$@\"".to_string(),
        "clew-launch".to_string(),
        dir.to_string_lossy().into_owned(),
        program.to_string_lossy().into_owned(),
    ];
    wrapped.extend(args);
    (PathBuf::from("/bin/sh"), wrapped)
}

/// The `PATH` a launched program gets: this process's own, with the user's
/// login-shell `PATH` ahead of it when this process was started without one
/// (an app opened from the Finder gets `/usr/bin:/bin:/usr/sbin:/sbin`, on
/// which `npx`, `uvx` and the agents' own launchers are not found).
///
/// The login shell is asked once, bounded, and only by an app bundle on
/// macOS — a process started from a terminal already has the user's `PATH`.
pub fn spawn_path() -> Option<OsString> {
    static PATH: OnceLock<Option<OsString>> = OnceLock::new();
    PATH.get_or_init(|| {
        let own = std::env::var_os("PATH");
        let login = if cfg!(target_os = "macos") && launched_without_shell() {
            login_shell_path()
        } else {
            None
        };
        merge_paths(login, own)
    })
    .clone()
}

/// Whether this process looks like it was started by the desktop rather than
/// a shell: macOS gives such an app the bare system `PATH`.
fn launched_without_shell() -> bool {
    match std::env::var("PATH") {
        Ok(path) => path
            .split(':')
            .all(|d| matches!(d, "/usr/bin" | "/bin" | "/usr/sbin" | "/sbin" | "")),
        Err(_) => true,
    }
}

/// `$SHELL -l -c 'printf %s "$PATH"'`, within a few seconds.
#[cfg(unix)]
fn login_shell_path() -> Option<OsString> {
    let shell = std::env::var_os("SHELL").unwrap_or_else(|| "/bin/zsh".into());
    shell_path(&shell, Duration::from_secs(5))
}

/// No login shell to ask.
#[cfg(not(unix))]
fn login_shell_path() -> Option<OsString> {
    None
}

/// The `PATH` login shell `shell` prints, if it does before `wait` is over.
///
/// Only the shell is waited for, not the end of its output: a login profile
/// that leaves a job running (`sleep 8 &`, a daemon it starts) leaves it
/// holding the output open once the shell is gone, and reading to its end
/// held every launch waiting on the `PATH` for as long — for good, with a
/// daemon. The shell leads a process group of its own, which is ended with
/// it, its jobs included; what the shell printed is in the pipe by then.
#[cfg(unix)]
fn shell_path(shell: &std::ffi::OsStr, wait: Duration) -> Option<OsString> {
    use std::os::unix::process::CommandExt;
    let mut child = std::process::Command::new(shell)
        .args(["-l", "-c", "printf '%s' \"$PATH\""])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    // Read as it comes: a profile printing more than the pipe holds stalled
    // the shell on it. And no further than a PATH could be: one printing
    // without end was held in memory as it came, up to the deadline.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = [0u8; 4096];
        let mut read = 0;
        while let Ok(n @ 1..) = stdout.read(&mut buf) {
            read += n;
            if tx.send(buf[..n].to_vec()).is_err() || read > MAX_LOGIN_PATH {
                return;
            }
        }
    });
    let deadline = std::time::Instant::now() + wait;
    // Left unreaped meanwhile, so its pid still names its group.
    let state = loop {
        match crate::framing::leader_state(child.id()) {
            crate::framing::LeaderState::Running if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(25));
            }
            state => break state,
        }
    };
    // Not waitable by us — reaped elsewhere — its pid may be another
    // process's by now: nothing is signalled.
    if state == crate::framing::LeaderState::Unknown {
        return None;
    }
    crate::procgroup::kill(child.id());
    let _ = child.kill();
    let _ = child.wait();
    if state != crate::framing::LeaderState::Exited {
        return None;
    }
    // Its group ended, nothing holds the output open but a job that left
    // the group: what came by then is all the shell printed.
    let mut out = Vec::new();
    let settle = std::time::Instant::now() + Duration::from_millis(500);
    while let Ok(chunk) =
        rx.recv_timeout(settle.saturating_duration_since(std::time::Instant::now()))
    {
        out.extend(chunk);
        if out.len() > MAX_LOGIN_PATH {
            return None;
        }
    }
    let out = String::from_utf8(out).ok()?;
    let out = out.trim();
    (!out.is_empty() && !out.contains('\n')).then(|| OsString::from(out))
}

/// Longest `PATH` a login shell is taken at: past it, its profile printed
/// something else.
#[cfg(unix)]
const MAX_LOGIN_PATH: usize = 64 * 1024;

/// `first`'s entries, then `second`'s that `first` lacks — absolute ones
/// only (see [`resolve_program`]).
fn merge_paths(first: Option<OsString>, second: Option<OsString>) -> Option<OsString> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    for path in [first, second].into_iter().flatten() {
        for dir in std::env::split_paths(&path) {
            if dir.is_absolute() && !dirs.contains(&dir) {
                dirs.push(dir);
            }
        }
    }
    if dirs.is_empty() {
        return None;
    }
    std::env::join_paths(dirs).ok()
}

#[cfg(test)]
mod tests {
    /// The login shell's `PATH` is read once the shell exits, though a job
    /// its profile left running holds its output open — the job ended with
    /// it — and a shell that does not exit in time gives none.
    #[cfg(unix)]
    #[test]
    fn a_login_shell_is_waited_for_and_not_the_jobs_it_leaves() {
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::testutil::TempDir::new("login-shell");
        let shell = |name: &str, body: &str| {
            let path = dir.path().join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        };
        let leaves = shell("leaves", "sleep 30 &\nprintf '%s' /login/bin:/usr/bin");
        let started = std::time::Instant::now();
        assert_eq!(
            super::shell_path(leaves.as_os_str(), std::time::Duration::from_secs(20)),
            Some(std::ffi::OsString::from("/login/bin:/usr/bin"))
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "waited for the job: {:?}",
            started.elapsed()
        );
        let hangs = shell("hangs", "printf '%s' /a:/b\nsleep 30");
        let started = std::time::Instant::now();
        assert_eq!(
            super::shell_path(hangs.as_os_str(), std::time::Duration::from_millis(300)),
            None
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        // One printing without end is read no further than a PATH could be.
        let endless = shell(
            "endless",
            "yes xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
        );
        // Left to it, the shell is done long before the deadline.
        let started = std::time::Instant::now();
        assert_eq!(
            super::shell_path(endless.as_os_str(), std::time::Duration::from_secs(20)),
            None
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "read to the deadline: {:?}",
            started.elapsed()
        );
        let chatty = shell(
            "chatty",
            "head -c 200000 /dev/zero | tr '\\0' x\nprintf '%s' /a",
        );
        assert_eq!(
            super::shell_path(chatty.as_os_str(), std::time::Duration::from_secs(20)),
            None,
            "a profile's output is no PATH"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_wrapped_launch_starts_in_its_folder_with_its_arguments() {
        let dir = crate::testutil::TempDir::new("launched-in");
        let (program, args) = super::launched_in(
            std::path::PathBuf::from("/bin/sh"),
            vec![
                "-c".into(),
                "pwd; printf '%s|' \"$@\"".into(),
                "x".into(),
                "a b".into(),
                "$HOME".into(),
            ],
            dir.path(),
        );
        let out = std::process::Command::new(program)
            .args(args)
            .current_dir("/")
            .output()
            .unwrap();
        let text = String::from_utf8(out.stdout).unwrap();
        let mut lines = text.lines();
        let pwd = std::path::PathBuf::from(lines.next().unwrap());
        assert_eq!(
            pwd.canonicalize().unwrap(),
            dir.path().canonicalize().unwrap()
        );
        assert_eq!(
            lines.next().unwrap(),
            "a b|$HOME|",
            "arguments pass as they are"
        );
    }

    use super::*;
    use tokio::io::{AsyncWriteExt, DuplexStream};

    /// A session over in-memory pipes, and the peer's ends of them.
    fn pair(options: Options) -> (Session, BufReader<DuplexStream>, DuplexStream) {
        let (ours_in, theirs_out) = tokio::io::duplex(1 << 16);
        let (theirs_in, ours_out) = tokio::io::duplex(1 << 16);
        let session = Session::over(ours_in, ours_out, options);
        (session, BufReader::new(theirs_in), theirs_out)
    }

    async fn read_msg(reader: &mut BufReader<DuplexStream>) -> Value {
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        serde_json::from_str(&line).unwrap()
    }

    async fn write_msg(writer: &mut DuplexStream, msg: Value) {
        writer
            .write_all(format!("{msg}\n").as_bytes())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_call_goes_out_as_one_line_and_its_answer_comes_back() {
        let (session, mut rx, mut tx) = pair(Options::default());
        let call = session.call_now(
            "tools/list",
            json!({"cursor": null}),
            Duration::from_secs(5),
        );
        let sent = read_msg(&mut rx).await;
        assert_eq!(sent["jsonrpc"], "2.0");
        assert_eq!(sent["method"], "tools/list");
        let id = sent["id"].clone();
        write_msg(
            &mut tx,
            json!({"jsonrpc": "2.0", "id": id, "result": {"tools": []}}),
        )
        .await;
        assert_eq!(call.await.unwrap(), json!({"tools": []}));
    }

    #[tokio::test]
    async fn an_error_answer_keeps_its_code_and_data() {
        let (session, mut rx, mut tx) = pair(Options::default());
        let call = session.call_now("x", Value::Null, Duration::from_secs(5));
        let sent = read_msg(&mut rx).await;
        assert!(sent.get("params").is_none(), "null params are left out");
        write_msg(
            &mut tx,
            json!({"jsonrpc": "2.0", "id": sent["id"], "error": {
                "code": -32022, "message": "Unsupported protocol version",
                "data": {"supported": ["2025-11-25"]}
            }}),
        )
        .await;
        let CallError::Failed(failure) = call.await.unwrap_err() else {
            panic!("expected a failure");
        };
        assert_eq!(failure.code, Some(-32022));
        assert_eq!(failure.data.unwrap()["supported"][0], "2025-11-25");
    }

    #[tokio::test]
    async fn the_peers_requests_reach_the_consumer_and_are_answered_later() {
        let (session, mut rx, mut tx) = pair(Options::default());
        let mut peer = session.take_peer().unwrap();
        assert!(session.take_peer().is_none(), "one stream of peer messages");
        write_msg(
            &mut tx,
            json!({"jsonrpc": "2.0", "id": "p-1", "method": "session/request_permission", "params": {"a": 1}}),
        )
        .await;
        let PeerMsg::Request { id, method, params } = peer.recv().await.unwrap() else {
            panic!("expected a request");
        };
        assert_eq!(
            (id.clone(), method.as_str()),
            (json!("p-1"), "session/request_permission")
        );
        assert_eq!(params["a"], 1);
        session.respond(&id, Ok(json!({"outcome": {"outcome": "cancelled"}})));
        let answer = read_msg(&mut rx).await;
        assert_eq!(answer["id"], "p-1");
        assert_eq!(answer["result"]["outcome"]["outcome"], "cancelled");

        write_msg(
            &mut tx,
            json!({"jsonrpc": "2.0", "method": "session/update", "params": {"u": 2}}),
        )
        .await;
        assert_eq!(
            peer.recv().await.unwrap(),
            PeerMsg::Notification {
                method: "session/update".into(),
                params: json!({"u": 2})
            }
        );
    }

    #[tokio::test]
    async fn quick_requests_are_answered_inside_the_session() {
        let quick: Quick = Box::new(|method, _| (method == "ping").then(|| Ok(json!({}))));
        let (session, mut rx, mut tx) = pair(Options {
            quick,
            cancel: None,
        });
        let mut peer = session.take_peer().unwrap();
        write_msg(
            &mut tx,
            json!({"jsonrpc": "2.0", "id": 7, "method": "ping"}),
        )
        .await;
        let answer = read_msg(&mut rx).await;
        assert_eq!(answer, json!({"jsonrpc": "2.0", "id": 7, "result": {}}));
        assert!(peer.try_recv().is_err(), "a quick request is not passed on");
    }

    #[tokio::test]
    async fn batches_blank_lines_and_junk_lines_do_not_break_the_stream() {
        let (session, _rx, mut tx) = pair(Options::default());
        let mut peer = session.take_peer().unwrap();
        tx.write_all(b"\n  \r\nnot json\n").await.unwrap();
        write_msg(
            &mut tx,
            json!([
                {"jsonrpc": "2.0", "method": "a"},
                {"jsonrpc": "2.0", "method": "b"}
            ]),
        )
        .await;
        for want in ["a", "b"] {
            let PeerMsg::Notification { method, .. } = peer.recv().await.unwrap() else {
                panic!("expected a notification");
            };
            assert_eq!(method, want);
        }
        assert!(session.alive());
        assert!(session.stderr_tail().contains("not JSON"));
    }

    #[tokio::test]
    async fn an_abandoned_call_is_cancelled_the_protocols_way() {
        let (session, mut rx, _tx) = pair(Options {
            quick: Box::new(|_, _| None),
            cancel: Some(CancelStyle::Mcp),
        });
        let err = session
            .call("slow", Value::Null, Duration::from_millis(50))
            .await
            .unwrap_err();
        assert_eq!(err, CallError::TimedOut);
        let sent = read_msg(&mut rx).await;
        let cancel = read_msg(&mut rx).await;
        assert_eq!(cancel["method"], "notifications/cancelled");
        assert_eq!(cancel["params"]["requestId"], sent["id"]);
        assert!(cancel.get("id").is_none(), "a notification carries no id");
    }

    #[tokio::test]
    async fn the_end_is_reported_and_waiting_calls_fail() {
        let (session, mut rx, tx) = pair(Options::default());
        let call = session.call_now("x", Value::Null, Duration::from_secs(5));
        let _ = read_msg(&mut rx).await;
        drop(tx);
        let err = call.await.unwrap_err();
        assert!(matches!(err, CallError::Failed(_)), "{err:?}");
        assert_eq!(session.wait_ended().await, "it exited");
        assert!(!session.alive());
    }

    #[test]
    fn programs_resolve_as_paths_or_on_path() {
        assert_eq!(resolve_program(""), None);
        assert_eq!(
            resolve_program("/opt/x/bin/server"),
            Some(PathBuf::from("/opt/x/bin/server"))
        );
        assert_eq!(
            resolve_program("./bin/s"),
            None,
            "relative paths are refused"
        );
        #[cfg(unix)]
        assert!(resolve_program("sh").is_some_and(|p| p.ends_with("sh")));
        assert_eq!(resolve_program("clew-surely-not-installed-xyz"), None);
    }

    #[test]
    fn paths_merge_without_repeats() {
        let merged = merge_paths(Some("/a:/b".into()), Some("/b:.:/c".into())).unwrap();
        assert_eq!(merged, OsString::from("/a:/b:/c"));
        assert_eq!(merge_paths(None, None), None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_spawned_peer_speaks_over_its_stdio_and_its_stderr_is_kept() {
        let launch = Launch {
            program: "sh".into(),
            args: vec![
                "-c".into(),
                r#"echo starting >&2; read line; echo '{"jsonrpc":"2.0","id":1,"result":{"ok":true}}'; read rest"#.into(),
            ],
            env: vec![],
            cwd: None,
        };
        let session = Session::spawn(&launch, Options::default()).unwrap();
        let answer = session
            .call("hello", Value::Null, Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(answer, json!({"ok": true}));
        // stderr is read by a task of its own: give it a moment.
        for _ in 0..200 {
            if session.stderr_tail().contains("starting") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(session.stderr_tail().contains("starting"));
        session.stop().await;
        assert!(!session.alive());
    }
}
