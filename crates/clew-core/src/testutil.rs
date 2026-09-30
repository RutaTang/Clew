//! Test support: scratch directories (per guard, per test, per process),
//! environment changes that are always put back, a scoped `CLEW_DATA_DIR`,
//! git kept away from the developer's own configuration, tests run in a
//! child process of their own, and fake network peers — HTTP and TLS
//! servers, http(s) and SOCKS proxies — whose every wait is bounded.
//!
//! Public (not `cfg(test)`) for the reason [`crate::env_lock`] is: the GUI
//! crate's tests use the same helpers, and a `cfg(test)` item is invisible to
//! another crate. Nothing in a production path uses it.
//!
//! Tests used to work in FIXED names under the system temp dir
//! (`$TMPDIR/clew-git-merge`). Two checkouts — two worktrees, or CI shards on
//! one runner — running the same test at the same time then shared, and
//! deleted, each other's fixtures; a crashed run left them behind for the next
//! one to trip over. Every directory here is unique to its process and its
//! call, and removed when its guard drops, a panicking test included.

use std::cell::RefCell;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// A fresh, empty directory under the system temp dir, removed (with
/// everything in it) when dropped.
#[derive(Debug)]
pub struct TempDir(PathBuf);

impl TempDir {
    /// A new directory whose name carries `tag` (for a human reading a
    /// leftover), the process id and a per-process sequence number.
    pub fn new(tag: &str) -> TempDir {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "clew-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create a scratch directory");
        TempDir(dir)
    }

    /// The directory itself.
    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Keep the directory past this guard, until the process exits (see
    /// [`remove_at_exit`]) — for the one directory a whole test binary shares.
    pub fn until_exit(self) -> PathBuf {
        let path = self.0.clone();
        // Not dropped: that would remove it now.
        std::mem::forget(self);
        remove_at_exit(&path);
        path
    }
}

impl std::ops::Deref for TempDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for TempDir {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // A test may leave directories it took permissions from (to provoke
        // an unreadable entry); give them back so the removal can descend.
        #[cfg(unix)]
        restore_permissions(&self.0);
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Make every directory under `dir` user-accessible again (best effort).
#[cfg(unix)]
fn restore_permissions(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(meta) = std::fs::symlink_metadata(dir) else {
        return;
    };
    if !meta.is_dir() {
        return;
    }
    if meta.permissions().mode() & 0o700 != 0o700 {
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            restore_permissions(&entry.path());
        }
    }
}

/// `CLEW_DATA_DIR` pointed at a fresh [`TempDir`] while this lives, under
/// [`crate::env_lock`] — and put back exactly as it was (set, or unset) when
/// it drops, also when the test panics, so no later test inherits a data
/// directory that is about to be deleted.
pub struct DataDir {
    // Field order is drop order: the directory goes, then the lock is
    // released — after `Drop::drop` has restored the variable.
    dir: TempDir,
    previous: Option<OsString>,
    _env: crate::EnvLock,
}

impl DataDir {
    pub fn new(tag: &str) -> DataDir {
        let env = crate::env_lock();
        let dir = TempDir::new(tag);
        let previous = std::env::var_os("CLEW_DATA_DIR");
        // SAFETY: every test that touches the environment holds `env_lock`.
        unsafe { std::env::set_var("CLEW_DATA_DIR", dir.path()) };
        DataDir {
            dir,
            previous,
            _env: env,
        }
    }

    /// The data directory.
    pub fn path(&self) -> &Path {
        self.dir.path()
    }
}

impl std::ops::Deref for DataDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        self.dir.path()
    }
}

impl Drop for DataDir {
    fn drop(&mut self) {
        // SAFETY: still under `env_lock` (released after this, with `_env`).
        unsafe {
            match self.previous.take() {
                Some(previous) => std::env::set_var("CLEW_DATA_DIR", previous),
                None => std::env::remove_var("CLEW_DATA_DIR"),
            }
        }
    }
}

/// Environment variables changed for as long as this lives, under
/// [`crate::env_lock`] (held until it drops), and put back exactly as they were
/// — set to the old value, or unset — when it drops, a panicking test
/// included. A test that ends with a hand-written restore leaves the change in
/// place for every later test the moment an assertion before it fails.
///
/// For example `EnvVars::new().set("CLEW_UPDATE_API", url).remove("HOME")`.
#[must_use = "the variables are restored when the guard drops"]
pub struct EnvVars {
    // Restored in `Drop::drop`, before the lock (the field) is released.
    saved: Vec<(OsString, Option<OsString>)>,
    _env: crate::EnvLock,
}

impl EnvVars {
    /// Take the env lock; change nothing yet.
    pub fn new() -> EnvVars {
        EnvVars {
            saved: Vec::new(),
            _env: crate::env_lock(),
        }
    }

    /// Set `key` to `value` until the guard drops.
    pub fn set(mut self, key: &str, value: impl AsRef<OsStr>) -> EnvVars {
        self.remember(key);
        // SAFETY: under `env_lock`, which this guard holds.
        unsafe { std::env::set_var(key, value) };
        self
    }

    /// Unset `key` until the guard drops.
    pub fn remove(mut self, key: &str) -> EnvVars {
        self.remember(key);
        // SAFETY: under `env_lock`, which this guard holds.
        unsafe { std::env::remove_var(key) };
        self
    }

    /// Record `key`'s value from before this guard first touched it.
    fn remember(&mut self, key: &str) {
        if !self.saved.iter().any(|(k, _)| k == key) {
            self.saved.push((key.into(), std::env::var_os(key)));
        }
    }
}

