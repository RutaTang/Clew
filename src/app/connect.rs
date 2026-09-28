//! The Connect modal and transport switching: the new-connection form, saved
//! hosts, switching the transport (`connect_to`), the remote folder browser,
//! trusting an unknown host key, and the per-host AI-key grant.
//!
//! Its messages, [`ConnectMsg`], arrive through `App::update_connect`.

use crate::app::prelude::*;
use crate::*;

/// The transport the Connect form's fields describe, when they describe one
/// completely (host and user set, a valid port) — to tell whether the form is
/// showing the LIVE host or a saved one.
pub(crate) fn connect_form_target(ui: &ConnectUi) -> Option<connect::ConnTarget> {
    let (host, user) = (ui.host.trim(), ui.user.trim());
    if host.is_empty() || user.is_empty() {
        return None;
    }
    let conn = connect::SavedConnection {
        name: ui.name.trim().to_string(),
        host: host.to_string(),
        user: user.to_string(),
        port: connect::parse_port(&ui.port).ok()?,
        identity: ui.identity.trim().to_string(),
        send_ai_keys: ui.send_ai_keys,
    };
    conn.validate().ok()?;
    Some(conn.target())
}

impl App {
    pub(crate) fn on_connect_field(&mut self, field: ConnectField, value: String) -> Task<Message> {
        let names_host = matches!(
            field,
            ConnectField::Host | ConnectField::User | ConnectField::Port | ConnectField::Identity
        );
        if let Some(ui) = &mut self.connect {
            match field {
                ConnectField::Name => ui.name = value,
                ConnectField::Host => ui.host = value,
                ConnectField::User => ui.user = value,
                // Keep only digits so the port stays parseable.
                ConnectField::Port => {
                    ui.port = value.chars().filter(char::is_ascii_digit).collect()
                }
                ConnectField::Identity => ui.identity = value,
            }
        }
        if names_host {
            self.sync_form_ai_opt_in();
        }
        Task::none()
    }

    /// Make the form's "send my AI keys" box show the grant of the host the
    /// form now names: the live connection's actual grant when it names the
    /// host this window is connected to, else the saved host's recorded
    /// opt-in. A form naming a host clew does not know keeps the user's tick.
    pub(crate) fn sync_form_ai_opt_in(&mut self) {
        let Some(target) = self
            .connect
            .as_ref()
            .and_then(crate::app::connect::connect_form_target)
        else {
            return;
        };
        let shown = if let Some(live) = self.live_ai_opt_in()
            && target == self.connection
        {
            Some(live)
        } else {
            self.saved_connections
                .iter()
                .find(|c| c.target() == target)
                .map(|c| c.send_ai_keys)
        };
        if let (Some(ui), Some(shown)) = (&mut self.connect, shown) {
            ui.send_ai_keys = shown;
        }
    }

    pub(crate) fn on_connect_submit(&mut self) -> Task<Message> {
        let Some(ui) = &self.connect else {
            return Task::none();
        };
        let host = ui.host.trim().to_string();
        let user = ui.user.trim().to_string();
        if host.is_empty() || user.is_empty() {
            if let Some(ui) = &mut self.connect {
                ui.stage = ConnectStage::Error("Host and user are required.".into());
            }
            return Task::none();
        }
        // A port that is not one is an error to show, not a silent 22.
        let port = connect::parse_port(&ui.port);
        let conn = connect::SavedConnection {
            name: ui.name.trim().to_string(),
            host,
            user,
            port: port.clone().unwrap_or(22),
            identity: ui.identity.trim().to_string(),
            send_ai_keys: ui.send_ai_keys,
        };
        if let Err(e) = port.and_then(|_| conn.validate()) {
            if let Some(ui) = &mut self.connect {
                ui.stage = ConnectStage::Error(e);
            }
            return Task::none();
        }
        self.remember_connection(conn.clone());
        let opt_in = conn.send_ai_keys;
        let stop_old = self.connect_to(conn.target());
        // After connect_to: it resets the opt-in for every transport switch.
        self.remote_ai_opt_in = opt_in;
        stop_old
    }

