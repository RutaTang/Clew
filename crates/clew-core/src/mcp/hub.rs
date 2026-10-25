//! The MCP connections of the whole process: one per configured server,
//! shared by every window, opened when first needed and kept while their
//! configuration stays the same.
//!
//! A server that failed to connect is not tried again for a short while
//! ([`RETRY_AFTER`]), so a broken entry does not make every question wait on
//! the same failure; [`Hub::forget`] (the settings' Reconnect, a changed
//! entry) clears that at once.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use super::{Client, ServerConfig, ServerInfo, ToolInfo};

/// How long a failed connect is remembered before the server is tried again.
pub const RETRY_AFTER: Duration = Duration::from_secs(15);

/// How one server stands, for the settings list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Not connected (yet): nothing has asked for it.
    Idle,
    Connecting,
    /// Connected, with the tools it offered when last listed.
    Connected {
        info: ServerInfo,
        tools: usize,
    },
    Failed(String),
}

/// One server's slot.
struct Slot {
    config: ServerConfig,
    /// Held while connecting, so callers asking at once wait for the one
    /// connect rather than starting their own.
    client: tokio::sync::Mutex<Option<Arc<Client>>>,
    state: Mutex<Status>,
    failed_at: Mutex<Option<Instant>>,
}

/// What [`Hub::gather`] found of one server.
#[derive(Debug, Clone)]
pub struct Gathered {
    pub config: ServerConfig,
    pub outcome: Result<(Arc<Client>, Arc<Vec<ToolInfo>>), String>,
}

/// The process's MCP connections.
#[derive(Default)]
pub struct Hub {
    slots: Mutex<HashMap<String, Arc<Slot>>>,
}

impl Hub {
    /// The one hub of the process.
    pub fn global() -> &'static Hub {
        static HUB: OnceLock<Hub> = OnceLock::new();
        HUB.get_or_init(Hub::default)
    }

    /// The slot for `config`, replacing one whose configuration changed (its
    /// connection is closed in the background).
    fn slot(&self, config: &ServerConfig) -> Arc<Slot> {
        let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(slot) = slots.get(&config.name)
            && same_connection(&slot.config, config)
        {
            return slot.clone();
        }
        let slot = Arc::new(Slot {
            config: config.clone(),
            client: tokio::sync::Mutex::new(None),
            state: Mutex::new(Status::Idle),
            failed_at: Mutex::new(None),
        });
        if let Some(old) = slots.insert(config.name.clone(), slot.clone()) {
            close_in_background(old);
        }
        slot
    }

    /// A connected client of `config`'s server: the one already open, or a
    /// new connection. Must run inside a tokio runtime.
    pub async fn client(&self, config: &ServerConfig) -> Result<Arc<Client>, String> {
        let slot = self.slot(config);
        let mut client = slot.client.lock().await;
        if let Some(c) = client.as_ref()
            && c.alive()
        {
            return Ok(c.clone());
        }
        if let Some(at) = *slot
            .failed_at
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            && at.elapsed() < RETRY_AFTER
            && let Status::Failed(why) = slot.status()
        {
            return Err(why);
        }
        slot.set(Status::Connecting);
        match Client::connect(config).await {
            Ok(c) => {
                let c = Arc::new(c);
                *client = Some(c.clone());
                slot.set(Status::Connected {
                    info: c.info().clone(),
                    tools: 0,
                });
                *slot
                    .failed_at
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = None;
                Ok(c)
            }
            Err(e) => {
                let why = e.to_string();
                *client = None;
                slot.set(Status::Failed(why.clone()));
                *slot
                    .failed_at
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = Some(Instant::now());
                Err(why)
            }
        }
    }

    /// Connect every enabled server of `configs` (at once) and list its
    /// tools, each within `timeout`.
    pub async fn gather(&self, configs: &[ServerConfig], timeout: Duration) -> Vec<Gathered> {
        let tasks = configs
            .iter()
            .filter(|c| c.enabled)
            .map(|config| async move {
                let outcome = tokio::time::timeout(timeout, async {
                    let client = self.client(config).await?;
                    let tools = client.tools().await.map_err(|e| e.to_string())?;
                    let slot = self.slot(config);
                    slot.set(Status::Connected {
                        info: client.info().clone(),
                        tools: tools.len(),
                    });
                    Ok((client, tools))
                })
                .await
                .unwrap_or_else(|_| Err("it did not answer in time".to_string()));
                Gathered {
                    config: config.clone(),
                    outcome,
                }
            });
        futures_join_all(tasks).await
    }

    /// How server `name` stands.
    pub fn status(&self, name: &str) -> Status {
        self.slots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(name)
            .map(|s| s.status())
            .unwrap_or(Status::Idle)
    }

    /// Drop server `name`'s connection and any remembered failure; the next
    /// ask connects afresh.
    pub fn forget(&self, name: &str) {
        let old = self
            .slots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(name);
        if let Some(old) = old {
            close_in_background(old);
        }
    }

    /// Drop the connections of servers no longer configured.
    pub fn retain(&self, names: &[String]) {
        let gone: Vec<Arc<Slot>> = {
            let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
            let stale: Vec<String> = slots
                .keys()
                .filter(|k| !names.contains(k))
                .cloned()
                .collect();
            stale.iter().filter_map(|k| slots.remove(k)).collect()
        };
        for slot in gone {
            close_in_background(slot);
        }
    }
}