impl Default for EnvVars {
    fn default() -> EnvVars {
        EnvVars::new()
    }
}

impl Drop for EnvVars {
    fn drop(&mut self) {
        for (key, previous) in self.saved.drain(..).rev() {
            // SAFETY: still under `env_lock` (released after this, with `_env`).
            unsafe {
                match previous {
                    Some(value) => std::env::set_var(&key, value),
                    None => std::env::remove_var(&key),
                }
            }
        }
    }
}

/// A directory for the calling TEST: `tag` inside a scratch directory private
/// to the test's thread, removed with everything in it when the test ends — a
/// failing test included. Calls with the same `tag` during one test return the
/// same path; the directory itself is not created.
///
/// For fixture helpers that hand a suite paths through many layers (a project
/// fixture that becomes an `App`'s root), where threading a guard back to every
/// caller would be all noise. It relies on libtest running each test on a
/// thread of its own and joining that thread — thread-local destructors
/// included — before it counts the test as done, so the destructor of this
/// thread-local IS the test's teardown. A path asked for on some other thread
/// the test started lives only as long as that thread.
pub fn scoped_dir(tag: &str) -> PathBuf {
    thread_local! {
        static SCOPE: RefCell<Option<TempDir>> = const { RefCell::new(None) };
    }
    SCOPE.with(|scope| {
        scope
            .borrow_mut()
            .get_or_insert_with(|| TempDir::new("test"))
            .join(tag)
    })
}

/// Run the freshly written executable `path` once, with no arguments and no
/// deadline, so the operating system's first-run check of a new file (macOS
/// assesses every new executable before its first exec: half a second on an
/// idle machine, past fifteen while a suite is starting fresh scripts of its
/// own) is paid here — not inside a timed operation under test, where it read
/// as a hung program. `path` must do nothing harmful when run bare.
///
/// The other first-run failure paid here is Linux's `ETXTBSY`: a child that
/// another thread of this process forked while `path` was still open for
/// writing inherited that descriptor, and until the child execs (closing it)
/// the kernel refuses to run the file. A parallel suite forks all the time,
/// so the window is met now and then; it lasts a moment, and this waits it
/// out rather than reading a busy file as one that does not run.
pub fn settle_new_executable(path: &Path) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let status = std::process::Command::new(path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        match status {
            Ok(_) => return,
            Err(e)
                if e.kind() == std::io::ErrorKind::ExecutableFileBusy
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(e) => panic!("{} does not run: {e}", path.display()),
        }
    }
}

/// Whether this process runs as root, who reads and writes past every file
/// mode: a test that observes a failed read or write through a `chmod` has
/// nothing to observe then, and returns early.
#[cfg(unix)]
pub fn running_as_root() -> bool {
    // SAFETY: `geteuid` takes nothing and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

/// Point git at no global and no system configuration for the rest of this
/// process: the developer's `~/.gitconfig` (a hook directory, a signing
/// setting, a pager) must not decide what a test sees, and git fails outright
/// when that file cannot be read. For a test binary's one-time setup, before
/// its tests run git (clew-core's own tests get the same through the git
/// runner itself).
pub fn isolate_git_config() {
    let _env = crate::env_lock();
    // SAFETY: under `env_lock`.
    unsafe {
        std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
        std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
    }
}

/// Paths [`remove_at_exit`] removes.
static AT_EXIT: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// Remove `path` (recursively) when this process exits — for the one
/// directory a test binary shares across all of its tests (a suite-wide
/// `CLEW_DATA_DIR`), which no single test's guard can own. The test harness
/// leaves through `exit` whether the suite passed or failed, so this runs
/// either way; a crash or a signal skips it, and the pid in the directory's
/// name then says which run it belonged to.
pub fn remove_at_exit(path: &Path) {
    static HOOK: std::sync::Once = std::sync::Once::new();
    AT_EXIT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(path.to_path_buf());
    #[cfg(unix)]
    HOOK.call_once(|| {
        // SAFETY: registers a plain `extern "C"` function; nothing else.
        unsafe { libc::atexit(remove_registered) };
    });
    #[cfg(not(unix))]
    let _ = &HOOK;
}

#[cfg(unix)]
extern "C" fn remove_registered() {
    let paths = std::mem::take(&mut *AT_EXIT.lock().unwrap_or_else(|e| e.into_inner()));
    for path in paths {
        restore_permissions(&path);
        let _ = std::fs::remove_dir_all(&path);
    }
}

/// Read one WHOLE HTTP request — headers plus the body its `Content-Length`
/// declares — off `conn`, for a fake server to answer; returns the body.
///
/// A fake server must not answer after a single `read`. Under load a request
/// arrives in several segments, and a server that answers and closes with
/// part of it unread resets the connection while the client is still reading
/// the answer. That reset is a real-world peer behaviour clew must survive
/// (see `net::guarded`), but a test that is not ABOUT it must not provoke it
/// at random.
pub fn read_http_request(conn: &mut impl HttpConn) -> String {
    read_http(conn).body
}

/// How long [`read_http`] waits for the rest of a request: a client that
/// stops short of its `Content-Length` and keeps the connection open fails
/// the test this long after, instead of hanging it for good.
pub const HTTP_READ_BOUND: std::time::Duration = std::time::Duration::from_secs(30);

/// A fake server's end of a connection: what [`read_http`] reads from, with
/// the TCP socket underneath, whose reads it bounds ([`HTTP_READ_BOUND`]).
pub trait HttpConn: std::io::Read {
    fn tcp(&self) -> &std::net::TcpStream;
}

impl HttpConn for std::net::TcpStream {
    fn tcp(&self) -> &std::net::TcpStream {
        self
    }
}

impl HttpConn for rustls::StreamOwned<rustls::ServerConnection, std::net::TcpStream> {
    fn tcp(&self) -> &std::net::TcpStream {
        &self.sock
    }
}

/// One HTTP request as a fake server read it (see [`read_http`]).
pub struct HttpRequest {
    /// The request line and the headers, as sent (CRLF-separated).
    pub head: String,
    pub body: String,
}

impl HttpRequest {
    /// The request target of the request line (`/v1/x?y`).
    pub fn path(&self) -> &str {
        self.head.split_whitespace().nth(1).unwrap_or("")
    }
}

/// [`read_http_request`], keeping the head too — for a fake server that
/// answers by path or checks what was sent. Each read waits at most
/// [`HTTP_READ_BOUND`].
pub fn read_http(conn: &mut impl HttpConn) -> HttpRequest {
    read_http_within(conn, HTTP_READ_BOUND)
}

/// [`read_http`], waiting at most `bound` for each read; the connection's
/// own read timeout is put back afterwards. For a fake server whose test is
/// ABOUT a request that stops short: it then fails that fast, rather than
/// [`HTTP_READ_BOUND`] later.
pub fn read_http_within(conn: &mut impl HttpConn, bound: std::time::Duration) -> HttpRequest {
    let before = conn.tcp().read_timeout().ok().flatten();
    let _ = conn.tcp().set_read_timeout(Some(bound));
    let request = read_http_unbounded(conn);
    let _ = conn.tcp().set_read_timeout(before);
    request
}

fn read_http_unbounded(conn: &mut impl std::io::Read) -> HttpRequest {
    let mut req: Vec<u8> = Vec::new();
    let mut buf = [0u8; 8192];
    let mut body_start: Option<usize> = None;
    let mut body_len = 0usize;
    loop {
        match conn.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => req.extend_from_slice(&buf[..n]),
        }
        if body_start.is_none()
            && let Some(pos) = req.windows(4).position(|w| w == b"\r\n\r\n")
        {
            body_start = Some(pos + 4);
            body_len = String::from_utf8_lossy(&req[..pos])
                .lines()
                .find_map(|l| {
                    let (name, value) = l.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())?
                })
                .unwrap_or(0);
        }
        if let Some(start) = body_start
            && req.len() >= start + body_len
        {
            break;
        }
    }
    let start = body_start.unwrap_or(req.len());
    HttpRequest {
        head: String::from_utf8_lossy(&req[..start])
            .trim_end()
            .to_string(),
        body: String::from_utf8_lossy(&req[start..]).into_owned(),
    }
}