    /// Add (or update) a saved connection, de-duplicated by `user@host:port`, and
    /// persist the list. Most-recent first, so it heads the Connect modal's list.
    ///
    /// The merge happens against the file, not against this window's copy: every
    /// window loads `saved_connections` once at startup, so writing this window's
    /// Vec wholesale deleted whatever another window had saved meanwhile. The
    /// merged list is adopted so the modal shows what is actually on disk.
    pub(crate) fn remember_connection(&mut self, conn: connect::SavedConnection) {
        match connect::upsert(conn) {
            Ok(merged) => self.saved_connections = merged,
            Err(e) => self.status = format!("Cannot save connections: {e}"),
        }
    }

    /// Switch the server transport to `target`. Drops the current project (it
    /// lives on the old host) and the stale request channel; restarting the
    /// subscription brings up the new transport, which hands back a fresh channel
    /// via `ServerMsg::Connected`. The Connect modal, if open, moves to "connecting".
    pub(crate) fn connect_to(&mut self, target: connect::ConnTarget) -> Task<Message> {
        let label = target.label();
        // Stop the old project's work FIRST: `drop_connection_state` drops the
        // project's link (its stream ids) and the server link (the channel) —
        // after either, a Cancel can no longer be addressed.
        let stop_old = self.drop_project_work();
        // Drop everything tied to the current transport: both links, whole.
        self.drop_connection_state();
        // Everything anchored to the project on the old host goes with it —
        // the same reset opening another project performs
        // (`forget_project_state`): its trail, bookmarks, notes and tours, its
        // caches and indexes, and every pane and popup naming its paths. Only
        // the project and the panes used to be dropped here, so the old
        // project's explanations, symbol index and import graph stayed on
        // screen, and fed the next question, until the new host's scan
        // replaced them. What it reports (a discarded draft, unsaved remote
        // state) is kept in the status line below.
        let before = self.status.clone();
        self.forget_project_state();
        let discarded = (self.status != before).then(|| self.status.clone());
        self.pending_scan_root = None;
        self.scanning = false;
        // A folder awaiting consent, or a file waiting to open once a scan
        // lands, names a path on the host being left. Kept, Allow would trust
        // — and open — the same path on the host being connected to, which is
        // another machine's project.
        self.pending_consent = None;
        self.pending_open = None;
        // A new transport instance: late messages still queued from the old
        // one (its key differs, but its channel already held them) carry the
        // old number and are dropped by the one check in `dispatch`. A user
        // switch connects immediately (no respawn backoff).
        self.conn_gen += 1;
        self.conn_respawn = false;
        self.project_epoch += 1;
        // Every transport switch starts without the AI-key opt-in; the
        // Connect flow re-grants it per host, explicitly.
        self.remote_ai_opt_in = false;
        self.connection = target;
        self.status = match discarded {
            Some(note) => format!("Connecting to {label}… — {note}"),
            None => format!("Connecting to {label}…"),
        };
        if let Some(ui) = &mut self.connect {
            ui.stage = ConnectStage::Connecting { label };
        }
        stop_old
    }

    /// Show the remote folder picker for `path` (home when `None`) and request its
    /// listing. The reply (`DirListing`) fills it in via `handle_server_event`.
    pub(crate) fn enter_remote_browser(&mut self, path: Option<String>) {
        // Keep the current directory shown (dimmed) while the next one loads;
        // start empty when there was no browser yet.
        let (cwd, parent, entries, omitted) = match self.connect.as_mut().map(|u| &mut u.stage) {
            Some(ConnectStage::Browsing(b)) => (
                b.cwd.clone(),
                b.parent.clone(),
                std::mem::take(&mut b.entries),
                b.omitted,
            ),
            _ => (String::new(), None, Vec::new(), 0),
        };
        if let Some(ui) = &mut self.connect {
            ui.stage = ConnectStage::Browsing(RemoteBrowser {
                cwd,
                parent,
                entries,
                omitted,
                loading: true,
            });
        }
        self.request_list_dir(path);
    }