impl Slot {
    fn status(&self) -> Status {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn set(&self, status: Status) {
        *self.state.lock().unwrap_or_else(PoisonError::into_inner) = status;
    }
}

/// Whether two configurations of a server can share a connection: the same
/// transport. The approval policy and time box are read per call.
fn same_connection(a: &ServerConfig, b: &ServerConfig) -> bool {
    a.transport == b.transport
}

/// Close a slot's connection without waiting for it, when a runtime is at
/// hand (else dropping it ends the process anyway).
fn close_in_background(slot: Arc<Slot>) {
    if let Ok(rt) = tokio::runtime::Handle::try_current() {
        rt.spawn(async move {
            if let Some(client) = slot.client.lock().await.take() {
                client.close().await;
            }
        });
    }
}

/// Await every future, in order of `futures`, concurrently.
async fn futures_join_all<F: Future>(futures: impl Iterator<Item = F>) -> Vec<F::Output> {
    let futures: Vec<_> = futures.map(Box::pin).collect();
    let mut outputs: Vec<Option<F::Output>> = futures.iter().map(|_| None).collect();
    let mut futures: Vec<Option<std::pin::Pin<Box<F>>>> = futures.into_iter().map(Some).collect();
    std::future::poll_fn(|cx| {
        let mut pending = false;
        for (i, slot) in futures.iter_mut().enumerate() {
            if let Some(fut) = slot {
                match fut.as_mut().poll(cx) {
                    std::task::Poll::Ready(out) => {
                        outputs[i] = Some(out);
                        *slot = None;
                    }
                    std::task::Poll::Pending => pending = true,
                }
            }
        }
        if pending {
            std::task::Poll::Pending
        } else {
            std::task::Poll::Ready(())
        }
    })
    .await;
    outputs
        .into_iter()
        .map(|o| o.expect("every future finished"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linerpc::Launch;
    use crate::mcp::{Approve, Transport};

    fn missing(name: &str) -> ServerConfig {
        ServerConfig {
            name: name.into(),
            transport: Transport::Stdio(Launch {
                program: "/definitely/not/here/mcp-server".into(),
                ..Launch::default()
            }),
            enabled: true,
            approve: Approve::Ask,
            allow: Vec::new(),
            timeout: Duration::from_secs(5),
        }
    }

    #[tokio::test]
    async fn a_failed_server_is_reported_and_not_retried_at_once() {
        let hub = Hub::default();
        let config = missing("gone");
        let mut off = missing("off");
        off.enabled = false;
        let found = hub
            .gather(&[config.clone(), off], Duration::from_secs(5))
            .await;
        assert_eq!(found.len(), 1, "a disabled server is not asked");
        let why = found[0].outcome.as_ref().unwrap_err();
        assert!(why.contains("could not start"), "{why}");
        assert!(matches!(hub.status("gone"), Status::Failed(_)));
        // Remembered: the second ask fails without another attempt.
        assert_eq!(hub.client(&config).await.unwrap_err(), *why);
        hub.forget("gone");
        assert_eq!(hub.status("gone"), Status::Idle);
        assert_eq!(hub.status("never"), Status::Idle);
    }

    #[tokio::test]
    async fn the_join_keeps_the_order_of_its_futures() {
        let outs = futures_join_all([30u64, 0, 10].into_iter().map(|ms| async move {
            tokio::time::sleep(Duration::from_millis(ms)).await;
            ms
        }))
        .await;
        assert_eq!(outs, [30, 0, 10]);
    }
}