// ------------------------------------------------------------ state files

/// What [`crate::statefile::merge_file`] leaves when the process dies after
/// writing the store and before confirming the edit: the store merged, the
/// edit still pending in the store's ledger. The merge's own result is an
/// error (the "crash"). For tests of the writers that must settle it.
pub fn merge_crashing_after_store_write(
    path: &Path,
    op: &clew_protocol::StateMerge,
    edit_id: &str,
) -> std::io::Result<crate::statefile::Merged> {
    crate::statefile::merge_file_until(
        path,
        op,
        edit_id,
        crate::statefile::Interrupt::AfterStoreWrite,
    )
}

// --------------------------------------------------------------------- TLS

/// A throwaway self-signed certificate for `127.0.0.1` and `localhost`, and
/// its key, for loopback TLS fakes. It protects nothing: only tests trust it,
/// explicitly ([`trusting_the_test_cert`]), and it signs nothing but their
/// handshakes.
pub const TEST_CERT: &str = "-----BEGIN CERTIFICATE-----\n\
MIIB0DCCAXegAwIBAgIUW5s1AG+u30ceptc9C2KiA7OsOhQwCgYIKoZIzj0EAwIw\n\
HTEbMBkGA1UEAwwSY2xldy10ZXN0LXByb3ZpZGVyMCAXDTI2MDkyNjIxMDA1OFoY\n\
DzIxMjYwOTAyMjEwMDU4WjAdMRswGQYDVQQDDBJjbGV3LXRlc3QtcHJvdmlkZXIw\n\
WTATBgcqhkjOPQIBBggqhkjOPQMBBwNCAAQVxeWI4kOpbX/NyZOM8xlj/Re5JLHs\n\
JePrDf6+JXQg4PHGeMQ9xRkJ39/IhFzw7cVUWgq9W8oZK3isPEw2BEJwo4GSMIGP\n\
MB0GA1UdDgQWBBTZgpsVOfy8lU0aZj3xSHv+AqgjczAfBgNVHSMEGDAWgBTZgpsV\n\
Ofy8lU0aZj3xSHv+AqgjczAaBgNVHREEEzARhwR/AAABgglsb2NhbGhvc3QwDAYD\n\
VR0TAQH/BAIwADAOBgNVHQ8BAf8EBAMCB4AwEwYDVR0lBAwwCgYIKwYBBQUHAwEw\n\
CgYIKoZIzj0EAwIDRwAwRAIgFbw00fYBiSW/y4mRomDQCtDWoiHaJR0s6i3plCap\n\
cw0CIFwa+qd5rHCpn8sKMDg3jnJzb0Iddn6PQlD2zoR9a42m\n\
-----END CERTIFICATE-----\n";
pub const TEST_KEY: &str = "-----BEGIN PRIVATE KEY-----\n\
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgp6km6GtvyAAAV9eg\n\
8e+YTkJFQgnmpqQ9Wi3ysuDEqVWhRANCAAQVxeWI4kOpbX/NyZOM8xlj/Re5JLHs\n\
JePrDf6+JXQg4PHGeMQ9xRkJ39/IhFzw7cVUWgq9W8oZK3isPEw2BEJw\n\
-----END PRIVATE KEY-----\n";