    /// Send a `ListDir` for the remote folder picker (`None` = the login home).
    pub(crate) fn request_list_dir(&mut self, path: Option<String>) {
        // Cleared FIRST: on either failure path below, leaving a previous
        // listing's id in place would let that older reply paint a directory
        // the user has already navigated past.
        self.server.pending_list_dir = None;
        if let Some(id) = self.send_to_server(clew_protocol::Request::ListDir { path }) {
            self.server.pending_list_dir = Some(id);
        }
    }

    pub(crate) fn on_remote_open_here(&mut self) -> Task<Message> {
        let cwd = match self.connect.as_ref().map(|u| &u.stage) {
            Some(ConnectStage::Browsing(b)) => Some(b.cwd.clone()),
            _ => None,
        };
        if let Some(cwd) = cwd {
            self.connect = None;
            // Through the workspace-trust gate, like every local open: the
            // approval is host-scoped (`request_open` keys it on this host),
            // and a remote repository's `.clew/lsp.toml` and `launch.json`
            // are acted on exactly as a local one's are. Opening it straight
            // away skipped the one question those guards rely on having been
            // asked.
            return self.request_open(PathBuf::from(cwd));
        }
        Task::none()
    }

    /// The host refused as unknown can be trusted from here: after the same
    /// cleanup as any failed connect, the Connect modal (opened if it was
    /// closed — a reconnect, a `CLEW_SSH` start) shows the key's fingerprint
    /// with Trust / Cancel. Nothing is trusted until the reader clicks.
    pub(crate) fn on_host_key_unknown(
        &mut self,
        reason: String,
        key: connect::ScannedHostKey,
    ) -> Task<Message> {
        let task = self.on_server_unavailable(reason.clone());
        if !self.connection.is_remote() {
            return task;
        }
        let target = self.connection.clone();
        let ui = self.connect.get_or_insert_with(ConnectUi::default);
        ui.stage = ConnectStage::TrustHost {
            target,
            reason,
            key,
        };
        self.code_focused = false;
        task
    }

    /// The host presented a changed key, and the key on record is one
    /// trusted in clew: after the same cleanup as any failed connect, the
    /// Connect modal (opened if it was closed) shows the refusal with the
    /// offer to forget that key. Nothing is forgotten until the reader clicks.
    pub(crate) fn on_host_key_changed(
        &mut self,
        reason: String,
        forget_host: String,
    ) -> Task<Message> {
        let task = self.on_server_unavailable(reason.clone());
        if !self.connection.is_remote() {
            return task;
        }
        let target = self.connection.clone();
        let ui = self.connect.get_or_insert_with(ConnectUi::default);
        ui.stage = ConnectStage::HostKeyChanged {
            target,
            reason,
            forget_host,
        };
        self.code_focused = false;
        task
    }

    /// Forget the changed key clew recorded under `host` and connect again.
    /// `host` is what the clicked prompt named: a click that raced a newer
    /// prompt (another host) forgets nothing. Only clew's own known_hosts is
    /// edited (`connect::forget_host_key`), and nothing is trusted here — the
    /// reconnect meets the host's new key as unknown, and the Connect modal
    /// shows its fingerprint to be checked before it is trusted.
    pub(crate) fn on_connect_forget_host_key(&mut self, host: &str) -> Task<Message> {
        let Some(ConnectUi {
            stage:
                ConnectStage::HostKeyChanged {
                    target,
                    forget_host,
                    ..
                },
            ..
        }) = &self.connect
        else {
            return Task::none();
        };
        if forget_host != host || *target != self.connection {
            return Task::none();
        }
        let target = target.clone();
        match connect::forget_host_key(host) {
            Ok(removed) => {
                // The same host again: its AI-key grant stands.
                let opt_in = self.remote_ai_opt_in;
                let task = self.connect_to(target);
                self.remote_ai_opt_in = opt_in;
                self.status = format!(
                    "Forgot {removed} key{} clew had recorded for {host} — connecting; check \
                     the new key's fingerprint before trusting it",
                    if removed == 1 { "" } else { "s" }
                );
                task
            }
            Err(e) => {
                if let Some(ui) = &mut self.connect {
                    ui.stage = ConnectStage::Error(format!("The old key was not forgotten: {e}"));
                }
                Task::none()
            }
        }
    }

