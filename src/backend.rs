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
}