/// The server side of a loopback TLS fake: [`TEST_CERT`], TLS 1.3.
pub fn test_tls_server_config() -> std::sync::Arc<rustls::ServerConfig> {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    std::sync::Arc::new(
        rustls::ServerConfig::builder_with_provider(ring_provider())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .expect("the ring provider speaks TLS 1.3")
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from_pem_slice(TEST_CERT.as_bytes()).expect("TEST_CERT")],
                PrivateKeyDer::from_pem_slice(TEST_KEY.as_bytes()).expect("TEST_KEY"),
            )
            .expect("TEST_CERT and TEST_KEY are a pair"),
    )
}

/// Client trust for a loopback TLS fake: [`TEST_CERT`] and nothing else.
pub fn trusting_the_test_cert() -> std::sync::Arc<rustls::ClientConfig> {
    use rustls::pki_types::CertificateDer;
    use rustls::pki_types::pem::PemObject;
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(CertificateDer::from_pem_slice(TEST_CERT.as_bytes()).expect("TEST_CERT"))
        .expect("TEST_CERT is a usable root");
    std::sync::Arc::new(
        rustls::ClientConfig::builder_with_provider(ring_provider())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .expect("the ring provider speaks TLS 1.3")
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
}

/// The server's end of a loopback TLS connection.
pub type TlsConn = rustls::StreamOwned<rustls::ServerConnection, std::net::TcpStream>;

/// One accepted connection's script for [`tls_serve`]: it runs once the
/// handshake is done and the whole request is read.
pub type TlsScript = Box<dyn FnOnce(&mut TlsConn) + Send>;

/// A loopback HTTPS server with [`TEST_CERT`] that hands each connection it
/// accepts to the next of `scripts` (a connection whose handshake fails
/// uses one up too). Returns its base URL, `https://127.0.0.1:<port>`.
pub fn tls_serve(scripts: Vec<TlsScript>) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a loopback port");
    let base = format!(
        "https://{}",
        listener.local_addr().expect("the listener's address")
    );
    let config = test_tls_server_config();
    std::thread::spawn(move || {
        for script in scripts {
            let Ok((tcp, _)) = listener.accept() else {
                return;
            };
            let config = config.clone();
            std::thread::spawn(move || {
                let Ok(server) = rustls::ServerConnection::new(config) else {
                    return;
                };
                let mut tls = rustls::StreamOwned::new(server, tcp);
                read_http(&mut tls);
                script(&mut tls);
            });
        }
    });
    base
}

/// A [`tls_serve`] script that answers `response` and closes.
pub fn tls_answer(response: &'static [u8]) -> TlsScript {
    Box::new(move |tls: &mut TlsConn| {
        use std::io::Write;
        let _ = tls.write_all(response);
        tls.conn.send_close_notify();
        let _ = tls.flush();
    })
}

/// What a silent fake peer ([`silent_socks_proxy`], [`silent_http_endpoint`])
/// reports about the client's side of its one connection: whether it went
/// away (`true`, and when), or had not within [`HTTP_READ_BOUND`] (`false`).
pub type PeerClosed = std::sync::mpsc::Receiver<(bool, std::time::Instant)>;

/// Wait on `conn`, whose client is waiting for an answer that never comes,
/// for that client to go away — at most [`HTTP_READ_BOUND`] — and tell
/// `closed` whether it did, and when ([`PeerClosed`]).
fn report_close(
    conn: &mut std::net::TcpStream,
    closed: &std::sync::mpsc::Sender<(bool, std::time::Instant)>,
) {
    use std::io::Read;
    let _ = conn.set_read_timeout(Some(HTTP_READ_BOUND));
    let gone = match conn.read(&mut [0u8; 1]) {
        Ok(_) => true,
        Err(e) => !matches!(
            e.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ),
    };
    let _ = closed.send((gone, std::time::Instant::now()));
}

/// A wedged SOCKS proxy on loopback: it accepts one connection, reads the
/// client's SOCKS5 greeting, and never answers. Returns its setting
/// (`socks5h://127.0.0.1:<port>`: the target's name is the proxy's to
/// resolve, so a name that resolves nowhere still reaches it), a channel
/// told once the greeting is in, and one told when the client's side of the
/// connection went away ([`PeerClosed`]).
pub fn silent_socks_proxy() -> (String, std::sync::mpsc::Receiver<()>, PeerClosed) {
    use std::io::Read;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a loopback port");
    let spec = format!(
        "socks5h://{}",
        listener.local_addr().expect("the listener's address")
    );
    let (greeted_tx, greeted) = std::sync::mpsc::channel();
    let (closed_tx, closed) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let Ok((mut conn, _)) = listener.accept() else {
            return;
        };
        // Bounded, so a client that never lets go fails its test instead of
        // hanging it.
        let _ = conn.set_read_timeout(Some(HTTP_READ_BOUND));
        let mut greeting = [0u8; 3];
        if conn.read_exact(&mut greeting).is_err() {
            return;
        }
        let _ = greeted_tx.send(());
        report_close(&mut conn, &closed_tx);
    });
    (spec, greeted, closed)
}

