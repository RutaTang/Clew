//! How `clew-server mcp`, which has no window, asks Clew's window to do what
//! only the window may: try a build on the open project, or ask the user to
//! install it.
//!
//! An ACP agent reaches Clew's plugin tools through `clew-server mcp`, a
//! process of its own, while the user and the plugin runtime are in Clew's
//! window. So the MCP server files a request in `<data>/plugins/requests/`
//! (`<nonce>.request.json`, written atomically), the window picks it up,
//! does it, and writes `<nonce>.answer.json`; the tool waits for that.
//! A request names the build by its file's digest: the window runs, or asks
//! about, exactly the bytes the agent built, read from Clew's own build
//! folder. A window that takes a request on [`claim`]s it first, by renaming
//! it to `<nonce>.taken.json`: with several windows open, one does it.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::inspect::hex;

/// How long a request waits to be picked up before it is dropped.
pub const STALE_AFTER: Duration = Duration::from_secs(60 * 60);

/// What is asked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Ask {
    /// Run the command `command` of the build of `plugin` whose file's
    /// SHA-256 is `sha256`, on the project open at `root`.
    Try {
        plugin: String,
        sha256: String,
        command: String,
        root: String,
        #[serde(default)]
        file: Option<String>,
        #[serde(default)]
        line: Option<u32>,
        #[serde(default)]
        input: Option<String>,
    },
    /// Ask the user to install that build.
    Install { plugin: String, sha256: String },
}

/// A request, as filed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub ask: Ask,
    /// The agent that asked, by its name, for the user to see.
    pub agent: String,
    /// When, in seconds since 1970.
    pub filed: u64,
}

/// What the window answered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "answer", rename_all = "lowercase")]
pub enum Answer {
    /// The command's result, as the agent reads it.
    Tried { result: String },
    /// The user installed it.
    Installed { fingerprint: String },
    /// The user said no.
    Declined,
    /// It could not be done, and why.
    Failed { why: String },
}

/// `<data>/plugins/requests`.
pub fn requests_dir(data: &Path) -> PathBuf {
    crate::store::plugins_dir(data).join("requests")
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A name no other request has: a digest of the time, the process and a
/// counter.
fn nonce() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut h = Sha256::new();
    h.update(nanos.to_le_bytes());
    h.update(std::process::id().to_le_bytes());
    h.update(COUNTER.fetch_add(1, Ordering::Relaxed).to_le_bytes());
    hex(&h.finalize()[..12])
}

/// Whether `name` is a nonce [`nonce`] could have made: so a file name read
/// back is never a path.
fn is_nonce(name: &str) -> bool {
    name.len() == 24 && name.bytes().all(|b| b.is_ascii_hexdigit())
}

/// File `ask`, from `agent`; its nonce, to wait for its answer with.
pub fn file(data: &Path, ask: Ask, agent: &str) -> Result<String, String> {
    let dir = requests_dir(data);
    clew_core::derived::ensure_private_dir(&crate::store::plugins_dir(data))
        .map_err(|e| e.to_string())?;
    clew_core::derived::ensure_private_dir(&dir).map_err(|e| e.to_string())?;
    let nonce = nonce();
    let request = Request {
        ask,
        agent: agent.chars().take(80).collect(),
        filed: now(),
    };
    let json = serde_json::to_vec(&request).map_err(|e| e.to_string())?;
    clew_core::statefile::write_atomic(&dir.join(format!("{nonce}.request.json")), &json)
        .map_err(|e| e.to_string())?;
    Ok(nonce)
}