    /// Trust the host key on offer and connect again. `fingerprint` is what
    /// the clicked prompt showed: a click that raced a newer prompt (another
    /// host, another key) records nothing. The key goes into clew's own
    /// known_hosts (`connect::trust_host_key`), never `~/.ssh/known_hosts`,
    /// and only for the host the refused connection looked up.
    pub(crate) fn on_connect_trust_host(&mut self, fingerprint: &str) -> Task<Message> {
        let Some(ConnectUi {
            stage: ConnectStage::TrustHost { target, key, .. },
            ..
        }) = &self.connect
        else {
            return Task::none();
        };
        if key.fingerprint != fingerprint || *target != self.connection {
            return Task::none();
        }
        let (target, key) = (target.clone(), key.clone());
        let host_field = match &target {
            connect::ConnTarget::Ssh { args, .. } => connect::ssh_host_port(args)
                .map(|(host, port)| connect::known_hosts_pattern(&host, port)),
            connect::ConnTarget::Local => None,
        };
        let recorded = match host_field {
            Some(host_field) => connect::trust_host_key(&key, &host_field),
            None => Err("this connection names no single host to trust".into()),
        };
        match recorded {
            Ok(path) => {
                // The same host again: its AI-key grant stands.
                let opt_in = self.remote_ai_opt_in;
                let task = self.connect_to(target);
                self.remote_ai_opt_in = opt_in;
                self.status = format!(
                    "Trusted {} key {} (recorded in {}) — connecting…",
                    key.kind,
                    key.fingerprint,
                    path.display()
                );
                task
            }
            Err(e) => {
                if let Some(ui) = &mut self.connect {
                    ui.stage = ConnectStage::Error(format!("The host key was not trusted: {e}"));
                }
                Task::none()
            }
        }
    }

    /// Grant or revoke this host's permission to hold the AI keys, and make it
    /// take effect on the live connection.
    ///
    /// Revoking has to be an ACTIVE step: the server already holds the keys,
    /// so merely deciding to stop sending new ones leaves it able to keep
    /// using the old ones for the rest of the session.
    pub(crate) fn set_remote_ai_opt_in(&mut self, on: bool) {
        if self.remote_ai_opt_in == on {
            return;
        }
        self.remote_ai_opt_in = on;
        self.send_ai_config();
    }

    /// Whether the host this window is connected to holds the AI keys —
    /// `None` when the connection is local (there is nothing to grant). For
    /// the Connect modal to show the LIVE host's grant (and offer
    /// [`ConnectMsg::RevokeAiKeys`]) separately from the form's checkbox,
    /// which describes the host being configured.
    pub(crate) fn live_ai_opt_in(&self) -> Option<bool> {
        self.connection.is_remote().then_some(self.remote_ai_opt_in)
    }

    /// Withdraw the AI keys from the host this window is connected to: the
    /// server is told to forget them now (`SetAiConfig` with nothing in it),
    /// and the saved host's opt-in is cleared so the next connect to it does
    /// not hand them back. The only path that changes the LIVE grant — the
    /// form's checkbox never does (see `ConnectMsg::ToggleAiKeys`).
    pub(crate) fn on_connect_revoke_ai_keys(&mut self) -> Task<Message> {
        if !self.connection.is_remote() {
            return Task::none();
        }
        self.set_remote_ai_opt_in(false);
        let live = self.connection.clone();
        if let Some(saved) = self
            .saved_connections
            .iter()
            .find(|c| c.target() == live && c.send_ai_keys)
            .cloned()
        {
            self.remember_connection(connect::SavedConnection {
                send_ai_keys: false,
                ..saved
            });
        }
        // A form open on this very host must not offer to re-grant silently.
        if let Some(ui) = &mut self.connect
            && connect_form_target(ui).is_some_and(|t| t == live)
        {
            ui.send_ai_keys = false;
        }
        self.status = "This host no longer holds your AI keys".into();
        Task::none()
    }
}