/// A wedged HTTP endpoint on loopback — a model server still loading, a
/// gateway whose upstream went quiet: it accepts one connection, reads the
/// request on it whole ([`read_http`]), and never answers. Returns its base
/// URL (`http://127.0.0.1:<port>`), a channel told once the request is in,
/// and one told when the client's side of the connection went away
/// ([`PeerClosed`]) — which, for a client that gives the request up, is
/// what reaches the endpoint of that.
pub fn silent_http_endpoint() -> (String, std::sync::mpsc::Receiver<()>, PeerClosed) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a loopback port");
    let base = format!(
        "http://{}",
        listener.local_addr().expect("the listener's address")
    );
    let (arrived_tx, arrived) = std::sync::mpsc::channel();
    let (closed_tx, closed) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let Ok((mut conn, _)) = listener.accept() else {
            return;
        };
        if read_http(&mut conn).head.is_empty() {
            return;
        }
        let _ = arrived_tx.send(());
        report_close(&mut conn, &closed_tx);
    });
    (base, arrived, closed)
}

fn ring_provider() -> std::sync::Arc<rustls::crypto::CryptoProvider> {
    std::sync::Arc::new(rustls::crypto::ring::default_provider())
}

/// The end of a TLS session inside a proxy's TLS session: a tunnel through an
/// `https://` proxy, ended by the proxy itself ([`http_proxy`]).
impl HttpConn for rustls::StreamOwned<rustls::ServerConnection, TlsConn> {
    fn tcp(&self) -> &std::net::TcpStream {
        &self.sock.sock
    }
}

/// The answer [`http_proxy`] gives every request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyAnswer {
    /// Grant a `CONNECT` — the tunnel then ended by a TLS server with
    /// [`TEST_CERT`] answering `ok` — and answer any other request `ok`.
    Serve,
    /// Answer every request, `CONNECT` or not, with this status (offering
    /// Basic logins).
    Status(u16),
    /// Answer every request with a `407` offering the logins of this
    /// `Proxy-Authenticate` value.
    Login(&'static str),
    /// Close the connection once the request is in, answering nothing: a
    /// proxy that refuses a login by hanging up.
    HangUp,
    /// Hold every request this long, then answer it with this status: a
    /// proxy that answers a `CONNECT` only once its connect to the target
    /// has ended, and a target slow to take it.
    Late(u16, std::time::Duration),
}

/// A fake http proxy listening on `bind` (`127.0.0.1:0`, `[::1]:0`), spoken
/// to over TLS with [`TEST_CERT`] when `tls`, for `connections` connections.
/// It answers as `answer` says, and reports the head of every request it
/// reads — its own, and a tunnel's. Returns its URL
/// (`http://127.0.0.1:<port>`, `https://…`, `http://[::1]:<port>`), or
/// `None` when `bind` cannot be bound (a host without IPv6).
pub fn http_proxy(
    bind: &str,
    tls: bool,
    connections: usize,
    answer: ProxyAnswer,
) -> Option<(String, std::sync::mpsc::Receiver<String>)> {
    let listener = std::net::TcpListener::bind(bind).ok()?;
    let addr = listener.local_addr().ok()?;
    let url = format!("{}://{addr}", if tls { "https" } else { "http" });
    let (heads_tx, heads) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for _ in 0..connections {
            let Ok((tcp, _)) = listener.accept() else {
                return;
            };
            let heads = heads_tx.clone();
            std::thread::spawn(move || {
                if tls {
                    let Ok(server) = rustls::ServerConnection::new(test_tls_server_config()) else {
                        return;
                    };
                    let conn = rustls::StreamOwned::new(server, tcp);
                    proxy_session(conn, answer, &heads, end_tunnel);
                } else {
                    proxy_session(tcp, answer, &heads, end_tunnel);
                }
            });
        }
    });
    Some((url, heads))
}

/// One connection of [`http_proxy`]'s: the request's head reported, and the
/// answer given — for a granted `CONNECT`, by `tunnel` on the connection.
fn proxy_session<C: HttpConn + std::io::Write>(
    mut conn: C,
    answer: ProxyAnswer,
    heads: &std::sync::mpsc::Sender<String>,
    tunnel: impl FnOnce(C, &std::sync::mpsc::Sender<String>),
) {
    let head = read_http(&mut conn).head;
    let connect = head.starts_with("CONNECT ");
    let _ = heads.send(head);
    // Every wait below bounded, so a client that never lets go fails its
    // test instead of hanging it.
    let _ = conn.tcp().set_read_timeout(Some(HTTP_READ_BOUND));
    let refuse = |conn: &mut C, status: u16, logins: &str| {
        let _ = write!(
            conn,
            "HTTP/1.1 {status} Refused\r\nProxy-Authenticate: {logins}\r\n\
             content-length: 0\r\nconnection: close\r\n\r\n"
        );
        let _ = conn.flush();
        // Held until the client lets go, so the answer is read whole.
        let _ = conn.read(&mut [0u8; 1]);
    };
    match answer {
        ProxyAnswer::Status(status) => refuse(&mut conn, status, "Basic realm=\"test\""),
        ProxyAnswer::Login(logins) => refuse(&mut conn, 407, logins),
        ProxyAnswer::HangUp => drop(conn),
        ProxyAnswer::Late(status, after) => {
            std::thread::sleep(after);
            refuse(&mut conn, status, "Basic realm=\"test\"");
        }
        ProxyAnswer::Serve if connect => {
            let _ = conn.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n");
            let _ = conn.flush();
            tunnel(conn, heads);
        }
        ProxyAnswer::Serve => {
            answer_ok(&mut conn);
            let _ = conn.read(&mut [0u8; 1]);
        }
    }
}