/// The requests waiting, oldest first. Ones older than [`STALE_AFTER`], and
/// files that are not requests, are removed; so are requests taken and
/// never answered, once stale.
pub fn pending(data: &Path) -> Vec<(String, Request)> {
    let dir = requests_dir(data);
    let mut found = Vec::new();
    for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(nonce) = name.strip_suffix(".taken.json") {
            let stale = !is_nonce(nonce)
                || entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .is_none_or(|age| age >= STALE_AFTER);
            if stale {
                let _ = std::fs::remove_file(entry.path());
            }
            continue;
        }
        let Some(nonce) = name.strip_suffix(".request.json") else {
            continue;
        };
        let request = is_nonce(nonce)
            .then(|| {
                clew_core::statefile::read_capped_checked(&entry.path(), 64 * 1024)
                    .ok()
                    .flatten()
            })
            .flatten()
            .and_then(|text| serde_json::from_str::<Request>(&text).ok());
        match request {
            Some(r) if now().saturating_sub(r.filed) < STALE_AFTER.as_secs() => {
                found.push((nonce.to_string(), r));
            }
            _ => {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    found.sort_by_key(|(_, r)| r.filed);
    found
}

/// Take request `nonce` on, if no one has: whether this caller has it now.
/// The rename is atomic, so of several windows one wins.
pub fn claim(data: &Path, nonce: &str) -> bool {
    if !is_nonce(nonce) {
        return false;
    }
    let dir = requests_dir(data);
    std::fs::rename(
        dir.join(format!("{nonce}.request.json")),
        dir.join(format!("{nonce}.taken.json")),
    )
    .is_ok()
}

/// Answer the request `nonce`, and take it off the list.
pub fn answer(data: &Path, nonce: &str, answer: &Answer) -> Result<(), String> {
    if !is_nonce(nonce) {
        return Err("not a request".into());
    }
    let dir = requests_dir(data);
    let json = serde_json::to_vec(answer).map_err(|e| e.to_string())?;
    clew_core::statefile::write_atomic(&dir.join(format!("{nonce}.answer.json")), &json)
        .map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(dir.join(format!("{nonce}.request.json")));
    let _ = std::fs::remove_file(dir.join(format!("{nonce}.taken.json")));
    Ok(())
}

/// Wait for the answer to `nonce`, until `timeout` passes or `cancel` is
/// set; `None` then, and the request is withdrawn.
pub fn wait(data: &Path, nonce: &str, timeout: Duration, cancel: &AtomicBool) -> Option<Answer> {
    let dir = requests_dir(data);
    let answer_path = dir.join(format!("{nonce}.answer.json"));
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Ok(Some(text)) = clew_core::statefile::read_capped_checked(&answer_path, 1024 * 1024)
        {
            let _ = std::fs::remove_file(&answer_path);
            return serde_json::from_str(&text).ok();
        }
        if cancel.load(Ordering::Relaxed) || std::time::Instant::now() >= deadline {
            let _ = std::fs::remove_file(dir.join(format!("{nonce}.request.json")));
            let _ = std::fs::remove_file(dir.join(format!("{nonce}.taken.json")));
            // An answer written as the wait gave up is not left behind.
            let _ = std::fs::remove_file(&answer_path);
            return None;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "clew-plugin-requests-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn install() -> Ask {
        Ask::Install {
            plugin: "sample".into(),
            sha256: "ab".repeat(32),
        }
    }

    #[test]
    fn a_request_is_filed_picked_up_and_answered() {
        let data = temp("round");
        let nonce = file(&data, install(), "Claude Code").unwrap();
        let waiting = pending(&data);
        assert_eq!(waiting.len(), 1);
        assert_eq!(waiting[0].0, nonce);
        assert_eq!(waiting[0].1.agent, "Claude Code");
        let window = {
            let data = data.clone();
            let nonce = nonce.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(300));
                answer(
                    &data,
                    &nonce,
                    &Answer::Installed {
                        fingerprint: "f".into(),
                    },
                )
                .unwrap();
            })
        };
        let got = wait(
            &data,
            &nonce,
            Duration::from_secs(10),
            &AtomicBool::new(false),
        );
        window.join().unwrap();
        assert_eq!(
            got,
            Some(Answer::Installed {
                fingerprint: "f".into()
            })
        );
        assert!(pending(&data).is_empty());
        assert_eq!(std::fs::read_dir(requests_dir(&data)).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn one_window_claims_a_request() {
        let data = temp("claim");
        let nonce = file(&data, install(), "agent").unwrap();
        assert!(claim(&data, &nonce));
        assert!(!claim(&data, &nonce), "a second window gets nothing");
        assert!(
            pending(&data).is_empty(),
            "a taken request is not offered again"
        );
        assert!(!claim(&data, "../../x"));
        answer(&data, &nonce, &Answer::Declined).unwrap();
        assert_eq!(
            wait(
                &data,
                &nonce,
                Duration::from_secs(1),
                &AtomicBool::new(false)
            ),
            Some(Answer::Declined)
        );
        assert_eq!(std::fs::read_dir(requests_dir(&data)).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn an_unanswered_request_is_withdrawn() {
        let data = temp("timeout");
        let nonce = file(&data, install(), "agent").unwrap();
        assert_eq!(
            wait(
                &data,
                &nonce,
                Duration::from_millis(300),
                &AtomicBool::new(false)
            ),
            None
        );
        assert!(pending(&data).is_empty());
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn stale_and_foreign_files_are_cleared() {
        let data = temp("stale");
        let dir = requests_dir(&data);
        file(&data, install(), "agent").unwrap();
        let old = Request {
            ask: install(),
            agent: "old".into(),
            filed: 1,
        };
        std::fs::write(
            dir.join(format!("{}.request.json", "0".repeat(24))),
            serde_json::to_vec(&old).unwrap(),
        )
        .unwrap();
        std::fs::write(dir.join("../../evil.request.json"), b"{}").ok();
        std::fs::write(dir.join("notanonce.request.json"), b"{}").unwrap();
        let waiting = pending(&data);
        assert_eq!(waiting.len(), 1);
        assert_eq!(waiting[0].1.agent, "agent");
        assert!(!dir.join("notanonce.request.json").exists());
        assert!(answer(&data, "../x", &Answer::Declined).is_err());
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn nonces_differ() {
        let a = nonce();
        let b = nonce();
        assert_ne!(a, b);
        assert!(is_nonce(&a));
    }
}