impl App {
    /// Handle a [`ConnectMsg`]: this feature's share of what `dispatch` routes
    /// (after its one ownership check and the menu bookkeeping).
    pub(crate) fn update_connect(&mut self, message: ConnectMsg) -> Task<Message> {
        match message {
            ConnectMsg::TrustHost { fingerprint } => self.on_connect_trust_host(&fingerprint),
            ConnectMsg::TrustCancel => {
                if let Some(ui) = &mut self.connect
                    && let ConnectStage::TrustHost { reason, .. }
                    | ConnectStage::HostKeyChanged { reason, .. } = &ui.stage
                {
                    ui.stage = ConnectStage::Error(reason.clone());
                }
                Task::none()
            }
            ConnectMsg::ForgetHostKey { host } => self.on_connect_forget_host_key(&host),
            ConnectMsg::Open => {
                self.connect = Some(ConnectUi::default());
                // The form's text inputs take the keyboard from here on.
                self.code_focused = false;
                // Already on a live remote? Skip the form and browse its folders.
                if self.connection.is_remote() && self.server.is_up() {
                    self.enter_remote_browser(None);
                }
                Task::none()
            }
            ConnectMsg::Close => {
                self.connect = None;
                Task::none()
            }
            ConnectMsg::Field(field, value) => self.on_connect_field(field, value),
            ConnectMsg::PickIdentity => {
                pick_file(self.main_window).map(|v| Message::Connect(ConnectMsg::IdentityPicked(v)))
            }
            ConnectMsg::IdentityPicked(path) => {
                if let (Some(ui), Some(path)) = (&mut self.connect, path) {
                    ui.identity = path.to_string_lossy().into_owned();
                }
                Task::none()
            }
            ConnectMsg::ToggleAiKeys(on) => {
                // The checkbox belongs to the FORM — the host being configured,
                // applied on Connect. It used to also flip the LIVE connection's
                // grant, so ticking it while filling in host B (the form shows
                // during a reconnect of host A) sent A the keys. Revoking from
                // the live host is its own action (`ConnectMsg::RevokeAiKeys`).
                if let Some(ui) = &mut self.connect {
                    ui.send_ai_keys = on;
                }
                Task::none()
            }
            ConnectMsg::RevokeAiKeys => self.on_connect_revoke_ai_keys(),
            ConnectMsg::Submit => self.on_connect_submit(),
            ConnectMsg::ToSaved { user_host, port } => match self
                .saved_connections
                .iter()
                .find(|c| c.user_host() == user_host && c.port == port)
                .cloned()
            {
                Some(conn) => {
                    let stop_old = self.connect_to(conn.target());
                    // After connect_to (which resets it): apply this host's
                    // saved AI-key opt-in.
                    self.remote_ai_opt_in = conn.send_ai_keys;
                    stop_old
                }
                None => Task::none(),
            },
            ConnectMsg::RemoveSaved { user_host, port } => {
                // Delete by identity against the file, not by index against this
                // window's copy: another window may have added or removed rows
                // since, and writing our stale Vec back resurrected everything
                // it had deleted.
                match connect::remove(&user_host, port) {
                    Ok(merged) => self.saved_connections = merged,
                    Err(e) => self.status = format!("Cannot save connections: {e}"),
                }
                Task::none()
            }
            ConnectMsg::Disconnect => {
                self.connect = None;
                if self.connection.is_remote() {
                    self.connect_to(connect::ConnTarget::Local)
                } else {
                    Task::none()
                }
            }
            ConnectMsg::BrowseTo(path) => {
                self.enter_remote_browser(Some(path));
                Task::none()
            }
            ConnectMsg::OpenHere => self.on_remote_open_here(),
        }
    }
}