/// The end of a tunnel [`http_proxy`] granted: a TLS server with
/// [`TEST_CERT`] on it, which reports the request's head and answers `ok`.
fn end_tunnel<C: std::io::Read + std::io::Write>(conn: C, heads: &std::sync::mpsc::Sender<String>)
where
    rustls::StreamOwned<rustls::ServerConnection, C>: HttpConn,
{
    let Ok(server) = rustls::ServerConnection::new(test_tls_server_config()) else {
        return;
    };
    let mut inner = rustls::StreamOwned::new(server, conn);
    let _ = heads.send(read_http(&mut inner).head);
    answer_ok(&mut inner);
    inner.conn.send_close_notify();
    let _ = std::io::Write::flush(&mut inner);
}

/// A complete `200` answer with the body `ok`, closing the connection.
fn answer_ok(conn: &mut impl std::io::Write) {
    let _ = conn.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok");
    let _ = conn.flush();
}

/// What [`socks5_proxy`] saw of its one client.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SocksSeen {
    /// The ways to log in the client offered.
    pub methods: Vec<u8>,
    /// The user name and password it logged in with, if it did.
    pub login: Option<(String, String)>,
    /// The command of its request (1: `CONNECT`).
    pub command: u8,
    /// The target of its request: the address type (1: IPv4, 3: a name,
    /// 4: IPv6), the address or name, and the port.
    pub target: Option<(u8, String, u16)>,
}

/// What [`socks5_proxy`] does with a `CONNECT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SocksAnswer {
    /// Connect to the target (a name resolved here) and relay both ways.
    Relay,
    /// Connect to this address, whatever the target — a name only the
    /// proxy's side of a network knows — and relay both ways.
    RelayTo(std::net::SocketAddr),
    /// Refuse with this SOCKS5 reply code (5: connection refused).
    Refuse(u8),
    /// Close the connection once the client has sent its user name and
    /// password (with a `login`) or its `CONNECT` (without one), answering
    /// nothing: a proxy that refuses a login by hanging up.
    HangUp,
}

/// A fake SOCKS5 proxy on loopback for one connection: it takes the client
/// in — with `login`'s user name and password when there is one, refusing
/// any other — and answers its `CONNECT` as `answer` says. Returns its
/// address (`127.0.0.1:<port>`), and a channel told what it saw once the
/// handshake is over.
pub fn socks5_proxy(
    login: Option<(&'static str, &'static str)>,
    answer: SocksAnswer,
) -> (String, std::sync::mpsc::Receiver<SocksSeen>) {
    socks5_proxy_for(1, login, answer)
}

/// [`socks5_proxy`] for `connections` connections, one after the other: the
/// channel is told what it saw of each.
pub fn socks5_proxy_for(
    connections: usize,
    login: Option<(&'static str, &'static str)>,
    answer: SocksAnswer,
) -> (String, std::sync::mpsc::Receiver<SocksSeen>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a loopback port");
    let addr = listener
        .local_addr()
        .expect("the listener's address")
        .to_string();
    let (seen_tx, seen) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for _ in 0..connections {
            let Ok((conn, _)) = listener.accept() else {
                return;
            };
            let seen_tx = seen_tx.clone();
            std::thread::spawn(move || socks5_session(conn, login, answer, &seen_tx));
        }
    });
    (addr, seen)
}

/// One connection of [`socks5_proxy_for`]'s.
fn socks5_session(
    mut conn: std::net::TcpStream,
    login: Option<(&'static str, &'static str)>,
    answer: SocksAnswer,
    seen_tx: &std::sync::mpsc::Sender<SocksSeen>,
) {
    use std::io::{Read, Write};
    let _ = conn.set_read_timeout(Some(HTTP_READ_BOUND));
    let mut saw = SocksSeen::default();
    let result = (|| -> std::io::Result<Option<std::net::TcpStream>> {
        let mut head = [0u8; 2];
        conn.read_exact(&mut head)?;
        saw.methods = vec![0u8; usize::from(head[1])];
        conn.read_exact(&mut saw.methods)?;
        if let Some((user, password)) = login {
            if !saw.methods.contains(&2) {
                conn.write_all(&[5, 0xFF])?;
                return Ok(None);
            }
            conn.write_all(&[5, 2])?;
            let field = |conn: &mut std::net::TcpStream| -> std::io::Result<String> {
                let mut len = [0u8; 1];
                conn.read_exact(&mut len)?;
                let mut text = vec![0u8; usize::from(len[0])];
                conn.read_exact(&mut text)?;
                Ok(String::from_utf8_lossy(&text).into_owned())
            };
            let mut version = [0u8; 1];
            conn.read_exact(&mut version)?;
            let given = (field(&mut conn)?, field(&mut conn)?);
            let right = given.0 == user && given.1 == password;
            saw.login = Some(given);
            if answer == SocksAnswer::HangUp {
                return Ok(None);
            }
            conn.write_all(&[1, if right { 0 } else { 1 }])?;
            if !right {
                return Ok(None);
            }
        } else {
            conn.write_all(&[5, 0])?;
        }
        let mut request = [0u8; 4];
        conn.read_exact(&mut request)?;
        let host = match request[3] {
            1 => {
                let mut ip = [0u8; 4];
                conn.read_exact(&mut ip)?;
                std::net::Ipv4Addr::from(ip).to_string()
            }
            4 => {
                let mut ip = [0u8; 16];
                conn.read_exact(&mut ip)?;
                std::net::Ipv6Addr::from(ip).to_string()
            }
            _ => {
                let mut len = [0u8; 1];
                conn.read_exact(&mut len)?;
                let mut name = vec![0u8; usize::from(len[0])];
                conn.read_exact(&mut name)?;
                String::from_utf8_lossy(&name).into_owned()
            }
        };
        let mut port = [0u8; 2];
        conn.read_exact(&mut port)?;
        let port = u16::from_be_bytes(port);
        saw.command = request[1];
        saw.target = Some((request[3], host.clone(), port));
        let upstream = match answer {
            SocksAnswer::Refuse(code) => {
                conn.write_all(&[5, code, 0, 1, 0, 0, 0, 0, 0, 0])?;
                return Ok(None);
            }
            SocksAnswer::HangUp => return Ok(None),
            SocksAnswer::RelayTo(to) => std::net::TcpStream::connect(to)?,
            SocksAnswer::Relay => std::net::TcpStream::connect((host.as_str(), port))?,
        };
        conn.write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0])?;
        Ok(Some(upstream))
    })();
    let _ = seen_tx.send(saw);
    if answer == SocksAnswer::HangUp {
        return;
    }
    let Ok(Some(mut upstream)) = result else {
        // Held until the client lets go, so it reads the answer whole.
        let _ = conn.read(&mut [0u8; 1]);
        return;
    };
    let _ = upstream.set_read_timeout(Some(HTTP_READ_BOUND));
    let (Ok(mut down_from), Ok(mut down_to)) = (upstream.try_clone(), conn.try_clone()) else {
        return;
    };
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut down_from, &mut down_to);
        let _ = down_to.shutdown(std::net::Shutdown::Write);
    });
    let _ = std::io::copy(&mut conn, &mut upstream);
    let _ = upstream.shutdown(std::net::Shutdown::Write);
}

