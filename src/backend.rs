//! The backend seam for the client/server split.
//!
//! Everything that touches the OS — the filesystem, spawning processes (git,
//! LSP, DAP), watching for changes — goes through a `Backend`. Today there is
//! one implementation, `Local`, that runs against this machine exactly as before.
//! A future `Remote` variant will run the same operations on a `clew-server`
//! reached over SSH, so feature code is identical whether the code is local or
//! remote (see the client/server architecture notes).
//!
//! The methods are async because a remote backend does network I/O; the local
//! backend simply runs the blocking `std::fs` calls on a blocking thread, which
//! matches how clew already reads files off the UI thread.
//!
//! Phase 1 covers the filesystem primitives. Process spawning (for git / LSP /
//! DAP) and file watching move behind this seam in later phases.

#![allow(dead_code)] // Foundation for the client/server split; wired in incrementally.

use std::io;
use std::path::PathBuf;
use std::process::Stdio;

/// One entry from a directory listing.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    pub is_symlink: bool,
}

/// The subset of file metadata clew relies on (size + coarse mtime for the
/// stat-based cache fast path, plus the kind).
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct Meta {
    pub len: u64,
    pub is_dir: bool,
    pub mtime_ns: u128,
}

/// How to launch a subprocess. clew spawns `git` (one-shot), LSP servers and
/// debug adapters (long-lived, streaming stdio), and provisioning commands.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ProcessSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
}

impl ProcessSpec {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        ProcessSpec { program: program.into(), args: Vec::new(), cwd: None, env: Vec::new() }
    }
    pub fn arg(mut self, a: impl Into<String>) -> Self {
        self.args.push(a.into());
        self
    }
    pub fn args<I: IntoIterator<Item = S>, S: Into<String>>(mut self, a: I) -> Self {
        self.args.extend(a.into_iter().map(Into::into));
        self
    }
    pub fn cwd(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cwd = Some(dir.into());
        self
    }
}

/// The result of a one-shot process run.
#[derive(Debug, Clone)]
pub struct Output {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Output {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

/// A spawned long-lived process, its stdio exposed as trait objects so the LSP /
/// DAP protocol loops read and write bytes without caring whether the process is
/// local or tunnelled from a remote clew-server.
pub struct Spawned {
    pub stdin: Box<dyn tokio::io::AsyncWrite + Unpin + Send>,
    pub stdout: Box<dyn tokio::io::AsyncRead + Unpin + Send>,
    pub stderr: Box<dyn tokio::io::AsyncRead + Unpin + Send>,
    pub proc: Proc,
}

/// A handle to a running process, for waiting on / killing it.
pub enum Proc {
    Local(tokio::process::Child),
    // Remote(...) — a process id on the clew-server (Phase 4).
}

impl Proc {
    pub async fn wait(&mut self) -> io::Result<Option<i32>> {
        match self {
            Proc::Local(c) => Ok(c.wait().await?.code()),
        }
    }
    pub async fn kill(&mut self) {
        match self {
            Proc::Local(c) => {
                let _ = c.kill().await;
            }
        }
    }
}

/// Where clew's system operations are executed. Cloneable and cheap to pass
/// around (a handle, not the resources themselves).
#[derive(Debug, Clone)]
pub enum Backend {
    /// This machine, via `std::fs` / `std::process` (today's behavior).
    Local,
    // Remote(RemoteBackend) — talks to a clew-server over a transport (Phase 4).
}

impl Backend {
    /// Read a file's bytes.
    pub async fn read(&self, path: PathBuf) -> io::Result<Vec<u8>> {
        match self {
            Backend::Local => blocking(move || std::fs::read(&path)).await,
        }
    }

    /// Read a file's contents as UTF-8 text.
    pub async fn read_to_string(&self, path: PathBuf) -> io::Result<String> {
        match self {
            Backend::Local => blocking(move || std::fs::read_to_string(&path)).await,
        }
    }

    /// List one directory (non-recursive).
    pub async fn read_dir(&self, path: PathBuf) -> io::Result<Vec<DirEntry>> {
        match self {
            Backend::Local => {
                blocking(move || {
                    let mut out = Vec::new();
                    for entry in std::fs::read_dir(&path)? {
                        let entry = entry?;
                        let ft = entry.file_type()?;
                        out.push(DirEntry {
                            name: entry.file_name().to_string_lossy().into_owned(),
                            is_dir: ft.is_dir(),
                            is_symlink: ft.is_symlink(),
                        });
                    }
                    Ok(out)
                })
                .await
            }
        }
    }

    /// Stat a path (following symlinks), or `None` if it doesn't exist.
    pub async fn metadata(&self, path: PathBuf) -> io::Result<Meta> {
        match self {
            Backend::Local => {
                blocking(move || {
                    let m = std::fs::metadata(&path)?;
                    let mtime_ns = m
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_nanos())
                        .unwrap_or(0);
                    Ok(Meta { len: m.len(), is_dir: m.is_dir(), mtime_ns })
                })
                .await
            }
        }
    }

