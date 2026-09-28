//! Preferences: the LLM settings modal (open, save), the appearance (theme)
//! preference and its live preview, and the cached chat-model config every AI
//! flow reads through one gate.
//!
//! Its messages, [`SettingsMsg`], arrive through `App::update_settings`.

use crate::app::prelude::*;
use crate::*;

impl App {
    /// The chat-model config, read once and then served from
    /// `llm_config_cache` (see there for when it is re-read).
    pub(crate) fn llm_config(&mut self) -> Option<llm::Config> {
        self.llm_config_cache
            .get_or_insert_with(llm::Config::load)
            .clone()
    }

    /// Drop the cached chat-model config so the next use re-reads it, and
    /// refresh the `llm_available` gate the views read.
    pub(crate) fn invalidate_llm_config(&mut self) {
        self.llm_config_cache = None;
        self.llm_available = llm::Config::available();
    }

    /// The one gate every flow that needs a chat model goes through: the
    /// config, or — with no key configured — the single wording every flow
    /// reports and, as the task to return, the Settings modal opened where the
    /// key is entered. Background callers that must not pop a modal drop the
    /// task.
    pub(crate) fn require_llm(&mut self) -> Result<llm::Config, Task<Message>> {
        match self.llm_config() {
            Some(cfg) => Ok(cfg),
            None => {
                self.status = format!("Add an API key in Settings ({})", llm::config_hint());
                Err(Task::done(Message::Settings(SettingsMsg::Open)))
            }
        }
    }

    pub(crate) fn on_open_settings(&mut self) -> Task<Message> {
        let c = llm::Config::current_or_default();
        self.settings.provider = c.provider;
        self.settings.model = c.model;
        self.settings.base_url = c.base_url;
        // Pre-fill the key from the FILE, never from `c.api_key`, which may
        // have been resolved from the environment. Saving the form stores
        // whatever is in the field, and a stored key follows `base_url`
        // anywhere — so a pre-filled form let a user with `ANTHROPIC_API_KEY`
        // exported type a gateway URL and write their real provider secret
        // into config.toml next to it, reaching the exact outcome the endpoint
        // check in `env_key_for_endpoint` refuses.
        let stored = llm::Config::stored_key();
        self.settings.key_from_env = stored.is_empty() && !c.api_key.is_empty();
        self.settings.key = stored;
        let e = embed::Config::current_or_default();
        self.settings.embed_model = e.model;
        self.settings.embed_base_url = e.base_url;
        let embed_stored = embed::Config::stored_key();
        self.settings.embed_key_from_env = embed_stored.is_empty() && !e.api_key.is_empty();
        self.settings.embed_key = embed_stored;
        // What the form was pre-filled WITH, so Save can tell a field the user
        // edited from one they never touched. Built the same way Save builds
        // the config it writes, so an untouched form compares equal field by
        // field.
        self.settings.ai_snapshot = (
            llm::Config::from_parts(
                self.settings.provider,
                self.settings.key.clone(),
                self.settings.model.clone(),
                self.settings.base_url.clone(),
            ),
            embed::Config::from_parts(
                self.settings.embed_key.clone(),
                self.settings.embed_model.clone(),
                self.settings.embed_base_url.clone(),
            ),
        );
        // Capture the stored appearance so theme changes can preview live and
        // revert on Close-without-Save (see `restore_theme_snapshot`).
        self.settings.theme_snapshot = (
            self.theme_pref,
            theme::current_light().id,
            theme::current_dark().id,
        );
        self.settings.open = true;
        // The form's text inputs take the keyboard: reading motions must not
        // act on the code behind the modal.
        self.code_focused = false;
        Task::none()
    }