/// Close `conn` with a reset instead of a FIN (`SO_LINGER` of zero): the
/// peer that drops a connection abruptly.
pub fn abort(conn: std::net::TcpStream) {
    use std::os::fd::AsRawFd;
    let linger = libc::linger {
        l_onoff: 1,
        l_linger: 0,
    };
    // SAFETY: a valid socket descriptor, and an option value of the size
    // passed.
    let rc = unsafe {
        libc::setsockopt(
            conn.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_LINGER,
            (&raw const linger).cast(),
            std::mem::size_of::<libc::linger>() as libc::socklen_t,
        )
    };
    assert_eq!(rc, 0, "SO_LINGER: {}", std::io::Error::last_os_error());
    drop(conn);
}

/// A plain-http peer on loopback that answers the first request with `head`,
/// then sends `unit` over and over — a response that never ends — until
/// the client stops taking it, or `bound` bytes of it are out. Returns its
/// base URL, and a channel told how many bytes of `unit` went out.
pub fn endless_http(
    head: &'static [u8],
    unit: &'static [u8],
    bound: u64,
) -> (String, std::sync::mpsc::Receiver<u64>) {
    use std::io::Write;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a loopback port");
    let base = format!(
        "http://{}",
        listener.local_addr().expect("the listener's address")
    );
    let (sent_tx, sent) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let Ok((mut conn, _)) = listener.accept() else {
            return;
        };
        read_http(&mut conn);
        // Bounded: a client that neither reads nor lets go ends this too.
        let _ = conn.set_write_timeout(Some(HTTP_READ_BOUND));
        let mut total = 0u64;
        if conn.write_all(head).is_ok() {
            while total < bound && conn.write_all(unit).is_ok() {
                total += unit.len() as u64;
            }
        }
        let _ = sent_tx.send(total);
        // Held until the client lets go (or the bound), so the end of the
        // sending is the client's doing, not a close of this side's.
        let _ = conn.set_read_timeout(Some(HTTP_READ_BOUND));
        let _ = std::io::Read::read(&mut conn, &mut [0u8; 1]);
    });
    (base, sent)
}

// ------------------------------------------------------------- child tests

/// Set in a child started by [`run_in_child`], for the child-side test.
const CHILD_MARK: &str = "CLEW_CORE_TEST_CHILD";

/// Whether this process is such a child. A child-side test returns at once
/// otherwise, so running the ignored tests directly passes without doing
/// anything.
pub fn in_child() -> bool {
    std::env::var_os(CHILD_MARK).is_some()
}