    /// Whether a path exists (following symlinks).
    pub async fn exists(&self, path: PathBuf) -> bool {
        match self {
            Backend::Local => blocking(move || Ok(path.exists())).await.unwrap_or(false),
        }
    }

    /// Write bytes to a path. Used for clew's own `.clew` cache/data, which lives
    /// wherever the backend runs (locally today; on the remote later).
    pub async fn write(&self, path: PathBuf, data: Vec<u8>) -> io::Result<()> {
        match self {
            Backend::Local => blocking(move || std::fs::write(&path, &data)).await,
        }
    }

    /// Run a process to completion, optionally feeding `stdin`, and collect its
    /// output. This is how one-shot commands (git, `--version` probes) run.
    pub async fn run(&self, spec: ProcessSpec, stdin: Option<Vec<u8>>) -> io::Result<Output> {
        match self {
            Backend::Local => {
                use tokio::io::AsyncWriteExt;
                let mut cmd = local_command(&spec);
                cmd.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() });
                cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
                let mut child = cmd.spawn()?;
                if let Some(data) = stdin
                    && let Some(mut si) = child.stdin.take()
                {
                    si.write_all(&data).await?;
                    si.shutdown().await?;
                }
                let out = child.wait_with_output().await?;
                Ok(Output { code: out.status.code(), stdout: out.stdout, stderr: out.stderr })
            }
        }
    }

    /// Spawn a long-lived process with piped stdio, for the LSP / DAP loops to
    /// talk to. The returned stdio is boxed so those loops are identical whether
    /// the process is local or tunnelled from a remote clew-server.
    pub async fn spawn(&self, spec: ProcessSpec) -> io::Result<Spawned> {
        match self {
            Backend::Local => {
                let mut cmd = local_command(&spec);
                cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
                let mut child = cmd.spawn()?;
                let stdin = Box::new(child.stdin.take().ok_or_else(|| io::Error::other("no stdin"))?);
                let stdout = Box::new(child.stdout.take().ok_or_else(|| io::Error::other("no stdout"))?);
                let stderr = Box::new(child.stderr.take().ok_or_else(|| io::Error::other("no stderr"))?);
                Ok(Spawned { stdin, stdout, stderr, proc: Proc::Local(child) })
            }
        }
    }
}

/// Build a `tokio` command from a spec (program, args, cwd, env).
fn local_command(spec: &ProcessSpec) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(&spec.program);
    cmd.args(&spec.args);
    if let Some(cwd) = &spec.cwd {
        cmd.current_dir(cwd);
    }
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    cmd
}

/// Run a blocking `std::fs`-style closure on a blocking thread, flattening the
/// join error into an `io::Error` so callers just see the operation's result.
async fn blocking<T, F>(f: F) -> io::Result<T>
where
    F: FnOnce() -> io::Result<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| io::Error::other(e.to_string()))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_backend_round_trips_fs() {
        let dir = std::env::temp_dir().join("clew-backend-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.txt");

        let b = Backend::Local;
        b.write(file.clone(), b"hello\n".to_vec()).await.unwrap();
        assert!(b.exists(file.clone()).await);
        assert_eq!(b.read_to_string(file.clone()).await.unwrap(), "hello\n");

        let meta = b.metadata(file.clone()).await.unwrap();
        assert_eq!(meta.len, 6);
        assert!(!meta.is_dir);

        let entries = b.read_dir(dir.clone()).await.unwrap();
        assert!(entries.iter().any(|e| e.name == "a.txt" && !e.is_dir));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn local_backend_runs_a_process_and_pipes_stdin() {
        let b = Backend::Local;
        // One-shot: collect stdout.
        let out = b.run(ProcessSpec::new("echo").arg("hi"), None).await.unwrap();
        assert!(out.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hi");

        // stdin is fed through: `cat` echoes it back.
        let out = b.run(ProcessSpec::new("cat"), Some(b"round trip\n".to_vec())).await.unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "round trip\n");
    }

    #[tokio::test]
    async fn local_backend_spawns_with_streaming_stdio() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let b = Backend::Local;
        // `cat` streams stdin → stdout; talk to it over the boxed handles.
        let Spawned { mut stdin, mut stdout, stderr: _stderr, mut proc } =
            b.spawn(ProcessSpec::new("cat")).await.unwrap();
        stdin.write_all(b"stream\n").await.unwrap();
        drop(stdin); // close cat's stdin → it hits EOF, echoes, and exits
        let mut got = String::new();
        stdout.read_to_string(&mut got).await.unwrap();
        assert_eq!(got, "stream\n");
        assert_eq!(proc.wait().await.unwrap(), Some(0));
    }
}