    pub(crate) fn on_settings_saved(&mut self) -> Task<Message> {
        // The embedding space the vectors in `self.embed_index` belong to, read
        // BEFORE the write. Not `self.settings.ai_snapshot.1`: that is the form
        // as the modal opened, which another window may have overtaken, while
        // this is what the config says one instant before we change it.
        let space_before = embed::stored_space();
        // Commit the previewed appearance (applied live but not persisted while
        // the modal was open) and refresh the snapshot so Close won't revert it.
        // A failed write is appended to whatever the AI half reports below.
        let theme_saved = theme::save(self.theme_pref);
        self.settings.theme_snapshot = (
            self.theme_pref,
            theme::current_light().id,
            theme::current_dark().id,
        );
        let cfg = llm::Config::from_parts(
            self.settings.provider,
            self.settings.key.clone(),
            self.settings.model.clone(),
            self.settings.base_url.clone(),
        );
        let emb = embed::Config::from_parts(
            self.settings.embed_key.clone(),
            self.settings.embed_model.clone(),
            self.settings.embed_base_url.clone(),
        );
        // Written as an EDIT of what the modal was opened with, not as a
        // wholesale replacement of both sections: this form's values are as
        // old as the modal, and Save is also the only way to commit a theme
        // change, so a Save the user believed only changed the theme wrote a
        // stale blank over an API key another window had stored in between —
        // and `send_ai_config` below then revoked it on the server too. A
        // field another writer has changed since is kept, and named.
        let saved = cfg
            .save_from(&self.settings.ai_snapshot.0)
            .and_then(|mut kept| {
                emb.save_from(&self.settings.ai_snapshot.1)
                    .map(|embed_kept| {
                        kept.extend(embed_kept);
                        kept
                    })
            });
        match saved {
            Ok(kept) => {
                // The cached chat config (and the `llm_available` gate) must
                // follow what was just written.
                self.invalidate_llm_config();
                self.embed_available = embed::Config::available();
                self.settings.open = false;
                // A changed embedding space makes every vector held in memory
                // unusable, and keeping them is worse than losing them: FIND
                // would embed the query at the NEW endpoint and rank it against
                // OLD-space vectors (cosine still answers confidently), and a
                // "Build index" would REUSE them — the builder's gate is the
                // summary hash, which a config change does not move — and then
                // save the mix stamped with the new space, at which point
                // `load_for` trusts it forever. `load_for` applies this rule to
                // the file, but only when the project opens; nothing was
                // re-applying it to the copy already in memory.
                let dropped = embed::stored_space() != space_before
                    && !self.proj.embed_index.entries.is_empty();
                if dropped {
                    self.proj.embed_index = embed::Index::default();
                    self.proj.semantic_results.clear();
                }
                self.status = if !kept.is_empty() {
                    format!(
                        "Settings saved — {} changed in another window and was kept",
                        kept.join(", ")
                    )
                } else if self.llm_available {
                    format!("Settings saved ({})", cfg.provider.label())
                } else {
                    "Saved — add an API key to enable Explain".into()
                };
                if dropped {
                    self.status
                        .push_str(" — semantic index dropped (new embedding space); rebuild it");
                }
                // The server holds a copy for server-endpoint AI calls (Ask's
                // agent turns) — keep it in step with the new settings.
                self.send_ai_config();
            }
            Err(e) => self.status = format!("Save failed: {e}"),
        }
        if let Err(e) = theme_saved {
            self.status
                .push_str(&format!(" — the appearance setting was not saved: {e}"));
        }
        Task::none()
    }

    /// Apply a new appearance preference: switch the palette, re-color any cached
    /// diagrams so an open explanation follows the change, and persist it.
    ///
    /// While the Settings modal is open the change is a live preview only —
    /// persistence is deferred to Save (Close reverts). Elsewhere (menu bar,
    /// shortcut) it commits immediately.
    pub(crate) fn set_theme(&mut self, pref: theme::ThemePref) {
        self.theme_pref = pref;
        theme::apply_pref(pref);
        self.restyle_svgs();
        self.status = format!("Appearance: {}", pref.label());
        if !self.settings.open {
            self.persist_theme();
        }
    }