/// Run the ignored test `name` alone in a child process of this test binary,
/// with `env` applied there (`None` removes a variable), failing unless it
/// ran and passed.
///
/// For a test that needs the process environment as it is from birth. Set
/// with `set_var` in this process, a variable races every test running beside
/// it: requests read the proxy variables on every call, unaware of any test
/// lock, and libc reads the environment underneath DNS and the system trust
/// store with no lock of ours at all. A child has its environment from birth
/// and runs that one test alone.
pub fn run_in_child(name: &str, env: &[(&str, Option<&OsStr>)]) {
    let mut child = std::process::Command::new(std::env::current_exe().expect("the test binary"));
    child
        .args([
            name,
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_MARK, "1");
    for (key, value) in env {
        match value {
            Some(value) => child.env(key, value),
            None => child.env_remove(key),
        };
    }
    let out = child.output().expect("the test binary runs again");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && stdout.contains("1 passed"),
        "{name} failed in its child process ({}):\n{stdout}\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_dirs_are_unique_and_removed() {
        let a = TempDir::new("testutil");
        let b = TempDir::new("testutil");
        assert_ne!(a.path(), b.path());
        std::fs::write(a.join("x"), "x").unwrap();
        let kept = a.path().to_path_buf();
        drop(a);
        assert!(!kept.exists());
    }

    #[test]
    fn env_vars_are_restored_even_after_a_panic() {
        const SET: &str = "CLEW_TESTUTIL_PROBE_SET";
        const GONE: &str = "CLEW_TESTUTIL_PROBE_GONE";
        {
            let _env = EnvVars::new().set(GONE, "before");
            let caught = std::panic::catch_unwind(|| {
                let _vars = EnvVars::new().set(SET, "during").remove(GONE);
                assert_eq!(std::env::var_os(SET).as_deref(), Some(OsStr::new("during")));
                assert_eq!(std::env::var_os(GONE), None);
                panic!("the test body fails");
            });
            assert!(caught.is_err());
            assert_eq!(std::env::var_os(SET), None, "a new variable must go again");
            assert_eq!(
                std::env::var_os(GONE).as_deref(),
                Some(OsStr::new("before")),
                "a removed variable must come back"
            );
        }
        let _env = crate::env_lock();
        assert_eq!(std::env::var_os(GONE), None);
    }

    /// The lock is re-entrant: a helper that locks for itself, reached by a
    /// test already holding the lock, must not wait on its own caller. It is
    /// released only with the outermost guard, and then other threads get it.
    #[test]
    fn the_env_lock_is_reentrant_and_released_by_the_outermost_guard() {
        let outer = crate::env_lock();
        let inner = crate::env_lock();
        let data = DataDir::new("testutil-reentrant");
        drop(data);
        drop(inner);
        let (tx, rx) = std::sync::mpsc::channel();
        let other = std::thread::spawn(move || {
            let _held = crate::env_lock();
            tx.send(()).unwrap();
        });
        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(200))
                .is_err(),
            "another thread took the lock while the outer guard lived"
        );
        drop(outer);
        rx.recv_timeout(std::time::Duration::from_secs(10))
            .expect("the lock was not released with the outermost guard");
        other.join().unwrap();
    }

    /// A test's scoped directories are gone once its thread has ended, also
    /// when it panicked.
    #[test]
    fn scoped_dirs_are_removed_when_the_test_thread_ends() {
        for fail in [false, true] {
            let (tx, rx) = std::sync::mpsc::channel();
            let worker = std::thread::spawn(move || {
                let first = scoped_dir("fixture");
                assert_eq!(first, scoped_dir("fixture"), "one tag, one path");
                assert_ne!(first, scoped_dir("other"));
                std::fs::create_dir_all(first.join("nested")).unwrap();
                tx.send(first).unwrap();
                assert!(!fail, "the test body fails");
            });
            let dir = rx.recv().unwrap();
            assert_eq!(worker.join().is_err(), fail);
            assert!(!dir.exists(), "{} outlived its test", dir.display());
            assert!(!dir.parent().unwrap().exists());
        }
    }

    #[test]
    fn the_data_dir_is_restored_even_after_a_panic() {
        let before = {
            let _env = crate::env_lock();
            std::env::var_os("CLEW_DATA_DIR")
        };
        let caught = std::panic::catch_unwind(|| {
            let data = DataDir::new("testutil-data");
            assert_eq!(
                std::env::var_os("CLEW_DATA_DIR").as_deref(),
                Some(data.path().as_os_str())
            );
            panic!("the test body fails");
        });
        assert!(caught.is_err());
        let _env = crate::env_lock();
        assert_eq!(std::env::var_os("CLEW_DATA_DIR"), before);
    }

    /// A client that sends less than its `Content-Length` and keeps the
    /// connection open ends the read at the bound — the test that fake
    /// server belongs to fails, instead of hanging — and the connection's own
    /// read timeout is put back.
    #[test]
    fn a_short_request_is_read_within_the_bound() {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        client
            .write_all(b"POST /v1 HTTP/1.1\r\nContent-Length: 10\r\n\r\nabc")
            .unwrap();
        let (mut conn, _) = listener.accept().unwrap();
        let (done_tx, done) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let request = read_http_within(&mut conn, std::time::Duration::from_millis(200));
            let _ = done_tx.send((
                request.path().to_string(),
                request.body,
                conn.read_timeout(),
            ));
        });
        let (path, body, timeout) = done
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the read waited past its bound for bytes that never came");
        assert_eq!((path.as_str(), body.as_str()), ("/v1", "abc"));
        assert_eq!(
            timeout.unwrap(),
            None,
            "the connection's own timeout is back"
        );
        drop(client);
    }

    /// `read_http` itself is bounded: every read it makes waits at most
    /// `HTTP_READ_BOUND`, and the connection's own timeout is back afterwards.
    /// Observed on the reads, not waited out, so this stays fast — and the
    /// test above, which names its own bound, cannot tell a `read_http` that
    /// stopped bounding its reads from one that still does.
    #[test]
    fn read_http_bounds_every_read_it_makes() {
        use std::io::Write;
        /// A server's end that records the read timeout in force at each read.
        struct Probe {
            tcp: std::net::TcpStream,
            seen: Vec<Option<std::time::Duration>>,
        }
        impl std::io::Read for Probe {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                self.seen.push(self.tcp.read_timeout()?);
                self.tcp.read(buf)
            }
        }
        impl HttpConn for Probe {
            fn tcp(&self) -> &std::net::TcpStream {
                &self.tcp
            }
        }

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        client
            .write_all(b"POST /v1 HTTP/1.1\r\nContent-Length: 3\r\n\r\nabc")
            .unwrap();
        let (tcp, _) = listener.accept().unwrap();
        let mut probe = Probe {
            tcp,
            seen: Vec::new(),
        };
        let request = read_http(&mut probe);
        assert_eq!((request.path(), request.body.as_str()), ("/v1", "abc"));
        assert!(
            !probe.seen.is_empty() && probe.seen.iter().all(|t| *t == Some(HTTP_READ_BOUND)),
            "a read waited without HTTP_READ_BOUND: {:?}",
            probe.seen
        );
        assert_eq!(
            probe.tcp.read_timeout().unwrap(),
            None,
            "the connection's own timeout is back"
        );
        drop(client);
    }
}
