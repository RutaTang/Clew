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
                // Not where the key is kept: Settings, opened here, is where
                // it is added (the file's whole path filled the status line).
                self.status = tr!("Add an API key in Settings").into();
                Err(Task::done(Message::Settings(SettingsMsg::OpenAt(
                    SettingsPage::LanguageModel,
                ))))
            }
        }
    }

    pub(crate) fn on_open_settings(&mut self) -> Task<Message> {
        // Open already (⌘, again, or an action that asks for a key opens
        // it): the form as it stands. Filled anew, a key typed and not yet
        // saved was lost, and the appearance Close goes back to became the
        // one being previewed.
        if self.settings.open {
            return Task::none();
        }
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
        // The service, as it is stored, whether or not it is the source.
        let e = embed::Config::service_or_default();
        self.settings.embed_here = embed::Config::stored_here();
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
        self.settings.embed_here_snapshot = self.settings.embed_here;
        self.settings.save_failed = false;
        self.settings.integrations.note = None;
        self.refresh_integrations();
        self.settings.open = true;
        // The form's text inputs take the keyboard: reading motions must not
        // act on the code behind the modal.
        self.code_focused = false;
        // The project's Python interpreters, listed afresh: environments come
        // and go between openings.
        self.refresh_language_options()
    }

    pub(crate) fn on_settings_saved(&mut self) -> Task<Message> {
        // The embedding space the vectors in `self.embed_index` belong to, read
        // BEFORE the write. Not `self.settings.ai_snapshot.1`: that is the form
        // as the modal opened, which another window may have overtaken, while
        // this is what the config says one instant before we change it.
        let space_before = embed::stored_space();
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
        // old as the modal, so a save the user believed changed something
        // else wrote a stale blank over an API key another window had stored
        // in between — and `send_ai_config` below then revoked it on the
        // server too. A field another writer has changed since is kept, and
        // named.
        let saved = cfg
            .save_from(&self.settings.ai_snapshot.0)
            .and_then(|mut kept| {
                emb.save_from(&self.settings.ai_snapshot.1)
                    .map(|embed_kept| {
                        kept.extend(embed_kept);
                        kept
                    })
            })
            .and_then(|mut kept| {
                embed::Config::save_source_from(
                    self.settings.embed_here,
                    self.settings.embed_here_snapshot,
                )
                .map(|source_kept| {
                    kept.extend(source_kept);
                    kept
                })
            });
        match saved {
            Ok(kept) => {
                // The cached chat config (and the `llm_available` gate) must
                // follow what was just written.
                self.invalidate_llm_config();
                self.refresh_embeddings();
                self.settings.open = false;
                // Said where the model is picked; read there, it is done.
                self.voice.download_errors.clear();
                self.voice.speech.download_errors.clear();
                self.code_focused = true;
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
                let space_changed = embed::stored_space() != space_before;
                // Vectors no longer made here: the model held in memory goes.
                if !embed::Config::stored_here() {
                    embed::local::let_go();
                }
                // A build embedding in the space just left stops now: every
                // batch still to come would be paid for (or made on this
                // computer) and then thrown away — its vectors are of a
                // space this window no longer searches in. One whose vectors
                // are all made, being merged into its own space's file, is
                // let finish: what it paid for is kept there, and a build
                // asked for meanwhile waits for it, as any does.
                let stopped = space_changed
                    && self.proj.building_embeddings
                    && self.proj.inflight.embed_build.is_some();
                if stopped {
                    self.proj.inflight.embed_build = None;
                    self.proj.building_embeddings = false;
                    self.proj.embed_build_pending = false;
                }
                let dropped = stopped || space_changed && !self.proj.embed_index.entries.is_empty();
                if space_changed {
                    self.retire_semantic_search();
                    self.proj.semantic_results.clear();
                }
                if dropped {
                    self.proj.embed_index = embed::Index::default();
                }
                // The fields kept, by the names their pages give them (the
                // chat model's and the embeddings' API key are both "API
                // key": named once), not by their keys in config.toml.
                let mut fields: Vec<&str> = Vec::new();
                for key in &kept {
                    let field = saved_field_name(key);
                    if !fields.contains(&field) {
                        fields.push(field);
                    }
                }
                self.status = if !fields.is_empty() {
                    trn!(
                        "Settings saved — {fields} was changed in another window, so that value was kept",
                        "Settings saved — {fields} were changed in another window, so those values were kept",
                        fields.len(),
                        fields = fields.join(", ")
                    )
                } else if self.llm_available {
                    tr!(
                        "Settings saved ({provider})",
                        provider = crate::i18n::lookup(cfg.provider.label())
                    )
                } else {
                    tr!("Settings saved — add an API key to use Explain").to_string()
                };
                if dropped {
                    self.status = tr!(
                        "{status} — the semantic index was cleared because the embedding \
                         settings changed; rebuild it",
                        status = self.status
                    );
                }
                // The server holds a copy for server-endpoint AI calls (Ask's
                // agent turns) — keep it in step with the new settings.
                self.send_ai_config();
            }
            Err(e) => {
                self.status = tr!("Could not save settings: {error}", error = e);
                self.settings.save_failed = true;
            }
        }
        Task::none()
    }

    /// Whether Settings shows the keyboard shortcuts: keys and menu chords
    /// are the page's then, not the app's behind it.
    pub(crate) fn shortcuts_shown(&self) -> bool {
        self.settings.open && self.settings.page == SettingsPage::Shortcuts
    }

    /// Keep the language servers' listing and log current while Settings
    /// shows its Languages page (`server_panel`): listed afresh when the
    /// page comes up, and no longer followed once it goes.
    pub(crate) fn sync_server_listing(&mut self) -> Task<Message> {
        let showing = self.settings.open && self.settings.page == SettingsPage::Languages;
        let was = self.server_panel;
        self.server_panel = showing;
        if showing && !was {
            self.refresh_server_panel()
        } else {
            Task::none()
        }
    }

    /// Whether the AI fields differ from what the form was filled with:
    /// closing then saves them, and with none changed writes nothing.
    pub(crate) fn settings_drafts_changed(&self) -> bool {
        let (chat, embedding) = &self.settings.ai_snapshot;
        let now = llm::Config::from_parts(
            self.settings.provider,
            self.settings.key.clone(),
            self.settings.model.clone(),
            self.settings.base_url.clone(),
        );
        let embed_now = embed::Config::from_parts(
            self.settings.embed_key.clone(),
            self.settings.embed_model.clone(),
            self.settings.embed_base_url.clone(),
        );
        now.provider != chat.provider
            || self.settings.embed_here != self.settings.embed_here_snapshot
            || now.api_key != chat.api_key
            || now.model != chat.model
            || now.base_url != chat.base_url
            || embed_now.api_key != embedding.api_key
            || embed_now.model != embedding.model
            || embed_now.base_url != embedding.base_url
    }

    /// Put Settings away, keeping what was changed in it: the AI fields
    /// saved when they were edited (a failed save leaves it open, its
    /// reason in the status bar); the appearance, the voice and the
    /// languages were saved as they changed.
    ///
    /// A save that fails again, its failure already said, closes all the
    /// same, the reason left in the status bar. Held open each time,
    /// Settings could not be left — by ×, Esc or a click outside — while
    /// `config.toml` could not be written (a typo in it, its lock held),
    /// until the file was fixed outside clew.
    pub(crate) fn close_settings(&mut self) -> Task<Message> {
        if self.settings_drafts_changed() {
            let said = self.settings.save_failed;
            let _ = self.on_settings_saved();
            if self.settings.open && !said {
                return Task::none();
            }
        }
        self.settings.open = false;
        self.rebinding = None;
        self.keymap_notice = None;
        // A model's failed download is said under it, in here; once
        // Settings is closed it has been read, and it goes.
        self.voice.download_errors.clear();
        self.voice.speech.download_errors.clear();
        // Back to reading, as the finder's close does: the motions the
        // form took from the code are the code's again.
        self.code_focused = true;
        self.sync_server_listing()
    }

    /// Apply a new appearance preference: switch the palette, re-color any cached
    /// diagrams so an open explanation follows the change, and persist it —
    /// from Settings as from the menu bar: Settings saves as it changes.
    pub(crate) fn set_theme(&mut self, pref: theme::ThemePref) {
        self.theme_pref = pref;
        theme::apply_pref(pref);
        self.restyle_svgs();
        self.status = tr!("Appearance: {mode}", mode = pref.label());
        self.persist_theme();
    }

    /// Write the appearance settings to `config.toml`, saying so when that
    /// fails: the palette has already changed on screen, so a silent failure
    /// looks like success until the next launch reverts it.
    pub(crate) fn persist_theme(&mut self) {
        if let Err(e) = theme::save(self.theme_pref) {
            self.status = tr!("Could not save the appearance setting: {error}", error = e);
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

    /// Apply a light- or dark-theme selection, re-color cached diagrams and
    /// persist it, as [`Self::set_theme`] does.
    pub(crate) fn set_theme_variant(&mut self, id: &str, is_light: bool) {
        if is_light {
            theme::set_light_theme(id);
        } else {
            theme::set_dark_theme(id);
        }
        self.restyle_svgs();
        self.persist_theme();
    }
}

/// A field of the `[llm]` or `[embedding]` section of config.toml, by the
/// name its Settings page gives it.
fn saved_field_name(key: &str) -> &str {
    match key {
        "provider" => tr!("Provider"),
        "api_key" => tr!("API key"),
        "model" => tr!("Model"),
        "base_url" => tr!("Base URL"),
        "source" => tr!("Where embeddings are made"),
        other => other,
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
            SettingsMsg::SetLanguage(pref) => {
                if let Err(e) = crate::i18n::apply_and_save(pref) {
                    self.status = tr!("Could not save the language: {error}", error = e);
                }
                // Every window speaks it: the others are told to draw again
                // (as for an appearance change; the shell also builds the
                // macOS menu bar again), and the server that makes this
                // window's AI calls is told which language to answer in.
                theme::redraw_everywhere();
                self.send_ai_config();
                self.respeak_explanation()
            }
            SettingsMsg::SetThemeVariant { id, is_light } => {
                self.set_theme_variant(id, is_light);
                Task::none()
            }
            SettingsMsg::ThemeResynced => {
                self.theme_pref = theme::current_pref();
                self.restyle_svgs();
                // It may have been the language another window chose: this
                // window's server answers in it too.
                self.send_ai_config();
                self.respeak_explanation()
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
            msg @ (SettingsMsg::EmbedHere(..)
            | SettingsMsg::DownloadEmbedModel
            | SettingsMsg::CancelEmbedDownload
            | SettingsMsg::EmbedDownloadProgress { .. }
            | SettingsMsg::EmbedDownloaded(..)
            | SettingsMsg::RemoveEmbedModel
            | SettingsMsg::EmbedModelRemoved(..)) => self.on_local_embedding(msg),
            SettingsMsg::Open => {
                let opened = self.on_open_settings();
                Task::batch([opened, self.sync_server_listing()])
            }
            SettingsMsg::OpenAt(page) => {
                self.settings.page = page;
                let opened = self.on_open_settings();
                Task::batch([opened, self.sync_server_listing()])
            }
            SettingsMsg::Page(page) => {
                self.settings.page = page;
                // A shortcut half rebound stays with the page it was on.
                self.rebinding = None;
                self.keymap_notice = None;
                self.sync_server_listing()
            }
            SettingsMsg::Close => self.close_settings(),
            SettingsMsg::ProviderPicked(p) => {
                // The drop-down sends the provider already picked too: that
                // changes nothing. It used to put the provider's defaults over
                // a model and base URL of the user's own, which closing
                // Settings then saved.
                if p == self.settings.provider {
                    return Task::none();
                }
                self.settings.provider = p;
                // Back to the provider Settings opened on: its model and base
                // URL as they were then, so a wrong pick undone costs nothing.
                // Another provider: its defaults (the user can still edit
                // them).
                let (model, base_url) = match &self.settings.ai_snapshot.0 {
                    opened if opened.provider == p => {
                        (opened.model.clone(), opened.base_url.clone())
                    }
                    _ => (
                        p.default_model().to_string(),
                        p.default_base_url().to_string(),
                    ),
                };
                self.settings.model = model;
                self.settings.base_url = base_url;
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

    /// The provider drop-down sends the provider already picked as well:
    /// picking it again changes nothing, and going back to it after a wrong
    /// pick brings back its model and base URL. Both used to put the
    /// provider's defaults over a model and gateway of the user's own — and
    /// Settings has no Cancel: closing it saved them.
    #[test]
    fn picking_the_provider_again_keeps_the_users_model_and_base_url() {
        use crate::app::tests::{blank_app, data_dir_override, test_dir};
        let dir = test_dir("settings-provider-again");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _env = data_dir_override(&dir);
        let config = "[llm]\nprovider = \"anthropic\"\napi_key = \"sk\"\n\
                      model = \"claude-x-custom\"\nbase_url = \"https://gateway.corp/v1\"\n";
        std::fs::write(dir.join("config.toml"), config).unwrap();
        let mut app = blank_app();
        let _ = app.update(Message::Settings(SettingsMsg::Open));
        let users = |app: &App| (app.settings.model.clone(), app.settings.base_url.clone());
        let own = (
            "claude-x-custom".to_string(),
            "https://gateway.corp/v1".to_string(),
        );
        assert_eq!(users(&app), own);

        let pick = |app: &mut App, p| {
            let _ = app.update(Message::Settings(SettingsMsg::ProviderPicked(p)));
        };
        pick(&mut app, llm::Provider::Anthropic);
        assert_eq!(users(&app), own, "picked again");
        pick(&mut app, llm::Provider::OpenAI);
        assert_eq!(
            app.settings.model,
            llm::Provider::OpenAI.default_model(),
            "another provider starts from its defaults"
        );
        pick(&mut app, llm::Provider::Anthropic);
        assert_eq!(users(&app), own, "a wrong pick undone");
        assert!(!app.settings_drafts_changed());
        let _ = app.update(Message::Settings(SettingsMsg::Close));
        assert!(!app.settings.open);
        assert_eq!(
            std::fs::read_to_string(dir.join("config.toml")).unwrap(),
            config,
            "nothing changed, nothing written"
        );
    }

    /// A save that keeps fields another window changed names them as their
    /// pages do, each name once, in the reader's language. It named their
    /// keys in config.toml: "api_key, api_key".
    #[test]
    fn a_save_names_the_fields_it_kept_as_settings_does() {
        use crate::app::tests::{blank_app, data_dir_override, test_dir};
        use crate::i18n::{Lang, with_lang};
        let config = |chat: &str, embedding: &str| {
            format!(
                "[llm]\nprovider = \"anthropic\"\napi_key = \"{chat}\"\nmodel = \"m\"\n\
                 base_url = \"https://gateway.corp/v1\"\n\n[embedding]\napi_key = \"{embedding}\"\n\
                 model = \"e\"\nbase_url = \"https://api.openai.com/v1\"\n"
            )
        };
        for lang in [Lang::En, Lang::Fr] {
            let dir = test_dir(&format!("settings-kept-named-{}", lang.code()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let _env = data_dir_override(&dir);
            std::fs::write(dir.join("config.toml"), config("sk-a", "ek-a")).unwrap();
            let mut app = blank_app();
            let _ = app.update(Message::Settings(SettingsMsg::Open));
            // Another window stores both keys meanwhile.
            std::fs::write(dir.join("config.toml"), config("sk-b", "ek-b")).unwrap();
            app.settings.model = "m2".into();
            with_lang(lang, || {
                let _ = app.on_settings_saved();
                let field = crate::i18n::lookup("API key");
                assert_eq!(
                    app.status,
                    trn!(
                        "Settings saved — {fields} was changed in another window, so that value was kept",
                        "Settings saved — {fields} were changed in another window, so those values were kept",
                        1,
                        fields = field
                    ),
                    "{lang:?}"
                );
            });
        }
    }

    /// A save that fails — `config.toml` holds a typo — keeps Settings open
    /// once, its reason said, so what was typed is not lost unseen; closing
    /// again closes. Every way out used to repeat the failed save, and
    /// Settings could not be left until the file was fixed outside clew.
    #[test]
    fn settings_close_after_a_failed_save_has_been_said() {
        use crate::app::tests::{blank_app, data_dir_override, test_dir};
        let dir = test_dir("settings-save-fails");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _env = data_dir_override(&dir);
        let broken = "[llm\napi_key = \"sk\"\n";
        std::fs::write(dir.join("config.toml"), broken).unwrap();
        let mut app = blank_app();
        for _ in 0..2 {
            let _ = app.update(Message::Settings(SettingsMsg::Open));
            app.settings.key = "pasted".into();
            let _ = app.update(Message::Settings(SettingsMsg::Close));
            assert!(app.settings.open, "the failure is said first");
            assert!(
                app.status.contains("Could not save settings"),
                "{}",
                app.status
            );
            let _ = app.update(Message::Settings(SettingsMsg::Close));
            assert!(!app.settings.open, "Settings cannot be left");
            assert!(
                app.status.contains("Could not save settings"),
                "{}",
                app.status
            );
        }
        assert_eq!(
            std::fs::read_to_string(dir.join("config.toml")).unwrap(),
            broken,
            "the user's bytes are theirs to fix"
        );
    }
}