    /// Write the appearance settings to `config.toml`, saying so when that
    /// fails: the palette has already changed on screen, so a silent failure
    /// looks like success until the next launch reverts it.
    pub(crate) fn persist_theme(&mut self) {
        if let Err(e) = theme::save(self.theme_pref) {
            self.status = format!("Could not save the appearance setting: {e}");
        }
    }

    /// Re-resolve the OS appearance, for a window that follows it. Called on
    /// focus and on the system's own notification.
    ///
    /// Gated on the GLOBAL preference, and it never writes one back. Reading
    /// `self.theme_pref` let a window that had not caught up act on a
    /// preference the user had already replaced, and the `apply_pref(System)`
    /// this used to call then stored that stale answer for every window —
    /// silently undoing an explicit Dark or Light.
    pub(crate) fn follow_system_appearance(&mut self) {
        if theme::current_pref() != theme::ThemePref::System {
            return;
        }
        let was_light = theme::is_light();
        theme::set_light(theme::system_is_light());
        if theme::is_light() != was_light {
            self.restyle_svgs();
        }
    }

    /// Apply a light- or dark-theme selection and re-color cached diagrams.
    /// Like [`Self::set_theme`], persistence is deferred to Save while the
    /// Settings modal is open (these pickers only live there).
    pub(crate) fn set_theme_variant(&mut self, id: &str, is_light: bool) {
        if is_light {
            theme::set_light_theme(id);
        } else {
            theme::set_dark_theme(id);
        }
        self.restyle_svgs();
        if !self.settings.open {
            self.persist_theme();
        }
    }

    /// Restore the appearance captured when the Settings modal opened. Used when
    /// it closes without saving, so a previewed theme reverts to the stored one.
    /// Runtime-only: the stored config already holds the snapshot (a preview
    /// never persisted), so this just re-points the live palette at it.
    pub(crate) fn restore_theme_snapshot(&mut self) {
        let (pref, light, dark) = self.settings.theme_snapshot;
        theme::set_light_theme(light);
        theme::set_dark_theme(dark);
        self.theme_pref = pref;
        theme::apply_pref(pref);
        self.restyle_svgs();
        // Drop the "Appearance: …" note left by the discarded preview.
        if self.status.starts_with("Appearance:") {
            self.status.clear();
        }
    }
}

impl App {
    /// Handle a [`SettingsMsg`]: this feature's share of what `dispatch` routes
    /// (after its one ownership check and the menu bookkeeping).
    pub(crate) fn update_settings(&mut self, message: SettingsMsg) -> Task<Message> {
        match message {
            SettingsMsg::SetThemePref(pref) => {
                self.set_theme(pref);
                Task::none()
            }
            SettingsMsg::SetThemeVariant { id, is_light } => {
                self.set_theme_variant(id, is_light);
                Task::none()
            }
            SettingsMsg::ThemeResynced => {
                self.theme_pref = theme::current_pref();
                self.restyle_svgs();
                Task::none()
            }
            SettingsMsg::SystemAppearanceChanged => {
                // Only follow the OS when the preference is System; re-color
                // cached diagrams if the resolved appearance actually flipped.
                self.follow_system_appearance();
                Task::none()
            }
            SettingsMsg::EmbedKeyChanged(s) => {
                self.settings.embed_key = s;
                Task::none()
            }
            SettingsMsg::EmbedModelChanged(s) => {
                self.settings.embed_model = s;
                Task::none()
            }
            SettingsMsg::EmbedBaseUrlChanged(s) => {
                self.settings.embed_base_url = s;
                Task::none()
            }
            SettingsMsg::Open => self.on_open_settings(),
            SettingsMsg::Close => {
                // Closing without Save discards any previewed theme change.
                self.restore_theme_snapshot();
                self.settings.open = false;
                Task::none()
            }
            SettingsMsg::ProviderPicked(p) => {
                // Switching provider resets model/base_url to that provider's
                // defaults (the user can still edit them).
                self.settings.provider = p;
                self.settings.model = p.default_model().to_string();
                self.settings.base_url = p.default_base_url().to_string();
                Task::none()
            }
            SettingsMsg::KeyChanged(s) => {
                self.settings.key = s;
                Task::none()
            }
            SettingsMsg::ModelChanged(s) => {
                self.settings.model = s;
                Task::none()
            }
            SettingsMsg::BaseUrlChanged(s) => {
                self.settings.base_url = s;
                Task::none()
            }
            SettingsMsg::Saved => self.on_settings_saved(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One indexed unit, with a vector that makes it the obvious hit for
    /// anything (so a stale index answering a query would be conspicuous).
    fn entry(name: &str) -> embed::Entry {
        embed::Entry {
            node: explain::Node::Function {
                file: PathBuf::from("/p/a.rs"),
                name: name.into(),
                ordinal: 0,
            },
            hash: 0,
            vec: vec![1.0, 0.0],
        }
    }

    /// Saving Settings is the one moment the embedding space can move under a
    /// session, and the vectors held in memory do not move with it. Keeping
    /// them is the silent failure: FIND embeds the query at the NEW endpoint
    /// and ranks it against the OLD space's vectors, and "Build index" reuses
    /// them (its gate is the summary hash, which a config change does not
    /// move) and then writes the mix stamped with the new space, at which
    /// point `embed::load_for` accepts the file for good.
    ///
    /// A repoint of the SAME model name at another provider is the case this
    /// has to catch: it is a different space, it is exactly why the on-disk
    /// index records the endpoint, and it is invisible to every check that
    /// compares model names.
    #[test]
    fn a_settings_save_that_moves_the_embedding_space_drops_the_in_memory_index() {
        use crate::app::tests::{blank_app, data_dir_override, test_dir};
        const OPENAI: &str = "https://api.openai.com/v1";
        let dir = test_dir("embed-space-settings");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // `App::blank()` reads `trust.toml` / `connections.toml` through the
        // data dir, and this test writes the config file the handler saves to,
        // so isolate both. The guard holds the env lock for the whole test and
        // hands the variable back to the suite's isolated default on drop,
        // including when an assertion below unwinds.
        let _env = data_dir_override(&dir);
        std::fs::write(
            dir.join("config.toml"),
            format!("[embedding]\napi_key = \"sk\"\nmodel = \"m\"\nbase_url = \"{OPENAI}\"\n"),
        )
        .unwrap();

        let mut app = blank_app();
        app.proj.embed_index = embed::Index {
            model: "m".into(),
            base_url: OPENAI.into(),
            entries: vec![entry("f")],
        };
        app.proj.semantic_results = vec![(entry("f").node, 0.9)];
        // The modal as it opened on that stored config.
        let opened_on = embed::Config::from_parts("sk".into(), "m".into(), OPENAI.into());
        app.settings.ai_snapshot.1 = opened_on.clone();
        app.settings.embed_key = "sk".into();
        app.settings.embed_model = "m".into();
        app.settings.embed_base_url = OPENAI.into();

        // Save is also the only way to commit a theme change, so a save that
        // leaves the space alone must not cost the user a full re-embed.
        let _ = app.on_settings_saved();
        assert_eq!(
            app.proj.embed_index.entries.len(),
            1,
            "an unchanged embedding config forced a rebuild"
        );

        // Same model, another provider serving it: a different space.
        app.settings.ai_snapshot.1 = opened_on;
        app.settings.embed_base_url = "http://localhost:1234/v1".into();
        let _ = app.on_settings_saved();
        assert!(
            app.proj.embed_index.entries.is_empty(),
            "old-space vectors survived the repoint: {} left",
            app.proj.embed_index.entries.len()
        );
        assert!(
            app.proj.semantic_results.is_empty(),
            "results ranked in the old space stayed on screen"
        );
        assert!(
            app.status.contains("rebuild it"),
            "the drop was not explained: {}",
            app.status
        );

        drop(_env);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
